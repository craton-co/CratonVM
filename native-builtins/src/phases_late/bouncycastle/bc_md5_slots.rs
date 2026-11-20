#[allow(unused_imports)]
use super::*;

pub static BC_MD5_SLOTS: cratonvm_types::lock_order::OrderedPlRwLock<Option<((usize, u32), BcMd5Slots)>> =
    cratonvm_types::lock_order::OrderedPlRwLock::new(
        None,
        cratonvm_types::lock_order::LockLevel::Scratch,
    );

pub fn bc_md5_slots(
    ctx: &dyn NativeContext,
    this: ObjectRef,
) -> Result<BcMd5Slots, MethodCallFailed> {
    let class_id = ctx.class_id_of_object(this);
    let key = (ctx.vm_identity(), class_id.as_u32());
    if let Some((cached_key, slots)) = *BC_MD5_SLOTS.read() {
        if cached_key == key {
            return Ok(slots);
        }
    }
    let bad = |what: &str| -> MethodCallFailed {
        RuntimeError::IllegalStateException {
            message: format!("MD5Digest: cannot resolve field {what}"),
        }
        .into()
    };
    let fields = ctx.declared_fields(class_id);
    let index_of = |name: &str| -> Option<(usize, u8)> {
        fields
            .iter()
            .find(|f| f.name == name && !f.is_static)
            .map(|f| (f.slot_index, f.descriptor.as_bytes().first().copied().unwrap_or(0)))
    };
    let mut h = [(0usize, 0u8); 4];
    for (slot, name) in h.iter_mut().zip(BC_MD5_STATE_FIELDS) {
        *slot = index_of(name).ok_or_else(|| bad(name))?;
    }
    let slots = BcMd5Slots {
        h,
        x: index_of("X").ok_or_else(|| bad("X"))?.0,
        x_off: index_of("xOff").ok_or_else(|| bad("xOff"))?,
    };
    *BC_MD5_SLOTS.write() = Some((key, slots));
    Ok(slots)
}

/// Native `org.bouncycastle.crypto.digests.MD5Digest.processBlock()` (`NativeKind::Intrinsic`).
pub fn bc_general_digest_update(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> cratonvm_types::error::MethodCallResult {
    use std::sync::atomic::{AtomicU32, Ordering};
    static MD5_CLASS_ID: AtomicU32 = AtomicU32::new(u32::MAX);

    let fallback = |ctx: &mut dyn NativeContext, args: &[Value]| {
        ctx.invoke_special_bytecode_only(
            "org/bouncycastle/crypto/digests/GeneralDigest",
            "update",
            "([BII)V",
            args,
        )
    };

    let (
        Some(Value::Object(Some(this_obj))),
        Some(Value::Object(Some(in_arr))),
        Some(Value::Int(in_off)),
        Some(Value::Int(len)),
    ) = (
        args.first().copied(),
        args.get(1).copied(),
        args.get(2).copied(),
        args.get(3).copied(),
    )
    else {
        return fallback(ctx, args);
    };

    if len < 64 || in_off < 0 {
        return fallback(ctx, args);
    }

    let cid = ctx.class_id_of_object(this_obj);
    let cached = MD5_CLASS_ID.load(Ordering::Relaxed);
    if cached != cid.as_u32() {
        if ctx.class_name_arc_of_id(cid).as_deref()
            != Some("org/bouncycastle/crypto/digests/MD5Digest")
        {
            return fallback(ctx, args);
        }
        MD5_CLASS_ID.store(cid.as_u32(), Ordering::Relaxed);
    }

    let arr_len = ctx.array_length(in_arr);
    let off_u = in_off as usize;
    let len_u = len as usize;
    if off_u.checked_add(len_u).map_or(true, |end| end > arr_len) {
        return fallback(ctx, args);
    }
    let Value::Int(xbuf_off) = ctx.get_field_by_name(this_obj, "xBufOff") else {
        return fallback(ctx, args);
    };
    let Value::Int(x_off) = ctx.get_field_by_name(this_obj, "xOff") else {
        return fallback(ctx, args);
    };
    if xbuf_off != 0 || x_off != 0 {
        return fallback(ctx, args);
    }
    let (Value::Int(h1), Value::Int(h2), Value::Int(h3), Value::Int(h4)) = (
        ctx.get_field_by_name(this_obj, "H1"),
        ctx.get_field_by_name(this_obj, "H2"),
        ctx.get_field_by_name(this_obj, "H3"),
        ctx.get_field_by_name(this_obj, "H4"),
    ) else {
        return fallback(ctx, args);
    };
    let Value::Object(Some(x_arr)) = ctx.get_field_by_name(this_obj, "X") else {
        return fallback(ctx, args);
    };
    let Value::Object(Some(xbuf_arr)) = ctx.get_field_by_name(this_obj, "xBuf") else {
        return fallback(ctx, args);
    };
    let Value::Long(byte_count) = ctx.get_field_by_name(this_obj, "byteCount") else {
        return fallback(ctx, args);
    };

    let mut state = [h1 as u32, h2 as u32, h3 as u32, h4 as u32];
    let mut x = [0u32; 16];
    let blocks = len_u / 64;
    let full_bytes = blocks * 64;
    let mut buf = [0u8; 4096];
    let mut processed = 0usize;
    while processed < full_bytes {
        let chunk = (full_bytes - processed).min(buf.len());
        let read = ctx.read_byte_array_into(in_arr, off_u + processed, &mut buf[..chunk]);
        if read != chunk {
            return Err(RuntimeError::IllegalStateException {
                message: "MD5Digest: short read".into(),
            }
            .into());
        }
        for blk in buf[..chunk].chunks_exact(64) {
            for (dst, w) in x.iter_mut().zip(blk.chunks_exact(4)) {
                *dst = u32::from_le_bytes([w[0], w[1], w[2], w[3]]);
            }
            cratonvm_native_builtins_crypto::bc_digest::md5_process_block(&mut state, &mut x);
        }
        processed += chunk;
    }

    let _ = ctx.set_field_by_name(this_obj, "H1", Value::Int(state[0] as i32));
    let _ = ctx.set_field_by_name(this_obj, "H2", Value::Int(state[1] as i32));
    let _ = ctx.set_field_by_name(this_obj, "H3", Value::Int(state[2] as i32));
    let _ = ctx.set_field_by_name(this_obj, "H4", Value::Int(state[3] as i32));

    let rem = len_u - full_bytes;
    if rem > 0 {
        let read = ctx.read_byte_array_into(in_arr, off_u + full_bytes, &mut buf[..rem]);
        if read != rem {
            return Err(RuntimeError::IllegalStateException {
                message: "MD5Digest: short tail read".into(),
            }
            .into());
        }
        let rem_words = rem / 4;
        if rem_words > 0 {
            let mut words = [0i32; 16];
            for i in 0..rem_words {
                let w = &buf[i * 4..i * 4 + 4];
                words[i] = i32::from_le_bytes([w[0], w[1], w[2], w[3]]);
            }
            ctx.write_int_array_from(x_arr, 0, &words[..rem_words]);
            let _ = ctx.set_field_by_name(this_obj, "xOff", Value::Int(rem_words as i32));
        }
        let rem_bytes = rem % 4;
        if rem_bytes > 0 {
            ctx.write_byte_array_from(xbuf_arr, 0, &buf[rem_words * 4..rem]);
            let _ = ctx.set_field_by_name(this_obj, "xBufOff", Value::Int(rem_bytes as i32));
        }
    }
    let _ = ctx.set_field_by_name(
        this_obj,
        "byteCount",
        Value::Long(byte_count.wrapping_add(len as i64)),
    );
    Ok(None)
}

pub fn register_bc_md5_digest(r: &mut NativeMethodRegistry) {
            r.register(
        "org/bouncycastle/crypto/digests/GeneralDigest",
        "update",
        "([BII)V",
        bc_general_digest_update, cratonvm_native_api::NativeKind::Intrinsic,
    );
        r.register(
        "org/bouncycastle/crypto/digests/MD5Digest",
        "processBlock",
        "()V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let slots = bc_md5_slots(ctx, this)?;
            let mut read = [Value::Int(0); 5];
            let mut read_slots = [(0usize, 0u8); 5];
            read_slots[..4].copy_from_slice(&slots.h);
            read_slots[4] = (slots.x, b'[');
            ctx.get_fields_typed(this, &read_slots, &mut read);

            let mut state = [0u32; 4];
            for (slot, value) in state.iter_mut().zip(read.iter()) {
                match value {
                    Value::Int(v) => *slot = *v as u32,
                    _ => {
                        return Err(RuntimeError::IllegalStateException {
                            message: "MD5Digest: malformed chaining word".into(),
                        }
                        .into())
                    }
                }
            }
            let x_arr = match read[4] {
                Value::Object(Some(o)) => o,
                _ => {
                    return Err(RuntimeError::IllegalStateException {
                        message: "MD5Digest: missing X".into(),
                    }
                    .into())
                }
            };
            if ctx.array_length(x_arr) != 16 {
                return Err(RuntimeError::IllegalStateException {
                    message: "MD5Digest: X is not a 16-word int[]".into(),
                }
                .into());
            }
            let mut words = [0i32; 16];
            if ctx.read_int_array_into(x_arr, 0, &mut words) != 16 {
                return Err(RuntimeError::IllegalStateException {
                    message: "MD5Digest: X is not a 16-word int[]".into(),
                }
                .into());
            }
            let mut x = [0u32; 16];
            for (dst, src) in x.iter_mut().zip(words.iter()) {
                *dst = *src as u32;
            }

            cratonvm_native_builtins_crypto::bc_digest::md5_process_block(&mut state, &mut x);

            words.fill(0);
            ctx.write_int_array_from(x_arr, 0, &words);

            let mut write = [Value::Int(0); 5];
            let mut write_slots = [(0usize, 0u8); 5];
            write_slots[..4].copy_from_slice(&slots.h);
            write_slots[4] = slots.x_off;
            for (dst, word) in write.iter_mut().zip(state.iter()) {
                *dst = Value::Int(*word as i32);
            }
            write[4] = Value::Int(0);
            ctx.set_fields_typed(this, &write_slots, &write);
            Ok(None)
        },
        cratonvm_native_api::NativeKind::Intrinsic,
    );
}

pub fn register_bc_long_digest(r: &mut NativeMethodRegistry) {
    r.register(
        "org/bouncycastle/crypto/digests/LongDigest",
        "processBlock",
        "()V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let slots = bc_long_digest_slots(ctx, this)?;

            let mut read = [Value::Long(0); 11];
            let mut read_slots = [(0usize, 0u8); 11];
            read_slots[..8].copy_from_slice(&slots.h);
            read_slots[8] = (slots.w, b'[');
            read_slots[9] = slots.bc1;
            read_slots[10] = slots.bc2;
            ctx.get_fields_typed(this, &read_slots, &mut read);

            let mut state = [0u64; 8];
            for (slot, value) in state.iter_mut().zip(read[..8].iter()) {
                match value {
                    Value::Long(v) => *slot = *v as u64,
                    _ => {
                        return Err(RuntimeError::IllegalStateException {
                            message: "LongDigest: malformed chaining word".into(),
                        }
                        .into())
                    }
                }
            }
            let w_arr = match read[8] {
                Value::Object(Some(o)) => o,
                _ => {
                    return Err(RuntimeError::IllegalStateException {
                        message: "LongDigest: missing W".into(),
                    }
                    .into())
                }
            };
            let mut bc1 = match read[9] {
                Value::Long(v) => v,
                _ => 0,
            };
            let mut bc2 = match read[10] {
                Value::Long(v) => v,
                _ => 0,
            };
            if bc1 > 0x1fff_ffff_ffff_ffff {
                bc2 = bc2.wrapping_add(((bc1 as u64) >> 61) as i64);
                bc1 &= 0x1fff_ffff_ffff_ffff;
            }

            let mut w_bytes = [0u8; 128];
            if ctx.read_primitive_array_bytes(w_arr, 0, &mut w_bytes) != 128 {
                return Err(RuntimeError::IllegalStateException {
                    message: "LongDigest: W is not a long[16+]".into(),
                }
                .into());
            }
            let mut w = [0u64; 16];
            for (dst, chunk) in w.iter_mut().zip(w_bytes.chunks_exact(8)) {
                *dst = u64::from_ne_bytes(chunk.try_into().unwrap());
            }

            cratonvm_native_builtins_crypto::bc_digest::sha512_compress(&mut state, &w);

            ctx.write_primitive_array_bytes(w_arr, 0, &[0u8; 128]);

            let mut write = [Value::Long(0); 11];
            let mut write_slots = [(0usize, 0u8); 11];
            write_slots[..8].copy_from_slice(&slots.h);
            write_slots[8] = slots.w_off;
            write_slots[9] = slots.bc1;
            write_slots[10] = slots.bc2;
            for (dst, word) in write[..8].iter_mut().zip(state.iter()) {
                *dst = Value::Long(*word as i64);
            }
            write[8] = Value::Int(0);
            write[9] = Value::Long(bc1);
            write[10] = Value::Long(bc2);
            ctx.set_fields_typed(this, &write_slots, &write);
            Ok(None)
        },
        cratonvm_native_api::NativeKind::Intrinsic,
    );
}

pub const BC_KECCAK_ROUND_CONSTANTS: [u64; 24] = [
    0x0000_0000_0000_0001,
    0x0000_0000_0000_8082,
    0x8000_0000_0000_808a,
    0x8000_0000_8000_8000,
    0x0000_0000_0000_808b,
    0x0000_0000_8000_0001,
    0x8000_0000_8000_8081,
    0x8000_0000_0000_8009,
    0x0000_0000_0000_008a,
    0x0000_0000_0000_0088,
    0x0000_0000_8000_8009,
    0x0000_0000_8000_000a,
    0x0000_0000_8000_808b,
    0x8000_0000_0000_008b,
    0x8000_0000_0000_8089,
    0x8000_0000_0000_8003,
    0x8000_0000_0000_8002,
    0x8000_0000_0000_0080,
    0x0000_0000_0000_800a,
    0x8000_0000_8000_000a,
    0x8000_0000_8000_8081,
    0x8000_0000_0000_8080,
    0x0000_0000_8000_0001,
    0x8000_0000_8000_8008,
];

pub fn bc_keccak_bad_state(message: &str) -> MethodCallFailed {
    RuntimeError::IllegalStateException {
        message: message.into(),
    }
    .into()
}

pub fn bc_keccak_state_array(
    ctx: &dyn NativeContext,
    this: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    let state_arr = match ctx.get_field_by_name(this, "state") {
        Value::Object(Some(o)) => o,
        _ => return Err(bc_keccak_bad_state("KeccakDigest: missing state")),
    };
    if ctx.array_length(state_arr) < 25 {
        return Err(RuntimeError::aioobe_index_only(25).into());
    }
    Ok(state_arr)
}

pub fn bc_keccak_data_queue(
    ctx: &dyn NativeContext,
    this: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    match ctx.get_field_by_name(this, "dataQueue") {
        Value::Object(Some(o)) => Ok(o),
        _ => Err(bc_keccak_bad_state("KeccakDigest: missing dataQueue")),
    }
}

pub fn bc_keccak_int_field(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    field: &str,
) -> Result<i32, MethodCallFailed> {
    match ctx.get_field_by_name(this, field) {
        Value::Int(v) => Ok(v),
        _ => Err(bc_keccak_bad_state("KeccakDigest: malformed int field")),
    }
}

pub fn bc_keccak_read_state(
    ctx: &dyn NativeContext,
    state_arr: ObjectRef,
) -> Result<[u64; 25], MethodCallFailed> {
    let mut state = [0u64; 25];
    for (i, slot) in state.iter_mut().enumerate() {
        match ctx.get_array_element(state_arr, i) {
            Value::Long(v) => *slot = v as u64,
            _ => return Err(bc_keccak_bad_state("KeccakDigest: malformed state")),
        }
    }
    Ok(state)
}

pub fn bc_keccak_write_state(
    ctx: &dyn NativeContext,
    state_arr: ObjectRef,
    state: &[u64; 25],
) {
    for (i, &word) in state.iter().enumerate() {
        ctx.set_array_element(state_arr, i, Value::Long(word as i64));
    }
}

pub fn bc_keccak_permute(state: &mut [u64; 25]) {
    let mut a00 = state[0];
    let mut a01 = state[1];
    let mut a02 = state[2];
    let mut a03 = state[3];
    let mut a04 = state[4];
    let mut a05 = state[5];
    let mut a06 = state[6];
    let mut a07 = state[7];
    let mut a08 = state[8];
    let mut a09 = state[9];
    let mut a10 = state[10];
    let mut a11 = state[11];
    let mut a12 = state[12];
    let mut a13 = state[13];
    let mut a14 = state[14];
    let mut a15 = state[15];
    let mut a16 = state[16];
    let mut a17 = state[17];
    let mut a18 = state[18];
    let mut a19 = state[19];
    let mut a20 = state[20];
    let mut a21 = state[21];
    let mut a22 = state[22];
    let mut a23 = state[23];
    let mut a24 = state[24];

    for rc in BC_KECCAK_ROUND_CONSTANTS {
        let mut c0 = a00 ^ a05 ^ a10 ^ a15 ^ a20;
        let mut c1 = a01 ^ a06 ^ a11 ^ a16 ^ a21;
        let c2 = a02 ^ a07 ^ a12 ^ a17 ^ a22;
        let c3 = a03 ^ a08 ^ a13 ^ a18 ^ a23;
        let c4 = a04 ^ a09 ^ a14 ^ a19 ^ a24;

        let d1 = c1.rotate_left(1) ^ c4;
        let d2 = c2.rotate_left(1) ^ c0;
        let d3 = c3.rotate_left(1) ^ c1;
        let d4 = c4.rotate_left(1) ^ c2;
        let d0 = c0.rotate_left(1) ^ c3;

        a00 ^= d1;
        a05 ^= d1;
        a10 ^= d1;
        a15 ^= d1;
        a20 ^= d1;
        a01 ^= d2;
        a06 ^= d2;
        a11 ^= d2;
        a16 ^= d2;
        a21 ^= d2;
        a02 ^= d3;
        a07 ^= d3;
        a12 ^= d3;
        a17 ^= d3;
        a22 ^= d3;
        a03 ^= d4;
        a08 ^= d4;
        a13 ^= d4;
        a18 ^= d4;
        a23 ^= d4;
        a04 ^= d0;
        a09 ^= d0;
        a14 ^= d0;
        a19 ^= d0;
        a24 ^= d0;

        c1 = a01.rotate_left(1);
        a01 = a06.rotate_left(44);
        a06 = a09.rotate_left(20);
        a09 = a22.rotate_left(61);
        a22 = a14.rotate_left(39);
        a14 = a20.rotate_left(18);
        a20 = a02.rotate_left(62);
        a02 = a12.rotate_left(43);
        a12 = a13.rotate_left(25);
        a13 = a19.rotate_left(8);
        a19 = a23.rotate_left(56);
        a23 = a15.rotate_left(41);
        a15 = a04.rotate_left(27);
        a04 = a24.rotate_left(14);
        a24 = a21.rotate_left(2);
        a21 = a08.rotate_left(55);
        a08 = a16.rotate_left(45);
        a16 = a05.rotate_left(36);
        a05 = a03.rotate_left(28);
        a03 = a18.rotate_left(21);
        a18 = a17.rotate_left(15);
        a17 = a11.rotate_left(10);
        a11 = a07.rotate_left(6);
        a07 = a10.rotate_left(3);
        a10 = c1;

        c0 = a00 ^ (!a01 & a02);
        c1 = a01 ^ (!a02 & a03);
        a02 ^= !a03 & a04;
        a03 ^= !a04 & a00;
        a04 ^= !a00 & a01;
        a00 = c0;
        a01 = c1;

        c0 = a05 ^ (!a06 & a07);
        c1 = a06 ^ (!a07 & a08);
        a07 ^= !a08 & a09;
        a08 ^= !a09 & a05;
        a09 ^= !a05 & a06;
        a05 = c0;
        a06 = c1;

        c0 = a10 ^ (!a11 & a12);
        c1 = a11 ^ (!a12 & a13);
        a12 ^= !a13 & a14;
        a13 ^= !a14 & a10;
        a14 ^= !a10 & a11;
        a10 = c0;
        a11 = c1;

        c0 = a15 ^ (!a16 & a17);
        c1 = a16 ^ (!a17 & a18);
        a17 ^= !a18 & a19;
        a18 ^= !a19 & a15;
        a19 ^= !a15 & a16;
        a15 = c0;
        a16 = c1;

        c0 = a20 ^ (!a21 & a22);
        c1 = a21 ^ (!a22 & a23);
        a22 ^= !a23 & a24;
        a23 ^= !a24 & a20;
        a24 ^= !a20 & a21;
        a20 = c0;
        a21 = c1;

        a00 ^= rc;
    }

    *state = [
        a00, a01, a02, a03, a04, a05, a06, a07, a08, a09, a10, a11, a12, a13, a14, a15, a16, a17,
        a18, a19, a20, a21, a22, a23, a24,
    ];
}

