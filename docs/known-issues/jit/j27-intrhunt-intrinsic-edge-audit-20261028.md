> **j27 wave 8 (lane W8REV, 2026-10-28): REVIEWED, no open finding, PENDING HOST VERIFICATION.** The page has no open finding list: its one side note (the `synthetic-jdk` `parseUnsigned*` natives) is outside both runtime policies. Re-checked off-host (Python, JDK formulas) the rows most likely to be mis-derived: `hashcode` and `hashcode-twice` (all six inputs), `roundf` (all 13), `compareto` (all 10, incl. the surrogate `-10178`) and `comparetoignorecase` (`µ` 859, `ÿ` 158, `ß` 126): every value matches the list below. New rows touching this audit's area went to lane W8REV's `J27W8LoopIntrinsicCallRows` (the `fill` / `sort` / `CRC32C` sites the optimizing tier keeps as calls under the opt-in `CRATONVM_JIT_IR_LOOP_INTRINSIC_CALL`; see the intrinsic-coverage page). Verify as below; nothing else changed.

> **j27 wave 5 (lane INTRHUNT, 2026-10-28): AUDIT, NO CODE CHANGE, PENDING HOST VERIFICATION.**
> A read-through of the JIT's intrinsic expansions in both tiers against the JDK 25 Java bodies found no
> HotSpot-parity defect in shipping code (list below). What it leaves is a regression probe,
> `tools/probes/jit/j27/J27IntrinsicEdgeRows.java`, which runs every reviewed edge input through a warmed
> (compiled) call site. No switch.
> **Verify:** `java -cp <dir> J27IntrinsicEdgeRows > hs.txt` on HotSpot 25, then
> `cratonvm --cp <dir> J27IntrinsicEdgeRows | diff - hs.txt` in `--jdk-only` and `--compatible`, with
> defaults and with `CRATONVM_JIT_IR_SCALAR_INTRINSICS=0` (single-pass rows only). Expect no diff. Also
> diff `hs.txt` against the list below. Those values were worked out from the JDK semantics (Python
> reference for the bit, FP and hash rows), not measured, so where `hs.txt` disagrees, HotSpot wins and
> this page needs fixing. With `CRATONVM_DBG_JITC=1`, check that the hot methods (`minF`, `roundD`, `hob`,
> `idx`, `cmpTo`, `copy`, ...) show as compiled.

| | |
|---|---|
| **Status** | OPEN until the host run above (audit page; retire when the probe diffs clean) |
| **Area** | JIT, both tiers: `jit/src/x64/op_invoke.rs` (Math / INT_BITS / LONG_BITS / FP_BITS / SCALAR_ROWS_SP / ARRAYCOPY / ARRAYS_OPS / ARRAYS_SORT / STRING_ACCESS / STRING_SEARCH / CRC32 arms, `emit_sp_math_round`), `jit/src/x64/inlining.rs` (`emit_int_long_minmax_cmov`), `jit/src/x64/string_code_point.rs`, `jit/src/x64/arith.rs` (`emit_fp_to_int_nan_fixup`), `jit/src/ir_lower.rs` (`ScalarIntrinsic` arm, `lower_fp_math_scalar`, `lower_fp_fma_scalar`, `fp_minmax_avx_bytes`, `emit_math_round_scalar`, `try_emit_ir_string_search_prefix` and its helpers, `try_emit_ir_latin1_compare_to_ci_prefix`), `jit/src/ir/pure_expand.rs`, `jit/src/ir/latin1_char_class.rs`, resolvers `jit/src/lib.rs` `try_resolve_intrinsic` / `sp_scalar_row` / `ir_arrays_equals_call_admits`, `jit/src/ir.rs` `try_ir_scalar_intrinsic` |
| **Reference** | JDK 25 bodies of `Math`, `StrictMath`, `Integer`, `Long`, `String`, `StringLatin1`, `StringUTF16`, `Arrays`, `CharacterDataLatin1`; HotSpot `TypeArrayKlass::copy_array` / `ObjArrayKlass::copy_array` / `JVM_ArrayCopy` messages |

