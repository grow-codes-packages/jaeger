package jaeger

import (
	"errors"
	"fmt"
	"go.opentelemetry.io/otel/sdk/trace/tracetest"
	"net/http"
	"strings"
	"testing"
	"time"
)

// newTestClient builds a client for url with fast retries; mutate adjusts the config.
func newTestClient(t *testing.T, url string, mutate func(*Config)) *Client {
	t.Helper()
	cfg := Config{CommonConfig: CommonConfig{URL: url, Retry: fastRetry(3)}, Token: NewSecret(testToken)}
	if mutate != nil {
		mutate(&cfg)
	}
	c, err := NewClient(cfg)
	if err != nil {
		t.Fatalf("NewClient: %v", err)
	}
	t.Cleanup(func() { _ = c.Close() })
	return c
}

func TestClientConfig(t *testing.T) {
	t.Run("core.config.env-precedence", func(t *testing.T) {
		env := map[string]string{EnvPrefix + "URL": "http://grow:1", "JAEGER_QUERY_URL": "http://vendor:2", EnvPrefix + "TOKEN": "grow-token"}
		cfg, err := ConfigFromEnvWith(Config{}, env)
		if err != nil || cfg.URL != "http://grow:1" || cfg.Token.Reveal() != "grow-token" {
			t.Fatalf("env: %v %s", err, cfg)
		}
		delete(env, EnvPrefix+"URL")
		if cfg, _ = ConfigFromEnvWith(Config{}, env); cfg.URL != "http://vendor:2" {
			t.Fatalf("JAEGER_QUERY_URL: %s", cfg.URL)
		}
		cfg, _ = ConfigFromEnvWith(Config{CommonConfig: CommonConfig{URL: "https://explicit"}, Token: NewSecret("explicit")}, env)
		if cfg.URL != "https://explicit" || cfg.Token.Reveal() != "explicit" {
			t.Fatal("explicit must win")
		}
		cfg, _ = ConfigFromEnvWith(Config{}, map[string]string{})
		if cfg.URL != DefaultURL || cfg.SamplingURL != DefaultSamplingURL || cfg.HealthURL != "" || cfg.RequestTimeout != DefaultRequestTimeout {
			t.Fatalf("defaults: %s", cfg)
		}
		_, err = ConfigFromEnvWith(Config{}, map[string]string{EnvPrefix + "TOTAL_TIMEOUT": "forever-xyz"})
		if !errors.Is(err, ErrConfig) || !strings.Contains(err.Error(), EnvPrefix+"TOTAL_TIMEOUT") || strings.Contains(err.Error(), "forever-xyz") {
			t.Fatalf("invalid value: %v", err)
		}
		if _, err := ConfigFromEnvWith(Config{}, map[string]string{EnvPrefix + "URL": "ftp://nope"}); !errors.Is(err, ErrConfig) {
			t.Fatalf("invalid url: %v", err)
		}
		t.Setenv(EnvPrefix+"URL", "http://process-env:9")
		if cfg, err := ConfigFromEnv(); err != nil || cfg.URL != "http://process-env:9" {
			t.Fatalf("process env: %v %s", err, cfg.URL)
		}
	})

	t.Run("service urls", func(t *testing.T) {
		env := map[string]string{
			EnvPrefix + "SAMPLING_URL": "http://grow-s:5778/sampling", "JAEGER_SAMPLING_ENDPOINT": "http://vendor-s:5778/sampling",
			EnvPrefix + "HEALTH_URL": "http://h:13133/status", EnvPrefix + "METRICS_URL": "http://h:8888/metrics",
		}
		cfg, err := ConfigFromEnvWith(Config{}, env)
		if err != nil || cfg.SamplingURL != "http://grow-s:5778/sampling" || cfg.HealthURL != "http://h:13133/status" || cfg.MetricsURL != "http://h:8888/metrics" {
			t.Fatalf("%v %s", err, cfg)
		}
		delete(env, EnvPrefix+"SAMPLING_URL")
		if cfg, _ = ConfigFromEnvWith(Config{}, env); cfg.SamplingURL != "http://vendor-s:5778/sampling" {
			t.Fatalf("vendor sampling: %s", cfg.SamplingURL)
		}
		for _, bad := range []Config{{SamplingURL: "nope"}, {HealthURL: "ftp://x"}, {MetricsURL: "http://"}} {
			if err := bad.Validate(); !errors.Is(err, ErrConfig) {
				t.Fatalf("%+v: %v", bad, err)
			}
		}
		if _, err := ConfigFromEnvWith(Config{}, map[string]string{EnvPrefix + "HEALTH_URL": "nope"}); !errors.Is(err, ErrConfig) {
			t.Fatalf("bad health url: %v", err)
		}
	})

	t.Run("core.redaction.config", func(t *testing.T) {
		cfg := Config{
			CommonConfig: CommonConfig{URL: "https://h", Headers: map[string]string{"X-Api-Key": "hdr-xyz"}},
			Token:        NewSecret("tok-xyz"),
			SamplingURL:  "https://s/sampling?token=q-xyz",
		}
		forms := fmt.Sprintf("%v|%+v|%#v|%s", cfg, cfg, cfg, cfg.String())
		if strings.Contains(forms, "tok-xyz") || strings.Contains(forms, "hdr-xyz") || strings.Contains(forms, "q-xyz") || !strings.Contains(forms, "Token:"+Redacted) {
			t.Fatalf("forms: %s", forms)
		}
	})

	t.Run("invalid config", func(t *testing.T) {
		if _, err := NewClient(Config{CommonConfig: CommonConfig{URL: "mailto:x"}}); !errors.Is(err, ErrConfig) {
			t.Fatalf("NewClient: %v", err)
		}
		if _, err := NewClient(Config{CommonConfig: CommonConfig{TLS: TLSConfig{CACertPEM: "bad"}}}); !errors.Is(err, ErrConfig) {
			t.Fatalf("bad TLS: %v", err)
		}
		if _, err := newEndpoint(CommonConfig{}, "::nope"); !errors.Is(err, ErrConfig) {
			t.Fatalf("bad endpoint: %v", err)
		}
		if _, err := newEndpoint(CommonConfig{TLS: TLSConfig{CACertPEM: "bad"}}, "http://h/x"); !errors.Is(err, ErrConfig) {
			t.Fatalf("bad endpoint TLS: %v", err)
		}
	})
}

