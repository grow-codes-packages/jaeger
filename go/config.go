package jaeger

import (
	"fmt"
	"net/url"
)

// DefaultURL is the default Jaeger query endpoint (UI and HTTP query API, port 16686).
const DefaultURL = "http://127.0.0.1:16686"

// DefaultSamplingURL is the default Jaeger remote sampling endpoint (port 5778).
const DefaultSamplingURL = "http://127.0.0.1:5778/sampling"

// vendorEnv lists the vendor variables consulted after GROW_JAEGER_*, in order.
var vendorEnv = VendorEnv{URL: []string{"JAEGER_QUERY_URL"}}

// vendorTokenEnv lists vendor variables for the token, in precedence order.
var vendorTokenEnv []string

// vendorSamplingEnv lists vendor variables for the sampling URL (the legacy Jaeger
// client name).
var vendorSamplingEnv = []string{"JAEGER_SAMPLING_ENDPOINT"}

// Config configures a Client. Build it with a struct literal or ConfigFromEnv;
// zero values mean "use the default". Call Validate to check it.
//
// Environment variables read by ConfigFromEnv (explicit value > GROW_JAEGER_* >
// vendor variables > default):
//
//	URL                  GROW_JAEGER_URL, JAEGER_QUERY_URL (default http://127.0.0.1:16686)
//	Token                GROW_JAEGER_TOKEN
//	SamplingURL          GROW_JAEGER_SAMPLING_URL, JAEGER_SAMPLING_ENDPOINT (default http://127.0.0.1:5778/sampling)
//	HealthURL            GROW_JAEGER_HEALTH_URL (optional)
//	MetricsURL           GROW_JAEGER_METRICS_URL (optional)
//
// plus the common GROW_JAEGER_CONNECT_TIMEOUT, REQUEST_TIMEOUT, TOTAL_TIMEOUT,
// MAX_RETRIES, CA_CERT, CLIENT_CERT, CLIENT_KEY, INSECURE_SKIP_VERIFY and PROXY.
// The span exporter has its own settings, see TracingConfigFromEnv.
type Config struct {
	CommonConfig

	// Token is sent as "Authorization: Bearer <token>" on query and sampling requests
	// (env GROW_JAEGER_TOKEN). Stock Jaeger has no auth; use it behind a reverse proxy
	// or with bearer token propagation.
	Token Secret
	// SamplingURL is the remote sampling endpoint, ending in /sampling.
	SamplingURL string
	// HealthURL is the optional healthcheckv2 endpoint (port 13133, /status). When set,
	// Health adds its result to the query root check.
	HealthURL string
	// MetricsURL is the optional Prometheus endpoint (port 8888, /metrics). When set,
	// ServerVersion falls back to its target_info metric.
	MetricsURL string
}

// ConfigFromEnv reads the configuration from the process environment.
func ConfigFromEnv() (Config, error) { return ConfigFromEnvWith(Config{}, nil) }

// ConfigFromEnvWith resolves explicit values > GROW_JAEGER_* > vendor
// variables > defaults. env replaces the process environment when non-nil.
func ConfigFromEnvWith(explicit Config, env map[string]string) (Config, error) {
	r := NewEnvReader(EnvPrefix, env)
	c := explicit
	if err := c.applyEnv(r, vendorEnv); err != nil {
		return Config{}, err
	}
	if c.Token.IsZero() {
		if v, ok := r.String("TOKEN", vendorTokenEnv...); ok {
			c.Token = NewSecret(v)
		}
	}
	if c.SamplingURL == "" {
		c.SamplingURL, _ = r.String("SAMPLING_URL", vendorSamplingEnv...)
	}
	if c.HealthURL == "" {
		c.HealthURL, _ = r.String("HEALTH_URL")
	}
	if c.MetricsURL == "" {
		c.MetricsURL, _ = r.String("METRICS_URL")
	}
	c = c.withServiceDefaults()
	if err := c.Validate(); err != nil {
		return Config{}, err
	}
	return c, nil
}

// withServiceDefaults applies the common and the Jaeger defaults.
func (c Config) withServiceDefaults() Config {
	c.CommonConfig = c.withDefaults(DefaultURL)
	if c.SamplingURL == "" {
		c.SamplingURL = DefaultSamplingURL
	}
	return c
}

// Validate checks the configuration and returns a CONFIG error.
func (c Config) Validate() error {
	if err := c.withDefaults(DefaultURL).validate(); err != nil {
		return err
	}
	for _, u := range []struct{ name, value string }{
		{"samplingUrl", c.SamplingURL}, {"healthUrl", c.HealthURL}, {"metricsUrl", c.MetricsURL},
	} {
		if u.value != "" && !isHTTPURL(u.value) {
			return ConfigError(u.name + " must be an absolute http or https URL")
		}
	}
	return nil
}

func isHTTPURL(s string) bool {
	u, err := url.Parse(s)
	return err == nil && (u.Scheme == "http" || u.Scheme == "https") && u.Host != ""
}

// String renders the configuration with every credential redacted.
func (c Config) String() string {
	return fmt.Sprintf("Config{%s Token:%s SamplingURL:%s HealthURL:%s MetricsURL:%s}",
		c.describe(), c.Token, RedactURL(c.SamplingURL), RedactURL(c.HealthURL), RedactURL(c.MetricsURL))
}

// GoString renders the configuration with every credential redacted.
func (c Config) GoString() string { return c.String() }
