use super::*;
use crate::test_utils::mock_ctx;
#[allow(unused_imports)]
use cratonvm_native_api::{
    NativeClassAccess, NativeExceptionAccess, NativeGpuAccess, NativeHeapAccess,
    NativeInvokeAccess, NativeSystemAccess, NativeThreadAccess,
};
use cratonvm_types::Value;

fn alloc_int_arr(ctx: &mut dyn NativeContext, vals: &[i32]) -> ObjectRef {
    let arr = ctx.new_array(ArrayElementType::Int, vals.len());
    for (i, v) in vals.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(*v));
    }
    arr
}

fn read_arr(ctx: &dyn NativeContext, arr: ObjectRef) -> Vec<i32> {
    let n = ctx.array_length(arr);
    (0..n)
        .map(|i| match ctx.get_array_element(arr, i) {
            Value::Int(v) => v,
            _ => panic!("expected Int"),
        })
        .collect()
}

/// Naive schoolbook multiplication of two big-endian magnitudes
/// represented as `int[]`. Used as ground truth in tests. The JDK's
/// algorithm yields exactly this output for the squaring case
/// (`y == x`).
///
/// This is an O(n^2) reference implementation — independent code path
/// from the implSquareToLen we're testing, so any algorithmic drift
/// in either is caught.
fn naive_mul(x: &[i32], y: &[i32]) -> Vec<i32> {
    let xn = x.len();
    let yn = y.len();
    let n = xn + yn;
    // out[0] is the most-significant limb (big-endian).
    let mut out = vec![0u32; n];
    for i in (0..xn).rev() {
        let xi = x[i] as u32 as u64;
        let mut carry: u64 = 0;
        for j in (0..yn).rev() {
            let pos = i + j + 1;
            let p = xi * (y[j] as u32 as u64) + (out[pos] as u64) + carry;
            out[pos] = p as u32;
            carry = p >> 32;
        }
        // Propagate carry into out[i] and possibly beyond.
        let mut k = i;
        let mut c = carry;
        while c != 0 {
            let p = (out[k] as u64) + c;
            out[k] = p as u32;
            c = p >> 32;
            if k == 0 {
                break;
            }
            k -= 1;
        }
    }
    out.into_iter().map(|w| w as i32).collect()
}

fn ref_square(x: &[i32]) -> Vec<i32> {
    naive_mul(x, x)
}

/// Interpret a big-endian `int[]` magnitude as a `u128`. Only used by the
/// Montgomery tests below, which deliberately stay at two 32-bit limbs so
/// every intermediate fits and the reference arithmetic is plain `u128` —
/// an independent code path from everything under test.
fn as_u128(v: &[i32]) -> u128 {
    let mut acc: u128 = 0;
    for &w in v {
        acc = (acc << 32) | (w as u32) as u128;
    }
    acc
}

/// `-m^-1 mod 2^32` for the low word of a big-endian magnitude — the `inv`
/// that `montReduce` consumes, by Newton iteration (the same shape as
/// `MutableBigInteger.inverseMod64`, one word narrower).
fn neg_inv32(m: &[i32]) -> i32 {
    let m0 = m[m.len() - 1] as u32;
    assert_eq!(m0 & 1, 1, "Montgomery needs an odd modulus");
    let mut t: u32 = 1;
    for _ in 0..5 {
        t = t.wrapping_mul(2u32.wrapping_sub(m0.wrapping_mul(t)));
    }
    assert_eq!(m0.wrapping_mul(t), 1);
    (t as i32).wrapping_neg()
}

// --- implMultiplyToLen ---------------------------------------------

#[test]
fn impl_multiply_to_len_matches_the_naive_reference() {
    let cases: &[(&[i32], &[i32])] = &[
        (&[0x1234_5678], &[0x7FFF_FFFF]),
        (&[-1], &[-1]),
        (&[0, 0], &[0x1234_5678, -1, 5]),
        (
            &[0x7FFF_FFFF, -1, -1, -2],
            &[0x1234_5678, -1698898192, 0x0F0F_0F0F, -252645136, 1],
        ),
    ];
    for (x, y) in cases {
        let mut z = vec![0i32; x.len() + y.len()];
        impl_multiply_to_len(x, x.len() as i32, y, y.len() as i32, &mut z).unwrap();
        assert_eq!(z, naive_mul(x, y), "x={x:?} y={y:?}");
    }
}

#[test]
fn impl_multiply_to_len_leaves_words_above_the_product_untouched() {
    // `montReduce` indexes from `z.len()`, so a wide buffer's tail is
    // load-bearing, not slack. MEASURED on HotSpot 25.0.3+9 as the
    // `MTL widez` row of `L2IntrinsicProbe.java`.
    let x: &[i32] = &[0x7FFF_FFFF, -1, -1, -2];
    let y: &[i32] = &[0x1234_5678, -1698898192, 0x0F0F_0F0F, -252645136, 1];
    let mut z = vec![0x5A5A_5A5Au32 as i32; 12];
    impl_multiply_to_len(x, 4, y, 5, &mut z).unwrap();
    assert_eq!(&z[..9], &naive_mul(x, y)[..]);
    assert_eq!(&z[9..], &[0x5A5A_5A5Au32 as i32; 3]);
}

#[test]
fn impl_multiply_to_len_reports_the_index_java_would_throw_on() {
    // xlen == 0: the JDK's `z[xstart]` is `z[-1]`, which throws rather
    // than doing nothing. A `usize` signature could not express this.
    let mut z: Vec<i32> = Vec::new();
    assert_eq!(impl_multiply_to_len(&[], 0, &[], 0, &mut z), Err(-1));
    // xlen past the operand: `x[xstart]` is the first bad read.
    let mut z = vec![0i32; 8];
    assert_eq!(impl_multiply_to_len(&[1, 2], 4, &[1, 2], 2, &mut z), Err(3));
}

// --- montReduce and its helpers ------------------------------------

#[test]
fn sub_n_returns_the_jdk_borrow() {
    let mut a = vec![0i32, 5];
    assert_eq!(sub_n(&mut a, &[0, 3], 2), 0);
    assert_eq!(a, vec![0, 2]);
    let mut a = vec![0i32, 3];
    assert_eq!(sub_n(&mut a, &[0, 5], 2), -1, "borrow out is -1, not 1");
    assert_eq!(a, vec![-1, -2]);
}

#[test]
fn int_array_cmp_to_len_is_unsigned() {
    // -1 is 0xFFFFFFFF, i.e. the LARGEST word, not the smallest.
    assert_eq!(int_array_cmp_to_len(&[-1], &[1], 1), 1);
    assert_eq!(int_array_cmp_to_len(&[1], &[-1], 1), -1);
    assert_eq!(int_array_cmp_to_len(&[1, -1], &[1, -1], 2), 0);
}

#[test]
fn add_one_carry_reports_the_carry_out() {
    // A carry that stays inside the number.
    let mut a = vec![0i32, 0, 0, -1];
    assert_eq!(add_one_carry(&mut a, 0, 3, 1), 0);
    // A carry that runs off the top: every word above the write is
    // 0xFFFF_FFFF, so the increment loop walks past index 0.
    let mut a = vec![-1i32, -1];
    assert_eq!(add_one_carry(&mut a, 0, 1, 1), 1);
}

