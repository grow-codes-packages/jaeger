package jaeger

import (
	"encoding/json"
	"errors"
	"net/http"
	"net/url"
	"reflect"
	"strconv"
	"strings"
	"testing"
	"time"
)

const fixtureTraceID = "31d0f4ed0c5cf87b3a63afbe7b0dc8d1"

var fixedNow = time.Date(2026, 11, 18, 21, 0, 0, 0, time.UTC)

// newQueryClient returns the query client of a test client with a fixed clock.
func newQueryClient(t *testing.T, url string, mutate func(*Config)) *QueryClient {
	t.Helper()
	q := newTestClient(t, url, mutate).Query()
	q.now = func() time.Time { return fixedNow }
	return q
}

// router dispatches by path; unknown paths get Jaeger's plain-text 404.
func router(routes map[string]func(w http.ResponseWriter, r *http.Request, n int)) func(w http.ResponseWriter, r *http.Request, n int) {
	return func(w http.ResponseWriter, r *http.Request, n int) {
		if h, ok := routes[r.URL.Path]; ok {
			h(w, r, n)
			return
		}
		writeText(w, http.StatusNotFound, "404 page not found\n")
	}
}

func spanByName(t *testing.T, tr *Trace, name string) Span {
	t.Helper()
	for _, s := range tr.Spans {
		if s.Name == name {
			return s
		}
	}
	t.Fatalf("no span %q in %+v", name, tr.Spans)
	return Span{}
}

func checkFixtureTrace(t *testing.T, tr *Trace) {
	t.Helper()
	if tr == nil || tr.TraceID != fixtureTraceID || len(tr.Spans) != 3 {
		t.Fatalf("trace %+v", tr)
	}
	if tr.Spans[0].Name != "GET /orders" || tr.Spans[1].Name != "handle /orders" || tr.Spans[2].Name != "db.query" {
		t.Fatalf("spans not sorted: %v %v %v", tr.Spans[0].Name, tr.Spans[1].Name, tr.Spans[2].Name)
	}
	root := spanByName(t, tr, "GET /orders")
	if root.ParentSpanID != "" || root.Kind != SpanKindClient || root.Status.Code != SpanStatusOK ||
		root.ServiceName != "sdk-gw-86837b62" || root.StartMicros != 1795034918142512 || root.DurationMicros != 1200000 ||
		root.Attributes["order.id"] != "86837b62" {
		t.Fatalf("root %+v", root)
	}
	h := spanByName(t, tr, "handle /orders")
	if h.ParentSpanID != "55bc22d6caba571a" || h.Kind != SpanKindServer || h.ServiceName != "sdk-wk-86837b62" ||
		h.Status.Code != SpanStatusError || h.Status.Message != "boom" {
		t.Fatalf("handler %+v", h)
	}
	if h.Attributes["int.attr"] != int64(42) || h.Attributes["double.attr"] != 1.5 || h.Attributes["bool.attr"] != true || h.Attributes["str.attr"] != "v" {
		t.Fatalf("attributes %#v", h.Attributes)
	}
	if len(h.Events) != 1 || h.Events[0].Name != "cache.miss" || h.Events[0].TimeMicros != 1795034918342512 || h.Events[0].Attributes["cache.key"] != "k1" {
		t.Fatalf("events %+v", h.Events)
	}
	if len(h.Links) != 1 || h.Links[0].TraceID != "85092f96d876e30b507cfd1361b0325e" || h.Links[0].SpanID != "00f067aa0ba902b7" {
		t.Fatalf("links %+v", h.Links)
	}
	db := spanByName(t, tr, "db.query")
	if db.ParentSpanID != "02811a5d0fecd819" || db.Kind != SpanKindInternal || db.Status.Code != SpanStatusUnset || db.DurationMicros != 5000 {
		t.Fatalf("db %+v", db)
	}
}