## What was checked, and what it showed

Every item was checked byte for byte against the JDK body. Each one matches.

* `Math.min` / `max` for float and double. Single-pass: two MINSS/MAXSS, OR/AND, then the NaN-argument fixups.
  IR: UCOMIS three-way, plus the opt-in AVX blend form. NaN rule: a NaN `a` is returned first, else `b`.
  Signed zeros: min(+0, -0) is -0 in either order, max is +0. Register encodings were decoded by hand.
* `Math.abs` for all widths: the sign mask keeps the NaN payload, and `abs(MIN_VALUE)` is `MIN_VALUE`. The
  int result is MOVSXD-sign-extended.
* `Math.round`, both tiers: the JDK bit algorithm with shift 1074 / 149, `(shift & -64) == 0` as one
  unsigned compare, and a saturating CVTT fallback with NaN -> 0 and positive overflow -> MAX.
* `floor` / `ceil` / `rint` via ROUNDSD with imm 9 / 10 / 8. `StrictMath`'s three stay a call (NaN bits).
* `sqrt` via SQRTSD.
* `fma` (VFMADD213SD / SS: xmm0 = xmm1*xmm0 + xmm2, one rounding). VEX bytes were decoded.
* `multiplyHigh` / `unsignedMultiplyHigh` (IMUL / MUL r64, result from RDX).
* `Integer` / `Long`: `bitCount` (POPCNT, gated), `numberOfLeading` / `TrailingZeros` (LZCNT / TZCNT, or
  BSR / BSF + CMOVZ), `highestOneBit` / `lowestOneBit`, `reverse` (SWAR + BSWAP; IR pure expansion),
  `reverseBytes`, `rotate*` (hardware count masking), `compare`. Every negative-int result is re-extended
  in the single-pass tier.
* `copySign` / `signum` pure expansions (`StrictMath.copySign` correctly excluded).
* `Character` surrogate / code-point tests.
* `System.arraycopy`, single-pass: null / kind / element-type guards, then the five bounds checks in 64-bit,
  the zero-length exit, and whole-element `REP MOVS*` with an overlap-aware backward copy. Every bail goes
  to the native or direct helper, which owns HotSpot's exception texts (`types/src/error.rs`
  `arraycopy_message`).
* `Arrays.fill` (REP STOS by width), `Arrays.equals` (integral only: float / double stay calls because of
  NaN canonicalisation and -0.0), and `Arrays.sort` (insertion sort up to 47, heapsort above).
* `String.hashCode`, both tiers: hash cache plus `hashIsZero`.
* `String.equals`: an array or non-String argument answers false or declines; a coder mismatch declines.
  REPE CMPSB of zero length still answers true: ZF comes from the equal-length CMP.
* `String.compareTo`: unsigned byte and char differences in every coder pair, else the length difference.
* `String.indexOf(I)` / `indexOf(II)`: `canEncode` for LATIN1, a lone surrogate matched as one unit,
  supplementary code points matched as a pair at `i < len - 1`, invalid code points -> -1, and `from` clamped
  to `[0, len]`.
* `codePointAt` / `Before` / `Count` and the `StringBuilder` code-point rows.
* `StringLatin1.compareToCI`: the fold maps 0xB5 -> 0x3BC and 0xFF -> 0xFF.
* `CRC32C.update`.

Seen in passing, outside the shipping tree: the synthetic-JDK-only natives in
`native-builtins/src/phases_late.rs` `register_p71_wrapper_extras` (`#[cfg(feature = "synthetic-jdk")]`
via `register_synthetic_overrides`) parse `Integer.parseUnsignedInt` / `Long.parseUnsignedLong` with
`trim().parse().unwrap_or(0)`. That returns 0 where the JDK throws `NumberFormatException`, and accepts
surrounding whitespace. Neither runtime policy registers them (no row in
`tools/scripts/baselines/jdk-only-kind-map-25-linux.tsv`), so no page was filed.

## Expected HotSpot 25 output of `J27IntrinsicEdgeRows` (stdout, in order)

