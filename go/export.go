package jaeger

import (
	"context"
	"net/http"
	"net/url"
	"strconv"
	"strings"
	"time"

	"go.opentelemetry.io/otel"
	sdktrace "go.opentelemetry.io/otel/sdk/trace"
)

// Jaeger OTLP export defaults (the all-in-one listens on 4318 for OTLP/HTTP and on
// 4317 for OTLP/gRPC).
const (
	DefaultOTLPHTTPEndpoint = "http://127.0.0.1:4318/v1/traces"
	DefaultOTLPGRPCEndpoint = "http://127.0.0.1:4317"
	// DefaultTracingProtocol is the Jaeger preset protocol: OTLP over HTTP/protobuf
	// (works through proxies). ProtocolGRPC selects OTLP/gRPC on 4317.
	DefaultTracingProtocol = ProtocolHTTPProtobuf
)

// TracingConfigFromEnv fills every unset field of explicit from the environment and
// applies the Jaeger preset (protocol http/protobuf, endpoint per protocol). env
// replaces the process environment when non-nil. Precedence: explicit >
// GROW_JAEGER_OTLP_* > OTEL_EXPORTER_OTLP_TRACES_* > OTEL_EXPORTER_OTLP_* > default:
//
//	Protocol     GROW_JAEGER_OTLP_PROTOCOL, OTEL_EXPORTER_OTLP_TRACES_PROTOCOL, OTEL_EXPORTER_OTLP_PROTOCOL
//	Endpoint     GROW_JAEGER_OTLP_GRPC_ENDPOINT or GROW_JAEGER_OTLP_HTTP_ENDPOINT (by protocol),
//	             GROW_JAEGER_OTLP_ENDPOINT, OTEL_EXPORTER_OTLP_TRACES_ENDPOINT (as is),
//	             OTEL_EXPORTER_OTLP_ENDPOINT (HTTP: "/v1/traces" appended); an HTTP endpoint
//	             without a path gets "/v1/traces" (see ResolveEndpoint)
//	Headers      GROW_JAEGER_OTLP_HEADERS, OTEL_EXPORTER_OTLP_TRACES_HEADERS, OTEL_EXPORTER_OTLP_HEADERS (k=v,k2=v2)
//	Compression  GROW_JAEGER_OTLP_COMPRESSION, OTEL_EXPORTER_OTLP_TRACES_COMPRESSION, OTEL_EXPORTER_OTLP_COMPRESSION
//	Timeout      GROW_JAEGER_OTLP_TIMEOUT, OTEL_EXPORTER_OTLP_TRACES_TIMEOUT, OTEL_EXPORTER_OTLP_TIMEOUT (Batch.ExportTimeout)
//	TLS CA       GROW_JAEGER_OTLP_CERTIFICATE, GROW_JAEGER_OTLP_CA_CERT, OTEL_EXPORTER_OTLP_TRACES_CERTIFICATE, OTEL_EXPORTER_OTLP_CERTIFICATE
//	TLS cert     GROW_JAEGER_OTLP_CLIENT_CERTIFICATE, OTEL_EXPORTER_OTLP_TRACES_CLIENT_CERTIFICATE, OTEL_EXPORTER_OTLP_CLIENT_CERTIFICATE
//	TLS key      GROW_JAEGER_OTLP_CLIENT_KEY, OTEL_EXPORTER_OTLP_TRACES_CLIENT_KEY, OTEL_EXPORTER_OTLP_CLIENT_KEY
//	ServiceName  GROW_JAEGER_SERVICE_NAME, OTEL_SERVICE_NAME, service.name in OTEL_RESOURCE_ATTRIBUTES
//	Resource     OTEL_RESOURCE_ATTRIBUTES (k=v,…; merged, explicit keys win)
//	Sampler      OTEL_TRACES_SAMPLER, OTEL_TRACES_SAMPLER_ARG (jaeger_remote and
//	             parentbased_jaeger_remote are applied by InitTracing)
//	Propagators  OTEL_PROPAGATORS
//
// A present but invalid value is a CONFIG error naming the variable, never its value.
func TracingConfigFromEnv(explicit TracingConfig, env map[string]string) (TracingConfig, error) {
	c, _, err := resolveTracingConfig(explicit, env)
	return c, err
}