func TestQueryGetTrace(t *testing.T) {
	t.Run("v3", func(t *testing.T) {
		ts := newTestServer(t, router(map[string]func(http.ResponseWriter, *http.Request, int){
			"/api/v3/traces/" + fixtureTraceID: serveFixture(t, "getTrace.v3_ok"),
		}))
		q := newQueryClient(t, ts.URL, nil)
		tr, err := q.GetTrace(t.Context(), strings.ToUpper(fixtureTraceID))
		if err != nil {
			t.Fatal(err)
		}
		checkFixtureTrace(t, tr)
		h := spanByName(t, tr, "handle /orders")
		if !reflect.DeepEqual(h.Attributes["array.attr"], []any{"a", "b"}) || h.Links[0].Attributes["link.kind"] != "follows" {
			t.Fatalf("array/link attrs %#v %#v", h.Attributes, h.Links)
		}
		req := ts.request(0)
		if req.URL.Query().Get("startTime") != fixedNow.Add(-DefaultGetTraceWindow).Format(time.RFC3339Nano) ||
			req.URL.Query().Get("endTime") != fixedNow.Format(time.RFC3339Nano) || req.URL.Query().Has("rawTraces") {
			t.Fatalf("query %s", req.URL.RawQuery)
		}
		if req.Header.Get("Authorization") != "Bearer "+testToken {
			t.Fatal("token not sent")
		}
	})

	t.Run("short id", func(t *testing.T) {
		ts := newTestServer(t, router(map[string]func(http.ResponseWriter, *http.Request, int){
			"/api/v3/traces/0000000000000000b94006dad19af3af": serveFixture(t, "getTrace.v3_short_id"),
		}))
		tr, err := newQueryClient(t, ts.URL, nil).GetTrace(t.Context(), "B94006DAD19AF3AF")
		if err != nil || tr == nil || tr.TraceID != "0000000000000000b94006dad19af3af" || len(tr.Spans) != 1 {
			t.Fatalf("%+v %v", tr, err)
		}
	})

	t.Run("not found and invalid", func(t *testing.T) {
		ts := newTestServer(t, router(map[string]func(http.ResponseWriter, *http.Request, int){
			"/api/v3/traces/ffffffffffffffffffffffffffffffff": serveFixture(t, "getTrace.v3_not_found"),
			"/api/v3/traces/00000000000000000000000000000abc": serveFixture(t, "getTrace.v3_invalid_id"),
		}))
		q := newQueryClient(t, ts.URL, nil)
		if tr, err := q.GetTrace(t.Context(), "ffffffffffffffffffffffffffffffff"); err != nil || tr != nil {
			t.Fatalf("not found: %+v %v", tr, err)
		}
		_, err := q.GetTrace(t.Context(), "abc")
		if !errors.Is(err, ErrValidation) || !strings.Contains(err.Error(), "malformed parameter") || asError(t, err).HTTPStatus != 400 {
			t.Fatalf("400: %v", err)
		}
		calls := ts.calls.Load()
		for _, bad := range []string{"", "xyz", strings.Repeat("a", 33), strings.Repeat("0", 32)} {
			if _, err := q.GetTrace(t.Context(), bad); !errors.Is(err, ErrValidation) {
				t.Fatalf("%q: %v", bad, err)
			}
		}
		if ts.calls.Load() != calls {
			t.Fatal("invalid ids must be rejected locally")
		}
		if _, err := q.GetTraceWith(t.Context(), fixtureTraceID, GetTraceOptions{Start: fixedNow, End: fixedNow.Add(-time.Hour)}); !errors.Is(err, ErrValidation) {
			t.Fatalf("window: %v", err)
		}
	})

	t.Run("options", func(t *testing.T) {
		ts := newTestServer(t, router(map[string]func(http.ResponseWriter, *http.Request, int){
			"/api/v3/traces/" + fixtureTraceID: serveFixture(t, "getTrace.v3_ok"),
		}))
		start := fixedNow.Add(-2 * time.Hour)
		tr, err := newQueryClient(t, ts.URL, nil).GetTraceWith(t.Context(), fixtureTraceID, GetTraceOptions{Start: start, RawTraces: true})
		if err != nil || tr == nil {
			t.Fatalf("%+v %v", tr, err)
		}
		q := ts.request(0).URL.Query()
		if q.Get("rawTraces") != "true" || q.Get("startTime") != start.Format(time.RFC3339Nano) || q.Get("endTime") != fixedNow.Format(time.RFC3339Nano) {
			t.Fatalf("query %v", q)
		}
	})

	t.Run("legacy fallback", func(t *testing.T) {
		ts := newTestServer(t, router(map[string]func(http.ResponseWriter, *http.Request, int){
			"/api/traces/" + fixtureTraceID:                serveFixture(t, "getTraceLegacy.ok"),
			"/api/traces/ffffffffffffffffffffffffffffffff": serveFixture(t, "getTraceLegacy.not_found"),
		}))
		q := newQueryClient(t, ts.URL, nil)
		tr, err := q.GetTraceWith(t.Context(), fixtureTraceID, GetTraceOptions{RawTraces: true})
		if err != nil {
			t.Fatal(err)
		}
		checkFixtureTrace(t, tr)
		h := spanByName(t, tr, "handle /orders")
		for _, k := range []string{"span.kind", "otel.status_code", "otel.status_description", "error", "otel.scope.name"} {
			if _, ok := h.Attributes[k]; ok {
				t.Fatalf("synthetic tag %q kept: %#v", k, h.Attributes)
			}
		}
		if h.Attributes["array.attr"] != `["a","b"]` {
			t.Fatalf("legacy array %#v", h.Attributes["array.attr"])
		}
		legacy := ts.request(1).URL
		if legacy.Path != "/api/traces/"+fixtureTraceID || legacy.Query().Get("raw") != "true" ||
			legacy.Query().Get("end") != strconv.FormatInt(fixedNow.UnixMicro(), 10) {
			t.Fatalf("legacy request %s", legacy)
		}
		if tr, err := q.GetTrace(t.Context(), "ffffffffffffffffffffffffffffffff"); err != nil || tr != nil {
			t.Fatalf("legacy not found: %+v %v", tr, err)
		}
		if _, err := q.GetTrace(t.Context(), "1234"); !errors.Is(err, ErrUnsupported) {
			t.Fatalf("both routes missing: %v", err)
		}
	})

	t.Run("legacy errors", func(t *testing.T) {
		ts := newTestServer(t, router(map[string]func(http.ResponseWriter, *http.Request, int){
			"/api/traces/" + fixtureTraceID:                serveFixture(t, "getTraceLegacy.invalid_id"),
			"/api/traces/eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee": func(w http.ResponseWriter, _ *http.Request, _ int) { writeJSON(w, 200, "{nope") },
			"/api/traces/dddddddddddddddddddddddddddddddd": serveFixture(t, "getTraceLegacy.ok"),
		}))
		q := newQueryClient(t, ts.URL, func(cfg *Config) { cfg.Retry = fastRetry(0) })
		if _, err := q.GetTrace(t.Context(), fixtureTraceID); !errors.Is(err, ErrValidation) {
			t.Fatalf("legacy 400: %v", err)
		}
		if _, err := q.GetTrace(t.Context(), "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"); !errors.Is(err, ErrUnavailable) {
			t.Fatalf("legacy bad JSON: %v", err)
		}
		if tr, err := q.GetTrace(t.Context(), "dddddddddddddddddddddddddddddddd"); err != nil || tr != nil {
			t.Fatalf("legacy other id: %+v %v", tr, err)
		}
	})

	t.Run("retries and errors", func(t *testing.T) {
		ok := loadFixture(t, "getTrace.v3_ok")
		ts := newTestServer(t, func(w http.ResponseWriter, _ *http.Request, n int) {
			if n == 1 {
				writeText(w, http.StatusServiceUnavailable, "busy")
				return
			}
			ok.serve(w)
		})
		tr, err := newQueryClient(t, ts.URL, nil).GetTrace(t.Context(), fixtureTraceID)
		if err != nil || tr == nil || ts.calls.Load() != 2 {
			t.Fatalf("retry: %+v %v calls=%d", tr, err, ts.calls.Load())
		}

		down := newTestServer(t, status(http.StatusBadGateway))
		if _, err := newQueryClient(t, down.URL, func(cfg *Config) { cfg.Retry = fastRetry(1) }).GetTrace(t.Context(), fixtureTraceID); !errors.Is(err, ErrUnavailable) || down.calls.Load() != 3 {
			t.Fatalf("exhausted: %v calls=%d", err, down.calls.Load())
		}

		flip := newTestServer(t, func(w http.ResponseWriter, _ *http.Request, n int) {
			if n == 1 {
				writeText(w, http.StatusServiceUnavailable, "busy")
				return
			}
			writeText(w, http.StatusNotFound, "404 page not found\n")
		})
		if tr, err := newQueryClient(t, flip.URL, nil).GetTrace(t.Context(), fixtureTraceID); err != nil || tr != nil {
			t.Fatalf("retry then 404: %+v %v", tr, err)
		}

		garbage := newTestServer(t, func(w http.ResponseWriter, _ *http.Request, _ int) { writeJSON(w, 200, "[") })
		if _, err := newQueryClient(t, garbage.URL, nil).GetTrace(t.Context(), fixtureTraceID); !errors.Is(err, ErrUnavailable) {
			t.Fatalf("bad JSON: %v", err)
		}
		other := newTestServer(t, serveFixture(t, "getTrace.v3_ok"))
		if tr, err := newQueryClient(t, other.URL, nil).GetTrace(t.Context(), "abcdef"); err != nil || tr != nil {
			t.Fatalf("other trace in body: %+v %v", tr, err)
		}

		ctx, cancel := contextWithCancel(t)
		cancel()
		if _, err := newQueryClient(t, other.URL, nil).GetTrace(ctx, fixtureTraceID); !errors.Is(err, ErrCancelled) {
			t.Fatalf("cancelled: %v", err)
		}
	})

	t.Run("wait for trace", func(t *testing.T) {
		notFound, ok := loadFixture(t, "getTrace.v3_not_found"), loadFixture(t, "getTrace.v3_ok")
		ts := newTestServer(t, func(w http.ResponseWriter, _ *http.Request, n int) {
			if n == 1 {
				notFound.serve(w)
				return
			}
			ok.serve(w)
		})
		tr, err := newQueryClient(t, ts.URL, nil).WaitForTrace(t.Context(), fixtureTraceID, WaitForTraceOptions{Interval: 5 * time.Millisecond, MinSpans: 3})
		if err != nil || len(tr.Spans) != 3 {
			t.Fatalf("%+v %v", tr, err)
		}
	})
}

