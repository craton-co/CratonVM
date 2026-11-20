#[allow(unused_imports)]
use super::super::*;

/// `CRATONVM_JIT_IR_STRING_INDEXOF_SUPPLEMENTARY` (JIT round 21, lane strintr,
/// STR16-P3, same page, probe `R21StrintrRows` indexof-supp; default ON, `=0`
/// off; inside `CRATONVM_JIT_IR_STRING_SEARCH_PREFIX`): the optimizing tier's
/// `String.indexOf(int)` prefix ([`IrStringSearchKind::IndexOfChar`]) answers
/// a UTF16 receiver for every needle -- `-1` for a negative or invalid
/// (`> 0x10FFFF`) code point, the surrogate-pair scan of
/// `StringUTF16.indexOfSupplementary` for a supplementary one -- where it
/// declined to the call for every needle outside `0..=0xFFFF`. `=0` restores
/// the decline exactly. Compile time only, not cached.
pub(crate) fn ir_string_indexof_supplementary_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_STRING_INDEXOF_FROM_PREFIX` (JIT j27 wave 1, lane STRRX,
/// page `r23-w5-compiled-jdk-string-split-and-regex-bodies-7-13x-20261021.md`;
/// perf, **default ON**, `=0` kill): the optimizing tier's String prefix answers
/// `String.indexOf(int, int)` inline ([`IrStringSearchKind::IndexOfCharFrom`])
/// and the splice planner keeps the site a call
/// ([`ir_string_prefix_site_stays_a_call`]) instead of splicing the JDK chain.
/// Measured on Windows (P-cores, 5 interleaved reps, medians of per-process
/// minima, ns/op, `R24RegexSplitRows`): `split-comma` 681 -> 549 (HotSpot 154),
/// `split-comma-10` 654 -> 556 (155), `--compatible` 694 -> 606 / 723 -> 611;
/// `list` and `cached` unchanged. `=0` leaves the site exactly as before (no
/// kind is named). Compile time only, not cached.
pub(crate) fn ir_string_indexof_from_prefix_enabled() -> bool {
    ir_string_search_prefix_enabled()
        && true
}



/// `CRATONVM_JIT_IR_STRING_SUBSTRING_PREFIX` (round rx27, lane splitfast,
/// page `r23-w5-compiled-jdk-string-split-and-regex-bodies-7-13x-20261021.md`;
/// perf, **default ON** since the rx27 A/B, `=0` kills): the optimizing tier's String
/// prefix answers `String.substring(int, int)` / `String.subSequence(int, int)`
/// inline ([`IrStringSearchKind::Substring`]: one TLAB bump for the `byte[]`
/// and the String, the copy inline) and the splice planner keeps the site a
/// call ([`ir_string_prefix_site_stays_a_call`]) instead of splicing
/// `substring` -> `StringLatin1.newString` -> `Arrays.copyOfRange([BII)` ->
/// `new String`. At `=0` no kind is named and the site is exactly as before.
/// Compile time only, not cached.
pub(crate) fn ir_string_substring_prefix_enabled() -> bool {
    ir_string_search_prefix_enabled()
}

/// `CRATONVM_JIT_IR_SEQ_SUBSEQUENCE_PREFIX` (round rx27, lane splitfast; perf,
/// **default ON**, `=0` kills): the same answer for `CharSequence.subSequence(int, int)`
/// on a receiver whose header says String
/// ([`IrStringSearchKind::SeqSubSequence`]; `Pattern.split`'s and
/// `Matcher.group`'s per-part `subSequence(..).toString()`), the site kept a
/// call. Its own switch, so the regex rows can be A/B'd apart from the split
/// fast path. Inside `CRATONVM_JIT_IR_CHARSEQ_PREFIX`. Compile time only.
pub(crate) fn ir_seq_subsequence_prefix_enabled() -> bool {
    ir_string_search_prefix_enabled()
}

/// `CRATONVM_JIT_IR_ARRAYS_COPY_OF_RANGE_PREFIX` (round rx27 wave 3, lane
/// splitfast, page `rx27-splitfast-copyofrange-byte-prefix-proposal-FIXED-20261029.md`;
/// perf, **opt-in** `=1`): the static `Arrays.copyOfRange(byte[], int, int)`
/// answered inline ([`IrStringSearchKind::ArraysCopyOfRangeBytes`]: a fresh
/// `byte[]` bumped and copied, no call), the site kept a call by the splice
/// planner. For the callers the substring prefix does not cover
/// (`StringLatin1.newString` reached from elsewhere, user code). Unset, no
/// kind is named. Compile time only, not cached.
pub(crate) fn ir_arrays_copy_of_range_prefix_enabled() -> bool {
    ir_string_search_prefix_enabled()
        && false
}

/// `CRATONVM_JIT_IR_ARRAYLIST_ADD_PREFIX` (round rx27 wave 3, lane splitfast,
/// page `docs/feature-designs/collections-interception.md`;
/// perf, **opt-in** `=1`): the optimizing tier answers `ArrayList.add(Object)`
/// inline where the JDK's own `ArrayList` bytecode runs (`--real-jdk`, or the
/// `JdkArrayList` alignment row): an exact `java/util/ArrayList` with room, a
/// non-null element, an empty slot and a store the collector is owed nothing
/// for gets `modCount + 1`, the store and `size + 1`, and answers `true`;
/// anything else is the call. The VM builds the layout's add half (with
/// `modCount`) only under this switch, and the splice planner keeps the site
/// a call ([`ir_arraylist_add_site_stays_call`]). Read per question.
pub fn ir_arraylist_add_prefix_enabled() -> bool {
    false
}

/// Is `class.name descriptor` at `invoke_kind` an `ArrayList.add(Object)`
/// site the add prefix may front: `java/util/ArrayList` virtual (or
/// devirtualised special), or `java/util/List` / `java/util/Collection`
/// through `invokeinterface`? The receiver's exact class is tested at run
/// time; a subclass, `Vector` or any other list is the call.
pub(crate) fn arraylist_add_site(
    class: &str,
    name: &str,
    descriptor: &str,
    invoke_kind: u8,
) -> bool {
    name == "add"
        && descriptor == "(Ljava/lang/Object;)Z"
        && match class {
            "java/util/ArrayList" => matches!(invoke_kind, 0 | 1),
            "java/util/List" | "java/util/Collection" => invoke_kind == 2,
            _ => false,
        }
}

/// The splice planner's question for an `ArrayList.add(Object)` site at
/// `opcode`: keep it a CALL, so the add prefix has an `Op::Call` to front,
/// instead of splicing `add(E)` -> `add(E, Object[], int)`. Only under the
/// switch and only when this compile's layout carries the add half (the
/// `modCount` slot the VM resolves under the same switch).
pub(crate) fn ir_arraylist_add_site_stays_call(
    class: &str,
    name: &str,
    descriptor: &str,
    opcode: u8,
    layout: Option<StringFieldLayout>,
) -> bool {
    let invoke_kind = match opcode {
        0xb6 => 0,
        0xb9 => 2,
        _ => return false,
    };
    ir_arraylist_add_prefix_enabled()
        && arraylist_add_site(class, name, descriptor, invoke_kind)
        && layout
            .and_then(|l| l.array_list)
            .is_some_and(|al| al.mod_count_compact_offset.is_some())
}





/// `CRATONVM_JIT_IR_LATIN1_COMPARE_TO_CI_PREFIX` (JIT round 21, lane strings;
/// proposal ZCP-4 in `jit-r20-zCp-proposals.md`, row `R20PNatLatin1`
/// latin1-compareToCI; default ON, `=0` off; inside
/// `CRATONVM_JIT_IR_STRING_SEARCH_PREFIX`): `StringLatin1.compareToCI(byte[],
/// byte[])` as [`IrStringSearchKind::Latin1CompareToCI`] -- an 8-byte equal
/// skip and the JDK's two-step case fold at a differing byte, inline -- and
/// with it the splice carve-out that keeps the site a CALL. `=0` restores
/// today's behaviour exactly. Compile time only, not cached.
pub(crate) fn ir_latin1_compare_to_ci_prefix_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_LATIN1_REGION_MATCHES_CI_PREFIX` (JIT round 21, lane
/// str21b; STRINGS2-21-4, gate G3 of `str21b-strings2-host-gates-20261102.md`;
/// **default ON**, `=0` off, following host triage verification; inside
/// `CRATONVM_JIT_IR_STRING_SEARCH_PREFIX`):
/// `StringLatin1.regionMatchesCI(byte[], int, byte[], int, int)` as
/// [`IrStringSearchKind::Latin1RegionMatchesCI`] -- the equal-bytes block loop
/// and the letter test of `x64::latin1_region_ci_emit` where the call ran the
/// JDK's per-byte `CharacterDataLatin1.equalsIgnoreCase` loop (and, spliced,
/// that loop in every `equalsIgnoreCase` caller). Unset restores today's
/// behaviour exactly. Compile time only, not cached.
pub(crate) fn ir_latin1_region_matches_ci_prefix_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_LATIN1_CASE_MAP_PREFIX` (JIT round 21, lane strings2;
/// proposal STRINGS21-1 in `jit-r21-strings-proposals.md`, page
/// `jr21-strings2-latin1-case-map-and-switch-interactions-20261014.md`; rows
/// `R20PNatLatin1` latin1-toLowerCase, `R21StringsLatin1Case` lower-mixed /
/// upper-mixed, `R21Strings2Rows`; default ON, `=0` off; inside
/// `CRATONVM_JIT_IR_STRING_SEARCH_PREFIX`): `StringLatin1.toLowerCase` /
/// `toUpperCase(String, byte[], Locale)` as
/// [`IrStringSearchKind::Latin1ToLowerCase`] / `Latin1ToUpperCase` -- the
/// scan, the `tr` / `az` / `lt` language screen, one inline bump of the
/// result's `byte[]` + String and the fold in place -- and with them the
/// splice carve-out that keeps the site a CALL. `=0` restores today's
/// behaviour exactly. Compile time only, not cached.
pub(crate) fn ir_latin1_case_map_prefix_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_LATIN1_CASE_MAP_SIMD` (JIT round 21, lane ci,
/// STRINGS2-21-2): the case-map prefix's scan and in-place fold run 16 bytes
/// per SSE2 step, branch-free (`x64::latin1_case_emit::emit_latin1_case_scan_with`
/// / `emit_latin1_case_map_in_place_with`); `=0` keeps the byte loops.
/// Default ON. Compile time only, not cached.
pub(crate) fn ir_latin1_case_map_simd_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_LATIN1_UPPER_SHARP_S` (JIT round 21, lane ci): the
/// upper-case prefix answers a string holding `0xDF` inline (the JDK's
/// `toUpperCaseEx(.., false)` maps it to "SS" and `StringUTF16.newString`
/// compresses the result back to a LATIN1 String of `len + count(0xDF)`
/// bytes) instead of declining to the call; `=0` declines it as before.
/// Default ON. Compile time only, not cached.
pub(crate) fn ir_latin1_upper_sharp_s_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_LATIN1_UPPER_UTF16` (JIT round 21, lane latin1up; page
/// `jr21-latin1-upper-utf16-result-declines-to-a-slow-jdk-path-FIXED-20261027.md`):
/// the upper-case prefix answers a string holding `0xB5` / `0xFF` inline --
/// the JDK's `toUpperCaseEx(.., false)` + `StringUTF16.newString`: a fresh
/// UTF16 String of `len + count(0xDF)` chars (`0x39C` / `0x178`, `0xDF` ->
/// "SS") -- instead of declining to the call; `=0` declines it as before.
/// Default ON. Compile time only, not cached.
pub(crate) fn ir_latin1_upper_utf16_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_STRING_INDEXOF_SCAN` (JIT r20 wave 12, lane qStr; book
/// `jit-r20-nio-proposals-w4-L5.md`, W4-L5-1; default ON, `=0` off; inside
/// `CRATONVM_JIT_IR_STRING_SSE2`): the String prefix's LATIN1 / LATIN1
/// `indexOf(String)` skips candidate starts 16 at a time with an SSE2
/// first-byte scan (`PCMPEQB` + `PMOVMSKB` + `BSF`) before the byte compare,
/// as HotSpot's `StringIndexOf` stub does, instead of re-entering the inner
/// loop at every start. `=0` emits the naive loop byte for byte. Compile time
/// only, not cached.
pub(crate) fn ir_string_indexof_scan_enabled() -> bool {
    true
}

/// Will the optimizing tier answer this site with its inline String search
/// prefix? [`ir_string_search_prefix_kind`] plus the one compile-time screen
/// the prefix makes that is not about the callee: narrow oops or an armed ZGC
/// read barrier (`x64::narrow_oops_block_inline_fields`), under which it reads
/// no reference field and the site would be a plain call -- so the planning
/// gate and the pin keep such a method on the single-pass backend, whose
/// region handles both.
pub(crate) fn ir_string_search_prefix_handles(class: &str, name: &str, descriptor: &str) -> bool {
    ir_string_search_prefix_handled_kind(class, name, descriptor).is_some()
}

/// [`ir_string_search_prefix_handles`], naming the kind, so a caller that
/// knows the site's opcode or invoke kind can ask
/// [`IrStringSearchKind::admits_opcode`] / `admits_invoke_kind` too.
pub(crate) fn ir_string_search_prefix_handled_kind(
    class: &str,
    name: &str,
    descriptor: &str,
) -> Option<IrStringSearchKind> {
    if false
        || crate::x64::narrow_oops_block_inline_fields()
    {
        return None;
    }
    // JIT round 21, lane strintr2 (STRINTR21-1): the String accessor kinds
    // front a left-behind call only; every pin / planning / splice decision
    // keeps asking the accessor expander for those three members.
    ir_string_search_prefix_kind(class, name, descriptor).filter(|k| !k.is_string_accessor_call())
}

/// Does this method contain a call site the single-pass backend would replace
/// with an inline `java/lang/String` / `java/lang/CharSequence` access
/// intrinsic?
///
/// Asks `try_resolve_string_intrinsic` — the one function the registration path
/// also calls — so the gate can never pin a method whose sites that path would
/// decline. Only `invokevirtual` (0xb6) and `invokeinterface` (0xb9) are
/// considered, matching the `invoke_kind == 0 || invoke_kind == 2` guard around
/// the registration itself.
pub(crate) fn has_string_intrinsic_site(
    invoke_ops: &[(usize, u16, u8)],
    cp_invoke_resolver: Option<&dyn Fn(u16) -> Option<(String, String, String)>>,
    layout: Option<StringFieldLayout>,
) -> bool {
    if layout.is_none() {
        return false;
    }
    let Some(resolver) = cp_invoke_resolver else {
        return false;
    };
    invoke_ops.iter().any(|&(_, cp_idx, opcode)| {
        matches!(opcode, 0xb6 | 0xb9)
            && resolver(cp_idx).is_some_and(|(class_name, method_name, descriptor)| {
                try_resolve_string_intrinsic(&class_name, &method_name, &descriptor, layout)
                    .is_some()
            })
    })
}

/// What the String-intrinsic pin decided about one method — and, when it
/// decided nothing, whether that was because there was nothing to decide or
/// because it was asked with an input missing.
///
/// # Why a bool was not enough
///
/// [`has_string_intrinsic_site`] answers `false` — "do not pin" — for two
/// completely different facts: "this method has no String access site" and "I
/// was handed a `None` and cannot tell". The pin is a term of the real
/// eligibility conjunction in `try_compile_inner`, but was absent from the
/// verdict chain that prints WHY a method was or was not admitted, so a method
/// the pin declined, a method the pin never saw, and a method with no site at
/// all all printed the identical `admitted to the optimizing pipeline`.
///
/// That is what made `String.charAt` unfalsifiable: a flat 186-196 ns/char from
/// 200,000 to 100,000,000 characters, against 3.0-3.5 ns/char for a
/// byte-identical body in a different probe on the SAME binary, and five
/// separate hypotheses about what selects the fast shape (OSR vs method entry,
/// callee warm order, caller kind across six shapes, first-compile context,
/// scale) each refuted by its own measurement without ever converging — because
/// the one instrument that could name the decision did not report it. See
/// string-charat-loop-cost-and-the-unsteerable-intrinsic-20260901.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StringPinVerdict {
    /// The pin has no opinion: no `invokevirtual`/`invokeinterface` site at
    /// all, or every site resolved and none of them is a String intrinsic.
    NoSite,
    /// A String access intrinsic site is present and the pin declined the
    /// optimizing tier for this method.
    Pinned,
    /// A String access intrinsic site is present and
    /// `CRATONVM_JIT_NO_STRING_INTRINSIC_PIN=1` switched the pin off, so the
    /// method IS admitted and WILL lose the inline decode. Not a defect — it is
    /// the B arm of the A/B — but it must never read as "no site here", because
    /// the two produce the same admission line and opposite conclusions.
    DisabledByFlag,
    /// A site is declared on `java/lang/String` / `java/lang/CharSequence`, but
    /// no String field layout resolved at this door, so the pin could not
    /// evaluate it.
    BlindNoLayout,
    /// The method has candidate call sites and this door supplied no
    /// constant-pool invoke resolver, so the pin could not read a single callee
    /// name.
    BlindNoResolver,
    /// nio20 wave 1, lane L3 (NIO19-8): String intrinsic sites are present,
    /// but every one of them is an accessor the OPTIMIZING tier expands itself
    /// (`ir.rs`'s `try_string_access_intrinsic`: `length` / `isEmpty` /
    /// `charAt` on a `java/lang/String`-declared `invokevirtual`), so the
    /// method keeps its inline decode on either tier and is not pinned. See
    /// [`every_string_intrinsic_site_ir_expands`]. nio20 wave 3 (lane L2):
    /// or one of the five search members the optimizing tier answers with its
    /// inline prefix ([`ir_string_search_prefix_handles`]).
    IrExpandsEverySite,
}

impl StringPinVerdict {
    /// The half of the verdict that QUALIFIES an admission rather than
    /// replacing it.
    ///
    /// [`StringPinVerdict::Pinned`] is a refusal and gets its own arm in the
    /// verdict chain; the other three all end with the method on the optimizing
    /// tier, and what separates them is what the pin saw — or failed to see —
    /// on the way past. Appending them to the admission line rather than
    /// replacing it keeps `metrics::CompilationReport::admission` meaning
    /// "admitted", which is what its consumers read it as.
    pub(crate) fn admission_note(self) -> Option<&'static str> {
        match self {
            StringPinVerdict::NoSite | StringPinVerdict::Pinned => None,
            StringPinVerdict::DisabledByFlag => Some(
                "CRATONVM_JIT_NO_STRING_INTRINSIC_PIN=1 and the method has a String \
                 intrinsic site, so the inline decode is lost here",
            ),
            StringPinVerdict::BlindNoLayout => Some(
                "no String field layout resolved at this door, so the pin could not see \
                 the site — and neither would the single-pass backend have emitted the \
                 intrinsic, so nothing was lost by admitting it",
            ),
            StringPinVerdict::BlindNoResolver => Some(
                "no constant-pool invoke resolver at this door, so the pin could not see \
                 the site",
            ),
            StringPinVerdict::IrExpandsEverySite => Some(
                "every String intrinsic site here is a length/isEmpty/charAt accessor the \
                 optimizing tier expands itself (or a hashCode/equals/compareTo/indexOf site, \
                 or a StringBuilder length/charAt/append(char)/append(String)/append(int)/ \
                 toString/appendCodePoint site or a String codePointAt site, its inline prefix \
                 answers), so the method is not pinned",
            ),
        }
    }
}

/// nio20 wave 1, lane L3 (NIO19-8): is every String intrinsic site of this
/// method one the OPTIMIZING tier's own expander emits?
///
/// `false` when there is no such site at all (the caller asks only after
/// [`has_string_intrinsic_site`] said there is one), when the expander is off
/// (`CRATONVM_JIT_IR_STRING_INTRINSICS=0`), and when ANY site is something
/// else: `hashCode`, `equals`, `compareTo`, either `indexOf`, a
/// `java/lang/CharSequence`-declared (guarded) accessor, an
/// `invokeinterface`, or a `StringBuilder` member -- each emitted only by the
/// single-pass region, so the pin keeps the method where it is emitted.
///
/// The accessor test is [`ir_string_access_expander_handles`], the SAME
/// predicate the invoke-planning gate uses to let these three sites reach the
/// expander, so the pin cannot release a method that gate would still bail;
/// and `CRATONVM_JIT_NO_IR_STRING_ACCESS_ADMIT=1`, which restores that gate's
/// blanket refusal, restores the pin with it. Sites whose callee name cannot
/// be read are skipped exactly as [`has_string_intrinsic_site`] skips them.
///
/// The expander measured a tie with the single-pass decode (3.2-3.5 ns/char,
/// `ir.rs`'s String section), which is what retires the pin for these
/// methods: the pin's own measurement (504 vs 135 ns/call) predates the
/// expander.
///
/// nio20 wave 3 (lane L2): a `hashCode` / `equals` / `compareTo` / `indexOf`
/// site on a `java/lang/String` `invokevirtual` counts as emitted too when
/// [`ir_string_search_prefix_handles`] says the optimizing tier answers it
/// with its inline search prefix -- the SAME predicate the planning gate
/// carves those sites out with, so the two refusals still lift together
/// (`CRATONVM_JIT_IR_STRING_SEARCH_PREFIX=0` restores both). The accessor
/// half still needs `CRATONVM_JIT_IR_STRING_INTRINSICS` on; the search half
/// does not ask it (a different emitter).
///
/// nio20 wave 4 (lane L5): likewise a `StringBuilder.length` / `charAt` /
/// `append(char)` site (`CRATONVM_JIT_IR_SB_PREFIX`), and since nio20 wave 5
/// `append(String)` (`CRATONVM_JIT_IR_SB_APPEND_STRING_PREFIX`); since JIT r20
/// wave 18 (lane xSb, STR20-2) `append(int)` and `toString()` too
/// (`CRATONVM_JIT_IR_SB_APPEND_INT_PREFIX` / `CRATONVM_JIT_IR_SB_TOSTRING_PREFIX`).
///
/// JIT r23 wave 1 (lane L5, INTRINSICS22-1): the single-pass
/// `String.codePointAt` / `StringBuilder.appendCodePoint` rows are released
/// here by their `CodePointAt` / `SbAppendCodePoint` prefix kinds -- named by
/// [`ir_string_search_prefix_kind`] by NAME, not through the resolver, so the
/// same branch below answers them; a row whose prefix is withdrawn pins
/// (`tests.rs` `r23_l5_code_point_rows_do_not_pin_while_their_prefix_kinds_answer`).
pub(crate) fn every_string_intrinsic_site_ir_expands(
    invoke_ops: &[(usize, u16, u8)],
    resolver: &dyn Fn(u16) -> Option<(String, String, String)>,
    layout: Option<StringFieldLayout>,
) -> bool {
    if layout.is_none() {
        return false;
    }
    let accessors_expand = ir::ir_string_intrinsics_enabled();
    let mut any = false;
    for &(_, cp_idx, opcode) in invoke_ops {
        if !matches!(opcode, 0xb6 | 0xb9) {
            continue;
        }
        let Some((class_name, method_name, descriptor)) = resolver(cp_idx) else {
            continue;
        };
        if try_resolve_string_intrinsic(&class_name, &method_name, &descriptor, layout).is_none() {
            continue;
        }
        let emitted = (opcode == 0xb6
            && accessors_expand
            && ir_string_access_expander_handles(&class_name, &method_name, &descriptor))
            || ir_string_search_prefix_handled_kind(&class_name, &method_name, &descriptor)
                .is_some_and(|k| k.admits_opcode(opcode));
        if !emitted {
            return false;
        }
        any = true;
    }
    any
}

/// `CRATONVM_JIT_STRING_PIN_SKIPS_COLD_THROW_ARMS` (JIT r23 wave 13, lane L6;
/// perf, **opt-in** `=1`: it LOST on the n23-w13a A/B -- `=0` read
/// `W8L6SplitParts comma` 967/1045/1063 ns/op against 2113/1584/1813 on, and
/// `RxLoop split-comma` 949/1339/988 against 2019/1890/1742; the
/// `copyOfRange` IR body without the inline byte arraycopy is slower than its
/// single-pass body): the String-intrinsic pin
/// ([`string_intrinsic_pin_verdict`] / [`string_intrinsic_pin_declines`])
/// does not count an invoke site that lies inside a cold throw arm
/// (`ir::cold_throw_arms`: the arm a conditional branch jumps around, ending
/// in its `athrow`). Such a site builds the exception's message. Its inline
/// single-pass decode is worth nothing, and it pinned the whole method to the
/// single-pass backend.
///
/// The measured case is JDK 25 `Arrays.copyOfRange([BII)`. Every
/// `String.substring` of a LATIN1 string calls it, and on the
/// `W8L6SplitParts cached` row it ran as its `[c1]` body (7 % of the row,
/// RIP sample on the w13 base). Its only String-family sites are the
/// `StringBuilder.append` / `toString` calls of
/// `throw new IllegalArgumentException(from + " > " + to)` (pcs 26..57, the
/// arm `ifge 58` at pc 23 jumps around). That kept its
/// `System.arraycopy` on the single-pass bail edge as well. `Pattern.<init>`'s
/// `Unknown flag` arm is the same shape. Read per compile.
pub(crate) fn string_pin_skips_cold_throw_arms_enabled() -> bool {
    // JIT r25 wave 10 (W10-2): unset, the skip follows
    // `CRATONVM_JIT_IR_INLINE_BYTE_ARRAYCOPY` -- the missing half of the lost
    // A/B above (`r25_w10_2_steady_c1::cold_arm_skip_follows_inline_copy`).
    if None::<std::ffi::OsString>
        .is_none()
    {
        return r25_w10_2_steady_c1::cold_arm_skip_follows_inline_copy();
    }
    false
}

/// The invoke sites the String-intrinsic pin is asked about: every site, or,
/// under [`string_pin_skips_cold_throw_arms_enabled`], every site outside the
/// method's cold throw arms. A body whose throw shape `ir::cold_throw_arms`
/// refuses (`None`) keeps all of its sites.
pub(crate) fn string_pin_invoke_ops(
    code: &[u8],
    code_len: usize,
    invoke_ops: &[(usize, u16, u8)],
) -> Vec<(usize, u16, u8)> {
    if !string_pin_skips_cold_throw_arms_enabled() {
        return invoke_ops.to_vec();
    }
    let arms = ir::cold_throw_arms_for_pin(code, code_len);
    invoke_ops_outside_arms(invoke_ops, &arms)
}

/// The pure half of [`string_pin_invoke_ops`]: the sites whose pc lies in no
/// arm's `[start, athrow_pc]`.
pub(crate) fn invoke_ops_outside_arms(
    invoke_ops: &[(usize, u16, u8)],
    arms: &[ir::ColdThrowArm],
) -> Vec<(usize, u16, u8)> {
    invoke_ops
        .iter()
        .copied()
        .filter(|&(pc, _, _)| !arms.iter().any(|a| a.start <= pc && pc <= a.athrow_pc))
        .collect()
}



/// `CRATONVM_JIT_IR_SB_CODE_POINT_UNPIN` (JIT round 21, lane strings, page
/// `jr21-string-code-point-at-native-funnel-20261014.md` "cp-append"; default
/// ON, `=0` off): does this method hold a `StringBuilder.appendCodePoint(int)`
/// `invokevirtual` that only the OPTIMIZING tier answers inline
/// ([`IrStringSearchKind::SbAppendCodePoint`])? Such a method is not pinned to
/// the single-pass backend for its other builder sites (`length`, `charAt`,
/// `append(char|String|int)`, `toString`): there each `appendCodePoint` is a
/// whole JDK call chain per code point (`R20PIRCodePoint` cp-append, 5007 ns
/// per 15 code points vs HotSpot 30), while the optimizing tier splices the
/// other members' small JDK bodies. `=0` restores the pin. Compile time only.
pub(crate) fn sb_code_point_site_unpins(
    invoke_ops: &[(usize, u16, u8)],
    resolver: &dyn Fn(u16) -> Option<(String, String, String)>,
) -> bool {
    invoke_ops.iter().any(|&(_, cp_idx, opcode)| {
            opcode == 0xb6
                && resolver(cp_idx).is_some_and(|(c, m, d)| {
                    ir_string_search_prefix_handled_kind(&c, &m, &d)
                        == Some(IrStringSearchKind::SbAppendCodePoint)
                })
        })
}

/// Classify a method for the String-intrinsic pin.
///
/// PURE: no counters, no output, no allocation beyond whatever the caller's own
/// resolver returns. That is a requirement, not a preference — the verdict chain
/// in `try_compile_inner` calls this under `CRATONVM_DBG_JITC` / metrics only,
/// and a diagnostic that perturbs the compile it is describing is worse than no
/// diagnostic at all. The counting lives in [`string_intrinsic_pin_declines`],
/// which the real conjunction calls exactly once per compile attempt.
pub(crate) fn string_intrinsic_pin_verdict(
    invoke_ops: &[(usize, u16, u8)],
    cp_invoke_resolver: Option<&dyn Fn(u16) -> Option<(String, String, String)>>,
    layout: Option<StringFieldLayout>,
) -> StringPinVerdict {
    // Only the two opcodes the registration path considers can be a blind spot
    // at all; a method with no `invokevirtual`/`invokeinterface` could never
    // have been intrinsified whatever the inputs were, so it must not be
    // reported as one. Cheap, and it keeps the two `Blind*` counters a count of
    // METHODS AT RISK rather than a count of compiles.
    if !invoke_ops
        .iter()
        .any(|&(_, _, opcode)| matches!(opcode, 0xb6 | 0xb9))
    {
        return StringPinVerdict::NoSite;
    }
    let Some(resolver) = cp_invoke_resolver else {
        // Reported ahead of a missing layout because it is the stronger
        // blindness: with no callee names, nothing whatever can be said about
        // these sites — including whether a layout would have mattered.
        return StringPinVerdict::BlindNoResolver;
    };
    if layout.is_none() {
        // Narrow the report to methods that actually call something DECLARED on
        // a String-family receiver. `try_resolve_string_intrinsic` tests the
        // class name and returns `None` before it ever looks at the layout
        // (`let layout = string_layout?;` comes after), so this is the same
        // first test it makes — and without it "no layout at this door" would be
        // printed against essentially every method in the program, which is how
        // a diagnostic becomes noise instead of evidence.
        return if invoke_ops.iter().any(|&(_, cp_idx, opcode)| {
            matches!(opcode, 0xb6 | 0xb9)
                && resolver(cp_idx).is_some_and(|(class_name, _, _)| {
                    class_name == "java/lang/String" || class_name == "java/lang/CharSequence"
                })
        }) {
            StringPinVerdict::BlindNoLayout
        } else {
            StringPinVerdict::NoSite
        };
    }
    if !has_string_intrinsic_site(invoke_ops, cp_invoke_resolver, layout) {
        return StringPinVerdict::NoSite;
    }
    // nio20 L3 (NIO19-8): nothing to keep when the optimizing tier emits
    // every one of the sites itself.
    if every_string_intrinsic_site_ir_expands(invoke_ops, resolver, layout) {
        return StringPinVerdict::IrExpandsEverySite;
    }
    // JIT round 21, lane strings (`CRATONVM_JIT_IR_SB_CODE_POINT_UNPIN`): a
    // method with a `StringBuilder.appendCodePoint` site the optimizing tier's
    // prefix answers is not pinned for its other builder sites.
    if sb_code_point_site_unpins(invoke_ops, resolver) {
        return StringPinVerdict::IrExpandsEverySite;
    }
    if string_intrinsic_pin_enabled() {
        StringPinVerdict::Pinned
    } else {
        StringPinVerdict::DisabledByFlag
    }
}

/// Compiles at which the pin actually declined the optimizing tier.
///
/// The zero is the reading that matters. `CRATONVM_JIT_NO_STRING_INTRINSIC_PIN=1`
/// measured 326.3 / 328.6 ns/char against a default of 329.5 / 333.7 on
/// `probes/CharAtCostCurve.java` — switching the pin OFF cost nothing, which is
/// what "it was never on" looks like from the outside. A timing cannot tell "the
/// pin fired and did not help" from "the pin never fired"; this can.
///
/// Both halves of that A/B are **superseded and pre-emitter**, and are kept
/// because they are what motivated the counter. Two things have since happened
/// to the numbers: the optimizing tier gained a String access expander (arm B,
/// 66-108 ns/char), and — the larger one — `java/lang/String` being `final`
/// meant the method-entry door's devirtualisation took every String access site
/// away from the inline intrinsic before the gate could claim it, so BOTH arms
/// of that A/B were measuring a program with no String intrinsic in it at all.
/// With that fixed the same rows read ~3.2 ns/char. Do not quote 326/329 as
/// current; see `DEVIRT_YIELDED_TO_INTRINSIC` and
/// string-charat-loop-cost-and-the-unsteerable-intrinsic-20260901.
pub(crate) static STRING_PIN_FIRED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// Compiles that reached the pin with candidate sites and no resolved
/// `java/lang/String` field layout.
pub(crate) static STRING_PIN_BLIND_NO_LAYOUT: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Compiles that reached the pin with candidate sites and no constant-pool
/// invoke resolver.
///
/// All three production doors (the eager-first-call, inline-mutator and
/// tiered-worker `try_compile_with_invokespecial_resolver` call sites in
/// `vm/src/runtime/interpreter/jit_bridge.rs`) pass `Some(&invoke_resolver)`, so
/// this is EXPECTED to read zero on a real workload — which is exactly why it is
/// worth counting. A zero kills the resolver hypothesis outright and sends the
/// next reader to [`STRING_PIN_BLIND_NO_LAYOUT`]; a non-zero one names a door
/// nobody knew existed. Neither answer was available before, at any price.
pub(crate) static STRING_PIN_BLIND_NO_RESOLVER: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Compiles the fail-closed rule actually held back — the blast radius of
/// [`string_pin_fail_closed_enabled`]. Separate from
/// [`STRING_PIN_BLIND_NO_RESOLVER`] because the blind count is a fact about the
/// doors and this one is a fact about the POLICY: it is the number a revert
/// would give back.
pub(crate) static STRING_PIN_FAIL_CLOSED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// IR String-access sites that got their two inline compact-field rows.
///
/// The number that says the expansion's `value`/`coder` reads are inline loads
/// rather than `jit_getfield` helper CALLs. A zero beside a non-zero
/// `emitted_charAt` means the rows were refused — no `coder`, a narrow compact
/// `value`, or compact ref fields off — and the emitter is paying two CALLs a
/// character, which is the shape this counter exists to make visible in one
/// line rather than by reading `getfield helper calls` and guessing.
pub(crate) static STRING_ACCESS_COMPACT_ROWS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// `(rows installed,)` — see [`STRING_ACCESS_COMPACT_ROWS`].
pub fn string_access_compact_rows() -> u64 {
    STRING_ACCESS_COMPACT_ROWS.load(std::sync::atomic::Ordering::Relaxed)
}

/// `(pin fired, blind: no layout, blind: no resolver, held back by fail-closed)`.
///
/// Same shape and same purpose as [`byte_element_helper_calls`]: a site count
/// and an engagement count answer different questions, and the gap between them
/// is where a mechanism hides.
///
/// `jit/src/tiered.rs::dump_method_stats_to_stderr` now prints all four on one
/// line — `JIT String-intrinsic pin: fired=… blind-no-layout=…
/// blind-no-resolver=… fail-closed=…` — at exit under
/// `CRATONVM_DBG=jit-method-stats` (`flags().jit.method_stats`, dispatched from
/// `vm-cli/src/main.rs`), and this accessor is what it reads. The line was
/// added with the counters; the sentence that used to stand here said "nothing
/// prints it yet", which was true for about a day and is exactly the
/// instrument-drift the counters exist to catch. The per-method verdict under
/// `CRATONVM_DBG_JITC` reports the same four states one method at a time.
///
/// # These four are PROCESS-GLOBAL, and that is how they read zero — 2026-09-01
///
/// All four read `0` on `probes/CharAtWarmShape.java` — the workload the pin
/// exists to govern — in a run where lifting the pin moved that workload 3x
/// (314-336 ns/char to 66-108). Nothing was wrong with the counting. The pin
/// is asked in `try_compile_inner`, i.e. at `CompileDoor::MethodEntry`, and
/// that method is compiled through the OSR door, which reaches the backend
/// without passing `try_compile_inner` at all. A counter installed at one door
/// reports zero for traffic through another.
///
/// So read [`string_intrinsic_pin_door_census_line`] FIRST. These four only
/// mean anything for the doors whose per-door `asked` is non-zero; for every
/// other door they are silence, not evidence. See
/// [`string_intrinsic_pin_enabled`] for the three-arm measurement.
pub fn string_intrinsic_pin_census() -> (u64, u64, u64, u64) {
    use std::sync::atomic::Ordering::Relaxed;
    // The per-door line is emitted HERE, from the getter, which is a
    // deliberate compromise and not an oversight. Its only caller is
    // `jit/src/tiered.rs::dump_method_stats_to_stderr`, which formats the four
    // globals below into the `JIT String-intrinsic pin:` line — and that file
    // was outside the lane that added the per-door counters, so the new line
    // could not be written beside the old one. Emitting it from the one
    // function the printer calls is what keeps both numbers in the same run's
    // output; the alternative was a per-door census nothing prints, which is
    // exactly the instrument-drift the paragraph above records happening once
    // already ("nothing prints it yet" was true for about a day).
    //
    // Guarded twice so it can perturb nothing else: on
    // `flags().jit.method_stats`, the same flag that gates the only caller, so
    // a unit test reading the census prints nothing; and on a `Once`, so a
    // second call cannot double the line. Move it into
    // `dump_method_stats_to_stderr` beside its sibling the moment that file is
    // in scope, and delete this paragraph with it.
    if cratonvm_types::flags().jit.method_stats {
        static PER_DOOR_LINE: std::sync::Once = std::sync::Once::new();
        PER_DOOR_LINE.call_once(|| eprintln!("{}", string_intrinsic_pin_door_census_line()));
    }
    (
        STRING_PIN_FIRED.load(Relaxed),
        STRING_PIN_BLIND_NO_LAYOUT.load(Relaxed),
        STRING_PIN_BLIND_NO_RESOLVER.load(Relaxed),
        STRING_PIN_FAIL_CLOSED.load(Relaxed),
    )
}

/// The per-door half of [`string_intrinsic_pin_census`], as one line.
///
/// # Why a per-door breakdown exists at all
///
/// `fired=0` is the whole lesson. It was read as "the pin is correctly inert"
/// on a workload where lifting the pin was worth 3x, and it meant "this
/// traffic went through a door that never asks the pin". The four global
/// counters cannot express that: *asked and found nothing to pin*, *never
/// asked*, and *the conjunction short-circuited before the pin term* all
/// produce the identical zero. These can, because per door
/// `asked + NOT-asked == admitted` is an exact identity — `admitted` is
/// `compile_gate::admissions`, `asked` is bumped by the first ask on each
/// admission token, and `NOT-asked` is charged by that token's `Drop`.
///
/// Shape:
///
/// ```text
/// [cratonvm] JIT String-intrinsic pin by door: method-entry: admitted=812 asked=44 pinned=7 NOT-asked=768 | eager-first-call: admitted=3 asked=0 pinned=0 NOT-asked=3 | osr: admitted=61 asked=0 pinned=0 NOT-asked=61
/// ```
///
/// How to read a row:
///
///   * `asked == admitted` — this door puts the question on every compilation.
///   * `NOT-asked == admitted` and `admitted > 0` — this door has never been
///     taught the question, and every global pin counter is blind to
///     everything it compiled. That is the state of `osr` and
///     `eager-first-call` as this lands; both are in `vm/**`.
///   * `NOT-asked > 0` at `method-entry` — compiles whose eligibility
///     conjunction short-circuited before the pin term, or that bailed before
///     reaching it. Worth seeing: a short-circuit is the *other* way a pin
///     term produces no census, and it is indistinguishable from the first in
///     the global four.
///   * `pinned` — the per-door half of the global `fired`, and the only number
///     here that says the pin changed an outcome.
pub fn string_intrinsic_pin_door_census_line() -> String {
    use crate::compile_gate::{
        admissions, string_pin_asked, string_pin_declined, string_pin_not_asked, CompileDoor,
    };
    let rows: Vec<String> = CompileDoor::ALL
        .iter()
        .map(|d| {
            format!(
                "{}: admitted={} asked={} pinned={} NOT-asked={}",
                d.label(),
                admissions(*d),
                string_pin_asked(*d),
                string_pin_declined(*d),
                string_pin_not_asked(*d),
            )
        })
        .collect();
    format!(
        "[cratonvm] JIT String-intrinsic pin by door: {}",
        rows.join(" | ")
    )
}

/// Does the pin FAIL CLOSED when the compile door supplied no constant-pool
/// invoke resolver? Default ON; `CRATONVM_JIT_NO_STRING_PIN_FAIL_CLOSED=1`
/// restores the historical fail-open behaviour so the trade is A/B-able in one
/// binary.
///
/// NOT `OnceLock`-cached, matching [`string_intrinsic_pin_enabled`] beside it:
/// compile-time only, never a runtime hot path, and caching would make the flag
/// racy against whichever thread compiles first.
pub(crate) fn string_pin_fail_closed_enabled() -> bool {
    !false
}

/// The pin's decision for the real eligibility conjunction: `true` = keep this
/// method off the optimizing tier. Counts, so it must run at most once per
/// compile attempt — which the conjunction's short-circuit guarantees.
///
/// # Why the two missing-input cases are decided DIFFERENTLY
///
/// They look like one case and they are not.
///
/// **No layout — keep failing open.** `resolved_string_layout` is resolved once
/// per compilation and the SAME value is threaded into the single-pass
/// registration loop's `try_resolve_string_intrinsic` call and into
/// `x64::compile`'s `compiler.string_layout`. With `None` that function returns
/// `None` at `let layout = string_layout?;` for every site, so the single-pass
/// backend registers no String sentinel and emits no inline decode either.
/// Pinning would buy a C1 body with no intrinsic in it in place of a C2 body — a
/// pure downgrade. The pin's original argument ("pinning would cost a C2 body
/// and buy nothing") holds here exactly, and was re-checked against the
/// registration loop rather than taken on trust. It is still COUNTED, because
/// "the layout did not resolve" is a hypothesis about the whole `charAt`
/// population that nothing has ever been able to test.
///
/// **No resolver — fail closed.** This is not evidence about the method, it is a
/// missing input at one door. The layout resolved, so the intrinsic is
/// emittable; the pin simply cannot read the callee names to find the sites, and
/// promoting on that promotes to the tier that provably cannot keep them (see
/// the grep in [`string_intrinsic_pin_enabled`]).
///
/// The cost is bounded, and was checked rather than assumed: with
/// `scan.invoke_ops` non-empty and no resolver, the single-pass path takes
/// `jitc_bail!("cp_invoke_resolver")`, which does NOT set `backend_attempted`
/// and so does NOT put the method on the permanent bail-list. The worst case is
/// one abandoned compile attempt, retried later — and since the resolver's
/// absence is a property of the DOOR, not of the method, a retry through any of
/// the three production doors succeeds. That is the same trade the
/// compiled-local-handler safety net in `try_compile` already makes: one wasted
/// compile rather than a permanently wrong body. The alternative is worse than
/// it looks, because a compiled body is installed and kept — an intrinsic-less
/// body produced at a blind door is not a slow first attempt, it is the body for
/// the life of the process.
///
/// Subordinate to [`string_intrinsic_pin_enabled`]: switching the pin off has to
/// switch the fail-closed rule off with it, or the B arm of the A/B would still
/// be pinning methods and the arms would not differ by one thing.
pub(crate) fn string_intrinsic_pin_declines(
    invoke_ops: &[(usize, u16, u8)],
    cp_invoke_resolver: Option<&dyn Fn(u16) -> Option<(String, String, String)>>,
    layout: Option<StringFieldLayout>,
) -> bool {
    use std::sync::atomic::Ordering::Relaxed;
    match string_intrinsic_pin_verdict(invoke_ops, cp_invoke_resolver, layout) {
        StringPinVerdict::NoSite
        | StringPinVerdict::DisabledByFlag
        | StringPinVerdict::IrExpandsEverySite => false,
        StringPinVerdict::Pinned => {
            STRING_PIN_FIRED.fetch_add(1, Relaxed);
            true
        }
        StringPinVerdict::BlindNoLayout => {
            STRING_PIN_BLIND_NO_LAYOUT.fetch_add(1, Relaxed);
            false
        }
        StringPinVerdict::BlindNoResolver => {
            STRING_PIN_BLIND_NO_RESOLVER.fetch_add(1, Relaxed);
            let fail_closed = string_intrinsic_pin_enabled()
                && layout.is_some()
                && string_pin_fail_closed_enabled();
            if fail_closed {
                STRING_PIN_FAIL_CLOSED.fetch_add(1, Relaxed);
            }
            fail_closed
        }
    }
}

/// JIT round 21 wave 20 (lane sbfuse, STRINTR21-4;
/// `CRATONVM_JIT_IR_SB_CHAIN_FUSE`, default ON): the method's
/// `new StringBuilder().append(..)...toString()` chains the optimizing tier
/// fuses into one concat ([`sb_chain_fuse::fusable_sb_chains`]), read through
/// the request's constant-pool resolvers. Empty with the switch off, without
/// the invoke / String-literal / concat-site resolvers, or without the bridge
/// helper the fused node's decline calls. Pure.
pub(crate) fn ir_sb_chains(
    req: &CompileRequest<'_>,
    code: &[u8],
    code_len: usize,
    census: bool,
) -> Vec<sb_chain_fuse::SbChain> {
    if !sb_chain_fuse::sb_chain_fuse_enabled()
        || req.direct_helpers.indy_bridge == 0
        || req.sb_concat_site_resolver.is_none()
    {
        return Vec::new();
    }
    let (Some(invoke), Some(ldc_text)) = (req.cp_invoke_resolver, req.cp_ldc_string_utf16_resolver)
    else {
        return Vec::new();
    };
    let code = &code[..code_len.min(code.len())];
    if !code.contains(&0xbb) {
        return Vec::new();
    }
    let targets = bytecode_analysis::branch_target_map(code, code.len());
    let handlers: Vec<(usize, usize, usize)> = req
        .cached
        .exception_table
        .iter()
        .map(|e| {
            (
                e.start_pc as usize,
                e.end_pc as usize,
                e.handler_pc as usize,
            )
        })
        .collect();
    let src = sb_chain_fuse::SbChainCode {
        code,
        branch_targets: &targets,
        handlers: &handlers,
    };
    let invoke_at = |pc: usize| {
        let cp = u16::from_be_bytes([*code.get(pc + 1)?, *code.get(pc + 2)?]);
        invoke(cp)
    };
    let ldc_string_at = |pc: usize| {
        let cp = match *code.get(pc)? {
            0x12 => u16::from(*code.get(pc + 1)?),
            0x13 => u16::from_be_bytes([*code.get(pc + 1)?, *code.get(pc + 2)?]),
            _ => return None,
        };
        ldc_text(cp)
    };
    let field_at = |cp: u16| {
        req.cp_field_resolver
            .and_then(|resolve| resolve(cp))
            .map(|(_, tag, _)| tag)
    };
    // sbf21 (SBFUSE21-2 / SBFUSE21-3's first step): under `CRATONVM_DBG_JITC`
    // the plan's caller lists every refused builder chain with the
    // recognizer's reason, one line each. Never from the pin's second read.
    if census && x64::dbg_jitc_enabled() {
        for (new_pc, why) in
            sb_chain_fuse::refused_sb_chains(&src, &invoke_at, &ldc_string_at, &field_at)
        {
            eprintln!(
                "[cratonvm-jitc] sb-chain-refused {}.{}{} new_pc={new_pc}: {why}",
                req.cached.class_name, req.cached.method_name, req.cached.method_descriptor,
            );
        }
    }
    sb_chain_fuse::fusable_sb_chains(&src, &invoke_at, &ldc_string_at, &field_at)
}

/// STRINTR21-4: the builder's fusion plan, `new pc -> (chain, concat bridge
/// site)` -- [`ir_sb_chains`], each with the process-lived site the VM made
/// of its recipe (`CompileRequest::sb_concat_site_resolver`). A chain the VM
/// refuses is built as before.
pub(crate) fn sb_chain_fusion_plan(
    req: &CompileRequest<'_>,
    code: &[u8],
    code_len: usize,
    deopt_on_overflow: bool,
) -> std::collections::HashMap<usize, sb_chain_fuse::SbChainFusion> {
    let mut plan = std::collections::HashMap::new();
    let Some(make_site) = req.sb_concat_site_resolver else {
        return plan;
    };
    for chain in ir_sb_chains(req, code, code_len, deopt_on_overflow) {
        let recipe = chain.recipe();
        if let Some(site) = make_site(
            recipe.recipe.as_slice(),
            recipe.constants.as_slice(),
            recipe.descriptor.as_str(),
            deopt_on_overflow,
        ) {
            if site != 0 {
                plan.insert(chain.new_pc, sb_chain_fuse::SbChainFusion { chain, site });
            }
        }
    }
    plan
}

pub(crate) fn ir_sb_chain_fusion_plan(
    req: &CompileRequest<'_>,
    code: &[u8],
    code_len: usize,
) -> std::collections::HashMap<usize, sb_chain_fuse::SbChainFusion> {
    sb_chain_fusion_plan(req, code, code_len, true)
}

/// The single-pass backend can carry the same concat bridge plan for the
/// stack-only, non-trapping subset. Its operands stay on the simulated JVM
/// stack until the synthetic indy at `toString`, where the ordinary bridge
/// staging path consumes them. Trapping reads stay on the original builder
/// path because this backend has no `new`-frame deopt snapshot to re-enter.
pub(crate) fn single_pass_sb_chain_fusion_plan(
    req: &CompileRequest<'_>,
    code: &[u8],
    code_len: usize,
) -> std::collections::HashMap<usize, sb_chain_fuse::SbChainFusion> {
    let mut plan = sb_chain_fusion_plan(req, code, code_len, false);
    let code = &code[..code_len.min(code.len())];
    plan.retain(|_, fusion| {
        let chain = &fusion.chain;
        if chain.local.is_some() {
            return false;
        }
        let Some(cp) = code
            .get(chain.new_pc + 1..chain.new_pc + 3)
            .map(|bytes| u16::from_be_bytes([bytes[0], bytes[1]]))
        else {
            return false;
        };
        let resolved = req
            .cp_new_resolver
            .and_then(|resolver| resolver(cp))
            .is_some_and(|site| matches!(site, JitNewSite::Resolved { .. }));
        let uninitialized = req
            .cp_uninit_new_guard_resolver
            .and_then(|resolver| resolver(cp, chain.new_pc))
            .is_some()
            || req
                .cp_uninit_new_trap_resolver
                .is_some_and(|resolver| resolver(cp, chain.new_pc));
        if !resolved || uninitialized {
            return false;
        }
        chain.parts.iter().all(|part| {
            let &sb_chain_fuse::SbChainPart::Value {
                operand_pc,
                append_pc,
                ..
            } = part
            else {
                return true;
            };
            let mut pc = operand_pc;
            while pc < append_pc {
                let op = code[pc];
                let nontrapping_pure = matches!(
                    op,
                    0x00..=0x14
                        | 0x15..=0x2d
                        | 0x60..=0x6b
                        | 0x72..=0x83
                        | 0x85..=0x98
                );
                if !nontrapping_pure {
                    return false;
                }
                let step = bytecode_analysis::step(code, pc);
                if step == 0 || pc + step > append_pc {
                    return false;
                }
                pc += step;
            }
            pc == append_pc
        })
    });
    plan
}

/// STRINTR21-4: the invoke sites the String-intrinsic pin weighs -- every
/// site except the builder invokes (`<init>`, `append`, `toString`) of a
/// chain the optimizing tier fuses ([`ir_sb_chains`]), which no tier then
/// calls. The same predicate as the builder's plan, so the pin releases
/// exactly the chains the builder removes; `CRATONVM_JIT_IR_SB_CHAIN_FUSE=0`
/// gives the pin every site back.
pub(crate) fn invoke_ops_for_string_pin<'s>(
    req: &CompileRequest<'_>,
    code: &[u8],
    code_len: usize,
    invoke_ops: &'s [(usize, u16, u8)],
) -> std::borrow::Cow<'s, [(usize, u16, u8)]> {
    let chains = ir_sb_chains(req, code, code_len, false);
    if chains.is_empty() {
        return std::borrow::Cow::Borrowed(invoke_ops);
    }
    let fused: std::collections::HashSet<usize> = chains
        .iter()
        .flat_map(sb_chain_fuse::SbChain::invoke_pcs)
        .collect();
    std::borrow::Cow::Owned(
        invoke_ops
            .iter()
            .copied()
            .filter(|&(pc, _, _)| !fused.contains(&pc))
            .collect(),
    )
}

