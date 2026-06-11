# CratonVM `jit` / `jit-api` crate review — Fable, 2026-06-10

Scope: `jit/src` (18 files, ~54k LOC, x86-64 codegen), `jit/tests` (9 files),
`jit-api/src` (2 files). Static review only — no builds run.

## Summary

The JIT crate is unusually well-documented and shows extensive prior auditing,
especially around the GC-root / safepoint-spill invariant, category-2 value
handling, the bytecode length/branch-target tables, and adversarial switch
parsing (`checked_tableswitch_count` / `checked_lookupswitch_npairs`, the
truncated-header bail-outs). The "never panic, bail to interpreter" contract is
mostly honoured: codegen overflow is handled by the sticky `ExecutableBuffer`
flag, and the unsupported-opcode reject in `jit_scan` (`_ => return None`) keeps
the compiler off any bytecode it cannot model.

The one material finding is a **soundness hole in bounds-check elimination for
loops with an inclusive comparison** (`<=` / the `if_icmpgt` exit form):
`analyze_loop_bound` accepts the inclusive comparators but records only the bound
local, and the speculative header guard proves only `array.length >= bound`,
which is off-by-one for an index that reaches `bound`. This is reachable from
ordinary (non-malicious) Java `for (i = 0; i <= n; i++) a[i] = …` and produces an
out-of-bounds heap read/write with the per-element check elided. A second, less
reachable correctness/panic bug is an unclamped `1u64 << local` in
`find_modified_locals` for methods with `max_locals > 64`.

The aarch64 backend (not the production target) contains a dead, stubbed
NEON-pattern detector returning hardcoded local indices. jit-api is a clean
data-structure/contract crate.

---

## Bugs

### B1 (HIGH) — BCE off-by-one for inclusive-comparison loops → OOB heap access
`jit/src/x64.rs:4044` (`analyze_loop_bound`), guard emitted at
`jit/src/x64.rs:12152`, elision at `jit/src/x64.rs:11026`.

`analyze_loop_bound` Pattern A accepts `if_icmpge (0xa2)` **and `if_icmpgt
(0xa3)`** as the exit test (line 4112); Pattern B accepts `if_icmplt (0xa1)`
**and `if_icmple (0xa4)`** as the continue test (line 4122). It records only
`bound_local` in `LoopBoundsInfo` (struct at `x64.rs:3873`) — the comparator is
**not** stored.

For the inclusive forms (`if_icmpgt` exit / `if_icmple` continue) the induction
variable reaches `bound` inclusive, so the maximum index accessed is `bound`,
requiring `array.length >= bound + 1`. The speculative header guard emits
`CMP array.length, bound; JB deopt` (`x64.rs:12182-12190`), proving only
`array.length >= bound`. When `array.length == bound` the guard passes and every
per-element bounds check is elided (`emit_bounds_check` early-returns for PCs in
`bounds_safe_pcs`, `x64.rs:11026`), so `array[bound]` is an out-of-bounds access
one element past the end. For `iastore`/`aastore` this is an out-of-bounds heap
**write**, matching the "kind=Object but array_length set" header-corruption
class this very pass was designed to prevent.

Reachability: standard javac output for `for (int i = 0; i <= n; i++) a[i] = x;`
emits exactly `iload i; iload n; if_icmpgt exit` (or the bottom-test
`if_icmple body`). No malicious bytecode required.

Fix: record the comparator in `LoopBoundsInfo`; for the inclusive forms either
(a) emit the guard against `bound + 1` (with overflow check), or (b) refuse to
mark such accesses safe / refuse the speculative guard. Note the same
`analyze_loop_bound` feeds the SIMD array-sum cleanup loop, so the fix must cover
both static (`find_safe_array_accesses`) and speculative paths.

### B2 (MEDIUM) — `find_modified_locals`: unclamped `1u64 << local` for `max_locals > 64`
`jit/src/x64.rs:3373` and `jit/src/x64.rs:3378`.

```rust
0x36..=0x3a => { modified |= 1 << code[pc + 1]; pc += 2; }   // store, wide index 0..255
0x84        => { modified |= 1 << code[pc + 1]; pc += 3; }   // iinc, index 0..255
```

