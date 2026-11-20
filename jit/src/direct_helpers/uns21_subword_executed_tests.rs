use super::*;
use std::sync::atomic::{AtomicU16, AtomicU32, AtomicU8, Ordering};

const DECLINED: u64 = u64::MAX;
const CLASS: u32 = 0x321;

fn finish(body: &[u8]) -> crate::ExecutableBuffer {
    let mut buf = crate::ExecutableBuffer::new(4096).expect("executable buffer");
    buf.emit(body);
    assert!(!buf.overflowed());
    buf.finalize();
    buf
}

/// `extern "C" fn(base, disp, expected, new) -> RAX`: RAX = base, R11 =
/// disp, R10 = expected, RDX = new (the prefix's entry contract), the CAS
/// bytes, `RET`.
fn build_rmw(rmw: UnsafeSubwordRmw) -> crate::ExecutableBuffer {
    let mut body: Vec<u8> = Vec::new();
    if cfg!(windows) {
        body.extend_from_slice(&[0x48, 0x89, 0xC8]); // MOV RAX, RCX
        body.extend_from_slice(&[0x49, 0x89, 0xD3]); // MOV R11, RDX
        body.extend_from_slice(&[0x4D, 0x89, 0xC2]); // MOV R10, R8
        body.extend_from_slice(&[0x4C, 0x89, 0xCA]); // MOV RDX, R9
    } else {
        body.extend_from_slice(&[0x48, 0x89, 0xF8]); // MOV RAX, RDI
        body.extend_from_slice(&[0x49, 0x89, 0xF3]); // MOV R11, RSI
        body.extend_from_slice(&[0x49, 0x89, 0xD2]); // MOV R10, RDX
        body.extend_from_slice(&[0x48, 0x89, 0xCA]); // MOV RDX, RCX
    }
    body.extend_from_slice(&unsafe_inline_subword_rmw_bytes(rmw));
    body.push(0xC3); // RET
    finish(&body)
}

type RmwFn = extern "C" fn(u64, u64, u64, u64) -> u64;

fn rmw_fn(buf: &crate::ExecutableBuffer) -> RmwFn {
    // SAFETY: a complete `extern "C" fn(u64, u64, u64, u64) -> u64` that
    // only touches the fake memory the caller keeps alive and writes RAX,
    // R10 and the flags (caller-saved on both ABIs).
    unsafe { std::mem::transmute::<*const u8, RmwFn>(buf.as_ptr()) }
}

/// Eight bytes of fake field storage, 8-aligned like a compact payload.
#[repr(C, align(8))]
struct Mem([AtomicU8; 8]);

impl Mem {
    fn new(bytes: [u8; 8]) -> Box<Self> {
        Box::new(Mem(bytes.map(AtomicU8::new)))
    }

    fn base(&self) -> u64 {
        self.0.as_ptr() as u64 // Cast: the fake storage's address
    }

    fn bytes(&self) -> [u8; 8] {
        std::array::from_fn(|i| self.0[i].load(Ordering::SeqCst))
    }

    fn short_at(&self, at: usize) -> &AtomicU16 {
        assert!(at % 2 == 0 && at + 2 <= 8);
        // SAFETY: a 2-aligned, in-bounds pair of the fake storage, only
        // ever accessed atomically by the tests.
        unsafe { &*(self.0.as_ptr().add(at) as *const AtomicU16) }
    }
}

