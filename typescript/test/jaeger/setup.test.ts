import { execFileSync } from "node:child_process";
import { mkdtempSync, readFileSync, rmSync } from "node:fs";
import { createServer, type Server } from "node:https";
import type { AddressInfo } from "node:net";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { OTLPTraceExporter as GrpcExporter } from "@opentelemetry/exporter-trace-otlp-grpc";
import { OTLPTraceExporter as HttpExporter } from "@opentelemetry/exporter-trace-otlp-proto";
import { afterAll, afterEach, beforeAll, describe, expect, it } from "vitest";
import {
  ConfigError,
  JAEGER_OTLP_GRPC_ENDPOINT,
  JAEGER_OTLP_HTTP_ENDPOINT,
  JaegerClient,
  JaegerRemoteSampler,
  SamplingPreset,
  initTracing,
  jaegerExporterFactory,
  resolveJaegerEndpoint,
  resolveTracingConfig,
  tracingConfigFromEnv,
  type TracingConfig,
} from "../../src/index.js";
import { jaegerServer } from "../helpers/jaeger.js";
import { type MockServer, startServer } from "../helpers/server.js";

const servers: MockServer[] = [];
afterEach(async () => {
  await Promise.all(servers.splice(0).map((s) => s.close()));
});

async function otlpReceiver(): Promise<MockServer> {
  const s = await startServer(() => ({
    status: 200,
    headers: { "content-type": "application/x-protobuf" },
    body: "",
  }));
  servers.push(s);
  return s;
}

describe("resolveJaegerEndpoint", () => {
  it("applies Jaeger defaults and appends /v1/traces to HTTP bases", () => {
    expect(resolveJaegerEndpoint("http/protobuf")).toBe(JAEGER_OTLP_HTTP_ENDPOINT);
    expect(resolveJaegerEndpoint("grpc", "")).toBe(JAEGER_OTLP_GRPC_ENDPOINT);
    expect(resolveJaegerEndpoint("http/protobuf", "http://jaeger:4318")).toBe(
      "http://jaeger:4318/v1/traces",
    );
    expect(resolveJaegerEndpoint("http/protobuf", "http://jaeger:4318/")).toBe(
      "http://jaeger:4318/v1/traces",
    );
    expect(resolveJaegerEndpoint("http/protobuf", "http://gw/otlp/v1/traces")).toBe(
      "http://gw/otlp/v1/traces",
    );
    expect(resolveJaegerEndpoint("grpc", "http://jaeger:4317")).toBe("http://jaeger:4317");
    expect(resolveJaegerEndpoint("http/protobuf", "not a url")).toBe("not a url");
  });
});

