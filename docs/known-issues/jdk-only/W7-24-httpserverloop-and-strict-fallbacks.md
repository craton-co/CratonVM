# `net_phase_e.rs` under strict: one door defect, two behaviour carriers, and what "fix the door" is not

**Status: source changes APPLIED, NOT REBUILT, 2026-08-11.** Every number below
was taken by running the already-built `dev` binary at
`C:/craton/CratonVM/target/release/cratonvm.exe` (built 2026-08-11 19:41), the
JDK 25 image at `C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot`, and
that image's `javap` and `java`. **No claim is made that the source changes in
this branch compile or work** — the binary predates them, and predates several
fixes merged to `dev` today.


> **VERIFIED AGAINST A BINARY 2026-09-02.** §1's door defect is FIXED and no
> longer reproduces. `probes/HttpServerWildcardAddressProbe.java` on
> `/data/l7dod-target/debug/cratonvm` (built 2026-09-02 00:43, tree
> `94a887093`) under `--jdk-only`: **rc=0**, all seven rows printed, no
> `NoClassDefFoundError: CratonVM$HttpServerLoop`. `--jdk-only` and `--real-jdk`
> are **byte-identical**, which is the shape §1 predicted — the door was the
> only thing strict mode was taking out.
>
> This record sat "APPLIED, NOT REBUILT" for **22 days**. The fix was right the
> whole time.
>
> **The probe could not be scored until today, and that was its own fault.**
> Every bind here asks for port 0, so the kernel picks a new number each run and
> a raw diff reported four differing rows on two runs of the SAME binary. The
> probe now normalises the trailing `:<port>` and keeps "a port was bound" as a
> separate boolean; both VMs are self-stable across two runs, and the diff means
> something. See its `norm()` comment for why the address itself must survive
> the rewrite.
>
> **What still differs from HotSpot, and why it is not this record's defect.**
> Four of seven rows, all one thing: HotSpot binds the wildcard as a dual-stack
> IPv6 socket and reports `/[0:0:0:0:0:0:0:0]`, CratonVM reports `0.0.0.0`.
> `isAnyLocalAddress()` is `true` on both, so a caller asking the behavioural
> question gets the same answer. `native-io/src/socket_channel.rs` names this
> divergence as knowingly left open above `advertised_listener_host`.
>
> **The probe's own header comment was stale and has been corrected.** It
> described the `0.0.0.0` → `127.0.0.1` rewrite as current; that rewrite was
> REMOVED on 2026-08-10 using this very probe, whose measurement is quoted in
> the source. A probe that misdescribes the code it measures sends its next
> reader looking for a fix that already landed.
>
> **A finding this run turned up that no record held.** In the registry dump,
> `sun/net/httpserver/HttpServerImpl.getAddress` shows **2 invocations** and
> `com/sun/net/httpserver/HttpServer.getAddress` shows **0** — the probe called
> `getAddress()` exactly twice, so the abstract class's registration is not the
> one that answers. `HttpServer` is abstract, every concrete subclass must
> override, and the dispatch door asks the DECLARING class: nine instance-method
> registrations on `com/sun/net/httpserver/HttpServer` are unreachable, with the
> `HttpServerImpl` twins carrying the traffic. The static `create` overload is
> the control — it is on the same class and shows 2 invocations, so the counter
> works and the zeros are real. This is the family
> `H5-1-the-abstract-registrations-are-fabricated-receivers-20260820.md` is
> about; it is recorded here, not fixed here, because removing a dead
> registration is a change to `net_phase_e.rs` that belongs to whoever owns it.

Branch: `fix/jdk-only-httpserverloop-and-strict-fallbacks-20260811`.
Files changed: `native-builtins/src/net_phase_e.rs` and this record.

This record continues `W7-17-vm-internal-door-sweep.md`, which classified 44
VM-minted classes three ways and left one open fatal door defect in this file.
It applies that fix (§1), and then does what the sweep says the *other* ~35
classes need: it takes this file's own surface, finds every site that mints a
compatibility stand-in whose natives strict drops, and puts a **real-JDK
fallback at the point of refusal** (§3).

---

## 1. `CratonVM$HttpServerLoop` — the door defect, and both gates checked

`com.sun.net.httpserver.HttpServer.start()` could not start under `--jdk-only`.
Reproduced on the shipped binary before touching anything:

