package integration

// Integration tests against the real Jaeger from docker/compose.yml. Every public
// function of the package is called at least once with a round-trip assertion (send
// spans → read them back through the query API).

import (
	"context"
	"crypto/rand"
	"encoding/hex"
	"errors"
	"net/http"
	"net/http/httptest"
	"os"
	"reflect"
	"regexp"
	"slices"
	"testing"
	"time"

	"go.opentelemetry.io/otel"
	"go.opentelemetry.io/otel/attribute"
	sdktrace "go.opentelemetry.io/otel/sdk/trace"
	"go.opentelemetry.io/otel/trace"

	jaeger "github.com/grow-codes-packages/jaeger/go"
)

// setup loads /bootstrap/env and returns a client from the environment.
func setup(t *testing.T) *jaeger.Client {
	t.Helper()
	bootstrap := os.Getenv("GROW_BOOTSTRAP_ENV")
	if bootstrap == "" {
		bootstrap = "/bootstrap/env"
	}
	loadBootstrapEnv(t, bootstrap)
	if os.Getenv("GROW_JAEGER_URL") == "" {
		t.Skip("GROW_JAEGER_URL is not set; run through docker/compose.yml")
	}
	c, err := jaeger.NewClientFromEnv()
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = c.Close() })
	return c
}

// runID is a random 8-char hex id; every test uses unique service names.
func runID(t *testing.T) string {
	t.Helper()
	b := make([]byte, 4)
	if _, err := rand.Read(b); err != nil {
		t.Fatal(err)
	}
	return hex.EncodeToString(b)
}

func startTracing(t *testing.T, service string, protocol jaeger.Protocol, opts ...jaeger.TracingOption) *jaeger.TracingSetup {
	t.Helper()
	h, err := jaeger.InitTracing(context.Background(), jaeger.TracingConfig{
		ServiceName: service, ServiceVersion: "1.2.3", Environment: "sdk-test",
		Protocol: protocol, DisableSetGlobal: true,
		Batch: jaeger.TracingBatchOptions{ScheduledDelay: 100 * time.Millisecond},
	}, opts...)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { _ = h.Shutdown(context.Background()) })
	return h
}

func waitTrace(t *testing.T, q *jaeger.QueryClient, traceID string, minSpans int) *jaeger.Trace {
	t.Helper()
	tr, err := q.WaitForTrace(context.Background(), traceID, jaeger.WaitForTraceOptions{Timeout: 30 * time.Second, Interval: 250 * time.Millisecond, MinSpans: minSpans})
	if err != nil {
		t.Fatalf("WaitForTrace %s: %v", traceID, err)
	}
	return tr
}

func span(t *testing.T, tr *jaeger.Trace, name string) jaeger.Span {
	t.Helper()
	for _, s := range tr.Spans {
		if s.Name == name {
			return s
		}
	}
	t.Fatalf("no span %q in trace %s", name, tr.TraceID)
	return jaeger.Span{}
}