`code[pc+1]` is a raw local index 0..255. `1u64 << n` for `n >= 64` panics in
debug builds (violating the JIT "never panic" contract) and silently masks to
`n % 64` in release builds, setting the **wrong** modified-bit. Every other
shift-by-local in the file is guarded with `< 64` or `.min(63)` (e.g.
`find_induction_variable` at lines 3978/3997; checks at 3408/3426/3811/4011/4357),
but this function is not. There is no method-wide `max_locals <= 64` reject —
only `compute_local_oop_masks` bails for `max_locals > 64` (line 2065); the BCE
path runs regardless.

Impact: a JIT-eligible method with `max_locals > 64` and an `istore`/`iinc` to a
high local inside a loop. Release-mode consequence is worse than a panic: a
genuinely modified bound/array/IV local at index ≥ 64 may fail to be flagged
modified (its bit lands on `index % 64`), defeating the BCE invariance check and
re-opening the OOB-via-stale-guard path of B1. Fix: clamp with `.min(63)` (and
ideally drop BCE for any access whose array/bound/IV local is ≥ 64, as the
`< 64` call sites already do).

### B3 (LOW) — `wide` (0xc4) missing from `bytecode_len_at` / `bc_len`
`jit/src/x64.rs:1601` (`bytecode_len_at`), `jit/src/regalloc.rs:87` (`bc_len`).

Neither length table has an arm for the `wide` prefix opcode `0xc4`; it falls to
`_ => 1`, which would desync every PC-stepping consumer (branch-target
precompute, `dup2_top_cat2`, DCE, unroll) after a `wide`. Currently **latent**:
`jit_scan` does not accept `0xc4` (it reaches `_ => return None`, `x64.rs:1452`),
so no method containing `wide` is ever compiled. This is a defense-in-depth /
future-proofing note: if `wide` is ever added to `jit_scan`, the length tables
must be updated in lockstep. `find_modified_locals` and `find_induction_variable`
likewise do not decode the `wide` prefix.

### B4 (LOW) — `assert!` in `emit_safe_idiv` can panic
`jit/src/x64.rs:11199` and `jit/src/x64.rs:11203`.

The rel8-range asserts on the INT_MIN/-1 guard branches `panic!` on
out-of-range displacement instead of tripping `buf.overflowed` like the rest of
the codegen. The surrounding block is fixed-size and small, so the comment is
correct that this "can only fire on a genuine codegen bug" — but it is still a
hard panic in a crate whose stated contract is to never panic. Prefer the
`try_patch_byte` → `overflowed` bail pattern used elsewhere.

---

## Vulnerabilities

### V1 (HIGH) — out-of-bounds heap write via inclusive-loop BCE
Same root cause as **B1**. Classified as a vulnerability because the elided
per-element check turns an ordinary `a[i] = …` inside a `i <= n` loop into a
one-past-the-end heap store when `n == a.length`, corrupting the adjacent
object's header — a memory-safety break reachable from benign user bytecode and
trivially from hostile bytecode. See B1 for the file/line breakdown and fix.

(No other memory-safety issues found. The variable-divisor path is guarded
against div-by-zero and INT_MIN/-1 (`emit_safe_idiv`); per-element array checks
use an unsigned compare that catches negative indices (`emit_bounds_check`,
`x64.rs:11035`); the switch parsers reject overflowing/negative counts and
truncated headers; `validate_code_ptr` range-checks pointers before transmute;
the inline TLAB `new` writes a walker-coherent header before publishing the bump
cursor.)

---

## Stubs and Unimplemented

### S1 — `detect_neon_patterns` returns hardcoded local indices (dead path)
`jit/src/aarch64_backend.rs:2452-2464`.

`IntArraySum`/`IntDotProduct` patterns are pushed with
`array_local: 0, accum_local: 0, counter_local: 0` and the comment
"placeholder — full analysis would resolve these". The function is only ever
called from its own tests (`aarch64_backend.rs:3982`, `:3992`); it is not wired
into any codegen path. If it were consumed it would vectorize against the wrong
locals (miscompile). aarch64 is not the production target. Report per policy:
this is a fake/placeholder analysis result.

No `unimplemented!`/`todo!`/`NotImplemented` exist in the in-scope code (the one
grep hit at `x64.rs:20` is a doc comment). Numerous "bail to interpreter"
`return None` sites exist but those are the *correct* fallback behaviour, not
stubs.

---

## Performance

