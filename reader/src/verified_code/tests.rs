use super::*;

#[test]
fn canonical_cfg_finds_merges_and_loop_headers() {
    // iconst_0; istore_0; iinc 0,1; iload_0; bipush 10;
    // if_icmplt -7; return
    let code = [
        0x03, 0x3b, 0x84, 0x00, 0x01, 0x1a, 0x10, 0x0a, 0xa1, 0xff, 0xfa, 0xb1,
    ];
    let verified = verified_code(&code).unwrap();
    assert_eq!(verified.loop_headers(), &[2]);
    assert_eq!(verified.merge_targets(), &[2, 11]);
    assert_eq!(verified.instructions().len(), 7);
    assert!(verified.is_instruction_start(8));
    assert!(!verified.is_instruction_start(9));
}

#[test]
fn rejects_branch_into_instruction_operand() {
    // bipush 1; goto -1 (targets the goto's first operand)
    let error = verified_code(&[0x10, 0x01, 0xa7, 0xff, 0xff]).unwrap_err();
    assert!(error.to_string().contains("instruction boundary"));
}

#[test]
fn cache_returns_same_analysis_arc() {
    let first = verified_code(&[0x03, 0xac]).unwrap();
    let second = verified_code(&[0x03, 0xac]).unwrap();
    assert!(Arc::ptr_eq(&first, &second));
}

#[test]
fn rejects_truncated_instruction() {
    assert!(verified_code(&[0x11, 0x01]).is_err());
}

/// SBR round 5, lane verify: `decode_instructions` (what the verifier walks)
/// and `verified_code` (what the compilers get) are one decoder: the same
/// instructions for valid code, the same error text for invalid code.
#[test]
fn uncached_decode_agrees_with_the_cached_one() {
    let valid: &[&[u8]] = &[
        &[
            0x03, 0x3b, 0x84, 0x00, 0x01, 0x1a, 0x10, 0x0a, 0xa1, 0xff, 0xfa, 0xb1,
        ],
        &[0x2a, 0xb7, 0x00, 0x01, 0xb1],
        &[0xb1],
        // tableswitch at pc 0 (3 pad bytes): default 24, low 0, high 1, two offsets
        &[
            0xaa, 0, 0, 0, 0, 0, 0, 24, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 25, 0, 0, 0, 26, 0xb1,
            0xb1, 0xb1,
        ],
    ];
    let invalid: &[&[u8]] = &[
        &[0x10, 0x01, 0xa7, 0xff, 0xff],
        &[0x11, 0x01],
        &[0xa7, 0x7f, 0xff],
        &[0xc4],
    ];
    for code in valid {
        let cached = verified_code(code).unwrap();
        let plain = decode_instructions(code).unwrap();
        assert_eq!(cached.instructions(), plain.instructions(), "{code:?}");
    }
    for code in invalid {
        let cached = verified_code(code).unwrap_err().to_string();
        let plain = decode_instructions(code).unwrap_err().to_string();
        assert_eq!(cached, plain, "{code:?}");
    }
    let too_long = vec![0u8; u16::MAX as usize + 1];
    assert_eq!(
        verified_code(&too_long).unwrap_err().to_string(),
        decode_instructions(&too_long).unwrap_err().to_string()
    );
}
