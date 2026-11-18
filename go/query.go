package jaeger

import (
	"bytes"
	"context"
	"encoding/json"
	"net/http"
	"net/url"
	"sort"
	"strconv"
	"strings"
	"time"
)

// Default query windows.
const (
	// DefaultSearchWindow is the search window when TraceSearchQuery has no Start:
	// the last hour before End (Jaeger requires both bounds).
	DefaultSearchWindow = time.Hour
	// DefaultGetTraceWindow is the time hint GetTrace sends: the last 7 days.
	DefaultGetTraceWindow = 7 * 24 * time.Hour
	// DefaultDependencyLookback is the GetDependencies lookback when Start is zero.
	DefaultDependencyLookback = 24 * time.Hour
)

// QueryClient reads traces from the Jaeger HTTP query API (port 16686). It uses the
// v3 API (/api/v3/traces, /api/v3/trace-summaries, /api/v3/services,
// /api/v3/operations), the legacy /api/traces/{id} as a fallback for GetTrace and the
// legacy /api/dependencies. Get it with Client.Query. It implements TraceQueryClient
// and is safe for concurrent use.
type QueryClient struct {
	t      *httpTransport
	header func() http.Header
	now    func() time.Time
}

var _ TraceQueryClient = (*QueryClient)(nil)

// GetTraceOptions narrow QueryClient.GetTraceWith. Zero values mean defaults.
type GetTraceOptions struct {
	// Start and End are a time hint for the storage (default: the last 7 days). The
	// in-memory store ignores them.
	Start time.Time
	End   time.Time
	// RawTraces skips Jaeger's span adjusters (clock skew, deduplication).
	RawTraces bool
}

// Operation is one operation of a service as listed by ListOperationsWith.
type Operation struct {
	// Name is the span name.
	Name string `json:"name"`
	// SpanKind is the span kind ("server", "client", …; "" when unknown).
	SpanKind SpanKind `json:"spanKind"`
}

// ListOperationsOptions narrow QueryClient.ListOperationsWith.
type ListOperationsOptions struct {
	// SpanKind keeps only operations of this kind (server, client, producer, consumer,
	// internal; "" = all). The SDK sends it lower-case (Jaeger is case-sensitive).
	SpanKind SpanKind
}

// rawResult is a response that was 2xx or 404; notFound marks the JSON error envelope
// ("the record does not exist") as opposed to a missing route.
type rawResult struct {
	res          *response
	notFound     bool
	routeMissing bool
}

// call sends req and returns 2xx and 404 responses to the caller (a 404 is split into
// "not found" and "route missing"); other statuses are retried and mapped to errors by
// the transport.
func (q *QueryClient) call(ctx context.Context, req *request) (rawResult, error) {
	req.Header = q.header()
	req.RawStatus = true
	res, err := q.t.do(ctx, req)
	if err != nil {
		return rawResult{}, err
	}
	switch {
	case res.Status >= 200 && res.Status < 300:
		return rawResult{res: res}, nil
	case res.Status == http.StatusNotFound:
		if isJSONEnvelope(res.Body) {
			return rawResult{res: res, notFound: true}, nil
		}
		return rawResult{res: res, routeMissing: true}, nil
	case retryableStatus(res.Status, true):
		// Let the transport retry and map the status.
		req.RawStatus = false
		res, err = q.t.do(ctx, req)
		if err != nil {
			if CodeOf(err) == ErrorCodeNotFound {
				return rawResult{notFound: true}, nil
			}
			return rawResult{}, err
		}
		return rawResult{res: res}, nil
	}
	return rawResult{}, StatusError(q.t.redact(req.Operation), res.Status, res.RequestID, vendorErrorText(res), q.t.knownSecrets())
}

// isJSONEnvelope reports whether a 404 body is Jaeger's JSON error envelope (v3
// {"error":{…}} or legacy {"errors":[…]}) rather than a missing route.
func isJSONEnvelope(body []byte) bool {
	var doc map[string]json.RawMessage
	if json.Unmarshal(bytes.TrimSpace(body), &doc) != nil {
		return false
	}
	_, v3 := doc["error"]
	_, legacy := doc["errors"]
	return v3 || legacy
}

func routeMissing(op, route string) error {
	return UnsupportedError(op, "the query API has no "+route+" route")
}

// GetTrace returns the trace with all spans sorted, or nil when it does not exist. The
// id is 1-32 hex characters (64-bit ids are left-padded). It calls
// /api/v3/traces/{id} and falls back to the legacy /api/traces/{id} when the v3 route
// is missing.
func (q *QueryClient) GetTrace(ctx context.Context, traceID string) (*Trace, error) {
	return q.GetTraceWith(ctx, traceID, GetTraceOptions{})
}

