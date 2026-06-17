# SB-SUITE-CRASH-04 — ~~JIT inline-`new` heap corruption~~ → **GC register-invisibility** (PluginXmlParser hang + AntoraAsciidoc wrong result)

> This is **manifestation A3** of the GC-root-coverage-under-JIT family — see
> [README.md](README.md) for the family map. Multi-thread sibling = [Fork6](fork6-fjp-multithread-jit-root-reclamation.md) (A4).

---
## SESSION UPDATE 2026-06-17 (#3) — re-verified on current `dev` (`fca424a4`, binary 2026-06-17); every mitigation still broken

Re-ran the deterministic repro on the current `dev` binary
(`MinRegexProbe code 20000`, `CRATONVM_DBG_GC_STRESS=524288`), correct-output metric:

| config | rc | `inconsistent header` | correct (`DONE` + right string)? | signature |
|---|---|---|---|---|
| `--nojit` | 0 | 0 | **YES (only one)** | — |
| default (JIT) | 1 | ~20 | NO | `Stale pointer … all-zero header` on `Pattern`/`Matcher` receiver |
| `CRATONVM_NO_JIT_SCAN_CACHE=1` | 1 | 79 | NO | all-zero `Matcher` |
| `CRATONVM_JIT_SAFEPOINT_REG_SPILL=1` | 1 | 152 | NO | (frame perturbation) |
| `…REG_SPILL=1 …NO_JIT_SCAN_CACHE=1` | 1 | 15 | NO | `Cannot invoke iterator on null` |
| `CRATONVM_DBG_FULLSTACK_SCAN=1` | 1 | 108 | NO | all-zero `Matcher` |
| `CRATONVM_SHADOW_STACK=1` (movable) | **139 SIGSEGV** | 0 | NO | Rust helper @ exe RVA `0x933305` reads `[rax+0x11]`, **rax=0** |
| `CRATONVM_SHADOW_STACK=1 …SHADOW_PIN=1` | **124 HANG** | 0 | NO | — |
| `CRATONVM_SHADOW_STACK=1 …NORELOAD=1` | **124 HANG** | 0 | NO | — |
| `…SHADOW_STACK=1 …NORELOAD=1 …NO_SELECTIVE_PROMOTE=1` | **124 HANG** | 0 | NO | — |
| `…SHADOW_STACK=1 …NO_SELECTIVE_PROMOTE=1` | **139 SIGSEGV** | 0 | NO | — |
| `--nojit …NO_SELECTIVE_PROMOTE=1` (control) | 0 | 0 | YES | — |

**Two hard conclusions:**

1. **No conservative scan can fix A3.** `NO_JIT_SCAN_CACHE` (the bug-06b/A2 fix —
   now default on dev), full-stack scan, blind callee-saved reg-spill, and every
   combination still crash. The live root is genuinely **register-resident** at the
   GC safepoint and unreachable by any read of stack memory. (This is the clean
   separation from A2: `NO_JIT_SCAN_CACHE` *does* fix ReflRepro, *does not* fix
   `MinRegexProbe`.)

2. **The `CRATONVM_SHADOW_STACK` precise-roots mechanism is itself broken right
   now** — in *both* directions:
   - **movable** (reload restores the GC-rewritten value into the home register)
     → **SIGSEGV**. The fault is in a Rust JIT helper at exe RVA `0x933305`,
     dereferencing a **null** pointer (`rax=0`) at offset `0x11`. Read: the shadow
     reload wrote a stale/zero value into a live oop home register (almost
     certainly an `invokevirtual` receiver — matches the default path's
     `Stale pointer … invokevirtual receiver` message), and the JIT then passed
     that null to a dispatch helper that dereferenced the object header.
   - **noreload / pin** (skip the restore) → **HANG**, even with
     `NO_SELECTIVE_PROMOTE` (no evacuation, so marking-only *should* suffice). A
     hang with 0 corruption strongly suggests the shadow `top` **drifts**
     (unbalanced push with no paired reload — e.g. an exception unwinding past a
     shadow-push'd CALL) so the buffer fills / the per-GC scan blows up. Probe with
     `CRATONVM_DBG_SHADOW_DEPTH=1`.

So the fix is *not* another conservative knob; it is to make precise JIT roots
**actually correct** (fix the reload-corrupts-register bug) and then
default-viable (the lazy-prologue perf lever). See the synthesis directly below.

### Pinned mechanism of the `SHADOW_STACK` movable SIGSEGV (4-agent diagnosis, 2026-06-17)

- **The faulting helper is `jit_getfield`** (`vm/src/jit/helpers.rs:1695`, via the
  inlined bounds-check `jit_putfield_slot_in_bounds` at `:1752/:1758`), **NOT** an
  invoke-dispatch helper. RVA `0x933305` = function entry `0x933270` + `0x95`; the
  faulting instruction is `mov eax, dword ptr [rdi+0x10]` — the `num_slots` (u32)
  read at object header offset 16. (Confirmed by cdb disasm against `cratonvm.pdb`:
  the surrounding code is `cmp rbx,rax; jae` index-bound check, `shl rbx,4`
  [SLOT_SIZE=16], `lea rax,[rdi+rbx]; add rax,0x28` [HEADER_SIZE=40],
  `movups xmm0,[rax]` = a 16-byte `Value` *load* → getfield, not store.)
- **The receiver is the integer `1`, not null.** `rdi=0x1`, so `[rdi+0x10] = [0x11]`
  faults. `rdi=1` is why the helper's `if obj_ptr==0` null guard (`test rdi,rdi; je`
  at the prologue) does **not** catch it. (`rax=0` is just the cleared load dest,
  `rbx=rdx=3` is `field_index`/slot 3 — earlier "rax=0 ⇒ reload wrote null" was a
  misread.)
