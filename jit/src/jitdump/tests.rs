use super::*;

fn u32_at(b: &[u8], at: usize) -> u32 {
    u32::from_ne_bytes(b[at..at + 4].try_into().unwrap())
}
fn i32_at(b: &[u8], at: usize) -> i32 {
    i32::from_ne_bytes(b[at..at + 4].try_into().unwrap())
}
fn u64_at(b: &[u8], at: usize) -> u64 {
    u64::from_ne_bytes(b[at..at + 8].try_into().unwrap())
}

#[test]
fn header_is_forty_bytes_with_magic_version_and_machine() {
    let mut out = Vec::new();
    encode_header(&mut out, EM_X86_64, 1234, 99);
    assert_eq!(out.len(), HEADER_SIZE);
    assert_eq!(u32_at(&out, 0), JITHEADER_MAGIC);
    assert_eq!(&out[0..4], &0x4A69_5444u32.to_ne_bytes());
    assert_eq!(u32_at(&out, 4), 1);
    assert_eq!(u32_at(&out, 8), 40);
    assert_eq!(u32_at(&out, 12), 62);
    assert_eq!(u32_at(&out, 20), 1234);
    assert_eq!(u64_at(&out, 24), 99);
    assert_eq!(u64_at(&out, 32), 0);
}

#[test]
fn code_load_record_layout() {
    let code = [0x55u8, 0x48, 0x89, 0xE5, 0xC3];
    let name = "a/B.m()V [c1]";
    let mut out = Vec::new();
    encode_code_load(&mut out, 7, 8, 42, 3, 0x1000, &code, name);
    assert_eq!(CODE_LOAD_FIXED_SIZE, 56);
    assert_eq!(out.len(), 56 + name.len() + 1 + code.len());
    assert_eq!(u32_at(&out, 0), JIT_CODE_LOAD);
    assert_eq!(u32_at(&out, 4) as usize, out.len());
    assert_eq!(u64_at(&out, 8), 42);
    assert_eq!(u32_at(&out, 16), 7);
    assert_eq!(u32_at(&out, 20), 8);
    assert_eq!(u64_at(&out, 24), 0x1000); // vma
    assert_eq!(u64_at(&out, 32), 0x1000); // code_addr
    assert_eq!(u64_at(&out, 40), code.len() as u64);
    assert_eq!(u64_at(&out, 48), 3);
    assert_eq!(&out[56..56 + name.len()], name.as_bytes());
    assert_eq!(out[56 + name.len()], 0);
    assert_eq!(&out[57 + name.len()..], &code);
}

#[test]
fn interior_nul_in_a_name_does_not_change_the_record_size() {
    let mut a = Vec::new();
    encode_code_load(&mut a, 1, 1, 1, 0, 0x10, &[0xC3], "ab\0c");
    assert_eq!(a.len(), code_load_size("ab\0c", 1));
    assert_eq!(u32_at(&a, 4) as usize, a.len());
}

#[test]
fn close_record_is_a_bare_prefix() {
    let mut out = Vec::new();
    encode_code_close(&mut out, 5);
    assert_eq!(out.len(), RECORD_PREFIX_SIZE);
    assert_eq!(u32_at(&out, 0), JIT_CODE_CLOSE);
    assert_eq!(u32_at(&out, 4), 16);
    assert_eq!(u64_at(&out, 8), 5);
}

#[test]
fn unwinding_record_is_padded_to_eight_and_sizes_agree() {
    for code_size in [1usize, 7, 8, 9, 4096, 12345] {
        let unwinding = build_x64_eh_frame(code_size);
        let mut out = Vec::new();
        encode_unwinding_info(&mut out, 1, &unwinding, EH_FRAME_HDR_SIZE);
        assert_eq!(out.len() % 8, 0);
        assert_eq!(u32_at(&out, 0), JIT_CODE_UNWINDING_INFO);
        assert_eq!(u32_at(&out, 4) as usize, out.len());
        assert_eq!(u64_at(&out, 16) as usize, unwinding.len());
        assert_eq!(u64_at(&out, 24) as usize, EH_FRAME_HDR_SIZE);
        assert_eq!(u64_at(&out, 32) as usize, unwinding.len());
        assert_eq!(&out[40..40 + unwinding.len()], &unwinding[..]);
    }
}

