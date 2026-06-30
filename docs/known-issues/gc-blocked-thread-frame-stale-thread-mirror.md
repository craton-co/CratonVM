# GC: live young `java.lang.Thread` mirror in a blocked thread's frame reclaimed by the young collector (Tomcat real-net/real-AQS HARD CRASHES)

**Status:** 🟢 **Thread-mirror manifestation RESOLVED via stale-mirror recovery** (the
`this.holder is null` NPE family); 🟢 **the JIT-spill half of the non-`Thread` gap is now fixed**
(see "Partial fix" below — the Tomcat DoHead AQS `ConditionObject`/`ConditionNode` flood drops
~99.8%); 🟠 a register-resident remainder still leaks (JIT `await` keeping the oop in a non-volatile
register across `park`, never spilled — the precise-oop-map / register-invisibility family).

**Partial fix (deposit-path JIT-frame parity, branch `claude/beautiful-satoshi-5c4027`).** The
blocking-deposit snapshot builder `NativeContextImpl::deposit_root_snapshot` (`vm/src/vm/vm_exec.rs`)
was the **only** root-snapshot builder that did NOT fold in the active-JIT-frame conservative scan
that the safepoint builder `interpreter::update_root_snapshot` already runs
(`scan_active_jit_frames` + the shadow-stack fold). So a worker parking **inside a JIT-compiled**
`AbstractQueuedSynchronizer$ConditionObject.await` deposited a snapshot with **no JIT roots**: when
`LifecycleBase.stop()` tore the executor down (making the `ConditionObject` heap-unreachable so it
was kept alive only by the parked frame), a cross-thread STW young sweep — which marks a parked
thread **only** from its deposited snapshot — reclaimed the live `ConditionObject` and its
`firstWaiter`→`ConditionNode` chain. Fix = call `scan_active_jit_frames` (+ shadow fold) in
`deposit_root_snapshot`, identical to the safepoint path; `deposit_root_snapshot` always runs on the
parking thread itself, so the thread-local JIT scan is the SAFE single-thread-in-JIT case. Measured
on `…DoHeadInvalidWrite0ValidWrite0` (idx 28): the AQS `ConditionObject` stale-receiver flood at
sub-test ~77 drops from **54810 (runaway → crash)** to **~2–108 (contained, no crash)**.

**REJECTED follow-up: conservative live-register capture.** An inline-asm capture of the parked
thread's non-volatile registers (`rbx,rbp,rsi,rdi,r12-r15`) as extra roots — to close the
register-resident remainder — was tried and **reverted**: it makes the flood WORSE, not better. On
the safepoint path it triggered a runaway `NioEndpoint$Poller` flood; on the deposit path it pushed
the AQS flood back up to 9069/120639. The captured registers are mostly false positives; pinning them
under the non-moving sweep over-retains young, raises GC pressure, and exacerbates the reclamation.
The remainder needs **precise oop maps**, not conservative register pinning.

**Resolution (Thread mirrors).** A moving / promoting young GC relocates a thread's
`java.lang.Thread` mirror and correctly remaps the registry + per-thread `java_thread_obj` field,
but a stale copy of the *vacated* address can survive in a running / blocked frame's operand stack
or local. We now make that recoverable: `ThreadRegistry::update_thread_objs_after_gc` records every
relocated mirror's **vacated address → owning tid** in a bounded table (`former_mirror_addrs`), and
the interpreter's virtual-invoke path, on detecting an all-zero-header (`class_id == 0`) receiver,
calls `recover_stale_mirror` to substitute that thread's live mirror before dispatch
(`vm/src/runtime/interpreter.rs`, `vm/src/threading/thread_registry.rs`). Precise — the table is
populated only by GC relocations and consulted only on genuinely-zeroed receivers, and a vacated
address uniquely identifies one thread's mirror, so there are zero false substitutions. This
eliminates the `Thread.getThreadGroup()`/`getPriority()`/`isDaemon()` NPEs (Tomcat
`TestDigestAuthenticator` + ~10 siblings: holder-NPE count → 0 in both jit and nojit; 4 of the 12
now PASS, the rest progress past server startup to *separate* downstream issues).

A **defensive crash-mitigation was previously MERGED** (commits `6a04b0e3` + `e06ed934`) that
converted the resulting VM aborts into a graceful Java-level `NullPointerException`; the recovery
above now repairs the `Thread`-mirror case so it neither crashes nor NPEs. For **non-`Thread`**
stale frame references the reclamation itself is still not prevented (see TODO).

**Severity:** was *critical* (SIGSEGV / Rust panic = VM abort); now *medium* (correctness — stale
reads degrade to null). **Manifests** under heavy concurrent networking + locking
(`CRATONVM_REAL_NET_SOCKETS=1` + `CRATONVM_REAL_AQS=1`), in **both** `--nojit` (moving Cheney young
collector) and JIT (non-moving sweep + selective promotion). GC-timing dependent.

## Symptom