```
$ cratonvm --jdk-only --java-home <JDK25> -cp <probes> HttpServerWildcardAddressProbe
channel getLocalAddress()   = /0.0.0.0:63877
channel SO_REUSEADDR        = true
Exception in thread "main" java/lang/NoClassDefFoundError: CratonVM$HttpServerLoop
	at HttpServerWildcardAddressProbe.main(HttpServerWildcardAddressProbe.java:49)
```

The same probe under `--real-jdk` prints all four address lines and exits Ok, so
this is strict-only. `bind()` and the wildcard-address reporting above it are
unaffected; `start()` is the call the door takes out, because
`re10_spawn_dispatcher` is reached from the `HttpServer.start` native and
`?`-propagates the refusal out of it.

**Was the recorded patch still needed?** Yes. `W7-17` §6 hunk A carries the
code; this campaign has fifteen records claiming a patch was never applied when
it was already in the tree, so it was checked against the source and against
`dev` first, not against the record: `git show origin/dev:native-builtins/src/net_phase_e.rs`
contains **zero** occurrences of `ensure_vm_internal_class`, and the mint site
was the bare `try_alloc_concurrent_synthetic(ctx, HS_LOOP_CLASS, 1)?`.

**Gate 1 — the class.** `javap CratonVM.HttpServerLoop` against the JDK 25
image answers *class not found*; the name is not in a JDK namespace at all. It
is a 1-slot `Runnable` this VM invents to carry `server_id` from
`re10_spawn_dispatcher` to `re10_serve_loop_run`, minted four times (one per
`HS_DISPATCHER_POOL` dispatcher) — contract §1 item 6's shape, permitted in
every mode through `ensure_vm_internal_class`. Through
`try_alloc_concurrent_synthetic` alone it took `ClassOrigin::CompatibilityStub`,
which `--jdk-only` correctly refuses.

**Gate 2 — its natives. Measured per class and per mode, not by analogy.**
`--dump-native-registry` over a boot in each mode reports exactly one
registration under this class name, and the same kind in both:

```json
compatible: {"class":"CratonVM$HttpServerLoop","name":"run","descriptor":"()V",
             "kind":"bridge","registered_by":"native-builtins/src/net_phase_e.rs:16435",
             "kind_stated":false,"kind_chosen":true,"owns_slot":true}
jdk-only:   {"class":"CratonVM$HttpServerLoop","name":"run","descriptor":"()V",
             "kind":"bridge", … identical … }
```

(The dumps' own `counts` line is the control that the strict dump really is
strict: `synthetic-stub` 1,262 in Compatible, **0** under `--jdk-only`, with
`bridge` 9,740 in both.) Read against `native-api/src/no_image_receiver.rs`,
that is what the tables predict: the name does not start with `cratonvm/`, so
`receiver_declared_by_no_supported_image` never consults
`VM_MINTED_STAND_IN_RECEIVERS`; it is in neither `NO_IMAGE_JDK_RECEIVERS` nor
`VM_SERVICE_RECEIVERS` nor `STRICT_STILL_FABRICATES`; so nothing re-tags it
`SyntheticStub` and the `JdkOnly` drop arm never sees it.

**Gate 2 is already open, gate 1 alone was refusing, so the door fix is
necessary *and* sufficient here** — which is not true of most of `W7-17`'s
table, and is the whole reason that record insists the registry dump is taken
twice. A concurrent lane measured the counter-example on
`cratonvm/internal/LinkedListSnapshotListItr`: clearing gate 1 alone moved the
failure from `NoClassDefFoundError` at the mint to `UnsatisfiedLinkError` at the
first call, which is a moved symptom, not a fix.

The fix is the one-line pre-mint from `W7-17` §6 hunk A, with its reasoning
written onto the site. It is a pre-mint rather than a replacement because
`fabricate_class` returns the existing `ClassId` for an already-loaded name
before it reaches `admit_compatibility_class`, so the allocation below keeps its
real-vs-requested field-count widening and its GC-safe retry unchanged, and only
the recorded ORIGIN moves. The `try_alloc_concurrent_synthetic(ctx,
"java/lang/Thread", …)` two lines below is deliberately untouched:
`java/lang/Thread` has real bytes, `fabricate_class` loads them, and no
`compatibility-class-requested` row is ever emitted for it.

