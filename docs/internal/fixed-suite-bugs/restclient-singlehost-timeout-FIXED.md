# ES failure family - RestClient single-host async timeout

Status: OPEN

Signal:
- `java.lang.AssertionError: timeout waiting for requests to be sent`

Full rerun count:
- Run: `es-nonpassed-rerun-20260708-191002`
- HTTP connection/timeout FAIL rows: 2 of 1064 total FAIL rows.
- Rows: `RestClientGzipCompressionTests` with `ConnectionClosedException`, and `RestClientSingleHostIntegTests` with the async timeout.

Current-dev proof:
- Probe run: `es-faildocs-probe-20260709-073704`
- HotSpot `RestClientGzipCompressionTests`: PASS, rc=0, 0.835s.
- HotSpot `RestClientSingleHostIntegTests`: PASS, rc=0, 0.941s.
- CratonVM JIT `RestClientGzipCompressionTests`: PASS, rc=0, 8.113s.
- CratonVM JIT `RestClientSingleHostIntegTests`: CRASH, rc=139, 15.754s, no Java-level exception captured.
- CratonVM --nojit `RestClientGzipCompressionTests`: PASS, rc=0, 8.107s.
- CratonVM --nojit `RestClientSingleHostIntegTests`: FAIL, rc=1, 32.153s, `timeout waiting for requests to be sent`.

Relationship to older notes:
- Older archived/internal docs record that the original RestClient embedded-server hang was fixed, with residual throughput and non-blocking connect issues left behind.
- This current-dev probe reopens the single-host async timeout as an active known issue because HotSpot passes and CratonVM --nojit still misses the 10s request latch.

Interpretation:
- The Gzip class no longer reproduces in current representative probes, so the active CratonVM signal is `RestClientSingleHostIntegTests`.
- The no-JIT failure is a Java-level timeout, while the JIT run exits 139; use the no-JIT row for root-cause work.
- The likely area remains CratonVM socket/NIO throughput or the synthetic `HttpServer` request path, not Lucene/randomizedtesting.

Next investigation:
- Reuse the older `RestClientSingleHostIntegTests.testManyAsyncRequests` residual work and rerun it on current `dev` with per-request timing.
- Separate client-side Apache async-NIO latency from synthetic `HttpServer` dispatch latency, then decide between keep-alive/server-loop work and selector/connect path work.
