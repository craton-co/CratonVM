use super::{
    emit_cell_writeback, promoted_static_cell_shape_of, CellWriteBack, CellWriteBackSrc,
    StaticCellWriteBack,
};
use crate::ir::{Graph, IrType, Op};
use crate::ExecutableBuffer;

/// Run one write-back as an `extern "C" fn()`. It clobbers only RAX, RCX,
/// R11 and the flags, all caller-saved on both ABIs.
fn run(wb: &CellWriteBack) {
    let mut buf = ExecutableBuffer::new(512).expect("executable buffer");
    buf.emit(&[0x48, 0x83, 0xEC, 40]); // SUB RSP, 40
    emit_cell_writeback(&mut buf, wb);
    buf.emit(&[0x48, 0x83, 0xC4, 40]); // ADD RSP, 40
    buf.emit(&[0xC3]); // RET
    assert!(!buf.overflowed());
    buf.finalize();
    // SAFETY: `buf` holds a complete `extern "C" fn()` that writes no
    // callee-saved register and only the block the test owns.
    let f = unsafe { std::mem::transmute::<*const u8, extern "C" fn()>(buf.as_ptr()) };
    f();
}

fn wb(base_cell: &u64, long: bool, value: i64, init: i64) -> CellWriteBack {
    CellWriteBack {
        base_off: 0,
        field_index: 1,
        long,
        value: CellWriteBackSrc::Imm(value),
        init: CellWriteBackSrc::Imm(init),
        helper: 0,
        static_cell: Some(StaticCellWriteBack {
            // Cast: the fake base cell's address.
            base_cell: base_cell as *const u64 as u64,
            cell_off: 16,
            hi_off: 24,
            long,
        }),
    }
}

#[test]
fn a_changed_static_cell_gets_the_inline_putstatic_words() {
    let mut block = [0xAAAA_AAAA_AAAA_AAAAu64; 6];
    // Cast: the block's address, as the VM's base cell holds it.
    let base_cell: u64 = block.as_mut_ptr() as u64;
    run(&wb(&base_cell, false, -5, 7));
    // Cast: the int payload's bits in the high half of word 0.
    assert_eq!(
        block[2],
        u64::from(-5i32 as u32) << 32,
        "tag Int | payload << 32"
    );
    assert_eq!(block[3], 0, "word 1 cleared");
    assert_eq!(
        block[1], 0xAAAA_AAAA_AAAA_AAAA,
        "the next cell is untouched"
    );
    run(&wb(&base_cell, true, 0x1_0000_0002, 2));
    assert_eq!(block[2], 1, "tag Long");
    assert_eq!(block[3], 0x1_0000_0002, "the 64-bit payload");
}

#[test]
fn an_unchanged_static_cell_is_not_written() {
    let mut block = [0x5555u64; 6];
    // Cast: the block's address.
    let base_cell: u64 = block.as_mut_ptr() as u64;
    run(&wb(&base_cell, false, 9, 9));
    run(&wb(&base_cell, true, -1, -1));
    assert_eq!(block, [0x5555u64; 6]);
}

/// The shape reader accepts exactly the diamond the promotion builds.
#[test]
fn the_exit_store_shape_is_read_back() {
    let mut g = Graph::empty(0);
    let start = g.add(Op::Start, IrType::Control, vec![], None);
    let mem = g.add(Op::Proj(1), IrType::Memory, vec![start], None);
    let ctrl = g.add(Op::Proj(0), IrType::Control, vec![start], None);
    let init = g.add(Op::Param(0), IrType::Int, vec![], None);
    let v = g.add(Op::Param(1), IrType::Int, vec![], None);
    let cond = g.add(
        Op::Cmp(crate::ir::CmpOp::Ne),
        IrType::Int,
        vec![v, init],
        None,
    );
    let iff = g.add(Op::If, IrType::Control, vec![ctrl, cond], None);
    let taken = g.add(Op::Proj(0), IrType::Control, vec![iff], None);
    let put = |tag: u8, is_volatile: bool| Op::StoreStatic {
        class_id: 9,
        field_index: 3,
        type_tag: tag,
        is_volatile,
    };
    let st = g.add(put(b'I', false), IrType::Memory, vec![taken, mem, v], None);
    let shape = promoted_static_cell_shape_of(&g, st).expect("the diamond's store");
    assert_eq!(
        (shape.class_id, shape.field_index, shape.long),
        (9, 3, false)
    );
    assert_eq!((shape.value, shape.init), (v, init));
    let vol = g.add(put(b'I', true), IrType::Memory, vec![taken, mem, v], None);
    assert!(
        promoted_static_cell_shape_of(&g, vol).is_none(),
        "volatile: never"
    );
    let float = g.add(put(b'F', false), IrType::Memory, vec![taken, mem, v], None);
    assert!(
        promoted_static_cell_shape_of(&g, float).is_none(),
        "float: never"
    );
    let bare = g.add(put(b'I', false), IrType::Memory, vec![ctrl, mem, v], None);
    assert!(
        promoted_static_cell_shape_of(&g, bare).is_none(),
        "no diamond: no cell"
    );
}