pub(crate) fn single_pass_only_lowering_for(
    code: &[u8],
    code_len: usize,
    cached: &CachedBytecodeMethod,
) -> Option<x64::SinglePassOnly> {
    let ranges: Vec<(usize, usize, usize)> = cached
        .exception_table
        .iter()
        .map(|e| {
            (
                e.start_pc as usize,
                e.end_pc as usize,
                e.handler_pc as usize,
            )
        })
        .collect();
    x64::single_pass_only_lowering(code, code_len, &ranges)
}

/// Whether the optimizing tier lowers the ARRAYLIST_ACCESS guarded prefix
/// (`x64/op_invoke.rs::try_emit_arraylist_access_prefix`) itself. While it does
/// not, [`arraylist_prefix_pins_single_pass`] keeps a method whose `List.get` /
/// `size` sites the single-pass backend answers inline OFF the optimizing tier.
///
/// Round 9 wave 9 (`evict9`): `false` -- the IR half is lane `irl9`'s
/// (`ir_lower.rs`). When it lands, the integrator measures `AlProbe list`,
/// `CratonBenchC2 pipeline` and `SpliceCastProbe` with and without
/// `CRATONVM_JIT_NO_ARRAYLIST_PIN=1` and sets this to `true` if the IR body with
/// the prefix is no slower (see `NOTES-w9-evict9.md`).
///
/// nio20 wave 3, lane L2 (proposal NIO20-L3-3): nobody took that measurement,
/// so the const became the runtime switch `CRATONVM_JIT_IR_ARRAYLIST_PREFIX`,
/// default ON (`=0` restores the pin, i.e. the old `false`). The IR prefix
/// (`ir_lower::Lowerer::try_emit_ir_arraylist_access_prefix`) was reviewed
/// against the single-pass one instruction by instruction: the same screens in
/// the same order (layout epoch, null, kind bits, exact class id at `[recv+0]`
/// -- `class_id_offset_in_obj` is 0 --, compact/legacy with the legacy cell
/// count and both tags, `size >= 0`, a non-null reference-array
/// `elementData`, unsigned index-vs-size and index-vs-length, the
/// `view_source_in` trailing-slot marker), the same answer loads, and every
/// miss a decline to the unchanged call (never a deopt). The lowerer receives
/// the layout from the one IR door (`resolved_string_layout.and_then(|l|
/// l.array_list)`), so a site the pin releases is a site the prefix sees.
/// NOT `OnceLock`-cached: compile time only.
pub(crate) fn ir_tier_lowers_arraylist_prefix() -> bool {
    true
}

/// `CRATONVM_JIT_NO_ARRAYLIST_PIN=1` switches [`arraylist_prefix_pins_single_pass`]
/// off (the one-binary A/B lever). Default: the pin is on.
pub(crate) fn arraylist_prefix_pin_enabled() -> bool {
    !false
}

