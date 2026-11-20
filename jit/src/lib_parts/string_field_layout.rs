#[allow(unused_imports)]
use super::super::*;

/// Compile-time resolved field layout of `java/lang/String`, for the
/// String call-site intrinsics (`length`/`charAt`/`hashCode`/`isEmpty`/
/// `equals`/`compareTo`/`indexOf`, implemented by a later wave).
///
/// # Why this exists
///
/// The intrinsic matcher ([`try_resolve_intrinsic`]) and the x64 codegen
/// only ever see `(class, name, descriptor)` strings — they have no view
/// of any class's heap layout. A String intrinsic, however, must read the
/// receiver's `value` (`byte[]`/`char[]`), `coder` (`byte`), and `hash`
/// (`int`) instance fields with inline machine code. This struct carries
/// the field positions resolved from the live `java/lang/String` class so
/// the codegen can emit `MOV [receiver + cell_offset + payload]` directly.
///
/// It is produced once per JIT compilation by the
/// `string_layout_resolver` callback passed to [`try_compile`] and threaded
/// unchanged into [`x64::compile`]. When the resolver returns `None` (e.g.
/// `java/lang/String` not yet loaded) the String intrinsics simply bail to
/// normal dispatch — exactly today's behaviour.
///
/// # How an intrinsic obtains a field offset
///
/// Each `*_field_index` is the abstract field slot index. Every field then
/// carries TWO ready-to-use byte offsets from the object base — one per
/// instance layout, each already the exact payload address, so codegen adds
/// nothing to them:
///
/// ```text
/// TEST BYTE [receiver + GC_FLAGS_BYTE_OFFSET], GC_FLAG_COMPACT ; JZ legacy
/// MOV    rax, [receiver + layout.value_compact_offset]  ; compact: bare ptr
/// MOVZX  eax, BYTE [receiver + layout.coder_compact_offset] ; natural width
/// legacy:
/// MOV    rax, [receiver + layout.value_legacy_offset]   ; cell payload
/// MOVSXD rax, [receiver + layout.coder_legacy_offset]
/// ```
///
/// This mirrors the per-object compact/legacy dispatch the inline `getfield`
/// codegen uses; see `x64.rs` opcode `0xb4`.
///
/// # `coder` may be absent
///
/// The legacy synthetic `{value:[C, hash:I}` String layout has no `coder`
/// field (the backing array is `char[]`, always UTF-16). `has_coder` is
/// `false` in that case and `coder_field_index`/`coder_*_offset` are
/// meaningless — an intrinsic that needs `coder` MUST check `has_coder`
/// first and bail to native dispatch when it is `false`. The compact
/// JDK-9+ `{value:[B, coder:B, hash:I, hashIsZero:Z}` layout sets it
/// `true`. The resolver decides which layout applies by inspecting the
/// loaded `java/lang/String` class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StringFieldLayout {
    /// Abstract field slot index of `String.value` (the backing array ref).
    pub value_field_index: usize,
    /// Byte offset of `value`'s bare pointer in a COMPACT instance.
    pub value_compact_offset: i32,
    /// Whether `value`'s COMPACT slot is a 4-byte **narrow** oop rather than a
    /// bare 8-byte pointer. True exactly when compressed oops are on *and* a
    /// `CompactLayout` was actually registered for `string_class_id` — the
    /// no-registered-layout fallback below points `value_compact_offset` at
    /// the LEGACY 8-byte cell payload, which must keep the wide load even
    /// under narrow oops.
    pub value_compact_is_narrow: bool,
    /// Byte offset of `value`'s 8-byte pointer payload in a LEGACY instance
    /// (`HEADER_SIZE + value_field_index * SLOT_SIZE + FIELD_CELL_PAYLOAD64_OFFSET`).
    pub value_legacy_offset: i32,
    /// Abstract field slot index of `String.hash` (the cached `int` hash).
    pub hash_field_index: usize,
    /// Byte offset of `hash`'s 4-byte payload in a COMPACT instance.
    pub hash_compact_offset: i32,
    /// Byte offset of `hash`'s 4-byte payload in a LEGACY instance.
    pub hash_legacy_offset: i32,
    /// Whether this String layout has a `coder` field (`true` for the
    /// compact `byte[]` layout, `false` for the legacy `char[]` layout).
    pub has_coder: bool,
    /// Abstract field slot index of `String.coder`. Meaningful only when
    /// [`has_coder`](Self::has_coder) is `true`.
    pub coder_field_index: usize,
    /// Byte offset of `coder`'s payload in a COMPACT instance. Meaningful
    /// only when [`has_coder`](Self::has_coder) is `true`.
    pub coder_compact_offset: i32,
    /// Byte offset of `coder`'s 4-byte payload in a LEGACY instance.
    /// Meaningful only when [`has_coder`](Self::has_coder) is `true`.
    pub coder_legacy_offset: i32,
    /// Whether `coder`'s COMPACT payload is a single byte. The registered
    /// `CompactLayout` stores every field at its natural Java width, so a
    /// `byte coder` occupies exactly one byte there and must be loaded with
    /// a byte-sized move: the three bytes that follow it belong to the
    /// class's padding, not to `coder`. `false` selects the 4-byte load,
    /// which is what the no-registered-layout fallback below needs.
    pub coder_compact_is_byte: bool,
    /// `ObjectHeader` class id of `java/lang/String` (the value stored at
    /// object offset 0). Used as the receiver class-id guard for the
    /// `java/lang/CharSequence` accessor intrinsics (`charAt`/`length`/
    /// `isEmpty`): a CharSequence call site inlines the String-layout decode
    /// only behind a `[recv+0] == string_class_id` guard, deopting to native
    /// dispatch for any other CharSequence (`StringBuilder`, …). For a
    /// `java/lang/String` site (final, monomorphic) no guard is emitted.
    pub string_class_id: u32,
    /// The `java/lang/StringBuilder` half, when that class is loaded and has a
    /// resolvable `count`/`value`/`coder` shape.
    ///
    /// Carried HERE rather than threaded as a second resolver because this
    /// struct already reaches all three compile doors, and a second parameter
    /// would have to be added at every one of them — the drift `compile_gate`'s
    /// module doc describes, where each door grows its own hand-copied subset
    /// of what the others pass. `None` leaves every StringBuilder site on
    /// ordinary native dispatch, which is where they all were before this.
    pub builder: Option<StringBuilderFieldLayout>,
    /// The `java/util/ArrayList` half (round 9 wave 8, `arraylist8`), for the
    /// ARRAYLIST_ACCESS family: `elementData`/`size` addresses and the exact
    /// receiver guard. Carried here for the reason `builder` is -- this struct
    /// already reaches every compile door. `None` (not loaded, `--real-jdk`,
    /// the kill switch, or any [`ArrayListFieldLayout::new`] refusal) leaves
    /// every `ArrayList.get`/`size` site on its ordinary dispatch.
    pub array_list: Option<ArrayListFieldLayout>,
    /// JIT round 19 proposal wave 6, lane str6 (STRGAP19-6,
    /// `CRATONVM_JIT_STRING_HASH_IS_ZERO`, opt-in): byte offset of
    /// `String.hashIsZero` (a `boolean`) in a COMPACT instance, when the
    /// registered `CompactLayout` stores it as ONE byte. `None` (no such
    /// field, no compact layout, another width) keeps every `hashCode` body's
    /// pre-switch shape: a zero `hash` recomputes (single-pass) or declines
    /// to the call (optimizing tier). Set by [`Self::with_hash_is_zero`].
    pub hash_is_zero_compact_offset: Option<i32>,
    /// JIT round 21, lane strings2 (STRINGS21-1): the `java/util/Locale` ->
    /// `BaseLocale.language` path the inline `StringLatin1.toLowerCase` /
    /// `toUpperCase` prefix reads ([`LocaleLanguageLayout`]). Carried here for
    /// the reason `builder` is. `None` declines every string holding a
    /// locale-sensitive byte to the call. Set by [`Self::with_locale_language`].
    pub locale_language: Option<LocaleLanguageLayout>,
}

/// Where `java.util.concurrent.atomic.AtomicInteger.value` lives, for the
/// ATOMIC_INT intrinsic region.
///
/// Same two-offsets-per-field discipline as [`StringFieldLayout`], and for the
/// same reason: a class with a registered `CompactLayout` may still have
/// LEGACY-laid-out instances, so the codegen dispatches per object on the
/// `GC_FLAG_COMPACT` header bit rather than assuming one layout.
///
/// `value` is an `int`, so both arms address a 4-byte payload — there is no
/// reference/narrow-oop case to get wrong here, which is why this layout is a
/// good deal simpler than the String one.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AtomicIntFieldLayout {
    /// Abstract field slot index of `AtomicInteger.value`.
    pub value_field_index: usize,
    /// Byte offset of `value`'s 4-byte payload in a COMPACT instance.
    pub value_compact_offset: i32,
    /// Byte offset of `value`'s 4-byte payload in a LEGACY instance.
    pub value_legacy_offset: i32,
    /// `ObjectHeader` class id of `java/util/concurrent/atomic/AtomicInteger`,
    /// used as the receiver guard. `AtomicInteger` is not final and its methods
    /// are not final, so a subclass could override them — the guard is what
    /// makes inlining the field access sound, and a mismatch falls back to
    /// ordinary dispatch (which runs the override).
    pub class_id: u32,
}

impl AtomicIntFieldLayout {
    /// Build a layout from the raw field index, precomputing the COMPACT and
    /// LEGACY payload addresses exactly the way [`StringFieldLayout::new`]
    /// does for its `hash` field (the identical shape: a 4-byte `int`).
    ///
    /// Returns `None` when no compact layout is registered AND the legacy
    /// address cannot be formed — the caller then simply does not offer the
    /// intrinsic and the call keeps its native dispatch.
    pub fn new(value_field_index: usize, class_id: u32) -> Option<Self> {
        if class_id == 0 {
            return None;
        }
        // LEGACY: uniform 16-byte `Value` cell, 4-byte int payload inside it.
        let legacy = (cratonvm_types::HEADER_SIZE + value_field_index * cratonvm_types::SLOT_SIZE)
            as i32
            + cratonvm_types::FIELD_CELL_PAYLOAD32_OFFSET as i32;
        // COMPACT: the registered `CompactLayout` body offset IS the payload
        // address. Same fallback rule as `StringFieldLayout::new` — with no
        // registered layout no instance can carry `GC_FLAG_COMPACT`, so
        // pointing the compact arm at the legacy address keeps it harmless
        // rather than wild if that invariant ever slips.
        //
        // REFUSE anything that is not exactly 4 bytes wide: this intrinsic
        // emits a 32-bit `LOCK XADD`, so a narrower or wider storage width
        // would read and write the wrong bytes. Falling back to `None` here
        // keeps the call on native dispatch instead.
        let mut compact = legacy;
        if cratonvm_types::compact_ref_fields_enabled() {
            match cratonvm_types::compact_field_disp_storage(class_id, value_field_index) {
                Some((body_off, storage)) => {
                    if storage.size_runtime() != 4 {
                        return None;
                    }
                    compact = body_off as i32;
                }
                None => {}
            }
        }
        Some(Self {
            value_field_index,
            value_compact_offset: compact,
            value_legacy_offset: legacy,
            class_id,
        })
    }
}

/// `AtomicLong.value`'s addresses — the 64-bit twin of
/// [`AtomicIntFieldLayout`], and different from it in exactly two places: the
/// legacy payload sits at [`cratonvm_types::FIELD_CELL_PAYLOAD64_OFFSET`]
/// rather than the 32-bit one, and a compact storage width that is not exactly
/// **8** bytes is refused.
///
/// Both differences are load-bearing rather than cosmetic. The emitted
/// instruction is a REX.W `LOCK XADD`, so pointing it at the 32-bit payload
/// offset would read four bytes of the cell's TAG along with half the value,
/// and admitting a narrower storage width would have it write past the field.
/// Refusing returns `None` and the call simply keeps its native dispatch.
pub struct AtomicLongFieldLayout {
    /// Abstract field slot index of `AtomicLong.value`.
    pub value_field_index: usize,
    /// Byte offset of `value`'s 8-byte payload in a COMPACT instance.
    pub value_compact_offset: i32,
    /// Byte offset of `value`'s 8-byte payload in a LEGACY instance.
    pub value_legacy_offset: i32,
    /// `ObjectHeader` class id of `java/util/concurrent/atomic/AtomicLong`,
    /// used as the receiver guard. `AtomicLong` is not final and its methods
    /// are not final, so a subclass could override them — the guard is what
    /// makes inlining the field access sound, and a mismatch falls back to
    /// ordinary dispatch, which runs the override.
    pub class_id: u32,
}

impl AtomicLongFieldLayout {
    pub fn new(value_field_index: usize, class_id: u32) -> Option<Self> {
        if class_id == 0 {
            return None;
        }
        // LEGACY: uniform 16-byte `Value` cell, 8-byte long payload inside it.
        let legacy = (cratonvm_types::HEADER_SIZE + value_field_index * cratonvm_types::SLOT_SIZE)
            as i32
            + cratonvm_types::FIELD_CELL_PAYLOAD64_OFFSET as i32;
        let mut compact = legacy;
        if cratonvm_types::compact_ref_fields_enabled() {
            if let Some((body_off, storage)) =
                cratonvm_types::compact_field_disp_storage(class_id, value_field_index)
            {
                if storage.size_runtime() != 8 {
                    return None;
                }
                compact = body_off as i32;
            }
        }
        Some(Self {
            value_field_index,
            value_compact_offset: compact,
            value_legacy_offset: legacy,
            class_id,
        })
    }
}

impl StringFieldLayout {
    /// Build a layout from raw field indices, precomputing, for each of
    /// `value`/`coder`/`hash`, the two byte offsets the codegen needs: the
    /// exact payload address under the COMPACT instance layout and the exact
    /// payload address under the LEGACY one. `coder_field_index` is ignored
    /// (and its offsets zeroed) when `has_coder` is `false`.
    /// `string_class_id` is the `java/lang/String` `ObjectHeader` class id
    /// used as the CharSequence receiver guard.
    ///
    /// # Why two offsets per field
    ///
    /// A class with a registered `CompactLayout` may still have
    /// LEGACY-laid-out instances (an allocation whose field count did not
    /// match the registered layout), so every String intrinsic dispatches
    /// per-object on the `GC_FLAG_COMPACT` header bit — exactly like the
    /// inline `getfield` path in `x64.rs`. The two layouts place a field at
    /// unrelated addresses:
    ///
    /// * COMPACT — `CompactLayout` packs each field at its natural Java
    ///   width with no tag word, and its registered displacement already IS
    ///   the payload address. Nothing may be added to it.
    /// * LEGACY — every field is a uniform 16-byte `Value` cell at
    ///   `HEADER_SIZE + field_index * SLOT_SIZE`, whose payload sits at
    ///   `FIELD_CELL_PAYLOAD64_OFFSET` (references, 8 bytes) or
    ///   `FIELD_CELL_PAYLOAD32_OFFSET` (int-category, 4 bytes) inside it.
    ///
    /// # BUG-STRING-CODER-COMPACT-20260726 — the compact offsets were +4
    ///
    /// This used to expose a single `*_cell_offset` per field that every
    /// `x64.rs` call site turned into an address by adding its own
    /// `FIELD_CELL_PAYLOAD32/64_OFFSET`, and the emitters derived the legacy
    /// address from the compact one by adding a further fixed
    /// `SLOT_SIZE - REF_FIELD_SIZE` (8). Both derivations were wrong:
    ///
    /// * for the COMPACT arm the extra `+4` on the int-category fields
    ///   pushed `coder`'s read onto `hash` and `hash`'s read onto
    ///   `hashIsZero` (real-JDK `String` packs `value@0, coder@8, hash@12,
    ///   hashIsZero@16`). `coder` normally reads 0 either way — LATIN1 — so
    ///   this was invisible until a String's `hash` cache was populated:
    ///   `length()` then evaluated `value.length >> (hash & 31)`, which
    ///   returns 0 for most hashes. H2 hit it on the interned `"PUBLIC"`
    ///   schema name (used as a `HashMap` key, so its hash was cached), and
    ///   wrote `CREATE SEQUENCE ""."SEQ1"` into its persisted metadata —
    ///   after which reopening the database failed with `Schema  not found`
    ///   (`h2-jitban-schema-not-found-on-reconnect-FIXED.md`).
    ///   `hashCode()` had the mirror defect: it read `hashIsZero` as the
    ///   cached hash, so `"".hashCode()` returned 1.
    /// * for the LEGACY arm the fixed `+8` happens to be right for field
    ///   indices 0 and 1 (`value`, `coder`) and wrong for every later field:
    ///   `hash` (index 2) resolved to `HEADER + 0x18` instead of its real
    ///   cell payload at `HEADER + 2 * SLOT_SIZE + 4`.
    ///
    /// Both arms are now computed from first principles, independently.
    pub fn new(
        value_field_index: usize,
        coder_field_index: Option<usize>,
        hash_field_index: usize,
        string_class_id: u32,
    ) -> Self {
        // LEGACY: uniform 16-byte `Value` cell, payload inside it.
        let legacy = |idx: usize, is_ref: bool| -> i32 {
            let cell = (cratonvm_types::HEADER_SIZE + idx * cratonvm_types::SLOT_SIZE) as i32;
            cell + if is_ref {
                cratonvm_types::FIELD_CELL_PAYLOAD64_OFFSET as i32
            } else {
                cratonvm_types::FIELD_CELL_PAYLOAD32_OFFSET as i32
            }
        };
        // COMPACT: the registered `CompactLayout` body offset IS the payload
        // address. Returns `(address, is_byte_wide)`.
        //
        // FALLBACK (compact ref fields globally off, or no `CompactLayout`
        // registered yet for `string_class_id` — e.g. a class that never
        // reached `ClassStore::add`, as every test in
        // `intrinsic_string_access.rs`/`intrinsic_string_search.rs`
        // deliberately forges via a fake `string_class_id`): reuse the legacy
        // address. Neither condition can produce an instance carrying
        // `GC_FLAG_COMPACT` (see `compact_object_body_size`, the single
        // predicate every collector's allocation path uses), so the compact
        // arm is unreachable, and pointing it at the legacy address keeps it
        // harmless rather than wild if that invariant ever slips.
        // Returns `(address, is_byte_wide, storage_width)`; `storage_width` is
        // 8 for the fallback so a reference there keeps its wide load.
        let compact = |idx: usize, is_ref: bool| -> (i32, bool, u32) {
            if cratonvm_types::compact_ref_fields_enabled() {
                if let Some((body_off, storage)) =
                    cratonvm_types::compact_field_disp_storage(string_class_id, idx)
                {
                    let width = storage.size_runtime();
                    return (body_off as i32, width == 1, width);
                }
            }
            (legacy(idx, is_ref), false, 8)
        };

        let (value_compact_offset, _, value_compact_width) = compact(value_field_index, true);
        let (hash_compact_offset, _, _) = compact(hash_field_index, false);
        let (coder_compact_offset, coder_compact_is_byte, _) =
            coder_field_index.map_or((0, false, 4), |idx| compact(idx, false));
        StringFieldLayout {
            value_field_index,
            value_compact_offset,
            value_compact_is_narrow: value_compact_width
                == cratonvm_types::narrow_oop::NARROW_REF_SIZE as u32,
            value_legacy_offset: legacy(value_field_index, true),
            hash_field_index,
            hash_compact_offset,
            hash_legacy_offset: legacy(hash_field_index, false),
            has_coder: coder_field_index.is_some(),
            coder_field_index: coder_field_index.unwrap_or(0),
            coder_compact_offset,
            coder_legacy_offset: coder_field_index.map_or(0, |idx| legacy(idx, false)),
            coder_compact_is_byte,
            string_class_id,
            builder: None,
            array_list: None,
            hash_is_zero_compact_offset: None,
            locale_language: None,
        }
    }

    /// JIT round 21, lane strings2 (STRINGS21-1): attach the `Locale` ->
    /// `BaseLocale.language` half ([`LocaleLanguageLayout`]).
    pub fn with_locale_language(mut self, locale_language: Option<LocaleLanguageLayout>) -> Self {
        self.locale_language = locale_language;
        self
    }

    /// STRGAP19-6: attach `String.hashIsZero`'s COMPACT address, from its
    /// abstract field slot index. Only a registered one-byte compact slot is
    /// recorded (the `hashCode` bodies read and write it with byte moves and
    /// reach it only on a COMPACT receiver); anything else leaves `None`.
    pub fn with_hash_is_zero(mut self, hash_is_zero_field_index: Option<usize>) -> Self {
        self.hash_is_zero_compact_offset = hash_is_zero_field_index.and_then(|idx| {
            if !cratonvm_types::compact_ref_fields_enabled() {
                return None;
            }
            let (body_off, storage) =
                cratonvm_types::compact_field_disp_storage(self.string_class_id, idx)?;
            (storage.size_runtime() == 1)
                .then(|| i32::try_from(body_off).ok())
                .flatten()
        });
        self
    }

    /// Attach the `java/lang/StringBuilder` half of the layout.
    ///
    /// A setter rather than a fifth parameter to [`Self::new`] because every
    /// existing caller — the resolver, the name probe, and six tests — wants
    /// the String half alone, and the builder half is legitimately absent
    /// (`StringBuilder` not loaded yet, or not the JDK-9+ shape). `None`
    /// leaves every StringBuilder site on ordinary native dispatch, which is
    /// what happened before this existed.
    pub fn with_builder(mut self, builder: Option<StringBuilderFieldLayout>) -> Self {
        self.builder = builder;
        self
    }

    /// Attach the `java/util/ArrayList` half of the layout -- the
    /// [`Self::with_builder`] twin, for the ARRAYLIST_ACCESS family. `None`
    /// leaves every `ArrayList.get`/`size` site on its ordinary dispatch.
    pub fn with_array_list(mut self, array_list: Option<ArrayListFieldLayout>) -> Self {
        self.array_list = array_list;
        self
    }
}

/// `java/lang/StringBuilder`'s `count` / `value` / `coder` addresses, for the
/// StringBuilder call-site intrinsics.
///
/// # Why this exists
///
/// Measured on this tree, one `StringBuilder` native costs what the JIT→native
/// boundary costs and then some: `System.identityHashCode` — the same shape,
/// one object argument, a trivial body — is 120 ns/op, `StringBuilder.length()`
/// is 194 ns and `StringBuilder.append(char)` is 349 ns. Against that,
/// `String.length()` is **2 ns**, because it is one of these intrinsics and
/// never leaves compiled code. The machinery for making a native cost 2 ns
/// instead of 349 already existed; `StringBuilder` was not on it.
///
/// # The shape, and the three fields
///
/// `AbstractStringBuilder` declares `value` (`byte[]`), `coder` (`byte`) and
/// `count` (`int`); `StringBuilder` adds none of its own. The representation is
/// `String`'s, with `count` characters used of `value.length >> coder`
/// available — which is why this carries the same compact/legacy offset pair
/// per field that [`StringFieldLayout`] does, for the same reason (a class with
/// a registered `CompactLayout` may still have LEGACY-laid-out instances, so
/// every access dispatches per object on the `GC_FLAG_COMPACT` header bit).
///
/// # The guard is the class id, and it is exact
///
/// `java/lang/StringBuilder` is `final`, so a site whose receiver's header
/// class id equals `class_id` is that class and no other. The guard is what
/// keeps `StringBuffer` — whose `append` is `synchronized` and whose
/// `toStringCache` must be invalidated on every mutation — off these paths
/// entirely. Neither obligation is emitted here, and neither has to be,
/// because a `StringBuffer` receiver fails the class-id compare and takes the
/// ordinary native call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StringBuilderFieldLayout {
    /// Raw field index of `AbstractStringBuilder.count`.
    pub count_field_index: u32,
    /// Byte offset of `count`'s 4-byte payload in a COMPACT instance.
    pub count_compact_offset: i32,
    /// Byte offset of `count`'s 4-byte payload in a LEGACY instance.
    pub count_legacy_offset: i32,
    /// Raw field index of `AbstractStringBuilder.value`.
    pub value_field_index: u32,
    /// Byte offset of `value`'s reference payload in a COMPACT instance.
    pub value_compact_offset: i32,
    /// Byte offset of `value`'s 8-byte reference payload in a LEGACY instance.
    pub value_legacy_offset: i32,
    /// Whether `value`'s COMPACT slot is a 4-byte **narrow** oop. Same meaning,
    /// and the same hazard, as [`StringFieldLayout::value_compact_is_narrow`].
    pub value_compact_is_narrow: bool,
    /// Raw field index of `AbstractStringBuilder.coder`.
    pub coder_field_index: u32,
    /// Byte offset of `coder`'s payload in a COMPACT instance.
    pub coder_compact_offset: i32,
    /// Byte offset of `coder`'s 4-byte payload in a LEGACY instance.
    pub coder_legacy_offset: i32,
    /// Whether `coder`'s COMPACT slot is one byte wide rather than four.
    pub coder_compact_is_byte: bool,
    /// `ObjectHeader` class id of `java/lang/StringBuilder`, the receiver
    /// guard. Never 0: [`Self::new`] refuses that, because an unguarded decode
    /// of an arbitrary receiver is exactly the hazard the guard exists for.
    pub class_id: u32,
    /// Byte offset of `AbstractStringBuilder.maybeLatin1`'s ONE-byte payload in
    /// a COMPACT instance, or `None` (JIT round 17 wave 6, lane sbzgc2,
    /// SBZGC17-2; `r17-sbzgc-builder-layout-has-no-maybelatin1-FIXED-20261005.md`).
    /// Set only by [`Self::with_maybe_latin1`]; read only by the inline
    /// `toString()`'s UTF16 arm, which declines legacy receivers, so there is
    /// no legacy twin.
    pub maybe_latin1_compact_offset: Option<i32>,
    /// Whether `maybeLatin1 == false` is EXACT for every builder this VM can
    /// hand the inline `toString()`, i.e. it was maintained by the JDK's own
    /// bytecode (`--real-jdk`, where no builder native survives). Only then
    /// may the UTF16 arm read `false` as "copy as UTF16, no scan", the JDK
    /// constructor's own answer. Under `--compatible` the natives mutate
    /// builders and never write the field, so `false` proves nothing and the
    /// arm keeps its scan.
    pub maybe_latin1_false_is_exact: bool,
}

