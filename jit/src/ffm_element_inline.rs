//! JIT round 23 wave 4, stages 2 and 4 of L8-8 (lane L8;
//! `CRATONVM_JIT_IR_FFM_ELEMENT_INLINE`, opt-in): the compile-time facts the
//! optimizing tier's inline `MemorySegment` element access needs from the VM.
//!
//! The expansion of `seg.get(JAVA_INT, off)` (and `getAtIndex`, `set`, ...)
//! reads the segment's own fields and must know its layout argument as a
//! constant. Neither is something the JIT crate can find: the field slots are
//! the JDK's private `jdk/internal/foreign` fields, resolved by the VM's class
//! store, and a layout's width, alignment and byte order are values of a
//! `static final` object. Both travel through the one compile-time callback the
//! VM already publishes, `DirectHelperTable::well_known_class_id`, as tagged
//! query words (the `vh_const_query` encoding, two new tags):
//!
//! * [`WELL_KNOWN_FFM_SEGMENT_SLOT_TAG`]: `(segment class id, selector)` ->
//!   `slot + 1` of one segment field, or the class's kind
//!   ([`FFM_SEGMENT_SEL_KIND`]); `0` = unknown. The VM answers only for the
//!   JDK's own segment classes with real class bytes, resolving each field from
//!   its DECLARING class (so a subclass's same-named field cannot stand in).
//! * [`WELL_KNOWN_FFM_LAYOUT_FACTS_TAG`]: `(holder class id, static field
//!   index)` -> the [`FfmLayoutFacts`] word the VM filed when the holder's
//!   `<clinit>` completed (`vm::jit::helpers::note_class_initialized_ffm_layout_facts`),
//!   `0` = none.
//!
//! Everything here is pure data plumbing: no code is emitted. The consumer
//! (the `IrBuilder` expansion, stage 5) is the next step; see
//! `docs/feature-designs/jit-ir-optimizations.md`, L8-8.

use crate::ir_lower::vh_const_ask;

/// The query tag of a segment-field question (see the module doc). Disjoint
/// from the `vh_const_query` tags (`0x5648..=0x564D`) and the two pointer tags
/// (`0x5650`, `0x5651`).
pub const WELL_KNOWN_FFM_SEGMENT_SLOT_TAG: i64 = 0x5652_i64 << 48;
/// The query tag of a layout-facts question (see the module doc).
pub const WELL_KNOWN_FFM_LAYOUT_FACTS_TAG: i64 = 0x5653_i64 << 48;

/// Selector: `AbstractMemorySegmentImpl.length` (`J`).
pub const FFM_SEGMENT_SEL_LENGTH: u16 = 0;
/// Selector: `AbstractMemorySegmentImpl.readOnly` (`Z`).
pub const FFM_SEGMENT_SEL_READ_ONLY: u16 = 1;
/// Selector: `AbstractMemorySegmentImpl.scope` (`MemorySessionImpl`).
pub const FFM_SEGMENT_SEL_SCOPE: u16 = 2;
/// Selector: `HeapMemorySegmentImpl.offset` (`J`), heap classes only.
pub const FFM_SEGMENT_SEL_HEAP_OFFSET: u16 = 3;
/// Selector: `HeapMemorySegmentImpl.base` (`Object`), heap classes only.
pub const FFM_SEGMENT_SEL_HEAP_BASE: u16 = 4;
/// Selector: `NativeMemorySegmentImpl.min` (`J`), native classes only.
pub const FFM_SEGMENT_SEL_NATIVE_MIN: u16 = 5;
/// Selector: the class's kind, answered as a byte rather than a slot:
/// [`FFM_SEGMENT_KIND_NATIVE`] for `NativeMemorySegmentImpl` /
/// `MappedMemorySegmentImpl`, or the heap base array's element descriptor
/// (`B C S I F J D`) for `HeapMemorySegmentImpl$Of*`; `0` otherwise.
pub const FFM_SEGMENT_SEL_KIND: u16 = 6;
/// The kind answer of a native (or mapped) segment class.
pub const FFM_SEGMENT_KIND_NATIVE: u8 = b'N';

/// One segment field the VM resolves: `(selector, declaring class, name,
/// descriptor)`. JDK 25's field set (`javap -p`, java.base); the VM resolves
/// each from its declaring class with
/// `resolve_field_index_in_hierarchy_desc`, so a missing or retyped field
/// answers `0` and the expansion declines.
pub const FFM_SEGMENT_FIELD_ROWS: [(u16, &str, &str, &str); 6] = [
    (
        FFM_SEGMENT_SEL_LENGTH,
        "jdk/internal/foreign/AbstractMemorySegmentImpl",
        "length",
        "J",
    ),
    (
        FFM_SEGMENT_SEL_READ_ONLY,
        "jdk/internal/foreign/AbstractMemorySegmentImpl",
        "readOnly",
        "Z",
    ),
    (
        FFM_SEGMENT_SEL_SCOPE,
        "jdk/internal/foreign/AbstractMemorySegmentImpl",
        "scope",
        "Ljdk/internal/foreign/MemorySessionImpl;",
    ),
    (
        FFM_SEGMENT_SEL_HEAP_OFFSET,
        "jdk/internal/foreign/HeapMemorySegmentImpl",
        "offset",
        "J",
    ),
    (
        FFM_SEGMENT_SEL_HEAP_BASE,
        "jdk/internal/foreign/HeapMemorySegmentImpl",
        "base",
        "Ljava/lang/Object;",
    ),
    (
        FFM_SEGMENT_SEL_NATIVE_MIN,
        "jdk/internal/foreign/NativeMemorySegmentImpl",
        "min",
        "J",
    ),
];

/// The query word for `(tag, class id, index)`: the `vh_const_query` encoding.
pub const fn ffm_inline_query(tag: i64, class_id: u32, index: u16) -> i64 {
    crate::ir_lower::vh_const_query(tag, class_id, index)
}

/// The VM's decode of an [`ffm_inline_query`] word: `(tag, class id, index)`
/// for one of this module's two tags, `None` for every other code.
pub fn ffm_inline_query_decode(which: i64) -> Option<(i64, u32, u16)> {
    let tag = which & !0xFFFF_FFFF_FFFF_i64;
    if tag != WELL_KNOWN_FFM_SEGMENT_SLOT_TAG && tag != WELL_KNOWN_FFM_LAYOUT_FACTS_TAG {
        return None;
    }
    // Cast: the two masked fields fit their widths exactly.
    let class_id = ((which >> 16) & 0xFFFF_FFFF) as u32;
    let index = (which & 0xFFFF) as u16; // Cast: as above
    Some((tag, class_id, index))
}

/// The heap base array's element descriptor of a `HeapMemorySegmentImpl`
/// leaf class, by name, or `None` for any other class.
pub fn ffm_heap_segment_element(class_name: &str) -> Option<u8> {
    Some(
        match class_name.strip_prefix("jdk/internal/foreign/HeapMemorySegmentImpl$Of")? {
            "Byte" => b'B',
            "Char" => b'C',
            "Short" => b'S',
            "Int" => b'I',
            "Float" => b'F',
            "Long" => b'J',
            "Double" => b'D',
            _ => return None,
        },
    )
}

/// Is `class_name` a native segment class (`NativeMemorySegmentImpl`, or its
/// `MappedMemorySegmentImpl` subclass, which adds no field the access reads)?
pub fn ffm_native_segment_class(class_name: &str) -> bool {
    matches!(
        class_name,
        "jdk/internal/foreign/NativeMemorySegmentImpl"
            | "jdk/internal/foreign/MappedMemorySegmentImpl"
    )
}

/// The [`FFM_SEGMENT_SEL_KIND`] answer for `class_name` (`0` = not a segment
/// class the expansion serves).
pub fn ffm_segment_kind_answer(class_name: &str) -> u8 {
    if ffm_native_segment_class(class_name) {
        FFM_SEGMENT_KIND_NATIVE
    } else {
        ffm_heap_segment_element(class_name).unwrap_or(0)
    }
}

/// Which selectors a class of `kind` must answer: the three common fields plus
/// `offset`/`base` for a heap class, `min` for a native one.
pub fn ffm_segment_selector_applies(kind: u8, selector: u16) -> bool {
    match selector {
        FFM_SEGMENT_SEL_LENGTH | FFM_SEGMENT_SEL_READ_ONLY | FFM_SEGMENT_SEL_SCOPE => kind != 0,
        FFM_SEGMENT_SEL_HEAP_OFFSET | FFM_SEGMENT_SEL_HEAP_BASE => {
            kind != 0 && kind != FFM_SEGMENT_KIND_NATIVE
        }
        FFM_SEGMENT_SEL_NATIVE_MIN => kind == FFM_SEGMENT_KIND_NATIVE,
        FFM_SEGMENT_SEL_KIND => true,
        _ => false,
    }
}

/// Where a heap segment's address comes from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FfmSegmentBase {
    /// `HeapMemorySegmentImpl$Of*`: `base` (the array) at slot `base`, the
    /// byte offset into it at slot `offset`; `element` is the array's
    /// element descriptor (it decides the alignment the JDK admits:
    /// `maxAlignMask`).
    Heap { base: u16, offset: u16, element: u8 },
    /// `NativeMemorySegmentImpl`: the address at slot `min`.
    Native { min: u16 },
}

/// The field slots of one segment class, for the guarded reads
/// (`UnboxOp::FieldLong` / `FieldBool` / `FieldRef`) the expansion emits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FfmSegmentShape {
    pub class_id: u32,
    pub length: u16,
    pub read_only: u16,
    pub scope: u16,
    pub base: FfmSegmentBase,
}

/// Decode one `slot + 1` answer into a slot that fits the `UnboxOp` field
/// variants' `u16`.
fn slot_of(answer: i64) -> Option<u16> {
    u16::try_from(answer.checked_sub(1)?).ok()
}

/// The [`FfmSegmentShape`] of `class_id` in this compile's VM, or `None`
/// (switch off, no resolver, not a JDK segment class, a field missing).
pub fn ffm_segment_shape(
    helpers: &crate::DirectHelperTable,
    class_id: u32,
) -> Option<FfmSegmentShape> {
    if class_id == 0 || !crate::ir::ir_ffm_element_inline_enabled() {
        return None;
    }
    let ask = |sel: u16| {
        vh_const_ask(
            helpers,
            ffm_inline_query(WELL_KNOWN_FFM_SEGMENT_SLOT_TAG, class_id, sel),
        )
    };
    let kind = u8::try_from(ask(FFM_SEGMENT_SEL_KIND)).ok()?;
    let base = if kind == FFM_SEGMENT_KIND_NATIVE {
        FfmSegmentBase::Native {
            min: slot_of(ask(FFM_SEGMENT_SEL_NATIVE_MIN))?,
        }
    } else {
        let element = kind;
        if !matches!(element, b'B' | b'C' | b'S' | b'I' | b'F' | b'J' | b'D') {
            return None;
        }
        FfmSegmentBase::Heap {
            base: slot_of(ask(FFM_SEGMENT_SEL_HEAP_BASE))?,
            offset: slot_of(ask(FFM_SEGMENT_SEL_HEAP_OFFSET))?,
            element,
        }
    };
    Some(FfmSegmentShape {
        class_id,
        length: slot_of(ask(FFM_SEGMENT_SEL_LENGTH))?,
        read_only: slot_of(ask(FFM_SEGMENT_SEL_READ_ONLY))?,
        scope: slot_of(ask(FFM_SEGMENT_SEL_SCOPE))?,
        base,
    })
}

/// A `ValueLayout` constant's access facts: the carrier (a descriptor byte,
/// `Z B C S I F J D`), the byte width, the byte alignment and the byte order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FfmLayoutFacts {
    pub carrier: u8,
    pub width: u8,
    pub align: u32,
    pub big_endian: bool,
}

/// Bit 56 of a facts word: set in every valid word, so a filed word is never
/// the `0` that means "none".
const FACTS_VALID: u64 = 1 << 56;

impl FfmLayoutFacts {
    /// The word the VM files and the query answers.
    pub fn pack(self) -> i64 {
        let w = FACTS_VALID
            | u64::from(self.carrier)
            | (u64::from(self.width) << 8)
            | (u64::from(self.align) << 16)
            | (u64::from(self.big_endian) << 48);
        // Cast: bit 63 is clear, so the word is a positive i64.
        w as i64
    }

    /// The facts of a packed word, re-validated, or `None` (`0`, a malformed
    /// word).
    pub fn unpack(word: i64) -> Option<Self> {
        // Cast: the word's bits.
        let w = word as u64;
        if w & FACTS_VALID == 0 || w >> 57 != 0 || (w >> 49) & 0x7F != 0 {
            return None;
        }
        let carrier = (w & 0xFF) as u8; // Cast: masked byte
        let width = ((w >> 8) & 0xFF) as u8; // Cast: masked byte
        let align = ((w >> 16) & 0xFFFF_FFFF) as u32; // Cast: masked word
        let big_endian = (w >> 48) & 1 == 1;
        let facts = Self {
            carrier,
            width,
            align,
            big_endian,
        };
        (ffm_carrier_width(carrier) == Some(width) && align.is_power_of_two()).then_some(facts)
    }
}

/// The byte width of a carrier descriptor, or `None`.
pub fn ffm_carrier_width(carrier: u8) -> Option<u8> {
    Some(match carrier {
        b'Z' | b'B' => 1,
        b'C' | b'S' => 2,
        b'I' | b'F' => 4,
        b'J' | b'D' => 8,
        _ => return None,
    })
}

