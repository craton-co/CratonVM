use super::*;

const HIT: i64 = 1;
const MISS: i64 = 0;
const SLOW: i64 = -2;
const OBJECT: u32 = 1;
const BASE: u32 = 0x50;
const A1: u32 = 0x51;
const A2: u32 = 0x52;
const OTHER: u32 = 0x53;
const VERDICT0: u32 = 0x54;
const SUB_OF_VERDICT0: u32 = 0x55;
const UNPUBLISHED: u32 = 0x56;
const DEEP: u32 = 0x57;
const SHAPE: u32 = 0x60;
const INT_ARRAY_BYTE: u8 = 1 | (10 << 2);
const HASHED_PLAIN_BYTE: u8 = 0b1111_1100;

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
        let off = self.1 - self.0.as_ptr() as usize + (id as usize) * ROW_BYTES;
        assert!(off + ROW_BYTES <= self.0.len() * std::mem::size_of::<u64>());
        // SAFETY: the aligned row and all its words fit in the owned buffer.
        let p = unsafe { (self.0.as_mut_ptr() as *mut u8).add(off) as *mut u32 };
        for (i, w) in words.iter().enumerate() {
            // SAFETY: the checked row contains every word produced by row_words.
            unsafe { p.add(i).write(*w) };
        }
    }
}

fn deep_chain() -> Vec<u32> {
    (0..12)
        .map(|i| if i == 0 { OBJECT } else { 0x100 + i })
        .chain([DEEP])
        .collect()
}

fn table() -> Table {
    let mut t = Table::new();
    t.fill(BASE, &[OBJECT, BASE], &[], true);
    t.fill(A1, &[OBJECT, BASE, A1], &[SHAPE], true);
    t.fill(A2, &[OBJECT, BASE, A1, A2], &[SHAPE], true);
    t.fill(OTHER, &[OBJECT, OTHER], &[], true);
    t.fill(VERDICT0, &[OBJECT, BASE, VERDICT0], &[SHAPE], false);
    t.fill(
        SUB_OF_VERDICT0,
        &[OBJECT, BASE, VERDICT0, SUB_OF_VERDICT0],
        &[SHAPE],
        false,
    );
    let deep = deep_chain();
    t.fill(DEEP, &deep, &[], true);
    t.fill(0x107, &deep[..8], &[], true);
    t
}

fn finish(body: Vec<u8>) -> crate::ExecutableBuffer {
    let mut buf = crate::ExecutableBuffer::new(1024).expect("executable buffer");
    buf.emit(&body);
    assert!(!buf.overflowed());
    buf.finalize();
    buf
}

fn patch(body: &mut [u8], operand: usize, to: usize) {
    let rel = i32::try_from(to).expect("small") - i32::try_from(operand + 4).expect("small");
    body[operand..operand + 4].copy_from_slice(&rel.to_le_bytes());
}

