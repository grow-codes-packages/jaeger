package jaeger

import (
	"context"
	"encoding/json"
	"net/http"
	"os"
	"path/filepath"
	"testing"
)

// fixture is a response recorded from the real jaeger 2.23.0 container
// (testdata/fixtures/<function>.<case>.json).
type fixture struct {
	Response struct {
		Status      int               `json:"status"`
		Headers     map[string]string `json:"headers"`
		Body        json.RawMessage   `json:"body"`
		BodyExcerpt string            `json:"bodyExcerpt"`
	} `json:"response"`
}

func loadFixture(t *testing.T, name string) fixture {
	t.Helper()
	data, err := os.ReadFile(filepath.Join("testdata", "fixtures", name+".json"))
	if err != nil {
		t.Fatalf("fixture %s: %v", name, err)
	}
	var f fixture
	if err := json.Unmarshal(data, &f); err != nil {
		t.Fatalf("fixture %s: %v", name, err)
	}
	return f
}

// body returns the recorded body bytes (JSON string bodies are unquoted).
func (f fixture) body() []byte {
	raw := f.Response.Body
	if len(raw) == 0 || string(raw) == "null" {
		if f.Response.BodyExcerpt != "" {
			return []byte("<!doctype html><html><head><script>// <![CDATA[\n" + f.Response.BodyExcerpt + "\n// ]]></script></head></html>")
		}
		return nil
	}
	var s string
	if raw[0] == '"' && json.Unmarshal(raw, &s) == nil {
		return []byte(s)
	}
	return raw
}

// serve writes the recorded response.
func (f fixture) serve(w http.ResponseWriter) {
	for k, v := range f.Response.Headers {
		w.Header().Set(k, v)
	}
	w.WriteHeader(f.Response.Status)
	_, _ = w.Write(f.body())
}

// serveFixture returns a handler that always serves the named fixture.
func serveFixture(t *testing.T, name string) func(w http.ResponseWriter, r *http.Request, n int) {
	f := loadFixture(t, name)
	return func(w http.ResponseWriter, _ *http.Request, _ int) { f.serve(w) }
}

// writeText writes a plain-text response.
func writeText(w http.ResponseWriter, status int, body string) {
	w.Header().Set("Content-Type", "text/plain; charset=utf-8")
	w.WriteHeader(status)
	_, _ = w.Write([]byte(body))
}

// writeJSON writes a JSON response.
func writeJSON(w http.ResponseWriter, status int, body string) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(status)
	_, _ = w.Write([]byte(body))
}

// contextWithCancel returns a cancellable child of the test context.
func contextWithCancel(t *testing.T) (context.Context, context.CancelFunc) {
	return context.WithCancel(t.Context())
}

func jsonNumber(s string) json.Number { return json.Number(s) }

func ptrNumber(s string) *json.Number {
	n := json.Number(s)
	return &n
}