### `Compatible` mode, at this site

**Unchanged.** `Compatible` never refuses the mint, so the only thing that moves
is the class's recorded origin: `--dump-class-origins` reports `vm-internal`
instead of `compatibility-stub`, `--jdk-only-report`'s
`counts.compatibility_classes` drops by one and `counts.generated_classes` rises
by one, and `vm_exec.rs`'s `stub_hint` stops appending *"class not found on any
classpath entry — synthetic stub, add the missing jar"* to a `NoSuchMethodError`
naming a class no jar can ever contain. All three are corrections.
`BASELINE_SYNTHETIC_STUBS` / `stub_ratchet` do not move: they count
`SyntheticStub`-tagged **registrations**, and this class's one registration is a
`bridge` in both modes (above), so there is nothing for them to count.

## 2. `HttpServer.start()` is not the only casualty — the census of this file

The door fix above is necessary and not sufficient for `net_phase_e.rs`, and
the way to see that is to census the file rather than to fix what the sweep
happened to name. Two instruments, and they answer different halves:

**Static — every name this file can mint.** All 64
`try_alloc_concurrent_synthetic` sites, with the four dynamic targets resolved
by reading them (`class_name` → `Inet4Address`/`Inet6Address` and
`HttpServer`/`sun.net.httpserver.HttpServerImpl`; `carrier` →
`JarURLConnection`/`HttpsURLConnection`/`HttpURLConnection`; the `RE5_*`,
`HS_LOOP_CLASS` and `JRT_URL_CONNECTION` constants), then `javap -p` on each
against the JDK 25 image. **Of the production names, all but five are declared
by the image** — those load real bytes, `fabricate_class` never reaches
`admit_compatibility_class`, and no `compatibility-class-requested` row is ever
emitted for them in either mode. The five that are not:
`CratonVM$HttpServerLoop`, `com/sun/net/httpserver/HttpExchange$ResponseBody`,
`cratonvm/net/HttpBodyReplaySubscription`, `javax/net/ssl/SSLSocketInputStream`
and `javax/net/ssl/SSLSocketOutputStream`. The remaining absent names
(`java/nio/HeapByteBuffer`, a Spring handler name, `test/…`) are all inside
`#[cfg(test)]`.

**Dynamic — which of them a run actually reaches, in Compatible mode, which is
the wider net** (execution continues past a mint that strict kills, so it
reaches sites strict never gets to). Two probes written for this, both with a
passing HotSpot 25 control:

* `HttpServeProbe` — `HttpServer.create` → `createContext` → `start` → a handler
  reading `getRequestBody()` and writing `getResponseBody()`, driven by both a
  `HttpURLConnection` and a `java.net.http.HttpClient` client.
* `RawServerBodyHandlerProbe` — a **custom** `HttpResponse.BodyHandler` /
  `BodySubscriber`, served from a plain `ServerSocket` rather than from
  `HttpServer`, precisely so its verdict does not depend on the §1 defect.

The two instruments merged — the five static names, with the `javap` verdict,
the per-mode `--dump-native-registry` kinds, and whether a probe reached them.
The first three rows are rows those runs actually produced, folded on
`(class, requester)`; the last two are static-only, and §5 records what it took
to make them appear at all:

| class | requester · fn | `javap` | natives compat → strict | carrier kind | strict verdict |
|---|---|---|---|---|---|
| `CratonVM$HttpServerLoop` | `re10_spawn_dispatcher` | not found | 1 bridge → **1 bridge** | **VM service** | door defect — §1, FIXED |
| `com/sun/net/httpserver/HttpExchange$ResponseBody` | `register_re10_http_server` · `getResponseBody` | not found | 5 synthetic-stub → **0** | **behaviour carrier** | fallback — §3, IMPLEMENTED |
| `cratonvm/net/HttpBodyReplaySubscription` | `re5_drive_body_handler` | not found | 2 synthetic-stub → **0** | **behaviour carrier** | fallback BLOCKED — §4 |
| `javax/net/ssl/SSLSocketInputStream` | `register_re1_socket` · `getInputStream` — **but see §5: the LIVE requester is `ssl_security.rs`'s twin, a `bridge` in both modes** | not found | 4 synthetic-stub → **0** | behaviour carrier | ~~unreachable by default~~ **FATAL — all HTTPS. Corrected 2026-08-12, §5** |
| `javax/net/ssl/SSLSocketOutputStream` | `register_re1_socket` · `getOutputStream` — same | not found | 4 synthetic-stub → **0** | behaviour carrier | ~~unreachable by default~~ **FATAL — the witness names this half. §5** |