pub fn register_bc_keccak_digest(r: &mut NativeMethodRegistry) {
        
    bc_register_rerun!(r, "org/bouncycastle/crypto/digests/KeccakDigest", "KeccakPermutation", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let state_arr = bc_keccak_state_array(ctx, this)?;
        let mut state = bc_keccak_read_state(ctx, state_arr)?;
        bc_keccak_permute(&mut state);
        bc_keccak_write_state(ctx, state_arr, &state);
        Ok(None)
    });

    bc_register_rerun!(r, "org/bouncycastle/crypto/digests/KeccakDigest", "KeccakAbsorb", "([BI)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let data_arr = obj_arg(args, 1)?;
        let off = match args.get(2) {
            Some(Value::Int(v)) if *v >= 0 => *v as usize,
            Some(Value::Int(v)) => return Err(RuntimeError::aioobe_index_only(*v).into()),
            _ => return Err(RuntimeError::aioobe_index_only(-1).into()),
        };
        let rate = bc_keccak_int_field(ctx, this, "rate")?;
        let count = (rate as usize) >> 6;
        let byte_count = count * 8;
        if off
            .checked_add(byte_count)
            .map_or(true, |end| end > ctx.array_length(data_arr))
        {
            return Err(RuntimeError::aioobe_index_only(off as i32).into());
        }

        let state_arr = bc_keccak_state_array(ctx, this)?;
        let mut state = bc_keccak_read_state(ctx, state_arr)?;
        let mut block = vec![0u8; byte_count];
        ctx.read_byte_array_into(data_arr, off, &mut block);
        for (i, chunk) in block.chunks_exact(8).enumerate() {
            state[i] ^= u64::from_le_bytes([
                chunk[0], chunk[1], chunk[2], chunk[3], chunk[4], chunk[5], chunk[6], chunk[7],
            ]);
        }
        bc_keccak_permute(&mut state);
        bc_keccak_write_state(ctx, state_arr, &state);
        Ok(None)
    });

    bc_register_rerun!(r, "org/bouncycastle/crypto/digests/KeccakDigest", "KeccakExtract", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let rate = bc_keccak_int_field(ctx, this, "rate")?;
        let count = (rate as usize) >> 6;
        let state_arr = bc_keccak_state_array(ctx, this)?;
        let data_queue = bc_keccak_data_queue(ctx, this)?;
        // Refuse a short queue BEFORE the state is written: the refusal
        // re-runs the bytecode (`bc_register_rerun!`), which must start from
        // the state this call started from (wave 103, lane L7).
        if ctx.array_length(data_queue) < count * 8 {
            return Err(RuntimeError::aioobe_index_only((count * 8) as i32).into());
        }
        let mut state = bc_keccak_read_state(ctx, state_arr)?;
        bc_keccak_permute(&mut state);
        bc_keccak_write_state(ctx, state_arr, &state);

        let mut out = vec![0u8; count * 8];
        for i in 0..count {
            out[i * 8..i * 8 + 8].copy_from_slice(&state[i].to_le_bytes());
        }
        if !ctx.write_byte_array_from(data_queue, 0, &out) {
            return Err(RuntimeError::aioobe_index_only(out.len() as i32).into());
        }
        ctx.set_field_by_name(this, "bitsInQueue", Value::Int(rate));
        Ok(None)
    });

    }

pub fn bc_sic_bad_state(message: &str) -> MethodCallFailed {
    RuntimeError::IllegalStateException {
        message: message.into(),
    }
    .into()
}

pub fn bc_sic_object_field(
    ctx: &dyn NativeContext,
    obj: ObjectRef,
    name: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    match ctx.get_field_by_name(obj, name) {
        Value::Object(Some(o)) => Ok(o),
        _ => Err(bc_sic_bad_state("SICBlockCipher: malformed object field")),
    }
}

pub fn bc_sic_write_reset_counter(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    counter_arr: ObjectRef,
    iv_arr: ObjectRef,
) -> Result<Vec<u8>, MethodCallFailed> {
    let bs = ctx.array_length(counter_arr);
    let iv_len = ctx.array_length(iv_arr);
    if iv_len > bs {
        return Err(RuntimeError::IllegalArgumentException {
            message: format!("CTR/SIC mode requires IV no greater than: {bs} bytes."),
        }
        .into());
    }
    let mut counter = vec![0u8; bs];
    if iv_len != 0 {
        ctx.read_byte_array_into(iv_arr, 0, &mut counter[..iv_len]);
    }
    ctx.write_byte_array_from(counter_arr, 0, &counter);
    ctx.set_field_by_name(this, "byteCount", Value::Int(0));
    Ok(counter)
}

pub fn bc_sic_add_blocks(counter: &mut [u8], mut blocks: u64) {
    let mut idx = counter.len();
    while blocks != 0 && idx != 0 {
        idx -= 1;
        let add = (blocks & 0xff) as u16;
        let sum = counter[idx] as u16 + add;
        counter[idx] = sum as u8;
        blocks = (blocks >> 8) + u64::from(sum >> 8);
    }
}

pub fn bc_sic_increment_counter(counter: &mut [u8]) {
    let mut idx = counter.len();
    while idx != 0 {
        idx -= 1;
        counter[idx] = counter[idx].wrapping_add(1);
        if counter[idx] != 0 {
            break;
        }
    }
}

