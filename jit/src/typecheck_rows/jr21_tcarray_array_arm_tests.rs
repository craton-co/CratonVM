use super::*;

const HIT: i64 = 1;
const SLOW: i64 = -2;
const PLAIN: i64 = -3;
const OBJECT: u32 = 1;
const BASE: u32 = 0x30;
const A1: u32 = 0x31;
const A2: u32 = 0x32;
const OTHER: u32 = 0x33;
const VERDICT0: u32 = 0x34;
const NESTED: u32 = 0x35;
const SHAPE: u32 = 0x40;
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

fn table() -> Table {
    let mut t = Table::new();
    t.fill(BASE, &[OBJECT, BASE], &[], true);
    t.fill(A1, &[OBJECT, BASE, A1], &[SHAPE], true);
    t.fill(A2, &[OBJECT, BASE, A1, A2], &[SHAPE], true);
    t.fill(OTHER, &[OBJECT, OTHER], &[], true);
    t.fill(VERDICT0, &[OBJECT, BASE, VERDICT0], &[SHAPE], false);
    t
}

fn build(
    component: Option<u32>,
    element: Option<(usize, RowTarget)>,
) -> crate::ExecutableBuffer {
    let prefix = typecheck_array_prefix_bytes(component).expect("a prefix");
    let mut body: Vec<u8> = Vec::new();
    #[cfg(target_os = "windows")]
    body.extend_from_slice(&[0x48, 0x89, 0xC8]); // MOV RAX, RCX
    #[cfg(not(target_os = "windows"))]
    body.extend_from_slice(&[0x48, 0x89, 0xF8]); // MOV RAX, RDI
    let at = body.len();
    body.extend_from_slice(&prefix.bytes);
    let mut hits: Vec<usize> = prefix.hit.iter().map(|e| at + e).collect();
    if let Some((rows, kind)) = element {
        let target = component.expect("a component");
        let arm = typecheck_row_first_bytes(rows, target, kind, None).expect("an arm");
        let arm_at = body.len();
        body.extend_from_slice(&arm.bytes);
        hits.extend(arm.hit.iter().map(|e| arm_at + e));
    }
    body.extend_from_slice(&[0x48, 0xC7, 0xC0, 0xFE, 0xFF, 0xFF, 0xFF, 0xC3]); // MOV RAX, -2 ; RET
    let plain_at = body.len();
    body.extend_from_slice(&[0x48, 0xC7, 0xC0, 0xFD, 0xFF, 0xFF, 0xFF, 0xC3]); // MOV RAX, -3 ; RET
    let hit_at = body.len();
    body.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00, 0xC3]); // MOV EAX, 1 ; RET
    let mut patch = |operand: usize, to: usize| {
        let rel =
            i32::try_from(to).expect("small") - i32::try_from(operand + 4).expect("small");
        body[operand..operand + 4].copy_from_slice(&rel.to_le_bytes());
    };
    for h in hits {
        patch(h, hit_at);
    }
    for p in &prefix.not_reference_array {
        patch(at + p, plain_at);
    }
    let mut buf = crate::ExecutableBuffer::new(1024).expect("executable buffer");
    buf.emit(&body);
    assert!(!buf.overflowed());
    buf.finalize();
    buf
}

fn ask(buf: &crate::ExecutableBuffer, id: u32, kind_byte: u8) -> i64 {
    let header = Box::new(u64::from(id) | (u64::from(kind_byte) << 48));
    // SAFETY: build finalized this buffer as executable code with the matching C ABI.
    let f =
        unsafe { std::mem::transmute::<*const u8, extern "C" fn(*const u64) -> i64>(buf.as_ptr()) };
    f(&*header)
}

#[test]
fn the_reference_array_byte_is_the_header_layouts() {
    assert_eq!(cratonvm_types::KIND_TAGS_BYTE_OFFSET, 6);
    assert_eq!(REFERENCE_ARRAY_KIND_BYTE, 1);
    assert_eq!(
        REFERENCE_ARRAY_KIND_BYTE & cratonvm_types::KIND_TAG_BYTE_MASK,
        cratonvm_types::ObjectKind::Array as u8
    );
    assert_eq!(
        cratonvm_types::primitive_array_kind_tags_byte("[I"),
        Some(INT_ARRAY_BYTE)
    );
    assert!(typecheck_array_prefix_bytes(Some(0)).is_none());
    let object = typecheck_array_prefix_bytes(None).expect("Object[]");
    assert_eq!((object.hit.len(), object.not_reference_array.len()), (1, 1));
}

#[test]
fn an_object_array_target_admits_every_reference_array_and_nothing_else() {
    let buf = build(None, None);
    for id in [OBJECT, BASE, NESTED, 0x8000_0001, u32::MAX] {
        assert_eq!(ask(&buf, id, REFERENCE_ARRAY_KIND_BYTE), HIT, "{id:#x}[]");
    }
    assert_eq!(ask(&buf, 0, INT_ARRAY_BYTE), PLAIN);
    assert_eq!(ask(&buf, OBJECT, 0), PLAIN);
    assert_eq!(ask(&buf, A1, HASHED_PLAIN_BYTE), PLAIN);
    assert_eq!(ask(&buf, 0, 2), PLAIN);
}

#[test]
fn a_class_component_answers_exact_and_subtype_hits_and_sends_the_rest_to_the_helper() {
    let t = table();
    let base = build(Some(BASE), Some((t.1, RowTarget::Class { depth: 1 })));
    for id in [BASE, A1, A2] {
        assert_eq!(
            ask(&base, id, REFERENCE_ARRAY_KIND_BYTE),
            HIT,
            "{id:#x}[] is a Base[]"
        );
    }
    for id in [OTHER, OBJECT, VERDICT0, 0x8000_0001, NESTED] {
        assert_eq!(ask(&base, id, REFERENCE_ARRAY_KIND_BYTE), SLOW, "{id:#x}[]");
    }
    assert_eq!(ask(&base, BASE, 0), PLAIN);
    assert_eq!(ask(&base, A1, HASHED_PLAIN_BYTE), PLAIN);
    assert_eq!(ask(&base, 0, INT_ARRAY_BYTE), PLAIN);
    let exact = build(Some(A1), None);
    assert_eq!(ask(&exact, A1, REFERENCE_ARRAY_KIND_BYTE), HIT);
    assert_eq!(ask(&exact, A2, REFERENCE_ARRAY_KIND_BYTE), SLOW);
    assert_eq!(ask(&exact, A1, 0), PLAIN);
    drop(t);
}

#[test]
fn an_interface_component_answers_placed_implementors() {
    let t = table();
    let shape = build(Some(SHAPE), Some((t.1, RowTarget::Interface)));
    for id in [A1, A2, SHAPE] {
        assert_eq!(
            ask(&shape, id, REFERENCE_ARRAY_KIND_BYTE),
            HIT,
            "{id:#x}[] is a Shape[]"
        );
    }
    for id in [BASE, OTHER, VERDICT0] {
        assert_eq!(
            ask(&shape, id, REFERENCE_ARRAY_KIND_BYTE),
            SLOW,
            "{id:#x}[]"
        );
    }
    assert_eq!(ask(&shape, A1, 0), PLAIN);
    drop(t);
}