// jaegerRoot serves the query root like the real UI (with or without the version).
func jaegerRoot(withVersion bool) func(w http.ResponseWriter, r *http.Request, n int) {
	return func(w http.ResponseWriter, r *http.Request, _ int) {
		switch r.URL.Path {
		case "/":
			w.Header().Set("Content-Type", "text/html; charset=utf-8")
			body := "<html><script>const x = 1;</script></html>"
			if withVersion {
				body = `<html><script>// <![CDATA[
JAEGER_VERSION = {"gitCommit":"d723f91","gitVersion":"v2.23.0","buildDate":"2026-11-04T16:02:09Z"};
// ]]></script></html>`
			}
			_, _ = w.Write([]byte(body))
		case "/api/v3/services":
			if r.Header.Get("Authorization") != "Bearer "+testToken {
				writeText(w, http.StatusUnauthorized, "unauthorized")
				return
			}
			writeJSON(w, 200, `{"services":["b","a"]}`)
		case "/status":
			writeJSON(w, 200, `{"start_time":"2026-11-18T20:48:29Z","healthy":true,"status":"StatusOK","attributes":{}}`)
		case "/metrics":
			writeText(w, 200, "# HELP target_info x\ntarget_info{service_instance_id=\"i\",service_name=\"jaeger\",service_version=\"v2.23.0\"} 1\n")
		default:
			writeText(w, http.StatusNotFound, "404 page not found\n")
		}
	}
}

