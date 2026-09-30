# JIT round 14 wave 2, lane compat2: proposals

Ranked. Each is behind its own default-on kill switch when it lands, per the owner's 2026-09-27
decision (correctness fixes on in `--compatible` too).

## CP2-1. One `Compatible`-mode retirement table instead of per-registrar `if`s

**What.** This wave retired eight `Optional` rows under `--compatible` with an `if` around each
`register` call (`native-collections` `optional_functional_left_to_bytecode`), the same shape as
`beans_change_support_left_to_bytecode` (wave 9). Every future `--compatible` retirement repeats
it. Move the decision into `NativeMethodRegistry::register` itself: a const table
`COMPATIBLE_RETIRED_TRIPLES` in `native-api` (beside `retired_shadow.rs`, a subset of its rows by
construction, asserted by a unit test), refused at registration when `real_jdk()` and the mode is
`Compatible`, unless `unretire::is_excluded` names the row or a per-table switch is off. The
refusal is recorded like the `JdkOnly` ones so `--dump-native-registry` shows it.

**Benefit.** One place to read which JDK methods run as bytecode in `--compatible`; the
`CRATONVM_UNRETIRE_NATIVE_SHADOW` pairing becomes automatic; the stub census (once it sets
`real_jdk`, see `r14w2-compat2-stub-ratchet-census-never-sets-real-jdk-FIXED-20260929.md`) moves
exactly by the table's size. **Cost.** ~80 lines in `registry.rs` plus moving the two existing
sites. **Risk.** Low: registration-time only, no dispatch change. **First step.** Land the
stub-ratchet `set_real_jdk(true)` patch so the move is measurable, then port the `Optional` rows.

## CP2-2. Leave `LinkedHashSet.reversed()` to the JDK's `ReverseLinkedHashSetView` under `--compatible`

**What.** The residual of `r13w8-hashcompat3-...` item 3. Stop registering
`native_lhs_reversed` on a real JDK in `--compatible` (the CP2-1 / `optional_functional_...`
pattern). The view's `size`, `add*`, `get*`, `remove*` then delegate to the live source natives,
and `iterator()` re-snapshots per call through `native_lhm_key_set` + `native_view_reversed`.
**Blocker:** `toArray()` is real `LinkedHashMap.keysToArray(.., true)` walking heap `tail` /
`before`; the natives mirror only `head` / `tail` / `size` / `table` to the heap
(`lhm_get`), so per-node `before` must be proven current (or `keysToArray` registered natively).
**Benefit.** A live, write-through reversed set; closes an observable `--compatible` divergence.
**Cost.** One gated registration plus, possibly, one `keysToArray` native. **Risk.** Medium
(wrong array contents if `before` is stale). **First step.** Probe row: `s.reversed().toArray()`
after `remove` + `addFirst` on the source, `--compatible`, with and without the registration.

## Round 14 wave 3 (lane compat3): CP2-2 landed

`register_hashset_natives` skips `native_lhs_reversed` on a real JDK in `--compatible`
(`lhs_reversed_left_to_bytecode`, kill switch `CRATONVM_COMPAT_LHS_REVERSED_REAL_VIEW=0`). The
`before` blocker was re-read and is not one: every native list edit writes both heap links (see
`r13w8-hashcompat3-...` "Round 14 wave 3"). Left: removal through the view's iterator (C3-1 of
`jit-r14-compat3-proposals.md`). Probe `R14Compat3LhsReversed`.

## CP2-3. `Optional.equals` / `hashCode` / `toString` to bytecode too

**What.** These three call the VALUE's `equals` / `hashCode` / `toString`, so an exception thrown
there, or a stack walk taken there, is missing the `Optional.<name>` frame exactly as `map` was.
Add them to `OPTIONAL_FUNCTIONAL_ROWS` (renamed) after measuring: `Optional.hashCode` is on
`HashMap` key paths, and the bytecode is `Objects.hashCode(value)`, i.e. one more frame and an
invokestatic. **Benefit.** Trace parity for user `toString` failures inside `"" + opt`. **Cost.**
Three table rows. **Risk.** Low (the JDK bodies are one line each). **First step.** Extend
`R14Compat2OptionalFrames` with a throwing `toString` value.

## CP2-4. One `Character.digit` helper for every Rust number parser

**What.** `lang_math.rs` `java_char_digit` (+ `JAVA_DIGIT_RUNS`, generated from JDK 25) and this
wave's `math_bignum.rs` `bi_java_digit` (+ `BMP_ND_ZEROS`, generated from the UCD 16.0.0) are two
tables of the same 36 blocks. Make `java_char_digit` `pub(crate)` with a `u16` twin and delete
`BMP_ND_ZEROS`; audit the other parsers that still use `char::to_digit` / `is_ascii_digit`
(`rg 'to_digit\(' native-builtins/src`: `Byte`/`Short`/`Long` wrappers, `BigDecimal(String)`
natives, `Scanner`, `Integer.decode`) for the same narrowing. **Benefit.** No disagreeing copies
(AGENTS.md's warning about duplicated allow-lists applies to tables too). **Cost.** Small.
**Risk.** Low. **First step.** `rg -n 'to_digit\(' native-builtins/src` and classify each hit as
"Java grammar says Character.digit" or "ASCII by spec" (e.g. `Double.parseDouble` is ASCII).

## CP2-5. Delete the P71 decimal fallback arms after one verified round

**What.** With this wave, every `register_p71_biginteger_extras` body reads limbs
(`rg -n 'bi_read\(ctx' native-builtins/src/phases_late.rs` lists only the four
`CRATONVM_BIGINT_P71_LIMB_ROAD=0` arms). Once the orchestrator's probe arms agree, delete those
arms and the switch, and then whichever of `bi_gcd_str`, `bi_to_byte_array_str`, `bi_compare`,
`bi_read`, `bi_alloc` lose their last shipping caller (the synthetic-jdk `math_bignum.rs`
natives still use some; `rg` each). **Benefit.** Less dead text in a 10k-line file; the switch
table shrinks by one. **Cost.** Small. **Risk.** Low. **First step.** The `R14Compat2BigIntDigits`
and `R14BigdecParse` arms at both switch values.
