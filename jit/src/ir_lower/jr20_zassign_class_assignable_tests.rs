use super::{
    class_assignable_answer_bytes, class_instance_subtype_answer_bytes, mirror_hint_slot,
    mirror_hint_word, CALL_ARG_REGS, GETCLASS_WORDS_CLASS_CAPACITY,
    GETCLASS_WORDS_MIRROR_HINT_BASE, GETCLASS_WORDS_TOTAL,
};

const OBJ: u32 = 1;
const CLASS: u32 = 0x1234;
const SUB: u32 = 0x2001;
const PROXYISH: u32 = 0x2003;
const IFACE_A: u32 = 0x3001;
const SYN: u32 = 0x3002;
const OTHER: u32 = 0x4321;
const DECLINED: i64 = -1;

/// `MOV dst, src` (64-bit).
fn mov(dst: u8, src: u8) -> [u8; 3] {
    [
        0x48 | (u8::from(src >= 8) << 2) | u8::from(dst >= 8),
        0x89,
        0xC0 | ((src & 7) << 3) | (dst & 7),
    ]
}

fn mirror(cid: u32) -> u64 {
    0x7100_0000 + u64::from(cid) * 0x148
}

struct World {
    table: Box<[usize]>,
    _displays: Vec<Box<[u32]>>,
    _display_table: Box<[u64]>,
    cell: Box<u64>,
    latch: Box<std::sync::atomic::AtomicU8>,
}

impl World {
    fn new() -> Self {
        let mut table = vec![0usize; GETCLASS_WORDS_TOTAL];
        table[0] = GETCLASS_WORDS_CLASS_CAPACITY;
        // (class, miss_ok)
        let classes = [
            (OBJ, true),
            (CLASS, true),
            (SUB, true),
            (PROXYISH, true),
            (IFACE_A, true),
            (SYN, false),
            (OTHER, true),
        ];
        let mut slots = std::collections::HashSet::new();
        for (cid, miss_ok) in classes {
            let m = usize::try_from(mirror(cid)).expect("64-bit");
            table[1 + usize::try_from(cid).expect("small")] = m;
            assert!(
                slots.insert(mirror_hint_slot(m)),
                "the test's mirrors must not collide"
            );
            table[GETCLASS_WORDS_MIRROR_HINT_BASE + mirror_hint_slot(m)] =
                usize::try_from(mirror_hint_word(cid, miss_ok)).expect("64-bit");
        }
        // (chain, interfaces, verdict); interfaces have no display.
        let rows: [(&[u32], &[u32], u32); 5] = [
            (&[OBJ], &[], 1),
            (&[OBJ, CLASS], &[], 1),
            (&[OBJ, CLASS, SUB], &[IFACE_A], 1),
            (&[OBJ, OTHER], &[], 1),
            (&[OBJ, PROXYISH], &[IFACE_A], 0),
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
        let mut dt = vec![0u64; usize::try_from(cap).expect("small") + 1];
        dt[0] = cap;
        for ((c, _, _), d) in rows.iter().zip(&displays) {
            let id = *c.last().expect("non-empty");
            dt[1 + usize::try_from(id).expect("small")] = d.as_ptr() as u64;
            // Cast: address
        }
        let dt = dt.into_boxed_slice();
        let cell = Box::new(dt.as_ptr() as u64); // Cast: address
        Self {
            table: table.into_boxed_slice(),
            _displays: displays,
            _display_table: dt,
            cell,
            latch: Box::new(std::sync::atomic::AtomicU8::new(0)),
        }
    }

    fn addrs(&self) -> (usize, usize, usize) {
        (
            self.table.as_ptr() as usize,       // Cast: address
            &*self.cell as *const u64 as usize, // Cast: address
            self.latch.as_ptr() as usize,       // Cast: address
        )
    }
}

/// `prologue` + the arm + `1` / `0` / `DECLINED` exits, as an
/// `extern "C" fn(u64, u64) -> i64`.
fn build(
    prologue: &[u8],
    arm: (Vec<u8>, Vec<usize>, Vec<usize>, usize),
) -> crate::ExecutableBuffer {
    let (bytes, decline, hits, miss) = arm;
    let mut body: Vec<u8> = prologue.to_vec();
    let at = body.len();
    body.extend_from_slice(&bytes);
    let hit_at = body.len();
    body.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00, 0xC3]); // MOV EAX, 1 ; RET
    let miss_at = body.len();
    body.extend_from_slice(&[0x31, 0xC0, 0xC3]); // XOR EAX, EAX ; RET
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
    patch(miss, miss_at);
    let mut buf = crate::ExecutableBuffer::new(2048).expect("executable buffer");
    buf.emit(&body);
    assert!(!buf.overflowed());
    buf.finalize();
    buf
}