func TestClient(t *testing.T) {
	ts := newTestServer(t, jaegerRoot(true))

	t.Run("health ping version", func(t *testing.T) {
		c := newTestClient(t, ts.URL, nil)
		hs, err := c.Health(t.Context())
		if err != nil || !hs.Healthy || hs.Status != "ok" || hs.Details["httpStatus"] != "200" {
			t.Fatalf("health %+v %v", hs, err)
		}
		if ts.request(int(ts.calls.Load())-1).URL.Path != "/" {
			t.Fatal("health must probe the query root")
		}
		if d, err := c.Ping(t.Context()); err != nil || d <= 0 {
			t.Fatalf("ping %s %v", d, err)
		}
		if v, err := c.ServerVersion(t.Context()); err != nil || v != "2.23.0" {
			t.Fatalf("version %q %v", v, err)
		}
		if info, err := c.BuildInfo(t.Context()); err != nil || info == nil || info.GitVersion != "v2.23.0" {
			t.Fatalf("build info %+v %v", info, err)
		}
		if c.Config().URL != ts.URL || c.Query() == nil {
			t.Fatal("Config()/Query()")
		}
	})

	t.Run("health detail", func(t *testing.T) {
		c := newTestClient(t, ts.URL, func(cfg *Config) { cfg.HealthURL = ts.URL + "/status" })
		hs, err := c.Health(t.Context())
		if err != nil || !hs.Healthy || hs.Details["healthcheck.status"] != "StatusOK" || hs.Details["healthcheck.healthy"] != "true" {
			t.Fatalf("health %+v %v", hs, err)
		}
		bad := newTestServer(t, func(w http.ResponseWriter, r *http.Request, _ int) {
			if r.URL.Path == "/status" {
				writeJSON(w, http.StatusServiceUnavailable, `{"healthy":false,"status":"StatusRecoverableError"}`)
				return
			}
			jaegerRoot(false)(w, r, 0)
		})
		c = newTestClient(t, bad.URL, func(cfg *Config) { cfg.HealthURL = bad.URL + "/status" })
		if hs, err := c.Health(t.Context()); err != nil || hs.Healthy || hs.Status != "degraded" {
			t.Fatalf("degraded %+v %v", hs, err)
		}
		c = newTestClient(t, ts.URL, func(cfg *Config) { cfg.HealthURL = ts.URL + "/status" })
		ctx, cancel := contextWithCancel(t)
		cancel()
		if _, err := c.Health(ctx); !errors.Is(err, ErrCancelled) {
			t.Fatalf("cancelled: %v", err)
		}
	})

	t.Run("health detail cancelled", func(t *testing.T) {
		ctx, cancel := contextWithCancel(t)
		srv := newTestServer(t, func(w http.ResponseWriter, r *http.Request, _ int) {
			if r.URL.Path == "/status" {
				cancel()
			}
			jaegerRoot(false)(w, r, 0)
		})
		c := newTestClient(t, srv.URL, func(cfg *Config) { cfg.HealthURL = srv.URL + "/status" })
		if _, err := c.Health(ctx); !errors.Is(err, ErrCancelled) {
			t.Fatalf("cancelled during detail: %v", err)
		}
	})

	t.Run("authentication errors", func(t *testing.T) {
		bad := newTestClient(t, ts.URL, func(cfg *Config) { cfg.Token = NewSecret("wrong-token") })
		if _, err := bad.Ping(t.Context()); !errors.Is(err, ErrAuthentication) {
			t.Fatalf("bad token: %v", err)
		}
		forbidden := newTestServer(t, func(w http.ResponseWriter, _ *http.Request, _ int) {
			writeText(w, http.StatusForbidden, `{"error":{"httpCode":403,"message":"access denied"}}`+"\n")
		})
		_, err := newTestClient(t, forbidden.URL, nil).Ping(t.Context())
		if !errors.Is(err, ErrPermissionDenied) || !strings.Contains(err.Error(), "access denied") {
			t.Fatalf("403: %v", err)
		}
	})

	t.Run("server version fallbacks", func(t *testing.T) {
		noVersion := newTestServer(t, jaegerRoot(false))
		c := newTestClient(t, noVersion.URL, func(cfg *Config) { cfg.MetricsURL = noVersion.URL + "/metrics" })
		if v, err := c.ServerVersion(t.Context()); err != nil || v != "2.23.0" {
			t.Fatalf("metrics fallback %q %v", v, err)
		}
		if _, err := newTestClient(t, noVersion.URL, nil).ServerVersion(t.Context()); !errors.Is(err, ErrUnsupported) {
			t.Fatalf("no version: %v", err)
		}
		c = newTestClient(t, noVersion.URL, func(cfg *Config) { cfg.MetricsURL = noVersion.URL + "/nope" })
		if _, err := c.ServerVersion(t.Context()); !errors.Is(err, ErrUnsupported) {
			t.Fatalf("metrics 404: %v", err)
		}
		c = newTestClient(t, noVersion.URL, func(cfg *Config) { cfg.MetricsURL = noVersion.URL + "/" })
		if _, err := c.ServerVersion(t.Context()); !errors.Is(err, ErrUnsupported) {
			t.Fatalf("metrics without target_info: %v", err)
		}
		ts404 := newTestServer(t, statusHandler(http.StatusNotFound))
		if _, err := newTestClient(t, ts404.URL, nil).ServerVersion(t.Context()); !errors.Is(err, ErrUnsupported) {
			t.Fatalf("root 404: %v", err)
		}
		if info, err := newTestClient(t, ts404.URL, nil).BuildInfo(t.Context()); err != nil || info != nil {
			t.Fatalf("build info 404: %+v %v", info, err)
		}
		if info, err := newTestClient(t, noVersion.URL, nil).BuildInfo(t.Context()); err != nil || info != nil {
			t.Fatalf("build info without version: %+v %v", info, err)
		}
		ts500 := newTestServer(t, statusHandler(http.StatusInternalServerError))
		if _, err := newTestClient(t, ts500.URL, func(cfg *Config) { cfg.Retry = fastRetry(0) }).BuildInfo(t.Context()); !errors.Is(err, ErrUnavailable) {
			t.Fatalf("build info 500: %v", err)
		}
		broken := newTestServer(t, func(w http.ResponseWriter, _ *http.Request, _ int) {
			_, _ = w.Write([]byte(`JAEGER_VERSION = {broken};`))
		})
		if info, err := newTestClient(t, broken.URL, nil).BuildInfo(t.Context()); err != nil || info != nil {
			t.Fatalf("broken build info: %+v %v", info, err)
		}
		c = newTestClient(t, ts500.URL, func(cfg *Config) { cfg.Retry = fastRetry(0) })
		if _, err := c.ServerVersion(t.Context()); !errors.Is(err, ErrUnavailable) {
			t.Fatalf("version 500: %v", err)
		}
		c = newTestClient(t, noVersion.URL, func(cfg *Config) { cfg.MetricsURL = ts500.URL + "/metrics"; cfg.Retry = fastRetry(0) })
		if _, err := c.ServerVersion(t.Context()); !errors.Is(err, ErrUnavailable) {
			t.Fatalf("metrics 500: %v", err)
		}
		garbled := newTestServer(t, func(w http.ResponseWriter, _ *http.Request, _ int) {
			_, _ = w.Write([]byte(`JAEGER_VERSION = {"gitVersion":""};`))
		})
		if _, err := newTestClient(t, garbled.URL, nil).ServerVersion(t.Context()); !errors.Is(err, ErrUnsupported) {
			t.Fatalf("empty gitVersion: %v", err)
		}
	})

	t.Run("core.close.idempotent", func(t *testing.T) {
		c := newTestClient(t, ts.URL, nil)
		first, second := c.Close(), c.Close()
		if first != nil || second != nil {
			t.Fatal("close")
		}
		if _, err := c.Ping(t.Context()); !errors.Is(err, ErrCancelled) {
			t.Fatalf("after close: %v", err)
		}
		if _, err := c.NewRemoteSampler("svc", time.Hour); !errors.Is(err, ErrCancelled) {
			t.Fatalf("sampler after close: %v", err)
		}
	})

	t.Run("core.health.unreachable", func(t *testing.T) {
		c := newTestClient(t, "http://"+closedAddr(t), func(cfg *Config) { cfg.ConnectTimeout = time.Second })
		if hs, err := c.Health(t.Context()); err != nil || hs.Healthy || hs.Status != "unreachable" {
			t.Fatalf("hs=%+v err=%v", hs, err)
		}
	})

	t.Run("from env", func(t *testing.T) {
		t.Setenv(EnvPrefix+"URL", ts.URL)
		t.Setenv(EnvPrefix+"TOKEN", testToken)
		c, err := NewClientFromEnv()
		if err != nil {
			t.Fatal(err)
		}
		defer func() { _ = c.Close() }()
		if _, err := c.Ping(t.Context()); err != nil {
			t.Fatal(err)
		}
		t.Setenv(EnvPrefix+"URL", "mailto:x")
		if _, err := NewClientFromEnv(); !errors.Is(err, ErrConfig) {
			t.Fatalf("bad env: %v", err)
		}
	})
}

