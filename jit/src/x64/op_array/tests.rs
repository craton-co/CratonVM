    //! Round 9 wave 8 (arr8): the `bastore` value proof that lets the `0x54`
    //! arm skip the `boolean[]` mask.

    use super::bastore_value_is_zero_or_one;

    /// Every pc of `code` starts an instruction except those in `operand_bytes`.
    fn starts(len: usize, operand_bytes: &[usize]) -> Vec<bool> {
        (0..len).map(|p| !operand_bytes.contains(&p)).collect()
    }

    #[test]
    fn a_constant_zero_or_one_right_before_the_store_is_proved() {
        // aload_0; iload_1; iconst_1; bastore
        let code = [0x2a, 0x1b, 0x04, 0x54];
        let st = starts(code.len(), &[]);
        let no_targets = vec![false; code.len()];
        assert!(bastore_value_is_zero_or_one(&code, 3, &no_targets, &st));
        // iconst_0
        let code0 = [0x2a, 0x1b, 0x03, 0x54];
        assert!(bastore_value_is_zero_or_one(&code0, 3, &no_targets, &st));
        // aload_0; iload_1; bipush 1; bastore
        let code_b = [0x2a, 0x1b, 0x10, 0x01, 0x54];
        let st_b = starts(code_b.len(), &[3]);
        let nt_b = vec![false; code_b.len()];
        assert!(bastore_value_is_zero_or_one(&code_b, 4, &nt_b, &st_b));
    }

    #[test]
    fn anything_else_keeps_the_runtime_mask() {
        let no_targets = vec![false; 8];
        // iconst_2 -- the case the page is about.
        let two = [0x2a, 0x1b, 0x05, 0x54];
        assert!(!bastore_value_is_zero_or_one(
            &two,
            3,
            &no_targets,
            &starts(4, &[])
        ));
        // iload_2 -- any computed value.
        let var = [0x2a, 0x1b, 0x1c, 0x54];
        assert!(!bastore_value_is_zero_or_one(
            &var,
            3,
            &no_targets,
            &starts(4, &[])
        ));
        // bipush 2
        let b2 = [0x2a, 0x1b, 0x10, 0x02, 0x54];
        assert!(!bastore_value_is_zero_or_one(
            &b2,
            4,
            &no_targets,
            &starts(5, &[3])
        ));
        // A merge point: another path may reach the store with its own value.
        let one = [0x2a, 0x1b, 0x04, 0x54];
        let mut target = vec![false; 4];
        target[3] = true;
        assert!(!bastore_value_is_zero_or_one(
            &one,
            3,
            &target,
            &starts(4, &[])
        ));
        // An operand byte that merely equals `iconst_1` is not an instruction:
        // `sipush 0x0004` ends in 0x04.
        let sip = [0x2a, 0x1b, 0x11, 0x00, 0x04, 0x54];
        assert!(!bastore_value_is_zero_or_one(
            &sip,
            5,
            &no_targets,
            &starts(6, &[3, 4])
        ));
        // Out-of-range maps answer "not proved".
        assert!(!bastore_value_is_zero_or_one(&one, 3, &[], &starts(4, &[])));
        assert!(!bastore_value_is_zero_or_one(
            &one,
            0,
            &no_targets,
            &starts(4, &[])
        ));
    }

    /// JIT r21 wave 2 (lane typecheck, TYPECHECK21-5): default ON since the r22 A/B.