/// Montgomery reduction has to leave `x * R^-1 mod m` in the high half,
/// fully reduced into `[0, m)`. Verified against `u128` arithmetic at two
/// limbs, where `R = 2^64` and every intermediate fits.
#[test]
fn mont_reduce_computes_x_times_r_inverse_mod_m() {
    let m: Vec<i32> = vec![0x4321_0ABD, 0x1357_9BDF];
    let len = m.len();
    let inv = neg_inv32(&m);
    let m_u = as_u128(&m);
    let r_inv = {
        // R^-1 mod m, by extended Euclid on u128.
        let (mut old_r, mut r) = ((1u128 << 64) % m_u, m_u);
        let (mut old_s, mut s) = (1i128, 0i128);
        while r != 0 {
            let q = old_r / r;
            let nr = old_r - q * r;
            old_r = r;
            r = nr;
            let ns = old_s - (q as i128) * s;
            old_s = s;
            s = ns;
        }
        assert_eq!(old_r, 1, "R must be invertible mod m");
        (((old_s % m_u as i128) + m_u as i128) % m_u as i128) as u128
    };

    for (x, y) in [
        (
            vec![0x0123_4567i32, -1985229329],
            vec![0x0FED_CBA9i32, 0x7654_3210],
        ),
        (vec![0i32, 0], vec![0x0FED_CBA9i32, 0x7654_3210]),
        (
            vec![0x4321_0ABDi32, 0x1357_9BDE],
            vec![0x4321_0ABDi32, 0x1357_9BDE],
        ),
    ] {
        let mut prod = vec![0i32; 2 * len];
        impl_multiply_to_len(&x, len as i32, &y, len as i32, &mut prod).unwrap();
        let full = as_u128(&prod);
        mont_reduce(&mut prod, &m, len, inv);
        let got = as_u128(&prod[..len]);
        let want = (full % m_u) * r_inv % m_u;
        assert_eq!(got, want, "montReduce({x:?} * {y:?}) mod {m:?}");
        assert!(got < m_u, "the residue must be fully reduced into [0, m)");
        assert_eq!(
            &prod[len..],
            &vec![0i32; len][..],
            "the low half must be zeroed by the reduction"
        );
    }
}

// 1) Trivial single-word — the simplest possible case.
#[test]
fn impl_square_to_len_single_word() {
    let x = vec![0x1234_5678i32];
    let mut z = vec![0i32; 2];
    impl_square_to_len(&x, 1, &mut z, 2);
    assert_eq!(z, ref_square(&x));
}

// 2) 2 limbs (64-bit operand).
#[test]
fn impl_square_to_len_two_words() {
    let x = vec![0x0000_0001i32, 0xFFFF_FFFFu32 as i32];
    let mut z = vec![0i32; 4];
    impl_square_to_len(&x, 2, &mut z, 4);
    assert_eq!(z, ref_square(&x));
}

// 3) 4 limbs (128-bit operand) — first non-trivial inner-loop pass.
#[test]
fn impl_square_to_len_four_words() {
    let x = vec![
        0x1234_5678i32,
        0x9ABC_DEF0u32 as i32,
        0x1111_2222i32,
        0x3333_4444i32,
    ];
    let mut z = vec![0i32; 8];
    impl_square_to_len(&x, 4, &mut z, 8);
    assert_eq!(z, ref_square(&x));
}

// 4) 32 limbs (1024-bit operand) — exercises the full diagonal +
//    off-diagonal path the way SunJCE's BigInteger uses it.
#[test]
fn impl_square_to_len_thirty_two_words() {
    // Pick a deterministic non-degenerate magnitude.
    let x: Vec<i32> = (0..32u32)
        .map(|i| 0x4000_0001u32.wrapping_add(i.wrapping_mul(0x1357_BDF1)) as i32)
        .collect();
    let mut z = vec![0i32; 64];
    impl_square_to_len(&x, 32, &mut z, 64);
    let expected = ref_square(&x);
    assert_eq!(z, expected, "1024-bit implSquareToLen must equal naive_mul");
}

// 5) 64 limbs (2048-bit operand) — the actual KC16 trigger size.
#[test]
fn impl_square_to_len_2048_bit() {
    let x: Vec<i32> = (0..64u32)
        .map(|i| 0xCAFE_BABEu32.wrapping_add(i.wrapping_mul(0x0123_4567)) as i32)
        .collect();
    let mut z = vec![0i32; 128];
    impl_square_to_len(&x, 64, &mut z, 128);
    let expected = ref_square(&x);
    assert_eq!(z, expected, "2048-bit implSquareToLen must equal naive_mul");
}

// 6) shiftLeftImplWorker - n=1 base case covers KC16 hot path.
//
//    `numIter` is 3, not 4, and that is the CONTRACT rather than a
//    shortened loop: `primitiveLeftShift` passes `len - 1` because it
//    patches `a[len - 1] <<= n` itself, and the worker reads
//    `oldArr[numIter]`, so `numIter == old.len()` would be out of range.
//    This test used to pass 4 against a 4-word array and assert the top
//    word stayed zero - which is also what an off-by-one in the worker
//    produces, so it could not tell the two apart.
#[test]
fn shift_left_impl_worker_n1() {
    let mut new_buf = vec![0i32; 4];
    let old = vec![
        0x1111_1111i32,
        0x2222_2222i32,
        0x3333_3333i32,
        0x4444_4444i32,
    ];
    shift_left_impl_worker(&mut new_buf, &old, 0, 1, 3);
    // new_buf[i] = (old[i]<<1) | (old[i+1]>>>31)
    let expected = vec![
        ((old[0] as u32) << 1 | ((old[1] as u32) >> 31)) as i32,
        ((old[1] as u32) << 1 | ((old[2] as u32) >> 31)) as i32,
        ((old[2] as u32) << 1 | ((old[3] as u32) >> 31)) as i32,
        0i32, // patched by primitiveLeftShift, which is why numIter is 3
    ];
    assert_eq!(new_buf, expected);
}

// 7) shiftLeftImplWorker — n=15 (mid-range).
#[test]
fn shift_left_impl_worker_n15() {
    let mut new_buf = vec![0i32; 3];
    let old = vec![0x1234_5678i32, 0x9ABC_DEF0u32 as i32, 0x1111_2222i32];
    shift_left_impl_worker(&mut new_buf, &old, 0, 15, 2);
    let expected = vec![
        ((old[0] as u32) << 15 | ((old[1] as u32) >> 17)) as i32,
        ((old[1] as u32) << 15 | ((old[2] as u32) >> 17)) as i32,
        0i32,
    ];
    assert_eq!(new_buf, expected);
}

