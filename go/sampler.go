package jaeger

import (
	"context"
	"encoding/json"
	"fmt"
	"math"
	"net/http"
	"net/url"
	"strings"
	"sync"
	"sync/atomic"
	"time"

	sdktrace "go.opentelemetry.io/otel/sdk/trace"
	"go.opentelemetry.io/otel/trace"
)

// Remote sampling defaults.
const (
	// DefaultSamplingPollInterval is how often a RemoteSampler polls (Jaeger default).
	DefaultSamplingPollInterval = time.Minute
	// DefaultInitialSamplingRate is the probability a RemoteSampler applies until the
	// first strategy arrives (Jaeger and OpenTelemetry default).
	DefaultInitialSamplingRate = 0.001
)

// Strategy types of a SamplingStrategy.
const (
	StrategyProbabilistic = "PROBABILISTIC"
	StrategyRateLimiting  = "RATE_LIMITING"
)

// OperationStrategy is the sampling probability of one operation (span name).
type OperationStrategy struct {
	Operation    string  `json:"operation"`
	SamplingRate float64 `json:"samplingRate"`
}

// SamplingStrategy is a Jaeger remote sampling strategy (the /sampling response).
type SamplingStrategy struct {
	// StrategyType is StrategyProbabilistic or StrategyRateLimiting.
	StrategyType string `json:"strategyType"`
	// SamplingRate is probabilisticSampling.samplingRate (0..1).
	SamplingRate float64 `json:"samplingRate"`
	// MaxTracesPerSecond is rateLimitingSampling.maxTracesPerSecond (>= 0).
	MaxTracesPerSecond float64 `json:"maxTracesPerSecond"`
	// DefaultSamplingProbability is operationSampling.defaultSamplingProbability.
	DefaultSamplingProbability float64 `json:"defaultSamplingProbability"`
	// DefaultLowerBoundTracesPerSecond is operationSampling.defaultLowerBoundTracesPerSecond
	// (reported only; the SDK does not enforce lower bounds).
	DefaultLowerBoundTracesPerSecond float64 `json:"defaultLowerBoundTracesPerSecond"`
	// Operations are the per-operation probabilities (operationSampling.perOperationStrategies).
	Operations []OperationStrategy `json:"operations"`

	hasProbabilistic bool
	hasRateLimiting  bool
	hasOperations    bool
}

// Validate checks rates (0..1) and the traces-per-second limit (>= 0).
func (s SamplingStrategy) Validate() error {
	const op = "sampling.strategy"
	if s.StrategyType != StrategyProbabilistic && s.StrategyType != StrategyRateLimiting {
		return ValidationError(op, "unknown strategy type")
	}
	if !validRate(s.SamplingRate) || !validRate(s.DefaultSamplingProbability) {
		return ValidationError(op, "samplingRate must be between 0 and 1")
	}
	if s.MaxTracesPerSecond < 0 || math.IsNaN(s.MaxTracesPerSecond) || s.DefaultLowerBoundTracesPerSecond < 0 {
		return ValidationError(op, "maxTracesPerSecond must not be negative")
	}
	for _, o := range s.Operations {
		if !validRate(o.SamplingRate) {
			return ValidationError(op, "samplingRate must be between 0 and 1")
		}
	}
	return nil
}

func validRate(r float64) bool { return r >= 0 && r <= 1 }

// OperationRate returns the probability that applies to operation and whether it
// comes from a per-operation strategy.
func (s SamplingStrategy) OperationRate(operation string) (float64, bool) {
	for _, o := range s.Operations {
		if o.Operation == operation {
			return o.SamplingRate, true
		}
	}
	return 0, false
}

type wireProbabilistic struct {
	SamplingRate float64 `json:"samplingRate"`
}

type wireStrategy struct {
	StrategyType          json.RawMessage    `json:"strategyType"`
	ProbabilisticSampling *wireProbabilistic `json:"probabilisticSampling"`
	RateLimitingSampling  *struct {
		MaxTracesPerSecond float64 `json:"maxTracesPerSecond"`
	} `json:"rateLimitingSampling"`
	OperationSampling *struct {
		DefaultSamplingProbability       float64 `json:"defaultSamplingProbability"`
		DefaultLowerBoundTracesPerSecond float64 `json:"defaultLowerBoundTracesPerSecond"`
		PerOperationStrategies           []struct {
			Operation             string             `json:"operation"`
			ProbabilisticSampling *wireProbabilistic `json:"probabilisticSampling"`
		} `json:"perOperationStrategies"`
	} `json:"operationSampling"`
}

