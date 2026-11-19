package jaeger

import (
	"bytes"
	"encoding/json"
	"strconv"
	"strings"
)

// --- OTLP/JSON (v3 API) -----------------------------------------------------------

type otlpAnyValue struct {
	StringValue *string          `json:"stringValue"`
	BoolValue   *bool            `json:"boolValue"`
	IntValue    *json.Number     `json:"intValue"`
	DoubleValue *json.Number     `json:"doubleValue"`
	BytesValue  *string          `json:"bytesValue"`
	ArrayValue  *otlpArrayValue  `json:"arrayValue"`
	KvlistValue *otlpKvlistValue `json:"kvlistValue"`
}

type otlpArrayValue struct {
	Values []otlpAnyValue `json:"values"`
}

type otlpKvlistValue struct {
	Values []otlpKeyValue `json:"values"`
}

type otlpKeyValue struct {
	Key   string       `json:"key"`
	Value otlpAnyValue `json:"value"`
}

type otlpEvent struct {
	TimeUnixNano json.Number    `json:"timeUnixNano"`
	Name         string         `json:"name"`
	Attributes   []otlpKeyValue `json:"attributes"`
}

type otlpLink struct {
	TraceID    string         `json:"traceId"`
	SpanID     string         `json:"spanId"`
	Attributes []otlpKeyValue `json:"attributes"`
}

type otlpStatus struct {
	Code    json.RawMessage `json:"code"`
	Message string          `json:"message"`
}

type otlpSpan struct {
	TraceID           string          `json:"traceId"`
	SpanID            string          `json:"spanId"`
	ParentSpanID      string          `json:"parentSpanId"`
	Name              string          `json:"name"`
	Kind              json.RawMessage `json:"kind"`
	StartTimeUnixNano json.Number     `json:"startTimeUnixNano"`
	EndTimeUnixNano   json.Number     `json:"endTimeUnixNano"`
	Attributes        []otlpKeyValue  `json:"attributes"`
	Events            []otlpEvent     `json:"events"`
	Links             []otlpLink      `json:"links"`
	Status            otlpStatus      `json:"status"`
}

type otlpTracesData struct {
	Result struct {
		ResourceSpans []struct {
			Resource struct {
				Attributes []otlpKeyValue `json:"attributes"`
			} `json:"resource"`
			ScopeSpans []struct {
				Spans []otlpSpan `json:"spans"`
			} `json:"scopeSpans"`
		} `json:"resourceSpans"`
	} `json:"result"`
}

func (v otlpAnyValue) toGo() any {
	switch {
	case v.StringValue != nil:
		return *v.StringValue
	case v.BoolValue != nil:
		return *v.BoolValue
	case v.IntValue != nil:
		if i, err := strconv.ParseInt(strings.Trim(string(*v.IntValue), `"`), 10, 64); err == nil {
			return i
		}
		return string(*v.IntValue)
	case v.DoubleValue != nil:
		if f, err := strconv.ParseFloat(strings.Trim(string(*v.DoubleValue), `"`), 64); err == nil {
			return f
		}
		return string(*v.DoubleValue)
	case v.BytesValue != nil:
		return *v.BytesValue
	case v.ArrayValue != nil:
		out := make([]any, len(v.ArrayValue.Values))
		for i, e := range v.ArrayValue.Values {
			out[i] = e.toGo()
		}
		return out
	case v.KvlistValue != nil:
		return otlpAttrs(v.KvlistValue.Values)
	}
	return nil
}

func otlpAttrs(kvs []otlpKeyValue) map[string]any {
	out := make(map[string]any, len(kvs))
	for _, kv := range kvs {
		out[kv.Key] = kv.Value.toGo()
	}
	return out
}

// otlpKind maps an OTLP span kind (number or enum name) to SpanKind.
func otlpKind(raw json.RawMessage) SpanKind {
	s := strings.Trim(string(raw), `"`)
	switch s {
	case "1", "SPAN_KIND_INTERNAL":
		return SpanKindInternal
	case "2", "SPAN_KIND_SERVER":
		return SpanKindServer
	case "3", "SPAN_KIND_CLIENT":
		return SpanKindClient
	case "4", "SPAN_KIND_PRODUCER":
		return SpanKindProducer
	case "5", "SPAN_KIND_CONSUMER":
		return SpanKindConsumer
	}
	return SpanKindUnspecified
}