// 6b) Both workers, against rows MEASURED on HotSpot 25.0.4+7 rather than
//     recomputed from this file's own arithmetic.
//
//     A test that rebuilds its expectation from the same expression the
//     implementation uses agrees with the implementation by construction;
//     tests 6 and 7 above do exactly that, which is half of why the
//     off-by-one survived them. These literals come from running
//     `L2IntrinsicProbe.java` on HotSpot, which calls the two
//     methods reflectively so nothing else is in the answer.
//
//     The magnitude below is (2^127 - 2)'s - the value `RJdkSecurity`
//     asserts on when `java/math/BigInteger` yields to real bytecode.
#[test]
fn the_shift_workers_match_hotspot_on_the_m127_magnitude() {
    let m127 = vec![
        0x7FFF_FFFFi32,
        0xFFFF_FFFFu32 as i32,
        0xFFFF_FFFFu32 as i32,
        0xFFFF_FFFEu32 as i32,
    ];

    // shiftRightImplWorker(newArr, m127, newIdx=1, shiftCount=1, numIter=3)
    let mut r1 = vec![0i32; 4];
    shift_right_impl_worker(&mut r1, &m127, 1, 1, 3);
    assert_eq!(r1, vec![0, -1, -1, -1], "right worker, newIdx = 1");

    // ...and with newIdx = 0, where OpenJDK starts at numIter - 1.
    let mut r0 = vec![0i32; 4];
    shift_right_impl_worker(&mut r0, &m127, 0, 1, 3);
    assert_eq!(r0, vec![-1, -1, -1, 0], "right worker, newIdx = 0");

    // numIter = 1 is ONE pass, not zero. The old body returned early for
    // `num_iter < 2` and wrote nothing at all.
    let mut r_one = vec![0i32; 4];
    shift_right_impl_worker(&mut r_one, &m127, 1, 1, 1);
    assert_eq!(r_one, vec![0, -1, 0, 0], "right worker, numIter = 1");

    let mut l_one = vec![0i32; 4];
    shift_left_impl_worker(&mut l_one, &m127, 0, 1, 1);
    assert_eq!(l_one, vec![-1, 0, 0, 0], "left worker, numIter = 1");

    // numIter = 0 writes nothing on both sides, and must not panic.
    let mut r_zero = vec![0i32; 4];
    shift_right_impl_worker(&mut r_zero, &m127, 1, 1, 0);
    assert_eq!(r_zero, vec![0, 0, 0, 0], "right worker, numIter = 0");
    let mut l_zero = vec![0i32; 4];
    shift_left_impl_worker(&mut l_zero, &m127, 0, 1, 0);
    assert_eq!(l_zero, vec![0, 0, 0, 0], "left worker, numIter = 0");
}

// 6c) An out-of-contract call declines instead of panicking.
//
//     A panic inside a native aborts the whole VM, so the pure helpers
//     bounds-check and return; the entry points raise the AIOOBE that Java
//     would. Reaching this is a caller bug either way - the point is which
//     failure the operator gets.
#[test]
fn an_undersized_array_declines_rather_than_panicking() {
    let short = vec![1i32, 2, 3];
    let mut out = vec![0i32; 3];
    shift_left_impl_worker(&mut out, &short, 0, 1, 3); // would read short[3]
    assert_eq!(out, vec![0, 0, 0]);
    let mut out2 = vec![0i32; 3];
    shift_right_impl_worker(&mut out2, &short, 1, 1, 3); // would read short[3]
    assert_eq!(out2, vec![0, 0, 0]);
}

// 8) primitive_left_shift_inplace — n=1 in-place, full chain.
#[test]
fn primitive_left_shift_inplace_n1() {
    let mut buf = vec![0x8000_0000u32 as i32, 0x4000_0000i32, 0x2000_0000i32];
    primitive_left_shift_inplace(&mut buf, 3, 1);
    // a[0] = (a[0]<<1) | (a[1]>>>31) = 0 | 0 = 0
    // a[1] = (a[1]<<1) | (a[2]>>>31) = 0x80000000 | 0 = 0x80000000
    // a[2] <<= 1 => 0x40000000
    assert_eq!(buf, vec![0i32, 0x8000_0000u32 as i32, 0x4000_0000i32]);
}

// 9) mul_add — single-iteration baseline. JDK contract: writes start
//    at `out[out.len() - offset - 1]` and walk down. Here offset=1,
//    out.len()=2 → writes start at out[0].
#[test]
fn mul_add_basic() {
    let in_ = vec![0x0000_0007i32];
    let mut out = vec![0i32, 0i32];
    let carry = mul_add(&mut out, &in_, 1, 1, 5);
    // out[0] += in_[0] * 5 = 7 * 5 = 35
    assert_eq!(out, vec![35, 0]);
    assert_eq!(carry, 0);
}

// 10) mul_add — produces a 32-bit carry. With offset=1, out.len()=2 the
//    write lands at out[0].
#[test]
fn mul_add_carry() {
    let in_ = vec![0xFFFF_FFFFu32 as i32];
    // out[0] starts as 0xFFFFFFFF; we add in[0]*k + out[0] + carry there.
    let mut out = vec![0xFFFF_FFFFu32 as i32, 0i32];
    let carry = mul_add(&mut out, &in_, 1, 1, 0x0000_0002);
    // 0xFFFFFFFF * 2 = 0x1_FFFFFFFE
    // + 0xFFFFFFFF = 0x2_FFFFFFFD
    // out[0] = 0xFFFFFFFD, carry = 2
    assert_eq!(out[0] as u32, 0xFFFF_FFFD);
    assert_eq!(carry as u32, 0x2);
}

// 11) Empty array — len=0 must early-return on primitive_left_shift_inplace.
#[test]
fn primitive_left_shift_inplace_empty() {
    let mut buf: Vec<i32> = vec![];
    primitive_left_shift_inplace(&mut buf, 0, 1);
    assert_eq!(buf, Vec::<i32>::new());
}

// 12) shift count 0 is a no-op.
#[test]
fn primitive_left_shift_inplace_n0() {
    let mut buf = vec![1i32, 2, 3];
    primitive_left_shift_inplace(&mut buf, 3, 0);
    assert_eq!(buf, vec![1, 2, 3]);
}

// 13) Heap-array native entry-point — implSquareToLen 4-word case.
#[test]
fn native_impl_square_to_len_dispatch() {
    let mut ctx = mock_ctx();
    let x_vals = vec![
        0x1234_5678i32,
        0x9ABC_DEF0u32 as i32,
        0x1111_2222i32,
        0x3333_4444i32,
    ];
    let x = alloc_int_arr(&mut ctx, &x_vals);
    let z = alloc_int_arr(&mut ctx, &vec![0i32; 8]);
    let r = native_impl_square_to_len(
        &mut ctx,
        &[
            Value::Object(Some(x)),
            Value::Int(4),
            Value::Object(Some(z)),
            Value::Int(8),
        ],
    )
    .expect("ok");
    match r {
        Some(Value::Object(Some(returned))) => {
            assert_eq!(read_arr(&ctx, returned), ref_square(&x_vals));
        }
        other => panic!("expected returned int[], got {other:?}"),
    }
}

// 14) Heap-array native entry-point — invalid len rejected with IAE.
#[test]
fn native_impl_square_to_len_invalid_len() {
    let mut ctx = mock_ctx();
    let x = alloc_int_arr(&mut ctx, &[1, 2, 3, 4]);
    let z = alloc_int_arr(&mut ctx, &vec![0i32; 8]);
    let r = native_impl_square_to_len(
        &mut ctx,
        &[
            Value::Object(Some(x)),
            Value::Int(0), // invalid: len < 1
            Value::Object(Some(z)),
            Value::Int(8),
        ],
    );
    assert!(r.is_err(), "len < 1 must throw IllegalArgumentException");
}

// 15) Heap-array native entry-point — len > x.length rejected.
#[test]
fn native_impl_square_to_len_len_oob() {
    let mut ctx = mock_ctx();
    let x = alloc_int_arr(&mut ctx, &[1, 2]);
    let z = alloc_int_arr(&mut ctx, &vec![0i32; 8]);
    let r = native_impl_square_to_len(
        &mut ctx,
        &[
            Value::Object(Some(x)),
            Value::Int(99), // len > x.length=2
            Value::Object(Some(z)),
            Value::Int(8),
        ],
    );
    assert!(r.is_err(), "len > x.length must throw");
}