Found 2026-06-29 in the Apache Tomcat suite (overnight matrix). Six classes that PASS/complete on
HotSpot **hard-crash the VM** (these are SIGSEGV / Rust panics, **not** test failures):

- `org.apache.catalina.servlets.TestDefaultServletEncodingWithBom`
- `org.apache.catalina.servlets.TestDefaultServletEncodingPassThroughBom`
- `org.apache.catalina.servlets.TestDefaultServletEncodingWithoutBom`
- `org.apache.catalina.tribes.test.channel.TestMulticastPackages`
- `org.apache.catalina.tribes.test.channel.TestDataIntegrity`
- `org.apache.catalina.tribes.group.TestNonBlockingCoordinator`

Two crash shapes, both downstream of the same corruption:

```
thread 'main-vm' panicked at types/src/compact_value.rs:502:
  CompactValue::object: pointer 0x7865746e6f63203a exceeds 47-bit address space
# EXCEPTION_ACCESS_VIOLATION (SIGSEGV) (0xC0000005) at pc=0x… (read at 0xFFFFFFFFFFFFFFFF)
```

Always preceded by thousands of (survivable) `gc::guard` warnings:

```
WARN cratonvm_vm::runtime::interpreter: Stale pointer detected in invokevirtual receiver
     (ptr=0x…, all-zero header) — falling back to CP class java/lang/Thread
WARN cratonvm::gc::guard: gen_heap::get_field/set_field … class_id=ClassId(0) java/lang/Object
```

i.e. a live `java.lang.Thread` mirror's header has been **zeroed** (reclaimed) while a frame slot
still references it. The fatal decode happens when the freed young slot is **reused for byte-buffer
data** and a surviving stale reference decodes those bytes as an object pointer — observed values:
`0x7865746e6f63203a`=`": contex"`, `0x7777777777777777`=`'w'` fill (TestDataIntegrity),
`0x8D8D8D8D8D8D8D8D` poison, `0x6E3A7461636D6F54`=`"Tomcat:n"`. All are unaligned / >47-bit, so they
either overflow the 47-bit `CompactValue` payload (panic) or deref to garbage (SIGSEGV).

## Repro

```
scripts/build-cpu.bat                            # build cratonvm.exe
# classpath: apps/tomcat/.suite/cp.txt ; tests under apps/tomcat/output/testclasses
cd apps/tomcat
CRATONVM_REAL_NET_SOCKETS=1 CRATONVM_REAL_AQS=1 CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 \
  cratonvm -Xmx2g [--nojit] -cp <cp.txt> org.junit.runner.JUnitCore \
  org.apache.catalina.servlets.TestDefaultServletEncodingWithoutBom
```

`--nojit` reproduces the `compact_value.rs:502` panic deterministically (overnight). The JIT SIGSEGV
is **intermittent** (GC-timing). **Trap:** a peer session running `taskkill /IM cratonvm.exe` to
reap its own strays will kill this run too (rc=127/139, false "crash") — run under a **unique binary
name** (e.g. `cratonvm_pike.exe`) to dodge it. Helper scripts:
`scripts/verify-crash-fix.sh`, `scripts/verify-encoding-jit.sh` (not committed; session tooling).

## Root cause

A **live young `java.lang.Thread` mirror** held in a thread's Java **frame local** is **not in that
thread's mark root set** at a young GC, so the collector reclaims it (moving: relocates and resets
from-space; non-moving sweep: zeroes the dead slot). The stale frame slot then reads an all-zero —
or, after slot reuse, a garbage — header.

Diagnosed with `CRATONVM_DBG_STALE_RECV=1` + `CRATONVM_DBG_BUG03=1`:

```
[stale-recv] ptr=0x1ca41f10 method=java/lang/Thread.getContextClassLoader() — Java frames:
  [55] org/apache/catalina/core/StandardContext.bind(…)…
      LOCAL[0] -> 0xc6244640 all_zero_header=false
      LOCAL[2] -> 0xc64072e8 all_zero_header=false
      LOCAL[3] -> 0xc6446840 all_zero_header=false
      LOCAL[4] -> 0x1ca41f10 all_zero_header=true STALE      <-- current-thread mirror
[BUG03-gc] e1 initiator=tid1 main_mirror=0x1caa1f08 moves_this_gc=true main_blocked=true
```

`LOCAL[4]` is the **current thread's mirror** (`Thread.currentThread()` → `getContextClassLoader()`).
It sits at a **young** address (`0x1ca41f10`, all-zero = reclaimed) while every other live local is
already in **old-gen** (`0xc6…`). The mirror was object-tagged (so this is **NOT** the lost-tag case
of `gc-moving-interpreter-lost-tag-missed-root.md`), yet it was reclaimed.