/// Round 9 wave 9 (`evict9`,
/// `retired/perf-collection-natives-cost-a-generic-native-dispatch-per-call-20260918-RETIRED-20260920.md`):
/// does this method have an `invokevirtual` / `invokeinterface` site the
/// single-pass backend answers with the exact-`ArrayList` `get` / `size` prefix?
/// Then an optimizing-tier body -- which lowers the same site as a plain call
/// into the MIC helper and the native -- is a DOWNGRADE, the String pin's
/// species ([`string_intrinsic_pin_declines`]).
///
/// Measured on the w8b binary, `CRATONVM_C2_SUPERSEDE=0` (single-pass body kept)
/// against the default (the C2 body supersedes it), same session:
///
/// | probe | default | single-pass body |
/// |---|---:|---:|
/// | `AlProbe list` (`List`-typed, round 4) | 876-2017 ms | 25 ms |
/// | `CratonBenchC2 pipeline` | 208 / 223 ms | 45 / 45 ms |
/// | `SpliceCastProbe` | 706-730 ms | 90-96 ms when the single-pass body won the publication race |
///
/// Asks exactly what the emitter asks: a resolved ArrayList layout on this
/// compile, a site `try_resolve_arraylist_intrinsic` answers, and no armed ZGC
/// read barrier (under which the emitter declines the prefix, so pinning would
/// buy nothing). PURE apart from the resolver calls.
pub(crate) fn arraylist_prefix_pins_single_pass(
    invoke_ops: &[(usize, u16, u8)],
    cp_invoke_resolver: Option<&dyn Fn(u16) -> Option<(String, String, String)>>,
    layout: Option<StringFieldLayout>,
) -> bool {
    if ir_tier_lowers_arraylist_prefix() || !arraylist_prefix_pin_enabled() {
        return false;
    }
    let Some(al) = layout.and_then(|l| l.array_list) else {
        return false;
    };
    let Some(resolver) = cp_invoke_resolver else {
        return false;
    };
    if crate::x64::zgc_read_barrier_blocks_inline_fields() {
        return false;
    }
    invoke_ops.iter().any(|&(_, cp_idx, opcode)| {
        matches!(opcode, 0xb6 | 0xb9)
            && resolver(cp_idx).is_some_and(|(class_name, method_name, descriptor)| {
                try_resolve_arraylist_intrinsic(&class_name, &method_name, &descriptor, Some(al))
                    .is_some()
            })
    })
}

/// Is per-stage reporting of the optimizing tier's refusals switched on?
///
/// The IR pipeline has four stages that can decline a method — `ir_compatible`,
/// `IrBuilder::build`, the live-`NewArray` check, and `ir_lower::lower_inner` —
/// and every one of them used to decline in silence, producing the identical
/// observable: no IR body. That is what made "the optimizing tier emits nothing"
/// unfalsifiable for the whole of the relocation-contract investigation
/// (`jit-ir-relocation-map-contract.md`): the absence never named
/// which stage produced it. Each stage now reports under this flag.
pub fn ir_stage_reporting() -> bool {
    crate::x64::dbg_jitc_enabled()
        || false
}

/// `CRATONVM_JIT_IR_OVER_INTRINSIC=1` — let the optimizing tier take a method
/// that contains a call-site intrinsic, losing the intrinsic to a dispatch.
///
/// Off by default; see the eligibility loop for the measurement. Exists so the
/// trade can be re-measured in one binary if the IR tier ever grows an
/// intrinsic emitter, at which point this whole refusal should go away.
pub fn ir_over_intrinsic_enabled() -> bool {
    false
}

pub fn ir_direct_calls_enabled() -> bool {
    direct_jit_callee_calls_enabled()
}

/// Whether the moving young generation is currently vetoing the optimizing
/// (C2 / IR) tier — and, the first time it does, say so.
///
/// The veto itself is correct: IR lowering publishes no exact-RBP and no
/// per-safepoint oop map, so a live IR frame cannot prove or rewrite its roots
/// for a relocating young collection. What was not correct is that the two
/// defaults were set independently and nothing reported the interaction, so
/// "the optimizing tier never runs" had to be rediscovered from a cluster of
/// unexplained `left: 0, right: 1` test failures and three separate open
/// throughput documents.
///
/// One line, once per process, at `warn`. It costs nothing on the compile path
/// (a relaxed atomic after the first call) and turns a cross-suite archaeology
/// exercise into an observation.
pub fn moving_young_disables_optimizing_tier() -> bool {
    // The relocation predicate, not the bare flag. Keyed on
    // `moving_young_enabled()` this reported the tier as disabled on every
    // default build — true when it was written, no longer true now that the
    // gate is scoped to whether a relocating collection can actually observe a
    // compiled frame. Reporting the old answer would be worse than reporting
    // nothing, since this function exists to make the interaction announce
    // itself *accurately*. It goes quiet exactly when the tier really does run,
    // and speaks again the moment `JIT_PUBLISHES_RELOCATION_CONTRACT` flips.
    if !x64::moving_young_relocates_compiled_frames() {
        return false;
    }
    static REPORTED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !REPORTED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        tracing::warn!(
            "[jit] optimizing (C2/IR) tier DISABLED: the moving young generation is active \
             and IR lowering publishes no exact-RBP or per-safepoint oop map, so an IR frame \
             cannot prove or rewrite its roots for a relocating collection. Every method still \
             compiles, but through the single-pass C1 backend only — the IR optimizer, its \
             inline caches and its direct-call lowering contribute nothing. \
             `CRATONVM_NO_MOVING_YOUNG=1` restores the optimizing tier and gives up compaction. \
             See jit-optimizing-tier-moving-young-gate-RETIRED-20260731.md",
        );
    }
    true
}

/// Bisect toggle (`CRATONVM_SHADOW_NO_END_GUARD`) — suppress the `end` overflow
/// guard both backends emit ahead of a shadow push, restoring the pre-guard
/// behaviour where an overrunning push stores straight on through the allocator
/// arena. Only useful to confirm that a given failure IS the overflow.
/// `CRATONVM_JIT_ZERO_SPID` (default ON) -- both backends establish the
/// "this frame has taken no safepoint" sentinel in their prologue instead of
/// leaving the safepoint-id slot holding whatever the previous frame at that
/// stack depth left there.
///
/// The slot's whole contract is that the collector reads it and matches an
/// `OopMapEntry` on the value. An uninitialised read is not merely a refused
/// cycle: safepoint ids are small and consecutive, so a stale word can equal a
/// VALID id for the method standing at that rbp, and relocation then rewrites
/// the frame against the map for a DIFFERENT program point. That is a silent
/// wrong answer, and it is the reason this is default-on with a kill switch
/// rather than a debug aid.
///
/// The two backends need different sentinels because they number their ids
/// differently -- the IR lowerer starts at 1, so `0` is free, while the
/// single-pass backend stores the bytecode pc and **bci 0 is legal**. See
/// [`crate::x64::safepoint::SP_ID_UNSET_BC_PC`].
pub fn sp_id_slot_init_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| {
        !matches!(
            Err::<String, std::env::VarError>(std::env::VarError::NotPresent).as_deref(),
            Ok("0") | Ok("false") | Ok("FALSE")
        )
    })
}

pub fn shadow_end_guard_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| !false)
}

/// Number of times a JIT-emitted shadow-stack push hit the `end` guard and
/// bailed instead of storing past the end of the thread's buffer.
///
/// Non-zero means at least one compiled method leaked pushes badly enough to
/// exhaust a 2 MiB (256K-slot) shadow stack — before the guard existed that
/// was silent heap corruption (the write ran on through the mimalloc arena,
/// including the `JvmThread`, until it left the mapping ~170 MiB later). It
/// also means the safepoints that bailed did NOT publish their oops, so their
/// compile-time `moving_young_coverage_complete` claim is not backed at
/// runtime. Until gcd d2/f (2026-09-27, found by JIT round 13) nothing read
/// this count on the GC side and a moving young cycle still trusted the claim;
/// now `vm/src/jit/conservative_roots.rs` (`shadow_bail_refuses_moving`, in
/// `refresh_moving_young_coverage_for_current_thread`) reads it as a
/// pre-filter: once it is non-zero, a moving young cycle is refused (the
/// collection falls back to the non-moving sweep) whenever a proving thread's
/// own shadow stack is still close enough to `end` for a bailed push to be in
/// flight (gcd d3/o; `CRATONVM_JIT_SHADOW_BAIL_STICKY=1` refuses every cycle
/// after the first bail, `CRATONVM_JIT_SHADOW_BAIL_BLOCKS_MOVING=0` restores
/// the old trust). Process-wide because the bail site bumps it with one
/// `lock inc` and cannot call out; it only gates the per-thread check.
pub static SHADOW_OVERFLOW_COUNT: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Address of a leaked NUL-terminated method label recorded by the most recent
/// shadow-stack overflow bail, or 0. Only written when
/// `CRATONVM_SHADOW_OVERFLOW_DIAG` is set (the label allocation is a permanent
/// leak, so it is not produced by default).
pub static SHADOW_OVERFLOW_LABEL: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// `(overflow_count, last_method_label)` when at least one shadow-stack push
/// has bailed on the `end` guard, else `None`.
pub fn shadow_overflow_status() -> Option<(usize, Option<String>)> {
    let n = SHADOW_OVERFLOW_COUNT.load(std::sync::atomic::Ordering::Relaxed);
    if n == 0 {
        return None;
    }
    let p = SHADOW_OVERFLOW_LABEL.load(std::sync::atomic::Ordering::Relaxed) as *const u8;
    let label = if p.is_null() {
        None
    } else {
        // SAFETY: the pointer, when non-zero, names a leaked NUL-terminated
        // `str` produced by the compiler for the overflowing method; it is
        // immortal and never rewritten in place.
        unsafe { std::ffi::CStr::from_ptr(p as *const std::os::raw::c_char) }
            .to_str()
            .ok()
            .map(|s| s.to_string())
    };
    Some((n, label))
}

/// How the direct-entry arms should treat a compiled callee's raw return
/// register — see `emit_inline_callee_deopt_check`.
///
/// `Debug` is derived so the interlock's own test can NAME the mode it is
/// asserting about; a failure that says `Off must force the ban on` is worth
/// more than one that says `assertion failed`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SpIcDeoptCheck {
    /// Compare against `i64::MIN` at every direct-entry call (the default).
    On,
    /// Emit no comparison at all.
    Off,
    /// Emit it except where the callee's descriptor returns `void`, which is
    /// exactly where the compared register holds no return value.
    SkipVoid,
}

/// `CRATONVM_JIT_SP_IC_DEOPT_CHECK` = `0` | `void` | anything else (default).
pub fn sp_ic_deopt_check_mode() -> SpIcDeoptCheck {
    use std::sync::OnceLock;
    static G: OnceLock<SpIcDeoptCheck> = OnceLock::new();
    *G.get_or_init(|| {
        match Err::<String, std::env::VarError>(std::env::VarError::NotPresent).as_deref() {
            Ok("0") => SpIcDeoptCheck::Off,
            Ok("void") => SpIcDeoptCheck::SkipVoid,
            _ => SpIcDeoptCheck::On,
        }
    })
}

/// May a STATICALLY BOUND site bake a direct `CALL` to a callee that declares
/// its own exception table?
///
/// The sibling of `mic_publish_exception_table_callees` (`vm/src/jit/helpers.rs`)
/// for the other door. Both `callee_compiler` ladders refuse such a callee for
/// the same stated reason — a raw `CALL` has no Rust frame to notice the
/// `i64::MIN` sentinel and run the callee's own handler — and that reason has
/// the same answer: `emit_inline_callee_deopt_check` is emitted after the baked
/// `CALL` too (`x64/bytecode_walk.rs`, the `invokestatic` and `invokespecial`
/// direct-call arms), and `jit_service_callee_deopt` resolves a statically bound
/// callee by name.
///
/// Two interlocks, because a direct `CALL` has one precondition the inline
/// cascade does not:
///
///  * `sp_ic_deopt_check_mode() == On`, exactly as the MIC gate requires; and
///  * the emitter must have been able to reserve the contiguous service-argument
///    slots that check needs. A site that could not is now a compile failure
///    (`direct-call-service-slots`) rather than an unserviced raw edge, so
///    "bound" implies "serviced" for every Java callee.
///
/// **Default-ON since 2026-08-21.** `CRATONVM_JIT_DIRECT_EXC_TABLE_PUBLISH=0`
/// restores the ban.
///
/// The measurement it was waiting for was taken and it is two-sided, so read
/// both halves before changing this again
/// (`osr-door-refused-every-exception-table-callee-FIXED-20260824.md`):
///
///  * **per call, 10.2x.** `probes/NativeFunnelFloorProbe.java`, ABBA on one
///    binary: a static callee with a never-taken `try`/`catch`, reached from an
///    ordinary frame, goes 107.30 -> 10.57 ns/op with both controls flat. Under
///    the ban it takes `jit_invoke_dispatch` on every call — 1 187 000 funnel
///    entries against zero for the same callee without the table.
///  * **per workload, nothing measurable.** 3-228 such sites per netty class
///    against 117-1 252 left on the helper, `native-shadow` dominating every
///    one, and an ABBA on the 228-site class moving nothing against a 39 %
///    spread.
///
/// So the flip is NOT justified by a workload win, and pretending otherwise
/// would be the kind of claim this tree does not make. What justifies it is
/// consistency and cost: the virtual/interface sibling
/// (`mic_publish_exception_table_callees`) had the IDENTICAL ban for the
/// IDENTICAL stated reason, was measured at 8.7x, and was lifted — leaving the
/// statically bound door, which is the EASY case, 11x worse than the virtual
/// one for the same callee (9.2 ns against 107). A default that makes the
/// simpler path the slower path is not a conservative default, it is a bug that
/// happens to be quiet.
///
/// The stated reason for the ban has expired, and `probes/ExcTableDirectCallOracle.java`
/// is what says so rather than the argument above: a callee's own handler,
/// `finally`, wrong-type non-catch, nested catch and rethrow all behave
/// identically under HotSpot, under the ban and with it lifted — including the
/// IMPLICIT AIOOBE/NPE/div-by-zero cases, which are the ones that actually
/// leave through the `i64::MIN` sentinel the ban's reason names.
///
/// The population note that used to stand here (16 of 892 refused binds on
/// `BigEndianHeapByteBufTest`) is still true and is still the reason not to
/// expect a workload win; the current census puts it at 17 of 732.
pub fn direct_call_exc_table_publish_enabled() -> bool {
    static G: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *G.get_or_init(|| {
        direct_call_exc_table_publish_decision(
            Err::<String, std::env::VarError>(std::env::VarError::NotPresent)
                .as_deref()
                .ok(),
            sp_ic_deopt_check_mode(),
        )
    })
}

/// The policy of [`direct_call_exc_table_publish_enabled`] as a pure function
/// of its two inputs.
///
/// Split out so the decision table can be unit-tested without an environment
/// variable. The public entry point memoises in a `OnceLock` and reads the
/// process environment, so a test that exercised IT would have to mutate
/// `environ` — which is a data race in a parallel test binary, not a
/// visibility question, and is exactly the pattern that was removed from
/// `native-builtins` on 2026-08-20.
pub(crate) fn direct_call_exc_table_publish_decision(flag: Option<&str>, check: SpIcDeoptCheck) -> bool {
    // The interlock is NOT a knob, and the default flipping does not make it
    // one: publishing while the sentinel check is suppressed is unsound, so a
    // suppressed check forces the ban back on whatever the flag says.
    if check != SpIcDeoptCheck::On {
        return false;
    }
    !matches!(flag, Some("0") | Some("false") | Some("off"))
}

pub fn direct_jit_callee_calls_enabled() -> bool {
    // A raw JIT-to-JIT call produces a callee frame with no `JitEntryGuard`, so
    // it is not reachable from the entry chain: the active-RBP mirror points at
    // the callee while the root-chain metadata still names the caller.
    //
    // 2026-07-31, ROUND 1: the gate was closed under moving-young because
    // `BasicErrorControllerIntegrationTests` SIGSEGV'd on every gate-open run,
    // and the comment here blamed a GC "selecting an incompatible oop map and
    // reclaiming a live root". ROUND 2 measured that and it is wrong on both
    // counts. The root scan behaves correctly: `chain_entry_rbp_is_foreign`
    // detects the unguarded callee frame, the per-cycle proof is incomplete,
    // `moving_young_precise_only` refuses it, and the collection diverts to the
    // non-moving sweep. Nothing is reclaimed. What actually killed the process
    // was plain machine code:
    //
    //   The inline PIC cascade's inter-slot `JNE` was a `rel8`, sized by a
    //   comment claiming "a single slot body is ~30 bytes". A slot body had
    //   since grown the post-call innermost-RBP republish and the callee-deopt
    //   service check, putting it past 127 bytes. The patch truncated the
    //   displacement (`rel as u8`) behind a `debug_assert!`, so RELEASE builds
    //   silently branched to `JNE -128` — into the middle of the pre-call
    //   shadow-stack push. Executed from there, the push ran as an infinite
    //   loop with nothing reloading `top`, marched off the end of the thread's
    //   2 MiB shadow buffer, overwrote the allocator arena behind it (including
    //   the `JvmThread`), and faulted ~170 MiB later at the arena end.
    //
    // Three things had to change before this could reopen, all of them fixes in
    // their own right: the inter-slot branch is now `rel32` and no `rel8` patch
    // may truncate (`patch_rel8_or_bail`); both backends now emit the `end`
    // overflow guard `ShadowStack::END_OFFSET` was always documented as having
    // (an overrunning push bails instead of corrupting the heap); and the
    // single-pass sibling tail-call now restores the shadow `top` watermark its
    // epilogue-without-ret used to skip.
    //
    // Acceptance: `BasicErrorControllerIntegrationTests`, default flags,
    // 14 consecutive clean runs, plus a same-binary gate-closed control.
    //
    // RUN AND MET, 2026-08-01: 14 consecutive clean runs on default flags, plus
    // 2 of 2 clean gate-closed controls, on one binary.
    //
    // It could not be run before that date, and not because of this gate: the
    // class failed 23 of 26 tests under JIT with the gate OPEN and with
    // `CRATONVM_JIT_DIRECT_CALLEE_CALLS=0` alike, on a defect with nothing to
    // do with direct calls (`run_jit_callee_handler` rebuilt a compiled
    // callee's handler frame from its incoming arguments and lost every other
    // local). Anyone reading a failure of this class as evidence against this
    // gate between 2026-07-31 and 2026-08-01 was reading the wrong defect; see
    // basicerrorcontroller-jit-only-failure-20260731-FIXED.md.
    //
    // The class is not immune to two intermittent failures unrelated to this
    // gate — a stall in Spring Boot's two-thread `OnClassCondition` filtering
    // and one SIGSEGV in an unmapped code buffer, roughly 2 in 54 runs across
    // 2026-08-01. Arm `--stack-dump-on-timeout` when running the gate, and read
    // the two reports under docs/known-issues before concluding anything from a
    // red run.
    // It could not be run before that date, and not because of this gate: the
    // class failed 23 of 26 tests under JIT with the gate OPEN and with
    // `CRATONVM_JIT_DIRECT_CALLEE_CALLS=0` alike, on a defect with nothing to
    // do with direct calls (`run_jit_callee_handler` rebuilt a compiled
    // callee's handler frame from its incoming arguments and lost every other
    // local). Anyone reading a failure of this class as evidence against this
    // gate between 2026-07-31 and 2026-08-01 was reading the wrong defect; see
    // basicerrorcontroller-jit-only-failure-20260731-FIXED.md.
    //
    // The OPTIMIZING-TIER gate stays scoped as
    // `moving_young_relocates_compiled_frames()` — that one protects frames
    // which DO carry a guard. These two gates ask different questions and must
    // not share a predicate; see
    // `x64::flag_and_header_contracts::direct_jit_callee_calls_open_under_moving_young`.
    true
}

thread_local! {
    /// Per-thread override for [`ir_virtual_calls_enabled`], for tests that must
    /// exercise the "virtual calls declined" routing without mutating a
    /// process-global setting (which would race parallel test threads). `None`
    /// means use the caller-supplied, centrally parsed VM policy.
    /// Mirrors the `SELFREC_DIRECT_TEST_OVERRIDE` convention.
    pub(crate) static IR_VIRTUAL_CALLS_TEST_OVERRIDE: std::cell::Cell<Option<bool>> =
        const { std::cell::Cell::new(None) };
}

/// Test hook: force IR virtual/interface call lowering on (`Some(true)`) / off
/// (`Some(false)`) for the CURRENT thread, or restore caller policy (`None`).
/// Thread-local so parallel tests don't race. Not part of the stable API.
#[doc(hidden)]
pub fn __set_ir_virtual_calls_override(v: Option<bool>) {
    IR_VIRTUAL_CALLS_TEST_OVERRIDE.with(|c| c.set(v));
}

/// IR inline-cache lowering — may the optimizing IR pipeline lower
/// `invokevirtual` / `invokeinterface` (jit-inlining-and-ir-calls)?
///
/// The capability itself is not new, but it was gated **opt-IN** at the VM call
/// sites for a single, now-obsolete
/// reason: the IR lowered every virtual call through the generic
/// `jit_invoke_dispatch` helper with no inline cache, so admitting virtual calls
/// made call-heavy methods SLOWER than the single-pass body they replaced —
/// single-pass has emitted a MIC/PIC cascade all along.
/// `ir_lower::emit_inline_cache_call` closes that gap, so the regression risk
/// that justified opt-in is gone and the flag inverts to a diagnostic opt-OUT,
/// matching every other IR capability gate in this file
/// (`CRATONVM_JIT_IR_CALL`, `…_CALL_SPECIAL`, `…_LONG`, `…_FP`,
/// `…_SELFREC_DIRECT`, `…_IR_DIRECT_CALL`), all of which default to true.
///
/// Configuration is parsed once by the VM and supplied through
/// `ir_emit_virtual_calls`; the compiler library does not read process-global
/// environment state. The test override only substitutes that supplied value.
pub fn ir_virtual_calls_enabled(requested: bool) -> bool {
    if let Some(v) = IR_VIRTUAL_CALLS_TEST_OVERRIDE.with(|c| c.get()) {
        return v;
    }
    requested
}

/// Number of platform C-ABI integer argument registers a compiled callee entry
/// reads its incoming arguments from — the planner-side mirror of
/// `ir_lower::ENTRY_ABI_REGS`.
///
/// The IR's direct-call and inline-cache hit paths marshal in registers only
/// (there is no stack-argument path), so a site whose receiver + arguments +
/// hidden VM context pointer exceed this must keep helper dispatch. Kept here so
/// the planner and the lowerer cannot disagree about the bound.
pub(crate) const fn ir_entry_abi_reg_count() -> usize {
    #[cfg(target_os = "windows")]
    {
        4
    }
    #[cfg(not(target_os = "windows"))]
    {
        6
    }
}

/// A dynamically dispatched bytecode site owns an inline cache regardless of
/// whether its declared signature appears in the direct-call recursion deny
/// set. The cache stores only concrete receiver targets resolved later by the
/// runtime helper; it never bakes the declared target.
#[inline]
pub(crate) const fn invoke_kind_uses_inline_cache(invoke_kind: u8) -> bool {
    matches!(invoke_kind, 0 | 2)
}

/// JIT round 21 wave 4, lane ic (IC21-2): hand a fresh `invokevirtual`
/// site's inline-cache slot to the VM's compile-time binder
/// (`CompileRequest::mega_site_binder`), so the emitter can bake the
/// shared-table selector and column (`runtime_lowering::mega_vtable_column_baked_for`).
/// Only with `CRATONVM_JIT_MEGA_VTABLE_COLUMN` and
/// `CRATONVM_JIT_MEGA_VTABLE_COLUMN_BAKED` on; never for an interface or bound site.
pub(crate) fn offer_mega_site_bind(req: &CompileRequest<'_>, info: &JitInvokeInfo, pic: &JitPICSlot) {
    if info.invoke_kind != 0 || pic.is_bound_site() {
        return;
    }
    let Some(binder) = req.mega_site_binder else {
        return;
    };
    if !crate::runtime_lowering::mega_vtable_column_enabled()
        || !crate::runtime_lowering::mega_vtable_column_baked_enabled()
    {
        return;
    }
    binder(info, pic);
}

/// The loaded class an invoke site's `class_name` names, for the
/// recursion-cycle sets (`JitVerdictRegistry::direct_call_requires_dispatch_of`):
/// the constant pool's own resolution of the site's class, while `class_name`
/// IS that class. A selection-start or private-owner redirect names another
/// class, whose identity the site does not carry, so it answers `None` (match
/// by names). Called only when a cycle member has the site's names.
pub(crate) fn cycle_site_class_id(
    cp_invoke_resolver: &dyn Fn(u16) -> Option<(String, String, String)>,
    cp_invoke_class_id_resolver: Option<&dyn Fn(u16) -> Option<u32>>,
    cp_idx: u16,
    class_name: &str,
) -> Option<cratonvm_types::ClassId> {
    let (cp_class, _, _) = cp_invoke_resolver(cp_idx)?;
    if cp_class != class_name {
        return None;
    }
    let resolve = cp_invoke_class_id_resolver?;
    resolve(cp_idx).map(cratonvm_types::ClassId::new)
}

/// The class the recursion-cycle checks must name a call site by when it is
/// not the site's own class: the class that DECLARES the resolved method, for
/// a site that names the constant-pool class (`names_cp_class`; no private /
/// final pin or selection-start redirect, which already name a declaring or
/// selecting class) and resolves to an INHERITED method.
///
/// The compile stack and the cycle sets key every member by its declaring
/// class (`JitCompileMethodKey::from_cached`), so `invokestatic Sub.f`, which
/// JVMS 5.4.3.3 resolves to `A.f`, never matched an open `A.f` and never
/// closed the cycle it is on (interpreter round i1 wave 13, L3; the VM's
/// eager-chain guard and cache probe ask by the declaring class too).
///
/// The declaring resolver takes the VM's class-manager lock and walks the
/// hierarchy, so it is asked only when a member could match at all: an open
/// compile or a recorded cycle member has this name and descriptor.
/// `None` keeps the site's class. The answer is a name; the ladders name the
/// same class's identity with [`cycle_site_declaring_class_id`] (wave 17), and
/// ask by names alone when that has no answer (the conservative direction).
pub(crate) fn cycle_site_declaring_class(
    verdicts: &JitVerdictRegistry,
    cp_invoke_declaring_class_resolver: Option<&dyn Fn(u16) -> Option<String>>,
    cp_idx: u16,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
    names_cp_class: impl FnOnce() -> bool,
) -> Option<String> {
    let resolve = cp_invoke_declaring_class_resolver?;
    if !verdicts.cycle_members_may_name_method(method_name, descriptor) || !names_cp_class() {
        return None;
    }
    resolve(cp_idx).filter(|declaring| declaring != class_name)
}

/// The loaded class [`cycle_site_declaring_class`] named, as the VM's
/// `CompileRequest::cp_invoke_declaring_class_id_resolver` answers it, for the
/// cycle asks' class identity (`JitVerdictRegistry::direct_call_requires_dispatch_of`,
/// `note_recursive_compile_cycle`): a cycle through one loader's `A.f` then
/// keeps only calls into THAT `A.f` on dispatch, as it already did for a site
/// named by its own class (interpreter round i1 wave 12). `None` (no resolver,
/// no answer, or the `0` sentinel) matches by names. Asked lazily, only when a
/// cycle member has the site's names.
pub(crate) fn cycle_site_declaring_class_id(
    cp_invoke_declaring_class_id_resolver: Option<&dyn Fn(u16) -> Option<u32>>,
    cp_idx: u16,
) -> Option<cratonvm_types::ClassId> {
    let resolve = cp_invoke_declaring_class_id_resolver?;
    resolve(cp_idx)
        .filter(|&id| id != 0)
        .map(cratonvm_types::ClassId::new)
}

/// Pack a `multianewarray` site's `(holder_class_id, cp_idx)` into the single
/// i64 immediate the backend hands to the `multianewarray_n` helper.
///
/// Windows x64 gives a helper only four register arguments and the helper
/// already spends three on `(vm_ptr, ndims, dims_ptr)`, so the site description
/// has to fit in one. `ClassId` is a `u32` and a constant-pool index is a
/// `u16`, so both fit with room to spare: class id in the low 32 bits, cp index
/// in the next 16.
///
/// It carries no arity, and does not need to: `multianewarray_alloc` keys its
/// own plan cache on `(class, cp_index, ndims)` and takes the dimensions as a
/// slice, so one site descriptor serves every arity.
///
/// The helper cannot be given a pre-resolved class instead. Resolution defines
/// array classes and can run a user `ClassLoader.loadClass` — arbitrary Java —
/// which a background compile thread must not do; it belongs on the executing
/// thread at first execution, the way the `new`/`anewarray` CP-indexed helpers
/// already work. See [`unpack_multianewarray_site`] for the inverse, and
/// `vm::jit::helpers::jit_multianewarray_n` for the consumer.
///
/// The top 16 bits carry the code's constant-pool stamp, truncated
/// (interpreter round i1 wave 21, lane L1,
/// `interpreter-L1-other-cp-indexed-jit-helpers-carry-no-cp-stamp-FIXED`): the
/// [`current_compile_cp_stamp`] modulo [`MULTIANEWARRAY_STAMP_MODULUS`], plus
/// one, `0` meaning none. The full stamp does not fit (the site word is the
/// only argument left, see above); [`multianewarray_site_stamp`] rebuilds it
/// against the count the helper reads, exactly unless the code is older than
/// `MULTIANEWARRAY_STAMP_MODULUS` redefinitions in the process.
#[inline]
#[must_use]
pub fn pack_multianewarray_site(holder_class_id: u32, cp_idx: u16) -> i64 {
    let stamp =
        current_compile_cp_stamp().map_or(0, |stamp| stamp % MULTIANEWARRAY_STAMP_MODULUS + 1);
    (u64::from(holder_class_id) | (u64::from(cp_idx) << 32) | (stamp << 48)) as i64
}