// GetTraceWith is GetTrace with a time hint and the raw-traces switch.
func (q *QueryClient) GetTraceWith(ctx context.Context, traceID string, opts GetTraceOptions) (*Trace, error) {
	const op = "traces.getTrace"
	id, err := NormalizeTraceID(traceID)
	if err != nil {
		return nil, err
	}
	end := opts.End
	if end.IsZero() {
		end = q.now()
	}
	start := opts.Start
	if start.IsZero() {
		start = end.Add(-DefaultGetTraceWindow)
	}
	if start.After(end) {
		return nil, ValidationError(op, "start must not be after end")
	}
	v3q := url.Values{"startTime": {formatTime(start)}, "endTime": {formatTime(end)}}
	if opts.RawTraces {
		v3q.Set("rawTraces", "true")
	}
	r, err := q.call(ctx, &request{Method: http.MethodGet, Path: "/api/v3/traces/" + id, Query: v3q, Operation: op})
	if err != nil {
		return nil, err
	}
	switch {
	case r.notFound:
		return nil, nil
	case !r.routeMissing:
		traces, err := parseOTLPTraces(op, r.res)
		if err != nil {
			return nil, err
		}
		for i := range traces {
			if traces[i].TraceID == id {
				return &traces[i], nil
			}
		}
		return nil, nil
	}
	legacyQ := url.Values{
		"start": {strconv.FormatInt(start.UnixMicro(), 10)},
		"end":   {strconv.FormatInt(end.UnixMicro(), 10)},
	}
	if opts.RawTraces {
		legacyQ.Set("raw", "true")
	}
	r, err = q.call(ctx, &request{Method: http.MethodGet, Path: "/api/traces/" + id, Query: legacyQ, Operation: op})
	if err != nil {
		return nil, err
	}
	switch {
	case r.notFound:
		return nil, nil
	case r.routeMissing:
		return nil, routeMissing(op, "/api/v3/traces or /api/traces")
	}
	traces, err := parseLegacyTraces(op, r.res)
	if err != nil {
		return nil, err
	}
	for i := range traces {
		if traces[i].TraceID == id {
			return &traces[i], nil
		}
	}
	return nil, nil
}

// searchParams builds the v3 query.* parameters (defaults: End now, Start End-1h,
// Limit 20).
func (q *QueryClient) searchParams(op string, s TraceSearchQuery) (url.Values, error) {
	s = s.WithDefaults()
	if s.MinDuration < 0 || s.MaxDuration < 0 {
		return nil, ValidationError(op, "durations must not be negative")
	}
	if s.MinDuration > 0 && s.MaxDuration > 0 && s.MinDuration > s.MaxDuration {
		return nil, ValidationError(op, "minDuration must not exceed maxDuration")
	}
	end := s.End
	if end.IsZero() {
		end = q.now()
	}
	start := s.Start
	if start.IsZero() {
		start = end.Add(-DefaultSearchWindow)
	}
	if start.After(end) {
		return nil, ValidationError(op, "start must not be after end")
	}
	v := url.Values{
		"query.startTimeMin": {formatTime(start)},
		"query.startTimeMax": {formatTime(end)},
		"query.searchDepth":  {strconv.Itoa(s.Limit)},
	}
	if s.Service != "" {
		v.Set("query.serviceName", s.Service)
	}
	if s.Operation != "" {
		v.Set("query.operationName", s.Operation)
	}
	if len(s.Tags) > 0 {
		raw, _ := json.Marshal(s.Tags) // a map[string]string always encodes
		v.Set("query.attributes", string(raw))
	}
	if s.MinDuration > 0 {
		v.Set("query.durationMin", formatDuration(s.MinDuration))
	}
	if s.MaxDuration > 0 {
		v.Set("query.durationMax", formatDuration(s.MaxDuration))
	}
	return v, nil
}

// FindTraces returns full traces matching q (GET /api/v3/traces), each with its
// spans sorted, in the order Jaeger returns them. Tags match attributes on any span
// of the trace; durations are inclusive. No match returns an empty slice. Defaults:
// End = now, Start = End - 1 h, Limit = 20.
func (q *QueryClient) FindTraces(ctx context.Context, query TraceSearchQuery) ([]Trace, error) {
	const op = "traces.findTraces"
	params, err := q.searchParams(op, query)
	if err != nil {
		return nil, err
	}
	r, err := q.call(ctx, &request{Method: http.MethodGet, Path: "/api/v3/traces", Query: params, Operation: op})
	if err != nil {
		return nil, err
	}
	switch {
	case r.notFound:
		return []Trace{}, nil
	case r.routeMissing:
		return nil, routeMissing(op, "/api/v3/traces")
	}
	return parseOTLPTraces(op, r.res)
}

