# TypeScript integration tests

A consumer project: it installs the packed `@grow-codes/jaeger` tarball (never the source tree)
and runs the integration and conformance tests with `node --test` against a real **Jaeger 2.23.0**
all-in-one (plus the `jaeger-sampling` instance with a strategies file).

```sh
cd typescript && make pack            # writes dist/grow-codes-jaeger-<version>.tgz
cd integration && npm run install-package
GROW_BOOTSTRAP_ENV=/path/to/env npm test   # KEY=value file as written by docker/bootstrap
```

In CI and locally with Docker this runs inside `docker/Dockerfile.typescript`
(`make test-docker LANGS=typescript` from the repository root), which reads `/bootstrap/env`
(`GROW_JAEGER_URL`, `GROW_JAEGER_OTLP_*_ENDPOINT`, `GROW_JAEGER_SAMPLING_URL`,
`GROW_JAEGER_SAMPLING_FIXTURE_URL`, `GROW_JAEGER_HEALTH_URL`, `GROW_JAEGER_METRICS_URL`, …).
