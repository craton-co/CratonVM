# JIT regalloc callee-saved-register clobber — the umbrella family behind the skip-list

**Severity:** Historical high — formerly contained by the largest cluster of targeted JIT bans.
**Status:** ✅ FIXED on dev (2026-07-04) — the x64 JIT no longer assigns integer locals to
callee-saved GPR homes by default, and the `is_known_miscompile` targeted family is inactive on
that safe path. The old register-home path remains opt-in only via
`CRATONVM_JIT_ENABLE_CALLEE_SAVED_GPR_LOCALS=1` for diagnostics, where the targeted guard still
applies.
**Mode:** Historical JIT-only — every manifestation passed under `--nojit` /
`CRATONVM_DISABLE_JIT=1`.

This doc consolidates what is known across the many per-session investigations (W2-CHM, RBC.1,
SPB.1–3, EXEC.1, NETTY.1, CM-FASTMATH, JUNIT.1, Tomcat Bug B/D, kafka-bug-C). They are not
independent bugs — they are one root-cause family seen from many entry points.

## Shared root cause

When a method is JIT-compiled and its callees/loops cross the tier-up thresholds, the register
allocator / calling-convention code can **leave a stale value in a callee-saved register across a
call**, or mishandle the **allocate-then-`putfield`** sequence, so a live value (a loop induction
variable, a `this`/array base, or a freshly-allocated object's header/fields) is corrupted. The
corruption surfaces at the next use of that register/slot.

Primary suspects in codegen (function names as cited across the ban comments):
- [`../../../jit/src/x64.rs`](../../../jit/src/x64.rs) `emit_invoke_virtual` / `patch_self_calls`
  (`patch_self_calls` at line ~20327) — callee-saved register preservation across JIT→JIT calls.
- [`../../../jit/src/x64.rs`](../../../jit/src/x64.rs) `emit_inline_tlab_new` (line ~9996) — the inline-TLAB
  fast allocation path; writes the object header at an R11/TLAB-cursor that can be wrong, so a
  freshly-`new`'d object's header/fields land in the wrong place (allocate-then-putfield).
- `../../../jit/src/regalloc.rs` — liveness/coalescing (the CM-FASTMATH bug was a `bc_len` desync that hid
  later local uses from liveness → two live values coalesced onto one register).

## Two symptom flavors

1. **Crash — `rc=139` / STATUS_ACCESS_VIOLATION (Windows).** A stale pointer in a callee-saved
   register is dereferenced at the next field access, or a mis-headed fresh object desyncs the
   non-moving young sweep (class_ids decay to interface ids; "implausible object size";
   "inconsistent header"). Examples: RBC.1 (BouncyCastle), SPB.1–3 (Spring boot), EXEC.1 (j.u.c.),
   JUNIT.1 (JUnitCore.main), Tomcat Bug D (HeapByteBuffer GC-capable `<init>`).
2. **Hang — `rc=124` (TIMEOUT).** A live loop induction variable or loop-termination value lives
   in a clobbered register, so the loop never advances/terminates. Examples: NETTY.1
   (`Arrays.fill` counted loop — since LIFTED), Tomcat Bug B (`WeakHashMap` iterator `hasNext`),
   kafka-bug-C (`WeakHashMap$ValueSpliterator.tryAdvance` — `tab[index++]` never advances).

## Trigger conditions

- JIT enabled. `--nojit` always correct.
- The method (or a callee) crosses a tier-up threshold: invocation `c1`/`c2` thresholds and/or the
  OSR back-edge threshold (`osr_threshold`, default `10_000` in `../../../jit/src/tiered.rs`; older ban
  comments cite a 1000/2000 era). Several bans note the bug needs *both* OSR and per-callee
  invocation thresholds crossed in the same outer frame.
- Frequently the **allocate-then-`putfield`** idiom (`new X; … putfield`) or a **hot loop with an
  intervening call** (a `field[index++]`/iterator loop whose body calls another JIT'd method).
- **Not GC-dependent in the hang case** — kafka-bug-C still hangs at `-Xmx4g` (no young GC). Some
  *crash* variants are genuinely GC-interaction (Tomcat D: a young GC during a GC-capable `<init>`).

## Affected-method catalog (the current bans)

Grouped from `is_known_miscompile`:
- **HashMap / LinkedHashMap (allocate-then-putfield):** `put`, `putVal`, `newNode`, `treeifyBin`,
  `hash`, `afterNode{Insertion,Access,Removal}`, `HashIterator.{<init>,hasNext,nextNode}`,
  `{Key,Entry,Value}Iterator.next`, `keysToArray`, `valuesToArray`, `prepareArray`;
  `LinkedHashMap.{newNode,newTreeNode,afterNode*}`.
- **WeakHashMap:** `HashIterator.{<init>,hasNext}`, `{Entry,Key,Value}Iterator.next`,
  `Entry.<init>`, `getTable`, `expungeStaleEntries`, **and (kafka-bug-C)
  `{Value,Key,Entry}Spliterator.{tryAdvance,forEachRemaining}`**.
- **Boxing:** `Integer.{valueOf*,<init>,parseInt}`, `Long.{<init>,parseLong}` (`valueOf` lifted —
  see below).
- **String:** `toLowerCase`, `toUpperCase`, `hashCode`.
- **java.util.concurrent:** `ThreadPoolExecutor.{execute,runWorker,getTask}`,
  `LinkedBlockingQueue.{offer,enqueue,take,dequeue}`, `AtomicInteger.{incrementAndGet,getAndIncrement}`,
  `CountDownLatch.{countDown,await}` + `Sync.{tryReleaseShared,tryAcquireShared}`,
  `AbstractQueuedSynchronizer.{acquire,release,acquireShared,releaseShared,signalNext,…}` +
  `ConditionObject.{signal,signalAll,doSignal,await,newConditionNode,enableWait}`,
  `ReentrantLock.{lock,unlock}`.
- **java.security:** `Provider.{put,parseLegacy,putService,implPut}`,
  `Provider$ServiceKey.{hashCode,equals}`.
- **Reflection / generics:** `Class.{getGenericInterfaces,getGenericSuperclass,getGenericInfo}`,
  `ClassRepository.{getSuperInterfaces,getSuperclass,make}`, `AbstractRepository.getTree`.
- **ByteBuddy (Hibernate proxy build):** `ByteBuddyState.make`.
- **Misc:** `AbstractCollection.{addAll,toArray}`, `HashSet.{<init>,iterator}`.

## Key finding (kafka-bug-C, 2026-06-18): it is NOT reproducible by bytecode shape

A direct fix attempt for the `tab[index++]` (`dup_x1` field-post-increment) case built **three
increasingly faithful standalone reproducers** — `int[]`; `Object[]` + GC write-barrier +
two-condition `while` loop; the full external-driver one-emit-per-call `while(tryStep())` pattern
with a linked `Entry` and a null table slot. **All three JIT-compile correctly and match HotSpot.**
So the `dup_x1`/`tab[index++]` *lowering* is correct in isolation; the miscompile only appears
inside the real method, driven by its specific register pressure and its `getFence`/`accept`
calls. This matches the broader pattern: the defect is **context-sensitive register allocation**,
not a single wrong opcode lowering — which is exactly why per-method skip bisection (not a small
synthetic repro) is what localizes each instance.

## Partial progress (bans that have been LIFTED after targeted fixes)

The family is being chipped away, not stuck:
- **CM-FASTMATH** (commons-math `FastMath` trig) — root-caused to a `regalloc.rs::bc_len` liveness
  desync (missing `ldc`/`ldc_w`/`ldc2_w`); **fixed**, ban removed, 6.3M-input sweep == `Math.sin`.
- **NETTY.1** (`Arrays.fill` counted-loop hang) — **lifted** after the regalloc/lentable fixes;
  `bench/FillProbe` is the regression witness.
- **W2-CHM** (`Integer.valueOf` returns `value=0`) — **lifted** after the `bintrees18` inline-TLAB
  header-coherence fix in `emit_inline_tlab_new`; `bench/ChmScale` is the witness.
- **bug-03 / Matcher.search** — was misfiled as a `search` miscompile; the real bug was the
  virtual-dispatch *bail* using the static call-site class, fixed in
  `jit/.../helpers.rs::bail_to_interpreter`.
- **TckLang.exc_hierarchy** (`RuntimeException instanceof Exception && Throwable`) — **lifted**
  after the 2026-07-01 retry. The former `Int(0)` reproducer now passes under forced inline JIT:
  `CRATONVM_JIT_ALLOW_PACKAGES=cratonvm/`, `CRATONVM_JIT_THRESHOLD=1`, `CRATONVM_BG_COMPILE=0`
  with `cargo test -p cratonvm-vm --features synthetic-jdk --test interpreter_tests
  test_s46_exc_hierarchy -- --nocapture`. The stale `cratonvm/TckLang.exc_hierarchy`
  `is_known_miscompile` entry was removed.

### Length-table hardening (2026-06-21) — defense-in-depth against the CM-FASTMATH class

The CM-FASTMATH bug was a `bc_len` length-table desync (missing `ldc*`). An audit of the two
JIT instruction-length twins found three more 5-byte opcodes absent from both
`regalloc.rs::bc_len` and `x64.rs::bytecode_len_at`: **`invokedynamic` (0xba), `goto_w` (0xc8),
`jsr_w` (0xc9)** — they fell through to the `_ => 1` arm, a 4-byte under-count that would desync
every PC-stepping consumer (liveness, branch-target precompute, DCE, OSR/unroll, oop maps), the
exact class of liveness-desync miscompile CM-FASTMATH was. **Currently latent** — `jit_scan`'s
catch-all rejects all three (no compiled method contains them today, same as `wide`/0xc4) — but
the tables are documented to stay correct as defense-in-depth, and `invokedynamic` JIT support is
being explored. Added to both twins with regression tests (`bc_len_five_byte_ops`,
`test_bytecode_len_invoke`); the `loop_analysis.rs` and `vm/.../skip_list.rs` length tables
already handled them. Tied to the family because a wrong length here is precisely how the
register-clobber surfaces.

So individual manifestations are tractable once a reproducer or a precise codegen site is found;
what remains open is the **general** clobber.

## Investigation recipe (how each instance gets localized)

1. `--nojit` vs JIT (confirm JIT-specific); `-Xmx4g` (GC-triggered?); `CRATONVM_JIT_THRESHOLD=1000000`
   (invocation tier-up?).
2. `--stack-dump-on-timeout <secs>` for hangs ("main thread in native Rust code" ⇒ a JIT loop not
   hitting safepoints; the dispatch ring names the area).
3. `CRATONVM_JIT_BISECT_ONLY=<class-prefix>` (only that prefix stays JIT-eligible) to bisect by
   package, then `CRATONVM_JIT_BISECT_SKIP=Class.method` (EXACT, comma-separated, needs the `.` —
   prefix entries are silently dropped) to bisect to the single method.
4. `CRATONVM_DBG_JIT_GEN=1` / `CRATONVM_DBG_JITC=1` list compiled method names;
   `CRATONVM_DBG_JIT_DISASM=1` dumps the emitted machine code (prints at compile time, before a
   hang). Lift a ban for study with `CRATONVM_JIT_ALLOW_PACKAGES=<prefix>`.
5. The fix is per-instance until the general regalloc work lands; the targeted ban is the interim
   disposition (correctness over the throughput of that one method).

## Fix path

- **Landed:** disable callee-saved GPR local homes in the default x64 backend path. Locals now use
  canonical frame homes unless `CRATONVM_JIT_ENABLE_CALLEE_SAVED_GPR_LOCALS=1` is explicitly set.
  This removes the stale-register state the umbrella family depended on, trading some throughput
  for correctness.
- **Diagnostic legacy path:** the graph-coloring allocator and the historical targeted
  `is_known_miscompile` table remain available for controlled bisection under the opt-in register
  homes flag. Do not make that flag a production default without a precise register-liveness/map
  replacement.

## Root-cause progress log

### 2026-07-04 — default-off callee-saved GPR local homes

Fixed the umbrella family by removing callee-saved GPR local homes from the default x64 codegen
path. `../../../jit/src/x64.rs` still runs graph coloring, but unless
`CRATONVM_JIT_ENABLE_CALLEE_SAVED_GPR_LOCALS=1` is set it replaces the GPR local assignment vector
with `None` entries and emits no callee-saved GPR save/restore metadata for those homes. XMM local
allocation and frame-slot locals remain unchanged.

`../../../vm/src/jit/skip_list.rs` now treats `is_known_miscompile` as a legacy guard for the opt-in GPR
local-home mode. Under the default safe mode the targeted regalloc family methods are JIT-eligible
again; constructor/interface/default-thread and unrelated package/cold-path guards remain separate.

Validation added:
- `jit::x64::tests::callee_saved_gpr_local_homes_are_default_off`
- skip-list tests for HashMap, WeakHashMap spliterators, AQS condition waits, and Keycloak
  credential lazy getters all assert JIT eligibility under the safe default while preserving the
  historical targeted table entries.

### 2026-07-01 — `TckLang.exc_hierarchy` retry no longer reproduces

Retried the documented `cratonvm/TckLang.exc_hierarchy` member with the method forced through the
inline JIT path (`CRATONVM_JIT_ALLOW_PACKAGES=cratonvm/`, `CRATONVM_JIT_THRESHOLD=1`,
`CRATONVM_BG_COMPILE=0`). The test now returns the expected `Int(1)`, so the old `Int(0)`
reproducer is stale on current `dev`. Removed the targeted `is_known_miscompile` ban and the
ignored/panicking Tier-1 reproducer note.

This does **not** close the umbrella family: the remaining `java/util`, JUC, reflection/generic,
ByteBuddy, and other targeted bans still represent active or not-yet-retried members of the
callee-saved/regalloc family.

### 2026-07-01 — wide local / wide branch liveness hardening

The 2026-06-21 length-table fix made `regalloc.rs::bc_len` walk `wide`
(0xc4), `goto_w` (0xc8), and `jsr_w` (0xc9) at the correct instruction
lengths, but two metadata helpers still lagged behind it: `local_access` did not
decode widened local operands, `find_float_locals` did not mark widened
`fload`/`dload`/`fstore`/`dstore` locals as XMM-only, and CFG branch metadata did
not treat `goto_w` / `jsr_w` as wide-offset branches. Today this is latent
because `jit_scan` rejects these opcodes, but once accepted it would miss local
uses/defs or branch targets while the PC walk itself stayed in sync, which is the
same class of silent liveness bug as CM-FASTMATH.

Fixed on branch `codex/jit-known-issues-20260701-8`: decode `wide`
load/store/iinc local metadata, classify widened float/double locals, decode
`goto_w` / `jsr_w` branch targets, and mark `goto_w` as an unconditional CFG
transfer. Added unit tests for all four metadata paths. This is a general
defense-in-depth fix for the regalloc metadata layer only; the broader
callee-saved clobber / targeted-ban family remains open.

### 2026-07-01 — JIT reentry borrow-suspend gap fixed

The distinct borrow-tracker sub-bug below is fixed on branch
`codex/jit-known-issues-20260701-2`: all direct compiled-entry fast paths now route through a
single `try_call_compiled_entry_reentrant` wrapper that suspends/restores the debug JIT borrow
flag around the raw callee call. This covers the thread-local `DISPATCH_CACHE` hit, the JIT-cache
hit, the post-compile fast path in `jit_invoke_dispatch`, and the MIC-hit path in
`jit_invoke_virtual_mic`.

This closes the JIT-to-JIT reentry borrow-suspend gap only. The broader regalloc/callee-saved
clobber family and the `WeakHashMap`-style JIT codegen work remain open.

### 2026-06-18 — a distinct JIT→JIT-reentry borrow-suspend gap (historical diagnosis)

While hunting the kafka-bug-C (`WeakHashMap` stream) hang, a scratch reproducer (`Dx4`: a field-
post-increment `while (idx<hi || cur!=null)` loop driving `Consumer.accept` via **invokeinterface**
to a JIT'd lambda) surfaced a **separate, deterministic** JIT soundness bug — and it is worth
fixing in its own right:

- **Symptom:** debug-build panic at `vm/src/jit/helpers.rs jit_thread_mut`: *"aliasing &mut
  JvmThread borrow detected … sibling fabrication"*. Trigger needs BOTH the field-post-inc loop AND
  invokeinterface-to-a-JIT'd-lambda (`invokevirtual` to a concrete class does NOT trip it — it
  inlines and avoids the nested dispatch).
- **Root cause:** `jit_invoke_virtual_mic`'s MIC-hit fast path (`helpers.rs` ~3506) re-enters the
  compiled callee via `try_call_compiled_entry` while still holding this frame's `_jit_thread_guard`
  borrow — **without** the `set_jit_thread` suspend the interpreter/bail path uses
  (`interpreter.rs:3458/14908/16902/17237`). The nested invokeinterface dispatch's `jit_thread_mut`
  is a legit child reborrow, but the tracker wasn't told → debug assert (and in **release**, where
  the assert is compiled out, two un-suspended `&mut JvmThread` = aliasing UB). The 3 sibling fast-
  path sites in `jit_invoke_dispatch` (2987/3036/3080) likely share the gap.
- **Historical WIP:** branch `wip/jit-reentry-borrow-suspend` wrapped the re-entry in
  `set_jit_thread`/`restore_jit_thread`. The panic goes away and normal `invokevirtual` dispatch
  still works — **but** `Dx4` then *hangs in BOTH JIT and interpreter* (so `Dx4` is a *compound*
  repro carrying a second, non-JIT bug, and is NOT a clean kafka-bug-C repro, which is JIT-only).
  The `WeakHashMap` hang is unchanged. So this fix removes a real (masking) debug false-positive but
  does not close the user-facing hang.
- **Why it stayed open at the time:** it was a HOT-PATH change (every MIC-hit dispatch), not yet
  regression-verified, and didn't fix the target hang. The JIT reentry borrow gap is now closed by
  the 2026-07-01 shared-wrapper fix above; a JIT-*only* `WeakHashMap`-style repro is still needed to
  chase the actual clobber.

## Related

- [kafka-bug-C-weakhashmap-stream-infinite-hang.md](kafka-bug-C-weakhashmap-stream-infinite-hang.md)
  — the most recently added member (hang, fixed via ban `1cd0ab26`; refined root cause here).
- The GC-root-coverage-under-JIT family (Family A in this folder's README) is a **separate** root
  cause (root *scanning* completeness, not register *clobber*) — don't conflate them.
