use super::*;

const FELL: i64 = -1;
const OBJECT: u32 = 1;
const BASE: u32 = 0x10;
const A1: u32 = 0x11;
const A2: u32 = 0x12;
const B1: u32 = 0x13;
const OTHER: u32 = 0x14;
const PROXYISH: u32 = 0x15;
const DEEP: u32 = 0x16;
const UNPUBLISHED: u32 = 0x17;
const MANY: u32 = 0x18;
const SHAPE: u32 = 0x40;
const NAMED: u32 = 0x41;
const CLASHING: u32 = 0x46;

struct Table(Vec<u64>, usize);

impl Table {
    fn new() -> Self {
        let words = ROWS_BYTES / 8 + ROW_BYTES / 8;
        let v = vec![0u64; words];
        let base = v.as_ptr() as usize; // Cast: the buffer's address
        let aligned = (base + ROW_BYTES - 1) & !(ROW_BYTES - 1);
        Table(v, aligned)
    }

    fn fill(&mut self, id: u32, chain: &[u32], ifaces: &[u32], hierarchy_only: bool) {
        let words = row_words(chain, ifaces, hierarchy_only);
        let off = self.1 - self.0.as_ptr() as usize + (id as usize) * ROW_BYTES; // Cast: addresses
        assert!(off + ROW_BYTES <= self.0.len() * std::mem::size_of::<u64>());
        // SAFETY: the aligned row and all its words fit in the owned buffer.
        let p = unsafe { (self.0.as_mut_ptr() as *mut u8).add(off) as *mut u32 };
        for (i, w) in words.iter().enumerate() {
            // SAFETY: the checked row contains every word produced by row_words.
            unsafe { p.add(i).write(*w) };
        }
    }
}

fn build(
    rows: usize,
    target: u32,
    kind: RowTarget,
    latch: Option<usize>,
) -> crate::ExecutableBuffer {
    let arm = typecheck_row_bytes(rows, target, kind, latch).expect("an arm");
    let mut body: Vec<u8> = Vec::new();
    #[cfg(target_os = "windows")]
    body.extend_from_slice(&[0x48, 0x89, 0xC8]); // MOV RAX, RCX
    #[cfg(not(target_os = "windows"))]
    body.extend_from_slice(&[0x48, 0x89, 0xF8]); // MOV RAX, RDI
    let at = body.len();
    body.extend_from_slice(&arm.bytes);
    body.extend_from_slice(&[0x48, 0xC7, 0xC0, 0xFF, 0xFF, 0xFF, 0xFF, 0xC3]); // MOV RAX, -1 ; RET
    let hit_at = body.len();
    body.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00, 0xC3]); // MOV EAX, 1 ; RET
    let miss_at = body.len();
    body.extend_from_slice(&[0x31, 0xC0, 0xC3]); // XOR EAX, EAX ; RET
    let mut patch = |field: usize, to: usize| {
        let operand = at + field;
        let rel =
            i32::try_from(to).expect("small") - i32::try_from(operand + 4).expect("small");
        body[operand..operand + 4].copy_from_slice(&rel.to_le_bytes());
    };
    for h in &arm.hit {
        patch(*h, hit_at);
    }
    for m in &arm.miss {
        patch(*m, miss_at);
    }
    let mut buf = crate::ExecutableBuffer::new(1024).expect("executable buffer");
    buf.emit(&body);
    assert!(!buf.overflowed());
    buf.finalize();
    buf
}

fn ask(buf: &crate::ExecutableBuffer, id: u32) -> i64 {
    let obj = Box::new(u64::from(id));
    // SAFETY: build finalized this buffer as executable code with the matching C ABI.
    let f =
        unsafe { std::mem::transmute::<*const u8, extern "C" fn(*const u64) -> i64>(buf.as_ptr()) };
    f(&*obj)
}

fn table() -> Table {
    let mut t = Table::new();
    t.fill(BASE, &[OBJECT, BASE], &[], true);
    t.fill(A1, &[OBJECT, BASE, A1], &[SHAPE], true);
    t.fill(A2, &[OBJECT, BASE, A1, A2], &[SHAPE, NAMED], true);
    t.fill(B1, &[OBJECT, BASE, B1], &[SHAPE, CLASHING], true);
    t.fill(OTHER, &[OBJECT, OTHER], &[], true);
    t.fill(PROXYISH, &[OBJECT, PROXYISH], &[SHAPE], false);
    let deep: Vec<u32> = (0..12)
        .map(|i| if i == 0 { OBJECT } else { 0x100 + i })
        .chain([DEEP])
        .collect();
    t.fill(DEEP, &deep, &[], true);
    t.fill(MANY, &[OBJECT, MANY], &[SHAPE, CLASHING], true);
    t
}