### P1 — `speculative_bce_guards` filtered with a linear scan per loop header
`jit/src/x64.rs:12160`. For every bytecode PC that is a loop header, the whole
`speculative_bce_guards` vector is `.iter().filter().cloned().collect()`-ed.
Typically tiny, but it is O(headers × guards) and clones each matched guard.
A `FxHashMap<header_pc, Vec<guard>>` built once would remove the per-PC scan.

### P2 — `field_info` / `static_field_info` / `invoke_info` linear `find` in `dup2_top_cat2`
`jit/src/x64.rs:5539`, `:5544`, `:5553`. Each `dup2` resolution linearly scans
the per-PC metadata vectors. Fine for small methods; for large methods with many
`dup2`s these are repeated O(n) scans. Index by PC if it shows up in profiles.

### P3 — `canonicalize_stack` parallel-move is O(n²) in stack depth
`jit/src/x64.rs:6013-6019`. The unblocked-move search is `pending.iter().position`
over `pending.iter().any`, i.e. O(depth²) per canonicalize, and canonicalize runs
at merge points. Operand stacks are shallow so this is acceptable, but worth a
note for pathological `max_stack`.

### P4 — `compute_branch_targets` allocates `Vec<bool>` of `code_len`
`jit/src/x64.rs:1742`. One bool per bytecode byte; a bitset (`Vec<u64>` /
`fixedbitset`) would cut this 8×. Minor; it is allocated once per compile.

### P5 — repeated `std::env::var_os` on analysis paths
e.g. `CRATONVM_JIT_NO_SPEC_BCE` read inside `analyze_bounds_elimination`
(`x64.rs:4462`). Most env gates in the file are `OnceLock`-cached
(`precise_jit_maps_enabled`, `shadow_*`); this one is read per call. Cache it in
a `OnceLock<bool>` like the others.

---

## Tests

Estimated coverage: **~70%** of in-scope code; does **not** plausibly reach 85%.

Basis (by reading, not running):
- ~795 `#[test]` functions across the crate; `x64.rs` alone has 199 inline
  tests plus 9 integration suites in `jit/tests` (arraycopy, arrays ops/sort,
  crc32, int/long bits, string access/search, and a `differential.rs` that
  actually executes JIT-compiled FP loops and compares against host IEEE-754).
- Well-covered: switch parser overflow/truncation/negative bailing
  (`test_tableswitch_count_overflow_bails`, `test_lookupswitch_negative_npairs_bails`,
  `…_past_end_bails`, `…_truncated_header_bails`, `test_branch_target_mid_instruction_bails`);
  inline array ops with NPE/AIOOBE behaviour (`test_inline_iaload_null_throws_npe`,
  `test_inline_iaload_out_of_bounds_throws_aioobe`, sign/zero-extend variants);
  loop unroll patterns; constructor putfield init; intrinsics (CRC32, bit ops,
  string access/search, arraycopy/sort); descriptor slot parsers (`lib.rs`
  tests); JitMICSlot/JitPICSlot offset pinning; deopt log/frame reconstruction
  (`deopt.rs`, 49 tests); escape-analysis and IR pipeline.
- BCE is tested **only for the safe exclusive cases**
  (`test_bounds_elimination_loop`, `test_bounds_elimination_javac_pattern` —
  both `if_icmpge`/`if_icmplt`). There is **no** test for the inclusive
  `if_icmpgt`/`if_icmple` forms (B1/V1), nor a negative test asserting that an
  inclusive-bound access keeps its check.

Most important missing tests:
1. **BCE inclusive comparison**: a loop `for (i=0;i<=n;i++) a[i]=…` must NOT be
   in `bounds_safe_pcs` (or the header guard must use `bound+1`). Directly
   covers B1/V1.
2. **`find_modified_locals` with `local >= 64`**: a loop storing/iinc-ing a high
   local — currently panics in debug. Covers B2.
3. **Speculative guard runtime behaviour**: compile-and-run a loop where
   `array.length == bound` and assert deopt vs. correct result (the `vm-tests`
   feature gate exists for compile-and-run; extend it to the boundary case).
4. **Non-unit / negative stride IV** (`find_induction_variable` Priority 2,
   `iadd+istore`): assert a non-positive-stride or non-invariant addend is not
   treated as a clean `0..bound` IV for BCE.
5. **`wide`-prefixed method** reaches the interpreter (lock in the `jit_scan`
   reject so a future regression that accepts `wide` also fixes the length
   tables — B3).

---

## Feature Suggestions

