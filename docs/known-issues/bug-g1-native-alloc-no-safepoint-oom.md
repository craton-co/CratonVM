# G1 backend can OOM-abort mid-boot/mid-workload because native methods that allocate internally never trigger a GC safepoint check

## Status
**OPEN, root-caused 2026-07-25.** Found while investigating the residual
`--nojit --Xmx 1g`/`--Xmx 4g` OOM noted in
`docs/known-issues/h2/bug-h2-testfilesystem-testconcurrent-async-hang.md`'s
"Open: remaining performance gap" section (next-step #4). This is a
**general VM/GC architecture gap**, not specific to H2 or `--nojit` — it
affects the G1 backend under ANY workload whose hot path calls native
methods that allocate heap objects internally without any interleaved
bytecode-level allocation instruction. Not fixed this session (see "Why not
fixed here" below); this doc exists so the fix is well-scoped for whoever
picks it up next.

## Reproduction
```
cd apps/h2database-suite-runner   # H2_ROOT pointed at a checkout of apps/h2database/h2
CP=<h2 test classpath>            # target/classes:target/test-classes:<m2 test-scope classpath>
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 <cratonvm-bin> --java-home <jdk25> \
  --Xmx 1g --nojit -c "$CP" org.h2.test.unit.TestFileSystem
# -> FATAL: G1: out of heap space for object allocation (40 bytes), SIGABRT
# Reproduces within 1-10s. --Xmx 4g/8g only delays it (see timeline below),
# does not prevent it.
```
`--nojit` is the trigger only because it is the ONE thing that flips the
default GC backend from `Generational` to `G1`
(`vm-cli/src/main.rs` ~line 2026, "Interpreter-only workloads..." comment) —
the bug lives entirely in `gc/src/g1.rs`, not in anything JIT-related.

## Root cause (confirmed via instrumentation + live gdb)
`maybe_gc()` (`vm/src/runtime/interpreter.rs:1110`), the ONLY call site that
ever triggers `G1Collector::collect_garbage()` during normal execution, is
invoked from exactly 5 places in the interpreter — all 5 are **bytecode-level
allocation instructions**: `New`, `Newarray`, `Anewarray`, `Multianewarray`,
and the array-via-constructor-reference special case. There is **no**
`maybe_gc()` call after a native method invocation returns, no matter how
much heap that native method allocated internally via `ctx.new_object`/
`alloc_synthetic`-style helpers.

Consequence: a hot loop whose body calls only native methods (no bytecode
`new`/`newarray` of its own — e.g. `AsynchronousFileChannel.read(buf, pos)`
in a tight loop, where every allocation happens INSIDE
`native_afc_read`/`afc_box_integer` on the Rust side to box the return
value) can run for many thousands of iterations with **zero** GC safepoint
checks. Each iteration's small boxed-Integer garbage silently piles up.
Eventually `G1Collector::alloc_object` (`gc/src/g1.rs:6913`, the "cannot
safely retry" native-allocator entry point — see its doc comment) exhausts
every region and calls `std::process::abort()` — there is no opportunity for
ANY collection to run in between, because nothing ever asked for one.

This is invisible under the default `Generational` backend only because
`GenerationalHeap::alloc_object` has an internal, self-contained escape
hatch: "on \[young-gen\] exhaustion spill to old gen (non-moving) BEFORE the
hard abort" (`gc/src/gen_heap.rs:951-956`). That fallback doesn't require an
external `maybe_gc()` call at all — it's the SAME direct-abort-styled entry
point as G1's, but it happens to have much more headroom built in (the whole
old-gen pool) before it needs to abort. **G1Collector::alloc_object has no
equivalent internal fallback** — it fails the instant its Free-region pool
is empty, with no safety net.

### Evidence trail
1. `--Xlog "gc*=info:stdout:..."` and `CRATONVM_GC_STATS=1` (both real,
   wired flags — see `vm-cli/src/main.rs` `--Xlog`/`--verbose:gc`) showed
   **zero** GC-related output before the abort on the fast (`--Xmx 1g`,
   OOMs in <1s during VM bootstrap) repro — misleading at first (looked like
   "GC never runs at all"), but this specific fast repro just never got far
   enough into user code to hit a `maybe_gc()` checkpoint at all yet.
2. A temporary instrumentation patch (`CRATONVM_DBG_G1DIAG=1`, printing
   region-type counts before/after every `collect_garbage()` call — not
   merged, see below) on an `--Xmx 1g` run that got further into
   `TestFileSystem`'s test list showed **5 healthy collections**, each
   correctly freeing/recycling regions (`free` oscillating between ~980 and
   ~1020 out of 1024 regions, `pinned=0` throughout — ruling out a
   JNI-critical-pin leak, and ruling out the previously-fixed "kept-region
   death spiral" from `docs/known-issues` history / `[[g1-serial-defect-stack-steadychurn]]`,
   since there were zero evacuation-failure `[RETRY]` events and zero
   permanently-stuck regions between collections).
3. Between the 5th collection's completion (`free=1020, eden=1`) and the
   abort, `free` went from 1020 straight to **0** with **no further
   `collect_garbage()` call in between** — i.e. ~1020 MB of straight-line
   allocation with zero safepoint checks.
4. `gdb -batch -ex 'break abort' -ex run -ex 'bt 40'` on the live repro
   caught the exact stack at the moment of abort:
   ```
   #3 g1.rs:6949 (the eprintln!+abort in alloc_object)
   #5 alloc_object () at gc/src/g1.rs:6917
   #6 alloc_object () at vm/src/vm/vm_exec.rs:6080
   #7 alloc_synthetic () at native-io/src/lib.rs:15349
   #8 afc_box_integer () at native-io/src/lib.rs:15915
   #9 native_afc_read () at native-io/src/lib.rs:15824
   #10 safe_native_call_impl closure () at vm/src/vm/vm_exec.rs:950
   ...
   #19 execute_invokevirtual_cached () at vm/src/runtime/interpreter.rs:40254
   ```
   Confirms: the failing allocation is `afc_box_integer` (boxing
   `AsynchronousFileChannel.read`'s int return value), invoked via ordinary
   cached-invokevirtual bytecode dispatch — exactly the "native call inside
   a loop with no bytecode `new`" shape predicted above.

## Why this wasn't fixed in this session
A complete, safe fix needs a `maybe_gc()`-equivalent checkpoint inserted
after every **top-level, bytecode-driven** native-method-call return (NOT
inside the shared `safe_native_call`/`safe_native_call_impl` wrapper in
`vm/src/vm/vm_exec.rs`, which is also used for *nested* re-entrant native
calls — e.g. a native calling `invoke_virtual` internally, seen live in one
gdb snapshot: `native_rq_remove_timeout` → `invoke_virtual` → ... →
`safe_native_call`). Adding the checkpoint at that shared low-level wrapper
would be the simplest one-line fix and would cover every native method at
once, but it changes the GC-safepoint frequency for deeply-nested
native-in-native call chains in a way that's hard to fully reason about
without a dedicated regression pass — the existing safety invariant ("a
native function must not hold an unrooted local `ObjectRef` across any call
that might trigger a moving GC") already technically applies today
(New/Newarray executed via a nested bytecode call can already move things),
but this change would exercise that invariant *far* more often, on every
single native call rather than only ones that happen to execute `new`/
`newarray` bytecode internally — which could surface previously-latent bugs
in less-audited natives.

The safer, scoped fix is to add the checkpoint only at the OUTERMOST
bytecode-dispatch call sites (mirroring the existing New/Newarray/
Anewarray/Multianewarray pattern exactly): `execute_invoke_kind`
(interpreter.rs:20593, the generic/slow invoke dispatcher),
`execute_invokevirtual_cached` (39411), `execute_invokestatic`/
`execute_invokestatic_cached` (31271/32249), and the
`intercept_force_registered_native`/`_cached` pair (29528/29774) — five to
six call sites, mirroring the five that already exist for the allocation
bytecodes. This is real, but nontrivial, interpreter surgery across several
hot dispatch paths that deserves its own isolated branch + full regression
suite pass (Spring/WildFly/Tomcat/H2/Kafka/ES), which didn't fit this
session's primary scope (the `TestFileSystem.testConcurrent` **JIT-mode**
perf gap this doc's sibling covers). Flagging it here precisely-scoped
rather than rushing a wide interpreter change with no regression budget
left.

## Suggested next steps for whoever picks this up
1. Add `maybe_gc(shared, thread)` at the 5-6 call sites listed above, right
   after a native method's result is available and (if non-void) pushed
   onto the frame's value stack — exactly the same position/pattern as the
   existing New-family checkpoints.
2. Full regression pass across every app suite runner before merging — this
   changes safepoint-checking frequency for literally every native call in
   the VM, so a narrow test is not sufficient evidence of safety.
3. Re-run this doc's repro (`TestFileSystem` under `--nojit --Xmx 1g`) to
   confirm it no longer OOMs, and separately confirm it doesn't regress
   `--jit on` behavior (should be a no-op there in practice, since
   Generational's internal old-gen spillover already masks the gap, but
   confirm no perf regression from the extra `needs_gc()` checks on the hot
   invoke path).
4. Consider whether `G1Collector::alloc_object`/`alloc_array` should ALSO
   gain a small emergency-reserve pool (a handful of regions held back from
   normal Eden allocation) as defense-in-depth, independent of fixing the
   safepoint gap — buys a bit more margin for any *other* native-only hot
   loop not yet identified, though it only delays rather than fixes the
   underlying issue on its own.

See [[g1-serial-defect-stack-steadychurn]] for the PREVIOUSLY fixed,
superficially-similar "kept-region death spiral" (ruled out here — no
evacuation failures observed) and
`docs/known-issues/h2/bug-h2-testfilesystem-testconcurrent-async-hang.md`
for the sibling JIT-mode investigation this was found alongside.