func TestClientCore(t *testing.T) {
	c := setup(t)
	ctx := context.Background()

	cfg, err := jaeger.ConfigFromEnv()
	if err != nil || cfg.SamplingURL == jaeger.DefaultSamplingURL || cfg.HealthURL == "" {
		t.Fatalf("ConfigFromEnv: %v %s", err, cfg)
	}
	if err := cfg.Validate(); err != nil {
		t.Fatal(err)
	}
	hs, err := c.Health(ctx)
	if err != nil || !hs.Healthy || hs.Status != "ok" || hs.Details["healthcheck.status"] != "StatusOK" {
		t.Fatalf("Health: %+v %v", hs, err)
	}
	if d, err := c.Ping(ctx); err != nil || d <= 0 {
		t.Fatalf("Ping: %s %v", d, err)
	}
	v, err := c.ServerVersion(ctx)
	if err != nil || v != "2.23.0" {
		t.Fatalf("ServerVersion: %q %v", v, err)
	}
	if info, err := c.BuildInfo(ctx); err != nil || info == nil || info.GitVersion != "v"+v {
		t.Fatalf("BuildInfo: %+v %v", info, err)
	}
	// Client.InitTracing: the handle exports to Jaeger and Close shuts it down.
	tracedService := "sdk-go-client-" + runID(t)
	th, err := c.InitTracing(ctx, jaeger.TracingConfig{ServiceName: tracedService, DisableSetGlobal: true,
		Batch: jaeger.TracingBatchOptions{ScheduledDelay: 100 * time.Millisecond}})
	if err != nil {
		t.Fatal(err)
	}
	_, span := th.Tracer("t").Start(ctx, "client-op")
	span.End()
	_ = th.ForceFlush(ctx)
	if tr := waitTrace(t, c.Query(), span.SpanContext().TraceID().String(), 1); tr.Spans[0].ServiceName != tracedService {
		t.Fatalf("client tracing: %+v", tr.Spans)
	}
	// The metrics fallback finds the same version when the UI page has none.
	metricsOnly, err := jaeger.NewClient(jaeger.Config{CommonConfig: jaeger.CommonConfig{URL: cfg.MetricsURL}, MetricsURL: cfg.MetricsURL})
	if err != nil {
		t.Fatal(err)
	}
	if mv, err := metricsOnly.ServerVersion(ctx); err != nil || mv != v {
		t.Fatalf("metrics version: %q %v", mv, err)
	}
	_ = metricsOnly.Close()

	unreachable, err := jaeger.NewClient(jaeger.Config{CommonConfig: jaeger.CommonConfig{URL: "http://127.0.0.1:1", ConnectTimeout: time.Second}})
	if err != nil {
		t.Fatal(err)
	}
	if hs, err := unreachable.Health(ctx); err != nil || hs.Healthy || hs.Status != "unreachable" {
		t.Fatalf("unreachable: %+v %v", hs, err)
	}
	if c.Config().URL == "" || c.Close() != nil || c.Close() != nil {
		t.Fatal("Config/Close")
	}
	if _, err := c.Ping(ctx); !errors.Is(err, jaeger.ErrCancelled) {
		t.Fatalf("ping after close: %v", err)
	}
}

