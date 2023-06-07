use std::{collections::HashMap, sync::Arc};

use analyzer::{PeerDDL, QueryAssocation};
use async_trait::async_trait;
use bytes::{BufMut, BytesMut};
use catalog::{Catalog, CatalogConfig};
use clap::Parser;
use cursor::PeerCursors;
use dashmap::DashMap;
use peer_bigquery::BigQueryQueryExecutor;
use peer_cursor::{
    util::{records_to_query_response, sendable_stream_to_query_response},
    QueryExecutor, QueryOutput, SchemaRef,
};
use peerdb_parser::{NexusParsedStatement, NexusQueryParser, NexusStatement};
use pgwire::{
    api::{
        auth::{
            md5pass::{hash_md5_password, MakeMd5PasswordAuthStartupHandler},
            AuthSource, LoginInfo, Password, ServerParameterProvider,
        },
        portal::{Format, Portal},
        query::{ExtendedQueryHandler, SimpleQueryHandler, StatementOrPortal},
        results::{DescribeResponse, Response, Tag},
        store::MemPortalStore,
        ClientInfo, MakeHandler, Type,
    },
    error::{ErrorInfo, PgWireError, PgWireResult},
    messages::response::{CommandComplete, ReadyForQuery},
    tokio::process_socket,
};
use pt::peers::{peer::Config, Peer};
use rand::Rng;
use tokio::sync::Mutex;
use tokio::{io::AsyncWriteExt, net::TcpListener};
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

mod cursor;

struct DummyAuthSource;

#[async_trait]
impl AuthSource for DummyAuthSource {
    async fn get_password(&self, login_info: &LoginInfo) -> PgWireResult<Password> {
        println!("login info: {:?}", login_info);

        // randomly generate a 4 byte salt
        let salt = rand::thread_rng().gen::<[u8; 4]>().to_vec();
        let password = "peerdb";

        let hash_password = hash_md5_password(
            login_info.user().map(|s| s.as_str()).unwrap_or(""),
            password,
            salt.as_ref(),
        );
        Ok(Password::new(Some(salt), hash_password.as_bytes().to_vec()))
    }
}

pub struct NexusBackend {
    catalog: Arc<Mutex<Catalog>>,
    portal_store: Arc<MemPortalStore<NexusParsedStatement>>,
    query_parser: Arc<NexusQueryParser>,
    peer_cursors: Arc<Mutex<PeerCursors>>,
    executors: Arc<DashMap<String, Arc<Box<dyn QueryExecutor>>>>,
}

impl NexusBackend {
    pub fn new(catalog: Arc<Mutex<Catalog>>) -> Self {
        let query_parser = NexusQueryParser::new(catalog.clone());
        Self {
            catalog,
            portal_store: Arc::new(MemPortalStore::new()),
            query_parser: Arc::new(query_parser),
            peer_cursors: Arc::new(Mutex::new(PeerCursors::new())),
            executors: Arc::new(DashMap::new()),
        }
    }

