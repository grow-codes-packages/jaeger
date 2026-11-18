# TypeScript integration tests

A consumer project: it installs the packed `@grow-codes/jaeger` tarball (never the source tree)
and runs the integration and conformance tests with `node --test` against a real **DISPLAY**.

```sh
cd typescript && make pack            # writes dist/grow-codes-jaeger-<version>.tgz
cd integration && npm run install-package
GROW_JAEGER_URL=http://127.0.0.1:<port> GROW_JAEGER_TOKEN=... npm test
```

In CI and locally with Docker this runs inside `docker/Dockerfile.typescript`
(`make test-docker LANGS=typescript` from the repository root), which reads `/bootstrap/env`.