/// The carrier of a `ValueLayouts$Of*Impl` class, by name, or `None` (an
/// address layout, a struct/sequence layout, anything else: the expansion
/// serves primitive carriers only).
pub fn ffm_value_layout_carrier(class_name: &str) -> Option<u8> {
    Some(
        match class_name.strip_prefix("jdk/internal/foreign/layout/ValueLayouts$Of")? {
            "BooleanImpl" => b'Z',
            "ByteImpl" => b'B',
            "CharImpl" => b'C',
            "ShortImpl" => b'S',
            "IntImpl" => b'I',
            "FloatImpl" => b'F',
            "LongImpl" => b'J',
            "DoubleImpl" => b'D',
            _ => return None,
        },
    )
}

/// The facts the VM files for a layout object of class `class_name` whose
/// `AbstractLayout.byteSize` / `byteAlignment` and byte order it read, or
/// `None` when they do not describe a plain primitive access (a size that is
/// not the carrier's width, an alignment that is not a power of two). An
/// alignment WIDER than the width is filed as is: the expansion declines it
/// (`getAtIndex` refuses such a layout, and a heap segment's `maxAlignMask`
/// may too), as the VM's `segment_layout_facts` does.
pub fn ffm_layout_facts_from_fields(
    class_name: &str,
    byte_size: i64,
    byte_alignment: i64,
    big_endian: bool,
) -> Option<FfmLayoutFacts> {
    let carrier = ffm_value_layout_carrier(class_name)?;
    let width = ffm_carrier_width(carrier)?;
    if byte_size != i64::from(width) {
        return None;
    }
    let align = u32::try_from(byte_alignment).ok()?;
    if !align.is_power_of_two() {
        return None;
    }
    Some(FfmLayoutFacts {
        carrier,
        width,
        align,
        big_endian,
    })
}

/// The layout facts the VM filed for the `static final` field `(holder,
/// field_index)`, or `None` (switch off, nothing filed, no resolver).
pub fn ffm_layout_facts(
    helpers: &crate::DirectHelperTable,
    holder: u32,
    field_index: usize,
) -> Option<FfmLayoutFacts> {
    if holder == 0 || !crate::ir::ir_ffm_element_inline_enabled() {
        return None;
    }
    let index = u16::try_from(field_index).ok()?;
    FfmLayoutFacts::unpack(vh_const_ask(
        helpers,
        ffm_inline_query(WELL_KNOWN_FFM_LAYOUT_FACTS_TAG, holder, index),
    ))
}

// ── Stage 5: the site plan and the access plan ────────────────────────────

/// The accessor shape of a `MemorySegment` element site the inline expansion
/// serves: the carrier (descriptor byte), whether it stores, and whether its
/// coordinate is an element index (`getAtIndex` / `setAtIndex`) or a byte
/// offset (`get` / `set`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FfmElementAccessor {
    pub carrier: u8,
    pub is_put: bool,
    pub by_index: bool,
}

/// The [`FfmElementAccessor`] of a call `class.method desc`, or `None`.
///
/// The integral carriers `B S C I J`, and (the r23 L8 close, L8-8,
/// `CRATONVM_JIT_IR_FFM_ELEMENT_INLINE_FP`) `F` / `D`, which the builder
/// moves through their raw bits around `ScopedGet`/`ScopedPut`
/// (`Float.intBitsToFloat` / `floatToRawIntBits`, the JDK's own accessor
/// does the same through `Unsafe`, so a NaN keeps its payload). `Z` reads a
/// byte and compares it, and an address layout returns a fresh segment: both
/// keep their other routes.
pub fn ffm_element_accessor(class: &str, method: &str, desc: &str) -> Option<FfmElementAccessor> {
    if class != "java/lang/foreign/MemorySegment" {
        return None;
    }
    let (is_put, by_index) = match method {
        "get" => (false, false),
        "set" => (true, false),
        "getAtIndex" => (false, true),
        "setAtIndex" => (true, true),
        _ => return None,
    };
    let rest = desc.strip_prefix("(Ljava/lang/foreign/ValueLayout$Of")?;
    let (name, tail) = rest.split_once(";J")?;
    let carrier = match name {
        "Byte" => b'B',
        "Short" => b'S',
        "Char" => b'C',
        "Int" => b'I',
        "Long" => b'J',
        "Float" if ffm_element_inline_fp_enabled() => b'F',
        "Double" if ffm_element_inline_fp_enabled() => b'D',
        _ => return None,
    };
    let c = carrier as char;
    let want = if is_put {
        format!("{c})V")
    } else {
        format!("){c}")
    };
    (tail == want).then_some(FfmElementAccessor {
        carrier,
        is_put,
        by_index,
    })
}

/// `CRATONVM_JIT_IR_FFM_ELEMENT_INLINE_FP` (the r23 L8 close, L8-8): `float`
/// and `double` element accessors in the inline expansion. Default ON (perf);
/// `=0` kills it (they keep the direct bind). Read per compile.
pub fn ffm_element_inline_fp_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_FFM_ELEMENT_INLINE_FP")
}

/// One site the optimizing tier expands inline
/// (`IrBuilder::try_emit_ffm_element_inline`), planned in `lib.rs` from the
/// site's monomorphic receiver profile.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FfmElementInlineSite {
    pub shape: FfmSegmentShape,
    pub accessor: FfmElementAccessor,
    /// No other route serves the site in this compile (no direct bind): a
    /// builder decline must refuse the method (the single-pass route), never
    /// leave a plain dispatch.
    pub bail_on_decline: bool,
}

/// What the expansion emits for one site and one constant layout: the access
/// width and sign, the alignment guard's mask (`0` = no guard) and the byte
/// order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FfmInlineAccess {
    pub width: u8,
    pub signed: bool,
    pub align_mask: i64,
    pub big_endian: bool,
}

/// The [`FfmInlineAccess`] for `site` with layout `facts`, or `None` when the
/// expansion declines (it would only ever deopt, or the carrier disagrees):
///
/// * the layout's carrier must be the accessor's (a mismatch cannot pass the
///   verifier anyway, but the facts are data);
/// * `align <= width`: `getAtIndex` raises `IllegalArgumentException` for a
///   wider alignment (`Utils.checkElementAlignment`), and the JDK's alignment
///   test below is only the plain mask test under it;
/// * a heap segment also needs `align <=` its array element's width:
///   `isAlignedForElement` ORs `maxAlignMask()` (the element width) into the
///   address, so a wider alignment fails on every access.
///
/// The emitted alignment test is `(unsafeGetOffset() + off) & (align - 1) ==
/// 0`, which equals the JDK's `((unsafeGetOffset() + off) | maxAlignMask()) &
/// (align - 1) == 0` exactly when `align <= maxAlignMask()` (heap; the OR
/// sets no bit under the mask) or `maxAlignMask() == 0` (native).
pub fn ffm_inline_access_plan(
    site: &FfmElementInlineSite,
    facts: FfmLayoutFacts,
) -> Option<FfmInlineAccess> {
    let carrier = site.accessor.carrier;
    let width = ffm_carrier_width(carrier)?;
    if facts.carrier != carrier || facts.width != width || facts.align > u32::from(width) {
        return None;
    }
    if let FfmSegmentBase::Heap { element, .. } = site.shape.base {
        if facts.align > u32::from(ffm_carrier_width(element)?) {
            return None;
        }
    }
    Some(FfmInlineAccess {
        width,
        signed: carrier != b'C',
        align_mask: i64::from(facts.align) - 1,
        big_endian: facts.big_endian,
    })
}

#[cfg(test)]
mod r23w4_ffm_element_inline_plan_tests {
    use super::*;

    fn site(base: FfmSegmentBase, accessor: FfmElementAccessor) -> FfmElementInlineSite {
        FfmElementInlineSite {
            shape: FfmSegmentShape {
                class_id: 9,
                length: 0,
                read_only: 1,
                scope: 2,
                base,
            },
            accessor,
            bail_on_decline: true,
        }
    }

    #[test]
    fn accessors_parse_the_primitive_shapes_only() {
        const SEG: &str = "java/lang/foreign/MemorySegment";
        let a = |m: &str, d: &str| ffm_element_accessor(SEG, m, d);
        assert_eq!(
            a("getAtIndex", "(Ljava/lang/foreign/ValueLayout$OfInt;J)I"),
            Some(FfmElementAccessor {
                carrier: b'I',
                is_put: false,
                by_index: true
            })
        );
        assert_eq!(
            a("set", "(Ljava/lang/foreign/ValueLayout$OfLong;JJ)V"),
            Some(FfmElementAccessor {
                carrier: b'J',
                is_put: true,
                by_index: false
            })
        );
        assert_eq!(
            a("get", "(Ljava/lang/foreign/ValueLayout$OfChar;J)C").map(|x| x.carrier),
            Some(b'C')
        );
        // L8-8: the FP carriers, unless their switch is off.
        assert_eq!(
            a("getAtIndex", "(Ljava/lang/foreign/ValueLayout$OfFloat;J)F").map(|x| x.carrier),
            Some(b'F')
        );
        assert_eq!(
            a("set", "(Ljava/lang/foreign/ValueLayout$OfDouble;JD)V"),
            Some(FfmElementAccessor {
                carrier: b'D',
                is_put: true,
                by_index: false
            })
        );
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_IR_FFM_ELEMENT_INLINE_FP", Some("0"))],
            || {
                assert_eq!(
                    a("getAtIndex", "(Ljava/lang/foreign/ValueLayout$OfDouble;J)D"),
                    None
                );
            },
        );
        assert_eq!(
            a("get", "(Ljava/lang/foreign/ValueLayout$OfBoolean;J)Z"),
            None
        );
        assert_eq!(
            a(
                "get",
                "(Ljava/lang/foreign/AddressLayout;J)Ljava/lang/foreign/MemorySegment;"
            ),
            None
        );
        assert_eq!(
            a("get", "(Ljava/lang/foreign/ValueLayout$OfInt;J)J"),
            None,
            "wrong return"
        );
        assert_eq!(
            a("setAtIndex", "(Ljava/lang/foreign/ValueLayout$OfInt;JJ)V"),
            None,
            "wrong value"
        );
        assert_eq!(
            a("asSlice", "(Ljava/lang/foreign/ValueLayout$OfInt;J)I"),
            None
        );
        assert_eq!(
            ffm_element_accessor(
                "java/nio/ByteBuffer",
                "get",
                "(Ljava/lang/foreign/ValueLayout$OfInt;J)I"
            ),
            None
        );
    }

    #[test]
    fn the_access_plan_mirrors_the_jdk_alignment_rules() {
        let int_at = FfmElementAccessor {
            carrier: b'I',
            is_put: false,
            by_index: true,
        };
        let facts = |align: u32, be: bool| FfmLayoutFacts {
            carrier: b'I',
            width: 4,
            align,
            big_endian: be,
        };
        let native = site(FfmSegmentBase::Native { min: 3 }, int_at);
        assert_eq!(
            ffm_inline_access_plan(&native, facts(4, false)),
            Some(FfmInlineAccess {
                width: 4,
                signed: true,
                align_mask: 3,
                big_endian: false
            })
        );
        assert_eq!(
            ffm_inline_access_plan(&native, facts(1, true)).map(|p| (p.align_mask, p.big_endian)),
            Some((0, true)),
            "an unaligned layout owes no alignment guard"
        );
        // Wider than the width: getAtIndex raises.
        assert_eq!(ffm_inline_access_plan(&native, facts(8, false)), None);
        // Heap byte[]: maxAlignMask 1, so an aligned int layout always fails.
        let heap_b = site(
            FfmSegmentBase::Heap {
                base: 4,
                offset: 3,
                element: b'B',
            },
            int_at,
        );
        assert_eq!(ffm_inline_access_plan(&heap_b, facts(4, false)), None);
        assert!(ffm_inline_access_plan(&heap_b, facts(1, false)).is_some());
        // Heap int[]: an aligned int layout is served.
        let heap_i = site(
            FfmSegmentBase::Heap {
                base: 4,
                offset: 3,
                element: b'I',
            },
            int_at,
        );
        assert!(ffm_inline_access_plan(&heap_i, facts(4, false)).is_some());
        // Carrier mismatch.
        let mut long_facts = facts(4, false);
        long_facts.carrier = b'J';
        long_facts.width = 8;
        assert_eq!(ffm_inline_access_plan(&native, long_facts), None);
        // char is unsigned.
        let char_get = FfmElementAccessor {
            carrier: b'C',
            is_put: false,
            by_index: false,
        };
        let c = FfmLayoutFacts {
            carrier: b'C',
            width: 2,
            align: 2,
            big_endian: false,
        };
        assert_eq!(
            ffm_inline_access_plan(&site(FfmSegmentBase::Native { min: 3 }, char_get), c)
                .map(|p| p.signed),
            Some(false)
        );
    }
}

#[cfg(test)]
mod r23w4_ffm_element_inline_resolver_tests {
    use super::*;

    const HEAP_CID: u32 = 0x1234;
    const NATIVE_CID: u32 = 0x1235;
    const OTHER_CID: u32 = 0x1236;
    const HOLDER_CID: u32 = 0x2000;

    /// A fake `jit_well_known_class_id`: a heap `byte[]` segment class (slots
    /// length 0, readOnly 1, scope 2, offset 3, base 4), a native one (min 3),
    /// a class it does not know, and one filed layout (holder field 7: a
    /// big-endian `int`, align 4).
    unsafe extern "C" fn fake_well_known(ctx: i64, which: i64) -> i64 {
        assert_eq!(ctx, 0x77, "the VM context must be passed through");
        let Some((tag, cid, index)) = ffm_inline_query_decode(which) else {
            return 0;
        };
        if tag == WELL_KNOWN_FFM_LAYOUT_FACTS_TAG {
            return if cid == HOLDER_CID && index == 7 {
                FfmLayoutFacts {
                    carrier: b'I',
                    width: 4,
                    align: 4,
                    big_endian: true,
                }
                .pack()
            } else {
                0
            };
        }
        let kind = match cid {
            HEAP_CID => b'B',
            NATIVE_CID => FFM_SEGMENT_KIND_NATIVE,
            _ => 0,
        };
        if index == FFM_SEGMENT_SEL_KIND {
            return i64::from(kind);
        }
        if !ffm_segment_selector_applies(kind, index) {
            return 0;
        }
        match (kind, index) {
            (FFM_SEGMENT_KIND_NATIVE, FFM_SEGMENT_SEL_NATIVE_MIN) => 4,
            (_, sel) => i64::from(sel) + 1,
        }
    }