    // execute a statement on a peer
    async fn execute_statement<'a>(
        &self,
        executor: Arc<Box<dyn QueryExecutor>>,
        stmt: &sqlparser::ast::Statement,
        peer_holder: Option<Box<Peer>>,
    ) -> PgWireResult<Vec<Response<'a>>> {
        let res = executor.execute(stmt).await?;
        match res {
            QueryOutput::AffectedRows(rows) => Ok(vec![Response::Execution(
                Tag::new_for_execution("OK", Some(rows)),
            )]),
            QueryOutput::Stream(rows) => {
                let schema = rows.schema();
                // todo: why is this a vector of response rather than a single response?
                // can this be because of multiple statements?
                let res = sendable_stream_to_query_response(schema, rows)?;
                Ok(vec![res])
            }
            QueryOutput::Records(records) => {
                let res = records_to_query_response(records)?;
                Ok(vec![res])
            }
            QueryOutput::Cursor(cm) => {
                println!("cursor modification: {:?}", cm);
                let mut peer_cursors = self.peer_cursors.lock().await;
                match cm {
                    peer_cursor::CursorModification::Created(cursor_name) => {
                        peer_cursors.add_cursor(cursor_name, peer_holder.unwrap());
                        Ok(vec![Response::Execution(Tag::new_for_execution(
                            "DECLARE CURSOR",
                            None,
                        ))])
                    }
                    peer_cursor::CursorModification::Closed(cursors) => {
                        for cursor_name in cursors {
                            peer_cursors.remove_cursor(cursor_name);
                        }
                        Ok(vec![Response::Execution(Tag::new_for_execution(
                            "CLOSE CURSOR",
                            None,
                        ))])
                    }
                }
            }
        }
    }

    async fn handle_query<'a>(
        &self,
        nexus_stmt: NexusStatement,
    ) -> PgWireResult<Vec<Response<'a>>> {
        // println!("handle query nexus statement: {:#?}", nexus_stmt);

        let mut peer_holder: Option<Box<Peer>> = None;
        match nexus_stmt {
            NexusStatement::PeerDDL { stmt: _, ddl } =>
            {
                #[allow(clippy::single_match)]
                match ddl {
                    PeerDDL::CreatePeer {
                        peer,
                        if_not_exists: _,
                    } => {
                        let catalog = self.catalog.lock().await;
                        catalog.create_peer(peer.as_ref()).await.map_err(|e| {
                            PgWireError::UserError(Box::new(ErrorInfo::new(
                                "ERROR".to_owned(),
                                "internal_error".to_owned(),
                                e.to_string(),
                            )))
                        })?;
                        Ok(vec![Response::Execution(Tag::new_for_execution(
                            "OK", None,
                        ))])
                    }
                }
            }
            NexusStatement::PeerQuery { stmt, assoc } => {
                // get the query executor
                let executor = match assoc {
                    QueryAssocation::Peer(peer) => {
                        println!("acquiring executor for peer query: {:?}", peer.name);
                        peer_holder = Some(peer.clone());
                        self.get_peer_executor(&peer).await
                    }
                    QueryAssocation::Catalog => {
                        println!("acquiring executor for catalog query");
                        let catalog = self.catalog.lock().await;
                        catalog.get_executor()
                    }
                };

                self.execute_statement(executor, &stmt, peer_holder).await
            }

            NexusStatement::PeerCursor { stmt, cursor } => {
                let executor = {
                    let peer_cursors = self.peer_cursors.lock().await;
                    let peer = match cursor {
                        analyzer::CursorEvent::Fetch(c, _) => peer_cursors.get_peer(&c),
                        analyzer::CursorEvent::CloseAll => todo!("close all cursors"),
                        analyzer::CursorEvent::Close(c) => peer_cursors.get_peer(&c),
                    };
                    match peer {
                        None => {
                            let catalog = self.catalog.lock().await;
                            catalog.get_executor()
                        }
                        Some(peer) => {
                            println!("acquiring executor for peer cursor query: {:?}", peer.name);
                            self.get_peer_executor(peer).await
                        }
                    }
                };

                self.execute_statement(executor, &stmt, peer_holder).await
            }
        }
    }

    async fn get_peer_executor(&self, peer: &Peer) -> Arc<Box<dyn QueryExecutor>> {
        if let Some(executor) = self.executors.get(&peer.name) {
            return Arc::clone(executor.value());
        }

        let executor = match &peer.config {
            Some(Config::BigqueryConfig(ref c)) => {
                let executor = BigQueryQueryExecutor::new(c).await.unwrap();
                Arc::new(Box::new(executor) as Box<dyn QueryExecutor>)
            }
            Some(Config::PostgresConfig(ref c)) => {
                let peername = Some(peer.name.clone());
                let executor = peer_postgres::PostgresQueryExecutor::new(peername, &c)
                    .await
                    .unwrap();
                Arc::new(Box::new(executor) as Box<dyn QueryExecutor>)
            }
            _ => {
                panic!("peer type not supported: {:?}", peer)
            }
        };

        self.executors
            .insert(peer.name.clone(), Arc::clone(&executor));
        executor
    }
}

