package jaeger

import (
	"context"
	"encoding/json"
	"net/http"
	"net/url"
	"regexp"
	"strings"
	"sync"
	"time"
)

// Client is the Jaeger client: health, version, the query API (Query) and remote
// sampling (SamplingStrategy, NewRemoteSampler). It is safe for concurrent use.
type Client struct {
	cfg       Config
	transport *httpTransport
	sampling  *endpoint
	health    *endpoint
	metrics   *endpoint
	query     *QueryClient

	mu       sync.Mutex
	samplers []*RemoteSampler
	handles  []*TracingSetup
	closed   bool

	closeOnce sync.Once
}

// endpoint is a transport bound to an absolute URL (sampling, health, metrics).
type endpoint struct {
	t     *httpTransport
	path  string
	query url.Values
}

// newEndpoint builds a transport for the absolute URL raw with the common settings of cfg.
func newEndpoint(cfg CommonConfig, raw string, secrets ...string) (*endpoint, error) {
	u, err := url.Parse(raw)
	if err != nil || u.Host == "" {
		return nil, ConfigError("endpoint url is invalid")
	}
	cfg.URL = u.Scheme + "://" + u.Host
	t, err := newHTTPTransport(cfg)
	if err != nil {
		return nil, err
	}
	t.addSecret(secrets...)
	return &endpoint{t: t, path: u.Path, query: u.Query()}, nil
}

// request returns a GET request for the endpoint with extra query values.
func (e *endpoint) request(operation string, header http.Header, extra url.Values) *request {
	q := url.Values{}
	for k, v := range e.query {
		q[k] = append([]string(nil), v...)
	}
	for k, v := range extra {
		q[k] = append([]string(nil), v...)
	}
	return &request{Method: http.MethodGet, Path: e.path, Query: q, Header: header, Operation: operation}
}

func (e *endpoint) close() {
	if e != nil {
		e.t.close()
	}
}

// NewClient validates cfg and returns a client. It makes no network call.
func NewClient(cfg Config) (*Client, error) {
	if err := cfg.Validate(); err != nil {
		return nil, err
	}
	cfg = cfg.withServiceDefaults()
	t, err := newHTTPTransport(cfg.CommonConfig)
	if err != nil {
		return nil, err
	}
	token := cfg.Token.Reveal()
	t.addSecret(token)
	c := &Client{cfg: cfg, transport: t}
	c.query = &QueryClient{t: t, header: c.authHeader, now: time.Now}
	if c.sampling, err = newEndpoint(cfg.CommonConfig, cfg.SamplingURL, token); err != nil {
		return nil, err
	}
	if cfg.HealthURL != "" {
		if c.health, err = newEndpoint(cfg.CommonConfig, cfg.HealthURL, token); err != nil {
			return nil, err
		}
	}
	if cfg.MetricsURL != "" {
		if c.metrics, err = newEndpoint(cfg.CommonConfig, cfg.MetricsURL, token); err != nil {
			return nil, err
		}
	}
	return c, nil
}

// NewClientFromEnv builds a client from the environment (see ConfigFromEnv).
func NewClientFromEnv() (*Client, error) {
	cfg, err := ConfigFromEnv()
	if err != nil {
		return nil, err
	}
	return NewClient(cfg)
}

// Config returns the effective configuration (defaults applied).
func (c *Client) Config() Config { return c.cfg }

// Query returns the query client for the Jaeger HTTP query API. It implements
// TraceQueryClient.
func (c *Client) Query() *QueryClient { return c.query }

func (c *Client) authHeader() http.Header {
	h := http.Header{}
	if !c.cfg.Token.IsZero() {
		h.Set("Authorization", "Bearer "+c.cfg.Token.Reveal())
	}
	return h
}

// Health checks liveness without credentials: GET on the query root (the UI, which is
// also the stack catalog health target). When Config.HealthURL is set, the
// healthcheckv2 result is added to Details with the prefix "healthcheck." and an
// unhealthy result there turns the status into "degraded". It reports an
// unreachable or unhealthy service in the result; it fails only for CANCELLED and
// CONFIG.
func (c *Client) Health(ctx context.Context) (HealthStatus, error) {
	hs, err := c.transport.checkHealth(ctx, "health", &request{
		Method: http.MethodGet, Path: "/", Header: http.Header{"Accept": {"text/html"}},
	})
	if err != nil || !hs.Healthy || c.health == nil {
		return hs, err
	}
	detail, err := c.health.t.checkHealth(ctx, "health", c.health.request("health", nil, nil))
	if err != nil {
		return HealthStatus{}, err
	}
	hs.Details["healthcheck.status"] = detail.Status
	for k, v := range detail.Details {
		hs.Details["healthcheck."+k] = v
	}
	if !detail.Healthy {
		hs.Healthy = false
		hs.Status = "degraded"
	}
	return hs, nil
}

// Ping makes an authenticated round trip (GET /api/v3/services) and returns its
// latency (AUTHENTICATION error on bad credentials behind an auth proxy).
func (c *Client) Ping(ctx context.Context) (time.Duration, error) {
	start := time.Now()
	_, err := c.transport.do(ctx, &request{Method: http.MethodGet, Path: "/api/v3/services", Header: c.authHeader(), Operation: "ping"})
	if err != nil {
		return 0, err
	}
	return time.Since(start), nil
}

var (
	jaegerVersionRe = regexp.MustCompile(`JAEGER_VERSION\s*=\s*(\{[^;]*\})\s*;`)
	targetInfoRe    = regexp.MustCompile(`target_info\{[^}]*service_version="([^"]+)"`)
)