fn build_instanceof(
    component: u32,
    element: Option<(usize, RowTarget)>,
    latch: usize,
) -> crate::ExecutableBuffer {
    let prefix = typecheck_array_prefix_bytes(Some(component)).expect("a prefix");
    let mut body: Vec<u8> = Vec::new();
    #[cfg(target_os = "windows")]
    body.extend_from_slice(&[0x48, 0x89, 0xC8]); // MOV RAX, RCX
    #[cfg(not(target_os = "windows"))]
    body.extend_from_slice(&[0x48, 0x89, 0xF8]); // MOV RAX, RDI
    let at = body.len();
    body.extend_from_slice(&prefix.bytes);
    let mut hits: Vec<usize> = prefix.hit.iter().map(|e| at + e).collect();
    let mut zeros: Vec<usize> = Vec::new();
    let mut slows: Vec<usize> = Vec::new();
    if let Some((rows, kind)) = element {
        let arm =
            typecheck_row_first_bytes(rows, component, kind, Some(latch)).expect("an arm");
        let arm_at = body.len();
        body.extend_from_slice(&arm.bytes);
        hits.extend(arm.hit.iter().map(|e| arm_at + e));
        zeros.extend(arm.miss.iter().map(|e| arm_at + e));
    }
    body.push(0xE9); // JMP slow (what the arms did not answer)
    slows.push(body.len());
    body.extend_from_slice(&[0, 0, 0, 0]);
    let miss_block_at = body.len();
    for p in &prefix.not_reference_array {
        patch(&mut body, at + p, miss_block_at);
    }
    let nr = typecheck_array_not_reference_miss_bytes(latch);
    body.extend_from_slice(&nr.bytes);
    slows.extend(nr.slow.iter().map(|e| miss_block_at + e));
    zeros.extend(nr.zero.iter().map(|e| miss_block_at + e));
    let slow_at = body.len();
    body.extend_from_slice(&[0x48, 0xC7, 0xC0, 0xFE, 0xFF, 0xFF, 0xFF, 0xC3]); // MOV RAX, -2 ; RET
    let zero_at = body.len();
    body.extend_from_slice(&[0x31, 0xC0, 0xC3]); // XOR EAX, EAX ; RET
    let hit_at = body.len();
    body.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00, 0xC3]); // MOV EAX, 1 ; RET
    for h in hits {
        patch(&mut body, h, hit_at);
    }
    for z in zeros {
        patch(&mut body, z, zero_at);
    }
    for s in slows {
        patch(&mut body, s, slow_at);
    }
    finish(body)
}

fn ask(buf: &crate::ExecutableBuffer, id: u32, kind_byte: u8) -> i64 {
    let header = Box::new(u64::from(id) | (u64::from(kind_byte) << 48));
    // SAFETY: build finalized this buffer as executable code with the matching C ABI.
    let f =
        unsafe { std::mem::transmute::<*const u8, extern "C" fn(*const u64) -> i64>(buf.as_ptr()) };
    f(&*header)
}

#[test]
fn a_class_array_target_answers_every_miss_the_hierarchy_proves() {
    let t = table();
    let latch = Box::new(std::sync::atomic::AtomicU8::new(0));
    let latch_addr = latch.as_ptr() as usize; // Cast: the latch's address
    let r = REFERENCE_ARRAY_KIND_BYTE;
    let base = build_instanceof(BASE, Some((t.1, RowTarget::Class { depth: 1 })), latch_addr);
    for id in [BASE, A1, A2] {
        assert_eq!(ask(&base, id, r), HIT, "{id:#x}[] is a Base[]");
    }
    assert_eq!(ask(&base, OTHER, r), MISS, "OTHER[] is not a Base[]");
    assert_eq!(ask(&base, DEEP, r), MISS, "DEEP[] is not a Base[]");
    for id in [VERDICT0, UNPUBLISHED, 0x8000_0001, ROWS_CAP] {
        assert_eq!(ask(&base, id, r), SLOW, "{id:#x}[]");
    }
    assert_eq!(ask(&base, BASE, 0), MISS, "a plain Base is not a Base[]");
    assert_eq!(ask(&base, A1, HASHED_PLAIN_BYTE), MISS);
    assert_eq!(ask(&base, 0, INT_ARRAY_BYTE), MISS, "int[] is not a Base[]");
    assert_eq!(ask(&base, 0x8000_0001, 0), SLOW);
    assert_eq!(ask(&base, 0, 2), SLOW);
    latch.store(1, std::sync::atomic::Ordering::Relaxed);
    assert_eq!(ask(&base, A1, r), HIT);
    assert_eq!(ask(&base, OTHER, r), SLOW);
    assert_eq!(ask(&base, BASE, 0), SLOW);
    assert_eq!(ask(&base, 0, INT_ARRAY_BYTE), MISS);
    drop(latch);
    drop(t);
}