impl StringBuilderFieldLayout {
    /// Build a layout from the three raw field indices, precomputing the
    /// COMPACT and LEGACY payload addresses the same way
    /// [`StringFieldLayout::new`] does — see that function for why each field
    /// needs two, and for the 2026-07-26 defect that came of deriving one from
    /// the other.
    ///
    /// Returns `None` when the class id is 0 (no guard is possible) or when
    /// `count`'s compact storage is not exactly 4 bytes: the emitted code
    /// reads and writes `count` as a 32-bit `int`, so a narrower or wider
    /// storage width would touch the wrong bytes. Both refusals leave the call
    /// on native dispatch.
    pub fn new(
        count_field_index: usize,
        value_field_index: usize,
        coder_field_index: usize,
        class_id: u32,
    ) -> Option<Self> {
        if class_id == 0 {
            return None;
        }
        // The family's opt-out, and it is HERE rather than at the emission
        // site on purpose: refusing the layout refuses registration too, so
        // the two cannot disagree about whether a site is an intrinsic — the
        // disagreement that dropped every method containing one to the
        // interpreter the first time this landed. `CRATONVM_NO_JIT_SB_INTRINSICS=1`
        // therefore gives an A/B whose two arms are one binary.
        if false {
            return None;
        }
        let legacy = |idx: usize, is_ref: bool| -> i32 {
            let cell = (cratonvm_types::HEADER_SIZE + idx * cratonvm_types::SLOT_SIZE) as i32;
            cell + if is_ref {
                cratonvm_types::FIELD_CELL_PAYLOAD64_OFFSET as i32
            } else {
                cratonvm_types::FIELD_CELL_PAYLOAD32_OFFSET as i32
            }
        };
        // `(address, storage width)`. The fallback — compact fields globally
        // off, or no `CompactLayout` registered for this class — reuses the
        // legacy address and reports width 8, exactly as the String twin does:
        // neither condition can produce an instance carrying
        // `GC_FLAG_COMPACT`, so the compact arm is unreachable, and pointing
        // it at the legacy address keeps it harmless rather than wild if that
        // invariant ever slips.
        let compact = |idx: usize, is_ref: bool| -> (i32, u32) {
            if cratonvm_types::compact_ref_fields_enabled() {
                if let Some((body_off, storage)) =
                    cratonvm_types::compact_field_disp_storage(class_id, idx)
                {
                    return (body_off as i32, storage.size_runtime());
                }
            }
            (legacy(idx, is_ref), 8)
        };

        let (count_compact_offset, count_width) = compact(count_field_index, false);
        // The fallback reports 8 and means "legacy address", which IS four
        // bytes of payload; a REGISTERED width that is not 4 is the refusal.
        if cratonvm_types::compact_ref_fields_enabled()
            && cratonvm_types::compact_field_disp_storage(class_id, count_field_index).is_some()
            && count_width != 4
        {
            return None;
        }
        let (value_compact_offset, value_width) = compact(value_field_index, true);
        let (coder_compact_offset, coder_width) = compact(coder_field_index, false);
        // A NARROW `value` oop would need the decode
        // `emit_load_string_value_ptr` does for the String family; the
        // StringBuilder codegen loads the reference with a plain 8-byte `MOV`
        // and refuses rather than half-decode it — the exact shape of hole 1
        // in `gc/src/compressed_oops.rs`, where an unconditional 64-bit load
        // took four bytes of narrow oop plus four bytes of the adjacent field
        // and dereferenced the result.
        //
        // Refused HERE, in the constructor, and not at the emission site: the
        // registration path and the codegen both go through this layout, so a
        // refusal either side of it alone would register an intrinsic the
        // emitter then could not emit — which fails the whole compile and
        // drops the method to the interpreter. Narrow oops are default-off on
        // this tree, so today this costs nothing.
        if value_width == cratonvm_types::narrow_oop::NARROW_REF_SIZE as u32 {
            return None;
        }

        Some(Self {
            count_field_index: u32::try_from(count_field_index).ok()?,
            count_compact_offset,
            count_legacy_offset: legacy(count_field_index, false),
            value_field_index: u32::try_from(value_field_index).ok()?,
            value_compact_offset,
            value_legacy_offset: legacy(value_field_index, true),
            value_compact_is_narrow: value_width
                == cratonvm_types::narrow_oop::NARROW_REF_SIZE as u32,
            coder_field_index: u32::try_from(coder_field_index).ok()?,
            coder_compact_offset,
            coder_legacy_offset: legacy(coder_field_index, false),
            coder_compact_is_byte: coder_width == 1,
            class_id,
            maybe_latin1_compact_offset: None,
            maybe_latin1_false_is_exact: false,
        })
    }

    /// Attach `AbstractStringBuilder.maybeLatin1` (JIT round 17 wave 6, lane
    /// sbzgc2, SBZGC17-2): its field index, and whether a `false` there is
    /// exact (see [`Self::maybe_latin1_false_is_exact`]; the VM resolver
    /// passes its `--real-jdk` policy).
    ///
    /// Refused -- the layout keeps `None`, and the UTF16 arm its wave-2 shape
    /// -- when the index is absent, when compact fields are off, or when the
    /// class has no registered compact slot for it or that slot is not
    /// exactly ONE byte: the arm reads it with a byte load, the way `count` is
    /// refused at any width but four.
    pub fn with_maybe_latin1(mut self, field_index: Option<usize>, false_is_exact: bool) -> Self {
        self.maybe_latin1_compact_offset = None;
        self.maybe_latin1_false_is_exact = false;
        let Some(idx) = field_index else {
            return self;
        };
        if !cratonvm_types::compact_ref_fields_enabled() {
            return self;
        }
        if let Some((off, storage)) = cratonvm_types::compact_field_disp_storage(self.class_id, idx)
        {
            if storage.size_runtime() == 1 {
                if let Ok(off) = i32::try_from(off) {
                    self.maybe_latin1_compact_offset = Some(off);
                    self.maybe_latin1_false_is_exact = false_is_exact;
                }
            }
        }
        self
    }
}

/// Compile-time resolved info for a method invocation from JIT code.
pub struct JitInvokeInfo {
    pub class_name: &'static str,
    pub method_name: &'static str,
    pub descriptor: &'static str,
    pub num_jit_args: usize,
    pub return_type: u8,
    /// Dispatch encoding read by `jit_invoke_dispatch`: 0 = virtual, 1 = special,
    /// 2 = interface, 3 = static. Value `4` = self-recursive static DIRECT call:
    /// an IR-only marker (set by the eligibility loop when
    /// `CRATONVM_JIT_IR_SELFREC_DIRECT` is on) that makes `ir_lower` emit a direct
    /// `CALL` to the method's own entry; such a call NEVER reaches the dispatch
    /// helper, so the helper need not handle 4.
    pub invoke_kind: u8,
    /// `ObjectHeader` class id of the method this call site lives IN, or `0`
    /// when the compiler could not supply one.
    ///
    /// The other three name fields are the raw constant-pool text, and a class
    /// NAME is not a class identity: two loaders can each define
    /// `org/h2/util/IntArray`. `jit_invoke_dispatch` resolves invokespecial
    /// targets by name, which silently picks whichever copy the global
    /// name->id map happens to hold — the caller's own loader is the only
    /// thing that says which one the constant pool meant.
    ///
    /// BUG-JIT-INVOKESPECIAL-LOADER-20260726: H2's `TestUpgrade` loads
    /// h2-1.2.120 in its own loader alongside the current H2 on the app
    /// classpath. Old `IntArray.add` does `invokespecial checkCapacity()V` on
    /// its own private method; once `add` was JIT-compiled, the dispatch
    /// resolved `org/h2/util/IntArray` to the CURRENT copy, which has
    /// `ensureCapacity(int)` and no `checkCapacity()` at all —
    /// `NoSuchMethodError: org.h2.util.IntArray.checkCapacity()V`, from a
    /// stack made entirely of 1.2.120 frames. The interpreter resolves the
    /// same site through the caller's constant pool and gets it right, which
    /// is why the failure was JIT-only.
    ///
    /// `0` means "no context, resolve as before" — every synthetic call site
    /// (the `Integer.valueOf` / `HashMap.get` direct-dispatch statics) uses it,
    /// and they name JDK classes that only ever have one definition.
    pub declaring_class_id: u32,
    /// For a statically bound (`invoke_kind == 1`) site whose `class_name` the
    /// compile SUBSTITUTED for the constant-pool class
    /// (`CompileRequest::cp_invokespecial_owner_resolver`: the declaring class
    /// of a private or `final` target, the JVMS §6.5 selection start of a
    /// super call), the `ObjectHeader` class id of that owner, as the
    /// substitution's own hierarchy walk found it. `0` = no substitution, or
    /// no id: `class_name` is the constant pool's own name and
    /// `jit_invoke_dispatch` resolves it through `declaring_class_id`'s
    /// loader as before.
    ///
    /// A substituted name is not one the caller's constant pool references,
    /// so resolving it by name in the caller's loader asks that loader for a
    /// class the bytecode never names (`java/lang/AbstractStringBuilder` for
    /// `StringBuilder.length()`), and can find another loader's copy of it.
    /// The helper uses this id instead while it still names a loaded class
    /// called `class_name`.
    pub owner_class_id: u32,
}

/// The one-shot first-execution resolution of a compiled call site whose
/// method reference no resolution door had resolved when the method was
/// compiled (interpreter round i1 wave 50, lane L1;
/// `interpreter-L5-getinitiatedclasses-residual-recording-differences-FIXED-20261007`, item 2).
///
/// JVMS §5.4.3 resolves a symbolic reference when the instruction that names
/// it first EXECUTES, and the resolution is what makes the referencing class's
/// loader an initiating loader of the reference's class
/// (`Instrumentation.getInitiatedClasses`). HotSpot's compilers leave such a
/// site to an uncommon trap, so a site that never runs records nothing and a
/// site that runs records at that first run. The compile door here peeks at
/// the site without recording, and the backend emits, in front of the site's
/// whole lowering (a splice, a direct bind, an inline cache or the dispatch
/// helper alike):
///
/// ```text
///   push rax ; mov rax, cell ; cmp byte [rax], 0 ; pop rax ; je .done
///   <save every volatile register> call [cell.resolve](cell) <restore>
/// .done:
/// ```
///
/// so the hot path of a resolved site is untouched, and the hot path of a
/// flagged one is one predictable branch. The cold arm is a LEAF: the VM's
/// `resolve` writes the owner's row and clears [`Self::armed`]; it allocates
/// nothing, runs no Java code and reaches no safepoint, which is why it may
/// sit in front of a site whose operand stack is still live.
///
/// `#[repr(C)]`: the emitted code reads [`Self::armed`] at offset 0 and
/// [`Self::resolve`] at [`Self::RESOLVE_OFFSET`]. Cells are owned by the VM
/// (one per `(class, constant-pool index, owner)`, never freed while the VM
/// lives), so a baked address outlives every body that names it.
#[repr(C)]
pub struct JitFirstCallCell {
    /// `1` while the site has not run in any body that guards it; `0` once
    /// the cold arm (or anything else that resolved the site) has run.
    pub armed: std::sync::atomic::AtomicU8,
    /// The VM's cold arm, `extern "C" fn(cell)`, called with this cell.
    pub resolve: unsafe extern "C" fn(*const JitFirstCallCell),
    /// The VM the cell belongs to (`*const SharedVm`).
    pub ctx: usize,
    /// The class whose constant pool names the site.
    pub referencing: u32,
    /// The class the site's `Methodref` class entry names in that class's
    /// namespace, found by the compile door's peek.
    pub owner: u32,
}

impl JitFirstCallCell {
    /// Byte offset of [`Self::resolve`], read by the emitted cold arm.
    pub const RESOLVE_OFFSET: u8 = 8;
}

const _: () = {
    assert!(std::mem::offset_of!(JitFirstCallCell, armed) == 0);
    assert!(
        std::mem::offset_of!(JitFirstCallCell, resolve)
            == JitFirstCallCell::RESOLVE_OFFSET as usize
    );
};

/// Every handler in THIS method's own exception table that can catch a throw at
/// one bci — so a compiled frame can enter its own `catch` block without
/// leaving compiled code.
///
/// ## Why this exists
///
/// Until 2026-08-20 nothing in this VM ran a `catch` block in compiled code. A
/// caught exception left the artifact entirely: reason-9 deopt, exceptional
/// frame reconstruction, an interpreted handler, and — inside an OSR'd loop —
/// a re-entry at the next hot back edge. Priced on
/// `probes/OsrExcRateProbe.java` that round trip is **~2 900 ns per catch**
/// against HotSpot's 6.7-16, and at `HttpHeaderValidationUtilTest`'s measured
/// 7.7% throw rate it is 223 ns of a 21 ns/iteration budget — the largest
/// single item on that class by an order of magnitude.
///
/// ## What the stub does with it
///
/// The compiled body's post-invoke sentinel edge branches to a per-site stub
/// which calls `jit_local_handler_lookup(vm, site, out_exc)`. That returns the
/// index into [`Self::candidates`] of the first handler whose catch type
/// matches the pending throwable — JVMS order, so the answer is the
/// interpreter's answer — or `-1`. On `-1` the stub falls through to exactly
/// the edge that would have run before (the reason-9 deopt stub or the shared
/// sentinel exit), so nothing about the propagating case changes. On a hit the
/// helper has already stored the throwable into the frame slot the handler's
/// operand stack starts at, and the stub jumps to the handler's compiled code.
///
/// ## The cache
///
/// One `u64`, not two `u32`s, and that is load-bearing: a reader must never be
/// able to pair one throwable's class id with another's index. `0` is empty;
/// otherwise the top bit is set, bits 32..62 hold the class id and the low 32
/// hold the index (`-1` for "no local handler", which is worth caching too — a
/// site that keeps propagating must not re-resolve catch types every time).
pub struct JitLocalHandlerSite {
    /// `(catch type name, handler bci)` in exception-table order, restricted to
    /// the entries whose `[start_pc, end_pc)` covers [`Self::throw_bci`]. An
    /// EMPTY name is a catch-all (`catch_type == 0`, i.e. `finally`), which
    /// matches every throwable.
    pub candidates: Vec<(&'static str, u32)>,
    /// The class this method is declared in. A catch-type NAME is not a class
    /// identity — two loaders can each define one — so the name is resolved
    /// through this class's loader, exactly as `find_jit_exception_handler`
    /// does for the interpreted route.
    pub declaring_class_id: u32,
    /// The throwing bci this site guards. Diagnostics only; the candidate list
    /// was already filtered by it at compile time.
    pub throw_bci: u32,
    /// Monomorphic `(exception class id -> candidate index)` cache. See the
    /// type doc for the encoding.
    pub cache: std::sync::atomic::AtomicU64,
    /// Resolved candidate index per IMPLICIT exception kind, indexed by
    /// [`JitLocalHandlerSite::IMPLICIT_KIND_AIOOBE`] /
    /// [`JitLocalHandlerSite::IMPLICIT_KIND_NPE`] /
    /// [`JitLocalHandlerSite::IMPLICIT_KIND_ARITHMETIC`].
    ///
    /// A separate table rather than a second use of [`Self::cache`], for two
    /// reasons. The kind is known from the SIGNAL, before any throwable
    /// exists, so this cache answers "does this frame catch it?" without
    /// building one -- which is the whole point on a site that propagates.
    /// And a site that sees both a dispatched throwable and an implicit trap
    /// would thrash one monomorphic slot between them.
    ///
    /// [`Self::IMPLICIT_UNRESOLVED`] is "not asked yet"; anything else is the
    /// candidate index, or `-1` for "this frame does not catch this kind".
    /// Never invalidated, exactly like [`Self::cache`]: the answer is a
    /// property of this method's own exception table and the class hierarchy
    /// above three JDK throwables, and the site lives and dies with the
    /// machine code that names it.
    pub implicit_cache: [std::sync::atomic::AtomicI32; 3],
    /// JIT round 19 wave 5 (lane handler5, SPLICE4-1): the site is an
    /// optimizing body's compiled-handler probe (`ir_lower::Lowerer::
    /// emit_catch_probes`), which runs where no safepoint map describes the
    /// frame. The lookup then must not reach a GC point: it answers only for
    /// an already-built pending throwable whose catch type is already loaded,
    /// and declines (propagate, nothing consumed) where the single-pass stubs
    /// would load a class or build an implicit throwable. `false` for every
    /// single-pass site, whose behaviour is unchanged.
    pub no_collect: bool,
}

impl JitLocalHandlerSite {
    /// `cache` is empty.
    pub const CACHE_EMPTY: u64 = 0;

    /// [`Self::implicit_cache`] slot for an `ArrayIndexOutOfBoundsException`
    /// raised by a compiled bounds check.
    pub const IMPLICIT_KIND_AIOOBE: usize = 0;
    /// [`Self::implicit_cache`] slot for a `NullPointerException` raised by a
    /// compiled inline null check.
    pub const IMPLICIT_KIND_NPE: usize = 1;
    /// [`Self::implicit_cache`] slot for an `ArithmeticException` raised by a
    /// compiled zero-divisor guard.
    pub const IMPLICIT_KIND_ARITHMETIC: usize = 2;
    /// [`Self::implicit_cache`] "not resolved yet". Distinct from `-1`, which
    /// is the resolved answer "this frame does not catch this kind".
    pub const IMPLICIT_UNRESOLVED: i32 = i32::MIN;

    /// Round nio21 wave 4 (lane L2): set in [`Self::throw_bci`] (a bci never
    /// reaches bit 31) for a site inside a SPLICED callee body whose own
    /// handlers the candidates are (`ir::IrCallThrowSite::spliced`). Such a
    /// site has no correct `-1` for a throwable a candidate catches: its
    /// fallback exit leaves through the caller's `invoke`, past the callee's
    /// `catch`. So `jit_local_handler_lookup` answers it even while a JVMTI
    /// exception client is armed (the edge then enters the handler, which
    /// resumes in the interpreter at once; the `ExceptionCatch` event of that
    /// catch is the cost, in the same window the athrow edges accept). The bit
    /// also keeps the site out of `deopt::local_handler_replay_edges`, whose
    /// bcis are the compiling method's own.
    pub const SPLICED_SCOPE_BIT: u32 = 1 << 31;

    /// [`Self::SPLICED_SCOPE_BIT`] is set.
    pub fn must_answer(&self) -> bool {
        self.throw_bci & Self::SPLICED_SCOPE_BIT != 0
    }

    /// t1gate (page `t1-SAFEPOLL-proposal-poll-on-backward-handler-edges`): is
    /// candidate `index` a handler edge the single-pass tier may DECLINE while a
    /// pause or handshake is pending -- one whose handler bci is at or below the
    /// throwing bci, so the exception edge closes a cycle that no back-edge poll
    /// sits on (`javac` never makes one except the self-covering `any` handler of
    /// a `synchronized` block; hand-written bytecode and other compilers can)?
    ///
    /// `false` for a spliced-scope site ([`Self::must_answer`]: its `-1` exit
    /// would skip the callee's own `catch`, so it has no correct decline, and
    /// its `throw_bci` is a callee bci), for an optimizing body's probe
    /// (`no_collect`: the IR polls every block edge, handler arms included), for
    /// a negative or out-of-range `index` (the "no local handler" answers), and
    /// for every forward edge.
    pub fn declinable_backward_handler_edge(&self, index: i32) -> bool {
        if self.must_answer() || self.no_collect {
            return false;
        }
        let Ok(i) = usize::try_from(index) else {
            return false;
        };
        self.candidates
            .get(i)
            .is_some_and(|&(_, handler)| handler <= self.throw_bci)
    }

    /// A fresh [`Self::implicit_cache`], all three kinds unresolved.
    pub fn empty_implicit_cache() -> [std::sync::atomic::AtomicI32; 3] {
        [
            std::sync::atomic::AtomicI32::new(Self::IMPLICIT_UNRESOLVED),
            std::sync::atomic::AtomicI32::new(Self::IMPLICIT_UNRESOLVED),
            std::sync::atomic::AtomicI32::new(Self::IMPLICIT_UNRESOLVED),
        ]
    }

    /// Encode a `(class id, index)` pair for [`Self::cache`].
    #[inline]
    pub fn encode_cache(class_id: u32, index: i32) -> u64 {
        // Cast: the index is packed as raw bits and decoded back as `i32`.
        (1u64 << 63) | ((class_id as u64) << 32) | (index as u32 as u64)
    }