#[async_trait]
impl SimpleQueryHandler for NexusBackend {
    async fn do_query<'a, C>(&self, _client: &C, sql: &'a str) -> PgWireResult<Vec<Response<'a>>>
    where
        C: ClientInfo + Unpin + Send + Sync,
    {
        let parsed = self.query_parser.parse_simple_sql(sql)?;
        let nexus_stmt = parsed.statement;
        self.handle_query(nexus_stmt).await
    }
}

fn parameter_to_string(portal: &Portal<NexusParsedStatement>, idx: usize) -> PgWireResult<String> {
    // the index is managed from portal's parameters count so it's safe to
    // unwrap here.
    let param_type = portal.statement().parameter_types().get(idx).unwrap();
    match param_type {
        &Type::VARCHAR | &Type::TEXT => Ok(format!(
            "'{}'",
            portal.parameter::<String>(idx)?.as_deref().unwrap_or("")
        )),
        &Type::BOOL => Ok(portal
            .parameter::<bool>(idx)?
            .map(|v| v.to_string())
            .unwrap_or_else(|| "".to_owned())),
        &Type::INT4 => Ok(portal
            .parameter::<i32>(idx)?
            .map(|v| v.to_string())
            .unwrap_or_else(|| "".to_owned())),
        &Type::INT8 => Ok(portal
            .parameter::<i64>(idx)?
            .map(|v| v.to_string())
            .unwrap_or_else(|| "".to_owned())),
        &Type::FLOAT4 => Ok(portal
            .parameter::<f32>(idx)?
            .map(|v| v.to_string())
            .unwrap_or_else(|| "".to_owned())),
        &Type::FLOAT8 => Ok(portal
            .parameter::<f64>(idx)?
            .map(|v| v.to_string())
            .unwrap_or_else(|| "".to_owned())),
        _ => Err(PgWireError::UserError(Box::new(ErrorInfo::new(
            "ERROR".to_owned(),
            "22023".to_owned(),
            "unsupported_parameter_value".to_owned(),
        )))),
    }
}

#[async_trait]
impl ExtendedQueryHandler for NexusBackend {
    type Statement = NexusParsedStatement;
    type PortalStore = MemPortalStore<Self::Statement>;
    type QueryParser = NexusQueryParser;

    fn portal_store(&self) -> Arc<Self::PortalStore> {
        self.portal_store.clone()
    }

    fn query_parser(&self) -> Arc<Self::QueryParser> {
        self.query_parser.clone()
    }

