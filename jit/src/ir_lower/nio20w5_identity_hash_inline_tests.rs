use cratonvm_types::{
    ArrayElementType, ClassId, ObjectHeader, ObjectKind, GC_FLAG_COMPACT, MARK_FORWARDED,
    MARK_STATE_MASK,
};

/// The read, EXECUTED over real `ObjectHeader`s: `Some(hash)` on the
/// fall-through, `None` on any decline edge.
pub(super) fn run(obj: &ObjectHeader) -> Option<i32> {
    use crate::ExecutableBuffer;
    let (bytes, decline) = super::identity_hash_read_bytes();
    // MOV RAX, arg0 ; <read> ; RET ; decline: MOV RAX, -1 ; RET.
    let arg0 = super::CALL_ARG_REGS[0];
    // MOV RAX, r64 (REX.W [+R] 89 /r).
    let mut body = vec![
        0x48 | (u8::from(arg0 >= 8) << 2),
        0x89,
        0xC0 | ((arg0 & 7) << 3),
    ];
    let at = body.len();
    body.extend_from_slice(&bytes);
    body.push(0xC3);
    let decline_at = body.len();
    body.extend_from_slice(&[0x48, 0xC7, 0xC0, 0xFF, 0xFF, 0xFF, 0xFF, 0xC3]);
    for d in decline {
        let operand = at + d;
        let rel = i32::try_from(decline_at).unwrap() - i32::try_from(operand + 4).unwrap();
        body[operand..operand + 4].copy_from_slice(&rel.to_le_bytes());
    }
    let mut buf = ExecutableBuffer::new(512).expect("executable buffer");
    buf.emit(&body);
    assert!(!buf.overflowed());
    buf.finalize();
    // SAFETY: `buf` is finalized and holds a complete
    // `extern "C" fn(u64) -> u64` that reads 16 bytes at its argument and
    // writes only RAX, RCX and RDX, all caller-saved on both ABIs.
    let f =
        unsafe { std::mem::transmute::<*const u8, extern "C" fn(u64) -> u64>(buf.as_ptr()) };
    let r = f(obj as *const ObjectHeader as u64) as i64;
    if r == -1 {
        None
    } else {
        // The fall-through sign-extends a non-negative i32.
        assert!(
            (0..=i64::from(i32::MAX)).contains(&r),
            "not a sign-extended i32: {r:#x}"
        );
        Some(r as i32) // Cast: range-checked above
    }
}

fn plain(cid: u32) -> Box<ObjectHeader> {
    Box::new(ObjectHeader::new(
        ClassId::new(cid),
        ObjectKind::Object,
        ArrayElementType::Reference,
        0,
        2,
    ))
}

pub(super) fn compact(cid: u32) -> Box<ObjectHeader> {
    let h = plain(cid);
    h.add_gc_flags(GC_FLAG_COMPACT);
    assert!(h.is_short());
    h
}

/// Long headers (legacy instance, primitive and reference arrays): the aux
/// word in every lock state; unhashed and forwarded decline.
#[test]
#[cfg(target_arch = "x86_64")]
fn long_headers_answer_the_aux_word_in_any_lock_state() {
    let makers: [fn() -> Box<ObjectHeader>; 3] = [
        || plain(7),
        || {
            Box::new(ObjectHeader::new(
                ClassId::new(9),
                ObjectKind::Array,
                ArrayElementType::Int,
                5,
                0,
            ))
        },
        || {
            Box::new(ObjectHeader::new(
                ClassId::new(11),
                ObjectKind::Array,
                ArrayElementType::Reference,
                3,
                0,
            ))
        },
    ];
    for make in makers {
        let h = make();
        assert_eq!(
            run(&h),
            None,
            "an unhashed long header declines (the call mints)"
        );
        let want = h.identity_hash(|| 0x1234_5678).unwrap();
        assert_eq!(h.installed_identity_hash(), want);
        assert_eq!(run(&h), Some(want));
        let m = h.mark();
        h.mark_word.store(
            ObjectHeader::make_thin_locked(m, 5, 1),
            std::sync::atomic::Ordering::Relaxed,
        );
        assert_eq!(
            run(&h),
            Some(want),
            "thin-locked long header keeps its aux hash"
        );
        h.mark_word.store(
            ObjectHeader::make_inflated(m),
            std::sync::atomic::Ordering::Relaxed,
        );
        assert_eq!(
            run(&h),
            Some(want),
            "inflated long header keeps its aux hash"
        );
        h.mark_word.store(
            (m & !MARK_STATE_MASK) | MARK_FORWARDED,
            std::sync::atomic::Ordering::Relaxed,
        );
        assert_eq!(run(&h), None, "a forwarded header always declines");
    }
}