/// The byte rows: a compare of AL only, a store of DL only, the neighbours
/// untouched, `compareAndSet` answers exactly `0` / `1`, and
/// `compareAndExchange` answers the witness sign-extended on both outcomes.
#[test]
fn uns21_the_byte_cas_bytes_compare_and_store_one_byte() {
    let cas = build_rmw(UnsafeSubwordRmw {
        width: 1,
        cae: false,
    });
    let cae = build_rmw(UnsafeSubwordRmw {
        width: 1,
        cae: true,
    });
    let (cas, cae) = (rmw_fn(&cas), rmw_fn(&cae));
    let m = Mem::new([0x11, 0x22, 0x33, 0x80, 0x55, 0x66, 0x77, 0x88]);
    // Success: only byte 1 changes; the upper bits of both operands are junk.
    assert_eq!(cas(m.base(), 1, 0xABCD_EF22, 0x1234_56AA), 1);
    assert_eq!(m.bytes(), [0x11, 0xAA, 0x33, 0x80, 0x55, 0x66, 0x77, 0x88]);
    // Failure: answers 0 and writes nothing.
    assert_eq!(cas(m.base(), 1, 0x22, 0x00), 0);
    assert_eq!(m.bytes(), [0x11, 0xAA, 0x33, 0x80, 0x55, 0x66, 0x77, 0x88]);
    // compareAndExchange, success: the witness is `expected`, sign-extended.
    assert_eq!(cae(m.base(), 3, 0x80, 0x01), 0xFFFF_FFFF_FFFF_FF80);
    assert_eq!(m.bytes(), [0x11, 0xAA, 0x33, 0x01, 0x55, 0x66, 0x77, 0x88]);
    // compareAndExchange, failure: the witness is the cell's, written nowhere.
    assert_eq!(cae(m.base(), 1, 0x01, 0x02), 0xFFFF_FFFF_FFFF_FFAA);
    assert_eq!(m.bytes(), [0x11, 0xAA, 0x33, 0x01, 0x55, 0x66, 0x77, 0x88]);
    // A positive witness is not sign-extended into the high bits.
    assert_eq!(cae(m.base(), 0, 0x11, 0x12), 0x11);
    assert_eq!(m.bytes()[0], 0x12);
}

/// The short rows: a 16-bit compare and store (`LOCK CMPXCHG [..], DX`),
/// the neighbouring bytes on each side untouched, the witness
/// sign-extended from 16 bits.
#[test]
fn uns21_the_short_cas_bytes_compare_and_store_two_bytes() {
    let cas = build_rmw(UnsafeSubwordRmw {
        width: 2,
        cae: false,
    });
    let cae = build_rmw(UnsafeSubwordRmw {
        width: 2,
        cae: true,
    });
    let (cas, cae) = (rmw_fn(&cas), rmw_fn(&cae));
    let m = Mem::new([0xA0, 0xA1, 0x01, 0x80, 0xB0, 0xB1, 0xB2, 0xB3]);
    assert_eq!(m.short_at(2).load(Ordering::SeqCst), 0x8001);
    // Success: expected / new are compared and stored as 16 bits only.
    assert_eq!(cas(m.base(), 2, 0xDEAD_8001, 0xBEEF_7FFE), 1);
    assert_eq!(m.bytes(), [0xA0, 0xA1, 0xFE, 0x7F, 0xB0, 0xB1, 0xB2, 0xB3]);
    // A mismatch in the HIGH byte alone fails (the compare is 16 bits wide).
    assert_eq!(cas(m.base(), 2, 0x00FE, 0x1111), 0);
    assert_eq!(m.bytes(), [0xA0, 0xA1, 0xFE, 0x7F, 0xB0, 0xB1, 0xB2, 0xB3]);
    // A mismatch in the LOW byte alone fails.
    assert_eq!(cas(m.base(), 2, 0x7FFF, 0x1111), 0);
    assert_eq!(m.bytes(), [0xA0, 0xA1, 0xFE, 0x7F, 0xB0, 0xB1, 0xB2, 0xB3]);
    // compareAndExchange: success answers `expected` sign-extended from
    // 16 bits, failure the cell's value, the same way.
    assert_eq!(cae(m.base(), 2, 0x7FFE, 0x8000), 0x7FFE);
    assert_eq!(m.short_at(2).load(Ordering::SeqCst), 0x8000);
    assert_eq!(cae(m.base(), 2, 0x0001, 0x0002), 0xFFFF_FFFF_FFFF_8000);
    assert_eq!(m.short_at(2).load(Ordering::SeqCst), 0x8000);
    assert_eq!(m.bytes(), [0xA0, 0xA1, 0x00, 0x80, 0xB0, 0xB1, 0xB2, 0xB3]);
}