    async fn do_query<'a, C>(
        &self,
        _client: &mut C,
        portal: &'a Portal<Self::Statement>,
        _max_rows: usize,
    ) -> PgWireResult<Response<'a>>
    where
        C: ClientInfo + Unpin + Send + Sync,
    {
        let stmt = portal.statement().statement();
        println!("[eqp] do_query: {}", stmt.query);

        // manually replace variables in prepared statement
        let mut sql = stmt.query.clone();
        for i in 0..portal.parameter_len() {
            sql = sql.replace(&format!("${}", i + 1), &parameter_to_string(portal, i)?);
        }

        let parsed = self.query_parser.parse_simple_sql(&sql)?;
        let nexus_stmt = parsed.statement;
        let result = self.handle_query(nexus_stmt).await?;
        if result.is_empty() {
            Ok(Response::EmptyQuery)
        } else {
            Ok(result.into_iter().next().unwrap())
        }
    }

    async fn do_describe<C>(
        &self,
        _client: &mut C,
        target: StatementOrPortal<'_, Self::Statement>,
    ) -> PgWireResult<DescribeResponse>
    where
        C: ClientInfo + Unpin + Send + Sync,
    {
        let (param_types, stmt, _format) = match target {
            StatementOrPortal::Statement(stmt) => {
                let param_types = Some(stmt.parameter_types().clone());
                (param_types, stmt.statement(), &Format::UnifiedBinary)
            }
            StatementOrPortal::Portal(portal) => (
                None,
                portal.statement().statement(),
                portal.result_column_format(),
            ),
        };

        println!("[eqp] do_describe: {}", stmt.query);
        let stmt = &stmt.statement;
        match stmt {
            NexusStatement::PeerDDL { .. } => Ok(DescribeResponse::no_data()),
            NexusStatement::PeerCursor { .. } => Ok(DescribeResponse::no_data()),
            NexusStatement::PeerQuery { stmt, assoc } => {
                let schema: Option<SchemaRef> = match assoc {
                    QueryAssocation::Peer(peer) => {
                        println!("acquiring executor for peer query: {:?}", peer.name);
                        // if the peer is of type bigquery, let us route the query to bq.
                        match &peer.config {
                            Some(Config::BigqueryConfig(c)) => {
                                let executor =
                                    BigQueryQueryExecutor::new(c).await.map_err(|e| {
                                        PgWireError::UserError(Box::new(ErrorInfo::new(
                                            "ERROR".to_owned(),
                                            "internal_error".to_owned(),
                                            e.to_string(),
                                        )))
                                    })?;
                                executor.describe(stmt).await?
                            }
                            _ => {
                                panic!("peer type not supported: {:?}", peer)
                            }
                        }
                    }
                    QueryAssocation::Catalog => {
                        let catalog = self.catalog.lock().await;
                        let executor = catalog.get_executor();
                        executor.describe(stmt).await?
                    }
                };
                if let Some(_schema) = schema {
                    Ok(DescribeResponse::no_data())
                } else {
                    Ok(DescribeResponse::no_data())
                }
            }
        }
    }
}

struct MakeNexusBackend {
    catalog: Arc<Mutex<Catalog>>,
}

impl MakeNexusBackend {
    fn new(catalog: Catalog) -> Self {
        Self {
            catalog: Arc::new(Mutex::new(catalog)),
        }
    }
}

impl MakeHandler for MakeNexusBackend {
    type Handler = Arc<NexusBackend>;

    fn make(&self) -> Self::Handler {
        Arc::new(NexusBackend::new(self.catalog.clone()))
    }
}

/// Arguments for the nexus server.
#[derive(Parser, Debug)]
struct Args {
    /// Host to bind to, defaults to localhost.
    #[clap(long, default_value = "0.0.0.0", env = "NEXUS_HOST")]
    host: String,

    /// Port of the server, defaults to `9900`.
    #[clap(short, long, default_value_t = 9900, env = "NEXUS_PORT")]
    port: u16,

    // define args for catalog postgres server - host, port, user, password, database
    /// Catalog postgres server host.
    /// Defaults to `localhost`.
    #[clap(long, default_value = "localhost", env = "NEXUS_CATALOG_HOST")]
    catalog_host: String,

    /// Catalog postgres server port.
    /// Defaults to `5432`.
    #[clap(long, default_value_t = 5432, env = "NEXUS_CATALOG_PORT")]
    catalog_port: u16,

    /// Catalog postgres server user.
    /// Defaults to `postgres`.
    #[clap(long, default_value = "postgres", env = "NEXUS_CATALOG_USER")]
    catalog_user: String,

    /// Catalog postgres server password.
    /// Defaults to `postgres`.
    #[clap(long, default_value = "postgres", env = "NEXUS_CATALOG_PASSWORD")]
    catalog_password: String,

    /// Catalog postgres server database.
    /// Defaults to `postgres`.
    #[clap(long, default_value = "postgres", env = "NEXUS_CATALOG_DATABASE")]
    catalog_database: String,

    /// Path to the TLS certificate file.
    #[clap(long, requires = "tls_key", env = "NEXUS_TLS_CERT")]
    tls_cert: Option<String>,

