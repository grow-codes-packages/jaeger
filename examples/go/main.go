// Command example shows the Jaeger Go SDK end to end: configuration from the
// environment, health and version, OpenTelemetry tracing exported to Jaeger over
// OTLP, reading the trace back through the query API, and remote sampling.
//
//	GROW_JAEGER_URL=http://127.0.0.1:16686 \
//	GROW_JAEGER_OTLP_HTTP_ENDPOINT=http://127.0.0.1:4318 \
//	GROW_JAEGER_SAMPLING_URL=http://127.0.0.1:5778/sampling go run .
package main

import (
	"context"
	"errors"
	"fmt"
	"log/slog"
	"os"
	"time"

	"go.opentelemetry.io/otel/attribute"
	"go.opentelemetry.io/otel/trace"

	jaeger "github.com/grow-codes-packages/jaeger/go"
)

func main() {
	if err := run(); err != nil {
		fmt.Fprintln(os.Stderr, "error:", err)
		os.Exit(1)
	}
}

func run() error {
	ctx, cancel := context.WithTimeout(context.Background(), time.Minute)
	defer cancel()

	cfg, err := jaeger.ConfigFromEnv()
	if err != nil {
		return err // CONFIG error naming the variable, never its value
	}
	cfg.Logger = slog.New(slog.NewTextHandler(os.Stderr, &slog.HandlerOptions{Level: slog.LevelWarn}))
	fmt.Println("config:", cfg) // credentials print as [REDACTED]

	client, err := jaeger.NewClient(cfg)
	if err != nil {
		return err
	}
	defer func() { _ = client.Close() }()

	health, err := client.Health(ctx)
	if err != nil {
		return err
	}
	fmt.Printf("health: healthy=%t status=%s\n", health.Healthy, health.Status)
	version, err := client.ServerVersion(ctx)
	switch {
	case errors.Is(err, jaeger.ErrUnsupported):
		fmt.Println("server version: not exposed")
	case err != nil:
		return err
	default:
		fmt.Println("server version:", version)
	}

	// Tracing: OTLP HTTP/protobuf to Jaeger (GROW_JAEGER_OTLP_* / OTEL_EXPORTER_OTLP_*).
	service := fmt.Sprintf("example-go-%d", time.Now().Unix())
	tracing, err := jaeger.InitTracing(ctx, jaeger.SamplingDevelopment.Apply(jaeger.TracingConfig{ServiceName: service}))
	if err != nil {
		return err
	}
	defer func() { _ = tracing.Shutdown(context.Background()) }()

	var traceID string
	err = jaeger.Traced(ctx, "checkout", func(ctx context.Context) error {
		traceID, _ = jaeger.CurrentTraceID(ctx)
		jaeger.CurrentSpan(ctx).SetAttributes(attribute.String("order.id", "42"))
		_, err := jaeger.WithSpan(ctx, "charge card", func(ctx context.Context) (string, error) {
			jaeger.AddEvent(ctx, "card.authorized", map[string]any{"amount": 1999})
			return "ok", nil
		}, jaeger.WithSpanKind(trace.SpanKindClient))
		return err
	})
	if err != nil {
		return err
	}
	if err := tracing.ForceFlush(ctx); err != nil {
		return err
	}

	// Query: wait until Jaeger has stored both spans, then search.
	q := client.Query()
	tr, err := q.WaitForTrace(ctx, traceID, jaeger.WaitForTraceOptions{MinSpans: 2, Interval: 250 * time.Millisecond})
	if err != nil {
		return err
	}
	for _, s := range tr.Spans {
		fmt.Printf("span %s kind=%s parent=%q duration=%dµs\n", s.Name, s.Kind, s.ParentSpanID, s.DurationMicros)
	}
	summaries, err := q.SearchTraces(ctx, jaeger.TraceSearchQuery{Service: service, Tags: map[string]string{"order.id": "42"}})
	if err != nil {
		return err
	}
	fmt.Println("matching traces:", len(summaries))
	operations, err := q.ListOperations(ctx, service)
	if err != nil {
		return err
	}
	fmt.Println("operations:", operations)

	// Remote sampling: the strategy Jaeger hands out for this service.
	strategy, err := client.SamplingStrategy(ctx, service)
	if err != nil {
		return err
	}
	fmt.Printf("sampling: %s rate=%g\n", strategy.StrategyType, strategy.SamplingRate)
	return nil
}
