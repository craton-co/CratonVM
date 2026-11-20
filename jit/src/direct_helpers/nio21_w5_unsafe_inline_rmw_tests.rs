use super::*;

/// JIT round 20 wave 11, lane pAtom (W6-L3-3 / AT-5): the inline
/// resolver keeps a Java-body RMW row as a call exactly when the prefix
/// would serve it; a CAS row, a refused mask, another class and a near-miss
/// descriptor are left to the ordinary screens.
#[test]
fn a_java_body_rmw_row_is_kept_as_a_call_only_where_the_prefix_serves_it() {
    if !cfg!(target_arch = "x86_64")
        || !unsafe_rmw_inline_enabled()
        || !unsafe_bitwise_inline_enabled()
        || !unsafe_accessor_inline_enabled()
        || !crate::ir_thin_prefix_enabled()
        || !unsafe_rmw_body_no_splice_enabled()
    {
        return;
    }
    let u = "jdk/internal/misc/Unsafe";
    let all = usize::MAX;
    let veto = |m: &str, d: &str, mask: usize| {
        unsafe_inline_rmw_body_splice_loses_prefix(u, m, d, mask)
    };
    assert!(veto("getAndAddInt", "(Ljava/lang/Object;JI)I", all));
    assert!(veto("getAndAddLong", "(Ljava/lang/Object;JJ)J", all));
    assert!(veto("getAndSetInt", "(Ljava/lang/Object;JI)I", all));
    assert!(veto("getAndSetLong", "(Ljava/lang/Object;JJ)J", all));
    assert!(veto("getAndBitwiseOrInt", "(Ljava/lang/Object;JI)I", all));
    assert!(veto(
        "getAndBitwiseXorLongRelease",
        "(Ljava/lang/Object;JJ)J",
        all
    ));
    // Exactly the row's own admission: `getAndAddInt` needs bits 14 + 10.
    assert!(veto(
        "getAndAddInt",
        "(Ljava/lang/Object;JI)I",
        (1 << 14) | (1 << 10)
    ));
    assert!(!veto("getAndAddInt", "(Ljava/lang/Object;JI)I", 1 << 14));
    assert!(!veto("getAndAddInt", "(Ljava/lang/Object;JI)I", 0));
    // A CAS body is one native call whose own prefix survives the splice.
    assert!(!veto(
        "weakCompareAndSetInt",
        "(Ljava/lang/Object;JII)Z",
        all
    ));
    assert!(!veto(
        "compareAndExchangeIntAcquire",
        "(Ljava/lang/Object;JII)I",
        all
    ));
    assert!(!veto("getAndAddInt", "(Ljava/lang/Object;JJ)J", all));
    assert!(!veto(
        "getAndAddReference",
        "(Ljava/lang/Object;JLjava/lang/Object;)Ljava/lang/Object;",
        all
    ));
    assert!(!unsafe_inline_rmw_body_splice_loses_prefix(
        "sun/misc/Unsafe",
        "getAndAddInt",
        "(Ljava/lang/Object;JI)I",
        all
    ));
}

