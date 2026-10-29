> **j27 wave 9 (lane W9VER, 2026-10-29): VERIFIED (Windows) for facts and unit tests; the page stays open as the coverage audit (its remaining rows are proposals / perf).** Windows, release LTO-off build of the tree at `47479dc6e0` plus the other wave-9 lanes' WIP (`C:\craton\j27-ver-bin\cvm-ver-2.exe`, test exes from the same tree), HotSpot 25.0.3 as the reference. `cratonvm-jit --lib`: `ir::pure_expand` 15, `ir::bit_compress` 6, `int_compare` 6, `ir_loop_intrinsic_call` 2 passed. `J27IntrinsicRows` (57), `J27W8IntrinsicRows` (18) and `J27W8LoopIntrinsicCallRows` (9 facts) equal HotSpot in `--jdk-only` and `--compatible` at the defaults and with the opt-ins `CRATONVM_JIT_IR_INT_COMPARE_EXPAND=1` / `CRATONVM_JIT_IR_LOOP_INTRINSIC_CALL=1`. Facts only; no timing taken (perf is out of scope this wave).

# JIT intrinsic coverage against HotSpot 25's C2 intrinsics (j27 audit)

> **j27 triage (builder lane TRIB, 2026-10-28, Windows, release LTO-off build of the j27 tree at 20:34 = `C:\craton\j27-trib-bin\cvm-trib-1.exe`): fp-classify VERIFIED (keep default ON); compress/expand FLIP RECOMMENDED.** `ir::pure_expand` 15 and `ir::bit_compress` 6 tests pass. The host has BMI2 (hybrid Intel). `J27IntrinsicRows` 57 facts equal HotSpot in default, `CRATONVM_JIT_IR_FP_CLASSIFY_EXPAND=0`, `CRATONVM_JIT_IR_BIT_COMPRESS_EXPAND=1` and `--nojit`, both modes. `pure-expansions` lines name IsNaN/IsInfinite/IsFinite D and F. With `=1`, `scalar-intrinsics J27IntrinsicRows.compressI/compressL/expandI/expandL: 1 site(s) lowered as arithmetic`. Microbench `C:\craton\j27-trib-bin\bench\TribCompressBench2.java` (7 interleaved runs, P-cores): Long compress^expand per element drops from 63.7 ns to 1.64 ns (HotSpot 0.56). The int row stays at 18.9 / 18.6, because its caller `ci` is spliced into `ti` and the nested compress calls are spliced as JDK bodies. See the new page `j27-scalar-intrinsic-lost-inside-spliced-callee-20261028.md`.

