# JIT double-codegen bugs behind the commons-math `transform` suite

**Date:** 2026-06-11
**Fix commit:** `caeb7f6c` on `dev` — `fix(jit): XMM0-clobber in double/float binops + FastMath trig skip-list`
**Bug-3 root cause + fix:** branch `fix/jit-fastmath-trig-miscompile` (2026-06-11, later session) — see the RESOLVED section at the end.
**Files:** `jit/src/x64.rs`, `jit/src/regalloc.rs`, `vm/src/jit/skip_list.rs`, `vm/src/jit/disasm.rs`

This documents three distinct JIT defects found while root-causing why the Apache
Commons Math `commons-math-transform` test suite fails under CratonVM with the JIT
on. Bug 3 — originally banned as "deep, not localizable" — has since been
root-caused and FIXED (it was the `regalloc.rs::bc_len` length-table desync, not a
codegen bug at all); the ban is lifted. Bug 4 (the FFT-construction NPE) is still open.

## TL;DR

| # | Bug | Status |
|---|-----|--------|
| 1 | `emit_double_binop`/`emit_float_binop` clobber XMM0 without flushing a live XMM0 operand (`f(x)+f(g(x))` corruption) | **FIXED** (caeb7f6c) |
| 2 | Inline entry doesn't flush caller-live scratch/XMM operands before emitting the callee body | **FIXED** (caeb7f6c, defensive) |
| 3 | commons-math3 `FastMath` trig family (sin/cos/tan/...) miscompiled — deep, multi-method, not localizable | **FIXED, ban lifted** (`regalloc.rs::bc_len` missing ldc/ldc_w/ldc2_w — see RESOLVED section) |
| 4 | `FastFourierTransformerTest.testAdHocData` NPE: "Cannot write field 'normalization' because the object is null" (FFT ctor under JIT) | **OPEN** (unchanged by the Bug-3 fix) |

Suite result moved **46/56 → 53/56** under JIT (HotSpot/TornadoVM: 56/56; commons-math
full Maven reactor on HotSpot/TornadoVM: 3204/0). Remaining 3 = 2× `testTransformReal`
(precision, see Bug 5) + 1× `testAdHocData` (Bug 4). After the Bug-3 fix with the ban
lifted: **54/56 and 52/56 across reruns** (the `testTransformReal` precision family is
flaky in every config incl. `--nojit`; `testAdHocData` is the one stable JIT-only fail).

## How the suite is exercised / how to reproduce

Clean classpath (drive-letter paths so `java.exe` resolves them; MSYS `/c/...` paths
are NOT resolvable by the native exe and silently drop jars — that footgun cost an hour):

```
CP = .bench-cache/junit-platform-console-standalone-1.10.2.jar
   ; apps/_test-suites/commons-math/commons-math-transform/target/{classes,test-classes}
   ; apps/_test-suites/commons-math/commons-math-core/target/classes
   ; <m2>/commons-numbers-core-1.3 ; commons-numbers-complex-1.3
   ; commons-rng-simple-1.7 ; commons-rng-client-api-1.7 ; commons-rng-core-1.7
   ; commons-math3-3.6.1
cratonvm --java-home <jdk25> -cp "$CP" \
  org.junit.platform.console.ConsoleLauncher execute \
  --select-package org.apache.commons.math4.transform --details=summary --disable-banner
```

Cross-config baseline (clean CP):
- HotSpot: **56/56**
- CratonVM `--nojit`: **52/56** (only the 4 `testTransformReal` precision fails)
- CratonVM JIT-on (before fix): **46/56** (structural fails + an NPE)
- CratonVM JIT-on (after caeb7f6c): **53/56**

The structural failures (`testSinFunction` in Sine/Cosine/Fourier, `testSample`,
`testTransformFunction`) are all downstream of Bug 3: the tests sample
`SIN = x -> new org.apache.commons.math3.analysis.function.Sin().value(x)` →
`FastMath.sin`, so wrong sine values poison the FFT/FST/FCT inputs.

---

## Bug 1 — XMM0 clobber in FP binops (FIXED)

### Symptom
`f(x) + f(g(x))`-shaped code miscompiled (wrong value) or crashed under JIT, where a
double result is live on the operand stack while the next argument runs FP math.