    fn helpers() -> crate::DirectHelperTable {
        let mut h = crate::DirectHelperTable::EMPTY;
        h.well_known_class_id = fake_well_known as *const () as usize; // Cast: fn -> slot
        h.static_base_resolver_ctx = 0x77;
        h
    }

    fn on<R>(f: impl FnOnce() -> R) -> R {
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_IR_FFM_ELEMENT_INLINE", Some("1"))],
            f,
        )
    }

    #[test]
    fn query_words_round_trip_and_stay_clear_of_the_vh_const_tags() {
        for tag in [
            WELL_KNOWN_FFM_SEGMENT_SLOT_TAG,
            WELL_KNOWN_FFM_LAYOUT_FACTS_TAG,
        ] {
            let w = ffm_inline_query(tag, 0xDEAD_BEEF, 0xFFFF);
            assert!(w > 0, "a query word is a positive i64");
            assert_eq!(ffm_inline_query_decode(w), Some((tag, 0xDEAD_BEEF, 0xFFFF)));
            assert_eq!(
                crate::ir_lower::vh_const_query_decode(w),
                None,
                "the VM's vh_const decoder must not claim this module's words"
            );
        }
        let vh =
            crate::ir_lower::vh_const_query(crate::ir_lower::WELL_KNOWN_VH_CONST_PLAN_TAG, 5, 6);
        assert_eq!(ffm_inline_query_decode(vh), None);
        assert_eq!(
            ffm_inline_query_decode(crate::direct_helpers::WELL_KNOWN_CLASS_INTEGER),
            None
        );
    }

    #[test]
    fn the_segment_shape_comes_from_the_vm_per_class() {
        let h = helpers();
        on(|| {
            assert_eq!(
                ffm_segment_shape(&h, HEAP_CID),
                Some(FfmSegmentShape {
                    class_id: HEAP_CID,
                    length: 0,
                    read_only: 1,
                    scope: 2,
                    base: FfmSegmentBase::Heap {
                        base: 4,
                        offset: 3,
                        element: b'B'
                    },
                })
            );
            assert_eq!(
                ffm_segment_shape(&h, NATIVE_CID).map(|s| s.base),
                Some(FfmSegmentBase::Native { min: 3 })
            );
            assert_eq!(ffm_segment_shape(&h, OTHER_CID), None, "an unknown class");
            assert_eq!(ffm_segment_shape(&h, 0), None);
            assert_eq!(
                ffm_segment_shape(&crate::DirectHelperTable::EMPTY, HEAP_CID),
                None,
                "no resolver"
            );
        });
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_IR_FFM_ELEMENT_INLINE", Some("0"))],
            || assert_eq!(ffm_segment_shape(&h, HEAP_CID), None, "the switch is off at =0"),
        );
    }

    #[test]
    fn layout_facts_come_from_the_filed_static_and_are_validated() {
        let h = helpers();
        on(|| {
            assert_eq!(
                ffm_layout_facts(&h, HOLDER_CID, 7),
                Some(FfmLayoutFacts {
                    carrier: b'I',
                    width: 4,
                    align: 4,
                    big_endian: true
                })
            );
            assert_eq!(ffm_layout_facts(&h, HOLDER_CID, 8), None, "nothing filed");
            assert_eq!(
                ffm_layout_facts(&h, HOLDER_CID, 0x1_0000),
                None,
                "index past u16"
            );
        });
        cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_IR_FFM_ELEMENT_INLINE", Some("0"))],
            || assert_eq!(ffm_layout_facts(&h, HOLDER_CID, 7), None, "the switch is off at =0"),
        );
    }

    #[test]
    fn facts_words_pack_and_reject_malformed_ones() {
        for (carrier, width) in [
            (b'Z', 1),
            (b'B', 1),
            (b'C', 2),
            (b'S', 2),
            (b'I', 4),
            (b'F', 4),
            (b'J', 8),
            (b'D', 8),
        ] {
            for align in [1u32, width.into(), 16] {
                for big_endian in [false, true] {
                    let f = FfmLayoutFacts {
                        carrier,
                        width,
                        align,
                        big_endian,
                    };
                    assert!(f.pack() > 0);
                    assert_eq!(FfmLayoutFacts::unpack(f.pack()), Some(f));
                }
            }
        }
        assert_eq!(FfmLayoutFacts::unpack(0), None);
        let good = FfmLayoutFacts {
            carrier: b'I',
            width: 4,
            align: 4,
            big_endian: false,
        }
        .pack();
        assert_eq!(
            FfmLayoutFacts::unpack(good & !(1 << 56)),
            None,
            "no valid bit"
        );
        assert_eq!(
            FfmLayoutFacts::unpack(good | (1 << 50)),
            None,
            "stray high bits"
        );
        // Width 8 under an int carrier.
        assert_eq!(FfmLayoutFacts::unpack((good & !0xFF00) | 0x0800), None);
        // Alignment 3.
        assert_eq!(
            FfmLayoutFacts::unpack((good & !(0xFFFF_FFFF << 16)) | (3 << 16)),
            None
        );
    }

    #[test]
    fn the_vm_side_classifiers_name_the_jdk_25_classes() {
        assert_eq!(
            ffm_segment_kind_answer("jdk/internal/foreign/HeapMemorySegmentImpl$OfByte"),
            b'B'
        );
        assert_eq!(
            ffm_segment_kind_answer("jdk/internal/foreign/HeapMemorySegmentImpl$OfLong"),
            b'J'
        );
        assert_eq!(
            ffm_segment_kind_answer("jdk/internal/foreign/HeapMemorySegmentImpl$OfDouble"),
            b'D'
        );
        assert_eq!(
            ffm_segment_kind_answer("jdk/internal/foreign/NativeMemorySegmentImpl"),
            b'N'
        );
        assert_eq!(
            ffm_segment_kind_answer("jdk/internal/foreign/MappedMemorySegmentImpl"),
            b'N'
        );
        assert_eq!(
            ffm_segment_kind_answer("jdk/internal/foreign/HeapMemorySegmentImpl"),
            0
        );
        assert_eq!(
            ffm_segment_kind_answer("jdk/internal/foreign/AbstractMemorySegmentImpl"),
            0
        );
        assert_eq!(
            ffm_segment_kind_answer("com/example/HeapMemorySegmentImpl$OfByte"),
            0
        );

        assert_eq!(
            ffm_value_layout_carrier("jdk/internal/foreign/layout/ValueLayouts$OfIntImpl"),
            Some(b'I')
        );
        assert_eq!(
            ffm_value_layout_carrier("jdk/internal/foreign/layout/ValueLayouts$OfBooleanImpl"),
            Some(b'Z')
        );
        assert_eq!(
            ffm_value_layout_carrier("jdk/internal/foreign/layout/ValueLayouts$OfAddressImpl"),
            None
        );
        assert_eq!(
            ffm_value_layout_carrier("jdk/internal/foreign/layout/StructLayoutImpl"),
            None
        );

        assert_eq!(
            ffm_layout_facts_from_fields(
                "jdk/internal/foreign/layout/ValueLayouts$OfLongImpl",
                8,
                8,
                false
            ),
            Some(FfmLayoutFacts {
                carrier: b'J',
                width: 8,
                align: 8,
                big_endian: false
            })
        );
        // JAVA_LONG_UNALIGNED: alignment 1.
        assert_eq!(
            ffm_layout_facts_from_fields(
                "jdk/internal/foreign/layout/ValueLayouts$OfLongImpl",
                8,
                1,
                true
            )
            .map(|f| (f.align, f.big_endian)),
            Some((1, true))
        );
        assert_eq!(
            ffm_layout_facts_from_fields(
                "jdk/internal/foreign/layout/ValueLayouts$OfIntImpl",
                8,
                4,
                false
            ),
            None
        );
        assert_eq!(
            ffm_layout_facts_from_fields(
                "jdk/internal/foreign/layout/ValueLayouts$OfIntImpl",
                4,
                6,
                false
            ),
            None
        );
        assert_eq!(
            ffm_layout_facts_from_fields(
                "jdk/internal/foreign/layout/ValueLayouts$OfIntImpl",
                4,
                0,
                false
            ),
            None
        );
    }

    #[test]
    fn the_field_rows_cover_every_selector_once() {
        let mut seen: Vec<u16> = FFM_SEGMENT_FIELD_ROWS.iter().map(|r| r.0).collect();
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), FFM_SEGMENT_FIELD_ROWS.len());
        for (sel, _, _, _) in FFM_SEGMENT_FIELD_ROWS {
            assert_ne!(sel, FFM_SEGMENT_SEL_KIND);
            assert!(
                ffm_segment_selector_applies(b'B', sel) || ffm_segment_selector_applies(b'N', sel),
                "selector {sel} applies to no kind"
            );
        }
    }
}

/// t1 wave 6 (lane J6, FFM27-1 of
/// `docs/internal/proposals/jit/tier1/jit-r27-loops-and-ffm-residuals-proposals.md`):
/// which of `crate::ir_ffm_element_inline_plan`'s conditions an element site
/// failed. The planner prints it under `CRATONVM_DBG_JITC` for every element
/// site it gives no plan (`[cratonvm-jitc] ir-ffm-element-plan MISS ...`), so a
/// `--compatible` method that lands on the refusal route says why.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FfmElementPlanMiss {
    /// The compile door found the site unresolved (a first-call site).
    FirstCall,
    /// Not an `invokevirtual` / `invokeinterface`.
    Opcode(u8),
    /// `CRATONVM_JIT_IR_FFM_ELEMENT_INLINE` is off.
    InlineOff,
    /// Not an accessor shape the expansion serves ([`ffm_element_accessor`]).
    NotAnAccessor,
    /// The method reads no filed layout of the site's carrier.
    NoLayoutFact { carrier: u8 },
    /// No receiver row at the site (the unreached trap's case).
    NoReceiverRow,
    /// A receiver row with no observation in it (the unreached trap's case).
    EmptyReceiverRow,
    /// Receivers, but neither the guarded one-class rule nor (after the
    /// unreached trap) the all-one-class rule names a class.
    ReceiversNotOneClass {
        observations: u32,
        types: usize,
        top_class: u32,
        top_count: u32,
        after_unreached_trap: bool,
    },
    /// One class, but not a JDK segment class the VM describes
    /// ([`ffm_segment_shape`] declined it).
    NotADescribedSegment { class_id: u32 },
}

impl std::fmt::Display for FfmElementPlanMiss {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match *self {
            Self::FirstCall => write!(f, "first-call site"),
            Self::Opcode(op) => write!(f, "opcode {op:#x} is not invokevirtual/invokeinterface"),
            Self::InlineOff => write!(f, "CRATONVM_JIT_IR_FFM_ELEMENT_INLINE off"),
            Self::NotAnAccessor => write!(f, "not an expandable element accessor"),
            Self::NoLayoutFact { carrier } => {
                write!(f, "no filed layout fact of carrier {}", carrier as char)
            }
            Self::NoReceiverRow => write!(f, "no receiver row"),
            Self::EmptyReceiverRow => write!(f, "empty receiver row"),
            Self::ReceiversNotOneClass {
                observations,
                types,
                top_class,
                top_count,
                after_unreached_trap,
            } => write!(
                f,
                "receivers not one class: observations={observations} types={types} \
                 top=class {top_class} x{top_count} after_unreached_trap={after_unreached_trap}"
            ),
            Self::NotADescribedSegment { class_id } => {
                write!(f, "receiver class id {class_id} is not a described segment class")
            }
        }
    }
}

/// t1 wave 6 (lane J6, FFM27-1): the first of
/// `crate::ir_ffm_element_inline_plan`'s conditions this site fails, in that
/// function's order, or `None` when every one holds (the plan is made).
/// `described(class_id)` is the compile's `ffm_segment_shape(table,
/// class_id).is_some()`. Diagnostic only: the planner calls it under
/// `CRATONVM_DBG_JITC`, never to decide anything.
#[allow(clippy::too_many_arguments)]
pub fn ffm_element_plan_miss(
    class: &str,
    method: &str,
    desc: &str,
    opcode: u8,
    pc: usize,
    first_call: bool,
    profile: Option<&crate::profile::MethodProfile>,
    layout_facts: &std::collections::HashMap<(u32, u32), FfmLayoutFacts>,
    after_unreached_trap: bool,
    described: impl Fn(u32) -> bool,
) -> Option<FfmElementPlanMiss> {
    if first_call {
        return Some(FfmElementPlanMiss::FirstCall);
    }
    if !matches!(opcode, 0xb6 | 0xb9) {
        return Some(FfmElementPlanMiss::Opcode(opcode));
    }
    if !crate::ir::ir_ffm_element_inline_enabled() {
        return Some(FfmElementPlanMiss::InlineOff);
    }
    let Some(accessor) = ffm_element_accessor(class, method, desc) else {
        return Some(FfmElementPlanMiss::NotAnAccessor);
    };
    if !layout_facts.values().any(|f| f.carrier == accessor.carrier) {
        return Some(FfmElementPlanMiss::NoLayoutFact {
            carrier: accessor.carrier,
        });
    }
    let Some(counts) = profile.and_then(|p| p.receivers.get(&pc)) else {
        return Some(FfmElementPlanMiss::NoReceiverRow);
    };
    let evidence = crate::profile::ReceiverEvidence::of(counts);
    let summary = evidence.summary();
    if summary.observations == 0 {
        return Some(FfmElementPlanMiss::EmptyReceiverRow);
    }
    let one_class = crate::ir::profile_guarded_receiver_class(profile, pc).or_else(|| {
        let (class_id, count) = summary.top?;
        (after_unreached_trap && count == summary.observations && class_id != 0)
            .then_some(class_id)
    });
    let Some(class_id) = one_class else {
        let (top_class, top_count) = summary.top.unwrap_or((0, 0));
        return Some(FfmElementPlanMiss::ReceiversNotOneClass {
            observations: summary.observations,
            types: summary.types,
            top_class,
            top_count,
            after_unreached_trap,
        });
    };
    if !described(class_id) {
        return Some(FfmElementPlanMiss::NotADescribedSegment { class_id });
    }
    None
}

