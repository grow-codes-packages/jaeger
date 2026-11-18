// Package jaeger is the Grow Codes SDK for Jaeger: OpenTelemetry tracing that
// exports to Jaeger over OTLP, a client for the Jaeger query API and Jaeger remote
// sampling.
//
// Tracing (OTLP HTTP/protobuf to port 4318 by default, gRPC to 4317 selectable;
// endpoint and protocol also come from GROW_JAEGER_OTLP_* and OTEL_EXPORTER_OTLP_*):
//
//	h, err := jaeger.InitTracing(ctx, jaeger.TracingConfig{ServiceName: "orders"})
//	if err != nil {
//		return err
//	}
//	defer h.Shutdown(context.Background())
//	err = jaeger.Traced(ctx, "checkout", func(ctx context.Context) error { … })
//
// Query and health (GROW_JAEGER_URL, default http://127.0.0.1:16686):
//
//	client, err := jaeger.NewClientFromEnv()
//	if err != nil {
//		return err
//	}
//	defer client.Close()
//	trace, err := client.Query().GetTrace(ctx, traceID)
//	summaries, err := client.Query().SearchTraces(ctx, jaeger.TraceSearchQuery{Service: "orders"})
//
// Remote sampling (GROW_JAEGER_SAMPLING_URL, default http://127.0.0.1:5778/sampling):
//
//	sampler, err := client.NewRemoteSampler("orders", time.Minute)
//	h, err := jaeger.InitTracing(ctx, cfg, jaeger.WithSampler(sampler))
//
// Every error is an *Error; compare codes with errors.Is against the Err*
// sentinels (errors.Is(err, jaeger.ErrNotFound)). Credentials are wrapped in
// Secret and never appear in errors, hooks or logs.
//
// Jaeger is a trademark of The Linux Foundation (Jaeger is a CNCF project). This package is not affiliated
// with or endorsed by The Linux Foundation (Jaeger is a CNCF project).
package jaeger