    /// Decode [`Self::cache`], or `None` when it is empty.
    #[inline]
    pub fn decode_cache(raw: u64) -> Option<(u32, i32)> {
        if raw & (1u64 << 63) == 0 {
            return None;
        }
        // Cast: exact inverse of `encode_cache`.
        Some((((raw >> 32) & 0x7fff_ffff) as u32, raw as u32 as i32))
    }
}


// ===========================================================================
// Real-JDK mode — the JIT's half of the native-dispatch contract
//
// See `docs/book/src/user-guide/real-jdk-mode.md` §1, §7 and §10.
//
// # Why this lives here and not behind `resolve_dispatch`
//
// `docs/book/src/user-guide/real-jdk-mode.md` §7 names
// `vm/src/vm/vm_exec.rs::resolve_dispatch` as THE single native-vs-bytecode
// decision point, and requires the JIT to route through it. This crate
// **cannot call it**: `cratonvm-vm` depends on `cratonvm-jit`, so the reverse
// edge would be a dependency cycle (see `jit/Cargo.toml` — the only crates in
// scope are `types`, `reader` and `jit-api`).
//
// Compiled code therefore reaches natives through the VM-implemented helper
// pointers in `JitRuntimeHelpers` (`invoke_dispatch`, `invoke_virtual_mic`),
// and *those* are the sites that must call `resolve_dispatch` — they live in
// `vm/src/jit/helpers.rs`, which this agent does not own. What the JIT crate
// owns, and what this module governs, is the set of places where the JIT
// **decides on its own, at compile time, to bypass that helper**:
//
//   1. the thin direct-call native helpers (`*_DIRECT_FN` below), which bake a
//      VM-side reimplementation of a registered native straight into the
//      emitted `CALL`; and
//   2. inline-cache publication of an *unowned* entry pointer
//      (`jit_entry_publishable`), which is how a native/builtin trampoline ends
//      up being `CALL R11`'d by generated code with no dispatch helper — and
//      therefore no policy check — anywhere on the path.
//
// Under `CompatibilityMode::JdkOnly` both are refused, and the site falls back
// to the ordinary dispatch helper, which is policy-checked. That is the
// wave-1-safe answer §10 asks for: refuse to bake, let the checked path handle
// it. It is never a silent fallback to a stub and never a wild jump — the
// fallback route is the same one an unresolvable call site already takes today.
//
// # Compile time vs execution time
//
// A JIT decision is made once, at compile time, and executes for the life of
// the artifact. A callback bound under one policy must never execute under
// another. Three properties give that:
//
//   * The policy is the VM's own (`VmConfig::execution_policy`, read as
//     `dispatch_policy(shared).is_jdk_only()`) and reaches the JIT as an
//     **argument** — `jdk_only` on `try_compile_with_invokespecial_resolver`,
//     and on `jit_entry_publishable` for inline-cache publication. It is fixed
//     for the life of the VM, so no artifact of that VM is ever compiled under
//     a different policy than it runs under.
//   * Every refusal is a compile-time *omission*, never a runtime branch in
//     emitted code, so an already-compiled Compatible artifact keeps running
//     under the policy it was built for.
//
// # Cost in Compatible mode
//
// Reading the policy is a `bool` argument test on a compile-time path
// (`try_compile`'s bytecode scan) or on the cold inline-cache *miss* path.
// Nothing is added to any emitted instruction sequence, so compiled Compatible
// code is byte-for-byte what it was.
//
// # No process global
//
// §2 of the design forbids process globals for this feature's state, and
// AGENTS.md forbids them for compatibility state generally. There was one: a
// `JIT_COMPATIBILITY_MODE` `AtomicU8` that `build_helpers` latched with
// `fetch_max`, so it only ever moved `Compatible -> JdkOnly`. A VM created in
// `--real-jdk` mode made every LATER VM in the process over-strict. Its
// dispatch-affecting readers were threaded per VM on 2026-08-06
// (`*_DIRECT_FN` binds) and 2026-08-10 (`jit_entry_publishable`); the last
// reader, the VM-less legacy [`try_compile`] wrapper, now passes `Compatible`
// explicitly — what it always read, because no VM calls that wrapper — and the
// static, its setter and its two readers were deleted on 2026-09-12. See
// `jit-compatibility-and-despec-state-per-vm-FIXED.md`.
// ===========================================================================

// t1 wave 3 (lane Y): the compile-time list, its two refusal counters and its
// drop counter were process statics here (`JDK_ONLY_VIOLATIONS`,
// `JDK_ONLY_DIRECT_NATIVE_REFUSALS`, `JDK_ONLY_IC_NATIVE_REFUSALS`,
// `JDK_ONLY_VIOLATIONS_DROPPED`), so every `--real-jdk` VM in the process wrote
// one report. They are a per-VM `jdk_only_sink::JdkOnlyReportSinks` on
// `JitCache` now; the free functions below read the PROCESS sinks, the ones
// work with no VM in scope records into (unit tests, the positional shim, an
// inline-cache refusal outside a compile scope). A VM's report reads
// `JitCache::jdk_only_sinks`.

/// Compile-time thin-native direct binds refused under `JdkOnly` with no VM
/// in scope ([`jdk_only_sink::process_jdk_only_sinks`]).
pub fn jdk_only_direct_native_refusals() -> u64 {
    crate::jdk_only_sink::process_jdk_only_sinks().jit_compile.events()
}

/// Inline-cache native-entry publications refused under `JdkOnly` outside
/// any VM's compile scope ([`jdk_only_sink::process_jdk_only_sinks`]).
pub fn jdk_only_ic_native_refusals() -> u64 {
    crate::jdk_only_sink::process_jdk_only_sinks().ic_native_refusals()
}

/// Default maximum number of distinct structured violations the JIT retains.
///
/// Overridable, since 2026-08-22, by the SAME knob the other two sinks read —
/// see [`jdk_only_violation_cap`]. The constant stays `pub` because records and
/// `vm_init.rs` quote it, but the value in force is the function's.
pub const JDK_ONLY_VIOLATION_CAP: usize = 256;

/// Ceiling on the operator override. Mirrors the other two sinks'.
pub(crate) const JDK_ONLY_VIOLATION_CAP_MAX: usize = 65_536;

/// The cap this process is actually using for the compile-time sink.
///
/// Reads `CRATONVM_NATIVE_SHADOW_SINK_CAP` through
/// [`cratonvm_types::flags::resolve_capped_usize`], which is the same resolver
/// the interpreter and JIT fast-path sinks use. Before this, the report's
/// saturation advice named a knob that moved two of the report's three bounded
/// collections and silently left the third at 256.
pub fn jdk_only_violation_cap() -> usize {
    static CAP: OnceLock<usize> = OnceLock::new();
    *CAP.get_or_init(|| {
        cratonvm_types::flags::resolve_capped_usize(
            "CRATONVM_NATIVE_SHADOW_SINK_CAP",
            "JIT compile-time violation sink",
            JDK_ONLY_VIOLATION_CAP,
            JDK_ONLY_VIOLATION_CAP_MAX,
        )
    })
}

/// How many distinct violations this sink had no room for.
///
/// Dedup is `Vec::contains` over RETAINED rows, so a repeat of an already
/// dropped row counts again. It errs upward, which is the safe direction for a
/// floor warning.
pub fn jdk_only_jit_sink_dropped() -> u64 {
    crate::jdk_only_sink::process_jdk_only_sinks().jit_compile.dropped()
}

/// How many distinct violations this sink is holding.
pub fn jdk_only_jit_sink_len() -> usize {
    crate::jdk_only_sink::process_jdk_only_sinks().jit_compile.len()
}

/// Did this sink drop anything? NOT `len == cap`: a run whose last distinct
/// violation exactly fills the sink drops nothing, exactly as
/// `cratonvm_vm::vm::jdk_only_native_shadow_sink_saturated` documents.
pub fn jdk_only_jit_sink_saturated() -> bool {
    jdk_only_jit_sink_dropped() > 0
}

/// Snapshot of the structured Real-JDK violations the JIT recorded this run.
///
/// Read by the launcher when building `--real-jdk-report`. Never on a hot path.
pub fn jdk_only_jit_violations() -> Vec<cratonvm_types::error::JdkOnlyViolation> {
    crate::jdk_only_sink::process_jdk_only_sinks().jit_compile.rows()
}

/// Build and record the violation for a refused thin-native direct bind.
///
/// `#[cold]` + `#[inline(never)]`, mirroring `vm_exec.rs`'s reject helpers: the
/// `String` allocations here exist only on the reject path and are never
/// reachable in Compatible mode.
/// H20-1: `pub` so the OSR and eager first-call ladders — which live in the VM
/// crate and cannot reach [`direct_native_helper`] at all — record their
/// refusals in the SAME violation list and the SAME counter as the MethodEntry
/// door. Two doors refusing the same triple into two different reports would be
/// a per-door `--real-jdk-report`, which is exactly the drift
/// `compile_gate`'s module doc says keeps happening here.
#[cold]
#[inline(never)]
pub fn record_jdk_only_direct_native_refusal(class: &str, method: &str, descriptor: &str) {
    // t1 wave 3 (lane Y): into the compiling VM's sinks (the scope
    // `try_compile_request` opened), or the process sinks with no VM in scope.
    crate::jdk_only_sink::with_current_jdk_only_sinks(|sinks| {
        record_jdk_only_direct_native_refusal_into(sinks, class, method, descriptor)
    });
}

/// [`record_jdk_only_direct_native_refusal`] into a named VM's sinks: for the
/// VM-side ladders (the OSR and eager first-call doors), which hold the VM.
#[cold]
#[inline(never)]
pub fn record_jdk_only_direct_native_refusal_into(
    sinks: &crate::jdk_only_sink::JdkOnlyReportSinks,
    class: &str,
    method: &str,
    descriptor: &str,
) {
    // The violation is built BEFORE the capacity test, deliberately: the old
    // order returned at the cap without ever asking whether this triple was
    // already recorded, so the sink could not tell "full of other rows" from
    // "full, and this row is one of them" — and therefore could not count a
    // DISTINCT drop. `helpers.rs` was fixed the same way on 2026-08-20.
    // `#[cold]`, so the two `String`s cost nothing on any reachable path.
    // `NativeShadowsBytecode` is the accurate shape: the thin helper is a
    // VM-side reimplementation of a registered native standing in front of the
    // real JDK's own bytecode for the same method (`HashMap.put`,
    // `Integer.valueOf`, `String.toLowerCase`, …). §1 rule 4 says concrete
    // bytecode wins, so binding the helper is exactly "a native shadows
    // bytecode". The kind string names the JIT mechanism rather than a
    // `NativeKind` because this crate cannot see that enum (it lives in
    // `native-api`, which `jit` does not depend on); the registry-side census
    // is authoritative for the registered kind of the same triple.
    let violation = cratonvm_types::error::JdkOnlyViolation::NativeShadowsBytecode {
        class: class.to_string(),
        method: method.to_string(),
        descriptor: descriptor.to_string(),
        native_kind: "jit-thin-direct-helper",
        // This crate holds a name triple and no class store — the paragraph
        // above says so about `native_kind` for the same reason. The report
        // writer fills the origin in through `with_class_origin` once, cold,
        // where the store is in hand.
        class_origin: None,
    };
    // The event counter, the dedup-before-cap test and the drop counter are
    // `JdkOnlyViolationSink::record`'s, unchanged from the process sink.
    sinks
        .jit_compile
        .record(Some(violation), jdk_only_violation_cap());
}

/// Record an inline-cache publication refused because the entry is an unowned
/// (native/builtin) target and `JdkOnly` is in force. Cold by construction.
#[cold]
#[inline(never)]
pub(crate) fn record_jdk_only_ic_native_refusal() {
    crate::jdk_only_sink::with_current_jdk_only_sinks(|sinks| {
        sinks
            .jit_ic_native_refusals
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    });
}

/// Load a thin direct-call native helper address, honouring Real-JDK policy.
///
/// Returns `0` — the established "not wired, use the generic dispatch helper"
/// sentinel every one of these sites already handles — when `JdkOnly` is in
/// force. Under `Compatible` this is a plain relaxed load, identical to the
/// bare `cell.load(Relaxed)` it replaced.
///
/// This runs inside `try_compile`'s compile-time bytecode scan, not in emitted
/// code, so the policy read costs nothing measurable.
#[inline]
pub(crate) fn direct_native_helper(
    entry: usize,
    jdk_only: bool,
    direct_bind_admission: Option<&dyn Fn(&str, &str, &str) -> bool>,
    class: &str,
    method: &str,
    descriptor: &str,
) -> usize {
    if entry != 0 && jdk_only {
        // JDK-ONLY-WAVE2 §4. The question is no longer "is this one of seven
        // hard-coded triples" — the seven are still how the RECOGNITION picks
        // a cell, because that is a triple-to-helper-address map the registry
        // does not have — but the POLICY answer now comes from the registry's
        // own `NativeKind`, which is where §1.4's reviewed-exception verdict
        // actually lives.
        //
        // 2026-09-22: the question the VM answers here CHANGED, and the
        // paragraph that used to stand in this place — "the seven split 3/3/1
        // … `HashMap.put`, `HashMap.get` and `ConcurrentMap.get` are `Bridge`
        // and [§1.4] does not [permit them]" — was true about the registry and
        // wrong about the outcome. Refusing a bind does not stop the call: the
        // site falls through to `invoke_info` and the generic tail,
        // `vm_exec::invoke_or_native`, whose own §7 step-3 input is
        // structurally `false` for a `Bridge` — so the tail ran the very
        // native this refusal was taken to prevent, having first paid a
        // three-string registry hash for the privilege.
        //
        // So the resolver now answers *would the tail admit this triple*, of
        // which "is it a reviewed `Intrinsic`" is one arm. `Bridge` is
        // admitted exactly when the tail would run it and refused exactly when
        // the tail would yield (the `CRATONVM_ENFORCE_NATIVE_SHADOW` dial);
        // `SyntheticStub` and unregistered stay refused, which is the
        // fail-closed direction and unchanged. `None` — no way to ask — still
        // refuses everything.
        //
        // The VM side is `jit::helpers::direct_native_bind_admitted`, which is
        // the same predicate the callee-side gate runs per call, and
        // `docs/feature-designs/jit-ir-optimizations.md`
        // has the measurement.
        let approved =
            direct_bind_admission.is_some_and(|admits| admits(class, method, descriptor));
        if !approved {
            record_jdk_only_direct_native_refusal(class, method, descriptor);
            return 0;
        }
    } else if entry != 0
        && compat_direct_bind_refused(direct_bind_admission, class, method, descriptor)
    {
        return 0;
    }
    entry
}

/// JIT round 23 wave 1, lane L6 (SYNC22-3 of `jit-r22-sync-proposals.md`):
/// the `--compatible` half of [`direct_native_helper`]'s bind-time question.
///
/// It used to be asked only under `--real-jdk`, so under `--compatible` a
/// recognised site bound its thin helper even where the registry holds no row
/// for the triple -- a `COMPAT_FAMILIES` row unregistered it, or it was never
/// registered -- and the interpreter and the generic tail therefore run the
/// JDK's own bytecode: compiled code then ran a VM-side body in front of JDK
/// bytecode the cold path runs
/// (`docs/feature-designs/jit-ir-optimizations.md`).
///
/// The policy is the VM's (`vm::jit::helpers::direct_native_bind_admitted`'s
/// `--compatible` arm, behind its per-VM switch
/// `CRATONVM_JIT_COMPAT_BIND_ASKS_REGISTRY`); a request without an admission
/// closure (hand-built, tests) keeps the old unconditional bind. A refused site
/// keeps its ordinary ladder (direct bind, IC or splice of the JDK body). Not a
/// `--real-jdk` violation, so nothing is recorded in that report;
/// `CRATONVM_DBG_JITC=1` names each refusal. `true` = refuse.
pub(crate) fn compat_direct_bind_refused(
    direct_bind_admission: Option<&dyn Fn(&str, &str, &str) -> bool>,
    class: &str,
    method: &str,
    descriptor: &str,
) -> bool {
    let refused = direct_bind_admission.is_some_and(|admits| !admits(class, method, descriptor));
    if refused && crate::x64::dbg_jitc_enabled() {
        eprintln!(
            "[cratonvm-jitc] compat-direct-bind REFUSED {class}.{method}{descriptor}: the \
             --compatible registry holds no row the helper could stand for; the site keeps \
             its bytecode dispatch"
        );
    }
    refused
}

/// JIT round 20 wave 14 (lane cjavac3): under `--compatible`, may an
/// exact-map thin helper (`hashmap_get` / `hashmap_put` /
/// `concurrent_hashmap_get`) be bound at all? [`direct_native_helper`] asks
/// the VM only under `--real-jdk`, but a `--compatible` row can run the JDK's
/// own map too (`jdk-hash-collections`, default on over a real JDK;
/// `jdk-concurrent-hash-map`), and then the helper's per-call gate refuses
/// every call (`vm::jit::helpers::hashmap_gate_under_jdk_model`): a bound
/// site paid the helper's screens and then the generic dispatch on every
/// call. So the door asks the VM's bind-time answer
/// (`direct_native_bind_admitted`, whose `--compatible` arm is
/// `compat_row_refuses_collection_bind`) about the IMPLEMENTING row, and a
/// refused site keeps the ordinary ladder -- the JDK `HashMap.get` / `put`
/// bytecode, dispatched, IC-bound or spliced -- as under `--real-jdk`. With
/// the row off the answer is `true` and today's bind stands. `true` = refuse.
/// `--real-jdk` is [`direct_native_helper_for_impl`]'s business, unchanged.
pub(crate) fn compat_collection_bind_refused(
    jdk_only: bool,
    direct_bind_admission: Option<&dyn Fn(&str, &str, &str) -> bool>,
    impl_class: &str,
    method: &str,
    descriptor: &str,
) -> bool {
    let refused = !jdk_only
        && direct_bind_admission.is_some_and(|admits| !admits(impl_class, method, descriptor));
    if refused && crate::x64::dbg_jitc_enabled() {
        eprintln!(
            "[cratonvm-jitc] compat-collection-bind REFUSED {impl_class}.{method}{descriptor}: \
             the --compatible row runs the JDK's own map; the site keeps its bytecode dispatch"
        );
    }
    refused
}

/// [`direct_native_helper`] for a ladder whose CALL-SITE class is not the class
/// whose native the helper actually runs.
///
/// # H7-1: a serviceability predicate must mirror the dispatch it guards
///
/// Two of the collection ladders below recognise an `invokeinterface` against
/// an interface — `java/util/Map.get` and
/// `java/util/concurrent/ConcurrentMap.get` — and bind a helper that runs the
/// *implementation's* native: `native_hashmap_get_exact`, registered on
/// `java/util/HashMap`, and `native_chm_get`, registered on
/// `java/util/concurrent/ConcurrentHashMap`. `direct_native_helper` asks the
/// registry about the triple it is handed, so those two sites were asking
/// §1.4's reviewed-exception question about a row that is not the one that
/// runs.
///
/// When H7-1 was written, it happened to answer correctly: all four rows were
/// `bridge` in `scripts/baselines/jdk-only-kind-map-25-linux.tsv`, so both
/// questions refused. That was a coincidence of the tagging, not a property of
/// the code — and the whole point of `H4-1`/`H0-3` is that this cluster's
/// tagging is what somebody is about to change. A retag that moved only the
/// interface row would bind, under `--real-jdk`, a helper whose implementation
/// the policy had not been asked about, with no arm able to see the
/// difference.
///
/// **Since 2026-09-22 the coincidence is gone in the other direction**: the
/// resolver answers "would the tail run this native", and for a `Bridge` with
/// no armed dial that is `true`, so all four rows now ADMIT. That makes asking
/// about both rows matter more, not less. The interface row and the
/// implementing row can carry different kinds — `SyntheticStub` on one and
/// `Bridge` on the other is the case that matters — and the row whose native
/// actually runs is the implementing one. Asking only the site's row would
/// then bind a helper standing in for a fabrication.
///
/// So both rows must be approved. Refusing names the implementing class,
/// because that is the registration the refusal is about.
///
/// Costs one extra resolver call, on the strict arm, at compile time, only for
/// a triple whose cell is already non-zero and whose site class differs from
/// its implementing class.
#[inline]
pub(crate) fn direct_native_helper_for_impl(
    entry: usize,
    jdk_only: bool,
    direct_bind_admission: Option<&dyn Fn(&str, &str, &str) -> bool>,
    site_class: &str,
    impl_class: &str,
    method: &str,
    descriptor: &str,
) -> usize {
    // `--compatible` (SYNC22-3, JIT r23 w1 L6): the one question asked there is
    // whether the registry still holds the row whose native the helper runs,
    // and that is the IMPLEMENTING row (an interface row such as
    // `ConcurrentMap.get` is not what the generic tail resolves for the
    // receiver).
    let asked_class = if jdk_only { site_class } else { impl_class };
    let entry = direct_native_helper(
        entry,
        jdk_only,
        direct_bind_admission,
        asked_class,
        method,
        descriptor,
    );
    if entry == 0 || !jdk_only || site_class == impl_class {
        return entry;
    }
    let approved =
        direct_bind_admission.is_some_and(|admits| admits(impl_class, method, descriptor));
    if !approved {
        record_jdk_only_direct_native_refusal(impl_class, method, descriptor);
        return 0;
    }
    entry
}

// ---------------------------------------------------------------------------
// JDK-ONLY-NOTE — native-dispatch sites reachable from compiled code that this
// crate cannot fix, recorded here because they are the JIT's obligations even
// though they live in files this crate does not own. Each one is a path from
// JIT-compiled machine code into a native callback that never consults
// `vm/src/vm/vm_exec.rs::resolve_dispatch` and never calls
// `NativeMethodRegistry::record_invocation`.
//
//  1. `vm/src/jit/helpers.rs::jit_invoke_dispatch` and
//     `jit_invoke_virtual_mic` — the two `JitRuntimeHelpers` entry points every
//     compiled invoke that is not inlined, intrinsified or directly bound goes
//     through. These are THE JIT's dispatch path and must be routed through
//     `resolve_dispatch` (or `resolve_native_dispatch_wave1`, since they decide
//     native-vs-bytecode from `JitInvokeInfo`'s name triple before method
//     resolution) and must call `record_invocation`.
//
//  2. `vm/src/jit/helpers.rs` JIT-only native fast paths, which resolve natives
//     by name entirely on their own: the `java/lang/ClassLoader`
//     `getResource*`-with-null intercept (a bare
//     `natives.native_methods.find(..)` + `safe_native_call`),
//     `hashmap_native_callback`, `matcher_native_callback` and
//     `stringbuilder_native_callback`. Each returns a raw `NativeCallback` with
//     no `NativeKind`, so none of them can honour §1 rule 3 or rule 4, and none
//     is counted. These are exactly the "JIT fast path that resolves natives on
//     its own" the acceptance criterion is aimed at.
//
//  3. `vm/src/jit/helpers.rs::build_helpers` — used to call a
//     `set_jit_execution_policy` latch with `config.execution_policy()` BEFORE
//     the first compilation. **CLOSED 2026-09-12:** the latch was a process
//     global and is deleted; every compile door passes its own VM's policy as
//     the `jdk_only` argument instead, so there is nothing to publish first.
//
//     This item used to continue: "and should skip the `set_*_direct_fn`
//     registrations entirely under `JdkOnly` (belt and braces …)".
//     **WITHDRAWN 2026-08-21 (H20-1), for two independent reasons.**
//
//     (a) It asks for something that was deliberately deleted. The module
//     comment ~300 lines above this one records the `*_DIRECT_FN` cells being
//     registered *unconditionally* as of 2026-08-06, because withholding a
//     process-invariant `fn` address was never per-VM protection — it was a
//     process-wide side effect on every other VM in the process. Re-adding it
//     would re-introduce that bug. An open ask for a closed decision is how
//     `vm/src/jit/helpers.rs` came to carry two contradictory paragraphs about
//     the same registrations for two weeks.
//
//     (b) It would not have covered the doors that need covering. Item 7
//     below: both direct doors took helper addresses as
//     `NAME as *const () as usize`, never reading the cells, so zeroing the
//     cells was invisible to them. Still true of the addresses in 2026-09-22's
//     closure -- `DirectNativeShadow::entry_addr` takes the ITEM's address for
//     the same registration-order reason -- so this stays withdrawn on both
//     counts.
//
//  4. `cp_elidable_init_resolver` (supplied to `try_compile` by
//     `vm/src/runtime/interpreter.rs`) decides whether a `<init>` may be
//     scalar-elided. Eliding a constructor that is shadowed by a registered
//     native skips the native — the `jit-elidable-ctor-must-check-native-shadow`
//     defect. The shadow check is on the VM side and needs the same
//     `resolve_dispatch` treatment.
//
//  5. `INDY_BRIDGE_FN` (below) is an `invokedynamic` *bootstrap*
//     bridge, not a native-method dispatch, and the interpreter reaches the
//     same bridge for the same sites — so gating it in the JIT would move the
//     call without changing the policy answer, while perturbing
//     `CompiledMethod::has_indy_trap` (an OSR-correctness input). Left alone
//     deliberately; the bridge itself is the VM's to review.
//
//  6. `MONITOR_ENTER_DIRECT_FN` / `MONITOR_EXIT_DIRECT_FN` are VM monitor
//     services, not registered natives. Not a dispatch site; no action.
//
//  7. `vm/src/runtime/interpreter/jit_bridge.rs`'s OSR direct-call ladder and
//     `vm/src/runtime/interpreter.rs`'s eager first-call ladder each build
//     `direct_calls` themselves and hand it to `x64::compile_with_param_slots`,
//     so neither reaches [`direct_native_helper`]. Both took helper addresses
//     as `NAME as *const () as usize` rather than reading the `*_DIRECT_FN`
//     cell, so item 3's withdrawn belt-and-braces would not have covered them
//     either. MEASURED 2026-08-20: under `--real-jdk` the OSR door bound
//     `jit_thread_current_thread_direct` (a `bridge` row) and compiled code
//     called it 298 000 times while the MethodEntry door refused all seven
//     sites it examined; `--fast-boot` reported the identical `OSR 1`. See
//     H12-1.
//
//     The largest such paths in the tree, and the reason items 1-6 read as a
//     complete list for three months while they were not.
//
//     **CLOSED 2026-09-22 (H12-1 O1/O2).** Both ladders obtain every
//     shadowing helper's address from
//     `vm/src/jit/helpers.rs::admit_direct_native_entry`, which derives the
//     policy from the VM it is compiling for and answers with the same
//     predicate the callee-side gate uses. It could not be applied
//     downstream: a row removed after the door leaves the pc with neither
//     invoke-info nor a direct-call plan, which `x64/driver.rs`'s
//     `reserve_stack_floor` walk defines as a raw self-call -- so the token
//     rides on the ENTRY ADDRESS and a refusal FALLS THROUGH to the
//     `invoke_info` build. See H20-1 for the argument and
//     [`compile_gate::CompileAdmission::admits_direct_bind`] for the type that
//     carries it.
//
//     The thirty-one helper items are `pub(in crate::jit)` as of the same
//     change, so a ladder outside that module cannot name one:
//     `direct_bind_source_witness` is the standing check that nobody widens
//     one back.
//
//     **What is still open at these two doors, and it is not a dispatch
//     question:** the eight `jdk/internal/misc/Unsafe` scalar accessors are
//     bound raw at BOTH the OSR and the single-pass door. They shadow no
//     bytecode, so §1.4 and §7 step 3 admit them and `--real-jdk` is not the
//     mode with the hole; what they bypass is
//     `check_native_dispatch_capability`, which `classify_native` maps the
//     whole class to `RawMemory` for. Fixing that at one door would be this
//     item's own defect with the modes swapped, so it is tracked as a
//     three-door change:
//     `docs/feature-designs/jit-ir-optimizations.md`.
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// `ByteBuffer.put(int,byte)` / `ByteBuffer.get(int)` thin direct-call binds.
//
// Census-driven, exactly like `Preconditions.checkIndex` above.
// `--dump-native-registry` on `BufProbe` (netty's `ByteBuf.writeByte` /
// `forEachByte` in a loop, the two operations
// `AbstractIntegrationTest.testHugeDecompress` runs 268 million times each)
// reports ONE `java/nio/DirectByteBuffer.put(IB)` per `writeByte` and one
// `get(I)B` per byte read, both `bridge`:
//
//     5 242 880  java/nio/DirectByteBuffer.put(IB)Ljava/nio/ByteBuffer;
//     3 145 728  java/nio/DirectByteBuffer.get(I)B
//
// against a measured 700 ns/op for `writeByte` and 349 ns/op for
// `forEachByte`, where HotSpot is 5.1 ns and 5.9 ns. `perf` puts
// `safe_native_call_impl` + `try_jit_site_cached_native_dispatch` +
// the argument marshalling + `is_object_address` at ~30 % of that profile:
// the native bodies are a bounds check and a one-byte copy, and everything
// else is the funnel around them.
//
// The site's constant-pool class is `java/nio/ByteBuffer` (netty's
// `PooledDirectByteBuf.memory` is declared as one), so the bind uses
// `direct_native_helper_for_impl`: the POLICY question has to be asked about
// `java/nio/DirectByteBuffer`, which is where the native the helper stands in
// for is registered. The helper declines any receiver whose class the funnel
// has not already served with the modelled layout -- including every
// `HeapByteBuffer` -- so a polymorphic site keeps today's behaviour on every
// receiver the fast path was not proven for.
/// Sites bound to the two `ByteBuffer` single-byte helpers at the SINGLE-PASS
/// door.
pub static NIO_BYTE_ELEMENT_SITES: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// The same, at the OSR door — `jit_bridge.rs`'s own callee-binding loop,
/// which is neither of the other two and is the one that compiles a hot LOOP
/// body. Its own comment calls it "the door that mattered", and the heap
/// control probe proved the point in the other direction: with the gate on the
/// other two doors only, this one still bound the site and the helper declined
/// 1 595 000 of 1 595 000 calls.
pub static NIO_BYTE_ELEMENT_SITES_OSR: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

pub fn nio_byte_element_sites_osr() -> u64 {
    NIO_BYTE_ELEMENT_SITES_OSR.load(std::sync::atomic::Ordering::Relaxed)
}

/// The same, at the OPTIMIZING (IR) door — counted apart from the single-pass
/// tally for the reason `census_direct_helper_sites` states about its own pair:
/// a bind at one door says nothing about the other, and this family's whole
/// history is a single-pass bind reading as landed while the tier the hot loop
/// actually runs in had no route to it at all.
pub static NIO_BYTE_ELEMENT_SITES_IR: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Calls the thin helpers actually SERVED, and calls they DECLINED to the
/// generic dispatcher.
///
/// A site count and a served count answer different questions, and the gap
/// between them is where a fast path hides: `ByteBuffer.byteElement=2` with
/// five million funnel invocations still in the census means the bind happened
/// and the helper said no to every call. Timings cannot tell those apart.
///
/// Per-call counters: [`PerThreadEventCounter`], not a shared `AtomicU64` --
/// see that type for why.
pub static NIO_BYTE_ELEMENT_SERVED: PerThreadEventCounter =
    PerThreadEventCounter::with_slot(PER_THREAD_EVENT_NIO_SERVED);
pub static NIO_BYTE_ELEMENT_DECLINED: PerThreadEventCounter =
    PerThreadEventCounter::with_slot(PER_THREAD_EVENT_NIO_DECLINED);
pub static MD_UPDATE_BYTE_SERVED: PerThreadEventCounter =
    PerThreadEventCounter::with_slot(PER_THREAD_EVENT_MD_SERVED);
pub static MD_UPDATE_BYTE_DECLINED: PerThreadEventCounter =
    PerThreadEventCounter::with_slot(PER_THREAD_EVENT_MD_DECLINED);

// ---------------------------------------------------------------------------
// Per-thread engagement counters for the thin direct helpers (round 9 wave 2)
// ---------------------------------------------------------------------------

pub(crate) const PER_THREAD_EVENT_NIO_SERVED: usize = 0;
pub(crate) const PER_THREAD_EVENT_NIO_DECLINED: usize = 1;
pub(crate) const PER_THREAD_EVENT_MD_SERVED: usize = 2;
pub(crate) const PER_THREAD_EVENT_MD_DECLINED: usize = 3;
pub(crate) const PER_THREAD_EVENT_SESSION_SERVED: usize = 4;
pub(crate) const PER_THREAD_EVENT_SESSION_DECLINED: usize = 5;
pub(crate) const PER_THREAD_EVENT_SCOPED_SERVED: usize = 6;
pub(crate) const PER_THREAD_EVENT_SCOPED_DECLINED: usize = 7;
pub(crate) const PER_THREAD_EVENT_UNSAFE_SERVED: usize = 8;
pub(crate) const PER_THREAD_EVENT_UNSAFE_DECLINED: usize = 9;
pub(crate) const PER_THREAD_EVENT_UNSAFE_CAS_SERVED: usize = 10;
pub(crate) const PER_THREAD_EVENT_UNSAFE_CAS_DECLINED: usize = 11;
/// One slot per counter above. A new counter takes the next index and bumps
/// this; `with_slot` rejects an out-of-range index at compile time.
pub(crate) const PER_THREAD_EVENT_SLOTS: usize = 12;

/// A call counter written by its OWN thread only and summed on read -- the
/// jit-crate twin of the VM's `JitCounterBlock` (`vm/src/jit/helpers.rs`).
///
/// The thin direct helpers (`ByteBuffer.get/put(int)`,
/// `MessageDigest.update(byte)`, `Buffer.session()`, the `ScopedMemoryAccess`
/// unaligned accessors) exist to be cheaper than the generic native funnel,
/// and their useful work can be a single byte load. As a process-global
/// `AtomicU64` each served call also paid a `lock xadd` (a full barrier, 4-5 ns
/// uncontended by this repo's `jit_native_dispatch_profile`) on ONE cache line
/// every thread running the helper writes -- so the counter, not the work,
/// capped multi-thread throughput
/// (`direct-helper-engagement-counters-pay-a-shared-lock-xadd-per-call-20260918.md`).
///
/// Here each thread owns a block of [`PER_THREAD_EVENT_SLOTS`] counters. The
/// owner increments with a relaxed load + store (no lock prefix; exact,
/// because nobody else writes the block); a reader sums every live block plus
/// the totals folded in by threads that have exited. Folding and deregistering
/// happen under the registry lock the reader also takes, so an exiting thread
/// is counted exactly once. Reads are rare (census/report points only).
///
/// The API is `fetch_add(n, ordering)` / `load(ordering)` -- the `AtomicU64`
/// shape -- so every increment site kept compiling unchanged. The ordering
/// argument is accepted and ignored: these are statistics, every access is
/// relaxed, and `fetch_add` returns THIS thread's previous count, not a global
/// one (no caller uses the return value).
pub struct PerThreadEventCounter {
    pub(crate) slot: usize,
}

impl PerThreadEventCounter {
    pub(crate) const fn with_slot(slot: usize) -> Self {
        assert!(
            slot < PER_THREAD_EVENT_SLOTS,
            "PerThreadEventCounter slot out of range"
        );
        Self { slot }
    }

    /// Add `n` to this thread's count. See the type doc for the ignored
    /// ordering and the return value.
    #[inline]
    pub fn fetch_add(&self, n: u64, _order: std::sync::atomic::Ordering) -> u64 {
        use std::sync::atomic::Ordering::Relaxed;
        let slot = self.slot;
        PER_THREAD_EVENT_BLOCK
            .try_with(|handle| {
                let cell = &handle.0.counts[slot];
                // Owner-only writer: a load + store pair is exact and needs no
                // lock prefix. Readers see either value, never a torn one.
                let prev = cell.load(Relaxed);
                cell.store(prev.wrapping_add(n), Relaxed);
                prev
            })
            // This thread's block is already torn down (a helper reached
            // during thread exit): count straight into the exited-threads
            // total so the event is still counted, exactly once.
            .unwrap_or_else(|_| PER_THREAD_EVENT_EXITED[slot].fetch_add(n, Relaxed))
    }

    /// The total across every thread, live and exited.
    pub fn load(&self, _order: std::sync::atomic::Ordering) -> u64 {
        use std::sync::atomic::Ordering::Relaxed;
        let blocks = PER_THREAD_EVENT_BLOCKS
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let live: u64 = blocks
            .iter()
            .map(|b| b.counts[self.slot].load(Relaxed))
            .fold(0u64, u64::wrapping_add);
        live.wrapping_add(PER_THREAD_EVENT_EXITED[self.slot].load(Relaxed))
    }
}

impl std::fmt::Debug for PerThreadEventCounter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "PerThreadEventCounter({})",
            self.load(std::sync::atomic::Ordering::Relaxed)
        )
    }
}

pub(crate) struct PerThreadEventBlock {
    pub(crate) counts: [std::sync::atomic::AtomicU64; PER_THREAD_EVENT_SLOTS],
}