#[test]
fn eh_frame_offsets_resolve_to_the_code_and_the_fde() {
    let code_size = 13usize; // align8 → 16
    let buf = build_x64_eh_frame(code_size);
    let eh_frame_size = buf.len() - EH_FRAME_HDR_SIZE;
    // CIE and FDE records are each 8-byte aligned, then a 4-byte terminator.
    let cie_len = u32_at(&buf, 0) as usize + 4;
    assert_eq!(cie_len % 8, 0);
    assert_eq!(u32_at(&buf, 4), 0, "CIE id");
    let fde_start = cie_len;
    let fde_len = u32_at(&buf, fde_start) as usize + 4;
    assert_eq!(fde_len % 8, 0);
    assert_eq!(
        u32_at(&buf, fde_start + 4) as usize,
        fde_start + 4,
        "CIE pointer"
    );
    assert_eq!(fde_start + fde_len + 4, eh_frame_size);
    assert_eq!(u32_at(&buf, eh_frame_size - 4), 0, "terminator");

    // Place .eh_frame at E = T + 16 for text base T; the code must be at T.
    let text: i64 = 0x80;
    let e = text + 16;
    let pc_begin_field = e + fde_start as i64 + 8;
    assert_eq!(pc_begin_field + i32_at(&buf, fde_start + 8) as i64, text);
    assert_eq!(u32_at(&buf, fde_start + 12) as usize, code_size);

    let hdr = eh_frame_size;
    let hdr_addr = e + hdr as i64;
    assert_eq!(buf[hdr], 1);
    assert_eq!(
        hdr_addr + 4 + i32_at(&buf, hdr + 4) as i64,
        e,
        "eh_frame_ptr"
    );
    assert_eq!(u32_at(&buf, hdr + 8), 1, "fde_count");
    assert_eq!(
        hdr_addr + i32_at(&buf, hdr + 12) as i64,
        text,
        "initial_loc"
    );
    assert_eq!(
        hdr_addr + i32_at(&buf, hdr + 16) as i64,
        e + fde_start as i64,
        "fde address"
    );
}

#[test]
fn the_frame_record_check_reads_the_bytes() {
    assert!(has_standard_x64_frame_record(&[
        0x55, 0x48, 0x89, 0xE5, 0x48
    ]));
    assert!(!has_standard_x64_frame_record(&[0x48, 0x89, 0xE5]));
    assert!(!has_standard_x64_frame_record(&[0x55]));
}

/// Interpreter round i1 wave 22, lane L6: a body behind the not-entrant
/// entry pad still gets its unwind table, with the push one pad later.
#[test]
fn the_padded_frame_record_is_found_after_the_entry_pad() {
    let mut code = crate::ENTRY_PATCH_PAD.to_vec();
    code.extend_from_slice(&[0x55, 0x48, 0x89, 0xE5, 0xC3]);
    assert_eq!(standard_x64_frame_record_lead(&code), Some(5));
    assert!(has_standard_x64_frame_record(&code));
    assert_eq!(
        standard_x64_frame_record_lead(&[0x55, 0x48, 0x89, 0xE5]),
        Some(0)
    );
    assert_eq!(
        standard_x64_frame_record_lead(&crate::ENTRY_PATCH_PAD),
        None
    );
    let plain = build_x64_eh_frame(16);
    let padded = build_x64_eh_frame_with_lead(16, 5);
    assert_eq!(plain.len(), padded.len());
    assert_ne!(plain, padded, "the first advance must move by the pad");
}
