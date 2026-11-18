// Package jaeger is the Grow Codes SDK for Jaeger.
//
// Create a client from the environment (GROW_JAEGER_URL,
// GROW_JAEGER_TOKEN, ...) or from a Config literal:
//
//	client, err := jaeger.NewClientFromEnv()
//	if err != nil {
//		return err
//	}
//	defer client.Close()
//	status, err := client.Health(ctx)
//
// Every error is an *Error; compare codes with errors.Is against the Err*
// sentinels (errors.Is(err, jaeger.ErrNotFound)). Credentials are wrapped in
// Secret and never appear in errors, hooks or logs.
//
// Jaeger is a trademark of The Linux Foundation (Jaeger is a CNCF project). This package is not affiliated
// with or endorsed by The Linux Foundation (Jaeger is a CNCF project).
package jaeger
