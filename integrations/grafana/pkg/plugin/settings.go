package plugin

import (
	"encoding/json"
	"fmt"
	"net/url"
	"strings"
	"time"

	"github.com/grafana/grafana-plugin-sdk-go/backend"
)

// ID is the plugin's id, as plugin.json and Grafana's
// GF_PLUGINS_ALLOW_LOADING_UNSIGNED_PLUGINS name it.
const ID = "fenecdb-datasource"

// Settings are a data source's: the server's URL from the data source's
// own URL field, the rest from its JSON data, the token from its secure
// JSON data -- which Grafana keeps encrypted and never sends to a browser.
type Settings struct {
	URL    string
	Tenant string
	// Wait is how long the server holds a query sent after a change it has
	// not reached (Fenec-Wait); zero leaves the server's five seconds.
	Wait time.Duration
	// Timeout bounds each request; zero is the client's 30 seconds.
	Timeout time.Duration
	Token   string
}

type jsonData struct {
	Tenant         string  `json:"tenant"`
	WaitMs         float64 `json:"waitMs"`
	TimeoutSeconds float64 `json:"timeoutSeconds"`
}

// LoadSettings reads and checks a data source's settings. The URL is the
// one thing every request is sent to, so it is held to a server's root or
// a prefix of a proxy: an http or https URL with a host, and nothing a
// query could take for another request -- no user, query or fragment.
func LoadSettings(s backend.DataSourceInstanceSettings) (Settings, error) {
	var jd jsonData
	if len(s.JSONData) > 0 {
		if err := json.Unmarshal(s.JSONData, &jd); err != nil {
			return Settings{}, fmt.Errorf("the data source's settings: %w", err)
		}
	}
	u, err := url.Parse(strings.TrimSpace(s.URL))
	if err != nil || (u.Scheme != "http" && u.Scheme != "https") || u.Host == "" {
		return Settings{}, fmt.Errorf("the data source's URL is not an http(s) URL with a host: %q", s.URL)
	}
	if u.User != nil || u.RawQuery != "" || u.Fragment != "" || u.Opaque != "" {
		return Settings{}, fmt.Errorf("the data source's URL holds a user, a query or a fragment: give the server's address alone")
	}
	if jd.WaitMs < 0 || jd.TimeoutSeconds < 0 {
		return Settings{}, fmt.Errorf("the data source's wait and timeout take no negative value")
	}
	return Settings{
		URL:     strings.TrimRight(u.String(), "/"),
		Tenant:  strings.TrimSpace(jd.Tenant),
		Wait:    time.Duration(jd.WaitMs * float64(time.Millisecond)),
		Timeout: time.Duration(jd.TimeoutSeconds * float64(time.Second)),
		Token:   s.DecryptedSecureJSONData["token"],
	}, nil
}