The three-way split `W7-17` predicted reproduces exactly, in one file, on five
classes: **one VM service, four behaviour carriers, no data carriers.** Every
one of the five passes the `javap` guardrail, and for four of them the guardrail
is the wrong question — which is the point that record makes, confirmed here
from a second direction.

**The second fatal defect, measured in isolation.** A custom `BodyHandler` under
`--jdk-only` dies without the `HttpServer` path being involved at all:

```
$ cratonvm --jdk-only --java-home <JDK25> -cp <probes> RawServerBodyHandlerProbe
responseInfo class = java.net.http.HttpResponse$ResponseInfo
Exception in thread "main" java/lang/NoClassDefFoundError: cratonvm/net/HttpBodyReplaySubscription
	at RawServerBodyHandlerProbe.main(RawServerBodyHandlerProbe.java:49)
```

HotSpot 25 runs that probe green (`subscription class =
jdk.internal.net.http.common.HttpBodySubscriberWrapper$SubscriptionWrapper`,
`status = 200`, `body = raw-body`), and CratonVM `--real-jdk` runs it green too,
so this is strict-only and not a broken vector. It is reached because
`java/net/http/HttpClient`'s own 19 natives are `bridge` in **both** modes — the
minting native is not itself dropped in strict, which is exactly the condition
`W7-17` §5 identifies as separating a live behaviour-carrier defect from the
`RJdkLambdas` / `RJdkProcess` rows where strict simply never reaches the mint.

## 3. `HttpExchange$ResponseBody` — the fallback, implemented

**Carrier kind: behaviour. `CompatibilityStub` is CORRECT and stays.** Five
natives registered under the name in Compatible, zero under `--jdk-only`; the
class is nothing but a holder for them. Flipping its origin would put back a
class strict mode has no implementation for and move the failure from
`NoClassDefFoundError` at `getResponseBody()` to `UnsatisfiedLinkError` at the
handler's first `write` — a different symptom, not a fix.

**The real JDK class that serves: `java.io.ByteArrayOutputStream`.** The
oracle's own answer cannot. Measured, HotSpot 25 returns
`sun.net.httpserver.PlaceholderOutputStream`, and `javap -p` on it shows why it
is unusable here: package-private, one constructor taking the `OutputStream` it
wraps, and a private `checkWrap()` that every write goes through and that throws
until a real `ExchangeImpl` has called `setWrappedStream`. There is no
`ExchangeImpl` on this path — CratonVM's `com.sun.net.httpserver` is native and
the response is serialised by `re10_dispatch_pending` after `handle()` returns.
`java.io.ByteArrayOutputStream` is a real, public `java.base` class whose every
needed method — `<init>()V`, `write(I)`, `write([B)`, `write([BII)`, `flush`,
`close`, `toByteArray` — is registered `bridge` in **both** modes
(`native-io/src/lib.rs`), so nothing about it is dropped by the strict policy,
and its semantics are the carrier's semantics: accumulate now, hand the bytes
over later. It is not the name HotSpot reports — but this VM's Compatible answer
(`com.sun.net.httpserver.HttpExchange$ResponseBody`) is not that name either, so
the fallback costs no fidelity that was there to lose and buys a working
response body where strict mode had none.

The implementation is `re10_real_jdk_response_body`, reached only from the `Err`
arm of the mint, in the shape `craton_alloc_system_logger` established. Three
things about it are worth stating, because they are where this kind of fix goes
wrong:

1. **The buffer is parked in the exchange's own chunk array (slot 6), not in a
   Rust side table.** The array is GC-traced from the exchange for exactly as
   long as the exchange lives, and the drain already reads it, in order. A side
   table keyed on an `ObjectRef` would need pinning across the whole handler
   call and would inherit a recycled address's state.