/// Inverse of [`pack_multianewarray_site`]: `(holder_class_id, cp_idx)`. The
/// stamp is [`multianewarray_site_stamp`]'s.
#[inline]
#[must_use]
pub fn unpack_multianewarray_site(site: i64) -> (u32, u16) {
    let bits = site as u64;
    (bits as u32, (bits >> 32) as u16)
}

/// The modulus of the truncated constant-pool stamp a
/// [`pack_multianewarray_site`] word carries: its 16-bit field holds
/// `stamp % MODULUS + 1`, so `0` stays free for "no stamp".
pub const MULTIANEWARRAY_STAMP_MODULUS: u64 = 0xFFFF;

/// The constant-pool stamp (a class-redefinition count, as
/// [`cp_holder_word_parts`] answers it) of the code that built the
/// [`pack_multianewarray_site`] word `site`, rebuilt against `now`, the count
/// the helper reads: the newest count not above `now` that the truncated field
/// names. `None` when the word carries no stamp or names a count above `now`.
///
/// Exact while the code is younger than [`MULTIANEWARRAY_STAMP_MODULUS`]
/// redefinitions. Older code is answered a NEWER stamp than its own, so a
/// translation covers only the redefinitions after that: never a translation
/// the code did not need (a holder not redefined since reads its index as it
/// is, as before the stamp existed).
#[inline]
#[must_use]
pub fn multianewarray_site_stamp(site: i64, now: u64) -> Option<u64> {
    let low = ((site as u64) >> 48).checked_sub(1)?;
    if low > now {
        return None;
    }
    Some(now - (now - low) % MULTIANEWARRAY_STAMP_MODULUS)
}

/// Try to JIT-compile a cached bytecode method.
///
/// The `helpers` parameter provides function pointer addresses for runtime callbacks
/// that will be embedded into the generated machine code.
///
/// round-7 fix (bug 1): wraps the inner pipeline so we can record a
/// permanent bail when the backend returns None.  See the bail-list
/// notes above.
///
/// wire-tiered-manager Step 3 — the trailing `optimize` flag selects the
/// backend per call: `true` (every historical caller) runs the optimizing IR
/// pipeline (the C2-equivalent tier); `false` skips IR lowering / escape
/// analysis / scheduling and routes straight to the single-pass `x64::compile`
/// backend — the fast, low-latency **C1** tier. The tiered manager's background
/// compile worker derives the flag from the task's target tier via
/// [`tiered::tier_uses_optimized_backend`], turning what used to be an advisory
/// hint into real backend routing.
#[allow(clippy::type_complexity)]
pub fn try_compile(
    cached: &CachedBytecodeMethod,
    cp_class_name_resolver: Option<&dyn Fn(u16) -> Option<String>>,
    cp_field_resolver: Option<&dyn Fn(u16) -> Option<(usize, u8, Option<(u32, bool)>)>>,
    cp_static_field_resolver: Option<&dyn Fn(u16) -> Option<(u32, usize, u8, bool)>>,
    cp_invoke_resolver: Option<&dyn Fn(u16) -> Option<(String, String, String)>>,
    callee_compiler: Option<&dyn Fn(&str, &str, &str) -> Option<(usize, bool)>>,
    cp_new_resolver: Option<&dyn Fn(u16) -> Option<JitNewSite>>,
    cp_ldc_resolver: Option<&dyn Fn(u16) -> Option<JitLdcConstant>>,
    cp_ldc2w_resolver: Option<&dyn Fn(u16) -> Option<(i64, bool)>>,
    profile: Option<&profile::MethodProfile>,
    helpers: &JitRuntimeHelpers,
    inline_resolver: Option<&dyn Fn(&str, &str, &str) -> Option<InlineSite>>,
    string_layout_resolver: Option<&dyn Fn() -> Option<StringFieldLayout>>,
    cp_invoke_class_id_resolver: Option<&dyn Fn(u16) -> Option<u32>>,
    cp_elidable_init_resolver: Option<&dyn Fn(u16) -> bool>,
    optimize: bool,
    ir_emit_calls: bool,
    ir_emit_special_calls: bool,
    ir_emit_long: bool,
    ir_emit_virtual_calls: bool,
    ir_emit_fp: bool,
    cp_invokedynamic_descriptor_resolver: Option<&dyn Fn(u16) -> Option<(String, usize)>>,
) -> Option<CompiledMethod> {
    // Thin compatibility wrapper: the overwhelming majority of callers
    // (every `jit` crate test, plus any VM call site that hasn't been
    // updated to supply one) have no need for the JVMS §6.5 `invokespecial`
    // super-call redirect below — `None` here reproduces this function's
    // exact pre-existing resolution behavior byte-for-byte.
    try_compile_with_invokespecial_resolver(
        cached,
        cp_class_name_resolver,
        cp_field_resolver,
        cp_static_field_resolver,
        cp_invoke_resolver,
        None,
        callee_compiler,
        cp_new_resolver,
        cp_ldc_resolver,
        cp_ldc2w_resolver,
        profile,
        helpers,
        inline_resolver,
        string_layout_resolver,
        cp_invoke_class_id_resolver,
        cp_elidable_init_resolver,
        optimize,
        ir_emit_calls,
        ir_emit_special_calls,
        ir_emit_long,
        ir_emit_virtual_calls,
        ir_emit_fp,
        cp_invokedynamic_descriptor_resolver,
        None,
        None,
        // No IR-tier inline resolver either: `try_compile`'s callers are this
        // crate's own tests, which have no class manager to resolve a callee
        // body against. `None` keeps `IrBuilder` splicing nothing.
        None,
        // jdk_only: `Compatible`. No VM is in scope, and no VM calls this
        // wrapper — every VM compile door passes its own
        // `dispatch_policy(shared).is_jdk_only()` to the full form. This
        // wrapper used to read the process-global latch, which nothing set in
        // a build without a VM, so it always answered `Compatible` here too.
        false,
        // No registry in scope from this wrapper. `None` refuses, which is
        // what the blanket strict refusal did before §4.
        None,
        // No VM, so no per-VM de-spec registry: nothing is de-spec'd.
        None,
        // No VM method resolution: `Atomic*` intrinsics match the exact
        // constant-pool class only.
        None,
    )
}



/// One compile request's answers from the VM's `sync_direct_lookup`, keyed by
/// the Methodref's constant-pool index.
pub(crate) type SyncDirectLookupMemo = HashMap<u16, Option<crate::compile_request::SyncDirectTarget>>;

/// `lookup(cp_idx)`, asked at most once per `cp_idx` for the life of `memo`.
///
/// The optimizing tier consults the lookup twice for one `static
/// synchronized` site (its caller-held CALL row in the invoke planner, then
/// the SS-2 static splice's mirror), the single-pass tables once more, and a
/// request may run those pipelines more than once (the IR splice rebuild, the
/// local-handler retry, the single-pass fall-through, the parity shadow). Each
/// VM answer is
/// `jit_bridge::sync_direct_target`, which may COMPILE an unpublished callee
/// (`sync_direct_compile_unpublished_callee`) and pins the answer's owner, so
/// a callee whose compile failed was compiled again by every later consumer
/// (`r14w4-review4-wave3-review-findings-FIXED-20260929.md` item 2b). The answer is
/// a function of the class state during one compile; the memo also makes
/// every consumer of one request see the SAME answer (and push its pin once).
/// The borrow is never held across
/// `lookup`, which may re-enter the JIT for a different method (with its own
/// request and memo).
pub(crate) fn sync_direct_lookup_memoized(
    memo: &std::cell::RefCell<SyncDirectLookupMemo>,
    lookup: &dyn Fn(u16) -> Option<crate::compile_request::SyncDirectTarget>,
    cp_idx: u16,
) -> Option<crate::compile_request::SyncDirectTarget> {
    if let Some(&answer) = memo.borrow().get(&cp_idx) {
        return answer;
    }
    let answer = lookup(cp_idx);
    memo.borrow_mut().insert(cp_idx, answer);
    answer
}


/// Compile one method through the method-entry door.
///
/// Admission, the local-handler retry, bail-listing and the JFR decision
/// record all live here; every input comes from `req`. The positional
/// `try_compile` and [`try_compile_with_invokespecial_resolver`] are thin
/// builders over this for tests and simple callers.
pub fn try_compile_request(req: &CompileRequest<'_>) -> Option<CompiledMethod> {
    // t1 wave 3 (lane Y): every `--real-jdk` refusal this compile records
    // (`direct_native_helper`'s thin-native binds) goes to THIS VM's sinks.
    let _jdk_only_sinks = crate::jdk_only_sink::JdkOnlySinkScope::enter(req.jdk_only_sinks);
    let cached = req.cached;
    // The admission gate. Four checks and two side effects, all of which used
    // to live inline here and NONE of which the other two backend doors (the
    // eager first-call compile and `compile_osr_artifact`) applied in full —
    // see `compile_gate`'s module doc for the three-door table and what each
    // door had hand-copied. The token owns the compile-epoch witness, so it
    // must outlive the whole pipeline below.
    //
    // The historical notes each check was written under are preserved verbatim
    // beneath, because they record *why* a whole-method refusal is worth its
    // cost and every one of them was paid for by a real defect.
    //
    // round-7 fix (bug 1): short-circuit re-attempts on methods the
    // backend already permanently bailed on.  Avoids ~50µs of wasted
    // scan/IR/lowering work per re-attempt (every 2000 invocations
    // under the default interpreter warmup gate).
    let admission = match compile_gate::admit(
        // THIS VM's bail list, the one the refusal below records into.
        req.verdicts(),
        cached.declaring_class_id,
        &cached.class_name,
        &cached.method_name,
        &cached.method_descriptor,
        compile_gate::CompileDoor::MethodEntry,
        // M7: THIS VM's runtime de-speculation verdicts, not the process's.
        // `&**r` rather than `Arc::as_ref`: `Arc` has both an inherent `Deref`
        // and an `AsRef` impl, so the bare path is ambiguous.
        req.runtime_despec.map(|r| &**r),
    ) {
        Ok(a) => a,
        Err(_) => return None,
    };

    // NO PACKAGE IS BANNED HERE ANY MORE. What used to sit at this point was a
    // set of hand-written "keep the final admission gate aligned with the VM
    // static skip-list" mirrors — `java/math/BigInteger`,
    // `com/sun/org/apache/xerces/internal/`, `org/hsqldb/` (twice),
    // `org/yaml/snakeyaml/emitter/Emitter.emit`, and earlier
    // `org/glassfish/jaxb/`. `d1979bec5` (2026-08-01) deleted every one of them
    // together with `vm/src/jit/skip_list.rs`, the file they mirrored, because
    // each ban's underlying defect had been fixed elsewhere.
    //
    // Their prose outlived them here by four days, and it did real damage:
    // `docs/known-issues/springboot/flywayautoconfigurationtests-silent-hang-…`
    // (2026-08-05) reasoned from the surviving `org/hsqldb/` paragraph that the
    // Flyway HSQLDB path "is running interpreted" in a run where it had been
    // JIT-eligible for four days, and looked for the stall in the wrong place.
    // The real defect was `383e7f5cf`. See
    // `flywayautoconfigurationtests-timeout-jit-site-cache-aliasing-FIXED-20260805.md`.
    //
    // If a package ever needs to be force-interpreted again, do it through the
    // bisect levers below (which `compile_gate::admit` applies at all three
    // compile doors), not by reintroducing a gate only this door enforces.
    // DBG (RandomizedContext WeakHashMap JIT investigation, 2026-07-02):
    // `CRATONVM_JIT_DENY` — comma-separated substrings matched against
    // `Class.method`; a matching method is force-interpreted (never
    // JIT-compiled) so a single suspect compiled method can be isolated
    // from the rest of a workload's JIT-compiled code, without disabling
    // JIT wholesale. No-op unless the env var is set.
    //
    // `CRATONVM_JIT_BISECT_ONLY` is the inverse: only the listed class-name
    // prefixes stay JIT-eligible. Both now live in `jit_force_interpret`, so
    // the eager first-call and OSR codegen paths — which do NOT come through
    // this function — can apply the identical predicate. See that function for
    // why sharing it matters. Both levers, and the live code-cache cap below
    // them, are now asked by `compile_gate::admit` above — one place, all three
    // doors. (Live code-cache cap: reclaimed bodies restore headroom, so it is
    // a transient admission check rather than a permanent compile stop.)

    // Consume the per-compile self-call identity proof FIRST — even an
    // early bail below must not leak a stale `true` into a later compile.
    // Stays here rather than in the gate: it is a proof the *caller* of this
    // function deposits for this one compile, and the other two doors neither
    // set nor read it.
    let self_call_identity_stable = req.self_call_identity_stable;
    // Start from a clean pipeline-stage stamp. The end of this function takes
    // it on both arms, so a compile that RETURNS never leaves one behind — but
    // a compile that PANICKED (caught by `tiered::contain_compile_panic`, which
    // turns it into a declined compile and keeps the worker thread) never
    // reached that take, and its last stage would otherwise be reported as the
    // refusal site of the next unrelated method this thread fails to compile
    // without naming a site of its own.
    let _ = take_jit_pipeline_stage();
    // Clears the armed flag now and on every exit, so the read below can only
    // see this compile's answer.
    let _local_handlers_armed_scope = LocalHandlersArmedScope::enter();
    // The one-shot bail-site clear that used to sit here is `compile_gate::
    // admit`'s job now — same discipline, every door.
    let _compile_stack_guard = JitCompileStackGuard::enter(cached);
    // Open this compile's slot in `JIT_ADMISSION_VERDICT` so the verdict
    // `try_compile_inner` builds can reach the funnel below. RAII, so an early
    // `return None` between here and the funnel cannot leave a verdict behind
    // for the next compile on this thread. Costs nothing, and allocates
    // nothing, unless `cratonvm.JitCompileDecision` is armed.
    let _jit_decision_frame = JitDecisionFrame::enter();

    // Round 14 wave 5 (lane fixup2, RV4-1): one `sync_direct_lookup` per site
    // per compile, across every tier, attempt and retry of this request. See
    // [`sync_direct_lookup_memoized`].
    let vm_sync_direct_lookup = req.sync_direct_lookup;
    let sync_direct_memo: std::cell::RefCell<SyncDirectLookupMemo> = Default::default();
    let memo_lookup = |cp_idx: u16| -> Option<crate::compile_request::SyncDirectTarget> {
        vm_sync_direct_lookup
            .and_then(|lookup| sync_direct_lookup_memoized(&sync_direct_memo, lookup, cp_idx))
    };
    let memo_req;
    let req: &CompileRequest<'_> =
        if vm_sync_direct_lookup.is_some() {
            memo_req = CompileRequest {
                sync_direct_lookup: Some(&memo_lookup),
                ..*req
            };
            &memo_req
        } else {
            req
        };

    // Inner pipeline: returns None on either a transient resolver miss
    // OR a permanent backend bail.  Only the latter pollutes the bail-
    // list, so we use a sentinel `bailed` ref-mut that the inner fn
    // sets when it traverses past the cheap resolver checks into
    // `x64::compile`.
    let mut backend_attempted = false;
    let result = try_compile_inner(
        req,
        &mut backend_attempted,
        self_call_identity_stable,
        &admission,
        false,
    );

    // ── The compiled-local-handler safety net ───────────────────────────
    //
    // Arming local handlers puts `catch` bodies into the emitted image; before
    // the feature they were dead code the walk skipped. So a construct the
    // single-pass emitter cannot model inside a `catch` block — one that has
    // never had to be modelled anywhere, because no handler body was ever
    // emitted — would turn a method that compiles today into one that does
    // not, trading a throughput win for the loss of compilation entirely.
    //
    // Retry once with the feature suppressed. That makes the worst case one
    // wasted compile rather than a permanently interpreted method, and it is
    // paid only on the failure path: a method that compiles armed never gets
    // here, and a method that fails for an unrelated reason retries once and
    // fails the same way. The bail site is cleared first so the retry's own
    // refusal (or success) is what the bail-list and the stats table see.
    let result = match result {
        Some(cm) => Some(cm),
        None if local_handlers_were_armed() => {
            let _ = take_jit_bail_site();
            let mut retry_backend_attempted = false;
            let retried = try_compile_inner(
                req,
                &mut retry_backend_attempted,
                self_call_identity_stable,
                &admission,
                true,
            );
            backend_attempted |= retry_backend_attempted;
            if retried.is_some() && crate::x64::dbg_jitc_enabled() {
                eprintln!(
                    "[cratonvm-jitc] local-handlers: {}.{}{} refused with handler bodies in the image; compiled without them",
                    cached.class_name, cached.method_name, cached.method_descriptor,
                );
            }
            retried
        }
        None => None,
    };
    // Both tiers' method-entry bodies: how many Java arguments the entry takes,
    // for the not-entrant record (`CompiledMethod::make_not_entrant`). The
    // single-pass driver stamps the same number for its other two doors.
    let result = result.map(|mut cm| {
        let java_args =
            count_param_slots(&cached.method_descriptor) + usize::from(!cached.is_static);
        cm.entry_java_args = u16::try_from(java_args).unwrap_or(u16::MAX);
        // gcd d9/d: see `CompiledMethod::method_declares_no_handlers`.
        cm.method_declares_no_handlers =
            cached.exception_table.is_empty() && !cached.is_synchronized;
        cm
    });
    // (The disarm is `try_compile_inner`'s `local_handlers_disarmed` argument,
    // not the `disarm_local_handlers_once` thread-local it replaced, so an
    // early resolver bail in the retry can leave nothing set for the next
    // method on this thread; the comment that used to stand here described
    // that thread-local.)

    // Take once and use for all three sinks: the bail-list decision below, the
    // trace line (only when `CRATONVM_DBG_JITC` is on), and the per-method
    // store the end-of-run stats table reads (always, so the reason survives
    // the compile). This used to be taken AFTER the bail-list decision, which
    // is why that decision could not consult it.
    let site = if result.is_none() {
        // A refusal with no explicit site is still a refusal, and it is about to
        // bail-list the method permanently. Attribute it to the last pipeline
        // stage entered rather than reporting `unrecorded` — see
        // [`JIT_PIPELINE_STAGE`] for why the fallback is a stage and not an
        // audit of every exit.
        Some(take_jit_bail_site().unwrap_or((take_jit_pipeline_stage(), 0, 0)))
    } else {
        // Reset the stamp even on success, so a compile that succeeds cannot
        // leave its last stage behind for the next one to report.
        let _ = take_jit_pipeline_stage();
        None
    };

    // JFR `cratonvm.JitCompileDecision` — THE one event for this compile.
    //
    // Here because this is the only point at which all three terms the audit
    // could not get out of stderr are simultaneously true and in scope:
    //
    //   * WHICH BACKEND actually produced the body — `used_ir_backend`, not the
    //     admission verdict's prediction of it. An admitted method can still
    //     fall back to single-pass inside the pipeline, and that gap is exactly
    //     the shape of the `String.charAt` finding: a method that is compiled,
    //     is fast-ish, never appears in a bail list, and has silently lost every
    //     optimization the IR tier would have applied.
    //   * WHICH DOOR asked — read straight off `admission.door()`, the gate's
    //     own token, which is already threaded through this whole function.
    //     That beats a thread-local mirror: it cannot drift from
    //     `compile_gate`'s three-door table, it cannot be forgotten by a door
    //     that neglects to set it, and it adds no fourth spelling of a term
    //     `compile_gate` exists to keep singular.
    //   * WHY — the bail site on a refusal, or the carried-forward admission
    //     verdict on a success.
    //
    // Gate checked before anything is built. `current_jit_admission_verdict`
    // clones a `String`, and the entire point of the producer-side gate is that
    // a default run never pays for one.
    if cratonvm_jfr::jit_decision::jit_decision_enabled() {
        use cratonvm_jfr::jit_decision as jd;
        // A verdict only survives to here for a compile that reached the
        // admission chain. A refusal is better described by its bail site
        // anyway, so the verdict is consulted only on the success arm.
        let verdict = current_jit_admission_verdict();
        let (reason, bail_bci, bail_opcode) = match site {
            // `(0, 0)` is `note_jit_bail_site`'s "not one bytecode's fault"
            // encoding, and 0 is a legal bci AND a legal opcode (`nop`), so it
            // must map to the sentinel rather than be passed through as a
            // coordinate a reader would believe.
            Some((s, 0, 0)) => (
                jd::DecisionText::Static(s),
                jd::NO_BAIL_SITE,
                jd::NO_BAIL_SITE,
            ),
            Some((s, pc, op)) => (
                jd::DecisionText::Static(s),
                i32::try_from(pc).unwrap_or(jd::NO_BAIL_SITE),
                i32::try_from(op).unwrap_or(jd::NO_BAIL_SITE),
            ),
            None => (
                match verdict.as_deref() {
                    Some(v) => jd::DecisionText::Borrowed(v),
                    // No verdict means the compile never reached the admission
                    // chain — `optimize` was not even asked. That is a fact
                    // about this compile, not a gap in the instrument.
                    None => jd::DecisionText::Static("compiled"),
                },
                jd::NO_BAIL_SITE,
                jd::NO_BAIL_SITE,
            ),
        };
        let outcome = match result.as_ref() {
            None => jd::CompileOutcome::Refused,
            Some(cm) if cm.used_ir_backend => jd::CompileOutcome::Optimizing,
            Some(_) => jd::CompileOutcome::SinglePass,
        };
        let duration_ns = _jit_decision_frame.elapsed_nanos();
        jd::record_jit_compile_decision(&jd::JitCompileDecision {
            // `&*` rather than `&`: these three fields are `Arc<str>` on
            // `CachedBytecodeMethod` and the event wants `&str`.
            class_name: &*cached.class_name,
            method_name: &*cached.method_name,
            method_descriptor: &*cached.method_descriptor,
            door: jfr_compile_door(admission.door()),
            outcome,
            reason,
            bail_bci,
            bail_opcode,
            bytecode_size: i32::try_from(cached.code.len()).unwrap_or(i32::MAX),
            start_time_ns: jit_decision_now_nanos().saturating_sub(duration_ns),
            duration_ns,
        });
    }

    // A code-buffer overflow is a MEASUREMENT, not a verdict: the backend now
    // records the size it wanted and the next attempt allocates from that
    // measurement instead of the heuristic (`code_buffer_hint`). Bail-listing
    // it would retire the method permanently on the strength of one wrong
    // estimate — and since the estimate is the only thing that changes between
    // the two attempts, the retry is exactly the experiment worth running.
    // Every other backend-attempted `None` is a property of the class file and
    // stays permanent.
    //
    // BOUNDED, though. Each retry is a full re-lowering plus an `mmap`/`munmap`
    // of the estimate, on the calling thread, and the exemption used to have no
    // stopping condition at all: a method the estimate could never satisfy was
    // re-lowered on every warmup-gate re-attempt for the whole run and never
    // became compiled. After `MAX_CODE_BUFFER_RETRIES` doublings (8x the first
    // failing capacity) the refusal becomes permanent like any other.
    //
    // OBS (round 10, lane `gauges`): the key is built ONCE here and shared
    // with the ledger call below. It used to be built inline inside the
    // `&&`, which is why it is a `let` now; the `Option` keeps the
    // short-circuit exactly as it was — `format!` still allocates only on the
    // code-buffer-overflow path, and `code_buffer_retries_exhausted` is still
    // called at most once per compile.
    let overflow_key = matches!(site, Some((CODE_BUFFER_TOO_SMALL_SITE, _, _))).then(|| {
        format!(
            "{}.{}:{}",
            cached.class_name, cached.method_name, cached.method_descriptor
        )
    });
    // A discarded compile whose only fault was a short buffer estimate IS a
    // refused code-cache allocation — it is `alloc_failure::SIZE_ESTIMATE_OVERRUN`
    // by that enum's own definition ("codegen overran its size estimate and
    // the emitted body did not fit the buffer it was given"). Until this call
    // existed that bucket could not leave zero, so the code-cache report said
    // "no failed allocations" for a process that had thrown away every
    // oversized method. Counted whether or not the retry is still permitted:
    // the buffer was refused either way, and the retry is a separate fact the
    // `code_buffer_bail` counters in `jfr_compile_decision` already carry.
    //
    // `code_buffer_hint` is the size the NEXT attempt will ask for — the
    // measured requirement, doubled, as its own doc explains. It is the
    // closest thing this site has to "bytes this compile could not get": the
    // capacity that actually failed was consumed inside the backend and is not
    // reachable from here without widening `note_code_buffer_shortfall`'s
    // private map, which this lane does not own. `None` (an overflow recorded
    // with a zero measurement) contributes zero bytes rather than a guess.
    if result.is_none() {
        if let Some(key) = overflow_key.as_deref() {
            note_jit_code_alloc_failure(
                code_buffer_hint(key).unwrap_or(0),
                code_alloc_failure::SIZE_ESTIMATE_OVERRUN,
            );
        }
    }
    let retryable = overflow_key
        .as_deref()
        .is_some_and(|key| !code_buffer_retries_exhausted(key));
    // Not for the optimizing OSR route's compile (interpreter round i1 wave
    // 38, lane L2): its refusal is the route's, memoised by the VM, and the
    // bail list would also close the single-pass OSR door, which compiles
    // the method through its own front end. See
    // `CompileRequest::osr_optimizing_route`.
    if result.is_none() && backend_attempted && !retryable && !req.osr_optimizing_route {
        // The heavy backend path ran and returned None — treat as
        // permanent.  Future try_compile calls for this method
        // short-circuit immediately at the check above. Into the registry
        // `admit` asked, so the next attempt's check can see it.
        req.verdicts().mark_bail_listed(
            cached.declaring_class_id,
            &cached.class_name,
            &cached.method_name,
            &cached.method_descriptor,
        );
    }
    if result.is_none() {
        if let Some(site) = site {
            req.verdicts().record_bail_reason(
                cached.declaring_class_id,
                &cached.class_name,
                &cached.method_name,
                &cached.method_descriptor,
                site,
            );
        }
        if crate::x64::dbg_jitc_enabled() {
            // `reason=` is the point of this line. `backend_attempted` alone
            // says only whether the bail is permanent (see `JIT_BAIL_SITE`),
            // which is not a diagnosis — a resolver that cannot represent an
            // `ldc <Class>` and a backend that ran out of code buffer both
            // print `true`.
            eprintln!(
                "[cratonvm-jitc] compile-bail {}.{}{} backend_attempted={} reason={}",
                cached.class_name,
                cached.method_name,
                cached.method_descriptor,
                backend_attempted,
                format_jit_bail_site(site),
            );
        }
    } else {
        req.verdicts().clear_bail_site(
            cached.declaring_class_id,
            &cached.class_name,
            &cached.method_name,
            &cached.method_descriptor,
        );
    }
    // DBG (env-gated): dump the emitted machine code for a specific method so
    // its prologue/epilogue + body can be disassembled offline. Set
    // CRATONVM_DBG_DUMP_JIT="Class.method" (slash-separated class) to target.
    if let Ok(target) = Err::<String, std::env::VarError>(std::env::VarError::NotPresent) {
        if let Some(ref cm) = result {
            let sig = format!("{}.{}", cached.class_name, cached.method_name);
            // LIST mode: print every compiled method's sig (reveals exact
            // class-name format + whether the target is compiled here at all).
            if target == "LIST" {
                eprintln!("[JIT_COMPILED] {}{}", sig, cached.method_descriptor);
            }
            if sig.contains(&target) || &*cached.method_name == target.as_str() {
                let bytes = cm.code_bytes();
                eprintln!(
                    "[JIT_DUMP] {}{} len={} entry={:p}",
                    sig,
                    cached.method_descriptor,
                    bytes.len(),
                    bytes.as_ptr(),
                );
                let mut line = String::new();
                for (i, b) in bytes.iter().enumerate() {
                    line.push_str(&format!("{:02x}", b));
                    if (i + 1) % 32 == 0 {
                        eprintln!("[JIT_DUMP] {}", line);
                        line.clear();
                    }
                }
                if !line.is_empty() {
                    eprintln!("[JIT_DUMP] {}", line);
                }
            }
        }
    }
    // DBG (spring-bug-11): list every successfully-compiled method that contains
    // a dup_x1 (0x5A), with the PCs and a small following-byte window, so the
    // crashing dup_x1 method (NO_DUP_X1 removes the Groovy SIGSEGV) can be pinned
    // and dumped. Proper opcode walk via bytecode_analysis::step so operand bytes that
    // happen to equal 0x5A are not mistaken for the opcode.
    if result.is_some() && false {
        let code: &[u8] = &cached.code;
        let n = code.len();
        let mut pc = 0usize;
        let mut hits: Vec<usize> = Vec::new();
        while pc < n {
            if code[pc] == 0x5a {
                hits.push(pc);
            }
            let l = crate::bytecode_analysis::step(code, pc);
            pc += if l == 0 { 1 } else { l };
        }
        if !hits.is_empty() {
            eprintln!(
                "[DUPX1] {}.{}{} dup_x1@{:?} code_len={}",
                cached.class_name, cached.method_name, cached.method_descriptor, hits, n
            );
            for &h in &hits {
                let end = (h + 10).min(n);
                let window: Vec<String> =
                    code[h..end].iter().map(|b| format!("{:02x}", b)).collect();
                eprintln!("[DUPX1]   @{} bytes: {}", h, window.join(" "));
            }
        }
    }
    result
}

