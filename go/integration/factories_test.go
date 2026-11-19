package integration

import (
	"context"
	"strings"
	"testing"
	"time"

	jaeger "github.com/grow-codes-packages/jaeger/go"
	"github.com/grow-codes-packages/jaeger/go/testkit"
)

// capabilities the deployment under test offers (see conformance "requires").
// Jaeger serves every tracing capability: query (v3 API), services and dependencies.
var capabilities = []string{"version", "query", "services", "dependencies"}

// categories runs "core" plus this service's categories.
func categories() []string {
	return append([]string{"core"}, strings.Split("tracing", ",")...)
}

// factories builds the conformance targets: the core client and, for tracing, an
// OTLP export to Jaeger (the Jaeger preset from the environment) plus the query client.
func factories(t *testing.T) testkit.Factories {
	t.Helper()
	return testkit.Factories{
		"core": testkit.CoreFactory(func(context.Context) (testkit.CoreTarget, error) {
			return jaeger.NewClientFromEnv()
		}),
		"tracing": testkit.TracingFactory(func(ctx context.Context, serviceName string) (testkit.TracingTarget, error) {
			client, err := jaeger.NewClientFromEnv()
			if err != nil {
				return testkit.TracingTarget{}, err
			}
			t.Cleanup(func() { _ = client.Close() })
			h, err := jaeger.InitTracing(ctx, jaeger.TracingConfig{
				ServiceName:      serviceName,
				DisableSetGlobal: true,
				Batch:            jaeger.TracingBatchOptions{ScheduledDelay: 100 * time.Millisecond},
			})
			if err != nil {
				return testkit.TracingTarget{}, err
			}
			return testkit.TracingTarget{Handle: h.TracingHandle, Query: client.Query()}, nil
		}),
	}
}