pub fn bc_sic_check_counter_prefix(
    ctx: &dyn NativeContext,
    counter: &[u8],
    iv_arr: ObjectRef,
) -> Result<(), MethodCallFailed> {
    let iv_len = ctx.array_length(iv_arr);
    if iv_len < counter.len() {
        let mut iv = vec![0u8; iv_len];
        ctx.read_byte_array_into(iv_arr, 0, &mut iv);
        for i in 0..iv_len {
            if counter[i] != iv[i] {
                return Err(bc_sic_bad_state("Counter in CTR/SIC mode out of range."));
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// SICBlockCipher's 2^64-block range bound
// ---------------------------------------------------------------------------
//
// `SICBlockCipher` enforces two DIFFERENT bounds, and the shims below used to
// implement one of them.
//
// * **Partial IV** (`IV.length < blockSize`): the counter may only occupy the
//   low `blockSize - IV.length` bytes, so the IV prefix must still match. That
//   is `bc_sic_check_counter_prefix`, and it was implemented.
// * **Full-block IV**: there is no prefix to compare, so the class tracks the
//   ADVANCE SINCE INIT instead and refuses at 2^64 blocks — `fullBlockIV`,
//   `guardByte`, `used` and the sticky `overflow` flag, checked per block by
//   `checkLastIncrement` and per skip/seekTo by `checkCounter`. Every one of
//   those four fields was invisible to these natives: `init` never wrote
//   `fullBlockIV`/`guardByte`, `reset` never cleared `used`/`overflow`, and
//   `processBytes`' inline `checkLastIncrement` carried the comment
//   "no-op when the IV fills the block", which is the branch it is not.
//
// The visible consequence, measured against HotSpot with the same bc-java jar
// (`SICPositionTest`, in `crypto.test.AllTests`): after `skip` correctly threw
// `Counter in CTR/SIC mode out of range.` — `skip` is Java and calls
// `checkCounter` — the very next `processBytes` produced KEYSTREAM instead of
// re-throwing, because the sticky flag the Java had just set was one this
// native never read. A CTR keystream reused past its bound is a two-time-pad,
// so this is a silent-wrong-crypto shape, not only a missing exception.
//
// The helpers below mirror `populateDelta`, `checkLastIncrement`,
// `checkCounter` and `incrementCounter` arm-for-arm. Cost per block is one byte
// compare against `guardByte`; `populateDelta` runs only when that matches.

/// Mirror of `SICBlockCipher.populateDelta`: big-endian modular subtraction of
/// the IV (zero-padded on the right) from `counter` into `res`, carrying the
/// borrow across every byte. True when any byte ABOVE the low 8-byte lane is
/// non-zero — the counter has advanced 2^64 blocks or more since init, or has
/// moved below its starting value.
pub fn bc_sic_populate_delta(counter: &[u8], iv: &[u8], lane_off: usize, res: &mut [u8]) -> bool {
    let mut borrow = 0i32;
    for i in (0..res.len()).rev() {
        let mut v = counter[i] as i32 - borrow;
        if i < iv.len() {
            v -= iv[i] as i32;
        }
        if v < 0 {
            v += 256;
            borrow = 1;
        } else {
            borrow = 0;
        }
        res[i] = v as u8;
    }
    res[..lane_off.min(res.len())].iter().any(|&b| b != 0)
}

/// The four range-bound fields of a `SICBlockCipher`, read once per native call
/// and written back only when they changed.
pub struct BcSicRange {
    pub lane_off: usize,
    pub guard_byte: u8,
    pub full_block_iv: bool,
    pub overflow: bool,
    pub used: u64,
    pub dirty: bool,
}

impl BcSicRange {
    pub fn read(ctx: &dyn NativeContext, this: ObjectRef) -> Self {
        let int_field = |name: &str| match ctx.get_field_by_name(this, name) {
            Value::Int(v) => v,
            _ => 0,
        };
        BcSicRange {
            lane_off: int_field("laneOff").max(0) as usize,
            guard_byte: int_field("guardByte") as u8,
            full_block_iv: int_field("fullBlockIV") != 0,
            overflow: int_field("overflow") != 0,
            used: match ctx.get_field_by_name(this, "used") {
                Value::Long(v) => v as u64,
                _ => 0,
            },
            dirty: false,
        }
    }

    pub fn write_back(&self, ctx: &mut dyn NativeContext, this: ObjectRef) {
        if !self.dirty {
            return;
        }
        ctx.set_field_by_name(this, "overflow", Value::Int(i32::from(self.overflow)));
        ctx.set_field_by_name(this, "used", Value::Long(self.used as i64));
    }

    /// Mirror of `SICBlockCipher.checkLastIncrement`, run before each keystream
    /// block is produced.
    pub fn check_last_increment(
        &mut self,
        counter: &[u8],
        iv: &[u8],
        delta: &mut [u8],
    ) -> Result<(), MethodCallFailed> {
        let out_of_range = || bc_sic_bad_state("Counter in CTR/SIC mode out of range.");
        if iv.len() < counter.len() {
            if counter[iv.len() - 1] != iv[iv.len() - 1] {
                return Err(out_of_range());
            }
        } else if self.lane_off > 0 {
            if self.overflow
                || (counter[self.lane_off - 1] == self.guard_byte
                    && bc_sic_populate_delta(counter, iv, self.lane_off, delta))
            {
                self.overflow = true;
                self.dirty = true;
                return Err(out_of_range());
            }
        } else if self.overflow {
            return Err(out_of_range());
        }
        Ok(())
    }

    /// Mirror of the `used`/`overflow` half of `SICBlockCipher.incrementCounter`.
    /// The counter bytes themselves are advanced by `bc_sic_increment_counter`.
    pub fn note_increment(&mut self) {
        if self.lane_off == 0 {
            self.used = self.used.wrapping_add(1);
            if self.used == 0 && self.full_block_iv {
                self.overflow = true;
            }
            self.dirty = true;
        }
    }

    /// Mirror of `SICBlockCipher.checkCounter`, run per `skip`/`seekTo`.
    pub fn check_counter(
        &mut self,
        counter: &[u8],
        iv: &[u8],
        delta: &mut [u8],
    ) -> Result<(), MethodCallFailed> {
        let out_of_range = || bc_sic_bad_state("Counter in CTR/SIC mode out of range.");
        if iv.len() < counter.len() {
            for i in (0..iv.len()).rev() {
                if counter[i] != iv[i] {
                    return Err(out_of_range());
                }
            }
        } else {
            if self.overflow || bc_sic_populate_delta(counter, iv, self.lane_off, delta) {
                self.overflow = true;
                self.dirty = true;
                return Err(out_of_range());
            }
            if self.lane_off == 0 {
                let n = delta.len();
                let mut acc = 0u64;
                for &b in &delta[n - 8..] {
                    acc = (acc << 8) | b as u64;
                }
                self.used = acc;
                self.dirty = true;
            }
        }
        Ok(())
    }
}

/// `reset()`'s share of the range state: `used = 0`, `overflow = false`.
/// Written unconditionally — this is the only path that CLEARS a sticky flag,
/// so it must not be elided by the dirty check.
pub fn bc_sic_reset_range(ctx: &mut dyn NativeContext, this: ObjectRef) {
    ctx.set_field_by_name(this, "used", Value::Long(0));
    ctx.set_field_by_name(this, "overflow", Value::Int(0));
}

pub fn bc_sic_encrypt_counter(
    ctx: &mut dyn NativeContext,
    cipher: ObjectRef,
    counter_arr: ObjectRef,
    counter_out_arr: ObjectRef,
    counter: &[u8],
) -> MethodCallResult {
    let is_aes = ctx
        .class_name_arc_of_id(ctx.class_id_of_object(cipher))
        .as_deref()
        == Some("org/bouncycastle/crypto/engines/AESEngine");
    if is_aes {
        if let Value::Object(Some(wk)) = ctx.get_field_by_name(cipher, "WorkingKey") {
            let kw = read_aes_kw(ctx, wk);
            if kw.len() >= 2 {
                let mut out = vec![0u8; counter.len()];
                crate::bc_aes::encrypt_block(&kw, counter, &mut out);
                ctx.write_byte_array_from(counter_out_arr, 0, &out);
                return Ok(Some(Value::Int(counter.len() as i32)));
            }
        }
    }

    ctx.write_byte_array_from(counter_arr, 0, counter);
    ctx.invoke_virtual(
        cipher,
        "processBlock",
        "([BI[BI)I",
        &[
            Value::Object(Some(counter_arr)),
            Value::Int(0),
            Value::Object(Some(counter_out_arr)),
            Value::Int(0),
        ],
    )
}

/// Native fast-path for `org.bouncycastle.crypto.modes.SICBlockCipher.processBytes`
/// (CTR mode). Once the AES engine is native, this per-byte XOR loop is the sole
/// remaining hot frame in `AESTest.testCounter` (profiled: ~112s vs ~0.4s for the
/// String half). Faithful reimplementation of the bytecode loop:
///   if byteCount==0 { checkLastIncrement(); cipher.processBlock(counter,counterOut);
///                     next = in ^ counterOut[byteCount++]; }
///   else { next = in ^ counterOut[byteCount++];
///          if byteCount==counter.length { byteCount=0; incrementCounter(); } }
/// For AES (the common case) the keystream block is produced natively from the
/// engine's `WorkingKey` (no per-block invoke, GC-safe — all loop state is in Rust
/// locals). For any other underlying cipher it falls back to invoking
/// `cipher.processBlock` per block (handles held across the invoke, matching the
/// apps_h2.rs convention; the default young GC is non-moving). `checkLastIncrement`
/// is a no-op when the IV fills the block (AESTest's case).
pub fn register_bc_sic_ctr(r: &mut NativeMethodRegistry) {
        
    bc_register_null_rerun!(
        r,
        "org/bouncycastle/crypto/modes/SICBlockCipher",
        "processBlock",
        "([BI[BI)I",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let in_arr = obj_arg(args, 1)?;
            let in_off_i = match args.get(2) {
                Some(Value::Int(v)) => *v,
                _ => 0,
            };
            let out_arr = obj_arg(args, 3)?;
            let out_off_i = match args.get(4) {
                Some(Value::Int(v)) => *v,
                _ => 0,
            };
            let obj_field = |name: &str| match ctx.get_field_by_name(this, name) {
                Value::Object(Some(o)) => Some(o),
                _ => None,
            };
            let cipher = obj_field("cipher").ok_or_else(|| bc_sic_bad_state("SIC: null cipher"))?;
            let counter_arr =
                obj_field("counter").ok_or_else(|| bc_sic_bad_state("SIC: null counter"))?;
            let counter_out_arr =
                obj_field("counterOut").ok_or_else(|| bc_sic_bad_state("SIC: null counterOut"))?;
            let byte_count = match ctx.get_field_by_name(this, "byteCount") {
                Value::Int(v) => v,
                _ => 0,
            };
            let bs = ctx.array_length(counter_arr);
            if bs == 0 {
                return Err(bc_sic_bad_state("SIC: zero block size"));
            }
            if byte_count != 0 {
                return ctx.invoke_virtual(
                    this,
                    "processBytes",
                    "([BII[BI)I",
                    &[
                        Value::Object(Some(in_arr)),
                        Value::Int(in_off_i),
                        Value::Int(bs as i32),
                        Value::Object(Some(out_arr)),
                        Value::Int(out_off_i),
                    ],
                );
            }
            if in_off_i < 0
                || out_off_i < 0
                || (in_off_i as usize).saturating_add(bs) > ctx.array_length(in_arr)
            {
                return Err(bc_gost_throw_crypto_exception(
                    ctx,
                    "org/bouncycastle/crypto/DataLengthException",
                    "input buffer too small",
                ));
            }
            if (out_off_i as usize).saturating_add(bs) > ctx.array_length(out_arr) {
                return Err(bc_gost_throw_crypto_exception(
                    ctx,
                    "org/bouncycastle/crypto/OutputLengthException",
                    "output buffer too short",
                ));
            }

            let mut counter = vec![0u8; bs];
            ctx.read_byte_array_into(counter_arr, 0, &mut counter);
            // `processBlock` calls `checkLastIncrement` before producing a
            // block, exactly as `processBytes` does — this native did not, so a
            // counter past its bound still yielded keystream through this door
            // even after the sibling one was fixed. Same predicate, both doors.
            let iv_arr = obj_field("IV").ok_or_else(|| bc_sic_bad_state("SIC: null IV"))?;
            let iv_len = ctx.array_length(iv_arr);
            let mut iv = vec![0u8; iv_len];
            ctx.read_byte_array_into(iv_arr, 0, &mut iv);
            let mut range = BcSicRange::read(ctx, this);
            let mut delta = vec![0u8; bs];
            if let Err(e) = range.check_last_increment(&counter, &iv, &mut delta) {
                range.write_back(ctx, this);
                return Err(e);
            }
            // gc-common w20-e: for a non-AES engine `bc_sic_encrypt_counter`
            // dispatches its `processBlock` (Java); every array and the
            // receiver were used through their entry addresses after it.
            let this_pin = ctx.pin_native_root(this);
            let in_pin = ctx.pin_native_root(in_arr);
            let out_pin = ctx.pin_native_root(out_arr);
            let counter_pin = ctx.pin_native_root(counter_arr);
            let counter_out_pin = ctx.pin_native_root(counter_out_arr);
            bc_sic_encrypt_counter(ctx, cipher, counter_arr, counter_out_arr, &counter)?;
            let this = ctx.read_native_pin(this_pin, this);
            let in_arr = ctx.read_native_pin(in_pin, in_arr);
            let out_arr = ctx.read_native_pin(out_pin, out_arr);
            let counter_arr = ctx.read_native_pin(counter_pin, counter_arr);
            let counter_out_arr = ctx.read_native_pin(counter_out_pin, counter_out_arr);
            ctx.unpin_native_roots(this_pin);
            let mut counter_out = vec![0u8; bs];
            ctx.read_byte_array_into(counter_out_arr, 0, &mut counter_out);
            let mut input = vec![0u8; bs];
            ctx.read_byte_array_into(in_arr, in_off_i as usize, &mut input);
            for i in 0..bs {
                input[i] ^= counter_out[i];
            }
            ctx.write_byte_array_from(out_arr, out_off_i as usize, &input);
            bc_sic_increment_counter(&mut counter);
            range.note_increment();
            ctx.write_byte_array_from(counter_arr, 0, &counter);
            range.write_back(ctx, this);
            Ok(Some(Value::Int(bs as i32)))
        },
    );

    bc_register_null_rerun!(
        r,
        "org/bouncycastle/crypto/modes/SICBlockCipher",
        "processBytes",
        "([BII[BI)I",
        |ctx, args| {
            let arg_i32 = |i: usize| -> i32 {
                match args.get(i) {
                    Some(Value::Int(v)) => *v,
                    _ => 0,
                }
            };
            let ise = |m: &str| -> MethodCallFailed {
                RuntimeError::IllegalStateException { message: m.into() }.into()
            };
            let this = obj_arg(args, 0)?;
            let in_arr = obj_arg(args, 1)?;
            let in_off = arg_i32(2);
            let len_i = arg_i32(3);
            let out_arr = obj_arg(args, 4)?;
            let out_off = arg_i32(5);

            let obj_field = |name: &str| match ctx.get_field_by_name(this, name) {
                Value::Object(Some(o)) => Some(o),
                _ => None,
            };
            let cipher = obj_field("cipher").ok_or_else(|| ise("SIC: null cipher"))?;
            let counter_arr = obj_field("counter").ok_or_else(|| ise("SIC: null counter"))?;
            let counter_out_arr =
                obj_field("counterOut").ok_or_else(|| ise("SIC: null counterOut"))?;
            let iv_arr = obj_field("IV").ok_or_else(|| ise("SIC: null IV"))?;
            let mut byte_count = match ctx.get_field_by_name(this, "byteCount") {
                Value::Int(v) => v,
                _ => 0,
            };

            let bs = ctx.array_length(counter_arr);
            if bs == 0 {
                return Err(ise("SIC: zero block size"));
            }
            // CTR misuse guard (BC throws DataLength/OutputLengthException; both
            // are RuntimeExceptions and this is a can't-happen path for valid use).
            if in_off < 0
                || len_i < 0
                || out_off < 0
                || (in_off as usize) + (len_i as usize) > ctx.array_length(in_arr)
                || (out_off as usize) + (len_i as usize) > ctx.array_length(out_arr)
            {
                return Err(ise("CTR/SIC buffer length out of range"));
            }
            let len = len_i as usize;

            let mut counter_stack = [0u8; 64];
            let mut counter_heap = if bs > 64 { vec![0u8; bs] } else { Vec::new() };
            let counter: &mut [u8] = if bs <= 64 { &mut counter_stack[..bs] } else { &mut counter_heap };
            ctx.read_byte_array_into(counter_arr, 0, counter);

            let mut keystream_stack = [0u8; 64];
            let mut keystream_heap = if bs > 64 { vec![0u8; bs] } else { Vec::new() };
            let keystream: &mut [u8] = if bs <= 64 { &mut keystream_stack[..bs] } else { &mut keystream_heap };
            ctx.read_byte_array_into(counter_out_arr, 0, keystream);

            let iv_len = ctx.array_length(iv_arr);
            let mut in_stack = [0u8; 256];
            let mut in_heap = if len > 256 { vec![0u8; len] } else { Vec::new() };
            let in_buf: &mut [u8] = if len <= 256 { &mut in_stack[..len] } else { &mut in_heap };
            ctx.read_byte_array_into(in_arr, in_off as usize, in_buf);

            let mut out_stack = [0u8; 256];
            let mut out_heap = if len > 256 { vec![0u8; len] } else { Vec::new() };
            let out_buf: &mut [u8] = if len <= 256 { &mut out_stack[..len] } else { &mut out_heap };

            let mut iv_stack = [0u8; 64];
            let mut iv_heap = if iv_len > 64 { vec![0u8; iv_len] } else { Vec::new() };
            let iv: &mut [u8] = if iv_len <= 64 { &mut iv_stack[..iv_len] } else { &mut iv_heap };
            ctx.read_byte_array_into(iv_arr, 0, iv);
            let mut range = BcSicRange::read(ctx, this);
            let mut delta_stack = [0u8; 64];
            let mut delta_heap = if bs > 64 { vec![0u8; bs] } else { Vec::new() };
            let delta: &mut [u8] = if bs <= 64 { &mut delta_stack[..bs] } else { &mut delta_heap };

            // AES fast path: produce the keystream block natively from WorkingKey.
            let is_aes = ctx
                .class_name_arc_of_id(ctx.class_id_of_object(cipher))
                .as_deref()
                == Some("org/bouncycastle/crypto/engines/AESEngine");
            let kw: AesKeySchedule = if is_aes {
                match ctx.get_field_by_name(cipher, "WorkingKey") {
                    Value::Object(Some(wk)) => read_aes_kw(ctx, wk),
                    _ => AesKeySchedule::empty(),
                }
            } else {
                AesKeySchedule::empty()
            };
            let use_aes = kw.len() >= 2;

            let (counter_arr, counter_out_arr, this, out_arr) = if use_aes {
                // No Java bytecode is invoked in the AES fast path: no GC points,
                // so no native root pinning is needed.
                let mut i = 0usize;
                let n_full = len / 16;
                if bs == 16
                    && byte_count == 0
                    && n_full > 0
                    && iv.len() == 16
                    && range.lane_off == 0
                    && !range.overflow
                    && (n_full as u64) < u64::MAX - range.used
                {
                    let batch_bytes = n_full * 16;
                    crate::bc_aes::encrypt_ctr_blocks(
                        &kw,
                        &mut counter[..16],
                        &in_buf[..batch_bytes],
                        &mut out_buf[..batch_bytes],
                    );
                    range.used += n_full as u64;
                    i = batch_bytes;
                }
                while i < len {
                    if byte_count == 0 {
                        if let Err(e) = range.check_last_increment(counter, iv, delta) {
                            range.write_back(ctx, this);
                            return Err(e);
                        }
                        crate::bc_aes::encrypt_block(&kw, counter, keystream);
                        if i + bs <= len {
                            for k in 0..bs {
                                out_buf[i + k] = in_buf[i + k] ^ keystream[k];
                            }
                            bc_sic_increment_counter(counter);
                            range.note_increment();
                            i += bs;
                            continue;
                        }
                        out_buf[i] = in_buf[i] ^ keystream[0];
                        byte_count = 1;
                        i += 1;
                    } else {
                        out_buf[i] = in_buf[i] ^ keystream[byte_count as usize];
                        byte_count += 1;
                        if byte_count as usize == bs {
                            byte_count = 0;
                            bc_sic_increment_counter(counter);
                            range.note_increment();
                        }
                        i += 1;
                    }
                }
                (counter_arr, counter_out_arr, this, out_arr)
            } else {
                // GC-safety: the non-AES arm below dispatches `processBlock` -- real
                // BouncyCastle bytecode -- once per block. Only re-read pins around
                // the actual `invoke_virtual` call and after the loop.
                let cipher_pin = ctx.pin_native_root(cipher);
                let counter_pin = ctx.pin_native_root(counter_arr);
                let counter_out_pin = ctx.pin_native_root(counter_out_arr);
                let this_pin = ctx.pin_native_root(this);
                let out_pin = ctx.pin_native_root(out_arr);
                let mut cur_cipher = cipher;
                let mut cur_counter_arr = counter_arr;
                let mut cur_counter_out_arr = counter_out_arr;
                for i in 0..len {
                    let next;
                    if byte_count == 0 {
                        if let Err(e) = range.check_last_increment(counter, iv, delta) {
                            let this_now = ctx.read_native_pin(this_pin, this);
                            ctx.unpin_native_roots(cipher_pin);
                            range.write_back(ctx, this_now);
                            return Err(e);
                        }
                        ctx.write_byte_array_from(cur_counter_arr, 0, counter);
                        ctx.invoke_virtual(
                            cur_cipher,
                            "processBlock",
                            "([BI[BI)I",
                            &[
                                Value::Object(Some(cur_counter_arr)),
                                Value::Int(0),
                                Value::Object(Some(cur_counter_out_arr)),
                                Value::Int(0),
                            ],
                        )?;
                        cur_cipher = ctx.read_native_pin(cipher_pin, cipher);
                        cur_counter_arr = ctx.read_native_pin(counter_pin, counter_arr);
                        cur_counter_out_arr = ctx.read_native_pin(counter_out_pin, counter_out_arr);
                        ctx.read_byte_array_into(cur_counter_out_arr, 0, keystream);
                        next = in_buf[i] ^ keystream[byte_count as usize];
                        byte_count += 1;
                    } else {
                        next = in_buf[i] ^ keystream[byte_count as usize];
                        byte_count += 1;
                        if byte_count as usize == bs {
                            byte_count = 0;
                            bc_sic_increment_counter(counter);
                            range.note_increment();
                        }
                    }
                    out_buf[i] = next;
                }
                let counter_arr = ctx.read_native_pin(counter_pin, counter_arr);
                let counter_out_arr = ctx.read_native_pin(counter_out_pin, counter_out_arr);
                let this = ctx.read_native_pin(this_pin, this);
                let out_arr = ctx.read_native_pin(out_pin, out_arr);
                ctx.unpin_native_roots(cipher_pin);
                (counter_arr, counter_out_arr, this, out_arr)
            };
            ctx.write_byte_array_from(counter_arr, 0, counter);
            ctx.write_byte_array_from(counter_out_arr, 0, keystream);
            ctx.set_field_by_name(this, "byteCount", Value::Int(byte_count));
            range.write_back(ctx, this);
            ctx.write_byte_array_from(out_arr, out_off as usize, out_buf);
            Ok(Some(Value::Int(len as i32)))
        },
    );

    }

/// Intrinsics for `org.bouncycastle.util.Strings` UTF-8 transcode. With BC
/// JIT-banned these otherwise run interpreted through `UTF8.transcodeToUTF16`
/// and the slow `new String(char[])` / `StringUTF16.compress` path, which
/// dominates `AESTest.testCounter` (255k growing-string round-trips) once the
/// AES engine is native. Strict UTF-8 (RFC 3629) matches BC's decoder for valid
/// input and raises the same `IllegalArgumentException("Invalid UTF-8 input")`
/// for invalid input; encoding a (valid) Java String yields standard UTF-8.
pub fn register_bc_strings_utf8(r: &mut NativeMethodRegistry) {
            const STRINGS: &str = "org/bouncycastle/util/Strings";

    bc_register_null_rerun!(
        r,
        STRINGS,
        "fromUTF8ByteArray",
        "([B)Ljava/lang/String;",
        |ctx, args| {
            let arr = obj_arg(args, 0)?;
            let len = ctx.array_length(arr);
            let mut stack = [0u8; 1024];
            let mut heap = Vec::new();
            let buf: &mut [u8] = if len <= stack.len() {
                &mut stack[..len]
            } else {
                heap.resize(len, 0);
                &mut heap
            };
            ctx.read_byte_array_into(arr, 0, buf);
            match std::str::from_utf8(buf) {
                // BC's `new String(chars, 0, len)` is a fresh, DISTINCT, un-interned
                // object — must NOT go through the pooling `create_string`, which
                // would (a) leak every dynamically-decoded string into the intern
                // pool (testCounter decodes 255k unique growing strings → heap
                // exhaustion) and (b) give wrong `==` identity semantics.
                Ok(text) => Ok(Some(Value::Object(Some(
                    ctx.create_string_uninterned(text),
                )))),
                Err(_) => Err(RuntimeError::IllegalArgumentException {
                    message: "Invalid UTF-8 input".into(),
                }
                .into()),
            }
        },
    );

    bc_register_null_rerun!(
        r,
        STRINGS,
        "toUTF8ByteArray",
        "(Ljava/lang/String;)[B",
        |ctx, args| {
            let str_obj = obj_arg(args, 0)?;
            let units = match ctx.read_string_units(str_obj) {
                Some(u) => u,
                None => return Err(RuntimeError::NullPointerException { message: None }.into()),
            };
            let mut stack = [0u8; 1024];
            let mut stack_len = 0usize;
            let use_stack = units.len().saturating_mul(3) <= stack.len();
            let mut heap = if use_stack {
                Vec::new()
            } else {
                Vec::with_capacity(units.len())
            };
            let mut i = 0;
            while i < units.len() {
                let c = units[i] as u32;
                i += 1;
                let bytes: [u8; 4];
                let n: usize;
                if c < 0x80 {
                    bytes = [c as u8, 0, 0, 0];
                    n = 1;
                } else if c < 0x800 {
                    bytes = [(0xC0 | (c >> 6)) as u8, (0x80 | (c & 0x3F)) as u8, 0, 0];
                    n = 2;
                } else if (0xD800..=0xDFFF).contains(&c) {
                    if c > 0xDBFF {
                        return Err(RuntimeError::IllegalStateException {
                            message: "invalid UTF-16 high surrogate".into(),
                        }
                        .into());
                    }
                    if i >= units.len() {
                        return Err(RuntimeError::IllegalStateException {
                            message: "invalid UTF-16 codepoint (truncated surrogate pair)".into(),
                        }
                        .into());
                    }
                    let w2 = units[i] as u32;
                    i += 1;
                    if !(0xDC00..=0xDFFF).contains(&w2) {
                        return Err(RuntimeError::IllegalStateException {
                            message: "invalid UTF-16 low surrogate".into(),
                        }
                        .into());
                    }
                    let cp = 0x10000 + (((c - 0xD800) << 10) | (w2 - 0xDC00));
                    bytes = [
                        (0xF0 | (cp >> 18)) as u8,
                        (0x80 | ((cp >> 12) & 0x3F)) as u8,
                        (0x80 | ((cp >> 6) & 0x3F)) as u8,
                        (0x80 | (cp & 0x3F)) as u8,
                    ];
                    n = 4;
                } else {
                    bytes = [
                        (0xE0 | (c >> 12)) as u8,
                        (0x80 | ((c >> 6) & 0x3F)) as u8,
                        (0x80 | (c & 0x3F)) as u8,
                        0,
                    ];
                    n = 3;
                }
                if use_stack {
                    stack[stack_len..stack_len + n].copy_from_slice(&bytes[..n]);
                    stack_len += n;
                } else {
                    heap.extend_from_slice(&bytes[..n]);
                }
            }
            let out: &[u8] = if use_stack { &stack[..stack_len] } else { &heap };
            let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, out.len());
            ctx.write_byte_array_from(arr, 0, out);
            Ok(Some(Value::Object(Some(arr))))
        },
    );

    }

/// BouncyCastle byte-array helpers used heavily by the crypto regression tests.
/// `AESTest.testCounter` appends to a synthetic file with
/// `org.bouncycastle.util.Arrays.copyOf(byte[], int)` three times per verify.
/// The Java body is tiny, but under the standing `org/bouncycastle/` JIT ban the
/// call dispatch and bytecode scaffolding dominate hundreds of thousands of
/// short copies.
pub fn register_bc_arrays_helpers(r: &mut NativeMethodRegistry) {
            const ARRAYS: &str = "org/bouncycastle/util/Arrays";

    bc_register_null_rerun!(r, ARRAYS, "copyOf", "([BI)[B", |ctx, args| {
        let original = obj_arg(args, 0)?;
        let new_len_i = match args.get(1) {
            Some(Value::Int(v)) => *v,
            _ => 0,
        };
        if new_len_i < 0 {
            return Err(RuntimeError::NegativeArraySizeException { size: new_len_i }.into());
        }
        let new_len = new_len_i as usize;
        // gc-common w8-b: an Intrinsic, so live in every mode. Reclaim and
        // retry before the OOME, with `original` (the only reference held)
        // refreshed across the collection.
        let mut live = [original];
        let copy = crate::new_array_reclaiming(
            ctx,
            cratonvm_types::ArrayElementType::Byte,
            new_len,
            &mut live,
            "Arrays.copyOf",
        )?;
        let [original] = live;
        let copy_len = ctx.array_length(original).min(new_len);
        if copy_len != 0 {
            let mut buf = vec![0u8; copy_len];
            ctx.read_byte_array_into(original, 0, &mut buf);
            ctx.write_byte_array_from(copy, 0, &buf);
        }
        Ok(Some(Value::Object(Some(copy))))
    });

    }

pub fn bc_clone_byte_array_range(
    ctx: &mut dyn NativeContext,
    src: ObjectRef,
    off: i32,
    len: i32,
) -> Result<ObjectRef, MethodCallFailed> {
    if off < 0 {
        return Err(RuntimeError::aioobe_index_only(off).into());
    }
    if len < 0 {
        return Err(RuntimeError::aioobe_index_only(len).into());
    }
    let off = off as usize;
    let len = len as usize;
    let src_len = ctx.array_length(src);
    let Some(end) = off.checked_add(len) else {
        return Err(RuntimeError::aioobe_index_only(i32::MAX).into());
    };
    if end > src_len {
        return Err(RuntimeError::aioobe_index_only(end.min(i32::MAX as usize) as i32).into());
    }
    // gc-common w20-e: the range is copied out BEFORE the clone's
    // allocation (a GC point); it used to be read through `src`'s address
    // from before it.
    let mut bytes = vec![0u8; len];
    if len != 0 {
        ctx.read_byte_array_into(src, off, &mut bytes);
    }
    let dst = ctx.new_array(cratonvm_types::ArrayElementType::Byte, len);
    if len != 0 {
        ctx.write_byte_array_from(dst, 0, &bytes);
    }
    Ok(dst)
}

/// `KeyParameter.<init>` / `ParametersWithIV.<init>`: store a copy of
/// `src[off..off + len]` into the receiver's `field` and, for the IV form,
/// the wrapped `parameters` first.
///
/// gc-common w20-e: the four constructors stored both through the
/// receiver's address from before the copy's allocation (a GC point), and
/// `parameters` through the argument copy in `args`. Both are rooted across
/// the copy here and re-read.
pub fn bc_params_init_with_copy(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    parameters: Option<Value>,
    src: ObjectRef,
    off: i32,
    len: i32,
    field: &str,
) -> MethodCallResult {
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let this_h = scope.root(this);
    let params_h = match parameters {
        Some(Value::Object(Some(p))) => Some(scope.root(p)),
        _ => None,
    };
    let copy = bc_clone_byte_array_range(&mut *scope, src, off, len)?;
    let this: ObjectRef = scope.get(&this_h);
    if let Some(given) = parameters {
        let now = match &params_h {
            Some(h) => Value::Object(Some(scope.get(h))),
            None => given,
        };
        scope.set_field_by_name(this, "parameters", now);
    }
    scope.set_field_by_name(this, field, Value::Object(Some(copy)));
    Ok(None)
}

pub fn register_bc_param_helpers(r: &mut NativeMethodRegistry) {
        
    bc_register_rerun!(r, "org/bouncycastle/crypto/params/KeyParameter", "<init>", "([B)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let key = obj_arg(args, 1)?;
        let len = ctx.array_length(key).min(i32::MAX as usize) as i32;
        bc_params_init_with_copy(ctx, this, None, key, 0, len, "key")
    });
    bc_register_rerun!(r, "org/bouncycastle/crypto/params/KeyParameter", "<init>", "([BII)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let key = obj_arg(args, 1)?;
        let off = match args.get(2) {
            Some(Value::Int(v)) => *v,
            _ => 0,
        };
        let len = match args.get(3) {
            Some(Value::Int(v)) => *v,
            _ => 0,
        };
        bc_params_init_with_copy(ctx, this, None, key, off, len, "key")
    });

    bc_register_rerun!(
        r,
        "org/bouncycastle/crypto/params/ParametersWithIV",
        "<init>",
        "(Lorg/bouncycastle/crypto/CipherParameters;[B)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let params = args.get(1).copied().unwrap_or(Value::Object(None));
            let iv = obj_arg(args, 2)?;
            let len = ctx.array_length(iv).min(i32::MAX as usize) as i32;
            bc_params_init_with_copy(ctx, this, Some(params), iv, 0, len, "iv")
        },
    );
    bc_register_rerun!(
        r,
        "org/bouncycastle/crypto/params/ParametersWithIV",
        "<init>",
        "(Lorg/bouncycastle/crypto/CipherParameters;[BII)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let params = args.get(1).copied().unwrap_or(Value::Object(None));
            let iv = obj_arg(args, 2)?;
            let off = match args.get(3) {
                Some(Value::Int(v)) => *v,
                _ => 0,
            };
            let len = match args.get(4) {
                Some(Value::Int(v)) => *v,
                _ => 0,
            };
            bc_params_init_with_copy(ctx, this, Some(params), iv, off, len, "iv")
        },
    );

    }