    /// Path to the TLS private key file.
    #[clap(long, requires = "tls_cert", env = "NEXUS_TLS_KEY")]
    tls_key: Option<String>,

    /// Path to the directory where nexus logs will be written to.
    ///
    /// This is only respected in release mode. In debug mode the logs
    /// will exlusively be written to stdout.
    #[clap(short, long, default_value = "/var/log/nexus", env = "NEXUS_LOG_DIR")]
    log_dir: String,

    /// host:port of the flow server for flow jobs.
    #[clap(long, env = "NEXUS_FLOW_SERVER_ADDR")]
    flow_server_addr: Option<String>,
}

// Get catalog config from args
fn get_catalog_config(args: &Args) -> CatalogConfig {
    CatalogConfig {
        host: args.catalog_host.clone(),
        port: args.catalog_port,
        user: args.catalog_user.clone(),
        password: args.catalog_password.clone(),
        database: args.catalog_database.clone(),
    }
}

pub struct NexusServerParameterProvider;

impl ServerParameterProvider for NexusServerParameterProvider {
    fn server_parameters<C>(&self, _client: &C) -> Option<HashMap<String, String>>
    where
        C: ClientInfo,
    {
        let mut params = HashMap::with_capacity(4);
        params.insert("server_version".to_owned(), "14".to_owned());
        params.insert("server_encoding".to_owned(), "UTF8".to_owned());
        params.insert("client_encoding".to_owned(), "UTF8".to_owned());
        params.insert("DateStyle".to_owned(), "ISO YMD".to_owned());
        params.insert("integer_datetimes".to_owned(), "on".to_owned());

        Some(params)
    }
}

// setup tracing
fn setup_tracing() {
    let fmt_layer = fmt::layer().with_target(false);
    let console_layer = console_subscriber::spawn();

    // add min tracing as info
    let filter_layer = EnvFilter::try_from_default_env()
        .or_else(|_| EnvFilter::try_new("info"))
        .unwrap();

    tracing_subscriber::registry()
        .with(console_layer)
        .with(fmt_layer)
        .with(filter_layer)
        .init();
}

#[tokio::main]
pub async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();

    setup_tracing();

    let args = Args::parse();

    let authenticator = Arc::new(MakeMd5PasswordAuthStartupHandler::new(
        Arc::new(DummyAuthSource),
        Arc::new(NexusServerParameterProvider),
    ));
    let catalog_config = get_catalog_config(&args);

    {
        // leave this in this scope so that the catalog is dropped before we
        // start the server.
        let mut catalog = Catalog::new(&catalog_config).await?;
        catalog.run_migrations().await?;
    }

    let server_addr = format!("{}:{}", args.host, args.port);
    let listener = TcpListener::bind(&server_addr).await.unwrap();
    println!("Listening on {}", server_addr);

    loop {
        let (mut socket, _) = listener.accept().await.unwrap();
        let catalog = match Catalog::new(&catalog_config).await {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("Failed to connect to catalog: {}", e);

                let mut buf = BytesMut::with_capacity(1024);
                buf.put_u8(b'E');
                buf.put_i32(0);
                buf.put(&b"FATAL"[..]);
                buf.put_u8(0);
                let error_message = format!("Failed to connect to catalog: {}", e);
                buf.put(error_message.as_bytes());
                buf.put_u8(0);
                buf.put_u8(b'\0');

                socket.write_all(&buf).await?;
                socket.shutdown().await?;
                continue;
            }
        };

        let authenticator_ref = authenticator.make();
        let processor = Arc::new(MakeNexusBackend::new(catalog));
        let processor_ref = processor.make();
        tokio::task::Builder::new()
            .name("tcp connection handler")
            .spawn(async move {
                process_socket(
                    socket,
                    None,
                    authenticator_ref,
                    processor_ref.clone(),
                    processor_ref,
                )
                .await
            })?;
    }
}
