# `ParameterizedSslHandlerTest` — the residual stall, and what it actually is

**Status: ONE residual left, OPEN — 2026-08-24.** The page opened with two
stalls that the `Object.wait()` lost-notify fix did not touch, each seen once,
neither explained, neither with a rate. Since then:

* **residual 2 is CLOSED, and was never a second stall.** Its distinguishing
  evidence — a `private volatile Object` holding `Int(0)` — was the watchdog
  dump mis-reading a never-written reference cell. Reproduced deterministically
  off netty and fixed in the dump. Read correctly, residual 2's stall reports
  exactly residual 1's state, so there was one residual, not two;
* **residual 1 is REPRODUCED on the current `dev`, and it is not a monitor, a
  promise, or a selector defect.** The server's TLS handshake dies on a
  `NoSuchMethodError` naming **`java.lang.Object`** as the receiver class, so
  no alert is produced and the thing that would complete the promise never
  runs. **A third catch, with the tracer armed, measures where the receiver
  goes stale — and it is NOT the JNI local-ref path this page asserted on
  2026-08-24. That claim is WITHDRAWN; see "The measured stale receiver"
  below.** The receiver reaches virtual dispatch naming an address the
  collector moved out from under it, `class_id_of` reads a dead base,
  `ClassId(0)` is `java.lang.Object`, and the dispatch raises
  `NoSuchMethodError`;
* a SECOND reproduction, on the same netty frame, is a different failure — a
  reactor that never returns from the tcnative `SSL_write` native at all,
  with the VM's own "STW … still waiting for cooperative mutators
  pending=1 taken=0" printed three times and zero times in the 18 runs that
  passed. That one is NOT yet attributed;
* the rate is measured, with a same-day HotSpot control on the same host:
  **1 in 163** whole-class runs, against this page's historical 5 in 80.

## The stall, end to end

One reproduction, `hl6` run 26, whole class, JUnit `@Timeout` disabled, VM
watchdog not armed (see "a watchdog dump is not a stall" below). In order:

```
WARN cratonvm_vm::vm::vm_exec: NoSuchMethodError
  method="java/lang/Object.checkClientTrusted([Ljava/security/cert/X509Certificate;Ljava/lang/String;)V"
  caller="io/netty/handler/codec/ByteToMessageDecoder.decodeRemovalReentryProtection(…) @pc=12"

[PSH] alert.server userEvent SslHandshakeCompletionEvent(
        java.lang.NoSuchMethodError: 'void java.lang.Object.checkClientTrusted(…)')
[PSH] alert.server exceptionCaught cause=java.lang.NoSuchMethodError
[PSH] alert.server userEvent SslCloseCompletionEvent(StacklessClosedChannelException)
[PSH] alert.client channelInactive promiseDone=false
[PSH] STUCK alert.promise#232 15797ms future.isDone=false
      ch=NioSocketChannel open=false active=false registered=false
      loop=33b375 inEventLoop=false shuttingDown=false pendingTasks=0
      ownerState=WAITING ownerAlive=true ownerInterrupted=false
```

`testAlertProducedAndSend` works by making the SERVER's `X509TrustManager`
throw a `CertificateException`, which the server's engine turns into a TLS
alert, which the client's engine turns into an `SSLException`, which the
client's `exceptionCaught` recognises and completes the test's promise with.
Every link after the first is conditional on the first.

Here the trust-manager call never reaches the test's `checkClientTrusted` at
all: dispatch resolves the receiver's class as **`java.lang.Object`** and
raises `NoSuchMethodError`. **One of its three known producers is now named
and fixed** (`native_properties_equals`, an unpinned native local); the other
two are not, and this page says which is which rather than generalising from
the one that was solved. That is a `LinkageError`, not the
`CertificateException` the engine is prepared to convert, so **no alert is
produced** — the server just closes. The client sees a plain close, its
`exceptionCaught` never fires, `promise.trySuccess(null)` is never reached,
and `promise.syncUninterruptibly()` waits forever.

Which is exactly what this page described, from the other end: a promise that
was never completed, with no notification due, and nothing wrong at the
monitor. Nothing here is a monitor, promise or selector defect.

### `java.lang.Object` as the receiver class is a named family in this tree

`vm_exec.rs`' own comment at the `NoSuchMethodError` terminal lists the three
faces of the `ClassId(0)` / H2-CID0 stale-receiver family, and the first is
`NoSuchMethodError java/lang/Object.<method>`: a reference slot that still
names an address the collector has since reclaimed or evacuated reads back an
all-zero header, and `ClassId(0)` is `java/lang/Object`.

The call is made from inside `SSL_do_handshake` — netty's OpenSSL provider
enters BoringSSL through tcnative, and BoringSSL calls back up into Java to
run the certificate verifier. So the thread is **inside a native call when the
callback re-enters Java**, which is the exact shape the H2-CID0 note describes:
*"a thread parked in a native publishes its frames exactly once
(`deposit_root_snapshot`) and is then invisible to every collector except
through that snapshot."*

**What is established:** the receiver's class resolved to `ClassId(0)`, and the
free-list verdict (`reclaim_guard::report_reclaimed_receiver`, which runs
flag-free at this terminal) did **not** fire — so the address is not a known
reclaimed hole. That leaves the evacuated-and-not-remapped face, which is the
one `CRATONVM_DBG_VACATED_FRAMES` exists to answer and which nothing has yet
asked here.

**What is NOT established, and must not be assumed:** that the missed slot
belongs to the JNI callback's own frames. That is the hypothesis the next run
tests, not a finding.

### The next step, and it is one run

Re-run the loop with the two tracers armed — `/data/nres/huntloop.sh` does
exactly this and stops on the first catch:

* `CRATONVM_DBG_CCE_BT=1` makes this dispatch miss dump the whole frame stack
  plus the receiver's address, blocked flag, collection epoch and mirror
  identity. That names the producing frame, which is the one thing the log
  above does not (`caller=` is the nearest interpreter frame, not the caller of
  `checkClientTrusted`);
