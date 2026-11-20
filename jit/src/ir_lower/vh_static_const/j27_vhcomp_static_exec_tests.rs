use super::*;
use crate::direct_helpers::{UnsafeBitOp, UnsafeRmwOp};

/// The answer of the decline edge.
const DECLINED: i64 = 0x7EAD_0000_0000_0001;

/// `MOV RDX, arg0 ; MOV R10, arg1` for this platform's C ABI.
#[cfg(windows)]
const PROLOGUE: [u8; 6] = [0x4C, 0x8B, 0xD2, 0x48, 0x8B, 0xD1]; // MOV R10, RDX ; MOV RDX, RCX
#[cfg(not(windows))]
const PROLOGUE: [u8; 6] = [0x4C, 0x8B, 0xD6, 0x48, 0x8B, 0xD7]; // MOV R10, RSI ; MOV RDX, RDI

/// `extern "C" fn(value, expected) -> i64` running one site of `op` /
/// `kind` against `cell`, [`DECLINED`] on a decline.
fn build(cell: &VhStaticCell, op: VhConstOp, kind: u8) -> crate::ExecutableBuffer {
    let (screen, edges) = vh_static_screen_bytes(cell as *const VhStaticCell as usize, kind);
    let mut body: Vec<u8> = PROLOGUE.to_vec();
    let at = body.len();
    body.extend_from_slice(&screen);
    body.extend_from_slice(&vh_static_access_bytes(op, kind));
    body.push(0xC3); // RET
    let decline = body.len();
    body.extend_from_slice(&[0x48, 0xB8]); // MOV RAX, imm64
    body.extend_from_slice(&DECLINED.to_le_bytes());
    body.push(0xC3);
    for e in edges {
        let field = at + e;
        let rel = i32::try_from(decline).expect("small") - i32::try_from(field + 4).expect("small");
        body[field..field + 4].copy_from_slice(&rel.to_le_bytes());
    }
    let mut buf = crate::ExecutableBuffer::new(512).expect("executable buffer");
    buf.emit(&body);
    assert!(!buf.overflowed());
    buf.finalize();
    buf
}

/// A statics block of four 16-byte cells (`[tag u32, payload ...]`) and its
/// base cell.
struct Statics {
    block: Vec<u64>,
    base: Box<usize>,
}

impl Statics {
    fn new() -> Self {
        let block = vec![0u64; 8];
        let base = Box::new(block.as_ptr() as usize); // Cast: the block's address
        Self { block, base }
    }
    fn base_cell(&self) -> usize {
        &*self.base as *const usize as usize // Cast: the cell's address
    }
    fn set_int(&mut self, index: usize, v: i32) {
        // Cast: tag 0 in the low half, the payload in the high half.
        self.block[index * 2] = u64::from(UNSAFE_STATIC_TAG_INT) | (u64::from(v as u32) << 32);
    }
    fn int(&self, index: usize) -> i32 {
        (self.block[index * 2] >> 32) as i32 // Cast: the payload half
    }
    fn set_long(&mut self, index: usize, v: i64) {
        self.block[index * 2] = u64::from(UNSAFE_STATIC_TAG_LONG);
        self.block[index * 2 + 1] = v as u64; // Cast: bit pattern
    }
    fn long(&self, index: usize) -> i64 {
        self.block[index * 2 + 1] as i64 // Cast: bit pattern
    }
}

fn call(buf: &crate::ExecutableBuffer, value: i64, expected: i64) -> i64 {
    // SAFETY: a complete `extern "C" fn(i64, i64) -> i64` that reads and
    // writes only the test's cell and block (alive across the call) and
    // scratch registers.
    let f = unsafe {
        std::mem::transmute::<*const u8, extern "C" fn(i64, i64) -> i64>(buf.as_ptr())
    };
    f(value, expected)
}

