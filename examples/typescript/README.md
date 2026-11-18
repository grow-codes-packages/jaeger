# TypeScript example

Sends a small trace to Jaeger over OTLP, reads it back through the query API, searches, lists
services/operations/dependencies and fetches the remote sampling strategy.

```sh
docker run --rm -p 16686:16686 -p 4318:4318 -p 5778:5778 jaegertracing/jaeger:2.23.0   # any Jaeger v2
(cd ../../typescript && npm install && npm run build)
npm install
GROW_JAEGER_URL=http://127.0.0.1:16686 \
GROW_JAEGER_OTLP_HTTP_ENDPOINT=http://127.0.0.1:4318 \
GROW_JAEGER_SAMPLING_URL=http://127.0.0.1:5778/sampling npm start
```

Replace the `file:` dependency in `package.json` with a version (`npm install @grow-codes/jaeger`)
to run it against the published package.
