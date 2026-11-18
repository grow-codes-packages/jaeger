#!/bin/sh
# Jaeger fixture bootstrap (GRO-1378). POSIX sh, idempotent: every run re-checks readiness,
# sends a fresh canary span and rewrites /bootstrap/env atomically. Jaeger needs no users,
# tokens or schema in this setup; the canary proves the OTLP -> storage -> query path works
# before any SDK test starts (healthcheck alone does not prove ingest).
set -eu

J="${JAEGER_HOST:-jaeger}"
JS="${JAEGER_SAMPLING_HOST:-jaeger-sampling}"
OUT="${BOOTSTRAP_DIR:-/bootstrap}"
mkdir -p "$OUT"

log() { echo "bootstrap: $*" >&2; }

wait_for() { # url, tries
  i=0
  until curl -fsS -o /dev/null "$1"; do
    i=$((i + 1))
    [ "$i" -ge "${2:-60}" ] && { log "timeout waiting for $1"; exit 1; }
    sleep 0.5
  done
}

for h in "$J" "$JS"; do
  wait_for "http://$h:13133/status"
  wait_for "http://$h:16686/api/v3/services"
  wait_for "http://$h:5778/sampling?service=bootstrap-probe"
done

# Canary: one span, random 128-bit trace id, service "grow-bootstrap", sent as OTLP/HTTP JSON.
hex() { od -An -N"$1" -tx1 /dev/urandom | tr -d ' \n'; }
TID="$(hex 16)"
SID="$(hex 8)"
NOW_S="$(date +%s)"
START_NS="${NOW_S}000000000"
END_NS="$((NOW_S + 1))000000000"
BODY=$(printf '{"resourceSpans":[{"resource":{"attributes":[{"key":"service.name","value":{"stringValue":"grow-bootstrap"}}]},"scopeSpans":[{"scope":{"name":"grow-bootstrap"},"spans":[{"traceId":"%s","spanId":"%s","name":"bootstrap-canary","kind":1,"startTimeUnixNano":"%s","endTimeUnixNano":"%s"}]}]}]}' "$TID" "$SID" "$START_NS" "$END_NS")

curl -fsS -o /dev/null -X POST -H 'Content-Type: application/json' --data "$BODY" "http://$J:4318/v1/traces"

i=0
until curl -fsS -o /dev/null "http://$J:16686/api/v3/traces/$TID"; do
  i=$((i + 1))
  [ "$i" -ge 100 ] && { log "canary trace $TID not queryable after ~20 s"; exit 1; }
  sleep 0.2
done
log "canary trace queryable after ~$((i * 200)) ms (poll count $i)"

TMP="$OUT/env.tmp.$$"
cat > "$TMP" <<ENV
# Written by the jaeger fixture bootstrap. No secrets: Jaeger runs without auth here.
GROW_JAEGER_URL=http://$J:16686
JAEGER_QUERY_URL=http://$J:16686
GROW_JAEGER_QUERY_GRPC_ENDPOINT=$J:16685
GROW_JAEGER_OTLP_GRPC_ENDPOINT=http://$J:4317
GROW_JAEGER_OTLP_HTTP_ENDPOINT=http://$J:4318
OTEL_EXPORTER_OTLP_ENDPOINT=http://$J:4318
OTEL_EXPORTER_OTLP_TRACES_ENDPOINT=http://$J:4318/v1/traces
GROW_JAEGER_SAMPLING_URL=http://$J:5778/sampling
GROW_JAEGER_HEALTH_URL=http://$J:13133/status
GROW_JAEGER_METRICS_URL=http://$J:8888/metrics
GROW_JAEGER_SAMPLING_FIXTURE_URL=http://$JS:5778/sampling
GROW_JAEGER_SAMPLING_FIXTURE_QUERY_URL=http://$JS:16686
GROW_JAEGER_SAMPLING_FIXTURE_OTLP_HTTP_ENDPOINT=http://$JS:4318
GROW_JAEGER_SAMPLING_STRATEGIES_FILE=/sampling/live/strategies.json
GROW_JAEGER_SAMPLING_RELOAD_INTERVAL_MS=1000
GROW_JAEGER_BOOTSTRAP_TRACE_ID=$TID
GROW_JAEGER_BOOTSTRAP_SERVICE=grow-bootstrap
ENV
chmod 0644 "$TMP"
mv "$TMP" "$OUT/env"
log "wrote $OUT/env"