func TestTraceRoundTrip(t *testing.T) {
	for _, protocol := range []jaeger.Protocol{jaeger.ProtocolHTTPProtobuf, jaeger.ProtocolGRPC} {
		t.Run(string(protocol), func(t *testing.T) {
			c := setup(t)
			q := c.Query()
			ctx := context.Background()
			run := runID(t)
			service := "sdk-go-" + map[jaeger.Protocol]string{jaeger.ProtocolGRPC: "grpc", jaeger.ProtocolHTTPProtobuf: "http"}[protocol] + "-" + run
			h := startTracing(t, service, protocol)
			tracer := h.Tracer("integration")

			// A separate trace to link to.
			_, other := tracer.Start(ctx, "linked-"+run, trace.WithNewRoot())
			other.End()

			rootName := "GET /orders " + run
			ctx, root := tracer.Start(ctx, rootName, trace.WithSpanKind(trace.SpanKindServer), trace.WithAttributes(
				attribute.String("order.id", run), attribute.Int("int.attr", 42), attribute.Float64("double.attr", 1.5),
				attribute.Bool("bool.attr", true), attribute.StringSlice("array.attr", []string{"a", "b"}),
			))
			traceID, ok := jaeger.CurrentTraceID(ctx)
			if !ok || jaeger.TraceFields(ctx)["trace_id"] != traceID {
				t.Fatal("CurrentTraceID/TraceFields")
			}
			if sid, ok := jaeger.CurrentSpanID(ctx); !ok || sid != root.SpanContext().SpanID().String() || jaeger.CurrentSpan(ctx) != root {
				t.Fatal("CurrentSpanID/CurrentSpan")
			}
			boom := errors.New("boom")
			_, err := jaeger.WithSpan(ctx, "db.query", func(ctx context.Context) (int, error) {
				jaeger.AddEvent(ctx, "cache.miss", map[string]any{"cache.key": "k1"})
				return 0, boom
			}, jaeger.WithSpanKind(trace.SpanKindClient), jaeger.WithSpanLinks(trace.Link{SpanContext: other.SpanContext()}))
			if !errors.Is(err, boom) {
				t.Fatalf("WithSpan: %v", err)
			}
			if err := jaeger.Traced(ctx, "traced-ok", func(ctx context.Context) error {
				jaeger.RecordException(ctx, errors.New("recorded only"))
				return nil
			}, jaeger.WithSpanAttributes(map[string]any{"step": 2})); err != nil {
				t.Fatal(err)
			}
			_, failing := jaeger.StartSpan(ctx, "set-error", jaeger.WithTracer(tracer))
			jaeger.SetError(trace.ContextWithSpan(ctx, failing), errors.New("explicit"))
			failing.End()
			root.End()
			if err := h.ForceFlush(ctx); err != nil {
				t.Fatal(err)
			}

			tr := waitTrace(t, q, traceID, 4)
			if tr.TraceID != traceID || len(tr.Spans) != 4 {
				t.Fatalf("trace %+v", tr)
			}
			r := span(t, tr, rootName)
			if r.ParentSpanID != "" || r.Kind != jaeger.SpanKindServer || r.ServiceName != service ||
				r.Attributes["order.id"] != run || r.Attributes["int.attr"] != int64(42) || r.Attributes["double.attr"] != 1.5 ||
				r.Attributes["bool.attr"] != true || !reflect.DeepEqual(r.Attributes["array.attr"], []any{"a", "b"}) {
				t.Fatalf("root %+v", r)
			}
			db := span(t, tr, "db.query")
			if db.ParentSpanID != r.SpanID || db.Kind != jaeger.SpanKindClient || db.Status.Code != jaeger.SpanStatusError || db.Status.Message != "boom" {
				t.Fatalf("db %+v", db)
			}
			if !slices.ContainsFunc(db.Events, func(e jaeger.SpanEvent) bool { return e.Name == "cache.miss" && e.Attributes["cache.key"] == "k1" }) {
				t.Fatalf("events %+v", db.Events)
			}
			if len(db.Links) != 1 || db.Links[0].TraceID != other.SpanContext().TraceID().String() {
				t.Fatalf("links %+v", db.Links)
			}
			ok2 := span(t, tr, "traced-ok")
			if ok2.Status.Code == jaeger.SpanStatusError || ok2.Attributes["step"] != int64(2) ||
				!slices.ContainsFunc(ok2.Events, func(e jaeger.SpanEvent) bool { return e.Name == "exception" }) {
				t.Fatalf("traced %+v", ok2)
			}
			if se := span(t, tr, "set-error"); se.Status.Code != jaeger.SpanStatusError || se.Status.Message != "explicit" {
				t.Fatalf("set-error %+v", se)
			}
			if roots := tr.Tree(); len(roots) != 1 || len(roots[0].Children) != 3 {
				t.Fatalf("tree %+v", roots)
			}

			same, err := q.GetTrace(ctx, traceID)
			if err != nil || same == nil || len(same.Spans) != 4 {
				t.Fatalf("GetTrace %+v %v", same, err)
			}
			raw, err := q.GetTraceWith(ctx, traceID, jaeger.GetTraceOptions{Start: time.Now().Add(-time.Hour), End: time.Now().Add(time.Minute), RawTraces: true})
			if err != nil || raw == nil || len(raw.Spans) != 4 {
				t.Fatalf("GetTraceWith %+v %v", raw, err)
			}
			if missing, err := q.GetTrace(ctx, "ffffffffffffffffffffffffffffff01"); err != nil || missing != nil {
				t.Fatalf("missing trace: %+v %v", missing, err)
			}
			if req, err := q.RequireTrace(ctx, traceID); err != nil || req.TraceID != traceID {
				t.Fatalf("RequireTrace %v", err)
			}
			if _, err := q.RequireTrace(ctx, "ffffffffffffffffffffffffffffff01"); !errors.Is(err, jaeger.ErrNotFound) {
				t.Fatalf("RequireTrace missing: %v", err)
			}

			found, err := q.FindTraces(ctx, jaeger.TraceSearchQuery{
				Service: service, Operation: rootName, Tags: map[string]string{"order.id": run},
				MinDuration: time.Nanosecond, MaxDuration: time.Hour,
			})
			if err != nil || len(found) != 1 || found[0].TraceID != traceID || len(found[0].Spans) != 4 {
				t.Fatalf("FindTraces %+v %v", found, err)
			}
			if none, err := q.FindTraces(ctx, jaeger.TraceSearchQuery{Service: service, Operation: "nope"}); err != nil || len(none) != 0 {
				t.Fatalf("FindTraces no match %+v %v", none, err)
			}
			sums, err := q.SearchTraces(ctx, jaeger.TraceSearchQuery{Service: service, Limit: 10})
			if err != nil || !slices.ContainsFunc(sums, func(s jaeger.TraceSummary) bool {
				return s.TraceID == traceID && s.RootName == rootName && s.RootServiceName == service && s.SpanCount == 4 && s.DurationMicros > 0
			}) {
				t.Fatalf("SearchTraces %+v %v", sums, err)
			}

			services, err := q.ListServices(ctx)
			if err != nil || !slices.Contains(services, service) || !slices.Contains(services, "jaeger") {
				t.Fatalf("ListServices %v %v", services, err)
			}
			ops, err := q.ListOperations(ctx, service)
			if err != nil || !slices.Contains(ops, rootName) || !slices.Contains(ops, "db.query") {
				t.Fatalf("ListOperations %v %v", ops, err)
			}
			server, err := q.GetOperations(ctx, service, jaeger.ListOperationsOptions{SpanKind: "SERVER"})
			if err != nil || len(server) != 1 || server[0] != (jaeger.Operation{Name: rootName, SpanKind: jaeger.SpanKindServer}) {
				t.Fatalf("GetOperations %+v %v", server, err)
			}
			names, err := q.ListOperationsWith(ctx, service, jaeger.ListOperationsOptions{SpanKind: "server"})
			if err != nil || !slices.Equal(names, []string{rootName}) {
				t.Fatalf("ListOperationsWith %v %v", names, err)
			}
		})
	}
}