/// `CRATONVM_JIT_IR_FFM_SEED_CALL_TEXTUAL_LOOP` (t1 wave 7, lane J6;
/// pathology fix, default ON since j27 2026-10-28, `=0` kills): see
/// [`ffm_seed_call_site_arms_in_loop`]. Read per armed site at compile time;
/// no static.
pub fn ffm_seed_call_textual_loop_enabled() -> bool {
    // WARNING j27-user-flip-20261028 (default ON; =0 restores the old default)
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_FFM_SEED_CALL_TEXTUAL_LOOP")
}

/// t1 wave 7 (lane J6): does an FFM SEED-CALL site (the planner's
/// `ir_ffm_seed_call_sites`: its unreached trap fired, the recompile found no
/// plan) count as "in a loop" for the inline-cache profile seed, whatever the
/// branch profile says?
///
/// The pathology (`T1J6FfmSeedRows mono-native-put`, 32.8 us/op on the Linux
/// host against HotSpot's 19 ns; 50-70 us locally): a kernel first called with
/// `n = 0` compiles before its loop body ever ran, so its element site gets the
/// unreached trap. The first real call fires it, and the eager recompile reads
/// the profile before the resumed interpreter recorded a receiver: the seed
/// call's case. But the seed planner asks the BRANCH profile whether the site
/// is in a loop (`ir_splice_order::profile_plan_order`), and that profile still
/// says the loop body never ran (`16:C`, cold). So the seed was never armed,
/// no recompile was ever asked, and the site stayed a generic inline-cache call
/// into the JDK accessor for the life of the body (~0.8 us per element). Which
/// runs lost depended on whether the interpreter recorded a receiver before
/// the recompile read the profile: bimodal per run.
///
/// The trap firing is itself the proof the site runs; the textual loop
/// ([`crate::bytecode_pc_in_loop`]) is the only question left. `false` for any
/// other site and with the switch off (the default).
pub fn ffm_seed_call_site_arms_in_loop(
    seed_call_site: bool,
    code: &[u8],
    code_len: usize,
    pc: usize,
) -> bool {
    seed_call_site
        && ffm_seed_call_textual_loop_enabled()
        && crate::bytecode_pc_in_loop(code, code_len, pc)
}

#[cfg(test)]
mod t1w7_j6_seed_call_loop_tests {
    use super::ffm_seed_call_site_arms_in_loop;

    const SW: &str = "CRATONVM_JIT_IR_FFM_SEED_CALL_TEXTUAL_LOOP";

    /// With the switch on, a seed-call site inside a textual loop arms, whatever
    /// the branch profile said; outside a loop, not a seed-call site, or with
    /// the switch off (the default), it does not.
    #[test]
    fn a_seed_call_site_in_a_textual_loop_arms_the_ic_seed() {
        // pc 0: nop; pc 1: nop (the site); pc 2: goto -2 (back to 0).
        let looped: [u8; 5] = [0x00, 0x00, 0xa7, 0xff, 0xfe];
        // pc 0: nop; pc 1: nop; pc 2: return.
        let straight: [u8; 3] = [0x00, 0x00, 0xb1];
        let ask = |v: Option<&str>, seed: bool, code: &[u8], pc: usize| {
            cratonvm_types::flags::with_thread_overrides(&[(SW, v)], || {
                ffm_seed_call_site_arms_in_loop(seed, code, code.len(), pc)
            })
        };
        assert!(ask(Some("1"), true, &looped, 1), "on, in the loop");
        assert!(!ask(Some("1"), true, &straight, 1), "no back edge");
        assert!(!ask(Some("1"), false, &looped, 1), "not a seed-call site");
        assert!(!ask(Some("0"), true, &looped, 1), "off at `=0`");
        assert!(ask(None, true, &looped, 1), "default ON since j27 2026-10-28");
    }
}

/// `CRATONVM_JIT_IR_FFM_RECEIVED_SEED_CALL` (t1 wave 6, lane J6, FFM27-1;
/// perf, **opt-in** `=1`): see [`ffm_element_received_seed_call_site`]. Read
/// per site at compile time; no static.
pub fn ffm_received_seed_call_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_IR_FFM_RECEIVED_SEED_CALL")
}

/// t1 wave 6 (lane J6, FFM27-1): should this element site, whose unreached
/// trap fired (`despec_at_pc`) and whose receiver row now HAS receivers but
/// gave no plan, stay an inline-cache call (the planner's seed-call set,
/// `crate::ir_ffm_element_seed_call_site`'s route) instead of refusing the
/// whole method to its single-pass body?
///
/// The case (round 27 closing battery, `--compatible`): the trap fires, the
/// eager recompile runs, and the row is neither empty (the seed call's case)
/// nor plannable; the method was refused and stayed on its first-tier body for
/// good (`R26SegVecPut native-put-add` 2657 ns/op against 258,
/// `R23SegMono --compatible nativeget` 4822). Kept a call, the method gets the
/// optimizing tier, and when the row is still thin (fewer than
/// `MIN_GUARDED_RECEIVER_EVIDENCE` observations) inside a loop the planner's
/// ordinary rule arms the cache's profile seed, whose one recompile may then
/// plan the expansion.
///
/// Opt-in: a site whose row never becomes plannable stays a generic
/// inline-cache call, which r23 w2d measured slower than the single-pass
/// intrinsic's helper road (`CRATONVM_JIT_IR_FFM_ELEMENT_CALL`, segoff 8.4 ->
/// 114 us/op). It needs the seed call's own switches
/// (`CRATONVM_JIT_IR_FFM_UNREACHED_SITE_TRAP`,
/// `CRATONVM_JIT_IR_FFM_UNREACHED_SEED_CALL`, `CRATONVM_JIT_IR_IC_PROFILE_SEED`).
#[allow(clippy::too_many_arguments)]
pub fn ffm_element_received_seed_call_site(
    class: &str,
    method: &str,
    desc: &str,
    opcode: u8,
    pc: usize,
    first_call: bool,
    profile: Option<&crate::profile::MethodProfile>,
    despec_at_pc: bool,
) -> bool {
    if first_call
        || !despec_at_pc
        || !matches!(opcode, 0xb6 | 0xb9)
        || !ffm_received_seed_call_enabled()
        || !crate::ir::ir_ffm_element_inline_enabled()
        || !crate::ir_ffm_unreached_site_trap_enabled()
        || !crate::ir_ffm_unreached_seed_call_enabled()
        || !crate::ir_ic_profile_seed_enabled()
        || ffm_element_accessor(class, method, desc).is_none()
    {
        return false;
    }
    // t1 wave 8 (lane FFM27): only a THIN row (fewer than
    // `MIN_GUARDED_RECEIVER_EVIDENCE` observations), the one the planner's
    // inline-cache profile seed can still arm on and whose seed recompile may
    // then plan. A row at or past the evidence bar that still gives no plan
    // (two classes with the bimorphic expansion off, an undescribed class)
    // never becomes plannable through the seed (`ir_ic_profile_seed_planned`
    // refuses it), so keeping it a call left a generic inline-cache dispatch
    // into the JDK accessor for good: `T1J6FfmSeedRows bi-mixed-put` ~34 us/op
    // with this switch on, slower than the single-pass refusal route it
    // replaced. Such a row keeps that route now.
    profile.and_then(|p| p.receivers.get(&pc)).is_some_and(|counts| {
        let observations = crate::profile::ReceiverEvidence::of(counts)
            .summary()
            .observations;
        observations > 0 && observations < crate::MIN_GUARDED_RECEIVER_EVIDENCE
    })
}

/// `CRATONVM_JIT_IR_FFM_BIMORPHIC_AFTER_TRAP` (t1 wave 8, lane FFM27, FFM27-2;
/// perf, **default ON since j27 2026-10-28**, `=0` kills, and only with
/// `CRATONVM_JIT_IR_FFM_ELEMENT_BIMORPHIC` on): see [`ffm_bimorphic_after_trap_pair`]. Read per site at compile time;
/// no static.
pub fn ffm_bimorphic_after_trap_enabled() -> bool {
    // WARNING j27-user-flip-20261028 (default ON; =0 restores the old default)
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_FFM_BIMORPHIC_AFTER_TRAP")
}

/// What the de-spec registry withdrew at an element site whose unreached trap
/// fired, as far as the bimorphic plan cares ([`ffm_after_trap_despec`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FfmAfterTrapDespec {
    /// Only the site's own unreached trap.
    TrapOnly,
    /// The trap, and the exact-class test of a one-class expansion
    /// (`ReceiverTypeChanged` at the bci, past `PER_BCI_DESPEC_LIMIT`
    /// charges): another class kept arriving. The bimorphic plan then needs
    /// a row of exactly two classes ([`ffm_bimorphic_after_trap_pair`]), so
    /// a third class ends the expansion instead of deopting it forever.
    TrapAndClassMiss,
}

/// t1 wave 8 (lane FFM27, FFM27-2): may the bimorphic plan stand at this
/// element site although the de-spec registry withdrew speculations at its
/// `pc` (`despec_ids`, the ids `DespecRegistry::entries_for` lists at that
/// bci)? Only when the site's OWN unreached-site trap is among them -- an
/// `UnstableIf` guard at the call's bci (`IrBuilder::try_emit_ffm_unreached_trap`,
/// stamped `UnstableIf` by the lowering, withdrawn after one charge) -- and
/// the only other id is the expansion's exact-class test
/// (`ReceiverTypeChanged`, the `Op::Unbox` class guard's reason).
///
/// The round 24 planner skipped the bimorphic expansion at ANY de-spec'd bci,
/// and the unreached trap's own firing is one: a kernel first compiled before
/// its loop ran could therefore never get the bimorphic expansion. HotSpot
/// keeps them apart: an `unreached` / `unstable_if` trap says nothing about
/// the receiver types, and a `class_check` trap past `PerBytecodeTrapLimit`
/// makes C2 stop trapping on the miss, not stop inlining the profiled pair.
///
/// `None` for an empty list, without the trap's id, for the wildcard
/// (`SPECULATION_ID_ANY`: the failing speculation could not be named), and
/// when anything else was withdrawn here: the expansion's bounds / read-only
/// / alignment guards (`UncommonTrap`), its null test (`NullCheck`), any
/// other speculation -- the repeated failures the round 24 rule protects.
pub fn ffm_after_trap_despec(despec_ids: &[u32], pc: usize) -> Option<FfmAfterTrapDespec> {
    use crate::deopt::{speculation_id, DeoptReason};
    let bci = u32::try_from(pc).ok()?;
    let trap = speculation_id(bci, DeoptReason::UnstableIf);
    let class_miss = speculation_id(bci, DeoptReason::ReceiverTypeChanged);
    if !despec_ids.contains(&trap) || despec_ids.iter().any(|&id| id != trap && id != class_miss)
    {
        return None;
    }
    Some(if despec_ids.contains(&class_miss) {
        FfmAfterTrapDespec::TrapAndClassMiss
    } else {
        FfmAfterTrapDespec::TrapOnly
    })
}

/// `CRATONVM_JIT_IR_FFM_ELEMENT_WITHDRAWN_GUARDS` (t1 wave 9, lane JITA;
/// `t1-FFM27-ffm-element-plan-ignores-withdrawn-guards`; policy, **default
/// ON**, `=0` kill): the one-class element plan
/// (`ir_ffm_element_inline_plan`) stands down at a site where the VM already
/// withdrew one of its guards ([`ffm_element_guards_withdrawn`]). Without it
/// every recompile planted the same bounds / read-only / alignment / null /
/// class guards again, so a site that keeps failing one (a kernel that catches
/// `IndexOutOfBoundsException`, writes a read-only view, passes `null`, or a
/// third class) traded one deopt and one recompile per trap until the deopt
/// budget made the method not compilable. HotSpot's C2 recompiles such a bci
/// without the failing speculation (`too_many_traps`) and keeps the method
/// compiled. Answers cannot change: the site keeps the call row (or the
/// refusal route) the planner gives any unplanned site. Read per site at
/// compile time; no static.
pub fn ffm_element_withdrawn_guards_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_FFM_ELEMENT_WITHDRAWN_GUARDS")
}

/// t1 wave 9 (lane JITA, [`ffm_element_withdrawn_guards_enabled`]): did the
/// de-spec registry withdraw, at an element site's `pc`, anything other than
/// the site's OWN unreached-site trap (`UnstableIf` at the bci, withdrawn after
/// its one charge; it says nothing about the expansion's guards, and it is what
/// lets the one-class plan widen after the trap)? `despec_ids` are the ids
/// `DespecRegistry::entries_for` lists at that bci. The wildcard
/// (`SPECULATION_ID_ANY`: the VM could not name the failing reason) counts as
/// withdrawn, as it does for `DespecRegistry::contains_speculation`.
pub fn ffm_element_guards_withdrawn(despec_ids: &[u32], pc: usize) -> bool {
    use crate::deopt::{speculation_id, DeoptReason};
    let Ok(bci) = u32::try_from(pc) else {
        return !despec_ids.is_empty();
    };
    let trap = speculation_id(bci, DeoptReason::UnstableIf);
    despec_ids.iter().any(|&id| id != trap)
}