// 16) Native shiftLeftImplWorker dispatch — verifies heap path matches
//     the in-memory algorithm.
#[test]
fn native_shift_left_impl_worker_dispatch() {
    let mut ctx = mock_ctx();
    let new_buf = alloc_int_arr(&mut ctx, &vec![0i32; 3]);
    let old = vec![0x1234_5678i32, 0x9ABC_DEF0u32 as i32, 0x1111_2222i32];
    let old_arr = alloc_int_arr(&mut ctx, &old);
    let r = native_shift_left_impl_worker(
        &mut ctx,
        &[
            Value::Object(Some(new_buf)),
            Value::Object(Some(old_arr)),
            Value::Int(0),
            Value::Int(15),
            Value::Int(2),
        ],
    )
    .expect("ok");
    assert_eq!(r, None);
    let result = read_arr(&ctx, new_buf);
    let expected = vec![
        ((old[0] as u32) << 15 | ((old[1] as u32) >> 17)) as i32,
        ((old[1] as u32) << 15 | ((old[2] as u32) >> 17)) as i32,
        0i32,
    ];
    assert_eq!(result, expected);
}

// 16b) The heap entry points reject `numIter == oldArr.length`, which the
//      corrected worker would read one past.
#[test]
fn native_shift_worker_rejects_num_iter_at_the_array_length() {
    let mut ctx = mock_ctx();
    let new_buf = alloc_int_arr(&mut ctx, &vec![0i32; 3]);
    let old_arr = alloc_int_arr(&mut ctx, &vec![1i32, 2, 3]);
    let args = [
        Value::Object(Some(new_buf)),
        Value::Object(Some(old_arr)),
        Value::Int(0),
        Value::Int(1),
        Value::Int(3),
    ];
    assert!(
        native_shift_left_impl_worker(&mut ctx, &args).is_err(),
        "left: numIter == oldArr.length must throw"
    );
    assert!(
        native_shift_right_impl_worker(&mut ctx, &args).is_err(),
        "right: numIter == oldArr.length must throw"
    );
}

// 17) Native shiftLeftImplWorker — a shift count outside 0..32 is taken
//     `& 31`, as `ishl` / `iushr` take it (JIT round 21: it used to throw
//     `IllegalArgumentException("invalid args: ...")`, which the Java body
//     never does).
#[test]
fn native_shift_left_impl_worker_masks_the_shift_count() {
    let mut ctx = mock_ctx();
    let new_buf = alloc_int_arr(&mut ctx, &vec![0i32; 3]);
    let old = vec![0x1234_5678i32, 0x9ABC_DEF0u32 as i32, 0x1111_2222i32];
    let old_arr = alloc_int_arr(&mut ctx, &old);
    let r = native_shift_left_impl_worker(
        &mut ctx,
        &[
            Value::Object(Some(new_buf)),
            Value::Object(Some(old_arr)),
            Value::Int(0),
            Value::Int(40), // 40 & 31 == 8
            Value::Int(2),
        ],
    )
    .expect("ok");
    assert_eq!(r, None);
    let expected = vec![
        ((old[0] as u32) << 8 | ((old[1] as u32) >> 24)) as i32,
        ((old[1] as u32) << 8 | ((old[2] as u32) >> 24)) as i32,
        0i32,
    ];
    assert_eq!(read_arr(&ctx, new_buf), expected);
}

// 18) Native implMulAdd dispatch — verifies the carry result.
//     With offset=1, out.length=2 the JDK rewrites offset to 0; the
//     write lands at out[0].
#[test]
fn native_impl_mul_add_carry() {
    let mut ctx = mock_ctx();
    let in_arr = alloc_int_arr(&mut ctx, &[0xFFFF_FFFFu32 as i32]);
    let out_arr = alloc_int_arr(&mut ctx, &[0xFFFF_FFFFu32 as i32, 0i32]);
    let r = native_impl_mul_add(
        &mut ctx,
        &[
            Value::Object(Some(out_arr)),
            Value::Object(Some(in_arr)),
            Value::Int(1),
            Value::Int(1),
            Value::Int(2),
        ],
    )
    .expect("ok");
    // carry = 2
    assert_eq!(r, Some(Value::Int(2)));
    let result = read_arr(&ctx, out_arr);
    assert_eq!(result[0] as u32, 0xFFFF_FFFD);
}

// Round 13 wave 9: the windowed native equals the whole-array kernel, and
// leaves every word outside the window alone.
#[test]
fn native_impl_mul_add_window_matches_whole_array_kernel() {
    let out0: Vec<i32> = (0..8)
        .map(|i| (0x9E37_79B9u32.wrapping_mul(i + 1)) as i32)
        .collect();
    let inp: Vec<i32> = (0..5)
        .map(|i| (0x7F4A_7C15u32.wrapping_mul(i + 3)) as i32)
        .collect();
    for (offset, len) in [(2usize, 3usize), (0, 5), (3, 5), (7, 1), (4, 0)] {
        let mut want = out0.clone();
        let want_carry = mul_add(&mut want, &inp, offset, len, 0x1234_5679);
        let mut ctx = mock_ctx();
        let in_arr = alloc_int_arr(&mut ctx, &inp);
        let out_arr = alloc_int_arr(&mut ctx, &out0);
        let r = native_impl_mul_add(
            &mut ctx,
            &[
                Value::Object(Some(out_arr)),
                Value::Object(Some(in_arr)),
                Value::Int(offset as i32),
                Value::Int(len as i32),
                Value::Int(0x1234_5679),
            ],
        )
        .expect("ok");
        assert_eq!(r, Some(Value::Int(want_carry)), "offset {offset} len {len}");
        assert_eq!(read_arr(&ctx, out_arr), want, "offset {offset} len {len}");
    }
}

// 19) Null array argument rejected with NPE.
#[test]
fn native_impl_square_to_len_null_array() {
    let mut ctx = mock_ctx();
    let r = native_impl_square_to_len(
        &mut ctx,
        &[
            Value::Object(None),
            Value::Int(1),
            Value::Object(None),
            Value::Int(2),
        ],
    );
    assert!(r.is_err(), "null array must throw NPE");
}

// 20) Registration smoke test — every native is wired.
#[test]
fn registers_all_intrinsics() {
    let mut reg = NativeMethodRegistry::new();
    register_biginteger_intrinsics(&mut reg);
    let bi = "java/math/BigInteger";
    assert!(reg.find(bi, "implSquareToLen", "([II[II)[I").is_some());
    assert!(reg.find(bi, "shiftLeftImplWorker", "([I[IIII)V").is_some());
    assert!(reg.find(bi, "shiftRightImplWorker", "([I[IIII)V").is_some());
    assert!(reg.find(bi, "implMulAdd", "([I[IIII)I").is_some());
    assert!(reg.find(bi, "mulAdd", "([I[IIII)I").is_some());
}

// --- SBR wave 5: the fast paths against the transcription ----------------

struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn word(&mut self) -> i32 {
        match self.next() % 8 {
            0 => 0,
            1 => -1,
            _ => self.next() as i32,
        }
    }
    fn words(&mut self, n: usize) -> Vec<i32> {
        (0..n).map(|_| self.word()).collect()
    }
}

fn arena(arrays: Vec<Vec<i32>>) -> Arena {
    let mut ar = Arena::new();
    for a in arrays {
        ar.dirty.push(false);
        ar.refs.push(None);
        ar.arrs.push(a);
    }
    ar
}

/// Run `f` twice on identical arenas — fast paths on, then transcription
/// only — and require identical results and identical arrays.
fn both<T: PartialEq + std::fmt::Debug>(
    arrays: Vec<Vec<i32>>,
    f: impl Fn(&mut Arena) -> Result<T, Jx>,
) -> (Result<T, Jx>, Vec<Vec<i32>>) {
    let mut fast = arena(arrays.clone());
    let rf = f(&mut fast);
    FORCE_TRANSCRIPTION.with(|c| c.set(true));
    let mut slow = arena(arrays);
    let rs = f(&mut slow);
    FORCE_TRANSCRIPTION.with(|c| c.set(false));
    assert_eq!(rf, rs, "result");
    assert_eq!(fast.arrs, slow.arrs, "arrays");
    (rf, fast.arrs)
}