2. **The drain tells the buffer from a chunk with `object_is_array`, not by
   class name.** A heap-allocated reference array reports its *component* class
   from `class_id_of_object`, so a name test cannot detect arrays at all;
   `object_is_array` is a heap object-kind check and is the only correct
   discriminator here.
3. **`toByteArray()` allocates, so it is drained after the chunk walk, not
   inside it.** The walk's stated invariant is that nothing in it allocates,
   which is what lets it hold `chunks` and each `ba` across iterations; an
   `invoke_virtual` in the middle of it would have quietly broken that. The
   exchange and its chunk array are re-read from the pin afterwards.

A full chunk array is an **error** in the fallback path, not a silent drop. The
three `write` natives drop a chunk silently when the array is full; a fallback
buffer the drain will never read must not be silent about it. That is `W7-17`
§8's "refusal laundered into a wrong answer", which the whole behaviour-carrier
verdict depends on not happening.

### `Compatible` behaviour, at this site

**Unchanged, and the reason is structural rather than a review.** The fallback
lives entirely inside the `Err` arm of `try_alloc_concurrent_synthetic`, and
`Compatible` does not refuse this mint: measured, the class is fabricated once
in each of the two Compatible probe runs that actually serve a request, and both
of those reports carry its row. The drain's added arm is guarded on
`!object_is_array`, and Compatible never parks a non-array in the chunk array —
only `re10_real_jdk_response_body` does, and it cannot run there. The two can
never interleave within one exchange either: whether the mint is refused is a
property of the run, not of the call.

## 4. `cratonvm/net/HttpBodyReplaySubscription` — no fallback landed, and why

**Carrier kind: behaviour** (2 natives in Compatible, 0 under `--jdk-only`), so
the origin is again correct and the answer is again a fallback at the refusal.
**No real JDK class can take this one over without changing the delivery
contract, so this record states what would have to exist instead of landing a
guess.**

What the carrier does (`re5_replay_subscription_request` / `_cancel`): a
one-shot replay. `re5_drive_body_handler` parks the subscriber and the wire body
in the subscription's Java fields and calls `subscriber.onSubscribe(...)`;
delivery (`onNext(List.of(ByteBuffer.wrap(body)))` then `onComplete()`) happens
**lazily, on the caller's thread, at the first `request(n)`**. That laziness is
load-bearing and the function says so in place: `BodyHandlers.ofPublisher()`
forwards to a downstream subscriber that attaches and signals demand later, on a
different thread, and pushing before that demand drops the body.

The three candidates, and what each costs:

| candidate | why it does not serve |
|---|---|
| `java.util.concurrent.Flow$Subscription` — a real image class, and its two natives are `bridge` in **both** modes | those natives are `streams.rs`'s demand **counter** (`native_flow_request`: saturating-add into a side map, no delivery), and `register()` is last-registration-wins with `streams.rs` the observed winner in the dump. Allocating one yields a subscription whose `request()` counts and never delivers, so `getBody().toCompletableFuture().join()` would **hang** rather than fail. Strictly worse than today's loud `NoClassDefFoundError`. |
| `java.util.concurrent.SubmissionPublisher` — real and public; `subscribe`/`submit`/`close` hand the subscriber a genuine real-bytecode `Flow.Subscription` | its default executor is `ForkJoinPool.commonPool()`, so `onSubscribe`/`onNext`/`onComplete` move **off the caller's thread** while `re5_drive_body_handler` blocks on `join()`. That inverts the delivery contract quoted above, and no public JDK `Executor` runs inline, so the synchronous variant cannot be built either. |
| `jdk.internal.net.http.common.HttpBodySubscriberWrapper$SubscriptionWrapper` — what HotSpot returns, measured | reachable only by registering the existing replay natives **over a real JDK class**, which shadows that class's bytecode for *every* instance and needs the per-instance-marker + `invoke_virtual_bytecode_only` treatment. It is also a `jdk.internal.*` name, which can move between releases. |

**What would have to exist**, stated so the next lane does not re-derive it: a
`Flow.Subscription` implementation with **real bytecode** and synchronous
replay-on-first-request semantics. There are two honest ways to get one — define
it from bytes the VM ships (`define_class_from_bytes` over a small generated
class, which makes it a VM-internal class in the contract §1 item 6 sense rather
than a compatibility stand-in), or make `re5_drive_body_handler` stop needing one
by driving a `SubmissionPublisher` and accepting the threading change, with a
test that proves the `ofPublisher()` case still works. Both are real changes with
real risk, and neither is verifiable in a lane that cannot build. Landing either
blind on a path whose failure mode is a **hang** would trade a loud, correct
refusal for a silent one, which is the specific failure this campaign exists to
stop.