// envRemote is a jaeger_remote sampler requested through OTEL_TRACES_SAMPLER.
type envRemote struct {
	parentBased bool
	samplingURL string
	interval    time.Duration
	initialRate float64
	hasInitial  bool
}

// otlpNames returns GROW_JAEGER_OTLP_<suffix> (without the prefix, for EnvReader),
// and the vendor chain GROW_JAEGER_OTLP_<alias>…, OTEL_EXPORTER_OTLP_TRACES_<suffix>,
// OTEL_EXPORTER_OTLP_<suffix>.
func otlpNames(suffix string, growAliases ...string) (string, []string) {
	vendors := make([]string, 0, len(growAliases)+2)
	for _, a := range growAliases {
		vendors = append(vendors, EnvPrefix+"OTLP_"+a)
	}
	vendors = append(vendors, "OTEL_EXPORTER_OTLP_TRACES_"+suffix, "OTEL_EXPORTER_OTLP_"+suffix)
	return "OTLP_" + suffix, vendors
}

func resolveTracingConfig(explicit TracingConfig, env map[string]string) (TracingConfig, *envRemote, error) {
	r := NewEnvReader(EnvPrefix, env)
	c := explicit
	if c.Protocol == "" {
		name, vendors := otlpNames("PROTOCOL")
		if v, variable, ok := r.find(name, vendors); ok {
			switch Protocol(strings.ToLower(strings.TrimSpace(v))) {
			case ProtocolGRPC:
				c.Protocol = ProtocolGRPC
			case ProtocolHTTPProtobuf:
				c.Protocol = ProtocolHTTPProtobuf
			default:
				return TracingConfig{}, nil, ConfigError(variable + " must be grpc or http/protobuf")
			}
		}
	}
	if c.Protocol == "" || c.Protocol == ProtocolNative {
		c.Protocol = DefaultTracingProtocol
	}
	if c.Endpoint == "" {
		c.Endpoint = endpointFromEnv(r, c.Protocol)
	}
	c.Endpoint = ResolveEndpoint(c.Protocol, c.Endpoint)
	if len(c.Headers) == 0 {
		name, vendors := otlpNames("HEADERS")
		if v, variable, ok := r.find(name, vendors); ok {
			h, err := parseKeyValueList(v)
			if err != nil {
				return TracingConfig{}, nil, ConfigError(variable + " must be a list of key=value pairs")
			}
			c.Headers = h
		}
	}
	if c.Compression == "" {
		name, vendors := otlpNames("COMPRESSION")
		if v, variable, ok := r.find(name, vendors); ok {
			switch strings.ToLower(strings.TrimSpace(v)) {
			case CompressionGzip:
				c.Compression = CompressionGzip
			case CompressionNone:
				c.Compression = CompressionNone
			default:
				return TracingConfig{}, nil, ConfigError(variable + " must be gzip or none")
			}
		}
	}
	if c.Batch.ExportTimeout == 0 {
		name, vendors := otlpNames("TIMEOUT")
		d, ok, err := r.Duration(name, vendors...)
		if err != nil {
			return TracingConfig{}, nil, err
		}
		if ok && d <= 0 {
			_, variable, _ := r.find(name, vendors)
			return TracingConfig{}, nil, ConfigError(variable + " must be a positive duration")
		}
		c.Batch.ExportTimeout = d
	}
	if c.TLS.CACertPath == "" && c.TLS.CACertPEM == "" {
		name, vendors := otlpNames("CERTIFICATE", "CA_CERT")
		c.TLS.CACertPath, _ = r.String(name, vendors...)
	}
	if c.TLS.ClientCertPath == "" {
		name, vendors := otlpNames("CLIENT_CERTIFICATE")
		c.TLS.ClientCertPath, _ = r.String(name, vendors...)
	}
	if c.TLS.ClientKeyPath == "" {
		name, vendors := otlpNames("CLIENT_KEY")
		c.TLS.ClientKeyPath, _ = r.String(name, vendors...)
	}
	var envServiceName string
	if v, variable, ok := r.find("", []string{"OTEL_RESOURCE_ATTRIBUTES"}); ok {
		attrs, err := parseKeyValueList(v)
		if err != nil {
			return TracingConfig{}, nil, ConfigError(variable + " must be a list of key=value pairs")
		}
		envServiceName = attrs["service.name"]
		delete(attrs, "service.name")
		merged := make(map[string]any, len(attrs)+len(c.ResourceAttributes))
		for k, val := range attrs {
			merged[k] = val
		}
		for k, val := range c.ResourceAttributes {
			merged[k] = val
		}
		c.ResourceAttributes = merged
	}
	if c.ServiceName == "" {
		c.ServiceName, _ = r.String("SERVICE_NAME", "OTEL_SERVICE_NAME")
	}
	if c.ServiceName == "" {
		c.ServiceName = envServiceName
	}
	var remote *envRemote
	if c.Sampler == "" {
		var err error
		if remote, err = samplerFromEnv(r, &c); err != nil {
			return TracingConfig{}, nil, err
		}
	}
	if len(c.Propagators) == 0 {
		if v, ok := r.String("", "OTEL_PROPAGATORS"); ok {
			for _, p := range strings.Split(v, ",") {
				if p = strings.TrimSpace(p); p != "" && p != "none" {
					c.Propagators = append(c.Propagators, p)
				}
			}
		}
	}
	return c, remote, nil
}