/// Full form of [`try_compile`] taking an additional resolver for the JVMS
/// §6.5 `invokespecial` super-call redirect (`cp_invokespecial_owner_resolver`
/// below). The VM's doors no longer call it: each builds a [`CompileRequest`]
/// with its own [`CompileRealm`] (`jit_bridge::with_compile_request`), while
/// this shim has no VM in scope and compiles against
/// [`CompileRealm::process`]; its callers are [`try_compile`] and this crate's
/// tests. Kept as a separate function (rather than adding the
/// parameter to `try_compile` itself) so the ~30 existing `try_compile`
/// call sites in this crate's own test suites need no changes.
#[allow(clippy::type_complexity, clippy::too_many_arguments)]
pub fn try_compile_with_invokespecial_resolver(
    cached: &CachedBytecodeMethod,
    cp_class_name_resolver: Option<&dyn Fn(u16) -> Option<String>>,
    cp_field_resolver: Option<&dyn Fn(u16) -> Option<(usize, u8, Option<(u32, bool)>)>>,
    cp_static_field_resolver: Option<&dyn Fn(u16) -> Option<(u32, usize, u8, bool)>>,
    cp_invoke_resolver: Option<&dyn Fn(u16) -> Option<(String, String, String)>>,
    // JVMS §6.5 `invokespecial` super-call redirect: given an `invokespecial`
    // (0xb7) call-site's CP index, returns the JVMS-correct class at which
    // method selection actually begins when that differs from the plain
    // `cp_invoke_resolver` class name — i.e. a genuine `super.m(...)` whose
    // constant-pool reference names an ancestor further up than this
    // method's own direct superclass. `None` (resolver absent, or it
    // returns `None` for a given site — the overwhelmingly common case)
    // leaves `class_name` exactly as `cp_invoke_resolver` returned it. See
    // `classloading::invokespecial_selection_start` for the algorithm. The
    // answer carries the owner's class id too (see
    // `CompileRequest::cp_invokespecial_owner_resolver`).
    cp_invokespecial_owner_resolver: Option<&dyn Fn(u16, u8) -> Option<(String, u32)>>,
    callee_compiler: Option<&dyn Fn(&str, &str, &str) -> Option<(usize, bool)>>,
    // CRIT-2 / cold-`new` fix — see [`JitNewSite`]. `Resolved` carries
    // (class_id, num_fields, has_nonzero_tag_primitive_init, has_finalizer);
    // the two flags feed the inline-TLAB `new` fast path and a resolver that
    // cannot compute them must report `true, true` so the post-init helper
    // call stays in place. `Deferred` means "class not loaded yet" and
    // compiles to the CP-indexed runtime-resolving helper; only `None` (a
    // malformed site) still bails the compile.
    cp_new_resolver: Option<&dyn Fn(u16) -> Option<JitNewSite>>,
    cp_ldc_resolver: Option<&dyn Fn(u16) -> Option<JitLdcConstant>>,
    cp_ldc2w_resolver: Option<&dyn Fn(u16) -> Option<(i64, bool)>>, // inc 35: (bits, is_double)
    profile: Option<&profile::MethodProfile>,
    helpers: &JitRuntimeHelpers,
    inline_resolver: Option<&dyn Fn(&str, &str, &str) -> Option<InlineSite>>,
    // Resolves the field layout of `java/lang/String` for the String
    // call-site intrinsics. Called at most once per compilation; see
    // `StringFieldLayout`. `None` (resolver absent, or it returns `None`)
    // makes String intrinsics bail to normal dispatch.
    string_layout_resolver: Option<&dyn Fn() -> Option<StringFieldLayout>>,
    // Maps an invoke* constant-pool index to the class id of the call's
    // *declared* class (the receiver class statically named at the site).
    // Consumed by the CRC32/CRC32C `update` call-site intrinsics, whose
    // codegen emits a receiver class-id guard against this constant. `None`
    // (resolver absent, or it returns `None` for a given site) makes the
    // CRC32 intrinsic at that site bail to normal dispatch.
    cp_invoke_class_id_resolver: Option<&dyn Fn(u16) -> Option<u32>>,
    // activate-ir-optimizer (scalar-new wiring): given an `invokespecial`
    // constant-pool index, returns `true` iff it targets a constructor whose
    // *construction* is elidable for escape-analysis scalar replacement — a
    // no-arg `<init>()V` of a direct `java/lang/Object` subclass whose body is
    // exactly `aload_0; invokespecial Object.<init>()V; return` (no field
    // initialiser, no escape, no side effect). `None` (the production default
    // unless the soak flag is set) leaves scalar-replacement of `new` OFF: the
    // IR builder bails on every `invokespecial`, so allocation-bearing methods
    // take the single-pass backend exactly as before.
    cp_elidable_init_resolver: Option<&dyn Fn(u16) -> bool>,
    // wire-tiered-manager Step 3: `true` → optimizing IR pipeline (C2);
    // `false` → single-pass `x64::compile` only (the fast C1 tier). See the
    // function doc above.
    optimize: bool,
    // Gap B (activate-ir-optimizer): `true` lets the IR builder lower an
    // int-only `invokestatic` in an oop-free method to `Op::Call` (dispatched
    // via `invoke_dispatch`). `false` (the default) keeps every invoke on
    // single-pass. Gated default-OFF behind `CRATONVM_JIT_IR_CALL` at the VM
    // call sites until it soaks.
    ir_emit_calls: bool,
    // inc 24 (Gap B): `true` additionally lets the IR builder lower a resolved
    // non-`<init>` `invokespecial` (private / `super.` / non-virtual instance
    // call) to `Op::Call`, with the receiver marshalled as arg0 and
    // `invoke_kind == 1`. `false` (the default) keeps every `invokespecial`
    // on single-pass (the builder bails). Gated default-OFF behind
    // `CRATONVM_JIT_IR_CALL_SPECIAL` at the VM call sites until it soaks.
    ir_emit_special_calls: bool,
    // inc 25 (category-2 foundation): `true` lets the optimizing IR path take
    // **long**-using methods (the `method_uses_category2` gate otherwise bails
    // the whole pipeline on any long/double opcode). Only long is admitted —
    // double/float and int div/rem still bail (the latter to keep a `long`
    // off a deopt point, since long deopt-resume is a follow-up). `false` (the
    // default) preserves the int/ref-only IR path. Gated default-OFF behind
    // `CRATONVM_JIT_IR_LONG` at the VM call sites until it soaks.
    ir_emit_long: bool,
    // inc 26 (Gap B): `true` additionally lets the IR builder lower a resolved
    // `invokevirtual` (0xb6) / `invokeinterface` (0xb9) to `Op::Call`, with the
    // receiver marshalled as arg0 and `invoke_kind == 0` (virtual) / `2`
    // (interface). Dispatch is fully dynamic: the baked `JitInvokeInfo` carries
    // the static call-site class/name/descriptor and `invoke_dispatch` resolves
    // the actual target on the receiver's RUNTIME class (no inline cache in the
    // emitted code — the generic helper does the vtable/itable lookup). `false`
    // (the default) keeps every virtual/interface invoke on single-pass. Gated
    // default-OFF behind `CRATONVM_JIT_IR_CALL_VIRTUAL` at the VM call sites
    // until it soaks.
    ir_emit_virtual_calls: bool,
    // inc 30 (double/float XMM value tier): `true` admits a `float`/`double`-using
    // method to the optimizing IR path (the `method_uses_fp` gate otherwise bails
    // it to single-pass). The lowerer marshals FP values through XMM
    // (`addsd`/`cvttsd2si`/…); FP value arithmetic (`fadd`/`dmul`/…), FP
    // constants (`fconst`/`dconst`), FP-local load/store, and the int/long⇄FP
    // conversions lower. `frem`/`drem`, FP compares/branches, FP array ops, and
    // FP params/returns/call-args still bail (their builder arms are absent), as
    // do int-div-bearing and `ldc2_w`-bearing FP methods (a follow-up). `false`
    // (the default) preserves the int/long/ref IR path byte-for-byte: no
    // FP-opcode method is admitted, so the builder never sees an FP opcode. Gated
    // default-OFF behind `CRATONVM_JIT_IR_FP` at the VM call sites until it soaks.
    ir_emit_fp: bool,
    // invokedynamic-uncommon-trap fix: resolves an `invokedynamic` (0xba)
    // CP index to just its target descriptor string (e.g. via
    // `NameAndType.descriptor` — no bootstrap/`CallSite` resolution needed).
    // `jit_scan` is CP-blind and no longer bails on 0xba (it just records the
    // site); this resolver lets the single-pass backend compute the site's
    // stack effect (arg count via `count_param_slots`, return type via
    // `return_type`) so it can lower the instruction to an unconditional jump
    // to the existing uncommon-trap deopt stub (`DeoptReason::UnreachedCode`)
    // while keeping the compiler's simulated operand stack consistent for
    // whatever bytecode follows. `None` (resolver absent, or it returns `None`
    // for a given site) bails the whole compile — see `try_compile_inner`.
    cp_invokedynamic_descriptor_resolver: Option<&dyn Fn(u16) -> Option<(String, usize)>>,
    // PGO-02: maps a receiver CLASS ID (not a CP index - the runtime
    // identity a guarded speculative inline's receiver class-id check
    // resolved against) to its class name, so a Monomorphic/Bimorphic
    // InlinePlan can record a SpeculatedReceiver invalidation dependency
    // (plan_inline refuses via NoInvalidationDependency without one - the
    // fail-closed rule in docs/feature-designs/profile-guided-inlining.md).
    // `None` (resolver absent, or it returns `None` for a given id) refuses
    // every speculative virtual/interface inline at that site; static/
    // special DirectBind sites are unaffected (no receiver dependency).
    class_id_name_resolver: Option<&dyn Fn(u32) -> Option<String>>,
    // PGO-02 R0: resolves the body a receiver of EXACTLY this class id would
    // dispatch to at a call site declared `(cp_class, method, descriptor)`.
    //
    // Separate from `inline_resolver` because they answer different questions.
    // `inline_resolver` resolves the CONSTANT-POOL callee, which is the right
    // answer for `invokestatic`/`invokespecial` and the WRONG one for a
    // guarded virtual/interface site: the guard admits a runtime class, and
    // wherever that class overrides the declared method the two bodies differ.
    // Splicing the constant-pool body behind such a guard is silent wrong code
    // (see `InlineRequest::receiver_callee_resolver`). `None` — or a `None`
    // answer — refuses the speculation; it never falls back to the other
    // resolver.
    receiver_inline_resolver: Option<&dyn Fn(u32, &str, &str, &str) -> Option<InlineSite>>,
    // IR-tier inlining: resolve a callee body for `IrBuilder` to SPLICE, under
    // the optimizing tier's own admission set (`resolve_ir_inline_site` in the
    // VM). Separate from `inline_resolver` because the two answer different
    // questions: that one resolves what the single-pass emitter can splice,
    // which refuses `new`, array access and `arraylength` — the three shapes an
    // allocating accessor is made of. `None` (no VM in scope, or the gate off)
    // splices nothing, which is byte-identical to the pre-inlining IR path.
    ir_inline_resolver: Option<&dyn Fn(&str, &str, &str) -> Option<InlineSite>>,
    // Real-JDK execution policy for THIS VM's compilations.
    //
    // Was the process-global `JIT_COMPATIBILITY_MODE` latch until 2026-08-06
    // (JDK-ONLY-WAVE2 §2 of the wave-2 markers record — the record's §6 is the
    // JNI table, a different process global). The latch only ever
    // moved toward strict, so one `JdkOnly` VM silently took the thin
    // direct-call helpers away from every `Compatible` VM sharing the process
    // — the hazard contract §2's no-process-globals rule exists to prevent.
    // Threaded here because the compile path already carries per-VM state and
    // this is per-VM state; the `JitRuntimeHelpers` table was the wrong home
    // (it is a `#[repr(C)]` ABI of helper ADDRESSES with baked offsets, and a
    // policy bit is not an address).
    jdk_only: bool,
    // JDK-ONLY-WAVE2 §4: asks the registry whether a triple is a reviewed
    // `NativeKind::Intrinsic`, i.e. the §1.4 exception that MAY shadow
    // concrete bytecode. Returns `false` for `Bridge`, for `SyntheticStub`,
    // and for a triple the registry has never heard of.
    //
    // This is the policy half of the seven thin direct-call ladders below.
    // Before it existed those ladders were refused wholesale under `JdkOnly`,
    // which is stricter than the contract: three of the seven are registered
    // `Intrinsic` and §1.4 permits exactly those.
    //
    // `None` refuses everything, which is the pre-2026-08-06 behaviour and the
    // fail-closed direction — a compile with no way to ask cannot bake a
    // native in front of real bytes.
    direct_bind_admission: Option<&dyn Fn(&str, &str, &str) -> bool>,
    // The compiling VM's per-bci de-spec registry (`JitRealm::despec_registry`
    // on the VM side). Was the process-global `deopt.rs` `DESPEC_SET` until
    // 2026-09-12, which let one VM's despeculation verdicts strip speculations
    // from every other VM's compiles. `None` (no VM in scope) consults nothing.
    despec: Option<&std::sync::Arc<crate::deopt::DespecRegistry>>,
    // Review #80: maps an invoke constant-pool index to the name of the class
    // that DECLARES the method the site resolves to. It sits beside
    // `cp_invokespecial_owner_resolver` but asks a different question: that one
    // answers only when a site binds statically (a private or final owner that
    // no native screen refuses). This one answers for any resolvable
    // `Methodref`. Consumed by the ATOMIC_INT / ATOMIC_LONG registration, so a
    // `Counter extends AtomicInteger` site can reach the intrinsic. `None`
    // keeps the exact constant-pool class match.
    cp_invoke_declaring_class_resolver: Option<&dyn Fn(u16) -> Option<String>>,
) -> Option<CompiledMethod> {
    try_compile_request(&CompileRequest {
        branch_counter_resolver: None,
        cached,
        cp_class_name_resolver,
        cp_field_resolver,
        cp_static_field_resolver,
        // No VM in scope: no static field is trusted final (round 17, irfeed).
        cp_static_field_final_resolver: None,
        // This positional shim has no volatility resolver to forward: every
        // VM door builds a `CompileRequest` directly and supplies one. See
        // `CompileRequest::cp_field_volatile_resolver`.
        cp_field_volatile_resolver: None,
        // No VM in scope: no final bit (nio21 wave 2, lane L3).
        cp_field_final_resolver: None,
        // No VM in scope: no `byte[]` field fact (JIT round 20, lane memory).
        cp_field_byte_array_resolver: None,
        // No VM in scope: a field site the door cannot resolve still refuses
        // (wave 92, `CRATONVM_JIT_DEFERRED_FIELD_SITES`).
        cp_field_deferred_resolver: None,
        // No VM in scope: no resolved declaring classes (CHN15-2 producer).
        cp_field_declaring_class_resolver: None,
        cp_invoke_resolver,
        // No VM in scope: no first-call cell to hand out.
        cp_invoke_first_call: None,
        // No VM in scope: no registered constant `VarHandle` to fold.
        static_varhandle_resolver: None,
        static_final_prim_resolver: None,
        // No VM in scope: no JIT cache, so no published body (t1 w10 LCAP2).
        installed_body_needs_context: None,
        cp_invokespecial_owner_resolver,
        callee_compiler,
        cp_new_resolver,
        // No VM in scope: no uninitialized-`new` trap (EAGER19-1).
        cp_uninit_new_trap_resolver: None,
        // No VM in scope: no unresolved-field trap (wave 94, lane L2).
        cp_field_trap_resolver: None,
        // No VM in scope: no class-initialized guard (EAGER2-3).
        cp_uninit_new_guard_resolver: None,
        cp_ldc_resolver,
        // No VM in scope: no literal text to hash.
        cp_ldc_string_hash_resolver: None,
        // No VM in scope: no literal text either (C14W3-4).
        cp_ldc_string_utf16_resolver: None,
        // No VM in scope: no `getClass()` display facts (NIO20-L3-1).
        getclass_exact_fold_resolver: None,
        // No VM in scope: no compile-time shared-table bind (IC21-2).
        mega_site_binder: None,
        cp_ldc2w_resolver,
        profile,
        helpers,
        inline_resolver,
        string_layout_resolver,
        cp_invoke_class_id_resolver,
        cp_elidable_init_resolver,
        optimize,
        ir_emit_calls,
        ir_emit_special_calls,
        ir_emit_long,
        ir_emit_virtual_calls,
        ir_emit_fp,
        cp_invokedynamic_descriptor_resolver,
        // No VM in scope: the CP-only indy resolver answers every site.
        cp_invokedynamic_site_resolver: None,
        // No VM in scope: no linked call site to compile as a plain call.
        cp_indy_direct_resolver: None,
        // No VM in scope: no mutable call site to guard (round 20 wave 11).
        cp_indy_mutable_guard_resolver: None,
        // No VM in scope: no record fold (round 21 wave 17, lane records).
        cp_indy_record_fold_resolver: None,
        // No VM in scope: no indy plan to ask in a class context (round 21,
        // lane indylate2).
        indy_context_callee_compiler: None,
        indy_context_ir_inline_resolver: None,
        // No VM in scope: no fused StringBuilder chain (round 21 wave 20,
        // lane sbfuse).
        sb_concat_site_resolver: None,
        // No VM in scope: no published lambda singleton (round 20 wave 11).
        indy_constant_word_resolver: None,
        // No VM in scope: no linked capturing proxy (round 20 wave 12).
        indy_capture_alloc_resolver: None,
        // No VM in scope: no late lambda site either (t1 wave 11, lane JLAM).
        indy_constant_lambda_site_resolver: None,
        // No VM in scope: no lambda impl either (round 21, lane lambda2).
        indy_lambda_impl_resolver: None,
        // No VM in scope: no constant handle either (round 16 wave 17).
        cp_mh_constant_resolver: None,
        cp_mh_constant_bound_resolver: None,
        cp_mh_constant_filter_resolver: None,
        cp_mh_constant_convert_resolver: None,
        class_id_name_resolver,
        receiver_inline_resolver,
        // Class-hierarchy analysis is a per-VM query, so this positional
        // shim — which has no VM in scope — has nothing to pass. A caller
        // that wants CHA builds a `CompileRequest` directly.
        unique_concrete_resolver: None,
        ir_inline_resolver,
        // No VM in scope: no exact-receiver resolution.
        ir_exact_receiver_resolver: None,
        // No VM in scope: no profile-selected resolution.
        ir_receiver_inline_resolver: None,
        jdk_only,
        direct_bind_admission,
        despec,
        // No VM in scope in this positional shim, so no runtime
        // de-speculation registry to consult. Fail-open: see
        // `CompileRequest::runtime_despec`.
        runtime_despec: None,
        cp_invoke_declaring_class_resolver,
        // No VM in scope: a declaring class matches the cycle sets by names.
        cp_invoke_declaring_class_id_resolver: None,
        self_call_identity_stable: false,
        direct_helpers: &DirectHelperTable::EMPTY,
        // No VM in scope: no direct-bind gates to ask, so no cycle-edge cell.
        cycle_edge_admission: None,
        // No VM in scope: no class to ask about finality.
        class_is_final_resolver: None,
        class_is_cha_leaf_resolver: None,
        // No VM in scope: no filed static final array lengths (JIT r20 wave
        // 11, lane pIR).
        static_final_array_length_resolver: None,
        // No VM in scope: no filed static final classes either.
        static_final_ref_class_resolver: None,
        frozen_facts: None,
        frozen_state: None,
        frozen_receivers: None,
        // No VM in scope: no JVMTI flags to test.
        exception_event_flag_addrs: None,
        // No VM in scope: no hierarchy to ask (JIT r20 wave 11, lane pIR).
        class_is_subclass_resolver: None,
        // No VM in scope: no published synchronized body to bind.
        sync_direct_lookup: None,
        // No VM in scope: the process-wide deferred-`new` memo and verdict
        // registry, named here, the one place they are the right answer
        // (`CompileRealm::process`).
        deferred_new_retries: CompileRealm::process().deferred_new_retries,
        verdicts: CompileRealm::process().verdicts,
        jdk_only_sinks: CompileRealm::process().jdk_only_sinks,
        // A positional compile is never the optimizing OSR door's.
        osr_mode_exit_polls: false,
        osr_optimizing_route: false,
        osr_requested_bci: None,
        // No VM in scope: no debugger to read its locals.
        debugger_observes_locals: false,
        // Nor a JVMTI agent (flip review item 2, lane flipprep).
        jvmti_locals_accessible: false,
        // Nor a monitor observer (t1 wave 2, lane M), nor any object
        // observer (wave 3).
        monitor_observer_at_compile: false,
        scalar_observer_at_compile: false,
        // No VM in scope: no JVMTI exception listener either.
        exception_events_armed_at_compile: false,
        exception_listener_armed_at_compile: false,
        // No VM in scope: no class-mirror slot, so a `static synchronized`
        // method keeps the wrapped entry.
        self_lock_mirror_slot: 0,
        caller_mirror_slot: 0,
        caller_security_walk_ignored: false,
        const_folds: None,
        const_folds_refresher: None,
        object_class_id: None,
    })
}

/// RBC.6 local-handler-safety fix. Conservative, sound check answering: for
/// this method's exception table, could ANY handler observe a local
/// variable that `route_jit_exception_through_method`'s params-only
/// handler-frame reconstruction cannot recover — i.e. anything other than
/// `this` / a declared parameter?
///
/// Delegates to `regalloc::handler_has_unsafe_local_read`, a real CFG-based
/// forward "definitely assigned" dataflow (dominance-respecting, not just
/// raw-pc-order) — see that function's own doc comment for the full
/// algorithm and why an earlier, simpler raw-pc-order approximation here
/// was both over-conservative (continued scanning past a handler's own
/// `athrow`/`return` into unrelated later code — confirmed, via the real
/// `org.apache.catalina.connector.Response.toAbsolute()` bytecode on the
/// Tomcat suite fixture, to be the actual remaining blocker for the doc's
/// own motivating case even after the RBC.6 gate itself was relaxed) and
/// had a latent, never-triggered soundness gap (path-insensitive: a store
/// on one branch and a read on a different, non-overlapping branch could
/// be wrongly accepted merely because the store's bytecode pc was lower).
///
/// **Confirmed necessary in the first place** via
/// `AthrowCountBisect.twoThrowsSequential`
/// (`vm/tests/jit_local_exception_handler_tests.rs`): two sequential,
/// non-nested try/catch blocks in one method, where the second handler's
/// own code reads a local (`a`) last assigned by the FIRST try's successful
/// (non-exceptional) path. Without SOME such check, that method compiled
/// and silently produced a wrong checksum.
///
/// (Everything above documents [`local_handler_reads_unsafe_local`], the
/// private compile-time gate at the bottom of this block — not the public
/// wrapper immediately below, which inherited the paragraphs when it was
/// added.)

/// VM-side view of [`local_handler_reads_unsafe_local`]: does resuming one of
/// this method's handlers require the locals a PRECISE exceptional frame
/// carries, rather than the `this`-plus-declared-parameters reconstruction the
/// interpreter can rebuild on its own?
///
/// The interpreter has two sinks that resume a compiled body at its own handler
/// (`route_jit_signal_exception` and `run_jit_callee_handler`). Only the first
/// consumes the reason-9 frame; the second rebuilds the frame from the incoming
/// arguments alone. That is sound ONLY for a method this predicate answers
/// `false` for — for a `true` one, every local beyond the parameters resumes as
/// 0/null, which is a silent wrong answer, not a crash. Exported so the sinks
/// can tell the two populations apart instead of assuming the compile gate kept
/// the `true` ones out (it no longer does: `precise_handler_frames_enabled`
/// admits them on the promise of a precise frame).
pub fn handler_resume_requires_precise_locals(
    code: &[u8],
    code_len: usize,
    exception_table: &[cratonvm_reader::attribute::ExceptionTableEntry],
    method_descriptor: &str,
    is_static: bool,
) -> bool {
    local_handler_reads_unsafe_local(
        code,
        code_len,
        exception_table,
        method_descriptor,
        is_static,
    )
}