/// `CRATONVM_JIT_IR_FFM_ELEMENT_WITHDRAWN_OWN_GUARDS_ONLY` (j27 lane
/// HANGHUNT, page `docs/internal/fixed-bugs/jit/j27-seglongloops-heap-sum-bistable-slow-state-FIXED-20261028.md`
/// candidate 1; perf, **opt-in** `=1`, pending the host A/B of the
/// `R26SegLongLoops` hang rate): see [`ffm_element_plan_withdrawn`]. Read per
/// de-spec'd element site at compile time; no static.
pub fn ffm_element_withdrawn_own_guards_only_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_IR_FFM_ELEMENT_WITHDRAWN_OWN_GUARDS_ONLY")
}

/// Does the de-spec registry's list at an element site's `pc` stand its plan
/// down? Off: [`ffm_element_guards_withdrawn`] (any id but the site's own
/// unreached trap, the wildcard included). On: only an id the expansion itself
/// plants at this bci -- its bounds / read-only / alignment guards
/// (`UncommonTrap`), its null test (`NullCheck`), its exact-class test
/// (`ReceiverTypeChanged`). The wildcard, charged by exits no expansion guard
/// owns, then no longer removes the expansion for the life of the VM (the
/// registry never forgets). Risk: an own-guard failure the VM could only
/// charge as the wildcard is planted again; the per-method deopt budget bounds
/// that.
pub fn ffm_element_plan_withdrawn(despec_ids: &[u32], pc: usize) -> bool {
    if !ffm_element_withdrawn_own_guards_only_enabled() {
        return ffm_element_guards_withdrawn(despec_ids, pc);
    }
    use crate::deopt::{speculation_id, DeoptReason};
    let Ok(bci) = u32::try_from(pc) else {
        return !despec_ids.is_empty();
    };
    let own = [
        DeoptReason::UncommonTrap,
        DeoptReason::NullCheck,
        DeoptReason::ReceiverTypeChanged,
    ]
    .map(|reason| speculation_id(bci, reason));
    despec_ids.iter().any(|id| own.contains(id))
}

// ── j27 wave 4 (lane SPLICEFFM): element sites inside a splice ───────────────
//
// Page `j27-splice-ffm-element-hangs-seglongloops-FIXED-20261029.md`. Under
// `CRATONVM_JIT_IR_SPLICE_FFM_ELEMENT` the VM's resolver lifted its
// `ffm-accessor-keeps-its-own-fast-path` refusal for EVERY optimizing splice,
// and the planner then skipped a site with a thin or mixed receiver row, so
// the accessor stayed a dispatched call inside the splice (`R26SegLongLoops`
// native-put 928 -> 295,708 ns/op) instead of the callee's own compiled body
// with its own expansion. Both sides now ask [`spliced_ffm_element_admission`].

/// One `MemorySegment` element call inside a body the VM may splice: its
/// target `(class, method, descriptor)`, and the CALLEE body holding it with
/// the call's pc in that body (`code[pc]` is the `invoke`).
#[derive(Clone, Copy, Debug)]
pub struct SplicedFfmCall<'a> {
    pub target: (&'a str, &'a str, &'a str),
    pub code: &'a [u8],
    pub pc: usize,
}

/// THE admission of a `MemorySegment` element call inside a spliced body: the
/// accessor the expansion serves and what `segment_class` answers for the
/// class the call's receiver row names, or `None`.
///
/// One decision, asked on both sides of the splice:
/// * the VM's inline resolver (`jit_bridge` `resolve_inline_site_from`) lifts
///   its refusal only for a call this admits, from the callee's own receiver
///   row at the call, with its class store as `segment_class`
///   ([`ffm_segment_class_served`]);
/// * the planner (`plan_spliced_ffm_element_sites`, via
///   `plan_spliced_ffm_element_site`) plans exactly the calls this admits,
///   from the row the resolver handed it, with the VM's segment shape.
///
/// `None` with `CRATONVM_JIT_IR_SPLICE_FFM_ELEMENT` or
/// `CRATONVM_JIT_IR_FFM_ELEMENT_INLINE` off, for a call the expansion does not
/// serve ([`ffm_element_accessor`]), without a row, or when the row names no
/// class for the one-class expansion (`spliced_ffm_element_receiver`).
///
/// j27 wave 4 (lane SPLICEFFM2): and for every builder decline the resolver
/// can see before the build (`IrBuilder::try_emit_ffm_element_inline`), so the
/// callee keeps its own compiled body with its own expansion instead:
/// * an FP carrier without the raw-bits rows (`CRATONVM_JIT_IR_FP_RAW_BITS=0`);
/// * a layout operand that is not a `getstatic` the straight-line walk proves
///   ([`ffm_layout_operand_getstatic`]), or whose field `layout_filed` (asked
///   with that `getstatic`'s pc in `call.code`) says has no filed facts;
/// * a native segment (`segment_class` answers `(answer, native)`) without the
///   JIT-readable arena table (`arena_table`, [`ffm_arena_table_published`]).
///
/// The declines it cannot see (the chain-snapshot budget, a synchronized
/// root) are the re-plan's: `spliced_ffm_declined_sites`.
pub fn spliced_ffm_element_admission<T>(
    call: SplicedFfmCall<'_>,
    counts: Option<&crate::profile::ReceiverCounts>,
    arena_table: bool,
    segment_class: impl FnOnce(u32) -> Option<(T, bool)>,
    layout_filed: impl FnOnce(usize) -> bool,
) -> Option<(FfmElementAccessor, T)> {
    if !spliced_ffm_element_switches_on() {
        return None;
    }
    let (class, method, desc) = call.target;
    let accessor =
        ffm_element_accessor(class, method, desc).filter(|a| ffm_carrier_lowers(a.carrier))?;
    let layout_pc = ffm_layout_operand_getstatic(call.code, call.pc, accessor)?;
    let receiver = spliced_ffm_element_receiver(counts?)?;
    let (answer, native) = segment_class(receiver)?;
    (layout_filed(layout_pc) && (arena_table || !native)).then_some((accessor, answer))
}

/// The admission's switches: both FFM element switches, and chain snapshots
/// (`CRATONVM_JIT_IR_SPLICE_FRAME_STATES`). Without chain snapshots the
/// in-splice expansion has no frame for its guards and declines every site
/// (`IrBuilder::try_emit_ffm_element_inline`), so nothing is admitted.
fn spliced_ffm_element_switches_on() -> bool {
    crate::ir_splice_ffm_element_enabled()
        && crate::ir::ir_ffm_element_inline_enabled()
        && crate::ir::ir_splice_frame_states_enabled()
}

/// Does the expansion lower `carrier`? An FP carrier moves through its raw
/// bits, which need the raw-bits rows (`CRATONVM_JIT_IR_FP_RAW_BITS`): the
/// builder's own test.
fn ffm_carrier_lowers(carrier: u8) -> bool {
    !matches!(carrier, b'F' | b'D') || crate::ir::ir_fp_raw_bits_enabled()
}

/// May the expansion serve a NATIVE segment with the JIT-readable arena table
/// the VM publishes at `table_addr` (`0` = none)? The test
/// `ir_lower::scoped_arena_jit_table` applies to the VM's answer
/// (`CRATONVM_JIT_IR_SCOPED_ARENA_FOLD` and an entry-aligned address), for the
/// VM's resolver, which reads the address directly. Without the table the
/// builder declines a native site (`scoped_offheap_fold` off).
pub fn ffm_arena_table_published(table_addr: u64) -> bool {
    table_addr != 0
        && table_addr % (1u64 << crate::ir_lower::SCOPED_ARENA_ENTRY_SHIFT) == 0
        && crate::ir_lower::ir_scoped_arena_fold_enabled()
}

/// The pc of the `getstatic` that pushes the layout operand of the element
/// call `accessor` at `invoke_pc` of `code`, or `None` when the walk cannot
/// prove one.
///
/// The walk goes back from the call over the coordinate's (and a put's
/// value's) computation, which must be straight-line code of known stack
/// effect (`ffm_operand_op_effect`: loads, constants, arithmetic,
/// conversions, primitive array loads) that no branch enters; the instruction
/// right under that computation is the layout's producer. Anything else (a
/// field read, a call, a branch target) answers `None`, which keeps the VM's
/// refusal. Exception-handler entries are not consulted: the answer decides
/// only whether to splice, and the builder still checks the operand node it
/// meets (a decline there is the re-plan's case, `spliced_ffm_declined_sites`).
pub fn ffm_layout_operand_getstatic(
    code: &[u8],
    invoke_pc: usize,
    accessor: FfmElementAccessor,
) -> Option<usize> {
    let starts = ffm_insn_starts_to(code, invoke_pc)?;
    let targets = crate::bytecode_analysis::branch_target_map(code, code.len());
    let mut need = ffm_slots_above_layout(accessor);
    let mut after = invoke_pc;
    for &pc in starts.iter().rev() {
        if targets.get(after).copied().unwrap_or(true) {
            return None;
        }
        if need == 0 {
            return (code[pc] == 0xb2).then_some(pc);
        }
        let (pops, pushes) = ffm_operand_op_effect(code[pc])?;
        if pushes > need {
            return None;
        }
        need = need - pushes + pops;
        after = pc;
    }
    None
}

/// The instruction starts of `code` before `end`, or `None` when `end` is no
/// instruction start of `code`.
fn ffm_insn_starts_to(code: &[u8], end: usize) -> Option<Vec<usize>> {
    if end >= code.len() {
        return None;
    }
    let mut starts = Vec::new();
    let mut pc = 0;
    while pc < end {
        starts.push(pc);
        pc += crate::bytecode_analysis::step(code, pc);
    }
    (pc == end).then_some(starts)
}

/// The stack slots an element call takes above its layout operand: the
/// `long` coordinate, and a put's value (two slots for `J` / `D`).
fn ffm_slots_above_layout(accessor: FfmElementAccessor) -> u32 {
    let value = match (accessor.is_put, accessor.carrier) {
        (false, _) => 0,
        (true, b'J' | b'D') => 2,
        (true, _) => 1,
    };
    2 + value
}

/// `(pops, pushes)` in stack slots of a straight-line opcode that may compute
/// an element call's coordinate or value, or `None` for every other opcode
/// (`wide`, field reads, calls, branches, stack shuffles: their effect is not
/// known here, or they end the straight line).
fn ffm_operand_op_effect(op: u8) -> Option<(u32, u32)> {
    Some(match op {
        // aconst_null, iconst_*, fconst_*, bipush, sipush, ldc, ldc_w,
        // iload, fload, aload and their short forms
        0x01..=0x08
        | 0x0b..=0x0d
        | 0x10..=0x13
        | 0x15
        | 0x17
        | 0x19
        | 0x1a..=0x1d
        | 0x22..=0x25
        | 0x2a..=0x2d => (0, 1),
        // lconst_*, dconst_*, ldc2_w, lload, dload and their short forms
        0x09 | 0x0a | 0x0e | 0x0f | 0x14 | 0x16 | 0x18 | 0x1e..=0x21 | 0x26..=0x29 => (0, 2),
        // iaload faload aaload baload caload saload
        0x2e | 0x30 | 0x32..=0x35 => (2, 1),
        // laload daload
        0x2f | 0x31 => (2, 2),
        // int / float binary arithmetic, int shifts and bitwise ops
        0x60 | 0x62 | 0x64 | 0x66 | 0x68 | 0x6a | 0x6c | 0x6e | 0x70 | 0x72 | 0x78 | 0x7a
        | 0x7c | 0x7e | 0x80 | 0x82 => (2, 1),
        // long / double binary arithmetic, long bitwise ops
        0x61 | 0x63 | 0x65 | 0x67 | 0x69 | 0x6b | 0x6d | 0x6f | 0x71 | 0x73 | 0x7f | 0x81
        | 0x83 => (4, 2),
        // lshl lshr lushr
        0x79 | 0x7b | 0x7d => (3, 2),
        // ineg fneg i2f f2i i2b i2c i2s arraylength
        0x74 | 0x76 | 0x86 | 0x8b | 0x91..=0x93 | 0xbe => (1, 1),
        // lneg dneg l2d d2l
        0x75 | 0x77 | 0x8a | 0x8f => (2, 2),
        // i2l i2d f2l f2d
        0x85 | 0x87 | 0x8c | 0x8d => (1, 2),
        // l2i l2f d2i d2f
        0x88 | 0x89 | 0x8e | 0x90 => (2, 1),
        // iinc
        0x84 => (0, 0),
        _ => return None,
    })
}

/// The class a spliced element call's receiver row names for the one-class
/// expansion: at least `MIN_GUARDED_RECEIVER_EVIDENCE` observations and one
/// class holding the guarded-cache share (`PROFILE_GUARDED_MIN_RECEIVER_PCT`);
/// never class `0`. The round-24 planner's evidence test, unchanged.
fn spliced_ffm_element_receiver(counts: &crate::profile::ReceiverCounts) -> Option<u32> {
    let evidence = crate::profile::ReceiverEvidence::of(counts);
    if evidence.summary().observations < crate::MIN_GUARDED_RECEIVER_EVIDENCE {
        return None;
    }
    evidence
        .seed_a_guarded_cache(crate::ir::PROFILE_GUARDED_MIN_RECEIVER_PCT)
        .filter(|&c| c != 0)
}

/// The VM resolver's `segment_class` answer for
/// [`spliced_ffm_element_admission`]: is `class_name`, defined by the
/// bootstrap loader from real class bytes, a segment class the expansion
/// serves? The same three tests the VM's segment-shape answer
/// (`vm::jit::helpers` `ffm_inline_well_known_answer`) makes before it
/// resolves any field slot, so the planner's [`ffm_segment_shape`] refuses a
/// class this admits only when a JDK field is missing.
pub fn ffm_segment_class_served(class_name: &str, bootstrap: bool, real_bytes: bool) -> bool {
    bootstrap && real_bytes && ffm_segment_kind_answer(class_name) != 0
}