// SearchTraces returns trace summaries matching q (GET /api/v3/trace-summaries), in
// the order Jaeger returns them. Same filters and defaults as FindTraces.
func (q *QueryClient) SearchTraces(ctx context.Context, query TraceSearchQuery) ([]TraceSummary, error) {
	const op = "traces.searchTraces"
	params, err := q.searchParams(op, query)
	if err != nil {
		return nil, err
	}
	r, err := q.call(ctx, &request{Method: http.MethodGet, Path: "/api/v3/trace-summaries", Query: params, Operation: op})
	if err != nil {
		return nil, err
	}
	switch {
	case r.notFound:
		return []TraceSummary{}, nil
	case r.routeMissing:
		return nil, routeMissing(op, "/api/v3/trace-summaries")
	}
	var body struct {
		Summaries []struct {
			TraceID              string      `json:"traceId"`
			RootServiceName      string      `json:"rootServiceName"`
			RootOperationName    string      `json:"rootOperationName"`
			MinStartTimeUnixNano json.Number `json:"minStartTimeUnixNano"`
			MaxEndTimeUnixNano   json.Number `json:"maxEndTimeUnixNano"`
			SpanCount            int         `json:"spanCount"`
		} `json:"summaries"`
	}
	if err := r.res.decodeJSON(op, &body); err != nil {
		return nil, err
	}
	out := make([]TraceSummary, 0, len(body.Summaries))
	for _, s := range body.Summaries {
		start := nanosToMicros(s.MinStartTimeUnixNano)
		end := nanosToMicros(s.MaxEndTimeUnixNano)
		out = append(out, TraceSummary{
			TraceID:         normalizeIDLoose(s.TraceID, 32),
			RootServiceName: s.RootServiceName,
			RootName:        s.RootOperationName,
			StartMicros:     start,
			DurationMicros:  max(end-start, 0),
			SpanCount:       s.SpanCount,
		})
	}
	return out, nil
}

// ListServices returns the names of all services with stored spans, sorted (GET
// /api/v3/services). The all-in-one always lists itself as "jaeger".
func (q *QueryClient) ListServices(ctx context.Context) ([]string, error) {
	const op = "traces.listServices"
	r, err := q.call(ctx, &request{Method: http.MethodGet, Path: "/api/v3/services", Operation: op})
	if err != nil {
		return nil, err
	}
	if r.routeMissing || r.notFound {
		return nil, routeMissing(op, "/api/v3/services")
	}
	var body struct {
		Services []string `json:"services"`
	}
	if err := r.res.decodeJSON(op, &body); err != nil {
		return nil, err
	}
	out := append([]string{}, body.Services...)
	sort.Strings(out)
	return out, nil
}

// ListOperations returns the operation (span) names of service, deduplicated across
// span kinds and sorted. An empty service is a VALIDATION error; an unknown service
// returns an empty slice.
func (q *QueryClient) ListOperations(ctx context.Context, service string) ([]string, error) {
	ops, err := q.ListOperationsWith(ctx, service, ListOperationsOptions{})
	if err != nil {
		return nil, err
	}
	seen := make(map[string]bool, len(ops))
	out := make([]string, 0, len(ops))
	for _, o := range ops {
		if !seen[o.Name] {
			seen[o.Name] = true
			out = append(out, o.Name)
		}
	}
	sort.Strings(out)
	return out, nil
}