func samplerFromEnv(r *EnvReader, c *TracingConfig) (*envRemote, error) {
	v, variable, ok := r.find("", []string{"OTEL_TRACES_SAMPLER"})
	if !ok {
		return nil, nil
	}
	name := Sampler(strings.ToLower(strings.TrimSpace(v)))
	switch name {
	case SamplerAlwaysOn, SamplerAlwaysOff, SamplerTraceIDRatio, SamplerParentBasedAlwaysOn,
		SamplerParentBasedAlwaysOff, SamplerParentBasedTraceIDRatio:
		c.Sampler = name
	case "jaeger_remote", "parentbased_jaeger_remote":
		return remoteFromEnv(r, name == "parentbased_jaeger_remote")
	default:
		return nil, ConfigError(variable + " names an unknown sampler")
	}
	if name == SamplerTraceIDRatio || name == SamplerParentBasedTraceIDRatio {
		if arg, argVar, ok := r.find("", []string{"OTEL_TRACES_SAMPLER_ARG"}); ok {
			ratio, err := strconv.ParseFloat(strings.TrimSpace(arg), 64)
			if err != nil || ratio < 0 || ratio > 1 {
				return nil, ConfigError(argVar + " must be a number between 0 and 1")
			}
			c.SamplerRatio = ratio
		}
	}
	return nil, nil
}

// remoteFromEnv parses OTEL_TRACES_SAMPLER_ARG for jaeger_remote
// (endpoint=<url>,pollingIntervalMs=<ms>,initialSamplingRate=<0..1>).
func remoteFromEnv(r *EnvReader, parentBased bool) (*envRemote, error) {
	remote := &envRemote{parentBased: parentBased}
	arg, argVar, ok := r.find("", []string{"OTEL_TRACES_SAMPLER_ARG"})
	if !ok {
		return remote, nil
	}
	values, err := parseKeyValueList(arg)
	if err != nil {
		return nil, ConfigError(argVar + " must be a list of key=value pairs")
	}
	if v, ok := values["endpoint"]; ok {
		remote.samplingURL = v
	}
	if v, ok := values["pollingIntervalMs"]; ok {
		d, err := ParseDuration(v)
		if err != nil || d <= 0 {
			return nil, ConfigError(argVar + " has an invalid pollingIntervalMs")
		}
		remote.interval = d
	}
	if v, ok := values["initialSamplingRate"]; ok {
		rate, err := strconv.ParseFloat(v, 64)
		if err != nil || !validRate(rate) {
			return nil, ConfigError(argVar + " has an invalid initialSamplingRate")
		}
		remote.initialRate, remote.hasInitial = rate, true
	}
	return remote, nil
}