// ParseSamplingStrategy decodes and validates a /sampling response body. Missing
// numbers count as 0 (proto3 JSON omits zero values); a missing strategyType means
// PROBABILISTIC.
func ParseSamplingStrategy(body []byte) (SamplingStrategy, error) {
	const op = "sampling.strategy"
	var w wireStrategy
	if err := json.Unmarshal(body, &w); err != nil {
		return SamplingStrategy{}, ValidationError(op, "invalid JSON")
	}
	s := SamplingStrategy{Operations: []OperationStrategy{}}
	switch strings.Trim(string(w.StrategyType), `"`) {
	case "", "0", StrategyProbabilistic:
		s.StrategyType = StrategyProbabilistic
	case "1", StrategyRateLimiting:
		s.StrategyType = StrategyRateLimiting
	default:
		return SamplingStrategy{}, ValidationError(op, "unknown strategy type")
	}
	if w.ProbabilisticSampling != nil {
		s.hasProbabilistic = true
		s.SamplingRate = w.ProbabilisticSampling.SamplingRate
	}
	if w.RateLimitingSampling != nil {
		s.hasRateLimiting = true
		s.MaxTracesPerSecond = w.RateLimitingSampling.MaxTracesPerSecond
	}
	if o := w.OperationSampling; o != nil {
		s.hasOperations = true
		s.DefaultSamplingProbability = o.DefaultSamplingProbability
		s.DefaultLowerBoundTracesPerSecond = o.DefaultLowerBoundTracesPerSecond
		if !s.hasProbabilistic {
			s.SamplingRate = o.DefaultSamplingProbability
		}
		for _, p := range o.PerOperationStrategies {
			rate := 0.0
			if p.ProbabilisticSampling != nil {
				rate = p.ProbabilisticSampling.SamplingRate
			}
			s.Operations = append(s.Operations, OperationStrategy{Operation: p.Operation, SamplingRate: rate})
		}
	}
	if err := s.Validate(); err != nil {
		return SamplingStrategy{}, err
	}
	return s, nil
}

// fetchStrategy calls GET <samplingUrl>?service=<service>.
func fetchStrategy(ctx context.Context, e *endpoint, header http.Header, service string) (SamplingStrategy, error) {
	const op = "sampling.fetch"
	if strings.TrimSpace(service) == "" {
		return SamplingStrategy{}, ValidationError(op, "service is required")
	}
	res, err := e.t.do(ctx, e.request(op, header, url.Values{"service": {service}}))
	if err != nil {
		return SamplingStrategy{}, err
	}
	s, err := ParseSamplingStrategy(res.Body)
	if err != nil {
		return SamplingStrategy{}, &Error{Code: ErrorCodeUnavailable, Message: op + ": invalid strategy response", HTTPStatus: res.Status, RequestID: res.RequestID, Retryable: true, Err: err}
	}
	return s, nil
}

// FetchSamplingStrategy fetches the strategy of service from samplingURL (for example
// http://127.0.0.1:5778/sampling) with the transport settings of cfg (cfg.URL is
// ignored).
func FetchSamplingStrategy(ctx context.Context, samplingURL, service string, cfg CommonConfig) (SamplingStrategy, error) {
	e, err := samplingEndpoint(samplingURL, cfg)
	if err != nil {
		return SamplingStrategy{}, err
	}
	defer e.close()
	return fetchStrategy(ctx, e, nil, service)
}

func samplingEndpoint(samplingURL string, cfg CommonConfig) (*endpoint, error) {
	if !isHTTPURL(samplingURL) {
		return nil, ConfigError("samplingUrl must be an absolute http or https URL")
	}
	cfg.URL = samplingURL
	cfg = cfg.withDefaults(samplingURL)
	if err := cfg.validate(); err != nil {
		return nil, err
	}
	return newEndpoint(cfg, samplingURL)
}

// RemoteSamplerOption configures a RemoteSampler.
type RemoteSamplerOption func(*remoteSamplerOptions)

type remoteSamplerOptions struct {
	initialRate float64
	onError     func(error)
	httpConfig  CommonConfig
	now         func() time.Time
}