func TestQueryFindAndSearch(t *testing.T) {
	t.Run("find traces", func(t *testing.T) {
		ts := newTestServer(t, router(map[string]func(http.ResponseWriter, *http.Request, int){
			"/api/v3/traces": serveFixture(t, "findTraces.by_service"),
		}))
		q := newQueryClient(t, ts.URL, nil)
		traces, err := q.FindTraces(t.Context(), TraceSearchQuery{
			Service: "sdk-wk-86837b62", Operation: "handle /orders", Tags: map[string]string{"order.id": "86837b62"},
			MinDuration: 1500 * time.Microsecond, MaxDuration: 2 * time.Second,
		})
		if err != nil || len(traces) != 1 {
			t.Fatalf("%+v %v", traces, err)
		}
		checkFixtureTrace(t, &traces[0])
		v := ts.request(0).URL.Query()
		want := url.Values{
			"query.serviceName":   {"sdk-wk-86837b62"},
			"query.operationName": {"handle /orders"},
			"query.attributes":    {`{"order.id":"86837b62"}`},
			"query.durationMin":   {"1500us"},
			"query.durationMax":   {"2000000us"},
			"query.searchDepth":   {"20"},
			"query.startTimeMin":  {fixedNow.Add(-time.Hour).Format(time.RFC3339Nano)},
			"query.startTimeMax":  {fixedNow.Format(time.RFC3339Nano)},
		}
		if !reflect.DeepEqual(v, want) {
			t.Fatalf("query\n got %v\nwant %v", v, want)
		}
	})

	t.Run("find grouping and window", func(t *testing.T) {
		// Jaeger pools all matching traces into one TracesData body.
		var a, b map[string]map[string][]any
		_ = json.Unmarshal(loadFixture(t, "getTrace.v3_ok").body(), &a)
		_ = json.Unmarshal(loadFixture(t, "getTrace.v3_short_id").body(), &b)
		a["result"]["resourceSpans"] = append(b["result"]["resourceSpans"], a["result"]["resourceSpans"]...)
		pooled, _ := json.Marshal(a)
		ts := newTestServer(t, func(w http.ResponseWriter, _ *http.Request, _ int) { writeJSON(w, 200, string(pooled)) })
		q := newQueryClient(t, ts.URL, nil)
		start, end := fixedNow.Add(-3*time.Hour), fixedNow.Add(-time.Hour)
		traces, err := q.FindTraces(t.Context(), TraceSearchQuery{Service: "sdk-many-86837b62", Start: start, End: end, Limit: 2})
		if err != nil || len(traces) != 2 || traces[0].TraceID != "0000000000000000b94006dad19af3af" || traces[1].TraceID != fixtureTraceID || len(traces[1].Spans) != 3 {
			t.Fatalf("%+v %v", traces, err)
		}
		v := ts.request(0).URL.Query()
		if v.Get("query.searchDepth") != "2" || v.Get("query.startTimeMin") != start.Format(time.RFC3339Nano) || v.Get("query.startTimeMax") != end.Format(time.RFC3339Nano) {
			t.Fatalf("query %v", v)
		}
		if _, err := q.FindTraces(t.Context(), TraceSearchQuery{End: end}); err != nil {
			t.Fatal(err)
		}
		if got := ts.request(1).URL.Query().Get("query.startTimeMin"); got != end.Add(-time.Hour).Format(time.RFC3339Nano) {
			t.Fatalf("default start %s", got)
		}
	})

	t.Run("find no match and errors", func(t *testing.T) {
		ts := newTestServer(t, router(map[string]func(http.ResponseWriter, *http.Request, int){
			"/api/v3/traces": func(w http.ResponseWriter, r *http.Request, _ int) {
				switch r.URL.Query().Get("query.serviceName") {
				case "bad":
					loadFixture(t, "findTraces.bad_attributes").serve(w)
				default:
					loadFixture(t, "findTraces.no_match").serve(w)
				}
			},
		}))
		q := newQueryClient(t, ts.URL, nil)
		if traces, err := q.FindTraces(t.Context(), TraceSearchQuery{Service: "x"}); err != nil || traces == nil || len(traces) != 0 {
			t.Fatalf("no match: %+v %v", traces, err)
		}
		if _, err := q.FindTraces(t.Context(), TraceSearchQuery{Service: "bad"}); !errors.Is(err, ErrValidation) || !strings.Contains(err.Error(), "malformed parameter") {
			t.Fatalf("400: %v", err)
		}
		missing := newTestServer(t, status(http.StatusNotFound))
		if _, err := newQueryClient(t, missing.URL, nil).FindTraces(t.Context(), TraceSearchQuery{}); !errors.Is(err, ErrUnsupported) {
			t.Fatalf("route missing: %v", err)
		}
		calls := ts.calls.Load()
		for _, bad := range []TraceSearchQuery{
			{MinDuration: -time.Second},
			{MaxDuration: -time.Second},
			{MinDuration: 2 * time.Second, MaxDuration: time.Second},
			{Start: fixedNow, End: fixedNow.Add(-time.Minute)},
		} {
			if _, err := q.FindTraces(t.Context(), bad); !errors.Is(err, ErrValidation) {
				t.Fatalf("%+v: %v", bad, err)
			}
			if _, err := q.SearchTraces(t.Context(), bad); !errors.Is(err, ErrValidation) {
				t.Fatalf("search %+v: %v", bad, err)
			}
		}
		if ts.calls.Load() != calls {
			t.Fatal("invalid queries must be rejected locally")
		}
		if _, err := newQueryClient(t, ts.URL, nil).FindTraces(t.Context(), TraceSearchQuery{Service: "bad"}); err == nil {
			t.Fatal("expected error")
		}
	})

	t.Run("search traces", func(t *testing.T) {
		ts := newTestServer(t, router(map[string]func(http.ResponseWriter, *http.Request, int){
			"/api/v3/trace-summaries": func(w http.ResponseWriter, r *http.Request, _ int) {
				switch r.URL.Query().Get("query.serviceName") {
				case "none":
					loadFixture(t, "findTraceSummaries.empty").serve(w)
				case "gone":
					loadFixture(t, "findTraces.no_match").serve(w)
				case "garbage":
					writeJSON(w, 200, "{")
				default:
					loadFixture(t, "findTraceSummaries.ok").serve(w)
				}
			},
		}))
		q := newQueryClient(t, ts.URL, nil)
		sums, err := q.SearchTraces(t.Context(), TraceSearchQuery{Service: "sdk-gw-86837b62"})
		if err != nil || len(sums) != 2 {
			t.Fatalf("%+v %v", sums, err)
		}
		want := TraceSummary{TraceID: fixtureTraceID, RootServiceName: "sdk-gw-86837b62", RootName: "GET /orders", StartMicros: 1795034918142512, DurationMicros: 1200000, SpanCount: 3}
		if sums[0] != want || sums[1].TraceID != "0000000000000000b94006dad19af3af" || sums[1].DurationMicros != 5000 {
			t.Fatalf("summaries %+v", sums)
		}
		if ts.request(0).URL.Path != "/api/v3/trace-summaries" {
			t.Fatal("path")
		}
		for _, svc := range []string{"none", "gone"} {
			if sums, err := q.SearchTraces(t.Context(), TraceSearchQuery{Service: svc}); err != nil || sums == nil || len(sums) != 0 {
				t.Fatalf("%s: %+v %v", svc, sums, err)
			}
		}
		if _, err := q.SearchTraces(t.Context(), TraceSearchQuery{Service: "garbage"}); !errors.Is(err, ErrUnavailable) {
			t.Fatalf("bad JSON: %v", err)
		}
		missing := newTestServer(t, status(http.StatusNotFound))
		if _, err := newQueryClient(t, missing.URL, nil).SearchTraces(t.Context(), TraceSearchQuery{}); !errors.Is(err, ErrUnsupported) {
			t.Fatalf("route missing: %v", err)
		}
		down := newTestServer(t, status(http.StatusBadRequest))
		if _, err := newQueryClient(t, down.URL, nil).SearchTraces(t.Context(), TraceSearchQuery{}); !errors.Is(err, ErrValidation) {
			t.Fatalf("400: %v", err)
		}
	})
}

