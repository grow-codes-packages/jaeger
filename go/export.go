package jaeger

import (
	"context"
	"net/url"
	"strconv"
	"strings"

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
//	             OTEL_EXPORTER_OTLP_ENDPOINT (HTTP: "/v1/traces" appended)
//	Headers      GROW_JAEGER_OTLP_HEADERS, OTEL_EXPORTER_OTLP_TRACES_HEADERS, OTEL_EXPORTER_OTLP_HEADERS (k=v,k2=v2)
//	Compression  GROW_JAEGER_OTLP_COMPRESSION, OTEL_EXPORTER_OTLP_TRACES_COMPRESSION, OTEL_EXPORTER_OTLP_COMPRESSION
//	TLS CA       GROW_JAEGER_OTLP_CA_CERT, OTEL_EXPORTER_OTLP_TRACES_CERTIFICATE, OTEL_EXPORTER_OTLP_CERTIFICATE
//	ServiceName  GROW_JAEGER_SERVICE_NAME, OTEL_SERVICE_NAME
//	Resource     OTEL_RESOURCE_ATTRIBUTES (k=v,…; explicit keys win)
//	Sampler      OTEL_TRACES_SAMPLER, OTEL_TRACES_SAMPLER_ARG (jaeger_remote values are
//	             ignored: pass a RemoteSampler with WithSampler instead)
//	Propagators  OTEL_PROPAGATORS
//
// A present but invalid value is a CONFIG error naming the variable, never its value.
func TracingConfigFromEnv(explicit TracingConfig, env map[string]string) (TracingConfig, error) {
	r := NewEnvReader(EnvPrefix, env)
	c := explicit
	if c.Protocol == "" {
		if v, variable, ok := r.find("OTLP_PROTOCOL", []string{"OTEL_EXPORTER_OTLP_TRACES_PROTOCOL", "OTEL_EXPORTER_OTLP_PROTOCOL"}); ok {
			switch Protocol(strings.ToLower(strings.TrimSpace(v))) {
			case ProtocolGRPC:
				c.Protocol = ProtocolGRPC
			case ProtocolHTTPProtobuf:
				c.Protocol = ProtocolHTTPProtobuf
			default:
				return TracingConfig{}, ConfigError(variable + " must be grpc or http/protobuf")
			}
		}
	}
	if c.Protocol == "" || c.Protocol == ProtocolNative {
		c.Protocol = DefaultTracingProtocol
	}
	if c.Endpoint == "" {
		c.Endpoint = resolveEndpoint(r, c.Protocol)
	}
	if len(c.Headers) == 0 {
		if v, variable, ok := r.find("OTLP_HEADERS", []string{"OTEL_EXPORTER_OTLP_TRACES_HEADERS", "OTEL_EXPORTER_OTLP_HEADERS"}); ok {
			h, err := parseKeyValueList(v)
			if err != nil {
				return TracingConfig{}, ConfigError(variable + " must be a list of key=value pairs")
			}
			c.Headers = h
		}
	}
	if c.Compression == "" {
		if v, variable, ok := r.find("OTLP_COMPRESSION", []string{"OTEL_EXPORTER_OTLP_TRACES_COMPRESSION", "OTEL_EXPORTER_OTLP_COMPRESSION"}); ok {
			switch strings.ToLower(strings.TrimSpace(v)) {
			case CompressionGzip:
				c.Compression = CompressionGzip
			case CompressionNone:
				c.Compression = CompressionNone
			default:
				return TracingConfig{}, ConfigError(variable + " must be gzip or none")
			}
		}
	}
	if c.TLS.CACertPath == "" && c.TLS.CACertPEM == "" {
		c.TLS.CACertPath, _ = r.String("OTLP_CA_CERT", "OTEL_EXPORTER_OTLP_TRACES_CERTIFICATE", "OTEL_EXPORTER_OTLP_CERTIFICATE")
	}
	if c.ServiceName == "" {
		c.ServiceName, _ = r.String("SERVICE_NAME", "OTEL_SERVICE_NAME")
	}
	if v, variable, ok := r.find("", []string{"OTEL_RESOURCE_ATTRIBUTES"}); ok {
		attrs, err := parseKeyValueList(v)
		if err != nil {
			return TracingConfig{}, ConfigError(variable + " must be a list of key=value pairs")
		}
		merged := make(map[string]any, len(attrs)+len(c.ResourceAttributes))
		for k, val := range attrs {
			merged[k] = val
		}
		for k, val := range c.ResourceAttributes {
			merged[k] = val
		}
		c.ResourceAttributes = merged
	}
	if c.Sampler == "" {
		if err := samplerFromEnv(r, &c); err != nil {
			return TracingConfig{}, err
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
	return c, nil
}

func samplerFromEnv(r *EnvReader, c *TracingConfig) error {
	v, variable, ok := r.find("", []string{"OTEL_TRACES_SAMPLER"})
	if !ok {
		return nil
	}
	name := Sampler(strings.ToLower(strings.TrimSpace(v)))
	switch name {
	case SamplerAlwaysOn, SamplerAlwaysOff, SamplerTraceIDRatio, SamplerParentBasedAlwaysOn,
		SamplerParentBasedAlwaysOff, SamplerParentBasedTraceIDRatio:
		c.Sampler = name
	case "jaeger_remote", "parentbased_jaeger_remote":
		return nil
	default:
		return ConfigError(variable + " names an unknown sampler")
	}
	if name == SamplerTraceIDRatio || name == SamplerParentBasedTraceIDRatio {
		if arg, argVar, ok := r.find("", []string{"OTEL_TRACES_SAMPLER_ARG"}); ok {
			ratio, err := strconv.ParseFloat(strings.TrimSpace(arg), 64)
			if err != nil || ratio < 0 || ratio > 1 {
				return ConfigError(argVar + " must be a number between 0 and 1")
			}
			c.SamplerRatio = ratio
		}
	}
	return nil
}

// resolveEndpoint picks the exporter endpoint for protocol from the environment or
// the Jaeger default.
func resolveEndpoint(r *EnvReader, protocol Protocol) string {
	specific := "OTLP_HTTP_ENDPOINT"
	if protocol == ProtocolGRPC {
		specific = "OTLP_GRPC_ENDPOINT"
	}
	if v, ok := r.String(specific); ok {
		return v
	}
	if v, ok := r.String("OTLP_ENDPOINT", "OTEL_EXPORTER_OTLP_TRACES_ENDPOINT"); ok {
		return v
	}
	if v, ok := r.String("", "OTEL_EXPORTER_OTLP_ENDPOINT"); ok {
		if protocol == ProtocolGRPC {
			return v
		}
		return strings.TrimSuffix(v, "/") + "/v1/traces"
	}
	if protocol == ProtocolGRPC {
		return DefaultOTLPGRPCEndpoint
	}
	return DefaultOTLPHTTPEndpoint
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
	env     map[string]string
	sampler sdktrace.Sampler
	factory ExporterFactory
}

// WithEnv makes InitTracing read env instead of the process environment.
func WithEnv(env map[string]string) TracingOption {
	return func(o *tracingOptions) { o.env = env }
}

// WithSampler replaces the head sampler named by TracingConfig.Sampler, for example
// with a *RemoteSampler. When TracingConfig.Sampler is a parentbased_* sampler, s is
// wrapped in ParentBased (sampled parents are kept, unsampled parents dropped).
func WithSampler(s sdktrace.Sampler) TracingOption {
	return func(o *tracingOptions) { o.sampler = s }
}

// WithExporterFactory replaces the OTLP exporter (for example with an in-memory
// exporter in tests). The factory receives the resolved config.
func WithExporterFactory(f ExporterFactory) TracingOption {
	return func(o *tracingOptions) { o.factory = f }
}

// InitTracing starts OpenTelemetry tracing that exports to Jaeger over OTLP. It
// resolves cfg with TracingConfigFromEnv (the Jaeger preset: OTLP HTTP/protobuf to
// http://127.0.0.1:4318/v1/traces unless configured otherwise), builds the exporter,
// a batch span processor, the sampler, the resource and the propagators, and installs
// them as the otel globals unless DisableSetGlobal is set. Call Shutdown on the
// handle before exit.
func InitTracing(ctx context.Context, cfg TracingConfig, opts ...TracingOption) (*TracingHandle, error) {
	o := tracingOptions{factory: NewOTLPExporter}
	for _, opt := range opts {
		opt(&o)
	}
	resolved, err := TracingConfigFromEnv(cfg, o.env)
	if err != nil {
		return nil, err
	}
	if o.sampler == nil {
		return InitTracingWith(ctx, resolved, o.factory)
	}
	return initTracingWithSampler(ctx, resolved, o.factory, o.sampler)
}

// initTracingWithSampler is InitTracingWith with a caller-supplied head sampler.
func initTracingWithSampler(ctx context.Context, cfg TracingConfig, factory ExporterFactory, sampler sdktrace.Sampler) (*TracingHandle, error) {
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
	switch cfg.Sampler {
	case SamplerParentBasedAlwaysOn, SamplerParentBasedAlwaysOff, SamplerParentBasedTraceIDRatio:
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