#[test]
fn nio21_w5_rmw_rows_ask_the_cas_bits() {
    assert_eq!(
        UnsafeInlineAccess::GATE_BITS[14],
        ("compareAndSetInt", "(Ljava/lang/Object;JII)Z")
    );
    assert_eq!(
        UnsafeInlineAccess::GATE_BITS[15],
        ("compareAndSetLong", "(Ljava/lang/Object;JJJ)Z")
    );
    if !unsafe_rmw_inline_enabled() {
        return;
    }
    let u = "jdk/internal/misc/Unsafe";
    let cas =
        unsafe_inline_rmw_site(u, "compareAndSetInt", "(Ljava/lang/Object;JII)Z").unwrap();
    assert_eq!(
        (cas.width, cas.op, cas.num_params(), cas.ret_type()),
        (4, UnsafeRmwOp::Cas, 4, b'Z')
    );
    assert!(cas.admitted(1 << 14) && !cas.admitted(0x3FFF) && !cas.admitted(1 << 15));
    let weak = unsafe_inline_rmw_site(
        u,
        "weakCompareAndSetLongAcquire",
        "(Ljava/lang/Object;JJJ)Z",
    )
    .unwrap();
    assert_eq!((weak.width, weak.op), (8, UnsafeRmwOp::Cas));
    assert!(weak.admitted(1 << 15) && !weak.admitted(1 << 14));
    let add = unsafe_inline_rmw_site(u, "getAndAddInt", "(Ljava/lang/Object;JI)I").unwrap();
    assert_eq!((add.num_params(), add.ret_type()), (3, b'I'));
    assert!(add.admitted((1 << 14) | (1 << 10)));
    assert!(
        !add.admitted(1 << 14),
        "the getIntVolatile row is needed too"
    );
    let set = unsafe_inline_rmw_site(u, "getAndSetLong", "(Ljava/lang/Object;JJ)J").unwrap();
    assert_eq!((set.op, set.ret_type()), (UnsafeRmwOp::GetAndSet, b'J'));
    assert!(set.admitted((1 << 15) | (1 << 11)) && !set.admitted((1 << 14) | (1 << 10)));
    for (c, m, d) in [
        // nio21 wave 7 L3: `compareAndExchangeInt` is served now; its
        // reference and mismatched neighbours are not.
        (
            u,
            "compareAndExchangeReference",
            "(Ljava/lang/Object;JLjava/lang/Object;Ljava/lang/Object;)Ljava/lang/Object;",
        ),
        (u, "compareAndExchangeInt", "(Ljava/lang/Object;JJJ)J"),
        (
            u,
            "compareAndSetReference",
            "(Ljava/lang/Object;JLjava/lang/Object;Ljava/lang/Object;)Z",
        ),
        // nio21 wave 6 L3: `getAndBitwiseOrInt` is served now; its
        // narrow, mismatched and `Nand`-shaped neighbours are not.
        (u, "getAndBitwiseOrByte", "(Ljava/lang/Object;JB)B"),
        (u, "getAndBitwiseOrInt", "(Ljava/lang/Object;JJ)J"),
        (u, "getAndBitwiseNandInt", "(Ljava/lang/Object;JI)I"),
        (u, "getAndAddByte", "(Ljava/lang/Object;JB)B"),
        (u, "compareAndSetInt", "(Ljava/lang/Object;JJJ)Z"),
        (
            "sun/misc/Unsafe",
            "compareAndSwapInt",
            "(Ljava/lang/Object;JII)Z",
        ),
        ("sun/misc/Unsafe", "getAndAddInt", "(Ljava/lang/Object;JI)I"),
    ] {
        assert!(unsafe_inline_rmw_site(c, m, d).is_none(), "{c}.{m}{d}");
    }
}