// ResolveEndpoint returns the exporter URL for protocol: without an endpoint the
// Jaeger default (http://127.0.0.1:4318/v1/traces or http://127.0.0.1:4317); for
// http/protobuf an endpoint without a path (a base such as http://jaeger:4318) gets
// "/v1/traces"; any other endpoint is used as is.
func ResolveEndpoint(protocol Protocol, endpoint string) string {
	if strings.TrimSpace(endpoint) == "" {
		if protocol == ProtocolGRPC {
			return DefaultOTLPGRPCEndpoint
		}
		return DefaultOTLPHTTPEndpoint
	}
	if protocol == ProtocolGRPC {
		return endpoint
	}
	if u, err := url.Parse(endpoint); err == nil && (u.Path == "" || u.Path == "/") {
		return strings.TrimRight(endpoint, "/") + "/v1/traces"
	}
	return endpoint
}

// endpointFromEnv picks the exporter endpoint for protocol from the environment
// ("" when none is set).
func endpointFromEnv(r *EnvReader, protocol Protocol) string {
	specific := "OTLP_HTTP_ENDPOINT"
	if protocol == ProtocolGRPC {
		specific = "OTLP_GRPC_ENDPOINT"
	}
	if v, ok := r.String(specific); ok {
		return v
	}
	if v, ok := r.String("OTLP_ENDPOINT"); ok {
		return v
	}
	if v, ok := r.String("", "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT"); ok {
		return v
	}
	if v, ok := r.String("", "OTEL_EXPORTER_OTLP_ENDPOINT"); ok {
		if protocol == ProtocolGRPC {
			return v
		}
		return strings.TrimSuffix(v, "/") + "/v1/traces"
	}
	return ""
}

// parseKeyValueList parses "k=v,k2=v2" (OTel list format; keys and values are
// URL-decoded and trimmed).
func parseKeyValueList(s string) (map[string]string, error) {
	out := map[string]string{}
	for _, part := range strings.Split(s, ",") {
		if strings.TrimSpace(part) == "" {
			continue
		}
		k, v, ok := strings.Cut(part, "=")
		if !ok {
			return nil, errInvalidList
		}
		key, err1 := url.QueryUnescape(strings.TrimSpace(k))
		val, err2 := url.QueryUnescape(strings.TrimSpace(v))
		if err1 != nil || err2 != nil || key == "" {
			return nil, errInvalidList
		}
		out[key] = val
	}
	return out, nil
}

var errInvalidList = ConfigError("invalid key=value list")

// NewExporter is the Jaeger ExporterFactory: it applies the Jaeger preset to cfg
// (protocol http/protobuf by default, endpoint per protocol, see TracingConfigFromEnv
// with the process environment) and builds the official OTLP exporter.
func NewExporter(ctx context.Context, cfg TracingConfig) (sdktrace.SpanExporter, error) {
	resolved, err := TracingConfigFromEnv(cfg, nil)
	if err != nil {
		return nil, err
	}
	return NewOTLPExporter(ctx, resolved)
}

// TracingOption customizes InitTracing.
type TracingOption func(*tracingOptions)

type tracingOptions struct {
	env           map[string]string
	sampler       sdktrace.Sampler
	factory       ExporterFactory
	samplerConfig *Config
}

// WithEnv makes InitTracing read env instead of the process environment.
func WithEnv(env map[string]string) TracingOption {
	return func(o *tracingOptions) { o.env = env }
}

// WithSampler replaces the head sampler named by TracingConfig.Sampler, for example
// with a *RemoteSampler (not closed by the handle). When TracingConfig.Sampler is a
// parentbased_* sampler (the default), s is wrapped in ParentBased (sampled parents
// are kept, unsampled parents dropped).
func WithSampler(s sdktrace.Sampler) TracingOption {
	return func(o *tracingOptions) { o.sampler = s }
}

// WithSamplerConfig sets the settings (SamplingURL, token, TLS, timeouts) of a
// remote sampler that InitTracing creates for OTEL_TRACES_SAMPLER=jaeger_remote.
// Default: ConfigFromEnvWith over the same environment (GROW_JAEGER_SAMPLING_URL,
// JAEGER_SAMPLING_ENDPOINT).
func WithSamplerConfig(cfg Config) TracingOption {
	return func(o *tracingOptions) { o.samplerConfig = &cfg }
}