// otlpStatusCode maps an OTLP status code (number or enum name).
func otlpStatusCode(raw json.RawMessage) SpanStatusCode {
	switch strings.Trim(string(raw), `"`) {
	case "1", "STATUS_CODE_OK":
		return SpanStatusOK
	case "2", "STATUS_CODE_ERROR":
		return SpanStatusError
	}
	return SpanStatusUnset
}

// parseOTLPTraces decodes an OTLP/JSON TracesData body and groups its spans into
// traces (in first-seen order, spans sorted).
func parseOTLPTraces(op string, res *response) ([]Trace, error) {
	var data otlpTracesData
	dec := json.NewDecoder(bytes.NewReader(res.Body))
	dec.UseNumber()
	if err := dec.Decode(&data); err != nil {
		return nil, &Error{Code: ErrorCodeUnavailable, Message: op + ": invalid JSON response", HTTPStatus: res.Status, RequestID: res.RequestID, Retryable: true}
	}
	g := newTraceGrouper()
	for _, rs := range data.Result.ResourceSpans {
		service := ""
		if v, ok := otlpAttrs(rs.Resource.Attributes)["service.name"].(string); ok {
			service = v
		}
		for _, ss := range rs.ScopeSpans {
			for _, s := range ss.Spans {
				g.add(convertOTLPSpan(s, service))
			}
		}
	}
	return g.traces(), nil
}

func convertOTLPSpan(s otlpSpan, service string) Span {
	start, end := nanosToMicros(s.StartTimeUnixNano), nanosToMicros(s.EndTimeUnixNano)
	out := Span{
		TraceID:        normalizeIDLoose(s.TraceID, 32),
		SpanID:         normalizeIDLoose(s.SpanID, 16),
		ParentSpanID:   normalizeIDLoose(s.ParentSpanID, 16),
		Name:           s.Name,
		Kind:           otlpKind(s.Kind),
		ServiceName:    service,
		StartMicros:    start,
		EndMicros:      end,
		DurationMicros: max(end-start, 0),
		Status:         SpanStatus{Code: otlpStatusCode(s.Status.Code), Message: s.Status.Message},
		Attributes:     otlpAttrs(s.Attributes),
		Events:         []SpanEvent{},
		Links:          []SpanLink{},
	}
	for _, e := range s.Events {
		out.Events = append(out.Events, SpanEvent{Name: e.Name, TimeMicros: nanosToMicros(e.TimeUnixNano), Attributes: otlpAttrs(e.Attributes)})
	}
	for _, l := range s.Links {
		out.Links = append(out.Links, SpanLink{TraceID: normalizeIDLoose(l.TraceID, 32), SpanID: normalizeIDLoose(l.SpanID, 16), Attributes: otlpAttrs(l.Attributes)})
	}
	return out
}

// traceGrouper collects spans per trace id in first-seen order.
type traceGrouper struct {
	order []string
	spans map[string][]Span
}

func newTraceGrouper() *traceGrouper { return &traceGrouper{spans: map[string][]Span{}} }

func (g *traceGrouper) add(s Span) {
	if _, ok := g.spans[s.TraceID]; !ok {
		g.order = append(g.order, s.TraceID)
	}
	g.spans[s.TraceID] = append(g.spans[s.TraceID], s)
}

func (g *traceGrouper) traces() []Trace {
	out := make([]Trace, 0, len(g.order))
	for _, id := range g.order {
		out = append(out, Trace{TraceID: id, Spans: SortSpans(g.spans[id])})
	}
	return out
}

// --- Legacy UI model (/api/traces/{id}) -------------------------------------------

type legacyKV struct {
	Key   string `json:"key"`
	Type  string `json:"type"`
	Value any    `json:"value"`
}

type legacyRef struct {
	RefType string `json:"refType"`
	TraceID string `json:"traceID"`
	SpanID  string `json:"spanID"`
}

type legacySpan struct {
	TraceID       string      `json:"traceID"`
	SpanID        string      `json:"spanID"`
	OperationName string      `json:"operationName"`
	References    []legacyRef `json:"references"`
	StartTime     int64       `json:"startTime"`
	Duration      int64       `json:"duration"`
	Tags          []legacyKV  `json:"tags"`
	Logs          []struct {
		Timestamp int64      `json:"timestamp"`
		Fields    []legacyKV `json:"fields"`
	} `json:"logs"`
	ProcessID string `json:"processID"`
}