describe("tracingConfigFromEnv", () => {
  it("GROW_JAEGER_OTLP_* > OTEL_EXPORTER_OTLP_TRACES_* > OTEL_EXPORTER_OTLP_* > default", () => {
    const env = {
      GROW_JAEGER_OTLP_HEADERS: "authorization=Bearer%20grow",
      OTEL_EXPORTER_OTLP_TRACES_HEADERS: "x-a=traces",
      OTEL_EXPORTER_OTLP_HEADERS: "x-a=generic",
      OTEL_EXPORTER_OTLP_TRACES_COMPRESSION: "none",
      OTEL_EXPORTER_OTLP_COMPRESSION: "gzip",
      OTEL_EXPORTER_OTLP_TIMEOUT: "2500",
      OTEL_EXPORTER_OTLP_TRACES_ENDPOINT: "http://traces:4318/custom",
      OTEL_EXPORTER_OTLP_ENDPOINT: "http://base:4318",
      OTEL_SERVICE_NAME: "svc-env",
      OTEL_RESOURCE_ATTRIBUTES: "team=core,service.name=ignored",
      OTEL_PROPAGATORS: "tracecontext, b3 ,none,",
    };
    const c = tracingConfigFromEnv({}, env);
    expect(c).toMatchObject({
      serviceName: "svc-env",
      headers: { authorization: "Bearer grow" },
      compression: "none",
      batch: { exportTimeoutMs: 2500 },
      endpoint: "http://traces:4318/custom",
      resourceAttributes: { team: "core" },
      propagators: ["tracecontext", "b3"],
    });
    expect(c.protocol).toBeUndefined();
    expect(
      tracingConfigFromEnv({}, { OTEL_EXPORTER_OTLP_ENDPOINT: "http://base:4318/" }).endpoint,
    ).toBe("http://base:4318/v1/traces");
    expect(
      tracingConfigFromEnv(
        { protocol: "grpc" },
        { OTEL_EXPORTER_OTLP_ENDPOINT: "http://base:4317" },
      ).endpoint,
    ).toBe("http://base:4317");
    expect(
      tracingConfigFromEnv({}, { OTEL_RESOURCE_ATTRIBUTES: "service.name=from-res" }).serviceName,
    ).toBe("from-res");
    expect(tracingConfigFromEnv({}, {}).serviceName).toBe("");
  });

  it("protocol-specific GROW endpoints win and HTTP bases get /v1/traces", () => {
    const env = {
      GROW_JAEGER_OTLP_GRPC_ENDPOINT: "http://jaeger:4317",
      GROW_JAEGER_OTLP_HTTP_ENDPOINT: "http://jaeger:4318",
      GROW_JAEGER_OTLP_ENDPOINT: "http://generic:4318",
      OTEL_EXPORTER_OTLP_TRACES_ENDPOINT: "http://other:4318/v1/traces",
      OTEL_EXPORTER_OTLP_PROTOCOL: "grpc",
    };
    expect(tracingConfigFromEnv({}, env)).toMatchObject({
      protocol: "grpc",
      endpoint: "http://jaeger:4317",
    });
    expect(tracingConfigFromEnv({ protocol: "http/protobuf" }, env)).toMatchObject({
      protocol: "http/protobuf",
      endpoint: "http://jaeger:4318/v1/traces",
    });
    expect(
      tracingConfigFromEnv({}, { GROW_JAEGER_OTLP_ENDPOINT: "http://generic:4318" }).endpoint,
    ).toBe("http://generic:4318/v1/traces");
    expect(tracingConfigFromEnv({ endpoint: "http://explicit:4318/v1/traces" }, env).endpoint).toBe(
      "http://explicit:4318/v1/traces",
    );
  });

  it("explicit values win field by field (batch and tls merged)", () => {
    const env = {
      OTEL_EXPORTER_OTLP_TIMEOUT: "10s",
      GROW_JAEGER_OTLP_CERTIFICATE: "/ca.pem",
      OTEL_EXPORTER_OTLP_CLIENT_CERTIFICATE: "/c.pem",
      OTEL_EXPORTER_OTLP_TRACES_CLIENT_KEY: "/k.pem",
      OTEL_SERVICE_NAME: "env",
    };
    const c = tracingConfigFromEnv(
      {
        serviceName: "explicit",
        batch: { scheduledDelayMs: 10 },
        tls: { insecureSkipVerify: true },
      },
      env,
    );
    expect(c.serviceName).toBe("explicit");
    expect(c.batch).toEqual({ exportTimeoutMs: 10_000, scheduledDelayMs: 10 });
    expect(c.tls).toEqual({
      caCertPath: "/ca.pem",
      clientCertPath: "/c.pem",
      clientKeyPath: "/k.pem",
      insecureSkipVerify: true,
    });
    expect(tracingConfigFromEnv({ batch: { maxQueueSize: 4 } }, {}).batch).toEqual({
      maxQueueSize: 4,
    });
  });

  it("parity: GROW_JAEGER_SERVICE_NAME, GROW_JAEGER_OTLP_CA_CERT alias, resource merge", () => {
    const env = {
      GROW_JAEGER_SERVICE_NAME: "grow-name",
      OTEL_SERVICE_NAME: "otel-name",
      GROW_JAEGER_OTLP_CA_CERT: "/alias.pem",
      OTEL_EXPORTER_OTLP_CERTIFICATE: "/otel.pem",
      OTEL_RESOURCE_ATTRIBUTES: "service.name=res,team=env,region=eu",
    };
    const c = tracingConfigFromEnv({ resourceAttributes: { team: "explicit" } }, env);
    expect(c.serviceName).toBe("grow-name");
    expect(c.tls?.caCertPath).toBe("/alias.pem");
    expect(c.resourceAttributes).toEqual({ team: "explicit", region: "eu" });
    expect(
      tracingConfigFromEnv(
        {},
        { GROW_JAEGER_OTLP_CERTIFICATE: "/grow.pem", GROW_JAEGER_OTLP_CA_CERT: "/alias.pem" },
      ).tls?.caCertPath,
    ).toBe("/grow.pem");
    expect(tracingConfigFromEnv({}, { OTEL_SERVICE_NAME: "otel-name" }).serviceName).toBe(
      "otel-name",
    );
    expect(
      tracingConfigFromEnv({}, { OTEL_RESOURCE_ATTRIBUTES: "service.name=res" }).serviceName,
    ).toBe("res");
  });

  it("SamplingPreset: development, production, followParent, off, ratio(r)", () => {
    expect(SamplingPreset.development).toEqual({ sampler: "always_on", samplerRatio: 1 });
    expect(SamplingPreset.production).toEqual({
      sampler: "parentbased_traceidratio",
      samplerRatio: 0.1,
    });
    expect(SamplingPreset.followParent).toEqual({
      sampler: "parentbased_always_on",
      samplerRatio: 1,
    });
    expect(SamplingPreset.off).toEqual({ sampler: "always_off", samplerRatio: 1 });
    expect(SamplingPreset.ratio(0.25)).toEqual({
      sampler: "parentbased_traceidratio",
      samplerRatio: 0.25,
    });
    expect(SamplingPreset.ratio(0)).toEqual({ sampler: "parentbased_always_off", samplerRatio: 1 });
    expect(SamplingPreset.ratio(2)).toBe(SamplingPreset.followParent);
    expect(() => SamplingPreset.ratio(Number.NaN)).toThrow(ConfigError);
    expect(resolveTracingConfig({ serviceName: "s", ...SamplingPreset.production })).toMatchObject({
      sampler: "parentbased_traceidratio",
      samplerRatio: 0.1,
    });
  });

  it("samplers: OTEL_TRACES_SAMPLER(+_ARG) and jaeger_remote", () => {
    expect(
      tracingConfigFromEnv(
        {},
        { OTEL_TRACES_SAMPLER: "parentbased_traceidratio", OTEL_TRACES_SAMPLER_ARG: "0.25" },
      ),
    ).toMatchObject({ sampler: "parentbased_traceidratio", samplerRatio: 0.25 });
    expect(
      tracingConfigFromEnv({}, { OTEL_TRACES_SAMPLER: "always_off", OTEL_TRACES_SAMPLER_ARG: "x" }),
    ).toMatchObject({
      sampler: "always_off",
    });
    expect(
      tracingConfigFromEnv(
        {},
        {
          OTEL_TRACES_SAMPLER: "parentbased_jaeger_remote",
          OTEL_TRACES_SAMPLER_ARG:
            "endpoint=http://s:5778/sampling,pollingIntervalMs=5000,initialSamplingRate=0.5",
        },
      ).remoteSampler,
    ).toEqual({
      samplingUrl: "http://s:5778/sampling",
      pollIntervalMs: 5000,
      initialSamplingRate: 0.5,
      parentBased: true,
    });
    expect(
      tracingConfigFromEnv({}, { OTEL_TRACES_SAMPLER: "jaeger_remote" }).remoteSampler,
    ).toEqual({
      parentBased: false,
    });
    const explicit = tracingConfigFromEnv(
      { sampler: "always_on" },
      { OTEL_TRACES_SAMPLER: "jaeger_remote" },
    );
    expect(explicit.remoteSampler).toBeUndefined();
    expect(explicit.sampler).toBe("always_on");
  });

  it("invalid values throw ConfigError naming the variable, never the value", () => {
    const cases: Record<string, string>[] = [
      { GROW_JAEGER_OTLP_PROTOCOL: "http/json" },
      { OTEL_EXPORTER_OTLP_HEADERS: "noequals-secretvalue" },
      { OTEL_EXPORTER_OTLP_HEADERS: "a=%E0%A4%A-secretvalue" },
      { OTEL_EXPORTER_OTLP_COMPRESSION: "brotli-secretvalue" },
      { OTEL_EXPORTER_OTLP_TIMEOUT: "soon-secretvalue" },
      { OTEL_EXPORTER_OTLP_TIMEOUT: "0" },
      { OTEL_TRACES_SAMPLER: "sometimes-secretvalue" },
      { OTEL_TRACES_SAMPLER: "traceidratio", OTEL_TRACES_SAMPLER_ARG: "2-secretvalue" },
      {
        OTEL_TRACES_SAMPLER: "jaeger_remote",
        OTEL_TRACES_SAMPLER_ARG: "pollingIntervalMs=x-secretvalue",
      },
      {
        OTEL_TRACES_SAMPLER: "jaeger_remote",
        OTEL_TRACES_SAMPLER_ARG: "initialSamplingRate=x-secretvalue",
      },
    ];
    for (const env of cases) {
      let err: unknown;
      try {
        tracingConfigFromEnv({}, env);
      } catch (e) {
        err = e;
      }
      expect(err, JSON.stringify(env)).toBeInstanceOf(ConfigError);
      expect((err as Error).message).not.toContain("secretvalue");
      expect((err as Error).message).toMatch(/OTEL_|GROW_/);
    }
  });
});

