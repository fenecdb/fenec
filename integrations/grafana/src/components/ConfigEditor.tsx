import React, { ChangeEvent } from 'react';
import { DataSourcePluginOptionsEditorProps } from '@grafana/data';
import { Field, FieldSet, Input, SecretInput } from '@grafana/ui';

import { FenecOptions, FenecSecureData } from '../types';

type Props = DataSourcePluginOptionsEditorProps<FenecOptions, FenecSecureData>;

/**
 * The server, the token and the tenant: every request goes to this URL and
 * a path the plugin names, so these are the only places a request's address
 * comes from. The token is write-only here: Grafana keeps it encrypted and
 * hands it to the backend alone.
 */
export function ConfigEditor({ options, onOptionsChange }: Props) {
  const { jsonData, secureJsonFields } = options;
  const setJson = (patch: Partial<FenecOptions>) => onOptionsChange({ ...options, jsonData: { ...jsonData, ...patch } });
  const number = (e: ChangeEvent<HTMLInputElement>) => (e.target.value === '' ? undefined : Number(e.target.value));

  return (
    <FieldSet label="fenec-server">
      <Field
        label="URL"
        description="The server's address, as Grafana's server reaches it: http://fenecdb:8080. Queries never change it."
        required
      >
        <Input
          id="fenecdb-url"
          width={50}
          value={options.url}
          placeholder="http://127.0.0.1:8080"
          onChange={(e: ChangeEvent<HTMLInputElement>) => onOptionsChange({ ...options, url: e.target.value.trim() })}
        />
      </Field>
      <Field
        label="Token"
        description="The server's --http-token, or a JSON Web Token whose policy reads what the dashboards show and writes nothing."
      >
        <SecretInput
          id="fenecdb-token"
          width={50}
          isConfigured={!!secureJsonFields?.token}
          value={options.secureJsonData?.token ?? ''}
          placeholder="a token"
          onReset={() =>
            onOptionsChange({
              ...options,
              secureJsonFields: { ...secureJsonFields, token: false },
              secureJsonData: { ...options.secureJsonData, token: '' },
            })
          }
          onChange={(e: ChangeEvent<HTMLInputElement>) =>
            onOptionsChange({ ...options, secureJsonData: { ...options.secureJsonData, token: e.target.value } })
          }
        />
      </Field>
      <Field label="Tenant" description="A tenant of a node or a router (--dir, fenec-shard): requests go to /t/<tenant>/.">
        <Input
          id="fenecdb-tenant"
          width={30}
          value={jsonData.tenant ?? ''}
          placeholder="none"
          onChange={(e: ChangeEvent<HTMLInputElement>) => setJson({ tenant: e.target.value.trim() || undefined })}
        />
      </Field>
      <Field
        label="Wait (ms)"
        description="How long a query sent after a change (a query's After) waits for it on a replica: Fenec-Wait, 5000 unless given, 30000 at most."
      >
        <Input
          id="fenecdb-wait"
          type="number"
          min={0}
          width={20}
          value={jsonData.waitMs ?? ''}
          placeholder="5000"
          onChange={(e: ChangeEvent<HTMLInputElement>) => setJson({ waitMs: number(e) })}
        />
      </Field>
      <Field label="Timeout (s)" description="Each request's bound, 30 unless given.">
        <Input
          id="fenecdb-timeout"
          type="number"
          min={0}
          width={20}
          value={jsonData.timeoutSeconds ?? ''}
          placeholder="30"
          onChange={(e: ChangeEvent<HTMLInputElement>) => setJson({ timeoutSeconds: number(e) })}
        />
      </Field>
    </FieldSet>
  );
}