/// The instruction is a LOCKED one: two threads CAS-incrementing a short
/// and two CAS-incrementing the byte beside it, all inside one 4-byte
/// word, never lose an update (a missing `LOCK` prefix, or a compare wider
/// than the lane, would).
#[test]
fn uns21_subword_cas_is_atomic_against_the_adjacent_lane() {
    const N: u64 = 20_000;
    let cae_s = build_rmw(UnsafeSubwordRmw {
        width: 2,
        cae: true,
    });
    let cae_b = build_rmw(UnsafeSubwordRmw {
        width: 1,
        cae: true,
    });
    let (fs, fb) = (cae_s.as_ptr() as usize, cae_b.as_ptr() as usize); // Cast: entry points
    let m = Mem::new([0; 8]);
    let base = m.base();
    let m_ref = &*m;
    std::thread::scope(|scope| {
        for _ in 0..2 {
            scope.spawn(move || {
                // SAFETY: the entry point of a live executable buffer.
                let f = unsafe { std::mem::transmute::<usize, RmwFn>(fs) };
                for _ in 0..N {
                    loop {
                        let cur = u64::from(m_ref.short_at(2).load(Ordering::SeqCst));
                        // The witness, sign-extended; its low 16 bits are the cell's.
                        let seen = f(base, 2, cur, (cur + 1) & 0xFFFF) & 0xFFFF;
                        if seen == cur {
                            break;
                        }
                    }
                }
            });
            scope.spawn(move || {
                // SAFETY: as above.
                let f = unsafe { std::mem::transmute::<usize, RmwFn>(fb) };
                for _ in 0..N {
                    loop {
                        let cur = u64::from(m_ref.0[1].load(Ordering::SeqCst));
                        let seen = f(base, 1, cur, (cur + 1) & 0xFF) & 0xFF;
                        if seen == cur {
                            break;
                        }
                    }
                }
            });
        }
    });
    assert_eq!(u64::from(m.short_at(2).load(Ordering::SeqCst)), 2 * N);
    assert_eq!(u64::from(m.0[1].load(Ordering::SeqCst)), (2 * N) & 0xFF);
    assert_eq!(
        m.0[0].load(Ordering::SeqCst),
        0,
        "a lane below is untouched"
    );
    assert_eq!(
        m.0[4].load(Ordering::SeqCst),
        0,
        "a lane above is untouched"
    );
}

/// The screen as `extern "C" fn(object, offset) -> displacement`, `-1` on a
/// decline edge (the harness of `upd21_cell_screen_executed_tests`).
fn build_screen(screen: &(Vec<u8>, Vec<usize>)) -> crate::ExecutableBuffer {
    let (bytes, edges) = screen;
    let mut body: Vec<u8> = Vec::new();
    if cfg!(windows) {
        body.extend_from_slice(&[0x48, 0x89, 0xC8, 0x49, 0x89, 0xD3]);
    } else {
        body.extend_from_slice(&[0x48, 0x89, 0xF8, 0x49, 0x89, 0xF3]);
    }
    let at = body.len();
    body.extend_from_slice(bytes);
    body.extend_from_slice(&[0x4C, 0x89, 0xD8, 0xC3]); // MOV RAX, R11 ; RET
    let decline_at = body.len();
    body.extend_from_slice(&[0x48, 0xC7, 0xC0, 0xFF, 0xFF, 0xFF, 0xFF, 0xC3]);
    for e in edges {
        let operand = at + e;
        let rel = i32::try_from(decline_at).expect("small")
            - i32::try_from(operand + 4).expect("small");
        body[operand..operand + 4].copy_from_slice(&rel.to_le_bytes());
    }
    finish(&body)
}

fn run_screen(buf: &crate::ExecutableBuffer, object: u64, offset: u64) -> u64 {
    // SAFETY: a complete `extern "C" fn(u64, u64) -> u64` that only reads
    // the fake object and table the caller keeps alive.
    let f = unsafe {
        std::mem::transmute::<*const u8, extern "C" fn(u64, u64) -> u64>(buf.as_ptr())
    };
    f(object, offset)
}

