package jaeger

import (
	"context"
	"errors"
	"net/http"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	sdktrace "go.opentelemetry.io/otel/sdk/trace"
	"go.opentelemetry.io/otel/trace"
)

func TestParseSamplingStrategy(t *testing.T) {
	cases := map[string]func(t *testing.T, s SamplingStrategy){
		"sampling.default_stock": func(t *testing.T, s SamplingStrategy) {
			if s.StrategyType != StrategyProbabilistic || s.SamplingRate != 1 || len(s.Operations) != 0 {
				t.Fatalf("%+v", s)
			}
		},
		"sampling.per_operation": func(t *testing.T, s SamplingStrategy) {
			if s.SamplingRate != 0.8 || s.DefaultSamplingProbability != 0.8 {
				t.Fatalf("%+v", s)
			}
			if r, ok := s.OperationRate("GET /fast"); !ok || r != 0.2 {
				t.Fatalf("GET /fast %v %v", r, ok)
			}
			if r, ok := s.OperationRate("/health"); !ok || r != 0 {
				t.Fatalf("/health %v %v", r, ok)
			}
			if _, ok := s.OperationRate("other"); ok {
				t.Fatal("other")
			}
		},
		"sampling.rate_limiting": func(t *testing.T, s SamplingStrategy) {
			if s.StrategyType != StrategyRateLimiting || s.MaxTracesPerSecond != 5 {
				t.Fatalf("%+v", s)
			}
		},
		"sampling.probabilistic": func(t *testing.T, s SamplingStrategy) {
			if s.SamplingRate != 0.25 {
				t.Fatalf("%+v", s)
			}
		},
		"sampling.default_strategy": func(t *testing.T, s SamplingStrategy) {
			if s.SamplingRate != 0.5 {
				t.Fatalf("%+v", s)
			}
		},
		"sampling.after_reload": func(t *testing.T, s SamplingStrategy) {
			if s.SamplingRate != 0 {
				t.Fatalf("%+v", s)
			}
		},
	}
	for name, check := range cases {
		t.Run(name, func(t *testing.T) {
			s, err := ParseSamplingStrategy(loadFixture(t, name).body())
			if err != nil {
				t.Fatal(err)
			}
			check(t, s)
		})
	}

	t.Run("proto3 zero values and enums", func(t *testing.T) {
		s, err := ParseSamplingStrategy([]byte(`{"operationSampling":{"defaultSamplingProbability":0.3,"perOperationStrategies":[{"operation":"x","probabilisticSampling":{}},{"operation":"y"}]}}`))
		if err != nil || s.StrategyType != StrategyProbabilistic || s.SamplingRate != 0.3 || len(s.Operations) != 2 || s.Operations[1].SamplingRate != 0 {
			t.Fatalf("%+v %v", s, err)
		}
		s, err = ParseSamplingStrategy([]byte(`{"strategyType":1,"rateLimitingSampling":{"maxTracesPerSecond":2}}`))
		if err != nil || s.StrategyType != StrategyRateLimiting || s.MaxTracesPerSecond != 2 {
			t.Fatalf("%+v %v", s, err)
		}
	})

	t.Run("invalid", func(t *testing.T) {
		for _, body := range []string{
			`{`, `{"strategyType":"ADAPTIVE"}`, `{"probabilisticSampling":{"samplingRate":1.5}}`,
			`{"strategyType":"RATE_LIMITING","rateLimitingSampling":{"maxTracesPerSecond":-1}}`,
			`{"operationSampling":{"perOperationStrategies":[{"operation":"x","probabilisticSampling":{"samplingRate":-0.1}}]}}`,
		} {
			if _, err := ParseSamplingStrategy([]byte(body)); !errors.Is(err, ErrValidation) {
				t.Fatalf("%s: %v", body, err)
			}
		}
		if err := (SamplingStrategy{}).Validate(); !errors.Is(err, ErrValidation) {
			t.Fatalf("empty type: %v", err)
		}
	})
}

// samplingServer serves the recorded strategy per service, plus failure modes.
type samplingServer struct {
	*testServer
	mu       sync.Mutex
	override map[string]string // service → raw body
	fail     atomic.Bool
}

