# OAuth2 issuer-URI test: CratonVM blows a 500 ms localhost HTTP budget

**Status:** 🔴 **OPEN**, CratonVM defect. The timeout is REAL, not spurious —
the read waited out its full budget and no response had arrived.

Two earlier versions of this doc were wrong and are superseded:

1. "host-load artifact of the harness" — decided by CratonVM-vs-CratonVM, which
   cannot answer *is this ours*. HotSpot on the same host is **11/11 clean**,
   including at load average 47 while CratonVM failed at 16.
2. "spurious `SocketTimeoutException`, probably the poll layer" — measured and
   false. See below.

## The measurement that settled it

Instrumenting every read-timeout site in
`native-builtins/src/http_url_connection.rs` with elapsed-vs-configured, run
under 10 added CPU burners, it fired on the **first** run:

```
[HUC-DIAG] site=read_io_err/response read waited=507ms configured=500ms pooled=false
                url=http://localhost:44017/test/.well-known/openid-configuration
[HUC-DIAG] site=raise3               waited=1948ms configured=500ms pooled=false
```

Two facts, both surprising:

* **The configured read timeout is 500 ms, not the 30 s I assumed.** Every OIDC
  discovery request in the class runs with `read_timeout_ms=Some(500)`.
* **The wait is real.** 507 ms against a 500 ms budget. Nothing is short-cutting
  the timeout; CratonVM genuinely did not have a response after 500 ms for a
  round trip to a `MockWebServer` **inside the same VM**.

So the defect is **latency**, not timeout handling: a localhost HTTP exchange
that must complete within 500 ms sometimes doesn't.

Corroborating: `probes/MockWebHangProbe.java` (same request shape, 150
iterations, suite env) shows CratonVM worst-case **179 ms** vs HotSpot **99 ms**
with nothing else running — already ~2x, and the suite adds JIT compilation and
GC on top.

## Established

| | |
| --- | --- |
| HotSpot, same host, interleaved | 11/11 PASS (incl. load 47) |
| CratonVM, JIT | ~12 failures in ~25 runs |
| CratonVM, `--nojit` | **0** failures in 8 |
| the test alone (`OneMethodRunner`) | 6/6 PASS, ~9s |
| effective read timeout | 500 ms, on JIT **and** `--nojit` |

`--nojit` being clean while the 500 ms budget is identical points at
**compilation-time stalls** (background compile, deopt, or the GC they drive)
on the thread serving or consuming the response — not at wrong configuration.

## Where the 500 ms comes from — unresolved, and a warning

CratonVM's own exception stack blamed
`SimpleClientHttpRequestFactory.prepareConnection` →
`JwtDecoderProviderConfigurationUtils.getConfiguration`, i.e. Spring Security's
static `RestTemplate`. That attribution is **wrong**:

* that factory holds `readTimeout=30000` on both VMs, dumped from inside the
  suite after the test runs;
* `-Dsun.net.client.defaultReadTimeout=7777` does not change the 500;
* installing a `SimpleClientHttpRequestFactory` subclass into that
  `RestTemplate` and running the test, **`prepareConnection` is never called —
  on HotSpot either**. The discovery requests do not go through it.

**Do not trust a VM-generated Java stack as the sole attribution here** — it
cost several hours. The 500 ms client has not been identified; it is some other
`RestClient`/factory inside Spring Security 7.1's `withIssuerLocation` path.
Whether HotSpot runs the same 500 ms budget is therefore **not yet proven**, and
that is the one remaining fork:

* if HotSpot also budgets 500 ms → pure CratonVM latency defect, as framed here;
* if HotSpot budgets more → there is *also* a configuration path we get wrong.

## Next step

Identify the client by instrumenting `huc_set_read_timeout` to dump the *caller
object's* class rather than relying on the stack, or by running the class under
a HotSpot agent that logs `HttpURLConnection.setReadTimeout`. Then measure the
CratonVM-side stall directly: timestamp request-write and response-first-byte in
`perform`, and correlate with `[cratonvm-jitc]` compile activity in the same
window.

## Reproducing

The flake needs the whole class *and* load. Under 10 CPU burners it fired on the
first run; in a quiet window it can pass 6 times running.

```bash
# instrumented hunt loop used above
/tmp/hunt.sh          # burners + suite until a [HUC-DIAG] line appears
```

Arms must be interleaved, never run as consecutive blocks — see
[[feedback_interleave_ab_arms_never_run_them_in_separate_blocks]].
