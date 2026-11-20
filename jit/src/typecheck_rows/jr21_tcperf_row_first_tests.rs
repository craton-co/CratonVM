use super::*;

const FELL: i64 = -1;
const OBJECT: u32 = 1;
const BASE: u32 = 0x20;
const A1: u32 = 0x21;
const A2: u32 = 0x22;
const B1: u32 = 0x23;
const OTHER: u32 = 0x24;
const VERDICT0: u32 = 0x25;
const DEEP: u32 = 0x26;
const UNPUBLISHED: u32 = 0x27;
const MANY: u32 = 0x28;
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

    fn write(&mut self, id: u32, words: &[u32; ROW_WORDS]) {
        let off = self.1 - self.0.as_ptr() as usize + (id as usize) * ROW_BYTES; // Cast: addresses
        assert!(off + ROW_BYTES <= self.0.len() * std::mem::size_of::<u64>());
        // SAFETY: the aligned row and all its words fit in the owned buffer.
        let p = unsafe { (self.0.as_mut_ptr() as *mut u8).add(off) as *mut u32 };
        for (i, w) in words.iter().enumerate() {
            // SAFETY: the checked row contains every word produced by row_words.
            unsafe { p.add(i).write(*w) };
        }
    }

    fn fill(&mut self, id: u32, chain: &[u32], ifaces: &[u32], hierarchy_only: bool) {
        let words = row_words(chain, ifaces, hierarchy_only);
        self.write(id, &words);
    }
}

fn build(
    rows: usize,
    target: u32,
    kind: RowTarget,
    latch: Option<usize>,
) -> crate::ExecutableBuffer {
    let arm = typecheck_row_first_bytes(rows, target, kind, latch).expect("an arm");
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
    t.fill(VERDICT0, &[OBJECT, BASE, VERDICT0], &[SHAPE], false);
    let deep: Vec<u32> = (0..12)
        .map(|i| if i == 0 { OBJECT } else { 0x100 + i })
        .chain([DEEP])
        .collect();
    t.fill(DEEP, &deep, &[], true);
    t.fill(MANY, &[OBJECT, MANY], &[SHAPE, CLASHING], true);
    t
}