* `CRATONVM_DBG_VACATED_FRAMES=1` asks the complementary question — was this
  address one the LAST collection moved an object away from, i.e. a frame slot
  the remap did not reach.

Both are terminal-path only, so a healthy run pays nothing for them.

## The measured stale receiver — and the claim this page withdraws

**WITHDRAWN, 2026-08-25.** An earlier revision of this page asserted the root
cause was `jni.rs::jobject_to_obj` handing back a stale LOCAL ref (it resolves
a global ref through a locked table and a local ref as *"a raw heap pointer"*,
and its own comment says such a ref can be stale from-space under a moving
collector). That is a true property of the code and it remains worth fixing,
but it was derived by READING the source, not by measuring, and the next catch
does not support it as the cause of this stall.

**What the next catch actually measured.** `hunt5` run 233, whole class,
`CRATONVM_DBG_CCE_BT=1` and `CRATONVM_DBG_VACATED_FRAMES=1` armed. Same
`NoSuchMethodError java/lang/Object.…` signature, a different method:

```
NoSuchMethodError method="java/lang/Object.address()J"
  caller="io/netty/util/internal/CleanerJava25.allocate(I)… @pc=11"

CCE-BT-STK[21] io/netty/util/internal/CleanerJava25.allocate            pc=11
CCE-BT-STK[20] io/netty/util/internal/PlatformDependent.allocateDirect  pc=40
…
CCE-BT-STK[7]  io/netty/buffer/AbstractByteBufAllocator.directBuffer    pc=7

NSME-RECV addr=0x20084404c50 tid=1847 blocked=false epoch=11
NSME-RECV SHAPE kind=Object num_fields=6 mirror_of=<not a registered mirror>
          [0]=Long(2202733641728) [1]=Long(131072) [2]=Object(…) [3]=Int(0)
```

Not a TLS path at all — netty's `CleanerJava25` calling `MemorySegment.address()`
on the FFM allocation path, through a MethodHandle. The receiver's fields are
a plausible `MemorySegment` (a base address, a 131 072-byte length, a scope),
so this is the intended object read at a dead address, not junk.

And the collector says so itself, on the same address, twice:

```
ERROR cratonvm::gc::guard: a dereference of an address that is NOT a live
  object base was swallowed into a default …
  site="kind_of" obj="0x20084404c50" moved_to="0x200849c12d0" was_vacated=true
```

`was_vacated` is the exact ledger — `gc_quiescence::note_allocated` removes
re-issued addresses, so a hit is proof rather than suspicion. The object was
moved to `0x200849c12d0` and the holder was never repaired.

### Why the read barrier did not save it

`VmHeap::load_and_forward` is the software read barrier, and `invoke_virtual`
calls it on every receiver before reading its class. On G1 it is a **no-op for
exactly this case**:

```rust
if !pre_validated && self.is_object_address(obj.as_ptr() as usize).is_none() {
    #[cfg(feature = "zgc")] … h.forwarded_after_slide(…) …   // ZGC only
    return (obj, false);                                     // G1: stale, unchanged
}
```

The barrier repairs by reading a forwarding word **at the old address**, which
requires the old address to still be a live object base. G1 frees the
from-region, and Phase 5 (`free_or_keep_cset`) deliberately zeroes
`forwarding_ptr` for a freed region, so there is nothing left to read. ZGC has
a relocation table (`forwarded_after_slide`) for precisely this; **G1 has no
equivalent**, so every caller that treats `load_and_forward` as the repair —
`invoke_virtual`, `invoke_virtual_bytecode_only`, `forward_boundary_args`, the
H2 natives — is unprotected under G1 for a reference that went stale before
the call. The function's own doc names ZGC as the unprotected collector; that
G1 is unprotected too, for a different reason, is new here.

### ANSWERED, and the holder is named: `native_properties_equals`

`hunt7` run 33 — the 600m arm, with `CRATONVM_DBG_GCPART` finally armed. The
run **passed** and still carried the defect, which is why the loop stops on the
`NoSuchMethodError` count rather than on a hang:

```
NoSuchMethodError method="java/lang/Object.entrySet()Ljava/util/Set;"
  caller="io/netty/handler/ssl/JdkSslContext.<init>(…)"

ERROR cratonvm::gc::guard: a dereference of an address that is NOT a live
  object base …  site="class_id_of" obj="0x2002c100180"
  moved_to="0x2002ba00260" was_vacated=true
    1: class_id_of                gc/src/vm_heap.rs:502
    2: invoke_virtual             vm/src/vm/vm_exec.rs:10647
    3: native_properties_equals   native-builtins/src/properties_sidetable.rs:3287
```

`properties_sidetable.rs` was **the only application-level holder in all
sixteen dead-base dereferences of that run**; every other frame in those
backtraces is downstream of it (`class_id_of`, `invoke_on_class_shared`, and
the NSME tracer itself re-reading the bad address).

`java.security.Provider extends Properties`, so `JdkSslContext.<init>` touching
a provider map reaches `Properties.equals` → this native → `entrySet()` on
`this`. The `entrySet` in the error and the holder in the backtrace are the
same call.

**The fork above resolves to the first branch.** `was_vacated` carries a
`moved_to`, so the forward WAS recorded; what failed is that the holder is a
raw Rust local in a native, which no `pointer_map`-keyed remap covers. The
`[gcpart]` ring reported `moved_to=None` for epochs 20–26 and
`appears_as_dest=true` at 25 — the move itself was epoch 27, the current cycle,
whose map is not in the ring yet.

### The fix

`native_properties_equals` already pinned `it`, `entry` and (late) `other`, and
its own comment said why. It did **not** pin:

* **`this`** — read from `args[0]` and used at `entrySet()` AFTER two
  `invoke_virtual` calls that can allocate and collect. This is the one the
  capture caught;
