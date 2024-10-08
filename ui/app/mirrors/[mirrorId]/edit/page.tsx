'use client';

import { TableMapRow } from '@/app/dto/MirrorsDTO';
import { notifyErr } from '@/app/utils/notify';
import { CDCFlowConfigUpdate, FlowStatus } from '@/grpc_generated/flow';
import {
  FlowStateChangeRequest,
  MirrorStatusResponse,
} from '@/grpc_generated/route';
import { Button } from '@/lib/Button';
import { Label } from '@/lib/Label';
import { RowWithTextField } from '@/lib/Layout';
import { ProgressCircle } from '@/lib/ProgressCircle';
import { TextField } from '@/lib/TextField';
import { Callout } from '@tremor/react';
import { useRouter } from 'next/navigation';
import { useCallback, useEffect, useMemo, useState } from 'react';
import { ToastContainer } from 'react-toastify';
import 'react-toastify/dist/ReactToastify.css';
import TableMapping from '../../create/cdc/tablemapping';
import { reformattedTableMapping } from '../../create/handlers';
import { blankCDCSetting } from '../../create/helpers/common';
import * as styles from '../../create/styles';
import { getMirrorState } from '../handlers';

type EditMirrorProps = {
  params: { mirrorId: string };
};

const EditMirror = ({ params: { mirrorId } }: EditMirrorProps) => {
  const defaultBatchSize = blankCDCSetting.maxBatchSize;
  const defaultIdleTimeout = blankCDCSetting.idleTimeoutSeconds;

  const [rows, setRows] = useState<TableMapRow[]>([]);
  const [loading, setLoading] = useState(false);
  const [mirrorState, setMirrorState] = useState<MirrorStatusResponse>();
  const [config, setConfig] = useState<CDCFlowConfigUpdate>({
    batchSize: defaultBatchSize,
    idleTimeout: defaultIdleTimeout,
    additionalTables: [],
    numberOfSyncs: 0,
  });
  const { push } = useRouter();

  const fetchStateAndUpdateDeps = useCallback(async () => {
    await getMirrorState(mirrorId).then((res) => {
      setMirrorState(res);

      setConfig({
        batchSize:
          (res as MirrorStatusResponse).cdcStatus?.config?.maxBatchSize ||
          defaultBatchSize,
        idleTimeout:
          (res as MirrorStatusResponse).cdcStatus?.config?.idleTimeoutSeconds ||
          defaultIdleTimeout,
        additionalTables: [],
        numberOfSyncs: 0,
      });
    });
  }, [mirrorId, defaultBatchSize, defaultIdleTimeout]);

  useEffect(() => {
    fetchStateAndUpdateDeps();
  }, [fetchStateAndUpdateDeps]);

  const omitAdditionalTablesMapping: Map<string, string[]> = useMemo(() => {
    const omitAdditionalTablesMapping: Map<string, string[]> = new Map();
    mirrorState?.cdcStatus?.config?.tableMappings.forEach((value) => {
      const sourceSchema = value.sourceTableIdentifier.split('.').at(0)!;
      const mapVal: string[] =
        omitAdditionalTablesMapping.get(sourceSchema) || [];
      // needs to be schema qualified
      mapVal.push(value.sourceTableIdentifier);
      omitAdditionalTablesMapping.set(sourceSchema, mapVal);
    });
    return omitAdditionalTablesMapping;
  }, [mirrorState]);

  const additionalTables = useMemo(() => reformattedTableMapping(rows), [rows]);

  if (!mirrorState) {
    return <ProgressCircle variant='determinate_progress_circle' />;
  }

  const sendFlowStateChangeRequest = async () => {
    setLoading(true);
    const req: FlowStateChangeRequest = {
      flowJobName: mirrorId,
      requestedFlowState: FlowStatus.STATUS_UNKNOWN,
      flowConfigUpdate: {
        cdcFlowConfigUpdate: { ...config, additionalTables },
      },
      dropMirrorStats: false,
    };
    const res = await fetch('/api/v1/mirrors/state_change', {
      method: 'POST',
      body: JSON.stringify(req),
      cache: 'no-store',
    });
    if (res.ok) {
      push(`/mirrors/${mirrorId}`);
    } else {
      notifyErr(`Something went wrong: ${res.statusText}`);
      setLoading(false);
    }
  };

  const isNotPaused =
    mirrorState.currentFlowState.toString() !==
    FlowStatus[FlowStatus.STATUS_PAUSED];

  return (
    <div>
      <RowWithTextField
        key={1}
        label={<Label>{'Pull Batch Size'} </Label>}
        action={
          <div
            style={{
              display: 'flex',
              flexDirection: 'row',
              alignItems: 'center',
            }}
          >
            <TextField
              variant='simple'
              type={'number'}
              onChange={(e: React.ChangeEvent<HTMLInputElement>) =>
                setConfig({
                  ...config,
                  batchSize: e.target.valueAsNumber,
                })
              }
              defaultValue={config.batchSize}
            />
          </div>
        }
      />

      <RowWithTextField
        key={2}
        label={<Label>{'Sync Interval (Seconds)'} </Label>}
        action={
          <div
            style={{
              display: 'flex',
              flexDirection: 'row',
              alignItems: 'center',
            }}
          >
            <TextField
              variant='simple'
              type={'number'}
              onChange={(e: React.ChangeEvent<HTMLInputElement>) =>
                setConfig({
                  ...config,
                  idleTimeout: e.target.valueAsNumber,
                })
              }
              defaultValue={config.idleTimeout}
            />
          </div>
        }
      />

      <Label variant='action' as='label' style={{ marginTop: '1rem' }}>
        Adding Tables
      </Label>
      {!isNotPaused && rows.some((row) => row.selected) && (
        <Callout
          title='Note on adding tables'
          color={'gray'}
          style={{ marginTop: '1rem' }}
        >
          CDC will be put on hold until initial load for these added tables have
          been completed.
          <br></br>
          The <b>replication slot will grow</b> during this period.
          <br></br>
          For custom publications, ensure that the tables are part of the
          publication you provided. This can be done with ALTER PUBLICATION
          pubname ADD TABLE table1, table2;
        </Callout>
      )}

      <TableMapping
        sourcePeerName={mirrorState.cdcStatus?.config?.sourceName ?? ''}
        peerType={mirrorState.cdcStatus?.destinationType}
        rows={rows}
        setRows={setRows}
        omitAdditionalTablesMapping={omitAdditionalTablesMapping}
        initialLoadOnly={false}
      />

      {isNotPaused && (
        <Callout title='' color={'rose'} style={{ marginTop: '1rem' }}>
          Mirror can only be edited while paused.
        </Callout>
      )}

      <div style={styles.MirrorButtonContainer}>
        <Button
          style={styles.MirrorButtonStyle}
          onClick={() => {
            push(`/mirrors/${mirrorId}`);
          }}
        >
          Back
        </Button>
        <Button
          style={styles.MirrorButtonStyle}
          variant='normalSolid'
          disabled={loading || isNotPaused}
          onClick={sendFlowStateChangeRequest}
        >
          {loading ? (
            <ProgressCircle variant='determinate_progress_circle' />
          ) : (
            'Edit Mirror'
          )}
        </Button>
      </div>
      <ToastContainer />
    </div>
  );
};

export default EditMirror;
