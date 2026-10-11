import React from 'react';
import { QueryEditorProps } from '@grafana/data';
import { CodeEditor, Field } from '@grafana/ui';

import type { DataSource } from '../datasource';
import { LANGUAGE, registerFenecQL } from '../fenecql/language';
import { FenecOptions, FenecQuery } from '../types';

type Props = QueryEditorProps<DataSource, FenecQuery, FenecOptions>;

/** A query variable's FenecQL: its first column is the variable's values. */
export function VariableQueryEditor({ query, onChange, onRunQuery }: Props) {
  const save = (text: string) => {
    onChange({ ...query, refId: query.refId ?? 'variable', queryText: text });
    onRunQuery();
  };
  return (
    <Field
      label="FenecQL"
      description="The first column's values, or the __text and __value columns: get ticks select sym, count(*) as n group sym order sym"
    >
      <CodeEditor
        language={LANGUAGE}
        value={query.queryText ?? ''}
        height={90}
        showLineNumbers={false}
        showMiniMap={false}
        onBeforeEditorMount={registerFenecQL}
        onBlur={save}
        onSave={save}
        monacoOptions={{ wordWrap: 'on' }}
      />
    </Field>
  );
}
