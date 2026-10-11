// fenecdb-datasource's backend: Grafana starts it as a process of its own
// and sends it every query, health check and resource call, from a
// dashboard's browser and from an alert rule alike.
package main

import (
	"os"

	"github.com/grafana/grafana-plugin-sdk-go/backend/datasource"
	"github.com/grafana/grafana-plugin-sdk-go/backend/log"

	"github.com/fenecdb/fenec/integrations/grafana/pkg/plugin"
)

func main() {
	// One instance a data source, made again when its settings change.
	if err := datasource.Manage(plugin.ID, plugin.NewDatasource, datasource.ManageOpts{}); err != nil {
		log.DefaultLogger.Error(err.Error())
		os.Exit(1)
	}
}