func TestClientSampling(t *testing.T) {
	perOp := loadFixture(t, "sampling.per_operation")
	ts := newTestServer(t, func(w http.ResponseWriter, r *http.Request, _ int) {
		if r.URL.Path != "/sampling" {
			writeText(w, 404, "404 page not found\n")
			return
		}
		if r.URL.Query().Get("service") == "" {
			writeText(w, http.StatusBadRequest, "'service' parameter must be provided once\n")
			return
		}
		perOp.serve(w)
	})
	c := newTestClient(t, ts.URL, func(cfg *Config) { cfg.SamplingURL = ts.URL + "/sampling?tenant=a" })

	s, err := c.SamplingStrategy(t.Context(), "sdk-sampling-ops")
	if err != nil || s.StrategyType != StrategyProbabilistic || s.SamplingRate != 0.8 || len(s.Operations) == 0 {
		t.Fatalf("strategy %+v %v", s, err)
	}
	last := ts.request(int(ts.calls.Load()) - 1)
	if last.URL.Query().Get("tenant") != "a" || last.Header.Get("Authorization") != "Bearer "+testToken {
		t.Fatalf("request %s %v", last.URL, last.Header)
	}

	rs, err := c.NewRemoteSampler("sdk-sampling-ops", time.Hour)
	if err != nil {
		t.Fatal(err)
	}
	if err := rs.Refresh(t.Context()); err != nil {
		t.Fatal(err)
	}
	if _, fetched := rs.Strategy(); !fetched {
		t.Fatal("strategy not fetched")
	}
	if _, err := c.NewRemoteSampler("", time.Hour); !errors.Is(err, ErrConfig) {
		t.Fatalf("empty service: %v", err)
	}
	_ = c.Close()
	select {
	case <-rs.done:
	case <-time.After(5 * time.Second):
		t.Fatal("client close must stop its samplers")
	}
}