> **j27 wave 8 (lane W8INTR, 2026-10-28): PARTLY (two more missing rows FIXED IN CODE, IR tier, opt-in), PENDING HOST VERIFICATION.**
> `Integer.compareUnsigned(II)I` / `Long.compareUnsigned(JJ)I` (HotSpot 25 `_compareUnsigned_i` / `_l`, C2 `CmpU3` /
> `CmpUL3`; missing from the table before) and `Integer.signum(I)I` / `Long.signum(J)I` (not C2 intrinsics, but here
> registered `Intrinsic` natives in `--compatible`, `phases_late::register_p71_wrapper_extras`, so a funnel call per op;
> `BigDecimal.signum` reaches `Long.signum`) are four new `PureExpansion` rows, each ONE existing three-way compare node:
> `compareUnsigned(x, y)` = `CompareI/L(x ^ MIN, y ^ MIN)` (the JDK body's `compare(x + MIN_VALUE, y + MIN_VALUE)`;
> `+ MIN` is `^ MIN`), `signum(v)` = `CompareI/L(v, 0)` (equal to `(i >> 31) | (-i >>> 31)` for every `i`, `MIN_VALUE`
> included). No new lowering (`ScalarOp::CompareI/L` is `CMP; SETG; SETL; SUB`), no planner or builder change: the
> existing `try_ir_pure_expansion` site (static calls) picks them up. New module `jit/src/ir/pure_expand_int_compare.rs`
> (`row`, `operand_type`, `compare_op`, `Emit::compare_unsigned` / `integral_signum`), wired by small arms in
> `jit/src/ir/pure_expand.rs` (enum variants, `arity`, `anchor`, `expand`, one early return in `try_ir_pure_expansion`).
> Switch `CRATONVM_JIT_IR_INT_COMPARE_EXPAND` (token `ir-int-compare-expand`), **default OFF, `=1` on** (unmeasured;
> also under `CRATONVM_JIT_IR_SCALAR_INTRINSICS`). Single-pass tier unchanged (native in `--compatible`).
> Rows still not taken, and why: `Integer` / `Long` `divideUnsigned` / `remainderUnsigned` (registered natives in
> `--compatible` too, but the zero divisor must throw `ArithmeticException: / by zero` from inside the JDK method, which
> a pure expansion cannot; it needs a guard that deopts to the invoke); `Integer.toUnsignedLong` / `Integer.sum` (no
> `ScalarIntrinsic` node to carry a first-call guard; the planner would have to refuse those at first-call sites);
> `Float.float16ToFloat` / `floatToFloat16` (registered `Intrinsic` with HotSpot's `VCVTPH2PS` NaN answer; an exact
> expansion exists (`intBitsToFloat((h & 0x7fff) << 13) * 0x1p112f` plus a NaN/inf select) but no hot JDK path reaches
> it).
> Tests (`cargo test -j 5 -p cratonvm-jit --lib ir::pure_expand::int_compare`):
> `j27_w8intr_expansions_answer_the_jdk_bodies_on_edge_inputs` (10 x 10 edge pairs per width: 0, +-1, +-2, MIN, MAX,
> MIN+1, high-half patterns, against the javadoc's unsigned order and the literal JDK signum bodies),
> `j27_w8intr_each_row_builds_its_anchor_once_as_the_int_result`, `j27_w8intr_rows_need_the_switch_and_their_exact_triples`,
> and (x86_64) `j27_w8intr_lowered_rows_compute_the_jdk_answers` (built, unoptimized and optimized, LOWERED, run). All
> fail before this change (the variants did not exist). The existing `ir::pure_expand::tests` must still pass.
> Probe `tools/probes/jit/j27/J27W8IntrinsicRows.java` (HotSpot 25 lines in its header: 13 literal `cmpu-*` rows,
> `signum-int` / `signum-long 0,1,-1,1,-1,-1,1,-1,1,-1`, `cmpu-mismatches 0`, `signum-mismatches 0`, `warm true`).
> **Verify:** (1) the unit tests; (2) `java -cp <dir> J27W8IntrinsicRows > hs.txt`, then `cratonvm --cp <dir>
> J27W8IntrinsicRows | diff - hs.txt` in `--jdk-only` and `--compatible`, unset and `CRATONVM_JIT_IR_INT_COMPARE_EXPAND=1`;
> (3) engagement with `=1`: `CRATONVM_DBG_IR_COMPILES=1` prints `[ir] pure-expansions J27W8IntrinsicRows.cmpuI(II)I:
> [CompareUnsignedI]` (and `cmpuL`, `signumI`, `signumL`); (4) `-Dj27.time=true` rows `cmpu-int`, `signum-long`, unset vs
> `=1`, `--compatible`, P-core pinned, interleaved x5. Flip rule: default ON if `=1` is not slower in either mode and
> the diff is empty in every arm (expected lower in `--compatible`: unset is a funnel call per op).

> **j27 wave 8 (lane W8REV, 2026-10-28): PARTLY (the three "method REFUSED" rows FIXED IN CODE, opt-in), PENDING HOST VERIFICATION.**
> The highest-cost gap in the table was not a missing intrinsic but a refusal: a method holding an `Arrays.fill`
> (integral), `Arrays.sort` (integral, whole array) or `CRC32C.update` (`(I)V`, `([BII)V`) site could not reach the
> optimizing tier at all, because the planner refuses every single-pass loop intrinsic site it has no node for.
> New module `jit/src/ir_loop_intrinsic_call.rs` (`admits`, `SWITCH`) lists those 13 triples; one `&& !` term in the
> planner's refusal (`jit/src/lib.rs`, next to `ir_arrays_equals_call_admits`, the r22 shape for `Arrays.equals`) keeps
> them as an ordinary call row instead, the shape a cold OSR intrinsic site already gets. The callee is the JDK body
> (or its registered native), so answers and exceptions are the JDK's by construction; nothing is emitted inline.
> Switch `CRATONVM_JIT_IR_LOOP_INTRINSIC_CALL` (token `ir-loop-intrinsic-call`), **default OFF, `=1` on** (a trade:
> the method gains C2, the site loses the inline `REP STOS` / sort / CRC loop to a call; unmeasured).
> Tests (`cargo test -j 5 -p cratonvm-jit --lib ir_loop_intrinsic_call`): `j27_w8rev_loop_intrinsic_rows_need_the_switch`,
> `j27_w8rev_every_loop_row_is_a_single_pass_intrinsic` (fails if a row drifts off `try_resolve_intrinsic`).
> Probe `tools/probes/jit/j27/J27W8LoopIntrinsicCallRows.java` (HotSpot 25 lines in its header: `fill-mismatches 0`,
> `sort-mismatches 0`, `crc32c-check e3069283`, `crc32c-mismatches 0`, four exception-class rows, `warm true`).
> **Verify:** (1) the unit tests; (2) the probe in both modes, unset and `=1`, diffed against HotSpot 25; (3) engagement
> with `=1`: `CRATONVM_DBG_IR_COMPILES=1` names `fillRow` / `sortRow` / `crcRow` compiled by the optimizing tier, and
> unset `CRATONVM_DBG_JITC=1` prints their "loop-shaped or memory call-site intrinsic" refusal; (4) `-Dj27.time=true`
> rows `fill`, `sort`, `crc32c`, unset vs `=1`, P-core pinned, interleaved x5. Flip rule: default ON only if no row
> is slower and a workload with such sites in a hot method (e.g. `ArrayList.clear` callers, `zip` CRC32C users)
> shows a gain; a site-level split (keep `fill` refused, admit `sort`) is a valid outcome.

> **j27 wave 6b (lane T1BCE, 2026-10-28): PARTLY (`compress` / `expand` FIXED IN CODE, IR tier, opt-in), PENDING HOST VERIFICATION.**
> `Integer.compress(II)I` / `Long.compress(JJ)J` / `Integer.expand(II)I` / `Long.expand(JJ)J` are `ScalarOp::CompressI/L`,
> `ExpandI/L`, lowered to one BMI2 `PEXT` / `PDEP rAX, rAX, rCX` (value in RAX, mask in RCX; `VEX.LZ.F3/F2.0F38.W0/W1
> F5 /r` = `C4 E2 7A|FA|7B|FB F5 C1`). New module `jit/src/ir/bit_compress.rs` owns the rows (`bit_compress_row`), the
> switch and the encoding (`pext_pdep_rax_rax_rcx`); small arms in `jit/src/ir.rs` (`ScalarOp` variants, `result_type`,
> `operands_are_long`, `as_str`, a `try_ir_scalar_intrinsic` arm, the `every_declared_family_is_recognised` signature
> match) and `jit/src/ir_lower.rs` (the `ScalarIntrinsic` arm, which also refuses the node with a bailout on a host
> without BMI2). CPU gate: new `x64::has_bmi2()` (`std::is_x86_feature_detected!("bmi2")`, no new static; BMI2 is a GP
> VEX family and needs no OS vector state). Without BMI2 or with the switch off the site stays the JDK body. Switch
> `CRATONVM_JIT_IR_BIT_COMPRESS_EXPAND` (token `ir-bit-compress-expand`), **default OFF, `=1` on** (unmeasured; PEXT/PDEP
> are microcoded on AMD before Zen 3), under `CRATONVM_JIT_IR_SCALAR_INTRINSICS`.
> Tests (`cargo test -j 5 -p cratonvm-jit --lib ir::bit_compress`): `the_encodings_are_the_sdm_bytes`,
> `the_encodings_decode_field_by_field`, `the_rows_need_the_switch_and_bmi2_and_their_exact_triples`,
> `the_rows_type_their_operands_as_the_descriptors`, `the_reference_meets_the_jdk_identities`, and (x86_64 with BMI2 only)
> `lowered_rows_compute_the_jdk_answers_on_edge_masks` (12 x 12 edge values per width: 0, -1, alternating, single low /
> high / middle bits, `0x80..01`, nibbles, mixed). Also `ir::tests::every_declared_family_is_recognised` must still pass.
> Probe: `J27IntrinsicRows` gains `compress-mismatches` and 14 literal facts (block below, computed from the javadoc
> definition) and timing row `compress-l`.
> **Verify:** (1) the unit tests (on a BMI2 host, so the executed test runs); (2) the probe in both modes, unset and with
> `CRATONVM_JIT_IR_BIT_COMPRESS_EXPAND=1`, diffed against HotSpot 25; (3) engagement with the switch on:
> `CRATONVM_DBG_IR_COMPILES=1` names `J27IntrinsicRows.compressL(JJ)J` compiled with no call at the site (or
> `CRATONVM_DBG_JITC=1` disassembly shows `pext`); (4) `-Dj27.time=1` row `compress-l`, unset vs `=1`, P-core pinned,
> interleaved x5 -- flip to default ON only if `=1` is faster on the host CPU and the facts are equal.

> **j27 wave 6 (lane T1BCE, 2026-10-28): PARTLY (one more missing row FIXED IN CODE, IR tier), PENDING HOST VERIFICATION.**
> `Double` / `Float` `isNaN` / `isInfinite` / `isFinite` (static `(D)Z` / `(F)Z`) join `jit/src/ir/pure_expand.rs`:
> `PureExpansion::IsNaND/F`, `IsInfiniteD/F`, `IsFiniteD/F`, each one raw-bits read plus `|bits| cc INF`
> (`>` NaN, `==` infinity, `<` finite; `Emit::fp_class`, mapping `PureExpansion::fp_class`). Exact for every bit
> pattern (JDK 25 bodies: `v != v`, `v == +/-INFINITY`, `Math.abs(v) <= MAX_VALUE`). No planner or builder change:
> the existing `try_ir_pure_expansion` site in `jit/src/lib.rs` (static calls only) picks the rows up. Switch
> `CRATONVM_JIT_IR_FP_CLASSIFY_EXPAND` (token `ir-fp-classify-expand`), **default ON, `=0` kills** (trivially exact,
> like this module's other rows; also under `CRATONVM_JIT_IR_SCALAR_INTRINSICS`). It removes a native-funnel call per
> test in `--compatible`; in `--jdk-only` it replaces the JDK body's FP compare. Single-pass tier unchanged (still the
> native in `--compatible`).
> Tests (`cargo test -j 5 -p cratonvm-jit --lib ir::pure_expand`): `classification_expansions_answer_the_jdk_predicates_on_edge_inputs`,
> `classification_expansions_read_the_bits_once_and_answer_an_int`,
> `classification_rows_match_their_static_triples_and_their_switch_kills_them`,
> `lowered_classification_expansions_compute_the_jdk_answers` (x86_64: built, optimized and unoptimized, lowered,
> run on zeros, infinities, quiet/signalling/negative NaNs, subnormals, `MAX_VALUE`). All fail before this change
> (the variants and the triples did not exist).
> Probe: `J27IntrinsicRows` gains `classify-mismatches`, `classify-d`, `classify-f` (block below) and timing row
> `isfinite-d`; its `-Dj27.time=1` now works as the header says (it used to need `=true`).
> **Verify:** (1) the unit tests; (2) the probe in both modes, unset and with `CRATONVM_JIT_IR_FP_CLASSIFY_EXPAND=0`,
> diffed against HotSpot 25; (3) engagement: `CRATONVM_DBG_IR_COMPILES=1` prints
> `[ir] pure-expansions J27IntrinsicRows.isNaN(D)Z: [IsNaND]` (and the five siblings); (4) `-Dj27.time=1` row
> `isfinite-d`, unset vs `=0`, `--compatible` (expect lower: `=0` is a funnel call per op).
> Rows left missing, and why (`compress` / `expand`: built in wave 6b, block above):
> `Float.float16ToFloat` / `floatToFloat16` (JDK bodies, no funnel; a rounding-exact expansion is long);
> `Long.divideUnsigned` / `remainderUnsigned` (JDK body, no funnel, and its divide-by-zero throw would need a
> guard); `Thread.onSpinWait` (JDK body is empty; only a `PAUSE` is missing, not observable); `Arrays.fill` /
> `CRC32C` in the IR tier (memory / loop intrinsics, not pure expansions). None of them is named by an open perf page.

> **j27 wave 2 (lane INTRINS, 2026-10-28): PARTLY (three missing rows FIXED IN CODE), HOST-VERIFIED s4 (2026-10-28).**
> Host facts (orchestrator log, snapshot s4): engagement -- the pure-expansion compile lines print (5 rows); the
> s4 battery's J27 probes are SAME in both modes, including the intrinsics `=0` arm. Not shown by the log: the
> per-switch `=0` arms one by one and the step (4) timing rows.
> New module `jit/src/ir/pure_expand.rs`: the optimizing tier expands `Integer.reverse(I)I` / `Long.reverse(J)J`,
> `Math.copySign(DD)D` / `(FF)F` and `Math` / `StrictMath.signum` (`D`, `F`) into plain IR arithmetic (the JDK 25
> bodies' bit operations over existing nodes, ending in the existing `ReverseBytes*` / raw-bits `ScalarOp`s). Wired
> at the scalar-intrinsic points only: planner `jit/src/lib.rs` (next to `ir::try_ir_scalar_intrinsic`, map
> `ir_pure_expansion_sites`, handed over with `IrBuilder::set_pure_expansions`), builder `jit/src/ir.rs` (the
> `try_emit_scalar_intrinsic` dispatch also asks `try_emit_pure_expansion`). Switches, all **default ON, `=0`
> kills**: `CRATONVM_JIT_IR_INTEGER_REVERSE_EXPAND`, `CRATONVM_JIT_IR_MATH_COPYSIGN_EXPAND`,
> `CRATONVM_JIT_IR_MATH_SIGNUM_EXPAND` (all under `CRATONVM_JIT_IR_SCALAR_INTRINSICS`). Unit tests
> `cratonvm-jit --lib ir::pure_expand::tests` (graph evaluation on edge inputs, the real builder, and the LOWERED
> machine code unoptimized and optimized). Probe `tools/probes/jit/j27/J27IntrinsicRows.java` (facts below).
> **Verify:** (1) `cargo test -j 5 -p cratonvm-jit --lib pure_expand`; (2) the probe in both modes, unset and with
> each switch `=0`, diffed against HotSpot 25; (3) engagement: `CRATONVM_DBG_IR_COMPILES=1` prints
> `[ir] pure-expansions J27IntrinsicRows.rev(I)I: [ReverseI]` (and the `copySign` / `signum` callers), and with
> `CRATONVM_JIT_IR_INTEGER_REVERSE_EXPAND=0` the `rev(I)I` compile reports the old
> "loop-shaped or memory call-site intrinsic" refusal again; (4) `-Dj27.time=1` rows `reverse-int`, `copysign-d`,
> `signum-d`, unset vs `=0` (expect each lower: the `=0` arm is a native-funnel call per op).

| | |
|---|---|
| **Status** | OPEN (audit page: the "missing" rows below are the work list) |
| **Area** | JIT, both tiers: `jit/src/lib.rs` `try_resolve_intrinsic` / `sp_scalar_row` (single-pass), `jit/src/ir.rs` `try_ir_scalar_intrinsic` / `try_ir_unbox_intrinsic`, `jit/src/ir/pure_expand.rs`, `ir_string_search_prefix_kind` (IR inline prefixes), the thin direct-call helpers (`direct_helpers.rs`, `vm/src/jit/helpers/fast_native.rs` `direct_native_shadows!`), the `splice_intrinsic_jdk_body_*` rows (IR splice of the JDK body over a registered `Intrinsic`) |
| **Reference** | HotSpot 25 `vmIntrinsics.hpp` / C2 `library_call.cpp` |

## Legend

* **both** -- each tier answers the site without a generic native-funnel crossing (inline code, or a thin direct
  helper call where HotSpot has a stub).
* **SP only** / **IR only** -- one tier; the other pays a funnel call (or, for a single-pass `JitIntrinsic` the IR
  tier has no node for, the IR planner REFUSES THE WHOLE METHOD: "loop-shaped or memory call-site intrinsic").
* **splice** -- the IR tier splices the JDK's own bytecode body (a reviewed row admits it over the registered
  `Intrinsic` native); the single-pass tier keeps a call.
* **helper** -- a thin direct call to a Rust helper (not inline; cheaper than the funnel, dearer than C2's code).
* **native** -- a registered `Intrinsic` native: a funnel crossing (~100-200 ns) from compiled code in both tiers.
* **missing** -- no tier does anything better than the native or the plain JDK bytecode.

## Coverage table

| HotSpot intrinsic (C2) | CratonVM single-pass | CratonVM IR | Verdict | Open perf pages it matters for |
|---|---|---|---|---|
| `ArraysSupport.vectorizedHashCode` (`Arrays.hashCode`, `String.hashCode` of a fresh string) | helper (`vectorized_hash_code_helper`) | helper (same, optimizing door) | both (helper) | -- |
| `ArraysSupport.vectorizedMismatch` (`Arrays.mismatch`, `equals` ranges) | helper | helper (`vectorized_mismatch_helper`) | both (helper) | regex `Matcher` region compares (r23-w2) |
| `Arrays.equals` integral overloads | inline (`ArraysEquals1/2/4/8`) | inline prefix (`IrStringSearchKind::ArraysEquals`) | both | -- |
| `Arrays.fill` primitive | inline `REP STOS` (`ArraysFill*`) | none: method REFUSED (loop-shaped intrinsic); **call under opt-in `CRATONVM_JIT_IR_LOOP_INTRINSIC_CALL` (j27 w8)** | SP only | -- |
| `Arrays.sort` integral whole-array | inline sort (`ArraysSort*`) | method REFUSED; **call under the same opt-in (j27 w8)** | SP only | -- |
| `Arrays.copyOf` / `copyOfRange` (`_copyOf`, `_copyOfRange`) | JDK body + `System.arraycopy` intrinsic | JDK body spliced; `Array.newArray` helper; inline byte arraycopy opt-in | both (no fused stub) | split / regex: `Arrays.copyOfRange` pinned single-pass by its String intrinsic (r23-w2, r25 steady-state) |
| `System.arraycopy` | inline (`ArraycopyPrimitive`) | inline (`ir_arraycopy_entry`) | both | -- |
| `String.equals` / `compareTo` / `indexOf` / `hashCode`; `StringLatin1` / `StringUTF16` `indexOf` / `compareTo` / `inflate` / `compress`; `String.indexOf(II)` | STRING_SEARCH / STRING_ACCESS intrinsics | inline prefixes (`ir_string_search_prefix_kind`; `inflate([BI[CII)V`, `getChars`, `IndexOfCharFrom` default ON) | **owned by lane STRRX; listed only** | split / regex / charset decode (r23-w2, r23-w5, small-jdk-method-call) |
| `StringCoding.countPositives` / `hasNegatives`; `implEncodeAsciiArray`; `implEncodeISOArray` | JDK body | inline prefix (`CountPositives`, `EncodeAsciiArray`); ISO array: JDK body | IR only (ISO encode missing) | `CharsetDecoder.decode` ASCII / Latin-1 runs (small-jdk-method-call); STRRX's area |
| `CRC32.update` / `updateBytes0` / `updateByteBuffer0`, `Adler32.updateBytes` | helper (`zip_checksum_helper`) | helper | both (helper; `ByteBuffer` forms native) | -- |
| `CRC32C.updateBytes` / `update(I)` | inline SSE4.2 (`Crc32cUpdate*`) | none: method REFUSED; **call under opt-in `CRATONVM_JIT_IR_LOOP_INTRINSIC_CALL` (j27 w8)** | SP only | -- |
| `Math.addExact` / `subtractExact` / `multiplyExact` / `incrementExact` / `decrementExact` / `negateExact` / `toIntExact` | native (leaf funnel) | splice of the JDK body; the cold `athrow` arm is a guard that deopts into the callee frame (`CRATONVM_JIT_SPLICE_INTRINSIC_JDK_BODY_EXACT`, `_MULTIPLY_EXACT_LONG`) | IR only | `HttpClient` deadline arithmetic (closed r20 page) |
| `Math.floorDiv` / `floorMod` (not a C2 intrinsic) | native | splice (`_FLOOR`) | IR only | `java.time` |
| `Math.fma` | inline FMA3 / helper | `ScalarOp::FmaD/F` (opt-in `CRATONVM_JIT_IR_ROUND_FMA_BITCOUNT_OPS`) | both (IR opt-in) | -- |
| `Math.abs` / `min` / `max` (all widths), `sqrt`, `floor` / `ceil` / `rint` (SSE4.1), `round`, `multiplyHigh` / `unsignedMultiplyHigh` | inline | `ScalarOp` (sqrt / roundings / products opt-in; FP min/max default ON) | both (some IR rows opt-in) | -- |
| `Math.signum` (`_dsignum`, `_fsignum`) | native | **`pure_expand` SignumD/F (j27 w2)** | IR only (was missing) | numeric code |
| `Math.copySign` (`_copySignD`, `_copySignF`) | native | **`pure_expand` CopySignD/F (j27 w2)**; `StrictMath.copySign` deliberately not (NaN sign rule) | IR only (was missing) | numeric code |
| `Math.sin` / `cos` / `tan` / `exp` / `log` / `log10` / `pow` / `tanh` / `cbrt` (stubs) | native (libm) | native | native | -- |
| `Integer` / `Long` `bitCount`, `numberOfLeading/TrailingZeros`, `reverseBytes`, `rotate*`, `highest/lowestOneBit`, `compare` | inline | `ScalarOp` (`bitCount` opt-in) | both | -- |
| `Integer.reverse` (`_reverse_i`) | inline (`IntReverse`) | **`pure_expand` ReverseI (j27 w2)**; before: method REFUSED | both (IR was refused) | -- |
| `Long.reverse` (`_reverse_l`) | native | **`pure_expand` ReverseL (j27 w2)** | IR only (was missing) | -- |
| `Integer` / `Long` `compress` / `expand` (`PEXT` / `PDEP`) | JDK loop | **`ScalarOp::Compress/Expand I/L` -> BMI2 PEXT / PDEP (j27 w6b, opt-in `CRATONVM_JIT_IR_BIT_COMPRESS_EXPAND`, BMI2 hosts)**; else JDK loop | IR only, opt-in | -- |
| `Long.divideUnsigned` / `remainderUnsigned` | JDK body (`--jdk-only`); native (`--compatible`) | same | missing (the zero-divisor throw needs a deopt guard; see the wave-8 block) | -- |
| `Integer` / `Long` `compareUnsigned` (`_compareUnsigned_i` / `_l`) | native (`--compatible`) / JDK body | **`pure_expand` CompareUnsignedI/L -> one `CompareI/L` (j27 w8, opt-in `CRATONVM_JIT_IR_INT_COMPARE_EXPAND`)** | IR only, opt-in | -- |
| `Integer` / `Long` `signum` (not C2 intrinsics; C2 inlines the body) | native (`--compatible`) / JDK body | **`pure_expand` SignumI/L -> `CompareI/L(v, 0)` (j27 w8, same opt-in)** | IR only, opt-in | `BigDecimal.signum` |
| `CharacterDataLatin1.isDigit` / `isLowerCase` / `isUpperCase` / `isWhitespace` (`Character.is*` of a Latin-1 char) | native (`Character.is*` registered `Intrinsic`) | native | missing | regex `CharPredicate`s, `split("\\s+")` (r23-w2, r23-w5) -- see proposal below |
| `Character.isHighSurrogate` / `isLowSurrogate` / `charCount` / `isBmpCodePoint` / `isValidCodePoint` (not C2 intrinsics) | inline | `ScalarOp` | both | UTF-8 encode |
| `Float` / `Double` `isInfinite` / `isFinite` / `isNaN` (`_floatIsInfinite` ...) | JDK body under `--jdk-only` (`WRAPPER_BOX_BYTECODE_TRIPLES`), else native | **`pure_expand` IsNaN/IsInfinite/IsFinite D/F (j27 w6)** | IR both modes; SP still native in compatible mode | -- |
| `Float.float16ToFloat` / `floatToFloat16` | JDK body | JDK body | missing | -- |
| `Double.doubleToRawLongBits` / `longBitsToDouble`, `Float` twins | inline | `ScalarOp` | both | -- |
| `Thread.currentThread` / `currentCarrierThread` | helper | inline mirror read (`CurrentThreadMirror`) / helper | both | -- |
| `Object.getClass` | helper / inline compare folds | inline (`ClassWordOf`, `getClass() == C.class` fold) | both | -- |
| `Object.hashCode` / `System.identityHashCode` | inline | inline prefix | both | -- |
| `Class.isInstance` / `isAssignableFrom` / `isArray` / `isPrimitive` / `getModifiers` / `cast` | call | `isInstance` / `isAssignableFrom` inline prefix; rest call | partly (IR) | -- |
| `Unsafe` get / put / CAS / volatile families, `fullFence` | helper (`direct_native_shadows!`) | helper; `ScopedMemoryAccess` and `VarHandle` forms inline | both (helper for raw `Unsafe`) | -- |
| `Reference.get` / `refersTo0` | `get()`: call (not audited further); `refersTo0`: helper | inline expansion (`try_reference_get_intrinsic`, `Weak`/`SoftReference` exactly, SATB keep-alive) / helper | IR inline, SP helper | -- |
| `Preconditions.checkIndex` (`Objects.checkIndex`) | helper (thin, declines to the JDK body for the throw) | helper; range-check elimination keys on it | both (helper) | regex / split bounds (r23-w2) |
| `Preconditions.checkFromToIndex` / `checkFromIndexSize` (not C2 intrinsics) | JDK body | JDK body (spliced) | n/a | r25 steady-state single-pass bodies |
| `BigInteger.implMulAdd` / `implSquareToLen` / `implMontgomeryMultiply` / `implMultiplyToLen` | native (`biginteger_intrinsics.rs`) | native | native (kernel in Rust, funnel per call) | -- |
| `Base64.Encoder.encodeBlock` / `Decoder.decodeBlock` | native (whole `Base64` coder registered) | native | native | -- |
| AES / GHASH / SHA / MD5 / ChaCha20 / Poly1305 kernels | native (`crypto_intrinsics.rs`, reviewed `Intrinsic`s) | native | native (kernel in Rust) | -- |
| `Thread.onSpinWait` (`PAUSE`), `Blackhole.consume*` | JDK body | JDK body | missing | -- |

## What this says for the open perf pages

The regex / split / charset-decode rows (r23-w2, r23-w5, small-jdk-method-call) are NOT intrinsic-coverage gaps
outside the String family: their non-string intrinsic calls (`checkIndex`, `vectorizedMismatch`, `arraycopy`,
`countPositives`) are all served. Their residuals are call overhead and steady-state single-pass bodies, as
those pages say. The one row that touches them and is still missing is the Latin-1 `Character.is*` family
(regex `CharPredicate`s such as `\\s`, `\\d`, `Character::isWhitespace` lambdas): proposal
`j27-latin1-character-class-expand-proposal-20261028.md`.

## Expected HotSpot 25 output of `J27IntrinsicRows` (stdout, in order)

```
fact int-reverse-1 -2147483648
fact int-reverse-12345678 510274632
fact int-reverse-0f0f0f0f -252645136
fact int-reverse-min 1
fact long-reverse-1 -9223372036854775808
fact long-reverse-0123456789abcdef -597899502893742976
fact long-reverse-neg2 9223372036854775807
fact reverse-mismatches 0
fact copysign-d-mismatches 0
fact copysign-d-neg0 bff0000000000000
fact copysign-d-nanpayload fff8000000000123
fact copysign-d-nansign bff0000000000000
fact strict-copysign-d-nansign 3ff0000000000000
fact copysign-f-neg0 c0200000
fact copysign-f-nanpayload ffc00123
fact signum-mismatches 0
fact signum-d-neg0 8000000000000000
fact signum-d-pos0 0
fact signum-d-nanpayload fff8000000000123
fact signum-d-minsub 3ff0000000000000
fact signum-d-neginf bff0000000000000
fact signum-f-neg bf800000
fact signum-f-nan 7fc00000
fact strict-signum-d-neg bff0000000000000
fact classify-mismatches 0
fact classify-d 001,001,001,001,010,010,100,100
fact classify-f 001,001,001,001,010,010,100,100
fact compress-mismatches 0
fact int-compress-cafebabe-ff00 186
fact int-expand-ba-ff00 47616
fact int-compress-neg1-hilo 3
fact int-expand-neg1-hilo -2147483647
fact int-compress-allmask 305419896
fact int-expand-zeromask 0
fact int-compress-odd-even 0
fact int-compress-neg1-even 65535
fact int-expand-ffff-odd 1431655765
fact long-compress-hi32 19088743
fact long-expand-hi32 -8526495043115024384
fact long-compress-neg1-hilo 3
fact long-expand-1-min -9223372036854775808
fact long-expand-alt -9186630160900511574
fact addexact-int java.lang.ArithmeticException: integer overflow
fact addexact-long java.lang.ArithmeticException: long overflow
fact multiplyexact-int java.lang.ArithmeticException: integer overflow
fact multiplyexact-long java.lang.ArithmeticException: long overflow
fact negateexact-int java.lang.ArithmeticException: integer overflow
fact incrementexact-long java.lang.ArithmeticException: long overflow
fact tointexact java.lang.ArithmeticException: integer overflow
fact floordiv-zero java.lang.ArithmeticException: / by zero
fact checkindex-high java.lang.IndexOutOfBoundsException: Index 5 out of bounds for length 3
fact checkindex-neg java.lang.IndexOutOfBoundsException: Index -1 out of bounds for length 3
fact checkindex-neglen java.lang.IndexOutOfBoundsException: Index 0 out of bounds for length -1
fact checkfromto java.lang.IndexOutOfBoundsException: Range [2, 1) out of bounds for length 5
fact checkfromsize java.lang.IndexOutOfBoundsException: Range [2, 2 + 4) out of bounds for length 5
fact exact-in-range 42 -21 -5
fact warm true
```

The literal values were computed off-host (bit-reversal and IEEE bit patterns), not from a HotSpot run: the
host run must diff the probe against a real HotSpot 25 first and correct this block if a line differs.

## What would close it

Each "missing" / "SP only" row is an independent item; in rough value order: the Latin-1 `Character.is*`
family (regex), `Arrays.fill` and `CRC32C` in the IR tier (today they refuse the method), `compress` /
`expand` (BMI2), the `Float`/`Double` classification rows in compatible mode, `onSpinWait`.