pub fn bc_scrypt_read_byte_array(ctx: &dyn NativeContext, arr: ObjectRef) -> Vec<u8> {
    let mut out = vec![0u8; ctx.array_length(arr)];
    let n = ctx.read_byte_array_into(arr, 0, &mut out);
    out.truncate(n);
    out
}

pub fn bc_scrypt_illegal(message: &str) -> MethodCallFailed {
    RuntimeError::IllegalArgumentException {
        message: message.into(),
    }
    .into()
}

pub fn bc_scrypt_le_bytes_to_i32(bytes: &[u8]) -> Vec<i32> {
    bytes
        .chunks_exact(4)
        .map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

pub fn bc_scrypt_i32_to_le_bytes(words: &[i32], out: &mut [u8]) {
    for (i, &word) in words.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&word.to_le_bytes());
    }
}

pub fn bc_scrypt_block_mix(
    b: &[i32],
    x1: &mut [i32; 16],
    x2: &mut [i32; 16],
    y: &mut [i32],
    r: usize,
) {
    x1.copy_from_slice(&b[b.len() - 16..]);
    let mut b_off = 0usize;
    let mut y_off = 0usize;
    let half_len = b.len() >> 1;
    for _ in 0..(2 * r) {
        for i in 0..16 {
            x2[i] = x1[i] ^ b[b_off + i];
        }
        crate::bc_chacha::salsa_core(8, x2, x1);
        y[y_off..y_off + 16].copy_from_slice(x1);
        y_off = half_len + b_off - y_off;
        b_off += 16;
    }
}

pub fn bc_scrypt_smix(b: &mut [i32], b_off: usize, n: usize, d: usize, r: usize) {
    let pow_n = n.trailing_zeros() as usize;
    let blocks_per_chunk = n >> d;
    let chunk_count = 1usize << d;
    let chunk_mask = blocks_per_chunk - 1;
    let chunk_pow = pow_n - d;
    let b_count = r * 32;

    let mut block_x1 = [0i32; 16];
    let mut block_x2 = [0i32; 16];
    let mut block_y = vec![0i32; b_count];
    let mut x = vec![0i32; b_count];
    let mut vv = Vec::with_capacity(chunk_count);
    x.copy_from_slice(&b[b_off..b_off + b_count]);

    for _ in 0..chunk_count {
        let mut v = vec![0i32; blocks_per_chunk * b_count];
        let mut off = 0usize;
        for _ in (0..blocks_per_chunk).step_by(2) {
            v[off..off + b_count].copy_from_slice(&x);
            off += b_count;
            bc_scrypt_block_mix(&x, &mut block_x1, &mut block_x2, &mut block_y, r);
            v[off..off + b_count].copy_from_slice(&block_y);
            off += b_count;
            bc_scrypt_block_mix(&block_y, &mut block_x1, &mut block_x2, &mut x, r);
        }
        vv.push(v);
    }

    let mask = n - 1;
    for _ in 0..n {
        let j = (x[b_count - 16] as usize) & mask;
        let v = &vv[j >> chunk_pow];
        let v_off = (j & chunk_mask) * b_count;
        block_y.copy_from_slice(&v[v_off..v_off + b_count]);
        for i in 0..b_count {
            block_y[i] ^= x[i];
        }
        bc_scrypt_block_mix(&block_y, &mut block_x1, &mut block_x2, &mut x, r);
    }

    b[b_off..b_off + b_count].copy_from_slice(&x);
}

pub fn bc_scrypt_generate_bytes(
    password: &[u8],
    salt: &[u8],
    n: usize,
    r: usize,
    p: usize,
    dk_len: usize,
) -> Vec<u8> {
    let mf_len_bytes = r * 128;
    let mut bytes =
        crate::phases_early::pbkdf2_derive_for(256, password, salt, 1, p * mf_len_bytes);
    let mut b = bc_scrypt_le_bytes_to_i32(&bytes);

    let mut d = 0usize;
    let mut total = n * r;
    while n.saturating_sub(d) > 2 && total > (1 << 10) {
        d += 1;
        total >>= 1;
    }

    let mf_len_words = mf_len_bytes >> 2;
    for b_off in (0..b.len()).step_by(mf_len_words) {
        bc_scrypt_smix(&mut b, b_off, n, d, r);
    }

    bc_scrypt_i32_to_le_bytes(&b, &mut bytes);
    crate::phases_early::pbkdf2_derive_for(256, password, &bytes, 1, dk_len)
}

pub fn register_bc_scrypt_generator(r: &mut NativeMethodRegistry) {
            bc_register_null_rerun!(
        r,
        "org/bouncycastle/crypto/generators/SCrypt",
        "generate",
        "([B[BIIII)[B",
        |ctx, args| {
            let p_arr = obj_arg(args, 0)?;
            let s_arr = obj_arg(args, 1)?;
            let n = match args.get(2) {
                Some(Value::Int(v)) => *v,
                _ => 0,
            };
            let r_param = match args.get(3) {
                Some(Value::Int(v)) => *v,
                _ => 0,
            };
            let p_param = match args.get(4) {
                Some(Value::Int(v)) => *v,
                _ => 0,
            };
            let dk_len = match args.get(5) {
                Some(Value::Int(v)) => *v,
                _ => 0,
            };

            if n <= 1 || (n & (n - 1)) != 0 {
                return Err(bc_scrypt_illegal("Cost parameter N must be > 1 and a power of 2"));
            }
            if r_param == 1 && n >= 65536 {
                return Err(bc_scrypt_illegal("Cost parameter N must be > 1 and < 65536."));
            }
            if r_param < 1 {
                return Err(bc_scrypt_illegal("Block size r must be >= 1."));
            }
            let max_parallel = i32::MAX / (128 * r_param * 8);
            if p_param < 1 || p_param > max_parallel {
                return Err(bc_scrypt_illegal(&format!(
                    "Parallelisation parameter p must be >= 1 and <= {max_parallel} (based on block size r of {r_param})"
                )));
            }
            if dk_len < 1 {
                return Err(bc_scrypt_illegal("Generated key length dkLen must be >= 1."));
            }

            let password = bc_scrypt_read_byte_array(ctx, p_arr);
            let salt = bc_scrypt_read_byte_array(ctx, s_arr);
            let derived = bc_scrypt_generate_bytes(
                &password,
                &salt,
                n as usize,
                r_param as usize,
                p_param as usize,
                dk_len as usize,
            );
            let out = ctx.new_array(cratonvm_types::ArrayElementType::Byte, derived.len());
            ctx.write_byte_array_from(out, 0, &derived);
            Ok(Some(Value::Object(Some(out))))
        },
    );
    }

pub const BC_ARGON2_BLOCK_QWORDS: usize = 128;

pub const BC_ARGON2_M32L: u64 = 0xffff_ffff;

pub fn bc_argon2_bad_state(message: &str) -> MethodCallFailed {
    RuntimeError::IllegalStateException {
        message: message.into(),
    }
    .into()
}

pub fn bc_argon2_bad_argument(message: &str) -> MethodCallFailed {
    RuntimeError::IllegalArgumentException {
        message: message.into(),
    }
    .into()
}

pub fn bc_argon2_read_byte_array(ctx: &dyn NativeContext, arr: ObjectRef) -> Vec<u8> {
    let mut out = vec![0u8; ctx.array_length(arr)];
    ctx.read_byte_array_into(arr, 0, &mut out);
    out
}

pub fn bc_argon2_read_optional_byte_array(ctx: &dyn NativeContext, value: Value) -> Vec<u8> {
    match value {
        Value::Object(Some(arr)) => bc_argon2_read_byte_array(ctx, arr),
        _ => Vec::new(),
    }
}

pub fn bc_argon2_read_int_field(
    ctx: &dyn NativeContext,
    obj: ObjectRef,
    name: &str,
) -> Result<i32, MethodCallFailed> {
    match ctx.get_field_by_name(obj, name) {
        Value::Int(v) => Ok(v),
        _ => Err(bc_argon2_bad_state("Argon2Parameters: malformed int field")),
    }
}

pub fn bc_argon2_class_name(
    ctx: &dyn NativeContext,
    obj: ObjectRef,
) -> Result<String, MethodCallFailed> {
    ctx.class_name_of_id(ctx.class_id_of_object(obj))
        .ok_or_else(|| bc_argon2_bad_state("Argon2 BlockPool: unknown receiver class"))
}

pub fn bc_argon2_exercise_block_pool(
    ctx: &mut dyn NativeContext,
    pool: ObjectRef,
    memory_blocks: usize,
) -> Result<(), MethodCallFailed> {
    let block_desc = "Lorg/bouncycastle/crypto/generators/Argon2BytesGenerator$Block;";
    let allocate_desc = format!("(){block_desc}");
    let deallocate_desc = format!("({block_desc})V");
    let pool_base = ctx.pin_native_root(pool);

    let result = (|| -> Result<(), MethodCallFailed> {
        let total_blocks = memory_blocks
            .checked_add(4)
            .ok_or_else(|| bc_argon2_bad_argument("Argon2 block count overflow"))?;
        let mut blocks = Vec::with_capacity(total_blocks);

        for _ in 0..total_blocks {
            let pool_now = ctx.read_native_pin(pool_base, pool);
            let pool_class = bc_argon2_class_name(ctx, pool_now)?;
            let block = match ctx.invoke(
                &pool_class,
                "allocate",
                &allocate_desc,
                &[Value::Object(Some(pool_now))],
            )? {
                Some(Value::Object(Some(block))) => block,
                _ => {
                    return Err(bc_argon2_bad_state(
                        "Argon2 BlockPool.allocate returned null",
                    ))
                }
            };
            let block_pin = ctx.pin_native_root(block);
            blocks.push((block_pin, block));
        }

        let filler_base = memory_blocks;
        for index in [
            filler_base + 2,
            filler_base + 3,
            filler_base,
            filler_base + 1,
        ] {
            let pool_now = ctx.read_native_pin(pool_base, pool);
            let pool_class = bc_argon2_class_name(ctx, pool_now)?;
            let (block_pin, block) = blocks[index];
            let block_now = ctx.read_native_pin(block_pin, block);
            ctx.invoke(
                &pool_class,
                "deallocate",
                &deallocate_desc,
                &[
                    Value::Object(Some(pool_now)),
                    Value::Object(Some(block_now)),
                ],
            )?;
        }

        for &(block_pin, block) in blocks.iter().take(memory_blocks) {
            let pool_now = ctx.read_native_pin(pool_base, pool);
            let pool_class = bc_argon2_class_name(ctx, pool_now)?;
            let block_now = ctx.read_native_pin(block_pin, block);
            ctx.invoke(
                &pool_class,
                "deallocate",
                &deallocate_desc,
                &[
                    Value::Object(Some(pool_now)),
                    Value::Object(Some(block_now)),
                ],
            )?;
        }
        Ok(())
    })();

    ctx.unpin_native_roots(pool_base);
    result
}

pub fn bc_argon2_block_words(
    ctx: &dyn NativeContext,
    block: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    let words = match ctx.get_field_by_name(block, "v") {
        Value::Object(Some(o)) => o,
        _ => {
            return Err(bc_argon2_bad_state(
                "Argon2BytesGenerator.Block: missing word array",
            ))
        }
    };
    if ctx.array_length(words) < BC_ARGON2_BLOCK_QWORDS {
        return Err(RuntimeError::aioobe_index_only(BC_ARGON2_BLOCK_QWORDS as i32).into());
    }
    Ok(words)
}

pub fn bc_argon2_read_block_words(
    ctx: &dyn NativeContext,
    block: ObjectRef,
) -> Result<[u64; BC_ARGON2_BLOCK_QWORDS], MethodCallFailed> {
    let words_arr = bc_argon2_block_words(ctx, block)?;
    let mut words = [0u64; BC_ARGON2_BLOCK_QWORDS];
    for (i, slot) in words.iter_mut().enumerate() {
        match ctx.get_array_element(words_arr, i) {
            Value::Long(v) => *slot = v as u64,
            _ => return Err(bc_argon2_bad_state("Argon2 block word array is malformed")),
        }
    }
    Ok(words)
}

pub fn bc_argon2_write_block_words(
    ctx: &dyn NativeContext,
    block: ObjectRef,
    words: &[u64; BC_ARGON2_BLOCK_QWORDS],
) -> Result<(), MethodCallFailed> {
    let words_arr = bc_argon2_block_words(ctx, block)?;
    for (i, &word) in words.iter().enumerate() {
        ctx.set_array_element(words_arr, i, Value::Long(word as i64));
    }
    Ok(())
}

pub fn bc_argon2_clear_block(
    ctx: &dyn NativeContext,
    block: ObjectRef,
) -> Result<(), MethodCallFailed> {
    bc_argon2_write_block_words(ctx, block, &[0u64; BC_ARGON2_BLOCK_QWORDS])
}

pub fn bc_argon2_alloc_block(
    ctx: &mut dyn NativeContext,
) -> Result<ObjectRef, MethodCallFailed> {
    let block_class = "org/bouncycastle/crypto/generators/Argon2BytesGenerator$Block";
    let class_id = match ctx.ensure_class_initialized(block_class) {
        Ok(cid) => cid,
        // Fallible since 2026-08-10 (Real-JDK wave 2, step 3): `Argon2BytesGenerator$Block`
        // is a Bouncy Castle class, so a fabricated stand-in for it is a
        // dependency substitution — exactly what contract §5 refuses. On a run
        // that actually has BC on the classpath the `Ok` arm is what runs.
        Err(_) => crate::util_concurrent_ext::refused_class(ctx, block_class, 1)?,
    };
    let n_fields = ctx.class_num_total_fields(class_id).max(1);
    // gc-common w20-e: the words array first, rooted across the block's
    // allocation; `v` used to be stored through the block's address from
    // before the array's allocation.
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let words = scope.new_array(
        cratonvm_types::ArrayElementType::Long,
        BC_ARGON2_BLOCK_QWORDS,
    );
    let words_h = scope.root(words);
    let block = scope.alloc_object(class_id, n_fields);
    let words: ObjectRef = scope.get(&words_h);
    scope.set_field_by_name(block, "v", Value::Object(Some(words)));
    Ok(block)
}

pub fn bc_argon2_fillblock_block_field(
    ctx: &dyn NativeContext,
    fill_block: ObjectRef,
    name: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    match ctx.get_field_by_name(fill_block, name) {
        Value::Object(Some(o)) => Ok(o),
        _ => Err(bc_argon2_bad_state("Argon2 FillBlock: missing block field")),
    }
}

pub fn bc_argon2_round_index(args: &[Value], idx: usize) -> Result<usize, MethodCallFailed> {
    match args.get(idx) {
        Some(Value::Int(v)) if (0..BC_ARGON2_BLOCK_QWORDS as i32).contains(v) => Ok(*v as usize),
        Some(Value::Int(v)) => Err(RuntimeError::aioobe_index_only(*v).into()),
        _ => Err(RuntimeError::aioobe_index_only(-1).into()),
    }
}

pub fn bc_argon2_quarter_round(
    v: &mut [u64; BC_ARGON2_BLOCK_QWORDS],
    x: usize,
    y: usize,
    z: usize,
    s: u32,
) {
    let mut a = v[x];
    let b = v[y];
    let mut c = v[z];
    a = a.wrapping_add(b).wrapping_add(
        2u64.wrapping_mul(a & BC_ARGON2_M32L)
            .wrapping_mul(b & BC_ARGON2_M32L),
    );
    c = (c ^ a).rotate_right(s);
    v[x] = a;
    v[z] = c;
}

pub fn bc_argon2_f(
    v: &mut [u64; BC_ARGON2_BLOCK_QWORDS],
    a: usize,
    b: usize,
    c: usize,
    d: usize,
) {
    bc_argon2_quarter_round(v, a, b, d, 32);
    bc_argon2_quarter_round(v, c, d, b, 24);
    bc_argon2_quarter_round(v, a, b, d, 16);
    bc_argon2_quarter_round(v, c, d, b, 63);
}

pub fn bc_argon2_round_function_words(
    words: &mut [u64; BC_ARGON2_BLOCK_QWORDS],
    v0: usize,
    v1: usize,
    v2: usize,
    v3: usize,
    v4: usize,
    v5: usize,
    v6: usize,
    v7: usize,
    v8: usize,
    v9: usize,
    v10: usize,
    v11: usize,
    v12: usize,
    v13: usize,
    v14: usize,
    v15: usize,
) {
    bc_argon2_f(words, v0, v4, v8, v12);
    bc_argon2_f(words, v1, v5, v9, v13);
    bc_argon2_f(words, v2, v6, v10, v14);
    bc_argon2_f(words, v3, v7, v11, v15);

    bc_argon2_f(words, v0, v5, v10, v15);
    bc_argon2_f(words, v1, v6, v11, v12);
    bc_argon2_f(words, v2, v7, v8, v13);
    bc_argon2_f(words, v3, v4, v9, v14);
}