Until then the refusal stands and is loud: `NoClassDefFoundError:
cratonvm/net/HttpBodyReplaySubscription`, naming the class, at the
`HttpClient.send` call site.

## 5. The two `javax/net/ssl/SSLSocket*Stream` sites — latent, not live

> **CORRECTED 2026-08-12. The heading is right about the two sites in THIS file
> and wrong as a verdict on the two class names.** §2's table row reads
> "unreachable by default — §5", and §7 carries it as a residual. Both
> statements are true of `net_phase_e.rs`'s `register_re1_socket` mints, which
> are still behind the `io.real_net_sockets` early return and still dead in a
> default run — re-checked in source today, `net_phase_e.rs:5273` and `:5296`,
> inside a function whose first statement is `if
> crate::vmflags().io.real_net_sockets { return (); }`.
>
> **The last paragraph of this section names the live twins and then stops.**
> That was the whole finding: `phases_late/ssl_security.rs`'s
> `SSLSocket.getInputStream()`/`getOutputStream()` are `bridge` in **both**
> modes — they survive `--jdk-only` — and they minted the same two absent class
> names with a bare `?`. So under strict a real TLS connection **handshook
> successfully and then died at the first stream access**:
>
>     java.lang.NoClassDefFoundError: javax/net/ssl/SSLSocketOutputStream
>
> with HotSpot 25 running the identical program green. That is all HTTPS, client
> and server, and it is the same shape as §1's `CratonVM$HttpServerLoop`: a
> surviving bridge asking for a class §5 forbids. **The refusal was correct; the
> survival of its caller was the defect.** This record classified the pair
> "behaviour carrier, unreachable by default" on the strength of the file it was
> auditing, and `W7-17`'s table then inherited the verdict at its line 248 —
> which is the falsification that opened the 2026-08-12 re-audit in
> `W7-17-vm-internal-door-sweep.md` §5.0.
>
> **The fix is neither of the two repairs §3 of `W7-17` names.** A door fix
> would have been wrong (gate 2 is shut: 4 natives → 0), and no real JDK class
> can be *fallen back to* here because the natives ARE the TLS implementation.
> The landed fix re-targets the **receiver name** onto the exact pair real
> JSSE's `SSLSocketImpl` returns — `TLS_APP_IN_CLASS` /
> `TLS_APP_OUT_CLASS` = `sun/security/ssl/SSLSocketImpl$AppInputStream` /
> `$AppOutputStream` — keeping the natives and making the class real, so both
> gates stop firing and `getClass().getName()` now agrees with HotSpot instead
> of naming a class HotSpot has never had. `W7-17` §5.0 records this as repair
> shape **4**, which its §3 taxonomy was missing.
>
> **Status of that fix, stated precisely:** it is in
> `native-builtins/src/phases_late/ssl_security.rs` in this **working tree,
> uncommitted**, by another lane. Not on `dev`. Not built, not run. This lane
> read it; it did not verify it.
>
> **Two things it does NOT close, and both are load-bearing:**
>
> 1. The legacy names are **still registered on purpose**
>    (`TLS_LEGACY_IN_CLASS` / `TLS_LEGACY_OUT_CLASS`, with the reason on the
>    constants) precisely because `net_phase_e.rs:5273`/`:5296` can still mint
>    them under `CRATONVM_SYNTHETIC_NET_SOCKETS=1`. Dropping the eight
>    registrations would turn that path's `NoClassDefFoundError` into an
>    `UnsatisfiedLinkError` — `STRICT_STILL_FABRICATES`'s warning in the other
>    direction. So both names must stay in `NO_IMAGE_JDK_RECEIVERS`, and the
>    §5 sites below remain exactly as described.
> 2. Registering natives on `sun/security/ssl/SSLSocketImpl$App*Stream` makes
>    them **shadow real bytecode** — a `native-shadows-bytecode` census row
>    where there was a fabricated-class row. That is a better row to hold, and
>    it is a different row; it is that lane's to place.



