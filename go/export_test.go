package jaeger

import (
	"context"
	"errors"
	"net/http"
	"net/http/httptest"
	"reflect"
	"strings"
	"testing"
	"time"

	"go.opentelemetry.io/otel"
	sdktrace "go.opentelemetry.io/otel/sdk/trace"
	"go.opentelemetry.io/otel/sdk/trace/tracetest"
	"go.opentelemetry.io/otel/trace"
)

func TestTracingConfigFromEnv(t *testing.T) {
	t.Run("preset defaults", func(t *testing.T) {
		c, err := TracingConfigFromEnv(TracingConfig{ServiceName: "s"}, map[string]string{})
		if err != nil || c.Protocol != ProtocolHTTPProtobuf || c.Endpoint != DefaultOTLPHTTPEndpoint {
			t.Fatalf("%v %s", err, c)
		}
		c, _ = TracingConfigFromEnv(TracingConfig{Protocol: ProtocolGRPC}, map[string]string{})
		if c.Endpoint != DefaultOTLPGRPCEndpoint {
			t.Fatalf("grpc default %s", c.Endpoint)
		}
		c, _ = TracingConfigFromEnv(TracingConfig{Protocol: ProtocolNative}, map[string]string{})
		if c.Protocol != ProtocolHTTPProtobuf {
			t.Fatalf("native %s", c.Protocol)
		}
	})

	t.Run("protocol precedence", func(t *testing.T) {
		env := map[string]string{
			EnvPrefix + "OTLP_PROTOCOL":          "grpc",
			"OTEL_EXPORTER_OTLP_TRACES_PROTOCOL": "http/protobuf",
			"OTEL_EXPORTER_OTLP_PROTOCOL":        "grpc",
		}
		if c, _ := TracingConfigFromEnv(TracingConfig{}, env); c.Protocol != ProtocolGRPC {
			t.Fatalf("grow: %s", c.Protocol)
		}
		delete(env, EnvPrefix+"OTLP_PROTOCOL")
		if c, _ := TracingConfigFromEnv(TracingConfig{}, env); c.Protocol != ProtocolHTTPProtobuf {
			t.Fatalf("traces: %s", c.Protocol)
		}
		if c, _ := TracingConfigFromEnv(TracingConfig{Protocol: ProtocolGRPC}, env); c.Protocol != ProtocolGRPC {
			t.Fatal("explicit wins")
		}
		_, err := TracingConfigFromEnv(TracingConfig{}, map[string]string{"OTEL_EXPORTER_OTLP_PROTOCOL": "http/json"})
		if !errors.Is(err, ErrConfig) || !strings.Contains(err.Error(), "OTEL_EXPORTER_OTLP_PROTOCOL") || strings.Contains(err.Error(), "http/json") {
			t.Fatalf("invalid: %v", err)
		}
	})

	t.Run("endpoint precedence", func(t *testing.T) {
		env := map[string]string{
			EnvPrefix + "OTLP_HTTP_ENDPOINT":     "http://grow-http:4318",
			EnvPrefix + "OTLP_GRPC_ENDPOINT":     "http://grow-grpc:4317",
			EnvPrefix + "OTLP_ENDPOINT":          "http://grow:1",
			"OTEL_EXPORTER_OTLP_TRACES_ENDPOINT": "http://traces:2/custom",
			"OTEL_EXPORTER_OTLP_ENDPOINT":        "http://base:3/",
		}
		check := func(protocol Protocol, want string) {
			t.Helper()
			c, err := TracingConfigFromEnv(TracingConfig{Protocol: protocol}, env)
			if err != nil || c.Endpoint != want {
				t.Fatalf("%s: got %q want %q (%v)", protocol, c.Endpoint, want, err)
			}
		}
		check(ProtocolHTTPProtobuf, "http://grow-http:4318")
		check(ProtocolGRPC, "http://grow-grpc:4317")
		delete(env, EnvPrefix+"OTLP_HTTP_ENDPOINT")
		delete(env, EnvPrefix+"OTLP_GRPC_ENDPOINT")
		check(ProtocolHTTPProtobuf, "http://grow:1")
		delete(env, EnvPrefix+"OTLP_ENDPOINT")
		check(ProtocolGRPC, "http://traces:2/custom")
		delete(env, "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT")
		check(ProtocolHTTPProtobuf, "http://base:3/v1/traces")
		check(ProtocolGRPC, "http://base:3/")
		if c, _ := TracingConfigFromEnv(TracingConfig{Endpoint: "http://explicit:9"}, env); c.Endpoint != "http://explicit:9" {
			t.Fatal("explicit endpoint wins")
		}
	})

	t.Run("headers compression tls", func(t *testing.T) {
		env := map[string]string{
			EnvPrefix + "OTLP_HEADERS":              "Authorization=Bearer%20abc, x-tenant = t1 ,",
			"OTEL_EXPORTER_OTLP_HEADERS":            "a=b",
			EnvPrefix + "OTLP_COMPRESSION":          "NONE",
			"OTEL_EXPORTER_OTLP_TRACES_CERTIFICATE": "/ca.pem",
		}
		c, err := TracingConfigFromEnv(TracingConfig{}, env)
		if err != nil || !reflect.DeepEqual(c.Headers, map[string]string{"Authorization": "Bearer abc", "x-tenant": "t1"}) ||
			c.Compression != CompressionNone || c.TLS.CACertPath != "/ca.pem" {
			t.Fatalf("%v %+v", err, c)
		}
		if strings.Contains(c.String(), "abc") {
			t.Fatal("header value leaked in String()")
		}
		c, _ = TracingConfigFromEnv(TracingConfig{Headers: map[string]string{"x": "y"}, Compression: CompressionGzip, TLS: TLSConfig{CACertPEM: "pem"}}, env)
		if !reflect.DeepEqual(c.Headers, map[string]string{"x": "y"}) || c.Compression != CompressionGzip || c.TLS.CACertPath != "" {
			t.Fatalf("explicit wins: %+v", c)
		}
		if c, _ := TracingConfigFromEnv(TracingConfig{}, map[string]string{"OTEL_EXPORTER_OTLP_COMPRESSION": "gzip"}); c.Compression != CompressionGzip {
			t.Fatal("gzip")
		}
		for _, bad := range []map[string]string{
			{"OTEL_EXPORTER_OTLP_HEADERS": "novalue"},
			{"OTEL_EXPORTER_OTLP_HEADERS": "=v"},
			{"OTEL_EXPORTER_OTLP_HEADERS": "k=%zz"},
			{"OTEL_EXPORTER_OTLP_TRACES_COMPRESSION": "zstd"},
			{"OTEL_RESOURCE_ATTRIBUTES": "nope"},
		} {
			if _, err := TracingConfigFromEnv(TracingConfig{}, bad); !errors.Is(err, ErrConfig) {
				t.Fatalf("%v: %v", bad, err)
			}
		}
	})

	t.Run("service resource sampler propagators", func(t *testing.T) {
		env := map[string]string{
			"OTEL_SERVICE_NAME":        "otel-svc",
			"OTEL_RESOURCE_ATTRIBUTES": "team=core,region=eu",
			"OTEL_TRACES_SAMPLER":      "parentbased_traceidratio",
			"OTEL_TRACES_SAMPLER_ARG":  "0.25",
			"OTEL_PROPAGATORS":         "tracecontext, b3 ,none",
		}
		c, err := TracingConfigFromEnv(TracingConfig{ResourceAttributes: map[string]any{"team": "explicit"}}, env)
		if err != nil || c.ServiceName != "otel-svc" || c.Sampler != SamplerParentBasedTraceIDRatio || c.SamplerRatio != 0.25 ||
			!reflect.DeepEqual(c.ResourceAttributes, map[string]any{"team": "explicit", "region": "eu"}) ||
			!reflect.DeepEqual(c.Propagators, []string{"tracecontext", "b3"}) {
			t.Fatalf("%v %+v", err, c)
		}
		env[EnvPrefix+"SERVICE_NAME"] = "grow-svc"
		if c, _ := TracingConfigFromEnv(TracingConfig{}, env); c.ServiceName != "grow-svc" {
			t.Fatal("GROW service name wins")
		}
		if c, _ := TracingConfigFromEnv(TracingConfig{Sampler: SamplerAlwaysOff, Propagators: []string{"baggage"}}, env); c.Sampler != SamplerAlwaysOff || len(c.Propagators) != 1 {
			t.Fatal("explicit sampler/propagators win")
		}
		for _, s := range []string{"always_on", "jaeger_remote", "parentbased_jaeger_remote"} {
			c, err := TracingConfigFromEnv(TracingConfig{}, map[string]string{"OTEL_TRACES_SAMPLER": s, "OTEL_TRACES_SAMPLER_ARG": "x"})
			if err != nil || (s == "always_on") != (c.Sampler == SamplerAlwaysOn) {
				t.Fatalf("%s: %v %s", s, err, c.Sampler)
			}
		}
		for _, bad := range []map[string]string{
			{"OTEL_TRACES_SAMPLER": "sometimes"},
			{"OTEL_TRACES_SAMPLER": "traceidratio", "OTEL_TRACES_SAMPLER_ARG": "2"},
			{"OTEL_TRACES_SAMPLER": "traceidratio", "OTEL_TRACES_SAMPLER_ARG": "half"},
		} {
			_, err := TracingConfigFromEnv(TracingConfig{}, bad)
			if !errors.Is(err, ErrConfig) || strings.Contains(err.Error(), "sometimes") || strings.Contains(err.Error(), "half") {
				t.Fatalf("%v: %v", bad, err)
			}
		}
	})
}