```
fact int-bitcount 0,1,32,1,31,13,1,4
fact int-nlz 32,31,0,0,1,3,24,8
fact int-ntz 32,0,0,31,0,3,7,20
fact int-highestonebit 0,1,-2147483648,-2147483648,1073741824,268435456,128,8388608
fact int-lowestonebit 0,1,1,-2147483648,1,8,128,1048576
fact int-reverse 0,-2147483648,-1,1,-2,510274632,16777216,3840
fact int-reversebytes 0,16777216,-1,128,-129,2018915346,-2147483648,61440
fact int-rotl-neg1 0,-2147483648,-1,1073741824,-1073741825,152709948,64,7864320
fact int-rotr-33 0,-2147483648,-1,1073741824,-1073741825,152709948,64,7864320
fact int-compare0 0,1,-1,-1,1,1,1,1
fact int-abs 0,1,1,-2147483648,2147483647,305419896,128,15728640
fact int-min-neg7 -7,-7,-7,-2147483648,-7,-7,-7,-7
fact int-max-neg7 0,1,-1,-7,2147483647,305419896,128,15728640
fact long-bitcount 0,1,64,1,63,32,1
fact long-nlz 64,63,0,0,1,7,56
fact long-ntz 64,0,0,63,0,0,7
fact long-highestonebit 0,1,-9223372036854775808,-9223372036854775808,4611686018427387904,72057594037927936,128
fact long-lowestonebit 0,1,1,-9223372036854775808,1,1,128
fact long-reverse 0,-9223372036854775808,-1,1,-2,-597899502893742976,72057594037927936
fact long-reversebytes 0,72057594037927936,-1,128,-129,-1167088121787636991,-9223372036854775808
fact long-rotl-65 0,2,-1,1,-2,163971058432973790,256
fact long-rotr-neg1 0,2,-1,1,-2,163971058432973790,256
fact long-compare0 0,1,-1,-1,1,1,1
fact long-abs 0,1,1,-9223372036854775808,9223372036854775807,81985529216486895,128
fact long-min-neg7 -7,-7,-7,-9223372036854775808,-7,-7,-7
fact long-max-neg7 0,1,-1,-7,9223372036854775807,81985529216486895,128
fact multiplyhigh 4611686018427387904,0,-1,4611686018427387903,0,6609981178781634,-1
fact unsignedmultiplyhigh 4611686018427387904,-2,0,4611686018427387903,9223372036854775807,6609981178781634,1
fact minf 80000000,80000000,80000000,7fc00000,7fc00000,3f800000,ff800000,bf800000
fact maxf 0,0,80000000,7fc00000,7fc00000,40000000,7f800000,80000000
fact mind 8000000000000000,8000000000000000,8000000000000000,7ff8000000000000,7ff8000000000000,3ff0000000000000,fff0000000000000,bff0000000000000
fact maxd 0,0,8000000000000000,7ff8000000000000,7ff8000000000000,4000000000000000,7ff0000000000000,8000000000000000
fact absf 0,7fc00123,3f800000,7f800000,1
fact absd 0,7ff8000000000123,4004000000000000,7ff0000000000000,1
fact roundf 0,0,1,-2,3,0,2147483647,-2147483648,8388609,0,0,2147483647,-1
fact roundd 0,0,3,-2,4503599627370497,9223372036854775807,-9223372036854775808,0,0,0,-9223372036854775808
fact floor 8000000000000000,bff0000000000000,0,c000000000000000,4000000000000000,c008000000000000,4008000000000000,7e37e43c8800759c,bff0000000000000,4320000000000000
fact ceil 8000000000000000,8000000000000000,3ff0000000000000,bff0000000000000,4008000000000000,c000000000000000,4010000000000000,7e37e43c8800759c,8000000000000000,4320000000000002
fact rint 8000000000000000,8000000000000000,0,c000000000000000,4000000000000000,c000000000000000,4010000000000000,7e37e43c8800759c,8000000000000000,4320000000000000
fact strict-floor 8000000000000000,bff0000000000000,0,c000000000000000,4000000000000000,c008000000000000,4008000000000000,7e37e43c8800759c,bff0000000000000,4320000000000000
fact strict-ceil 8000000000000000,8000000000000000,3ff0000000000000,bff0000000000000,4008000000000000,c000000000000000,4010000000000000,7e37e43c8800759c,8000000000000000,4320000000000002
fact strict-rint 8000000000000000,8000000000000000,0,c000000000000000,4000000000000000,c000000000000000,4010000000000000,7e37e43c8800759c,8000000000000000,4320000000000000
fact sqrt 8000000000000000,3ff6a09e667f3bcd,4000000000000000,7ff0000000000000,1e60000000000000,20ca2fe76a3f9475
fact sqrt-neg1-isnan true
fact fmad 3c90000000000000,4024000000000000,8000000000000000,0
fact fmaf 32800000,41200000,80000000,0
fact indexof -1,-1,2,0,-1,1,-1,2,-1,-1,1
fact indexof-from 4,0,-1,3,-1,1,-1
fact compareto 132,-1,1,-3,1,-1,-10178,-1,127,127
fact comparetoignorecase 859,158,-1,0,126
fact equals true,false,true,true,true,false,false,false
fact hashcode -2147483648,0,0,1772899,227742,7722850
fact hashcode-twice 4294967296,0,0,4859210,595268,18108740
fact char-counts 10,117,127,10,56,61
fact char-edges true,false,false,true,false,true,false,false,true,true
fact arrays-equals true,false,false,true,true,false,false,true,false,true,true
fact arrays-fill 33,-256,java.lang.NullPointerException
fact arraycopy-overlap [0,0,1,2,3,4,5,6,7,8],[0,1,2,3,4,5,6,7,8,8],[1,2,3,2,3,4],ababcd
fact arraycopy-throw-0 ArrayIndexOutOfBoundsException:arraycopy: source index -1 out of bounds for int[4]
fact arraycopy-throw-1 ArrayIndexOutOfBoundsException:arraycopy: destination index -1 out of bounds for int[4]
fact arraycopy-throw-2 ArrayIndexOutOfBoundsException:arraycopy: length -1 is negative
fact arraycopy-throw-3 ArrayIndexOutOfBoundsException:arraycopy: last source index 5 out of bounds for int[4]
fact arraycopy-throw-4 ArrayIndexOutOfBoundsException:arraycopy: last destination index 5 out of bounds for int[4]
fact arraycopy-throw-5 ArrayIndexOutOfBoundsException:arraycopy: last source index 2147483648 out of bounds for int[4]
fact arraycopy-throw-6 ArrayStoreException:arraycopy: type mismatch: can not copy int[] into long[]
fact arraycopy-throw-7 ArrayStoreException:arraycopy: type mismatch: can not copy object array[] into int[]
fact arraycopy-throw-8 ArrayStoreException:arraycopy: source type java.lang.String is not an array
fact arraycopy-throw-9 ArrayStoreException:arraycopy: destination type java.lang.String is not an array
fact arraycopy-throw-10 ArrayStoreException:arraycopy: element type mismatch: can not cast one of the elements of java.lang.Object[] to the type of the destination array, java.lang.String
fact arraycopy-throw-11 ArrayIndexOutOfBoundsException:arraycopy: source index -1 out of bounds for short[4]
fact arraycopy-throw-12 no-throw
fact arraycopy-null-src java.lang.NullPointerException
```

The order follows `main`: the int and long bit rows, the FP rows (min/max, abs, round, the six roundings,
sqrt, fma), strings, chars, arrays, then the arraycopy throw rows. In the FP block the probe prints
`floor`, `ceil`, `rint` and then the three `strict-*` rows, which is the order of the list above. stderr
carries one `sink` line, which is not a fact.

## What would fix it

Nothing is known to be broken. If the host diff shows a row that differs, file it under its own
`j27-<slug>-20261028.md` page with the failing row's tier, found with `CRATONVM_JIT_IR_SCALAR_INTRINSICS=0`
and with `--nojit`.
