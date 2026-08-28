# A nested JNI call cleared the ENCLOSING native's thread context — FIXED

*(filed under its symptom until 2026-08-28 as "Java re-entering tcnative from
inside BoringSSL's verify callback loses the TLSv1.3 client certificate")*

## Status

**FIXED 2026-08-28.** The symptom the page was named for is real and was
reproduced exactly as described; the mechanism it proposed was not the
mechanism. It is not about tcnative, not about BoringSSL's state machine, not
about the `SSL*`, and not TLSv1.3-specific. It is this:

> `JniContextGuard::drop` cleared the JNI thread-local context unconditionally.
> A native that calls back into Java, whose Java calls another native, installs
> two guards — and the inner one's exit tore down the **outer** call's context.
> Every up-call the outer native made afterwards found no context and was
> answered as if the VM were detached: a null `jobject`, silently.

All three questions the page left under "What is NOT established" are answered
below, by measurement rather than by reading. Both of its live witnesses now
pass: its own probe's `@@REPRO` rows, and
`OpenSslEngineTest.mustCallResumeTrustedOnSessionResumption`, which is the arm
it identified as the one no workaround could reach.

## The bisection that found it

`probes/OpenSslTls13ReentryBisectProbe.java` (added here). One mutual-TLS
handshake per row; the ONLY thing that varies is what the client's trust
manager does on BoringSSL's verify callback before delegating. Every row is a
subset or superset of its neighbours, so the table is a bisection rather than a
list. A counting key manager rides along and records whether the client was ever
asked for a certificate at all.

`useTasks=true` is the control on every row: netty defers the callback out of
`SSL_do_handshake`, so the same Java runs with no native beneath it.

HotSpot 25: **26/26 PASS**. CratonVM before the fix:

| what the trust manager did on the callback | useTasks=false | useTasks=true | keyAsks |
|---|---|---|---|
| nothing (control) | PASS | PASS | 1 |
| `synchronized (engine) {}` — the monitor, no native | PASS | PASS | 1 |
| a Java field read on the engine — no native, no monitor | PASS | PASS | 1 |
| allocate 64 MiB of garbage | PASS | PASS | 1 |
| `System.gc()` | PASS | PASS | 1 |
| `SSL.getOptions(ssl)` alone | **FAIL** | PASS | **0** |
| `SSL.getCiphers(ssl)` alone | **FAIL** | PASS | **0** |
| `SSL.getVersion(ssl)` alone | **FAIL** | PASS | **0** |
| **`SSL.getLastErrorNumber()` — touches no `SSL*` at all** | **FAIL** | PASS | **0** |
| **`SSL.newMemBIO()` — no `SSL*`** | **FAIL** | PASS | **0** |
| **`SSL.getOptions(otherSsl)` — a DIFFERENT, idle `SSL*`** | **FAIL** | PASS | **0** |
| `engine.getEnabledCipherSuites()` | **FAIL** | PASS | **0** |
| `engine.getSSLParameters()` — the known reproducer | **FAIL** | PASS | **0** |

Read the three bold rows together and every hypothesis the page listed dies at
once. `getLastErrorNumber()` and `newMemBIO()` never touch an `SSL*`;
`getOptions` on an unrelated idle `SSL*` never touches *this* one. What all
seven failing rows share, and none of the five passing rows has, is exactly one
thing: **a second JNI native call while a JNI native is already on the stack.**

### Which re-entrant call breaks it — answered

Not one of them. *Any* of them. The page asked whether it was `SSL.getOptions`,
`SSL.getCiphers` or the bare `synchronized` block, and pre-committed to the
consequence: "a lock-ordering problem and a BoringSSL state-machine problem need
opposite fixes". It is neither. The monitor-only row passes, and a native that
BoringSSL's state machine cannot even see fails.

### Send side or receive side — answered, without a packet capture

`keyAsks` is the client key manager's `chooseEngineClientAlias` /
`getCertificateChain` count. It is **1 on every passing row and 0 on every
failing one**. The client is never *asked* for a certificate, so nothing was
lost in flight: BoringSSL's certificate callback reached no Java, the client
decided it had nothing to send, and the server correctly reported
`PEER_DID_NOT_RETURN_A_CERTIFICATE`. The loss is entirely on the decide-to-send
side, one callback earlier than the page assumed.

### Whether it is the `ClassId(0)` / stale-receiver family — answered: no

The `ALLOC` row allocates 64 MiB of garbage on the callback and the `GC` row
calls `System.gc()` outright. Both PASS. A collection during the callback — the
premise of the stale-receiver family — does not reproduce this.

## The defect

`vm/src/vm/vm_exec.rs`, the JNI dispatch arms:

```rust
impl Drop for JniContextGuard {
    fn drop(&mut self) {
        crate::native::jni::clear_jni_context();   // unconditional
        crate::native::jni::clear_jni_thread();    // unconditional
    }
}
```

The guard was introduced for a good reason and does that job correctly: it
replaced a manual set/clear pair that skipped the clear on an unwind, leaving a
dangling `*mut JvmThread` in TLS. What it did not account for is that a JNI
dispatch **nests**:

```text
  install (outer: SSL_do_handshake)
      -> tcnative calls Java (the verify callback)
          -> Java calls SSL.getOptions
              install (inner)
              DROP (inner)  ->  CLEAR      <-- the outer call's context, gone
      <- Java returns, BoringSSL continues inside the SAME outer native
      -> BoringSSL fires the CERTIFICATE callback
          -> tcnative's CallObjectMethod -> with_jni_context() -> None
```

`with_jni_context` returns `None` when either half of the context is missing,
and every JNI up-call is written to answer `None` as a null `jobject` or a zero
— which a host library reads as an ordinary answer, not an error. Nothing logs,
nothing throws, and the handshake completes on the client side. That silence is
why the page could reproduce the symptom for two days without reaching the
cause.

HotSpot is unaffected because a real JVM's `JNIEnv` is a permanent per-thread
structure, not something a call installs and removes.

## The fix

`JniContextGuard` now **saves and restores** instead of clearing:

```rust
unsafe fn install(shared: &SharedVm, thread: *mut JvmThread) -> Self {
    let prev_vm = crate::native::jni::replace_jni_context(shared);
    let prev_thread = crate::native::jni::replace_jni_thread(thread);
    JniContextGuard { prev_vm, prev_thread }
}
```

with `replace_jni_context` / `restore_jni_context` and
`replace_jni_thread` / `restore_jni_thread` added beside the existing
`set_`/`clear_` pair in `vm/src/native/jni.rs`.

This is a strict generalisation, not a weakening: at the **outermost** native
call the saved value is `None`/null, so its exit still drops the `Arc<SharedVm>`
and still nulls the thread pointer — the release property `clear_jni_context`'s
contract is about, and the unwind-safety property the guard was added for, are
both preserved. Only the nested case changes, and only from "wrong" to "right".

`set_jni_context` is **deleted**, not left beside the new pair. It was the
non-nesting install, the guard was its only production caller, and the
`no_test_only_public_api` ratchet caught it going test-only the moment the fix
landed — which is the right answer to "an item whose sole real caller is a
test": the plain install/clear pair IS the defect, and leaving it in the API is
an invitation to reintroduce it. `set_jni_context_arc` stays (the Invocation
API's entry, where there is no enclosing call to preserve) and so does
`clear_jni_context` (the foreign-thread detach path, which really is a
teardown).

Two instruments landed with it, because the failure mode here is silence:

* `vm/src/native/jni.rs` — `nested_native_call_restores_the_enclosing_jni_context`,
  a unit test that installs, nests, exits the inner call and asserts the outer
  context survives. A test that checks only the outermost exit — which the old
  code got right — cannot see this, which is why it is written as a nesting
  assertion.
* `JNI_UPCALLS_WITHOUT_CONTEXT`, a counter on `with_jni_context`'s `None` arm,
  reported by `CRATONVM_INTRINSIC_STATS=1` as
  `JNI up-calls answered with NO thread context: N`. It is a count and not a
  `warn!` because a genuinely detached host thread reaches that arm
  legitimately; for a run whose native calls all originate from Java the
  expected value is 0. This is the number that would have found the defect in
  an afternoon.

## Verification

One binary, Azure host 2, netty + netty-tcnative BoringSSL-static, JDK 25.

| instrument | before | after | HotSpot |
|---|---|---|---|
| `OpenSslTls13ReentryBisectProbe` | 7 of 26 rows FAIL | **26/26 PASS** | 26/26 PASS |
| `OpenSslTls13ClientCertProbe` `@@PROBE` | 0 failures | **0 failures** | 0 |
| `OpenSslTls13ClientCertProbe` `@@REPRO` | `rows_failed=1` | **`rows_failed=0`** | 0 |
| `OpenSslEngineTest#mustCallResumeTrustedOnSessionResumption` | 36 ok / **12 failed**, 860 s | **48 ok / 0 failed, 66 s** | 48/48, 17.7 s |

The witness's twelve failing invocations before the fix are
`10 12 14 16 26 28 30 32 42 44 46 48` — the same set the page recorded on two
earlier arms, decoding to `TLSv1.3` × `useTasks=false` across all three buffer
types and both `delegate`/`useTickets` values. Each burned the method's own 60 s
`@Timeout`, which is where the ~800 s went.

`@@REPRO rows_failed=0` is the bar the page itself set: *"`rows_failed=1` is
this VM today, `rows_failed=0` is HotSpot and is what a fix has to reach."*

## What this changes about the workaround in the tree

`x509_manager::extended_tm_identification_algorithm` reads netty's
`endpointIdentificationAlgorithm` field directly instead of calling
`engine.getSSLParameters()`. It is **no longer load-bearing** — the call it
avoids is safe again — and it is kept anyway, on its own merits: a field read
runs no Java, allocates nothing and enters no native, which is the right shape
for a function that exists to discover, in the common case, that there is
nothing to do. Its comment is updated to say that rather than to warn about a
hazard that is gone.

## Reach: the whole class, before and after, stopped at the same test

The witness above is one method. `OpenSslEngineTest` as a whole was run on both
binaries, same host, same argfile, same 3-hour cap — and both runs stop in the
same place, on `testSrcsLenOverFlowCorrectlyHandled` invocation #1, which hangs
identically before and after and is nothing to do with this defect. That makes
the comparison exact rather than approximate: both arms executed the same 2 022
invocations and then stopped.

| | before | after |
|---|---:|---:|
| invocations begun | 2 022 | 2 022 |
| ok | 1 963 | **2 020** |
| **failed** | **58** | **1** |
| `mustCallResumeTrustedOnSessionResumption` timeouts | 12 | **0** |
| `testMutualAuthSameCertChain` timeouts | 33 | **1** |
| bare `NullPointerException` | 0 | 0 |
| wall to reach invocation 2 022 | ~3 h | **~23 min** |

**57 of the 58 failures were this one defect**, across two methods rather than
the one the page had a witness for — and the class reaches the same point about
eight times faster, because each of those failures was a 30 s or 60 s JUnit
timeout being burned in full.

The single remaining failure is one `testMutualAuthSameCertChain` timeout out of
48 parameterisations, against 33 before. That is not claimed as fixed or as
anything else here; it is one row, and one row is not a rate.

`testSrcsLenOverFlowCorrectlyHandled` hanging is a separate open question this
page does not own: it is unaffected by the fix, it reproduces on both binaries,
and it is why no whole-class run of `OpenSslEngineTest` on this host has ever
finished.

## The NPE cluster the page set aside

The page's addendum recorded a second cluster in the same class — bare
`NullPointerException`s split roughly evenly between TLSv1.2 and TLSv1.3
(268 vs 266 in one sample) — and set it aside because *"this page's mechanism
... is TLSv1.3-specific by construction"*.

**That reasoning no longer applies.** The mechanism is "a nested JNI call
clears the outer context", which is protocol-independent: a cleared context
makes any up-call answer null on any protocol, so an even TLSv1.2/TLSv1.3 split
is what this defect predicts rather than evidence against it. The premise the
cluster was excluded on was the mechanism this page got wrong.

What the cluster **is** could not be settled here, and the reason is worth
recording rather than leaving as an implied result: **it does not reproduce on
Linux at all.** A whole-class run on Azure host 2 against the pre-fix binary,
capped at three hours, reached 2 022 invocations and recorded

```
ok=1963  failed=58     npe=0     timeout60=12     peer_no_cert=0
```

Zero bare `NullPointerException`s in 2 022 tests. The page's counts came from a
Windows complete-suite run, so the cluster is either Windows-specific or
specific to that run's conditions; either way there is nothing on this host for
a fix to remove, and claiming this fix removed it would be claiming a result
from an arm that never showed the symptom. The run also hit its cap rather than
finishing, so it says nothing about invocations past 2 022.

The 58 failures it did record are this page's own defect and its neighbours —
12 `mustCallResumeTrustedOnSessionResumption` timeouts, 33
`testMutualAuthSameCertChain` timeouts, and 84
`DecoderException: ReferenceCountedOpenSslEngine$OpenSslException` rows, which
is the shape a lost client certificate produces at the far end.

## Reach beyond netty

More generally, this was never a netty defect. Any host library that calls back
into Java, and whose Java touches that library again, hit it — JNA, jnr-ffi,
tcnative, a JNI-based codec. netty's OpenSSL provider with `useTasks=false` is
simply the shape that exercises it every handshake.

## Repro

```bash
cd apps/netty-suite-runner
./gen-openssl-args.sh -o /tmp/ossl.args     # BoringSSL-static; see the script's header
javac -d /tmp/cls -cp "$(sed -n 2p /tmp/ossl.args)" \
    ../../probes/OpenSslTls13ReentryBisectProbe.java
java @/tmp/ossl.args -cp "$(sed -n 2p /tmp/ossl.args):/tmp/cls" \
    OpenSslTls13ReentryBisectProbe          # HotSpot: failed=0
cratonvm --java-home <jdk25> --Xmx 1500m -XX:+UseG1GC \
    @/tmp/ossl.args -cp "$(sed -n 2p /tmp/ossl.args):/tmp/cls" \
    OpenSslTls13ReentryBisectProbe          # before: failed=7   after: failed=0
```

## Related files

- `vm/src/vm/vm_exec.rs` — `JniContextGuard`, both JNI dispatch arms and the `JNI_OnLoad` arm
- `vm/src/native/jni.rs` — `replace_jni_context` / `restore_jni_context`, `replace_jni_thread` / `restore_jni_thread`, `with_jni_context`, `JNI_UPCALLS_WITHOUT_CONTEXT`
- `probes/OpenSslTls13ReentryBisectProbe.java` — the bisection
- `probes/OpenSslTls13ClientCertProbe.java` — the page's original probe
- `apps/netty-suite-runner/MethodProgressRunner.java` — one method, every parameterisation, one line per invocation
- `native-builtins/src/x509_manager.rs` — `extended_tm_identification_algorithm`
- `ssl-parameterized-classes-exceed-180s-timeout-masking-real-failures-20260826.md` — the hostname-verification fix that first surfaced this