type legacyBody struct {
	Data []struct {
		TraceID   string       `json:"traceID"`
		Spans     []legacySpan `json:"spans"`
		Processes map[string]struct {
			ServiceName string `json:"serviceName"`
		} `json:"processes"`
	} `json:"data"`
}

// parseLegacyTraces decodes the legacy {"data":[…]} body into normalized traces.
func parseLegacyTraces(op string, res *response) ([]Trace, error) {
	var body legacyBody
	dec := json.NewDecoder(bytes.NewReader(res.Body))
	dec.UseNumber()
	if err := dec.Decode(&body); err != nil {
		return nil, &Error{Code: ErrorCodeUnavailable, Message: op + ": invalid JSON response", HTTPStatus: res.Status, RequestID: res.RequestID, Retryable: true}
	}
	g := newTraceGrouper()
	for _, t := range body.Data {
		for _, s := range t.Spans {
			g.add(convertLegacySpan(s, t.Processes[s.ProcessID].ServiceName))
		}
	}
	return g.traces(), nil
}

// legacyValue converts a typed legacy tag value (int64 numbers decode as int64).
func legacyValue(kv legacyKV) any {
	n, isNum := kv.Value.(json.Number)
	if !isNum {
		return kv.Value
	}
	if kv.Type != "float64" {
		if i, err := n.Int64(); err == nil {
			return i
		}
	}
	f, _ := n.Float64() // a json.Number from the decoder is always a valid float
	return f
}

func convertLegacySpan(s legacySpan, service string) Span {
	traceID := normalizeIDLoose(s.TraceID, 32)
	out := Span{
		TraceID:        traceID,
		SpanID:         normalizeIDLoose(s.SpanID, 16),
		Name:           s.OperationName,
		Kind:           SpanKindUnspecified,
		ServiceName:    service,
		StartMicros:    s.StartTime,
		EndMicros:      s.StartTime + s.Duration,
		DurationMicros: max(s.Duration, 0),
		Status:         SpanStatus{Code: SpanStatusUnset},
		Attributes:     map[string]any{},
		Events:         []SpanEvent{},
		Links:          []SpanLink{},
	}
	parent := -1
	for i, r := range s.References {
		if r.RefType == "CHILD_OF" && normalizeIDLoose(r.TraceID, 32) == traceID {
			parent = i
			break
		}
	}
	if parent < 0 {
		for i, r := range s.References {
			if normalizeIDLoose(r.TraceID, 32) == traceID {
				parent = i
				break
			}
		}
	}
	for i, r := range s.References {
		if i == parent {
			out.ParentSpanID = normalizeIDLoose(r.SpanID, 16)
			continue
		}
		out.Links = append(out.Links, SpanLink{TraceID: normalizeIDLoose(r.TraceID, 32), SpanID: normalizeIDLoose(r.SpanID, 16), Attributes: map[string]any{}})
	}
	errorTag := false
	for _, kv := range s.Tags {
		v := legacyValue(kv)
		switch {
		case kv.Key == "span.kind":
			if k, ok := v.(string); ok {
				out.Kind = legacyKind(k)
			}
		case kv.Key == "otel.status_code":
			switch v {
			case "OK":
				out.Status.Code = SpanStatusOK
			case "ERROR":
				out.Status.Code = SpanStatusError
			}
		case kv.Key == "otel.status_description":
			out.Status.Message, _ = v.(string)
		case kv.Key == "error":
			errorTag = v == true || v == "true"
		case strings.HasPrefix(kv.Key, "otel.scope."):
		default:
			out.Attributes[kv.Key] = v
		}
	}
	if errorTag && out.Status.Code == SpanStatusUnset {
		out.Status.Code = SpanStatusError
	}
	for _, l := range s.Logs {
		ev := SpanEvent{TimeMicros: l.Timestamp, Attributes: map[string]any{}}
		for _, f := range l.Fields {
			v := legacyValue(f)
			if f.Key == "event" {
				if name, ok := v.(string); ok {
					ev.Name = name
					continue
				}
			}
			ev.Attributes[f.Key] = v
		}
		out.Events = append(out.Events, ev)
	}
	return out
}

func legacyKind(k string) SpanKind {
	switch SpanKind(strings.ToLower(k)) {
	case SpanKindServer:
		return SpanKindServer
	case SpanKindClient:
		return SpanKindClient
	case SpanKindProducer:
		return SpanKindProducer
	case SpanKindConsumer:
		return SpanKindConsumer
	case SpanKindInternal:
		return SpanKindInternal
	}
	return SpanKindUnspecified
}