/// JIT r21 wave 2 (lane typecheck, TYPECHECK21-5): the single-pass
    /// prefix the arm is emitted with -- `MOV EDX, R10D` (value id) and
    /// `MOV ECX, [R11 + disp32]` (the array's component id) -- in front of
    /// `aastore_display_arm_bytes`, EXECUTED as `fn(value_id, array) -> 1 |
    /// -1` over a fake display table and a fake array header.
    #[cfg(target_arch = "x86_64")]
    #[test]
    fn r21_sp_aastore_display_prefix_feeds_the_arm() {
        #[cfg(target_os = "windows")]
        const A: [u8; 2] = [1, 2]; // RCX, RDX
        #[cfg(not(target_os = "windows"))]
        const A: [u8; 2] = [7, 6]; // RDI, RSI
        const BASE: u32 = 0x101;
        const SUB: u32 = 0x102;
        const IFACE: u32 = 0x301;
        let mov = |dst: u8, src: u8| {
            [
                0x48 | (u8::from(src >= 8) << 2) | u8::from(dst >= 8),
                0x89,
                0xC0 | ((src & 7) << 3) | (dst & 7),
            ]
        };
        let rows: [(&[u32], &[u32]); 3] = [(&[1], &[]), (&[1, BASE], &[]), (&[1, BASE, SUB], &[IFACE])];
        let displays: Vec<Box<[u32]>> = rows
            .iter()
            .map(|(c, i)| {
                let mut d = vec![u32::try_from(c.len()).expect("small")];
                d.extend_from_slice(c);
                d.push(u32::try_from(i.len()).expect("small"));
                d.extend_from_slice(i);
                d.push(1);
                d.into_boxed_slice()
            })
            .collect();
        let mut table = vec![0u64; 0x401];
        table[0] = 0x400;
        for ((c, _), d) in rows.iter().zip(&displays) {
            let id = *c.last().expect("non-empty");
            table[1 + usize::try_from(id).expect("small")] = d.as_ptr() as u64; // Cast: address
        }
        let table = table.into_boxed_slice();
        let cell = Box::new(table.as_ptr() as u64); // Cast: address
        let cell_addr = &*cell as *const u64 as usize; // Cast: the cell's address
        let (bytes, decline, hits) = crate::ir_lower::aastore_display_arm_bytes(cell_addr);
        let mut body: Vec<u8> = Vec::new();
        body.extend_from_slice(&mov(10, A[0])); // R10 = value id
        body.extend_from_slice(&mov(11, A[1])); // R11 = array
        body.extend_from_slice(&[0x44, 0x89, 0xD2]); // MOV EDX, R10D
        body.extend_from_slice(&[0x41, 0x8B, 0x8B, 0, 0, 0, 0]); // MOV ECX, [R11 + 0]
        let at = body.len();
        body.extend_from_slice(&bytes);
        let hit_at = body.len();
        body.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00, 0xC3]); // MOV EAX, 1 ; RET
        let decline_at = body.len();
        body.extend_from_slice(&[0x48, 0xC7, 0xC0, 0xFF, 0xFF, 0xFF, 0xFF, 0xC3]);
        let mut patch = |field: usize, to: usize| {
            let operand = at + field;
            let rel =
                i32::try_from(to).expect("small") - i32::try_from(operand + 4).expect("small");
            body[operand..operand + 4].copy_from_slice(&rel.to_le_bytes());
        };
        for d in decline {
            patch(d, decline_at);
        }
        for h in hits {
            patch(h, hit_at);
        }
        let mut buf = crate::ExecutableBuffer::new(1024).expect("executable buffer");
        buf.emit(&body);
        assert!(!buf.overflowed());
        buf.finalize();
        // SAFETY: a complete `extern "C" fn(u64, u64) -> i64` reading only the
        // fixtures kept alive below and writing caller-saved registers.
        let f = unsafe {
            std::mem::transmute::<*const u8, extern "C" fn(u64, u64) -> i64>(buf.as_ptr())
        };
        let array = |component: u32| Box::new([u64::from(component), 0, 0, 0]);
        let (base_arr, iface_arr, sub_arr) = (array(BASE), array(IFACE), array(SUB));
        let ask = |v: u32, a: &[u64; 4]| f(u64::from(v), a.as_ptr() as u64);
        assert_eq!(ask(SUB, &base_arr), 1, "Sub into Base[]");
        assert_eq!(ask(SUB, &iface_arr), 1, "Sub into IFACE[]");
        assert_eq!(ask(BASE, &sub_arr), -1, "Base into Sub[] declines");
        assert_eq!(ask(BASE, &iface_arr), -1, "Base into IFACE[] declines");
        drop((cell, displays, table));
    }