* **`other`** — pinned only at the loop head, so the `size()` call before that
  was unprotected;
* **`key`** — from `getKey()`, passed to `get()` AFTER `getValue()`;
* **`value`** — from `getValue()`, used as a receiver AFTER `get()`.

All four now use the tree's own idiom (`pin_native_root` / `read_native_pin`,
~4900 uses), with the per-iteration pins released at the bottom of the loop so
a large map does not grow `native_pin_roots` by three entries per entry. This
is an instance the `unpinned-native-locals` audit's search did not reach.

`cargo test --release`: native-builtins **4167 passed, 0 failed**; vm **2623
passed, 0 failed**.

**The A/B is IN FLIGHT and this page will not claim the stall closed until it
reads.** Two binaries — the one that caught and the same tree plus the fix —
interleaved run by run at the 600m heap the catch came from, because this
host's load average has swung 6–148 today and a sequential before/after here
would be comparing two machines. The metric is the `NoSuchMethodError
java/lang/Object` count and the number of guard events naming
`properties_sidetable`, NOT the raw `was_vacated` line count: that counts every
stale-reference reporter in the process, from several unrelated holders, and
each reporter caps itself at 12. Three PRE runs and three POST runs differed on
it while naming completely different holders — noise dressed as signal, and it
is written down here because it nearly went into this page as evidence.

On the multi-face evidence above, this A/B should not be expected to close the
stall outright either — only to remove one of three producers.

First 21 runs (11 unfixed / 10 fixed, interleaved, 600m):

| arm | runs | `NoSuchMethodError java/lang/Object` | guard frames naming `properties_sidetable` |
|---|---:|---:|---:|
| unfixed | 11 | 1 | **0** |
| fixed | 10 | 0 | **0** |

**That is not yet evidence for the fix, and it would be easy to present as if
it were.** The one catch in the unfixed arm is the `checkClientTrusted` face —
a producer this fix does not touch — and the fixed arm's zero is one run of a
1-in-33 event. More to the point, `properties_sidetable` appears in NEITHER
arm's backtraces over those 21 runs, so the site the fix changes did not fire
at all: the two arms cannot have differed because of it. The run that named
that site (`hunt7` run 33) remains the only observation of that face.

What this A/B can eventually show is a difference in the total
`NoSuchMethodError java/lang/Object` rate across enough runs to see a 1-in-33
event move. Until then the fix stands on the capture that named its holder and
on the code being wrong on its own terms — a raw `ObjectRef` live across a call
that allocates — not on this table.

### The `checkClientTrusted` face: holder named, and the JNI hypothesis is dead

`hunt9`'s predecessor `hunt8` caught at run 5 with `CRATONVM_DBG=jni-localref`
armed — the instrument built specifically to test whether this face arrives
through a stale JNI local ref. **It does not.** The audit fired 24 times and
named real JNI entry points (`jni_get_direct_buffer_address`,
`jni_new_object_a/v`, `jni_new_global_ref`), and **none of them is the failing
receiver.** That receiver's holder is a CratonVM native:

```
site="class_id_of" obj=0x2002344e480 moved_to="0x2002b0261c0" was_vacated=true
  1: class_id_of                     gc/src/vm_heap.rs:502
  2: tm_is_extended                  native-builtins/src/t27_tls.rs:15025
  3: engine_consult_trust_managers   native-builtins/src/t27_tls.rs:14822
  4: engine_run_trust_check          native-builtins/src/t27_tls.rs:14679
  5: do_unwrap                       native-builtins/src/t27_tls.rs:17843
  6: unwrap_single                   native-builtins/src/t27_tls.rs:17073
```

So the JNI-local-ref hypothesis — asserted from source-reading, withdrawn on
one face's evidence, revived as "live again for THIS face" — is now **refuted
by measurement on the face it was revived for**. The local-ref hazard in
`jobject_to_obj` is real and still worth fixing on its own terms; it is not
what stalls this test. Three assertions, one measurement: the measurement wins.

### And this one is NOT an unpinned local — the PIN was stale

That distinction matters, because it is a different and worse defect.
`engine_consult_trust_managers` does the right thing already:

```rust
let tm_pins: Vec<usize> = trust_managers.iter().map(|tm| ctx.pin_native_root(*tm)).collect();
…
let tm_now = ctx.read_native_pin(tm_pins[i], trust_managers[i]);
let engine_now = engine_pin.map(|(pin, e)| ctx.read_native_pin(pin, e));   // no allocation
match engine_now { Some(engine) if tm_is_extended(ctx, tm_now) => …
```

`tm_now` is re-derived from its pin on the line before the read that faulted,
and nothing between them can collect. `tm_is_extended` is clean too — its
first statement is the `class_id_of_object` that faulted. So the value the pin
HANDED BACK was already stale: this is not "a native forgot to pin", it is
"the pin did not hold".

### There is now a SECONDS-scale reproducer, off netty

`probes/TmChurnProbe.java` drives the caught native directly: in-memory
`SSLEngine` handshake pairs with a custom `X509TrustManager` on the server (so
`engine_consult_trust_managers` has managers to consult) and peer threads
allocating hard, so a collection can land in the window. No sockets, no event
loops, no netty class.

| arm | heap | peers | result |
|---|---|---|---|
| stress | 600m | 2 | **8 catches** of `NoSuchMethodError java/lang/Object.checkClientTrusted(…)` |
| control | 4g | 0 | **0 in 2165 handshakes** |

Same binary, so the difference is the collection and not the code path. That
turns a 1-in-20-to-1-in-230 whole-class hunt into a ~50/50 coin flip per
process launch, which is the difference between a hand-off and a debugging
loop.

**It is BIMODAL, and that is a finding of its own.** A launch either catches
within the first handful of handshakes or runs ~1200 clean and never catches.
So it is a STARTUP-window race — class loading, JIT warm-up and the first heap
growth — not a steady-state one. Quote launches caught, never handshakes.