fn odd_modulus(r: &mut Rng, len: usize) -> (Vec<i32>, i64) {
    let mut m = r.words(len);
    if m[0] == 0 {
        m[0] = 1;
    }
    m[len - 1] |= 1;
    let n0 = (m[len - 1] as u32 as u64) | ((m[len - 2] as u32 as u64) << 32);
    (m, neg_inv64(n0) as i64)
}

fn below(r: &mut Rng, m: &[i32]) -> Vec<i32> {
    let mut a = r.words(m.len());
    a[0] = ((a[0] as u32 as u64) % (m[0] as u32 as u64).max(1)) as i32;
    a
}

#[test]
fn fast_montgomery_matches_the_java_body() {
    let mut r = Rng(0x1234_5678_9abc_def1);
    for len in (2..=66).step_by(2) {
        for shape in 0..4 {
            let (m, inv) = odd_modulus(&mut r, len);
            let a = below(&mut r, &m);
            let b = below(&mut r, &m);
            let prod = match shape {
                0 => None,
                1 => Some(vec![0x5a5a_5a5a; 2 * len]),
                2 => Some(vec![7; len]), // short: Java allocates
                _ => Some(vec![9; 2 * len]),
            };
            let mut arrays = vec![a, b, m];
            if let Some(p) = prod {
                arrays.push(p);
            }
            let has_p = arrays.len() == 4;
            let (res, arrs) = both(arrays.clone(), |ar| {
                let p = if has_p { Some(3) } else { None };
                j_impl_montgomery_multiply(ar, Some(0), Some(1), Some(2), len as i32, inv, p)
                    .map(|h| h.map(|i| ar.arrs[i].clone()))
            });
            let out = res.expect("in-domain call").expect("non-null");
            assert_eq!(out.len(), 2 * len);
            assert!(out[len..].iter().all(|&w| w == 0), "low half zeroed");
            let _ = arrs;
            // Square, same shapes.
            let (res, _) = both(arrays, |ar| {
                let p = if has_p { Some(3) } else { None };
                j_impl_montgomery_square(ar, Some(0), Some(2), len as i32, inv, p)
                    .map(|h| h.map(|i| ar.arrs[i].clone()))
            });
            res.expect("in-domain square");            }        }    }

#[test]
fn fast_multiply_and_square_match_the_java_body() {
    let mut r = Rng(0xdead_beef_cafe_f00d);
    for xl in 1..=40usize {
        let yl = 1 + (r.next() % 40) as usize;
        let x = r.words(xl);
        let y = r.words(yl);
        let z = vec![0x3c3c_3c3c; xl + yl + (r.next() % 3) as usize];
        let (res, arrs) = both(vec![x.clone(), y.clone(), z], |ar| {
            j_impl_multiply_to_len(ar, Some(0), xl as i32, Some(1), yl as i32, Some(2))
        });
        assert_eq!(res, Ok(Some(2)));
        assert_eq!(&arrs[2][..xl + yl], &naive_mul(&x, &y)[..]);
        let (res, arrs) = both(vec![x.clone(), vec![0; 2 * xl]], |ar| {
            j_impl_square_to_len(ar, Some(0), xl as i32, Some(1), 2 * xl as i32)
        });
        assert_eq!(res, Ok(Some(1)));
        assert_eq!(arrs[1], ref_square(&x));
        // mulAdd over the same operands.
        let off = (r.next() % 3) as i32;
        let (_res, _) = both(vec![r.words(xl + 3), x.clone()], |ar| {
            j_mul_add(ar, Some(0), Some(1), off, xl as i32, -7)
        });
    }
}

/// Exception texts measured on HotSpot 25.0.4+7 `-Xint` (the Java bodies,
/// called reflectively; `java.base` carries parameter names).
#[test]
fn the_java_body_exceptions_carry_hotspot_texts() {
    let npe = |s: &str| Err::<Option<usize>, Jx>(Jx::Npe(Some(s.to_string())));
    let mut ar = arena(vec![vec![0; 1], vec![0; 2]]);
    assert_eq!(
        j_impl_multiply_to_len(&mut ar, None, 1, Some(0), 1, Some(1)),
        npe("Cannot load from int array because \"x\" is null")
    );
    let mut ar = arena(vec![vec![0; 1], vec![0; 2]]);
    assert_eq!(
        j_impl_multiply_to_len(&mut ar, Some(0), 1, Some(0), 1, None),
        npe("Cannot store to int array because \"z\" is null")
    );
    let mut ar = arena(vec![vec![0; 1], vec![0; 1], vec![0; 2]]);
    assert_eq!(
        j_impl_multiply_to_len(&mut ar, Some(0), 0, Some(1), 0, Some(2)),
        Err(Jx::Index {
            index: -1,
            length: 2
        })
    );
    let mut ar = arena(vec![vec![0; 2], vec![0; 2]]);
    assert_eq!(
        j_mul_add(&mut ar, Some(0), Some(1), -1, 1, 3),
        Err(Jx::Iae("input offset is invalid: -1".into()))
    );
    assert_eq!(
        j_mul_add(&mut ar, Some(0), Some(1), 5, 1, 3),
        Err(Jx::Iae("input offset is out of bound: 5 > 1".into()))
    );
    assert_eq!(
        j_mul_add(&mut ar, Some(0), None, 0, 1, 3),
        Err(Jx::Npe(Some(
            "Cannot read the array length because \"in\" is null".into()
        )))
    );
    assert_eq!(
        j_impl_mul_add(&mut ar, Some(0), Some(1), -1, 1, 3),
        Err(Jx::Index {
            index: 2,
            length: 2
        })
    );
    // implMontgomeryMultiply(null, ...) -> multiplyToLenCheck's requireNonNull.
    let mut ar = arena(vec![vec![0; 2], vec![1, 3]]);
    assert_eq!(
        j_impl_montgomery_multiply(&mut ar, None, Some(0), Some(1), 2, 1, None),
        Err(Jx::Npe(None))
    );
    // ... with mod null -> implMulAddCheck's `in.length`.
    let mut ar = arena(vec![vec![0; 2], vec![0; 2]]);
    assert_eq!(
        j_impl_montgomery_multiply(&mut ar, Some(0), Some(1), None, 2, 1, None),
        Err(Jx::Npe(Some(
            "Cannot read the array length because \"in\" is null".into()
        )))
    );
    // ... len -2 -> new int[-4].
    let mut ar = arena(vec![vec![0; 2], vec![0; 2], vec![1, 3]]);
    assert_eq!(
        j_impl_montgomery_multiply(&mut ar, Some(0), Some(1), Some(2), -2, 1, None),
        Err(Jx::Nase(-4))
    );
    // ... mod shorter than len.
    let mut ar = arena(vec![vec![0; 2], vec![0; 2], vec![1]]);
    assert_eq!(
        j_impl_montgomery_multiply(&mut ar, Some(0), Some(1), Some(2), 2, 1, None),
        Err(Jx::Iae("input length is out of bound: 2 > 1".into()))
    );
    // implMontgomerySquare(null a) -> implSquareToLenChecks' `x.length`.
    let mut ar = arena(vec![vec![1, 3]]);
    assert_eq!(
        j_impl_montgomery_square(&mut ar, None, Some(0), 2, 1, None),
        Err(Jx::Npe(Some(
            "Cannot read the array length because \"x\" is null".into()
        )))
    );
}