/// The plan of one spliced element call `call` with receiver row `counts`:
/// [`spliced_ffm_element_admission`] with the VM's segment shape, this
/// compile's arena table (`ir_lower::scoped_arena_jit_table`, the builder's
/// `scoped_offheap_fold` input) and the filed facts of the field
/// `layout_field` names for the layout operand's `getstatic` (its pc in
/// `call.code`; `None` = no static-field row there). A spliced site never
/// refuses the method (`bail_on_decline` is `false`); a builder decline keeps
/// the call row, and the re-plan drops its splice
/// ([`spliced_ffm_declined_sites`]).
pub(crate) fn plan_spliced_ffm_element_site(
    helpers: &crate::DirectHelperTable,
    call: SplicedFfmCall<'_>,
    counts: Option<&crate::profile::ReceiverCounts>,
    layout_field: impl FnOnce(usize) -> Option<(u32, usize)>,
) -> Option<FfmElementInlineSite> {
    let arena_table = crate::ir_lower::scoped_arena_jit_table(helpers).is_some();
    let (accessor, shape) = spliced_ffm_element_admission(
        call,
        counts,
        arena_table,
        |c| {
            let shape = ffm_segment_shape(helpers, c)?;
            Some((shape, matches!(shape.base, FfmSegmentBase::Native { .. })))
        },
        |pc| {
            layout_field(pc)
                .is_some_and(|(holder, index)| ffm_layout_facts(helpers, holder, index).is_some())
        },
    )?;
    Some(FfmElementInlineSite {
        shape,
        accessor,
        bail_on_decline: false,
    })
}

/// Has the VM withdrawn the call site a spliced element site sits under?
///
/// A guard the expansion plants inside a splice resumes through the splice's
/// CHAIN frame, and every chain sink charges such a trap at the OUTERMOST call
/// site of the compiling method `method_key` under the wildcard
/// (`vm::jit::helpers` `chain_trap_charge`, `deopt_resume` `stash_charge_site`:
/// no element guard has a chain id of its own). So the question is
/// `crate::ir_splice_site_withdrawn` at that call site's bci
/// (`IrInlineFrameSites::enclosing_bci_at` of the site's combined pc), the
/// record the whole-splice rule reads too. Not the callee's key, and not the
/// combined pc: nothing is ever recorded there. `None` (the pc lies in no
/// spliced body) answers withdrawn: fail closed, the site keeps its call row.
pub(crate) fn spliced_ffm_call_site_withdrawn(
    despec: Option<&crate::deopt::DespecRegistry>,
    method_key: &str,
    call_site_bci: Option<u32>,
) -> bool {
    match call_site_bci {
        // Cast: a u32 bci widened to the pc type.
        Some(bci) => crate::ir_splice_site_withdrawn(despec, method_key, bci as usize),
        None => true,
    }
}

/// j27 wave 4 (lane SPLICEFFM2): the builder declines the admission cannot see
/// before the build -- the chain-snapshot word budget ran out at the site
/// (`IR_MAX_FRAME_SNAPSHOT_WORDS`), a synchronized root (no chain frames), a
/// planner that did not plan an admitted site (a withdrawn call site, an
/// unread shape) -- each leave a `MemorySegment` element call inside the
/// splice: a dispatched accessor, about 300x the callee's own compiled body
/// with its own expansion (`R26SegLongLoops` native-put 928 -> 295,708
/// ns/op).
///
/// The build's own record of them is its surviving `Op::Call` rows: `calls`
/// gives each with its (combined) pc and target. `None`: no element call
/// survives inside a splice (or `CRATONVM_JIT_IR_SPLICE_FFM_ELEMENT` is off;
/// without it the VM never splices an element call it would expand).
/// `Some(sites)`: the build declined, and `sites` are the TOP-LEVEL caller pcs
/// whose splices hold those calls (`ranges` and [`crate::ir_splice_rebuild_site`]
/// map a combined pc to its top-level site), sorted and once each. `ir_tier`
/// refuses the build and re-plans ONCE without those splices, so each callee
/// is a call into its own compiled body; a decline the re-plan leaves (or
/// one with no site to drop) refuses the optimizing body, never publishes a
/// dispatched accessor.
pub(crate) fn spliced_ffm_declined_sites<'a>(
    calls: impl IntoIterator<Item = (usize, (&'a str, &'a str, &'a str))>,
    code_len: usize,
    ranges: &[(usize, usize, usize)],
) -> Option<Vec<usize>> {
    if !crate::ir_splice_ffm_element_enabled() {
        return None;
    }
    let declined: Vec<usize> = calls
        .into_iter()
        .filter(|&(pc, (class, method, desc))| {
            pc >= code_len && ffm_element_accessor(class, method, desc).is_some()
        })
        .map(|(pc, _)| pc)
        .collect();
    if declined.is_empty() {
        return None;
    }
    let mut sites: Vec<usize> = declined
        .into_iter()
        .filter_map(|pc| crate::ir_splice_rebuild_site(pc, code_len, ranges))
        .collect();
    sites.sort_unstable();
    sites.dedup();
    Some(sites)
}

/// t1 wave 8 (lane FFM27, FFM27-2): the two receiver classes of an element
/// site's row when, after its unreached trap fired, the row names EXACTLY two
/// classes and nothing else (top first), whatever the counts; `None`
/// otherwise.
///
/// After the trap the counts are not a sample of the site's traffic. The
/// inline-cache profile seed tops every class it serves up to
/// `MIN_GUARDED_RECEIVER_EVIDENCE` (`ProfileStore::seed_ic_observed_receiver`),
/// and the interpreter records only the calls a one-class expansion deopted
/// (the other class runs compiled and records nothing), so a heap / native
/// kernel reads 80 % one class and plans the one-class expansion whose guards
/// then deopt on every call of the other: the two receivers never gave a plan
/// (`T1J6FfmSeedRows bi-mixed-put`). Two classes seen at all is the evidence;
/// both arms test their exact class and anything else deopts at the call's
/// bci, so no answer depends on the counts. A thin row (fewer than
/// `MIN_GUARDED_RECEIVER_EVIDENCE`) qualifies too, as the one-class
/// after-trap rule takes a single observation.
pub fn ffm_bimorphic_after_trap_pair(
    profile: Option<&crate::profile::MethodProfile>,
    pc: usize,
) -> Option<(u32, u32)> {
    let counts = profile?.receivers.get(&pc)?;
    let evidence = crate::profile::ReceiverEvidence::of(counts);
    let s = evidence.summary();
    let (top, top_n) = s.top?;
    let (second, second_n) = s.second?;
    if top == 0 || second == 0 || top_n == 0 || second_n == 0 || !s.is_exact() {
        return None;
    }
    // Exactly the two: every observation is one of them (a zero-count entry
    // for a third class is no observation).
    (u64::from(top_n) + u64::from(second_n) == u64::from(s.observations)).then_some((top, second))
}

#[cfg(test)]
mod t1w6_j6_ffm27_tests {
    use super::*;
    use std::collections::HashMap;

    const SEG: &str = "java/lang/foreign/MemorySegment";
    const PUT: &str = "(Ljava/lang/foreign/ValueLayout$OfInt;JI)V";
    const INLINE: &str = "CRATONVM_JIT_IR_FFM_ELEMENT_INLINE";
    const RECEIVED: &str = "CRATONVM_JIT_IR_FFM_RECEIVED_SEED_CALL";

    type Prof<'a> = Option<&'a crate::profile::MethodProfile>;

    fn facts(carrier: u8) -> HashMap<(u32, u32), FfmLayoutFacts> {
        let mut m = HashMap::new();
        m.insert(
            (7, 1),
            FfmLayoutFacts {
                carrier,
                width: 4,
                align: 4,
                big_endian: false,
            },
        );
        m
    }

    fn miss(
        op: u8,
        first: bool,
        p: Prof<'_>,
        f: &HashMap<(u32, u32), FfmLayoutFacts>,
        trap: bool,
        described: bool,
    ) -> Option<FfmElementPlanMiss> {
        cratonvm_types::flags::with_thread_overrides(&[(INLINE, Some("1"))], || {
            ffm_element_plan_miss(SEG, "setAtIndex", PUT, op, 19, first, p, f, trap, |_| described)
        })
    }

    /// The miss reason walks the plan's conditions in order; a plannable site
    /// answers `None`.
    #[test]
    fn the_plan_miss_names_the_first_failed_condition() {
        let int_facts = facts(b'I');
        let mut one = crate::profile::MethodProfile::default();
        one.record_receiver(19, 826);
        let mut two = crate::profile::MethodProfile::default();
        two.record_receiver(19, 826);
        two.record_receiver(19, 900);
        let mut empty_row = crate::profile::MethodProfile::default();
        empty_row.receivers.insert(19, Default::default());
        let none = crate::profile::MethodProfile::default();
        use FfmElementPlanMiss as M;
        assert_eq!(miss(0xb9, true, Some(&one), &int_facts, true, true), Some(M::FirstCall));
        assert_eq!(miss(0xb8, false, Some(&one), &int_facts, true, true), Some(M::Opcode(0xb8)));
        assert_eq!(
            miss(0xb9, false, Some(&one), &facts(b'J'), true, true),
            Some(M::NoLayoutFact { carrier: b'I' })
        );
        assert_eq!(miss(0xb9, false, Some(&none), &int_facts, true, true), Some(M::NoReceiverRow));
        assert_eq!(miss(0xb9, false, None, &int_facts, true, true), Some(M::NoReceiverRow));
        assert_eq!(
            miss(0xb9, false, Some(&empty_row), &int_facts, true, true),
            Some(M::EmptyReceiverRow)
        );
        // One thin observation plans only after the unreached trap.
        assert_eq!(miss(0xb9, false, Some(&one), &int_facts, true, true), None);
        assert!(matches!(
            miss(0xb9, false, Some(&one), &int_facts, false, true),
            Some(M::ReceiversNotOneClass {
                observations: 1,
                types: 1,
                after_unreached_trap: false,
                ..
            })
        ));
        assert!(matches!(
            miss(0xb9, false, Some(&two), &int_facts, true, true),
            Some(M::ReceiversNotOneClass {
                observations: 2,
                types: 2,
                ..
            })
        ));
        assert_eq!(
            miss(0xb9, false, Some(&one), &int_facts, true, false),
            Some(M::NotADescribedSegment { class_id: 826 })
        );
        let off = cratonvm_types::flags::with_thread_overrides(&[(INLINE, Some("0"))], || {
            ffm_element_plan_miss(
                SEG,
                "setAtIndex",
                PUT,
                0xb9,
                19,
                false,
                Some(&one),
                &int_facts,
                true,
                |_| true,
            )
        });
        assert_eq!(off, Some(M::InlineOff));
        assert!(M::NotADescribedSegment { class_id: 5 }
            .to_string()
            .contains("class id 5"));
    }

    fn received(sw: &[(&str, Option<&str>)], first: bool, p: Prof<'_>, despec: bool) -> bool {
        let mut all: Vec<(&str, Option<&str>)> = vec![(INLINE, Some("1"))];
        all.extend_from_slice(sw);
        cratonvm_types::flags::with_thread_overrides(&all, || {
            ffm_element_received_seed_call_site(SEG, "setAtIndex", PUT, 0xb9, 19, first, p, despec)
        })
    }

    /// The received seed call: opt-in, only after the trap fired, only with a
    /// receiver in the row (the empty row is the existing seed call's case),
    /// and only with the seed call's own switches on.
    #[test]
    fn a_fired_site_with_receivers_but_no_plan_is_a_received_seed_call_opt_in() {
        let mut seen = crate::profile::MethodProfile::default();
        seen.record_receiver(19, 826);
        seen.record_receiver(19, 900);
        let empty = crate::profile::MethodProfile::default();
        let on: &[(&str, Option<&str>)] = &[(RECEIVED, Some("1"))];
        assert!(received(on, false, Some(&seen), true), "fired, receivers, no plan");
        assert!(!received(&[(RECEIVED, None)], false, Some(&seen), true), "opt-in");
        assert!(!received(on, false, Some(&seen), false), "trap not fired");
        assert!(!received(on, false, Some(&empty), true), "empty row: the seed call's case");
        assert!(!received(on, false, None, true), "no profile: the seed call's case");
        assert!(!received(on, true, Some(&seen), true), "first-call site");
        let no_seed = [
            (RECEIVED, Some("1")),
            ("CRATONVM_JIT_IR_FFM_UNREACHED_SEED_CALL", Some("0")),
        ];
        assert!(!received(&no_seed, false, Some(&seen), true), "needs the seed call switch");
        let no_ic = [(RECEIVED, Some("1")), ("CRATONVM_JIT_IR_IC_PROFILE_SEED", Some("0"))];
        assert!(!received(&no_ic, false, Some(&seen), true), "needs the IC profile seed");
        let other = cratonvm_types::flags::with_thread_overrides(
            &[(INLINE, Some("1")), (RECEIVED, Some("1"))],
            || {
                ffm_element_received_seed_call_site(
                    SEG,
                    "byteSize",
                    "()J",
                    0xb9,
                    19,
                    false,
                    Some(&seen),
                    true,
                )
            },
        );
        assert!(!other, "not an element accessor");
        // t1 wave 8 (lane FFM27): a row at the evidence bar that still gave no
        // plan keeps the refusal route; the seed could never arm on it.
        let mut full = crate::profile::MethodProfile::default();
        for _ in 0..crate::MIN_GUARDED_RECEIVER_EVIDENCE {
            full.record_receiver(19, 826);
            full.record_receiver(19, 900);
        }
        assert!(!received(on, false, Some(&full), true), "not a thin row");
    }
}

#[cfg(test)]
mod t1w8_ffm27_bimorphic_after_trap_tests {
    use super::{
        ffm_after_trap_despec, ffm_bimorphic_after_trap_enabled, ffm_bimorphic_after_trap_pair,
        FfmAfterTrapDespec,
    };
    use crate::deopt::{speculation_id, DeoptReason, SPECULATION_ID_ANY};

    /// The site's own unreached trap (its `UnstableIf` id at the bci) admits
    /// the bimorphic plan after a de-spec, strictly when the one-class
    /// expansion's class test (`ReceiverTypeChanged`) was withdrawn too; the
    /// wildcard, the bounds guards' `UncommonTrap`, the null test, another
    /// bci's id, or no trap at all do not.
    #[test]
    fn only_the_unreached_trap_at_the_bci_admits_the_bimorphic_plan() {
        let trap = speculation_id(19, DeoptReason::UnstableIf);
        let class_miss = speculation_id(19, DeoptReason::ReceiverTypeChanged);
        let guard = speculation_id(19, DeoptReason::UncommonTrap);
        let null = speculation_id(19, DeoptReason::NullCheck);
        let other_bci = speculation_id(20, DeoptReason::UnstableIf);
        assert_eq!(ffm_after_trap_despec(&[trap], 19), Some(FfmAfterTrapDespec::TrapOnly));
        assert_eq!(
            ffm_after_trap_despec(&[class_miss, trap], 19),
            Some(FfmAfterTrapDespec::TrapAndClassMiss)
        );
        assert_eq!(ffm_after_trap_despec(&[], 19), None, "nothing withdrawn");
        assert_eq!(ffm_after_trap_despec(&[class_miss], 19), None, "no trap fired");
        assert_eq!(ffm_after_trap_despec(&[trap, guard], 19), None, "bounds guards failed");
        assert_eq!(ffm_after_trap_despec(&[trap, null], 19), None, "null receivers");
        assert_eq!(ffm_after_trap_despec(&[trap, SPECULATION_ID_ANY], 19), None, "wildcard");
        assert_eq!(ffm_after_trap_despec(&[other_bci], 19), None, "another bci's id");
    }

    /// After the trap, exactly two classes plan the pair whatever their
    /// shares or counts; one class, three classes or class 0 do not.
    #[test]
    fn exactly_two_classes_after_the_trap_are_a_pair() {
        let mut skewed = crate::profile::MethodProfile::default();
        for _ in 0..crate::MIN_GUARDED_RECEIVER_EVIDENCE * 4 {
            skewed.record_receiver(19, 826);
        }
        skewed.record_receiver(19, 900);
        assert_eq!(ffm_bimorphic_after_trap_pair(Some(&skewed), 19), Some((826, 900)));
        let pair = || {
            let mut p = crate::profile::MethodProfile::default();
            p.record_receiver(19, 900);
            p.record_receiver(19, 826);
            p
        };
        let thin = pair();
        assert_eq!(
            ffm_bimorphic_after_trap_pair(Some(&thin), 19),
            Some((826, 900)),
            "a tie ranks by ascending class id"
        );
        let mut one = crate::profile::MethodProfile::default();
        one.record_receiver(19, 826);
        assert_eq!(ffm_bimorphic_after_trap_pair(Some(&one), 19), None);
        let mut three = pair();
        three.record_receiver(19, 950);
        assert_eq!(ffm_bimorphic_after_trap_pair(Some(&three), 19), None);
        let mut unknown = crate::profile::MethodProfile::default();
        unknown.record_receiver(19, 0);
        unknown.record_receiver(19, 826);
        assert_eq!(ffm_bimorphic_after_trap_pair(Some(&unknown), 19), None, "class 0");
        // A zero-count entry is no observation.
        let mut zero = pair();
        zero.receivers.get_mut(&19).unwrap().insert(950, 0);
        assert_eq!(ffm_bimorphic_after_trap_pair(Some(&zero), 19), Some((826, 900)));
        assert_eq!(ffm_bimorphic_after_trap_pair(Some(&thin), 20), None, "no row");
        assert_eq!(ffm_bimorphic_after_trap_pair(None, 19), None);
        let unset = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_IR_FFM_BIMORPHIC_AFTER_TRAP", None)],
            ffm_bimorphic_after_trap_enabled,
        );
        assert!(unset, "default ON since j27 2026-10-28");
        let off = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_IR_FFM_BIMORPHIC_AFTER_TRAP", Some("0"))],
            ffm_bimorphic_after_trap_enabled,
        );
        assert!(!off, "`=0` kills");
    }
}