#[test]
fn a_class_row_arm_answers_hits_and_hierarchy_only_misses() {
    let t = table();
    let latch = Box::new(std::sync::atomic::AtomicU8::new(0));
    let latch_addr = latch.as_ptr() as usize; // Cast: the latch's address
    let base_arm = build(t.1, BASE, RowTarget::Class { depth: 1 }, Some(latch_addr));
    for id in [BASE, A1, A2, B1] {
        assert_eq!(ask(&base_arm, id), 1, "{id:#x} is a BASE");
    }
    for id in [OTHER, DEEP, MANY] {
        assert_eq!(ask(&base_arm, id), 0, "{id:#x} is not a BASE");
    }
    for id in [PROXYISH, UNPUBLISHED, ROWS_CAP, 0x8000_0001, u32::MAX] {
        assert_eq!(ask(&base_arm, id), FELL, "{id:#x} falls through");
    }
    let a1_arm = build(t.1, A1, RowTarget::Class { depth: 2 }, Some(latch_addr));
    assert_eq!(ask(&a1_arm, A1), 1);
    assert_eq!(ask(&a1_arm, A2), 1);
    assert_eq!(ask(&a1_arm, B1), 0);
    assert_eq!(ask(&a1_arm, BASE), 0, "shallower than the target");
    let d7 = build(t.1, 0x107, RowTarget::Class { depth: 7 }, Some(latch_addr));
    assert_eq!(ask(&d7, DEEP), 1);
    assert_eq!(ask(&d7, A2), 0);
    let hit_only = build(t.1, BASE, RowTarget::Class { depth: 1 }, None);
    assert_eq!(ask(&hit_only, A2), 1);
    assert_eq!(ask(&hit_only, OTHER), FELL);
    latch.store(1, std::sync::atomic::Ordering::Relaxed);
    assert_eq!(ask(&base_arm, A1), 1);
    assert_eq!(ask(&base_arm, OTHER), FELL);
    drop(latch);
}

#[test]
fn an_interface_row_arm_answers_placed_hits_and_bloom_misses() {
    let t = table();
    let latch = Box::new(std::sync::atomic::AtomicU8::new(0));
    let latch_addr = latch.as_ptr() as usize; // Cast: the latch's address
    let shape = build(t.1, SHAPE, RowTarget::Interface, Some(latch_addr));
    for id in [A1, A2, B1, MANY] {
        assert_eq!(ask(&shape, id), 1, "{id:#x} implements SHAPE");
    }
    for id in [BASE, OTHER, DEEP] {
        assert_eq!(ask(&shape, id), 0, "{id:#x} does not implement SHAPE");
    }
    assert_eq!(ask(&shape, PROXYISH), 1);
    let named = build(t.1, NAMED, RowTarget::Interface, Some(latch_addr));
    assert_eq!(ask(&named, A2), 1);
    assert_eq!(ask(&named, A1), 0);
    assert_eq!(ask(&named, PROXYISH), FELL, "verdict 0 never misses");
    let clashing = build(t.1, CLASHING, RowTarget::Interface, Some(latch_addr));
    assert_eq!(ask(&clashing, MANY), FELL);
    assert_eq!(ask(&clashing, B1), FELL);
    assert_eq!(ask(&clashing, A1), 0);
    for id in [UNPUBLISHED, 0x8000_0001] {
        assert_eq!(ask(&clashing, id), FELL);
    }
    drop(latch);
}

#[test]
fn row_words_lay_out_chain_bloom_and_home_slots() {
    let w = row_words(&[OBJECT, BASE, A1], &[SHAPE, NAMED, CLASHING], true);
    assert_eq!(w[0] & STATUS_MASK, STATUS_HIERARCHY_ONLY);
    assert_eq!(w[0] >> STATUS_DEPTH_SHIFT, 2, "A1's own depth");
    assert_eq!(&w[1..=7], &[BASE, A1, 0, 0, 0, 0, 0]);
    let bloom = u64::from(w[BLOOM_WORD]) | (u64::from(w[BLOOM_WORD + 1]) << 32);
    assert_eq!(
        bloom,
        (1 << bloom_bit(SHAPE)) | (1 << bloom_bit(NAMED)) | (1 << bloom_bit(CLASHING))
    );
    assert_eq!(w[iface_slot_word(SHAPE)], SHAPE);
    assert_eq!(w[iface_slot_word(NAMED)], NAMED);
    assert_eq!(iface_slot_word(SHAPE), iface_slot_word(CLASHING));
    assert_eq!(row_words(&[OBJECT], &[], false)[0], STATUS_PUBLISHED);
    assert!(typecheck_row_bytes(0x1000, BASE, RowTarget::Class { depth: 0 }, None).is_none());
    assert!(typecheck_row_bytes(0x1000, BASE, RowTarget::Class { depth: 8 }, None).is_none());
    assert!(typecheck_row_bytes(0x1008, BASE, RowTarget::Class { depth: 1 }, None).is_none());
    assert!(typecheck_row_bytes(0x1000, 0, RowTarget::Interface, None).is_none());
}