func TestClientInitTracing(t *testing.T) {
	srv := newSamplingServer(t)
	c := newTestClient(t, srv.URL, func(cfg *Config) { cfg.SamplingURL = srv.URL + "/sampling"; cfg.Token = NewSecret("client-token") })
	h, err := c.InitTracing(t.Context(), TracingConfig{ServiceName: "svc-client", DisableSetGlobal: true},
		WithEnv(map[string]string{"OTEL_TRACES_SAMPLER": "jaeger_remote"}),
		WithExporterFactory(memoryFactory(tracetest.NewInMemoryExporter(), nil)))
	if err != nil || h.RemoteSampler() == nil {
		t.Fatalf("init: %v", err)
	}
	if err := h.RemoteSampler().Refresh(t.Context()); err != nil {
		t.Fatal(err)
	}
	if got := srv.request(int(srv.calls.Load()) - 1).Header.Get("Authorization"); got != "Bearer client-token" {
		t.Fatalf("client token not used by the remote sampler: %q", got)
	}
	if _, err := c.InitTracing(t.Context(), TracingConfig{}, WithEnv(map[string]string{})); !errors.Is(err, ErrConfig) {
		t.Fatalf("invalid config: %v", err)
	}
	_ = c.Close()
	select {
	case <-h.RemoteSampler().done:
	default:
		t.Fatal("Close must shut the handle down")
	}
	if _, err := c.InitTracing(t.Context(), TracingConfig{ServiceName: "s"}); !errors.Is(err, ErrCancelled) {
		t.Fatalf("closed client: %v", err)
	}
}
