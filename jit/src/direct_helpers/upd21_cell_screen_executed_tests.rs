use super::*;
use std::sync::atomic::{AtomicU32, Ordering};

const DECLINED: u64 = u64::MAX;
const CLASS: u32 = 0x123;

/// A fake object: the class id at 0, the compact bit in the gc-flags byte.
#[repr(C, align(16))]
struct FakeObj([u8; 128]);

impl FakeObj {
    fn new(class_id: u32, compact: bool) -> Box<Self> {
        let mut o = Box::new(FakeObj([0; 128]));
        o.0[..4].copy_from_slice(&class_id.to_le_bytes());
        if compact {
            o.0[cratonvm_types::GC_FLAGS_BYTE_OFFSET] |= cratonvm_types::GC_FLAG_COMPACT;
        }
        o
    }

    fn addr(&self) -> u64 {
        self.0.as_ptr() as u64 // Cast: the fake object's address
    }
}

/// The screen as `extern "C" fn(object, offset) -> displacement`, `-1` on a
/// decline edge: RAX = arg0, R11 = arg1, the screen, `MOV RAX, R11 ; RET`.
fn build(screen: &(Vec<u8>, Vec<usize>)) -> crate::ExecutableBuffer {
    let (bytes, edges) = screen;
    let mut body: Vec<u8> = Vec::new();
    if cfg!(windows) {
        body.extend_from_slice(&[0x48, 0x89, 0xC8]); // MOV RAX, RCX
        body.extend_from_slice(&[0x49, 0x89, 0xD3]); // MOV R11, RDX
    } else {
        body.extend_from_slice(&[0x48, 0x89, 0xF8]); // MOV RAX, RDI
        body.extend_from_slice(&[0x49, 0x89, 0xF3]); // MOV R11, RSI
    }
    let at = body.len();
    body.extend_from_slice(bytes);
    body.extend_from_slice(&[0x4C, 0x89, 0xD8, 0xC3]); // MOV RAX, R11 ; RET
    let decline_at = body.len();
    // MOV RAX, -1 ; RET
    body.extend_from_slice(&[0x48, 0xC7, 0xC0, 0xFF, 0xFF, 0xFF, 0xFF, 0xC3]);
    for e in edges {
        let operand = at + e;
        let rel = i32::try_from(decline_at).expect("small")
            - i32::try_from(operand + 4).expect("small");
        body[operand..operand + 4].copy_from_slice(&rel.to_le_bytes());
    }
    let mut buf = crate::ExecutableBuffer::new(4096).expect("executable buffer");
    buf.emit(&body);
    assert!(!buf.overflowed());
    buf.finalize();
    buf
}

fn run(buf: &crate::ExecutableBuffer, object: u64, offset: u64) -> u64 {
    // SAFETY: a complete `extern "C" fn(u64, u64) -> u64` that only reads
    // the fake object and the fake table the caller keeps alive, and
    // writes RAX, RCX, RDX, R10 and R11 (caller-saved on both ABIs).
    let f = unsafe {
        std::mem::transmute::<*const u8, extern "C" fn(u64, u64) -> u64>(buf.as_ptr())
    };
    f(object, offset)
}

/// A fresh table of the layout epoch `epoch`: the epoch word, then the
/// int/long, reference and sub-word halves.
fn fresh_table(epoch: u32) -> Vec<u64> {
    let mut t = vec![0u64; 1 + 3 * UNSAFE_FIELD_CELL_COUNT];
    t[0] = u64::from(epoch);
    t
}

fn publish(t: &mut [u64], half: usize, class: u32, slot: usize, wide: bool, disp: usize) {
    let word = unsafe_field_cell_word(class, slot, wide, disp).expect("fits a word");
    t[1 + half + unsafe_field_cell_index(class, slot, wide)] = word;
}