- **The two safepoints are named and disassembled (`CRATONVM_DBG_SHADOW2` +
  `CRATONVM_DBG_DUMP_JIT` + capstone):** exactly **two** pushes in the whole run,
  both `this` (local 0) below a call —
  `Matcher.reset()` pc=110 (`aload_0; aload_0; invokevirtual getTextLength`),
  `this`→**R13**; and `Pattern.append(II)V` pc=29 (`...; invokestatic Arrays.copyOf`),
  `this`→**R15**. **The oop mark is CORRECT** — `this` is genuinely an object, and the
  disasm confirms R13/R15 hold a valid `this` at the push (no write to R13 between
  the prologue `mov r13,rdx` and the push). So the earlier "oop-mistag" theory is
  **DISPROVEN**: the push stores a valid pointer.
- **The bug is the RELOAD reading a shadow slot that was corrupted to `1` during the
  nested call.** In `reset`'s disasm: push at `09dc mov [r11],r13` (shadow[top]=this,
  savebase=[rbp-0x20]); after the `getTextLength` dispatch (`0a13 call`), the reload
  `0a53 mov r13,[r11]` loads R13 back from the (savebase-validated) slot — and the
  very next getfield (`0a90 call jit_getfield`, = the crash frame `reset+0xA92`,
  bytecode pc=118 `getfield modCount`) uses R13 as receiver → `rdi=1`. Both `reset`
  (R13) and `append` (R15) corrupt `this`→`1` the same way (hence `r13=1` **and**
  `r15=1` at the crash). So between the push (stores valid `this`) and the reload,
  **`[savebase]` becomes `1`** — the shadow buffer slot holding the caller's live
  `this` is overwritten across the nested call.
- **Leading mechanism (to confirm):** the boundary heal `restore_jit_thread`
  (`helpers.rs:356-364`) resets `shadow_stack.top` to the watermark
  **snapshotted at the *callee's* `set_jit_thread`** — but a JIT→interpreter→JIT
  re-entry can snapshot a `top` at or below the caller's still-live pushed slot, so a
  nested push then **overwrites** `[savebase]`. The `savebase ∈ [base,end)` validation
  in the reload (`0a26-0a3a`) can't catch this — `savebase` is still in range; it just
  reads the overwritten slot. (Exact origin of the value `1` not yet pinned — needs a
  runtime log of `[savebase]` at reload, or of `set_top`/`remap` during the nested
  call. GC remap is sound — `gc.rs update_all_roots` passes the full `evac_map` — so
  this is buffer-slot reuse, not a bad remap.)
- **Per the no-stub/no-mask rule, do NOT add a non-canonical receiver guard to
  `jit_getfield`** (same class as the avrora real-RAF `jit_putfield obj_ptr=0x1`).
  The fix is in the shadow-stack bookkeeping: a caller's pushed slots must not be
  reusable by a nested JIT re-entry — e.g. `set_jit_thread` should snapshot/raise the
  watermark to the *current* `top` (so nested pushes start *above* the caller's live
  slots), not reset below them.
- **Top-drift is NOT the default-path bug** (it is real only under the
  `CRATONVM_SHADOW_NORELOAD` toggle / OSR-track loops): the throwing-unwind path is
  triple-balanced — per-safepoint reload is emitted *before* the post-invoke
  exception check (`x64.rs:17347` then `:17360`); every abnormal-exit stub
  (`:12206/:12289/:12067/:12139/:15476`) runs the full `emit_epilogue` which
  restores the prologue-saved entry `top` (`:9358-9366`/`:9435-9442`); and
  `restore_jit_thread` re-clamps `top` even after a Rust panic
  (`helpers.rs:302/356-365`, `interpreter.rs:14692-14709`). The `noreload`/`pin`
  HANG is a *separate* latent gap: inline `emit_shadow_push` (`x64.rs:6265-6278`)
  has **no `top>=end` overflow guard** (the `push()` helper at `shadow_stack.rs:168`
  does), so skipping the pop grows `top` unbounded → buffer overflow + O(n) scan.