The holder thread is **blocked** (`main_blocked=true`). A thread blocked in a native call
(`CRATONVM_REAL_NET_SOCKETS` socket I/O, `CRATONVM_REAL_AQS` `LockSupport.park`) is **excluded from
the STW barrier** and contributes its roots only via its **deposited `root_snapshot`**
(`deposit_root_snapshot` → `collect_all_root_snapshots`, `fold_pointer_map_into_blocked`). The
sibling locals (`0,2,3`) were in that snapshot (kept + promoted); `LOCAL[4]` was **not** → swept.
Either (a) the mirror was loaded into `LOCAL[4]` after the snapshot was deposited while the thread
was still flagged `in_blocked_region`, or (b) `Thread.currentThread()` returned an already-stale
mirror (see "Likely same underlying bug" below). The initiator can't safely scan another thread's
live frames — that is *why* the snapshot mechanism exists — so the gap is that the snapshot is
stale/incomplete relative to the blocked thread's current frame.

## Likely the same underlying bug as `gc-concurrent-spawn-reclamation` (whose fix is NOT on dev)

`docs/known-issues/repros/gc-concurrent-spawn-reclamation/README.md` is marked **✅ FIXED 2026-06-29**
but on branch **`feat/precise-maps-a4-finish` which was NOT pushed** — so **the fix is not on dev**,
and these Tomcat crashes confirm the bug is **still live on dev**. Its root cause: `currentThread()`
lazily builds the main mirror (`current_thread_object` → `build_thread_field_holder` →
`get_or_create_main_thread_group`); each step allocates, a young GC relocates the mirror, and VM
native code holds it in a **bare Rust local across the allocation** → stale. Its fix: in the four
`vm_exec.rs` helpers, re-read the mirror from the GC-remapped `java_thread_obj` field (or pin on
`native_pin_roots`) after every allocating step. If `currentThread()` returns a stale mirror, that
stale value is exactly what lands in `StandardContext.bind` `LOCAL[4]` here — so porting that fix to
dev is the most likely complete fix. Siblings in the same "all-zero header stale receiver" family:
`gc-moving-interpreter-lost-tag-missed-root.md` (OPEN, lost-tag variant),
`fork6-fjp-multithread-jit-root-reclamation.md` (A4), and the non-moving-sweep correctness section in
`README.md`.

## Mitigation MERGED to dev (defense-in-depth, does NOT fix the reclamation)

A stale/garbage reference is provably-not-a-pointer (unaligned | null-guard-page | >47-bit). New
`cratonvm_types::plausible_heap_pointer(raw)` (pure bit ops, **zero false positives** — real objects
are 8-aligned, ≥0x1000, ≤47-bit) is applied at **every reference-decode boundary** so a stale ref
degrades to `null` (a Java-level NPE, matching HotSpot) instead of aborting the VM:

- **Interpreter** (`6a04b0e3`): `read_prim_element` Reference arm (`gc/src/heap.rs`);
  `CompactValue::from_value` (`types/src/compact_value.rs`, the named panic site).
- **JIT** (`6a04b0e3` + `e06ed934`): field/array read results, and **every receiver-deref helper**
  unified from `if ptr==0` to `if !plausible_heap_pointer(ptr)` (since `plausible_heap_pointer(0)` is
  already false this preserves the null path and routes a stale receiver through it instead of
  dereferencing): `jit_arraylength`, `jit_baload/iaload/bastore/iastore/aaload/aastore`,
  `jit_putfield_*`, `jit_getfield`, `jit_getstatic`, `jit_checkcast`, `jit_instanceof`. The
  `jit_invoke_*` path was already guarded (`&0x7 || >=1<<48`).

**Result:** all 6 classes are **crash-free in both jit and nojit** (verified; `WithBom` jit — which
SIGSEGV'd intermittently — ran clean 3×). The 6 still **fail/time out** (the live object is still
reclaimed → null reads break test logic), but the **VM survives**. The commit `6a04b0e3` also adds
the unrelated minor IBM850/CP850 charset (`UnsupportedEncodingException: ibm850`).

## Proper fix (TODO)

1. **Port the `gc-concurrent-spawn-reclamation` `currentThread()` re-read fix to dev** (most likely
   the complete fix for the Tomcat manifestation) and re-verify the 6 classes *pass*, not just
   survive.
2. Failing that, close the blocked-thread frame-coverage gap so a thread cannot execute Java with a
   stale `root_snapshot` while flagged `in_blocked_region` (refresh/clear on the blocking native's
   wake path), and audit that the real-net (`native-io`) and real-AQS park natives bracket with
   `begin_blocking_region`/`end_blocking_region`.
3. ✅ **DONE (deposit-path JIT-frame parity):** `deposit_root_snapshot` now folds in
   `scan_active_jit_frames` + the shadow-stack roots exactly like the safepoint `update_root_snapshot`
   — closing the JIT-spill half of the blocked-thread coverage gap (see "Partial fix" above).
4. **Remaining:** the register-resident remainder (oop kept in a non-volatile register across `park`,
   never spilled). Needs precise oop maps / a shadow stack for parked JIT frames — conservative
   register pinning is counterproductive (see "REJECTED follow-up"). Tracked with the
   register-invisibility family (`SB-CRASH-04` / precise-jit-stack-maps).