#[test]
fn an_unfilled_cell_declines_and_a_filled_one_adds_in_place() {
    let mut s = Statics::new();
    s.set_int(2, 10);
    let cell = VhStaticCell::new();
    let xadd = build(&cell, VhConstOp::Rmw(UnsafeRmwOp::GetAndAdd), b'I');
    assert_eq!(call(&xadd, 5, 0), DECLINED, "not filed yet");
    assert!(cell.fill(s.base_cell(), 2, b'I'));
    assert_eq!(call(&xadd, 5, 0), 10, "the previous value");
    assert_eq!(s.int(2), 15);
    assert_eq!(call(&xadd, -20, 0), 15);
    assert_eq!(s.int(2), -5, "wrapping, sign-extended next time");
    assert_eq!(call(&xadd, 0, 0), -5);
    cell.clear();
    assert_eq!(call(&xadd, 1, 0), DECLINED, "an unload cleared it");
    assert_eq!(s.int(2), -5);
}

#[test]
fn kind_tag_and_block_screens_decline() {
    let mut s = Statics::new();
    s.set_long(1, 7);
    let cell = VhStaticCell::new();
    assert!(cell.fill(s.base_cell(), 1, b'J'));
    let int_site = build(&cell, VhConstOp::Rmw(UnsafeRmwOp::GetAndAdd), b'I');
    assert_eq!(call(&int_site, 1, 0), DECLINED, "an int site on a long static");
    let long_site = build(&cell, VhConstOp::Rmw(UnsafeRmwOp::GetAndAdd), b'J');
    assert_eq!(call(&long_site, 1 << 40, 0), 7);
    assert_eq!(s.long(1), 7 + (1 << 40));
    s.set_int(1, 3); // the cell's tag is no longer Long
    assert_eq!(call(&long_site, 1, 0), DECLINED, "the tag guard");
    s.set_long(1, 3);
    *s.base = 0; // no statics block
    assert_eq!(call(&long_site, 1, 0), DECLINED, "a null block");
}

#[test]
fn cas_cae_xchg_bitwise_get_and_set_answer_as_the_natives() {
    let mut s = Statics::new();
    s.set_int(0, 15);
    let cell = VhStaticCell::new();
    assert!(cell.fill(s.base_cell(), 0, b'I'));
    let site = |op| build(&cell, op, b'I');
    let cas = site(VhConstOp::Rmw(UnsafeRmwOp::Cas));
    assert_eq!(call(&cas, 7, 14), 0, "a failed CAS");
    assert_eq!(s.int(0), 15);
    assert_eq!(call(&cas, 7, 15), 1);
    assert_eq!(s.int(0), 7);
    let cae = site(VhConstOp::Rmw(UnsafeRmwOp::Cae));
    assert_eq!(call(&cae, 3, 7), 7, "the witness");
    assert_eq!(s.int(0), 3);
    let xchg = site(VhConstOp::Rmw(UnsafeRmwOp::GetAndSet));
    assert_eq!(call(&xchg, 81000, 0), 3);
    let or = site(VhConstOp::Rmw(UnsafeRmwOp::GetAndBitwise(UnsafeBitOp::Or)));
    s.set_int(0, 7);
    assert_eq!(call(&or, 8, 0), 7);
    assert_eq!(s.int(0), 15);
    let get = site(VhConstOp::Get);
    s.set_int(0, -2);
    assert_eq!(call(&get, 0, 0), -2, "sign-extended");
    let set = site(VhConstOp::Set { volatile: true });
    call(&set, 42, 0);
    assert_eq!(s.int(0), 42);
    let long = VhStaticCell::new();
    s.set_long(3, -1);
    assert!(long.fill(s.base_cell(), 3, b'J'));
    let set = build(&long, VhConstOp::Set { volatile: false }, b'J');
    call(&set, 5, 0);
    assert_eq!(s.long(3), 5);
    let get = build(&long, VhConstOp::Get, b'J');
    assert_eq!(call(&get, 0, 0), 5);
}
