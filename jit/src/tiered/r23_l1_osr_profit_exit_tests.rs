use super::osr_profit_exit_headers;

/// Two nested `goto` loops and a bottom-tested (`if_icmplt`) loop: every
/// back-edge target once, sorted; a method without a loop has none.
#[test]
fn r23_l1_profit_exit_headers_are_every_back_edge_target() {
    let code: Vec<u8> = vec![
        0x03, // 0: iconst_0
        0x3c, // 1: istore_1
        0x84, 0x01, 0x01, // 2: iinc 1 1        <- outer header
        0x84, 0x02, 0x01, // 5: iinc 2 1        <- inner header
        0xa7, 0xff, 0xfd, // 8: goto 5          (inner back edge)
        0xa7, 0xff, 0xf7, // 11: goto 2         (outer back edge)
        0x84, 0x03, 0x01, // 14: iinc 3 1       <- third header
        0x1b, // 17: iload_1
        0x1c, // 18: iload_2
        0xa1, 0xff, 0xfb, // 19: if_icmplt 14   (bottom-tested back edge)
        0xa7, 0xff, 0xeb, // 22: goto 1         (an edge back to pc 1)
        0xb1, // 25: return
    ];
    assert_eq!(osr_profit_exit_headers(&code), vec![1, 2, 5, 14]);
    // Two edges to one header count once.
    let twice: Vec<u8> = vec![
        0x84, 0x01, 0x01, // 0: iinc 1 1
        0x1b, // 3: iload_1
        0x99, 0xff, 0xfc, // 4: ifeq 0
        0xa7, 0xff, 0xf9, // 7: goto 0
        0xb1, // 10: return
    ];
    assert_eq!(osr_profit_exit_headers(&twice), vec![0]);
    assert!(osr_profit_exit_headers(&[0x03, 0xac]).is_empty());
    assert!(osr_profit_exit_headers(&[]).is_empty());
}