// WithInitialSamplingRate sets the probability used until the first strategy arrives
// (default DefaultInitialSamplingRate).
func WithInitialSamplingRate(rate float64) RemoteSamplerOption {
	return func(o *remoteSamplerOptions) { o.initialRate = rate }
}

// WithSamplerErrorHandler receives poll errors (the last good strategy stays active).
func WithSamplerErrorHandler(fn func(error)) RemoteSamplerOption {
	return func(o *remoteSamplerOptions) { o.onError = fn }
}

// WithSamplerHTTPConfig sets the transport settings (timeouts, retries, TLS, proxy,
// headers, hooks) of a sampler made by NewRemoteSampler. Client.NewRemoteSampler uses
// the client's settings instead.
func WithSamplerHTTPConfig(cfg CommonConfig) RemoteSamplerOption {
	return func(o *remoteSamplerOptions) { o.httpConfig = cfg }
}

// RemoteSampler is an OpenTelemetry head sampler driven by Jaeger remote sampling:
// it polls GET <samplingUrl>?service=<service> every poll interval and applies the
// strategy. Operations listed in the per-operation strategies are sampled with their
// own probability; every other operation uses the service-level rate limiter
// (whenever rateLimitingSampling is present) or probability. A failed or invalid poll keeps the
// last good strategy. Use it with InitTracing(…, WithSampler(s)) or
// sdktrace.WithSampler. Close stops polling.
type RemoteSampler struct {
	e        *endpoint
	header   http.Header
	owns     bool
	service  string
	interval time.Duration
	onError  func(error)
	now      func() time.Time
	initial  float64

	state atomic.Pointer[samplerState]

	ctx       context.Context
	cancel    context.CancelFunc
	done      chan struct{}
	closeOnce sync.Once
	mu        sync.Mutex
}

type samplerState struct {
	strategy SamplingStrategy
	fetched  bool
	ops      map[string]sdktrace.Sampler
	fallback sdktrace.Sampler
}

var _ sdktrace.Sampler = (*RemoteSampler)(nil)

// NewRemoteSampler starts a sampler for service that polls samplingURL (for example
// http://127.0.0.1:5778/sampling) every pollInterval (0 = DefaultSamplingPollInterval).
// The first poll starts at once in the background; call Refresh to wait for it.
func NewRemoteSampler(samplingURL, service string, pollInterval time.Duration, opts ...RemoteSamplerOption) (*RemoteSampler, error) {
	o := remoteSamplerOptions{}
	for _, opt := range opts {
		opt(&o)
	}
	e, err := samplingEndpoint(samplingURL, o.httpConfig)
	if err != nil {
		return nil, err
	}
	s, err := newRemoteSampler(e, nil, true, service, pollInterval, opts)
	if err != nil {
		e.close()
		return nil, err
	}
	return s, nil
}

func newRemoteSampler(e *endpoint, header http.Header, owns bool, service string, pollInterval time.Duration, opts []RemoteSamplerOption) (*RemoteSampler, error) {
	o := remoteSamplerOptions{initialRate: DefaultInitialSamplingRate, now: time.Now}
	for _, opt := range opts {
		opt(&o)
	}
	if strings.TrimSpace(service) == "" {
		return nil, ConfigError("sampling: service is required")
	}
	if !validRate(o.initialRate) {
		return nil, ConfigError("sampling: initial sampling rate must be between 0 and 1")
	}
	if pollInterval < 0 {
		return nil, ConfigError("sampling: poll interval must not be negative")
	}
	if pollInterval == 0 {
		pollInterval = DefaultSamplingPollInterval
	}
	ctx, cancel := context.WithCancel(context.Background())
	s := &RemoteSampler{
		e: e, header: header, owns: owns, service: service, interval: pollInterval,
		onError: o.onError, now: o.now, initial: o.initialRate, ctx: ctx, cancel: cancel, done: make(chan struct{}),
	}
	s.state.Store(&samplerState{
		strategy: SamplingStrategy{StrategyType: StrategyProbabilistic, SamplingRate: o.initialRate, Operations: []OperationStrategy{}},
		fallback: sdktrace.TraceIDRatioBased(o.initialRate),
	})
	go s.loop()
	return s, nil
}

func (s *RemoteSampler) loop() {
	defer close(s.done)
	ticker := time.NewTicker(s.interval)
	defer ticker.Stop()
	for {
		if err := s.Refresh(s.ctx); err != nil && s.ctx.Err() == nil && s.onError != nil {
			safeCall(func() { s.onError(err) })
		}
		select {
		case <-s.ctx.Done():
			return
		case <-ticker.C:
		}
	}
}

