# New JIT SIGSEGV regression cluster — dev `8f29af96` (2026-07-04)

**Status:** FIXED and MERGED to `dev` at `042a064c` (merge of
`fix/jit-sigsegv-regression-a11025aa`, fix commit `839f1ea27`), per explicit
user sign-off on 2026-07-04 — see "Candidate fix" and "Post-merge
verification" below for what was (and wasn't) re-checked before landing.
**Severity: HIGH** — hard crashes (SIGSEGV), worse than the FAIL/HANG statuses
these classes previously had.

## How this was found
Rerunning the 255 non-passed classes from a full Hibernate suite run at dev
`81a31c08` (real-JDK, JIT-on, `CRATONVM_LOADER_AWARE_RESOLUTION` default-on,
TIMEOUT=600s, Azure Linux host), after syncing to dev `8f29af96` (34 new
commits, mostly JIT-related) and rebuilding: **CRASH count jumped 17 → 31**,
with **18 of those newly `rc=139` (SIGSEGV)** on classes that previously had a
DIFFERENT, non-crash status at `81a31c08`:

| class | status @ `81a31c08` | status @ `8f29af96` |
|---|---|---|
| `batch.BatchTest` | HANG | **CRASH rc=139** |
| `boot.database.qualfiedTableNaming.DefaultCatalogAndSchemaTest` | HANG | **CRASH rc=139** |
| `bulkid.OracleInlineMutationStrategyIdTest` | HANG | **CRASH rc=139** |
| `bytecode.enhancement.lazy.basic.EagerAndLazyBasicUpdateTest` | FAIL | **CRASH rc=139** |
| `bytecode.enhancement.orphan.EagerOneToManyPersistAndLoadTest` | FAIL/PASS (flaky) | **CRASH rc=139** |
| `bytecode.enhancement.orphan.EagerSubSelectOneToManyPersistAndLoadTest` | FAIL/PASS (flaky) | **CRASH rc=139** |
| `bytecode.enhancement.orphan.LazyOneToManyPersistAndLoadTest` | PASS/FAIL (flaky) | **CRASH rc=139** |
| `bytecode.enhancement.orphan.OrphanTest` | FAIL | **CRASH rc=139** |
| `hql.ASTParserLoadingTest` | HANG | **CRASH rc=139** |
| `hql.BulkManipulationTest` | FAIL | **CRASH rc=139** |
| `joinedsubclassbatch.IdentityJoinedSubclassBatchingTest` | FAIL | **CRASH rc=139** |
| `query.hql.FunctionTests` | FAIL | **CRASH rc=139** |
| `sql.exec.SmokeTests` | PASS/FAIL (flaky, known OSR/GC-lambda family) | **CRASH rc=139** |
| `type.temporal.{OffsetDateTimeTest,OffsetTimeTest,ZonedDateTimeTest}` | already flaky-CRASH (known temporal-GC cluster) | CRASH rc=139 (consistent with prior non-determinism, not necessarily new) |

The other 13 CRASH entries are `rc=1` (clean exit) — the already-documented
wrong-vtable-dispatch family (`JsonEmbeddable*`, `InVmGenerations*`,
`ListenerTest`, `ManyToManyHqlMemberOfQueryTest`, etc.) — unchanged from
before, not part of this new regression.

## Suspect commits (34 landed between the two data points, `81a31c08..8f29af96`)
The newly-SIGSEGV classes span JIT-heavy hot loops (query parsing, batch
insert loops, lazy-enhancement dispatch) — strongly suggests a JIT codegen
regression, not a GC or classloading issue. Prime suspects, in rough order of
how directly they touch codegen:

1. **`a4913d8b` / `7e0c9e26` "fix(jit): disable callee-saved GPR local homes"**
   (appears twice in the log — possibly the same change landing via two merge
   paths, or two related patches). Disabling callee-saved-register local homes
   is exactly the kind of register-allocation change that could silently
   corrupt a live value across a call in some code shapes not covered by
   whatever regression prompted the "fix" — highest suspicion.
2. **`a11025aa` "fix(jit): stop blacklisting whole methods for a dead
   invokedynamic"** — if a method was previously blacklisted (interpreted) and
   is now JIT-compiled, any latent codegen bug in that method's shape would
   newly surface.
3. **`8aa720b6` "Fix tiered JIT manager marking failed bg-compiles as done"**
   — could cause a bad/partial compiled method to be installed and used
   instead of correctly falling back to interpreted.
