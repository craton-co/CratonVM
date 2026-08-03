# OAuth2 issuer-URI test: a CratonVM-only, JIT-only spurious read timeout

**Status:** 🔴 **OPEN**, CratonVM defect. Mechanism not yet located.

**This doc replaces an earlier version that called this a host-load artifact of
the test harness. That conclusion was wrong** — it was reached by comparing
CratonVM against CratonVM, which can only ever answer "did my change cause it",
never "is this us". The control that decides it is HotSpot on the same host, and
it was never run. It has been now.

## Symptom

`OAuth2ResourceServerAutoConfigurationTests`, always the same single test of 52:

```
autoConfigurationShouldConfigureResourceServerUsingOAuthIssuerUri()
  => JwtDecoderInitializationException: Failed to lazily resolve the supplied JwtDecoder
  Caused by: IllegalArgumentException: Unable to resolve the Configuration with the
             provided Issuer of "http://localhost:<port>/test"
  Caused by: ResourceAccessException: I/O error on GET request for
             "http://localhost:<port>/test/.well-known/openid-configuration": Read timed out
  Caused by: java.net.SocketTimeoutException: Read timed out
```

The test starts an in-VM `MockWebServer`, enqueues exactly four responses
(404, 404, the OIDC config, the JWK set) and has Spring Security fetch the
discovery document. The URI named is the **first** of the three discovery
candidates — and `JwtDecoderProviderConfigurationUtils.getConfiguration` only
continues to the next candidate on a **4xx**; a `ResourceAccessException` aborts
immediately. So the very first request times out, with all four responses
already sitting in the mock's queue.

## It is ours

| arm | runs | failures |
| --- | --- | --- |
| **HotSpot** (`/data/hsrun.sh`, same host, same class) | 11 | **0** |
| CratonVM, JIT | ~25 | ~12 |
| CratonVM, `--nojit` | 8 | **0** |

Interleaved (arms alternating inside one window, so both see the same load —
`/data/data/otout/il2.summary`): HotSpot passed in all six rounds **including at
load average 47**, while CratonVM failed at load 16. Load is not the variable.

A second interleave, JIT vs `--nojit` on the same binary
(`otout/il3.summary`): JIT 2 failures in 5, `--nojit` 0 in 5.

## What is established

* **JIT-dependent.** `--nojit` has never failed it (0/8).
* **Needs the whole class.** Run alone via `OneMethodRunner`, the test passes
  6/6 at ~9s each. The ~30 preceding tests — each starting and closing its own
  `MockWebServer` on a fresh ephemeral port — are part of the trigger.
* **The timeout is 30s** (`sun.net.client.defaultReadTimeout`, defaulted to
  `"30000"` in `JwtDecoderProviderConfigurationUtils`'s static initializer),
  **but a failing run is not 30s longer than a passing one** (107s vs 92/99s).
  Either the wait is not really 30s, or it overlaps work that a passing run
  also does. Unresolved, and the most useful thing to measure next.
* **No TCP connection survives the stall.** A 1Hz `ss -tanp` poll across a
  failing run caught established CratonVM connections in only 6 samples of
  ~106, none persisting. A 30-second blocking read should have been visible.

## Eliminated

* **`sun/nio/ch/Net.poll` is not on this path at all** — an instrumented build
  logged 173 `[NET]` operations (socket/bind/accept/read/write/close) and
  **zero** poll calls across a failing run. Any reasoning that starts from the
  JDK's `NioSocketImpl.timedRead`/`park` is reasoning about code that does not
  run here. (I nearly drew a conclusion from a clean `SHORT-FALSE=0` counter
  before checking the counter's site was reachable — see
  [[reference_inert_lever_is_not_an_elimination]].)
* **The client is CratonVM's own native `HttpURLConnection`**
  (`native-builtins/src/http_url_connection.rs`, ~4900 lines), not the JDK's
  Java one. It uses a blocking socket with `SO_RCVTIMEO`, maps
  `WouldBlock`/`TimedOut` to `READ_TIMEOUT_SENTINEL`, and raises
  `SocketTimeoutException` from `huc_real_perform`. Three sites raise it
  (`cached`, `streaming`, `buffered` — lines 600 / 654 / 791).
* **The obvious shape does not reproduce standalone.** `probes/MockWebHangProbe.java`
  drives one `MockWebServer` and three sequential `HttpURLConnection` GETs
  (404/404/200, 30s timeout), 150 iterations, with the suite's exact env
  (`CRATONVM_REAL=net-sockets,aqs`, `CRATONVM_JIT=rootsnap-cache`, `--Xmx 4g`):
  **0 slow iterations, worst 179ms**. HotSpot: worst 99ms.

## Where to look next

`perform`'s **plain-HTTP keep-alive pool**, keyed `(host, port)`. Every
`MockWebServer` in the class is `localhost:<ephemeral port>`; over 52 tests the
kernel recycles ports, so a pooled socket can key to a port whose original peer
is gone. `try_pooled_request` bounds only the FIRST byte by
`POOL_PROBE_TIMEOUT` and then hands off to `read_response_with_prefix` under
the caller's full `read_timeout`; its errors do fall back to a fresh connect
(`Err(_) => pool_clear(...)`), so the fallback looks right on inspection — but
it is the one piece of state that persists ACROSS tests, which is exactly the
property the repro needs.

The measurement that would settle it: instrument the three
`socket_timeout_ex("Read timed out")` sites with elapsed-vs-configured and a
pooled/fresh flag. That build exists in this branch's history; it produced no
output only because the flake did not fire in the four runs it got. **Run it
under load** (the failure rate tracks something that varied between 0% and 100%
across the day) rather than in a quiet window.

## Reproducing

```bash
# both arms, alternating, same window
/data/hsrun.sh module/spring-boot-security-oauth2-resource-server \
  org.springframework.boot.security.oauth2.server.resource.autoconfigure.OAuth2ResourceServerAutoConfigurationTests /tmp/hs 1
/data/sbrun.sh <exe> jit module/spring-boot-security-oauth2-resource-server \
  org.springframework.boot.security.oauth2.server.resource.autoconfigure.OAuth2ResourceServerAutoConfigurationTests /tmp/cv 1 1200
```

Never run the arms as two consecutive blocks — see
[[feedback_interleave_ab_arms_never_run_them_in_separate_blocks]], which this
investigation is the origin of.