/// The RBC.6 refusal every single-pass door asks for a method whose handler
/// needs the precise locals ([`handler_resume_requires_precise_locals`]): the
/// first `(pc, opcode)` in a protected range that cannot publish a precise
/// exceptional frame ([`first_unsupported_precise_frame_site`]), `(0, 0)`
/// when the relaxation is switched off wholesale
/// (`CRATONVM_NO_JIT_PRECISE_HANDLER_FRAMES`) or on a target whose backend
/// publishes no such frame; `None` when the door may compile with
/// `BackendRequest::precise_exception_frames` set.
///
/// Shared by the method-entry door ([`try_compile_inner`]) and the eager
/// first-call door (`vm/src/runtime/interpreter.rs` `execute`), which asked
/// nothing until interpreter round i1 wave 28, lane L2
/// (`docs/feature-designs/jit-ir-optimizations.md`).
/// Defined for every target, so a door type-checked everywhere can call it.
pub fn precise_handler_frame_blocking_site(
    code: &[u8],
    code_len: usize,
    exception_table: &[cratonvm_reader::attribute::ExceptionTableEntry],
) -> Option<(usize, u8)> {
    #[cfg(target_arch = "x86_64")]
    {
        if precise_handler_frames_enabled() {
            first_unsupported_precise_frame_site(code, code_len, exception_table)
        } else {
            // The relaxation is switched off wholesale, so no single
            // bytecode is at fault; `(0, 0)` renders as a bare site name,
            // which is the honest report.
            Some((0, 0))
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    {
        let _ = (code, code_len, exception_table);
        Some((0, 0))
    }
}

pub(crate) fn local_handler_reads_unsafe_local(
    code: &[u8],
    code_len: usize,
    exception_table: &[cratonvm_reader::attribute::ExceptionTableEntry],
    method_descriptor: &str,
    is_static: bool,
) -> bool {
    // Widening: `this` (slot 0 for instance methods) + declared param slots.
    let param_slot_count =
        count_param_slots_jvm_spec(method_descriptor) as u16 + if is_static { 0 } else { 1 };
    // Slots >= 64 can't be represented in the u64 bitmask the dataflow uses;
    // `regalloc::handler_has_unsafe_local_read` conservatively treats any
    // load of such a slot as unsafe regardless of this mask, so capping the
    // shift here (rather than overflowing) just keeps this bit of arithmetic
    // well-defined — it does not change which methods are accepted.
    let initial_safe_slots: u64 = if param_slot_count >= 64 {
        u64::MAX
    } else {
        (1u64 << param_slot_count) - 1
    };
    let dbg = false;
    for entry in exception_table {
        let handler_pc = entry.handler_pc as usize;
        let unsafe_found =
            regalloc::handler_has_unsafe_local_read(code, code_len, handler_pc, initial_safe_slots);
        if unsafe_found {
            if dbg {
                eprintln!(
                    "[rbc6-dbg] local_handler_reads_unsafe_local UNSAFE (CFG dataflow) handler_pc={} param_slot_count={}",
                    handler_pc, param_slot_count
                );
            }
            return true;
        }
    }
    false
}

/// Whether the precise-handler-frame relaxation of the RBC.6 gate is enabled.
///
/// The relaxation compiles a method whose exception handler (or code reachable
/// from it) reads a local beyond the incoming parameters, on the promise that
/// every throwing site in its protected ranges publishes a precise exceptional
/// frame instead of the params-only reconstruction
/// `route_jit_exception_through_method` can do on its own. Without it, ordinary
/// `try`/`catch` methods stay interpreted forever — `JSONValue.toJSONString`,
/// `org.apache.tomcat.util.buf.CharsetCache.getCharset` (~15us interpreted vs
/// ~2us compiled), `StringCache.toString`.
///
/// **Default OFF between 2026-07-27 and 2026-07-28**, because a json-smart
/// round-trip probe (`docs/known-issues/repros/jsonsmart/`, `-Xmx64m`) crashed
/// or corrupted within ~20,000 iterations whenever it was open. Four defects
/// were behind that, all fixed; keep them in mind before changing anything
/// here:
///
/// 1. `remap_active_jit_frames` (vm) walked the JIT rbp chain and passed the
///    *unvalidated* parent link to a helper that dereferences
///    `[parent_rbp - sp_id_slot_off]` and rewrites every oop-map slot. A zero
///    link faulted; a garbage one rewrote arbitrary stack. This was the actual
///    json-smart failure and is not specific to this gate — the gate only made
///    the chain deep enough to walk. 3 SIGSEGVs in 4 probe runs before, 0 in 10
///    after.
/// 2. The exceptional frame was keyed on the throwing invoke's SUCCESSOR bci,
///    but its consumer uses that bci as the THROW pc for the handler's
///    `[start_pc, end_pc)` test — and javac routinely ends a protected range
///    exactly at that successor, so the exception escaped its own catch block.
/// 3. The frame was stashed in `LAST_DEOPT`, where every other consumer treats
///    a stash as "resume this method at `bci`" — which for an exceptional frame
///    executes past a call that never returned. It has its own stash now
///    (`deopt::take_exceptional_frame`) and its own `DeoptReason`.
/// 4. Register allocation and `regalloc::plan_safepoint_publication` both built
///    their liveness from a CFG with no exception edges, which is only sound
///    while this gate refuses the population that reads handler-only locals.
///    Both now model the handler ranges for this population.
///
/// Regression fixture: `vm/tests/resources/cratonvm/JitPreciseHandlerFrame.java`
/// (three shapes, each returning a mismatch count that must be 0).
/// `CRATONVM_NO_JIT_PRECISE_HANDLER_FRAMES` restores the params-only refusal.
/// Whether the single-pass backend may scalar-replace in a method compiled with
/// precise exception frames — **default ON** since 2026-09-22;
/// `CRATONVM_JIT_SCALAR_UNDER_PRECISE_FRAMES=0` is the kill switch.
///
/// The gate it opens is the one that made the backend's monitor-relock feature
/// unreachable: `local_handler_reads_unsafe_local` is true for EVERY javac
/// `synchronized` block (the synthesised monitor temporary is never a parameter
/// slot), so `precise_exception_frames` is true for every one of them, and
/// `x64/driver.rs` answered that by switching scalar replacement off wholesale.
/// The same gate also cost every method with an ordinary `try`/`catch` over a
/// non-parameter local its scalar replacement, which is a much larger
/// population than the monitors this was filed about.
///
/// What had to exist first, and now does:
///
///   * the reason-9 exceptional-frame sinks rebuild a scalar-replaced object
///     and re-acquire an elided lock before resuming the handler
///     (`exception_dispatch::materialize_and_relock_precise_frame`) — without
///     that, an exception inside the protected range of such a method
///     PROPAGATED past a handler that should have caught it;
///   * the OSR entry gate refuses an artifact whose rethrow points name a
///     virtual object (`osr_entry::osr_exit_policy`), because the in-place
///     OSR-exception transfer has no materialization path and its refusal
///     propagates rather than falling back;
///   * compiled local handler blocks are disarmed for a method that
///     scalar-replaces (`x64/driver.rs`), because such a block would run in the
///     compiled frame and read the dummy-zero the elided `new` left behind.
///
/// Kept as a switch because this widens what the compiler emits into the most
/// miscompile-prone area this tree has a comment history about, and a binary
/// that can answer "is it this?" in one run is worth one `OnceLock`.
pub(crate) fn scalar_under_precise_frames_enabled() -> bool {
    true
}

pub(crate) fn precise_handler_frames_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| {
        !false
    })
}

/// `CRATONVM_JIT_IR_PRECISE_HANDLER_FRAMES=1` — let the OPTIMIZING tier take a
/// method that needs precise exceptional frames (RBC.6: a handler reads a
/// non-parameter local, which is every javac `synchronized` block). **Default
/// ON** since round 11 wave 13, after its soak (the R11 + legacy battery with
/// the switch on in the default, `CRATONVM_C2_ACCEPT=always`,
/// `CRATONVM_JIT_OSR=0`, `--compatible` and `-XX:+UseG1GC` arms, IR census
/// 0/264, zero post-lowering discards on CratonBench, and a flat CratonBench
/// A/B); `CRATONVM_JIT_IR_PRECISE_HANDLER_FRAMES=0` turns it off (declared by
/// `r11w10-irexc-types-ir-precise-handler-frames-flag-patch-FIXED-20260924.md`).
///
/// On, `ir_lower` puts a reason-9 rethrow pad on every call-exception exit
/// whose throw bci lies in a protected range (`Lowerer::push_rethrow_pad`):
/// the pad publishes the snapshot at that bci as a `PendingException` frame —
/// exactly the frame the single-pass backend's reason-9 stub publishes — and
/// the VM's drains route the exception through the method's own table with
/// those locals. The lowerer REFUSES the compile when such a frame would name
/// anything the drains cannot hand a handler as-is (an undescribable slot, a
/// scalar-replaced object, a lock it would have to re-acquire), so the
/// admission cannot widen into a frame the consumer propagates past its
/// handler.
///
/// Two populations stay on the single-pass backend with the switch on:
/// `ACC_SYNCHRONIZED` methods (the reason-9 drain re-acquires the method
/// monitor from the frame's slot 0, which an IR snapshot may describe as dead)
/// and methods [`ir_precise_frames_trap_post_check`] rejects after lowering.
///
/// Read without a `static` (`process_global_statics_ratchet`); asked at most
/// once per RBC.6 compile.
pub(crate) fn ir_precise_handler_frames_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_PRECISE_FRAMES_SYNCHRONIZED=1` (round 14 wave 1, lane
/// sync; backlog #9, irexc proposal W11-2): let the optimizing tier take an
/// `ACC_SYNCHRONIZED` RBC.6 method too (a `synchronized` method whose own
/// `synchronized` block or `catch` reads a non-parameter local). **Default
/// OFF** until the probes named on
/// `r12w8-orch-synchronized-instance-calls-and-bigdecimal-are-slow-CLOSED-20261006.md`
/// ("Round 14 wave 1") pass with it on.
///
/// Why it is sound by reading: the IR body of a synchronized method is a
/// WRAPPED body (the door or caller holds the method monitor; no frame state
/// names it), so its reason-9 pads name only the builder's `monitorenter`
/// locks, which `rethrow_frame_refusal` already restricts to taken,
/// describable ones, and refuses any scalar-replaced local. The one IR-only
/// hazard the exclusion was written for -- a snapshot describing a dead slot
/// 0 as `Undefined`, while the handler sink re-acquires the method monitor
/// from slot 0 -- is repaired in both handler sinks: `restore_dead_receiver`
/// in `route_jit_signal_exception` and the unconditional receiver restore in
/// `precise_handler_frame_for` (`vm/src/runtime/interpreter/exception_dispatch.rs`).
/// Guard exits are checked after lowering exactly as for any synchronized
/// IR body (`first_point_needing_unsound_replay_of_method` with
/// `SinkMethodMonitor`). Read without a `static`
/// (`process_global_statics_ratchet`); asked at most once per RBC.6 compile
/// of a synchronized method.
pub(crate) fn ir_precise_frames_synchronized_enabled() -> bool {
    false
}



/// `CRATONVM_JIT_OSR_OPAQUE_HANDLER_HEADERS` (round 13 wave 8, lane irexc2;
/// `r11w11-irexc-osr-entry-as-a-real-predecessor`): with
/// `CRATONVM_JIT_OSR_OPAQUE_ENTRY` off, the optimizing OSR door's compile of a
/// precise-frame method still builds opaque OSR arms at the loop headers one
/// of its exception handlers reaches, so those headers (a retry loop, a
/// parse-and-skip loop: a `catch` that continues the loop) get an optimizing
/// OSR entry instead of none. **Default ON**; `0` restores the withheld
/// entries. Read without a `static` (`process_global_statics_ratchet`); asked
/// at most once per OSR-door compile of a method with an exception table.
///
/// JIT round 18 wave 11 (lane osrfollow): since commit 44df838b5f every
/// OSR-door build is opaque at EVERY header (`osr_opaque_all` in
/// `ir_tier_attempt`), which leaves this switch nothing to narrow: it changes
/// a build again only under [`osr_narrow_opaque_door_enabled`]
/// (`r18-osr-opaque-handler-headers-switch-inert-under-the-always-opaque-door-CLOSED-20261006.md`).
pub(crate) fn osr_opaque_handler_headers_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_OSR_NARROW_OPAQUE_DOOR` (JIT round 18 wave 11, lane
/// osrfollow, OSRPOLY18-2; **default OFF**, `=1` opts in; a perf switch). The
/// optimizing OSR door's compile first builds WITHOUT the door's own opaque
/// merges (`ir_tier_attempt`'s `osr_opaque_all` then holds only for the
/// process switch and unloaded static splices; the round-13 handler-header
/// arms, [`osr_opaque_handler_headers_enabled`], come back), and rebuilds with
/// an opaque merge at every header only when that body's guard exits need one
/// (`CompiledMethod::ir_osr_guard_fact_needs_opaque_entries`; `ir_tier`).
///
/// ## Why this keeps 44df838b5f's soundness argument
///
/// 44df838b5f: "An OSR-only artifact may resume after any speculative guard,
/// including atomic class/layout guards. Its headers must merge arbitrary live
/// interpreter locals and memory, rather than retain facts from pc zero." The
/// hazard is a SPECULATIVE guard exit: the interpreter continues where the
/// graph pruned (a never-taken branch, a class/layout guess), and the door may
/// re-enter the same body at a header whose facts assumed the speculation.
/// The door enters a body that has guard exits only under a published guard
/// fact (`CompiledMethod::ir_osr_guard_exit_bcis`), and the lowerer publishes
/// one in exactly two cases:
///
/// * the STRICT fact (`osr_exit::guard_exit_fact`): every guard is EXACT (its
///   failure makes the re-executed bytecode throw) or VALUE-EQUIVALENT (the
///   interpreter computes the value the body would have). No continuation of
///   such an exit leaves the graph's own paths, so no header fact can be
///   falsified, whatever kind of entry the header has. This was the door's
///   rule before 44df838b5f (round 11 wave 12, tier proposal 27 phase 3);
/// * the RELAXED fact (`osr_exit::guard_exit_fact_opaque`), which skips the
///   classification: published only when every emitted entry is an opaque
///   merge (`ir_lower::lower_inner`, `non_opaque_entries == 0`).
///
/// So a non-opaque narrowed body whose fact is strict is sound for the same
/// reason a pre-44df838b5f door body was, and a narrowed body that needs the
/// relaxed fact is never handed to the door: `ir_tier` drops it and rebuilds
/// exactly the 44df838b5f body (every header opaque). A body with no guard
/// exit at all has no continuation that leaves the graph. Unloaded static
/// splices keep their own every-header arms (b46d4a4d0e), unchanged.
///
/// What it buys: header facts (array lengths, ranges, constants) for every
/// door body whose exits are exact, and the round-13 handler-header narrowing
/// again. What it costs: one extra build for a door body with speculative
/// guards, and a body whose header keeps a memory phi no local names is again
/// refused an entry (as before 44df838b5f) unless
/// `CRATONVM_JIT_IR_OSR_EXCUSE_MEMORY_TOKENS` excuses it. Read once per
/// OSR-door IR compile; no `static`.
pub(crate) fn osr_narrow_opaque_door_enabled() -> bool {
    false
}

/// `CRATONVM_JIT_OSR_NARROW_DOOR_ENTRY_REBUILD=1` (JIT round 19 proposal
/// wave, lane osr19, OSRFOLLOW18-1 of `docs/feature-designs/deopt-osr.md`;
/// **default on**, a perf switch that acts only under
/// [`osr_narrow_opaque_door_enabled`], itself OFF). When the narrowed OSR-door
/// body (built without the door's every-header opaque merges) has no entry at
/// the header the door fired at (`CompileRequest::osr_requested_bci`, the
/// route's `entry_pc`), drop it and build 44df838b5f's every-header-opaque
/// body, which merges whatever the interpreter holds there. Without it, such a
/// loop stays interpreted (or on the single-pass body) under the narrowed door
/// where the always-opaque door entered it, so the narrowed door's flip would
/// need a measurement of that population too.
///
/// Sound: the rebuilt body is exactly the body the door builds with the
/// narrowing off. Costs one extra build for each such loop (the build is
/// never handed out twice: the second attempt is not narrowed). Read once per
/// narrowed door body; no `static`.
pub(crate) fn osr_narrow_door_entry_rebuild_enabled() -> bool {
    true
}

/// Did the narrowed OSR-door body `cm` publish no optimizing entry at the
/// door's own header `requested`? `false` when the compile names no header (a
/// method-entry compile) or the pc does not fit a `u32` (no body can enter
/// it either way, so a rebuild would buy nothing).
pub(crate) fn narrowed_body_lacks_door_entry(cm: &CompiledMethod, requested: Option<usize>) -> bool {
    requested
        .and_then(|pc| u32::try_from(pc).ok())
        .is_some_and(|bci| cm.ir_osr_entry_addr(bci).is_none())
}

/// Keep `covered[pc]` only at the pcs `reach` lists
/// (`deopt::pcs_reachable_from_handlers`, sorted); `None` (unanalysable code)
/// clears every pc. `IrBuilder::build_opaque_osr_merge` builds no arm at an
/// uncovered header, so this narrows the arms to handler-reached headers.
pub(crate) fn mask_osr_arm_coverage_to_handler_reach(covered: &mut [bool], reach: Option<&[u32]>) {
    for (pc, c) in covered.iter_mut().enumerate() {
        let kept = match (reach, u32::try_from(pc)) {
            (Some(pcs), Ok(pc)) => pcs.binary_search(&pc).is_ok(),
            _ => false,
        };
        *c = *c && kept;
    }
}


/// The post-lowering half of the unresumable-trap waiver the precise-frame
/// admission takes (round 11 wave 10, lane irexc): does every GUARD exit of
/// this optimizing artifact resume precisely?
///
/// [`ir_unresumable_protected_trap`] refuses a protected range holding an
/// inline trap plus a side effect because its premise was that such a guard
/// deopt could only be replayed from method entry. For the precise-frame
/// population that premise is checked here instead of assumed: a guard point
/// is resumed precisely when its frame state is resumable AND a sink will try
/// — `can_deopt_resume` (the lowerer vouches for every body whose snapshots
/// hold a monitor) or the default-on sink resume
/// ([`deopt_sink_resume_enabled`]) for a body with no monitor. `false`
/// discards the artifact and the single-pass backend keeps the method, which
/// is what the pre-compile gate did.
///
/// `is_synchronized` (round 14 wave 1, lane sync): the sinks' second arm
/// (`deopt_resume.rs` `sink_precise_resume_allowed`) refuses an
/// `ACC_SYNCHRONIZED` method as it refuses a body that takes a monitor, so
/// such a method's guard exits resume only through the first arm. Reachable
/// with `true` only under [`ir_precise_frames_synchronized_enabled`].
pub(crate) fn ir_precise_frames_trap_post_check(
    cm: &CompiledMethod,
    code: &[u8],
    code_len: usize,
    is_synchronized: bool,
) -> bool {
    // The two arms of the sinks' resume gate
    // (`jit_bridge.rs`: `(deopt_real_enabled() && can_deopt_resume) ||
    // sink_precise_resume_allowed_for(..)`), the second of which refuses any
    // body whose bytecode takes a monitor, and any synchronized method.
    let sink_resumes = if is_synchronized || bytecode_holds_monitor(code, code_len) {
        cm.can_deopt_resume && deopt_real_enabled()
    } else {
        deopt_sink_resume_enabled() || (cm.can_deopt_resume && deopt_real_enabled())
    };
    // A back-edge poll's mode exit (`OsrExit`, interpreter round i1 wave 15,
    // lane L2) is not a guard: only the OSR door's planless transfer takes it,
    // never a method-entry sink, and the lowerer built it resumable.
    cm._deopt_point_boxes
        .iter()
        .filter(|p| !p.semantics.rethrow_exception && p.reason != deopt::DeoptReason::OsrExit)
        .all(|p| sink_resumes && deopt::frame_state_is_resumable(&p.frame_state))
}

/// `CRATONVM_JIT_IR_IN_PLACE_TRAP_FRAME_CHECK` (JIT round 23 wave 8, lane
/// W8-L1, proxy-define page; correctness, default ON, `=0` kills it): the
/// post-lowering half of `CRATONVM_JIT_IR_UNRESOLVED_TYPECHECK_IN_PLACE`. The
/// builder admits an in-place unresolved type-check trap by the frame it SEES
/// (`IrBuilder::frame_resumes_in_place_cleanly`); the frame the lowerer
/// PUBLISHES at that trap can still be one the VM's precise sink only rebuilds
/// with a fallback (an object scalar-replaced across the trap, a slot no one
/// could describe), and the fallback for such a trap is the replay rule, which
/// refuses it with an `InternalError`. Read per compile.
pub(crate) fn ir_in_place_trap_frame_check_enabled() -> bool {
    true
}

/// The pcs at which the builder planted an unresolved type-check trap
/// (`Graph::trap_causes`, cause [`ir::TrapCause::UnresolvedTypeCheck`]).
/// Under the default-off `CRATONVM_JIT_IR_UNRESOLVED_CLASS_TRAP` such a trap
/// may also be a replayed one; checking it as well only makes the check
/// stricter.
///
/// Wave 8: also every [`ir::TrapCause::UnresolvedField`] pc while
/// `CRATONVM_JIT_IR_UNRESOLVED_FIELD_IN_PLACE` is on (the in-place field site
/// trap). A field trap the replay rule admitted is checked too, which can only
/// discard a body, never admit one.
pub(crate) fn ir_unresolved_typecheck_trap_pcs(graph: &ir::Graph) -> Vec<usize> {
    let fields = ir::ir_unresolved_field_in_place_enabled();
    graph
        .trap_causes
        .iter()
        .filter(|&&(_, cause)| match cause {
            ir::TrapCause::UnresolvedTypeCheck => true,
            ir::TrapCause::UnresolvedField => fields,
            _ => false,
        })
        .map(|&(pc, _)| pc)
        .collect()
}

/// Does every own-frame deopt point of `cm` at one of `trap_pcs` carry a frame
/// the VM's precise sink rebuilds directly ([`in_place_trap_frame_is_plain`])?
/// `true` when there is no such pc. A point with a caller chain is a spliced
/// callee's (an in-place trap is planted only with no splice open), so it is
/// not one of these traps whatever its bci; `OsrExit` and rethrow points are
/// not guards (as in [`ir_precise_frames_trap_post_check`]).
pub(crate) fn in_place_typecheck_trap_frames_rebuild(cm: &CompiledMethod, trap_pcs: &[usize]) -> bool {
    first_unrebuildable_in_place_trap(cm, trap_pcs).is_none()
}

/// Wave 8c (diagnostic): the first own-frame point of `cm` at one of
/// `trap_pcs` whose frame [`in_place_trap_frame_refusal`] refuses, as
/// `(bci, rule)`; `None` when every such frame is rebuilt directly.
pub(crate) fn first_unrebuildable_in_place_trap(
    cm: &CompiledMethod,
    trap_pcs: &[usize],
) -> Option<(u32, String)> {
    if trap_pcs.is_empty() {
        return None;
    }
    cm._deopt_point_boxes
        .iter()
        .filter(|p| {
            !p.semantics.rethrow_exception
                && p.reason != deopt::DeoptReason::OsrExit
                && p.frame_state.caller.is_none()
                && usize::try_from(p.bci).is_ok_and(|bci| trap_pcs.contains(&bci))
        })
        .find_map(|p| in_place_trap_frame_refusal(&p.frame_state).map(|why| (p.bci, why)))
}

/// `CRATONVM_JIT_IR_IN_PLACE_TRAP_VIRTUALS` (JIT round 23 wave 8c, lane W8-L1;
/// default ON, `=0` refuses a virtual object again): a virtual (scalar-replaced)
/// object in an in-place trap's frame is admitted. The precise sink
/// materializes it exactly as for every other optimizing-tier guard deopt
/// (`deopt_resume::build_deopt_frame_inner` -> `materialize_virtual_objects`);
/// its refusals before allocation are the unmappable slots
/// [`deopt::frame_state_is_resumable`] already rejects here (it reaches into
/// virtual field graphs for the eliminated marker), the synchronized-receiver
/// shape (an in-place admission excludes synchronized methods), and a heap
/// failure, which throws `OutOfMemoryError` under the default-on
/// `CRATONVM_DEOPT_REMATERIALISE_OOM` (HotSpot's realloc failure) rather than
/// falling back to the replay rule. Read per compile.
pub(crate) fn ir_in_place_trap_virtuals_enabled() -> bool {
    true
}

/// Why the precise sink could NOT rebuild `fs` directly, or `None` when it
/// can: a caller scope, a monitor, an unresumable slot
/// ([`deopt::frame_state_is_resumable`]; the first offending top-level slot is
/// named), or -- with [`ir_in_place_trap_virtuals_enabled`] off -- a virtual
/// object in a local or stack slot.
pub(crate) fn in_place_trap_frame_refusal(fs: &deopt::FrameState) -> Option<String> {
    use deopt::FrameValue;
    if fs.caller.is_some() {
        return Some("caller scope".to_string());
    }
    if !fs.monitors.is_empty() {
        return Some("monitor".to_string());
    }
    let blocks = |v: &FrameValue| {
        matches!(
            v,
            FrameValue::Unsupported | FrameValue::MaterializationRequired(_)
        )
    };
    if !deopt::frame_state_is_resumable(fs) {
        let slot = fs
            .locals
            .iter()
            .position(blocks)
            .map(|i| format!("local {i}"))
            .or_else(|| fs.stack.iter().position(blocks).map(|i| format!("stack {i}")))
            .unwrap_or_else(|| "inside a virtual object".to_string());
        return Some(format!("not resumable ({slot})"));
    }
    if !ir_in_place_trap_virtuals_enabled() {
        let virt = |v: &FrameValue| {
            matches!(v, FrameValue::VirtualObject(_) | FrameValue::VirtualObjectRef(_))
        };
        if let Some(i) = fs.locals.iter().position(virt) {
            return Some(format!("virtual object (local {i})"));
        }
        if let Some(i) = fs.stack.iter().position(virt) {
            return Some(format!("virtual object (stack {i})"));
        }
    }
    None
}

/// A frame the precise sink rebuilds without a fallback
/// ([`in_place_trap_frame_refusal`] has nothing to say).
pub(crate) fn in_place_trap_frame_is_plain(fs: &deopt::FrameState) -> bool {
    in_place_trap_frame_refusal(fs).is_none()
}


/// Opt-out for the STUB-S8 fix: when set, a method declaring an exception table
/// is refused by the optimizing tier exactly as it was before that fix, so the
/// same binary can be measured with and without the change.
pub(crate) fn exc_table_c2_disabled() -> bool {
    false
}

/// Whether every potentially throwing bytecode covered by this method's
/// exception table already exits through an x64 runtime call site that can
/// publish a precise reason-9 exceptional frame.
///
/// This recognises the four invoke opcodes plus the monitor operations, and
/// keeps array, field, allocation, cast, divide, `athrow` and ldc failure paths
/// behind the existing params-only safety gate until each of those lowerings
/// publishes the same snapshot.
///
/// The three non-static invokes (`invokevirtual` / `invokespecial` /
/// `invokeinterface`, 0xb6/0xb7/0xb9) were added 2026-07-27 after auditing
/// every lowering their codegen arm can select. Refusing them left ordinary
/// `try`/`catch` methods permanently interpreted:
/// `org.apache.tomcat.util.buf.CharsetCache.getCharset` (tomcat doc 23) and
/// `org.apache.tomcat.util.buf.StringCache.toString` (tomcat doc 30) each die
/// on a single `invokevirtual` inside their protected range, and an
/// interpreted `getCharset` costs ~15us per call against ~2us compiled.
///
/// The `0xb6 | 0xb7 | 0xb9` arm of `x64::compile` has exactly four exits, and
/// all four are safe:
///
/// * the scalar-replacement `<init>` skip and the `java/lang/Object.<init>()V`
///   elision emit no call at all, so they cannot throw;
/// * the inline path is unreachable here — `try_compile_inner` already runs
///   `inline_sites.clear()` whenever `precise_exception_frames` is set, for
///   exactly this reason (an inlined callee's throwing operations would land in
///   this body without independently snapshotting each one); and
/// * the direct-call path and the MIC / `jit_invoke_dispatch` path both end in
///   `emit_post_invoke_exception_check`, which is what builds and records the
///   reason-9 deopt point.
///
/// The one lowering that could not honour this is the sibling tail-call, which
/// tears the frame down before the callee runs, so a throw escapes past the
/// handler covering that pc. It is now suppressed inside protected ranges
/// outright (`x64::Compiler::pc_is_protected`) — that was a latent hole for
/// `invokestatic`, which this list has always admitted, not something the
/// opcodes added here introduced.
#[cfg(target_arch = "x86_64")]
/// Does `op`, inside a protected range, always exit through a call site that
/// publishes a precise (reason-9) exceptional frame?
///
/// `invokespecial`/`invokestatic` and the monitor ops have always been here.
/// `invokevirtual` (0xb6) and `invokeinterface` (0xb9) are here again as of
/// 2026-07-31, and the reason they can be is that the hole that took them out
/// has since been plugged in the codegen rather than in this list:
/// `a523715a84` added the `protected_precise_handler_call` suppression to
/// `x64.rs`, which forces a protected virtual/interface call onto the dispatch
/// path (ending in `emit_post_invoke_exception_check`) instead of the inline
/// MIC/PIC cascade — and the inline cascade machine-CALLing a raw entry, with
/// nothing recording the caller's locals, is exactly what let Spring's
/// `SimpleApplicationEventMulticaster.invokeListener` read its pre-`try`
/// `errorHandler` local back as null. The narrowing landed in the same commit
/// as the fix and was belt to its braces; `probes/HandlerLocalAcrossProtected
/// InvokeProbe.java` is the acceptance test that the braces hold on their own.
///
/// The remaining lowerings a protected 0xb6/0xb9 can select all publish:
/// the two that emit no call at all cannot throw; the inline-callee path is
/// unreachable because `try_compile_inner` clears `inline_sites` under
/// `precise_exception_frames`; the sibling tail-call is suppressed inside a
/// protected range (`pc_is_protected`, x64.rs); and the direct-call and
/// MIC/`jit_invoke_dispatch` paths both end in
/// `emit_post_invoke_exception_check`.
///
/// `getfield`/`putfield` (0xb4/0xb5) are here as of 2026-08-02, and the
/// condition their old exclusion asked for — "a precise frame at the top-level
/// field arms" — is what now holds. The exclusion was written when the only
/// precise null check (`5bf306bb0`, 2026-07-28) had a single call site, on the
/// INLINED-CALLEE `putfield` path. Both top-level arms grew one afterwards and
/// nobody revisited this list:
///
/// * `putfield` — `cd451facc` ("a compiled putfield on a null receiver must
///   throw NPE") put `emit_precise_null_check_field_store` at the top of the
///   top-level `0xb5` arm, ahead of every inline/compact/helper sub-path, so a
///   null receiver inside a protected range records a reason-10 frame at the
///   trapping bci. The one arm it deliberately skips is the scalar-replaced
///   store, whose "objectref" is a dummy with no receiver behind it and
///   therefore cannot NPE.
/// * `getfield` — a null receiver reaches `helpers.getfield` on EVERY
///   sub-path (compact-inline, uniform-inline, resolved-helper and
///   unresolved-helper), because both receiver checks that guard the inline
///   loads — `emit_trusted_oop_receiver_check` and
///   `emit_guarded_getfield_receiver_check` — begin with a null test that
///   branches to the helper. Each of those calls is followed by
///   `emit_post_invoke_exception_check`, which is precisely where the reason-9
///   frame is built.
///
/// The single exception is the opt-in RAW inline `getfield`
/// (`CRATONVM_JIT_INLINE_GETFIELD`), which keeps historical null-reads-as-0
/// semantics: it neither throws nor publishes. `precise_field_ops_enabled`
/// therefore withdraws this admission whenever that flag is on, so the two can
/// never be combined.
///
/// `probes/Rbc6FieldProbe.java` is the acceptance test the old exclusion cited
/// against these opcodes — five methods whose handler reads a non-parameter
/// local written inside the `try`, differentially checked against HotSpot and
/// `--nojit`.
///
/// `invokedynamic` (0xba) is deliberately absent too: it lowers to an
/// unconditional deopt trap, not to a call site that publishes a frame.
pub(crate) fn precise_frame_publishing_opcode(op: u8) -> bool {
    if matches!(op, 0xb6 | 0xb9) {
        return precise_virtual_invokes_enabled();
    }
    if matches!(op, 0xb4 | 0xb5) {
        return precise_field_ops_enabled();
    }
    // `putstatic` (0xb3) rides the `getstatic` switch: one helper arm, the same
    // `<clinit>`-failure sentinel, the same publishing exit. See
    // [`precise_getstatic_checkcast_enabled`]. `instanceof` (0xc1) rides it
    // too since i11-L2: its single-pass arm now ends in the same
    // `emit_post_invoke_exception_check` as `checkcast`'s, for the
    // `IllegalAccessError` / resolution error an unbound site raises.
    if matches!(op, 0xb2 | 0xb3 | 0xc0 | 0xc1) {
        return precise_getstatic_checkcast_enabled();
    }
    if matches!(op, 0xbb | 0xbf) {
        return precise_alloc_athrow_enabled();
    }
    // `ldc` / `ldc_w` (String and Class constants) ride the SAME switch as
    // `new`/`athrow`, because they end in the same publishing exit. See
    // [`precise_alloc_athrow_enabled`].
    if matches!(op, 0x12 | 0x13) {
        return precise_alloc_athrow_enabled();
    }
    // `newarray` (0xbc) / `anewarray` (0xbd) / `multianewarray` (0xc5) ride the
    // SAME switch: every lowering (`op_object.rs`'s three arms, including the
    // CP-deferred `anewarray` arm and the 2-D `multianewarray` helper) ends in
    // `emit_post_alloc_oom_check`, exactly like `new`. See
    // [`precise_alloc_athrow_enabled`].
    if matches!(op, 0xbc | 0xbd | 0xc5) {
        return precise_alloc_athrow_enabled();
    }
    if op == 0xba {
        return precise_indy_enabled();
    }
    // Array LOADS, array stores and `arraylength` (0xbe, round 9 wave 2). The
    // loop-invariant length hoist cannot move a protected `arraylength` out of
    // place: `x64/driver.rs` drops such a hoist under precise frames
    // (`array_len_hoist_site_is_protected`).
    //
    // `aastore` (0x53) and the integer divides (0x6c/0x6d/0x70/0x71) ride the
    // same switch since round 11 wave 8: their lowerings publish on every
    // throwing edge (reasons 9/10/11 for `aastore`, reason 12 for the zero
    // divisor), and the `aastore` ZGC fallback refuses the compile when
    // protected (r11w8-x64-admit-aastore-and-integer-divides-patch).
    if matches!(op, 0x2e..=0x35 | 0x4f..=0x56 | 0xbe | 0x6c | 0x6d | 0x70 | 0x71) {
        return precise_array_access_enabled();
    }
    matches!(op, 0xb7 | 0xb8 | 0xc2 | 0xc3)
}

/// Whether a protected array LOAD (0x2e..=0x35), PRIMITIVE array STORE
/// (0x4f..=0x52, 0x54..=0x56) or `arraylength` (0xbe) may be treated as
/// publishing a precise exceptional frame.
///
/// **This one IS a lowering change**, unlike `precise_indy_enabled`. Both of
/// these opcodes' throwing edges published nothing before:
///
/// * the AIOOBE pad returned the sentinel through the epilogue, so the handler
///   was entered from the INTERPRETER's frame. `emit_bounds_check` now routes a
///   protected site to deopt-stub reason 11, which publishes the exception via
///   `jit_throw_aioobe` and then materialises the frame -- the shape reason 10
///   uses for a locally-detected NPE.
/// * the array null check went to the shared stub. `emit_null_check_array_
///   store_at` now routes a protected site to reason 10, carrying its JEP-358
///   action per bci (`precise_npe_action_by_bci`) so an array NPE inside a try
///   block keeps the message naming which access was null.
///   `emit_null_check_array_load_at` and `emit_null_check_arraylength` do the
///   same through the shared `emit_precise_array_npe_check` as of round 9 (the
///   `ea` lane's fix 12, `x64/arrays.rs`). Until then an array LOAD admitted
///   here still took the shared, frame-less pad on its NPE edge.
///
/// # `arraylength` and the loop-invariant length hoist
///
/// `ArrayLenHoist` can move an `aload A; arraylength` into a loop pre-header
/// whose null check is the frame-less shared stub. Under precise exception
/// frames `x64/driver.rs` drops any such hoist with a site inside a protected
/// range (`array_len_hoist_site_is_protected`), so every protected
/// `arraylength` keeps its publishing in-place check and none is refused
/// here.
///
/// # Why `aastore` (0x53) was excluded until round 11 wave 8
///
/// It has a THIRD edge the others do not: the JVMS covariance check. The inline
/// arm publishes it correctly (`jit_aastore_type_check` +
/// `emit_post_invoke_exception_check`), but the ZGC-barrier fallback arm calls
/// the void `jit_aastore` helper and, in that call site's own words, takes
/// "deliberately NO `emit_post_invoke_exception_check`" -- its exceptions travel
/// the pending-signal channel for the interpreter to drain.
///
/// RBC.6 decides before codegen and cannot know which arm the emitter will pick,
/// so admitting `aastore` would be admitting the unpublished arm too. That the
/// arm is unreachable today (nothing arms the barrier;
/// `AASTORE_ZGC_GATE_FALLBACKS` is expected to be zero for the life of every
/// shipping process) is exactly the kind of "provably unreachable" argument that
/// stops being true quietly. Admitting it needs that arm to publish first.
///
/// **Round 11 wave 8: that condition now holds without the reachability
/// argument.** Two things are true of `op_array.rs`'s `0x53` arm today. The
/// fallback runs AFTER the inline null check, bounds check and covariance check
/// (r9w3 x64obj3), each of which publishes (reasons 10, 11 and 9 when
/// protected), so a store the helper would refuse has already left through a
/// published exit; and under `precise_exception_frames` a PROTECTED site that
/// would take the fallback refuses the whole compile
/// (`aastore-zgc-fallback-in-protected-range`) instead of emitting the void
/// call. RBC.6 therefore no longer has to know which arm is picked, and `0x53`
/// is admitted under this switch (with the integer divides, whose zero guard
/// publishes through deopt-stub reason 12) since the same wave
/// (r11w8-x64-admit-aastore-and-integer-divides-patch). That also frees
/// `MVMap.flushAppendBuffer`, which was refused at `0x53`.
pub(crate) fn precise_array_access_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| {
        !false
    })
}