func TestQueryServicesAndOperations(t *testing.T) {
	ts := newTestServer(t, router(map[string]func(http.ResponseWriter, *http.Request, int){
		"/api/v3/services": serveFixture(t, "listServices.ok"),
		"/api/v3/operations": func(w http.ResponseWriter, r *http.Request, _ int) {
			switch {
			case r.URL.Query().Get("service") == "garbage":
				writeJSON(w, 200, "{")
			case r.URL.Query().Get("service") == "dup":
				writeJSON(w, 200, `{"operations":[{"name":"b","spanKind":"SERVER"},{"name":"a","spanKind":"client"},{"name":"b","spanKind":"client"}]}`)
			case r.URL.Query().Get("spanKind") == "server":
				loadFixture(t, "listOperations.span_kind_server").serve(w)
			case r.URL.Query().Get("service") == "none":
				loadFixture(t, "listOperations.unknown_service").serve(w)
			default:
				loadFixture(t, "listOperations.ok").serve(w)
			}
		},
	}))
	q := newQueryClient(t, ts.URL, nil)

	services, err := q.ListServices(t.Context())
	if err != nil || len(services) != 10 || services[0] != "grow-bootstrap" || !strings.Contains(strings.Join(services, ","), "jaeger") {
		t.Fatalf("services %v %v", services, err)
	}
	for i := 1; i < len(services); i++ {
		if services[i-1] > services[i] {
			t.Fatalf("not sorted: %v", services)
		}
	}

	ops, err := q.ListOperations(t.Context(), "sdk-wk-86837b62")
	if err != nil || !reflect.DeepEqual(ops, []string{"db.query", "handle /orders"}) {
		t.Fatalf("ops %v %v", ops, err)
	}
	if ops, err := q.ListOperations(t.Context(), "dup"); err != nil || !reflect.DeepEqual(ops, []string{"a", "b"}) {
		t.Fatalf("dedupe %v %v", ops, err)
	}
	detailed, err := q.ListOperationsWith(t.Context(), "dup", ListOperationsOptions{})
	if err != nil || len(detailed) != 3 || detailed[1] != (Operation{Name: "b", SpanKind: SpanKindClient}) || detailed[2].SpanKind != SpanKindServer {
		t.Fatalf("detailed %+v %v", detailed, err)
	}
	server, err := q.ListOperationsWith(t.Context(), "sdk-wk-86837b62", ListOperationsOptions{SpanKind: "SERVER"})
	if err != nil || len(server) != 1 || server[0] != (Operation{Name: "handle /orders", SpanKind: SpanKindServer}) {
		t.Fatalf("server ops %+v %v", server, err)
	}
	if got := ts.request(int(ts.calls.Load()) - 1).URL.Query().Get("spanKind"); got != "server" {
		t.Fatalf("spanKind sent as %q", got)
	}
	if ops, err := q.ListOperations(t.Context(), "none"); err != nil || ops == nil || len(ops) != 0 {
		t.Fatalf("unknown service %v %v", ops, err)
	}

	calls := ts.calls.Load()
	if _, err := q.ListOperations(t.Context(), " "); !errors.Is(err, ErrValidation) {
		t.Fatalf("empty service: %v", err)
	}
	if _, err := q.ListOperationsWith(t.Context(), "s", ListOperationsOptions{SpanKind: SpanKindUnspecified}); !errors.Is(err, ErrValidation) {
		t.Fatalf("bad kind: %v", err)
	}
	if ts.calls.Load() != calls {
		t.Fatal("invalid input must be rejected locally")
	}
	if _, err := q.ListOperations(t.Context(), "garbage"); !errors.Is(err, ErrUnavailable) {
		t.Fatalf("bad JSON: %v", err)
	}

	missing := newQueryClient(t, newTestServer(t, status(http.StatusNotFound)).URL, nil)
	if _, err := missing.ListServices(t.Context()); !errors.Is(err, ErrUnsupported) {
		t.Fatalf("services route missing: %v", err)
	}
	if _, err := missing.ListOperations(t.Context(), "s"); !errors.Is(err, ErrUnsupported) {
		t.Fatalf("operations route missing: %v", err)
	}
	garbage := newQueryClient(t, newTestServer(t, func(w http.ResponseWriter, _ *http.Request, _ int) { writeJSON(w, 200, "{") }).URL, nil)
	if _, err := garbage.ListServices(t.Context()); !errors.Is(err, ErrUnavailable) {
		t.Fatalf("bad JSON: %v", err)
	}
	failing := newQueryClient(t, newTestServer(t, status(http.StatusInternalServerError)).URL, func(cfg *Config) { cfg.Retry = fastRetry(0) })
	if _, err := failing.ListServices(t.Context()); !errors.Is(err, ErrUnavailable) {
		t.Fatalf("500: %v", err)
	}
	if _, err := failing.ListOperations(t.Context(), "s"); !errors.Is(err, ErrUnavailable) {
		t.Fatalf("500: %v", err)
	}
}