/// A tiny modulus with operands far above it: the Java body's final
/// `while (n >= mod) subN` loop would run ~2^32 times; the closed form
/// must land where the loop does (checked against a direct `%`).
#[test]
fn mont_reduce_closed_form_matches_the_subtraction_loop() {
    // mod = [0, 3] (value 3), len 2, a = b = 2^48 - 1 (far above mod).
    let a = vec![0xFFFFi32, -1];
    let m = vec![0i32, 3];
    let inv = neg_inv64(3) as i64;
    let mut ar = arena(vec![a.clone(), m.clone()]);
    let out = j_impl_montgomery_multiply(&mut ar, Some(0), Some(0), Some(1), 2, inv, None)
        .expect("terminates")
        .expect("non-null");
    let got = &ar.arrs[out];
    // T = a^2 < 2^96; U = (T + Q*3) / 2^64 with Q = -T*3^-1 mod 2^64.
    let av: u128 = (1u128 << 48) - 1;
    let t: u128 = av * av;
    let q = (t as u64).wrapping_mul(neg_inv64(3)) as u128;
    let u = (t + q * 3) >> 64;
    let want = (u % 3) as u64;
    assert_eq!(got[0], 0);
    assert_eq!(got[1] as u32 as u64, want);
    assert_eq!(&got[2..], &[0, 0]);
}

// --- JIT round 21 (lane bignum): the shift workers and the direct roads ---

#[test]
fn shift_worker_fast_paths_match_the_java_body() {
    let mut r = Rng(0x0123_4567_89ab_cdef);
    for len in 1..=9usize {
        let old = r.words(len);
        let new = r.words(len + 2);
        for ni in -1..=3i32 {
            for n in -1..=(len as i32 + 1) {
                for sh in [-1i32, 0, 1, 13, 31, 32, 40] {
                    let _ = both(vec![new.clone(), old.clone()], |ar| {
                        j_shift_left_impl_worker(ar, Some(0), Some(1), ni, sh, n)
                    });
                    let _ = both(vec![new.clone(), old.clone()], |ar| {
                        j_shift_right_impl_worker(ar, Some(0), Some(1), ni, sh, n)
                    });
                    // One array passed twice: the Java loop reads back its
                    // own stores.
                    let _ = both(vec![old.clone()], |ar| {
                        j_shift_left_impl_worker(ar, Some(0), Some(0), ni, sh, n)
                    });
                    let _ = both(vec![old.clone()], |ar| {
                        j_shift_right_impl_worker(ar, Some(0), Some(0), ni, sh, n)
                    });
                }
            }
        }
    }
}

#[test]
fn shift_workers_follow_the_java_body_on_edge_shapes() {
    let npe = |s: &str| Err::<(), Jx>(Jx::Npe(Some(s.to_string())));
    // No pass runs, so nothing is dereferenced.
    let mut ar = arena(vec![]);
    assert_eq!(
        j_shift_left_impl_worker(&mut ar, None, None, 0, 1, 0),
        Ok(())
    );
    assert_eq!(
        j_shift_right_impl_worker(&mut ar, None, None, 0, 1, -5),
        Ok(())
    );
    // `iastore` evaluates its value first: the loads come before the store.
    let mut ar = arena(vec![vec![1, 2]]);
    assert_eq!(
        j_shift_left_impl_worker(&mut ar, None, Some(0), 0, 1, 1),
        npe("Cannot store to int array because \"newArr\" is null")
    );
    assert_eq!(
        j_shift_left_impl_worker(&mut ar, Some(0), None, 0, 1, 1),
        npe("Cannot load from int array because \"oldArr\" is null")
    );
    assert_eq!(
        j_shift_right_impl_worker(&mut ar, None, Some(0), 1, 1, 1),
        npe("Cannot store to int array because \"newArr\" is null")
    );
    // numIter == oldArr.length: the passes before the bad load are stored.
    let mut ar = arena(vec![vec![0; 4], vec![1, 2, 3]]);
    assert_eq!(
        j_shift_left_impl_worker(&mut ar, Some(0), Some(1), 0, 1, 3),
        Err(Jx::Index {
            index: 3,
            length: 3
        })
    );
    assert_eq!(ar.arrs[0], vec![2, 4, 0, 0]);
    for force in [false, true] {
        FORCE_TRANSCRIPTION.with(|c| c.set(force));
        // Aliased right shift, newIdx 0: pass 2 loads the word pass 1 stored.
        let mut ar = arena(vec![vec![0x8000_0001u32 as i32, 3, 5]]);
        assert_eq!(
            j_shift_right_impl_worker(&mut ar, Some(0), Some(0), 0, 1, 2),
            Ok(())
        );
        assert_eq!(
            ar.arrs[0],
            vec![0xC000_0001u32 as i32, 0x8000_0002u32 as i32, 5],
            "force={force}"
        );
        // Aliased left shift, newIdx 1: the same, upward.
        let mut ar = arena(vec![vec![
            0x8000_0000u32 as i32,
            0x8000_0000u32 as i32,
            0,
            0,
        ]]);
        assert_eq!(
            j_shift_left_impl_worker(&mut ar, Some(0), Some(0), 1, 1, 2),
            Ok(())
        );
        assert_eq!(
            ar.arrs[0],
            vec![0x8000_0000u32 as i32, 1, 2, 0],
            "force={force}"
        );
    }
    FORCE_TRANSCRIPTION.with(|c| c.set(false));
    // ...and through the heap entry, with one reference passed twice.
    let mut ctx = mock_ctx();
    let a = alloc_int_arr(&mut ctx, &[0x8000_0001u32 as i32, 3, 5]);
    native_shift_right_impl_worker(
        &mut ctx,
        &[
            Value::Object(Some(a)),
            Value::Object(Some(a)),
            Value::Int(0),
            Value::Int(1),
            Value::Int(2),
        ],
    )
    .expect("ok");
    assert_eq!(
        read_arr(&ctx, a),
        vec![0xC000_0001u32 as i32, 0x8000_0002u32 as i32, 5]
    );
}

type Body = fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult;

fn obj(o: ObjectRef) -> Value {
    Value::Object(Some(o))
}

/// Run `body` on a fresh mock heap holding `arrays`; `args` builds the
/// argument list from their references. Returns the answer (an `int[]`
/// result, which argument it is if any, and its words; an `int`; `void`;
/// or `err`) and every argument array afterwards.
fn run_body(
    arrays: &[Vec<i32>],
    body: Body,
    args: &dyn Fn(&[ObjectRef]) -> Vec<Value>,
) -> (String, Vec<Vec<i32>>) {
    let mut ctx = mock_ctx();
    let refs: Vec<ObjectRef> = arrays.iter().map(|a| alloc_int_arr(&mut ctx, a)).collect();
    let argv = args(&refs);
    let answer = match body(&mut ctx, &argv) {
        Ok(Some(Value::Object(Some(r)))) => {
            let which = refs
                .iter()
                .position(|q| std::ptr::eq(q.as_ptr(), r.as_ptr()));
            format!("arr {which:?} {:?}", read_arr(&ctx, r))
        }
        Ok(Some(Value::Int(v))) => format!("int {v}"),
        Ok(None) => "void".to_string(),
        Ok(other) => format!("{other:?}"),
        Err(_) => "err".to_string(),
    };
    let after = refs.iter().map(|r| read_arr(&ctx, *r)).collect();
    (answer, after)
}