describe("jaegerExporterFactory", () => {
  const resolve = (c: Partial<TracingConfig>) => resolveTracingConfig({ serviceName: "s", ...c });

  it("builds the HTTP and gRPC OTLP exporters", async () => {
    const http = jaegerExporterFactory(resolve({ compression: "none", headers: { "x-k": "v" } }));
    expect(http).toBeInstanceOf(HttpExporter);
    await http.shutdown();
    const grpc = jaegerExporterFactory(resolve({ protocol: "grpc", headers: { "X-Tenant": "t" } }));
    expect(grpc).toBeInstanceOf(GrpcExporter);
    await grpc.shutdown();
    const grpcTls = jaegerExporterFactory(
      resolve({
        protocol: "grpc",
        endpoint: "https://jaeger:4317",
        tls: { insecureSkipVerify: true },
      }),
    );
    expect(grpcTls).toBeInstanceOf(GrpcExporter);
    await grpcTls.shutdown();
    expect(() => jaegerExporterFactory(resolve({ protocol: "native" }))).toThrow(ConfigError);
    expect(() =>
      jaegerExporterFactory(resolve({ tls: { caCertPath: "/does/not/exist-secret.pem" } })),
    ).toThrow(/cannot read a TLS file/);
  });
});

describe("initTracing", () => {
  it("exports spans over OTLP/HTTP protobuf with headers and gzip", async () => {
    const receiver = await otlpReceiver();
    const handle = initTracing(
      {
        serviceName: "svc-http",
        endpoint: receiver.url,
        headers: { "x-api-key": "k-123" },
        setGlobal: false,
      },
      { env: false },
    );
    expect(handle.endpoint).toBe(`${receiver.url}/v1/traces`);
    expect(handle.remoteSampler).toBeUndefined();
    handle.tracer("t").startSpan("op").end();
    await handle.forceFlush(5000);
    await handle.shutdown();
    expect(receiver.requests).toHaveLength(1);
    const req = receiver.requests[0]!;
    expect(req.method).toBe("POST");
    expect(req.url).toBe("/v1/traces");
    expect(req.headers["content-type"]).toBe("application/x-protobuf");
    expect(req.headers["content-encoding"]).toBe("gzip");
    expect(req.headers["x-api-key"]).toBe("k-123");
  });

  it("reads the environment beneath the explicit config", async () => {
    const receiver = await otlpReceiver();
    const handle = initTracing(
      { serviceName: "svc-env", setGlobal: false },
      {
        env: {
          GROW_JAEGER_OTLP_HTTP_ENDPOINT: receiver.url,
          OTEL_EXPORTER_OTLP_COMPRESSION: "none",
        },
      },
    );
    handle.tracer("t").startSpan("op").end();
    await handle.shutdown();
    expect(receiver.requests[0]!.headers["content-encoding"]).toBeUndefined();
    const fromProcess = initTracing({
      serviceName: "svc-proc",
      setGlobal: false,
      protocol: "grpc",
    });
    expect(fromProcess.endpoint).toMatch(/^http/);
    await fromProcess.shutdown();
  });

  it("remote sampler: owned sampler decides and is closed by shutdown()", async () => {
    const receiver = await otlpReceiver();
    const sampling = await jaegerServer({
      "/sampling": {
        status: 200,
        body: {
          strategyType: "PROBABILISTIC",
          probabilisticSampling: { samplingRate: 1 },
          operationSampling: {
            perOperationStrategies: [{ operation: "drop-me", probabilisticSampling: {} }],
          },
        },
      },
    });
    servers.push(sampling);
    const handle = initTracing(
      {
        serviceName: "svc-remote",
        endpoint: receiver.url,
        setGlobal: false,
        remoteSampler: { samplingUrl: `${sampling.url}/sampling`, initialSamplingRate: 0 },
      },
      { env: false },
    );
    const remote = handle.remoteSampler!;
    expect(remote).toBeInstanceOf(JaegerRemoteSampler);
    expect(remote.serviceName).toBe("svc-remote");
    await remote.ready();
    const tracer = handle.tracer("t");
    const kept = tracer.startSpan("keep-me");
    const dropped = tracer.startSpan("drop-me");
    expect(kept.isRecording()).toBe(true);
    expect(dropped.isRecording()).toBe(false);
    kept.end();
    dropped.end();
    await handle.shutdown();
    await handle.shutdown();
    expect(receiver.requests).toHaveLength(1);
    await expect(remote.refresh()).rejects.toMatchObject({ code: "CONFIG" });
  });

  it("remote sampler: an existing sampler is not closed; parentBased false; true uses the env sampling config", async () => {
    const sampler = new JaegerRemoteSampler({
      serviceName: "x",
      autoStart: false,
      initialSamplingRate: 0,
    });
    const handle = initTracing(
      { serviceName: "svc", setGlobal: false, remoteSampler: sampler },
      { env: false },
    );
    expect(handle.tracer("t").startSpan("a").isRecording()).toBe(false);
    await handle.shutdown();
    expect(sampler.strategy).toBeNull();
    await sampler.close();

    const raw = initTracing(
      {
        serviceName: "svc",
        setGlobal: false,
        remoteSampler: { parentBased: false, autoStart: false, initialSamplingRate: 1 },
      },
      { env: false },
    );
    expect(raw.tracer("t").startSpan("a").isRecording()).toBe(true);
    await raw.shutdown();

    const defaults = initTracing(
      { serviceName: "svc", setGlobal: false, remoteSampler: true },
      {
        env: {
          OTEL_TRACES_SAMPLER: "always_on",
          GROW_JAEGER_SAMPLING_URL: "http://127.0.0.1:1/sampling",
        },
      },
    );
    expect(defaults.remoteSampler?.samplingUrl).toBe("http://127.0.0.1:1/sampling");
    await defaults.shutdown();
  });

  it("remote sampler errors shut the provider down", () => {
    expect(() =>
      initTracing(
        { serviceName: "svc", setGlobal: false, remoteSampler: { pollIntervalMs: -1 } },
        { env: false },
      ),
    ).toThrow(ConfigError);
  });

  it("client.initTracing handles are shut down by client.close()", async () => {
    const receiver = await otlpReceiver();
    const client = new JaegerClient();
    const handle = client.initTracing(
      { serviceName: "svc-client", endpoint: receiver.url, setGlobal: false },
      { env: false },
    );
    handle.tracer("t").startSpan("op").end();
    await client.close();
    expect(receiver.requests).toHaveLength(1);
    expect(() => client.initTracing({ serviceName: "x" })).toThrow(ConfigError);
  });

  it("client.initTracing: a remote sampler uses the client's sampling config", async () => {
    const client = new JaegerClient({ samplingUrl: "http://127.0.0.1:1/client-sampling" });
    const handle = client.initTracing(
      {
        serviceName: "svc-remote",
        setGlobal: false,
        remoteSampler: { autoStart: false },
      },
      { env: false },
    );
    expect(handle.remoteSampler?.samplingUrl).toBe("http://127.0.0.1:1/client-sampling");
    await client.close();
  });
});

