# TestGroupChannelSenderConnections — intermittent SIGSEGV

**Status:** OPEN, re-diagnosed 2026-07-28. It has **two** causes; one is now
FIXED, the other is not, so the class still crashes at roughly its original
rate. The original title blamed `NioSender.keepalive` — see "It is NOT a JIT
bug in the Tribes sender" below for why that reading was wrong.

Found while closing
[22-tribes-realnetwork-membership-bug](../../internal/fixed-suite-bugs/tomcat/22-tribes-realnetwork-membership-bug-FIXED.md).
Neither cause is Tribes-specific — this class is just a cheap reproducer.

## Symptom

`org.apache.catalina.tribes.group.TestGroupChannelSenderConnections` crashes the
VM part-way through `testConnectionLinger`, roughly 1 run in 6. That is the test
which sends its 3 messages back-to-back with **zero delay** (the other two space
them 1–2 s apart), so the pooled sender is returned and reused rapidly.

The class also fails non-fatally at a similar rate with an assertion failure or
`GroupChannel.messageReceived Unable to deserialize message:[ClusterData…]`.
Both are consistent with the same use-after-free, but neither has been pinned.

## It is NOT a JIT bug in the Tribes sender

The original filing read the `NioSender.read` / `ParallelNioSender.keepalive`
Java frames plus three `external/jit` native frames as "the fault is reached
from JIT-compiled code in the TCP sender". Both halves were wrong:

* The Java frame list is explicitly *"published at the last blocking/safepoint
  deposit — may lag the faulting instruction"*. It names where the mutator last
  parked, not where it faulted.
* The three `external/jit` frames sit at byte-identical addresses in **every**
  crash, including ones whose faulting PC is completely different. They are the
  Windows exception-dispatch path above the handler, not compiled Java.

Symbolize the real frames with `CRATONVM_SYMBOLIZE`, run **from the build tree**
so the 97 MB PDB next to the binary is found — a copied-out `.exe` symbolizes
every address to `<unresolved>`.

## Cause 1 — finalizable objects were not GC roots ✅ FIXED

Dominant baseline site: `interpreter.rs::run_finalizers`, faulting in
`heap.class_id_of` on an address returned by `finalizer_thread.dequeue()`.

`ReferenceProcessor::mark_finalizer_enqueued` flags an entry so
`finalizer_referent_addresses` stops reporting it (that flag stops the
resurrection channel re-enqueuing the same object every cycle). From then on the
only thing referring to the object is the pending finalization queue — a raw
address no marker can see. `update_after_gc` covered a moving collection; a
non-moving young mark-sweep simply freed the object. `run_finalizers` also
returns early whenever a JIT borrow is live, so entries routinely sit queued
across several collections, widening the window.

Separately, only the `System.gc` path passed finalizable roots at all; the four
allocation-driven `collect_garbage` sites passed `&[]` — silently falsifying the
invariant `process_references_after_gc` already documents ("Finalizable objects
are rooted via `finalizer_addrs`, so a LIVE one is always in the pointer map").

Fixed by seeding one `finalizable_roots` set (registered + pending, deduped) at
all five sites. Effect over 24 runs each: **baseline 3 of 4 crashes at
`run_finalizers`; after the fix 0 of 6.** The site is gone.

## Cause 2 — JIT-dependent missing root on an interpreter operand stack 🔴 OPEN

With cause 1 fixed the crash just moves. Sites across 6 crashes in 24 runs:

| faulting frame | count |
|---|---|
| `VmHeap::load_and_forward` ← `execute_instruction` (`Getfield`) | 3 |
| `vm_exec::object_num_fields` ← `native_map_put_evict` | 2 |
| `vm_exec::invoke_on_class_shared_inner` | 1 |

All three read a *reclaimed* object header. The `Getfield` one is the clearest:
the receiver is popped off the operand stack into a bare Rust local, and the
barrier added there (`load_and_forward`, "GCBARRIER-CDLWAIT-FIX") heals a
receiver that **moved** but faults outright on one that was **freed**. So the
reference was already dangling on the interpreter operand stack — a missing
root, not staleness after a move.

**The JIT is required.** `--nojit`: **16 runs, 0 crashes.** Every crash report
also carries `gc young-gen policy: non-moving (STW mark-sweep young)`. Note the
JIT-frame state is *not* constant: some reports say
`jit: guarded compiled frames live process-wide: YES (quiescence depth=4)` and
others `no (quiescence depth=0)`, so "a sweep caught peers inside JIT code" is
**not** a sufficient description — compiled code merely has to have run.

This is most likely the long-running "JIT corruptor" family rather than a new
bug; compare `reference_osr_main_corruptor` (many apparent JIT corruptors are
really the non-moving young sweep) and the fork6 GC-stress corruption notes.

### Already ruled out

**The RBC.6 precise-handler-frame re-gating does not fix this.** Dev's
`f09c9dff1` / `docs/known-issues/jit-precise-handler-frame-drops-live-locals-20260727.md`
describes a very similar shape (a live local dropped from a reconstructed
handler frame is a root the collector cannot see, JIT-only, needs a GC at the
wrong moment) and closes the gate by default. Rebuilding on top of it changes
nothing here: **24 runs, 4 crashes**, at `load_and_forward` (×2) and
`invoke_on_class_shared_inner` (×2). Worth knowing before spending a build on
that hypothesis again.

Crash counts across the three builds, 24 runs each, 4-way parallel:

| build | CRASH | other non-PASS |
|---|---|---|
| dev `39b1258fa` | 4 (3 of them `run_finalizers`) | 4 |
| + finalizable-roots fix | 6 (0 `run_finalizers`) | 1 |
| + latest dev incl. the RBC.6 re-gate | 4 | 1 |

### Where to pick it up

`CRATONVM_DBG_HEAP_STALE=1` — which flags a live object whose *field* points at
reclaimed memory — stays **silent** across these crashes. That is itself a
result: it places the dangling reference in a frame/operand stack rather than in
the heap graph, where that verifier cannot see it. The useful next probes are
`CRATONVM_DBG_STRAYSTACK=1` and `CRATONVM_DBG_CORRUPT_FRAMES=1`, plus forcing a
moving young collection to confirm the non-moving sweep is required.

## Reproduction

This class starts its channels with `SND_RX_SEQ|SND_TX_SEQ` only — no membership
service, so no multicast group is joined and the receiver ports auto-bind.
Unlike the membership tests it is therefore safe to run several copies
concurrently, which is what makes a ~1-in-6 crash cheap to hunt:

```powershell
$env:CRATONVM_REAL_NET_SOCKETS='1'; $env:CRATONVM_REAL_AQS='1'
<cratonvm.exe> -Xmx2g -Djava.net.preferIPv4Stack=true -cp <tomcat suite cp> org.junit.runner.JUnitCore org.apache.catalina.tribes.group.TestGroupChannelSenderConnections
```

Run ~24 copies 4-way parallel; expect 4–6 access violations.

## Evidence it predates the doc-22 membership fixes

Interleaved A/B, alternating binaries run-by-run so host load is shared, 12 runs
each: clean `origin/dev` 2 crashes, `origin/dev` + doc-22 fixes 2 crashes —
unchanged. The class passes on HotSpot in the same fixture.