// WithExporterFactory replaces the OTLP exporter (for example with an in-memory
// exporter in tests). The factory receives the resolved config.
func WithExporterFactory(f ExporterFactory) TracingOption {
	return func(o *tracingOptions) { o.factory = f }
}

// TracingSetup is a running Jaeger tracing setup (the other languages call it
// JaegerTracingHandle): the category TracingHandle
// plus the exporter endpoint and the remote sampler in use. Shutdown also closes a
// remote sampler that InitTracing created.
type TracingSetup struct {
	*TracingHandle
	endpoint   string
	remote     *RemoteSampler
	ownsRemote bool
}

// Endpoint returns the exporter endpoint in use.
func (h *TracingSetup) Endpoint() string { return h.endpoint }

// RemoteSampler returns the remote sampler created for OTEL_TRACES_SAMPLER=jaeger_remote,
// or nil.
func (h *TracingSetup) RemoteSampler() *RemoteSampler { return h.remote }

// Shutdown flushes and stops the provider and closes an owned remote sampler. It is
// idempotent.
func (h *TracingSetup) Shutdown(ctx context.Context) error {
	err := h.TracingHandle.Shutdown(ctx)
	if h.ownsRemote {
		_ = h.remote.Close()
	}
	return err
}

// InitTracing starts OpenTelemetry tracing that exports to Jaeger over OTLP. It
// resolves cfg with TracingConfigFromEnv (the Jaeger preset: OTLP HTTP/protobuf to
// http://127.0.0.1:4318/v1/traces unless configured otherwise), builds the exporter,
// a batch span processor, the sampler, the resource and the propagators, and installs
// them as the otel globals unless DisableSetGlobal is set. With
// OTEL_TRACES_SAMPLER=jaeger_remote (or parentbased_jaeger_remote) and no explicit
// sampler it starts a RemoteSampler (URL from OTEL_TRACES_SAMPLER_ARG endpoint=…, else
// the sampling config, see WithSamplerConfig) that the handle closes on Shutdown.
// Call Shutdown on the handle before exit.
func InitTracing(ctx context.Context, cfg TracingConfig, opts ...TracingOption) (*TracingSetup, error) {
	o := tracingOptions{factory: NewOTLPExporter}
	for _, opt := range opts {
		opt(&o)
	}
	resolved, remote, err := resolveTracingConfig(cfg, o.env)
	if err != nil {
		return nil, err
	}
	if o.sampler != nil {
		h, err := initTracingWithSampler(ctx, resolved, o.factory, o.sampler, isParentBased(resolved.WithDefaults().Sampler))
		if err != nil {
			return nil, err
		}
		return &TracingSetup{TracingHandle: h, endpoint: resolved.Endpoint}, nil
	}
	if remote == nil {
		h, err := InitTracingWith(ctx, resolved, o.factory)
		if err != nil {
			return nil, err
		}
		return &TracingSetup{TracingHandle: h, endpoint: resolved.Endpoint}, nil
	}
	if err := resolved.Validate(); err != nil {
		return nil, err
	}
	var scfg Config
	if o.samplerConfig != nil {
		scfg = o.samplerConfig.withServiceDefaults()
	} else if scfg, err = ConfigFromEnvWith(Config{}, o.env); err != nil {
		return nil, err
	}
	if remote.samplingURL != "" {
		scfg.SamplingURL = remote.samplingURL
	}
	var sopts []RemoteSamplerOption
	if remote.hasInitial {
		sopts = append(sopts, WithInitialSamplingRate(remote.initialRate))
	}
	sampler, err := newConfiguredSampler(scfg, resolved.ServiceName, remote.interval, sopts)
	if err != nil {
		return nil, err
	}
	h, err := initTracingWithSampler(ctx, resolved, o.factory, sampler, remote.parentBased)
	if err != nil {
		_ = sampler.Close()
		return nil, err
	}
	return &TracingSetup{TracingHandle: h, endpoint: resolved.Endpoint, remote: sampler, ownsRemote: true}, nil
}