#[cfg(test)]
mod t1w9_jita_withdrawn_guards_tests {
    use super::{ffm_element_guards_withdrawn, ffm_element_withdrawn_guards_enabled};
    use crate::deopt::{speculation_id, DeoptReason, SPECULATION_ID_ANY};

    /// Only the site's own unreached trap leaves the one-class plan standing;
    /// any withdrawn guard of the expansion, the wildcard, or a mix does not.
    #[test]
    fn any_withdrawn_guard_but_the_unreached_trap_stands_the_plan_down() {
        let trap = speculation_id(19, DeoptReason::UnstableIf);
        let class_miss = speculation_id(19, DeoptReason::ReceiverTypeChanged);
        let guard = speculation_id(19, DeoptReason::UncommonTrap);
        let null = speculation_id(19, DeoptReason::NullCheck);
        assert!(!ffm_element_guards_withdrawn(&[], 19), "nothing withdrawn");
        assert!(!ffm_element_guards_withdrawn(&[trap], 19), "the unreached trap alone");
        assert!(ffm_element_guards_withdrawn(&[guard], 19), "bounds / read-only / alignment");
        assert!(ffm_element_guards_withdrawn(&[trap, null], 19), "null receivers");
        assert!(ffm_element_guards_withdrawn(&[class_miss, trap], 19), "a class miss");
        assert!(ffm_element_guards_withdrawn(&[SPECULATION_ID_ANY], 19), "wildcard");
        // Another bci's trap id is not this site's trap.
        let other_trap = speculation_id(20, DeoptReason::UnstableIf);
        assert!(ffm_element_guards_withdrawn(&[other_trap], 19));
        let on = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_IR_FFM_ELEMENT_WITHDRAWN_GUARDS", None)],
            ffm_element_withdrawn_guards_enabled,
        );
        assert!(on, "default ON");
        let off = cratonvm_types::flags::with_thread_overrides(
            &[("CRATONVM_JIT_IR_FFM_ELEMENT_WITHDRAWN_GUARDS", Some("0"))],
            ffm_element_withdrawn_guards_enabled,
        );
        assert!(!off, "=0 kills it");
    }

    /// j27 HANGHUNT (`CRATONVM_JIT_IR_FFM_ELEMENT_WITHDRAWN_OWN_GUARDS_ONLY`,
    /// opt-in): with the switch, only the expansion's OWN named guards at this
    /// bci stand the plan down; the wildcard and another bci's id do not.
    /// Without it, the t1 wave 9 rule above.
    #[test]
    fn j27_hanghunt_only_own_guards_withdraw_the_plan_with_the_switch() {
        use super::ffm_element_plan_withdrawn;
        const SW: &str = "CRATONVM_JIT_IR_FFM_ELEMENT_WITHDRAWN_OWN_GUARDS_ONLY";
        let trap = speculation_id(19, DeoptReason::UnstableIf);
        let class_miss = speculation_id(19, DeoptReason::ReceiverTypeChanged);
        let guard = speculation_id(19, DeoptReason::UncommonTrap);
        let null = speculation_id(19, DeoptReason::NullCheck);
        let other = speculation_id(20, DeoptReason::UncommonTrap);
        let ask = |v: Option<&str>, ids: &[u32]| {
            cratonvm_types::flags::with_thread_overrides(&[(SW, v)], || {
                ffm_element_plan_withdrawn(ids, 19)
            })
        };
        // Off (the default): the old answers, the wildcard withdraws.
        assert!(ask(None, &[SPECULATION_ID_ANY]), "default OFF: wildcard withdraws");
        assert!(ask(None, &[other]));
        // On: the wildcard and a foreign id leave the plan standing.
        let on = Some("1");
        assert!(!ask(on, &[SPECULATION_ID_ANY]), "wildcard");
        assert!(!ask(on, &[other]), "another bci's guard");
        assert!(!ask(on, &[trap]), "the unreached trap alone");
        assert!(!ask(on, &[trap, SPECULATION_ID_ANY]));
        assert!(ask(on, &[guard]), "bounds / read-only / alignment");
        assert!(ask(on, &[trap, null]), "null receivers");
        assert!(ask(on, &[class_miss]), "a class miss");
        assert!(ask(on, &[SPECULATION_ID_ANY, guard]), "own guard among others");
    }
}

#[cfg(test)]
mod j27_spliceffm_tests {
    use super::*;
    use crate::deopt::{speculation_id, DeoptReason, DespecRegistry, SPECULATION_ID_ANY};
    use crate::profile::ReceiverCounts;

    const HEAP: u32 = 0x4100;
    const OTHER: u32 = 0x4101;
    const HEAP_NAME: &str = "jdk/internal/foreign/HeapMemorySegmentImpl$OfInt";
    const GET: (&str, &str, &str) = (
        "java/lang/foreign/MemorySegment",
        "getAtIndex",
        "(Ljava/lang/foreign/ValueLayout$OfInt;J)I",
    );
    const SPLICE: &str = "CRATONVM_JIT_IR_SPLICE_FFM_ELEMENT";
    const INLINE: &str = "CRATONVM_JIT_IR_FFM_ELEMENT_INLINE";
    const FLOOR: &str = "CRATONVM_JIT_DOMINANT_RECEIVER_FLOOR";
    /// `int get(MemorySegment s, int i) { return s.getAtIndex(JAVA_INT, i); }`:
    /// `aload_0; getstatic #1; iload_1; i2l; invokeinterface #2 3 0; ireturn`.
    /// The call is at pc 6, its layout operand's `getstatic` at pc 1.
    pub(super) const GET_BODY: [u8; 12] = [
        0x2a, 0xb2, 0, 1, 0x1b, 0x85, 0xb9, 0, 2, 3, 0, 0xac,
    ];
    pub(super) const GET_PC: usize = 6;
    /// The filed `static final` layout `(holder, field index)` the fake VM
    /// answers facts for (`int`, 4-byte aligned, little-endian).
    const LAYOUT: (u32, u16) = (0x4200, 3);