`register_re1_socket`'s `getInputStream`/`getOutputStream` mint
`javax/net/ssl/SSLSocketInputStream` / `…OutputStream` for a layered TLS socket
(`sid >= RUSTLS_SOCK_ID_BASE`). Both names are absent from the image and both
are behaviour carriers — 4 natives each in Compatible, **0** under `--jdk-only`.
They are nevertheless **not reachable in a default run**, and the check that
says so is worth recording because it inverts what the source looks like:
`register_re1_socket` opens with `if crate::vmflags().io.real_net_sockets {
return (); }`, and `real_net_sockets` is default-ON (`types/src/flags.rs`:
`!present(src, "CRATONVM_SYNTHETIC_NET_SOCKETS")`). Measured rather than read:
`--dump-native-registry` on a default boot reports **zero** registrations under
`java/net/Socket`; the same boot with `CRATONVM_SYNTHETIC_NET_SOCKETS=1` reports
**40**.

Under that flag the split shows up in one pair of lines, and it is the whole
classification in miniature:

```
CRATONVM_SYNTHETIC_NET_SOCKETS=1     compat        strict
java/net/Socket$SocketInputStream    5 bridge  →   5 bridge     (real image class)
javax/net/ssl/SSLSocketInputStream   4 stub    →   0            (no image class)
```

The plain-TCP branch already mints a real image class whose natives survive
strict; only the TLS branch does not. `java/net/Socket$SocketInputStream` cannot
be reused as the fallback as it stands: its natives resolve the stream through
`re1_socket_read_stream`, which looks the id up in `s2_registry().streams`, and
a rustls id is not in that map — the lookup fails with "Socket stream not
found". Serving the TLS branch from it means teaching that helper the
`RUSTLS_SOCK_ID_BASE` branch the SSL natives implement today, on a code path no
default run executes and no lane that cannot build can test. Recorded, not
attempted.