func TestSamplingPresets(t *testing.T) {
	c := SamplingProduction.Apply(TracingConfig{ServiceName: "s"})
	if c.Sampler != SamplerParentBasedTraceIDRatio || c.SamplerRatio != 0.1 || c.ServiceName != "s" {
		t.Fatalf("%+v", c)
	}
	for _, p := range []SamplingPreset{SamplingDevelopment, SamplingProduction, SamplingFollowParent, SamplingOff, SamplingRatio(0.5), SamplingRatio(0), SamplingRatio(2)} {
		if err := p.Apply(TracingConfig{ServiceName: "s"}).Validate(); err != nil {
			t.Fatalf("%+v: %v", p, err)
		}
	}
	if SamplingRatio(-1).Sampler != SamplerParentBasedAlwaysOff || SamplingRatio(1) != SamplingFollowParent || SamplingRatio(0.3).Ratio != 0.3 {
		t.Fatal("SamplingRatio edges")
	}
}

func memoryFactory(exp *tracetest.InMemoryExporter, seen *TracingConfig) ExporterFactory {
	return func(_ context.Context, cfg TracingConfig) (sdktrace.SpanExporter, error) {
		if seen != nil {
			*seen = cfg
		}
		return exp, nil
	}
}

func TestInitTracing(t *testing.T) {
	ctx := context.Background()

	t.Run("preset and factory", func(t *testing.T) {
		exp := tracetest.NewInMemoryExporter()
		var seen TracingConfig
		h, err := InitTracing(ctx, TracingConfig{DisableSetGlobal: true},
			WithEnv(map[string]string{"OTEL_SERVICE_NAME": "svc-env", EnvPrefix + "OTLP_HTTP_ENDPOINT": "http://jaeger:4318"}),
			WithExporterFactory(memoryFactory(exp, &seen)))
		if err != nil {
			t.Fatal(err)
		}
		if seen.ServiceName != "svc-env" || seen.Endpoint != "http://jaeger:4318" || seen.Protocol != ProtocolHTTPProtobuf {
			t.Fatalf("resolved %s", seen)
		}
		_, span := h.Tracer("t").Start(ctx, "op")
		span.End()
		if err := h.ForceFlush(ctx); err != nil {
			t.Fatal(err)
		}
		if len(exp.GetSpans()) != 1 {
			t.Fatal("span not exported")
		}
		_ = h.Shutdown(ctx)
	})

	t.Run("errors", func(t *testing.T) {
		if _, err := InitTracing(ctx, TracingConfig{}, WithEnv(map[string]string{})); !errors.Is(err, ErrConfig) {
			t.Fatalf("no service name: %v", err)
		}
		if _, err := InitTracing(ctx, TracingConfig{ServiceName: "s"}, WithEnv(map[string]string{"OTEL_EXPORTER_OTLP_PROTOCOL": "x"})); !errors.Is(err, ErrConfig) {
			t.Fatalf("bad env: %v", err)
		}
		always := sdktrace.AlwaysSample()
		if _, err := InitTracing(ctx, TracingConfig{}, WithEnv(map[string]string{}), WithSampler(always)); !errors.Is(err, ErrConfig) {
			t.Fatalf("custom sampler invalid config: %v", err)
		}
		boom := errors.New("boom")
		if _, err := InitTracing(ctx, TracingConfig{ServiceName: "s"}, WithSampler(always), WithEnv(map[string]string{}),
			WithExporterFactory(func(context.Context, TracingConfig) (sdktrace.SpanExporter, error) { return nil, boom })); !errors.Is(err, boom) {
			t.Fatalf("factory error: %v", err)
		}
		if _, err := InitTracing(ctx, TracingConfig{ServiceName: "s", TLS: TLSConfig{CACertPEM: "bad"}}, WithEnv(map[string]string{})); !errors.Is(err, ErrConfig) {
			t.Fatalf("bad tls: %v", err)
		}
	})

	t.Run("custom sampler", func(t *testing.T) {
		exp := tracetest.NewInMemoryExporter()
		h, err := InitTracing(ctx, TracingConfig{ServiceName: "s", Sampler: SamplerAlwaysOn, DisableSetGlobal: true},
			WithEnv(map[string]string{}), WithSampler(sdktrace.NeverSample()), WithExporterFactory(memoryFactory(exp, nil)))
		if err != nil {
			t.Fatal(err)
		}
		_, span := h.Tracer("t").Start(ctx, "dropped")
		span.End()
		_ = h.ForceFlush(ctx)
		if len(exp.GetSpans()) != 0 {
			t.Fatal("custom sampler ignored")
		}
		_ = h.Shutdown(ctx)

		// parentbased_*: a sampled remote parent keeps the child even with NeverSample.
		exp.Reset()
		prevTP, prevProp := otel.GetTracerProvider(), otel.GetTextMapPropagator()
		t.Cleanup(func() { otel.SetTracerProvider(prevTP); otel.SetTextMapPropagator(prevProp) })
		h, err = InitTracing(ctx, TracingConfig{ServiceName: "s"}, WithEnv(map[string]string{}),
			WithSampler(sdktrace.NeverSample()), WithExporterFactory(memoryFactory(exp, nil)))
		if err != nil {
			t.Fatal(err)
		}
		if otel.GetTracerProvider() != h.TracerProvider() {
			t.Fatal("not installed globally")
		}
		parent := trace.NewSpanContext(trace.SpanContextConfig{
			TraceID: trace.TraceID{1}, SpanID: trace.SpanID{2}, TraceFlags: trace.FlagsSampled, Remote: true,
		})
		_, child := h.Tracer("t").Start(trace.ContextWithRemoteSpanContext(ctx, parent), "child")
		child.End()
		_, root := h.Tracer("t").Start(ctx, "root")
		root.End()
		_ = h.ForceFlush(ctx)
		if spans := exp.GetSpans(); len(spans) != 1 || spans[0].Name != "child" {
			t.Fatalf("parent-based wrapping: %v", spans)
		}
		_ = h.Shutdown(ctx)
	})

	t.Run("otlp http export", func(t *testing.T) {
		c := &ttCollector{}
		srv := httptest.NewServer(http.HandlerFunc(c.handler))
		defer srv.Close()
		h, err := InitTracing(ctx, TracingConfig{ServiceName: "s", DisableSetGlobal: true},
			WithEnv(map[string]string{EnvPrefix + "OTLP_ENDPOINT": srv.URL, EnvPrefix + "OTLP_HEADERS": "X-Test=jaeger"}))
		if err != nil {
			t.Fatal(err)
		}
		_, span := h.Tracer("t").Start(ctx, "op")
		span.End()
		if err := h.Shutdown(ctx); err != nil {
			t.Fatal(err)
		}
		c.mu.Lock()
		defer c.mu.Unlock()
		if len(c.paths) != 1 || c.paths[0] != "/v1/traces" || c.headers[0] != "jaeger" || c.encoding[0] != "gzip" {
			t.Fatalf("collector saw %+v", c)
		}
	})

	t.Run("new exporter", func(t *testing.T) {
		t.Setenv("OTEL_EXPORTER_OTLP_PROTOCOL", "grpc")
		exp, err := NewExporter(ctx, TracingConfig{ServiceName: "s"})
		if err != nil {
			t.Fatal(err)
		}
		cctx, cancel := context.WithTimeout(ctx, time.Second)
		defer cancel()
		_ = exp.Shutdown(cctx)
		t.Setenv("OTEL_EXPORTER_OTLP_PROTOCOL", "bogus")
		if _, err := NewExporter(ctx, TracingConfig{ServiceName: "s"}); !errors.Is(err, ErrConfig) {
			t.Fatalf("bad env: %v", err)
		}
	})
}