func newSamplingServer(t *testing.T) *samplingServer {
	s := &samplingServer{override: map[string]string{}}
	fixtures := map[string]fixture{
		"sdk-sampling-ops":  loadFixture(t, "sampling.per_operation"),
		"sdk-sampling-rate": loadFixture(t, "sampling.rate_limiting"),
		"sdk-sampling-prob": loadFixture(t, "sampling.probabilistic"),
	}
	missing := loadFixture(t, "sampling.missing_service")
	def := loadFixture(t, "sampling.default_strategy")
	s.testServer = newTestServer(t, func(w http.ResponseWriter, r *http.Request, _ int) {
		if s.fail.Load() {
			writeText(w, http.StatusBadRequest, "nope")
			return
		}
		svc := r.URL.Query().Get("service")
		s.mu.Lock()
		body, ok := s.override[svc]
		s.mu.Unlock()
		switch {
		case ok:
			writeJSON(w, 200, body)
		case svc == "":
			missing.serve(w)
		default:
			if f, ok := fixtures[svc]; ok {
				f.serve(w)
				return
			}
			def.serve(w)
		}
	})
	return s
}

func (s *samplingServer) set(service, body string) {
	s.mu.Lock()
	defer s.mu.Unlock()
	s.override[service] = body
}

func params(name string) sdktrace.SamplingParameters {
	return sdktrace.SamplingParameters{ParentContext: context.Background(), TraceID: trace.TraceID{0xff, 1}, Name: name}
}

func sampled(s sdktrace.Sampler, name string) bool {
	return s.ShouldSample(params(name)).Decision == sdktrace.RecordAndSample
}

