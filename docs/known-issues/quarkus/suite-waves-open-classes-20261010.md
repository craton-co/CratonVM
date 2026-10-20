# Quarkus suite, sharded waves: classes that pass on HotSpot and still do not on CratonVM — 2026-10-10

Harness: `apps/quarkus-suite-runner/modrun/` (gitignored fixture): `master.tsv` (8 583 test classes of the 611 modules with
compiled tests; `master-ok.tsv` = the 8 240 whose module resolves offline), shards of 400 (`waves/shard-NN`), `runwave.sh`
(4 processes x ~100 classes, each with its own `quarkus.http.test-port`), then a SERIAL pass (`modrun.sh <exe> <out> <list> 600 1`,
no port override) over every non-pass, and HotSpot (`modrun.sh hs ...`) as the oracle. A class is "identical" when both
give the same status (`waves/hs-same.txt`); the code-cache cap is lifted to 2 GiB in the harness
(`CRATONVM_JIT_CODE_CACHE_MAX_MB`; dev-mode restarts retain dead-loader code and hit the 256 MiB cap in about 90 s,
after which every method stays interpreted: a performance matter, out of scope here, but it turns tests into timeouts).

## Wave 2 (shard 1 + the wave-1 leftovers), after the serial pass on the fixed binary

| cluster | classes | status |
|---|---|---|
| Quarkus Elytron security over RESTEasy + Servlet (`elytron-security-properties-file`: `BasicAuthTestCase`, `CustomAuthEmbedded*`; `elytron-security-jdbc` x6; `elytron-security-ldap` x3) | 12 | HotSpot PASS. The request flow is identical to HotSpot's (`UnauthorizedExceptionMapper` builds the 401 challenge, `Response.status(int)` is right in isolation), but the client receives `HTTP/1.1 200 OK` with the `www-authenticate` header. `--nojit` fails the same; the status is lost between RESTEasy's `HttpServletResponseWrapper.setStatus` and the Vert.x response (`HttpServletResponseImpl.setStatus` skips the store when `responseStarted()` / `treatAsCommitted`, otherwise `VertxHttpExchange.setStatusCode`). Next step: a Java-level trace of those two methods (override jar), or a VM method-entry trace. |
| devui test runner / MCP (`TestRunnerOneShotTestCase`, `ExcludeTagsTestCase`, `DevMcpTest`, `DevUICorsTest`) | 4 | HotSpot PASS for the first two; `DevMcpTest`/`DevUICorsTest` HotSpot FAIL/CRASH, CratonVM HANG. |
| gRPC (`RegularGrpcServiceWithSSL*`, `ClientDeadlineTest`, `GrpcDevModeTest`) | 4 | HotSpot PASS, CratonVM FAIL/HANG on the serial pass. |
| `FlywayDevModeCreateFromHibernateTest` | 1 | HotSpot PASS. |
| `AeshSshIdleActivityResetTest` | 1 | see `aesh-first-connection-cold-start-timing-20261009.md`. |

Fixed in these waves (records under `docs/internal/`): the `LocaleConverter` native in front of the real converter, a JIT
by-name tail that invoked the abstract `Function.apply` on a lambda of a sub-interface, and `ClassLoader.name` read through a
subclass's own `name` field.
