use super::*;

/// Run `body` as a leaf `extern "C" fn(a0, a1, a2) -> i64` snippet. On
/// entry RAX = `a0`, R8 = `a1`, RDX = ECX = `a2`; the body leaves its
/// answer in RAX.
fn run(body: impl Fn(&mut Compiler), a0: i64, a1: i64, a2: i64) -> i64 {
    let mut c = crate::x64::flag_and_header_contracts::bounds_check_test_compiler();
    let entry_off = c.buf.pos();
    // Arguments first into registers no other argument lives in.
    c.emit_mov_r64_r64(R11, ARG_REGS[2]);
    c.emit_mov_r64_r64(R10, ARG_REGS[1]);
    c.emit_mov_r64_r64(RAX, ARG_REGS[0]);
    c.emit_mov_r64_r64(R8, R10);
    c.emit_mov_r64_r64(RDX, R11);
    c.emit_mov_r64_r64(RCX, R11);
    body(&mut c);
    c.buf.emit(&[0xC3]); // ret
    assert!(!c.buf.overflowed());
    crate::platform::make_executable(c.buf.as_ptr() as *mut u8, c.buf.capacity())
        .expect("the test buffer must be flippable to RX");
    // SAFETY: `entry_off` is a byte offset inside the same allocation.
    let entry = unsafe { c.buf.as_ptr().add(entry_off) };
    // SAFETY: a leaf snippet that touches only the caller's buffers named
    // by its arguments (alive across the call) and caller-saved registers.
    let f: extern "C" fn(i64, i64, i64) -> i64 = unsafe { std::mem::transmute(entry) };
    let r = f(a0, a1, a2);
    drop(c);
    r
}

/// The decimal text the emitted renderer writes for `v` (`int_kind`:
/// sign-extend the low 32 bits first, as the `int` operand does).
fn render(v: i64, int_kind: bool) -> String {
    let mut buf = [0xAAu8; 40];
    let base = buf.as_mut_ptr();
    // Cast: the buffer's address as the snippet's i64 argument.
    let end = run(
        move |c| {
            if int_kind {
                c.buf.emit(&[0x48, 0x63, 0xC0]); // MOVSXD RAX,EAX
            }
            ce::emit_decimal(&mut c.buf);
            c.emit_mov_r64_r64(RAX, R8);
        },
        v,
        base as i64,
        0,
    );
    // Cast: the end pointer back to an offset into `buf`.
    let n = (end as usize) - (base as usize);
    assert!(n <= 20, "{v}: {n} bytes");
    assert!(
        buf[n..].iter().all(|&b| b == 0xAA),
        "{v}: wrote past its digits"
    );
    String::from_utf8(buf[..n].to_vec()).expect("ASCII")
}

#[test]
fn longs_render_as_long_to_string() {
    let mut values = vec![
        0i64,
        1,
        -1,
        9,
        10,
        11,
        99,
        100,
        -100,
        i64::MAX,
        i64::MIN,
        i64::MIN + 1,
        999_999_999_999_999_999,
        1_000_000_000_000_000_000,
        -1_000_000_000_000_000_000,
        4_294_967_296,
        -2_147_483_649,
    ];
    let mut p = 1i64;
    for _ in 0..18 {
        p *= 10;
        values.extend([p - 1, p, p + 1, -p]);
    }
    for v in values {
        assert_eq!(render(v, false), v.to_string());
    }
}

#[test]
fn ints_render_as_integer_to_string_from_the_low_word() {
    for v in [
        0i32,
        7,
        -7,
        10,
        -10,
        i32::MAX,
        i32::MIN,
        i32::MIN + 1,
        999_999_999,
        1_000_000_000,
        -1_000_000_000,
        123_456_789,
    ] {
        // Garbage in the high half: the operand is the low 32 bits.
        // Cast: the int's bits, zero-extended under a garbage high word.
        let raw = (0x5A5A_5A5Ai64 << 32) | i64::from(v as u32);
        assert_eq!(render(raw, true), v.to_string());
    }
}

#[test]
fn the_length_pass_digit_count_matches_the_rendering() {
    for v in [
        0u64,
        9,
        10,
        99,
        100,
        1 << 31,
        u64::from(u32::MAX),
        1 << 63,
        9_999_999_999_999_999_999,
    ] {
        // R8 starts at 5: the count is added to what is there.
        // Cast: the magnitude's bits as the i64 argument.
        let got = run(
            |c| {
                c.emit_mov_imm32_sx(R8, 5);
                ce::emit_digit_count(&mut c.buf, R8);
                c.emit_mov_r64_r64(RAX, R8);
            },
            v as i64,
            0,
            0,
        );
        // Cast: a digit count.
        assert_eq!(got, 5 + v.to_string().len() as i64, "{v}");
    }
}

#[test]
fn literals_are_stored_exactly() {
    for len in 0..=40usize {
        // Cast: `i % 26` fits a byte.
        let text: Vec<u8> = (0..len).map(|i| b'a' + (i % 26) as u8).collect();
        let mut buf = [0xAAu8; 64];
        let base = buf.as_mut_ptr();
        let t = text.clone();
        // Cast: the buffer's address as the snippet's i64 argument.
        let end = run(
            move |c| {
                ce::emit_literal(&mut c.buf, &t);
                c.emit_mov_r64_r64(RAX, R8);
            },
            0,
            base as i64,
            0,
        );
        // Cast: the end pointer back to an offset into `buf`.
        assert_eq!((end as usize) - (base as usize), len);
        assert_eq!(&buf[..len], text.as_slice());
        assert!(
            buf[len..].iter().all(|&b| b == 0xAA),
            "len {len}: wrote past"
        );
    }
}

#[test]
fn the_copy_moves_exactly_count_bytes() {
    let src: Vec<u8> = (0..64u8)
        .map(|b| b.wrapping_mul(7).wrapping_add(1))
        .collect();
    for count in 0..=40usize {
        let mut dst = [0xAAu8; 64];
        let d = dst.as_mut_ptr();
        let s = src.as_ptr();
        // RDX = src, ECX = count, R8 = dst.
        // Cast: addresses and the count as the snippet's i64 arguments.
        let end = run(
            move |c| {
                c.emit_mov_imm64_full(RDX, s as i64);
                ce::emit_copy(&mut c.buf);
                c.emit_mov_r64_r64(RAX, R8);
            },
            0,
            d as i64,
            count as i64,
        );
        // Cast: the end pointer back to an offset into `dst`.
        assert_eq!((end as usize) - (d as usize), count);
        assert_eq!(&dst[..count], &src[..count]);
        assert!(
            dst[count..].iter().all(|&b| b == 0xAA),
            "count {count}: wrote past"
        );
    }
}

/// A fake well-known reader: for the null literal
/// slot: the context word IS the slot's address (the test puts the fake
/// literal there), any other query answers 0.
///
/// # Safety
///
/// None beyond the `extern "C"` ABI: it reads nothing and answers the context word or 0.
unsafe extern "C" fn fake_well_known(ctx: i64, which: i64) -> i64 {
    if which == crate::direct_helpers::WELL_KNOWN_NULL_LITERAL_SLOT {
        ctx
    } else {
        0
    }
}