- **GC remap is sound** (`gc.rs` `update_all_roots` passes the full `evac_map` as the
  pointer_map; `gen_heap.rs:4411`), so the movable shadow slot *is* correctly
  rewritten after evacuation — reinforcing that the SIGSEGV is a JIT home-register
  miscompile, not a missing remap.
- **A3 == A2 (same class).** Update 2026-06-17: the bug-06b "scan-cache unsound"
  framing was **reverted** (`b41c0484`) — on current dev `NO_JIT_SCAN_CACHE` fixes
  *neither* ReflRepro nor `MinRegexProbe`; both are register-resident missed roots.
  See [reflrepro-register-resident-jit-root-handoff.md](reflrepro-register-resident-jit-root-handoff.md).
  **Asymmetry that matters for the fix:** `CRATONVM_SHADOW_STACK` *fixes* ReflRepro
  (no crash) but *crashes* `MinRegexProbe` via the reload bug pinned below — so the
  A2 handoff's "complete the shadow stack" plan must also fix this reload codegen.

**Concrete next step (pinning the corrupting site):** there are only **two**
candidate safepoints (`pc=29` and `pc=110`, each one `CalleeSaved` home). Identify
the two compiled methods (`CRATONVM_DBG_SHADOW2` prints the pc; add the method name
to that trace, or `CRATONVM_DBG_JIT_DISASM` the small compiled set — only ~2
methods have a register-resident oop at a safepoint) and disassemble around those
PCs. Find the operand entry that `stack_oop_marks` tags `true` while the
callee-saved register actually carries an `iconst_1`/boolean/`arraylength`-style
`1`. Then fix the provenance: the bytecode/codegen path that pushed a primitive
into a slot the marks still believe is an oop (dup/swap/merge-reconstruct, a
`getfield`-of-int result reusing an oop's `CalleeSaved` register, or a phi/merge
that didn't clear the mark). Acceptance: `SHADOW_STACK=1 MinRegexProbe code 20000`
under `GC_STRESS=524288` → `DONE` with the correct string, **and** the default
(non-shadow) all-zero-`Pattern` reclaim is also gone once the same marks drive a
complete root set.

---
## SESSION UPDATE 2026-06-16 (#2) — metric correction + conservative-spill experiment (branch `fix/sb-crash-04-register-invisibility`, worktree `C:\craton\CratonVM-sbreg`, built off dev `6088b9be`)

**TL;DR: the "register-invisibility" framing is CONFIRMED, but the documented fixes (shadow_stack / disable_inline) do NOT actually fix this on current dev — they only suppress the `inconsistent header` WARNING COUNT, which is a misleading metric. By the honest "correct output" metric, only `--nojit` passes.**

### The metric trap (important)
Every prior status here measured `inconsistent header` warning count. That counts GC-WALKER DESYNCS, not live-object reclamation. A live object can be cleanly zeroed (all-zero header) and the walker re-syncs over it WITHOUT a warning. So "0 inconsistent headers" ≠ fixed.

Verified on the clean `6088b9be` worktree binary (`MinRegexProbe code`, correct = produces `DONE` + the right ``a `X` b`` output):

| config | inconsistent-header count | **correct output?** |
|---|---|---|
| `--nojit` | 0 | **YES** (the only one) |
| default (JIT) | high | NO (segv / NPE) |
| `CRATONVM_SHADOW_STACK=1` | ~0 | **NO** (hangs w/ GC_STRESS; wrong answer w/o) |
| `CRATONVM_JIT_DISABLE_INLINE_NEW=1` | varies | NO |
| full callee-saved reg-spill (below) | much lower | NO |

So **shadow_stack does NOT fix SB-CRASH-04 on dev `6088b9be`.** Either it regressed since the report's `sbloop d760e63e` binary, or the original "FIXES IT" verdict was the metric trap (0 headers, but wrong/incomplete output). Re-verify any "fix" with correct-output, never header count.

Corroboration at *natural* GC frequency (`code 200000`, no GC_STRESS, N=3): `default`=2/3 correct, but `shadow`=0/3, `disable_inline`=0/3 (3 hangs), `spill`=0/3 (1 segv, 1 hang). So all three "fixes" are actually WORSE than the untouched baseline at natural GC — they perturb timing/layout without addressing the root cause. The bug is timing-sensitive; GC_STRESS is what makes it deterministic.

### Refuted: "FIX == NOSTORE" / "conservative fix impossible"
Built a gated experiment `CRATONVM_JIT_SAFEPOINT_REG_SPILL` (jit/src/x64.rs): at every GC-capable safepoint, blind-spill the CURRENT value of EVERY used callee-saved GPR (`alloc_used_regs`) into reserved frame slots (vs the report's earlier *selective operand-stack* spill). Also hoists the spill before the inline-hit MIC/PIC virtual-dispatch cascade (a real default-path gap: inline-hit virtual calls never spilled — only the `.miss` slow path did). Default-off = byte-identical. `=nostore` reserves slots but skips stores (frame-perturbation control).

A/B (8 runs, GC_STRESS=512K, code 20000), total `inconsistent header`:
- off (baseline layout) = **472**
- **spill = 128**
- nostore (same layout as spill, no stores) = **656**

`spill` (128) vs `nostore` (656) is the SAME frame layout ⇒ the **stores recover ~80%** of the desyncs. So callee-saved register-invisibility IS a major real contributor (refutes the doc's "FIX==NOSTORE"; the prior selective spill simply missed the UNTRACKED callee-saved oops). Note: frame perturbation alone makes it worse (472→656) and under *natural* GC the perturbation can dominate — so the conservative spill is layout-sensitive and **does NOT give correct output** → not a viable standalone fix (the doc's "conservative is fragile" stands, but for a sharper reason).

### The residual (what still reclaims a live object after the spill)
`CRATONVM_DBG_SWEEP_EDGES`: every sweep that drops a live node reports `edges: root=0 young-survivor=0 old-gen=0 => case (a) register/native root gap` — confirms register/native, never a heap edge.
`CRATONVM_DBG_CORRUPT_FRAMES`: the corrupting GC's mutator stack is shallow — **`java/util/regex/Pattern.<init>` (pattern compilation) ← `MinRegexProbe.main`** — and the crash is `Cannot invoke iterator on null` / `arraylength null in Matcher.reset` ⇒ a **collection/Matcher-typed object reclaimed-while-live** during the regex compile/allocation storm.

Since the victim survives `spill+CRATONVM_NO_JIT_SCAN_CACHE=1` (fresh scan, 0 headers but still crashes), its live root is **NOT** a heap edge, **NOT** a callee-saved register at a safepoint, and **NOT** a stale-scan-cache entry. Remaining candidates: a **caller-saved register** oop, or an **in-flight freshly-allocated object** held only in a Rust helper frame's register across a GC the helper itself triggers (`jit_new_object` GCs *before* alloc via the probe path, so look at `jit_post_tlab_init` / `jit_anewarray_object` / `jit_newarray` and the GC_STRESS trigger point).

### Recommended next step
Stop iterating conservative spills (confirmed fragile + layout-sensitive). Two real options: (1) **root-cause why shadow_stack stopped giving correct output on current dev** (it's the report's intended fix; correct-output regression hunt between `d760e63e`→`6088b9be`; note the main checkout also has ~3.2k lines of uncommitted JIT WIP in regalloc/pgo/escape that the report's binary may have included), or (2) pin the residual register/native root at `Pattern.<init>` pc with the annotated-disasm tool + per-PC oop dump and close it precisely. Both are multi-session GC/JIT-core. The `CRATONVM_JIT_SAFEPOINT_REG_SPILL` gate + `test-regspill.sh`/`test-correct.sh` harnesses are on the branch for whoever picks this up.

---

**Status (2026-06-16 — REDIAGNOSED, the inline-`new` theory is DISPROVEN):**
🟢 Root cause is now CERTAIN and it is **NOT** a JIT inline-`new` wild store. It is the
**register-invisibility GC hazard** already documented for kafka bug-21/22 and
spring-bug-10: CratonVM's *conservative* JIT-frame root scan only reads STACK memory, so a
live heap reference held **only in a callee-saved register** at a young-GC safepoint is
invisible to the mark phase → the non-moving young sweep zeroes that still-live object
(`gen_heap::sweep_young_non_moving` `write_bytes(obj, 0, total_size)` on the unmarked /
forwarded arms) → its all-zero header surfaces as the `Stale pointer detected … all-zero
header` on `java/util/regex/Pattern` receivers and as the GC-walker `inconsistent header`
desync. The seven "inline-`new` codegen" fix attempts below all chased the wrong layer.

`CRATONVM_JIT_DISABLE_INLINE_NEW=1` "fixing" it was a **timing artifact**, not a
localization: inline-`new` changes GC frequency + register pressure, and this bug is
extraordinarily timing/heap-layout sensitive.

## ✅ Deterministic repro (seconds, no JUnit suite needed)

`apps/spring-boot/buildSrc/runner/MinRegexProbe.java` (a tight `String.replaceAll` loop —
recompiles a `java.util.regex.Pattern` every iteration, the exact allocation churn that
trips it). Force frequent young GC with `CRATONVM_DBG_GC_STRESS=<bytes>` so the
register-only window is hit on essentially every collection:

```bash
cd apps/spring-boot/buildSrc; CP="runner;$(cat test-classpath.txt)"
CV=…/target/release/cratonvm.exe; JDK="…/jdk-25"
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 CRATONVM_DBG_GC_STRESS=524288 \
  "$CV" --java-home "$JDK" -cp "$CP" MinRegexProbe code 20000
# default → reliably ~20 `inconsistent header` + `Stale pointer detected … Pattern`; rc=1
```

### Confirmation matrix (clean dev binary, `GC_STRESS=524288`, `code 20000`)

| Config | `inconsistent header` | Note |
|---|---|---|
| default (conservative scan) | **~20, reproducible** | corrupts |
| `CRATONVM_SHADOW_STACK=1` (precise JIT roots) | **0, DONE** | **FIXES IT** |
| `--nojit` (no register-only refs) | **0, DONE** | clean |
| `CRATONVM_JIT_DISABLE_INLINE_NEW=1` | **0, DONE** | timing mitigation only |

`SHADOW_STACK=1` (which publishes the register-resident operand-stack oops as precise mark
roots — `x64.rs::collect_live_oop_homes`/`emit_shadow_push`, scanned in
`vm/src/memory/roots.rs` step 14b) makes it deterministically clean. This is the signature
of register-invisibility, identical to bug-21's `--nojit` / `SHADOW_STACK=1` proof.

## A conservative-side fix is IMPOSSIBLE — proven (do not chase it)

The conservative scan cannot see CPU registers, full stop. Tweaking *how much stack* it
scans or *cache freshness* only perturbs GC timing; on the deterministic repro
(`code 20000`, `GC_STRESS=524288`):

| Conservative tweak | `inconsistent header` |
|---|---|
| baseline | 20 |
| `CRATONVM_NO_JIT_SCAN_CACHE=1` | 13 |
| `CRATONVM_DBG_FULLSTACK_SCAN=1` | 15 |
| **both together** | **20** (back up — NON-monotonic) |

Non-monotonic ⇒ these are **timing noise, not root recovery**. A from-scratch
`emit_pre_safepoint_spill`-side experiment (spill register-resident operand-stack oops to
dedicated/locals frame slots so the conservative scan finds them) was **built, tested, and
reverted**: (a) its +56-byte frame growth ALONE moved the count 20→57 (`FIX == NOSTORE`,
i.e. the stores had *zero* effect — the bug is dominated by frame layout, not by the oops
that approach pins); (b) an earlier variant that rewrote the spilled entry to
`Frame(off)` in the operand spill region broke the `pop_stack` LIFO slot-reclaim invariant
→ reused a still-live slot → **null receiver in JIT `Matcher.reset`** — the *same* fault
the shadow reload had (spring-bug-10). The genuine register-only residual cannot be reached
by any conservative trick.

## The only correct fix = precise JIT roots (shadow stack), made default-viable

`CRATONVM_SHADOW_STACK=1` (+ `CRATONVM_SHADOW_PIN` — pin-aware reload landed dev `f4249f7a`)
is the proven-correct fix. It is held DEFAULT-OFF purely for PERF (~7× on call-heavy code:
the per-method prologue `get_current_thread` CALL + per-safepoint push/reload + marking
scan — see spring-bug-10 doc). The landable path is the documented perf lever: make the
prologue thread-fetch **lazy/conditional** (emit it only for methods that actually have a
register-resident-oop safepoint — the `CRATONVM_DBG_SPOOP` probe shows MinRegexProbe's
compiled set has only ~6 such sites; the vast majority of methods have none and would pay
nothing), then run the `SHADOW_STACK` regression + bt16/bt18 correctness/no-OOM sweep on a
quiet machine and flip the default. This is multi-session GC/JIT-core work, NOT a wild-store
codegen patch.

**Mitigation today:** run affected (regex/Pattern-heavy, allocation-dense) classes with
`CRATONVM_SHADOW_STACK=1` (deterministically clean). Do not pursue `DISABLE_INLINE_NEW`
or any `emit_inline_tlab_new` edit — they are the wrong layer.

---

## ⛔ SUPERSEDED below — the original (incorrect) "inline-`new` wild store" investigation

The material below is retained for history. Its root-cause framing is WRONG (it is GC
register-invisibility, not a JIT inline-`new` codegen defect); the seven ruled-out
hypotheses were all within the wrong subsystem.

**Status (orig):** 🟡 Root-caused + candidate fix applied (writes the full 40-byte header in
`emit_inline_tlab_new`). Pending rebuild+verify.

## Broader than one class

This is not PluginXmlParser-only. On the quiet optimized binary (loop5) it ALSO breaks
**AntoraAsciidocAttributesTests** — CV-FAIL+ (`passed=20 failed=1` vs HotSpot 21/0): the
test's ~40-entry `LinkedHashMap` of dependency versions silently **loses the
`spring-data-mongodb` entry** (`IllegalArgumentException: No version found for
org.springframework.data:spring-data-mongodb`). Its CV log carries **>4500
`inconsistent header` warnings** — same inline-`new` corruption, but here it manifests
as *silent data corruption* (a wrong test result) rather than a hang. So the bug
produces BOTH hangs AND wrong answers in string/allocation-heavy classes.

## Candidate fix #2 — pin new object across `jit_post_tlab_init` — TRIED, DISPROVEN (reverted)

Hypothesis (A): the inline path commits the object, then `jit_post_tlab_init`'s
`class_manager.read()` lock is a GC quiescence point where the just-committed object
(not in the `new` safepoint's oop map, reachable only via the helper's `obj_ptr` arg) is
freed by the non-moving sweep → reused → `<init>` writes corrupt the reuser. Fix: pin the
object in `thread.native_pin_roots` across `jit_post_tlab_init`. **Result: no effect** —
PluginXmlParser still crashes (rc=1, ~1730 `inconsistent header`). Reverted. So the
unrooted-during-post-init theory is wrong (or the GC window is elsewhere). The helper path
(`jit_new_object`) additionally calls `jit_safepoint_flush_satb` and probes+collects
*before* allocating — that, not post-init rooting, is the load-bearing difference, but the
exact mechanism by which the inline path produces a wild header-overwriting store is still
unpinned.

## Candidate fix #1 — full header write — TRIED, DISPROVEN (reverted)

Hypothesis: the inline path trusts the (known-unreliable) TLAB-zero for header offsets 8
and 20–39 (`gc_flags`, `forwarding_ptr`, `mark_word`); a stale `gc_flags`@21 would desync
the sweep. Fix attempt: write the full 40-byte header before commit. **Result: no effect.**
Rebuilt + tested: AntoraAsciidoc still `passed=20 failed=1` with **4501** `inconsistent
header` warnings (unchanged); PluginXmlParser still crashes. **Reverted.**

## Refined root cause — a wild store overwriting LIVE headers (needs runtime debugging)

Verified by inspection that this is NOT a header-content or size/alignment bug:
- All object sizes are `HEADER_SIZE + n*8` (8-multiples); array sizes use
  `array_data_size` which rounds data up to 8 — and `gen_object_total_size` (the walker)
  uses the SAME rounding, so allocator and walker AGREE on every stride. No gaps.
- The inline path writes a correct, walker-coherent header before committing the cursor
  (TSO-ordered). Writing MORE of it changes nothing.

So the corrupt header the sweep trips on (`kind=Object`, `array_length`=a **heap pointer**)
is a **live object's header field (array_length@12 / num_slots@16) overwritten by an
errant store** — a JIT wild/OOB store landing on a neighbouring object's header, which then
mis-strides the linear sweep into a field region. `CRATONVM_JIT_NO_SPEC_BCE=1` does NOT fix
it (so it is NOT the speculative counted-loop bounds-check elision). Candidates: a
non-speculative array-store / `putfield` offset miscompile, or a callee-saved register
clobbered across the inline-`new` safepoint so a later array-store address is garbage.

**This requires a Windows heap-write watchpoint** on the corrupted header address (or a
debug build that validates every just-allocated object's header on the next safepoint and
logs the writing PC) — it is not pinnable by source inspection. Same archetype/tooling-need
as the `skip_list.rs` `JUnitCore.main` / `ByteBuffer.allocate` inline-alloc bans.

**Working mitigation (reliable):** `CRATONVM_JIT_DISABLE_INLINE_NEW=1` → 0 corruption,
PluginXmlParser PASSES 2/2 (global opt-out of inline object `new`; perf cost).

## Bisection — a targeted skip-list is NOT viable (proven)

`CRATONVM_JIT_BISECT_SKIP` (keep most JIT, skip one package; fast enough to reach the
corruption threshold) over PluginXmlParser:

| Skipped | corruption (default = 9217) |
|---|---|
| `java/lang/` | 1282 |
| `com/sun/org/apache/` (Xerces/Xalan) | 1282 |
| `java/lang/,com/sun/,jdk/,sun/` | 1668 |
| *(global)* `DISABLE_INLINE_NEW` | **0** |

Skipping *any* big allocator drops the count to the same ~1300–1700 floor but never to 0;
the residual comes from whatever is still JIT-compiled. (`BISECT_ONLY` runs all read 0 but
are CONFOUNDED — restricting JIT makes the run too slow to reach the ~enter_count 8360
threshold within the timeout.) Conclusion: the wild store is **not localizable to one
method** — it's the inline-`new` codegen itself, triggered once allocation volume crosses a
threshold (why short version-test classes pass clean and only the long string/XML-heavy
PluginXmlParser + AntoraAsciidoc corrupt). **No single `skip_list.rs` entry fixes it.**

The two real options: (1) make `DISABLE_INLINE_NEW` the default (or a broad skip_list gate)
— reliable, global perf cost; (2) fix the `emit_inline_tlab_new` codegen via runtime
heap-write-watchpoint debugging to pin the wild store — proper, multi-session.

## Investigation state (2026-06-16) — 5 hypotheses ruled out by inspection

Pursuing option (2). Eliminated, each by inspection or test:
1. **Stale/partial header** — wrote the full 40-byte header in `emit_inline_tlab_new`,
   rebuilt+tested: no change (AntoraAsciidoc still 20/21, 4501 warnings). Reverted.
2. **Speculative bounds-check elision** — `CRATONVM_JIT_NO_SPEC_BCE=1` doesn't help.
3. **Single method** — `BISECT_SKIP` of any big allocator drops the count to a ~1300
   floor, never 0; only global `DISABLE_INLINE_NEW`→0. Distributed/volume-triggered.
4. **Un-flushed operand scratch (R8/R9)** — the `new` arm DOES call
   `flush_scratch_registers()` (x64.rs:19100), like `anewarray`/`newarray`.
5. **`aastore` register clobber across its SATB barrier** — the `aastore` arm (x64.rs:13055)
   correctly RELOADS RAX/RCX/RDX from frame slots after the barrier call (13087–13089).

**Signature:** the victim is a live Object whose header offsets 0/12/16 hold **heap
pointers** — i.e. reference-typed stores writing object refs into a *header*, which means
either a store base/offset is wrong OR the victim object was freed-then-reused.

**Two surviving hypotheses (need an instrumented build):**
- **(A) GC-reclaim of the new object during `tlab_post_init`.** The inline path commits the
  object to the heap (bumps cursor) and THEN calls `tlab_post_init`; if a GC runs there and
  the just-committed object (held only in a register / as `tlab_post_init`'s int arg, not
  yet pushed to the operand stack) isn't found by the conservative root scan, the
  non-moving sweep frees it → `RAX` now aliases free/reused space → the field-init writes
  corrupt it. The helper path (`new_object`) pins the object on its Rust frame across the
  same window, which is why `DISABLE_INLINE_NEW` is clean.
- **(B) a `putfield`/array store with a wrong base/index** elsewhere in the JIT, whose
  bad register is only produced once the inline-`new` path runs (a regalloc interaction).

### Exact next-session instrumentation plan
1. Wire a debug helper `jit_debug_validate_header(obj_ptr, expected_class_id)` into
   `JitRuntimeHelpers` (vm/src/jit/helpers.rs + the table). It re-reads `[obj+0]`; on
   mismatch it logs `obj_ptr`, found vs expected class_id, and the current bytecode PC.
2. In `emit_inline_tlab_new`, gated by an env (e.g. `CRATONVM_DBG_VALIDATE_NEW`), emit a
   call to it at the `done` label (RAX=obj) — this tests hypothesis (A) directly: if the
   header is already wrong right after allocation, the object was reclaimed during
   `tlab_post_init`.
3. If (A) is confirmed: fix by spilling the new object to a frame slot (a GC root) BEFORE
   the `tlab_post_init` call and reloading after — or by registering it as a temporary
   root. If (A) is NOT confirmed, add the same validation in the `putfield`/`aastore`
   arms to catch the wild store at its source (hypothesis B).

Code is reverted to dev (the failed header-write fix is removed); `jit/src/x64.rs`
matches dev.

---

**Original root-cause notes:**
**Status (orig):** 🔴 Open — root-caused to JIT inline-TLAB object allocation. Mitigation
known; deep codegen fix outstanding.
**Severity:** CV-only heap corruption → GC walker desync → `ExceptionInInitializerError`
/ AIOOBE / NPE, surfacing in the suite as a **HANG** (the GC churns re-syncing).
HotSpot passes `PluginXmlParserTests` 2/2 in ~2–54s.
**Binary:** sbloop `d760e63e` (dev + the 3 suite fixes), fat-LTO optimized.

## This supersedes HANG-01

Earlier reports called PluginXmlParser a *regex performance hang* (HANG-01). With
the optimized binary fast enough to reach the failure, the real cause is now clear:
**JIT inline-`new` (inline-TLAB object allocation) corrupts the young-gen heap** when
PluginXmlParser's regex/JUnit code crosses the JIT compile threshold. The "hang" is
the non-moving young sweep repeatedly re-syncing over corrupt object headers.

## Symptom (optimized binary, default config)

```
WARN cratonvm_gc::gen_heap: GC: inconsistent header — kind=Object but
  array_length=983230600 (num_slots=1271, class_id=983227944);
  inline-alloc forgot to set kind=Array. Treating as corrupt so the walker can re-sync.
...
<clinit> failed — wrapping in ExceptionInInitializerError
  class=org/junit/platform/commons/util/ExceptionUtils
  cause=java/lang/ArrayIndexOutOfBoundsException Index 111 out of bounds for length 0
Exception in thread "main" java/lang/NullPointerException: Cannot invoke isEmpty on null
```

`array_length=983230600` (0x3A9D…) and `class_id=983227944` (0x3A9C…) are **heap
pointers ~2.6 KB apart**, i.e. two adjacent reference-field cells being misread as a
header — the GC walker landed *inside* an object's field region, meaning a preceding
object's size/header was wrong. The GC's "forgot to set kind=Array" text is a guess;
the real fault is a committed object the walker can't step over correctly.

## Triple-confirmed it's JIT inline-`new`

| Run (PluginXmlParserTests, optimized binary) | Result | Corruption |
|---|---|---|
| default (JIT on) | HANG (300s) / crash when CPU-boosted | **yes** (many) |
| `--nojit` | **PASS 2/2** in 284s | **0** |
| `CRATONVM_JIT_DISABLE_INLINE_NEW=1` | **PASS 2/2** in ~200s | **0** |
| `CRATONVM_JIT_NO_SPEC_BCE=1` | HANG | yes (so NOT bounds-check elision) |

So: JIT on + inline-`new` on ⇒ corruption. Disable JIT **or** just inline-`new` ⇒
clean pass. `CRATONVM_JIT_DISABLE_INLINE_NEW` gates the inline-TLAB `new` path at
`jit/src/x64.rs:19139`; the emitter is `emit_inline_tlab_new` (`x64.rs:9362`).

It is **PluginXmlParser-specific** in this suite — every passing class's CV log is
clean (no `inconsistent header`). Its regex-heavy allocation pattern (Pattern/Matcher
/ node trees + JUnit reflection) is what crosses the threshold and trips it.

## Analysis / why it's subtle

`emit_inline_tlab_new` *looks* correct: it aligns the cursor up to 8, writes the full
header (class_id, kind=Object, array_length=0, num_slots=num_fields) BEFORE committing
`thread.tlab.cursor` (x86-64 TSO ⇒ commit not reordered ahead of header stores). So the
object is walker-coherent at publish. The corruption must be a second-order effect —
candidates:
- a wrong `num_fields` for some class → wrong stride → walker missteps into the next
  object (the observed signature);
- an interaction with the interleaved inline **array** allocation path (which the
  object path's 8-byte cursor re-alignment comment at `x64.rs:9418` explicitly guards
  against — the array path may not align/commit-order symmetrically);
- a callee-saved register clobber across `emit_pre_safepoint_spill` / `tlab_post_init`.

Same archetype as the `skip_list.rs` inline-alloc bans (`JUnitCore.main`,
`ByteBuffer.allocate`, `MessageBytes.newInstance` — "emit_inline_tlab_new writing the
header at a wrong R11/TLAB-cursor"). Needs a Windows heap-write watchpoint on the
corrupt header address to pin the exact faulty store.

## Mitigation (to get the class green now)

`CRATONVM_JIT_DISABLE_INLINE_NEW=1` makes PluginXmlParser pass at ~200s (< the 300s
harness timeout) while keeping JIT for everything else — but it's a global opt-out of
a real optimization. A targeted `skip_list.rs` entry for the offending method would be
better once bisected (the corruptor is in the regex `Pattern`/`Matcher` or JUnit
`ExceptionUtils`/reflection allocation path).

## Repro

```bash
cd apps/spring-boot/buildSrc; CP=$(cat test-classpath.txt); RUNCP="runner;$CP"
CV=C:/craton/CratonVM-sbloop/target/release/cratonvm.exe
# corrupts (grep the log for "inconsistent header"):
"$CV" --java-home "$JDK" -cp "$RUNCP" RunJUnit \
   org.springframework.boot.build.mavenplugin.PluginXmlParserTests
# clean pass:
CRATONVM_JIT_DISABLE_INLINE_NEW=1 "$CV" --java-home "$JDK" -cp "$RUNCP" RunJUnit \
   org.springframework.boot.build.mavenplugin.PluginXmlParserTests   # tests=2 passed=2
```

## Next step

Bisect the corruptor method with `CRATONVM_JIT_BISECT_SKIP` (keep-only) over the
regex + JUnit allocation methods, then either skip-list it or — better — fix
`emit_inline_tlab_new` / the inline array-alloc path. Cross-check the `num_fields`
value passed for the class at the corrupting site, and whether the inline **array**
allocation matches the object path's cursor-alignment + header-before-commit ordering.
```
