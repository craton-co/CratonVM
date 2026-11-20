use super::*;

fn targets(len: usize, hit: &[usize]) -> Vec<bool> {
    let mut t = vec![false; len + 4];
    for &p in hit {
        t[p] = true;
    }
    t
}

#[test]
fn a_vm_free_window_ends_at_its_xreturn() {
    // `x.m(); return this;`
    let code = [0x57u8, 0x2a, 0xb0];
    assert_eq!(lazy_return_window_end(&code, 0, &targets(3, &[])), Some(2));
    // `x.m(); return true;`
    let code = [0x57u8, 0x04, 0xac];
    assert_eq!(lazy_return_window_end(&code, 0, &targets(3, &[])), Some(2));
    // `return x.m() + 1;`
    let code = [0x04u8, 0x60, 0xac];
    assert_eq!(lazy_return_window_end(&code, 0, &targets(3, &[])), Some(2));
    // `return (byte) (x.m() ^ k & 7);` with a two- and a three-byte operand.
    let code = [0x15u8, 0x02, 0x82, 0x11, 0x00, 0x07, 0x7e, 0x91, 0xac];
    assert_eq!(lazy_return_window_end(&code, 0, &targets(9, &[])), Some(8));
    // The zero-length window is the wave-7 rule.
    let code = [0xb1u8];
    assert_eq!(lazy_return_window_end(&code, 0, &targets(1, &[])), Some(0));
}

#[test]
fn a_window_refuses_anything_that_can_enter_the_vm_or_merge() {
    let none =
        |code: &[u8], bt: &[usize]| lazy_return_window_end(code, 0, &targets(code.len(), bt));
    // getfield (implicit null check), ldc (resolution), checkcast (slow
    // arm), idiv / irem (zero-divisor throw), an invoke, stores, iinc, a
    // branch, athrow.
    for op in [
        0xb4u8, 0x12, 0xc0, 0x6c, 0x70, 0xb6, 0x3a, 0x36, 0x84, 0xa7, 0x99, 0xbf,
    ] {
        assert_eq!(
            none(&[0x2a, op, 0x00, 0x00, 0xb0], &[]),
            None,
            "op {op:#04x}"
        );
    }
    // A merge point inside the window, at its start, or on its `xreturn`.
    assert_eq!(none(&[0x57, 0x2a, 0xb0], &[1]), None);
    assert_eq!(none(&[0x57, 0x2a, 0xb0], &[2]), None);
    assert_eq!(none(&[0x57, 0x2a, 0xb0], &[0]), None);
    // Longer than the bound.
    let mut long = vec![0x00u8; LAZY_RETURN_WINDOW_MAX_OPS + 1];
    long.push(0xb1);
    assert_eq!(none(&long, &[]), None);
    let mut fits = vec![0x00u8; LAZY_RETURN_WINDOW_MAX_OPS];
    fits.push(0xb1);
    assert_eq!(none(&fits, &[]), Some(LAZY_RETURN_WINDOW_MAX_OPS));
    // Running off the code.
    assert_eq!(none(&[0x57, 0x2a], &[]), None);
    assert_eq!(none(&[0x11, 0x00], &[]), None);
}


/// Production text of `file` (relative to `jit/src`), cut at its first
/// `#[cfg(test)]`.
fn production(file: &str) -> String {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join(file);
    if path.exists() {
        let text = std::fs::read_to_string(&path).expect("source is UTF-8");
        return text.split("#[cfg(test)]").next().unwrap_or("").to_string();
    }
    let dir = path.with_extension("");
    let mut out = String::new();
    for entry in std::fs::read_dir(&dir).expect("source dir").flatten() {
        let p = entry.path();
        if p.extension().is_some_and(|e| e == "rs")
            && p.file_name().is_some_and(|n| {
                let s = n.to_string_lossy();
                s != "tests.rs" && !s.ends_with("_tests.rs")
            })
        {
            let text = std::fs::read_to_string(&p).expect("source is UTF-8");
            out.push_str(text.split("#[cfg(test)]").next().unwrap_or(""));
            out.push('\n');
        }
    }
    out
}

/// The single-pass walk's family of `op`, read from the `let family =
/// match op {` table of `x64/bytecode_walk.rs`.
fn walk_family_of(op: u8) -> Option<String> {
    let text = production("x64/bytecode_walk.rs");
    let start = text
        .find("let family = match op {")
        .expect("the family table exists");
    let table = &text[start..];
    let end = table.find("\n            };").unwrap_or(table.len());
    let hex = |t: &str| u8::from_str_radix(t.trim().trim_start_matches("0x"), 16).ok();
    for line in table[..end].lines() {
        let Some((pats, fam)) = line.split_once("=> WalkFamily::") else {
            continue;
        };
        let fam = fam.trim().trim_end_matches(',').to_string();
        for pat in pats.split('|') {
            let pat = pat.trim();
            let hit = match pat.split_once("..=") {
                Some((a, b)) => {
                    matches!((hex(a), hex(b)), (Some(a), Some(b)) if (a..=b).contains(&op))
                }
                None => hex(pat) == Some(op),
            };
            if hit {
                return Some(fam);
            }
        }
    }
    None
}