#[repr(C, align(16))]
struct FakeObj([u8; 128]);

fn fake_obj(class_id: u32, compact: bool) -> Box<FakeObj> {
    let mut o = Box::new(FakeObj([0; 128]));
    o.0[..4].copy_from_slice(&class_id.to_le_bytes());
    if compact {
        o.0[cratonvm_types::GC_FLAGS_BYTE_OFFSET] |= cratonvm_types::GC_FLAG_COMPACT;
    }
    o
}

fn addr_of(o: &FakeObj) -> u64 {
    o.0.as_ptr() as u64 // Cast: the fake object's address
}

/// The sub-word half is its own word range: a `byte` access matches only a
/// 1-byte cell, a `short` access only a 2-byte one (the key's low bit), and
/// neither screen reads the int/long half or the reference half.
#[test]
fn uns21_the_subword_screen_reads_its_half_and_keys_the_width() {
    let count = UNSAFE_FIELD_CELL_COUNT;
    let epoch = AtomicU32::new(9);
    let epoch_addr = epoch.as_ptr() as usize; // Cast: the epoch cell's address
    let mut table = vec![0u64; 1 + 3 * count];
    table[0] = 9;
    let put = |t: &mut Vec<u64>, half: usize, slot: usize, wide: bool, disp: usize| {
        let w = unsafe_field_cell_word(CLASS, slot, wide, disp).expect("fits a word");
        t[1 + half + unsafe_field_cell_index(CLASS, slot, wide)] = w;
    };
    // A `byte` field at slot 3 and a `short` at slot 4 in the sub-word
    // half; an int cell and a reference cell in the other halves.
    put(&mut table, 2 * count, 3, false, 28);
    put(&mut table, 2 * count, 4, true, 30);
    put(&mut table, 0, 5, false, 40);
    put(&mut table, count, 6, true, 48);
    // Cast: an address, tagged as a byte-offset VM's table.
    let tagged = table.as_ptr() as usize | UNSAFE_FIELD_CELLS_BYTE_OFFSETS_TAG;
    let byte = build_screen(&unsafe_inline_subword_field_cell_bytes(
        tagged, epoch_addr, 1,
    ));
    let short = build_screen(&unsafe_inline_subword_field_cell_bytes(
        tagged, epoch_addr, 2,
    ));
    let obj = fake_obj(CLASS, true);
    let addr = addr_of(&obj);
    let at = |slot: u64| 16 + slot * 8;
    assert_eq!(
        run_screen(&byte, addr, at(3)),
        28,
        "the published byte field"
    );
    assert_eq!(
        run_screen(&short, addr, at(4)),
        30,
        "the published short field"
    );
    assert_eq!(
        run_screen(&short, addr, at(3)),
        DECLINED,
        "a short access to a byte cell"
    );
    assert_eq!(
        run_screen(&byte, addr, at(4)),
        DECLINED,
        "a byte access to a short cell"
    );
    assert_eq!(
        run_screen(&byte, addr, at(5)),
        DECLINED,
        "an int cell is not a sub-word cell"
    );
    assert_eq!(run_screen(&short, addr, at(5)), DECLINED);
    assert_eq!(
        run_screen(&short, addr, at(6)),
        DECLINED,
        "nor is a reference cell"
    );
    assert_eq!(
        run_screen(&byte, addr, at(3) + 1),
        DECLINED,
        "inside a field"
    );
    assert_eq!(
        run_screen(&byte, addr, 3),
        DECLINED,
        "a slot index under byte offsets"
    );
    let other = fake_obj(CLASS + 1, true);
    assert_eq!(
        run_screen(&byte, addr_of(&other), at(3)),
        DECLINED,
        "another class"
    );
    let legacy = fake_obj(CLASS, false);
    assert_eq!(
        run_screen(&byte, addr_of(&legacy), at(3)),
        DECLINED,
        "a legacy instance"
    );
    epoch.store(10, Ordering::Relaxed);
    assert_eq!(
        run_screen(&byte, addr, at(3)),
        DECLINED,
        "a stale layout epoch"
    );
}