/// A thread's registration: registers its block on first use, and on thread
/// exit folds the block into [`PER_THREAD_EVENT_EXITED`] and deregisters it,
/// both under the registry lock.
pub(crate) struct PerThreadEventHandle(pub(crate) Arc<PerThreadEventBlock>);

impl PerThreadEventHandle {
    pub(crate) fn register() -> Self {
        #[allow(clippy::declare_interior_mutable_const)]
        const ZERO: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let block = Arc::new(PerThreadEventBlock {
            counts: [ZERO; PER_THREAD_EVENT_SLOTS],
        });
        PER_THREAD_EVENT_BLOCKS
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(Arc::clone(&block));
        Self(block)
    }
}

impl Drop for PerThreadEventHandle {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering::Relaxed;
        let mut blocks = PER_THREAD_EVENT_BLOCKS
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for (slot, cell) in self.0.counts.iter().enumerate() {
            PER_THREAD_EVENT_EXITED[slot].fetch_add(cell.load(Relaxed), Relaxed);
        }
        blocks.retain(|b| !Arc::ptr_eq(b, &self.0));
    }
}

thread_local! {
    pub(crate) static PER_THREAD_EVENT_BLOCK: PerThreadEventHandle = PerThreadEventHandle::register();
}

/// Every live thread's block. Taken on a thread's FIRST counted event, at its
/// exit, and by readers -- never on the increment path.
pub(crate) static PER_THREAD_EVENT_BLOCKS: std::sync::Mutex<Vec<Arc<PerThreadEventBlock>>> =
    std::sync::Mutex::new(Vec::new());

/// Counts of threads that have exited (and of events counted during a
/// thread's own teardown).
pub(crate) static PER_THREAD_EVENT_EXITED: [std::sync::atomic::AtomicU64; PER_THREAD_EVENT_SLOTS] = {
    #[allow(clippy::declare_interior_mutable_const)]
    const ZERO: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    [ZERO; PER_THREAD_EVENT_SLOTS]
};

/// `(nio served, nio declined, md served, md declined)`.
pub fn byte_element_helper_calls() -> (u64, u64, u64, u64) {
    use std::sync::atomic::Ordering::Relaxed;
    (
        NIO_BYTE_ELEMENT_SERVED.load(Relaxed),
        NIO_BYTE_ELEMENT_DECLINED.load(Relaxed),
        MD_UPDATE_BYTE_SERVED.load(Relaxed),
        MD_UPDATE_BYTE_DECLINED.load(Relaxed),
    )
}

/// Bound `ByteBuffer` single-byte element sites, for a census that can tell
/// "the fast path was never installed" from "it was installed and is no
/// faster" -- the distinction timings alone cannot make.
pub fn nio_byte_element_sites() -> u64 {
    NIO_BYTE_ELEMENT_SITES.load(std::sync::atomic::Ordering::Relaxed)
}

/// The same count for the optimizing door. Reported beside the single-pass one
/// so `single-pass/IR` reads as a pair.
pub fn nio_byte_element_sites_ir() -> u64 {
    NIO_BYTE_ELEMENT_SITES_IR.load(std::sync::atomic::Ordering::Relaxed)
}

/// `CRATONVM_JIT_BUFFER_SESSION_DIRECT=0` — send every `Buffer.session()` site
/// back through the generic native funnel. Default ON.
pub fn buffer_session_direct_enabled() -> bool {
    true
}

/// Sites bound to the `session()` helper, per door, and the calls it served
/// against the calls it declined. Read together: a bind count without a served
/// count cannot tell "installed" from "installed and answering nothing", which
/// is the failure this whole family has hit twice.
pub static BUFFER_SESSION_SITES_SP: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub static BUFFER_SESSION_SITES_IR: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub static BUFFER_SESSION_SITES_OSR: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub static BUFFER_SESSION_SERVED: PerThreadEventCounter =
    PerThreadEventCounter::with_slot(PER_THREAD_EVENT_SESSION_SERVED);
pub static BUFFER_SESSION_DECLINED: PerThreadEventCounter =
    PerThreadEventCounter::with_slot(PER_THREAD_EVENT_SESSION_DECLINED);

/// `(sp, ir, osr, served, declined)` for the `session()` helper.
pub fn buffer_session_census() -> (u64, u64, u64, u64, u64) {
    use std::sync::atomic::Ordering::Relaxed;
    (
        BUFFER_SESSION_SITES_SP.load(Relaxed),
        BUFFER_SESSION_SITES_IR.load(Relaxed),
        BUFFER_SESSION_SITES_OSR.load(Relaxed),
        BUFFER_SESSION_SERVED.load(Relaxed),
        BUFFER_SESSION_DECLINED.load(Relaxed),
    )
}

/// Is this call site `java.nio.Buffer.session()`?
///
/// Matched on `(name, descriptor)` rather than on the receiver class, and that
/// is deliberate. `session()` is `final` on `java/nio/Buffer`, so there is
/// exactly one implementation and every buffer class names it at a call site
/// through its OWN class (`HeapByteBuffer.getInt` emits
/// `invokevirtual HeapByteBuffer.session`). A class-name list here would have
/// to repeat the registration's eleven names and would inherit their known
/// omissions; the receiver check that actually matters is made at run time by
/// the helper, against the classes the shim really served.
pub fn is_buffer_session_site(name: &str, descriptor: &str) -> bool {
    name == "session" && descriptor == "()Ljdk/internal/foreign/MemorySessionImpl;"
}

/// JIT round 17 wave 1, lane nio (NIO2-2 of `jit-r16-nio2-proposals-RETIRED-20261006.md`):
/// `CRATONVM_JIT_REACHABILITY_FENCE_ELIDE` makes the single-pass method-entry
/// and OSR doors emit `Reference.reachabilityFence` as
/// [`JitIntrinsic::ReachabilityFence`], i.e. as no code, instead of the thin
/// `jit_reachability_fence_direct` CALL (`--compatible`) or the bound or
/// spliced empty JDK body (`--real-jdk`, where the native is retired). x86-64
/// only: the aarch64 backend has no arm for the sentinel. Read live (compile
/// time, one matching site at a time), so no cache static.
///
/// Default ON since nio20 wave 2 (lane L2, proposal L2-3; `=0` restores the
/// call). The GC-liveness argument, which is why the default may move:
///
/// * The fence's whole contract (JDK 25 `Reference.reachabilityFence`) is that
///   the referent is strongly reachable at every point BEFORE the call; the
///   body is an empty `return`. A collection observes reachability only at a
///   GC point, and every GC point of a single-pass body is a safepoint with an
///   oop map (or a conservative scan of the whole frame).
/// * The single-pass backend takes every liveness fact from the BYTECODE, and
///   the `aload` (or the expression) that feeds the fence is still in it. A
///   frame-homed reference local stays in every map until it is overwritten
///   (`local_oop_masks` is a forward "holds a reference" dataflow); a
///   register-shared local is dropped from a map only where bytecode liveness
///   says it is dead at that safepoint's pc (`dead_shared_register_oop_locals`),
///   and the feeding load makes it live at every safepoint that can reach the
///   fence -- in a loop, at every safepoint of the loop. An operand-stack
///   referent (`reachabilityFence(foo())`) is mapped while it is on the stack.
/// * Between the feeding load and the fence there is no GC point, so the one
///   map the elision removes -- the one AT the fence -- was owed only because
///   the CALL was a GC point. This is HotSpot's treatment (a liveness
///   constraint, no code) and the optimizing tier's since NIO19-9.
/// * No Java-visible behaviour moves in either mode: no registry row is
///   consulted or bypassed, and the body that no longer runs is empty.
pub fn reachability_fence_elide_enabled() -> bool {
    cfg!(target_arch = "x86_64")
}

/// Is this call site `java.lang.ref.Reference.reachabilityFence(Object)`?
pub fn is_reachability_fence_site(class: &str, name: &str, descriptor: &str) -> bool {
    class == "java/lang/ref/Reference"
        && name == "reachabilityFence"
        && descriptor == "(Ljava/lang/Object;)V"
}

/// JIT round 17 wave 1, lane nio (NIO2-2): `CRATONVM_JIT_OBJECT_GET_CLASS_DIRECT`
/// (default ON since the round-17 close, `=0` off; A/B in
/// jit-optin-switch-triage-r17-CLOSED-20261006.md) binds `invokevirtual Object.getClass()` at all three
/// compile doors to the thin `jit_object_get_class_direct` helper: a null
/// receiver raises the JEP 358 receiver NPE, a plain object whose class has a
/// mirror and a memoised "report the stamped class" display verdict is
/// answered from the mirror table, and every other receiver (an array, an
/// aliased display class, no mirror yet) declines to the generic dispatcher,
/// which runs the registered native. Read live, so no cache static.
pub fn object_get_class_direct_enabled() -> bool {
    true
}

/// W2A (`CRATONVM_JIT_IDENTITY_HASH_DIRECT`, `=0` kills): the optimizing compile door
/// binds `invokestatic System.identityHashCode(Object)` to the thin
/// `jit_identity_hash_code_direct` helper, which mints the hash in place
/// (the registered leaf's own steps, with no dispatch funnel, argument decode
/// or return coercion). The header-read prefix still answers an already
/// hashed object first; this is the call it declines to.
pub fn identity_hash_direct_enabled() -> bool {
    true
}

/// The `System.identityHashCode(Object)I` triple the thin helper serves.
pub fn is_identity_hash_code_site(class: &str, name: &str, descriptor: &str) -> bool {
    class == "java/lang/System"
        && name == "identityHashCode"
        && descriptor == "(Ljava/lang/Object;)I"
}

/// `CRATONVM_JIT_CLASS_GET_NAME_DIRECT` (JIT round 22 wave 2, lane runtime,
/// RUNTIME22-2 of `docs/feature-designs/jit-ir-optimizations.md`;
/// perf, **default ON (flipped; `=0` kills)**): both compile doors bind
/// `invokevirtual java/lang/Class.getName()` to the thin
/// `jit_class_get_name_direct` helper, which answers from the `getName()`
/// memos (`lang_class::class_get_name_fast`: the per-thread mirror line, then
/// the class-id row) with no dispatch funnel and no native door, and declines
/// every miss to the generic dispatcher (the registered native, which fills
/// the memos). Read per site at compile time.
pub fn class_get_name_direct_enabled() -> bool {
    // R22-AB-FLIP: default ON since the JIT r22 opt-in A/B (docs/feature-designs/jit-ir-optimizations.md); `=0` is the kill switch.
    true
}

/// Is `class.name descriptor` the call `Class.getName()`? `java/lang/Class` is
/// final and declares `getName`, so a site naming that triple calls it.
pub fn is_class_get_name_site(class: &str, name: &str, descriptor: &str) -> bool {
    class == "java/lang/Class" && name == "getName" && descriptor == "()Ljava/lang/String;"
}

pub fn is_class_get_superclass_site(class: &str, name: &str, descriptor: &str) -> bool {
    class == "java/lang/Class" && name == "getSuperclass" && descriptor == "()Ljava/lang/Class;"
}

