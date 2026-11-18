# Java examples

Install the package into your local Maven repository, point it at a running Jaeger and run an
example:

```bash
make -C ../../java pack            # builds and installs codes.grow.packages:jaeger
export GROW_JAEGER_URL=http://127.0.0.1:16686
export GROW_JAEGER_OTLP_HTTP_ENDPOINT=http://127.0.0.1:4318   # OTLP export (TracingExample)
mvn -q compile exec:java                                                  # BasicExample
mvn -q compile exec:java -Dexec.mainClass=codes.grow.packages.jaeger.examples.AsyncExample
mvn -q compile exec:java -Dexec.mainClass=codes.grow.packages.jaeger.examples.TracingExample
```

Against the published package, set `-Dsdk.version=<version>` (or edit `pom.xml`) instead of
installing it locally.

| Class | Shows |
| - | - |
| `BasicExample` | blocking client: health, ping, server version, error handling |
| `AsyncExample` | `…Async` methods, combining futures, cancellation |
| `HooksAndLoggingExample` | request hooks, a logger, retries and timeouts |
| `TracingExample` | OTLP export, `waitForTrace`, `findTraces`, `searchTraces`, services, operations, dependencies, sampling strategy |
