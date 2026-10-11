import { DataSourcePlugin } from '@grafana/data';

import { ConfigEditor } from './components/ConfigEditor';
import { QueryEditor } from './components/QueryEditor';
import { DataSource } from './datasource';
import { FenecOptions, FenecQuery, FenecSecureData } from './types';

export const plugin = new DataSourcePlugin<DataSource, FenecQuery, FenecOptions, FenecSecureData>(DataSource)
  .setConfigEditor(ConfigEditor)
  .setQueryEditor(QueryEditor);