// newConfiguredSampler starts a RemoteSampler with the settings and token of cfg.
func newConfiguredSampler(cfg Config, service string, interval time.Duration, opts []RemoteSamplerOption) (*RemoteSampler, error) {
	if err := cfg.Validate(); err != nil {
		return nil, err
	}
	token := cfg.Token.Reveal()
	e, err := newEndpoint(cfg.CommonConfig, cfg.SamplingURL, token)
	if err != nil {
		return nil, err
	}
	header := http.Header{}
	if token != "" {
		header.Set("Authorization", "Bearer "+token)
	}
	s, err := newRemoteSampler(e, header, true, service, interval, opts)
	if err != nil {
		e.close()
		return nil, err
	}
	return s, nil
}

func isParentBased(s Sampler) bool {
	switch s {
	case SamplerParentBasedAlwaysOn, SamplerParentBasedAlwaysOff, SamplerParentBasedTraceIDRatio:
		return true
	}
	return false
}

// initTracingWithSampler is InitTracingWith with a caller-supplied head sampler
// (wrapped in ParentBased when parentBased is true).
func initTracingWithSampler(ctx context.Context, cfg TracingConfig, factory ExporterFactory, sampler sdktrace.Sampler, parentBased bool) (*TracingHandle, error) {
	if err := cfg.Validate(); err != nil {
		return nil, err
	}
	cfg = cfg.WithDefaults()
	exporter, err := factory(ctx, cfg)
	if err != nil {
		return nil, err
	}
	res, err := buildResource(ctx, cfg)
	if err != nil {
		_ = exporter.Shutdown(ctx)
		return nil, err
	}
	if parentBased {
		sampler = sdktrace.ParentBased(sampler)
	}
	provider := sdktrace.NewTracerProvider(
		sdktrace.WithResource(res),
		sdktrace.WithSampler(sampler),
		sdktrace.WithBatcher(exporter,
			sdktrace.WithMaxQueueSize(cfg.Batch.MaxQueueSize),
			sdktrace.WithMaxExportBatchSize(cfg.Batch.MaxExportBatchSize),
			sdktrace.WithBatchTimeout(cfg.Batch.ScheduledDelay),
			sdktrace.WithExportTimeout(cfg.Batch.ExportTimeout),
		),
	)
	h := &TracingHandle{provider: provider, propagator: buildPropagator(cfg.Propagators), config: cfg}
	if !cfg.DisableSetGlobal {
		otel.SetTracerProvider(provider)
		otel.SetTextMapPropagator(h.propagator)
	}
	return h, nil
}

// SamplingPreset is a named head-sampling choice for TracingConfig.
type SamplingPreset struct {
	// Sampler is the sampler name.
	Sampler Sampler
	// Ratio is the ratio for the *traceidratio samplers.
	Ratio float64
}

// Sampling presets.
var (
	// SamplingDevelopment records every trace (always_on).
	SamplingDevelopment = SamplingPreset{Sampler: SamplerAlwaysOn, Ratio: 1}
	// SamplingProduction keeps 10 % of new traces and follows the parent decision
	// (parentbased_traceidratio 0.1).
	SamplingProduction = SamplingPreset{Sampler: SamplerParentBasedTraceIDRatio, Ratio: 0.1}
	// SamplingFollowParent records new traces and follows the parent decision (the
	// OpenTelemetry default, parentbased_always_on).
	SamplingFollowParent = SamplingPreset{Sampler: SamplerParentBasedAlwaysOn, Ratio: 1}
	// SamplingOff records nothing (always_off).
	SamplingOff = SamplingPreset{Sampler: SamplerAlwaysOff, Ratio: 1}
)

// SamplingRatio returns a parentbased_traceidratio preset with ratio (0..1). A ratio
// of 0 or less returns parentbased_always_off, 1 or more parentbased_always_on.
func SamplingRatio(ratio float64) SamplingPreset {
	switch {
	case ratio <= 0:
		return SamplingPreset{Sampler: SamplerParentBasedAlwaysOff, Ratio: 1}
	case ratio >= 1:
		return SamplingFollowParent
	}
	return SamplingPreset{Sampler: SamplerParentBasedTraceIDRatio, Ratio: ratio}
}

// Apply returns cfg with the preset's sampler and ratio.
func (p SamplingPreset) Apply(cfg TracingConfig) TracingConfig {
	cfg.Sampler = p.Sampler
	cfg.SamplerRatio = p.Ratio
	return cfg
}