// Refresh fetches the strategy now and applies it. On error the previous strategy
// stays active and the error is returned.
func (s *RemoteSampler) Refresh(ctx context.Context) error {
	strategy, err := fetchStrategy(ctx, s.e, s.header, s.service)
	if err != nil {
		return err
	}
	s.apply(strategy)
	return nil
}

// apply builds the samplers of a strategy (decisions 4 and 7a: per-operation
// strategies win for listed operations; every other operation uses the rate limiter
// whenever rateLimitingSampling is present, else probabilisticSampling, else
// operationSampling.defaultSamplingProbability, else the initial rate).
func (s *RemoteSampler) apply(strategy SamplingStrategy) {
	s.mu.Lock()
	defer s.mu.Unlock()
	prev := s.state.Load()
	next := &samplerState{strategy: strategy, fetched: true, ops: make(map[string]sdktrace.Sampler, len(strategy.Operations))}
	for _, o := range strategy.Operations {
		next.ops[o.Operation] = sdktrace.TraceIDRatioBased(o.SamplingRate)
	}
	if strategy.hasRateLimiting {
		if old, ok := prev.fallback.(*rateLimitingSampler); ok && old.rate == strategy.MaxTracesPerSecond {
			next.fallback = old
		} else {
			next.fallback = newRateLimitingSampler(strategy.MaxTracesPerSecond, s.now)
		}
	} else if strategy.hasProbabilistic || strategy.hasOperations {
		next.fallback = sdktrace.TraceIDRatioBased(strategy.SamplingRate)
	} else {
		next.fallback = sdktrace.TraceIDRatioBased(s.initial)
	}
	s.state.Store(next)
}

// Strategy returns the active strategy and whether it came from the server (false
// while only the initial probability applies).
func (s *RemoteSampler) Strategy() (SamplingStrategy, bool) {
	st := s.state.Load()
	return st.strategy, st.fetched
}

// ShouldSample implements sdktrace.Sampler.
func (s *RemoteSampler) ShouldSample(p sdktrace.SamplingParameters) sdktrace.SamplingResult {
	st := s.state.Load()
	if sampler, ok := st.ops[p.Name]; ok {
		return sampler.ShouldSample(p)
	}
	return st.fallback.ShouldSample(p)
}

// Description implements sdktrace.Sampler.
func (s *RemoteSampler) Description() string {
	return fmt.Sprintf("JaegerRemoteSampler{service=%s,url=%s}", s.service, RedactURL(s.e.t.base.String()+s.e.path))
}

// Close stops polling. It is idempotent.
func (s *RemoteSampler) Close() error {
	s.closeOnce.Do(func() {
		s.cancel()
		<-s.done
		if s.owns {
			s.e.close()
		}
	})
	return nil
}

// rateLimitingSampler samples at most rate traces per second (token bucket with a
// burst of max(rate, 1), starting full: a rate of 0 lets the first trace through and
// then none, as in Jaeger).
type rateLimitingSampler struct {
	mu      sync.Mutex
	rate    float64
	burst   float64
	balance float64
	last    time.Time
	now     func() time.Time
}

func newRateLimitingSampler(rate float64, now func() time.Time) *rateLimitingSampler {
	burst := math.Max(rate, 1)
	return &rateLimitingSampler{rate: rate, burst: burst, balance: burst, last: now(), now: now}
}

func (r *rateLimitingSampler) allow() bool {
	r.mu.Lock()
	defer r.mu.Unlock()
	now := r.now()
	if elapsed := now.Sub(r.last).Seconds(); elapsed > 0 {
		r.balance = math.Min(r.burst, r.balance+elapsed*r.rate)
	}
	r.last = now
	if r.balance >= 1 {
		r.balance--
		return true
	}
	return false
}

func (r *rateLimitingSampler) ShouldSample(p sdktrace.SamplingParameters) sdktrace.SamplingResult {
	decision := sdktrace.Drop
	if r.allow() {
		decision = sdktrace.RecordAndSample
	}
	return sdktrace.SamplingResult{Decision: decision, Tracestate: trace.SpanContextFromContext(p.ParentContext).TraceState()}
}

func (r *rateLimitingSampler) Description() string {
	return fmt.Sprintf("RateLimitingSampler{%g}", r.rate)
}
