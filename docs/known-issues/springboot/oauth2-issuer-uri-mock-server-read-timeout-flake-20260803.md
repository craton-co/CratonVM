# OAuth2 issuer-URI test: CratonVM blows a 500 ms localhost HTTP budget

**Status:** 🔴 **OPEN**, CratonVM defect. The timeout is REAL, not spurious —
the read waited out its full budget and no response had arrived.

**2026-08-06:** two of the three open questions are now closed — the 500 ms
budget is library behaviour that **HotSpot applies identically**, and the client
is identified. A cheap local repro with both controls now exists. What is still
open is the stall itself.

Three earlier versions of this doc were wrong and are superseded:

1. "host-load artifact of the harness" — decided by CratonVM-vs-CratonVM, which
   cannot answer *is this ours*. HotSpot on the same host is **11/11 clean**,
   including at load average 47 while CratonVM failed at 16.
2. "spurious `SocketTimeoutException`, probably the poll layer" — measured and
   false. See below.
3. "the 500 ms client has not been identified, so whether HotSpot budgets the
   same is unproven" — both are now measured, and HotSpot budgets the same.

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

## A cheap local reproduction, with both controls (2026-08-06)

The Azure recipe (whole class + 10 CPU burners) is no longer needed. **Six
concurrent lanes of the class on one Windows box reproduces it**, and the same
harness reproduces this page's own `--nojit` control:

| arm | runs | runs with >=1 failed test |
|---|---:|---:|
| CratonVM, JIT | 12 | **7** |
| CratonVM, `--nojit` | 12 | **0** |

Failures are `SocketTimeoutException: Read timed out` on
`autoConfigurationShouldConfigureResourceServerUsingOidcIssuerUri` and
`autoConfigurationShouldConfigureCustomValidators` — the same symptom this page
was opened for. The lanes supply their own load; no burners.

```powershell
# 6 lanes x 2 rounds, ~6 min per arm
run-single-class.ps1 -Module module/spring-boot-security-oauth2-resource-server `
  -ClassName org.springframework.boot.security.oauth2.server.resource.autoconfigure.OAuth2ResourceServerAutoConfigurationTests `
  -SpringBootRoot <built fixture> -Exe <binary>     # add -NoJit for the control arm
```

## What it is NOT: a baseline latency deficit

`probes/IssuerBudgetProbe.java`'s `headroom` mode sweeps the server delay: at
delay D the exchange has 500-D ms of budget left, so the largest D a VM still
passes measures what that VM consumes. On a quiet box:

| VM | headroom limit | consumed |
|---|---:|---|
| HotSpot | 480 ms | < 20 ms |
| CratonVM, JIT, **buffered** server read | **480 ms** | **< 20 ms** |
| CratonVM, JIT, byte-at-a-time server read | 400 ms | ~100 ms |

**CratonVM matches HotSpot when the server reads the way a real server does.**
So there is no constant per-exchange tax to find: the flake is a *transient*
that only appears under concurrent load with the JIT on, exactly as the
`--nojit` control implies. That eliminates a whole family of explanations
(connect cost, poll granularity, per-request HTTP overhead), all of which would
have shown up as a steady-state gap here and do not.

**The third row is a warning, not a result.** It was the first number this probe
produced, and it is an artifact of the probe: `readLine` issued one `read()` per
byte, and CratonVM's per-call socket-read cost (see below) turned ~150 header
bytes into ~10 ms of server-side latency. Read one way it "confirmed" a 100 ms
CratonVM latency deficit; the deficit was the instrument. A probe that reads
differently from the thing it models measures itself.

### The probe alone does not reproduce it — bounded negative

Shrinking the repro to the probe was tried and **failed**, which is worth
knowing before anyone tries again. `IssuerBudgetProbe soak 40` run 6-wide (240
exchanges, zero server delay, JIT on) produced **0 failures**, though it does get
close: 58 of 240 exchanges exceeded 400 ms and the worst was 1651 ms. The
per-request 500 ms read budget was never blown, because a slow *exchange* is not
a slow *read* — the budget is per read, and the probe's two reads stayed inside
it even when the surrounding work did not.

So the trigger needs what the probe lacks: 52 test methods each building a Spring
context, and MockWebServer, i.e. far more class loading and compilation churn
than two HTTP requests generate. Keep the 6-lane class repro; do not spend more
time trying to shrink it to a probe without a new idea about the mechanism.

## A separate, real defect found on the way: single-byte socket reads are ~35x

Not the cause of this flake — MockWebServer reads through buffered Okio segments
— but genuine, and worth its own fix. With the payload already in the receive
buffer (so no blocking, no wakeup, pure per-call cost),
`probes/IssuerBudgetProbe.java readcost` measures 8192 single-byte
`InputStream.read()` calls on a connected socket:

| VM | single-byte read | bulk read |
|---|---:|---:|
| HotSpot | ~1.5 µs/call | ~0.11 µs/call |
| CratonVM | **~55 µs/call** | ~0.3 µs/call |

Bulk reads are fine on both; it is fixed overhead per `read()` call. Any Java
code that parses a protocol byte-at-a-time off a raw socket — a hand-rolled
header parser, `DataInputStream.readLine`, an unbuffered `InputStreamReader` —
pays ~35x on CratonVM.