func TestRemoteSampler(t *testing.T) {
	srv := newSamplingServer(t)

	t.Run("initial then fetched", func(t *testing.T) {
		srv.set("svc-a", `{"strategyType":"PROBABILISTIC","probabilisticSampling":{"samplingRate":1},"operationSampling":{"defaultSamplingProbability":1,"perOperationStrategies":[{"operation":"/health","probabilisticSampling":{"samplingRate":0}}]}}`)
		s, err := NewRemoteSampler(srv.URL+"/sampling", "svc-a", time.Hour, WithInitialSamplingRate(0))
		if err != nil {
			t.Fatal(err)
		}
		defer func() { _ = s.Close() }()
		if err := s.Refresh(t.Context()); err != nil {
			t.Fatal(err)
		}
		st, fetched := s.Strategy()
		if !fetched || st.SamplingRate != 1 {
			t.Fatalf("%+v %v", st, fetched)
		}
		if !sampled(s, "GET /orders") || sampled(s, "/health") {
			t.Fatal("per-operation decision")
		}
		if !strings.Contains(s.Description(), "svc-a") {
			t.Fatal(s.Description())
		}
		if got := srv.request(0).URL; got.Path != "/sampling" || got.Query().Get("service") != "svc-a" {
			t.Fatalf("request %s", got)
		}
		// A failed poll keeps the last good strategy.
		srv.fail.Store(true)
		if err := s.Refresh(t.Context()); !errors.Is(err, ErrValidation) {
			t.Fatalf("failed poll: %v", err)
		}
		srv.fail.Store(false)
		srv.set("svc-a", `{"probabilisticSampling":{"samplingRate":7}}`)
		if err := s.Refresh(t.Context()); !errors.Is(err, ErrUnavailable) {
			t.Fatalf("invalid strategy: %v", err)
		}
		if st, _ := s.Strategy(); st.SamplingRate != 1 || !sampled(s, "GET /orders") {
			t.Fatal("last good strategy must stay")
		}
	})

	t.Run("initial rate before first poll", func(t *testing.T) {
		blocked := make(chan struct{})
		slow := newTestServer(t, func(w http.ResponseWriter, _ *http.Request, _ int) {
			<-blocked
			writeJSON(w, 200, `{}`)
		})
		s, err := NewRemoteSampler(slow.URL+"/sampling", "svc", time.Hour)
		if err != nil {
			t.Fatal(err)
		}
		st, fetched := s.Strategy()
		if fetched || st.SamplingRate != DefaultInitialSamplingRate {
			t.Fatalf("%+v %v", st, fetched)
		}
		close(blocked)
		_ = s.Close()
		_ = s.Close()
	})

	t.Run("operations win over rate limiting", func(t *testing.T) {
		now := time.Unix(1000, 0)
		var mu sync.Mutex
		clock := func() time.Time { mu.Lock(); defer mu.Unlock(); return now }
		advance := func(d time.Duration) { mu.Lock(); now = now.Add(d); mu.Unlock() }
		srv.set("svc-r", `{"strategyType":"RATE_LIMITING","rateLimitingSampling":{"maxTracesPerSecond":2},"operationSampling":{"defaultSamplingProbability":0.5,"perOperationStrategies":[{"operation":"/health","probabilisticSampling":{"samplingRate":0}},{"operation":"/always","probabilisticSampling":{"samplingRate":1}}]}}`)
		s, err := NewRemoteSampler(srv.URL+"/sampling", "svc-r", time.Hour, func(o *remoteSamplerOptions) { o.now = clock })
		if err != nil {
			t.Fatal(err)
		}
		defer func() { _ = s.Close() }()
		if err := s.Refresh(t.Context()); err != nil {
			t.Fatal(err)
		}
		if sampled(s, "/health") || !sampled(s, "/always") || !sampled(s, "/always") || !sampled(s, "/always") {
			t.Fatal("per-operation strategies must win")
		}
		if !sampled(s, "a") || !sampled(s, "b") || sampled(s, "c") {
			t.Fatal("rate limit of 2/s")
		}
		advance(500 * time.Millisecond)
		if !sampled(s, "d") || sampled(s, "e") {
			t.Fatal("refill after 0.5 s")
		}
		// The same rate keeps the limiter (no burst reset); a new rate replaces it.
		if err := s.Refresh(t.Context()); err != nil {
			t.Fatal(err)
		}
		if sampled(s, "f") {
			t.Fatal("limiter must survive a refresh with the same rate")
		}
		srv.set("svc-r", `{"strategyType":"RATE_LIMITING","rateLimitingSampling":{"maxTracesPerSecond":0}}`)
		if err := s.Refresh(t.Context()); err != nil {
			t.Fatal(err)
		}
		advance(10 * time.Second)
		if !sampled(s, "g") || sampled(s, "g2") {
			t.Fatal("rate 0 lets the first trace through, then none")
		}
		advance(10 * time.Second)
		if sampled(s, "g3") {
			t.Fatal("rate 0 never refills")
		}
		srv.set("svc-r", `{"strategyType":"RATE_LIMITING","probabilisticSampling":{"samplingRate":1}}`)
		if err := s.Refresh(t.Context()); err != nil || !sampled(s, "h") {
			t.Fatalf("rate limiting without limiter falls back to probability: %v", err)
		}
		srv.set("svc-r", `{"strategyType":"PROBABILISTIC","probabilisticSampling":{"samplingRate":1},"rateLimitingSampling":{"maxTracesPerSecond":1}}`)
		if err := s.Refresh(t.Context()); err != nil || !sampled(s, "i") || sampled(s, "j") {
			t.Fatalf("rateLimitingSampling applies whenever present: %v", err)
		}
		srv.set("svc-r", `{"strategyType":"RATE_LIMITING"}`)
		if err := s.Refresh(t.Context()); err != nil || !strings.Contains(s.state.Load().fallback.Description(), "0.001") {
			t.Fatalf("no service-level strategy uses the initial rate (0.001): %v", err)
		}
	})

	t.Run("rate limiting fixture", func(t *testing.T) {
		s, err := NewRemoteSampler(srv.URL+"/sampling", "sdk-sampling-rate", time.Hour)
		if err != nil {
			t.Fatal(err)
		}
		defer func() { _ = s.Close() }()
		if err := s.Refresh(t.Context()); err != nil {
			t.Fatal(err)
		}
		n := 0
		for i := 0; i < 50; i++ {
			if sampled(s, "op") {
				n++
			}
		}
		if n < 5 || n > 6 {
			t.Fatalf("sampled %d of 50 with 5/s", n)
		}
		if sampled(s, "/health") {
			t.Fatal("default /health operation must be dropped")
		}
		r := s.state.Load().fallback
		if !strings.Contains(r.Description(), "5") || r.ShouldSample(params("x")).Tracestate.Len() != 0 {
			t.Fatal(r.Description())
		}
	})

	t.Run("polling reports errors", func(t *testing.T) {
		var errs atomic.Int32
		s, err := NewRemoteSampler(srv.URL+"/sampling", "sdk-sampling-prob", 5*time.Millisecond, WithSamplerErrorHandler(func(error) { errs.Add(1) }))
		if err != nil {
			t.Fatal(err)
		}
		deadline := time.Now().Add(5 * time.Second)
		for {
			if st, ok := s.Strategy(); ok && st.SamplingRate == 0.25 {
				break
			}
			if time.Now().After(deadline) {
				t.Fatal("first poll never applied")
			}
			time.Sleep(5 * time.Millisecond)
		}
		srv.fail.Store(true)
		for errs.Load() == 0 {
			if time.Now().After(deadline) {
				t.Fatal("poll errors never reported")
			}
			time.Sleep(5 * time.Millisecond)
		}
		srv.fail.Store(false)
		_ = s.Close()
		if st, _ := s.Strategy(); st.SamplingRate != 0.25 {
			t.Fatal("last good strategy must stay")
		}
	})

	t.Run("constructor errors", func(t *testing.T) {
		for _, tc := range []struct {
			url, service string
			interval     time.Duration
			opts         []RemoteSamplerOption
		}{
			{"nope", "s", 0, nil},
			{srv.URL, "", 0, nil},
			{srv.URL, "s", -time.Second, nil},
			{srv.URL, "s", 0, []RemoteSamplerOption{WithInitialSamplingRate(2)}},
			{srv.URL, "s", 0, []RemoteSamplerOption{WithSamplerHTTPConfig(CommonConfig{TLS: TLSConfig{CACertPEM: "bad"}})}},
			{srv.URL, "s", 0, []RemoteSamplerOption{WithSamplerHTTPConfig(CommonConfig{ConnectTimeout: -1})}},
		} {
			if _, err := NewRemoteSampler(tc.url, tc.service, tc.interval, tc.opts...); !errors.Is(err, ErrConfig) {
				t.Fatalf("%+v: %v", tc, err)
			}
		}
		s, err := NewRemoteSampler(srv.URL+"/sampling", "s", 0)
		if err != nil || s.interval != DefaultSamplingPollInterval {
			t.Fatalf("default interval: %v", err)
		}
		_ = s.Close()
	})

	t.Run("fetch", func(t *testing.T) {
		s, err := FetchSamplingStrategy(t.Context(), srv.URL+"/sampling", "sdk-sampling-ops", CommonConfig{Headers: map[string]string{"X-Tenant": "t"}})
		if err != nil || len(s.Operations) == 0 {
			t.Fatalf("%+v %v", s, err)
		}
		if srv.request(int(srv.calls.Load())-1).Header.Get("X-Tenant") != "t" {
			t.Fatal("headers not sent")
		}
		if _, err := FetchSamplingStrategy(t.Context(), srv.URL+"/sampling", " ", CommonConfig{}); !errors.Is(err, ErrValidation) {
			t.Fatalf("empty service: %v", err)
		}
		if _, err := FetchSamplingStrategy(t.Context(), "ftp://x", "s", CommonConfig{}); !errors.Is(err, ErrConfig) {
			t.Fatalf("bad url: %v", err)
		}
		missing := newTestServer(t, serveFixture(t, "sampling.missing_service"))
		_, err = FetchSamplingStrategy(t.Context(), missing.URL+"/sampling", "s", CommonConfig{})
		if !errors.Is(err, ErrValidation) || !strings.Contains(err.Error(), "'service' parameter") {
			t.Fatalf("400: %v", err)
		}
	})
}