/// nio21 wave 7 L3 (W6-L3-2): the `compareAndExchange*` rows ask their
/// own bit (16 / 17) and the width's CAS bit, and keep the witness.
#[test]
fn nio21_w7_cae_rows_keep_the_witness_behind_their_own_bits() {
    assert_eq!(
        UnsafeInlineRmw::CAE_FIRST_BIT,
        UnsafeInlineAccess::GATE_BITS.len()
    );
    let cae4 = unsafe_inline_rmw_bytes(UnsafeInlineRmw {
        width: 4,
        op: UnsafeRmwOp::Cae,
    });
    assert_eq!(
        cae4,
        [0x49, 0x92, 0xF0, 0x43, 0x0F, 0xB1, 0x14, 0x1A, 0x48, 0x63, 0xC0]
    );
    let cae8 = unsafe_inline_rmw_bytes(UnsafeInlineRmw {
        width: 8,
        op: UnsafeRmwOp::Cae,
    });
    assert_eq!(cae8, [0x49, 0x92, 0xF0, 0x4B, 0x0F, 0xB1, 0x14, 0x1A]);
    if !unsafe_rmw_inline_enabled() || !unsafe_cae_inline_enabled() {
        return;
    }
    let u = "jdk/internal/misc/Unsafe";
    for (m, d, w, ret) in [
        (
            "compareAndExchangeInt",
            "(Ljava/lang/Object;JII)I",
            4u8,
            b'I',
        ),
        (
            "compareAndExchangeIntAcquire",
            "(Ljava/lang/Object;JII)I",
            4,
            b'I',
        ),
        (
            "compareAndExchangeLongRelease",
            "(Ljava/lang/Object;JJJ)J",
            8,
            b'J',
        ),
    ] {
        let r = unsafe_inline_rmw_site(u, m, d).unwrap_or_else(|| panic!("{m}{d}"));
        assert_eq!(
            (r.width, r.op, r.num_params(), r.ret_type()),
            (w, UnsafeRmwOp::Cae, 4, ret)
        );
        assert!(r.has_expected());
        let own = 1usize << (16 + (w == 8) as usize);
        let cas = 1usize << (14 + (w == 8) as usize);
        assert!(r.admitted(own | cas), "{m}");
        assert!(!r.admitted(own), "{m}: the CAS row is needed too");
        assert!(
            !r.admitted(0xFFFF),
            "{m}: the sixteen static rows do not admit it"
        );
    }
    assert_eq!(
        UnsafeInlineRmw::CAE_GATE_BITS,
        [
            ("compareAndExchangeInt", "(Ljava/lang/Object;JII)I"),
            ("compareAndExchangeLong", "(Ljava/lang/Object;JJJ)J"),
        ]
    );
}


/// nio21 wave 6 L3 (W5-L3-2): the `getAndBitwise*` rows and their loop.
#[test]
fn nio21_w6_bitwise_rows_and_bytes() {
    if !unsafe_rmw_inline_enabled() || !unsafe_bitwise_inline_enabled() {
        return;
    }
    let u = "jdk/internal/misc/Unsafe";
    let or =
        unsafe_inline_rmw_site(u, "getAndBitwiseOrInt", "(Ljava/lang/Object;JI)I").unwrap();
    assert_eq!(
        (or.width, or.op),
        (4, UnsafeRmwOp::GetAndBitwise(UnsafeBitOp::Or))
    );
    assert_eq!((or.num_params(), or.ret_type()), (3, b'I'));
    assert!(or.admitted((1 << 14) | (1 << 10) | (1 << 3)));
    assert!(
        !or.admitted((1 << 14) | (1 << 10)),
        "the Release body's getInt row"
    );
    let xr =
        unsafe_inline_rmw_site(u, "getAndBitwiseXorLongRelease", "(Ljava/lang/Object;JJ)J")
            .unwrap();
    assert_eq!(
        (xr.width, xr.op, xr.ret_type()),
        (8, UnsafeRmwOp::GetAndBitwise(UnsafeBitOp::Xor), b'J')
    );
    let and4 = unsafe_inline_rmw_bytes(UnsafeInlineRmw {
        width: 4,
        op: UnsafeRmwOp::GetAndBitwise(UnsafeBitOp::And),
    });
    assert_eq!(
        and4,
        [
            0x49, 0x89, 0xC2, // MOV R10, RAX
            0x43, 0x8B, 0x04, 0x1A, // MOV EAX, [R10+R11]
            0x89, 0xC1, // MOV ECX, EAX
            0x21, 0xD1, // AND ECX, EDX
            0xF0, 0x43, 0x0F, 0xB1, 0x0C, 0x1A, // LOCK CMPXCHG [R10+R11], ECX
            0x75, 0xF4, // JNE -12
            0x48, 0x63, 0xC0, // MOVSXD RAX, EAX
        ]
    );
    let or8 = unsafe_inline_rmw_bytes(UnsafeInlineRmw {
        width: 8,
        op: UnsafeRmwOp::GetAndBitwise(UnsafeBitOp::Or),
    });
    assert_eq!(&or8[7..13], &[0x48, 0x89, 0xC1, 0x48, 0x09, 0xD1]);
    assert_eq!(&or8[19..], &[0x75, 0xF2], "JNE -14, no widening");
}