#[test]
fn an_interface_array_target_answers_bloom_proved_misses() {
    let t = table();
    let latch = Box::new(std::sync::atomic::AtomicU8::new(0));
    let latch_addr = latch.as_ptr() as usize; // Cast: the latch's address
    let r = REFERENCE_ARRAY_KIND_BYTE;
    let shape = build_instanceof(SHAPE, Some((t.1, RowTarget::Interface)), latch_addr);
    for id in [A1, A2, SHAPE] {
        assert_eq!(ask(&shape, id, r), HIT, "{id:#x}[] is a Shape[]");
    }
    for id in [BASE, OTHER, DEEP] {
        assert_eq!(ask(&shape, id, r), MISS, "{id:#x}[] is not a Shape[]");
    }
    assert_eq!(ask(&shape, VERDICT0, r), SLOW);
    assert_eq!(ask(&shape, A1, 0), MISS);
    assert_eq!(ask(&shape, 0, INT_ARRAY_BYTE), MISS);
    let exact = build_instanceof(A1, None, latch_addr);
    assert_eq!(ask(&exact, A1, r), HIT);
    assert_eq!(ask(&exact, A2, r), SLOW);
    assert_eq!(ask(&exact, A1, 0), MISS);
    drop(latch);
    drop(t);
}

#[test]
fn the_not_reference_array_block_refuses_odd_array_bytes() {
    let latch = Box::new(std::sync::atomic::AtomicU8::new(0));
    let latch_addr = latch.as_ptr() as usize; // Cast: the latch's address
    let nr = typecheck_array_not_reference_miss_bytes(latch_addr);
    assert_eq!((nr.slow.len(), nr.zero.len()), (4, 2));
    let exact = build_instanceof(A1, None, latch_addr);
    assert_eq!(ask(&exact, A1, REFERENCE_ARRAY_KIND_BYTE | 0x40), SLOW);
    for elem in 1u8..=10 {
        assert_eq!(ask(&exact, 0, 1 | (elem << 2)), MISS, "element {elem}");
    }
    drop(latch);
}

const CONTINUE: i64 = 7;

fn build_guard(latch: usize) -> crate::ExecutableBuffer {
    let guard = typecheck_array_miss_guard_bytes(latch);
    let mut body: Vec<u8> = Vec::new();
    #[cfg(target_os = "windows")]
    body.extend_from_slice(&[0x48, 0x89, 0xC8]); // MOV RAX, RCX
    #[cfg(not(target_os = "windows"))]
    body.extend_from_slice(&[0x48, 0x89, 0xF8]); // MOV RAX, RDI
    let at = body.len();
    body.extend_from_slice(&guard.bytes);
    body.extend_from_slice(&[0xB8, 0x07, 0x00, 0x00, 0x00, 0xC3]); // MOV EAX, CONTINUE ; RET
    let slow_at = body.len();
    body.extend_from_slice(&[0x48, 0xC7, 0xC0, 0xFE, 0xFF, 0xFF, 0xFF, 0xC3]); // MOV RAX, -2 ; RET
    let zero_at = body.len();
    body.extend_from_slice(&[0x31, 0xC0, 0xC3]); // XOR EAX, EAX ; RET
    for s in &guard.slow {
        patch(&mut body, at + s, slow_at);
    }
    for z in &guard.zero {
        patch(&mut body, at + z, zero_at);
    }
    finish(body)
}

#[test]
fn the_array_miss_guard_passes_reference_arrays_and_answers_the_rest() {
    let latch = Box::new(std::sync::atomic::AtomicU8::new(0));
    let latch_addr = latch.as_ptr() as usize; // Cast: the latch's address
    let guard = build_guard(latch_addr);
    let r = REFERENCE_ARRAY_KIND_BYTE;
    for id in [0, BASE, A1, 0x8000_0001] {
        assert_eq!(ask(&guard, id, r), CONTINUE, "{id:#x}[]");
    }
    assert_eq!(ask(&guard, BASE, 0), MISS);
    assert_eq!(ask(&guard, A1, HASHED_PLAIN_BYTE), MISS);
    assert_eq!(ask(&guard, 0, INT_ARRAY_BYTE), MISS);
    assert_eq!(ask(&guard, 0x8000_0001, 0), SLOW);
    assert_eq!(ask(&guard, 0, 2), SLOW);
    assert_eq!(ask(&guard, A1, r | 0x40), SLOW);
    latch.store(1, std::sync::atomic::Ordering::Relaxed);
    assert_eq!(ask(&guard, BASE, 0), SLOW);
    assert_eq!(ask(&guard, 0, INT_ARRAY_BYTE), MISS);
    assert_eq!(ask(&guard, A1, r), CONTINUE);
    drop(latch);
}