The reachable twins of these two sites are
`native-builtins/src/phases_late/ssl_security.rs`'s own
`try_alloc_concurrent_synthetic("javax/net/ssl/SSLSocketInputStream", 1)` /
`…OutputStream`, which is also where all eight of the natives are registered
(`--dump-native-registry`'s `registered_by` says so) and which is another lane's
file.

## 6. Out-of-file patch (not applied)

**None is needed for the changes in this branch.** Both landed changes are
inside `native-builtins/src/net_phase_e.rs`, and neither moves a field count, a
registration or a table:

* `classloading/src/class_manager.rs`'s
  `"com/sun/net/httpserver/HttpExchange$ResponseBody" => instance_fields(2)`
  entry is unchanged and still correct — the fallback does not allocate that
  class at all, it allocates a real `java.io.ByteArrayOutputStream`.
* `native-api/src/no_image_receiver.rs`'s tables are unchanged. Nothing here
  re-tags a native, so `BASELINE_SYNTHETIC_STUBS`, `stub_ratchet` and
  `bridge-ratchet.sh` cannot move in either direction.

## 7. What this record does not fix

* **`cratonvm/net/HttpBodyReplaySubscription`** (§4) — the second fatal strict
  defect in this file, left loud and recorded rather than guessed at.
* **The two `SSLSocket*Stream` sites** (§5), and their live twins in
  `phases_late/ssl_security.rs`.
* **Nothing here is rebuilt.** Every measurement above was taken on a binary
  predating both changes, so the evidence is for the *defects* and for the
  *classification*, never for the fixes. Three things a build should check
  first: that a `--jdk-only` `HttpServeProbe` prints `responseBody class =
  java.io.ByteArrayOutputStream` and `HUC body = hello:3`; that the same probe
  under `--real-jdk` still prints
  `com.sun.net.httpserver.HttpExchange$ResponseBody` with byte-identical bodies;
  and that `--jdk-only-report` on the Compatible run still holds exactly one
  `compatibility-class-requested` row for `HttpExchange$ResponseBody` and now
  **none** for `CratonVM$HttpServerLoop`.

### 7.1 Re-verified 2026-08-12 — both residuals are STILL OPEN, and both landed fixes are STILL PRESENT

Source-level re-read on this tree, because this campaign has fifteen records
claiming a patch was never applied when it was, and the inverse mistake is
cheaper to make than to notice:

* **§1's door fix is present** — `net_phase_e.rs`'s `re10_spawn_dispatcher` calls
  `ctx.ensure_vm_internal_class(HS_LOOP_CLASS, 1)` before the mint, with the W7-24
  reasoning on the site.
* **§3's fallback is present** — `re10_real_jdk_response_body` exists and is
  reached from the `Err` arm of the `getResponseBody` mint.
* **§4 is unchanged and still loud** — the `RE5_REPLAY_SUBSCRIPTION` mint is a
  bare `try_alloc_concurrent_synthetic(ctx, RE5_REPLAY_SUBSCRIPTION,
  RE5_SUB_NUM_FIELDS)?`, so `--jdk-only` still refuses it by name at
  `HttpClient.send`. **Do not "fix" this by allocating a `Flow.Subscription`**:
  the table in §4 shows every candidate either counts demand without delivering
  (a HANG in place of a loud refusal) or moves delivery off the caller's thread.
* **§5 is unchanged** — `register_re1_socket`'s two mints are still there
  (`net_phase_e.rs`), still behind the `io.real_net_sockets` early return that
  makes them unreachable in a default run, and their live twins are still in
  `phases_late/ssl_security.rs` with all eight natives registered there.
  **AMENDED later the same day: "their live twins are still in
  `phases_late/ssl_security.rs`" was the whole defect and this bullet reports it
  as an inventory item.** Those twins are `bridge` in both modes, so they ran
  under `--jdk-only`, minted a class no image declares, and took every TLS
  stream with them. A working-tree fix re-targets the receiver onto
  `sun/security/ssl/SSLSocketImpl$App{In,Out}putStream`; the `net_phase_e.rs`
  half of this bullet is still accurate and still open. Full correction in §5's
  header block.

**No fixture assertion was added for either residual, deliberately.** Both are
strict-only refusals whose current correct reading is a *failure*: an assertion
over §4 would be a scheduled RED, and §5 is unreachable without
`CRATONVM_SYNTHETIC_NET_SOCKETS`, which no `class_args` entry sets. The coverage
this lane did add is for a different record and a landed fix — see
W7-53-blocking-close-family.md, "Two of the thirteen shapes are now SCHEDULED".

## 8. How to re-take all of this

```sh
BIN=target/release/cratonvm.exe
JDK="C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot"

# gate 2, PER CLASS and PER MODE — one dump is not enough, the kinds differ
"$BIN" --real-jdk --dump-native-registry reg_compat.json --java-home "$JDK" -cp regression-suite/build RArraysMismatch
"$BIN" --jdk-only --dump-native-registry reg_strict.json --java-home "$JDK" -cp regression-suite/build RArraysMismatch
# and the flag that makes §5 exist at all
CRATONVM_SYNTHETIC_NET_SOCKETS=1 "$BIN" --jdk-only --dump-native-registry reg_strict_synsock.json ...
# fold on natives[] where class == <name>, comparing `kind` between the two.
# The control that the strict dump IS strict: counts.synthetic-stub == 0 there
# and 1,262 in Compatible, with counts.bridge 9,740 in both.

# which sites a run reaches — Compatible is the WIDER net
"$BIN" --real-jdk --jdk-only-report rep.json  --java-home "$JDK" -cp <probes> HttpServeProbe
"$BIN" --real-jdk --jdk-only-report rep2.json --java-home "$JDK" -cp <probes> RawServerBodyHandlerProbe
# fold on violations[].kind == "compatibility-class-requested", (class, requester)

# the guardrail, against the image the run used — necessary, NOT sufficient
"$JDK/bin/javap.exe" -p 'com.sun.net.httpserver.HttpExchange$ResponseBody'
"$JDK/bin/javap.exe" -p sun.net.httpserver.PlaceholderOutputStream

# and always the control: both probes run green on HotSpot 25
"$JDK/bin/java.exe" -cp <probes> HttpServeProbe
```

The two probes live in this record's runs, not in the tree
(`HttpServeProbe.java`, `RawServerBodyHandlerProbe.java`); the second exists
because serving from a raw `ServerSocket` is what separates the `BodyHandler`
verdict from the `HttpServer.start()` one.
`probes/HttpServerWildcardAddressProbe.java` is tracked and is the §1
reproducer.