// TLS: export to an HTTPS receiver with a private CA.
function hasOpenssl(): boolean {
  try {
    execFileSync("openssl", ["version"], { stdio: "ignore" });
    return true;
  } catch {
    return false;
  }
}

describe.runIf(hasOpenssl())("TLS exporters", () => {
  const dir = mkdtempSync(join(tmpdir(), "jaeger-tls-"));
  let server: Server | undefined;
  let url = "";
  const seen: string[] = [];

  beforeAll(async () => {
    execFileSync(
      "openssl",
      [
        "req",
        "-x509",
        "-newkey",
        "rsa:2048",
        "-nodes",
        "-days",
        "1",
        "-subj",
        "/CN=localhost",
        "-addext",
        "subjectAltName=IP:127.0.0.1,DNS:localhost",
        "-keyout",
        join(dir, "key.pem"),
        "-out",
        join(dir, "cert.pem"),
      ],
      { stdio: "ignore" },
    );
    server = createServer(
      { key: readFileSync(join(dir, "key.pem")), cert: readFileSync(join(dir, "cert.pem")) },
      (req, res) => {
        seen.push(req.url ?? "");
        req.resume();
        req.on("end", () => {
          res.writeHead(200, { "content-type": "application/x-protobuf" });
          res.end();
        });
      },
    );
    await new Promise<void>((r) => server?.listen(0, "127.0.0.1", r));
    url = `https://127.0.0.1:${(server!.address() as AddressInfo).port}`;
  });

  afterAll(async () => {
    await new Promise<void>((r) => server?.close(() => r()));
    rmSync(dir, { recursive: true, force: true });
  });

  it("HTTP exporter trusts the configured CA and presents a client cert", async () => {
    const handle = initTracing(
      {
        serviceName: "svc-tls",
        endpoint: url,
        setGlobal: false,
        tls: {
          caCertPath: join(dir, "cert.pem"),
          clientCertPath: join(dir, "cert.pem"),
          clientKeyPath: join(dir, "key.pem"),
        },
      },
      { env: false },
    );
    handle.tracer("t").startSpan("op").end();
    await handle.shutdown();
    expect(seen).toContain("/v1/traces");
  });

  it("insecureSkipVerify and caCertPem are accepted; gRPC builds TLS credentials", async () => {
    const insecure = initTracing(
      {
        serviceName: "svc-tls2",
        endpoint: url,
        setGlobal: false,
        tls: { insecureSkipVerify: true },
      },
      { env: false },
    );
    insecure.tracer("t").startSpan("op").end();
    await insecure.shutdown();
    const pem = readFileSync(join(dir, "cert.pem"), "utf8");
    const grpc = jaegerExporterFactory(
      resolveTracingConfig({
        serviceName: "s",
        protocol: "grpc",
        tls: {
          caCertPem: pem,
          clientCertPath: join(dir, "cert.pem"),
          clientKeyPath: join(dir, "key.pem"),
        },
      }),
    );
    await grpc.shutdown();
    expect(seen.length).toBeGreaterThanOrEqual(2);
  });
});