fn build_aastore(rows: usize) -> crate::ExecutableBuffer {
    let (arm, hit) = aastore_subtype_bytes(rows).expect("an arm");
    let mut body: Vec<u8> = Vec::new();
    #[cfg(target_os = "windows")]
    {
        body.extend_from_slice(&[0x49, 0x89, 0xCB]); // MOV R11, RCX
        body.extend_from_slice(&[0x48, 0x89, 0xD0]); // MOV RAX, RDX
    }
    #[cfg(not(target_os = "windows"))]
    {
        body.extend_from_slice(&[0x49, 0x89, 0xFB]); // MOV R11, RDI
        body.extend_from_slice(&[0x48, 0x89, 0xF0]); // MOV RAX, RSI
    }
    body.extend_from_slice(&[0x8B, 0x10]); // MOV EDX, [RAX]
    let at = body.len();
    body.extend_from_slice(&arm);
    body.extend_from_slice(&[0x48, 0xC7, 0xC0, 0xFE, 0xFF, 0xFF, 0xFF, 0xC3]); // MOV RAX, -2 ; RET
    let hit_at = body.len();
    body.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00, 0xC3]); // MOV EAX, 1 ; RET
    for h in hit {
        patch(&mut body, at + h, hit_at);
    }
    finish(body)
}

fn store(buf: &crate::ExecutableBuffer, component: u32, value: u32) -> i64 {
    let array = Box::new(u64::from(component) | (u64::from(REFERENCE_ARRAY_KIND_BYTE) << 48));
    let value = Box::new(u64::from(value));
    // SAFETY: build finalized this buffer as executable code with the matching C ABI.
    let f = unsafe {
        std::mem::transmute::<*const u8, extern "C" fn(*const u64, *const u64) -> i64>(buf.as_ptr())
    };
    f(&*array, &*value)
}

#[test]
fn an_aastore_of_a_subclass_value_passes_inline() {
    let t = table();
    let buf = build_aastore(t.1);
    for (component, value) in [
        (BASE, A1),
        (BASE, A2),
        (A1, A2),
        (BASE, VERDICT0),
        (VERDICT0, SUB_OF_VERDICT0),
        (0x107, DEEP),
    ] {
        assert_eq!(
            store(&buf, component, value),
            HIT,
            "{value:#x} into {component:#x}[]"
        );
    }
    for (component, value) in [
        (BASE, OTHER),
        (A1, BASE),
        (A2, A1),
        (SHAPE, A1),
        (DEEP, DEEP + 1),
        (UNPUBLISHED, A1),
        (BASE, 0x8000_0001),
        (0x8000_0001, A1),
        (ROWS_CAP, A1),
        (BASE, ROWS_CAP),
        (BASE, UNPUBLISHED),
    ] {
        assert_eq!(
            store(&buf, component, value),
            SLOW,
            "{value:#x} into {component:#x}[]"
        );
    }
    assert!(aastore_subtype_bytes(0).is_none());
    assert!(aastore_subtype_bytes(0x1008).is_none());
    drop(t);
}

#[test]
fn a_row_carries_its_own_depth_in_byte_one_only_when_the_rows_store_it() {
    assert_eq!(row_words(&[OBJECT], &[], true)[0], STATUS_HIERARCHY_ONLY);
    assert_eq!(
        row_words(&[OBJECT, BASE], &[], true)[0],
        STATUS_HIERARCHY_ONLY | (1 << STATUS_DEPTH_SHIFT)
    );
    assert_eq!(
        row_words(&deep_chain()[..8], &[], false)[0],
        STATUS_PUBLISHED | (7 << STATUS_DEPTH_SHIFT)
    );
    assert_eq!(
        row_words(&deep_chain(), &[], true)[0],
        STATUS_HIERARCHY_ONLY
    );
    assert_eq!(STATUS_DEPTH_SHIFT % 8, 0, "a byte of its own");
}