#[test]
fn a_status_first_class_arm_answers_hits_and_misses_in_one_decision() {
    let t = table();
    let latch = Box::new(std::sync::atomic::AtomicU8::new(0));
    let latch_addr = latch.as_ptr() as usize; // Cast: the latch's address
    let base_arm = build(t.1, BASE, RowTarget::Class { depth: 1 }, Some(latch_addr));
    for id in [BASE, A1, A2, B1] {
        assert_eq!(ask(&base_arm, id), 1, "a BASE");
    }
    for id in [OTHER, DEEP, MANY] {
        assert_eq!(ask(&base_arm, id), 0, "not a BASE");
    }
    for id in [VERDICT0, UNPUBLISHED, ROWS_CAP, 0x8000_0001, u32::MAX] {
        assert_eq!(ask(&base_arm, id), FELL, "falls through");
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
    assert_eq!(ask(&hit_only, VERDICT0), FELL);
    latch.store(1, std::sync::atomic::Ordering::Relaxed);
    assert_eq!(ask(&base_arm, A1), 1);
    assert_eq!(ask(&base_arm, OTHER), FELL);
    drop(latch);
}

#[test]
fn a_status_first_interface_arm_answers_placed_hits_and_bloom_misses() {
    let t = table();
    let latch = Box::new(std::sync::atomic::AtomicU8::new(0));
    let latch_addr = latch.as_ptr() as usize; // Cast: the latch's address
    let shape = build(t.1, SHAPE, RowTarget::Interface, Some(latch_addr));
    for id in [A1, A2, B1, MANY] {
        assert_eq!(ask(&shape, id), 1, "implements SHAPE");
    }
    for id in [BASE, OTHER, DEEP] {
        assert_eq!(ask(&shape, id), 0, "does not implement SHAPE");
    }
    assert_eq!(
        ask(&shape, VERDICT0),
        FELL,
        "verdict 0: the old arms answer it"
    );
    let clashing = build(t.1, CLASHING, RowTarget::Interface, Some(latch_addr));
    assert_eq!(
        ask(&clashing, MANY),
        FELL,
        "listed in the bloom, not placed"
    );
    assert_eq!(ask(&clashing, B1), FELL);
    assert_eq!(ask(&clashing, A1), 0);
    for id in [UNPUBLISHED, 0x8000_0001] {
        assert_eq!(ask(&clashing, id), FELL);
    }
    let named_hit_only = build(t.1, NAMED, RowTarget::Interface, None);
    assert_eq!(ask(&named_hit_only, A2), 1);
    assert_eq!(ask(&named_hit_only, A1), FELL);
    drop(latch);
}

#[test]
fn a_row_with_words_but_no_status_answers_nothing() {
    let mut t = table();
    let mut words = row_words(&[OBJECT, BASE, A1, UNPUBLISHED], &[SHAPE], true);
    words[0] = 0;
    t.write(UNPUBLISHED, &words);
    let latch = Box::new(std::sync::atomic::AtomicU8::new(0));
    let latch_addr = latch.as_ptr() as usize; // Cast: the latch's address
    let a1_arm = build(t.1, A1, RowTarget::Class { depth: 2 }, Some(latch_addr));
    assert_eq!(ask(&a1_arm, UNPUBLISHED), FELL);
    let other_arm = build(t.1, OTHER, RowTarget::Class { depth: 1 }, Some(latch_addr));
    assert_eq!(ask(&other_arm, UNPUBLISHED), FELL);
    let shape = build(t.1, SHAPE, RowTarget::Interface, Some(latch_addr));
    assert_eq!(ask(&shape, UNPUBLISHED), FELL);
    drop(latch);
}

fn build_word(
    rows: usize,
    target: u32,
    kind: RowTarget,
    latch: Option<usize>,
) -> crate::ExecutableBuffer {
    let arm =
        typecheck_row_first_from_word_bytes(rows, target, kind, latch).expect("a word arm");
    let mut body: Vec<u8> = Vec::new();
    #[cfg(not(target_os = "windows"))]
    body.extend_from_slice(&[0x48, 0x89, 0xF9]); // MOV RCX, RDI
    body.extend_from_slice(&[0x85, 0xC9]); // TEST ECX, ECX
    body.extend_from_slice(&[0x0F, 0x84]); // JZ fall
    let jz = body.len();
    body.extend_from_slice(&[0, 0, 0, 0]);
    let at = body.len();
    body.extend_from_slice(&arm.bytes);
    let fall_at = body.len();
    body.extend_from_slice(&[0x48, 0xC7, 0xC0, 0xFF, 0xFF, 0xFF, 0xFF, 0xC3]); // MOV RAX, -1 ; RET
    let hit_at = body.len();
    body.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00, 0xC3]); // MOV EAX, 1 ; RET
    let miss_at = body.len();
    body.extend_from_slice(&[0x31, 0xC0, 0xC3]); // XOR EAX, EAX ; RET
    let mut patch = |operand: usize, to: usize| {
        let rel =
            i32::try_from(to).expect("small") - i32::try_from(operand + 4).expect("small");
        body[operand..operand + 4].copy_from_slice(&rel.to_le_bytes());
    };
    patch(jz, fall_at);
    for h in &arm.hit {
        patch(at + *h, hit_at);
    }
    for m in &arm.miss {
        patch(at + *m, miss_at);
    }
    let mut buf = crate::ExecutableBuffer::new(1024).expect("executable buffer");
    buf.emit(&body);
    assert!(!buf.overflowed());
    buf.finalize();
    buf
}

fn ask_word(buf: &crate::ExecutableBuffer, id: u32) -> i64 {
    let word = i64::from(id as i32) as u64;
    // SAFETY: build finalized this buffer as executable code with the matching C ABI.
    let f = unsafe { std::mem::transmute::<*const u8, extern "C" fn(u64) -> i64>(buf.as_ptr()) };
    f(word)
}