1. **Record the loop comparator in `LoopBoundsInfo`** and thread it through BCE
   so inclusive bounds are handled correctly rather than rejected — this both
   fixes B1 and *widens* BCE coverage to `<=` loops safely.
2. **Centralize the bytecode length table.** `bytecode_len_at` (x64.rs),
   `bc_len` (regalloc.rs), and the inline length switch in
   `aarch64_backend.rs:2469` are three hand-maintained copies that already drift
   (none handle `wide`). One shared `pub(crate) fn` consumed everywhere removes
   the drift class entirely.
3. **`debug_assert` lockstep invariants in BCE** like the operand-stack code
   already does for `stack.len() == stack_oop_marks.len()`: assert every local
   index fed to a `1 << x` is `< 64`, catching B2-class bugs at their source.
4. **Property/differential testing for BCE**: a small fuzzer that generates
   counted loops (varying comparator, stride, bound source) and checks the JIT
   result against the interpreter would have caught B1 and is cheap given the
   existing `differential.rs` harness.
5. **Wire or delete `detect_neon_patterns`.** Either complete the operand
   analysis (reuse `analyze_array_access_operands`) and connect it to aarch64
   codegen, or remove the dead stub before open-sourcing (S1).
6. **Promote per-method env gates to `OnceLock`** uniformly (P5) and consider a
   single `JitConfig` snapshot read once at compiler construction.

---

## Files sampled vs fully read

Fully read (function-by-function on the risk-relevant regions):
- `jit/src/x64.rs` — read the codegen-critical regions in depth: `bytecode_len_at`,
  `compute_branch_targets`, `checked_tableswitch_count`/`checked_lookupswitch_npairs`,
  `jit_scan` opcode walk, `find_induction_variable`, `analyze_loop_bound`,
  `analyze_bounds_elimination`, `find_safe_array_accesses`,
  `find_speculative_array_accesses`, speculative-guard emission,
  `emit_bounds_check`, `emit_safe_idiv`, `emit_idiv_pow2`/`emit_irem_pow2`/
  `magic_signed_div32`/`emit_idiv_magic`, `emit_inline_tlab_new`,
  operand-stack model (`push_stack`/`pop_stack`/`emit_dup_top_slot`/`dup2_top_cat2`),
  safepoint spill/oop-map (`emit_pre_safepoint_spill`/`emit_oop_map_for_safepoint`/
  `emit_post_safepoint_reload`), `canonicalize_stack`, `find_modified_locals`,
  `emit_simd_int_array_sum`, plus the BCE/switch test modules. Structure of the
  rest skimmed via the full fn list. (29k lines — not every emit helper read
  line-by-line.)
- `jit/src/lib.rs` — descriptor/slot parsers, `ExecutableBuffer`,
  `JitCodeRegion`/`validate_code_ptr`, Value-layout probes, `CompileError`,
  category-2 detection. Structure of the cache/PIC/MIC sections skimmed.
- `jit-api/src/lib.rs` — `JitRuntimeHelpers::validate`/`null_pointers`, test
  fixtures (read fully; clean).
- `jit-api/src/gpu_lowering.rs` — read fully (clean trait/contract).
- `jit/src/regalloc.rs` — `bc_len` read fully; rest of file structure surveyed.
- `jit/src/deopt.rs` — fn list + frame reconstruction / `add_merge_predecessor`
  unwrap checked; large inline test module noted.
- `jit/src/ir.rs` — `unreachable!`/`unwrap` sites verified safe (exhaustive
  matches; `ensure_merge` precedes the `get_mut().unwrap()`).
- `jit/src/aarch64_backend.rs` — `detect_neon_patterns` (S1) and `to_reg`/
  `to_fpreg` conversions read; rest surveyed for stub/TODO markers.
- `jit/tests/differential.rs` — harness read; other `jit/tests/*` inventoried by
  test-name grep.

Sampled (grep + targeted reads only): `aarch64.rs`, `escape_analysis.rs`,
`ir_lower.rs`, `ir_optimize.rs`, `ir_schedule.rs`, `loop_analysis.rs`,
`null_check_elim.rs`, `pgo.rs`, `platform.rs`, `profile.rs`, `scev.rs`,
`tiered.rs`. These are either secondary/analysis pipelines (the production
backend is `x64.rs`) or lower-risk; grepped for `unsafe`/`todo!`/`panic!`/
`unwrap` and shift-by-local patterns, none of which surfaced additional
reachable issues within the time budget.