fn call(buf: &crate::ExecutableBuffer, a: u64, b: u64) -> i64 {
    // SAFETY: a complete `extern "C" fn(u64, u64) -> i64` that reads only
    // the fake tables the test keeps alive and writes RAX, RCX, RDX, R10,
    // R11 (caller-saved on both ABIs).
    let f = unsafe {
        std::mem::transmute::<*const u8, extern "C" fn(u64, u64) -> i64>(buf.as_ptr())
    };
    f(a, b)
}

/// `t.isAssignableFrom(s)` as `f(t mirror, s mirror)`: hits, screened
/// misses, and every decline (null, no hint, a stale hint, an interface or
/// verdict-0 source, a synthetic target, the latch).
#[test]
fn the_assignable_arm_answers_hierarchy_pairs_and_declines_the_rest() {
    let mut w = World::new();
    let (table, cell, latch) = w.addrs();
    // RAX = arg0, then RDX = arg1 (on Windows arg1 IS RDX).
    let mut prologue = Vec::new();
    prologue.extend_from_slice(&mov(0, CALL_ARG_REGS[0]));
    prologue.extend_from_slice(&mov(2, CALL_ARG_REGS[1]));
    let buf = build(&prologue, class_assignable_answer_bytes(table, cell, latch));
    let ask = |t: u32, s: u32| call(&buf, mirror(t), mirror(s));
    for (t, s) in [
        (CLASS, SUB),
        (OBJ, SUB),
        (IFACE_A, SUB),
        (OBJ, OTHER),
        (CLASS, CLASS),
        (IFACE_A, IFACE_A),
        (OBJ, PROXYISH),
        (IFACE_A, PROXYISH),
    ] {
        assert_eq!(ask(t, s), 1, "{t:#x} <- {s:#x}");
    }
    for (t, s) in [(OTHER, SUB), (SUB, CLASS), (IFACE_A, OTHER), (CLASS, OBJ)] {
        assert_eq!(ask(t, s), 0, "{t:#x} <- {s:#x}");
    }
    for (t, s) in [
        (SYN, SUB),        // a synthetic target's miss
        (CLASS, PROXYISH), // a verdict-0 source's miss
        (OBJ, IFACE_A),    // an interface source (no display)
    ] {
        assert_eq!(ask(t, s), DECLINED, "{t:#x} <- {s:#x}");
    }
    assert_eq!(call(&buf, 0, mirror(SUB)), DECLINED, "null receiver");
    assert_eq!(
        call(&buf, mirror(CLASS), 0),
        DECLINED,
        "null argument: the NPE"
    );
    assert_eq!(call(&buf, mirror(CLASS), 0x7F00_0008), DECLINED, "no hint");
    // A hint whose class word is not the mirror (a moved or re-used
    // address) declines.
    let stale = usize::try_from(mirror(OTHER)).expect("64-bit");
    w.table[1 + usize::try_from(OTHER).expect("small")] = stale + 0x1000;
    assert_eq!(
        call(&buf, mirror(OBJ), mirror(OTHER)),
        DECLINED,
        "stale hint"
    );
    w.table[1 + usize::try_from(OTHER).expect("small")] = stale;
    assert_eq!(call(&buf, mirror(OBJ), mirror(OTHER)), 1);
    // The latch turns every miss into a decline; hits stand.
    w.latch.store(1, std::sync::atomic::Ordering::Relaxed);
    assert_eq!(ask(CLASS, SUB), 1);
    assert_eq!(ask(OTHER, SUB), DECLINED);
    drop(buf);
    drop(w);
}

/// `t.isInstance(obj)` past the exact compare, as `f(s class id, t
/// mirror)`.
#[test]
fn the_instance_subtype_arm_answers_from_the_object_class() {
    let w = World::new();
    let (table, cell, latch) = w.addrs();
    // RCX = arg0, then RDX = arg1 (no-ops on Windows).
    let mut prologue = Vec::new();
    prologue.extend_from_slice(&mov(1, CALL_ARG_REGS[0]));
    prologue.extend_from_slice(&mov(2, CALL_ARG_REGS[1]));
    let buf = build(
        &prologue,
        class_instance_subtype_answer_bytes(table, cell, latch),
    );
    let ask = |s: u32, t: u32| call(&buf, u64::from(s), mirror(t));
    assert_eq!(ask(SUB, CLASS), 1);
    assert_eq!(ask(SUB, IFACE_A), 1);
    assert_eq!(ask(SUB, SUB), 1);
    assert_eq!(ask(OTHER, SUB), 0);
    assert_eq!(ask(CLASS, IFACE_A), 0);
    assert_eq!(ask(SUB, SYN), DECLINED);
    assert_eq!(ask(PROXYISH, CLASS), DECLINED);
    assert_eq!(
        ask(0x8000_0001, OBJ),
        DECLINED,
        "a lambda proxy has no display"
    );
    assert_eq!(call(&buf, u64::from(SUB), 0), DECLINED, "null receiver");
    drop(buf);
    drop(w);
}