## Where the 500 ms comes from — RESOLVED 2026-08-06, and HotSpot budgets it too

**The fork is closed on the "pure CratonVM latency defect" side.** There is no
configuration path we get wrong: HotSpot applies the *same* 500 ms budget to the
*same* request.

`probes/IssuerBudgetProbe.java` settles it without instrumenting either VM, so
one measurement runs on both. It stands up a plain-`ServerSocket` OIDC endpoint
that stalls every response by a fixed delay (applied *after* the request is fully
read, so it is pure response latency), calls `JwtDecoders.fromIssuerLocation` —
the entry point the failing test reaches through `SupplierJwtDecoder`'s delegate
— and reports the outcome per delay. **HotSpot, JDK 25.0.3:**

| delay | outcome | elapsed | last request served |
|---:|---|---:|---|
| 0 ms | OK | 652 ms | jwks.json |
| 200 ms | OK | 408 ms | jwks.json |
| 400 ms | OK | 817 ms | jwks.json |
| 600 ms | **FAIL** | 503 ms | openid-configuration |
| 900 ms | **FAIL** | 507 ms | openid-configuration |
| 1500 ms | **FAIL** | 513 ms | openid-configuration |
| 3000 ms | **FAIL** | 509 ms | openid-configuration |

`SocketTimeoutException: Read timed out` at ~503-513 ms against a 500 ms budget,
on the discovery request — the same URL, the same margin and the same message
CratonVM's `[HUC-DIAG]` line recorded. The 500 ms is library behaviour, not ours.

**The client, from HotSpot's stack (which can be trusted):**

```
JwtDecoderProviderConfigurationUtils.getConfiguration(...:165)   RestTemplate.exchange
NimbusJwtDecoder.lambda$withIssuerLocation$0(NimbusJwtDecoder.java:233)
NimbusJwtDecoder$JwkSetUriJwtDecoderBuilder.jwkSource/processor/build
JwtDecoders.fromIssuerLocation(JwtDecoders.java:92)
```

and the budget is hard-coded in a **private inner class**,
`NimbusJwtDecoder$RestTemplateWithNimbusDefaultTimeouts`:

```
 4: new           SimpleClientHttpRequestFactory
13: sipush        500
16: invokevirtual SimpleClientHttpRequestFactory.setConnectTimeout:(I)V
20: sipush        500
23: invokevirtual SimpleClientHttpRequestFactory.setReadTimeout:(I)V
```

500 is a literal, matching Nimbus's `RemoteJWKSet.DEFAULT_HTTP_CONNECT_TIMEOUT` /
`DEFAULT_HTTP_READ_TIMEOUT` (both `= 500`, confirmed with `javap -constants` on
nimbus-jose-jwt 10.6).

That reconciles every earlier observation rather than contradicting them. The old
attribution was right about the *kind* of client and wrong about the *instance*:

* it really is a `SimpleClientHttpRequestFactory`, which is why the stack said so;
* but it is a **per-`withIssuerLocation` instance owned by that inner-class
  `RestTemplate`**, not the static `JwtDecoderProviderConfigurationUtils.rest` —
  so a subclass installed into the static one is never consulted, exactly as
  observed, on either VM;
* the static one really does hold 30000, and is genuinely not the client here;
* `-Dsun.net.client.defaultReadTimeout` cannot move a `sipush 500`.

**The warning still stands, with a sharper edge.** The stack was not *wrong*, it
was under-specified — and "right class, wrong instance" reads exactly like a
correct attribution. When a stack names a type you can reach two ways, identify
the *object*, not the class. The cheap way here turned out not to be
instrumentation at all: rebuild the shape in a standalone probe and measure both
VMs with it.

## Next step

Two things are done and should not be redone: the client is identified, and the
"steady-state latency" hypothesis is dead. What remains is a **transient stall
under concurrent load with the JIT on**.

Use the 6-lane repro above — it gives a 7/12-vs-0/12 signal in ~12 minutes, with
a positive and a negative control, on one box. Then:

1. Timestamp request-write and response-first-byte inside `perform`, and dump any
   exchange over ~100 ms with the wall-clock window it covers.
2. Correlate those windows against `[cratonvm-jitc]` compile activity and GC.
   The question is narrow now: *what does the JIT do that parks the
   MockWebServer thread (or the reading thread) for >500 ms?*
3. `--stack-sample-ms N` is the time-weighted profiler for this;
   `--stack-dump-on-timeout` is a CALL-COUNT trace and cannot see compiled
   frames, so it will not answer it.

Worth checking early, because it is cheap and would reframe the search: whether
the stall is one long pause or many small ones. The headroom result says
CratonVM has no steady-state deficit, so a >500 ms miss is very unlikely to be
an accumulation of small costs.

## Reproducing

The flake needs the whole class *and* load. Under 10 CPU burners it fired on the
first run; in a quiet window it can pass 6 times running.

```bash
# instrumented hunt loop used above
/tmp/hunt.sh          # burners + suite until a [HUC-DIAG] line appears
```

Arms must be interleaved, never run as consecutive blocks — see
[[feedback_interleave_ab_arms_never_run_them_in_separate_blocks]].