func TestDependenciesAndPropagation(t *testing.T) {
	c := setup(t)
	q := c.Query()
	ctx := context.Background()
	run := runID(t)
	parentSvc, childSvc := "sdk-go-dep-a-"+run, "sdk-go-dep-b-"+run
	a := startTracing(t, parentSvc, jaeger.ProtocolHTTPProtobuf)
	b := startTracing(t, childSvc, jaeger.ProtocolGRPC)

	// Service A calls service B: propagate through a header map and a carrier map.
	actx, call := a.Tracer("a").Start(ctx, "call-b", trace.WithSpanKind(trace.SpanKindClient))
	headers := http.Header{}
	a.Propagator().Inject(actx, propagationHeaders(headers))
	carrier := map[string]string{}
	prev := otel.GetTextMapPropagator()
	otel.SetTextMapPropagator(a.Propagator())
	t.Cleanup(func() { otel.SetTextMapPropagator(prev) })
	jaeger.Inject(actx, carrier)
	if carrier["traceparent"] == "" || carrier["traceparent"] != headers.Get("traceparent") {
		t.Fatalf("Inject %v / %v", carrier, headers)
	}
	h2 := http.Header{}
	jaeger.InjectHeaders(actx, h2)
	bctx := jaeger.ExtractHeaders(ctx, h2)
	if trace.SpanContextFromContext(jaeger.Extract(ctx, carrier)).TraceID() != call.SpanContext().TraceID() {
		t.Fatal("Extract")
	}
	_, handle := b.Tracer("b").Start(bctx, "handle-call", trace.WithSpanKind(trace.SpanKindServer))
	handle.End()
	call.End()
	_ = a.ForceFlush(ctx)
	_ = b.ForceFlush(ctx)

	tr := waitTrace(t, q, call.SpanContext().TraceID().String(), 2)
	if span(t, tr, "handle-call").ParentSpanID != call.SpanContext().SpanID().String() || span(t, tr, "handle-call").ServiceName != childSvc {
		t.Fatalf("cross-service parent %+v", tr.Spans)
	}
	want := jaeger.DependencyLink{Parent: parentSvc, Child: childSvc, CallCount: 1}
	links, err := q.GetDependencies(ctx, jaeger.DependencyOptions{Start: time.Now().Add(-time.Hour)})
	if err != nil || !slices.Contains(links, want) {
		t.Fatalf("GetDependencies %+v %v", links, err)
	}
	links, err = q.GetDependenciesAt(ctx, time.Now(), time.Hour)
	if err != nil || !slices.Contains(links, want) {
		t.Fatalf("GetDependenciesAt %+v %v", links, err)
	}
	old, err := q.GetDependenciesAt(ctx, time.Now().Add(-24*time.Hour), time.Minute)
	if err != nil || slices.Contains(old, want) {
		t.Fatalf("old window %+v %v", old, err)
	}
}