/// Compact instances: the NEUTRAL hash bits mixed with the class id, for
/// many class ids and hash values (both halves of the 20 bits, the forced
/// `1` of a zero low part); locked and unhashed decline.
#[test]
#[cfg(target_arch = "x86_64")]
fn compact_instances_answer_exactly_what_the_header_decoder_answers() {
    for cid in [
        0u32,
        1,
        2,
        7,
        1000,
        65_535,
        0x7FFF_FFFF,
        0x8000_0001,
        u32::MAX - 1,
    ] {
        let h = compact(cid);
        assert_eq!(
            run(&h),
            None,
            "unhashed compact instance declines (cid {cid})"
        );
        for mint in [
            1i32,
            2,
            0x3FFF,
            0x4000,
            0xF_FFFF,
            0x10_0000,
            0x7FFF_FFFF,
            0x1234_5678,
            -5,
        ] {
            let h = compact(cid);
            let want = h.identity_hash(|| mint).unwrap();
            assert_ne!(want, 0);
            assert_eq!(h.installed_identity_hash(), want);
            assert_eq!(run(&h), Some(want), "cid {cid} mint {mint:#x}");
        }
        let h = compact(cid);
        let m = h.mark();
        h.mark_word.store(
            ObjectHeader::make_thin_locked(m, 3, 0),
            std::sync::atomic::Ordering::Relaxed,
        );
        assert_eq!(run(&h), None, "thin-locked compact instance declines");
        h.mark_word.store(
            ObjectHeader::make_inflated(m),
            std::sync::atomic::Ordering::Relaxed,
        );
        assert_eq!(
            run(&h),
            None,
            "inflated compact instance declines (hash in the monitor)"
        );
    }
}

#[test]
fn the_site_screen_names_only_identity_hash_sites() {
    use super::ir_identity_hash_site as s;
    assert_eq!(
        s(
            "java/lang/System",
            "identityHashCode",
            "(Ljava/lang/Object;)I",
            3
        ),
        Some(true)
    );
    assert_eq!(
        s(
            "java/lang/System",
            "identityHashCode",
            "(Ljava/lang/Object;)I",
            0
        ),
        None
    );
    assert_eq!(s("java/lang/Class", "hashCode", "()I", 0), Some(false));
    assert_eq!(s("java/lang/Class", "hashCode", "()I", 2), Some(false));
    assert_eq!(s("java/lang/Class", "hashCode", "()I", 1), None);
    assert_eq!(s("java/lang/Object", "hashCode", "()I", 0), None);
    assert_eq!(s("java/lang/String", "hashCode", "()I", 0), None);
}

#[test]
fn the_class_selector_is_its_own() {
    let others = [
        crate::direct_helpers::WELL_KNOWN_CLASS_INTEGER,
        crate::direct_helpers::WELL_KNOWN_CLASS_STRING,
        crate::direct_helpers::WELL_KNOWN_STRING_HASH_FIELD,
        super::WELL_KNOWN_INTEGER_CACHE_HIGH,
        super::WELL_KNOWN_CLASS_LONG,
        super::WELL_KNOWN_INTEGER_CACHE_SLOTS,
        super::WELL_KNOWN_INTEGER_CACHE_SLOTS_LEN,
        super::WELL_KNOWN_LONG_CACHE_SLOTS,
        super::WELL_KNOWN_SESSION_GLOBAL,
        super::WELL_KNOWN_SESSION_HEAP,
        super::WELL_KNOWN_SESSION_IMPLICIT,
    ];
    assert!(!others.contains(&super::WELL_KNOWN_CLASS_CLASS));
    unsafe extern "C" fn answers(ctx: i64, which: i64) -> i64 {
        match (ctx, which) {
            (0x1000, super::WELL_KNOWN_CLASS_CLASS) => 42,
            (0x1000, _) => 9,
            _ => 0,
        }
    }
    let table = |ctx: usize| crate::DirectHelperTable {
        static_base_resolver_ctx: ctx,
        well_known_class_id: answers as *const () as usize,
        ..crate::DirectHelperTable::EMPTY
    };
    assert_eq!(super::ir_class_class_id(&table(0x1000)), Some(42));
    assert_eq!(super::ir_class_class_id(&table(0x2000)), None);
    assert_eq!(super::ir_class_class_id(&table(0)), None);
    assert_eq!(
        super::ir_class_class_id(&crate::DirectHelperTable::EMPTY),
        None
    );
}