/// A byte-offset VM's tagged screen runs the decode, then the probe: it
/// hits exactly the published `(class, slot, width)` at `16 + slot * 8`,
/// declines an offset inside a field, below the base or in the synthetic
/// band, a stale layout epoch, a legacy instance and another class, and
/// the int/long and reference halves never answer each other's screens.
#[test]
fn upd21_the_tagged_cell_screen_executes_decode_then_probe() {
    let count = UNSAFE_FIELD_CELL_COUNT;
    let epoch = AtomicU32::new(7);
    let epoch_addr = epoch.as_ptr() as usize; // Cast: the epoch cell's address
    let mut table = fresh_table(7);
    publish(&mut table, 0, CLASS, 3, false, 28);
    publish(&mut table, count, CLASS, 5, true, 64);
    publish(&mut table, 0, CLASS, 6, true, 80);
    // Cast: an address, tagged as a byte-offset VM's table.
    let tagged = table.as_ptr() as usize | UNSAFE_FIELD_CELLS_BYTE_OFFSETS_TAG;
    let int4 = build(&unsafe_inline_field_cell_bytes(tagged, epoch_addr, 4));
    let long8 = build(&unsafe_inline_field_cell_bytes(tagged, epoch_addr, 8));
    let refs = build(&unsafe_inline_ref_field_cell_bytes(tagged, epoch_addr));
    let obj = FakeObj::new(CLASS, true);
    let at = |slot: u64| 16 + slot * 8;
    assert_eq!(run(&int4, obj.addr(), at(3)), 28, "the published int field");
    assert_eq!(run(&int4, obj.addr(), at(3) + 4), DECLINED, "inside a field");
    assert_eq!(run(&int4, obj.addr(), at(3) + 1), DECLINED, "inside a field");
    assert_eq!(run(&int4, obj.addr(), at(4)), DECLINED, "an unpublished slot");
    assert_eq!(run(&int4, obj.addr(), 8), DECLINED, "below the base");
    assert_eq!(run(&int4, obj.addr(), 0), DECLINED, "offset zero");
    assert_eq!(run(&int4, obj.addr(), 0x2000_0000), DECLINED, "the synthetic band");
    assert_eq!(run(&int4, obj.addr(), u64::MAX - 7), DECLINED, "a negative offset");
    assert_eq!(run(&long8, obj.addr(), at(3)), DECLINED, "the wide key differs");
    // The halves are separate words: an int/long screen never reads a
    // reference cell and a reference screen never reads an int/long one.
    assert_eq!(run(&refs, obj.addr(), at(5)), 64, "the published reference field");
    assert_eq!(run(&long8, obj.addr(), at(5)), DECLINED, "a reference cell is not a long cell");
    assert_eq!(run(&refs, obj.addr(), at(3)), DECLINED, "an int cell is not a reference cell");
    assert_eq!(run(&long8, obj.addr(), at(6)), 80, "the published long field");
    assert_eq!(
        run(&refs, obj.addr(), at(6)),
        DECLINED,
        "a long cell has the reference key but not its half"
    );
    // Receiver shapes.
    let other = FakeObj::new(CLASS + 1, true);
    assert_eq!(run(&int4, other.addr(), at(3)), DECLINED, "another class");
    let legacy = FakeObj::new(CLASS, false);
    assert_eq!(run(&int4, legacy.addr(), at(3)), DECLINED, "a legacy instance");
    // A layout replacement moves the epoch: nothing the table holds hits.
    epoch.store(8, Ordering::Relaxed);
    assert_eq!(run(&int4, obj.addr(), at(3)), DECLINED, "a stale table");
    assert_eq!(run(&refs, obj.addr(), at(5)), DECLINED, "a stale table");
    epoch.store(7, Ordering::Relaxed);
    assert_eq!(run(&int4, obj.addr(), at(3)), 28);
}

/// An untagged table (slot offsets) keys the raw offset as the slot, with
/// no decode: slot 3 hits, and a byte offset is only another slot.
#[test]
fn upd21_the_untagged_cell_screen_keys_the_raw_offset() {
    let epoch = AtomicU32::new(7);
    let epoch_addr = epoch.as_ptr() as usize; // Cast: the epoch cell's address
    let mut table = fresh_table(7);
    publish(&mut table, 0, CLASS, 3, false, 28);
    let plain = table.as_ptr() as usize; // Cast: an address
    let int4 = build(&unsafe_inline_field_cell_bytes(plain, epoch_addr, 4));
    let obj = FakeObj::new(CLASS, true);
    assert_eq!(run(&int4, obj.addr(), 3), 28);
    assert_eq!(run(&int4, obj.addr(), 16 + 3 * 8), DECLINED, "a byte offset is another slot");
    assert_eq!(run(&int4, obj.addr(), 4), DECLINED);
    assert_eq!(run(&int4, obj.addr(), 0x8000), DECLINED, "past the key");
}

/// On a two-way table a key whose first cell another field owns is served
/// from its second cell, and the owner keeps the first.
#[test]
fn upd21_a_second_cell_serves_a_key_whose_first_cell_is_owned() {
    if !unsafe_field_cell_two_way_enabled() {
        return;
    }
    // `(0x125, slot 0, int)` indexes `0x125 ^ 0`, as `(0x123, slot 3, int)`
    // does (`0x123 ^ 6`): the same first cell, 0x25.
    const OWNER: u32 = 0x125;
    let first = unsafe_field_cell_index(CLASS, 3, false);
    assert_eq!(first, unsafe_field_cell_index(OWNER, 0, false));
    let second = unsafe_field_cell_index2(CLASS, 3, false);
    if second == first {
        return; // the hash puts both in one cell: nothing to pin
    }
    let epoch = AtomicU32::new(7);
    let epoch_addr = epoch.as_ptr() as usize; // Cast: the epoch cell's address
    let mut table = fresh_table(7);
    publish(&mut table, 0, OWNER, 0, false, 20);
    table[1 + second] = unsafe_field_cell_word(CLASS, 3, false, 28).expect("fits a word");
    // Cast: an address, tagged as a byte-offset VM's table.
    let tagged = table.as_ptr() as usize | UNSAFE_FIELD_CELLS_BYTE_OFFSETS_TAG;
    let int4 = build(&unsafe_inline_field_cell_bytes(tagged, epoch_addr, 4));
    let key = FakeObj::new(CLASS, true);
    let owner = FakeObj::new(OWNER, true);
    assert_eq!(run(&int4, key.addr(), 16 + 3 * 8), 28, "the second cell");
    assert_eq!(run(&int4, owner.addr(), 16), 20, "the first owner keeps its cell");
    assert_eq!(run(&int4, key.addr(), 16 + 4 * 8), DECLINED, "an unpublished slot");
}