/// nio20 wave 1, lane L3 (NIO19-5, was NIO17-1 / BUF18-1):
/// `CRATONVM_JIT_GETCLASS_INLINE` (default ON, `=0` off) -- the optimizing
/// tier answers `invokevirtual X.getClass()` with two loads (the receiver's
/// class id from its header, then that class's word in the VM's
/// compiled-`getClass()` table, `DirectHelperTable::getclass_mirror_words`)
/// ahead of the call, which stays as the decline edge for a null receiver,
/// an array, an id past the table and a class with no published word. The
/// VM publishes a word only for a class the thin helper or the leaf arm has
/// answered (a stamped display verdict and an existing mirror), and the
/// collector re-keys and withdraws words under the `class_mirrors` lock.
/// Read live (per compile and per VM table build), so no cache static.
pub fn getclass_inline_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_CLASS_IS_INSTANCE_INLINE` (JIT round 20 wave 17, lane pAtom2;
/// perf, **default ON**, `=0` off): `Class.isInstance(obj)` where `obj`'s
/// exact class is the mirror's -- the field updaters' `accessCheck`
/// (`cclass.isInstance(obj)`) on every `get` / `set` / `getAndAdd`. Two halves:
/// the VM's site-cached native arm (`LeafNativeKind::ClassIsInstance`) answers
/// that case without the native and publishes the class's compiled-`getClass()`
/// word (when its `getClass()` reports its own class), and the optimizing
/// tier's prefix (`ir_lower::try_emit_ir_class_is_instance_prefix`) answers it
/// from that word with no call: `obj`'s word equal to the receiver mirror is
/// `true`; everything else (null, an array, a subclass, an unpublished word)
/// is the unchanged call. Needs `CRATONVM_JIT_GETCLASS_INLINE` (the table).
/// Read live (per compile, per site-cache fill and per publish), so no cache
/// static.
pub fn class_is_instance_inline_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_CLASS_IS_ASSIGNABLE_INLINE` (JIT round 20 wave 19, lane
/// zAssign; perf, **default ON**, `=0` off; page
/// `jr20-class-is-assignable-from-takes-the-class-manager-lock-per-call-20261013.md`,
/// probe rows `R20PNatAssignable` / `R20ZAssignRows`): the optimizing tier
/// answers `Class.isAssignableFrom(Class)` with no call when both mirrors map
/// to class ids through the compiled-`getClass()` table's mirror-hint band
/// (`ir_lower::class_mirror_hint_resolve`, validated against the class
/// words) and the source class's supers display decides it
/// (`ir_lower::class_assignable_core`): a hit is `true`, a miss of a
/// hierarchy-only source against a target that is not a `synthetic_implements`
/// target, with the duplicate-class-name latch clear, is `false`; everything
/// else (an array or primitive mirror, an unfilled hint, an interface source,
/// a lambda proxy) is the unchanged call. The VM's half fills the hints from
/// the native's resolved pairs (`NativeClassAccess::note_class_mirror_id`).
/// Needs `CRATONVM_JIT_GETCLASS_INLINE` (the table) and the supers displays.
/// Read per compile, and once per VM by the hint filler.
pub fn class_is_assignable_inline_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_CLASS_IS_INSTANCE_SUBTYPE_INLINE` (JIT round 20 wave 19, lane
/// zAssign; perf, **default ON**, `=0` off): the `Class.isInstance` prefix of
/// `CRATONVM_JIT_CLASS_IS_INSTANCE_INLINE` also answers an object whose class
/// is NOT the receiver's exactly -- a subclass or implementor `true`, a
/// definite miss `false` -- by the same mirror-hint and display test as
/// `CRATONVM_JIT_CLASS_IS_ASSIGNABLE_INLINE` (whose hint filler it shares, so
/// it needs that switch too). Only for an object whose class has a published
/// compiled-`getClass()` word (its `getClass()` reports its own class, so the
/// native's display-class disjunct is the hierarchy). Read per compile.
pub fn class_is_instance_subtype_inline_enabled() -> bool {
    true
}

/// JIT round 20 wave 12, lane qMisc (NIO20-L3-1 of
/// `docs/feature-designs/jit-ir-optimizations.md`, the sound
/// route): `CRATONVM_JIT_IR_GETCLASS_EXACT_FOLD` (perf, **default ON** again
/// since 2026-10-26; `=0` keeps every `x.getClass() == C.class` on its call
/// and `ldc`). It was opt-in after the w12a host A/B measured the ON arm ~4x
/// slower; the fold has since noted `ScalarIntrinsic` acceptance evidence, and
/// the 2026-10-26 re-measure (`R21AcceptGetClassFold`, 5 interleaved reps,
/// both modes) found the folded body published (fold ~= fold + accept-always)
/// and faster: ne-kernel 36 vs 190 ns, long OSR loop 3310 vs 4769 ns, other
/// rows unchanged (`docs/feature-designs/jit-ir-optimizations.md`). The
/// optimizing tier builds the pair as `Guard(x != null)` (REEXECUTE at the
/// `invokevirtual`, whose interpreter raises the NPE) and
/// `ExactClassIs(x, C)` when the VM states that `getClass()` reports `C`
/// exactly for an object whose header names `C`
/// (`CompileRequest::getclass_exact_fold_resolver`). Read per compile.
pub fn ir_getclass_exact_fold_enabled() -> bool {
    true
}

/// JIT r21 wave 2, lane typecheck (TYPECHECK21-1 part A of
/// `docs/feature-designs/jit-ir-optimizations.md`):
/// `CRATONVM_JIT_IR_INSTANCEOF_HEADER_READ` (perf, **opt-in**): the optimizing
/// tier builds its `instanceof` as a header-only `Op::InstanceOf`
/// (`ir::MemAccess::HeaderRead`): a read of immutable header words that writes
/// nothing, instead of an opaque helper call. Read per site at compile time.
pub fn ir_instanceof_header_read_enabled() -> bool {
    false
}

/// JIT r21 wave 2, lane typecheck (TYPECHECK21-1 part A):
/// `CRATONVM_JIT_IR_INSTANCEOF_OFF_CHAIN` (perf, **opt-in**, needs
/// [`ir_instanceof_header_read_enabled`]): the header-only `Op::InstanceOf`
/// also stops producing a memory token (`self.mem` is not advanced), like
/// `Op::ArrayLength`, so no chain walk ever reaches it. Read per site.
pub fn ir_instanceof_off_chain_enabled() -> bool {
    false
}


/// JIT r21 wave 1, lane typecheck (TYPECHECK21-3):
/// `CRATONVM_JIT_TYPECHECK_ARRAY_TARGETS` (perf, **default ON (flipped; `=0` kills)**): the optimizing
/// tier's `instanceof` / `checkcast` against a one-dimensional reference
/// array type `[LT;` answer a reference-array receiver whose component is `T`
/// or a proven subtype of `T` inline (`ir_lower::typecheck_array_hit_bytes`).
/// Read per site at compile time.
pub fn typecheck_array_targets_enabled() -> bool {
    // R22-AB-FLIP: default ON since the JIT r22 opt-in A/B (docs/feature-designs/jit-ir-optimizations.md); `=0` is the kill switch.
    true
}

/// The `x.getClass() == C.class` / `!=` pairs of the compiling method the
/// optimizing tier may fold ([`ir_getclass_exact_fold_enabled`]): the pc of
/// each `invokevirtual Object.getClass()` immediately followed by an
/// `ldc`/`ldc_w` of a `CONSTANT_Class` and an `if_acmpeq`/`if_acmpne`,
/// mapped to the class id the VM resolved that constant to, for exactly the
/// classes `fold` admits (loaded, accessible from the holder -- the
/// `cp_new_resolver` route `method_monitor_class_cps` uses -- not an array,
/// and reported by `getClass()` exactly when the header names it). The
/// builder re-checks the shape at the pc it walks
/// (`ir::IrBuilder::try_fold_getclass_exact_compare`), so a pc here that is
/// not an instruction boundary is never consulted. JIT round 20 wave 12,
/// lane qMisc.
#[allow(clippy::too_many_arguments)]
pub(crate) fn ir_getclass_exact_compares(
    code: &[u8],
    code_len: usize,
    ldc_ops: &[(usize, u16)],
    ldc: &dyn Fn(u16) -> Option<JitLdcConstant>,
    new_site: &dyn Fn(u16) -> Option<JitNewSite>,
    invoke: &dyn Fn(u16) -> Option<(String, String, String)>,
    declaring: &dyn Fn(u16) -> Option<String>,
    class_name: &dyn Fn(u16) -> Option<String>,
    fold: &dyn Fn(u32) -> bool,
) -> std::collections::HashMap<usize, u32> {
    let mut out = std::collections::HashMap::new();
    let len = code_len.min(code.len());
    for &(pc, cp_idx) in ldc_ops {
        let width = match code.get(pc).copied() {
            Some(0x12) => 2,
            Some(0x13) => 3,
            _ => continue,
        };
        let acmp = pc + width;
        if pc < 3 || acmp + 2 >= len || !matches!(code[acmp], 0xa5 | 0xa6) {
            continue;
        }
        let invoke_pc = pc - 3;
        if code[invoke_pc] != 0xb6 {
            continue;
        }
        let invoke_cp = u16::from_be_bytes([code[invoke_pc + 1], code[invoke_pc + 2]]);
        let is_get_class = invoke(invoke_cp)
            .is_some_and(|(_, name, descriptor)| is_object_get_class_site(&name, &descriptor));
        // JIT r20 lane rev12 (correctness, unconditional): the name and
        // descriptor alone do not make the call `Object.getClass`. A class may
        // declare a PRIVATE (or static) `getClass()Ljava/lang/Class;` -- the
        // final-override check skips private and static methods -- and an
        // `invokevirtual` of its own class then selects that method (JDK 11+
        // nestmate private invokevirtual) or raises ICCE (static). Fold only
        // when the site resolves to the method `java/lang/Object` declares.
        if !is_get_class || declaring(invoke_cp).as_deref() != Some("java/lang/Object") {
            continue;
        }
        let is_class_constant = matches!(
            ldc(cp_idx),
            Some(JitLdcConstant::ClassMirror { .. } | JitLdcConstant::ClassMirrorSlot { .. })
        );
        if !is_class_constant || class_name(cp_idx).is_none_or(|n| n.starts_with('[')) {
            continue;
        }
        let Some(JitNewSite::Resolved { class_id, .. }) = new_site(cp_idx) else {
            continue;
        };
        if class_id != 0 && fold(class_id) {
            out.insert(invoke_pc, class_id);
        }
    }
    out
}


/// nio20 wave 3, lane L3 (W2L2-4): `CRATONVM_JIT_IR_THIN_PREFIX` (perf,
/// **default ON**; `0` keeps the optimizing tier's `getClass()` and `Unsafe`
/// accessor sites on their thin-helper calls). The optimizing half of those
/// prefixes only, so a bisect can isolate one tier on one binary; the
/// single-pass half is `CRATONVM_JIT_SP_THIN_PREFIX`, and
/// `CRATONVM_JIT_GETCLASS_INLINE` / `CRATONVM_JIT_UNSAFE_ACCESSOR_INLINE`
/// still turn off both halves together. Read per site compile.
pub(crate) fn ir_thin_prefix_enabled() -> bool {
    true
}

/// Is this call site `Object.getClass()`?
///
/// Matched on `(name, descriptor)` and the virtual kind only: `getClass` is
/// `final` on `java/lang/Object`, so no class can declare another
/// `getClass()Ljava/lang/Class;` (the verifier rejects the override) and an
/// interface may not declare one either (the JLS forbids an interface method
/// override-equivalent to a `final` method of `Object`), which makes every
/// `invokevirtual X.getClass()Ljava/lang/Class;` resolve to `Object.getClass`.
pub fn is_object_get_class_site(name: &str, descriptor: &str) -> bool {
    name == "getClass" && descriptor == "()Ljava/lang/Class;"
}

/// JIT round 20 (lane qMisc, correctness;
/// `docs/feature-designs/jit-ir-optimizations.md`):
/// is the call site at constant-pool index `cp_idx` `Object.getClass()` --
/// named `getClass()Ljava/lang/Class;` ([`is_object_get_class_site`]) AND
/// resolved to the method `java/lang/Object` declares (`declaring`,
/// `CompileRequest::cp_invoke_declaring_class_resolver`)? The name alone is
/// not enough: the final-override check skips private and static methods, so
/// a class may declare its own private (or static) `getClass()`, which an
/// `invokevirtual` of that class then selects (JDK 11+) or rejects with
/// `IncompatibleClassChangeError` (static). `false` with no resolver or an
/// unresolved site: such a site keeps its call.
pub fn object_get_class_site_resolved(
    name: &str,
    descriptor: &str,
    declaring: Option<&dyn Fn(u16) -> Option<String>>,
    cp_idx: u16,
) -> bool {
    is_object_get_class_site(name, descriptor)
        && declaring
            .and_then(|r| r(cp_idx))
            .is_some_and(|owner| owner == "java/lang/Object")
}

/// Resolves `Object.notify()V` / `Object.notifyAll()V` via the site's
/// constant-pool declaring class so private/static edge cases stay clean.
pub fn object_notify_direct_helper(
    direct_helpers: &DirectHelperTable,
    jdk_only: bool,
    direct_bind_admission: Option<&dyn Fn(&str, &str, &str) -> bool>,
    name: &str,
    descriptor: &str,
    declaring: Option<&dyn Fn(u16) -> Option<String>>,
    cp_idx: u16,
) -> usize {
    let cell = match (name, descriptor) {
        ("notify", "()V") => direct_helpers.object_notify,
        ("notifyAll", "()V") => direct_helpers.object_notify_all,
        _ => return 0,
    };
    if !declaring
        .and_then(|r| r(cp_idx))
        .is_some_and(|owner| owner == "java/lang/Object")
    {
        return 0;
    }
    direct_native_helper(
        cell,
        jdk_only,
        direct_bind_admission,
        "java/lang/Object",
        name,
        descriptor,
    )
}

/// Resolves `System.currentTimeMillis()J` / `System.nanoTime()J` thin helpers.
pub fn system_clock_direct_helper(
    direct_helpers: &DirectHelperTable,
    jdk_only: bool,
    direct_bind_admission: Option<&dyn Fn(&str, &str, &str) -> bool>,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> usize {
    if class_name != "java/lang/System" || descriptor != "()J" {
        return 0;
    }
    let cell = match method_name {
        "currentTimeMillis" => direct_helpers.system_current_time_millis,
        "nanoTime" => direct_helpers.system_nano_time,
        _ => return 0,
    };
    direct_native_helper(
        cell,
        jdk_only,
        direct_bind_admission,
        class_name,
        method_name,
        descriptor,
    )
}

/// `CRATONVM_JIT_SCOPED_MEMORY_DIRECT=0` — send every `ScopedMemoryAccess` unaligned
/// site back through the generic native funnel. Default ON.
pub fn scoped_memory_direct_enabled() -> bool {
    true
}

/// Round 16 wave 17 (lane mega5, NIO2-1 of `jit-r16-nio2-proposals-RETIRED-20261006.md`):
/// `CRATONVM_JIT_SCOPED_MEMORY_BIND_ADMISSION` (**default ON** since the w17a A/B, `=0` off; a
/// correctness-neutral speedup: directInts 1700 -> 427 ns) asks the VM's bind-time policy
/// (`CompileRequest::direct_bind_admission`, i.e.
/// `helpers::direct_native_bind_admitted`) before the method-entry
/// single-pass and optimizing doors bind a `ScopedMemoryAccess.*Unaligned`
/// site to its thin helper -- what the OSR door already does through
/// `admit_direct_native_entry`, and what the `Unsafe` accessor binds beside
/// these already do.
///
/// Why it matters: the helper's per-call gate (`jit_direct_helper_refused`)
/// asks the SAME policy on every call, and under `--real-jdk` (the default)
/// it refuses every one of these sixteen triples, which have no registered
/// row (`direct_helper_refusal`: "No registration ... `None => true`"). So a
/// site the two doors bound unconditionally declined on EVERY call into
/// `jit_invoke_dispatch` with the helper's static `JitInvokeInfo`
/// (`invoke_kind` 0, no declaring class): the blind virtual arm, and until its
/// per-thread memo filled, the by-name `jit_dispatch_virtual_tail` --
/// `R16Nio2Census` `directbb`'s 1 808 / 1 501 `getIntUnaligned` /
/// `getLongUnaligned` tail dispatches. With the bind refused the site keeps
/// the ordinary ladder: the `final`-devirtualised direct bind or splice of the
/// real bytecode, which is what the tail ran anyway. `--compatible` is
/// unchanged (`direct_native_bind_admitted` answers `true` there for an
/// unclassified triple).
pub fn scoped_memory_bind_admission_enabled() -> bool {
    true
}

/// May a compile door bind the `ScopedMemoryAccess` thin helper at a site
/// naming `(class, method, descriptor)`? Always `true` with
/// [`scoped_memory_bind_admission_enabled`] off (the old unconditional bind).
/// With it on, the VM's `direct_bind_admission` answers (`true` when the
/// request carries none: a hand-built request keeps the old bind).
/// `CRATONVM_DBG_JITC=1` names each refusal.
pub fn scoped_memory_bind_admitted(
    direct_bind_admission: Option<&dyn Fn(&str, &str, &str) -> bool>,
    class: &str,
    method: &str,
    descriptor: &str,
) -> bool {
    if !scoped_memory_bind_admission_enabled() {
        return true;
    }
    let admitted = direct_bind_admission.map_or(true, |admits| admits(class, method, descriptor));
    if !admitted && crate::x64::dbg_jitc_enabled() {
        eprintln!(
            "[cratonvm-jitc] scoped-memory-bind REFUSED {class}.{method}{descriptor}: the \
             helper's per-call gate would decline every call; the site keeps its bytecode bind"
        );
    }
    admitted
}

/// Sites bound to the `ScopedMemoryAccess` unaligned helpers, per door, and the calls served vs declined.
pub static SCOPED_MEMORY_SITES_SP: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub static SCOPED_MEMORY_SITES_IR: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub static SCOPED_MEMORY_SITES_OSR: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub static SCOPED_MEMORY_SERVED: PerThreadEventCounter =
    PerThreadEventCounter::with_slot(PER_THREAD_EVENT_SCOPED_SERVED);
pub static SCOPED_MEMORY_DECLINED: PerThreadEventCounter =
    PerThreadEventCounter::with_slot(PER_THREAD_EVENT_SCOPED_DECLINED);

/// `(sp, ir, osr, served, declined)` for the `ScopedMemoryAccess` helpers.
pub fn scoped_memory_census() -> (u64, u64, u64, u64, u64) {
    use std::sync::atomic::Ordering::Relaxed;
    (
        SCOPED_MEMORY_SITES_SP.load(Relaxed),
        SCOPED_MEMORY_SITES_IR.load(Relaxed),
        SCOPED_MEMORY_SITES_OSR.load(Relaxed),
        SCOPED_MEMORY_SERVED.load(Relaxed),
        SCOPED_MEMORY_DECLINED.load(Relaxed),
    )
}

/// `CRATONVM_JIT_UNSAFE_ACCESSOR_DIRECT=0` — send every plain
/// `jdk/internal/misc/Unsafe` scalar accessor site back through the generic
/// native funnel. Default ON.
pub fn unsafe_accessor_direct_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_UNSAFE_REFERENCE_DIRECT=0` -- send every `Unsafe.getReference` /
/// `getReferenceVolatile` site back through the generic native funnel (SBR round
/// 2, wave 8, lane `tlocal`). Default ON. Independent of the scalar accessors'
/// switch, so the reference read can be A/B'd on its own.
pub fn unsafe_reference_direct_enabled() -> bool {
    true
}

/// Sites bound to the `Unsafe` accessor helpers, per door, and the calls served vs declined.
pub static UNSAFE_ACCESSOR_SITES_SP: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub static UNSAFE_ACCESSOR_SITES_IR: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub static UNSAFE_ACCESSOR_SITES_OSR: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub static UNSAFE_ACCESSOR_SERVED: PerThreadEventCounter =
    PerThreadEventCounter::with_slot(PER_THREAD_EVENT_UNSAFE_SERVED);
pub static UNSAFE_ACCESSOR_DECLINED: PerThreadEventCounter =
    PerThreadEventCounter::with_slot(PER_THREAD_EVENT_UNSAFE_DECLINED);

/// `(sp, ir, osr, served, declined)` for the `Unsafe` accessor helpers.
pub fn unsafe_accessor_census() -> (u64, u64, u64, u64, u64) {
    use std::sync::atomic::Ordering::Relaxed;
    (
        UNSAFE_ACCESSOR_SITES_SP.load(Relaxed),
        UNSAFE_ACCESSOR_SITES_IR.load(Relaxed),
        UNSAFE_ACCESSOR_SITES_OSR.load(Relaxed),
        UNSAFE_ACCESSOR_SERVED.load(Relaxed),
        UNSAFE_ACCESSOR_DECLINED.load(Relaxed),
    )
}

/// `CRATONVM_JIT_UNSAFE_CAS_DIRECT=0` — send every compiled
/// `Unsafe.compareAndSet{Int,Long}(Object,long,X,X)Z` site back through the
/// generic native funnel. Default ON.
///
/// Its own switch rather than `CRATONVM_JIT_UNSAFE_ACCESSOR_DIRECT`'s: the CAS
/// is the hot one (every AQS acquire and release), and a regression bisected
/// to this bind has to be priced on ONE binary without also moving the eight
/// accessors.
pub fn unsafe_cas_direct_enabled() -> bool {
    true
}

/// Sites bound to the `Unsafe` compare-and-set helpers, per door, and the
/// calls served vs declined. Kept apart from the accessor family's counters so
/// a CAS reading cannot hide inside, or be hidden by, eight other methods.
pub static UNSAFE_CAS_SITES_SP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static UNSAFE_CAS_SITES_IR: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static UNSAFE_CAS_SITES_OSR: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub static UNSAFE_CAS_SERVED: PerThreadEventCounter =
    PerThreadEventCounter::with_slot(PER_THREAD_EVENT_UNSAFE_CAS_SERVED);
pub static UNSAFE_CAS_DECLINED: PerThreadEventCounter =
    PerThreadEventCounter::with_slot(PER_THREAD_EVENT_UNSAFE_CAS_DECLINED);

/// `(sp, ir, osr, served, declined)` for the `Unsafe` compare-and-set helpers.
pub fn unsafe_cas_census() -> (u64, u64, u64, u64, u64) {
    use std::sync::atomic::Ordering::Relaxed;
    (
        UNSAFE_CAS_SITES_SP.load(Relaxed),
        UNSAFE_CAS_SITES_IR.load(Relaxed),
        UNSAFE_CAS_SITES_OSR.load(Relaxed),
        UNSAFE_CAS_SERVED.load(Relaxed),
        UNSAFE_CAS_DECLINED.load(Relaxed),
    )
}

/// Sites where a `ByteBuffer` element bind was refused because the site's own
/// profile says its receiver is one the helper cannot serve.
pub static NIO_BYTE_ELEMENT_SITES_REFUSED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

pub fn nio_byte_element_sites_refused() -> u64 {
    NIO_BYTE_ELEMENT_SITES_REFUSED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Should this `ByteBuffer.get(int)` / `put(int,byte)` site be bound to the
/// thin element helper at all?
///
/// # The question this exists to ask
///
/// The helper reads a DIRECT buffer's off-heap `address`; for a
/// `HeapByteBuffer` receiver it declines, and a declined call is strictly
/// worse than no bind — it pays the helper crossing, the decline, and then the
/// generic funnel, where an unbound site would have taken the inline cache
/// straight to the compiled `HeapByteBuffer.get`. Measured on
/// `probes/HeapBufferShapeControlProbe.java`, a heap-only site: 190 ns bound
/// against 20 ns unbound, with the helper census reading
/// `served=0 declined=1195000` — a bind that was pure loss on every call.
///
/// # Why it refuses only on POSITIVE evidence
///
/// `None` — no profile, or no receiver holding 80% of the site — binds, which
/// is the behaviour every door had before this gate existed. The gate is here
/// to remove a bind that is provably a loss, not to require proof before
/// making one: `class_is_served` is a table the FUNNEL fills in, so it is
/// empty until the interpreter has run the site, and a "prove it first" rule
/// would refuse every bind on a method compiled early and never revisit it.
///
/// Both inputs are observed rather than assumed. `dominant_receiver` is the
/// same profile the IR planner seeds its MIC from, and `class_is_served` is
/// the same table the helper's own prologue consults before it agrees to
/// answer — so planner and helper cannot disagree about who gets served.
pub fn nio_byte_element_bind_refused(
    direct_helpers: &DirectHelperTable,
    profile: Option<&profile::MethodProfile>,
    pc: usize,
    write: bool,
    door: &str,
) -> bool {
    let dominant = profile
        .and_then(|prof| prof.receivers.get(&pc))
        .and_then(|counts| profile::dominant_receiver(counts, 80));
    let served =
        dominant.is_some_and(|d| direct_helpers.nio_byte_element_class_is_served(d, write));
    // `CRATONVM_DBG_JITC=1` — the three inputs of this decision, per door.
    // Added because the counters alone cannot tell "no profile yet" from
    // "profiled and served": both read as a bind, and only one of them is
    // right. The heap control probe bound at the single-pass door and refused
    // at the IR door in the same process, which is only explicable by reading
    // these.
    if crate::x64::dbg_jitc_enabled() {
        eprintln!(
            "[cratonvm-jitc] nio-byte-bind door={door} pc={pc} write={write} \
             profile={} dominant={dominant:?} served={served}",
            profile.is_some()
        );
    }
    if dominant.is_none() || served {
        return false;
    }
    NIO_BYTE_ELEMENT_SITES_REFUSED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    true
}

/// Sites bound to [`MD_UPDATE_BYTE_DIRECT_FN`].
pub static MD_UPDATE_BYTE_SITES: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Bound `MessageDigest.update(byte)` sites — the same "was it ever installed?"
/// census [`nio_byte_element_sites`] exists for.
pub fn md_update_byte_sites() -> u64 {
    MD_UPDATE_BYTE_SITES.load(std::sync::atomic::Ordering::Relaxed)
}

/// `CRATONVM_JIT_MD_UPDATE_DIRECT_HELPER=0` — send every
/// `MessageDigest.update(byte)` back through the generic native funnel.
/// Default ON; the kill switch and B arm, as beside.
pub fn md_update_direct_helper_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_NIO_BYTE_DIRECT_HELPERS=0` — send every `ByteBuffer`
/// single-byte element access back through the generic native funnel. Default
/// ON, so a KILL SWITCH and the B arm of a one-binary A/B, for the same reason
/// `census-direct-helpers` and `long-box-direct-helpers` beside it have one.
pub fn nio_byte_direct_helpers_enabled() -> bool {
    true
}

/// Call sites bound to one of the three COLLECTION thin direct helpers by the
/// **single-pass** ladder. The OSR door's `HashMap` binds are counted
/// separately, by [`HASHMAP_GET_DIRECT_SITES_OSR`].
///
/// # H7-1: why a counter, and why this one
///
/// `H4-1` §1c and `H0-3` are about VM-side writers of a container's state that
/// the native registry cannot see. These three binds put a *compiled* one in
/// that population: after them, a `HashMap.get` in a tiered-up method and the
/// same `HashMap.get` interpreted reach the map's state by two different
/// routes. Nothing in the arms could say whether a given run had any such site
/// at all — `--real-jdk-report` marks these registry rows'
/// [`invocations`] incomplete (`DIRECT_CALL_HELPER_NATIVES` in
/// `vm/src/jit/helpers.rs`) but "incomplete" is a static claim about wiring,
/// not a count of what this run bound.
///
/// So this is the same instrument, and the same lesson, as `LEAF_NATIVE_HITS`:
/// **timings cannot tell "the fast path was never installed" from "it was
/// installed and is no faster"** — and a strict-mode run cannot tell "the gate
/// refused" from "no compile ever reached the site". Read beside
/// [`jdk_only_direct_native_refusals`], the two together answer both:
///
/// | sites | refusals | reading |
/// |---|---|---|
/// | 0 | 0 | no compiled site ever saw one of these calls — the run says NOTHING about the gate |
/// | 0 | >0 | the gate fired; strict mode did not bind |
/// | >0 | 0 | the gate admitted the bind (the ordinary shape in BOTH modes since 2026-09-22) |
/// | >0 | >0 | two VMs in one process, or a policy that changed between compiles |
///
/// **`sites` means both doors, and until 2026-09-22 it could only mean one.**
/// `H12-1` N2: the OSR door bound `HashMap.get`/`put` with no counter at all,
/// so `H7-1` §6c's prediction that `collection_direct_helper_sites()` is
/// `(0,0,0)` under `--real-jdk` was confirmed by a measurement blind to the
/// door the binds were happening at. Read this beside
/// [`collection_direct_helper_sites_osr`] or the row you are in is a guess.
///
/// **Row 2 and row 3 changed meaning on 2026-09-22.** Row 2 used to read "the
/// gate fired; strict mode is taking the interpreter's route", and row 3 used
/// to read "compiled code holds a second entry into the map's state (expected
/// under `--fast-boot`)". Both sentences assumed a refusal sent the call to
/// bytecode. For a REGISTERED `Bridge` it did not: the refusal routed to
/// `vm_exec::invoke_or_native`, whose §7 step-3 input is structurally `false`
/// for a `Bridge`, so the tail ran the registered native and the refusal only
/// chose how long the trip took. See
/// `docs/feature-designs/jit-ir-optimizations.md`.
///
/// **These three triples are not that case, and an earlier version of this
/// paragraph said they were.** It read: *"`HashMap.get`, `HashMap.put` and
/// `ConcurrentMap.get` are all `bridge`, so all three are admitted under strict
/// now and row 3 is the expected shape in both modes."* MEASURED 2026-09-22,
/// `--dump-native-registry` on a real-JDK VM, agreeing to the row with the
/// checked-in `scripts/baselines/jdk-only-kind-map-25-linux.tsv`:
///
/// ```text
///                                      --compatible    --real-jdk
///   java/util/HashMap.get              synthetic-stub  UNREGISTERED
///   java/util/HashMap.put              synthetic-stub  UNREGISTERED
///   java/util/concurrent/
///     ConcurrentHashMap.get            synthetic-stub  UNREGISTERED
///   java/util/Map.get                  bridge          (interface row)
///   java/util/concurrent/
///     ConcurrentMap.get                bridge          (interface row)
/// ```
///
/// The `bridge` kind belongs to the INTERFACE rows a call site names; the
/// IMPLEMENTING rows these helpers run are `synthetic-stub`, and under
/// `--real-jdk` the overlay is not registered at all. So **row 2 is the strict
/// shape** for all three — `sites == 0`, `refusals > 0` — and row 3 is the
/// compatible shape. `H12-1` O1 made that true at the OSR door too, where the
/// bind used to be unconditional. This is H7-1 §4a's finding with the kinds
/// measured rather than assumed, and it is stronger than that page states: a
/// guard asking about the interface row asks about a `bridge` while the helper
/// runs a `synthetic-stub`, which §1.3 says may not be invoked at all.
///
/// Relaxed adds on a cold compile path; nothing here is on a hot path.
pub static HASHMAP_GET_DIRECT_SITES: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub static HASHMAP_PUT_DIRECT_SITES: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub static CONCURRENT_HASHMAP_GET_DIRECT_SITES: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// The OSR door's copy of the two `HashMap` counters, incremented by the VM
/// crate's `jit_bridge::compile_osr_artifact`.
///
/// `H12-1` N2, and it is the same lesson as
/// [`THREAD_CURRENT_THREAD_SITES_OSR`] one screen down: the bind that is easy
/// to forget is the one at the door that carries its own ladder, and a
/// prediction checked against counters that cannot see that door is confirmed
/// by construction. There is no `ConcurrentHashMap` twin because the OSR
/// ladder does not recognise that site — if it ever does, it needs a third
/// static here in the same change.
pub static HASHMAP_GET_DIRECT_SITES_OSR: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub static HASHMAP_PUT_DIRECT_SITES_OSR: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// `(HashMap.get, HashMap.put, ConcurrentMap.get)` sites bound to a collection
/// thin direct helper since process start. See
/// [`HASHMAP_GET_DIRECT_SITES`] for how to read it.
pub fn collection_direct_helper_sites() -> (u64, u64, u64) {
    (
        HASHMAP_GET_DIRECT_SITES.load(std::sync::atomic::Ordering::Relaxed),
        HASHMAP_PUT_DIRECT_SITES.load(std::sync::atomic::Ordering::Relaxed),
        CONCURRENT_HASHMAP_GET_DIRECT_SITES.load(std::sync::atomic::Ordering::Relaxed),
    )
}

/// `(HashMap.get, HashMap.put)` sites bound to a collection thin direct helper
/// by the **OSR** door since process start. See
/// [`HASHMAP_GET_DIRECT_SITES_OSR`] for why this exists separately, and
/// [`HASHMAP_GET_DIRECT_SITES`] for how to read the pair.
pub fn collection_direct_helper_sites_osr() -> (u64, u64) {
    (
        HASHMAP_GET_DIRECT_SITES_OSR.load(std::sync::atomic::Ordering::Relaxed),
        HASHMAP_PUT_DIRECT_SITES_OSR.load(std::sync::atomic::Ordering::Relaxed),
    )
}

/// `CRATONVM_JIT_INT_VALUE_DIRECT` — default-ON, `=0` opts out. The kill switch
/// for the `Integer.intValue` / `Long.longValue` binds at BOTH doors; see
/// [`INTEGER_INT_VALUE_DIRECT_SITES`].
pub fn int_value_direct_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_TL_GET_DIRECT` — default-ON, `=0` opts out (SBR round 2, wave 8,
/// lane `tlocal`). The kill switch for the `ThreadLocal.get()` thin direct bind
/// at every door; the helper itself additionally declines under `--compatible`
/// and whenever the real-field `ThreadLocal` natives are not the registered ones.
pub fn thread_local_get_direct_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_SPLICE_TL_THIN` — default-ON, `=0` opts out (JIT round 21,
/// lane tl2, `docs/feature-designs/native-dispatch-architecture.md`).
/// A `ThreadLocal.get()` / `set(Object)` call left in an IR-SPLICED body binds
/// the same thin helper the compiling method's own sites bind (the
/// `spliced_thin_helper` closure); `=0` leaves it on the inline-cache cascade,
/// whose miss path is the generic native funnel, as before. Read per compile.
pub fn ir_splice_tl_thin_enabled() -> bool {
    true
}

/// Is `cn.mn desc` one of the two `ThreadLocal` thin-helper rows
/// (`get()` / `set(Object)` on `ThreadLocal` or `InheritableThreadLocal`)?
pub(crate) fn thread_local_thin_row(cn: &str, mn: &str, desc: &str) -> bool {
    (cn == "java/lang/ThreadLocal" || cn == "java/lang/InheritableThreadLocal")
        && ((mn == "get" && desc == "()Ljava/lang/Object;")
            || (mn == "set" && desc == "(Ljava/lang/Object;)V"))
}

/// The thin-helper cell a spliced body's surviving `ThreadLocal` call binds
/// (JIT round 21, lane tl2), or `None` (not a row, or
/// `CRATONVM_JIT_IR_SPLICE_TL_THIN=0`). The policy questions (`--real-jdk`,
/// `CRATONVM_JIT_TL_GET_DIRECT`, the VM's bind admission) are the caller's.
pub(crate) fn spliced_thread_local_thin_cell(
    direct_helpers: &DirectHelperTable,
    cn: &str,
    mn: &str,
    desc: &str,
) -> Option<usize> {
    if !thread_local_thin_row(cn, mn, desc) || !ir_splice_tl_thin_enabled() {
        return None;
    }
    Some(if mn == "get" {
        direct_helpers.thread_local_get
    } else {
        direct_helpers.thread_local_set
    })
}

/// Compile-time engagement counter for the `ThreadLocal.get()` bind: call sites
/// any door bound. Zero beside a `ThreadLocal.get`-heavy run means the bind never
/// fired, which a timing alone cannot tell from "bound and no faster".
pub static THREAD_LOCAL_GET_DIRECT_SITES: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Read [`THREAD_LOCAL_GET_DIRECT_SITES`].
pub fn thread_local_get_direct_sites() -> u64 {
    THREAD_LOCAL_GET_DIRECT_SITES.load(std::sync::atomic::Ordering::Relaxed)
}

/// Count one `ThreadLocal.get()` direct bind.
pub fn note_thread_local_get_direct_site() {
    THREAD_LOCAL_GET_DIRECT_SITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// Compile-time engagement counter for the `Integer.intValue` bind — how many
/// call sites any of the three doors actually bound. Read by
/// `CRATONVM_DBG_DIRECT_BINDS=1`; see `vm::runtime::interp_census`.
pub static INTEGER_INT_VALUE_DIRECT_SITES: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// `Long.longValue()` sibling of [`INTEGER_INT_VALUE_DIRECT_SITES`].
///
/// Separate because the two binds answer separate questions and one counter
/// covering both cannot be read: the composition workload binds two `intValue`
/// sites and one `longValue` site, and a single `sites_bound=3` says which of
/// the two recognitions fired only if you already know.
pub static LONG_LONG_VALUE_DIRECT_SITES: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Read [`INTEGER_INT_VALUE_DIRECT_SITES`].
pub fn integer_int_value_direct_sites() -> u64 {
    INTEGER_INT_VALUE_DIRECT_SITES.load(std::sync::atomic::Ordering::Relaxed)
}

/// Read [`LONG_LONG_VALUE_DIRECT_SITES`].
pub fn long_long_value_direct_sites() -> u64 {
    LONG_LONG_VALUE_DIRECT_SITES.load(std::sync::atomic::Ordering::Relaxed)
}

/// Count one bound `Integer.intValue` site. Called by the two doors in the
/// `vm` crate, which cannot name the static across the dependency edge in a
/// `static` initialiser but can call this.
pub fn note_integer_int_value_direct_site() {
    INTEGER_INT_VALUE_DIRECT_SITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// Count one bound `Long.longValue` site.
pub fn note_long_long_value_direct_site() {
    LONG_LONG_VALUE_DIRECT_SITES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// `CRATONVM_JIT_LONG_BOX_DIRECT_HELPERS=0` — stop binding `Long.valueOf` /
/// `Long.longValue` to their thin direct helpers and send both back through the
/// generic native funnel. Default ON.
///
/// Same reason the `census-direct-helpers` switch beside it exists: the blast
/// radius of these binds has to be measurable on ONE binary. A control built
/// from a different commit manufactured a 13-19% "regression" on phases that
/// contain neither call the last time that shortcut was taken.
pub fn long_box_direct_helpers_enabled() -> bool {
    true
}

/// Sites bound to the two `Long` boxing helpers, so "did this land" is
/// answerable without a timing run — the lesson `LEAF_NATIVE_HITS` was added
/// for, and the one `the field site cache shipped default-OFF for 13 days`
/// was re-learned from.
pub static LONG_VALUE_OF_SITES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static LONG_LONG_VALUE_SITES: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// `(Long.valueOf, Long.longValue)` sites bound to a thin direct helper.
pub fn long_box_direct_helper_sites() -> (u64, u64) {
    (
        LONG_VALUE_OF_SITES.load(std::sync::atomic::Ordering::Relaxed),
        LONG_LONG_VALUE_SITES.load(std::sync::atomic::Ordering::Relaxed),
    )
}

/// The `VarHandle` access modes served by [`VARHANDLE_READ_DIRECT_FNS`], in
/// slot-major order. All four are plain reads of the variable; the VM's
/// registry maps `getVolatile`/`getOpaque`/`getAcquire` to the same
/// `varhandle_get` callback `get` uses, so binding them together does not
/// widen what a bound site may do.
pub const VARHANDLE_READ_MODES: [&str; 4] = ["get", "getVolatile", "getOpaque", "getAcquire"];

/// The primitive return kinds served by [`VARHANDLE_READ_DIRECT_FNS`], in
/// slot-minor order, occupying kinds `0..8`. The two REFERENCE kinds follow
/// them at [`VARHANDLE_READ_KIND_REF_OBJECT`] and
/// [`VARHANDLE_READ_KIND_REF_STRICT`]; `[` is still absent, because an array
/// return is outside `varhandle_reference_return_mismatch`'s fire set today and
/// a stand-in descriptor would have to reproduce that exclusion for no measured
/// caller.
pub const VARHANDLE_READ_RETURNS: [u8; 8] = *b"ZBCSIJFD";

/// Slot-minor kind for a site whose declared return is exactly
/// `Ljava/lang/Object;`. The helpers' erased stand-in descriptor is that site's
/// real descriptor, so nothing is lost on either arm.
pub const VARHANDLE_READ_KIND_REF_OBJECT: usize = 8;

/// Slot-minor kind for a site whose declared return is a reference type no
/// boxed primitive is assignable to. The cold arm may therefore treat "a box
/// came back" as a W6-1 mismatch without knowing which class the site named.
pub const VARHANDLE_READ_KIND_REF_STRICT: usize = 9;

/// Slot-minor kinds: the eight primitives plus the two reference kinds.
pub const VARHANDLE_READ_KINDS: usize = 10;

/// `VARHANDLE_READ_MODES.len() * VARHANDLE_READ_KINDS`.
pub const VARHANDLE_READ_SLOTS: usize = 40;

/// The thirteen reference types, besides `java/lang/Object`, that a boxed
/// primitive can legally arrive at: the five shared wrapper supertypes and the
/// eight wrappers themselves.
///
/// A site returning one of these is NOT bound. Whether a box satisfies it
/// depends on WHICH wrapper the access produced (`Character` at a
/// `java/lang/Number` site is a mismatch, `Integer` is not), and that is the
/// one question neither slot kind can answer without the site's descriptor.
///
/// The list is the union of `vm::vm_exec::boxed_primitive_supertypes`' rows
/// minus `java/lang/Object`, plus its `PRIMITIVE_WRAPPER_CLASSES`. A name added
/// there and not here can only cost a bind, never correctness: the
/// classification errs towards `REF_STRICT`, and `REF_STRICT` throws where the
/// funnel would have thrown.
pub const VARHANDLE_BOX_ACCEPTING_RETURNS: [&str; 13] = [
    "java/lang/Number",
    "java/lang/Comparable",
    "java/io/Serializable",
    "java/lang/constant/Constable",
    "java/lang/constant/ConstantDesc",
    "java/lang/Boolean",
    "java/lang/Byte",
    "java/lang/Character",
    "java/lang/Short",
    "java/lang/Integer",
    "java/lang/Long",
    "java/lang/Float",
    "java/lang/Double",
];

/// The access-mode half of a slot.
pub const fn varhandle_read_slot_mode(slot: usize) -> usize {
    slot / VARHANDLE_READ_KINDS
}

/// The return-kind half of a slot.
pub const fn varhandle_read_slot_kind(slot: usize) -> usize {
    slot % VARHANDLE_READ_KINDS
}

/// The `return_type` byte a slot's call site carries: the primitive char for
/// kinds `0..8`, and `L` for both reference kinds.
pub const fn varhandle_read_slot_return(slot: usize) -> u8 {
    let kind = varhandle_read_slot_kind(slot);
    if kind < VARHANDLE_READ_RETURNS.len() {
        VARHANDLE_READ_RETURNS[kind]
    } else {
        b'L'
    }
}

/// Is this slot one of the two REFERENCE-returning kinds?
pub const fn varhandle_read_slot_is_reference(slot: usize) -> bool {
    varhandle_read_slot_kind(slot) >= VARHANDLE_READ_RETURNS.len()
}

/// `CRATONVM_JIT_VARHANDLE_REF_READ_DIRECT=0` — keep binding the PRIMITIVE read
/// kinds and send the two REFERENCE kinds back to the generic funnel, which
/// serves them through `try_varhandle_instance_field_read`.
///
/// Separate from `CRATONVM_JIT_VARHANDLE_READ_DIRECT_HELPERS` on purpose: the
/// reference half landed twelve days after the primitive half, and the A/B that
/// prices it has to be a single binary with one switch between the arms — the
/// rule the whole `varhandle` family of switches beside it exists for. Turning
/// the WHOLE read bind off would move the primitive rows too and price the
/// wrong change. Default ON.
pub fn varhandle_ref_read_direct_enabled() -> bool {
    true
}

/// The single-reference-coordinate, primitive-return shape this bind serves,
/// as a slot into [`VARHANDLE_READ_DIRECT_FNS`], or `None` when the site is
/// not that shape.
///
/// The descriptor test is what keeps the bind to *instance field* reads
/// without asking the VM anything at compile time:
///
/// * a **static-field** handle's read site is `()X` — zero coordinates;
/// * an **array element** or **byte-array/ByteBuffer view** handle's is
///   `([BI)X` / `(Ljava/nio/ByteBuffer;I)X` — two;
/// * a **`MemorySegment`** handle's carries a `J` offset — also two.
///
/// Only an instance-field handle reads with exactly one reference coordinate,
/// so `(L…;)X` / `([…)X` with a primitive `X` is the shape, and every other
/// site keeps the dispatch it has today. The helper re-checks the handle
/// itself at runtime and declines to the generic dispatcher when the side
/// table does not describe it as a resolved instance field, so this test is a
/// cheap pre-filter and not the correctness argument.
pub fn varhandle_read_helper_slot(method: &str, descriptor: &str) -> Option<usize> {
    let mode = VARHANDLE_READ_MODES.iter().position(|m| *m == method)?;
    if !descriptor.starts_with('(') {
        return None;
    }
    let close = cratonvm_jit_api::descriptor_param_list_end(descriptor)?;
    // Exactly one parameter, and it is a reference. A leading `[` covers an
    // array-TYPED field (`[I`, `[[B`, `[Ljava/lang/String;`), which is still an
    // ordinary reference-valued instance field, not an array-ELEMENT handle.
    let params = &descriptor[1..close];
    let one_reference_param = match params.as_bytes().first() {
        Some(b'L') => is_single_object_descriptor(params),
        Some(b'[') => {
            let elem = params.trim_start_matches('[');
            match elem.as_bytes().first() {
                Some(b'L') => is_single_object_descriptor(elem),
                Some(c) => elem.len() == 1 && VARHANDLE_READ_RETURNS.contains(c),
                None => false,
            }
        }
        _ => false,
    };
    if !one_reference_param {
        return None;
    }
    let ret = &descriptor[close + 1..];
    let kind = if ret.len() == 1 {
        VARHANDLE_READ_RETURNS
            .iter()
            .position(|r| *r == ret.as_bytes()[0])?
    } else if !varhandle_ref_read_direct_enabled() {
        // The reference half's kill switch is read HERE rather than at the emit
        // sites, so all three doors (single-pass, OSR, and the IR
        // intrinsic-site test) refuse together and the primitive half is
        // untouched by it. Three doors that disagree about one bind is the
        // defect `try_compile is NOT the only compile door` is written about.
        return None;
    } else if ret == "Ljava/lang/Object;" {
        VARHANDLE_READ_KIND_REF_OBJECT
    } else if ret.starts_with('L') && is_single_object_descriptor(ret) {
        let class = &ret[1..ret.len() - 1];
        if VARHANDLE_BOX_ACCEPTING_RETURNS.contains(&class) {
            return None;
        }
        VARHANDLE_READ_KIND_REF_STRICT
    } else {
        // `V`, an array return, or a malformed descriptor.
        return None;
    };
    Some(mode * VARHANDLE_READ_KINDS + kind)
}

/// Plain byte-array and judged ByteBuffer view reads share two coordinates
/// and a separate ABI row from instance-field reads. Ordered modes remain native.
pub const VARHANDLE_BYTE_ARRAY_VIEW_RETURNS: [u8; 6] = *b"SCIJFD";
pub fn varhandle_byte_array_view_read_slot(method: &str, descriptor: &str) -> Option<usize> {
    // Kept latched (j27 LATCHES): the VM's helper dispatch asks it per call.
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    if !*ENABLED.get_or_init(|| {
        true
    }) {
        return None;
    }
    if method != "get" {
        return None;
    }
    let ret = if descriptor.len() == 6 && descriptor.starts_with("([BI)") {
        descriptor.as_bytes()[5]
    } else if varhandle_judged_binds_enabled()
        && descriptor.len() == "(Ljava/nio/ByteBuffer;I)".len() + 1
        && descriptor.starts_with("(Ljava/nio/ByteBuffer;I)")
    {
        // The shared four-word ABI requires the body's actual descriptor for
        // this second carrier family; unjudged rows retain array-only binds.
        *descriptor.as_bytes().last()?
    } else {
        return None;
    };
    VARHANDLE_BYTE_ARRAY_VIEW_RETURNS
        .iter()
        .position(|&kind| kind == ret)
}

/// Byte-array and ByteBuffer view SET share the judged five-word ABI.
pub fn varhandle_byte_buffer_view_write_slot(name: &str, desc: &str) -> Option<usize> {
    if name != "set" || !varhandle_judged_binds_enabled() {
        return None;
    }
    let prefix = if desc.starts_with("([BI") {
        "([BI"
    } else {
        "(Ljava/nio/ByteBuffer;I"
    };
    let bytes = desc.as_bytes();
    if !desc.starts_with(prefix)
        || bytes.len() != prefix.len() + 3
        || &bytes[prefix.len() + 1..] != b")V"
    {
        return None;
    }
    VARHANDLE_BYTE_ARRAY_VIEW_RETURNS
        .iter()
        .position(|&kind| kind == bytes[prefix.len()])
}

/// `Lfoo/Bar;` and nothing after it — one object descriptor consuming the
/// whole slice. Two parameters (`Lfoo;Lbar;`) fail on the interior `;`.
pub(crate) fn is_single_object_descriptor(s: &str) -> bool {
    s.len() >= 3 && s.ends_with(';') && !s[1..s.len() - 1].contains(';')
}

/// `CRATONVM_JIT_VARHANDLE_READ_DIRECT_HELPERS=0` — stop binding `VarHandle`
/// read modes to their thin direct helpers and send every one back through the
/// generic native funnel (where `try_varhandle_instance_field_read` still
/// serves them, one funnel round trip later). Default ON.
///
/// Same reason the `census-direct-helpers` and `long-box-direct-helpers`
/// switches beside it exist: the blast radius has to be measurable on ONE
/// binary. A control built from a different commit manufactured a 13-19 %
/// "regression" on phases containing neither call the last time that shortcut
/// was taken.
pub fn varhandle_read_direct_helpers_enabled() -> bool {
    true
}

// ---------------------------------------------------------------------------
// `VarHandle` WRITE-mode thin direct-call bind.
//
// The read table above serves `get`-family sites whose value is a PRIMITIVE.
// Writes were never in scope, so `VarHandle.set` still pays the whole generic
// native funnel — measured at 303x HotSpot for a reference field (1.0 ns
// against 303.4 ns), against a 9x plain-field-store baseline. A native census
// of `HibfixVarHandleProbe` counts 698 000 `VarHandle.set` invocations for
// 500 000 probe writes: every one is a full dispatch.
//
// See `varhandle-writes-and-cas-have-no-fast-path-FIXED-20260827.md`
// (retired to the internal tree 2026-08-27, FIXED).
//
// A write is the EASIER half of what the read bind refused, not the harder one.
// The read table excludes `L`/`[` because a reference RETURN has to be
// published as a handoff root before the caller can store it, and the direct
// arm takes no thread borrow to publish it with. A write has no return: the
// reference travels INWARD, in a register the compiled caller's own frame
// already describes, so there is no window to root across and reference values
// are in scope here from the start.
//
// `compareAndSet` IS bound now — see `VARHANDLE_CAS_DIRECT_FNS` below. The
// objection recorded here until 2026-08-28 was that its helper is
// `(vm_ptr, vh, receiver, expected, new)`, five arguments against Windows'
// four ARG_REGS, "so it needs the stack-argument setup this bind does not".
// That was true when the write bind was written and had stopped being true
// before it was read: the `0xb6 | 0xb7 | 0xb9` arm calls
// `emit_stack_arg_setup(&arg_slots, callee_needs_ctx)`, whose own comment says
// "stack-arg setup for invokespecial/virtual direct calls whose receiver+params
// exceed ARG_REGS". A fifth argument lands on the stack on Win64 and in
// `ARG_REGS[4]` on SysV, and neither door needed a line changed for it.
// ---------------------------------------------------------------------------

/// The `VarHandle` write access modes served by [`VARHANDLE_WRITE_DIRECT_FNS`],
/// in slot-major order.
///
/// All four are stores of the variable, registered as `VarHandle` natives
/// (`varhandle_set`, and `varhandle_set_volatile` — the same store between two
/// SeqCst fences — for `setVolatile`), so binding them together does not widen
/// what a bound site may do — the same argument the read modes rest on. The
/// helper stores plain `set` with the plain store and the other three through
/// `set_field_volatile_as`, the STRONGEST of the orderings, which is always a
/// legal implementation of a weaker one. It declines (to the native) a handle
/// over a `final` field, a receiver outside the handle's class, and a
/// reference the field's type does not admit — the native's three refusals.
pub const VARHANDLE_WRITE_MODES: [&str; 4] = ["set", "setVolatile", "setRelease", "setOpaque"];

/// The value kinds served, in slot-minor order.
///
/// `L` IS here, unlike [`VARHANDLE_READ_RETURNS`] — see this section's header
/// for why a reference is safe inbound. `L` and `R` are both REFERENCE value
/// kinds and differ only in the static type the cold arm's synthetic call site
/// names (`varhandle_reference_value_kind`): `L` for an `Object`-equivalent
/// site, `R` for a site typed as any other class or as an array (round 11
/// wave 18). `R` is never a descriptor byte; the helper normalises it to `L`
/// before the shared store body, so an array-typed field is stored under the
/// plan's `L` in both.
pub const VARHANDLE_WRITE_KINDS: [u8; 10] = *b"ZBCSIJFDLR";

/// `VARHANDLE_WRITE_MODES.len() * VARHANDLE_WRITE_KINDS.len()`.
pub const VARHANDLE_WRITE_SLOTS: usize = 40;

/// The bind kind of a REFERENCE-typed `VarHandle` value parameter: `L`, `R`,
/// or `None` for a static type the thin helpers must not take (round 11 waves
/// 17-18, `r11w16-orch-varhandle-string-site-still-cce-in-single-pass`).
///
/// A bound site has no `JitInvokeInfo` of its own: when the helper declines,
/// its cold arm calls the native through a per-SLOT synthetic call site, and
/// the site-cached dispatch arms the native's value-site cell from THAT
/// descriptor. For a PRIMITIVE variable the static type decides the answer
/// (`asType`: `poly_call_site::vh_site`), so the synthetic site's value type
/// has to fall in the same group as the real one. By static-type group:
///
/// * `Object`, `Serializable`, `Comparable`, `Constable` — every wrapper is
///   one: `L`, whose synthetic site says `Object` (unbox / CCE / NPE).
/// * `Number`, `ConstantDesc` and the eight wrappers — the verdict depends on
///   WHICH primitive the variable is (`Long` into an `int` field is WMTE,
///   `Object`-site CCE). Not bound (`None`): the site keeps its own
///   `JitInvokeInfo`, whose real descriptor the dispatch arms from. Such
///   sites are rare, so the funnel's cost there is immaterial.
/// * any other class or interface, and every array — a supertype of NO
///   wrapper, so a primitive variable refuses it statically (WMTE, value or
///   null): `R`, whose synthetic site says `java/lang/Void` (a class the
///   classifier files as `OTHER`). These are the common typed sites
///   (`NEXT.compareAndSet(p, null, node)`), so they stay bound; on a
///   REFERENCE variable the native ignores the site class for a reference
///   value, so `R` answers exactly as `L` there.
///
/// The class list is `cratonvm_native_api::poly_call_site`'s `vh_param_class`
/// (this crate does not depend on that one); keep the two in step.
pub(crate) fn varhandle_reference_value_kind(desc: &str) -> Option<u8> {
    match desc {
        "Ljava/lang/Object;"
        | "Ljava/io/Serializable;"
        | "Ljava/lang/Comparable;"
        | "Ljava/lang/constant/Constable;" => Some(b'L'),
        "Ljava/lang/Number;"
        | "Ljava/lang/constant/ConstantDesc;"
        | "Ljava/lang/Boolean;"
        | "Ljava/lang/Byte;"
        | "Ljava/lang/Character;"
        | "Ljava/lang/Short;"
        | "Ljava/lang/Integer;"
        | "Ljava/lang/Long;"
        | "Ljava/lang/Float;"
        | "Ljava/lang/Double;" => None,
        _ => Some(b'R'),
    }
}

/// The single-reference-coordinate, one-value, `void`-returning shape this bind
/// serves, as a slot into [`VARHANDLE_WRITE_DIRECT_FNS`].
///
/// The descriptor test mirrors [`varhandle_read_helper_slot`]'s and rules out
/// the same shapes for the same reasons: a static-field handle writes with
/// `(X)V` (zero coordinates), an array-element or view handle with `([BIX)V`
/// (two), a `MemorySegment` handle with a `J` offset (two). Only an
/// instance-field handle writes with exactly one reference coordinate followed
/// by one value.
///
/// The helper re-checks the handle at runtime and declines to the generic
/// dispatcher for anything the side table does not describe as a resolved
/// instance field of this kind, so this is a cheap pre-filter and not the
/// correctness argument.
pub fn varhandle_write_helper_slot(method: &str, descriptor: &str) -> Option<usize> {
    let mode = VARHANDLE_WRITE_MODES.iter().position(|m| *m == method)?;
    if !descriptor.starts_with('(') {
        return None;
    }
    let close = cratonvm_jit_api::descriptor_param_list_end(descriptor)?;
    if &descriptor[close + 1..] != "V" {
        return None;
    }
    let params = &descriptor[1..close];
    // Split the leading reference coordinate off; whatever remains is the value.
    let coord_len = match params.as_bytes().first()? {
        b'L' => params.find(';')? + 1,
        b'[' => {
            let after = params.trim_start_matches('[');
            let consumed = params.len() - after.len();
            match after.as_bytes().first()? {
                b'L' => consumed + after.find(';')? + 1,
                // `L`/`R` are the reference slot kinds, not descriptor bytes.
                c if !matches!(*c, b'L' | b'R') && VARHANDLE_WRITE_KINDS.contains(c) => {
                    consumed + 1
                }
                _ => return None,
            }
        }
        _ => return None,
    };
    let value = &params[coord_len..];
    let kind = match value.as_bytes().first()? {
        // A reference-typed value, however deeply arrayed, is one word -- but
        // its static type picks the synthetic call site of the cold arm
        // (`varhandle_reference_value_kind`).
        b'L' if is_single_object_descriptor(value) => varhandle_reference_value_kind(value)?,
        // An array type is a supertype of no wrapper: the `R` group. It must
        // be ONE descriptor consuming the rest, as the read bind requires:
        // `(LH;[II)V` is two values, and binding it with `num_params: 2`
        // would leave a word on the operand stack (round 11 wave 18).
        b'[' => {
            let elem = value.trim_start_matches('[');
            let single = match elem.as_bytes().first() {
                Some(b'L') => is_single_object_descriptor(elem),
                Some(c) => {
                    elem.len() == 1
                        && !matches!(*c, b'L' | b'R')
                        && VARHANDLE_WRITE_KINDS.contains(c)
                }
                None => false,
            };
            if !single {
                return None;
            }
            b'R'
        }
        // A bare `L` or `R` is not a descriptor.
        c if value.len() == 1 && !matches!(*c, b'L' | b'R') => *c,
        _ => return None,
    };
    let kind_idx = VARHANDLE_WRITE_KINDS.iter().position(|k| *k == kind)?;
    Some(mode * VARHANDLE_WRITE_KINDS.len() + kind_idx)
}

/// `CRATONVM_JIT_VARHANDLE_WRITE_DIRECT_HELPERS=0` — send every `VarHandle`
/// write back through the generic native funnel. Default ON.
///
/// The twin of [`varhandle_read_direct_helpers_enabled`], and there for the
/// same reason: the blast radius has to be measurable on ONE binary.
pub fn varhandle_write_direct_helpers_enabled() -> bool {
    true
}

/// Sites bound to a `VarHandle` write helper, split by compile door.
pub static VARHANDLE_WRITE_SITES_SINGLEPASS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub static VARHANDLE_WRITE_SITES_OSR: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// `(single-pass, OSR)` `VarHandle` write sites bound to a thin direct helper.
pub fn varhandle_write_direct_helper_sites() -> (u64, u64) {
    (
        VARHANDLE_WRITE_SITES_SINGLEPASS.load(std::sync::atomic::Ordering::Relaxed),
        VARHANDLE_WRITE_SITES_OSR.load(std::sync::atomic::Ordering::Relaxed),
    )
}

// ---------------------------------------------------------------------------
// `VarHandle` compareAndSet thin direct-call bind.
//
// The CAS was served from INSIDE the funnel (`try_varhandle_instance_field_cas`)
// rather than bound, which covers every interpreter dispatch and every site the
// JIT declines — but leaves the funnel's own entry cost on every call. A native
// profile of an isolated CAS probe puts that at ~25% of a reference CAS:
// 13.2% in `try_jit_site_cached_native_dispatch` and 11.9% in
// `forward_jit_reference_args`/`forward_jit_arg_at`, against 3.1% in
// `compare_and_swap_field_shared` and 1.2% in `hw_atomic_addr` — i.e. the
// hardware atomic and its barriers are ~4% of what a CAS costs and the dispatch
// is most of the rest. A further 10.3% was `CharSearcher::next_match`: the
// funnel arm re-parses the call site's descriptor on every call to learn the
// operand kinds, and a bound slot knows them at bind time.
//
// The in-funnel arm STAYS. It is what serves the interpreter and the declined
// sites, and the two now share one implementation
// (`varhandle_instance_field_cas_shared`) for the reason the write pair does:
// the SATB pre-barrier fires on `expected` BEFORE the store and the post
// `write_barrier` only on success, and two copies of that would drift.
//
// Reference values are in scope, on the write bind's argument and not the read
// bind's: `compareAndSet` returns `Z`, so there is no reference RETURN to
// publish as a handoff root, and `expected`/`new` travel INWARD in registers
// the compiled caller's own frame already describes.
// ---------------------------------------------------------------------------

/// The value kinds served, in slot order — the same ten the write bind takes,
/// and for the same reason `L` and `R` are among them.
pub const VARHANDLE_CAS_KINDS: [u8; 10] = *b"ZBCSIJFDLR";

/// `VARHANDLE_CAS_KINDS.len()`. One mode only: `compareAndSet`.
///
/// `weakCompareAndSet*` and `compareAndExchange` are NOT bound with it.
/// The registry maps each to its own callback with its own result shape
/// (`compareAndExchange` returns the witness value, not a `boolean`), so
/// binding them together would widen what a bound site may do — which is
/// exactly the property the write bind's four modes have and these do not.
pub const VARHANDLE_CAS_SLOTS: usize = 10;

/// The single-reference-coordinate, two-value, `boolean`-returning shape this
/// bind serves, as a slot into [`VARHANDLE_CAS_DIRECT_FNS`].
///
/// Rules out the same shapes [`varhandle_write_helper_slot`] does and for the
/// same reasons — a static-field handle CASes with `(XX)Z` (zero coordinates),
/// an array-element or view handle with `([BIXX)Z` (two) — and adds the one
/// this shape needs: `expected` and `new` must be the SAME kind, which a
/// `VarHandle` call site always satisfies and a mis-shaped descriptor does not.
/// The two reference groups `L` and `R` count as one kind here; a mixed pair
/// takes `R` (see the body).
///
/// The helper re-checks the handle at runtime and declines to the generic
/// dispatcher for anything the side table does not describe as a resolved
/// instance field of this kind, so this is a cheap pre-filter and not the
/// correctness argument.
pub fn varhandle_cas_helper_slot(method: &str, descriptor: &str) -> Option<usize> {
    if method != "compareAndSet" {
        return None;
    }
    if !descriptor.starts_with('(') {
        return None;
    }
    let close = cratonvm_jit_api::descriptor_param_list_end(descriptor)?;
    if &descriptor[close + 1..] != "Z" {
        return None;
    }
    let params = &descriptor[1..close];
    // Split the leading reference coordinate off; two equal values must remain.
    let coord_len = match params.as_bytes().first()? {
        b'L' => params.find(';')? + 1,
        b'[' => {
            let after = params.trim_start_matches('[');
            let consumed = params.len() - after.len();
            match after.as_bytes().first()? {
                b'L' => consumed + after.find(';')? + 1,
                // `L`/`R` are the reference slot kinds, not descriptor bytes.
                c if !matches!(*c, b'L' | b'R') && VARHANDLE_CAS_KINDS.contains(c) => consumed + 1,
                _ => return None,
            }
        }
        _ => return None,
    };
    let rest = &params[coord_len..];
    let (expected, after_expected) = split_one_cas_operand(rest)?;
    let (new, tail) = split_one_cas_operand(after_expected)?;
    if !tail.is_empty() {
        return None;
    }
    // `asType` checks every parameter before converting any value, so one
    // `R` operand makes a primitive variable's call WMTE whatever the other
    // is: a mixed `L`/`R` pair takes the `R` slot. A reference variable
    // answers `L` and `R` alike.
    let kind = match (expected, new) {
        (b'L' | b'R', b'L' | b'R') => {
            if expected == b'R' || new == b'R' {
                b'R'
            } else {
                b'L'
            }
        }
        (e, n) if e == n => e,
        _ => return None,
    };
    let kind_idx = VARHANDLE_CAS_KINDS.iter().position(|k| *k == kind)?;
    Some(kind_idx)
}

/// One operand descriptor off the front of `s`, collapsed to its kind byte the
/// way `varhandle_instance_field_plan` collapses a field descriptor, except
/// that a reference keeps its static-type group: `b'L'` for an
/// `Object`-equivalent type, `b'R'` for any other class and for every array,
/// and a `Number`/`ConstantDesc`/wrapper type refuses the whole bind
/// (`varhandle_reference_value_kind`, round 11 waves 17-18).
pub(crate) fn split_one_cas_operand(s: &str) -> Option<(u8, &str)> {
    let b = s.as_bytes();
    match b.first()? {
        b'L' => {
            let end = s.find(';')? + 1;
            Some((varhandle_reference_value_kind(&s[..end])?, &s[end..]))
        }
        b'[' => {
            let after = s.trim_start_matches('[');
            let consumed = s.len() - after.len();
            match after.as_bytes().first()? {
                b'L' => Some((b'R', &s[consumed + after.find(';')? + 1..])),
                c if !matches!(*c, b'L' | b'R') && VARHANDLE_CAS_KINDS.contains(c) => {
                    Some((b'R', &s[consumed + 1..]))
                }
                _ => None,
            }
        }
        c if !matches!(*c, b'L' | b'R') && VARHANDLE_CAS_KINDS.contains(c) => Some((*c, &s[1..])),
        _ => None,
    }
}

/// `CRATONVM_JIT_VARHANDLE_CAS_DIRECT_HELPERS=0` — send every `VarHandle` CAS
/// back through the generic native funnel, where
/// `try_varhandle_instance_field_cas` still serves it one funnel round trip
/// later. Default ON.
///
/// The twin of the read and write switches, and there for the same reason: the
/// blast radius has to be measurable on ONE binary.
pub fn varhandle_cas_direct_helpers_enabled() -> bool {
    true
}

/// Sites bound to a `VarHandle` CAS helper, split by compile door.
///
/// Split because the write bind proved the split is what catches an inert
/// bind: bound in the single-pass ladder alone, its census moved by zero,
/// because a `main` loop is an OSR body.
pub static VARHANDLE_CAS_SITES_SINGLEPASS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub static VARHANDLE_CAS_SITES_OSR: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// `(single-pass, OSR)` `VarHandle` CAS sites bound to a thin direct helper.
pub fn varhandle_cas_direct_helper_sites() -> (u64, u64) {
    (
        VARHANDLE_CAS_SITES_SINGLEPASS.load(std::sync::atomic::Ordering::Relaxed),
        VARHANDLE_CAS_SITES_OSR.load(std::sync::atomic::Ordering::Relaxed),
    )
}

/// The class name under which a body that bound a thin `VarHandle` helper
/// (read, write or CAS; single-pass or OSR door) records that fact in
/// [`CompiledMethod::inlined_methods`] (round 15 wave 3, lane vh3; VH2-2 in
/// `jit-r15w2-vh2-proposals-RETIRED-20261003.md`,
/// `r15w2-vh2-thin-helpers-bound-before-first-exact-mint-FIXED-20260930.md`).
///
/// A thin bind carries no `JitInvokeInfo`, so its helper cannot judge an
/// invoke-exact `VarHandle` against the site's descriptor; binds are made only
/// while the VM has minted no exact handle. The VM's first exact mint then
/// withdraws every body that bound one with
/// `JitCache::invalidate_for_class(VARHANDLE_THIN_BIND_DEPENDENCY)` -- the
/// class-hierarchy invalidation channel, which also withdraws the raw direct
/// callers of such a body, retargets inline caches, and (through its
/// invalidation log) refuses a thin-bound compile still in flight. Its next
/// compile sees the gate open and binds nothing.
///
/// Not `java/lang/invoke/VarHandle` itself, on purpose: the class-define
/// listener invalidates by every SUPERTYPE of a new class, so loading any
/// `VarHandle` subclass would withdraw these bodies for nothing. No class can
/// have this name (`java.*` is a prohibited package outside the boot image,
/// and the image has no such class), so only the sweep ever names it.
pub const VARHANDLE_THIN_BIND_DEPENDENCY: &str = "java/lang/invoke/VarHandle$CratonThinBind";

/// `CRATONVM_JIT_VH_THIN_BIND_RETIRE` (default on; round 15 wave 3, lane
/// vh3): record [`VARHANDLE_THIN_BIND_DEPENDENCY`] on every thin-bound body,
/// and withdraw them at the VM's first exact `VarHandle` mint. `0` restores
/// round 15 wave 2 (a body compiled before the first mint keeps its binds).
/// Read at compile time and once per VM at the mint, never per call, so no
/// static.
pub fn varhandle_thin_bind_retire_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_VH_RETIRE_SUPERSEDED` (default on; round 15 wave 4, lane vh6;
/// `r15w4-vh5-via-caller-keeps-thin-bind-after-mint-FIXED-20260930.md`): the
/// first-exact-mint sweep (`JitCache::invalidate_for_class` of
/// [`VARHANDLE_THIN_BIND_DEPENDENCY`]) also retires every live SUPERSEDED body
/// that bound a thin `VarHandle` helper or bakes a raw call into a body the
/// sweep withdraws, and withdraws the published raw callers of those. `0`
/// sweeps the published bodies only (the round-15-wave-3 reach). Read once per
/// VM, at the sweep, so no static.
pub(crate) fn varhandle_retire_superseded_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_VH_MINT_FORCES_EXITS` (default on; round 15 wave 5, lane vh7;
/// `r15w3-vh3-running-thin-bound-activation-keeps-binds-FIXED-20260930.md`): the
/// first-exact-mint sweep ([`JitCache::invalidate_varhandle_thin_binds`]) also
/// marks every body it withdrew withdrawn and forces its exit polls and
/// post-call exits, so a frame ALREADY RUNNING one (a thin-bound OSR loop, or a
/// loop that bakes a raw call into a thin-bound callee) leaves for the
/// interpreter at its next back edge, or when the call it is inside returns,
/// instead of running the unjudged thin helper to the end of the loop. `0`
/// withdraws only (the round-15-wave-4 reach). Read once per VM, at the sweep.
pub(crate) fn varhandle_mint_forces_exits_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_VH_MINT_NOT_ENTRANT` (default on; round 15 wave 5, lane vh7;
/// the entry half of VH6-3 in `jit-r15w4-vh6-proposals-RETIRED-20261003.md`): the
/// first-exact-mint sweep also makes every method-entry body that bound a thin
/// `VarHandle` helper NOT ENTRANT ([`JitCache::make_not_entrant`]), so a raw
/// call the reverse closure cannot see (an optimizing body already running
/// its loop, which has no forceable exit; a caller the closure withdrew but a
/// frame still runs) re-dispatches through the VM, which judges the access,
/// instead of entering the unjudged binds. The VM arms the cache's not-entrant
/// hook for it when this is on (`vm::jit::helpers::retire_varhandle_thin_bound_bodies`).
/// The closure still withdraws the published callers. `0` patches nothing.
/// Read once per VM, at the sweep.
pub fn varhandle_mint_not_entrant_enabled() -> bool {
    true
}



/// `CRATONVM_JIT_VH_JUDGED_BINDS` (default on, x86-64 only; round 15 wave 6,
/// lane vh8; VH7-1 of `jit-r15w5-vh7-proposals-RETIRED-20261003.md`): bind a thin `VarHandle`
/// helper as a JUDGED site ([`VarHandleJudgedSite`]) whose call carries the
/// site's own `JitInvokeInfo`, so the helper refuses a mismatching exact
/// handle itself (it sends an exact receiver down its cold arm, which
/// dispatches with the REAL descriptor and so judges), and the bind is made
/// before AND after the VM's first exact mint. A judged body records no
/// [`VARHANDLE_THIN_BIND_DEPENDENCY`], so the first-mint sweep has nothing of
/// it to withdraw: the post-mint cliff (every `VarHandle` field access of the
/// VM falling to the funnel for the rest of its life) is gone for every door
/// that builds judged rows. `0` restores the round-15-wave-5 unjudged binds and
/// their sweep. Read at compile time only (uncached: no static), and by the VM
/// when it builds a compile's helper table. The aarch64 backend has no judged
/// lowering, so it keeps the unjudged binds and the gate.
pub fn varhandle_judged_binds_enabled() -> bool {
    cfg!(target_arch = "x86_64")
}

/// [`JitDirectCall::guard_class_id`] value that marks a row as a JUDGED thin
/// `VarHandle` bind (round 15 wave 6, lane vh8; VH7-1). Like
/// [`JIT_CYCLE_EDGE_CELL_GUARD`], the row's `entry` is not code: it is the
/// address of the site's [`VarHandleJudgedSite`], whose `helper` is the thin
/// helper the single-pass `invokevirtual` arm CALLs (`x64/op_invoke.rs`), with
/// `ARG_REGS[0]` holding the record's address tagged with
/// [`VARHANDLE_JUDGED_SITE_TAG`] instead of the bare `SharedVm` pointer. Only
/// planned on an `invokevirtual` of `java/lang/invoke/VarHandle` with
/// `needs_context`; outside the `0xFFFF_FFFx` markers'
/// [`sync_direct_instance_guard_class`] range, so no real class id is ever
/// compared against it.
pub const JIT_VARHANDLE_JUDGED_GUARD: u32 = 0xFFFF_FFF4;

/// Bit 0 of a thin `VarHandle` helper's first argument word: set, the word is
/// a [`VarHandleJudgedSite`] address (8-aligned, so the bit is free); clear,
/// it is the `SharedVm` pointer of an unjudged bind (also 8-aligned), which is
/// what every helper received before round 15 wave 6. One test on the helper's
/// path tells the two apart, so judged and unjudged rows share the helpers,
/// their addresses and their `DirectHelperTable` signatures.
pub const VARHANDLE_JUDGED_SITE_TAG: usize = 1;

/// The per-site record of a JUDGED thin `VarHandle` bind (round 15 wave 6,
/// lane vh8; VH7-1 of `jit-r15w5-vh7-proposals-RETIRED-20261003.md`, the VH2-1 / VH6-2 idea).
///
/// A thin bind used to carry no site descriptor (`JitDirectCall` has no site
/// constant), so its helper could not judge an invoke-exact handle and every
/// thin bind had to be withdrawn at the VM's first exact mint. The record
/// gives the call its site: `info` is the `JitInvokeInfo` the generic push
/// would have built for this pc (real class, mode name and descriptor), which
/// the helper's cold arm dispatches with, so an exact receiver is judged by
/// `jit_invoke_dispatch` exactly as at an unbound site. `vm_ptr` is the
/// `SharedVm` the body was compiled for (the word the tag displaces), and
/// `helper` the per-slot thin helper the row CALLs.
///
/// Owned by the artifact whose code bakes its address
/// ([`CompiledMethod::_jit_vh_judged_sites`]), and owns the three names its
/// `info` borrows: a boxed str's payload never moves, so the `&'static str`s
/// stay valid for exactly as long as the record does, the same contract the
/// `_jit_strings` / `_jit_invoke_infos` arenas give every other site.
pub struct VarHandleJudgedSite {
    /// The `SharedVm` pointer the helper would have received untagged.
    pub vm_ptr: usize,
    /// The per-slot thin helper this site CALLs.
    pub helper: usize,
    /// The site's own dispatch info (see the type's doc).
    pub info: JitInvokeInfo,
    /// JIT round 23 wave 4, lane L8: the [`ffm_element_site_word`] of this
    /// site, decided once here so the FFM element thin helper reads one word
    /// per call instead of re-parsing the descriptor and method name (27% of
    /// the bound route's time, measured by RIP sampling on n23-w3). `0` for
    /// every site that is not a `MemorySegment` element accessor.
    pub ffm_element: u32,
    pub(crate) _names: [std::sync::Arc<str>; 3],
}

/// Bit 16 of an [`ffm_element_site_word`]: set for every FFM element site, so
/// the word is never the `0` of "not one".
pub const FFM_ELEMENT_SITE_VALID: u32 = 1 << 16;
/// Bit 8 of an [`ffm_element_site_word`]: the site's coordinate is a BYTE
/// offset (`get(layout, long)` / `set`), not an element index (`getAtIndex` /
/// `setAtIndex`).
pub const FFM_ELEMENT_SITE_BYTE_OFFSET: u32 = 1 << 8;
/// Bit 9 of an [`ffm_element_site_word`]: a byte-offset site whose offset is
/// NOT a multiple of the width may still take the cached road, as a raw byte
/// offset ([`ffm_unaligned_offset_access_enabled`], decided at bind time).
pub const FFM_ELEMENT_SITE_UNALIGNED_OK: u32 = 1 << 9;

/// The word 0 the single-pass FFM element block carries for a byte-offset
/// access whose offset is not a multiple of the width (JIT r23 wave 4, lane
/// L8): the helper then takes the offset as pushed from word 4. No converted
/// index can equal it: `offset >> log2(width)` with a width of 2 or more is at
/// least `i64::MIN / 2`, and a byte site is never converted.
pub const FFM_BYTE_OFFSET_MARK: i64 = i64::MIN;

/// `CRATONVM_JIT_FFM_UNALIGNED_OFFSET_ACCESS` (JIT r23 wave 4, lane L8; perf,
/// default ON, kill `0`): a `MemorySegment.get/set(layout, long offset)` site
/// whose offset is not a multiple of the element width (an unaligned layout:
/// `JAVA_INT_UNALIGNED`, `withByteAlignment(1)`) takes the FFM element
/// helper's cached road with the raw byte offset instead of the native
/// dispatch (measured on n23-w3: `get(JAVA_INT_UNALIGNED, 1 + 4k)` 76 us per
/// 64 accesses, against 1.4 us for `getAtIndex` of the same segment). The road
/// makes the JDK's checks -- bounds, read-only, then the layout's alignment
/// against the address -- and declines any failure to the dispatch, which
/// raises the JDK's exception, so a misaligned offset through an ALIGNED
/// layout still raises `IllegalArgumentException`. Read at compile time.
pub fn ffm_unaligned_offset_access_enabled() -> bool {
    true
}

/// The per-site word of a `MemorySegment` element accessor (JIT round 23 wave
/// 4, lane L8): [`FFM_ELEMENT_SITE_VALID`], [`FFM_ELEMENT_SITE_BYTE_OFFSET`]
/// for `get` / `set`, and the FFM kind ([`ffm_kind_for_descriptor`], `0..=6`)
/// in the low byte; `0` for any other site, a kind the intrinsic does not
/// serve, or `CRATONVM_JIT_NO_FFM_INTRINSIC` (whose sites then dispatch on
/// every call, as that switch wants).
pub fn ffm_element_site_word(class_name: &str, method_name: &str, descriptor: &str) -> u32 {
    if class_name != "java/lang/foreign/MemorySegment" {
        return 0;
    }
    let byte_offset = match method_name {
        "get" | "set" => true,
        "getAtIndex" | "setAtIndex" => false,
        _ => return 0,
    };
    let Some(kind) = ffm_kind_for_descriptor(descriptor).and_then(|k| u32::try_from(k).ok())
    else {
        return 0;
    };
    if kind > 6 {
        return 0;
    }
    let unaligned_ok = byte_offset && ffm_unaligned_offset_access_enabled();
    FFM_ELEMENT_SITE_VALID
        | (u32::from(byte_offset) * FFM_ELEMENT_SITE_BYTE_OFFSET)
        | (u32::from(unaligned_ok) * FFM_ELEMENT_SITE_UNALIGNED_OK)
        | kind
}


impl VarHandleJudgedSite {
    /// The record for one bound site. `helper` must be the thin helper the
    /// ladder chose for the site's slot, and `vm_ptr` the compile's `SharedVm`
    /// pointer (`DirectHelperTable::static_base_resolver_ctx` on the
    /// single-pass door, `shared` on the OSR door); the remaining arguments
    /// are the generic `JitInvokeInfo` push's for the same pc.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        vm_ptr: usize,
        helper: usize,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        num_jit_args: usize,
        return_type: u8,
        invoke_kind: u8,
        declaring_class_id: u32,
        owner_class_id: u32,
    ) -> Box<Self> {
        let names: [Arc<str>; 3] = [class_name.into(), method_name.into(), descriptor.into()];
        let class_ref = Arc::as_ptr(&names[0]);
        let method_ref = Arc::as_ptr(&names[1]);
        let desc_ref = Arc::as_ptr(&names[2]);
        Box::new(Self {
            vm_ptr,
            helper,
            ffm_element: ffm_element_site_word(class_name, method_name, descriptor),
            info: JitInvokeInfo {
                // SAFETY: `_names` owns the immutable Arc payloads; moving an Arc preserves their shared borrows.
                class_name: unsafe { &*class_ref },
                // SAFETY: as `class_name` above.
                method_name: unsafe { &*method_ref },
                // SAFETY: as `class_name` above.
                descriptor: unsafe { &*desc_ref },
                num_jit_args,
                return_type,
                invoke_kind,
                declaring_class_id,
                owner_class_id,
            },
            _names: names,
        })
    }

    /// The `direct_calls` row that binds this site: `entry` is the record's
    /// address, marked by [`JIT_VARHANDLE_JUDGED_GUARD`]. `num_params` and
    /// `return_type` are the unjudged row's (the coordinate and values, not
    /// counting the `VarHandle`; the slot's return kind).
    pub fn direct_call(&self, num_params: usize, return_type: u8) -> JitDirectCall {
        JitDirectCall {
            entry: self as *const Self as usize,
            needs_context: true,
            num_params,
            return_type,
            guard_class_id: JIT_VARHANDLE_JUDGED_GUARD,
        }
    }
}

/// The thin helper a [`JIT_VARHANDLE_JUDGED_GUARD`] row's `entry` names, or
/// `None` for a word that cannot be a record address (zero, misaligned) or a
/// record with no helper. Read by the single-pass emitter only.
///
/// # Safety
///
/// `record` is a row's `entry` that a door built with
/// [`VarHandleJudgedSite::direct_call`] from a record it still owns (the
/// compile's arenas outlive the backend run, as every `JitInvokeInfo` pointer
/// in the tables does).
pub(crate) unsafe fn varhandle_judged_site_helper(record: usize) -> Option<usize> {
    if record == 0 || record % std::mem::align_of::<VarHandleJudgedSite>() != 0 {
        return None;
    }
    // SAFETY: see the function's contract.
    let helper = unsafe { (*(record as *const VarHandleJudgedSite)).helper };
    (helper != 0).then_some(helper)
}

/// The `direct_calls` row of one thin `VarHandle` bind of the single-pass
/// ladder (round 15 wave 6, lane vh8): JUDGED ([`VarHandleJudgedSite`], no
/// thin-bind dependency) under [`varhandle_judged_binds_enabled`] when the
/// compile names its VM (`vm_ptr != 0`, always so for a VM-built table), else
/// the unjudged row with [`VARHANDLE_THIN_BIND_DEPENDENCY`] recorded, exactly
/// as before. `entry` is the slot's helper, `num_params` / `return_type` the
/// row's, and `site` + the four trailing scalars what the generic
/// `JitInvokeInfo` push would have recorded for this pc.
#[allow(clippy::too_many_arguments)]
pub(crate) fn single_pass_varhandle_row(
    entry: usize,
    vm_ptr: usize,
    num_params: usize,
    return_type: u8,
    site: (&str, &str, &str),
    num_jit_args: usize,
    site_return: u8,
    invoke_kind: u8,
    declaring_class_id: u32,
    owner_class_id: u32,
    owned_vh_judged_sites: &mut Vec<Box<VarHandleJudgedSite>>,
    inlined_methods: &mut Vec<(String, String, String)>,
) -> JitDirectCall {
    if vm_ptr != 0 && varhandle_judged_binds_enabled() {
        let record = VarHandleJudgedSite::new(
            vm_ptr,
            entry,
            site.0,
            site.1,
            site.2,
            num_jit_args,
            site_return,
            invoke_kind,
            declaring_class_id,
            owner_class_id,
        );
        // The row names the boxed record's heap address, which moving the
        // `Box` into the arena does not change.
        let row = record.direct_call(num_params, return_type);
        owned_vh_judged_sites.push(record);
        return row;
    }
    // Round 15 wave 3 (lane vh3): withdrawn at the VM's first exact mint.
    add_varhandle_thin_bind_dependency(inlined_methods);
    JitDirectCall {
        entry,
        needs_context: true,
        num_params,
        return_type,
        guard_class_id: 0,
    }
}


/// Primitive read shapes the optimizing tier can preserve with the same
/// judged thin helper as method entry. Reference reads keep their existing
/// return-root and cast policy. Field and byte-array reads bind under
/// `CRATONVM_JIT_IR_VH_FIELD_READ_BINDS` (default ON since j27 2026-10-28, `=0`
/// kills; it was opt-in before). ByteBuffer views bind in both
/// tiers: baseline-only protection otherwise prevents larger callers from
/// optimizing when their view read becomes thin.
///
/// `CRATONVM_JIT_IR_VH_READ_BINDS` is the default-ON kill switch over every
/// optimizing-tier read bind (gcj j3/x2: it used to be the opt-in widening
/// as well, a mixed default its inventory row could not state); `=0`
/// withdraws the field / byte-array arm too.
pub fn varhandle_ir_read_shape(name: &str, desc: &str) -> bool {
    varhandle_judged_binds_enabled()
        && ir_direct_calls_enabled()
        && direct_jit_callee_calls_enabled()
        && varhandle_read_direct_helpers_enabled()
        && true
        // WARNING j27-user-flip-20261028 (default ON; =0 restores the old default)
        && (true
            || (desc.starts_with("(Ljava/nio/ByteBuffer;I)")
                && varhandle_byte_array_view_read_slot(name, desc).is_some()))
        && (varhandle_read_helper_slot(name, desc)
            .is_some_and(|slot| varhandle_read_slot_kind(slot) < 8)
            || varhandle_byte_array_view_read_slot(name, desc).is_some())
}

pub fn varhandle_ir_access_shape(name: &str, desc: &str) -> bool {
    varhandle_ir_read_shape(name, desc)
        || (ir_direct_calls_enabled()
            && direct_jit_callee_calls_enabled()
            && varhandle_write_direct_helpers_enabled()
            && varhandle_byte_buffer_view_write_slot(name, desc).is_some())
}
pub(crate) fn varhandle_ir_access_entry(
    name: &str,
    desc: &str,
    table: &DirectHelperTable,
    jdk_only: bool,
    admission: Option<&dyn Fn(&str, &str, &str) -> bool>,
) -> Option<usize> {
    if let Some(slot) = varhandle_byte_buffer_view_write_slot(name, desc) {
        if !varhandle_ir_access_shape(name, desc) || table.static_base_resolver_ctx == 0 {
            return None;
        }
        let entry = direct_native_helper(
            table.varhandle_byte_buffer_view_write[slot],
            jdk_only,
            admission,
            "java/lang/invoke/VarHandle",
            name,
            "([Ljava/lang/Object;)V",
        );
        return (entry != 0).then_some(entry);
    }
    varhandle_ir_read_entry(name, desc, table, jdk_only, admission)
}

/// `CRATONVM_JIT_SP_REF_ARRAYCOPY_DIRECT` (JIT r23 wave 3c, lane L5; perf,
/// default ON, `=0` restores the plain dispatch): the single-pass
/// `System.arraycopy` intrinsic's bail edge (a reference copy, a mismatch, a
/// bound the JDK throws on) calls the optimizing tier's direct helper
/// (`DirectHelperTable::system_arraycopy`, `jit_system_arraycopy_direct`),
/// which answers the reference copies that cannot throw itself and runs the
/// same native dispatch for everything else. Compile time only.
pub(crate) fn sp_ref_arraycopy_direct_enabled() -> bool {
    true
}

pub(crate) fn ir_arraycopy_entry(
    class: &str,
    name: &str,
    desc: &str,
    table: &DirectHelperTable,
    jdk_only: bool,
    admission: Option<&dyn Fn(&str, &str, &str) -> bool>,
) -> Option<usize> {
    if class != "java/lang/System"
        || name != "arraycopy"
        || desc != "(Ljava/lang/Object;ILjava/lang/Object;II)V"
        || !ir_direct_calls_enabled()
        || table.static_base_resolver_ctx == 0
        || !true
    {
        return None;
    }
    let entry = direct_native_helper(
        table.system_arraycopy,
        jdk_only,
        admission,
        class,
        name,
        desc,
    );
    (entry != 0).then_some(entry)
}

pub(crate) fn varhandle_ir_read_entry(
    name: &str,
    desc: &str,
    table: &DirectHelperTable,
    jdk_only: bool,
    admission: Option<&dyn Fn(&str, &str, &str) -> bool>,
) -> Option<usize> {
    if !varhandle_ir_read_shape(name, desc) || table.static_base_resolver_ctx == 0 {
        return None;
    }
    let entry = if let Some(slot) =
        varhandle_read_helper_slot(name, desc).filter(|&slot| varhandle_read_slot_kind(slot) < 8)
    {
        table.varhandle_read[slot]
    } else {
        table.varhandle_byte_array_view_read[varhandle_byte_array_view_read_slot(name, desc)?]
    };
    (entry != 0
        && (!jdk_only
            || admission.is_some_and(|admits| {
                admits(
                    "java/lang/invoke/VarHandle",
                    name,
                    "([Ljava/lang/Object;)Ljava/lang/Object;",
                )
            })))
    .then_some(entry)
}

/// Does the optimizing tier's `VarHandle` refusal (`ir_tier_attempt`, which
/// holds a method with a thin-bindable `VarHandle` site out of the optimizing
/// tier so its single-pass body keeps the bind) protect a bind the single-pass
/// ladder would actually make for the site `name``desc` (round 15 wave 4, lane
/// vh6; F1 of `r15w4-vh5-review-findings-CLOSED-20260930.md`)?
///
/// The ladder binds when the family's switch is on, the site has a slot, this
/// compile's `table` wires it (zero once the VM minted an exact handle, and for
/// a `REF_STRICT` read), the raw-callee gate is open
/// ([`direct_jit_callee_calls_enabled`]) and, under `--real-jdk`, the registry
/// admits the `Bridge` bind on the ERASED registered descriptor
/// ([`direct_native_helper`]'s question, asked here without its refusal
/// record). The refusal used to ask the first three only, so under
/// `CRATONVM_JIT_DIRECT_CALLEE_CALLS=0`, or `--real-jdk` with the native-shadow
/// dial armed, every method with a `VarHandle` access lost the optimizing tier
/// to protect nothing. `CRATONVM_JIT_IR_VH_REFUSAL_ASKS_ADMISSION=0` restores
/// the three-question refusal. Compile-time only (once per such site per
/// optimizing attempt), so read uncached.
pub(crate) fn varhandle_ir_refusal_protects_bind(
    name: &str,
    desc: &str,
    table: &DirectHelperTable,
    jdk_only: bool,
    admission: Option<&dyn Fn(&str, &str, &str) -> bool>,
) -> bool {
    let erased = if varhandle_read_direct_helpers_enabled()
        && (varhandle_read_helper_slot(name, desc)
            .is_some_and(|slot| table.varhandle_read[slot] != 0)
            || varhandle_byte_array_view_read_slot(name, desc)
                .is_some_and(|slot| table.varhandle_byte_array_view_read[slot] != 0))
    {
        "([Ljava/lang/Object;)Ljava/lang/Object;"
    } else if varhandle_write_direct_helpers_enabled()
        && (varhandle_write_helper_slot(name, desc)
            .is_some_and(|slot| table.varhandle_write[slot] != 0)
            || varhandle_byte_buffer_view_write_slot(name, desc)
                .is_some_and(|slot| table.varhandle_byte_buffer_view_write[slot] != 0))
    {
        "([Ljava/lang/Object;)V"
    } else if varhandle_cas_direct_helpers_enabled()
        && varhandle_cas_helper_slot(name, desc).is_some_and(|slot| table.varhandle_cas[slot] != 0)
    {
        "([Ljava/lang/Object;)Z"
    } else {
        return false;
    };
    direct_jit_callee_calls_enabled()
        && (!jdk_only
            || admission.is_some_and(|admits| admits("java/lang/invoke/VarHandle", name, erased)))
}

/// `CRATONVM_JIT_IR_VH_CONST_ADMIT` (nio21 wave 12 L2, **default ON**, `=0`
/// restores the method-level refusal): a method whose thin-bindable
/// `VarHandle` sites read a constant handle reaches the optimizing tier,
/// where `ir_lower::Lowerer::try_emit_ir_vh_const_prefix` serves the access
/// inline and the thin helper, bound as the IR call
/// ([`varhandle_ir_const_bind_entry`]), is the fallback -- the same helper the
/// single-pass ladder would have bound. Page:
/// `r21-varhandle-sites-hold-methods-out-of-the-optimizing-tier-FIXED-20261012.md`.
/// Read per compile.
pub(crate) fn ir_vh_const_admit_enabled() -> bool {
    ir_lower::vh_const_inline_enabled()
}

/// nio21 wave 12 L2: does the `VarHandle` access at `pc` (`name` + `desc`)
/// read a constant handle the optimizing tier's prefix serves? `planned` is
/// the method's own `getstatic`s of a filed constant-`VarHandle` field, as
/// `(pc, kind, class id, static field index)`. Admitted when the site has a
/// shape the prefix serves, the nearest planned `getstatic` before it is of
/// the site's kind -- the same guess the single-pass prefix makes -- and
/// `served(class id, static field index)` says the prefix would be emitted for
/// that handle (`ir_lower::vh_const_site_served`: the plan, the reference
/// facts and every compile-time screen of `vh_const_core`). The prefix itself
/// only fires on a handle operand that IS that `LoadStatic`, so a wrong guess
/// costs the bound thin helper (or, for a reference read, the call), never a
/// wrong access.
pub(crate) fn ir_vh_const_site_admitted(
    pc: usize,
    name: &str,
    desc: &str,
    planned: &[(usize, u8, u32, usize)],
    served: &dyn Fn(u32, usize) -> bool,
) -> bool {
    if planned.is_empty() || !ir_vh_const_admit_enabled() {
        return false;
    }
    let Some(shape) = ir_lower::vh_const_site_shape(name, desc) else {
        return false;
    };
    planned
        .iter()
        .filter(|&&(p, _, _, _)| p < pc)
        .max_by_key(|&&(p, _, _, _)| p)
        .is_some_and(|&(_, kind, class_id, field_index)| {
            kind == shape.kind && served(class_id, field_index)
        })
}

/// nio21 wave 12 L2: the thin helper an admitted optimizing-tier `VarHandle`
/// call is bound to, as the single-pass ladder binds it (`get*` with a
/// PRIMITIVE return, `set*`, `compareAndSet`), or `None`. Reference returns
/// keep their call (the read helper's return-root policy is the single-pass
/// door's). The ladder's own gates: the family's switch, a wired slot, the
/// raw-callee gate and, under `--real-jdk`, the registry's answer on the
/// ERASED registered descriptor ([`varhandle_ir_refusal_protects_bind`]).
pub(crate) fn varhandle_ir_const_bind_entry(
    name: &str,
    desc: &str,
    table: &DirectHelperTable,
    jdk_only: bool,
    admission: Option<&dyn Fn(&str, &str, &str) -> bool>,
) -> Option<usize> {
    if !ir_vh_const_admit_enabled()
        || !ir_direct_calls_enabled()
        || !direct_jit_callee_calls_enabled()
        || table.static_base_resolver_ctx == 0
    {
        return None;
    }
    let (entry, erased) = if let Some(slot) =
        varhandle_read_helper_slot(name, desc).filter(|&slot| {
            // JIT round 22 wave 7, lane sync (vm test
            // `jit_primitive_varhandle_reads::optimizing_varhandle_reads_preserve_judged_cold_paths`):
            // an explicit `CRATONVM_JIT_IR_VH_READ_BINDS=0` withdraws EVERY
            // optimizing-tier VarHandle read bind, this constant-handle arm's
            // too. It bound read sites past the kill switch (the same
            // `ir-varhandle-read BOUND` line as the first arm), so `=0` did not
            // restore the unbound reads. Unset or `1` keeps this arm as it was.
            varhandle_read_direct_helpers_enabled()
                && varhandle_read_slot_kind(slot) < 8
                && true
        }) {
        (
            table.varhandle_read[slot],
            "([Ljava/lang/Object;)Ljava/lang/Object;",
        )
    } else if let Some(slot) =
        varhandle_write_helper_slot(name, desc).filter(|_| varhandle_write_direct_helpers_enabled())
    {
        (table.varhandle_write[slot], "([Ljava/lang/Object;)V")
    } else if let Some(slot) =
        varhandle_cas_helper_slot(name, desc).filter(|_| varhandle_cas_direct_helpers_enabled())
    {
        (table.varhandle_cas[slot], "([Ljava/lang/Object;)Z")
    } else {
        return None;
    };
    (entry != 0
        && (!jdk_only
            || admission.is_some_and(|admits| admits("java/lang/invoke/VarHandle", name, erased))))
    .then_some(entry)
}

/// JIT round 20 wave 11 (lane pIndy, W14L4-2 = W12L2-3 of
/// `jit-r21-nio-proposals-w14-L4.md`): the ERASED registered descriptor of the
/// thin helper the optimizing tier's post-build bind loop binds a
/// `java/lang/invoke/VarHandle.<name><desc>` call to -- in ANY body of the
/// graph, the compiling method's own or a spliced callee's -- or `None`.
///
/// The questions of [`varhandle_ir_const_bind_entry`] (the loop's second arm)
/// that need no helper table: `CRATONVM_JIT_IR_VH_CONST_ADMIT` and the
/// constant prefix, IR direct calls, the raw-callee gate, the family's switch
/// and slot (a primitive read, a write, a `compareAndSet`). The VM's splice
/// resolver asks it (`varhandle_splice_keeps_thin_bind`, with this VM's
/// exact-handle gate and the registry's answer on the returned descriptor --
/// the table half) before refusing a callee to protect a bind the splice
/// would lose: a spliced call this answers for keeps the very helper, and a
/// constant handle's access is served inline by the prefix
/// (`ir_lower::Lowerer::try_emit_ir_vh_const_prefix`), which fires on a
/// spliced call as on an own one.
pub fn varhandle_ir_bound_family(name: &str, desc: &str) -> Option<&'static str> {
    if !ir_vh_const_admit_enabled()
        || !ir_direct_calls_enabled()
        || !direct_jit_callee_calls_enabled()
    {
        return None;
    }
    let read = varhandle_read_helper_slot(name, desc).is_some_and(|slot| {
        // As `varhandle_ir_const_bind_entry`'s read arm (JIT round 22 wave 7):
        // `CRATONVM_JIT_IR_VH_READ_BINDS=0` withdraws it, so the splice
        // resolver does not protect a bind the loop no longer makes.
        varhandle_read_direct_helpers_enabled()
            && varhandle_read_slot_kind(slot) < 8
            && true
    });
    if read {
        Some("([Ljava/lang/Object;)Ljava/lang/Object;")
    } else if varhandle_write_direct_helpers_enabled()
        && varhandle_write_helper_slot(name, desc).is_some()
    {
        Some("([Ljava/lang/Object;)V")
    } else if varhandle_cas_direct_helpers_enabled()
        && varhandle_cas_helper_slot(name, desc).is_some()
    {
        Some("([Ljava/lang/Object;)Z")
    } else {
        None
    }
}


/// Add the thin-bind dependency ([`VARHANDLE_THIN_BIND_DEPENDENCY`]) to a
/// compile's dependency list once, under
/// [`varhandle_thin_bind_retire_enabled`]. Called by every door right where it
/// binds a thin `VarHandle` helper.
pub fn add_varhandle_thin_bind_dependency(inlined_methods: &mut Vec<(String, String, String)>) {
    if !varhandle_thin_bind_retire_enabled() {
        return;
    }
    if inlined_methods
        .iter()
        .any(|(class_name, _, _)| class_name == VARHANDLE_THIN_BIND_DEPENDENCY)
    {
        return;
    }
    inlined_methods.push((
        VARHANDLE_THIN_BIND_DEPENDENCY.to_string(),
        "<thin-bind>".to_string(),
        "()V".to_string(),
    ));
}


/// Sites bound to a `VarHandle` read helper, split by compile door, so "did
/// this land" is answerable without a timing run. The OSR column is the one
/// that matters here: netty reads `refCnt` inside the transfer loops, and a
/// loop body is an OSR compilation.
pub static VARHANDLE_READ_SITES_SINGLEPASS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub static VARHANDLE_READ_SITES_OSR: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// `(single-pass, OSR)` `VarHandle` read sites bound to a thin direct helper.
pub fn varhandle_read_direct_helper_sites() -> (u64, u64) {
    (
        VARHANDLE_READ_SITES_SINGLEPASS.load(std::sync::atomic::Ordering::Relaxed),
        VARHANDLE_READ_SITES_OSR.load(std::sync::atomic::Ordering::Relaxed),
    )
}

/// How many `Thread.currentThread()` call sites the compiler has bound to the
/// thin direct helper, split by backend.
///
/// A *compile-time* counter, distinct from the VM's run-time
/// `JIT_FUNNEL_BYPASS_HITS`. The two answer different questions and both are
/// needed: sites-bound `== 0` means the recognition never ran, while
/// sites-bound `> 0` with run-time hits `== 0` means it ran and the emitted
/// call is not being taken. Nothing else separates those, and the first cut
/// of this bypass was inert in exactly the first way — for a reason no
/// timing could have shown (the optimizing tier resolves direct calls in its
/// own loop, which had no route to the thin helpers at all).
///
/// Reported by `CRATONVM_INTRINSIC_STATS=1`.
pub static THREAD_CURRENT_THREAD_SITES_SINGLEPASS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub static THREAD_CURRENT_THREAD_SITES_IR: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// How many `invokestatic` sites each backend's direct-call ladder actually
/// examined.
///
/// The denominators for the two counters above, and they exist because
/// "bound 0 sites" has two completely different causes that no other signal
/// separates: the ladder ran and the triple did not match (denominator > 0),
/// or the ladder never ran for this method at all (denominator == 0). The
/// first is a recognition bug; the second means the compile took a route
/// neither ladder is on. Diagnosing the inert first cut of the
/// `Thread.currentThread` bypass needed exactly this distinction.
pub static STATIC_SITES_SEEN_SINGLEPASS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub static STATIC_SITES_SEEN_IR: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// The OSR door's copy of the counter, incremented by the VM crate's
/// `jit_bridge::compile_osr_artifact` — the **third** compile door, which
/// reaches `x64::compile_with_param_slots` directly and carries its own
/// direct-call ladder.
///
/// It lives here, next to the other two, precisely because that door is easy
/// to forget: a `Thread.currentThread` bypass bound in `jit::try_compile`'s
/// two ladders and not in this one reported `0` bypasses on a loop making
/// 8,000,000 calls. Three doors, three binds, three counters in one line.
pub static THREAD_CURRENT_THREAD_SITES_OSR: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Statically bound call sites at which a ladder ASKED `callee_compiler` for a
/// direct target, split by whether it got one.
///
/// The question these answer is the one netty's census raised and no other
/// instrument could: `AdaptiveByteBufAllocatorTest` runs 259 M
/// `jit_invoke_dispatch` calls of which **98.4% are `DISPATCH_CACHE` hits** —
/// a compiled callee, reached through a Rust helper, on every call. The callee
/// is compiled; the caller simply could not bind it, because at the caller's
/// compile time it was not compiled YET, and a call site's binding is decided
/// once and never revisited. A miss here is that event, counted.
///
/// Compile-time only — one relaxed `fetch_add` per statically bound site per
/// compile, never on a runtime path.
pub static DIRECT_CALLEE_BIND_HITS: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub static DIRECT_CALLEE_BIND_MISSES: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// The OSR door's share of the two counters above — a SUBSET, not a third
/// total: `compile_osr_artifact`'s ladder bumps the shared pair and these.
///
/// The reason this split exists is a measurement that read as a working
/// feature. `osr-door-refused-every-exception-table-callee-FIXED-20260824.md` flipped
/// `CRATONVM_JIT_DIRECT_EXC_TABLE_PUBLISH` on and the OSR rung of
/// `probes/NativeFunnelFloorProbe.java` did not move (102.68 -> 100.26 ns/op),
/// while the census printed `direct callee binds: 0 bound, 0 left on the
/// dispatch helper` in BOTH arms. Zero sites examined and zero refusals tallied
/// is not "the gate is off", it is "this door never reported": the OSR ladder
/// bound and refused callees without touching either counter, so the one
/// instrument that could have named the gap said nothing. Splitting the door
/// out makes "the OSR ladder examined N sites" a readable number instead of an
/// inference from a flat timing.
pub static DIRECT_CALLEE_BIND_HITS_OSR: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub static DIRECT_CALLEE_BIND_MISSES_OSR: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Tally one statically bound OSR call site that DID get a direct `CALL`.
///
/// Bumps the shared pair too, so the census total stays "every site any ladder
/// asked about" rather than acquiring a third door nobody sums in.
#[inline]
pub fn note_osr_direct_callee_bind_hit() {
    DIRECT_CALLEE_BIND_HITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    DIRECT_CALLEE_BIND_HITS_OSR.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// Tally one statically bound OSR call site left on the dispatch helper, with
/// the reason, into the same table the other two doors use.
#[inline]
pub fn note_osr_direct_callee_bind_miss(reason: DirectBindRefusal) {
    DIRECT_CALLEE_BIND_MISSES.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    DIRECT_CALLEE_BIND_MISSES_OSR.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    note_direct_callee_bind_refusal(reason);
}

/// `(bound, unbound)` for the OSR door alone — a subset of
/// [`direct_callee_bind_counts`].
pub fn osr_direct_callee_bind_counts() -> (u64, u64) {
    (
        DIRECT_CALLEE_BIND_HITS_OSR.load(std::sync::atomic::Ordering::Relaxed),
        DIRECT_CALLEE_BIND_MISSES_OSR.load(std::sync::atomic::Ordering::Relaxed),
    )
}

/// Why a statically bound site was NOT offered a direct `CALL`, one counter per
/// refusal reason.
///
/// `DIRECT_CALLEE_BIND_MISSES` alone says a site stayed on the Rust dispatch
/// helper; it does not say WHICH gate refused, and the gates are not
/// interchangeable — "the callee was not compiled at that instant" is a
/// compile-ORDER accident that a re-bind can repair, while "the callee declares
/// an exception table" is a standing policy that no amount of re-binding
/// touches. A single number cannot be used to choose between those two fixes,
/// which is exactly the choice
/// `internal/performance/a-compiled-call-goes-out-to-rust-two-causes-RETIRED-20260817.md`
/// left open, and which this counter is what closed.
///
/// Compile-time only: one relaxed `fetch_add` per refused site per compile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub enum DirectBindRefusal {
    FjpBlocklist = 0,
    NativeShadow = 1,
    CalleeClassNotFound = 2,
    CalleeMethodNotFound = 3,
    Synchronized = 4,
    CalleeExceptionTable = 5,
    DeclaringClassNotInitialized = 6,
    CalleeNotYetCompiled = 7,
    EagerChainDepth = 8,
    EagerChainCycle = 9,
    EagerChainBudget = 10,
    EagerChainCompileDeclined = 11,
    IndyTrap = 12,
    /// The mutator-side `callee_compiler` door, which returns a bare `None`
    /// from several arms that have no separate reason string.
    MutatorDoorOther = 13,
    /// The callee's class NAME, resolved in the calling class's own loader,
    /// is a different class from the one the flat binary-name map answers:
    /// two loaders define the name and this caller sees the other copy. Every
    /// by-name compile or lookup would bind the wrong class's body.
    AnotherLoadersCopy = 14,
    /// The callee has not been called often enough to be worth compiling on its
    /// caller's behalf (`CRATONVM_JIT_EAGER_CHAIN_MIN_INVOCATIONS`). Not a late-bind
    /// waiter: a caller that stays hot is rebound by its own re-tier.
    EagerChainCold = 15,
    /// Interpreter round i7 wave 136, lane L2
    /// (`CRATONVM_JIT_IFACE_SPECIAL_RECEIVER_GUARD`): the callee is the target of
    /// an interface sender's `invokespecial` whose receiver is not proven to be
    /// `this`; the site must reach the VM's dispatch helper, which makes
    /// HotSpot's receiver check. A standing verdict, not a late-bind waiter.
    IfaceSpecialReceiverGuard = 16,
}

/// Names in `DirectBindRefusal` declaration order — index IS the discriminant.
pub const DIRECT_BIND_REFUSAL_NAMES: [&str; DIRECT_BIND_REFUSAL_COUNT] = [
    "fjp-blocklist",
    "native-shadow",
    "callee-class-not-found",
    "callee-method-not-found",
    "synchronized",
    "callee-exception-table",
    "declaring-class-not-initialized",
    "callee-not-yet-compiled",
    "eager-callee-chain-depth",
    "eager-callee-chain-cycle",
    "eager-callee-chain-budget",
    "eager-callee-chain-compile-declined",
    "indy-trap",
    "mutator-door-other",
    "another-loaders-copy",
    "eager-callee-chain-cold",
    "iface-special-receiver-guard",
];

pub const DIRECT_BIND_REFUSAL_COUNT: usize = 17;

pub static DIRECT_CALLEE_BIND_REFUSALS: [std::sync::atomic::AtomicU64; DIRECT_BIND_REFUSAL_COUNT] =
    [const { std::sync::atomic::AtomicU64::new(0) }; DIRECT_BIND_REFUSAL_COUNT];

/// Tally one refusal. Called from the VM crate's two `callee_compiler` doors.
#[inline]
pub fn note_direct_callee_bind_refusal(reason: DirectBindRefusal) {
    DIRECT_CALLEE_BIND_REFUSALS[reason as usize].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// `(reason, count)` for every reason with a non-zero count, largest first.
pub fn direct_callee_bind_refusal_reasons() -> Vec<(&'static str, u64)> {
    let mut out: Vec<(&'static str, u64)> = DIRECT_BIND_REFUSAL_NAMES
        .iter()
        .enumerate()
        .map(|(i, name)| {
            (
                *name,
                DIRECT_CALLEE_BIND_REFUSALS[i].load(std::sync::atomic::Ordering::Relaxed),
            )
        })
        .filter(|(_, count)| *count > 0)
        .collect();
    out.sort_by(|a, b| b.1.cmp(&a.1));
    out
}

/// `(bound, unbound)` — see [`DIRECT_CALLEE_BIND_HITS`].
pub fn direct_callee_bind_counts() -> (u64, u64) {
    (
        DIRECT_CALLEE_BIND_HITS.load(std::sync::atomic::Ordering::Relaxed),
        DIRECT_CALLEE_BIND_MISSES.load(std::sync::atomic::Ordering::Relaxed),
    )
}

/// `(single-pass, IR)` counts of `invokestatic` sites each direct-call ladder
/// examined since process start.
pub fn static_sites_seen() -> (u64, u64) {
    (
        STATIC_SITES_SEEN_SINGLEPASS.load(std::sync::atomic::Ordering::Relaxed),
        STATIC_SITES_SEEN_IR.load(std::sync::atomic::Ordering::Relaxed),
    )
}

/// `(single-pass, IR, OSR)` sites bound to the `Thread.currentThread` thin
/// direct helper since process start — one per compile door.
pub fn thread_current_thread_bound_sites() -> (u64, u64, u64) {
    (
        THREAD_CURRENT_THREAD_SITES_SINGLEPASS.load(std::sync::atomic::Ordering::Relaxed),
        THREAD_CURRENT_THREAD_SITES_IR.load(std::sync::atomic::Ordering::Relaxed),
        THREAD_CURRENT_THREAD_SITES_OSR.load(std::sync::atomic::Ordering::Relaxed),
    )
}

/// `CRATONVM_JIT='-census-direct-helpers'` — stop binding
/// `Preconditions.checkIndex` / `Reference.reachabilityFence` to their thin
/// direct helpers and send both back through the generic native funnel.
/// Default ON.
///
/// The switch exists so the blast radius of these binds can be measured on ONE
/// binary rather than argued across two. It was added after exactly that
/// mistake: a CratonBench A/B was run against a control binary a day older than
/// the branch, and reported ~13-19% "regressions" on `arithmetic` and `fib` —
/// phases that contain no `checkIndex` and no `reachabilityFence` call at all,
/// so the binds cannot have caused them. A same-binary A/B cannot be
/// confounded that way.
pub fn census_direct_helpers_enabled() -> bool {
    true
}

/// Sites bound to the two census-driven helpers above, split by compile door,
/// so "did this land" is answerable without a timing run — the lesson
/// `LEAF_NATIVE_HITS` was added for.
pub static PRECONDITIONS_CHECK_INDEX_SITES: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);
pub static REACHABILITY_FENCE_SITES: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// JIT round 16, lane strings (STR16-6): the
/// `DecimalDigits.uncheckedGetCharsLatin1` thin helpers' census, indexed by
/// [`DECIMAL_DIGITS_SITES_SP`] and [`DECIMAL_DIGITS_SITES_IR`] (sites bound,
/// counted at compile time). One array, one process static
/// (`process_global_statics_ratchet`); read it with [`decimal_digits_census`].
/// j27 wave 3 (lane BACKLOG, STR16-P7): the per-call served / declined slots
/// are gone -- the helper paid a shared-cache-line `fetch_add` on every call
/// for counters nothing read.
pub static DECIMAL_DIGITS_CENSUS: [std::sync::atomic::AtomicU64; 2] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];
/// Single-pass sites bound to a `decimal_digits_latin1_*` helper.
pub const DECIMAL_DIGITS_SITES_SP: usize = 0;
/// Optimizing-tier sites bound to a `decimal_digits_latin1_*` helper.
pub const DECIMAL_DIGITS_SITES_IR: usize = 1;

/// `(sites_sp, sites_ir)` of [`DECIMAL_DIGITS_CENSUS`].
pub fn decimal_digits_census() -> (u64, u64) {
    let read = |i: usize| DECIMAL_DIGITS_CENSUS[i].load(std::sync::atomic::Ordering::Relaxed);
    (read(DECIMAL_DIGITS_SITES_SP), read(DECIMAL_DIGITS_SITES_IR))
}

/// `CRATONVM_JIT_DECIMAL_DIGITS_DIRECT` (JIT round 16, lane strings, STR16-6;
/// default ON since the orchestrator's A/B; `=0` kills). Not memoised
/// (`process_global_statics_ratchet`): read once per candidate site of a
/// compile.
pub fn decimal_digits_direct_enabled() -> bool {
    true
}

/// The thin helper for an `invokestatic` of
/// `jdk/internal/util/DecimalDigits.uncheckedGetCharsLatin1` (`(II[B)I` or
/// `(JI[B)I`), or `0`. Both doors ask this, so they cannot disagree about the
/// shapes. Not a `direct_native_helper` question: the callee is plain JDK
/// bytecode with no registration to shadow, and the helper writes exactly
/// the bytes that bytecode writes (declining to it for every shape it does
/// not serve) -- the same standing as the inline `String.hashCode()` body.
pub fn decimal_digits_latin1_helper(
    table: &DirectHelperTable,
    class: &str,
    method: &str,
    descriptor: &str,
) -> usize {
    if class != "jdk/internal/util/DecimalDigits" || method != "uncheckedGetCharsLatin1" {
        return 0;
    }
    let entry = match descriptor {
        "(II[B)I" => table.decimal_digits_latin1_int,
        "(JI[B)I" => table.decimal_digits_latin1_long,
        _ => 0,
    };
    if entry != 0 && decimal_digits_direct_enabled() {
        entry
    } else {
        0
    }
}

/// `CRATONVM_JIT_BYTE_ARRAY_CLONE_DIRECT` (strings round 2, `str17`; default
/// ON, `=0` restores the inline-cache route). Not memoised: read once per
/// candidate site.
pub fn byte_array_clone_direct_enabled() -> bool {
    true
}

/// The thin helper for an `invokevirtual` of `[B.clone()Ljava/lang/Object;`,
/// or `0`. Both doors ask this.
///
/// An array receiver never sits in a monomorphic inline cache, so every
/// compiled `byte[].clone()` took `jit_invoke_virtual_mic` and its slow path
/// (~2 400 cycles), and JDK 25's `Arrays.copyOfRange(v, 0, v.length)` IS
/// `v.clone()`: every `StringBuilder.toString()` of a full builder (21 % of
/// `R16Bench strings`' iterations). The site's static type is `byte[]`, so
/// the target is `Object.clone` on a `byte[]` whatever the receiver; the
/// strict-mode admission asks about that native alone (not
/// `direct_native_helper_for_impl`, which first asks about the site's own
/// triple, and no registry row names `[B.clone`).
pub(crate) fn byte_array_clone_helper(
    table: &DirectHelperTable,
    jdk_only: bool,
    direct_bind_admission: Option<&dyn Fn(&str, &str, &str) -> bool>,
    class: &str,
    method: &str,
    descriptor: &str,
) -> usize {
    if class != "[B"
        || method != "clone"
        || descriptor != "()Ljava/lang/Object;"
        || !byte_array_clone_direct_enabled()
    {
        return 0;
    }
    direct_native_helper(
        table.byte_array_clone,
        jdk_only,
        direct_bind_admission,
        "java/lang/Object",
        method,
        descriptor,
    )
}

/// `CRATONVM_JIT_OBJECT_CLONE_DIRECT` (in-process `javac` residual,
/// 2026-10-27; default ON, `=0` restores the inline-cache route). Not
/// memoised: read once per candidate site.
pub fn object_clone_direct_enabled() -> bool {
    true
}

/// Whether `class.method descriptor` names `Object.clone()` itself. Bound to
/// [`object_clone_helper`] at an `invokespecial` only: that is `super.clone()`
/// reaching `Object`'s, whatever the receiver. A virtual call of the same
/// triple selects the receiver's own override.
pub(crate) fn object_clone_row(class: &str, method: &str, descriptor: &str) -> bool {
    class == "java/lang/Object" && method == "clone" && descriptor == "()Ljava/lang/Object;"
}

/// The thin helper for an `invokespecial java/lang/Object.clone()`, or `0`.
/// Every door asks this, at an `invokespecial` site only (see
/// [`object_clone_row`]).
///
/// The site took the inline-cache door into the registered native
/// (`safe_native_call`, a `NativeContext`, the class questions, a validated
/// copy): ~1.3 us a clone, and `TimeZone.getDefault()` clones once per call.
/// The helper serves a plain instance itself and declines everything else to
/// the generic dispatcher, so the native still decides those. The strict-mode
/// admission is the registry's (`direct_native_helper`).
pub(crate) fn object_clone_helper(
    table: &DirectHelperTable,
    jdk_only: bool,
    direct_bind_admission: Option<&dyn Fn(&str, &str, &str) -> bool>,
    class: &str,
    method: &str,
    descriptor: &str,
) -> usize {
    if !object_clone_row(class, method, descriptor) || !object_clone_direct_enabled() {
        return 0;
    }
    direct_native_helper(
        table.object_clone,
        jdk_only,
        direct_bind_admission,
        class,
        method,
        descriptor,
    )
}


/// `CRATONVM_JIT_VECTORIZED_HASH_DIRECT` (strings round 2, `str17`; default
/// ON, `=0` restores the generic door). Not memoised
/// (`process_global_statics_ratchet`): read once per candidate site.
pub fn vectorized_hash_direct_enabled() -> bool {
    true
}

/// The thin helper for an `invokestatic` of
/// `jdk/internal/util/ArraysSupport.vectorizedHashCode(Ljava/lang/Object;IIII)I`,
/// or `0`. Both doors ask this, so they cannot disagree about the shape.
///
/// Every `String.hashCode()` of a string whose hash is not cached yet ends
/// here (`StringLatin1.hashCode` -> `ArraysSupport.hashCodeOfUnsigned`, or
/// `StringUTF16.hashCode`) unless its own call site is the inline
/// `StringHashCode` intrinsic. The callee is a registered `Intrinsic` native,
/// so compiled code reached it through the generic dispatcher: one door per
/// fresh string, ~1 500 cycles for a 17-byte hash, most of
/// `R16Bench strings`. The helper folds the payload itself and declines every
/// shape it does not serve back to that dispatcher, so the native still
/// decides those. The strict-mode admission is the registry's
/// (`direct_native_helper`).
pub(crate) fn vectorized_hash_code_helper(
    table: &DirectHelperTable,
    jdk_only: bool,
    direct_bind_admission: Option<&dyn Fn(&str, &str, &str) -> bool>,
    class: &str,
    method: &str,
    descriptor: &str,
) -> usize {
    if class != "jdk/internal/util/ArraysSupport"
        || method != "vectorizedHashCode"
        || descriptor != "(Ljava/lang/Object;IIII)I"
        || !vectorized_hash_direct_enabled()
    {
        return 0;
    }
    direct_native_helper(
        table.vectorized_hash_code,
        jdk_only,
        direct_bind_admission,
        class,
        method,
        descriptor,
    )
}

/// `CRATONVM_JIT_REFLECT_NEW_ARRAY_DIRECT` (JIT round 23 wave 4, lane L5; perf,
/// default ON, `=0` restores the plain dispatch): both compile doors bind an
/// `invokestatic java/lang/reflect/Array.newArray(Class, int)` site -- the
/// native behind `Array.newInstance`, `Arrays.copyOf(T[], int)` and
/// `Collection.toArray(T[])` -- to the thin `jit_reflect_new_array_direct`
/// helper (`DirectHelperTable::reflect_new_array`), which allocates a
/// reference component's array from the caller's TLAB and dispatches the
/// registered native for everything else (every exception included). Read per
/// site at compile time.
pub(crate) fn reflect_new_array_direct_enabled() -> bool {
    true
}

/// The [`reflect_new_array_direct_enabled`] bind for `class.method descriptor`,
/// or `0`. The registered row is a `Bridge` for a JDK `native` method, so the
/// `--real-jdk` admission asks the same question every thin helper asks.
pub(crate) fn reflect_new_array_helper(
    table: &DirectHelperTable,
    jdk_only: bool,
    direct_bind_admission: Option<&dyn Fn(&str, &str, &str) -> bool>,
    class: &str,
    method: &str,
    descriptor: &str,
) -> usize {
    if class != "java/lang/reflect/Array"
        || method != "newArray"
        || descriptor != "(Ljava/lang/Class;I)Ljava/lang/Object;"
        || !reflect_new_array_direct_enabled()
    {
        return 0;
    }
    direct_native_helper(
        table.reflect_new_array,
        jdk_only,
        direct_bind_admission,
        class,
        method,
        descriptor,
    )
}

/// `CRATONVM_JIT_STRING_REGEX_DIRECT` (JIT round 23 wave 4, lane L5; perf,
/// default ON, `=0` restores the plain dispatch): both compile doors bind an
/// `invokevirtual` / `invokespecial` of `String.matches(String)`,
/// `replaceAll(String, String)` or `replaceFirst(String, String)` to a thin
/// helper (`DirectHelperTable::string_matches` / `string_replace`) that runs
/// the registered native's exact path without the dispatch funnel and
/// dispatches the native for everything else (every exception included).
/// `String` is final, so the triple names the call. Read per site at compile
/// time.
pub(crate) fn string_regex_direct_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_SPLICED_STRING_REGEX_DIRECT` (JIT round 23 wave 5, lane L5;
/// perf, default ON, `=0` leaves spliced sites on the inline cache): the
/// optimizing tier also binds a [`string_regex_helper`] site that a SPLICED
/// callee body left behind (its own sites keep the method single-pass, which
/// binds them). Needs [`string_regex_direct_enabled`] too. Read per compile.
pub(crate) fn ir_spliced_string_regex_direct_enabled() -> bool {
    true
}

/// The [`string_regex_direct_enabled`] bind for `class.method descriptor` as
/// `(entry, parameter words after the receiver, return type)`, or `None`. The
/// natives are reviewed `Intrinsic`s (`--real-jdk`) / the compatible rows of
/// the same functions; the admission asks what every thin helper asks.
pub(crate) fn string_regex_helper(
    table: &DirectHelperTable,
    jdk_only: bool,
    direct_bind_admission: Option<&dyn Fn(&str, &str, &str) -> bool>,
    class: &str,
    method: &str,
    descriptor: &str,
) -> Option<(usize, usize, u8)> {
    const REPLACE: &str = "(Ljava/lang/String;Ljava/lang/String;)Ljava/lang/String;";
    if class != "java/lang/String" || !string_regex_direct_enabled() {
        return None;
    }
    let (entry, params, ret) = match (method, descriptor) {
        ("matches", "(Ljava/lang/String;)Z") => (table.string_matches, 1, b'Z'),
        ("replaceAll", REPLACE) => (table.string_replace[0], 2, b'L'),
        ("replaceFirst", REPLACE) => (table.string_replace[1], 2, b'L'),
        _ => return None,
    };
    let entry =
        direct_native_helper(entry, jdk_only, direct_bind_admission, class, method, descriptor);
    (entry != 0).then_some((entry, params, ret))
}


/// `CRATONVM_JIT_IR_ARRAYS_EQUALS_CALL` (JIT round 22 wave 2, lane
/// intrinsics, the first step of INTRINSICS22-5; perf, **opt-in**): a
/// `java/util/Arrays.equals` site of the integral overloads the single-pass
/// backend answers inline (`JitIntrinsic::ArraysEquals1/2/4/8`) no longer
/// refuses its whole method the optimizing tier (`try_compile_inner`'s
/// loop-shaped-intrinsic refusal); there it stays an ordinary static call
/// (the JDK body, which ends in `ArraysSupport.vectorizedMismatch` -- a thin
/// helper under `CRATONVM_JIT_VECTORIZED_MISMATCH_DIRECT`). Exactly the shape
/// the planner already gives a cold OSR intrinsic site. Compile time only.
pub(crate) fn ir_arrays_equals_call_admits(class: &str, method: &str, descriptor: &str) -> bool {
    class == "java/util/Arrays"
        && method == "equals"
        && matches!(
            descriptor,
            "([B[B)Z" | "([Z[Z)Z" | "([C[C)Z" | "([S[S)Z" | "([I[I)Z" | "([J[J)Z"
        )
        // R22-AB-FLIP: default ON since the JIT r22 opt-in A/B (docs/feature-designs/jit-ir-optimizations.md); `=0` is the kill switch.
        && true
}