#[test]
fn tcf21_the_word_arm_answers_what_the_header_arm_answers() {
    let t = table();
    let latch = Box::new(std::sync::atomic::AtomicU8::new(0));
    let latch_addr = latch.as_ptr() as usize; // Cast: the latch's address
    let ids = [
        0,
        OBJECT,
        BASE,
        A1,
        A2,
        B1,
        OTHER,
        VERDICT0,
        DEEP,
        UNPUBLISHED,
        MANY,
        ROWS_CAP - 1,
        ROWS_CAP,
        0x7FFF_FFFF,
        0x8000_0000,
        0x8000_0001,
        u32::MAX,
    ];
    let targets = [
        (BASE, RowTarget::Class { depth: 1 }),
        (A1, RowTarget::Class { depth: 2 }),
        (0x107, RowTarget::Class { depth: 7 }),
        (SHAPE, RowTarget::Interface),
        (NAMED, RowTarget::Interface),
        (CLASHING, RowTarget::Interface),
    ];
    for (target, kind) in targets {
        for latch_opt in [Some(latch_addr), None] {
            let header = build(t.1, target, kind, latch_opt);
            let word = build_word(t.1, target, kind, latch_opt);
            for id in ids {
                assert_eq!(
                    ask_word(&word, id),
                    ask(&header, id),
                    "target {target:#x} ({kind:?}), receiver {id:#x}, latch {latch_opt:?}"
                );
            }
        }
    }
    let base_word = build_word(t.1, BASE, RowTarget::Class { depth: 1 }, Some(latch_addr));
    assert_eq!(ask_word(&base_word, 0), FELL);
    assert_eq!(ask_word(&base_word, A2), 1);
    assert_eq!(ask_word(&base_word, OTHER), 0);
    assert_eq!(ask_word(&base_word, 0x8000_0001), FELL);
    drop(latch);
}

#[test]
fn tcf21_the_word_builder_drops_exactly_the_header_load() {
    for (target, kind) in [
        (SHAPE, RowTarget::Interface),
        (BASE, RowTarget::Class { depth: 1 }),
    ] {
        for latch in [Some(0x2000usize), None] {
            let header =
                typecheck_row_first_bytes(0x1000, target, kind, latch).expect("an arm");
            let word = typecheck_row_first_from_word_bytes(0x1000, target, kind, latch)
                .expect("a word arm");
            assert_eq!(&header.bytes[..2], &[0x8Bu8, 0x08][..], "the header load");
            assert_eq!(word.bytes, header.bytes[2..].to_vec());
            assert_eq!(word.hit.len(), header.hit.len());
            assert_eq!(word.miss.len(), header.miss.len());
            for (w, h) in word.hit.iter().zip(&header.hit) {
                assert_eq!(*w + 2, *h);
            }
            for (w, h) in word.miss.iter().zip(&header.miss) {
                assert_eq!(*w + 2, *h);
            }
        }
    }
    let class1 = RowTarget::Class { depth: 1 };
    assert!(typecheck_row_first_from_word_bytes(
        0x1000,
        BASE,
        RowTarget::Class { depth: 0 },
        None
    )
    .is_none());
    assert!(typecheck_row_first_from_word_bytes(
        0x1000,
        BASE,
        RowTarget::Class { depth: 8 },
        None
    )
    .is_none());
    assert!(typecheck_row_first_from_word_bytes(0x1008, BASE, class1, None).is_none());
    assert!(
        typecheck_row_first_from_word_bytes(0x1000, 0, RowTarget::Interface, None).is_none()
    );
    assert!(typecheck_row_first_from_word_bytes(0, BASE, class1, None).is_none());
}

#[test]
fn the_status_first_builder_refuses_what_the_rows_cannot_answer() {
    let class1 = RowTarget::Class { depth: 1 };
    assert!(
        typecheck_row_first_bytes(0x1000, BASE, RowTarget::Class { depth: 0 }, None).is_none()
    );
    assert!(
        typecheck_row_first_bytes(0x1000, BASE, RowTarget::Class { depth: 8 }, None).is_none()
    );
    assert!(typecheck_row_first_bytes(0x1008, BASE, class1, None).is_none());
    assert!(typecheck_row_first_bytes(0x1000, 0, RowTarget::Interface, None).is_none());
    assert!(typecheck_row_first_bytes(0, BASE, class1, None).is_none());
    let arm = typecheck_row_first_bytes(0x1000, BASE, class1, None).expect("an arm");
    assert!(arm.miss.is_empty());
    assert_eq!(arm.hit.len(), 1);
    let arm = typecheck_row_first_bytes(0x1000, SHAPE, RowTarget::Interface, Some(0x2000))
        .expect("an arm");
    assert_eq!(arm.miss.len(), 1);
    assert_eq!(arm.hit.len(), 1);
}
