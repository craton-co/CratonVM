use super::{
    typecheck_array_component_miss_bytes, typecheck_array_hit_bytes, TypecheckArrayPlan,
    VhSubtypeDisplay, CALL_ARG_REGS,
};

/// `java/lang/Object`: the first class a VM loads, id 0.
const OBJECT: u32 = 0;
const BASE: u32 = 0x1001;
const SUB: u32 = 0x1002;
const OTHER: u32 = 0x1004;
/// A class whose closure holds an instance-admitted shape (verdict word 0).
const STAMP: u32 = 0x1006;
/// Never published.
const UNPUB: u32 = 0x1007;
/// A nested array class as a component (`ClassOrigin::VmArray`: verdict 0).
const ARRCLS: u32 = 0x1010;
const CLONEABLE: u32 = 0x2001;
const IF_A: u32 = 0x3001;
const CAP: u64 = 0x5000;
const HIT: i64 = 1;
const MISS: i64 = 0;
const DECLINED: i64 = -1;

fn mov(dst: u8, src: u8) -> [u8; 3] {
    [
        0x48 | (u8::from(src >= 8) << 2) | u8::from(dst >= 8),
        0x89,
        0xC0 | ((src & 7) << 3) | (dst & 7),
    ]
}

/// The displays, the table that holds them and the cell that holds the
/// table, all kept alive by the caller.
struct Fixture {
    cell: Box<u64>,
    _table: Box<[u64]>,
    _displays: Vec<Box<[u32]>>,
}

impl Fixture {
    fn new() -> Self {
        // (id, chain root first, interfaces, verdict)
        let rows: [(u32, &[u32], &[u32], u32); 7] = [
            (OBJECT, &[OBJECT], &[], 1),
            (BASE, &[OBJECT, BASE], &[], 1),
            (SUB, &[OBJECT, BASE, SUB], &[IF_A], 1),
            (OTHER, &[OBJECT, OTHER], &[], 1),
            (STAMP, &[OBJECT, STAMP], &[IF_A], 0),
            (ARRCLS, &[OBJECT, ARRCLS], &[CLONEABLE], 0),
            (CLONEABLE, &[OBJECT, CLONEABLE], &[], 1),
        ];
        let displays: Vec<Box<[u32]>> = rows
            .iter()
            .map(|(_, c, i, v)| {
                let mut d = vec![u32::try_from(c.len()).expect("small")];
                d.extend_from_slice(c);
                d.push(u32::try_from(i.len()).expect("small"));
                d.extend_from_slice(i);
                d.push(*v);
                d.into_boxed_slice()
            })
            .collect();
        let mut table = vec![0u64; usize::try_from(CAP).expect("small") + 1];
        table[0] = CAP;
        for ((id, _, _, _), d) in rows.iter().zip(&displays) {
            table[1 + usize::try_from(*id).expect("small")] = d.as_ptr() as u64; // Cast: address
        }
        let table = table.into_boxed_slice();
        let cell = Box::new(table.as_ptr() as u64); // Cast: address
        Fixture {
            cell,
            _table: table,
            _displays: displays,
        }
    }

    fn display(&self, depth: u32, interface: bool) -> VhSubtypeDisplay {
        VhSubtypeDisplay {
            cell: &*self.cell as *const u64 as usize, // Cast: the cell's address
            depth,
            interface,
        }
    }
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

/// `extern "C" fn(u64) -> 1 | 0 | -1`: the hit arm of `plan`, then the
/// component miss over its declines, as the lowerer emits them.
fn build(
    plan: TypecheckArrayPlan,
    display: VhSubtypeDisplay,
    latch: usize,
) -> crate::ExecutableBuffer {
    let (hit_bytes, hit_decline, hit_hits) = typecheck_array_hit_bytes(plan);
    let (miss_bytes, miss_decline, miss_hit, miss_miss) =
        typecheck_array_component_miss_bytes(display, plan.component, latch);
    let mut body: Vec<u8> = mov(0, CALL_ARG_REGS[0]).to_vec(); // RAX = arg0
    let hit_at = body.len();
    body.extend_from_slice(&hit_bytes);
    let miss_at = body.len();
    body.extend_from_slice(&miss_bytes);
    let one_at = body.len();
    body.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00, 0xC3]); // MOV EAX, 1 ; RET
    let zero_at = body.len();
    body.extend_from_slice(&[0x31, 0xC0, 0xC3]); // XOR EAX, EAX ; RET
    let decline_at = body.len();
    body.extend_from_slice(&[0x48, 0xC7, 0xC0, 0xFF, 0xFF, 0xFF, 0xFF, 0xC3]); // MOV RAX, -1 ; RET
    let mut patch = |field: usize, to: usize| {
        let rel = i32::try_from(to).expect("small") - i32::try_from(field + 4).expect("small");
        body[field..field + 4].copy_from_slice(&rel.to_le_bytes());
    };
    for d in hit_decline {
        patch(hit_at + d, miss_at);
    }
    for h in hit_hits {
        patch(hit_at + h, one_at);
    }
    for d in miss_decline {
        patch(miss_at + d, decline_at);
    }
    patch(miss_at + miss_hit, one_at);
    patch(miss_at + miss_miss, zero_at);
    let mut buf = crate::ExecutableBuffer::new(4096).expect("executable buffer");
    buf.emit(&body);
    assert!(!buf.overflowed());
    buf.finalize();
    buf
}

fn ask(buf: &crate::ExecutableBuffer, h: &[u64; 4]) -> i64 {
    // SAFETY: a complete `extern "C" fn(u64) -> i64` reading only the fake
    // header, the fixtures the caller keeps alive and the latch byte, and
    // writing RAX, RCX, R11 (caller-saved on both ABIs).
    let f =
        unsafe { std::mem::transmute::<*const u8, extern "C" fn(u64) -> i64>(buf.as_ptr()) };
    f(h.as_ptr() as u64)
}

#[test]
fn tca21_a_class_target_answers_a_hierarchy_only_component_miss() {
    let fx = Fixture::new();
    let latch = Box::new(0u8);
    let latch_addr = &*latch as *const u8 as usize; // Cast: the latch's address
    let r = cratonvm_types::ObjectKind::Array as u8;
    let display = fx.display(1, false);
    let buf = build(
        TypecheckArrayPlan {
            component: BASE,
            object_component: false,
            display: Some(display),
        },
        display,
        latch_addr,
    );
    // Hits stay hits: `Sub[]` and `Base[]` are `Base[]` (covariance).
    assert_eq!(ask(&buf, &header(SUB, r)), HIT, "Sub[] is a Base[]");
    assert_eq!(ask(&buf, &header(BASE, r)), HIT, "Base[] is a Base[]");
    // A hierarchy-only component outside Base's subtree is `false`.
    assert_eq!(ask(&buf, &header(OTHER, r)), MISS, "Other[] is no Base[]");
    // `Object[]` (component id 0) is no `Base[]`: Object is not a Base.
    assert_eq!(ask(&buf, &header(OBJECT, r)), MISS, "Object[] is no Base[]");
    // Every other outcome asks the helper: an instance-admitted closure
    // (verdict 0), a nested array's component (a VmArray closure), a
    // class with no display, a synthetic id, an id past the table.
    for (id, what) in [
        (STAMP, "a verdict-0 component"),
        (ARRCLS, "an array class as the component"),
        (UNPUB, "an unpublished component"),
        (0x8000_0001, "a lambda-proxy id"),
        (u32::MAX, "an id past the table"),
    ] {
        assert_eq!(ask(&buf, &header(id, r)), DECLINED, "{what}");
    }
    drop((fx, latch));
}



