use super::*;
use crate::ir::{CatchArmRow, IrBuilder};
use crate::ir_schedule;
use cratonvm_types::flags::with_thread_overrides;

const LOOKUP: usize = 0x1111_2222_3333_4450;

/// `static int f(int n) { int c = 0; for (int i = 0; i < n; i++) { try
/// { g(i); } catch (E e) { c++; } } return c; }`, `[9, 13) -> 16, p/E`
/// (the `ir` tests' fixture).
const LOOP: [u8; 28] = [
    0x03, 0x3d, 0x03, 0x3c, 0x1b, 0x1a, 0xa2, 0x00, 0x14, 0x1b, 0xb8, 0x00, 0x02, 0xa7, 0x00,
    0x07, 0x4e, 0x84, 0x02, 0x01, 0x84, 0x01, 0x01, 0xa7, 0xff, 0xed, 0x1c, 0xac,
];

fn graph() -> crate::ir::Graph {
    let info: &'static JitInvokeInfo = Box::leak(Box::new(JitInvokeInfo {
        class_name: "p/Host",
        method_name: "g",
        descriptor: "(I)V",
        num_jit_args: 1,
        return_type: b'V',
        invoke_kind: 3,
        declaring_class_id: 0x7101,
        owner_class_id: 0,
    }));
    let mut b = IrBuilder::new(1, 4);
    b.set_param_types(&[IrType::Int]);
    b.set_invoke_info(HashMap::from([(
        10usize,
        (info as *const JitInvokeInfo as usize, 1usize, b'V'),
    )]));
    b.set_catch_arm_table(
        vec![CatchArmRow {
            start_pc: 9,
            end_pc: 13,
            handler_pc: 16,
            catch_name: "p/E",
        }],
        7,
    );
    b.build(&LOOP, LOOP.len()).expect("builds")
}

fn helpers() -> JitRuntimeHelpers {
    // SAFETY: `JitRuntimeHelpers` is `#[repr(C)]` and every field is a `usize`, so all-zero is a valid value; the body is never run, only emitted.
    let mut h: JitRuntimeHelpers = unsafe { std::mem::zeroed() };
    h.invoke_dispatch = 0x1111_2222_3333_4440;
    h.dispatch_threw = 0x1111_2222_3333_4442;
    h.service_callee_deopt = 0x1111_2222_3333_4443;
    h.local_handler_lookup = LOOKUP;
    h
}

fn mentions(cm: &CompiledMethod, addr: usize) -> usize {
    let target = addr.to_le_bytes();
    cm.code_bytes().windows(8).filter(|w| *w == target).count()
}



/// `static int f(int n) { int c = 0, s = 0; for (int i = 0; i < n; i++) {
/// try { s += g(i); } catch (E e) { c++; } } return s + c; }`, `[11, 18)
/// -> 21, p/E`. Unlike [`LOOP`], the call's VALUE is used: the `iadd`
/// reading it is placed schedule-early, i.e. after the call in the call's
/// own block -- the `R19Handler5Catch` kernels' shape (`s += f(i)`).
const VALUE_LOOP: [u8; 36] = [
    0x03, 0x3d, 0x03, 0x3e, 0x03, 0x3c, // c = 0; s = 0; i = 0
    0x1b, 0x1a, 0xa2, 0x00, 0x18, // 6: if (i >= n) goto 32
    0x1d, 0x1b, 0xb8, 0x00, 0x02, // 11: s, i, invokestatic g (pc 13)
    0x60, 0x3e, 0xa7, 0x00, 0x08, // 16: iadd; istore_3; goto 26
    0x3a, 0x04, 0x84, 0x02, 0x01, // 21: astore 4; c++
    0x84, 0x01, 0x01, 0xa7, 0xff, 0xe9, // 26: i++; goto 6
    0x1d, 0x1c, 0x60, 0xac, // 32: return s + c
];

fn value_graph() -> crate::ir::Graph {
    let info: &'static JitInvokeInfo = Box::leak(Box::new(JitInvokeInfo {
        class_name: "p/Host",
        method_name: "g",
        descriptor: "(I)I",
        num_jit_args: 1,
        return_type: b'I',
        invoke_kind: 3,
        declaring_class_id: 0x7101,
        owner_class_id: 0,
    }));
    let mut b = IrBuilder::new(1, 5);
    b.set_param_types(&[IrType::Int]);
    b.set_invoke_info(HashMap::from([(
        13usize,
        (info as *const JitInvokeInfo as usize, 1usize, b'I'),
    )]));
    b.set_catch_arm_table(
        vec![CatchArmRow {
            start_pc: 11,
            end_pc: 18,
            handler_pc: 21,
            catch_name: "p/E",
        }],
        7,
    );
    b.build(&VALUE_LOOP, VALUE_LOOP.len()).expect("builds")
}




/// `static int f(int x) { try { return g(x); } catch (A a) { return -1; }
/// catch (B b) { return -2; } }`: `[0, 4) -> 5, p/A; [0, 4) -> 8, p/B`.
const TWO_CATCH: [u8; 12] = [
    0x1a, 0xb8, 0x00, 0x02, 0xac, 0x4c, 0x02, 0xac, 0x4c, 0x10, 0xfe, 0xac,
];