/// The direct road and the Arena entry must leave the same answer and
/// the same heap, shape by shape.
fn same_answer(
    arrays: &[Vec<i32>],
    direct: Body,
    arena_entry: Body,
    args: &dyn Fn(&[ObjectRef]) -> Vec<Value>,
) -> String {
    let d = run_body(arrays, direct, args);
    let a = run_body(arrays, arena_entry, args);
    assert_eq!(d, a, "direct road vs Arena entry");
    d.0
}

#[test]
fn direct_roads_match_the_arena_entries() {
    let mut r = Rng(0x0bad_cafe_1234_5679);
    let mul_d: Body = native_impl_multiply_to_len_direct;
    let mul_a: Body = native_impl_multiply_to_len;
    for xl in 1..=70usize {
        let yl = 1 + (r.next() % 70) as usize;
        let x = r.words(xl);
        let y = r.words(yl);
        let z = vec![0x3c3c_3c3c; xl + yl + (r.next() % 3) as usize];
        let (xi, yi) = (xl as i32, yl as i32);
        let ans = same_answer(
            &[x.clone(), y.clone(), z.clone()],
            mul_d,
            mul_a,
            &|o: &[ObjectRef]| {
                vec![
                    obj(o[0]),
                    Value::Int(xi),
                    obj(o[1]),
                    Value::Int(yi),
                    obj(o[2]),
                ]
            },
        );
        assert!(ans.starts_with("arr Some(2)"), "{ans}");
        // x passed as both operands.
        same_answer(
            &[x.clone(), z.clone()],
            mul_d,
            mul_a,
            &|o: &[ObjectRef]| {
                vec![
                    obj(o[0]),
                    Value::Int(xi),
                    obj(o[0]),
                    Value::Int(xi),
                    obj(o[1]),
                ]
            },
        );
        // z aliased with x: the Arena entry's transcription answers.
        same_answer(
            &[x.clone(), y.clone()],
            mul_d,
            mul_a,
            &|o: &[ObjectRef]| {
                vec![
                    obj(o[0]),
                    Value::Int(xi),
                    obj(o[1]),
                    Value::Int(yi),
                    obj(o[0]),
                ]
            },
        );
        // squareToLen's shape, and a wider z (transcription).
        for extra in [0usize, 1] {
            let zs = vec![7; 2 * xl + extra];
            let zlen = 2 * xi;
            same_answer(
                &[x.clone(), zs],
                native_impl_square_to_len_direct,
                native_impl_square_to_len,
                &|o: &[ObjectRef]| vec![obj(o[0]), Value::Int(xi), obj(o[1]), Value::Int(zlen)],
            );
        }
        // mulAdd / implMulAdd: in-range windows, and shapes the checks or
        // the body throw on.
        let out = r.words(xl + 3);
        for off in [-1i32, 0, 1, 2, 3, 4] {
            for len in [0i32, 1, xi, xi + 1] {
                let k = r.word();
                let args = |o: &[ObjectRef]| {
                    vec![
                        obj(o[0]),
                        obj(o[1]),
                        Value::Int(off),
                        Value::Int(len),
                        Value::Int(k),
                    ]
                };
                same_answer(
                    &[out.clone(), x.clone()],
                    native_mul_add_direct,
                    native_mul_add,
                    &args,
                );
                same_answer(
                    &[out.clone(), x.clone()],
                    native_impl_mul_add_direct,
                    native_impl_mul_add,
                    &args,
                );
            }
        }
    }
}

#[test]
fn direct_montgomery_matches_the_arena_entries() {
    let mut r = Rng(0x5eed_0000_0000_0021);
    for len in 2..=68usize {
        let (m, inv) = odd_modulus(&mut r, len);
        let a = below(&mut r, &m);
        let b = below(&mut r, &m);
        let li = len as i32;
        for shape in 0..5 {
            let prod: Option<Vec<i32>> = match shape {
                0 => None,
                1 => Some(vec![0x5a5a_5a5a; 2 * len]),
                2 => Some(vec![7; len]),         // short: Java allocates
                3 => Some(vec![9; 2 * len + 1]), // long: transcription
                _ => Some(vec![1; 2 * len]),
            };
            let mut arrays = vec![a.clone(), b.clone(), m.clone()];
            if let Some(p) = prod {
                arrays.push(p);
            }
            let has_p = arrays.len() == 4;
            let pv = move |o: &[ObjectRef]| {
                if has_p {
                    obj(o[3])
                } else {
                    Value::Object(None)
                }
            };
            same_answer(
                &arrays,
                native_impl_montgomery_multiply_direct,
                native_impl_montgomery_multiply,
                &|o: &[ObjectRef]| {
                    vec![
                        obj(o[0]),
                        obj(o[1]),
                        obj(o[2]),
                        Value::Int(li),
                        Value::Long(inv),
                        pv(o),
                    ]
                },
            );
            same_answer(
                &arrays,
                native_impl_montgomery_square_direct,
                native_impl_montgomery_square,
                &|o: &[ObjectRef]| {
                    vec![
                        obj(o[0]),
                        obj(o[2]),
                        Value::Int(li),
                        Value::Long(inv),
                        pv(o),
                    ]
                },
            );            }        }    }

/// misc21 (BIGNUM21-3): the shift workers' direct roads against the Arena
/// entries, shape by shape: every shape the roads take, the ones they
/// decline (one array passed twice at the wrong `newIdx`, short or negative
/// counts, a null array) and the ones that throw. Same answer, same heap.
#[test]
fn misc21_direct_shift_workers_match_the_arena_entries() {
    let mut r = Rng(0x0021_5b1f_7e57_0003);
    let left_d: Body = native_shift_left_impl_worker_direct;
    let left_a: Body = native_shift_left_impl_worker;
    let right_d: Body = native_shift_right_impl_worker_direct;
    let right_a: Body = native_shift_right_impl_worker;
    for len in 1..=9usize {
        let old = r.words(len);
        let new = r.words(len + 2);
        for ni in -1..=3i32 {
            for n in -1..=(len as i32 + 1) {
                for sh in [-1i32, 0, 1, 31, 40] {
                    let two = |o: &[ObjectRef]| {
                        vec![
                            obj(o[0]),
                            obj(o[1]),
                            Value::Int(ni),
                            Value::Int(sh),
                            Value::Int(n),
                        ]
                    };
                    same_answer(&[new.clone(), old.clone()], left_d, left_a, &two);
                    same_answer(&[new.clone(), old.clone()], right_d, right_a, &two);
                    // One array passed twice: the Java loop may read back
                    // its own stores.
                    let one = |o: &[ObjectRef]| {
                        vec![
                            obj(o[0]),
                            obj(o[0]),
                            Value::Int(ni),
                            Value::Int(sh),
                            Value::Int(n),
                        ]
                    };
                    same_answer(&[old.clone()], left_d, left_a, &one);
                    same_answer(&[old.clone()], right_d, right_a, &one);
                }
            }
        }
    }
    // A null array: the roads decline and the Arena entries answer.
    for n in [0i32, 1] {
        let args = move |o: &[ObjectRef]| {
            vec![
                Value::Object(None),
                obj(o[0]),
                Value::Int(0),
                Value::Int(1),
                Value::Int(n),
            ]
        };
        same_answer(&[vec![1, 2, 3]], left_d, left_a, &args);
        same_answer(&[vec![1, 2, 3]], right_d, right_a, &args);
    }
}

