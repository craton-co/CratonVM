    use super::{
        typecheck_array_hit_bytes, typecheck_receiver_verdict_bytes, TypecheckArrayPlan,
        VhSubtypeDisplay, CALL_ARG_REGS,
    };

    const OBJ: u32 = 1;
    const BASE: u32 = 0x1001;
    const SUB: u32 = 0x1002;
    const OTHER: u32 = 0x1004;
    const UNPUB: u32 = 0x1005;
    const STAMP: u32 = 0x1006;
    const ARRCLS: u32 = 0x1010;
    const IF_A: u32 = 0x3001;
    const DECLINED: i64 = -1;

    fn mov(dst: u8, src: u8) -> [u8; 3] {
        [
            0x48 | (u8::from(src >= 8) << 2) | u8::from(dst >= 8),
            0x89,
            0xC0 | ((src & 7) << 3) | (dst & 7),
        ]
    }

    #[allow(clippy::type_complexity)]
    fn fixture() -> (Box<u64>, Vec<Box<[u32]>>, Box<[u64]>) {
        // (chain, interfaces, verdict)
        let rows: [(&[u32], &[u32], u32); 5] = [
            (&[OBJ], &[], 1),
            (&[OBJ, BASE], &[], 1),
            (&[OBJ, BASE, SUB], &[IF_A], 1),
            (&[OBJ, OTHER], &[], 1),
            (&[OBJ, STAMP], &[IF_A], 0),
        ];
        let displays: Vec<Box<[u32]>> = rows
            .iter()
            .map(|(c, i, v)| {
                let mut d = vec![u32::try_from(c.len()).expect("small")];
                d.extend_from_slice(c);
                d.push(u32::try_from(i.len()).expect("small"));
                d.extend_from_slice(i);
                d.push(*v);
                d.into_boxed_slice()
            })
            .collect();
        let cap = 0x5000u64;
        let mut table = vec![0u64; usize::try_from(cap).expect("small") + 1];
        table[0] = cap;
        for ((c, _, _), d) in rows.iter().zip(&displays) {
            let id = *c.last().expect("non-empty");
            table[1 + usize::try_from(id).expect("small")] = d.as_ptr() as u64; // Cast: address
        }
        let table = table.into_boxed_slice();
        let cell = Box::new(table.as_ptr() as u64); // Cast: address
        (cell, displays, table)
    }

    /// `extern "C" fn(u64) -> 1 | DECLINED`: `prologue`, the arm's bytes
    /// (falling through, or jumping through `hits`, to the `1`), then the
    /// decline target.
    fn build(prologue: &[u8], bytes: &[u8], decline: &[usize], hits: &[usize]) -> crate::ExecutableBuffer {
        let mut body: Vec<u8> = prologue.to_vec();
        let at = body.len();
        body.extend_from_slice(bytes);
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
        for &d in decline {
            patch(d, decline_at);
        }
        for &h in hits {
            patch(h, hit_at);
        }
        let mut buf = crate::ExecutableBuffer::new(1024).expect("executable buffer");
        buf.emit(&body);
        assert!(!buf.overflowed());
        buf.finalize();
        buf
    }

    fn call(buf: &crate::ExecutableBuffer, arg: u64) -> i64 {
        // SAFETY: a complete `extern "C" fn(u64) -> i64` reading only the
        // fixtures the caller keeps alive and writing RAX, RCX, R11.
        let f =
            unsafe { std::mem::transmute::<*const u8, extern "C" fn(u64) -> i64>(buf.as_ptr()) };
        f(arg)
    }

    #[test]
    fn r21_final_miss_verdict_admits_only_hierarchy_only_receivers() {
        let (cell, displays, table) = fixture();
        let cell_addr = &*cell as *const u64 as usize; // Cast: the cell's address
        let (bytes, decline) = typecheck_receiver_verdict_bytes(cell_addr);
        let mut prologue = mov(1, CALL_ARG_REGS[0]).to_vec(); // RCX = arg0
        prologue.extend_from_slice(&[0x89, 0xC9]); // MOV ECX, ECX
        let buf = build(&prologue, &bytes, &decline, &[]);
        for id in [OBJ, BASE, SUB, OTHER] {
            assert_eq!(call(&buf, u64::from(id)), 1, "{id:#x} is hierarchy-only");
        }
        for id in [STAMP, UNPUB, 0x4FFF, 0x8000_0001, u32::MAX] {
            assert_eq!(call(&buf, u64::from(id)), DECLINED, "{id:#x} asks the helper");
        }
        drop((cell, displays, table));
    }

/// A fake header: class id at offset 0, the kind byte at its offset.
    fn header(class_id: u32, kind_tags: u8) -> Box<[u64; 4]> {
        let mut h = Box::new([0u64; 4]);
        let p = h.as_mut_ptr() as *mut u8;
        // SAFETY: both writes land inside the 32-byte box.
        unsafe {
            std::ptr::copy_nonoverlapping(class_id.to_le_bytes().as_ptr(), p, 4);
            *p.add(cratonvm_types::KIND_TAGS_BYTE_OFFSET) = kind_tags;
        }
        h
    }