/// TAIL7-19-1's obligation, pinned: no window bytecode is lowered by
/// anything that can call. Every row is walked by `walk_local_stack`
/// (which emits no call of any kind) or by `walk_arith` (whose only call
/// is the `frem`/`drem` helper, under the `0x72 | 0x73` arm). A new row,
/// a family remap or a new call in either walker fails here.
#[test]
fn every_window_bytecode_is_lowered_without_a_call() {
    let arith: &[u8] = &[
        0x60, 0x64, 0x68, 0x74, 0x78, 0x7a, 0x7c, 0x7e, 0x80, 0x82, 0x91, 0x92, 0x93,
    ];
    for &(op, _) in LAZY_RETURN_WINDOW_OPS {
        let want = if arith.contains(&op) {
            "Arith"
        } else {
            "LocalStack"
        };
        assert_eq!(
            walk_family_of(op).as_deref(),
            Some(want),
            "window op {op:#04x} is no longer walked by the reviewed family"
        );
    }
    let local = production("x64/op_local_stack.rs");
    for needle in [
        "emit_call",
        "self.helpers.",
        "emit_jmp_absolute",
        "0xFF, 0xD0",
        "0xE8]",
    ] {
        assert!(
            !local.contains(needle),
            "`walk_local_stack` grew `{needle}`: prove it cannot reach the VM or \
             drop its rows from LAZY_RETURN_WINDOW_OPS"
        );
    }
    let arith_text = production("x64/op_arith.rs");
    assert_eq!(
        arith_text.matches("emit_call").count(),
        1,
        "`walk_arith` grew a call: prove it cannot reach the VM or drop its rows \
         from LAZY_RETURN_WINDOW_OPS"
    );
    let frem_arm = arith_text
        .find("0x72 | 0x73 =>")
        .expect("the frem/drem arm exists");
    let call = arith_text.find("emit_call").unwrap();
    assert!(
        call > frem_arm,
        "the one call of walk_arith is the frem/drem helper"
    );
    assert!(
        !production("x64/arith.rs").contains("emit_call"),
        "the arithmetic emitters grew a call"
    );
}

/// The window is consulted only inside the one admission function the
/// four lazy sites ask (tail7's ratchet counts those).
#[test]
fn the_window_is_asked_only_by_the_admission_function() {
    let text = production("x64/op_invoke.rs");
    let needle = "lazy_return_window_end(code, pc + invoke_len, branch_targets)";
    assert_eq!(text.matches(needle).count(), 1);
    let admit = text
        .find("fn sp_lazy_return_republish_admitted(")
        .expect("the admission function exists");
    assert!(text.find(needle).unwrap() > admit);
}

/// The instance arm's twin site sits after the thin prefixes and BEFORE
/// anything of the ordinary call (the service copy is the first thing
/// that call emits), so its decline edge is that call, byte for byte.
#[test]
fn the_instance_twin_precedes_the_ordinary_call() {
    let text = production("x64/op_invoke.rs");
    let needle = "self.try_emit_sp_instance_tail_leaf_twin(";
    assert_eq!(text.matches(needle).count(), 1);
    let site = text.find(needle).unwrap();
    let prefix_end = text
        .find("// ===== INTRINSIC REGION END: SP_THIN_HELPER_PREFIX =====")
        .expect("the prefix region exists");
    let service = text[site..]
        .find("self.reserve_direct_call_service_slots(args_frame_top, &arg_slots)")
        .expect("the ordinary call's service copy follows");
    let next_call = text[site..]
        .find("self.emit_call_absolute(callee_entry);")
        .expect("the ordinary call follows");
    assert!(prefix_end < site && service < next_call);
}

fn test_compiler() -> Compiler {
    let alloc_result = crate::regalloc::RegAllocResult {
        assignments: Vec::new(),
        xmm_assignments: Vec::new(),
        used_callee_saved: Vec::new(),
        used_xmm_regs: Vec::new(),
        block_live_in: Vec::new(),
    };
    Compiler::new(
        "r19-tail8-test".to_string(),
        crate::ExecutableBuffer::new(4096).expect("test executable buffer"),
        0,
        0,
        8,
        false,
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        Vec::new(),
        alloc_result,
        false,
        cratonvm_jit_api::JitRuntimeHelpers::default(),
        0,
        false,
        false,
        false,
        false,
        false,
        Vec::new(),
    )
}

fn decline_bytes(decline: i64) -> Vec<u8> {
    let mut c = test_compiler();
    let start = c.buf.pos();
    c.emit_cmp_rax_leaf_twin_decline(decline);
    c.buf.as_slice()[start..c.buf.pos()].to_vec()
}

#[test]
fn the_decline_compare_takes_the_narrowest_exact_encoding() {
    assert_eq!(decline_bytes(-1), vec![0x48, 0x83, 0xF8, 0xFF]);
    assert_eq!(
        decline_bytes(0x1234),
        vec![0x48, 0x81, 0xF8, 0x34, 0x12, 0x00, 0x00]
    );
    let wide = decline_bytes(crate::direct_helpers::LEAF_TWIN_DECLINE_OUTSIDE_INT);
    assert_eq!(&wide[wide.len() - 3..], &[0x4C, 0x39, 0xD8], "CMP RAX, R11");
    let imm = crate::direct_helpers::LEAF_TWIN_DECLINE_OUTSIDE_INT.to_le_bytes();
    assert!(
        wide.windows(8).any(|w| w == imm),
        "the full 64-bit decline is materialized"
    );
    // `0` keeps the wave-7 `TEST RAX, RAX`.
    assert_eq!(decline_bytes(0).len(), 3);
}

#[test]
fn the_tail_shape_test_matches_the_static_arms_table() {
    for (ret, op) in [
        (b'I', 0xac),
        (b'Z', 0xac),
        (b'J', 0xad),
        (b'F', 0xae),
        (b'D', 0xaf),
        (b'L', 0xb0),
        (b'[', 0xb0),
        (b'V', 0xb1),
    ] {
        assert!(tail_return_op_matches(ret, op));
    }
    assert!(!tail_return_op_matches(b'Z', 0xb0));
    assert!(!tail_return_op_matches(b'L', 0xac));
    assert!(!tail_return_op_matches(b'V', 0xac));
    assert!(!tail_return_op_matches(b'I', 0x57));
}