// BuildInfo is the build information embedded in the Jaeger UI page (JAEGER_VERSION).
type BuildInfo struct {
	// GitVersion is for example "v2.23.0".
	GitVersion string `json:"gitVersion"`
	// GitCommit is the commit the server was built from.
	GitCommit string `json:"gitCommit"`
	// BuildDate is the build date.
	BuildDate string `json:"buildDate"`
}

// BuildInfo returns the build information parsed from the UI page (GET /), or nil
// when the page has none or the UI is disabled (404).
func (c *Client) BuildInfo(ctx context.Context) (*BuildInfo, error) {
	res, err := c.transport.do(ctx, &request{
		Method: http.MethodGet, Path: "/", Header: mergeHeader(c.authHeader(), "Accept", "text/html"), Operation: "serverVersion",
	})
	if err != nil {
		if CodeOf(err) == ErrorCodeNotFound {
			return nil, nil
		}
		return nil, err
	}
	m := jaegerVersionRe.FindSubmatch(res.Body)
	if m == nil {
		return nil, nil
	}
	var raw map[string]any
	_ = json.Unmarshal(m[1], &raw)
	text := func(k string) string { v, _ := raw[k].(string); return v }
	if raw == nil {
		return nil, nil
	}
	return &BuildInfo{GitVersion: text("gitVersion"), GitCommit: text("gitCommit"), BuildDate: text("buildDate")}, nil
}

// ServerVersion returns the Jaeger version without the leading "v" (for example
// "2.23.0"). It reads JAEGER_VERSION from the UI index page and falls back to the
// target_info metric of Config.MetricsURL. UNSUPPORTED when neither exposes a version
// (for example when the UI is disabled).
func (c *Client) ServerVersion(ctx context.Context) (string, error) {
	const op = "serverVersion"
	res, err := c.transport.do(ctx, &request{
		Method: http.MethodGet, Path: "/", Header: mergeHeader(c.authHeader(), "Accept", "text/html"), Operation: op,
	})
	if err != nil && CodeOf(err) != ErrorCodeNotFound {
		return "", err
	}
	if err == nil {
		if m := jaegerVersionRe.FindSubmatch(res.Body); m != nil {
			var info struct {
				GitVersion string `json:"gitVersion"`
			}
			if json.Unmarshal(m[1], &info) == nil && info.GitVersion != "" {
				return strings.TrimPrefix(info.GitVersion, "v"), nil
			}
		}
	}
	if c.metrics != nil {
		mres, merr := c.metrics.t.do(ctx, c.metrics.request(op, mergeHeader(c.authHeader(), "Accept", "text/plain"), nil))
		if merr != nil && CodeOf(merr) != ErrorCodeNotFound {
			return "", merr
		}
		if merr == nil {
			if m := targetInfoRe.FindSubmatch(mres.Body); m != nil {
				return strings.TrimPrefix(string(m[1]), "v"), nil
			}
		}
	}
	return "", UnsupportedError(op, "the server exposes no version")
}

func mergeHeader(h http.Header, key, value string) http.Header {
	h.Set(key, value)
	return h
}

// SamplingStrategy fetches the remote sampling strategy of service from
// Config.SamplingURL (GET <samplingUrl>?service=<service>).
func (c *Client) SamplingStrategy(ctx context.Context, service string) (SamplingStrategy, error) {
	return fetchStrategy(ctx, c.sampling, c.authHeader(), service)
}

// NewRemoteSampler returns a RemoteSampler for service that polls Config.SamplingURL
// every pollInterval (0 = DefaultSamplingPollInterval) with this client's transport
// settings. Close stops it; Client.Close stops every sampler created here.
func (c *Client) NewRemoteSampler(service string, pollInterval time.Duration, opts ...RemoteSamplerOption) (*RemoteSampler, error) {
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.closed {
		return nil, &Error{Code: ErrorCodeCancelled, Message: "sampling: client is closed"}
	}
	s, err := newRemoteSampler(c.sampling, c.authHeader(), false, service, pollInterval, opts)
	if err != nil {
		return nil, err
	}
	c.samplers = append(c.samplers, s)
	return s, nil
}

// InitTracing starts OpenTelemetry tracing to Jaeger (see the package-level
// InitTracing); a remote sampler started for OTEL_TRACES_SAMPLER=jaeger_remote uses
// this client's configuration (WithSamplerConfig given in opts wins). Close shuts the
// handle down.
func (c *Client) InitTracing(ctx context.Context, cfg TracingConfig, opts ...TracingOption) (*TracingSetup, error) {
	c.mu.Lock()
	defer c.mu.Unlock()
	if c.closed {
		return nil, &Error{Code: ErrorCodeCancelled, Message: "initTracing: client is closed"}
	}
	h, err := InitTracing(ctx, cfg, append([]TracingOption{WithSamplerConfig(c.cfg)}, opts...)...)
	if err != nil {
		return nil, err
	}
	c.handles = append(c.handles, h)
	return h, nil
}

// Close shuts down the tracing handles (flushing them) and stops the remote samplers
// created by the client, then releases its connections. It is idempotent.
func (c *Client) Close() error {
	c.closeOnce.Do(func() {
		c.mu.Lock()
		c.closed = true
		samplers, handles := c.samplers, c.handles
		c.samplers, c.handles = nil, nil
		c.mu.Unlock()
		for _, h := range handles {
			_ = h.Shutdown(context.Background())
		}
		for _, s := range samplers {
			_ = s.Close()
		}
		c.transport.close()
		c.sampling.close()
		c.health.close()
		c.metrics.close()
	})
	return nil
}
