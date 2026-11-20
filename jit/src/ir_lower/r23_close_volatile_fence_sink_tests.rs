use super::*;
use cratonvm_types::Value;
use std::sync::atomic::AtomicPtr;

const LOCK_ADD: [u8; 5] = [0xF0, 0x83, 0x04, 0x24, 0x00];

extern "C" fn helper_marker(_vm: i64, _c: i64, _f: i64, _v: i64) -> i64 {
    0
}

/// The static helpers are present (the lowering refuses a graph whose
/// helper slot is empty) but never reached: both routes here are inline.
fn no_helpers() -> JitRuntimeHelpers {
    // SAFETY: `JitRuntimeHelpers` is `#[repr(C)]` and every field is a `usize`, so all-zero is a valid value.
    let mut h: JitRuntimeHelpers = unsafe { std::mem::zeroed() };
    h.putstatic_int = helper_marker as *const () as usize;
    h.getstatic = helper_marker as *const () as usize;
    h
}

/// `ctx` IS the base-pointer cell's address; only `(3, 1)` resolves.
unsafe extern "C" fn resolver(ctx: i64, class_id: i64, field_index: i64) -> i64 {
    if class_id == 3 && field_index == 1 {
        ctx
    } else {
        0
    }
}

/// A leaked two-cell statics block and the pointer cell naming it.
fn fresh_block() -> (usize, *mut Value) {
    let block: &'static mut [Value] =
        Box::leak(vec![Value::Int(0), Value::Int(0)].into_boxed_slice());
    let ptr = block.as_mut_ptr();
    for i in 0..2 {
        // SAFETY: `i` indexes the two-cell block just leaked.
        unsafe { cratonvm_types::write_value_atomic(ptr.add(i), Value::Int(0)) };
    }
    let cell: &'static AtomicPtr<Value> = Box::leak(Box::new(AtomicPtr::new(ptr)));
    (cell as *const AtomicPtr<Value> as usize, ptr)
}

/// `v1 -> S (volatile); [read S (volatile)]; v2 -> S (volatile); return
/// [the volatile read | a plain read of S]`, all on the inline static route.
fn graph(read_between: bool) -> Graph {
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    g.entry = start;
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let store = |g: &mut Graph, mem: NodeId, v: i64, pc: usize| {
        let value = g.add(Op::Const(v), IrType::Int, vec![], None);
        g.add(
            Op::StoreStatic {
                class_id: 3,
                field_index: 1,
                type_tag: b'I',
                is_volatile: true,
            },
            IrType::Memory,
            vec![ctrl, mem, value],
            Some(pc),
        )
    };
    let m1 = store(&mut g, mem, 11, 1);
    let (m2, result) = if read_between {
        let read = g.add(
            Op::LoadStatic {
                class_id: 3,
                field_index: 1,
                type_tag: b'I',
                is_volatile: true,
            },
            IrType::Int,
            vec![ctrl, m1],
            Some(4),
        );
        (read, read)
    } else {
        (m1, g.add(Op::Const(7), IrType::Int, vec![], None))
    };
    let m3 = store(&mut g, m2, 22, 7);
    // Without a read between, the method returns a PLAIN read of the cell
    // after both stores (transparent to the sink): it must see the last.
    let result = if read_between {
        result
    } else {
        g.add(
            Op::LoadStatic {
                class_id: 3,
                field_index: 1,
                type_tag: b'I',
                is_volatile: false,
            },
            IrType::Int,
            vec![ctrl, m3],
            Some(9),
        )
    };
    g.exit = g.add(Op::Return, IrType::Void, vec![ctrl, result], Some(10));
    g
}

fn lower_with(g: &Graph, cell: usize) -> CompiledMethod {
    let direct = crate::DirectHelperTable {
        static_base_resolver: resolver as *const () as usize,
        static_base_resolver_ctx: cell,
        ..crate::DirectHelperTable::EMPTY
    };
    let sch = crate::ir_schedule::schedule(g);
    lower_inner_with_direct_helpers(
        &direct,
        g,
        &sch,
        0,
        0,
        &no_helpers(),
        &HashMap::new(),
        &[],
        None,
        &HashMap::new(),
        &HashMap::new(),
        &HashMap::new(),
        &crate::ir::IrInlineFrameSites::default(),
    )
    .expect("volatile putstatic lowers")
}

fn fences(cm: &CompiledMethod) -> usize {
    cm.code_bytes()
        .windows(LOCK_ADD.len())
        .filter(|w| *w == LOCK_ADD)
        .count()
}

/// Two volatile static stores with a plain read share a fence and retain the last value.
#[test]
fn r23_close_two_volatile_static_stores_share_one_fence() {
    let (cell, _block) = fresh_block();
    let cm = lower_with(&graph(false), cell);
    assert_eq!(fences(&cm), 1);
    // SAFETY: the inline routes make no call and read no context.
    let got = unsafe { cm.try_call_with_context(0, &[]) }.expect("call");
    assert_eq!(got as i32, 22, "the last store wins");
}

/// A volatile LOAD between the stores is what the fence orders against:
/// the first store's fence stands before it, the second's before the
/// return -- two fences under the sink as without it, and the fence
/// precedes the load's bytes.
#[test]
fn r23_close_a_volatile_load_between_settles_the_owed_fence() {
    let (cell, _block) = fresh_block();
    let cm = lower_with(&graph(true), cell);
    assert_eq!(fences(&cm), 2);
    // SAFETY: as above.
    let got = unsafe { cm.try_call_with_context(0, &[]) }.expect("call");
    assert_eq!(got as i32, 11, "the load reads the first store");
}

/// The transparency rule: plain statics, any putstatic and arithmetic
/// carry an owed fence; a volatile getstatic, a return, a call do not.
#[test]
fn r23_close_what_the_sink_sees_through() {
    let ls = |is_volatile| Op::LoadStatic {
        class_id: 3,
        field_index: 1,
        type_tag: b'I',
        is_volatile,
    };
    assert!(fence_sink_transparent(&ls(false)));
    assert!(!fence_sink_transparent(&ls(true)));
    assert!(fence_sink_transparent(&Op::StoreStatic {
        class_id: 3,
        field_index: 1,
        type_tag: b'L',
        is_volatile: true,
    }));
    assert!(fence_sink_transparent(&Op::Add));
    assert!(!fence_sink_transparent(&Op::Return));
    assert!(!fence_sink_transparent(&Op::Call { info_ptr: 0 }));
}