### The PIN-STALE canary was blind, and fixing it named the producer

`pin_native_root` already carried a canary for "this caller pinned an address
that was ALREADY stale" — which a pin cannot repair, since no later
`pointer_map` holds a long-dead key. It asked `debug_forwarded_target`, which
reads the forwarding word AT THE OLD ADDRESS, and G1 zeroes that word when it
frees the from-region. **Same blind spot as `load_and_forward`.** So its zero
meant "nothing stale was pinned whose from-region happened to survive", and
was about to be read as the strong claim.

It now also asks `gc_quiescence::was_vacated`, the exact ledger. It
immediately fired — 6 hits, its report cap, on the catching run — and named
the producer:

```
[blockgc] PIN-STALE tid=6 0x2002f949818->0x20030449c48
          class=sun/security/ssl/SSLEngineImpl caller:
    3: engine_consult_trust_managers   t27_tls.rs:14781
    4: engine_run_trust_check          t27_tls.rs:14679
```

**The engine reference is already dead when it is pinned.** That explains why
every downstream instrument reads clean — PIN-DANGLING 0, DISCARDS 0, the
`[gcpart]` ring holding the forward, no STW give-up: nothing downstream is
broken. The staleness arrives from upstream.

### Three pin fixes in that file, and none of them moved the rate

Applied in order, each on the reading that the previous capture supported:

1. `do_wrap`: pin `this`/`dst` across the trust-manager callback;
2. `do_unwrap`: pin `this`/`src` and every element of `dsts` across it;
3. both: move those pins to FUNCTION ENTRY, before the first `ctx` call,
   because a pin taken mid-body pins what `this` has already decayed to.

Measured on the reproducer, six launches each, same flags:

| binary | launches caught |
|---|---|
| before the entry-pin move | 3 / 6 |
| after it | 4 / 6 |

**No improvement.** The pins are correct on their own terms — a raw
`ObjectRef` live across a call that allocates is a defect whether or not it is
this one — and they are kept for that reason, but they are NOT this stall's
fix and this page does not present them as one. `this` is already stale when
`do_unwrap` receives it, so the producer is above these functions: the native
argument handling, or `unwrap_single`/`wrap_single`. That is where the next
capture should look, and `PIN-STALE` now points at it.

### ANSWERED: `native_pin_roots` entries miss remaps

There were two canaries around the pin table, answering opposite questions —
`PIN-STALE` at pin time ("the caller handed us an already-dead address") and
`PIN-TABLE-STALE` at read time ("the table entry missed a remap"). Both asked
`debug_forwarded_target`, which reads the forwarding word AT THE OLD ADDRESS,
and G1 zeroes that when it frees the from-region. **Both were blind, and their
silence was being read as information.** Corrected to also consult
`gc_quiescence::was_vacated` — the exact ledger — and both fire immediately:

```
[blockgc] PIN-TABLE-STALE tid=5 handle=11 len=13 0x2001db77e00->0x20030455730
          (pin-table entry missed a remap) reader:
    1: {closure#5}                      t27_tls.rs:14820
    3: engine_consult_trust_managers    t27_tls.rs:14820
    4: engine_run_trust_check           t27_tls.rs:14679
    5: do_wrap
```

`handle=11 len=13` — the handle is **well in range**, and the slot still names
an address the ledger says was moved. Five occurrences across three threads in
one launch (tid 3 handle=4/len=5, tid 5 handle=11/len=13, tid 6 handles
12,13,18 of len 14,14,19), all in range.

**So the pin table itself is not reliably remapped**, and this reframes
everything above it:

* the natives were doing the right thing. `engine_consult_trust_managers`
  pins and re-derives correctly; so, after this page's fixes, do `do_wrap` and
  `do_unwrap`. They read stale values from a table that was not updated;
* which is exactly why three pin fixes moved the rate from 3/6 to 4/6 —
  **no fix in native code can repair this**, and that null result was the
  clue rather than a failure;
* and it means `PIN-STALE`'s "the caller pinned an already-dead address" is
  the SAME defect one call earlier: the address the caller held came out of a
  pin table that had already missed a remap.

### The refutation this overturns, and the lesson in it

An earlier revision of this page struck out "the pin slot was never remapped"
as **REFUTED by inspection**, on the grounds that all three remap paths —
`update_all_roots`, `check_post_block_gc_refs`, `apply_pointer_map_to_thread`
— demonstrably iterate `native_pin_roots`. Every word of that inspection was
correct and the conclusion was wrong.

**Reading that a remap EXISTS is not evidence that it RAN.** The measurement
says entries with in-range handles still hold pre-move addresses, so for those
collections none of the three paths reached that thread's table. Which path,
and why, is the open question — but it is now a question about remap coverage,
not about native code, and it has an instrument that answers in seconds.

### What to do next

`PIN-TABLE-STALE` fires on the reproducer within one launch, so the next step
is to make it say WHICH collection was missed: stamp each pin-table entry with
the collection count at pin time and print that beside the epoch at read time.
The gap between the two names the collection whose remap did not reach this
thread, and from there the path is a matter of reading one code path rather
than four.

### Four ways that can happen, and where each one stands

1. **The pin slot was never remapped** — **CONFIRMED by measurement, after
   being wrongly refuted by inspection.** `PIN-TABLE-STALE` (once given the
   exact ledger) reports table entries with IN-RANGE handles still holding
   pre-move addresses, five times across three threads in one launch. See
   "ANSWERED" above.

   The inspection that struck this out was accurate about the code and wrong
   about the world: there ARE three paths that iterate `native_pin_roots` —
   `update_all_roots` (the thread RUNNING the collection),
   `check_post_block_gc_refs` (a blocked-region wake) and
   `apply_pointer_map_to_thread` (a running mutator a PEER's STW stopped at an
   interpreter safepoint). All three exist. For the collections that produce
   this defect, none of them reached the affected thread's table. **Which one
   should have, and why it did not, is the open question.**