pub fn bc_argon2_apply_blake_words(words: &mut [u64; BC_ARGON2_BLOCK_QWORDS]) {
    for i in 0..8 {
        let i16 = 16 * i;
        bc_argon2_round_function_words(
            words,
            i16,
            i16 + 1,
            i16 + 2,
            i16 + 3,
            i16 + 4,
            i16 + 5,
            i16 + 6,
            i16 + 7,
            i16 + 8,
            i16 + 9,
            i16 + 10,
            i16 + 11,
            i16 + 12,
            i16 + 13,
            i16 + 14,
            i16 + 15,
        );
    }

    for i in 0..8 {
        let i2 = 2 * i;
        bc_argon2_round_function_words(
            words,
            i2,
            i2 + 1,
            i2 + 16,
            i2 + 17,
            i2 + 32,
            i2 + 33,
            i2 + 48,
            i2 + 49,
            i2 + 64,
            i2 + 65,
            i2 + 80,
            i2 + 81,
            i2 + 96,
            i2 + 97,
            i2 + 112,
            i2 + 113,
        );
    }
}

pub fn register_bc_argon2_bytes_generator(r: &mut NativeMethodRegistry) {
            const GENERATOR_CLASS: &str = "org/bouncycastle/crypto/generators/Argon2BytesGenerator";
    const BLOCK_CLASS: &str = "org/bouncycastle/crypto/generators/Argon2BytesGenerator$Block";
    const FILL_BLOCK_CLASS: &str =
        "org/bouncycastle/crypto/generators/Argon2BytesGenerator$FillBlock";
    const FIXED_POOL_CLASS: &str =
        "org/bouncycastle/crypto/generators/Argon2BytesGenerator$FixedBlockPool";

    bc_register_rerun!(
        r,
        "org/bouncycastle/crypto/generators/Argon2BytesGenerator",
        "generateBytes",
        "([B[BII)I",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let password_arr = obj_arg(args, 1)?;
            let out_arr = obj_arg(args, 2)?;
            let out_off = match args.get(3) {
                Some(Value::Int(v)) => *v,
                _ => return Err(bc_argon2_bad_argument("invalid output offset")),
            };
            let out_len = match args.get(4) {
                Some(Value::Int(v)) => *v,
                _ => return Err(bc_argon2_bad_argument("invalid output length")),
            };

            if out_len < 4 {
                return Err(bc_argon2_bad_state("output length less than 4"));
            }
            if out_off < 0
                || out_len < 0
                || (out_off as usize)
                    .checked_add(out_len as usize)
                    .map_or(true, |end| end > ctx.array_length(out_arr))
            {
                return Err(RuntimeError::aioobe_index_only(out_off).into());
            }

            let params = match ctx.get_field_by_name(this, "parameters") {
                Value::Object(Some(o)) => o,
                _ => return Err(bc_argon2_bad_state("Argon2BytesGenerator not initialized")),
            };

            let password = bc_argon2_read_byte_array(ctx, password_arr);
            let salt =
                bc_argon2_read_optional_byte_array(ctx, ctx.get_field_by_name(params, "salt"));
            let secret =
                bc_argon2_read_optional_byte_array(ctx, ctx.get_field_by_name(params, "secret"));
            let additional = bc_argon2_read_optional_byte_array(
                ctx,
                ctx.get_field_by_name(params, "additional"),
            );

            let iterations = bc_argon2_read_int_field(ctx, params, "iterations")?;
            let memory = bc_argon2_read_int_field(ctx, params, "memory")?;
            let lanes = bc_argon2_read_int_field(ctx, params, "lanes")?;
            let version = bc_argon2_read_int_field(ctx, params, "version")?;
            let argon_type = bc_argon2_read_int_field(ctx, params, "type")?;

            let algorithm = match argon_type {
                0 => argon2::Algorithm::Argon2d,
                1 => argon2::Algorithm::Argon2i,
                2 => argon2::Algorithm::Argon2id,
                _ => return Err(bc_argon2_bad_state("unknown Argon2 type")),
            };
            let version = match version {
                0x10 => argon2::Version::V0x10,
                0x13 => argon2::Version::V0x13,
                _ => return Err(bc_argon2_bad_state("unknown Argon2 version")),
            };

            let configured_pool = match ctx.get_field_by_name(params, "blockPool") {
                Value::Object(Some(pool)) => Some(pool),
                _ => None,
            };
            if configured_pool.is_some() {
                return ctx.invoke_special_bytecode_only(
                    GENERATOR_CLASS,
                    "generateBytes",
                    "([B[BII)I",
                    args,
                );
            }

            let mut params_builder = argon2::ParamsBuilder::new();
            params_builder
                .m_cost(memory as u32)
                .t_cost(iterations as u32)
                .p_cost(lanes as u32)
                .output_len(out_len as usize);
            if !additional.is_empty() {
                match argon2::AssociatedData::new(&additional) {
                    Ok(ad) => {
                        params_builder.data(ad);
                    }
                    Err(_) => {
                        return ctx.invoke_special_bytecode_only(
                            GENERATOR_CLASS,
                            "generateBytes",
                            "([B[BII)I",
                            args,
                        );
                    }
                }
            }
            let native_params = match params_builder.build() {
                Ok(p) => p,
                Err(_) => {
                    return ctx.invoke_special_bytecode_only(
                        GENERATOR_CLASS,
                        "generateBytes",
                        "([B[BII)I",
                        args,
                    );
                }
            };
            let native_argon2 = if secret.is_empty() {
                argon2::Argon2::new(algorithm, version, native_params)
            } else {
                match argon2::Argon2::new_with_secret(&secret, algorithm, version, native_params) {
                    Ok(a) => a,
                    Err(_) => {
                        return ctx.invoke_special_bytecode_only(
                            GENERATOR_CLASS,
                            "generateBytes",
                            "([B[BII)I",
                            args,
                        );
                    }
                }
            };

            let mut derived = vec![0u8; out_len as usize];
            if native_argon2
                .hash_password_into(&password, &salt, &mut derived)
                .is_err()
            {
                return ctx.invoke_special_bytecode_only(
                    GENERATOR_CLASS,
                    "generateBytes",
                    "([B[BII)I",
                    args,
                );
            }
            if !ctx.write_byte_array_from(out_arr, out_off as usize, &derived) {
                return Err(RuntimeError::aioobe_index_only(out_off).into());
            }
            Ok(Some(Value::Int(out_len)))
        },
    );

    bc_register_null_rerun!(r, BLOCK_CLASS, "fromBytes", "([B)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let input = obj_arg(args, 1)?;
        if ctx.array_length(input) < 1024 {
            return Err(RuntimeError::IllegalArgumentException {
                message: "input shorter than blocksize".into(),
            }
            .into());
        }
        let mut bytes = [0u8; 1024];
        ctx.read_byte_array_into(input, 0, &mut bytes);
        let mut words = [0u64; BC_ARGON2_BLOCK_QWORDS];
        for (i, word) in words.iter_mut().enumerate() {
            let off = i * 8;
            *word = u64::from_le_bytes([
                bytes[off],
                bytes[off + 1],
                bytes[off + 2],
                bytes[off + 3],
                bytes[off + 4],
                bytes[off + 5],
                bytes[off + 6],
                bytes[off + 7],
            ]);
        }
        bc_argon2_write_block_words(ctx, this, &words)?;
        Ok(None)
    });

    bc_register_null_rerun!(r, BLOCK_CLASS, "toBytes", "([B)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let output = obj_arg(args, 1)?;
        if ctx.array_length(output) < 1024 {
            return Err(RuntimeError::IllegalArgumentException {
                message: "output shorter than blocksize".into(),
            }
            .into());
        }
        let words = bc_argon2_read_block_words(ctx, this)?;
        let mut bytes = [0u8; 1024];
        for (i, &word) in words.iter().enumerate() {
            bytes[i * 8..i * 8 + 8].copy_from_slice(&word.to_le_bytes());
        }
        ctx.write_byte_array_from(output, 0, &bytes);
        Ok(None)
    });

    bc_register_null_rerun!(

        r,
        BLOCK_CLASS,
        "copyBlock",
        "(Lorg/bouncycastle/crypto/generators/Argon2BytesGenerator$Block;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let other = obj_arg(args, 1)?;
            let words = bc_argon2_read_block_words(ctx, other)?;
            bc_argon2_write_block_words(ctx, this, &words)?;
            Ok(None)
        },
    );

    bc_register_null_rerun!(

        r,
        BLOCK_CLASS,
        "xor",
        "(Lorg/bouncycastle/crypto/generators/Argon2BytesGenerator$Block;Lorg/bouncycastle/crypto/generators/Argon2BytesGenerator$Block;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let b1 = obj_arg(args, 1)?;
            let b2 = obj_arg(args, 2)?;
            let w1 = bc_argon2_read_block_words(ctx, b1)?;
            let w2 = bc_argon2_read_block_words(ctx, b2)?;
            let mut out = [0u64; BC_ARGON2_BLOCK_QWORDS];
            for i in 0..BC_ARGON2_BLOCK_QWORDS {
                out[i] = w1[i] ^ w2[i];
            }
            bc_argon2_write_block_words(ctx, this, &out)?;
            Ok(None)
        },
    );

    bc_register_null_rerun!(

        r,
        BLOCK_CLASS,
        "xorWith",
        "(Lorg/bouncycastle/crypto/generators/Argon2BytesGenerator$Block;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let b1 = obj_arg(args, 1)?;
            let mut out = bc_argon2_read_block_words(ctx, this)?;
            let w1 = bc_argon2_read_block_words(ctx, b1)?;
            for i in 0..BC_ARGON2_BLOCK_QWORDS {
                out[i] ^= w1[i];
            }
            bc_argon2_write_block_words(ctx, this, &out)?;
            Ok(None)
        },
    );

    bc_register_null_rerun!(

        r,
        BLOCK_CLASS,
        "xorWith",
        "(Lorg/bouncycastle/crypto/generators/Argon2BytesGenerator$Block;Lorg/bouncycastle/crypto/generators/Argon2BytesGenerator$Block;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let b1 = obj_arg(args, 1)?;
            let b2 = obj_arg(args, 2)?;
            let mut out = bc_argon2_read_block_words(ctx, this)?;
            let w1 = bc_argon2_read_block_words(ctx, b1)?;
            let w2 = bc_argon2_read_block_words(ctx, b2)?;
            for i in 0..BC_ARGON2_BLOCK_QWORDS {
                out[i] ^= w1[i] ^ w2[i];
            }
            bc_argon2_write_block_words(ctx, this, &out)?;
            Ok(None)
        },
    );

    bc_register_null_rerun!(

        r,
        BLOCK_CLASS,
        "clear",
        "()Lorg/bouncycastle/crypto/generators/Argon2BytesGenerator$Block;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            bc_argon2_clear_block(ctx, this)?;
            Ok(Some(Value::Object(Some(this))))
        },
    );

    bc_register_null_rerun!(

        r,
        FIXED_POOL_CLASS,
        "allocate",
        "()Lorg/bouncycastle/crypto/generators/Argon2BytesGenerator$Block;",
        |ctx, args| {
            ctx.invoke_special_bytecode_only(
                FIXED_POOL_CLASS,
                "allocate",
                "()Lorg/bouncycastle/crypto/generators/Argon2BytesGenerator$Block;",
                args,
            )
        },
    );

    bc_register_null_rerun!(

        r,
        FIXED_POOL_CLASS,
        "deallocate",
        "(Lorg/bouncycastle/crypto/generators/Argon2BytesGenerator$Block;)V",
        |ctx, args| {
            ctx.invoke_special_bytecode_only(
                FIXED_POOL_CLASS,
                "deallocate",
                "(Lorg/bouncycastle/crypto/generators/Argon2BytesGenerator$Block;)V",
                args,
            )
        },
    );

    bc_register_null_rerun!(r, FILL_BLOCK_CLASS, "applyBlake", "()V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let z_block = bc_argon2_fillblock_block_field(ctx, this, "Z")?;
        let mut z = bc_argon2_read_block_words(ctx, z_block)?;
        bc_argon2_apply_blake_words(&mut z);
        bc_argon2_write_block_words(ctx, z_block, &z)?;
        Ok(None)
    });

    bc_register_null_rerun!(

        r,
        FILL_BLOCK_CLASS,
        "fillBlock",
        "(Lorg/bouncycastle/crypto/generators/Argon2BytesGenerator$Block;Lorg/bouncycastle/crypto/generators/Argon2BytesGenerator$Block;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let y_block = obj_arg(args, 1)?;
            let current_block = obj_arg(args, 2)?;
            let z_block = bc_argon2_fillblock_block_field(ctx, this, "Z")?;
            let y = bc_argon2_read_block_words(ctx, y_block)?;
            let mut z = y;
            bc_argon2_apply_blake_words(&mut z);
            let mut current = [0u64; BC_ARGON2_BLOCK_QWORDS];
            for i in 0..BC_ARGON2_BLOCK_QWORDS {
                current[i] = y[i] ^ z[i];
            }
            bc_argon2_write_block_words(ctx, z_block, &z)?;
            bc_argon2_write_block_words(ctx, current_block, &current)?;
            Ok(None)
        },
    );

    bc_register_null_rerun!(

        r,
        FILL_BLOCK_CLASS,
        "fillBlock",
        "(Lorg/bouncycastle/crypto/generators/Argon2BytesGenerator$Block;Lorg/bouncycastle/crypto/generators/Argon2BytesGenerator$Block;Lorg/bouncycastle/crypto/generators/Argon2BytesGenerator$Block;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let x_block = obj_arg(args, 1)?;
            let y_block = obj_arg(args, 2)?;
            let current_block = obj_arg(args, 3)?;
            let r_block = bc_argon2_fillblock_block_field(ctx, this, "R")?;
            let z_block = bc_argon2_fillblock_block_field(ctx, this, "Z")?;
            let x = bc_argon2_read_block_words(ctx, x_block)?;
            let y = bc_argon2_read_block_words(ctx, y_block)?;
            let mut r_words = [0u64; BC_ARGON2_BLOCK_QWORDS];
            for i in 0..BC_ARGON2_BLOCK_QWORDS {
                r_words[i] = x[i] ^ y[i];
            }
            let mut z = r_words;
            bc_argon2_apply_blake_words(&mut z);
            let mut current = [0u64; BC_ARGON2_BLOCK_QWORDS];
            for i in 0..BC_ARGON2_BLOCK_QWORDS {
                current[i] = r_words[i] ^ z[i];
            }
            bc_argon2_write_block_words(ctx, r_block, &r_words)?;
            bc_argon2_write_block_words(ctx, z_block, &z)?;
            bc_argon2_write_block_words(ctx, current_block, &current)?;
            Ok(None)
        },
    );

    bc_register_null_rerun!(

        r,
        FILL_BLOCK_CLASS,
        "fillBlockWithXor",
        "(Lorg/bouncycastle/crypto/generators/Argon2BytesGenerator$Block;Lorg/bouncycastle/crypto/generators/Argon2BytesGenerator$Block;Lorg/bouncycastle/crypto/generators/Argon2BytesGenerator$Block;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let x_block = obj_arg(args, 1)?;
            let y_block = obj_arg(args, 2)?;
            let current_block = obj_arg(args, 3)?;
            let r_block = bc_argon2_fillblock_block_field(ctx, this, "R")?;
            let z_block = bc_argon2_fillblock_block_field(ctx, this, "Z")?;
            let x = bc_argon2_read_block_words(ctx, x_block)?;
            let y = bc_argon2_read_block_words(ctx, y_block)?;
            let mut current = bc_argon2_read_block_words(ctx, current_block)?;
            let mut r_words = [0u64; BC_ARGON2_BLOCK_QWORDS];
            for i in 0..BC_ARGON2_BLOCK_QWORDS {
                r_words[i] = x[i] ^ y[i];
            }
            let mut z = r_words;
            bc_argon2_apply_blake_words(&mut z);
            for i in 0..BC_ARGON2_BLOCK_QWORDS {
                current[i] ^= r_words[i] ^ z[i];
            }
            bc_argon2_write_block_words(ctx, r_block, &r_words)?;
            bc_argon2_write_block_words(ctx, z_block, &z)?;
            bc_argon2_write_block_words(ctx, current_block, &current)?;
            Ok(None)
        },
    );

    bc_register_null_rerun!(

        r,
        GENERATOR_CLASS,
        "roundFunction",
        "(Lorg/bouncycastle/crypto/generators/Argon2BytesGenerator$Block;IIIIIIIIIIIIIIII)V",
        |ctx, args| {
            let block = obj_arg(args, 0)?;
            let mut words = bc_argon2_read_block_words(ctx, block)?;

            let v0 = bc_argon2_round_index(args, 1)?;
            let v1 = bc_argon2_round_index(args, 2)?;
            let v2 = bc_argon2_round_index(args, 3)?;
            let v3 = bc_argon2_round_index(args, 4)?;
            let v4 = bc_argon2_round_index(args, 5)?;
            let v5 = bc_argon2_round_index(args, 6)?;
            let v6 = bc_argon2_round_index(args, 7)?;
            let v7 = bc_argon2_round_index(args, 8)?;
            let v8 = bc_argon2_round_index(args, 9)?;
            let v9 = bc_argon2_round_index(args, 10)?;
            let v10 = bc_argon2_round_index(args, 11)?;
            let v11 = bc_argon2_round_index(args, 12)?;
            let v12 = bc_argon2_round_index(args, 13)?;
            let v13 = bc_argon2_round_index(args, 14)?;
            let v14 = bc_argon2_round_index(args, 15)?;
            let v15 = bc_argon2_round_index(args, 16)?;

            bc_argon2_round_function_words(
                &mut words, v0, v1, v2, v3, v4, v5, v6, v7, v8, v9, v10, v11, v12, v13, v14, v15,
            );
            bc_argon2_write_block_words(ctx, block, &words)?;
            Ok(None)
        },
    );
    }

pub fn bc_pkcs12_adjust(a: &mut [u8], a_off: usize, b: &[u8]) {
    let mut x = u16::from(b[b.len() - 1]) + u16::from(a[a_off + b.len() - 1]) + 1;
    a[a_off + b.len() - 1] = x as u8;
    x >>= 8;

    for i in (0..b.len() - 1).rev() {
        x += u16::from(b[i]) + u16::from(a[a_off + i]);
        a[a_off + i] = x as u8;
        x >>= 8;
    }
}

pub fn bc_pkcs12_repeat_to_v(input: &[u8], v: usize) -> Vec<u8> {
    if input.is_empty() {
        return Vec::new();
    }
    let len = v * input.len().div_ceil(v);
    let mut out = vec![0u8; len];
    for i in 0..len {
        out[i] = input[i % input.len()];
    }
    out
}