func TestQueryDependencies(t *testing.T) {
	ts := newTestServer(t, router(map[string]func(http.ResponseWriter, *http.Request, int){
		"/api/dependencies": func(w http.ResponseWriter, r *http.Request, _ int) {
			switch r.URL.Query().Get("lookback") {
			case "1":
				loadFixture(t, "getDependencies.bad_lookback").serve(w)
			case "2":
				writeText(w, http.StatusNotFound, `{"data":null,"errors":[{"code":404,"msg":"none"}]}`)
			case "3":
				writeJSON(w, 200, "{")
			case "60000":
				loadFixture(t, "getDependencies.outside_window").serve(w)
			default:
				loadFixture(t, "getDependencies.ok").serve(w)
			}
		},
	}))
	q := newQueryClient(t, ts.URL, nil)

	links, err := q.GetDependencies(t.Context(), DependencyOptions{})
	want := []DependencyLink{{Parent: "sdk-pb-86837b62", Child: "sdk-pb-86837b62-wk", CallCount: 1}, {Parent: "sdk-gw-86837b62", Child: "sdk-wk-86837b62", CallCount: 1}}
	if err != nil || !reflect.DeepEqual(links, want) {
		t.Fatalf("links %+v %v", links, err)
	}
	v := ts.request(0).URL.Query()
	if v.Get("endTs") != strconv.FormatInt(fixedNow.UnixMilli(), 10) || v.Get("lookback") != "86400000" {
		t.Fatalf("default window %v", v)
	}
	start, end := fixedNow.Add(-2*time.Hour), fixedNow.Add(-time.Hour)
	if _, err := q.GetDependencies(t.Context(), DependencyOptions{Start: start, End: end}); err != nil {
		t.Fatal(err)
	}
	if v := ts.request(1).URL.Query(); v.Get("endTs") != strconv.FormatInt(end.UnixMilli(), 10) || v.Get("lookback") != "3600000" {
		t.Fatalf("window %v", v)
	}
	if links, err := q.GetDependenciesAt(t.Context(), end, time.Minute); err != nil || links == nil || len(links) != 0 {
		t.Fatalf("outside window %+v %v", links, err)
	}
	if _, err := q.GetDependenciesAt(t.Context(), time.Time{}, 0); err != nil {
		t.Fatal(err)
	}
	if v := ts.request(3).URL.Query(); v.Get("endTs") != strconv.FormatInt(fixedNow.UnixMilli(), 10) || v.Get("lookback") != "86400000" {
		t.Fatalf("at defaults %v", v)
	}
	if _, err := q.GetDependenciesAt(t.Context(), end, time.Millisecond); !errors.Is(err, ErrValidation) {
		t.Fatalf("server 400: %v", err)
	}
	if links, err := q.GetDependenciesAt(t.Context(), end, 2*time.Millisecond); err != nil || len(links) != 0 {
		t.Fatalf("JSON 404: %+v %v", links, err)
	}
	if _, err := q.GetDependenciesAt(t.Context(), end, 3*time.Millisecond); !errors.Is(err, ErrUnavailable) {
		t.Fatalf("bad JSON: %v", err)
	}
	calls := ts.calls.Load()
	if _, err := q.GetDependenciesAt(t.Context(), end, -time.Second); !errors.Is(err, ErrValidation) {
		t.Fatalf("negative lookback: %v", err)
	}
	if _, err := q.GetDependencies(t.Context(), DependencyOptions{Start: end, End: start}); !errors.Is(err, ErrValidation) {
		t.Fatalf("start after end: %v", err)
	}
	if ts.calls.Load() != calls {
		t.Fatal("invalid input must be rejected locally")
	}
	missing := newQueryClient(t, newTestServer(t, status(http.StatusNotFound)).URL, nil)
	if _, err := missing.GetDependencies(t.Context(), DependencyOptions{}); !errors.Is(err, ErrUnsupported) {
		t.Fatalf("route missing: %v", err)
	}
	failing := newQueryClient(t, newTestServer(t, status(http.StatusInternalServerError)).URL, func(cfg *Config) { cfg.Retry = fastRetry(0) })
	if _, err := failing.GetDependencies(t.Context(), DependencyOptions{}); !errors.Is(err, ErrUnavailable) {
		t.Fatalf("500: %v", err)
	}
}