2. **The forward was never recorded**, so no remap could have applied.
   A real instance of this shape WAS found — the CAS-loser arm, below — and
   its engagement on this workload measured **0**. So the shape exists in the
   collector and is now closed, but it is not this stall.
3. **Pin-stack imbalance.** `read_native_pin` silently falls back to the RAW
   `fallback` address when its handle is past the pin stack — a callee
   truncated below this caller's pins. The tree already has the diagnostic
   (`PIN-DANGLING`, plus a ring naming the truncator) behind
   `CRATONVM_DBG=blockgc` / `unpin-ring`. **0 hits across 78 armed
   whole-class runs** — all of which PASSED, so this is not yet a negative for
   a stalling run. Inspection agrees so far: every `pin_base` in `vm/src` is
   `native_pin_roots.len()` captured at entry, which is the correct
   discipline.
4. **A discarded fixup at native-unblock.** A thread that a peer's collection
   stopped while it was inside a blocking native cannot have its `JvmThread`
   touched by the collector, so the map is folded into the shared
   `gc_block_state.fixup` instead. `check_post_block_gc_refs` APPLIES that
   fixup (including to `native_pin_roots`); `mark_native_thread_unblocked`
   CLEARS it, with a debug line that says so outright —
   `"[blockgc] native-unblock DISCARDS {} fixups"`. A thread leaving through
   `VmNativeThreadBlocker::leave_blocked` takes the second path.
   **0 DISCARDS lines across the same 78 armed runs**, i.e. the fixup was
   always already empty there — so on healthy runs `check_post_block_gc`
   drains it first, as intended. Same caveat: no stalling run has been caught
   with this armed.

So of four, one is refuted by inspection, one is found-and-closed but
measured inert on this workload, and two are unmeasured ON A STALLING RUN
while reading zero on 78 healthy ones. What is established is the holder and
that its pin did not hold; which mechanism explains it is still open, and the
page does not pick.

The next catch decides it: `huntloop.sh` arms `PIN-DANGLING`, the
`DISCARDS` line, the `[gcpart]` ring and the CAS-loser counter together, so a
single stalling run answers all four at once.

### A second unrecorded-forward hole, found by inspection — and its engagement is ZERO

Mechanism 2 above ("the forward was never recorded") turns out to have a real
instance, and it is the same shape as one already fixed. `evacuate`'s parallel
path can return a forwarding address three ways, and only two of them recorded
it in `forwards` — which becomes the cycle's `pointer_map`:

| path | recorded? |
|---|---|
| fast path: already forwarded when we looked | yes — DEFECT-2 part 1 |
| we won the CAS and copied | yes |
| **we LOST the CAS to another worker** | **no** |

The third arm returned the winner's target and dropped `old_ptr -> target`,
which is verbatim what DEFECT-2 part 1 fixed for the first — and that fix's own
comment names the symptom it leaves: *"no root naming `old_ptr` could be
remapped … it dangled when the region was reused (the rare
`java/lang/Object`)."* It also has the right shape for the load dependence this
page has recorded since 2026-08-24 and never explained: losing that CAS needs
TWO WORKERS RACING ON ONE OBJECT, so it gets commoner exactly as parallelism
rises.

That is a good story, and **the counter shipped with the fix refuses it.**
`CRATONVM_DBG=jit-method-stats` prints the arm's hit count at exit:

```
[cratonvm] G1 evacuation CAS losses (forwards this VM would have dropped
           before the 2026-08-26 fix): 0
```

**Zero, on a full clean whole-class run at 600m** — 63 tests, the same
configuration the catches come from. So the hole is real and worth closing, but
on this workload the arm is not reached and it **cannot be what stalls this
test**. The fix is landed as a latent defect, not as this page's answer.

What the zero does NOT settle: a clean run is not a stalling one, and the arm
needs the race that load produces. `huntloop.sh` now prints the counter on
every run, so the next CATCH carries its own answer for the run that failed.

### The fix A/B is INCONCLUSIVE, and the reason is worth more than the table

Second attempt, 61 unfixed / 61 fixed runs interleaved at 600m:

| arm | runs | `NoSuchMethodError java/lang/Object` |
|---|---:|---:|
| unfixed | 61 | **0** |
| fixed | 61 | **0** |

**Neither arm reproduced the defect at all**, so the comparison is empty — this
is not "the fix worked". Set against the same week's other numbers, the event
rate is not a rate at all: 1 run in 5 (`hunt8`), 1 in 11 (`fixab1`'s unfixed
arm), 1 in 33 (`hunt7`), 1 in ~230 (`hunt5`), and now 0 in 122. The runs that
caught were at load 20-27; these 122 ran mostly at load 3-7.

Two consequences, both of which cost time here:

* **an A/B on this event cannot work while the rate swings this far.** Sample
  sizes that would settle a 1-in-33 event say nothing about a 1-in-230 one,
  and the arms cannot be held at a fixed rate because the rate is the host's;
* **the binary is not the dominant variable.** `hunt8` caught in FIVE runs on a
  binary carrying both pin fixes, while the unfixed arm above caught nothing in
  61. Any story of the form "the fixed binary is cleaner" has to survive that,
  and this one does not.

So the two pin fixes stand on the captures that named their holders and on the
code being wrong on its own terms — a raw `ObjectRef` live across a call that
allocates — and this page does not offer the A/B as evidence for either.

### Other holders the same captures name, not yet investigated

The stripped backtraces of those runs also name, repeatedly and outside the
reporter's own frames: `native-collections/src/lib.rs:15086`, `:16335`,
`:16577`, and several interpreter sites. Whether those are holders of the same
shape or ordinary frames on the path has NOT been checked — they are recorded
so the next pass has a list rather than a hunt.

### The question that decided the fix

### The one question that decides the fix, and it is not yet answered

The remaining fork is whether that forward reached `pointer_map`:

* **in the map** — the forward WAS recorded, and some holding slot missed the
  remap. `update_thread_objs_after_gc` remaps `native_pin_roots`, handle
  slots, `native_pending_return` and the JIT caches, all keyed on
  `pointer_map`; the fix would be at whichever slot is not in that list;
* **not in the map** — the forward was never recorded, no remap could have
  fixed any slot, and the fix is in G1's recording. `evacuate`'s parallel
  fast path had exactly this hole and it was fixed as DEFECT-2 part 1 (its
  comment names "the rare `java/lang/Object`" as the symptom); this binary
  carries that fix, so a hit here would be a SECOND hole.

**Those need opposite changes and must not be guessed between.**
`gcpart_probe` answers it in one line and the tracer already calls it — the
ring is simply never populated unless `CRATONVM_DBG_GCPART` is set, which is
why the catch above printed no `[gcpart]` lines. It is armed in `huntloop.sh`
now.

### The isolated reproducer does NOT reproduce — recorded as a negative

Catching this in netty costs about five hours a hit (1 whole-class run in
100-230), which is not a loop anyone can iterate a fix in, so
`probes/MhStaleReceiverProbe.java` tries to build the shape directly: a
six-field receiver constructed immediately before the call, invoked **through a
MethodHandle** (so it routes through `mh_dispatch` rather than an ordinary
`invokevirtual`), with heavy allocation in the window between the two.

| arm | rounds | heap | result |
|---|---:|---|---|
| single-threaded | 1 500 000 | 256m | 0 caught, **0** `was_vacated` events |
| 6 workers + a dedicated GC-pressure thread | 2 000 000 | 256m | 0 caught, **0** `was_vacated` events |

Both with `CRATONVM_DBG_VACATED_FRAMES`, `CRATONVM_DBG_GCPART` and
`CRATONVM_DBG_CCE_BT` armed. **Zero** vacated-reference events means the ledger
never even saw a stale reference, not merely that no dispatch failed — so the
three ingredients this probe has are NOT sufficient:

* a MethodHandle virtual invoke through `mh_dispatch`;
* a receiver allocated immediately before the call;
* young evacuations forced by a PEER thread rather than by the caller.

Whatever else the netty path contributes — JIT-compiled callers around the
dispatch, the FFM/`Arena` allocation itself, the `invokedynamic` bridge, a
deopt in the window — is load-bearing, and the next probe should add those
rather than repeat these. Until then the netty loop is the only capture, and it
is what `huntloop.sh` runs.

### Where this belongs

This is the `ClassId(0)` / stale-receiver family, not a netty defect:
the retired `unpinned-native-locals-audit` write-up (whose 48 fixes landed on
`dev` on 2026-08-25, while this was in flight) describes the same
signature (*"the next read sees an all-zero header — which the class manager
names `java.lang.Object`"*) and fixes the in-tree native instances with
`pin_native_root` / `read_native_pin`. The catch above is a witness that the
family survives that idiom under G1, because the idiom's re-read
(`read_native_pin`) is keyed on the same `pointer_map` remap.

## The SECOND reproduction is a DIFFERENT shape, on the same netty frame

`hunt2` run 9, `testCompositeBufSizeEstimation…` invocation #4, load average
141. `PshProbe` reports `composite.donePromise#88` outstanding — and this time
the client channel is **open, active and registered**, `pendingTasks=0`, so
nothing closed and nothing is queued. Data simply stopped.

The reactor stacks say why. One reactor is frozen at the SAME frame at
152 969 ms and again at 447 579 ms — five minutes apart, `RUNNABLE`:

```
at io.netty.handler.ssl.ReferenceCountedOpenSslEngine.writePlaintextData(…:628)
at io.netty.handler.ssl.ReferenceCountedOpenSslEngine.wrap(…)
at io.netty.handler.ssl.SslHandler.wrap(…)
at io.netty.handler.codec.ByteToMessageDecoder.decodeRemovalReentryProtection(…:545)
at io.netty.handler.codec.ByteToMessageDecoder.callDecode(…:484)
```

`writePlaintextData` line 628 is `SSL.writeToSSL(…)` — a genuine tcnative JNI
native in the BoringSSL `.so`; this VM registers no shim for it
(`grep -r writeToSSL --include=*.rs` finds nothing). So the thread is in C.

And beside it, three times in that run's log:

```
WARN cratonvm_vm::runtime::interpreter::gc_and_alloc:
  STW cross-thread JIT takeover is still waiting for cooperative mutators
  rounds=64 pending=1 taken=0
```

**Zero occurrences in the 18 runs that passed, three in the one that hung.**
The `[gcbarrier-tripwire]` beside that warning — which catches a thread that
entered the blocked region without depositing a root snapshot — did NOT fire,
so the pending thread is a genuinely COUNTED running mutator that never
reaches a safepoint, not a mis-accounted blocked one.

Note that both reproductions are on the same netty frame:
`ByteToMessageDecoder.decodeRemovalReentryProtection` re-entering the
BoringSSL boundary. The alert-test hang comes back from that boundary with a
receiver whose class reads `ClassId(0)`; the composite-test hang does not come
back at all.

**What is NOT established:** that the frozen reactor is the `pending=1`
mutator, or that either is the cause rather than a consequence of the other.
`pending=1` is a count with no subject until `CRATONVM_DBG_STW_CENSUS=1` names
it, and that flag was not armed on this run. It is armed in `huntloop.sh` now,
along with a stop condition on the STW warning itself — which, at 0-in-18
against 3-in-1, is a better trigger than the hang.

## The rate, measured — which this page could not do

One binary (`fix/psh-residual-stalls-20260824`, from `origin/dev` `2e9286dde`),
one host, one afternoon, JUnit's `@Timeout` **disabled** so a hang stays a hang.

| arm | runs | hangs |
|---|---:|---:|
| CratonVM, whole class | 163 | **1** |
| CratonVM, `testCompositeBufSizeEstimation…` alone | 60 | 0 |
| HotSpot 25, whole class (control) | 40 | 0 |

Host load average over these runs ranged 18–148 — the same band this page's own
rates were taken in (it records 4/20 at load 12–84 and 1/30 at 6–13). 40 of the
163 CratonVM runs used an argfile WITHOUT the instrumented copy of the test
class, so the instrument is not what suppressed the rate.

**1 in 163 (0.6%) against this page's historical 5 in 80 (6.25%).** The
monitor fix that closed the first stall is on this binary and was not on most
of the historical runs, which is the obvious explanation for most of that gap;
what is left is this defect, and it is a tenth as frequent, which is why
catching it a second time needs a loop rather than a run.

### A watchdog dump is not a stall — the trap that cost three attempts

`--stack-dump-on-timeout` fires on a fixed wall-clock deadline, and this host
reached load average 148 while these loops ran. Two runs tripped a 280 s and
then a 420 s deadline and were logged STALL. **Neither was one:**
`[WAIT-CENSUS]` reported `waited_ms=6` in the first — a healthy mid-test wait —
and no waiter at all in the second, and tests were still finishing every 30 s
in both. One CratonVM run passed cleanly in **667 s**.

Three columns settle it, all already in the log: the census's `waited_ms` (a
real stall reads 348 859; a healthy wait reads single digits), whether ONE
netty operation stayed outstanding for minutes, and whether tests were still
completing. `/data/nres/triage.sh` prints them.