4. **`3415d052` "fix(jit): reject unsafe OSR dead-local entries"** — this is
   the fix that resolved `CompoundNaturalIdTest` (see
   `jit-osr-linux-regression-triad.md` #3) with no apparent side effects; lower
   suspicion but include in the bisect range since it touches OSR entry-state
   reconstruction.

## Repro
```
cd /home/victor/hibpkg/runner   # or the Windows apps/hib-suite-runner mirror
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 <cv> --java-home <jdk25> --Xmx 1500m @common.linux.args \
  -Dcraton.batch=1 CratonRunner <listfile-with-BatchTest-or-similar> 0
```
`org.hibernate.orm.test.batch.BatchTest` is a good first repro target — it was
a known, well-understood *slowness* HANG before (not a correctness bug), so a
clean SIGSEGV now is an unambiguous new regression signal, not noise from an
already-flaky class.

## Bisection plan
`git bisect` between `81a31c08` (last known-good for these classes) and
`8f29af96` (34 commits later) using `BatchTest`/`ASTParserLoadingTest` as the
bisect probe (both were reliable non-crashing HANG before — a `rc=139` on
either is an unambiguous bad marker). Get a VEH/backtrace via
`CRATONVM_SYMBOLIZE` on the crash to confirm it's JIT-generated-code related
(vs. e.g. a stack-depth/native issue) before assuming the JIT commits above
are the cause.

## Also landed in this window (not regressions — improvements)
30 classes newly PASS since `81a31c08`, mostly the `bytecode.enhancement.
detached.{initialization,reference}.*` fetch-variant family (lazy-enhancement
gate work continuing) plus `SessionDelegatorBaseImplTest`, `CompoundNaturalIdTest`
(the OSR fix), and several misc XML/HBM classes.

## Bisection result (2026-07-04)

Ran a real `git bisect` (not manual guessing) between `81a31c08` (good) and
`8f29af96` (bad) in a scratch worktree on the Azure Linux build host
(`/home/victor/wt-jitbisect`), using `org.hibernate.orm.test.batch.BatchTest`
as the probe (`rc=139` = bad, a clean HANG/rc=124 = good — BatchTest's
pre-regression baseline is a legitimate slowness HANG, not a crash, so any
`rc=139` is an unambiguous new-regression signal). 4 bisection steps, no
skips needed, fully unambiguous result:

**Culprit: `a11025aa25abbabdb4e28ca26ad899ca5a64015b` — "fix(jit): stop
blacklisting whole methods for a dead invokedynamic"**

This was the #2-ranked suspect in the original triage above, not the
top-ranked callee-saved-GPR commits. The callee-saved-GPR commits
(`a4913d8bb`/`7e0c9e264`) were confirmed GOOD in bisection — they *disable* a
suspect register-allocator feature by default (the opposite direction of
introducing a new bug), so in hindsight they were never a strong fit for a
*new* crash. The two identical-message callee-saved-GPR commits are NOT a
duplicate merge artifact with different content: they are byte-for-byte
identical except one extra defensive line in `a4913d8bb` (non-x86_64
architectures keep the legacy conservative skip-list active); irrelevant to
this x86_64 Linux repro either way.

### Root cause

`a11025aa2` made methods containing `invokedynamic` JIT-eligible for the
first time (previously ANY invokedynamic anywhere in a method's bytecode —
even on a dead-assert branch — permanently blacklisted the whole method from
JIT compilation). Its codegen lowers `invokedynamic` (bytecode `0xba`) to an
**unconditional jump to the existing shared uncommon-trap deopt stub**
(`DeoptReason::UnreachedCode`), on the theory that this is a safe fallback:
if the JIT-compiled path ever actually reaches that jump, the method just
deopts back to the interpreter.

That shared deopt stub (`emit_deopt_stubs` in `../../../jit/src/x64.rs`) calls
`jit_uncommon_trap(vm_ptr, reason, bci)`, and loads `vm_ptr` by reading the
compiled frame's hidden VM-pointer slot at `self.heap_local_offset`. That
slot is a JIT compiler internal that is **only reserved when `needs_heap` is
set** during the `jit_scan` pre-pass — otherwise `heap_local_offset` degrades
to alias local slot 0 (this exact failure mode is already documented in a
pre-existing comment on the `aastore`/`0x53` scan arm a few lines above the
`0xba` arm, describing the identical "vm_ptr becomes a stack address → SIGSEGV"
bug for a different opcode). The new `0xba` scan arm in `jit_scan`
(`../../../jit/src/x64.rs`) records the invokedynamic call site for the codegen but
**never sets `needs_heap = true`**, unlike every other opcode whose codegen
depends on that same slot (`invoke*`, `new`, `anewarray`, `aastore`).

Consequence: for a method containing an invokedynamic but no *other*
heap-requiring opcode — the overwhelmingly common shape per the original
commit's own rationale, `assert cond : "msg" + var;` on an
assertions-disabled dead branch, often the *only* heap-touching-looking
construct in an otherwise arithmetic/dispatch-heavy hot method —
`heap_local_offset` is never reserved. When that unreachable branch is
JIT-compiled (correctly, since it must still be compiled even though it's
dead code) and the shared trap stub is emitted for it, the stub loads
whatever garbage/aliased-local value sits at the un-reserved slot and passes
it as `vm_ptr` into `jit_uncommon_trap`, which casts it straight to `&SharedVm`
and dereferences deep into it. The "safe fallback" trap was itself unsafe.

### Backtrace evidence (confirms JIT-generated-code corruption)

Linux has no VEH-equivalent hardware-fault handler with register/backtrace
dumping (that's Windows-only, see `runtime::crash_handler::windows_fault`);
the Linux signal handler in the same file is deliberately async-signal-safe
only and writes just a minimal marker file. Built a `release-with-debug`
profile binary (keeps line-table debuginfo, unstripped) at the culprit
commit and ran the `BatchTest` repro under
`gdb --batch -ex run -ex "bt full" -ex "info registers" -ex "thread apply all bt"`.
Real crash, with full frames:

```
Thread 2 "main-vm" received signal SIGSEGV, Segmentation fault.
#0  atomic_compare_exchange_weak<u8> ()  (core::sync::atomic)
#1  compare_exchange_weak ()
#2  lock ()  (parking_lot::raw_mutex::RawMutex)
#3  lock<RawMutex, cratonvm_jit::deopt::DeoptimizationLog> ()
#4  record_deoptimization ()  vm/src/vm/vm_init.rs:3694
#5  deoptimize ()  vm/src/jit/helpers.rs:4662
#6  jit_uncommon_trap ()  vm/src/jit/helpers.rs:4863
#7  0x00007fffec72e073 in ?? ()   <-- raw JIT-generated code, no symbol table
#8..14  <raw JIT frame / stack slots, no symbols>
#15 try_lambda_dispatch ()  vm/src/runtime/interpreter.rs:18078
```

Frame `#7` — the direct caller of `jit_uncommon_trap` — has no symbol table:
it is JIT-emitted machine code (the deopt-stub's `CALL jit_uncommon_trap`),
exactly matching the mechanism above. The fault is a SIGSEGV *inside the
mutex-lock CAS itself* (`atomic_compare_exchange_weak`), i.e. `self` for
`Mutex<DeoptimizationLog>` was already garbage by the time `record_deoptimization`
tried to lock it — consistent with a corrupted `vm_ptr` (`&SharedVm`)
propagating a bad pointer all the way down through `deoptimize` into the
mutex it dereferences. This is unambiguous JIT-codegen/runtime-boundary
memory corruption, not a stack-depth or unrelated native issue.

### Spot-check scope (how contained is this regression)

Ran 28 classes at the candidate-fix commit (see below): the 12 non-temporal
newly-SIGSEGV classes from the table above (excluding
`OffsetDateTimeTest`/`OffsetTimeTest`/`ZonedDateTimeTest`, a separate
already-flaky temporal/GC-lambda cluster, not part of this regression) plus
15 classes sampled from `passed.txt` (4498 classes) in
`/home/victor/hibpkg/runner`. **28/28 — zero SIGSEGVs.** All 12
previously-SIGSEGVing classes reverted to their pre-regression `81a31c08`
status (HANG or a `@@RESULT` with the same FAIL shape as before); all 15
sampled `passed.txt` classes still complete cleanly (two show non-zero
`failed=N` sub-test counts, e.g. `DirtyTrackingDynamicUpdateAndInheritanceTest`,
`ManyToOneNoProxyTest` — this harness's "passed" list tracks
crash/hang/no-crash at the class level, not per-JUnit-method greenness, so a
class with some failing `@Test` methods that still completes without
crashing is correctly "passed" in this harness's sense, both before and
after the fix). No new failure modes observed anywhere in the sample.

### Candidate fix

Branch `fix/jit-sigsegv-regression-a11025aa`, on top of `8f29af96`, in
`/home/victor/wt-jitbisect` on the Azure Linux build host (NOT pushed, NOT
merged to dev — commit `839f1ea27`). One-line functional change: sets
`needs_heap = true` in the `0xba` (invokedynamic) arm of `jit_scan`
(`../../../jit/src/x64.rs`), mirroring every other opcode whose codegen reads
`heap_local_offset`.

Verified so far:
- `BatchTest` reverts from `SIGSEGV(rc=139)` to `HANG(rc=124)`, exactly
  matching its `81a31c08` baseline (no crash).
- The 28-class spot-check above (12 previously-SIGSEGVing + 15 previously-passing):
  zero SIGSEGVs, all statuses consistent with pre-regression behavior.

**NOT yet verified** (needed before this can merge to dev):
- Full rerun of all 255 `nonpassed_latest.txt` classes (only 12 of the 18
  newly-SIGSEGV classes were spot-checked; the temporal cluster and the
  other ~237 non-regression classes in that list were not re-checked against
  this fix).
- Full `passed.txt` regression sweep (4498 classes; only 15 sampled).
- `cargo test -p cratonvm-jit` (the culprit commit's own test suite, 870+
  tests) has not been re-run against the fix.
- No review of whether other opcodes added/changed in the same
  `81a31c08..8f29af96` window have a similar missing-`needs_heap` gap for
  any *other* codegen path that reads `heap_local_offset` (this fix only
  addresses the `0xba` arm specifically).

### Post-merge verification (2026-07-04)

Merged to `dev` at the user's explicit request before the full 255-class
rerun could complete (a bisection + build cycle already consumed the
available window). Before merging, additionally ran on the Windows box (on
top of the already-landed merge of `origin/dev`, which brought in 2 more
commits — see the "concurrent origin/dev sync" note below):
- `cargo check --workspace`: clean, zero errors.
- `cargo test -p cratonvm-jit` (dev profile — the jit crate's suite relies on
  `debug_assert!`, which is compiled out under `--release`, so 4
  aarch64-branch-offset-overflow tests spuriously fail in `--release` only;
  confirmed pre-existing/unrelated to this fix, not a regression): **873
  passed, 0 failed.**

**Still NOT done** (follow-up, not blocking since the fix is minimal,
mirrors an established pattern (`needs_heap` on every other opcode touching
`heap_local_offset`), and passed its own targeted verification pre-merge):
- Full rerun of all 255 `nonpassed_latest.txt` classes.
- Full `passed.txt` regression sweep (4498 classes; only 15 sampled
  pre-merge).
- Audit of whether any *other* opcode added/changed in
  `81a31c08..8f29af96` has a similar missing-`needs_heap` gap.

### Concurrent origin/dev sync (found while merging this fix)

While merging this fix branch into `dev`, `origin/dev` had moved 2 commits
ahead of local `dev` (`b7ea8b93`/`81762470`, a `jla_define_class`
duplicate-definition cleanup) — unrelated to this bug, but merging it
surfaced a **silent, non-conflicting** bad auto-merge: local `dev` had
independently "restored" `jla_define_class` after an earlier bad merge
sequence, but restored the *wrong* (stale, `cl_define_class_basic`-backed)
implementation, while `origin/dev` had deliberately kept the correct,
newer `define_class_via_full`-backed one. Git's merge picked the local
(wrong) version with no conflict marker since the two diffs didn't
textually overlap. Fixed as a separate follow-up commit
(`f081f587`) that replaces it with `origin/dev`'s version — unrelated to
the JIT fix itself, noted here only because it landed in the same push.

**Guidance for other agents:** the fix is merged — JIT-heavy workloads on
current `dev` should no longer hit this specific SIGSEGV. That said, any
method containing a reachable-at-runtime invokedynamic with no other
heap-touching bytecode was the exposure (general JIT-codegen bug, not
Hibernate-specific), so treat the "still NOT done" items above as open
follow-up work rather than fully closed. If a *new* SIGSEGV surfaces in a
similar shape (deopt-stub / `heap_local_offset`-adjacent), check other
opcodes for the same missing-`needs_heap` gap before assuming it's a new
bug.