type propagationHeaders http.Header

func (h propagationHeaders) Get(k string) string { return http.Header(h).Get(k) }
func (h propagationHeaders) Set(k, v string)     { http.Header(h).Set(k, v) }
func (h propagationHeaders) Keys() []string {
	out := make([]string, 0, len(h))
	for k := range h {
		out = append(out, k)
	}
	return out
}

func TestHTTPAdapters(t *testing.T) {
	c := setup(t)
	ctx := context.Background()
	run := runID(t)
	service := "sdk-go-adapters-" + run
	h, err := jaeger.InitTracing(ctx, jaeger.TracingConfig{ServiceName: service, Batch: jaeger.TracingBatchOptions{ScheduledDelay: 100 * time.Millisecond}})
	if err != nil {
		t.Fatal(err)
	}
	prevTP, prevProp := otel.GetTracerProvider(), otel.GetTextMapPropagator()
	t.Cleanup(func() {
		_ = h.Shutdown(context.Background())
		otel.SetTracerProvider(prevTP)
		otel.SetTextMapPropagator(prevProp)
	})
	if otel.GetTracerProvider() != h.TracerProvider() || h.Config().ServiceName != service {
		t.Fatal("InitTracing must install the global provider")
	}
	srv := httptest.NewServer(jaeger.HTTPMiddleware("orders-api")(http.HandlerFunc(func(w http.ResponseWriter, _ *http.Request) {
		w.WriteHeader(http.StatusNoContent)
	})))
	defer srv.Close()
	client := &http.Client{Transport: jaeger.HTTPTransport(nil)}

	rctx, root := h.Tracer("adapters").Start(ctx, "adapters-root-"+run)
	req, _ := http.NewRequestWithContext(rctx, http.MethodGet, srv.URL+"/orders", nil)
	resp, err := client.Do(req)
	if err != nil {
		t.Fatal(err)
	}
	_ = resp.Body.Close()
	root.End()
	_ = h.ForceFlush(ctx)

	tr := waitTrace(t, c.Query(), root.SpanContext().TraceID().String(), 3)
	var clientSpan, serverSpan jaeger.Span
	for _, s := range tr.Spans {
		switch s.Kind {
		case jaeger.SpanKindClient:
			clientSpan = s
		case jaeger.SpanKindServer:
			serverSpan = s
		}
	}
	if clientSpan.ParentSpanID != root.SpanContext().SpanID().String() || serverSpan.ParentSpanID != clientSpan.SpanID {
		t.Fatalf("adapter spans %+v", tr.Spans)
	}
}