pub fn bc_pkcs12_derive_sha1(
    password: &[u8],
    salt: &[u8],
    iteration_count: i32,
    id_byte: u8,
    n: usize,
) -> Vec<u8> {
    const U: usize = 20;
    const V: usize = 64;

    let d = vec![id_byte; V];
    let s = bc_pkcs12_repeat_to_v(salt, V);
    let p = bc_pkcs12_repeat_to_v(password, V);
    let mut i_buf = Vec::with_capacity(s.len() + p.len());
    i_buf.extend_from_slice(&s);
    i_buf.extend_from_slice(&p);

    let mut d_key = vec![0u8; n];
    let c = n.div_ceil(U);
    for i in 1..=c {
        let mut input = Vec::with_capacity(d.len() + i_buf.len());
        input.extend_from_slice(&d);
        input.extend_from_slice(&i_buf);
        let mut a = crate::real_sha1(&input);
        for _ in 1..iteration_count {
            a = crate::real_sha1(&a);
        }

        let mut b = vec![0u8; V];
        for j in 0..V {
            b[j] = a[j % a.len()];
        }
        for j in 0..(i_buf.len() / V) {
            bc_pkcs12_adjust(&mut i_buf, j * V, &b);
        }

        let d_off = (i - 1) * U;
        let copy_len = if i == c { n - d_off } else { U };
        d_key[d_off..d_off + copy_len].copy_from_slice(&a[..copy_len]);
    }
    d_key
}

pub fn bc_pkcs12_generator_bytes(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    name: &str,
) -> Vec<u8> {
    match ctx.get_field_by_name(this, name) {
        Value::Object(Some(arr)) => {
            let len = ctx.array_length(arr);
            let mut out = vec![0u8; len];
            let n = ctx.read_byte_array_into(arr, 0, &mut out);
            out.truncate(n);
            out
        }
        _ => Vec::new(),
    }
}

/// Is this `PKCS12ParametersGenerator` the SHA-1 one the native KDF computes?
///
/// PKCS#12's KDF is parameterised by a digest: `new PKCS12ParametersGenerator(
/// new SHA1Digest())` is the common case and the only one
/// `bc_pkcs12_derive_sha1` implements. Every other digest — SHA-256, GOST3411 —
/// must run BouncyCastle's OWN bytecode, exactly as the `PKCS5S2` sibling does
/// for a PRF this VM has no arm for. Raising instead turned three bc-java
/// `PfxPduTest` cases (`testGOST1`, `testGOST2`, `testCreateAES256andSHA256`)
/// into hard errors from inside the MAC-calculator builder.
pub fn bc_pkcs12_is_sha1(ctx: &dyn NativeContext, this: ObjectRef) -> bool {
    let Value::Object(Some(digest)) = ctx.get_field_by_name(this, "digest") else {
        return false;
    };
    matches!(
        ctx.class_name_arc_of_id(ctx.class_id_of_object(digest))
            .as_deref(),
        Some("org/bouncycastle/crypto/digests/SHA1Digest")
    )
}

pub fn bc_pkcs12_require_sha1(
    ctx: &dyn NativeContext,
    this: ObjectRef,
) -> Result<(), MethodCallFailed> {
    let digest = match ctx.get_field_by_name(this, "digest") {
        Value::Object(Some(o)) => o,
        _ => {
            return Err(RuntimeError::IllegalStateException {
                message: "PKCS12ParametersGenerator: missing digest".into(),
            }
            .into())
        }
    };
    match ctx
        .class_name_arc_of_id(ctx.class_id_of_object(digest))
        .as_deref()
    {
        Some("org/bouncycastle/crypto/digests/SHA1Digest") => Ok(()),
        _ => Err(RuntimeError::IllegalStateException {
            message: "PKCS12ParametersGenerator native supports SHA1Digest".into(),
        }
        .into()),
    }
}

pub fn bc_pkcs12_read_state(
    ctx: &dyn NativeContext,
    this: ObjectRef,
) -> Result<(Vec<u8>, Vec<u8>, i32), MethodCallFailed> {
    bc_pkcs12_require_sha1(ctx, this)?;
    let password = bc_pkcs12_generator_bytes(ctx, this, "password");
    let salt = bc_pkcs12_generator_bytes(ctx, this, "salt");
    // SECURITY — the sibling `bc_pkcs5s2_read_state` below already refuses a
    // non-positive iteration count; this one accepted it. That mattered because
    // `get_field_by_name` is not descriptor-aware and cannot distinguish "the
    // generator was never `init`-ed" from "iterationCount is genuinely 0": an
    // unwritten slot answers `Value::Int(0)`, which the first arm ACCEPTED, so
    // the `_ => 0` fallback was not even the path that produced the zero. A
    // PKCS#12 KDF run with zero iterations degenerates the derived key. Read by
    // resolved index and refuse. See `docs/feature-designs/by-name-field-reads.md`.
    let iteration_count = match crate::field_read::int_field_strict(ctx, this, "iterationCount") {
        Some(v) if v > 0 => v,
        _ => {
            return Err(RuntimeError::IllegalArgumentException {
                message: "iteration count must be at least 1.".into(),
            }
            .into())
        }
    };
    Ok((password, salt, iteration_count))
}

pub fn bc_pkcs5s2_read_state(
    ctx: &dyn NativeContext,
    this: ObjectRef,
) -> Result<(Vec<u8>, Vec<u8>, u32), MethodCallFailed> {
    let password = bc_pkcs12_generator_bytes(ctx, this, "password");
    let salt = bc_pkcs12_generator_bytes(ctx, this, "salt");
    let iteration_count = match ctx.get_field_by_name(this, "iterationCount") {
        Value::Int(v) if v > 0 => v as u32,
        _ => {
            return Err(RuntimeError::IllegalArgumentException {
                message: "iteration count must be at least 1.".into(),
            }
            .into())
        }
    };
    Ok((password, salt, iteration_count))
}

pub fn bc_pkcs12_byte_array(ctx: &mut dyn NativeContext, bytes: &[u8]) -> ObjectRef {
    let arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, bytes.len());
    if !bytes.is_empty() {
        ctx.write_byte_array_from(arr, 0, bytes);
    }
    arr
}

pub fn bc_pkcs12_key_parameter(
    ctx: &mut dyn NativeContext,
    key: &[u8],
) -> Result<ObjectRef, MethodCallFailed> {
    let key_arr = bc_pkcs12_byte_array(ctx, key);
    let key_pin = ctx.pin_native_root(key_arr);
    let key_arr = ctx.read_native_pin(key_pin, key_arr);
    let result = ctx.new_object_initialized(
        "org/bouncycastle/crypto/params/KeyParameter",
        "([BII)V",
        &[
            Value::Object(Some(key_arr)),
            Value::Int(0),
            Value::Int(key.len() as i32),
        ],
    );
    ctx.unpin_native_roots(key_pin);
    match result? {
        Some(Value::Object(Some(obj))) => Ok(obj),
        _ => Err(RuntimeError::IllegalStateException {
            message: "PKCS12ParametersGenerator: KeyParameter allocation failed".into(),
        }
        .into()),
    }
}

pub fn bc_pkcs12_parameters_with_iv(
    ctx: &mut dyn NativeContext,
    key: &[u8],
    iv: &[u8],
) -> Result<ObjectRef, MethodCallFailed> {
    let key_param = bc_pkcs12_key_parameter(ctx, key)?;
    let key_pin = ctx.pin_native_root(key_param);
    let iv_arr = bc_pkcs12_byte_array(ctx, iv);
    let iv_pin = ctx.pin_native_root(iv_arr);
    let key_param = ctx.read_native_pin(key_pin, key_param);
    let iv_arr = ctx.read_native_pin(iv_pin, iv_arr);
    let result = ctx.new_object_initialized(
        "org/bouncycastle/crypto/params/ParametersWithIV",
        "(Lorg/bouncycastle/crypto/CipherParameters;[BII)V",
        &[
            Value::Object(Some(key_param)),
            Value::Object(Some(iv_arr)),
            Value::Int(0),
            Value::Int(iv.len() as i32),
        ],
    );
    ctx.unpin_native_roots(key_pin);
    match result? {
        Some(Value::Object(Some(obj))) => Ok(obj),
        _ => Err(RuntimeError::IllegalStateException {
            message: "PKCS12ParametersGenerator: ParametersWithIV allocation failed".into(),
        }
        .into()),
    }
}

pub fn register_bc_pkcs12_parameters_generator(r: &mut NativeMethodRegistry) {
        
    const CLS: &str = "org/bouncycastle/crypto/generators/PKCS12ParametersGenerator";
    bc_register_null_rerun!(
        r,
        CLS,
        "generateDerivedParameters",
        "(I)Lorg/bouncycastle/crypto/CipherParameters;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let key_size = match args.get(1) {
                Some(Value::Int(v)) if *v >= 0 => (*v as usize) / 8,
                Some(Value::Int(v)) => {
                    return Err(RuntimeError::NegativeArraySizeException { size: *v }.into())
                }
                _ => 0,
            };
            if !bc_pkcs12_is_sha1(ctx, this) {
                return ctx.invoke_virtual_bytecode_only(
                    this,
                    "generateDerivedParameters",
                    "(I)Lorg/bouncycastle/crypto/CipherParameters;",
                    &[args.get(1).copied().unwrap_or(Value::Int(0))],
                );
            }
            let (password, salt, iteration_count) = bc_pkcs12_read_state(ctx, this)?;
            let key = bc_pkcs12_derive_sha1(&password, &salt, iteration_count, 1, key_size);
            let obj = bc_pkcs12_key_parameter(ctx, &key)?;
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    bc_register_null_rerun!(
        r,
        CLS,
        "generateDerivedParameters",
        "(II)Lorg/bouncycastle/crypto/CipherParameters;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let key_size = match args.get(1) {
                Some(Value::Int(v)) if *v >= 0 => (*v as usize) / 8,
                Some(Value::Int(v)) => {
                    return Err(RuntimeError::NegativeArraySizeException { size: *v }.into())
                }
                _ => 0,
            };
            let iv_size = match args.get(2) {
                Some(Value::Int(v)) if *v >= 0 => (*v as usize) / 8,
                Some(Value::Int(v)) => {
                    return Err(RuntimeError::NegativeArraySizeException { size: *v }.into())
                }
                _ => 0,
            };
            if !bc_pkcs12_is_sha1(ctx, this) {
                return ctx.invoke_virtual_bytecode_only(
                    this,
                    "generateDerivedParameters",
                    "(II)Lorg/bouncycastle/crypto/CipherParameters;",
                    &[
                        args.get(1).copied().unwrap_or(Value::Int(0)),
                        args.get(2).copied().unwrap_or(Value::Int(0)),
                    ],
                );
            }
            let (password, salt, iteration_count) = bc_pkcs12_read_state(ctx, this)?;
            let key = bc_pkcs12_derive_sha1(&password, &salt, iteration_count, 1, key_size);
            let iv = bc_pkcs12_derive_sha1(&password, &salt, iteration_count, 2, iv_size);
            let obj = bc_pkcs12_parameters_with_iv(ctx, &key, &iv)?;
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    bc_register_null_rerun!(
        r,
        CLS,
        "generateDerivedMacParameters",
        "(I)Lorg/bouncycastle/crypto/CipherParameters;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let key_size = match args.get(1) {
                Some(Value::Int(v)) if *v >= 0 => (*v as usize) / 8,
                Some(Value::Int(v)) => {
                    return Err(RuntimeError::NegativeArraySizeException { size: *v }.into())
                }
                _ => 0,
            };
            if !bc_pkcs12_is_sha1(ctx, this) {
                return ctx.invoke_virtual_bytecode_only(
                    this,
                    "generateDerivedMacParameters",
                    "(I)Lorg/bouncycastle/crypto/CipherParameters;",
                    &[args.get(1).copied().unwrap_or(Value::Int(0))],
                );
            }
            let (password, salt, iteration_count) = bc_pkcs12_read_state(ctx, this)?;
            let key = bc_pkcs12_derive_sha1(&password, &salt, iteration_count, 3, key_size);
            let obj = bc_pkcs12_key_parameter(ctx, &key)?;
            Ok(Some(Value::Object(Some(obj))))
        },
    );

    }

/// Which PRF does this `PKCS5S2ParametersGenerator` actually carry?
///
/// PKCS#5 v2.0 is parameterised by an HMAC: `new PKCS5S2ParametersGenerator()`
/// is HMAC-SHA1, but `new PKCS5S2ParametersGenerator(new SHA256Digest())` — the
/// form BouncyCastle's own `PBE$Util.makePBEGenerator` uses for every non-SHA1
/// PRF — is not. The three `generateDerived*` intrinsics below used to pass a
/// hardcoded `1` (SHA-1) to `pbkdf2_derive_for` regardless, so **every**
/// non-SHA1 PBKDF2 through BouncyCastle silently derived the wrong key.
///
/// Measured 2026-08-13 against HotSpot 25 with the same jars: netty's
/// `SslContextBuilderTest`/`JdkSsl*ContextTest` `testPkcs8Des3EncryptedRsa`
/// reads `rsa_pkcs8_des3_encrypted.key`, PBES2 with PBKDF2-HMAC-**SHA256** and
/// DESede-CBC. `PBE$Util.makePBEMacParameters(spec, PKCS5S2_UTF8, SHA256, 192)`
/// returned `992951E4…` (the SHA-1 answer) instead of `9559B2B3…`, the DESede
/// decrypt then failed `BadPaddingException: pad block corrupted`, netty fell
/// back to the JDK PBES2 parser, and the test surfaced the JDK's own
/// `IOException: PBE parameter parsing error: expecting the object identifier
/// for AES cipher` — an error message three layers away from the defect.
///
/// Reads the generator's `hMac` field and asks it its own name
/// (`HMac.getAlgorithmName()` is `"<digest>/HMAC"`). Returns `None` for any
/// digest this VM's `pbkdf2_derive_for` does not implement (GOST3411, SM3,
/// SHA3-*, RIPEMD160, Whirlpool, …) so the caller can fall back to
/// BouncyCastle's own bytecode instead of substituting a PRF of our choosing —
/// substituting is exactly what produced the defect above.
pub fn bc_pkcs5s2_prf_code(ctx: &mut dyn NativeContext, this: ObjectRef) -> Option<i32> {
    let hmac = match ctx.get_field_by_name(this, "hMac") {
        Value::Object(Some(o)) => o,
        _ => return None,
    };
    let name = match ctx.invoke_virtual(hmac, "getAlgorithmName", "()Ljava/lang/String;", &[]) {
        Ok(Some(Value::Object(Some(s)))) => ctx.read_string(s).unwrap_or_default(),
        _ => return None,
    };
    // "SHA-256/HMAC" -> "SHA256"; also tolerates BC's older "SHA-256" spellings.
    let norm: String = name
        .split('/')
        .next()
        .unwrap_or("")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .collect::<String>()
        .to_ascii_uppercase();
    match norm.as_str() {
        "SHA1" => Some(1),
        "SHA224" => Some(224),
        "SHA256" => Some(256),
        "SHA384" => Some(384),
        "SHA512" => Some(512),
        _ => None,
    }
}

pub fn register_bc_pkcs5s2_parameters_generator(r: &mut NativeMethodRegistry) {
        
    const CLS: &str = "org/bouncycastle/crypto/generators/PKCS5S2ParametersGenerator";
    bc_register_null_rerun!(
        r,
        CLS,
        "generateDerivedParameters",
        "(I)Lorg/bouncycastle/crypto/CipherParameters;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let key_size = match args.get(1) {
                Some(Value::Int(v)) if *v >= 0 => (*v as usize) / 8,
                Some(Value::Int(v)) => {
                    return Err(RuntimeError::NegativeArraySizeException { size: *v }.into())
                }
                _ => 0,
            };
            let Some(prf) = bc_pkcs5s2_prf_code(ctx, this) else {
                return ctx.invoke_virtual_bytecode_only(
                    this,
                    "generateDerivedParameters",
                    "(I)Lorg/bouncycastle/crypto/CipherParameters;",
                    &[args.get(1).copied().unwrap_or(Value::Int(0))],
                );
            };
            let (password, salt, iteration_count) = bc_pkcs5s2_read_state(ctx, this)?;
            let key = crate::phases_early::pbkdf2_derive_for(
                prf,
                &password,
                &salt,
                iteration_count,
                key_size,
            );
            let obj = bc_pkcs12_key_parameter(ctx, &key)?;
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    bc_register_null_rerun!(
        r,
        CLS,
        "generateDerivedParameters",
        "(II)Lorg/bouncycastle/crypto/CipherParameters;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let key_size = match args.get(1) {
                Some(Value::Int(v)) if *v >= 0 => (*v as usize) / 8,
                Some(Value::Int(v)) => {
                    return Err(RuntimeError::NegativeArraySizeException { size: *v }.into())
                }
                _ => 0,
            };
            let iv_size = match args.get(2) {
                Some(Value::Int(v)) if *v >= 0 => (*v as usize) / 8,
                Some(Value::Int(v)) => {
                    return Err(RuntimeError::NegativeArraySizeException { size: *v }.into())
                }
                _ => 0,
            };
            let Some(prf) = bc_pkcs5s2_prf_code(ctx, this) else {
                return ctx.invoke_virtual_bytecode_only(
                    this,
                    "generateDerivedParameters",
                    "(II)Lorg/bouncycastle/crypto/CipherParameters;",
                    &[
                        args.get(1).copied().unwrap_or(Value::Int(0)),
                        args.get(2).copied().unwrap_or(Value::Int(0)),
                    ],
                );
            };
            let (password, salt, iteration_count) = bc_pkcs5s2_read_state(ctx, this)?;
            let derived = crate::phases_early::pbkdf2_derive_for(
                prf,
                &password,
                &salt,
                iteration_count,
                key_size + iv_size,
            );
            let obj = bc_pkcs12_parameters_with_iv(
                ctx,
                &derived[..key_size],
                &derived[key_size..key_size + iv_size],
            )?;
            Ok(Some(Value::Object(Some(obj))))
        },
    );
    bc_register_null_rerun!(
        r,
        CLS,
        "generateDerivedMacParameters",
        "(I)Lorg/bouncycastle/crypto/CipherParameters;",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let key_size = match args.get(1) {
                Some(Value::Int(v)) if *v >= 0 => (*v as usize) / 8,
                Some(Value::Int(v)) => {
                    return Err(RuntimeError::NegativeArraySizeException { size: *v }.into())
                }
                _ => 0,
            };
            let Some(prf) = bc_pkcs5s2_prf_code(ctx, this) else {
                return ctx.invoke_virtual_bytecode_only(
                    this,
                    "generateDerivedMacParameters",
                    "(I)Lorg/bouncycastle/crypto/CipherParameters;",
                    &[args.get(1).copied().unwrap_or(Value::Int(0))],
                );
            };
            let (password, salt, iteration_count) = bc_pkcs5s2_read_state(ctx, this)?;
            let key = crate::phases_early::pbkdf2_derive_for(
                prf,
                &password,
                &salt,
                iteration_count,
                key_size,
            );
            let obj = bc_pkcs12_key_parameter(ctx, &key)?;
            Ok(Some(Value::Object(Some(obj))))
        },
    );

    }

pub fn bc_bcrypt_illegal(message: &str) -> MethodCallFailed {
    RuntimeError::IllegalArgumentException {
        message: message.into(),
    }
    .into()
}

pub fn bc_bcrypt_byte_array_arg(
    ctx: &dyn NativeContext,
    args: &[Value],
    index: usize,
    null_message: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    match args.get(index) {
        Some(Value::Object(Some(o))) => Ok(*o),
        Some(Value::Object(None)) => Err(bc_bcrypt_illegal(null_message)),
        _ => Err(RuntimeError::IllegalStateException {
            message: "BCrypt.generate: malformed byte[] argument".into(),
        }
        .into()),
    }
    .map(|arr| {
        let _ = ctx.array_length(arr);
        arr
    })
}

