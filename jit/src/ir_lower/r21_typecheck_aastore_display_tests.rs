use super::{aastore_display_arm_bytes, CALL_ARG_REGS};

const OBJ: u32 = 1;
const BASE: u32 = 0x1001;
const SUB: u32 = 0x1002;
const SUBSUB: u32 = 0x1003;
const OTHER: u32 = 0x1004;
const UNPUB: u32 = 0x1005;
const IF_A: u32 = 0x3001;
const IF_B: u32 = 0x3002;
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
    let rows: [(&[u32], &[u32]); 5] = [
        (&[OBJ], &[]),
        (&[OBJ, BASE], &[]),
        (&[OBJ, BASE, SUB], &[IF_A]),
        (&[OBJ, BASE, SUB, SUBSUB], &[IF_A, IF_B]),
        (&[OBJ, OTHER], &[IF_B]),
    ];
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
    let cap = 0x5000u64;
    let mut table = vec![0u64; usize::try_from(cap).expect("small") + 1];
    table[0] = cap;
    for ((c, _), d) in rows.iter().zip(&displays) {
        let id = *c.last().expect("non-empty");
        table[1 + usize::try_from(id).expect("small")] = d.as_ptr() as u64; // Cast: address
    }
    let table = table.into_boxed_slice();
    let cell = Box::new(table.as_ptr() as u64); // Cast: address
    (cell, displays, table)
}

#[test]
fn r21_aastore_display_arm_proves_subclasses_and_implementors_only() {
    let (cell, displays, table) = fixture();
    let cell_addr = &*cell as *const u64 as usize; // Cast: the cell's address
    let (bytes, decline, hits) = aastore_display_arm_bytes(cell_addr);
    let mut body: Vec<u8> = Vec::new();
    // RAX = arg0 (value id), RCX = arg1 (component id), RDX = RAX: an
    // order that reads both argument registers before writing them on
    // both ABIs.
    body.extend_from_slice(&mov(0, CALL_ARG_REGS[0]));
    body.extend_from_slice(&mov(1, CALL_ARG_REGS[1]));
    body.extend_from_slice(&mov(2, 0));
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
    assert!(!decline.is_empty() && hits.len() == 2);
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
    // SAFETY: a complete `extern "C" fn(u64, u64) -> i64` that reads only
    // the fake tables this test keeps alive and writes RAX, RCX, RDX, R10
    // (caller-saved on both ABIs).
    let f = unsafe {
        std::mem::transmute::<*const u8, extern "C" fn(u64, u64) -> i64>(buf.as_ptr())
    };
    let ask = |value: u32, component: u32| f(u64::from(value), u64::from(component));
    for (v, c) in [
        (BASE, BASE),
        (SUB, BASE),
        (SUBSUB, BASE),
        (SUBSUB, SUB),
        (SUB, OBJ),
        (OTHER, OBJ),
        (SUB, IF_A),
        (SUBSUB, IF_A),
        (SUBSUB, IF_B),
        (OTHER, IF_B),
    ] {
        assert_eq!(ask(v, c), 1, "{v:#x} stores into {c:#x}[]");
    }
    for (v, c) in [
        (BASE, SUB),
        (SUB, SUBSUB),
        (OTHER, BASE),
        (BASE, OTHER),
        (SUB, IF_B),
        (BASE, IF_A),
        (OTHER, IF_A),
        (UNPUB, BASE),
        (UNPUB, IF_A),
        (SUB, UNPUB),
        (SUB, 0x4FFF_0000),
        (0x8000_0001, BASE),
        (u32::MAX, IF_A),
    ] {
        assert_eq!(ask(v, c), DECLINED, "{v:#x} into {c:#x}[] declines");
    }
    drop((cell, displays, table));
}