func TestRemoteSampling(t *testing.T) {
	c := setup(t)
	ctx := context.Background()
	fixtureURL := os.Getenv("GROW_JAEGER_SAMPLING_FIXTURE_URL")
	if fixtureURL == "" {
		t.Fatal("GROW_JAEGER_SAMPLING_FIXTURE_URL missing from /bootstrap/env")
	}

	// Stock all-in-one: probability 1 for every service.
	stock, err := c.SamplingStrategy(ctx, "sdk-go-stock-"+runID(t))
	if err != nil || stock.StrategyType != jaeger.StrategyProbabilistic || stock.SamplingRate != 1 {
		t.Fatalf("stock strategy %+v %v", stock, err)
	}

	// Strategies file of the jaeger-sampling fixture instance.
	ops, err := jaeger.FetchSamplingStrategy(ctx, fixtureURL, "sdk-sampling-ops", jaeger.CommonConfig{})
	if err != nil || ops.SamplingRate != 0.8 {
		t.Fatalf("ops strategy %+v %v", ops, err)
	}
	if r, ok := ops.OperationRate("GET /fast"); !ok || r != 0.2 {
		t.Fatalf("GET /fast %v %v", r, ok)
	}
	if _, err := jaeger.FetchSamplingStrategy(ctx, fixtureURL, " ", jaeger.CommonConfig{}); !errors.Is(err, jaeger.ErrValidation) {
		t.Fatalf("empty service: %v", err)
	}

	sampled := func(s sdktrace.Sampler, name string) bool {
		return s.ShouldSample(sdktrace.SamplingParameters{ParentContext: ctx, TraceID: trace.TraceID{0xab, 1}, Name: name}).Decision == sdktrace.RecordAndSample
	}

	rate, err := jaeger.NewRemoteSampler(fixtureURL, "sdk-sampling-rate", time.Hour)
	if err != nil {
		t.Fatal(err)
	}
	defer func() { _ = rate.Close() }()
	if err := rate.Refresh(ctx); err != nil {
		t.Fatal(err)
	}
	if st, ok := rate.Strategy(); !ok || st.StrategyType != jaeger.StrategyRateLimiting || st.MaxTracesPerSecond != 5 {
		t.Fatalf("rate strategy %+v %v", st, ok)
	}
	n := 0
	for i := 0; i < 50; i++ {
		if sampled(rate, "op") {
			n++
		}
	}
	if n < 5 || n > 7 || sampled(rate, "/health") {
		t.Fatalf("rate limiting sampled %d of 50", n)
	}
	if !regexp.MustCompile(`sdk-sampling-rate`).MatchString(rate.Description()) {
		t.Fatal(rate.Description())
	}

	prob, err := jaeger.NewRemoteSampler(fixtureURL, "sdk-sampling-prob", time.Hour)
	if err != nil {
		t.Fatal(err)
	}
	defer func() { _ = prob.Close() }()
	if err := prob.Refresh(ctx); err != nil {
		t.Fatal(err)
	}
	if st, _ := prob.Strategy(); st.SamplingRate != 0.25 {
		t.Fatalf("prob strategy %+v", st)
	}

	// The sampler drives InitTracing: "/health" (rate 0) is dropped, the stock
	// strategy (rate 1) keeps everything and the span reaches Jaeger.
	opsSampler, err := c.NewRemoteSampler("sdk-sampling-ops", time.Hour)
	if err != nil {
		t.Fatal(err)
	}
	fixtureOps, err := jaeger.NewRemoteSampler(fixtureURL, "sdk-sampling-ops", time.Hour)
	if err != nil {
		t.Fatal(err)
	}
	defer func() { _ = fixtureOps.Close() }()
	if err := fixtureOps.Refresh(ctx); err != nil {
		t.Fatal(err)
	}
	dropping := startTracing(t, "sdk-go-sampled-"+runID(t), jaeger.ProtocolHTTPProtobuf, jaeger.WithSampler(fixtureOps))
	if _, s := dropping.Tracer("t").Start(ctx, "/health"); s.IsRecording() {
		t.Fatal("/health must be dropped by the remote strategy")
	}
	if err := opsSampler.Refresh(ctx); err != nil {
		t.Fatal(err)
	}
	service := "sdk-go-remote-" + runID(t)
	h := startTracing(t, service, jaeger.ProtocolHTTPProtobuf, jaeger.WithSampler(opsSampler))
	_, s := h.Tracer("t").Start(ctx, "sampled-op")
	s.End()
	_ = h.ForceFlush(ctx)
	if tr := waitTrace(t, c.Query(), s.SpanContext().TraceID().String(), 1); tr.Spans[0].ServiceName != service {
		t.Fatalf("remote-sampled span %+v", tr.Spans)
	}

	// OTEL_TRACES_SAMPLER=parentbased_jaeger_remote: InitTracing builds and owns the
	// sampler (URL from the sampler arg) and applies the fixture's per-operation rule.
	t.Setenv("OTEL_TRACES_SAMPLER", "parentbased_jaeger_remote")
	t.Setenv("OTEL_TRACES_SAMPLER_ARG", "endpoint="+fixtureURL+",initialSamplingRate=0")
	envHandle := startTracing(t, "sdk-sampling-ops", jaeger.ProtocolHTTPProtobuf)
	remote := envHandle.RemoteSampler()
	if remote == nil || remote.Refresh(ctx) != nil {
		t.Fatal("jaeger_remote from the environment did not start a sampler")
	}
	if _, s := envHandle.Tracer("t").Start(ctx, "/health"); s.IsRecording() {
		t.Fatal("env remote sampler must drop /health")
	}
	_, envSpan := envHandle.Tracer("t").Start(ctx, "GET /fast")
	envSpan.End()
	if envSpan.SpanContext().IsSampled() {
		_ = envHandle.ForceFlush(ctx)
		waitTrace(t, c.Query(), envSpan.SpanContext().TraceID().String(), 1)
	}
	_ = c.Close() // stops opsSampler too
}

