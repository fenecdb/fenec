import React, { ChangeEvent, useState } from 'react';
import { css } from '@emotion/css';
import { GrafanaTheme2, QueryEditorProps, SelectableValue } from '@grafana/data';
import { getTemplateSrv } from '@grafana/runtime';
import {
  Alert,
  Button,
  CodeEditor,
  CodeEditorSuggestionItem,
  Combobox,
  ComboboxOption,
  CodeEditorSuggestionItemKind,
  IconButton,
  InlineField,
  Input,
  RadioButtonGroup,
  Stack,
  useStyles2,
} from '@grafana/ui';

import { DataSource } from '../datasource';
import { BUILTINS, LANGUAGE, registerFenecQL } from '../fenecql/language';
import { Explained, FenecOptions, FenecQuery, Format, Param, PARAM_TYPES, ParamType } from '../types';

type Props = QueryEditorProps<DataSource, FenecQuery, FenecOptions>;

const FORMATS: Array<SelectableValue<Format>> = [
  { label: 'Auto', value: 'auto', description: 'A time series when the statement groups by its first column, a time' },
  { label: 'Table', value: 'table' },
  { label: 'Time series', value: 'time_series', description: 'The first column a time; text columns are labels' },
];

const TYPES: Array<ComboboxOption<ParamType>> = PARAM_TYPES.map((t) => ({ label: t, value: t }));

/** Completions for the names the backend binds: Grafana's and the dashboard's. */
function suggestions(): CodeEditorSuggestionItem[] {
  const builtins = BUILTINS.map((name) => ({
    label: '$' + name,
    kind: CodeEditorSuggestionItemKind.Constant,
    detail: 'milliseconds, from the request',
  }));
  const vars = getTemplateSrv()
    .getVariables()
    .map((v) => ({ label: '$' + v.name, kind: CodeEditorSuggestionItemKind.Field, detail: 'a dashboard variable, bound as a value' }));
  return [...builtins, ...vars];
}

export function QueryEditor({ datasource, query, onChange, onRunQuery, range, data }: Props) {
  const styles = useStyles2(getStyles);
  const [explained, setExplained] = useState<Explained | undefined>();
  const [explainError, setExplainError] = useState<string | undefined>();
  const [explaining, setExplaining] = useState(false);
  const params = query.params ?? [];
  const lines = (query.queryText ?? '').split('\n').length;

  const setParams = (next: Param[]) => onChange({ ...query, params: next });
  const setParam = (i: number, patch: Partial<Param>) => setParams(params.map((p, j) => (j === i ? { ...p, ...patch } : p)));

  const explain = async () => {
    if (explained || explainError) {
      setExplained(undefined);
      setExplainError(undefined);
      return;
    }
    setExplaining(true);
    try {
      const at = range ?? data?.timeRange;
      if (!at) {
        throw new Error('no time range to bind $__from and $__to to');
      }
      setExplained(await datasource.explain(query, at, data?.request?.intervalMs ?? 1000, data?.request?.maxDataPoints ?? 1000));
    } catch (e: unknown) {
      const err = e as { data?: { error?: string }; message?: string };
      setExplainError(err?.data?.error ?? err?.message ?? String(e));
    } finally {
      setExplaining(false);
    }
  };

  return (
    <Stack direction="column" gap={1}>
      <CodeEditor
        language={LANGUAGE}
        value={query.queryText ?? ''}
        height={Math.min(Math.max(lines, 4), 20) * 19 + 12}
        showLineNumbers={true}
        showMiniMap={false}
        onBeforeEditorMount={registerFenecQL}
        getSuggestions={suggestions}
        onChange={(text) => onChange({ ...query, queryText: text })}
        onBlur={(text) => {
          onChange({ ...query, queryText: text });
          onRunQuery();
        }}
        onSave={(text) => {
          onChange({ ...query, queryText: text });
          onRunQuery();
        }}
        monacoOptions={{ wordWrap: 'on', scrollBeyondLastLine: false }}
      />
      <Stack direction="row" gap={1} alignItems="center" wrap="wrap">
        <InlineField label="Format" tooltip="Auto makes a time series of a statement grouped by its first column, a time">
          <RadioButtonGroup<Format>
            options={FORMATS}
            value={query.format ?? 'auto'}
            onChange={(format) => {
              onChange({ ...query, format });
              onRunQuery();
            }}
          />
        </InlineField>
        <InlineField label="After" tooltip="A change the server must hold before it answers (Fenec-After): a number, or $variable">
          <Input
            width={14}
            value={query.after ?? ''}
            placeholder="a change"
            onChange={(e: ChangeEvent<HTMLInputElement>) => onChange({ ...query, after: e.target.value || undefined })}
            onBlur={onRunQuery}
          />
        </InlineField>
        <Button icon="play" size="sm" onClick={onRunQuery}>
          Run
        </Button>
        <Button icon="info-circle" size="sm" variant="secondary" onClick={explain} disabled={explaining}>
          {explained || explainError ? 'Hide plan' : 'Explain'}
        </Button>
      </Stack>
      <div className={styles.params}>
        {params.map((p, i) => (
          <Stack key={i} direction="row" gap={1} alignItems="center">
            <span className={styles.slot}>${i + 1}</span>
            <Combobox<ParamType>
              width={14}
              options={TYPES}
              value={p.type ?? 'text'}
              onChange={(v) => setParam(i, { type: v.value })}
            />
            <Input
              width={40}
              value={p.value}
              placeholder="a value, or $variable"
              onChange={(e: ChangeEvent<HTMLInputElement>) => setParam(i, { value: e.target.value })}
              onBlur={onRunQuery}
            />
            <IconButton
              name="trash-alt"
              tooltip={`Remove $${i + 1}`}
              onClick={() => {
                setParams(params.filter((_, j) => j !== i));
                onRunQuery();
              }}
            />
          </Stack>
        ))}
        <Button
          icon="plus"
          size="sm"
          variant="secondary"
          fill="text"
          onClick={() => setParams([...params, { type: 'text', value: '' }])}
        >
          Parameter ${params.length + 1}
        </Button>
      </div>
      <div className={styles.help}>
        Every value is a parameter, never text: <code>$__from</code> and <code>$__to</code> (ms),{' '}
        <code>$__interval_ms</code>, <code>$__max_data_points</code>; a dashboard variable as <code>$sym</code> or{' '}
        <code>{'${n:int}'}</code>; a multi-value one after <code>in</code>: <code>where sym in $syms</code>.
      </div>
      {explainError && (
        <Alert severity="error" title="Explain" onRemove={() => setExplainError(undefined)}>
          {explainError}
        </Alert>
      )}
      {explained && (
        <pre className={styles.plan} aria-label="The plan">
          {explained.statement}
          {'\n\n'}
          {explained.plan.join('\n')}
        </pre>
      )}
    </Stack>
  );
}

const getStyles = (theme: GrafanaTheme2) => ({
  params: css({ display: 'flex', flexDirection: 'column', gap: theme.spacing(0.5) }),
  slot: css({ fontFamily: theme.typography.fontFamilyMonospace, color: theme.colors.text.secondary, minWidth: theme.spacing(3) }),
  help: css({ color: theme.colors.text.secondary, fontSize: theme.typography.bodySmall.fontSize }),
  plan: css({ margin: 0, whiteSpace: 'pre-wrap', fontSize: theme.typography.bodySmall.fontSize }),
});