// ListOperationsWith returns the operations of service with their span kinds (GET
// /api/v3/operations), optionally filtered by span kind, sorted by name then kind.
func (q *QueryClient) ListOperationsWith(ctx context.Context, service string, opts ListOperationsOptions) ([]Operation, error) {
	const op = "traces.listOperations"
	if strings.TrimSpace(service) == "" {
		return nil, ValidationError(op, "service is required")
	}
	params := url.Values{"service": {service}}
	if opts.SpanKind != "" {
		kind := SpanKind(strings.ToLower(string(opts.SpanKind)))
		switch kind {
		case SpanKindServer, SpanKindClient, SpanKindProducer, SpanKindConsumer, SpanKindInternal:
		default:
			return nil, ValidationError(op, "spanKind must be server, client, producer, consumer or internal")
		}
		params.Set("spanKind", string(kind))
	}
	r, err := q.call(ctx, &request{Method: http.MethodGet, Path: "/api/v3/operations", Query: params, Operation: op})
	if err != nil {
		return nil, err
	}
	if r.routeMissing || r.notFound {
		return nil, routeMissing(op, "/api/v3/operations")
	}
	var body struct {
		Operations []Operation `json:"operations"`
	}
	if err := r.res.decodeJSON(op, &body); err != nil {
		return nil, err
	}
	out := append([]Operation{}, body.Operations...)
	for i := range out {
		out[i].SpanKind = SpanKind(strings.ToLower(string(out[i].SpanKind)))
	}
	sort.SliceStable(out, func(i, j int) bool {
		if out[i].Name != out[j].Name {
			return out[i].Name < out[j].Name
		}
		return out[i].SpanKind < out[j].SpanKind
	})
	return out, nil
}

// GetDependencies returns the service dependency links of spans in [Start, End]
// (defaults: End = now, Start = End - 24 h) from the legacy /api/dependencies. The
// in-memory store derives them on the fly; production stores need the
// spark-dependencies job, so an empty result is normal there.
func (q *QueryClient) GetDependencies(ctx context.Context, opts DependencyOptions) ([]DependencyLink, error) {
	end := opts.End
	if end.IsZero() {
		end = q.now()
	}
	start := opts.Start
	if start.IsZero() {
		start = end.Add(-DefaultDependencyLookback)
	}
	if start.After(end) {
		return nil, ValidationError("traces.getDependencies", "start must not be after end")
	}
	return q.GetDependenciesAt(ctx, end, end.Sub(start))
}

// GetDependenciesAt returns the dependency links of the lookback window that ends at
// endTs (Jaeger's endTs/lookback form). A zero endTs means now, a zero lookback 24 h;
// a negative lookback is a VALIDATION error.
func (q *QueryClient) GetDependenciesAt(ctx context.Context, endTs time.Time, lookback time.Duration) ([]DependencyLink, error) {
	const op = "traces.getDependencies"
	if lookback < 0 {
		return nil, ValidationError(op, "lookback must not be negative")
	}
	if endTs.IsZero() {
		endTs = q.now()
	}
	if lookback == 0 {
		lookback = DefaultDependencyLookback
	}
	params := url.Values{
		"endTs":    {strconv.FormatInt(endTs.UnixMilli(), 10)},
		"lookback": {strconv.FormatInt(lookback.Milliseconds(), 10)},
	}
	r, err := q.call(ctx, &request{Method: http.MethodGet, Path: "/api/dependencies", Query: params, Operation: op})
	if err != nil {
		return nil, err
	}
	switch {
	case r.routeMissing:
		return nil, routeMissing(op, "/api/dependencies")
	case r.notFound:
		return []DependencyLink{}, nil
	}
	var body struct {
		Data []DependencyLink `json:"data"`
	}
	if err := r.res.decodeJSON(op, &body); err != nil {
		return nil, err
	}
	return append([]DependencyLink{}, body.Data...), nil
}

// WaitForTrace polls GetTrace until the trace has at least MinSpans spans (test
// helper; see the package-level WaitForTrace).
func (q *QueryClient) WaitForTrace(ctx context.Context, traceID string, opts WaitForTraceOptions) (*Trace, error) {
	return WaitForTrace(ctx, q, traceID, opts)
}

// formatTime renders t as RFC3339Nano in UTC (Jaeger's v3 time format).
func formatTime(t time.Time) string { return t.UTC().Format(time.RFC3339Nano) }

// formatDuration renders d as whole microseconds in Go duration syntax ("1500us").
func formatDuration(d time.Duration) string {
	return strconv.FormatInt(max(d.Microseconds(), 1), 10) + "us"
}

// nanosToMicros converts a decimal nanosecond string to microseconds (0 when invalid).
func nanosToMicros(n json.Number) int64 {
	v, err := strconv.ParseInt(string(n), 10, 64)
	if err != nil {
		return 0
	}
	return v / 1000
}

// normalizeIDLoose lower-cases and left-pads a hex id; invalid ids are returned
// lower-cased as is (the server is trusted to send valid ids).
func normalizeIDLoose(id string, size int) string {
	if id == "" {
		return ""
	}
	if n, err := normalizeHexID("id", id, size); err == nil {
		return n
	}
	return strings.ToLower(id)
}