pub fn bc_bcrypt_read_byte_array(ctx: &dyn NativeContext, arr: ObjectRef) -> Vec<u8> {
    let len = ctx.array_length(arr);
    let mut out = vec![0u8; len];
    let n = ctx.read_byte_array_into(arr, 0, &mut out);
    out.truncate(n);
    out
}

pub fn bc_bcrypt_read_static_i32_array(
    ctx: &dyn NativeContext,
    class_id: ClassId,
    field_name: &str,
) -> Result<Vec<i32>, MethodCallFailed> {
    let field = ctx
        .static_field_index_by_name(class_id, field_name)
        .ok_or_else(|| RuntimeError::IllegalStateException {
            message: format!("BCrypt native: missing static field {field_name}"),
        })?;
    let arr = match ctx.get_static_field(class_id, field) {
        Value::Object(Some(o)) => o,
        _ => {
            return Err(RuntimeError::IllegalStateException {
                message: format!("BCrypt native: malformed static field {field_name}"),
            }
            .into())
        }
    };
    let len = ctx.array_length(arr);
    let mut out = Vec::with_capacity(len);
    for i in 0..len {
        match ctx.get_array_element(arr, i) {
            Value::Int(v) => out.push(v),
            _ => {
                return Err(RuntimeError::IllegalStateException {
                    message: format!("BCrypt native: malformed int[] field {field_name}"),
                }
                .into())
            }
        }
    }
    Ok(out)
}

pub fn bc_bcrypt_f(s: &[i32], x: i32) -> i32 {
    let x = x as u32;
    let a = s[(x >> 24) as usize];
    let b = s[256 + ((x >> 16) & 0xff) as usize];
    let c = s[512 + ((x >> 8) & 0xff) as usize];
    let d = s[768 + (x & 0xff) as usize];
    (a.wrapping_add(b) ^ c).wrapping_add(d)
}

pub fn bc_bcrypt_encipher(p: &[i32], s: &[i32], mut xl: i32, mut xr: i32) -> (i32, i32) {
    xl ^= p[0];
    let mut i = 1usize;
    while i < 16 {
        xr ^= bc_bcrypt_f(s, xl) ^ p[i];
        xl ^= bc_bcrypt_f(s, xr) ^ p[i + 1];
        i += 2;
    }
    xr ^= p[17];
    (xr, xl)
}

pub fn bc_bcrypt_process_p(p: &mut [i32], s: &[i32], mut xl: i32, mut xr: i32) {
    let mut idx = 0usize;
    while idx < p.len() {
        let (left, right) = bc_bcrypt_encipher(p, s, xl, xr);
        p[idx] = left;
        p[idx + 1] = right;
        xr = right;
        xl = left;
        idx += 2;
    }
}

pub fn bc_bcrypt_process_s(p: &[i32], s: &mut [i32], mut xl: i32, mut xr: i32) {
    let mut idx = 0usize;
    while idx < s.len() {
        let (left, right) = bc_bcrypt_encipher(p, s, xl, xr);
        s[idx] = left;
        s[idx + 1] = right;
        xr = right;
        xl = left;
        idx += 2;
    }
}

pub fn bc_bcrypt_process_p_with_salt(
    p: &mut [i32],
    s: &[i32],
    salt: &[i32; 4],
    iv1: i32,
    iv2: i32,
) {
    let mut xl = iv1 ^ salt[0];
    let mut xr = iv2 ^ salt[1];
    let mut idx = 0usize;
    while idx < p.len() {
        let (left, right) = bc_bcrypt_encipher(p, s, xl, xr);
        p[idx] = left;
        p[idx + 1] = right;

        let yl = salt[2] ^ left;
        let yr = salt[3] ^ right;
        if idx + 2 >= p.len() {
            break;
        }

        let (left2, right2) = bc_bcrypt_encipher(p, s, yl, yr);
        p[idx + 2] = left2;
        p[idx + 3] = right2;
        xl = salt[0] ^ left2;
        xr = salt[1] ^ right2;
        idx += 4;
    }
}

pub fn bc_bcrypt_process_s_with_salt(
    p: &[i32],
    s: &mut [i32],
    salt: &[i32; 4],
    iv1: i32,
    iv2: i32,
) {
    let mut xl = iv1 ^ salt[0];
    let mut xr = iv2 ^ salt[1];
    let mut idx = 0usize;
    while idx < s.len() {
        let (left, right) = bc_bcrypt_encipher(p, s, xl, xr);
        s[idx] = left;
        s[idx + 1] = right;

        let yl = salt[2] ^ left;
        let yr = salt[3] ^ right;
        if idx + 2 >= s.len() {
            break;
        }

        let (left2, right2) = bc_bcrypt_encipher(p, s, yl, yr);
        s[idx + 2] = left2;
        s[idx + 3] = right2;
        xl = salt[0] ^ left2;
        xr = salt[1] ^ right2;
        idx += 4;
    }
}

pub fn bc_bcrypt_cyclic_xor_key(p: &mut [i32], key: &[u8]) {
    let mut key_index = 0usize;
    for slot in p.iter_mut() {
        let mut data = 0i32;
        for _ in 0..4 {
            data = (data << 8) | i32::from(key[key_index] & 0xff);
            key_index += 1;
            if key_index >= key.len() {
                key_index = 0;
            }
        }
        *slot ^= data;
    }
}

pub fn bc_bcrypt_be_i32(bytes: &[u8], off: usize) -> i32 {
    i32::from_be_bytes([bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]])
}

pub fn bc_bcrypt_ints_to_be_bytes(ints: &[i32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(ints.len() * 4);
    for &v in ints {
        out.extend_from_slice(&v.to_be_bytes());
    }
    out
}

/// Execute the common BCrypt key schedule against a library's published P/S
/// constants. Both Bouncy Castle and Spring Security expose the same standard
/// Blowfish tables, but they use different class/field names and otherwise
/// leave this deliberately expensive work in Java bytecode.
pub fn bc_bcrypt_generate_raw_with_constants(
    ctx: &mut dyn NativeContext,
    password: &[u8],
    salt: &[u8],
    cost: i32,
    class_name: &str,
    p_fields: &[&str],
    s_fields: &[&str],
) -> Result<Vec<u8>, MethodCallFailed> {
    let class_id = ctx.ensure_class_initialized(class_name)?;
    let mut p = Vec::new();
    for field in p_fields {
        p.extend_from_slice(&bc_bcrypt_read_static_i32_array(ctx, class_id, field)?);
    }
    let mut s = Vec::with_capacity(1024);
    for field in s_fields {
        s.extend_from_slice(&bc_bcrypt_read_static_i32_array(ctx, class_id, field)?);
    }
    if p.len() != 18 || s.len() != 1024 {
        return Err(RuntimeError::IllegalStateException {
            message: "BCrypt native: malformed P/S constants".into(),
        }
        .into());
    }

    let mut psw = if password.is_empty() {
        vec![0u8; 4]
    } else {
        password.to_vec()
    };
    let salt32 = [
        bc_bcrypt_be_i32(salt, 0),
        bc_bcrypt_be_i32(salt, 4),
        bc_bcrypt_be_i32(salt, 8),
        bc_bcrypt_be_i32(salt, 12),
    ];
    let salt32_swapped = [salt32[2], salt32[3], salt32[0], salt32[1]];

    bc_bcrypt_cyclic_xor_key(&mut p, &psw);
    bc_bcrypt_process_p_with_salt(&mut p, &s, &salt32, 0, 0);
    let p_tail0 = p[p.len() - 2];
    let p_tail1 = p[p.len() - 1];
    bc_bcrypt_process_s_with_salt(&p, &mut s, &salt32_swapped, p_tail0, p_tail1);

    let rounds = 1u32.checked_shl(cost as u32).unwrap_or(0);
    for _ in 0..rounds {
        bc_bcrypt_cyclic_xor_key(&mut p, &psw);
        bc_bcrypt_process_p(&mut p, &s, 0, 0);
        let p_tail0 = p[p.len() - 2];
        let p_tail1 = p[p.len() - 1];
        bc_bcrypt_process_s(&p, &mut s, p_tail0, p_tail1);

        bc_bcrypt_cyclic_xor_key(&mut p, salt);
        bc_bcrypt_process_p(&mut p, &s, 0, 0);
        let p_tail0 = p[p.len() - 2];
        let p_tail1 = p[p.len() - 1];
        bc_bcrypt_process_s(&p, &mut s, p_tail0, p_tail1);
    }

    let mut text = [
        0x4f72_7068i32,
        0x6561_6e42i32,
        0x6568_6f6ci32,
        0x6465_7253i32,
        0x6372_7944i32,
        0x6f75_6274i32,
    ];
    for _ in 0..64 {
        for j in (0..6).step_by(2) {
            let (left, right) = bc_bcrypt_encipher(&p, &s, text[j], text[j + 1]);
            text[j] = left;
            text[j + 1] = right;
        }
    }
    psw.fill(0);
    Ok(bc_bcrypt_ints_to_be_bytes(&text))
}

pub fn bc_bcrypt_generate_raw(
    ctx: &mut dyn NativeContext,
    password: &[u8],
    salt: &[u8],
    cost: i32,
) -> Result<Vec<u8>, MethodCallFailed> {
    bc_bcrypt_generate_raw_with_constants(
        ctx,
        password,
        salt,
        cost,
        "org/bouncycastle/crypto/generators/BCrypt",
        &["KP"],
        &["KS0", "KS1", "KS2", "KS3"],
    )
}

pub fn bc_bcrypt_generate(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let pw_arr = bc_bcrypt_byte_array_arg(ctx, args, 0, "pwInput and salt are required")?;
    let salt_arr = bc_bcrypt_byte_array_arg(ctx, args, 1, "pwInput and salt are required")?;
    let cost = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    if ctx.array_length(salt_arr) != 16 {
        return Err(bc_bcrypt_illegal("BCrypt salt must be 128 bits"));
    }
    if ctx.array_length(pw_arr) > 72 {
        return Err(bc_bcrypt_illegal("BCrypt password must be <= 72 bytes"));
    }
    if !(4..=31).contains(&cost) {
        return Err(bc_bcrypt_illegal("BCrypt cost must be from 4..31"));
    }
    let password = bc_bcrypt_read_byte_array(ctx, pw_arr);
    let salt = bc_bcrypt_read_byte_array(ctx, salt_arr);
    let hash = bc_bcrypt_generate_raw(ctx, &password, &salt, cost)?;
    let out = ctx.new_array(cratonvm_types::ArrayElementType::Byte, hash.len());
    ctx.write_byte_array_from(out, 0, &hash);
    Ok(Some(Value::Object(Some(out))))
}

/// Spring Security's BCrypt implementation spends virtually all of its time
/// in `crypt_raw` repeatedly invoking its Java `encipher` loop. At the normal
/// cost of 10, that is fast on HotSpot but takes minutes in the interpreter,
/// turning ordinary password assertions into suite timeouts. Keep Spring's
/// surrounding bytecode responsible for salt parsing, revision handling,
/// output encoding, and comparison; replace only the standard key schedule.
///
/// `sign_ext_bug` is only used for the historical `$2x$` compatibility mode.
/// Spring's supported `$2a$`, `$2b$`, and `$2y$` paths pass false and share the
/// standard BCrypt schedule used below. Do not apply this intrinsic to `$2x$`:
/// that obsolete compatibility variant must retain Spring's bytecode semantics.
pub fn spring_security_bcrypt_crypt_raw(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let password_arr = bc_bcrypt_byte_array_arg(ctx, args, 1, "Bad password")?;
    let salt_arr = bc_bcrypt_byte_array_arg(ctx, args, 2, "Bad salt length")?;
    let cost = match args.get(3) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let sign_ext_bug = matches!(args.get(4), Some(Value::Int(v)) if *v != 0);
    if sign_ext_bug {
        return ctx.invoke_special_bytecode_only(
            "org/springframework/security/crypto/bcrypt/BCrypt",
            "crypt_raw",
            "([B[BIZIZ)[B",
            args,
        );
    }
    if ctx.array_length(salt_arr) != 16 {
        return Err(bc_bcrypt_illegal("Bad salt length"));
    }
    if !(4..=31).contains(&cost) {
        return Err(bc_bcrypt_illegal("Bad number of rounds"));
    }
    let password = bc_bcrypt_read_byte_array(ctx, password_arr);
    let salt = bc_bcrypt_read_byte_array(ctx, salt_arr);
    let hash = bc_bcrypt_generate_raw_with_constants(
        ctx,
        &password,
        &salt,
        cost,
        "org/springframework/security/crypto/bcrypt/BCrypt",
        &["P_orig"],
        &["S_orig"],
    )?;
    let out = ctx.new_array(cratonvm_types::ArrayElementType::Byte, hash.len());
    ctx.write_byte_array_from(out, 0, &hash);
    Ok(Some(Value::Object(Some(out))))
}

pub fn register_bc_bcrypt_generator(r: &mut NativeMethodRegistry) {
            bc_register_null_rerun!(
        r,
        "org/bouncycastle/crypto/generators/BCrypt",
        "generate",
        "([B[BI)[B",
        bc_bcrypt_generate,
    );
    bc_register_null_rerun!(
        r,
        "org/springframework/security/crypto/bcrypt/BCrypt",
        "crypt_raw",
        "([B[BIZIZ)[B",
        spring_security_bcrypt_crypt_raw,
    );
}

pub fn gcm_fallback(ctx: &mut dyn NativeContext, this: ObjectRef, args: &[Value]) -> MethodCallResult {
    ctx.invoke_virtual_bytecode_only(this, "processBytes", "([BII[BI)I", &args[1..])
}

pub fn bc_aes_gcm_process_bytes(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let in_arr = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return gcm_fallback(ctx, this, args),
    };
    let in_off = match args.get(2) {
        Some(Value::Int(v)) if *v >= 0 => *v as usize,
        _ => return gcm_fallback(ctx, this, args),
    };
    let len = match args.get(3) {
        Some(Value::Int(v)) if *v >= 16 => *v as usize,
        _ => return gcm_fallback(ctx, this, args),
    };
    let out_arr = match args.get(4) {
        Some(Value::Object(Some(o))) => *o,
        _ => return gcm_fallback(ctx, this, args),
    };
    let out_off = match args.get(5) {
        Some(Value::Int(v)) if *v >= 0 => *v as usize,
        _ => return gcm_fallback(ctx, this, args),
    };
    if in_arr == out_arr {
        return gcm_fallback(ctx, this, args);
    }
    if in_off.saturating_add(len) > ctx.array_length(in_arr) {
        return gcm_fallback(ctx, this, args);
    }
    if !matches!(ctx.get_field_by_name(this, "initialised"), Value::Int(v) if v != 0) {
        return gcm_fallback(ctx, this, args);
    }
    if !matches!(ctx.get_field_by_name(this, "bufOff"), Value::Int(0)) {
        return gcm_fallback(ctx, this, args);
    }
    let for_encryption = matches!(ctx.get_field_by_name(this, "forEncryption"), Value::Int(v) if v != 0);
    let mac_size = match ctx.get_field_by_name(this, "macSize") {
        Value::Int(v) if v > 0 && v <= 16 => v as usize,
        _ => return gcm_fallback(ctx, this, args),
    };
    let num_blocks = if for_encryption {
        len / 16
    } else {
        if len < mac_size {
            return gcm_fallback(ctx, this, args);
        }
        (len - mac_size) / 16
    };
    if num_blocks == 0 {
        return gcm_fallback(ctx, this, args);
    }
    let result_len = num_blocks * 16;
    if out_off.saturating_add(result_len) > ctx.array_length(out_arr) {
        return gcm_fallback(ctx, this, args);
    }
    let mut blocks_remaining = match ctx.get_field_by_name(this, "blocksRemaining") {
        Value::Int(v) if v != 0 && ((v as u32) as usize) >= num_blocks => v as u32,
        _ => return gcm_fallback(ctx, this, args),
    };
    let cipher = match ctx.get_field_by_name(this, "cipher") {
        Value::Object(Some(c)) => c,
        _ => return gcm_fallback(ctx, this, args),
    };
    if !matches!(ctx.get_field_by_name(cipher, "forEncryption"), Value::Int(v) if v != 0) {
        return gcm_fallback(ctx, this, args);
    }
    let wk_arr = match ctx.get_field_by_name(cipher, "WorkingKey") {
        Value::Object(Some(w)) => w,
        _ => return gcm_fallback(ctx, this, args),
    };
    let kw = read_aes_kw(ctx, wk_arr);
    if kw.len() != 11 && kw.len() != 13 && kw.len() != 15 {
        return gcm_fallback(ctx, this, args);
    }
    let h_arr = match ctx.get_field_by_name(this, "H") {
        Value::Object(Some(o)) if ctx.array_length(o) >= 16 => o,
        _ => return gcm_fallback(ctx, this, args),
    };
    let s_arr = match ctx.get_field_by_name(this, "S") {
        Value::Object(Some(o)) if ctx.array_length(o) >= 16 => o,
        _ => return gcm_fallback(ctx, this, args),
    };
    let counter_arr = match ctx.get_field_by_name(this, "counter") {
        Value::Object(Some(o)) if ctx.array_length(o) >= 16 => o,
        _ => return gcm_fallback(ctx, this, args),
    };
    let rem = len - result_len;
    let buf_block_arr = match ctx.get_field_by_name(this, "bufBlock") {
        Value::Object(Some(o)) if ctx.array_length(o) >= rem => o,
        _ => return gcm_fallback(ctx, this, args),
    };
    let total_length = match ctx.get_field_by_name(this, "totalLength") {
        Value::Long(v) if v >= 0 => v,
        _ => return gcm_fallback(ctx, this, args),
    };

    let mut h = [0u8; 16];
    ctx.read_byte_array_into(h_arr, 0, &mut h);
    let mut s_state = [0u8; 16];
    ctx.read_byte_array_into(s_arr, 0, &mut s_state);
    let mut counter = [0u8; 16];
    ctx.read_byte_array_into(counter_arr, 0, &mut counter);

    if total_length == 0 {
        let at_length = match ctx.get_field_by_name(this, "atLength") {
            Value::Long(v) => v,
            _ => return gcm_fallback(ctx, this, args),
        };
        let at_block_pos = match ctx.get_field_by_name(this, "atBlockPos") {
            Value::Int(v) if v >= 0 && v <= 16 => v as usize,
            _ => return gcm_fallback(ctx, this, args),
        };
        let mut at_length_pre = match ctx.get_field_by_name(this, "atLengthPre") {
            Value::Long(v) => v,
            _ => return gcm_fallback(ctx, this, args),
        };
        if at_length > 0 || at_block_pos > 0 || at_length_pre > 0 {
            let s_at_arr = match ctx.get_field_by_name(this, "S_at") {
                Value::Object(Some(o)) if ctx.array_length(o) >= 16 => o,
                _ => return gcm_fallback(ctx, this, args),
            };
            let s_at_pre_arr = match ctx.get_field_by_name(this, "S_atPre") {
                Value::Object(Some(o)) if ctx.array_length(o) >= 16 => o,
                _ => return gcm_fallback(ctx, this, args),
            };
            let mut s_at_pre = [0u8; 16];
            if at_length > 0 {
                ctx.read_byte_array_into(s_at_arr, 0, &mut s_at_pre);
                at_length_pre = at_length;
            } else {
                ctx.read_byte_array_into(s_at_pre_arr, 0, &mut s_at_pre);
            }
            if at_block_pos > 0 {
                let at_block_arr = match ctx.get_field_by_name(this, "atBlock") {
                    Value::Object(Some(o)) if ctx.array_length(o) >= at_block_pos => o,
                    _ => return gcm_fallback(ctx, this, args),
                };
                let mut at_blk = [0u8; 16];
                ctx.read_byte_array_into(at_block_arr, 0, &mut at_blk[..at_block_pos]);
                cratonvm_native_builtins_crypto::bc_aes::gcm_ghash_partial(
                    &mut s_at_pre, &h, &at_blk[..at_block_pos],
                );
                at_length_pre += at_block_pos as i64;
            }
            if at_length > 0 || at_block_pos > 0 {
                ctx.write_byte_array_from(s_at_pre_arr, 0, &s_at_pre);
                ctx.set_field_by_name(this, "atLengthPre", Value::Long(at_length_pre));
            }
            if at_length_pre > 0 {
                s_state = s_at_pre;
            }
        }
    }

    let mut in_buf = [0u8; 4096];
    let mut out_buf = [0u8; 4096];
    let mut done = 0usize;
    while done < result_len {
        let chunk = (result_len - done).min(4096);
        ctx.read_byte_array_into(in_arr, in_off + done, &mut in_buf[..chunk]);
        if cratonvm_native_builtins_crypto::bc_aes::gcm_process_blocks(
            &kw,
            &h,
            &mut s_state,
            &mut counter,
            &mut blocks_remaining,
            for_encryption,
            &in_buf[..chunk],
            &mut out_buf[..chunk],
        )
        .is_err()
        {
            return gcm_fallback(ctx, this, args);
        }
        ctx.write_byte_array_from(out_arr, out_off + done, &out_buf[..chunk]);
        done += chunk;
    }

    ctx.write_byte_array_from(s_arr, 0, &s_state);
    ctx.write_byte_array_from(counter_arr, 0, &counter);
    ctx.set_field_by_name(this, "blocksRemaining", Value::Int(blocks_remaining as i32));
    ctx.set_field_by_name(
        this,
        "totalLength",
        Value::Long(total_length + result_len as i64),
    );
    if rem > 0 {
        let mut tail = [0u8; 32];
        ctx.read_byte_array_into(in_arr, in_off + result_len, &mut tail[..rem]);
        ctx.write_byte_array_from(buf_block_arr, 0, &tail[..rem]);
    }
    ctx.set_field_by_name(this, "bufOff", Value::Int(rem as i32));
    Ok(Some(Value::Int(result_len as i32)))
}