/// misc21 (BIGNUM21-3): the roads are actually TAKEN where the contract
/// says so (the equivalence test above passes if they never are), the
/// answers are the Java loop's, and the aliased shapes that read back their
/// own stores are left to the Arena entries.
#[test]
fn misc21_direct_shift_workers_are_taken_on_the_jdk_shapes() {
    let mut ctx = mock_ctx();
    let old = alloc_int_arr(&mut ctx, &[1, 2, 3, 4]);
    let new = alloc_int_arr(&mut ctx, &[0; 6]);
    // newArr[1..4) = oldArr[k] << 3 | oldArr[k + 1] >>> 29.
    let args = [
        obj(new),
        obj(old),
        Value::Int(1),
        Value::Int(3),
        Value::Int(3),
    ];
    assert!(direct_shift_left(&mut ctx, &args).is_some());
    assert_eq!(read_arr(&ctx, new), vec![0, 8, 16, 24, 0, 0]);
    assert_eq!(read_arr(&ctx, old), vec![1, 2, 3, 4]);
    // Right, newIdx 1: newArr[3], [2], [1] from oldArr[3..2], [2..1], [1..0].
    let new = alloc_int_arr(&mut ctx, &[0; 6]);
    let args = [
        obj(new),
        obj(old),
        Value::Int(1),
        Value::Int(1),
        Value::Int(3),
    ];
    assert!(direct_shift_right(&mut ctx, &args).is_some());
    assert_eq!(
        read_arr(&ctx, new),
        vec![0, 0x8000_0001u32 as i32, 1, 0x8000_0002u32 as i32, 0, 0]
    );
    // primitiveLeftShift's shape (one array, newIdx 0) and
    // primitiveRightShift's (one array, newIdx 1) are taken ...
    let a = alloc_int_arr(&mut ctx, &[0x8000_0001u32 as i32, 3, 5]);
    let args = [obj(a), obj(a), Value::Int(0), Value::Int(1), Value::Int(2)];
    assert!(direct_shift_left(&mut ctx, &args).is_some());
    // a[0] = a[0] << 1 | a[1] >>> 31 = 2, a[1] = 3 << 1 | 5 >>> 31 = 6.
    assert_eq!(read_arr(&ctx, a), vec![2, 6, 5]);
    let a = alloc_int_arr(&mut ctx, &[0x8000_0001u32 as i32, 3, 5]);
    let args = [obj(a), obj(a), Value::Int(1), Value::Int(1), Value::Int(2)];
    assert!(direct_shift_right(&mut ctx, &args).is_some());
    // a[2] = 5 >>> 1 | 3 << 31, then a[1] = 3 >>> 1 | a[0] << 31.
    assert_eq!(
        read_arr(&ctx, a),
        vec![
            0x8000_0001u32 as i32,
            0x8000_0001u32 as i32,
            0x8000_0002u32 as i32,
        ]
    );
    // ... and the shapes that read back a store are not.
    let a = alloc_int_arr(&mut ctx, &[0x8000_0001u32 as i32, 3, 5]);
    let args = [obj(a), obj(a), Value::Int(1), Value::Int(1), Value::Int(2)];
    assert!(direct_shift_left(&mut ctx, &args).is_none());
    let args = [obj(a), obj(a), Value::Int(0), Value::Int(1), Value::Int(2)];
    assert!(direct_shift_right(&mut ctx, &args).is_none());
    // Out of range, no pass, a null array: declined, nothing stored.
    let b = alloc_int_arr(&mut ctx, &[9, 9, 9]);
    let args = [
        obj(b),
        obj(old),
        Value::Int(0),
        Value::Int(1),
        Value::Int(4),
    ];
    assert!(direct_shift_left(&mut ctx, &args).is_none());
    let args = [
        obj(b),
        obj(old),
        Value::Int(0),
        Value::Int(1),
        Value::Int(0),
    ];
    assert!(direct_shift_left(&mut ctx, &args).is_none());
    let args = [
        Value::Object(None),
        obj(old),
        Value::Int(0),
        Value::Int(1),
        Value::Int(1),
    ];
    assert!(direct_shift_right(&mut ctx, &args).is_none());
    assert_eq!(read_arr(&ctx, b), vec![9, 9, 9]);
}

/// misc21 (BIGNUM21-2): the JDK's `oddModPow` pads an odd-word modulus with
/// a LEADING zero word before `montgomeryMultiply` /  `montgomerySquare`
/// (the `(modLen & 1) != 0` arm, JDK 21 `BigInteger.class` checked with
/// `javap`; also `implMontgomeryMultiplyChecks` throws for an odd `len`), so
/// the kernels only ever see an even word count and the 64-bit road serves
/// 96-, 160-, 224- and 2080-bit moduli too. This holds the road to the
/// transcription on exactly that padded shape and requires it to be TAKEN.
#[test]
fn misc21_fast_montgomery_serves_the_zero_padded_odd_word_modulus() {
    let mut r = Rng(0x0021_0dd0_7357_0002);
    for odd in (1..=65usize).step_by(2) {
        // The JDK's modulus: `odd` words, top word non-zero, bottom odd.
        let mut m = r.words(odd);
        if m[0] == 0 {
            m[0] = 1;
        }
        m[odd - 1] |= 1;
        // Two operands below it (top word reduced below the modulus' top).
        let mut a = r.words(odd);
        let mut b = r.words(odd);
        a[0] = ((a[0] as u32 as u64) % (m[0] as u32 as u64).max(1)) as i32;
        b[0] = ((b[0] as u32 as u64) % (m[0] as u32 as u64).max(1)) as i32;
        // Zero-pad in front: `len = odd + 1` words.
        let pad = |w: &[i32]| -> Vec<i32> {
            let mut p = vec![0];
            p.extend_from_slice(w);
            p
        };
        let (a, b, m) = (pad(&a), pad(&b), pad(&m));
        let len = odd + 1;
        let inv = neg_inv64(m[len - 1] as u32 as u64) as i64;
        let mut ar = arena(vec![a.clone(), b.clone(), m.clone()]);
        assert!(
            fast_montgomery(&mut ar, Some(0), Some(1), Some(2), len as i32, inv, None)
                .is_some(),
            "odd = {odd}: the zero-padded modulus takes the 64-bit road"
        );
        let arrays = vec![a, b, m];
        let (res, _) = both(arrays.clone(), |ar| {
            j_impl_montgomery_multiply(ar, Some(0), Some(1), Some(2), len as i32, inv, None)
                .map(|h| h.map(|i| ar.arrs[i].clone()))
        });
        let out = res.expect("in-domain call").expect("non-null");
        assert_eq!(out.len(), 2 * len);
        assert!(out[len..].iter().all(|&w| w == 0), "low half zeroed");
        let (res, _) = both(arrays, |ar| {
            j_impl_montgomery_square(ar, Some(0), Some(2), len as i32, inv, None)
                .map(|h| h.map(|i| ar.arrs[i].clone()))
        });
        res.expect("in-domain square");        }    }

#[test]
fn the_kernel_direct_switch_keeps_every_triple_intrinsic() {
    let mut reg = NativeMethodRegistry::new();
    register_biginteger_intrinsics(&mut reg);
    let bi = "java/math/BigInteger";
    for (name, desc) in [
        ("implSquareToLen", "([II[II)[I"),
        ("shiftLeftImplWorker", "([I[IIII)V"),
        ("shiftRightImplWorker", "([I[IIII)V"),
        ("implMulAdd", "([I[IIII)I"),
        ("mulAdd", "([I[IIII)I"),
        ("implMultiplyToLen", "([II[II[I)[I"),
        ("implMontgomeryMultiply", "([I[I[IIJ[I)[I"),
        ("implMontgomerySquare", "([I[IIJ[I)[I"),
    ] {
        assert!(reg.find(bi, name, desc).is_some(), "{name}{desc}");
    }
}