func TestQueryModelEdges(t *testing.T) {
	t.Run("otlp value kinds", func(t *testing.T) {
		body := `{"result":{"resourceSpans":[{"resource":{"attributes":[]},"scopeSpans":[{"spans":[
			{"traceId":"AB","spanId":"CD","name":"x","kind":"SPAN_KIND_PRODUCER","startTimeUnixNano":"2000","endTimeUnixNano":"1000",
			 "status":{"code":"STATUS_CODE_ERROR","message":"m"},
			 "attributes":[{"key":"kv","value":{"kvlistValue":{"values":[{"key":"n","value":{"intValue":7}}]}}},
			               {"key":"b","value":{"bytesValue":"AQI="}},{"key":"empty","value":{}},
			               {"key":"bigint","value":{"intValue":"99999999999999999999"}},{"key":"d","value":{"doubleValue":"2.5"}}]},
			{"traceId":"ab","spanId":"ce","name":"y","kind":"SPAN_KIND_CONSUMER","status":{"code":"STATUS_CODE_OK"}},
			{"traceId":"ab","spanId":"cf","name":"z","kind":5},{"traceId":"ab","spanId":"d0","name":"w","kind":"SPAN_KIND_INTERNAL"},
			{"traceId":"ab","spanId":"d1","name":"v","kind":"SPAN_KIND_SERVER"},{"traceId":"ab","spanId":"d2","name":"u","kind":"SPAN_KIND_CLIENT"},
			{"traceId":"ab","spanId":"d3","name":"t","kind":4},{"traceId":"ab","spanId":"d4","name":"s","kind":0}
		]}]}]}}`
		traces, err := parseOTLPTraces("op", &response{Status: 200, Body: []byte(body)})
		if err != nil || len(traces) != 1 || len(traces[0].Spans) != 8 {
			t.Fatalf("%+v %v", traces, err)
		}
		s := spanByName(t, &traces[0], "x")
		if s.TraceID != "000000000000000000000000000000ab" || s.SpanID != "00000000000000cd" || s.Kind != SpanKindProducer ||
			s.Status != (SpanStatus{Code: SpanStatusError, Message: "m"}) || s.DurationMicros != 0 || s.ServiceName != "" {
			t.Fatalf("span %+v", s)
		}
		if !reflect.DeepEqual(s.Attributes["kv"], map[string]any{"n": int64(7)}) || s.Attributes["b"] != "AQI=" || s.Attributes["empty"] != nil ||
			s.Attributes["bigint"] != "99999999999999999999" || s.Attributes["d"] != 2.5 {
			t.Fatalf("attrs %#v", s.Attributes)
		}
		kinds := map[string]SpanKind{"y": SpanKindConsumer, "z": SpanKindConsumer, "w": SpanKindInternal, "v": SpanKindServer, "u": SpanKindClient, "t": SpanKindProducer, "s": SpanKindUnspecified}
		for name, k := range kinds {
			if got := spanByName(t, &traces[0], name).Kind; got != k {
				t.Fatalf("%s kind %s", name, got)
			}
		}
		if spanByName(t, &traces[0], "y").Status.Code != SpanStatusOK {
			t.Fatal("status enum")
		}
		bad := otlpAnyValue{DoubleValue: ptrNumber("x")}
		if bad.toGo() != "x" {
			t.Fatal("invalid double")
		}
	})

	t.Run("legacy references and tags", func(t *testing.T) {
		s := convertLegacySpan(legacySpan{
			TraceID: "AB", SpanID: "1", OperationName: "op", StartTime: 10, Duration: 5,
			References: []legacyRef{{RefType: "FOLLOWS_FROM", TraceID: "cd", SpanID: "2"}, {RefType: "FOLLOWS_FROM", TraceID: "ab", SpanID: "3"}},
			Tags: []legacyKV{
				{Key: "error", Type: "bool", Value: true}, {Key: "span.kind", Value: "PRODUCER"},
				{Key: "f", Type: "float64", Value: jsonNumber("2")}, {Key: "i", Type: "int64", Value: jsonNumber("3")},
				{Key: "big", Type: "int64", Value: jsonNumber("1e3")},
			},
		}, "svc")
		if s.ParentSpanID != "0000000000000003" || len(s.Links) != 1 || s.Links[0].TraceID != "000000000000000000000000000000cd" ||
			s.Status.Code != SpanStatusError || s.Kind != SpanKindProducer || s.EndMicros != 15 {
			t.Fatalf("span %+v", s)
		}
		if s.Attributes["f"] != 2.0 || s.Attributes["i"] != int64(3) || s.Attributes["big"] != 1000.0 {
			t.Fatalf("attrs %#v", s.Attributes)
		}
		for in, want := range map[string]SpanKind{"server": SpanKindServer, "client": SpanKindClient, "consumer": SpanKindConsumer, "internal": SpanKindInternal, "x": SpanKindUnspecified} {
			if legacyKind(in) != want {
				t.Fatalf("legacyKind(%s)", in)
			}
		}
		ok := convertLegacySpan(legacySpan{TraceID: "ab", SpanID: "1", Tags: []legacyKV{{Key: "otel.status_code", Value: "OK"}, {Key: "error", Value: "true"}}}, "")
		if ok.Status.Code != SpanStatusOK {
			t.Fatalf("explicit OK must win: %+v", ok.Status)
		}
		ev := convertLegacySpan(legacySpan{TraceID: "ab", SpanID: "1", Logs: []struct {
			Timestamp int64      `json:"timestamp"`
			Fields    []legacyKV `json:"fields"`
		}{{Timestamp: 5, Fields: []legacyKV{{Key: "event", Value: jsonNumber("1")}}}}}, "")
		if ev.Events[0].Name != "" || ev.Events[0].Attributes["event"] != int64(1) {
			t.Fatalf("non-string event %+v", ev.Events)
		}
	})

	t.Run("helpers", func(t *testing.T) {
		if formatDuration(10*time.Nanosecond) != "1us" || formatDuration(2*time.Millisecond) != "2000us" {
			t.Fatal("formatDuration")
		}
		if nanosToMicros("x") != 0 || nanosToMicros("5000") != 5 {
			t.Fatal("nanosToMicros")
		}
		if normalizeIDLoose("", 16) != "" || normalizeIDLoose("XYZ", 16) != "xyz" {
			t.Fatal("normalizeIDLoose")
		}
		if isJSONEnvelope([]byte(`{"data":[]}`)) || !isJSONEnvelope([]byte(`{"errors":[]}`)) || isJSONEnvelope([]byte("404 page not found")) {
			t.Fatal("isJSONEnvelope")
		}
		if _, err := parseLegacyTraces("op", &response{Body: []byte("[")}); !errors.Is(err, ErrUnavailable) {
			t.Fatal("legacy bad JSON")
		}
	})
}