pub fn gcm_fallback_method(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    name: &str,
    desc: &str,
    args: &[Value],
) -> MethodCallResult {
    ctx.invoke_virtual_bytecode_only(this, name, desc, &args[1..])
}

pub fn bc_aes_gcm_process_aad_bytes(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let in_arr = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return gcm_fallback_method(ctx, this, "processAADBytes", "([BII)V", args),
    };
    let in_off = match args.get(2) {
        Some(Value::Int(v)) if *v >= 0 => *v as usize,
        _ => return gcm_fallback_method(ctx, this, "processAADBytes", "([BII)V", args),
    };
    let len = match args.get(3) {
        Some(Value::Int(v)) if *v >= 0 => *v as usize,
        _ => return gcm_fallback_method(ctx, this, "processAADBytes", "([BII)V", args),
    };
    if in_off.saturating_add(len) > ctx.array_length(in_arr) {
        return gcm_fallback_method(ctx, this, "processAADBytes", "([BII)V", args);
    }
    if !matches!(ctx.get_field_by_name(this, "initialised"), Value::Int(v) if v != 0) {
        return gcm_fallback_method(ctx, this, "processAADBytes", "([BII)V", args);
    }
    if len == 0 {
        return Ok(None);
    }
    let h_arr = match ctx.get_field_by_name(this, "H") {
        Value::Object(Some(o)) if ctx.array_length(o) >= 16 => o,
        _ => return gcm_fallback_method(ctx, this, "processAADBytes", "([BII)V", args),
    };
    let s_at_arr = match ctx.get_field_by_name(this, "S_at") {
        Value::Object(Some(o)) if ctx.array_length(o) >= 16 => o,
        _ => return gcm_fallback_method(ctx, this, "processAADBytes", "([BII)V", args),
    };
    let at_block_arr = match ctx.get_field_by_name(this, "atBlock") {
        Value::Object(Some(o)) if ctx.array_length(o) >= 16 => o,
        _ => return gcm_fallback_method(ctx, this, "processAADBytes", "([BII)V", args),
    };
    let mut at_block_pos = match ctx.get_field_by_name(this, "atBlockPos") {
        Value::Int(v) if (0..16).contains(&v) => v as usize,
        _ => return gcm_fallback_method(ctx, this, "processAADBytes", "([BII)V", args),
    };
    let mut at_length = match ctx.get_field_by_name(this, "atLength") {
        Value::Long(v) if v >= 0 => v,
        _ => return gcm_fallback_method(ctx, this, "processAADBytes", "([BII)V", args),
    };

    let mut h = [0u8; 16];
    let mut s_at = [0u8; 16];
    let mut at_block = [0u8; 16];
    ctx.read_byte_array_into(h_arr, 0, &mut h);
    ctx.read_byte_array_into(s_at_arr, 0, &mut s_at);
    if at_block_pos > 0 {
        ctx.read_byte_array_into(at_block_arr, 0, &mut at_block[..at_block_pos]);
    }

    let mut stack = [0u8; 1024];
    let mut heap = Vec::new();
    let in_buf: &mut [u8] = if len <= stack.len() {
        &mut stack[..len]
    } else {
        heap.resize(len, 0);
        &mut heap
    };
    ctx.read_byte_array_into(in_arr, in_off, in_buf);

    let mut pos = 0usize;
    if at_block_pos > 0 {
        let take = (16 - at_block_pos).min(len);
        at_block[at_block_pos..at_block_pos + take].copy_from_slice(&in_buf[..take]);
        at_block_pos += take;
        pos += take;
        if at_block_pos == 16 {
            crate::bc_aes::gcm_ghash_partial(&mut s_at, &h, &at_block);
            at_block_pos = 0;
            at_length += 16;
        }
    }
    let rem_full = (len - pos) / 16;
    if rem_full > 0 {
        let bytes = rem_full * 16;
        crate::bc_aes::gcm_ghash_blocks(&mut s_at, &h, &in_buf[pos..pos + bytes]);
        at_length += bytes as i64;
        pos += bytes;
    }
    let tail = len - pos;
    if tail > 0 {
        at_block[..tail].copy_from_slice(&in_buf[pos..]);
        at_block_pos = tail;
    }

    ctx.write_byte_array_from(s_at_arr, 0, &s_at);
    ctx.write_byte_array_from(at_block_arr, 0, &at_block);
    ctx.set_field_by_name(this, "atBlockPos", Value::Int(at_block_pos as i32));
    ctx.set_field_by_name(this, "atLength", Value::Long(at_length));
    Ok(None)
}

pub fn bc_aes_gcm_do_final(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let out_arr = match args.get(1) {
        Some(Value::Object(Some(o))) => *o,
        _ => return gcm_fallback_method(ctx, this, "doFinal", "([BI)I", args),
    };
    let out_off = match args.get(2) {
        Some(Value::Int(v)) if *v >= 0 => *v as usize,
        _ => return gcm_fallback_method(ctx, this, "doFinal", "([BI)I", args),
    };
    if !matches!(ctx.get_field_by_name(this, "initialised"), Value::Int(v) if v != 0) {
        return gcm_fallback_method(ctx, this, "doFinal", "([BI)I", args);
    }
    if !matches!(
        ctx.get_field_by_name(this, "initialAssociatedText"),
        Value::Object(None)
    ) {
        return gcm_fallback_method(ctx, this, "doFinal", "([BI)I", args);
    }
    let for_encryption = matches!(ctx.get_field_by_name(this, "forEncryption"), Value::Int(v) if v != 0);
    let mac_size = match ctx.get_field_by_name(this, "macSize") {
        Value::Int(v) if (4..=16).contains(&v) => v as usize,
        _ => return gcm_fallback_method(ctx, this, "doFinal", "([BI)I", args),
    };
    let buf_off = match ctx.get_field_by_name(this, "bufOff") {
        Value::Int(v) if (0..=32).contains(&v) => v as usize,
        _ => return gcm_fallback_method(ctx, this, "doFinal", "([BI)I", args),
    };
    let extra = if for_encryption {
        buf_off
    } else {
        if buf_off < mac_size {
            return gcm_fallback_method(ctx, this, "doFinal", "([BI)I", args);
        }
        buf_off - mac_size
    };
    let result_len = if for_encryption { extra + mac_size } else { extra };
    if out_off.saturating_add(result_len) > ctx.array_length(out_arr) {
        return gcm_fallback_method(ctx, this, "doFinal", "([BI)I", args);
    }
    let cipher = match ctx.get_field_by_name(this, "cipher") {
        Value::Object(Some(c)) => c,
        _ => return gcm_fallback_method(ctx, this, "doFinal", "([BI)I", args),
    };
    if !matches!(ctx.get_field_by_name(cipher, "forEncryption"), Value::Int(v) if v != 0) {
        return gcm_fallback_method(ctx, this, "doFinal", "([BI)I", args);
    }
    let wk_arr = match ctx.get_field_by_name(cipher, "WorkingKey") {
        Value::Object(Some(w)) => w,
        _ => return gcm_fallback_method(ctx, this, "doFinal", "([BI)I", args),
    };
    let kw = read_aes_kw(ctx, wk_arr);
    if kw.len() < 2 {
        return gcm_fallback_method(ctx, this, "doFinal", "([BI)I", args);
    }
    let mut blocks_remaining = match ctx.get_field_by_name(this, "blocksRemaining") {
        Value::Int(v) => v as u32,
        _ => return gcm_fallback_method(ctx, this, "doFinal", "([BI)I", args),
    };
    if extra > 0 && blocks_remaining == 0 {
        return gcm_fallback_method(ctx, this, "doFinal", "([BI)I", args);
    }

    let h_arr = match ctx.get_field_by_name(this, "H") {
        Value::Object(Some(o)) if ctx.array_length(o) >= 16 => o,
        _ => return gcm_fallback_method(ctx, this, "doFinal", "([BI)I", args),
    };
    let s_arr = match ctx.get_field_by_name(this, "S") {
        Value::Object(Some(o)) if ctx.array_length(o) >= 16 => o,
        _ => return gcm_fallback_method(ctx, this, "doFinal", "([BI)I", args),
    };
    let s_at_arr = match ctx.get_field_by_name(this, "S_at") {
        Value::Object(Some(o)) if ctx.array_length(o) >= 16 => o,
        _ => return gcm_fallback_method(ctx, this, "doFinal", "([BI)I", args),
    };
    let s_at_pre_arr = match ctx.get_field_by_name(this, "S_atPre") {
        Value::Object(Some(o)) if ctx.array_length(o) >= 16 => o,
        _ => return gcm_fallback_method(ctx, this, "doFinal", "([BI)I", args),
    };
    let at_block_arr = match ctx.get_field_by_name(this, "atBlock") {
        Value::Object(Some(o)) if ctx.array_length(o) >= 16 => o,
        _ => return gcm_fallback_method(ctx, this, "doFinal", "([BI)I", args),
    };
    let j0_arr = match ctx.get_field_by_name(this, "J0") {
        Value::Object(Some(o)) if ctx.array_length(o) >= 16 => o,
        _ => return gcm_fallback_method(ctx, this, "doFinal", "([BI)I", args),
    };
    let counter_arr = match ctx.get_field_by_name(this, "counter") {
        Value::Object(Some(o)) if ctx.array_length(o) >= 16 => o,
        _ => return gcm_fallback_method(ctx, this, "doFinal", "([BI)I", args),
    };
    let buf_block_arr = match ctx.get_field_by_name(this, "bufBlock") {
        Value::Object(Some(o)) if ctx.array_length(o) >= buf_off => o,
        _ => return gcm_fallback_method(ctx, this, "doFinal", "([BI)I", args),
    };
    let mut total_length = match ctx.get_field_by_name(this, "totalLength") {
        Value::Long(v) if v >= 0 => v,
        _ => return gcm_fallback_method(ctx, this, "doFinal", "([BI)I", args),
    };
    let at_block_pos = match ctx.get_field_by_name(this, "atBlockPos") {
        Value::Int(v) if (0..16).contains(&v) => v as usize,
        _ => return gcm_fallback_method(ctx, this, "doFinal", "([BI)I", args),
    };
    let mut at_length = match ctx.get_field_by_name(this, "atLength") {
        Value::Long(v) if v >= 0 => v,
        _ => return gcm_fallback_method(ctx, this, "doFinal", "([BI)I", args),
    };
    let mut at_length_pre = match ctx.get_field_by_name(this, "atLengthPre") {
        Value::Long(v) if v >= 0 => v,
        _ => return gcm_fallback_method(ctx, this, "doFinal", "([BI)I", args),
    };

    let mut h = [0u8; 16];
    let mut s = [0u8; 16];
    let mut j0 = [0u8; 16];
    let mut counter = [0u8; 16];
    ctx.read_byte_array_into(h_arr, 0, &mut h);
    ctx.read_byte_array_into(j0_arr, 0, &mut j0);
    ctx.read_byte_array_into(counter_arr, 0, &mut counter);

    if total_length == 0 {
        // Inline initCipher() + GCMBlockCipher.doFinal line 513 (atLength += atBlockPos)
        if at_length > 0 || at_block_pos > 0 || at_length_pre > 0 {
            let mut s_at_pre = [0u8; 16];
            if at_length > 0 {
                ctx.read_byte_array_into(s_at_arr, 0, &mut s_at_pre);
                at_length_pre = at_length;
            } else {
                ctx.read_byte_array_into(s_at_pre_arr, 0, &mut s_at_pre);
            }
            if at_block_pos > 0 {
                let mut at_block = [0u8; 16];
                ctx.read_byte_array_into(at_block_arr, 0, &mut at_block[..at_block_pos]);
                crate::bc_aes::gcm_ghash_partial(&mut s_at_pre, &h, &at_block[..at_block_pos]);
                at_length_pre += at_block_pos as i64;
                at_length += at_block_pos as i64;
            }
            if at_length_pre > 0 {
                s = s_at_pre;
            }
        }
    } else {
        if at_block_pos > 0 || at_length > at_length_pre {
            return gcm_fallback_method(ctx, this, "doFinal", "([BI)I", args);
        }
        ctx.read_byte_array_into(s_arr, 0, &mut s);
    }

    let mut buf_block = [0u8; 32];
    if buf_off > 0 {
        ctx.read_byte_array_into(buf_block_arr, 0, &mut buf_block[..buf_off]);
    }
    let mut out_partial = [0u8; 16];
    if extra > 0 {
        blocks_remaining -= 1;
        let _ = blocks_remaining;
        let ctr_word = u32::from_be_bytes([counter[12], counter[13], counter[14], counter[15]]).wrapping_add(1);
        counter[12..16].copy_from_slice(&ctr_word.to_be_bytes());
        let mut ctr_block = [0u8; 16];
        crate::bc_aes::encrypt_block(&kw, &counter, &mut ctr_block);
        if for_encryption {
            for k in 0..extra {
                out_partial[k] = buf_block[k] ^ ctr_block[k];
            }
            crate::bc_aes::gcm_ghash_partial(&mut s, &h, &out_partial[..extra]);
        } else {
            crate::bc_aes::gcm_ghash_partial(&mut s, &h, &buf_block[..extra]);
            for k in 0..extra {
                out_partial[k] = buf_block[k] ^ ctr_block[k];
            }
        }
        total_length += extra as i64;
    }

    let mut len_block = [0u8; 16];
    len_block[0..8].copy_from_slice(&((at_length as u64).wrapping_mul(8)).to_be_bytes());
    len_block[8..16].copy_from_slice(&((total_length as u64).wrapping_mul(8)).to_be_bytes());
    crate::bc_aes::gcm_ghash_partial(&mut s, &h, &len_block);

    let mut tag_mask = [0u8; 16];
    crate::bc_aes::encrypt_block(&kw, &j0, &mut tag_mask);
    let mut calc_mac = [0u8; 16];
    for k in 0..mac_size {
        calc_mac[k] = tag_mask[k] ^ s[k];
    }

    if !for_encryption {
        let mut diff = 0u8;
        for k in 0..mac_size {
            diff |= calc_mac[k] ^ buf_block[extra + k];
        }
        if diff != 0 {
            return gcm_fallback_method(ctx, this, "doFinal", "([BI)I", args);
        }
    }

    if extra > 0 {
        ctx.write_byte_array_from(out_arr, out_off, &out_partial[..extra]);
    }
    if for_encryption {
        ctx.write_byte_array_from(out_arr, out_off + extra, &calc_mac[..mac_size]);
    }
    let mac_arr = ctx.new_array(cratonvm_types::ArrayElementType::Byte, mac_size);
    ctx.write_byte_array_from(mac_arr, 0, &calc_mac[..mac_size]);
    ctx.set_field_by_name(this, "macBlock", Value::Object(Some(mac_arr)));

    // Inline reset(false)
    let zeros = [0u8; 16];
    ctx.write_byte_array_from(s_arr, 0, &zeros);
    ctx.write_byte_array_from(s_at_arr, 0, &zeros);
    ctx.write_byte_array_from(s_at_pre_arr, 0, &zeros);
    ctx.write_byte_array_from(at_block_arr, 0, &zeros);
    let buf_len = ctx.array_length(buf_block_arr);
    if buf_len > 0 && buf_len <= 32 {
        let bzeros = [0u8; 32];
        ctx.write_byte_array_from(buf_block_arr, 0, &bzeros[..buf_len]);
    }
    ctx.write_byte_array_from(counter_arr, 0, &j0);
    ctx.set_field_by_name(this, "atBlockPos", Value::Int(0));
    ctx.set_field_by_name(this, "atLength", Value::Long(0));
    ctx.set_field_by_name(this, "atLengthPre", Value::Long(0));
    ctx.set_field_by_name(this, "blocksRemaining", Value::Int(-2));
    ctx.set_field_by_name(this, "bufOff", Value::Int(0));
    ctx.set_field_by_name(this, "totalLength", Value::Long(0));
    if for_encryption {
        ctx.set_field_by_name(this, "initialised", Value::Int(0));
    }

    Ok(Some(Value::Int(result_len as i32)))
}

pub fn register_bc_gcm_block_cipher(r: &mut NativeMethodRegistry) {
            r.register(
        "org/bouncycastle/crypto/modes/GCMBlockCipher",
        "processBytes",
        "([BII[BI)I",
        bc_aes_gcm_process_bytes, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        "org/bouncycastle/crypto/modes/GCMBlockCipher",
        "processAADBytes",
        "([BII)V",
        bc_aes_gcm_process_aad_bytes, cratonvm_native_api::NativeKind::Intrinsic,
    );
    r.register(
        "org/bouncycastle/crypto/modes/GCMBlockCipher",
        "doFinal",
        "([BI)I",
        bc_aes_gcm_do_final, cratonvm_native_api::NativeKind::Intrinsic,
    );
    }