    /// The call [`GET`] at [`GET_PC`] of [`GET_BODY`].
    pub(super) fn get_call() -> SplicedFfmCall<'static> {
        SplicedFfmCall {
            target: GET,
            code: &GET_BODY,
            pc: GET_PC,
        }
    }

    /// A fake `jit_well_known_class_id` that knows one heap `int[]` segment
    /// class (every slot `selector + 1`), the facts of one layout field, and
    /// nothing else.
    unsafe extern "C" fn fake_well_known(_ctx: i64, which: i64) -> i64 {
        let Some((tag, cid, index)) = ffm_inline_query_decode(which) else {
            return 0;
        };
        if tag == WELL_KNOWN_FFM_LAYOUT_FACTS_TAG {
            let int = FfmLayoutFacts {
                carrier: b'I',
                width: 4,
                align: 4,
                big_endian: false,
            };
            return if (cid, index) == LAYOUT { int.pack() } else { 0 };
        }
        if tag != WELL_KNOWN_FFM_SEGMENT_SLOT_TAG || cid != HEAP {
            return 0;
        }
        if index == FFM_SEGMENT_SEL_KIND {
            return i64::from(b'I');
        }
        if ffm_segment_selector_applies(b'I', index) {
            i64::from(index) + 1
        } else {
            0
        }
    }

    fn helpers() -> crate::DirectHelperTable {
        let mut h = crate::DirectHelperTable::EMPTY;
        h.well_known_class_id = fake_well_known as *const () as usize; // Cast: fn -> slot
        // `vh_const_ask` asks nothing without a context.
        h.static_base_resolver_ctx = 0x77;
        h
    }

    fn row(pairs: &[(u32, u32)]) -> ReceiverCounts {
        pairs.iter().copied().collect()
    }

    fn with<R>(splice: Option<&str>, inline: Option<&str>, f: impl FnOnce() -> R) -> R {
        cratonvm_types::flags::with_thread_overrides(
            &[
                (SPLICE, splice),
                (INLINE, inline),
                (FLOOR, None),
                ("CRATONVM_JIT_IR_SPLICE_FRAME_STATES", Some("1")),
            ],
            f,
        )
    }

    /// The VM's side: the class store answers by class name, no arena table
    /// (a heap class needs none), and every provable `getstatic` layout
    /// operand counts as filed (the resolver cannot read the facts under its
    /// class-manager lock).
    fn vm_admits(counts: Option<&ReceiverCounts>) -> bool {
        spliced_ffm_element_admission(
            get_call(),
            counts,
            false,
            |c| {
                let name = if c == HEAP { HEAP_NAME } else { "p/NotASegment" };
                ffm_segment_class_served(name, true, true).then_some(((), ffm_native_segment_class(name)))
            },
            |_| true,
        )
        .is_some()
    }

    /// The planner's side: the layout operand's `getstatic` (pc 1 of
    /// [`GET_BODY`]) reads the filed [`LAYOUT`] field.
    fn plan(h: &crate::DirectHelperTable, counts: Option<&ReceiverCounts>) -> Option<FfmElementInlineSite> {
        plan_spliced_ffm_element_site(h, get_call(), counts, |pc| {
            (pc == 1).then_some((LAYOUT.0, usize::from(LAYOUT.1)))
        })
    }

    /// Item 1: the resolver lifts its refusal for exactly the rows the planner
    /// plans -- a thin row, a mixed row and a row naming a non-segment class
    /// keep the refusal (the callee stays a call into its own compiled body),
    /// so no admitted call is left a dispatched accessor by the planner.
    #[test]
    fn j27_spliceffm_the_resolver_lifts_its_refusal_exactly_where_the_planner_plans() {
        let h = helpers();
        let rows: [(Option<ReceiverCounts>, bool, &str); 6] = [
            (None, false, "no row"),
            (Some(row(&[(HEAP, 31)])), false, "under MIN_GUARDED_RECEIVER_EVIDENCE"),
            (Some(row(&[(HEAP, 32)])), true, "one class, enough evidence"),
            (Some(row(&[(HEAP, 40), (OTHER, 40)])), false, "mixed"),
            (Some(row(&[(OTHER, 64)])), false, "not a segment class"),
            (Some(row(&[(HEAP, 90), (OTHER, 10)])), true, "the guarded-cache share"),
        ];
        with(Some("1"), Some("1"), || {
            for (counts, want, why) in &rows {
                let planned = plan(&h, counts.as_ref());
                assert_eq!(vm_admits(counts.as_ref()), *want, "resolver: {why}");
                assert_eq!(planned.is_some(), *want, "planner: {why}");
                if let Some(site) = planned {
                    assert_eq!(site.shape.class_id, HEAP);
                    assert_eq!(
                        site.accessor,
                        FfmElementAccessor {
                            carrier: b'I',
                            is_put: false,
                            by_index: true
                        }
                    );
                    assert!(!site.bail_on_decline, "a spliced site never refuses the method");
                }
            }
        });
    }

    /// Either switch off, or a call the expansion does not serve: nothing is
    /// admitted, so the refusal stands.
    #[test]
    fn j27_spliceffm_nothing_is_admitted_with_a_switch_off_or_for_another_call() {
        let counts = row(&[(HEAP, 64)]);
        let admit = |target: (&str, &str, &str)| {
            let call = SplicedFfmCall {
                target,
                ..get_call()
            };
            spliced_ffm_element_admission(call, Some(&counts), true, |_| Some(((), false)), |_| true)
                .is_some()
        };
        with(Some("1"), Some("1"), || {
            assert!(admit(GET));
            assert!(!admit((
                "java/lang/foreign/MemorySegment",
                "asSlice",
                "(J)Ljava/lang/foreign/MemorySegment;"
            )));
            assert!(!admit(("p/MySegment", GET.1, GET.2)), "not the interface");
        });
        with(Some("0"), Some("1"), || assert!(!admit(GET), "splice switch off"));
        with(None, Some("1"), || assert!(!admit(GET), "the splice switch is opt-in"));
        with(Some("1"), Some("0"), || assert!(!admit(GET), "inline switch off"));
        let no_chain_frames = cratonvm_types::flags::with_thread_overrides(
            &[
                (SPLICE, Some("1")),
                (INLINE, Some("1")),
                ("CRATONVM_JIT_IR_SPLICE_FRAME_STATES", Some("0")),
            ],
            || admit(GET),
        );
        assert!(!no_chain_frames, "no chain frame: the builder would decline every site");
        // The VM's class test: all three facts are needed.
        assert!(ffm_segment_class_served(HEAP_NAME, true, true));
        assert!(ffm_segment_class_served("jdk/internal/foreign/NativeMemorySegmentImpl", true, true));
        assert!(!ffm_segment_class_served(HEAP_NAME, false, true), "another loader");
        assert!(!ffm_segment_class_served(HEAP_NAME, true, false), "no real bytes");
        assert!(!ffm_segment_class_served("p/NotASegment", true, true));
    }

    /// Item 2: a chain trap of an in-splice guard is charged at the compiling
    /// method's OUTERMOST call site under the wildcard; that record, and only
    /// that one, withdraws the spliced sites under it.
    #[test]
    fn j27_spliceffm_a_withdrawn_call_site_stands_its_spliced_sites_down() {
        const ROOT: &str = "p/Main.main:([Ljava/lang/String;)V";
        const CALLEE: &str = "p/Main.heapPut:(Ljava/lang/foreign/MemorySegment;)V";
        let despec = DespecRegistry::new();
        assert!(!spliced_ffm_call_site_withdrawn(Some(&despec), ROOT, Some(7)), "nothing recorded");
        assert!(!spliced_ffm_call_site_withdrawn(None, ROOT, Some(7)), "no registry");
        assert!(spliced_ffm_call_site_withdrawn(Some(&despec), ROOT, None), "no call site");
        // A named id at the call site belongs to its own guard (the splice's
        // receiver guard here), not to the element expansion.
        despec.insert(ROOT, 7, speculation_id(7, DeoptReason::ReceiverTypeChanged));
        assert!(!spliced_ffm_call_site_withdrawn(Some(&despec), ROOT, Some(7)));
        // The chain sinks' record.
        despec.insert(ROOT, 7, SPECULATION_ID_ANY);
        assert!(spliced_ffm_call_site_withdrawn(Some(&despec), ROOT, Some(7)));
        assert!(!spliced_ffm_call_site_withdrawn(Some(&despec), ROOT, Some(8)), "another call site");
        // A record under the callee's own key (its separately compiled body)
        // says nothing about this splice.
        let callee_only = DespecRegistry::new();
        callee_only.insert(CALLEE, 7, SPECULATION_ID_ANY);
        assert!(!spliced_ffm_call_site_withdrawn(Some(&callee_only), ROOT, Some(7)));
    }
}

#[cfg(test)]
mod j27_spliceffm2_tests {
    use super::j27_spliceffm_tests::{get_call, GET_BODY, GET_PC};
    use super::*;

    const SPLICE: &str = "CRATONVM_JIT_IR_SPLICE_FFM_ELEMENT";
    const SEG: &str = "java/lang/foreign/MemorySegment";
    const GET_INT: (&str, &str, &str) = (SEG, "getAtIndex", "(Ljava/lang/foreign/ValueLayout$OfInt;J)I");
    const SET_INT: (&str, &str, &str) = (SEG, "setAtIndex", "(Ljava/lang/foreign/ValueLayout$OfInt;JI)V");
    const SET_LONG: (&str, &str, &str) = (SEG, "setAtIndex", "(Ljava/lang/foreign/ValueLayout$OfLong;JJ)V");
    const GET_DOUBLE: (&str, &str, &str) = (SEG, "get", "(Ljava/lang/foreign/ValueLayout$OfDouble;J)D");

    fn accessor(t: (&str, &str, &str)) -> FfmElementAccessor {
        ffm_element_accessor(t.0, t.1, t.2).expect("an accessor the expansion serves")
    }

    fn row() -> crate::profile::ReceiverCounts {
        [(0x4100_u32, 64_u32)].into_iter().collect()
    }

    /// The admission as a side answers it: one heap or native segment class,
    /// the arena table and the layout's filing as given.
    fn admits(call: SplicedFfmCall<'_>, native: bool, arena: bool, filed: bool) -> bool {
        admits_raw_bits(None, call, native, arena, filed)
    }

    /// [`admits`] with `CRATONVM_JIT_IR_FP_RAW_BITS` as given (one override
    /// scope: a nested `with_thread_overrides` replaces the outer one).
    fn admits_raw_bits(
        raw_bits: Option<&str>,
        call: SplicedFfmCall<'_>,
        native: bool,
        arena: bool,
        filed: bool,
    ) -> bool {
        let counts = row();
        cratonvm_types::flags::with_thread_overrides(
            &[
                ("CRATONVM_JIT_IR_FP_RAW_BITS", raw_bits),
                (SPLICE, Some("1")),
                ("CRATONVM_JIT_IR_FFM_ELEMENT_INLINE", Some("1")),
                ("CRATONVM_JIT_IR_SPLICE_FRAME_STATES", Some("1")),
                ("CRATONVM_JIT_DOMINANT_RECEIVER_FLOOR", None),
            ],
            || {
                spliced_ffm_element_admission(call, Some(&counts), arena, |_| Some(((), native)), |_| filed)
                    .is_some()
            },
        )
    }

    /// Remaining design item 4: the layout operand's producer is found by a
    /// straight-line walk back from the call, over the coordinate's (and a
    /// put's value's) computation only.
    #[test]
    fn j27_spliceffm2_the_layout_operand_is_the_getstatic_under_the_coordinate() {
        let find = |code: &[u8], pc: usize, t| ffm_layout_operand_getstatic(code, pc, accessor(t));
        assert_eq!(find(&GET_BODY, GET_PC, GET_INT), Some(1));
        // `s.setAtIndex(JAVA_INT, i, v + 1)`: aload_0; getstatic; lload_1; iload_3;
        // iconst_1; iadd; invokeinterface.
        let put = [0x2a, 0xb2, 0, 1, 0x1f, 0x1d, 0x04, 0x60, 0xb9, 0, 2, 5, 0, 0xb1];
        assert_eq!(find(&put, 8, SET_INT), Some(1));
        // `s.setAtIndex(JAVA_LONG, a[i], x)`: aload_0; getstatic; aload_1; iload_2;
        // iaload; i2l; lload_3; invokeinterface.
        let long_put = [0x2a, 0xb2, 0, 1, 0x2b, 0x1c, 0x2e, 0x85, 0x21, 0xb9, 0, 2, 6, 0, 0xb1];
        assert_eq!(find(&long_put, 9, SET_LONG), Some(1));
        // The layout is a parameter (`aload_2`), not a `getstatic`.
        let param = [0x2a, 0x2c, 0x1b, 0x85, 0xb9, 0, 2, 3, 0, 0xac];
        assert_eq!(find(&param, 4, GET_INT), None);
        // The coordinate is a field read: its width is not known here.
        let field = [0x2a, 0xb2, 0, 1, 0x2a, 0xb4, 0, 4, 0xb9, 0, 2, 3, 0, 0xac];
        assert_eq!(find(&field, 8, GET_INT), None);
        // A jump lands inside the coordinate's computation (`goto` to the
        // `i2l` at pc 5): another path may have pushed the operands.
        let joined = [0x2a, 0xb2, 0, 1, 0x1b, 0x85, 0xb9, 0, 2, 3, 0, 0xac, 0xa7, 0xff, 0xf9];
        assert_eq!(find(&joined, 6, GET_INT), None);
        // A pc that is no instruction start, and a get whose operand count is
        // wrong for the accessor (a put read as a get).
        assert_eq!(find(&GET_BODY, 5 + 1 + 1, GET_INT), None);
        assert_eq!(find(&GET_BODY, 40, GET_INT), None, "past the body");
        assert_eq!(find(&put, 8, GET_INT), None);
    }

    /// Remaining design items 2-4 as inputs of the one admission: a native
    /// segment needs the arena table, an FP carrier the raw-bits rows, and
    /// the layout operand a filed `getstatic`.
    #[test]
    fn j27_spliceffm2_the_builder_declines_are_inputs_of_the_admission() {
        let call = get_call();
        assert!(admits(call, false, false, true), "heap: no arena table needed");
        assert!(!admits(call, true, false, true), "native without the arena table");
        assert!(admits(call, true, true, true), "native with it");
        assert!(!admits(call, false, true, false), "the layout's facts are not filed");
        let param = [0x2a, 0x2c, 0x1b, 0x85, 0xb9, 0, 2, 3, 0, 0xac];
        let not_static = SplicedFfmCall {
            target: GET_INT,
            code: &param,
            pc: 4,
        };
        assert!(!admits(not_static, false, true, true), "the layout is no getstatic");
        // `s.get(JAVA_DOUBLE, off)`: aload_0; getstatic; lload_1; invokeinterface.
        let fp = [0x2a, 0xb2, 0, 1, 0x1f, 0xb9, 0, 2, 4, 0, 0xaf];
        let fp_call = SplicedFfmCall {
            target: GET_DOUBLE,
            code: &fp,
            pc: 5,
        };
        let raw_bits = |v: Option<&str>| admits_raw_bits(v, fp_call, false, true, true);
        assert!(raw_bits(None), "FP raw bits default ON");
        assert!(!raw_bits(Some("0")), "an FP carrier without the raw-bits rows");
    }

    /// Remaining design item 1 and the backstop: a `MemorySegment` element
    /// call the build left INSIDE a splice names its top-level site, once
    /// per site; a root call, another call, or the switch off names nothing.
    #[test]
    fn j27_spliceffm2_a_declined_spliced_accessor_names_its_top_level_site() {
        const CODE_LEN: usize = 100;
        // Two top-level splices: the one at caller pc 10 covers [100, 180)
        // (its nested bodies included), the one at pc 40 covers [180, 220).
        let ranges = [(100, 180, 10), (180, 220, 40)];
        let other = ("p/Main", "heapPut", "(Ljava/lang/foreign/MemorySegment;)V");
        let declined = |calls: &[(usize, (&'static str, &'static str, &'static str))], on: Option<&str>| {
            cratonvm_types::flags::with_thread_overrides(&[(SPLICE, on)], || {
                spliced_ffm_declined_sites(calls.iter().copied(), CODE_LEN, &ranges)
            })
        };
        let on = Some("1");
        assert_eq!(declined(&[], on), None, "no call");
        assert_eq!(declined(&[(20, GET_INT)], on), None, "a root accessor is not a splice decline");
        assert_eq!(declined(&[(120, other)], on), None, "another spliced call");
        assert_eq!(
            declined(&[(150, SET_INT), (120, GET_INT), (190, GET_INT)], on),
            Some(vec![10, 40]),
            "each declining splice once"
        );
        assert_eq!(declined(&[(300, GET_INT)], on), Some(vec![]), "declined, no site to drop");
        assert_eq!(declined(&[(120, GET_INT)], None), None, "the opt-in switch off");
    }
}