/// Whether a protected `invokedynamic` (0xba) may be treated as publishing a
/// precise exceptional frame.
///
/// **This admission is BOOKKEEPING, and the distinction matters** -- the `new`
/// admission below is emphatic that admitting an opcode whose lowering does not
/// publish is a miscompile, not a speedup. Both of this opcode's arms already
/// publish, and neither needed a new exit:
///
/// * A **bridged** site (`StringConcatFactory`, `LambdaMetafactory`,
///   `SwitchBootstraps`, `ObjectMethods`) calls its bridge and then runs
///   `emit_post_invoke_exception_check`, which is the SAME publishing exit
///   `invokestatic`/`invokespecial` use -- and those are admitted
///   unconditionally on the line above.
/// * Every other site takes an UNCONDITIONAL deopt at the bci before the call
///   is made. Nothing executes in compiled code there, so no compiled frame can
///   reach a handler unpublished; the interpreter runs the bootstrap and owns
///   the exception with its own precise frame.
///
/// # What it was costing
///
/// `MVMap.flushAppendBuffer` -- **15.2% of CPU** on
/// `org.h2.test.jdbc.TestConcurrentConnectionUsage`, measured with
/// `CRATONVM_PROFILE_SAMPLE_MS=10` -- was refused for an `invokedynamic` at
/// pc=195 inside a protected range, `tier_fail_count=3 queued=false`, i.e.
/// permanently interpreted. It is H2's MVStore append path, and the workload
/// that found it is 12-21x slower than HotSpot with the gap scaling in
/// contention.
///
/// This is the case `first_unsupported_precise_frame_site`'s doc anticipated
/// when it said the pc/opcode is what makes a refusal actionable: the reason
/// alone named a policy, and the opcode named the lowering.
pub(crate) fn precise_indy_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| !false)
}

/// Whether a protected `new` (0xbb) / `athrow` (0xbf) may be treated as
/// publishing a precise exceptional frame.
///
/// **Unlike `getfield`/`getstatic`, this admission is NOT bookkeeping.** Both
/// lowerings grew a publishing exit in the same change that added them here,
/// which is the condition
/// `rbc6-protected-field-ops-FIXED-20260802.md` states and the
/// netty adaptive-allocator page insisted on: admitting `new` without giving
/// its lowering a precise frame is a miscompile, not a speedup.
///
/// `new` (0xbb) — every non-scalar-replaced lowering funnels through
/// `emit_post_alloc_oom_check`: the resolved inline-TLAB arm (whose slow edge
/// falls into `jit_new_object`), the resolved helper arm
/// (`emit_new_object_stub`) and the DEFERRED CP arm
/// (`emit_new_object_cp_stub`, for a class not yet loaded at compile time).
/// That guard now records a reason-9 frame at the allocating bci whenever the
/// pc is protected. All three ways this site can raise — a `<clinit>` failure,
/// a class-resolution failure on the deferred arm, and heap exhaustion — are
/// reported by the same `0`/null sentinel the guard already tested for, so one
/// publishing exit covers the lot.
///
/// The scalar-replaced arm (`self.scalar_replaced`) emits no call at all: it
/// zero-fills frame slots and pushes a dummy. It raises nothing, and a site
/// that raises nothing cannot hand a handler an unpublished frame. (That arm
/// also skips `<clinit>`, which is a pre-existing residual recorded on
/// `jit_new_object`, not something this admission introduces or depends on.)
///
/// `athrow` (0xbf) — its lowering calls `jit_throw_exception`, which stashes
/// the exception together with this athrow's own bci, and then returned the
/// sentinel straight through the epilogue. Inside a protected range it now
/// jumps to the reason-9 stub instead, which spills the trapping registers,
/// materializes the frame and runs the same epilogue. It is admitted alongside
/// `new` rather than after it because `throw new X(...)` compiles to
/// `new`/`dup`/`invokespecial`/`athrow`: netty's
/// `AdaptivePoolingAllocator$Magazine.allocate` carries exactly that sequence
/// at pc 338-345, so admitting `new` on its own would only have moved the bail
/// seven bytes — the trap the page warned about in the same paragraph that
/// named the refusal.
///
/// `ldc` / `ldc_w` (0x12 / 0x13) — admitted under this same switch, and like
/// `getstatic`/`checkcast` this one is BOOKKEEPING: the lowering already
/// published, and the exclusion outlived the change that made it true. Every
/// arm of `op_local_stack.rs`'s `0x12`/`0x13` is one of:
///
/// * `emit_ldc_class` and `emit_ldc_string` — a helper call
///   (`ldc_class_cp` / `ldc_string_cp`) whose `0` return is a published pending
///   exception (`NoClassDefFoundError` and friends; an interning allocation
///   failure), followed by `emit_post_alloc_oom_check`, the very guard `new`
///   relies on. It records a reason-9 frame at the ldc's own bci whenever the pc
///   is protected. Both emitters were already calling it; the comment that said
///   the `0`-return "branches to the SHARED sentinel stub" described the code
///   from before that guard learned to publish.
/// * an int/float constant — `mov`+push. It raises nothing.
/// * a constant this backend cannot lower (`ldc_class_info_idx` /
///   `ldc_string_info_idx` recognised but unemittable, or no entry at all) —
///   `WalkStep::Return(false)`, i.e. the whole compile bails. That leaves the
///   method interpreted; it cannot produce a body holding an unpublished site.
///
/// The IR tier sees such a method only on the precise-frame route
/// (`CRATONVM_JIT_IR_PRECISE_HANDLER_FRAMES=1`, round 11 wave 10), where its
/// `Op::ConstString` / `Op::ConstClass` failure exit is a call-exception exit
/// and so gets a reason-9 rethrow pad inside a protected range
/// (`ir_lower::Lowerer::push_rethrow_pad`); otherwise `precise_exception_frames`
/// routes it to the single-pass backend. `inline_sites` is cleared under the
/// same flag, so no single-pass inlined callee can carry an `ldc` into this
/// body.
///
/// What it was costing: `java.util.concurrent.ConcurrentHashMap.putVal` — the
/// `throw new IllegalStateException("Recursive update")` inside its
/// `synchronized (f)` block is `new; dup; ldc; invokespecial; athrow`, and the
/// `ldc` was the ONE bytecode left after `new`/`athrow` were admitted, so
/// `putVal` stayed interpreted (`compile-bail ...
/// reason=rbc6-handler-reads-unsafe-local(pc=372,op=0x12)`) in every mode.
/// `Rbc6LdcProbe` in `tools/probes` is the differential acceptance test,
/// including the one case that can actually raise here without exhausting the
/// heap: an `ldc` of a class whose file is gone.
///
/// `newarray` (0xbc) / `anewarray` (0xbd) / `multianewarray` (0xc5) — admitted
/// under this same switch 2026-09-21, and like `getstatic`/`checkcast` this one
/// is BOOKKEEPING: all three lowerings were already publishing before this
/// admission, and the exclusion outlived them the same way `ldc`'s did.
///
/// * `newarray` (`op_object.rs`'s `0xbc` arm) — the inline-TLAB fast path
///   cannot throw (it declines and falls through for any shape it cannot
///   serve); the unconditional-fallback arm calls `helpers.newarray`
///   (`jit_newarray`) and both arms merge into the shared
///   `emit_post_alloc_oom_check`. `jit_newarray_body` returns the 0/null
///   sentinel through `JIT_PENDING_EXCEPTION` for a negative length
///   (`jit_negative_array_size`), an overflowing size or a genuine OOM
///   (`jit_newarray_oom`) — the same three-way sentinel `new` relies on.
/// * `anewarray` (`0xbd` arm) — the resolved arm calls `helpers.
///   anewarray_object` (`jit_anewarray_object`), which shares `jit_newarray`'s
///   negative-length and OOM handling; the CP-deferred arm (component class
///   not yet loaded at compile time, `helpers.anewarray_object_cp`) resolves
///   the class first (`jit_resolve_cp_class`, itself sentinel-based — a
///   missing/renamed class stashes `NoClassDefFoundError` and returns the
///   sentinel) and then calls the very same `jit_anewarray_object`. Both arms
///   already call `emit_post_alloc_oom_check` and, on the deferred arm,
///   `emit_post_call_frame_republish` first — resolution can run a user
///   `ClassLoader.loadClass`, i.e. arbitrary Java, exactly as the `new`
///   CP-deferred arm's comment explains.
/// * `multianewarray` (`0xc5` arm, ANY arity up to
///   `MAX_JIT_MULTIANEWARRAY_DIMS` — `walk_object` refuses only `ndims == 0`
///   and anything past that budget, `singlepass-codegen/multianewarray-arity`)
///   calls `helpers.multianewarray_n` (`jit_multianewarray_n`), which checks
///   EVERY dimension for negative before resolving anything, then runs the same
///   `multianewarray_alloc` the interpreter uses. Round 9 wave 6 fixed the
///   one gap here: `multianewarray_alloc`'s `Err` case used to be discarded
///   (`Err(_) => 0`, publishing the sentinel with nothing pending, so a
///   `catch (OutOfMemoryError e)` around `new long[a][b]` never ran); it now
///   stashes the real exception through `jit_cp_alloc_stash_failure`, so the
///   sentinel this admission relies on always has a pending exception behind
///   it.
///
/// `Rbc6ArrayAllocProbe` in `tools/probes` is the differential acceptance
/// test: a negative length/dimension, a genuine allocation, and a missing
/// component class (`Gone[]`, the same after-javac-delete trick `Rbc6LdcProbe`
/// uses) for each of the three opcodes, each inside a handler that reads a
/// local written inside the `try`.
///
/// `CRATONVM_JIT_NO_PRECISE_ALLOC_ATHROW=1` withdraws the admission so one
/// binary can be A/B'd against its own pre-change behaviour; comparing against
/// a separately built branch would confound this with everything else that
/// landed. It withdraws only the ADMISSION — the publishing exits stay emitted,
/// because they are correct for the opcodes already admitted around them.
pub(crate) fn precise_alloc_athrow_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| {
        !false
    })
}

/// Whether a protected `getstatic` / `checkcast` may be treated as publishing a
/// precise exceptional frame.
///
/// Both already did, on every path that can throw — this admission is a
/// bookkeeping correction, not new codegen, exactly as
/// `precise_field_ops_enabled` was for `getfield`/`putfield` (see
/// `rbc6-protected-field-ops-FIXED-20260802`, where the exclusion had outlived
/// its cause by five days).
///
/// `checkcast` (0xc0) has ONE lowering: it always calls `helpers.checkcast` and
/// always follows it with `emit_post_invoke_exception_check(b'L')`, which
/// records the reason-9 frame at the trapping bci whenever the pc is protected.
/// A failed cast returns the `i64::MIN` sentinel through that same guard, so
/// the CCE and the frame are published together.
///
/// `getstatic` (0xb2) has two, and neither leaves a handler without a frame:
///
/// * the helper arm calls `helpers.getstatic` and then
///   `emit_post_invoke_exception_check(type_tag)`. That is the arm that can
///   throw — the helper runs `<clinit>` on first touch and stashes
///   `ExceptionInInitializerError`/`NoClassDefFoundError` behind the sentinel.
/// * the inline arm (`try_emit_inline_getstatic`) emits a baked address plus
///   two loads and no call at all. It is reached only when
///   `DirectHelperTable::resolve_static_base` answers, and the VM side of that resolver
///   (`jit_resolve_static_base`) returns 0 unless
///   `class_init_memo::is_initialized` already holds for the owning class.
///   Initialization is monotonic in the JVM, so a site compiled on that arm can
///   never later run `<clinit>`: it is a plain memory read that cannot throw,
///   and a site that raises no exception cannot hand a handler an unpublished
///   frame.
///
/// `putstatic` (0xb3) was admitted under this same switch in round 11 wave 8,
/// and it is bookkeeping for the same reason. Its top-level lowering
/// (`x64/op_field.rs`, the `0xb3` arm) has ONE arm: it always calls one of the
/// five `helpers.putstatic_*` and always follows it with
/// `emit_post_invoke_exception_check(b'V')`, which records the reason-9 frame at
/// the putstatic's own bci whenever the pc is protected. The helpers
/// (`jit_putstatic_*`, `vm/src/jit/helpers.rs`) return `0` on success and the
/// `i64::MIN` sentinel from every raising path — a failed `<clinit>`
/// (`jit_putstatic_class_init_guard` stashes the Java exception) and a panic
/// (`contain(.., OnPanic::Throw ..)`) — so the check and the exception travel
/// together; `putstatic` returns `void`, so no legitimate result can collide
/// with the sentinel. The field itself is resolved at compile time (an
/// unresolved site refuses the compile, `unresolved-field-site`), so no
/// resolution error can be raised from compiled code. The inlined-callee
/// `0xb3` arm (`x64/inlining.rs`) is unreachable here: `try_compile_inner`
/// clears `inline_sites` under `precise_exception_frames`.
///
/// Its exclusion cost every javac `synchronized (x) { .. }` block that stores
/// a static: the block's catch-any handler reads the monitor local, which is
/// not a parameter, so RBC.6 applies to the whole block and the one `putstatic`
/// in it kept the method interpreted (`rbc6-handler-reads-unsafe-local(pc=20,
/// op=0xb3)` on `R11LockInlineHitRate.plain`; page
/// `r11w7-orch-putstatic-in-a-synchronized-block-keeps-the-method-interpreted`).
///
/// `arraylength` (0xbe), the third opcode the WebSocket-latency investigation
/// turned up, was NOT admitted here when this switch was written — its null
/// check then went to the shared, frame-less stub. Round 9 gave it a precise
/// exit (`emit_precise_array_npe_check`, reason 10, carrying its JEP-358 action
/// per bci) and it is admitted under [`precise_array_access_enabled`] instead.
///
/// `CRATONVM_JIT_NO_PRECISE_GETSTATIC_CHECKCAST=1` withdraws the admission so
/// one binary can be A/B'd against its own pre-change behaviour; comparing
/// against a separately built branch would confound this with everything else
/// that landed.
pub(crate) fn precise_getstatic_checkcast_enabled() -> bool {
    !false
}

/// `CRATONVM_DBG_RBC6_EMIT=1` — trace which exit each protected throwing site
/// is given at EMIT time.
///
/// The `[rbc6-dbg]` family traces the RUNTIME sinks. A frame that never reaches
/// them can be missing for two different reasons and only this says which: the
/// site chose the shared sentinel stub (no frame was ever built), or it chose
/// the reason-9 stub and the frame was lost between the stub and the sink.
pub(crate) fn rbc6_emit_dbg() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| false)
}

/// Whether a protected `getfield`/`putfield` may be treated as publishing a
/// precise exceptional frame — see the list on
/// [`precise_frame_publishing_opcode`] for why it does. (This doc used to sit
/// on `rbc6_emit_dbg` above, spliced into that function's own doc.)
///
/// Two ways to say no:
///
/// * `CRATONVM_JIT_NO_PRECISE_FIELD_OPS=1`, so one binary can be A/B'd against
///   its own pre-change behaviour. Comparing against a separately built branch
///   would confound this with everything else that landed.
/// * `CRATONVM_JIT_INLINE_GETFIELD=1` (the opt-in RAW inline `getfield`), whose
///   null path leaves `RAX = 0` and returns the field as zero rather than
///   throwing. It publishes no frame because it raises no exception at all, so
///   admitting `getfield` while it is on would hand a handler a frame that is
///   never built. Not cached here: `inline_getfield_enabled` does its own
///   `OnceLock`, and this runs per protected opcode at COMPILE time, never on
///   any hot path.
pub(crate) fn precise_field_ops_enabled() -> bool {
    if false {
        return false;
    }
    !x64::inline_getfield_enabled()
}

/// Opt-out for admitting `invokevirtual`/`invokeinterface` above, so one
/// binary can be A/B'd against itself.
pub(crate) fn precise_virtual_invokes_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| {
        !false
    })
}

/// Does this method contain an **inline trapping bytecode inside a protected
/// range** that the optimizing tier lowers to a *deopt it cannot resume*, with
/// a side effect in the replay prefix that whole-method replay would repeat?
///
/// Returns the offending `(pc, opcode)`, or `None` when the optimizing tier may
/// take the method.
///
/// ## What goes wrong without this
///
/// The IR tier lowers an array access, an `arraylength`, a field access and a
/// division to **deopt guards** (`emit_array_null_bounds_guards`,
/// `emit_deopt_if_zero`), on the stated promise that "the interpreter
/// re-executes the opcode and throws the exact NPE / AIOOBE with full
/// semantics (including any in-method handler)". Re-executing needs a precise
/// resume — and `can_deopt_resume` is only ever set on the IR path in the
/// narrow scalar-replacement case (`ir_lower.rs`, guarded by
/// `CRATONVM_SCALAR_DEOPT` + `CRATONVM_DEOPT_REAL`), so on a production
/// artifact it is **false**. The interpreter then has to fall back to replaying
/// the whole method, refuses because that would re-run the side effects already
/// committed, and raises a hard `InternalError`.
///
/// The single-pass backend has no such problem: its bounds check calls
/// `jit_throw_aioobe` and returns the sentinel through the epilogue, and the
/// interpreter routes the exception through the method's own exception table
/// without resuming anything. So declining here is not "stay interpreted" — it
/// is "use the backend that handles this shape", at single-pass code quality.
///
/// ## Why it is this narrow
///
/// `internal/fixed-bugs/unresumable-unconditional-trap-mvmap-FIXED-20260802.md`
/// warns in as many words: *"Do not apply the publish-side rule blind... the
/// naive form would refuse every trap-carrying artifact, including the many
/// whose re-run-from-entry fallback works fine."* Two narrowing terms keep that
/// from happening:
///
/// 1. **only the deopt-guarded opcodes.** Invokes, `new`, `ldc`, `checkcast`
///    and the monitor ops all leave through the `i64::MIN` sentinel and the
///    exception-routing path, which needs no resume. They are not listed.
/// 2. **only when replay can repeat a side effect.** A read-only
///    `try { return a[i]; } catch (...)` replays harmlessly, so the refusal
///    would buy nothing and cost the compile. A straight-line guard before a
///    call is also safe to replay: the call has not run yet. `advance()` — the
///    reported witness — stores two fields before the trapping `baload`, which
///    is exactly what makes its replay observably wrong.
///
/// For each guard, the scan asks whether replaying the loop-closed prefix can
/// repeat a side effect. This includes effects textually after the guard when a
/// backward branch or handler edge lets a later arrival trap; it excludes a
/// straight-line effect after the guard. Pc order alone cannot distinguish
/// those cases.
/// `CRATONVM_JIT_IR_UNRESUMABLE_TRAP_GUARD=0` — **MEASUREMENT ONLY, AND
/// UNSOUND.** Stop declining the optimizing tier to a protected range that
/// carries an unresumable trap, and let it compile the method anyway.
///
/// # Do not ship a workload with this off
///
/// The refusal exists because the IR tier lowers array/field access and
/// division to a **deopt guard**, `can_deopt_resume` is false on a production
/// artifact, and the interpreter's fallback is to replay the method from entry
/// — which it refuses when the range has already committed a store, raising a
/// hard `InternalError`. With this switch off, a method whose trap ACTUALLY
/// FIRES inside such a range gets that `InternalError` instead of the exception
/// the program expected. Nothing about that changes here; the switch does not
/// make the deopt resumable, it only stops the compiler from avoiding it.
///
/// # Why it exists anyway
///
/// The old whole-range rule had a measurable price on class-loader misses:
/// `T1L71JarLoadMicro` put an `aaload` before `ClassLoader.loadClass` in one
/// protected range, and the whole-range scan declined C2 even though replay at
/// the guard could not repeat the later call. The narrowed replay-prefix rule
/// keeps the reported `advance()` refusal while allowing that safe ordering.
///
/// The replay prefix is conservative: it includes all bytecode before the
/// loop-closed extent, even if some instructions are unreachable from the
/// guard. A false positive uses the single-pass backend. The refusal must stay
/// enabled because disabling it can still turn an actually trapped,
/// side-effecting replay into a hard `InternalError`.
pub fn ir_unresumable_trap_guard_enabled() -> bool {
    true
}