### Minimal deterministic repro (no commons-math needed)
```java
static double id(double x){ return x; }
static double t(double x){ return id(x) + id(x + 1.0); }   // JIT: WRONG (should be 2x+1)
static double ok(double x){ return id(x) + id(x); }        // JIT: correct
```
Triangulation (each in its own loop, JIT vs HotSpot):
- `id(x) + id(x+1.0)` → FAIL · `id(x+1.0) + id(x)` → OK · `a=id(x);b=id(x+1.0);a+b` (locals) → OK · `id(x)+id(x)` → OK
- Trigger = **a value live on the operand stack across a call whose argument is a COMPUTED double** (not a plain `dload`).

### Root cause
The operand stack uses a deferred-spill model (`StackSlot::{Frame,CalleeSaved,Scratch,Xmm}`
in `jit/src/x64.rs`). A call result (or `push_from_rax_as_xmm0`) can be parked in
**XMM0**. `emit_double_binop`/`emit_float_binop` load `slot1 → XMM0` as scratch
**without first flushing a value still live deeper on the stack in XMM0** — destroying
it before it's consumed. `emit_fcmp` already guarded this with `flush_xmm0_slots()`;
the binops did not. (Only XMM0 needs flushing: no code path ever parks a stack operand
in XMM1 — it's a transient temp.)

### Fix
Add `self.flush_xmm0_slots();` in `emit_double_binop` and `emit_float_binop` after
popping the two operands, before loading them. Covers dadd/dsub/dmul/ddiv + float
equivalents.

### Validation
All minimal repros (id/leaf/DoubleMin/DoubleInline/W/U/P/HP-22-doubles/CW) match
HotSpot; `cargo test -p cratonvm-jit` = **696/0**; bench checksums (arith/fib/sieve/
matrix/**bintrees18=68332206**/vadd) unregressed.

---

## Bug 2 — inline entry doesn't flush caller-live scratch/XMM (FIXED, defensive)

`try_emit_inline_body` emits the callee body, which freely uses the volatile scratch
GPRs (R8/R9) and XMM0-7 — exactly the registers the deferred model parks live values
in — but never flushed the caller's live operands first, despite the codebase rule
"Scratch/XMM must be flushed before any call." An inline IS a call boundary. Added
`self.flush_scratch_registers();` at inline entry (rolled back cleanly on mid-body
bail by the snapshot/restore in `try_emit_inline`). Fixed a synthetic crash class;
note the inline body has no FP-arithmetic handlers (it bails on dadd/dmul), so this
mainly matters for non-FP inlined callees.

---

## Bug 3 — FastMath trig family miscompile (BANNED, NOT FIXED) ⚠ the deep one

### Symptom
commons-math3 `FastMath.sin` is badly miscompiled by the JIT:
- `FastMath.sin(3π/4)` → **1.2252377287349119** (correct: 0.7071067811865476).
- **~97% of inputs wrong** (`fails ≈ 6.2M` of 6.4M calls). `--nojit` and HotSpot are exact.
- Curious: wrong values are consistent and roughly **1.7327× the correct value**
  (`1.2252/0.7071 = 1.7327`), and the same multiplier appears for both `sinQ(π/4)` and
  `cosQ(π/4)` — suggests a structural formula/scaling corruption, not a random clobber.

### Repro (fast, deterministic)
```java
// cp must include commons-math3-3.6.1
import org.apache.commons.math3.util.FastMath;
for k in 0..31: x = k*PI/32; assert |FastMath.sin(x) - Math.sin(x)| < 1e-12   // JIT: ~all fail
```
NOTE: the tests' `Math::sin` (java intrinsic) is fine — only commons-math3
`FastMath.sin` breaks. Using `Math::sin` in a repro hides the bug.

### What is known / ruled out
- **JIT codegen** (interpreter exact, JIT wrong). Not GC (`CRATONVM_SHADOW_STACK=1`
  precise roots did NOT change it). Deterministic for the structural failures.
- **NOT inlining**: the JIT inline body has no FP handlers, so sin/sinQ/polySine are
  normal calls, not inlined (verified: 0 inline-trace hits).
- **NOT Bug 1**: the binop XMM0-flush fix did not change FastMath's wrong value at all.
- **Spread across multiple methods**: `CRATONVM_JIT_BISECT_SKIP` shows **no single OR
  pairwise** method skip fixes it — not sin, not sinQ, not cosQ, not polySine, not
  polyCosine, not `sinQ+cosQ`. Only skipping the **whole family** → `fails=0`. So
  `sin`'s own JIT body (the arg-reduction path) AND `sinQ`/`cosQ` are each independently
  miscompiled. (For x>π/2 sin uses a CodyWaite reduction then `cosQ`; skip-sinQ+cosQ
  still fails @ 3π/4 with cosQ interpreted ⇒ sin's reduction is also wrong.)
- **NOT synthetically reproducible**: every hand-written analog passed under JIT —
  single/two-double params, computed args, daload (static double[] table), d2i,
  the 2^30 Dekker split, object+virtual-double CodyWaite mimic, and a 22-live-double
  high-pressure kernel with a call. The bug needs `sinQ`'s exact shape (~19 live
  doubles + table loads + compensated two-sum/two-product + 2 poly calls).
- **NOT a recent regression**: HEAD~120 and HEAD~200 both already fail (HEAD~600 won't
  build with the current toolchain). The transform suite was historically blocked on an
  unrelated gap and only recently ran under JIT, so "it worked N commits ago" predates
  it ever being exercised. Treat as long-standing.

### Why not fixed
Pinning the exact codegen site needs to **disassemble the generated machine code** for
`sinQ`/`cosQ`/sin — and this JIT has **no disassembler** (no iced/capstone dep, no asm
dump in `CRATONVM_DBG_JITC`). Static analysis + black-box narrowing hit a wall.

### Current mitigation (the ban)
`vm/src/jit/skip_list.rs::is_known_miscompile` now lists:
`org/apache/commons/math3/util/FastMath` × {sin, cos, tan, sinQ, cosQ, polySine,
polyCosine, reducePayneHanek}. They run interpreted (exact, matches HotSpot). Lifted
under `CRATONVM_JIT_ALLOW_PACKAGES=org/apache/commons/` for diagnosis.

### Recommended next steps to actually fix it
1. **Add a disassembler** (iced-x86) behind a debug flag and dump the codegen for
   `sinQ`. The 1.7327× consistent multiplier is the key clue — look for a double term
   added twice / a mis-scaled compensated sum.
2. Reproduce by **copying `FastMath.sinQ` + `polySine`/`polyCosine` + the 5 static
   `double[]` tables verbatim** into a standalone class (the only faithful repro), then
   delete pieces until the miscompile vanishes — pinpointing the op without a disassembler.
3. Suspect area: register allocation / spill of doubles under extreme live-set pressure
   (`jit/src/regalloc.rs` has a separate XMM coloring pass; LOCAL_XMMS = XMM8-15, 8
   regs; sinQ needs ~19 live doubles → forced spills), interacting with the
   compensated two-product (`x * 0x1.0p30`) sequences.

---

## Bug 4 — `testAdHocData` FFT-construction NPE (OPEN)

`FastFourierTransformerTest.testAdHocData` throws, under JIT only:
`java.lang.NullPointerException: Cannot write field 'normalization' because the object is null`.
This is a `putfield normalization` on a null `this` inside `FastFourierTransform.<init>`
(`this.normalization = normalization;`). It is a **different** bug from Bug 3 (object
allocation / constructor codegen, the documented allocate-then-putfield miscompile
family — cf. the HashMap/`Integer.valueOf` bans), and is NOT fixed by the FastMath ban
or the binop fix. Not yet root-caused. Likely an inline-new / TLAB / `dup` issue where
the freshly-allocated objectref is null when the ctor's putfield runs.

---

## Bug 5 — interpreter precision gap (separate, minor, NOT a JIT bug)

`testTransformReal` (FastSine/FastCosine) fails even under `--nojit`: CratonVM's
transform result is accurate to ~13 significant digits vs the test's `1e-14` *relative*
tolerance. Present without the JIT; a numeric-precision issue in the FFT/FST double
arithmetic or FastMath, not the codegen bug. Out of scope for the transform regression.

## Artifacts / how this was measured
- Comparison harness + results: `test-infra/run-session-cmp.sh`, `test-infra/suite-results/`
- `is_known_miscompile` ban + `CRATONVM_JIT_BISECT_SKIP`/`BISECT_ONLY` env hooks live in
  `vm/src/jit/skip_list.rs`.
- The slot model + binop codegen are in `jit/src/x64.rs` (`StackSlot`, `emit_double_binop`,
  `emit_float_binop`, `flush_xmm0_slots`, `flush_scratch_registers`, `try_emit_inline_body`).


---

## RESOLVED — Bug 3 root cause (2026-06-11, branch fix/jit-fastmath-trig-miscompile)

### The bug was never in the trig methods' codegen

`jit/src/regalloc.rs::bc_len` — the register allocator's PRIVATE copy of the
bytecode length table — was missing `ldc` (0x12, 2 bytes), `ldc_w` (0x13,
3 bytes), and `ldc2_w` (0x14, 3 bytes); all three fell through to `_ => 1`.
The x64.rs `bytecode_len_at` twin had been fixed for exactly this (the BC
SPHINCS branch-misalign bug, dev 38f8760) and its comment says "Keep the
regalloc.rs `bc_len` twin in sync" — the twin was never updated.

Every liveness/CFG walk in the allocator therefore stepped INTO the 2-byte
constant-pool index operand of each `ldc2_w` and read it as opcodes:

- **Small CP** (every synthetic repro): index bytes like `0x00 0x2D` decode as
  `nop` + `aload_3` — harmless 1-byte phantoms, the walk resyncs, liveness
  stays conservative. This is why the bug "resisted synthetic reproduction".
- **Huge CP** (the real `FastMath`, ~190+ Double entries each taking TWO pool
  slots): `polySine`'s coefficients sit at #175–#181 → operand bytes
  `0xAF/0xB1/0xB3/0xB5` decode as `dreturn`/`return`/`putstatic`/`putfield` —
  **phantom block terminators / multi-byte ops**. A phantom `dreturn` ends the
  basic block; every later real instruction (including the `dload_0` that uses
  `x` at the end of `polySine`) becomes CFG-unreachable, so its uses are
  invisible to liveness.

The allocator then coalesced two genuinely-live doubles onto one XMM register.
Confirmed by disassembly (the new `CRATONVM_DBG_JIT_DISASM`): in the broken
build, `x` (local 0) and `x2` (local 2) both got **XMM9**, so `polySine`'s
final `p * x2 * x` compiled as `p * x2 * x2`. At ε=π/4−0.75 that returns
−2.6e-7 instead of −7.4e-6; in the real class the corruption lands differently
per method (different index bytes → different phantoms), producing the gross
`sin(3π/4)=1.2252 ≈ (1+costA)·sin(x)` value.

### Why the original bisection said "multi-method, not localizable"

Two compounding artifacts:

1. **Skipping a method shifts which methods compile.** Compile triggers are
   per-call-site counters; once `sin` is JIT'd, its callees' counters freeze
   one short of threshold (the JIT'd caller no longer bumps them). The live
   compile set in any FastMath workload is `{sin, polySine, polyCosine}` —
   `sinQ`/`cosQ` never actually compile. Skip `sin` and suddenly `sinQ`
   compiles (its caller stays interpreted) and breaks the same way: the
   "defect" follows the compile frontier, so no single/pairwise skip fixes it.
2. The defect is a function of **constant-pool index bytes**, not of any
   method's logic — so every verbatim-logic copy with a small pool was clean.

### The fix (all in this branch)

- `jit/src/regalloc.rs::bc_len`: added `0x12 => 2`, `0x13 | 0x14 => 3` (plus
  `0xa8` jsr / `0xa9` ret for full parity with `bytecode_len_at`).
- `jit/src/x64.rs::detect_loops`: the ad-hoc PC advance (missing the same
  opcodes plus field/invoke ops) replaced with `bytecode_len_at` — a desync
  there can fabricate/miss backward branches and feed garbage loop ranges to
  BCE/strength-reduction/LICM.
- `jit/src/x64.rs::estimate_max_stack`: same replacement — it also treated
  tableswitch/lookupswitch as 1-byte and walked their offset tables as
  opcodes; a phantom return zeroes the depth estimate and can UNDER-size the
  operand stack.
- `vm/src/jit/skip_list.rs`: the 8-entry FastMath ban removed (comment kept,
  marked RESOLVED with the root cause).
- **New diagnostic:** `CRATONVM_DBG_JIT_DISASM=<substr,substr|*>` dumps
  iced-x86 NASM disassembly of compiled methods at every compile path
  (`vm/src/jit/disasm.rs`) — this is what pinned the XMM coalescing.
  `CRATONVM_DBG_JIT_LDC=1` traces ldc2_w constant resolution.

### Regression tests

- `regalloc::tests::bc_len_constant_loads` — pins the constant-load lengths.
- `regalloc::tests::polysine_high_cp_indices_do_not_coalesce_live_doubles` —
  the exact published `FastMath.polySine` bytecode with high CP indices;
  asserts locals 0/2 never share an XMM register. Both FAIL against the old
  table (verified by temporary revert).
- `bench/FmRepro.java` / `FmRepro2.java` / `FmRepro5.java` — verbatim FastMath
  sine-family copies: small CP (passes pre-fix), real-jar slot layout, and
  CP-padded (~#187+, reproduces pre-fix); `RealFm.java` / `DocShape.java`
  drive the real commons-math3 jar.

### Validation

- 6.3M-input sweep `FastMath.sin(x) == Math.sin(x)` (RealFm, real jar,
  FastMath JIT-compiled): fails 0 (pre-fix: 1,239,509).
- transform suite, FastMath JIT-compiled: 54/56, 52/56 across reruns —
  identical failure set to the banned/nojit configs (testAdHocData = Bug 4;
  testTransformReal = the flaky Bug-5 precision family).
- `cargo test -p cratonvm-jit --release`: 696/0 + all integration tests.
- bintrees18 checksum unchanged (68332206 = HotSpot).

### Follow-up candidates (not done here)

The same desync family may explain other standing bans that were diagnosed as
"unlocalizable codegen bugs" on big-constant-pool classes — notably the
`java/util/HashMap` put/get/resize ban, the Spring `ClassUtils.<clinit>`
crash, the BC RBC.1 blanket ban, and the JUnitCore.main stopgap. Each should
be retested with the fixed length tables before assuming its root cause.

---

## RESOLVED — Bug 4 root cause (2026-06-11, same branch, follow-up session)

`testAdHocData` (and the whole `FastFourierTransformerTest` class) now passes
under JIT: suite is 54/56 across reruns with ONLY the Bug-5 `testTransformReal`
interpreter-precision flakes remaining.

### Root cause: JIT frame overflow from a spill-cursor ratchet

The invoke-dispatch emission sites (`jit/src/x64.rs`, invokestatic and
invokevirtual/special/interface) carve their outgoing args buffer at the
current `next_spill_offset` watermark, then after the call "reclaimed" the
cursor to `pre_pop_spill` — the WITH-ARGS depth — and pushed the return value
on top. Net effect: every non-void dispatch left the cursor `n_args` slots
above the true operand depth. Across testAdHocData's ~19 call sites the
cursor crept ~12 slots past the spill region, so the FFT-constructor call's
args buffer landed at `[rbp-0x178..0x188]` while the prologue had reserved
only `sub rsp, 0x170`: the buffer sat BELOW RSP, where the dispatch helper's
own CALL/prologue immediately overwrote it — the helper then read receiver=0
from its own clobbered stack and the interpreted 2-arg ctor got an all-zero
frame ("Cannot write field 'normalization' because the object is null").
Proven by: `[JIT_ALLOC]` (alloc returned a valid object) vs `[JIT_DISPATCH]`
(arg0=0x0 two loads later) + the `CRATONVM_DBG_JIT_DISASM` dump showing the
buffer offsets past the prologue reservation.

### Why it was so hard to see

The method executed as JIT code through the FIRST-CALL compile path inside
`invoke_method_shared` — which had NO `DBG_JITC` logging — so reflectively
invoked test methods compiled and ran invisibly (5,400+ silent JIT entries in
one ConsoleLauncher run). `BISECT_SKIP`-ing every *logged* compile changed
nothing, which made the bug look like "JIT-infrastructure, not codegen".
Three observability holes fixed alongside: first-call path and the
upgrade-path `callee_compiler` closure now log (`first-compile` /
`callee-compile`) and feed `CRATONVM_DBG_JIT_DISASM`; the closure also now
applies the static skip list (S-HIB.1 twin — it previously compiled complex
`<init>` bodies every other path bans, observed on `Pattern.<init>`).

### The fix (both required)

- **Cursor:** restore `next_spill_offset` to the POST-pop level after each
  dispatch (the args buffer is dead once the helper returns; the return value
  now lands at its semantic depth). Two sites.
- **Frame sizing:** reserve `max_stack_estimate + max(num_jit_args over
  invoke sites)` spill slots so the worst-case args buffer always fits inside
  `sub rsp, frame_size` (it could previously also overlap the callee-saved
  save area).

### New diagnostics from this hunt

`CRATONVM_DBG_NULLTHIS` (frame stack + locals on null-receiver putfield),
`CRATONVM_DBG_JIT_ALLOC=<class_id>` (JIT allocation tracing),
`[JIT_DISPATCH]` arg dumps (pre-existing `CRATONVM_DBG_JIT_DISPATCH`).

### Validation

FastFourierTransformerTest 10/10; transform suite 54/56 ×2 (remaining = Bug-5
flakes); `cargo test -p cratonvm-jit` 698/0; bench checksums unchanged
(bintrees18=68332206, sieve250k, matrix600, fib44); FastMath RealFm sweep
still 0 fails; FillProbe/ChmScale/HashMapProbe/ParseProbe HotSpot-identical in
DEFAULT env (NETTY.1 Arrays.fill + W2-CHM Integer/Long.valueOf bans lifted);
pool probes kafka/spring/tomcat/felix/lucene PASS.