So the loops behind the table do not arm the VM watchdog at all. `PshProbe`
halts the JVM with exit 97 once one netty operation has been outstanding for
two minutes, after printing that operation's state and every reactor's stack.
A hang costs two minutes, a slow run costs whatever it costs, and "the loop
found nothing" means something.

## Residual 2 — `DefaultChannelPromise.result` holding `Int(0)` — CLOSED

This page asked two questions and forbade assuming either answer: is the
`Int(0)` the CAUSE or an artefact of a mis-resolved field index, and are the
run's 34 `primitive-into-reference` guard hits on THIS field?

### It is neither. It is a never-written reference cell, and the dump was the one reader in the VM that did not know

`Value` is `#[repr(u32)]` with `Int = 0` and `Object = 4`
(`types/src/value.rs`), so a zero-filled 16-byte cell decodes as
`Value::Int(0)` and **not** as `Object(None)`. The interpreter's
`init_primitive_fields` has written the `Object(None)` tag into every reference
field since G56-1 (2026-08-17) — its doc comment carries the table — but the
JIT's allocation arms do not: `jit_post_alloc_init`'s per-class recipe stores
only `PrimKind::{Int,Long,Float,Double}`, `jit_init_primitive_fields`'
`_ => None` skips `L`/`[`, and the inline-TLAB arm deliberately relies on the
zero fill (`jit_new_site_flags` counts only `J`/`F`/`D` as needing init).

Every OTHER reader repairs the tag locally — the interpreter's `getfield`
fixup, the inline read's payload-only load, `coerce_field_value_for_slot`, and
`values_equal_for_cas`, which equates `Object(None)` and `Int(0)` in **both**
directions so netty's `RESULT_UPDATER.compareAndSet(this, null, …)` still
succeeds against such a cell. The watchdog dump did not, and reported
`result_is=not-a-reference-slot`, which reads as heap corruption.

**Reproduced without netty, interleaved, on the shipped binary.**
`probes/JitZeroCellProbe.java` allocates a two-field promise-shaped object from
a hot method 400 000 times, never writes its reference field, and parks on the
last one so the watchdog dumps it:

| arm | runs | `result` cell | `keep.result == null`, read in Java |
|---|---:|---|---|
| JIT (default flags) | 5 | `Int(0)` in **2**, `Object(None)` in 3 | **true in all 5** |
| `--nojit` | 5 | `Object(None)` in **5** | **true in all 5** |

`--nojit` never produces it; with the JIT on it is a coin toss, because what
decides the tag is which allocation arm served that particular allocation, and
that is a compile-timing outcome. **That intermittency is the netty
observation** — one stall showing `Int(0)` where others showed a proper
`Object`, on the same field of the same class.