/// Methods whose bytecode carries the unresumable-trap SHAPE, and methods the
/// guard actually declined because of it.
///
/// Two counters and not one, because the A/B needs both halves. With the guard
/// off, `shape` keeps counting and `refused` goes to zero — which is what says
/// the OFF arm moved exactly `shape` methods and not some other number. A
/// single counter cannot tell "the shape is rare" from "the switch is not
/// wired".
pub static IR_UNRESUMABLE_TRAP_SHAPE: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub static IR_UNRESUMABLE_TRAP_REFUSED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// `(shape seen, actually declined)` for the unresumable-trap refusal.
pub fn ir_unresumable_trap_counts() -> (u64, u64) {
    (
        IR_UNRESUMABLE_TRAP_SHAPE.load(std::sync::atomic::Ordering::Relaxed),
        IR_UNRESUMABLE_TRAP_REFUSED.load(std::sync::atomic::Ordering::Relaxed),
    )
}

/// The admission GATE for [`ir_unresumable_protected_trap`]: applies the
/// measurement switch and keeps the census.
///
/// Called from the eligibility conjunction ONLY — exactly once per compile
/// attempt. The diagnostic ladder above deliberately calls the raw scan
/// instead: it runs a second time under `CRATONVM_DBG_IR_COMPILES` /
/// `metrics.is_enabled()`, and counting there would double every number the
/// moment somebody turned the diagnostic on.
pub(crate) fn ir_unresumable_trap_declines(
    code: &[u8],
    code_len: usize,
    exception_table: &[cratonvm_reader::attribute::ExceptionTableEntry],
    is_static: bool,
) -> bool {
    if ir_unresumable_protected_trap_in(code, code_len, exception_table, is_static).is_none() {
        return false;
    }
    IR_UNRESUMABLE_TRAP_SHAPE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if !ir_unresumable_trap_guard_enabled() {
        return false;
    }
    IR_UNRESUMABLE_TRAP_REFUSED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    true
}

/// What makes a whole-method replay observably wrong: a store the JVM can see
/// from outside this frame, a call, or a monitor action. A pure computation can
/// be re-run.
///
/// Shared deliberately with the consumer of the refusal this predicate feeds.
/// [`ir_unresumable_protected_trap`] admits the optimizing tier for a protected
/// range whose trap is unresumable ON THE STATED GROUND that "a read-only
/// `try { return a[i]; } catch (...)` replays harmlessly, so the refusal would
/// buy nothing and cost the compile" — and the interpreter's tier-up sink then
/// refused EVERY whole-method replay with a hard `InternalError`, harmless or
/// not. The compiler's narrowing rested on a behaviour the consumer did not
/// have. One predicate, asked at both ends, is what stops that from drifting
/// again; see [`bytecode_commits_side_effect`].
pub fn opcode_commits_side_effect(op: u8) -> bool {
    matches!(
        op,
        0x4f..=0x56 // array stores
            | 0xb3 | 0xb5 // putstatic / putfield
            | 0xb6..=0xba // the invokes
            | 0xc2 | 0xc3 // monitorenter / monitorexit
    )
}

/// Does this method body commit any side effect a re-run from entry would
/// duplicate?
///
/// `false` means a whole-method replay is observably equivalent to the
/// abandoned compiled attempt: the locals are rebuilt from the same arguments,
/// nothing outside the frame was written, and no call was made. That is the
/// exact condition under which the interpreter's deopt sink may replay instead
/// of raising `InternalError: precise deoptimization unavailable`.
///
/// Conservative on anything it cannot read: a walk that loses instruction sync
/// answers `true`, because it has not proved anything about the rest of the
/// method.
pub fn bytecode_commits_side_effect(code: &[u8], code_len: usize) -> bool {
    let mut pc = 0;
    while pc < code_len {
        let op = code[pc];
        if opcode_commits_side_effect(op) {
            return true;
        }
        let len = crate::bytecode_analysis::step(code, pc);
        if len == 0 || pc.saturating_add(len) > code_len {
            return true;
        }
        pc += len;
    }
    false
}

/// Does this method body enter or exit a monitor anywhere?
///
/// Asked by the deopt sinks, not by the compiler. Written when every
/// `FrameState` the optimizing IR lowerer built hard-coded `monitors:
/// Vec::new()`, so a frame reconstructed from a body that had taken a lock
/// described a frame that believed it held none. The optimizing tier now
/// records the builder's monitor stack, with a `relock` marker on elided locks
/// (`ir-frame-states-carry-no-monitor-stack-FIXED-20260912.md`).
///
/// The predicate is still needed by the sinks' additive arm, which cannot tell
/// which backend produced an artifact: the SINGLE-PASS backend describes only
/// the monitors it scalar-replaced, and a non-scalar elision there WOULD leave
/// no trace in the frame. Note the mood — `has_elided_monitor` is assigned
/// `false` once and `true` never, so no such elision happens today and the
/// hazard is prospective rather than live
/// (`docs/feature-designs/jit-ir-optimizations.md`).
/// The predicate is kept precisely because it does not depend on that: it fires
/// on something the sink CAN see for both backends, the callee's bytecode, and
/// so it keeps holding on the day the elision becomes real.
///
/// Whole-body and conservative, for the reason
/// [`ir_unresumable_protected_trap`]'s side-effect scan is: pc order is not
/// execution order, and a lock taken at a lower pc can still be held at the
/// trap. A walk that loses instruction sync answers `true`.
pub fn bytecode_holds_monitor(code: &[u8], code_len: usize) -> bool {
    let mut pc = 0;
    while pc < code_len {
        // monitorenter / monitorexit
        if matches!(code[pc], 0xc2 | 0xc3) {
            return true;
        }
        let len = crate::bytecode_analysis::step(code, pc);
        if len == 0 || pc.saturating_add(len) > code_len {
            return true;
        }
        pc += len;
    }
    false
}

/// May a deopt sink resume a trapped compiled body from its reconstructed
/// frame even when `can_deopt_resume` is false? **Default ON**;
/// `CRATONVM_JIT_DEOPT_SINK_RESUME=0` restores the pre-2026-09-07 behaviour.
///
/// # What the old behaviour was, and why it was a hard abort
///
/// `execute`'s first-call tier-up sink (`vm/src/runtime/interpreter.rs`,
/// `execute-first-call-tierup`) attempted a precise resume only under
/// `compiled.can_deopt_resume`, and raised
/// `InternalError: precise deoptimization unavailable ... refusing
/// side-effecting replay` when it could not. On an optimizing-tier artifact
/// that flag is false by construction — `ir_lower` only ever sets it under
/// `CRATONVM_SCALAR_DEOPT` + `CRATONVM_DEOPT_REAL` — so EVERY trap taken in an
/// IR body reaching that sink, in a body that commits any side effect (any
/// store, any call), was a hard uncatchable abort.
///
/// That is not a theoretical shape. The optimizing tier PLANTS an unconditional
/// uncommon trap at every `invokedynamic` it cannot lower
/// ([`ir::ir_site_trap_enabled`], default ON), so the trap is not a rare
/// mis-speculation — it fires the first time the compiled body reaches that
/// site. Measured population on 2026-09-07: every one of the 8 CRASH classes in
/// the full 3-arm H2 suite run, 7 hibernate-reactive classes across 3 GC arms,
/// and at least one Spring Framework class, all with the identical message.
///
/// # Why resuming is the right answer and not a relaxation
///
/// The frame is already reconstructed and stashed, and the sibling consumer of
/// that same stash — `vm::jit::helpers::try_resume_trapped_callee`, the path a
/// compiled caller's dispatch helper takes — resumes it PRECISELY, in
/// production, with no `can_deopt_resume` anywhere in its conditions. Both build
/// the frame with the same `build_deopt_frame_inner`, which bails to `None` on
/// an inlined chain, an identity mismatch, an out-of-range bci, an unmappable
/// slot and a malformed monitor. `can_deopt_resume` mostly gates a DIFFERENT
/// consumer (`resume_real_ir_deopt`'s scalar-replacement materialisation);
/// asking it here refused a resume almost nothing needed it for. Almost:
/// see the elided-monitor section below for the one thing it did carry, and
/// what carries it now.
///
/// The new arm keeps three refusals the reconstructed frame genuinely cannot
/// answer, and they are the same three the helper makes or the emission side
/// names: an `ACC_SYNCHRONIZED` method, a body that takes a monitor
/// ([`bytecode_holds_monitor`] — the IR frame states record none), and a resume
/// bci past the method's code.
///
/// They hang on the NEW arm and nothing else. The old `can_deopt_resume`
/// condition is left exactly as it was, because a backend that SET that flag
/// has already vouched that no monitor was elided — so applying these guards
/// there too would refuse a single-pass body with an ordinary `synchronized`
/// block that resumes correctly today, turning a working path into the very
/// abort this exists to remove. The change is strictly additive by
/// construction.
///
/// # The one thing `can_deopt_resume` was protecting, and what now protects it
///
/// On the SINGLE-PASS side that flag is `!deopt_points.is_empty() &&
/// !has_elided_monitor` (`x64/driver.rs`). The second conjunct describes a REAL
/// HAZARD but is not, today, a live guard: escape analysis may elide a
/// `monitorenter` over a non-escaping object, and an elided monitor leaves NO
/// trace in the reconstructed frame, so a resumed body would run the matching
/// `monitorexit` against a lock nothing entered — but `has_elided_monitor` is
/// assigned `false` once and `true` never, so the conjunct constant-folds away.
/// It is kept deliberately, as the only statement in the tree that the two
/// resume gates are coupled, and because the failure it would prevent is a
/// silent hang rather than a crash. See
/// `docs/feature-designs/jit-ir-optimizations.md`.
///
/// The monitor refusal above covers exactly that case, and covers it on
/// evidence the sink can actually see. A body whose monitor was elided still
/// CONTAINS the `monitorenter`/`monitorexit` that was elided — eliding is a
/// codegen decision, not a bytecode rewrite — so `bytecode_holds_monitor` is
/// true for it and the resume is refused. `ACC_SYNCHRONIZED` covers the
/// method-level monitor the same way. What is left, `deopt_points.is_empty()`,
/// describes an artifact no trap can arrive at; a frame stashed against one
/// anyway is still identity- and bci-checked by `build_deopt_frame_inner`.
///
/// # The OFF arm
///
/// Off, the sink asks `can_deopt_resume` again and aborts as before. It is a
/// measurement lever for what the resume costs against the abort, not a
/// configuration anyone should ship: on a workload that trips a planted site
/// trap it turns a completed run into a dead process.
pub fn deopt_sink_resume_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        !matches!(
            Err::<String, std::env::VarError>(std::env::VarError::NotPresent).as_deref(),
            Ok("0") | Ok("false") | Ok("off") | Ok("no")
        )
    })
}

pub(crate) fn ir_unresumable_protected_trap(
    code: &[u8],
    code_len: usize,
    exception_table: &[cratonvm_reader::attribute::ExceptionTableEntry],
) -> Option<(usize, u8)> {
    ir_unresumable_protected_trap_in(code, code_len, exception_table, true)
}

/// Does `code` ever store local 0 (`astore_0`, `astore 0`, wide `astore 0`)?
/// Any other opcode byte that merely looks like one inside an operand could
/// only make the answer `true`, which is the conservative direction.
pub(crate) fn code_may_store_local_zero(code: &[u8], code_len: usize) -> bool {
    let mut pc = 0;
    while pc < code_len {
        let op = code[pc];
        match op {
            0x4b => return true,                                 // astore_0
            0x3a if code.get(pc + 1) == Some(&0) => return true, // astore 0
            0xc4 if code.get(pc + 1) == Some(&0x3a)
                && code.get(pc + 2) == Some(&0)
                && code.get(pc + 3) == Some(&0) =>
            {
                return true; // wide astore 0
            }
            _ => {}
        }
        let len = crate::bytecode_analysis::step(code, pc);
        if len == 0 {
            return true;
        }
        pc += len;
    }
    false
}

/// [`ir_unresumable_protected_trap`] that knows whether the method is static.
///
/// In an INSTANCE method local 0 is `this`, which JVMS 2.6.1 makes non-null, so
/// `aload_0; getfield` cannot take the null-check trap the scan is counting.
/// The immediately preceding `aload_0` (a read-only, branch-free producer of
/// the getfield's base) and a method that never stores local 0 are the whole
/// proof; every other `getfield` remains a possible trap.
pub(crate) fn ir_unresumable_protected_trap_in(
    code: &[u8],
    code_len: usize,
    exception_table: &[cratonvm_reader::attribute::ExceptionTableEntry],
    is_static: bool,
) -> Option<(usize, u8)> {
    if exception_table.is_empty() {
        return None;
    }
    let this_is_stable = !is_static && !code_may_store_local_zero(code, code_len);
    let branch_targets = crate::bytecode_analysis::branch_target_map(code, code_len);
    let mut prev_op: u8 = 0xff;
    let covered = |pc: usize| {
        exception_table
            .iter()
            .any(|entry| pc >= entry.start_pc as usize && pc < entry.end_pc as usize)
    };
    // The opcodes whose IR lowering is a deopt guard. Kept in step with
    // `ir_lower.rs`: array element access and `arraylength` go through
    // `emit_array_null_bounds_guards` / `emit_deopt_if_zero`, `getfield` /
    // `putfield` through the inline null check, and the integer divisions
    // through the div-by-zero guard.
    let deopt_guarded = |op: u8| {
        matches!(
            op,
            0x2e..=0x35 // array loads
                | 0x4f..=0x56 // array stores
                | 0x6c | 0x6d | 0x70 | 0x71 // idiv / irem / ldiv / lrem
                | 0xb4 | 0xb5 // getfield / putfield
                | 0xbe // arraylength
        )
    };
    let handler_edges: Vec<(usize, usize, usize)> = exception_table
        .iter()
        .map(|entry| {
            (
                entry.start_pc as usize,
                entry.end_pc as usize,
                entry.handler_pc as usize,
            )
        })
        .collect();
    let mut unsafe_trap: Option<(usize, u8)> = None;
    let mut cached_back_edges: Option<Result<Vec<(usize, usize)>, usize>> = None;
    let mut pc = 0;
    while pc < code_len {
        let op = code[pc];
        if covered(pc) {
            let on_this = op == 0xb4
                && this_is_stable
                && prev_op == 0x2a
                && !branch_targets.get(pc).copied().unwrap_or(true);
            if unsafe_trap.is_none() && deopt_guarded(op) && !on_this {
                // A guard can succeed on an earlier loop iteration and fail
                // on a later one. Bound the replay prefix by every loop and
                // backward handler edge that can reach this guard, then ask
                // whether replaying that prefix could repeat an effect. A
                // straight-line effect after the guard is safe: the trap
                // resumes before it and the abandoned attempt has not run it.
                let len = code_len.min(code.len());
                let back_res = cached_back_edges.get_or_insert_with(|| {
                    crate::ir::collect_replay_backward_edges(code, len, &handler_edges)
                });
                let replay_extent = match back_res {
                    Ok(back) => crate::ir::replay_committed_extent_from_back_edges(len, pc, back),
                    Err(full_len) => *full_len,
                };
                if bytecode_commits_side_effect(code, replay_extent) {
                    unsafe_trap = Some((pc, op));
                }
            }
        }
        // Same walk `first_unsupported_precise_frame_site` uses; a length of 0
        // or one that runs off the end means the scan lost sync, and the
        // conservative answer to "I can no longer read this code" is to
        // decline the tier rather than guess.
        let len = crate::bytecode_analysis::step(code, pc);
        if len == 0 || pc.saturating_add(len) > code_len {
            return unsafe_trap.or(Some((pc, op)));
        }
        prev_op = op;
        pc += len;
    }
    unsafe_trap
}

/// Thin `bool` wrapper over [`first_unsupported_precise_frame_site`], kept for
/// the call sites that only need the verdict.
#[cfg(target_arch = "x86_64")]
pub(crate) fn precise_exception_frame_sites_supported(
    code: &[u8],
    code_len: usize,
    exception_table: &[cratonvm_reader::attribute::ExceptionTableEntry],
) -> bool {
    first_unsupported_precise_frame_site(code, code_len, exception_table).is_none()
}

/// The first `(pc, opcode)` inside a protected range that can throw WITHOUT
/// publishing a precise exceptional frame, or `None` when every one of them
/// can.
///
/// Returning the site rather than a bare `bool` is what makes the refusal
/// actionable. `reason=rbc6-handler-reads-unsafe-local` on its own names a
/// policy, not a cause: the reader cannot tell whether the method was refused
/// for an `ldc`, a `checkcast`, an array load, or an `invokedynamic`, and those
/// need different work to admit. With the pc and opcode attached, the
/// end-of-run stats table says which lowering to teach next — on the Hibernate
/// concurrency workload five of the hottest still-interpreted methods share
/// this refusal, and until now nothing said what they shared.
///
/// A malformed instruction stream also answers `Some` (at the offending pc,
/// with the opcode that could not be measured): a walk that cannot find the
/// next boundary has not proved anything about the rest of the method.
///
/// **Public because the OSR door asks the same question.** RBC.6b refused any
/// OSR compile of a method with a non-empty exception table outright, on the
/// grounds that an OSR artifact carries no handler ranges. Staging the same
/// three requests the method-entry path stages (`set_precise_exception_frame_
/// request` / `BackendRequest::protected_ranges` / `BackendRequest::exception_ranges`)
/// gives the OSR body reason-9 frames at its protected-range invokes — but only
/// where every throwing site in those ranges publishes one. That is exactly
/// this predicate, so `compile_osr_artifact` calls it rather than growing a
/// second, drifting copy of the opcode table. See
/// `osr-refuses-any-method-with-an-exception-table-FIXED-20260817.md`.
#[cfg(target_arch = "x86_64")]
pub fn first_unsupported_precise_frame_site(
    code: &[u8],
    code_len: usize,
    exception_table: &[cratonvm_reader::attribute::ExceptionTableEntry],
) -> Option<(usize, u8)> {
    let covered = |pc: usize| {
        exception_table
            .iter()
            .any(|entry| pc >= entry.start_pc as usize && pc < entry.end_pc as usize)
    };
    // `ldc`/`ldc_w` (0x12/0x13) left this list on 2026-09-19. A String constant
    // allocates through `helpers.ldc_string_cp` and a Class constant resolves
    // through `helpers.ldc_class_cp` (which can run a user
    // `ClassLoader.loadClass` and then throw), and both end in
    // `emit_post_alloc_oom_check`, which publishes a reason-9 frame at the
    // site's own bci when it is protected. The earlier note here said the
    // `0`-return guard "branches to the SHARED sentinel stub"; that was true
    // before the guard learned to publish and was never revisited. The
    // argument is on `precise_alloc_athrow_enabled`, which now governs them.
    //
    // `ldc2_w` (0x14) is NOT in that family, despite having been lumped in with
    // it. It can only push a `long` or a `double`. The one form that could run
    // Java is a `CONSTANT_Dynamic` with a long/double descriptor — and
    // `cp_ldc2w_resolver` accepts only `Long`/`Double` pool entries, answering
    // `None` for everything else, which is a PERMANENT compile bail
    // (`ldc2w-constant-unsupported`). So an `ldc2_w` that survives to codegen is
    // a bare constant push: it cannot throw, allocate or GC, and admitting it
    // here cannot produce a compiled body containing a throwing one, because
    // that compile has already bailed. Keeping it excluded cost every
    // `long`-arithmetic-inside-`try` method its compile — `probes/
    // Rbc6FieldProbe.java`'s `getfieldLongHandlerLocal` is the witness: one
    // `long 1000003L` literal in the protected range was the whole reason it
    // stayed interpreted while its `int` twin compiled.
    // `instanceof` (0xc1) is deliberately NOT in this set, and it is the only
    // member of the old `0xbb..=0xc1` range that left it.
    //
    // Every other opcode here is excluded because it CAN throw and its lowering
    // does not publish a frame. `instanceof` cannot throw at all in this
    // backend: its arm emits a single call to `helpers.instanceof_check`
    // (`vm::jit::helpers::jit_instanceof`), and that helper returns `0` or `1`
    // on every path — a null or implausible receiver, a non-UTF-8 class name, an
    // address the heap does not recognise, and a target class it cannot resolve
    // all fail soft to `0`. It never calls `set_jit_pending_exception`, and the
    // codegen emits no post-call exception check after it because there is
    // nothing to check. A site that raises no exception cannot hand a handler an
    // unpublished frame, so gating on it protected nothing.
    //
    // i11-L2 (2026-09-25): that is no longer so. An unbound site now raises
    // the interpreter's `IllegalAccessError` / resolution error, and the arm
    // ends in `emit_post_invoke_exception_check` like `checkcast`'s — so it
    // PUBLISHES on the one edge that can throw, and it is back in the
    // candidate set below, exempted by `checkcast`'s switch
    // (`precise_frame_publishing_opcode`). Admitted by default, as before.
    //
    // It cost real coverage: `JitPreciseHandlerFrame.loopStep`'s first draft had
    // an `instanceof` in its protected range, which refused the whole method and
    // made that regression test read 0 mismatches in BOTH arms of its own A/B —
    // the fixture says so in its own comment. On the Hibernate concurrency
    // workload it is one of five distinct opcodes blocking five of the hottest
    // still-interpreted methods (`reason=rbc6-handler-reads-unsafe-local`, which
    // now names the pc and opcode).
    //
    // 2026-08-11: `getstatic` (0xb2) has left that list too, along with
    // `checkcast` (0xc0) — both were ALREADY publishing on every path that can
    // throw and were simply never revisited here, the same way `getfield`/
    // `putfield` outlived their exclusion by five days in
    // `rbc6-protected-field-ops-FIXED-20260802`. The argument for each is on
    // `precise_getstatic_checkcast_enabled`. They stay listed in the
    // `may_throw_without_precise_frame` set below — that set is the CANDIDATE
    // set, and `precise_frame_publishing_opcode` is what exempts them.
    //
    // 2026-08-17: `new` (0xbb) and `athrow` (0xbf) have left it as well, and
    // unlike every departure above them this one is NOT bookkeeping — both
    // lowerings grew a publishing exit in the same change. See
    // `precise_alloc_athrow_enabled` for the per-path argument, and note the
    // two had to leave TOGETHER: `throw new X(...)` is one four-bytecode
    // sequence, so admitting either alone only moves the bail to the other.
    //
    // Still genuinely blocking, because their lowerings really do not publish:
    // `arraylength` (0xbe) used to be listed here -- it routed its NPE through
    // the SHARED null-check stub -- and was the one site blocking Tomcat's
    // `IntrospectionUtils.setProperty` in the WebSocket-latency run; round 9 gave
    // it a precise exit and wave 2 admits it; since wave 3 the driver drops an
    // `ArrayLenHoist` any of whose sites is protected, so the loop-hoistable
    // shape is admitted too. `ldc` (0x12) was listed here as well until
    // 2026-09-19; see above. `newarray`/`anewarray`/`multianewarray` (0xbc/
    // 0xbd/0xc5) were listed here until 2026-09-21: all three reach the same
    // publishing `emit_post_alloc_oom_check` that `new` does (audited per-arm,
    // including the NegativeArraySizeException edge — see
    // `precise_alloc_athrow_enabled`), so admitting them is bookkeeping, not a
    // lowering change. `putstatic` (0xb3) was listed until round 11 wave 8:
    // its one arm always ends in `emit_post_invoke_exception_check`, exactly
    // like `getstatic`'s helper arm (bookkeeping; the argument is on
    // `precise_getstatic_checkcast_enabled`). `aastore` and the integer divides
    // left it in the same wave: both publish on every throwing edge (the
    // divide's zero guard takes deopt-stub reason 12 when protected, and
    // `aastore`'s ZGC helper-fallback arm refuses the compile when protected),
    // and are admitted under `precise_array_access_enabled`. Every candidate is
    // now admitted; the scanner refuses only when a kill switch withdraws an
    // admission or the instruction stream is malformed.
    let may_throw_without_precise_frame = |op: u8| {
        matches!(
            op,
            0x12 | 0x13 // ldc / ldc_w — String and Class constants
                | 0x2e..=0x35 // array loads
                | 0x4f..=0x56 // array stores
                | 0x6c | 0x6d | 0x70 | 0x71 // integer divide/remainder
                | 0xb2..=0xba // fields, invokes, and invokedynamic
                | 0xbb..=0xc1 // allocations, arraylength, athrow, checkcast, instanceof
                | 0xc5 // multianewarray
        )
    };

    let mut pc = 0;
    while pc < code_len {
        let op = code[pc];
        // Which opcodes are admitted, and why, is documented on
        // `precise_frame_publishing_opcode`.
        if covered(pc)
            && may_throw_without_precise_frame(op)
            && !precise_frame_publishing_opcode(op)
        {
            return Some((pc, op));
        }
        let len = crate::bytecode_analysis::step(code, pc);
        if len == 0 || pc.saturating_add(len) > code_len {
            return Some((pc, op));
        }
        pc += len;
    }
    None
}

thread_local! {
    /// Did the optimizing pipeline get entered on this thread's current
    /// compile, and then hand the method to the single-pass backend?
    ///
    /// `metrics` already records this as `fell_through_to_single_pass`, but the
    /// metrics ring is behind `CRATONVM_JIT_METRICS` and keeps only a bounded
    /// history, so it can answer questions ABOUT a run and cannot be consulted
    /// DURING one. Anything that changes behaviour on this fact needs a signal
    /// that is on when the metrics are off, or the diagnostic becomes the
    /// thing it is measuring.
    ///
    /// Thread-local because a compile runs start-to-finish on one worker; read
    /// it immediately after the compile call, on the same thread.
    pub(crate) static IR_PIPELINE_ENTERED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    pub(crate) static IR_FELL_THROUGH_TO_SINGLE_PASS: std::cell::Cell<bool> =
        const { std::cell::Cell::new(false) };
    /// Why the optimizing attempt of this thread's current compile produced
    /// no graph, as far as the builder classifies it
    /// ([`last_compile_fall_through_cause`]). Interpreter round i6 wave 115,
    /// lane L2 (`i100-L2-proposal-fall-through-carries-its-bail-site`).
    pub(crate) static IR_FALL_THROUGH_CAUSE: std::cell::Cell<IrFallThroughCause> =
        const { std::cell::Cell::new(IrFallThroughCause::Unclassified) };
}

/// Why an optimizing compile fell through to the single-pass backend
/// ([`last_compile_fall_through_cause`]; interpreter round i6 wave 115, lane
/// L2, the first slice of
/// `i39-L2-proposal-a-compile-request-returns-why-it-produced-no-body`).
///
/// Only the cause a consumer acts on is classified; every other bail (a
/// builder shape, a lowering refusal, the graph cap, a verifier failure) is
/// [`Self::Unclassified`], which is what every fall-through answered before.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrFallThroughCause {
    /// Not classified (see the enum note).
    Unclassified,
    /// The IR builder refused the method at one of its OWN field sites
    /// (`getstatic` / `putstatic` / `getfield` / `putfield`, never a spliced
    /// callee's) because the site had no field row -- its owner was not
    /// loaded when the method compiled, or the row was gated off -- and no
    /// unresolved-field trap was planted there. The single-pass body compiled
    /// in its place defers such a site (`CRATONVM_JIT_DEFERRED_FIELD_SITES`),
    /// and once it links a recompile's field peek finds the row: the case the
    /// deferred-field-site re-tier was built for.
    RowlessFieldSite { pc: u32, opcode: u8 },
    /// The optimizing body LOWERED and the acceptance gate refused it ("no
    /// evidence" / cost), so the single-pass body was compiled in its place
    /// (JIT r22 wave 2, lane inline, INLINE22-1): the shape the acceptance
    /// re-offer (`CRATONVM_JIT_C2_ACCEPTANCE_REOFFER`) arms for.
    AcceptanceRefused,
}

/// What a local's home slot holds at a safepoint of a compile a debugger may
/// read (`CompiledMethod::debugger_home_locals`; interpreter round i6 wave
/// 120, lane L2, `CRATONVM_JIT_DEBUGGER_LOCALS_HOME_SPILL`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DebuggerHomeLocal {
    /// Nothing the backend can vouch for: the frame stays `OPAQUE_FRAME`.
    Unknown,
    /// Never written on any path here (or a cat-2 value's upper slot): reads
    /// as `int` 0, as an interpreter frame's never-written slot does.
    Undefined,
    /// The home slot holds an `int` (low 32 bits).
    Int,
    /// The home slot holds a `long` (64 bits).
    Long,
    /// The home slot holds a `float` (low 32 bits of the stored XMM word).
    Float,
    /// The home slot holds a `double` (64 bits).
    Double,
    /// The home slot holds a reference, named by the safepoint's oop map.
    Ref,
    /// A constant the backend kept out of the frame: kind byte (`I` `J` `F`
    /// `D`) and its bits.
    Constant(u8, u64),
}