func TestSamplingPresetsAndConfig(t *testing.T) {
	setup(t)
	cfg, err := jaeger.TracingConfigFromEnv(jaeger.TracingConfig{ServiceName: "x"}, nil)
	if err != nil || cfg.Endpoint == jaeger.DefaultOTLPHTTPEndpoint || cfg.Protocol != jaeger.DefaultTracingProtocol {
		t.Fatalf("TracingConfigFromEnv %s %v", cfg, err)
	}
	run := runID(t)
	exp, err := jaeger.NewExporter(context.Background(), jaeger.TracingConfig{ServiceName: "sdk-go-exporter-" + run})
	if err != nil {
		t.Fatal(err)
	}
	h, err := jaeger.InitTracingWith(context.Background(), jaeger.SamplingDevelopment.Apply(jaeger.TracingConfig{
		ServiceName: "sdk-go-exporter-" + run, DisableSetGlobal: true,
	}), func(context.Context, jaeger.TracingConfig) (sdktrace.SpanExporter, error) { return exp, nil })
	if err != nil {
		t.Fatal(err)
	}
	_, s := h.Tracer("t").Start(context.Background(), "preset-op")
	s.End()
	_ = h.Shutdown(context.Background())
	c := setup(t)
	if tr := waitTrace(t, c.Query(), s.SpanContext().TraceID().String(), 1); tr.Spans[0].Name != "preset-op" {
		t.Fatalf("%+v", tr)
	}
	if p := jaeger.SamplingRatio(0.5).Apply(jaeger.TracingConfig{ServiceName: "x"}); p.Validate() != nil || jaeger.SamplingOff.Sampler != jaeger.SamplerAlwaysOff ||
		jaeger.SamplingProduction.Ratio != 0.1 || jaeger.SamplingFollowParent.Sampler != jaeger.SamplerParentBasedAlwaysOn {
		t.Fatal("presets")
	}
	if _, err := jaeger.ParseSamplingStrategy([]byte(`{"probabilisticSampling":{"samplingRate":0.5}}`)); err != nil {
		t.Fatal(err)
	}
}