The last column settles this page's question: Java reads the cell as `null` in
every run of both arms. The value is real, the field index was right (the dump
now prints it, with the receiver's whole declared layout beside it), nothing
was corrupt, and the promise in run 8 was **PENDING** — residual 1's state.

(An earlier pass of this A/B ran each arm once and would have gone into this
page as "the compact field layout is the discriminator". It is not — the
default, compact-enabled arm produces `Int(0)` two runs in five. One run per
arm was not a measurement.)

### The 34 guard hits

Same population, and not an attribution. The guard fires on a
descriptor-aware READ of a reference field whose cell still holds the
allocator's zero; `init_primitive_fields`' own G56-1 note measured 1 105 of
1 120 coercion events as exactly that shape on a `--jdk-only` run, before the
interpreter half was fixed. A count of them says how many JIT-allocated
reference fields were read before first write. It cannot name a field, and this
page was right to refuse to read it as if it could.

### What changed, and what deliberately did not

`dump_wait_object_state` now resolves `result` through
`resolve_declared_instance_field` and prints the **slot index, the declared
descriptor and the declaring class**, plus the receiver's complete instance
layout — so "the dump read the wrong slot" became a claim a reader can check
rather than one they must trust. It normalises a non-`Object` value at a slot
declared `L`/`[` to `Object(None)` before classifying, exactly as the VM's
other four readers do, printing both readings and naming which one the verdict
is about. And it answers `no-result-field(NOTHING-WAS-READ)` rather than
`not-a-reference-slot` when the receiver has no `result` field at all — the
same class of mislabel, found by the positive control below.

The allocator asymmetry is left alone, on purpose. Making the JIT write the
`Object(None)` tag would add a store per reference instance field to the
allocation path and would disable the inline-TLAB no-call arm for essentially
every class, for **zero** behavioural change — the table above is the evidence
for "zero".

## What the archived stalls said, once the census question was asked of them

This page's own next step for residual 1 was: *"at stall time, dump every
thread parked in `Object.wait()`… the raw material may be on disk."* It was.

**`OFF 19` — `testAlertProducedAndSend`, the `result_is=null(PENDING)` stall.**
Of 260 registered threads, **four were alive**: `main`, parked in
`DefaultPromise.awaitUninterruptibly` on the test's own promise, and three
reactors, all `blocked=true deposit=live` at `NioIoHandler.select@136`. **No
other thread was in `Object.wait()` at all** — so "the thread that would have
completed the promise is itself parked" is refuted for that stall, and the
completer was not blocked on a monitor either. Its selector census: 16 open
selectors, and **exactly one registered key in the whole process** — the
listener, `interest=0x10` (`OP_ACCEPT`), `ready=0x0`. Both data channels were
already closed and deregistered.

**`run 8` — `testCompositeBufSizeEstimation…`, the `Int(0)` stall.** Same
selector shape, but the reactors are NOT parked: `in_flight_selects=0` on every
selector, all three reactors `blocked=false` with no live dump. And the awaited
object is a `DefaultChannelPromise`, i.e. a **close** future rather than the
test's own promise — so the composite test's failure is reached by a different
route than the alert test's, and only the alert route has been caught with the
proximate cause in hand.

## The instruments — what a returning stall prints

### `[WAIT-CENSUS]` — every thread parked in `Object.wait()`

`ThreadRegistry::waiting_monitor_census` reports every registered thread whose
`jmx_waiting_monitor` slot is set — the same GC-forwarded root the wait-site
dump already resolves through, so every row is sound under a moving collector,
unlike `Monitor::wait`'s own entry-time local.
`SharedVm::dump_object_wait_census` prints one `[WAIT-OBJECT]` block per row
from the watchdog, right after the thread summary.

An EMPTY census is printed as a result rather than omitted: it means no thread
in the process is inside `Object.wait()`, so the stall is parked on something
else and the whole `Object.wait()` line of enquiry is the wrong one. That line
has already earned its keep — it is how one of the false watchdog fires above
was recognised.

Positive control, `probes/WaitCensusProbe.java`: two threads park on two
different objects, one carrying a `result` field and one not. Both rows appear;
the slot-provenance line resolves on the first and reports `UNRESOLVED` on the
second. Unit-tested in `thread_registry`
(`waiting_monitor_census_lists_every_waiter_and_only_waiters`), including that
a thread which LEAVES the wait drops out — a high-water mark here would
manufacture the second reading.

### `kernel=` — the KERNEL's readiness for every registered socket

The selector census printed this multiplexer's OWN bookkeeping
(`interest_ops` / `ready_ops`), and "the peer never sent anything" and "the
peer sent and this multiplexer never reported it" both render as `ready=0x0`.
Each key now carries a zero-timeout `poll(2)` of its fd taken at census time,
rendered `IN|OUT|PRI|ERR|HUP|NVAL`, beside the selector's `epoll_fd`, its
wakeup-pipe fds, and whether that pipe holds an unconsumed wakeup. `POLLIN` on
a socket whose key reads `ready=0x0` with the reactor parked is readiness lost
inside the VM; a quiet poll on every socket says the bytes were never sent, and
the defect is upstream of the selector entirely.

### `monitor owner=` on the wait-object dump

`Object.wait()` must have RELEASED the monitor. An `owner` still equal to the
waiting thread would mean no completer's `synchronized` block can ever run,
which presents as `notify=0` on a promise nobody completed — indistinguishable
from a completer that never ran, and those need opposite investigations. Read
under the state lock the wait loop already holds, beside `entry_count`,
`parked_waiters` and `pending_notifies`.

### `PshProbe` — the Java side

`/data/nres/src-probe/io/netty/handler/ssl/PshProbe.java`, loaded through a
classpath overlay so netty's own sources are untouched. It registers each
awaited netty operation, reports one that has been outstanding for 15 s with
the channel's open/active/registered state and — the deciding number — the
owning event loop's `pendingTasks()`, and halts the JVM at two minutes.
`pendingTasks > 0` on a sleeping reactor would be a lost `Selector.wakeup()`;
the reproduction above reads `pendingTasks=0`, which is what ruled that out.

## Repro

The harnesses are in-tree at `probes/psh-probe/` (with their own README);
the copies below are the working ones on Azure host 2 (`azureuser@20.80.105.49`),
under `/data/nres`, carrying that host absolute paths:

```bash
bash /data/nres/huntloop.sh 400 <tag>                # tracers armed, stops on the first catch
bash /data/nres/hangloop.sh 40 <tag> full            # load-proof; PshProbe halts at 120 s stuck
bash /data/nres/hangloop.sh 40 <tag> full hotspot    # the same command on HotSpot 25
NRES_ARGS=/data/nres/ossl-plain.args \
  bash /data/nres/hangloop.sh 40 <tag> full          # ... without the test-class overlay
bash /data/nres/hangloop.sh 60 <tag> comp            # the composite test alone
bash /data/nres/nresloop.sh 14 <tag> full            # VM watchdog armed (census + kernel poll)
bash /data/nres/zerocell-ab.sh 5                     # the JIT vs --nojit zero-cell A/B
bash /data/nres/triage.sh   /data/nres/<tag>         # stall vs merely-slow
```

`/data/nres/ossl.args` is a private copy of `gen-openssl-args.sh`'s output with
`/data/nres/overlay` prepended, so the instrumented copy of the test class
shadows netty's for these runs only and no other session on this shared host
sees it. `overlay.py` generates that copy from netty's source; every insertion
is a registration or a print, and `PshProbe.await` calls exactly the
`syncUninterruptibly()` the test called. `ossl-plain.args` is the same argfile
WITHOUT the overlay.

`OpenSsl.isAvailable` must be true; `gen-openssl-args.sh` is what makes it so.
