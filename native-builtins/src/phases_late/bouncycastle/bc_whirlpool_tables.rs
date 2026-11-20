#[allow(unused_imports)]
use super::*;

pub fn bc_whirlpool_tables() -> &'static BcWhirlpoolTables {
    static TABLES: std::sync::OnceLock<BcWhirlpoolTables> = std::sync::OnceLock::new();
    TABLES.get_or_init(|| {
        let mut c = [[0u64; 256]; 8];
        for (i, &v1) in BC_WHIRLPOOL_SBOX.iter().enumerate() {
            let v1 = v1 as u32;
            let v2 = bc_whirlpool_mul_x(v1);
            let v4 = bc_whirlpool_mul_x(v2);
            let v5 = v4 ^ v1;
            let v8 = bc_whirlpool_mul_x(v4);
            let v9 = v8 ^ v1;

            c[0][i] = bc_whirlpool_pack(v1, v1, v4, v1, v8, v5, v2, v9);
            c[1][i] = bc_whirlpool_pack(v9, v1, v1, v4, v1, v8, v5, v2);
            c[2][i] = bc_whirlpool_pack(v2, v9, v1, v1, v4, v1, v8, v5);
            c[3][i] = bc_whirlpool_pack(v5, v2, v9, v1, v1, v4, v1, v8);
            c[4][i] = bc_whirlpool_pack(v8, v5, v2, v9, v1, v1, v4, v1);
            c[5][i] = bc_whirlpool_pack(v1, v8, v5, v2, v9, v1, v1, v4);
            c[6][i] = bc_whirlpool_pack(v4, v1, v8, v5, v2, v9, v1, v1);
            c[7][i] = bc_whirlpool_pack(v1, v4, v1, v8, v5, v2, v9, v1);
        }

        let mut rc = [0u64; 11];
        for r in 1..=10 {
            let i = 8 * (r - 1);
            rc[r] = (c[0][i] & 0xff00_0000_0000_0000)
                ^ (c[1][i + 1] & 0x00ff_0000_0000_0000)
                ^ (c[2][i + 2] & 0x0000_ff00_0000_0000)
                ^ (c[3][i + 3] & 0x0000_00ff_0000_0000)
                ^ (c[4][i + 4] & 0x0000_0000_ff00_0000)
                ^ (c[5][i + 5] & 0x0000_0000_00ff_0000)
                ^ (c[6][i + 6] & 0x0000_0000_0000_ff00)
                ^ (c[7][i + 7] & 0x0000_0000_0000_00ff);
        }

        BcWhirlpoolTables { c, rc }
    })
}

pub fn bc_whirlpool_mul_x(input: u32) -> u32 {
    (input << 1) ^ (0u32.wrapping_sub(input >> 7) & 0x011d)
}

#[allow(clippy::too_many_arguments)]
pub fn bc_whirlpool_pack(
    b7: u32,
    b6: u32,
    b5: u32,
    b4: u32,
    b3: u32,
    b2: u32,
    b1: u32,
    b0: u32,
) -> u64 {
    ((b7 as u64) << 56)
        ^ ((b6 as u64) << 48)
        ^ ((b5 as u64) << 40)
        ^ ((b4 as u64) << 32)
        ^ ((b3 as u64) << 24)
        ^ ((b2 as u64) << 16)
        ^ ((b1 as u64) << 8)
        ^ b0 as u64
}

pub fn bc_whirlpool_bad_state(message: &str) -> MethodCallFailed {
    RuntimeError::IllegalStateException {
        message: message.into(),
    }
    .into()
}

pub fn bc_whirlpool_array_field(
    ctx: &dyn NativeContext,
    digest: ObjectRef,
    field: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    match ctx.get_field_by_name(digest, field) {
        Value::Object(Some(o)) => Ok(o),
        _ => Err(bc_whirlpool_bad_state("WhirlpoolDigest: malformed state")),
    }
}

pub fn bc_whirlpool_read_long8(
    ctx: &dyn NativeContext,
    digest: ObjectRef,
    field: &str,
) -> Result<[u64; 8], MethodCallFailed> {
    let arr = bc_whirlpool_array_field(ctx, digest, field)?;
    if ctx.array_length(arr) < 8 {
        return Err(bc_whirlpool_bad_state("WhirlpoolDigest: short long array"));
    }
    let mut out = [0u64; 8];
    for (i, slot) in out.iter_mut().enumerate() {
        match ctx.get_array_element(arr, i) {
            Value::Long(v) => *slot = v as u64,
            _ => return Err(bc_whirlpool_bad_state("WhirlpoolDigest: bad long array")),
        }
    }
    Ok(out)
}

pub fn bc_whirlpool_write_long8(
    ctx: &mut dyn NativeContext,
    digest: ObjectRef,
    field: &str,
    values: &[u64; 8],
) -> Result<(), MethodCallFailed> {
    let arr = bc_whirlpool_array_field(ctx, digest, field)?;
    if ctx.array_length(arr) < 8 {
        return Err(bc_whirlpool_bad_state("WhirlpoolDigest: short long array"));
    }
    for (i, &value) in values.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Long(value as i64));
    }
    Ok(())
}

pub fn bc_whirlpool_read_buffer(
    ctx: &dyn NativeContext,
    digest: ObjectRef,
) -> Result<[u8; 64], MethodCallFailed> {
    let arr = bc_whirlpool_array_field(ctx, digest, "_buffer")?;
    if ctx.array_length(arr) < 64 {
        return Err(bc_whirlpool_bad_state("WhirlpoolDigest: short buffer"));
    }
    let mut out = [0u8; 64];
    if ctx.read_byte_array_into(arr, 0, &mut out) != 64 {
        return Err(bc_whirlpool_bad_state(
            "WhirlpoolDigest: failed to read buffer",
        ));
    }
    Ok(out)
}

pub fn bc_whirlpool_write_buffer(
    ctx: &mut dyn NativeContext,
    digest: ObjectRef,
    buffer: &[u8; 64],
) -> Result<(), MethodCallFailed> {
    let arr = bc_whirlpool_array_field(ctx, digest, "_buffer")?;
    if ctx.array_length(arr) < 64 || !ctx.write_byte_array_from(arr, 0, buffer) {
        return Err(bc_whirlpool_bad_state(
            "WhirlpoolDigest: failed to write buffer",
        ));
    }
    Ok(())
}

pub fn bc_whirlpool_read_bit_count(
    ctx: &dyn NativeContext,
    digest: ObjectRef,
) -> Result<[u8; 32], MethodCallFailed> {
    let arr = bc_whirlpool_array_field(ctx, digest, "_bitCount")?;
    if ctx.array_length(arr) < 32 {
        return Err(bc_whirlpool_bad_state("WhirlpoolDigest: short bit count"));
    }
    let mut out = [0u8; 32];
    for (i, slot) in out.iter_mut().enumerate() {
        match ctx.get_array_element(arr, i) {
            Value::Int(v) => *slot = v as u8,
            _ => return Err(bc_whirlpool_bad_state("WhirlpoolDigest: bad bit count")),
        }
    }
    Ok(out)
}

pub fn bc_whirlpool_write_bit_count(
    ctx: &mut dyn NativeContext,
    digest: ObjectRef,
    bit_count: &[u8; 32],
) -> Result<(), MethodCallFailed> {
    let arr = bc_whirlpool_array_field(ctx, digest, "_bitCount")?;
    if ctx.array_length(arr) < 32 {
        return Err(bc_whirlpool_bad_state("WhirlpoolDigest: short bit count"));
    }
    for (i, &value) in bit_count.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(value as i32));
    }
    Ok(())
}

pub fn bc_whirlpool_add_bits(bit_count: &mut [u8; 32], bytes: usize) {
    let mut carry = (bytes as u128) * 8;
    for slot in bit_count.iter_mut().rev() {
        let sum = (*slot as u128) + (carry & 0xff);
        *slot = sum as u8;
        carry = (carry >> 8) + (sum >> 8);
        if carry == 0 {
            break;
        }
    }
}

pub fn bc_whirlpool_pack_block(buffer: &[u8; 64]) -> [u64; 8] {
    let mut block = [0u64; 8];
    for (i, slot) in block.iter_mut().enumerate() {
        let off = i * 8;
        *slot = u64::from_be_bytes([
            buffer[off],
            buffer[off + 1],
            buffer[off + 2],
            buffer[off + 3],
            buffer[off + 4],
            buffer[off + 5],
            buffer[off + 6],
            buffer[off + 7],
        ]);
    }
    block
}

pub fn bc_whirlpool_process_block_inner(
    hash: &mut [u64; 8],
    k: &mut [u64; 8],
    l: &mut [u64; 8],
    block: &[u64; 8],
    state: &mut [u64; 8],
) {
    let tables = bc_whirlpool_tables();
    for i in 0..8 {
        k[i] = hash[i];
        state[i] = block[i] ^ k[i];
    }

    for round in 1..=10 {
        for i in 0..8 {
            l[i] = tables.c[0][((k[i] >> 56) & 0xff) as usize]
                ^ tables.c[1][((k[(i + 7) & 7] >> 48) & 0xff) as usize]
                ^ tables.c[2][((k[(i + 6) & 7] >> 40) & 0xff) as usize]
                ^ tables.c[3][((k[(i + 5) & 7] >> 32) & 0xff) as usize]
                ^ tables.c[4][((k[(i + 4) & 7] >> 24) & 0xff) as usize]
                ^ tables.c[5][((k[(i + 3) & 7] >> 16) & 0xff) as usize]
                ^ tables.c[6][((k[(i + 2) & 7] >> 8) & 0xff) as usize]
                ^ tables.c[7][(k[(i + 1) & 7] & 0xff) as usize];
        }
        *k = *l;
        k[0] ^= tables.rc[round];

        for i in 0..8 {
            l[i] = k[i]
                ^ tables.c[0][((state[i] >> 56) & 0xff) as usize]
                ^ tables.c[1][((state[(i + 7) & 7] >> 48) & 0xff) as usize]
                ^ tables.c[2][((state[(i + 6) & 7] >> 40) & 0xff) as usize]
                ^ tables.c[3][((state[(i + 5) & 7] >> 32) & 0xff) as usize]
                ^ tables.c[4][((state[(i + 4) & 7] >> 24) & 0xff) as usize]
                ^ tables.c[5][((state[(i + 3) & 7] >> 16) & 0xff) as usize]
                ^ tables.c[6][((state[(i + 2) & 7] >> 8) & 0xff) as usize]
                ^ tables.c[7][(state[(i + 1) & 7] & 0xff) as usize];
        }
        *state = *l;
    }

    for i in 0..8 {
        hash[i] ^= state[i] ^ block[i];
    }
}

pub fn bc_whirlpool_write_digest_state(
    ctx: &mut dyn NativeContext,
    digest: ObjectRef,
    hash: &[u64; 8],
    k: &[u64; 8],
    l: &[u64; 8],
    block: &[u64; 8],
    state: &[u64; 8],
) -> Result<(), MethodCallFailed> {
    bc_whirlpool_write_long8(ctx, digest, "_hash", hash)?;
    bc_whirlpool_write_long8(ctx, digest, "_K", k)?;
    bc_whirlpool_write_long8(ctx, digest, "_L", l)?;
    bc_whirlpool_write_long8(ctx, digest, "_block", block)?;
    bc_whirlpool_write_long8(ctx, digest, "_state", state)?;
    Ok(())
}

pub fn bc_whirlpool_native_process_block(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let mut hash = bc_whirlpool_read_long8(ctx, this, "_hash")?;
    let mut k = bc_whirlpool_read_long8(ctx, this, "_K")?;
    let mut l = bc_whirlpool_read_long8(ctx, this, "_L")?;
    let block = bc_whirlpool_read_long8(ctx, this, "_block")?;
    let mut state = bc_whirlpool_read_long8(ctx, this, "_state")?;
    bc_whirlpool_process_block_inner(&mut hash, &mut k, &mut l, &block, &mut state);
    bc_whirlpool_write_digest_state(ctx, this, &hash, &k, &l, &block, &state)?;
    Ok(None)
}

pub fn bc_whirlpool_native_update_array(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let input = obj_arg(args, 1)?;
    let in_off_i = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let len_i = match args.get(3) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    if len_i <= 0 {
        return Ok(None);
    }
    if in_off_i < 0 {
        return Err(RuntimeError::aioobe_index_only(in_off_i).into());
    }
    let in_off = in_off_i as usize;
    let len = len_i as usize;
    if in_off.saturating_add(len) > ctx.array_length(input) {
        return Err(RuntimeError::aioobe_index_only(
            in_off.saturating_add(len).min(i32::MAX as usize) as i32,
        )
        .into());
    }

    let mut data = vec![0u8; len];
    ctx.read_byte_array_into(input, in_off, &mut data);

    let mut buffer = bc_whirlpool_read_buffer(ctx, this)?;
    let mut buffer_pos = match ctx.get_field_by_name(this, "_bufferPos") {
        Value::Int(v) if (0..=64).contains(&v) => v as usize,
        _ => {
            return Err(bc_whirlpool_bad_state(
                "WhirlpoolDigest: bad buffer position",
            ))
        }
    };
    let mut bit_count = bc_whirlpool_read_bit_count(ctx, this)?;
    let mut hash = bc_whirlpool_read_long8(ctx, this, "_hash")?;
    let mut k = bc_whirlpool_read_long8(ctx, this, "_K")?;
    let mut l = bc_whirlpool_read_long8(ctx, this, "_L")?;
    let mut block = bc_whirlpool_read_long8(ctx, this, "_block")?;
    let mut state = bc_whirlpool_read_long8(ctx, this, "_state")?;

    let mut pos = 0usize;
    while pos < data.len() {
        let to_copy = (64 - buffer_pos).min(data.len() - pos);
        buffer[buffer_pos..buffer_pos + to_copy].copy_from_slice(&data[pos..pos + to_copy]);
        buffer_pos += to_copy;
        pos += to_copy;

        if buffer_pos == 64 {
            block = bc_whirlpool_pack_block(&buffer);
            bc_whirlpool_process_block_inner(&mut hash, &mut k, &mut l, &block, &mut state);
            buffer_pos = 0;
            buffer = [0u8; 64];
        }
    }

    bc_whirlpool_add_bits(&mut bit_count, len);
    bc_whirlpool_write_buffer(ctx, this, &buffer)?;
    ctx.set_field_by_name(this, "_bufferPos", Value::Int(buffer_pos as i32));
    bc_whirlpool_write_bit_count(ctx, this, &bit_count)?;
    bc_whirlpool_write_digest_state(ctx, this, &hash, &k, &l, &block, &state)?;
    Ok(None)
}

/// Native fast-path for BouncyCastle's table-based Whirlpool digest. Its
/// `update(byte[], int, int)` implementation feeds large inputs through a
/// byte-at-a-time interpreted loop under the BC JIT ban, so the million-'a'
/// vector is dominated by VM dispatch rather than the digest itself.
pub fn register_bc_whirlpool_digest(r: &mut NativeMethodRegistry) {
        
    const CLS: &str = "org/bouncycastle/crypto/digests/WhirlpoolDigest";
    bc_register_null_rerun!(
        r,
        CLS,
        "processBlock",
        "()V",
        bc_whirlpool_native_process_block,
    );
    bc_register_rerun!(
        r,
        "org/bouncycastle/crypto/digests/WhirlpoolDigest",
        "update",
        "([BII)V",
        bc_whirlpool_native_update_array,
    );

    }

pub fn bc_poly1305_bad_state(message: &str) -> MethodCallFailed {
    RuntimeError::IllegalStateException {
        message: message.into(),
    }
    .into()
}

pub fn bc_poly1305_i32(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    field: &str,
) -> Result<u32, MethodCallFailed> {
    match ctx.get_field_by_name(this, field) {
        Value::Int(v) => Ok(v as u32),
        _ => Err(bc_poly1305_bad_state("Poly1305: malformed integer state")),
    }
}

pub fn bc_poly1305_set_i32(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    field: &str,
    value: u32,
) {
    ctx.set_field_by_name(this, field, Value::Int(value as i32));
}

pub fn bc_poly1305_block(
    ctx: &dyn NativeContext,
    this: ObjectRef,
) -> Result<ObjectRef, MethodCallFailed> {
    match ctx.get_field_by_name(this, "currentBlock") {
        Value::Object(Some(o)) if ctx.array_length(o) >= 16 => Ok(o),
        _ => Err(bc_poly1305_bad_state("Poly1305: malformed current block")),
    }
}

pub fn bc_poly1305_read_block(
    ctx: &dyn NativeContext,
    this: ObjectRef,
) -> Result<[u8; 16], MethodCallFailed> {
    let arr = bc_poly1305_block(ctx, this)?;
    let mut out = [0u8; 16];
    if ctx.read_byte_array_into(arr, 0, &mut out) != 16 {
        return Err(bc_poly1305_bad_state("Poly1305: failed to read block"));
    }
    Ok(out)
}

pub fn bc_poly1305_write_block(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    block: &[u8; 16],
) -> Result<(), MethodCallFailed> {
    let arr = bc_poly1305_block(ctx, this)?;
    if !ctx.write_byte_array_from(arr, 0, block) {
        return Err(bc_poly1305_bad_state("Poly1305: failed to write block"));
    }
    Ok(())
}

pub fn bc_poly1305_load_state(
    ctx: &dyn NativeContext,
    this: ObjectRef,
) -> Result<([u32; 5], [u32; 5], [u32; 4]), MethodCallFailed> {
    let h = [
        bc_poly1305_i32(ctx, this, "h0")?,
        bc_poly1305_i32(ctx, this, "h1")?,
        bc_poly1305_i32(ctx, this, "h2")?,
        bc_poly1305_i32(ctx, this, "h3")?,
        bc_poly1305_i32(ctx, this, "h4")?,
    ];
    let r = [
        bc_poly1305_i32(ctx, this, "r0")?,
        bc_poly1305_i32(ctx, this, "r1")?,
        bc_poly1305_i32(ctx, this, "r2")?,
        bc_poly1305_i32(ctx, this, "r3")?,
        bc_poly1305_i32(ctx, this, "r4")?,
    ];
    let k = [
        bc_poly1305_i32(ctx, this, "k0")?,
        bc_poly1305_i32(ctx, this, "k1")?,
        bc_poly1305_i32(ctx, this, "k2")?,
        bc_poly1305_i32(ctx, this, "k3")?,
    ];
    Ok((h, [r[0], r[1], r[2], r[3], r[4]], [k[0], k[1], k[2], k[3]]))
}

pub fn bc_poly1305_store_h(ctx: &mut dyn NativeContext, this: ObjectRef, h: &[u32; 5]) {
    bc_poly1305_set_i32(ctx, this, "h0", h[0]);
    bc_poly1305_set_i32(ctx, this, "h1", h[1]);
    bc_poly1305_set_i32(ctx, this, "h2", h[2]);
    bc_poly1305_set_i32(ctx, this, "h3", h[3]);
    bc_poly1305_set_i32(ctx, this, "h4", h[4]);
}

pub fn bc_poly1305_le_u32(block: &[u8; 16], off: usize) -> u64 {
    u32::from_le_bytes([block[off], block[off + 1], block[off + 2], block[off + 3]]) as u64
}

pub fn bc_poly1305_process_block_inner(
    h: &mut [u32; 5],
    r: &[u32; 5],
    s: &[u32; 5],
    block: &[u8; 16],
    full: bool,
) {
    let t0 = bc_poly1305_le_u32(block, 0);
    let t1 = bc_poly1305_le_u32(block, 4);
    let t2 = bc_poly1305_le_u32(block, 8);
    let t3 = bc_poly1305_le_u32(block, 12);

    h[0] = h[0].wrapping_add((t0 & 0x03ff_ffff) as u32);
    h[1] = h[1].wrapping_add((((t1 << 32) | t0) >> 26 & 0x03ff_ffff) as u32);
    h[2] = h[2].wrapping_add((((t2 << 32) | t1) >> 20 & 0x03ff_ffff) as u32);
    h[3] = h[3].wrapping_add((((t3 << 32) | t2) >> 14 & 0x03ff_ffff) as u32);
    h[4] = h[4].wrapping_add((t3 >> 8) as u32);
    if full {
        h[4] = h[4].wrapping_add(1 << 24);
    }

    let m = |a: u32, b: u32| -> u64 { (a as u64) * (b as u64) };
    let tp0 = m(h[0], r[0]) + m(h[1], s[4]) + m(h[2], s[3]) + m(h[3], s[2]) + m(h[4], s[1]);
    let mut tp1 = m(h[0], r[1]) + m(h[1], r[0]) + m(h[2], s[4]) + m(h[3], s[3]) + m(h[4], s[2]);
    let mut tp2 = m(h[0], r[2]) + m(h[1], r[1]) + m(h[2], r[0]) + m(h[3], s[4]) + m(h[4], s[3]);
    let mut tp3 = m(h[0], r[3]) + m(h[1], r[2]) + m(h[2], r[1]) + m(h[3], r[0]) + m(h[4], s[4]);
    let mut tp4 = m(h[0], r[4]) + m(h[1], r[3]) + m(h[2], r[2]) + m(h[3], r[1]) + m(h[4], r[0]);

    h[0] = (tp0 as u32) & 0x03ff_ffff;
    tp1 += tp0 >> 26;
    h[1] = (tp1 as u32) & 0x03ff_ffff;
    tp2 += tp1 >> 26;
    h[2] = (tp2 as u32) & 0x03ff_ffff;
    tp3 += tp2 >> 26;
    h[3] = (tp3 as u32) & 0x03ff_ffff;
    tp4 += tp3 >> 26;
    h[4] = (tp4 as u32) & 0x03ff_ffff;
    h[0] = h[0].wrapping_add(((tp4 >> 26) as u32).wrapping_mul(5));
    h[1] = h[1].wrapping_add(h[0] >> 26);
    h[0] &= 0x03ff_ffff;
}

pub fn bc_poly1305_native_update(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let input = obj_arg(args, 1)?;
    let in_off_i = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let len_i = match args.get(3) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    if len_i <= 0 {
        return Ok(None);
    }
    if in_off_i < 0 {
        return Err(RuntimeError::aioobe_index_only(in_off_i).into());
    }
    let in_off = in_off_i as usize;
    let len = len_i as usize;
    if in_off.saturating_add(len) > ctx.array_length(input) {
        return Err(RuntimeError::aioobe_index_only(
            in_off.saturating_add(len).min(i32::MAX as usize) as i32,
        )
        .into());
    }

    let mut data = vec![0u8; len];
    ctx.read_byte_array_into(input, in_off, &mut data);
    let (mut h, r, k) = bc_poly1305_load_state(ctx, this)?;
    let s = [
        0,
        r[1].wrapping_mul(5),
        r[2].wrapping_mul(5),
        r[3].wrapping_mul(5),
        r[4].wrapping_mul(5),
    ];
    let mut block = bc_poly1305_read_block(ctx, this)?;
    let mut block_off = match ctx.get_field_by_name(this, "currentBlockOffset") {
        Value::Int(v) if (0..=16).contains(&v) => v as usize,
        _ => return Err(bc_poly1305_bad_state("Poly1305: bad block offset")),
    };

    let mut pos = 0usize;
    while pos < data.len() {
        if block_off == 16 {
            bc_poly1305_process_block_inner(&mut h, &r, &s, &block, true);
            block_off = 0;
        }
        let to_copy = (16 - block_off).min(data.len() - pos);
        block[block_off..block_off + to_copy].copy_from_slice(&data[pos..pos + to_copy]);
        block_off += to_copy;
        pos += to_copy;
        if block_off == 16 {
            bc_poly1305_process_block_inner(&mut h, &r, &s, &block, true);
            block_off = 0;
        }
    }

    let _ = k;
    bc_poly1305_store_h(ctx, this, &h);
    bc_poly1305_write_block(ctx, this, &block)?;
    ctx.set_field_by_name(this, "currentBlockOffset", Value::Int(block_off as i32));
    Ok(None)
}

pub fn bc_poly1305_native_do_final(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let out_arr = obj_arg(args, 1)?;
    let out_off_i = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    if out_off_i < 0 {
        return Err(RuntimeError::aioobe_index_only(out_off_i).into());
    }
    let out_off = out_off_i as usize;
    if out_off.saturating_add(16) > ctx.array_length(out_arr) {
        return Err(bc_gost_throw_crypto_exception(
            ctx,
            "org/bouncycastle/crypto/OutputLengthException",
            "Output buffer is too short.",
        ));
    }

    let (mut h, r, k) = bc_poly1305_load_state(ctx, this)?;
    let s = [
        0,
        r[1].wrapping_mul(5),
        r[2].wrapping_mul(5),
        r[3].wrapping_mul(5),
        r[4].wrapping_mul(5),
    ];
    let mut block = bc_poly1305_read_block(ctx, this)?;
    let block_off = match ctx.get_field_by_name(this, "currentBlockOffset") {
        Value::Int(v) if (0..=16).contains(&v) => v as usize,
        _ => return Err(bc_poly1305_bad_state("Poly1305: bad block offset")),
    };
    if block_off > 0 {
        if block_off < 16 {
            block[block_off] = 1;
            for b in &mut block[block_off + 1..] {
                *b = 0;
            }
        }
        bc_poly1305_process_block_inner(&mut h, &r, &s, &block, block_off == 16);
    }

    h[1] = h[1].wrapping_add(h[0] >> 26);
    h[0] &= 0x03ff_ffff;
    h[2] = h[2].wrapping_add(h[1] >> 26);
    h[1] &= 0x03ff_ffff;
    h[3] = h[3].wrapping_add(h[2] >> 26);
    h[2] &= 0x03ff_ffff;
    h[4] = h[4].wrapping_add(h[3] >> 26);
    h[3] &= 0x03ff_ffff;
    h[0] = h[0].wrapping_add((h[4] >> 26).wrapping_mul(5));
    h[4] &= 0x03ff_ffff;
    h[1] = h[1].wrapping_add(h[0] >> 26);
    h[0] &= 0x03ff_ffff;

    let mut g0 = h[0].wrapping_add(5);
    let mut b = g0 >> 26;
    g0 &= 0x03ff_ffff;
    let mut g1 = h[1].wrapping_add(b);
    b = g1 >> 26;
    g1 &= 0x03ff_ffff;
    let mut g2 = h[2].wrapping_add(b);
    b = g2 >> 26;
    g2 &= 0x03ff_ffff;
    let mut g3 = h[3].wrapping_add(b);
    b = g3 >> 26;
    g3 &= 0x03ff_ffff;
    let g4 = h[4].wrapping_add(b).wrapping_sub(1 << 26);

    let mask = (g4 >> 31).wrapping_sub(1);
    let nmask = !mask;
    h[0] = (h[0] & nmask) | (g0 & mask);
    h[1] = (h[1] & nmask) | (g1 & mask);
    h[2] = (h[2] & nmask) | (g2 & mask);
    h[3] = (h[3] & nmask) | (g3 & mask);
    h[4] = (h[4] & nmask) | (g4 & mask);

    let mut f0 = (((h[0] | (h[1] << 26)) as u64) & 0xffff_ffff) + k[0] as u64;
    let mut f1 = ((((h[1] >> 6) | (h[2] << 20)) as u64) & 0xffff_ffff) + k[1] as u64;
    let mut f2 = ((((h[2] >> 12) | (h[3] << 14)) as u64) & 0xffff_ffff) + k[2] as u64;
    let mut f3 = ((((h[3] >> 18) | (h[4] << 8)) as u64) & 0xffff_ffff) + k[3] as u64;

    let mut out = [0u8; 16];
    out[0..4].copy_from_slice(&(f0 as u32).to_le_bytes());
    f1 += f0 >> 32;
    out[4..8].copy_from_slice(&(f1 as u32).to_le_bytes());
    f2 += f1 >> 32;
    out[8..12].copy_from_slice(&(f2 as u32).to_le_bytes());
    f3 += f2 >> 32;
    out[12..16].copy_from_slice(&(f3 as u32).to_le_bytes());
    ctx.write_byte_array_from(out_arr, out_off, &out);

    for field in ["h0", "h1", "h2", "h3", "h4"] {
        bc_poly1305_set_i32(ctx, this, field, 0);
    }
    ctx.set_field_by_name(this, "currentBlockOffset", Value::Int(0));
    Ok(Some(Value::Int(16)))
}

/// Native Poly1305 accumulator/update/finalization. BC's key setup remains in
/// Java, but the block arithmetic and byte buffering are hot under the package
/// JIT ban and feed both standalone Poly1305 and ChaCha20-Poly1305 tests.
pub fn register_bc_poly1305(r: &mut NativeMethodRegistry) {
            bc_register_rerun!(
        r,
        "org/bouncycastle/crypto/macs/Poly1305",
        "update",
        "([BII)V",
        bc_poly1305_native_update,
    );
    bc_register_rerun!(
        r,
        "org/bouncycastle/crypto/macs/Poly1305",
        "doFinal",
        "([BI)I",
        bc_poly1305_native_do_final,
    );
    }

pub fn bc_digest_random_bad_state(message: &str) -> MethodCallFailed {
    RuntimeError::IllegalStateException {
        message: message.into(),
    }
    .into()
}

pub fn bc_digest_random_aioobe(index: i32) -> MethodCallFailed {
    RuntimeError::aioobe_index_only(index).into()
}

pub fn bc_digest_random_range(
    ctx: &dyn NativeContext,
    bytes: ObjectRef,
    start: i32,
    len: i32,
) -> Result<(usize, usize), MethodCallFailed> {
    if start < 0 {
        return Err(bc_digest_random_aioobe(start));
    }
    if len < 0 {
        return Err(bc_digest_random_aioobe(len));
    }

    let start = start as usize;
    let len = len as usize;
    let arr_len = ctx.array_length(bytes);
    let Some(end) = start.checked_add(len) else {
        return Err(bc_digest_random_aioobe(i32::MAX));
    };
    if end > arr_len {
        return Err(bc_digest_random_aioobe(end.min(i32::MAX as usize) as i32));
    }
    Ok((start, len))
}

pub fn bc_digest_random_object_field(
    ctx: &dyn NativeContext,
    obj: ObjectRef,
    field: &str,
) -> Result<ObjectRef, MethodCallFailed> {
    match ctx.get_field_by_name(obj, field) {
        Value::Object(Some(o)) => Ok(o),
        _ => Err(bc_digest_random_bad_state(
            "DigestRandomGenerator: malformed object field",
        )),
    }
}

pub fn bc_digest_random_long_field(
    ctx: &dyn NativeContext,
    obj: ObjectRef,
    field: &str,
) -> Result<i64, MethodCallFailed> {
    match ctx.get_field_by_name(obj, field) {
        Value::Long(v) => Ok(v),
        _ => Err(bc_digest_random_bad_state(
            "DigestRandomGenerator: malformed counter field",
        )),
    }
}

pub fn bc_i32_field(ctx: &dyn NativeContext, obj: ObjectRef, field: &str) -> Option<i32> {
    match ctx.get_field_by_name(obj, field) {
        Value::Int(v) => Some(v),
        _ => None,
    }
}

pub fn bc_i32_field_is(
    ctx: &dyn NativeContext,
    obj: ObjectRef,
    field: &str,
    expected: u32,
) -> bool {
    bc_i32_field(ctx, obj, field) == Some(expected as i32)
}

pub fn bc_digest_general_is_reset(ctx: &dyn NativeContext, digest: ObjectRef) -> bool {
    bc_i32_field(ctx, digest, "xBufOff") == Some(0)
        && matches!(ctx.get_field_by_name(digest, "byteCount"), Value::Long(0))
}

pub fn bc_digest_fast_kind_if_reset(
    ctx: &dyn NativeContext,
    digest: ObjectRef,
) -> Option<BcDigestKind> {
    let class = ctx.class_name_of_id(ctx.class_id_of_object(digest))?;
    if !bc_digest_general_is_reset(ctx, digest) {
        return None;
    }

    match class.as_str() {
        "org/bouncycastle/crypto/digests/SHA1Digest"
            if bc_i32_field(ctx, digest, "xOff") == Some(0)
                && bc_i32_field_is(ctx, digest, "H1", 0x6745_2301)
                && bc_i32_field_is(ctx, digest, "H2", 0xefcd_ab89)
                && bc_i32_field_is(ctx, digest, "H3", 0x98ba_dcfe)
                && bc_i32_field_is(ctx, digest, "H4", 0x1032_5476)
                && bc_i32_field_is(ctx, digest, "H5", 0xc3d2_e1f0) =>
        {
            Some(BcDigestKind::Sha1)
        }
        "org/bouncycastle/crypto/digests/SHA256Digest"
            if bc_i32_field(ctx, digest, "xOff") == Some(0)
                && bc_i32_field_is(ctx, digest, "H1", 0x6a09_e667)
                && bc_i32_field_is(ctx, digest, "H2", 0xbb67_ae85)
                && bc_i32_field_is(ctx, digest, "H3", 0x3c6e_f372)
                && bc_i32_field_is(ctx, digest, "H4", 0xa54f_f53a)
                && bc_i32_field_is(ctx, digest, "H5", 0x510e_527f)
                && bc_i32_field_is(ctx, digest, "H6", 0x9b05_688c)
                && bc_i32_field_is(ctx, digest, "H7", 0x1f83_d9ab)
                && bc_i32_field_is(ctx, digest, "H8", 0x5be0_cd19) =>
        {
            Some(BcDigestKind::Sha256)
        }
        _ => None,
    }
}

pub fn bc_digest_random_read_bytes(ctx: &dyn NativeContext, arr: ObjectRef) -> Vec<u8> {
    let len = ctx.array_length(arr);
    let mut out = vec![0u8; len];
    let n = ctx.read_byte_array_into(arr, 0, &mut out);
    out.truncate(n);
    out
}

pub fn bc_digest_random_counter_bytes(counter: i64) -> [u8; 8] {
    (counter as u64).to_le_bytes()
}

pub fn bc_digest_random_generate_state_fast(
    kind: BcDigestKind,
    state: &mut Vec<u8>,
    seed: &mut Vec<u8>,
    state_counter: &mut i64,
    seed_counter: &mut i64,
) {
    let counter = *state_counter;
    *state_counter = state_counter.wrapping_add(1);

    let mut msg = Vec::with_capacity(8 + state.len() + seed.len());
    msg.extend_from_slice(&bc_digest_random_counter_bytes(counter));
    msg.extend_from_slice(state);
    msg.extend_from_slice(seed);
    *state = kind.digest(&msg);

    if *state_counter % 10 == 0 {
        let counter = *seed_counter;
        *seed_counter = seed_counter.wrapping_add(1);
        let mut msg = Vec::with_capacity(seed.len() + 8);
        msg.extend_from_slice(seed);
        msg.extend_from_slice(&bc_digest_random_counter_bytes(counter));
        *seed = kind.digest(&msg);
    }
}

pub fn bc_digest_random_next_fast(
    ctx: &mut dyn NativeContext,
    kind: BcDigestKind,
    this: ObjectRef,
    bytes: ObjectRef,
    start: usize,
    len: usize,
) -> MethodCallResult {
    let state_arr = bc_digest_random_object_field(ctx, this, "state")?;
    let seed_arr = bc_digest_random_object_field(ctx, this, "seed")?;
    if ctx.array_length(state_arr) != kind.len() || ctx.array_length(seed_arr) != kind.len() {
        return Err(bc_digest_random_bad_state(
            "DigestRandomGenerator: digest/state length mismatch",
        ));
    }

    let mut state = bc_digest_random_read_bytes(ctx, state_arr);
    let mut seed = bc_digest_random_read_bytes(ctx, seed_arr);
    if state.len() != kind.len() || seed.len() != kind.len() {
        return Err(bc_digest_random_bad_state(
            "DigestRandomGenerator: failed to read state arrays",
        ));
    }

    let mut state_counter = bc_digest_random_long_field(ctx, this, "stateCounter")?;
    let mut seed_counter = bc_digest_random_long_field(ctx, this, "seedCounter")?;
    let mut out = vec![0u8; len];
    let mut state_off = 0usize;

    bc_digest_random_generate_state_fast(
        kind,
        &mut state,
        &mut seed,
        &mut state_counter,
        &mut seed_counter,
    );

    for b in &mut out {
        if state_off == state.len() {
            bc_digest_random_generate_state_fast(
                kind,
                &mut state,
                &mut seed,
                &mut state_counter,
                &mut seed_counter,
            );
            state_off = 0;
        }
        *b = state[state_off];
        state_off += 1;
    }

    if !ctx.write_byte_array_from(bytes, start, &out)
        || !ctx.write_byte_array_from(state_arr, 0, &state)
        || !ctx.write_byte_array_from(seed_arr, 0, &seed)
    {
        return Err(bc_digest_random_bad_state(
            "DigestRandomGenerator: failed to write state arrays",
        ));
    }
    ctx.set_field_by_name(this, "stateCounter", Value::Long(state_counter));
    ctx.set_field_by_name(this, "seedCounter", Value::Long(seed_counter));
    Ok(None)
}

pub fn bc_digest_update_counter_virtual(
    ctx: &mut dyn NativeContext,
    digest: ObjectRef,
    counter: i64,
) -> MethodCallResult {
    let mut value = counter as u64;
    // GC-safety: `update` is real BouncyCastle bytecode -- it allocates, and a
    // moving young collection relocates `digest`. `digest` is a bare Rust local
    // carried into all EIGHT turns of this loop, so from turn two on the
    // dispatch is on a pre-GC address. Pin once, re-read at the top of the
    // body (the shadow leaves the outer binding alone).
    let digest_pin = ctx.pin_native_root(digest);
    for _ in 0..8 {
        let digest = ctx.read_native_pin(digest_pin, digest);
        ctx.invoke_virtual(
            digest,
            "update",
            "(B)V",
            &[Value::Int(value as u8 as i8 as i32)],
        )?;
        value >>= 8;
    }
    Ok(None)
}

pub fn bc_digest_update_array_virtual(
    ctx: &mut dyn NativeContext,
    digest: ObjectRef,
    arr: ObjectRef,
) -> MethodCallResult {
    let len = ctx.array_length(arr);
    ctx.invoke_virtual(
        digest,
        "update",
        "([BII)V",
        &[
            Value::Object(Some(arr)),
            Value::Int(0),
            Value::Int(len.min(i32::MAX as usize) as i32),
        ],
    )?;
    Ok(None)
}

pub fn bc_digest_do_final_virtual(
    ctx: &mut dyn NativeContext,
    digest: ObjectRef,
    arr: ObjectRef,
) -> MethodCallResult {
    ctx.invoke_virtual(
        digest,
        "doFinal",
        "([BI)I",
        &[Value::Object(Some(arr)), Value::Int(0)],
    )?;
    Ok(None)
}

pub fn bc_digest_random_cycle_seed_virtual(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    digest: ObjectRef,
    seed_arr: ObjectRef,
    seed_counter: &mut i64,
) -> MethodCallResult {
    bc_digest_update_array_virtual(ctx, digest, seed_arr)?;
    let counter = *seed_counter;
    *seed_counter = seed_counter.wrapping_add(1);
    ctx.set_field_by_name(this, "seedCounter", Value::Long(*seed_counter));
    bc_digest_update_counter_virtual(ctx, digest, counter)?;
    bc_digest_do_final_virtual(ctx, digest, seed_arr)
}

pub fn bc_digest_random_generate_state_virtual(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    digest: ObjectRef,
    state_arr: ObjectRef,
    seed_arr: ObjectRef,
    state_counter: &mut i64,
    seed_counter: &mut i64,
) -> MethodCallResult {
    let counter = *state_counter;
    *state_counter = state_counter.wrapping_add(1);
    ctx.set_field_by_name(this, "stateCounter", Value::Long(*state_counter));
    bc_digest_update_counter_virtual(ctx, digest, counter)?;
    bc_digest_update_array_virtual(ctx, digest, state_arr)?;
    bc_digest_update_array_virtual(ctx, digest, seed_arr)?;
    bc_digest_do_final_virtual(ctx, digest, state_arr)?;

    if *state_counter % 10 == 0 {
        bc_digest_random_cycle_seed_virtual(ctx, this, digest, seed_arr, seed_counter)?;
    }
    Ok(None)
}

pub fn bc_digest_random_next_virtual(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    digest: ObjectRef,
    bytes: ObjectRef,
    start: usize,
    len: usize,
) -> MethodCallResult {
    let state_arr = bc_digest_random_object_field(ctx, this, "state")?;
    let seed_arr = bc_digest_random_object_field(ctx, this, "seed")?;
    let state_len = ctx.array_length(state_arr);
    if state_len == 0 {
        return Err(bc_digest_random_bad_state(
            "DigestRandomGenerator: empty state",
        ));
    }

    let mut state_counter = bc_digest_random_long_field(ctx, this, "stateCounter")?;
    let mut seed_counter = bc_digest_random_long_field(ctx, this, "seedCounter")?;
    let mut state_off = 0usize;

    bc_digest_random_generate_state_virtual(
        ctx,
        this,
        digest,
        state_arr,
        seed_arr,
        &mut state_counter,
        &mut seed_counter,
    )?;

    // GC-safety: `bc_digest_random_generate_state_virtual` dispatches
    // `Digest.update`/`doFinal` -- real bytecode that allocates -- and it runs
    // INSIDE this loop, once per state refill. Every reference the body carries
    // in from outside (`this`, `digest`, and both byte arrays, one of which is
    // read and one written on EVERY turn) is a bare Rust local nothing
    // rewrites. Pin all four and re-read at the top of the body.
    let this_pin = ctx.pin_native_root(this);
    let digest_pin = ctx.pin_native_root(digest);
    let state_pin = ctx.pin_native_root(state_arr);
    let seed_pin = ctx.pin_native_root(seed_arr);
    let bytes_pin = ctx.pin_native_root(bytes);
    for i in start..start + len {
        let this = ctx.read_native_pin(this_pin, this);
        let digest = ctx.read_native_pin(digest_pin, digest);
        let state_arr = ctx.read_native_pin(state_pin, state_arr);
        let seed_arr = ctx.read_native_pin(seed_pin, seed_arr);
        let bytes = ctx.read_native_pin(bytes_pin, bytes);
        if state_off == state_len {
            bc_digest_random_generate_state_virtual(
                ctx,
                this,
                digest,
                state_arr,
                seed_arr,
                &mut state_counter,
                &mut seed_counter,
            )?;
            state_off = 0;
        }
        // Re-read once more: the refill above ran bytecode.
        let state_arr = ctx.read_native_pin(state_pin, state_arr);
        let bytes = ctx.read_native_pin(bytes_pin, bytes);
        let v = ctx.get_array_element(state_arr, state_off);
        ctx.set_array_element(bytes, i, v);
        state_off += 1;
    }
    ctx.unpin_native_roots(this_pin);
    Ok(None)
}

pub fn bc_digest_random_next_locked(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    bytes: ObjectRef,
    start: usize,
    len: usize,
) -> MethodCallResult {
    let digest = bc_digest_random_object_field(ctx, this, "digest")?;
    if let Some(kind) = bc_digest_fast_kind_if_reset(ctx, digest) {
        return bc_digest_random_next_fast(ctx, kind, this, bytes, start, len);
    }
    bc_digest_random_next_virtual(ctx, this, digest, bytes, start, len)
}

/// Native fast-path for BouncyCastle's synchronized
/// `DigestRandomGenerator.nextBytes`. The generator is hot in
/// `DigestRandomNumberTest`: two count tests call `nextBytes(byte[])` one
/// million times each. The x64 JIT intentionally bails on non-elided
/// monitorenter/monitorexit bytecode, so the synchronized three-arg body stayed
/// interpreted even when `CRATONVM_JIT_ALLOW_PACKAGES=org/bouncycastle/` was
/// used to investigate BC JIT correctness. This intrinsic preserves the object
/// monitor via the VM's monitor table and mirrors the BC state transitions.
pub fn register_bc_digest_random_generator(r: &mut NativeMethodRegistry) {
        
    // Interpreter round i5 wave 106, lane L7 (`i100-L7-native-shadow-npe-
    // texts`): a null array was `obj_arg`'s own "null object argument" NPE;
    // it now re-runs BC's `nextBytes(bytes, 0, bytes.length)`, whose
    // `arraylength` raises the JEP 358 NPE. The refusal comes before the
    // monitor and before any write.
    bc_register_rerun!(r, "org/bouncycastle/crypto/prng/DigestRandomGenerator", "nextBytes", "([B)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let bytes = obj_arg(args, 1)?;
        let len = ctx.array_length(bytes).min(i32::MAX as usize) as i32;
        ctx.monitor_enter(this);
        let result = bc_digest_random_next_locked(ctx, this, bytes, 0, len as usize);
        ctx.monitor_exit(this);
        result
    });

    bc_register_rerun!(r, "org/bouncycastle/crypto/prng/DigestRandomGenerator", "nextBytes", "([BII)V", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let bytes = obj_arg(args, 1)?;
        let start = match args.get(2) {
            Some(Value::Int(v)) => *v,
            _ => 0,
        };
        let len = match args.get(3) {
            Some(Value::Int(v)) => *v,
            _ => 0,
        };
        let (start, len) = bc_digest_random_range(ctx, bytes, start, len)?;
        ctx.monitor_enter(this);
        let result = bc_digest_random_next_locked(ctx, this, bytes, start, len);
        ctx.monitor_exit(this);
        result
    });

    }

pub fn register_bc_primes_small_factors(r: &mut NativeMethodRegistry) {
        
    // BC's RSA-keygen primality pre-screen (`Primes.isProbablePrime` path).
    bc_register_null_rerun!(
        r,
        "org/bouncycastle/math/Primes",
        "implHasAnySmallFactors",
        "(Ljava/math/BigInteger;)Z",
        |ctx, args| {
            let x = bi_read_int(ctx, obj_arg(args, 0)?);
            let has = bc_has_any_small_factors(x.mag_le(), x.is_neg());
            Ok(Some(Value::Int(i32::from(has))))
        },
    );

    bc_register_null_rerun!(
        r,
        "org/bouncycastle/math/Primes",
        "isMRProbablePrime",
        "(Ljava/math/BigInteger;Ljava/security/SecureRandom;I)Z",
        |ctx, args| {
            let candidate = bc_primes_candidate_arg(ctx, args, 0, "candidate")?;
            if !matches!(args.get(1), Some(Value::Object(Some(_)))) {
                return Err(RuntimeError::IllegalArgumentException {
                    message: "'random' cannot be null".into(),
                }
                .into());
            }
            let iterations = match args.get(2) {
                Some(Value::Int(v)) => *v,
                _ => 0,
            };
            if iterations < 1 {
                return Err(RuntimeError::IllegalArgumentException {
                    message: "'iterations' must be > 0".into(),
                }
                .into());
            }
            Ok(Some(Value::Int(i32::from(candidate.is_probable_prime()))))
        },
    );

    bc_register_null_rerun!(
        r,
        "org/bouncycastle/math/Primes",
        "isMRProbablePrimeToBase",
        "(Ljava/math/BigInteger;Ljava/math/BigInteger;)Z",
        |ctx, args| {
            let candidate = bc_primes_candidate_arg(ctx, args, 0, "candidate")?;
            let base = bc_primes_candidate_arg(ctx, args, 1, "base")?;
            let max_base = candidate.sub(&bc_bigint_small(1));
            if base.cmp(&max_base) != std::cmp::Ordering::Less {
                return Err(RuntimeError::IllegalArgumentException {
                    message: "'base' must be < ('candidate' - 1)".into(),
                }
                .into());
            }
            Ok(Some(Value::Int(i32::from(bc_mr_probable_prime_to_base(
                &candidate, &base,
            )))))
        },
    );

    // BC's `BigIntegers.createRandomPrime` initial-candidate sieve, which
    // otherwise runs the interpreted safegcd `Mod.modOddIsCoprimeVar /
    // updateFG30` against SMALL_PRIMES_PRODUCT — the dominant cost once
    // implHasAnySmallFactors above is native. Equivalent single-word-mod sieve.
    bc_register_null_rerun!(
        r,
        "org/bouncycastle/util/BigIntegers",
        "hasAnySmallFactors",
        "(Ljava/math/BigInteger;)Z",
        |ctx, args| {
            let x = bi_read_int(ctx, obj_arg(args, 0)?);
            let has = bc_util_has_any_small_factors(x.mag_le());
            Ok(Some(Value::Int(i32::from(has))))
        },
    );
    // Native bridge for BC's safegcd inverse wrappers. The raw
    // org/bouncycastle/math/raw/Mod path is part of the documented
    // cross-package JIT hazard; use the VM's already-tested BigInteger inverse
    // core instead of entering that interpreted/JIT-sensitive loop.
    bc_register_null_rerun!(
        r,
        "org/bouncycastle/util/BigIntegers",
        "modOddInverse",
        "(Ljava/math/BigInteger;Ljava/math/BigInteger;)Ljava/math/BigInteger;",
        |ctx, args| bc_big_integers_mod_odd_inverse(ctx, args, false),
    );
    bc_register_null_rerun!(
        r,
        "org/bouncycastle/util/BigIntegers",
        "modOddInverseVar",
        "(Ljava/math/BigInteger;Ljava/math/BigInteger;)Ljava/math/BigInteger;",
        |ctx, args| bc_big_integers_mod_odd_inverse(ctx, args, true),
    );

}

pub struct AesKeySchedule {
    pub rounds: [[u32; 4]; 15],
    pub len: usize,
}

impl AesKeySchedule {
    #[inline]
    pub const fn empty() -> Self {
        Self {
            rounds: [[0u32; 4]; 15],
            len: 0,
        }
    }
}

impl std::ops::Deref for AesKeySchedule {
    type Target = [[u32; 4]];
    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.rounds[..self.len]
    }
}

/// Read BouncyCastle's expanded AES key schedule (`int[][] KW`) into
/// a stack-allocated `AesKeySchedule` — `KW[round][col]`.
pub fn read_aes_kw(ctx: &dyn NativeContext, outer: ObjectRef) -> AesKeySchedule {
    let rows = ctx.array_length(outer);
    if rows == 0 || rows > 15 {
        return AesKeySchedule::empty();
    }
    let mut kw = AesKeySchedule::empty();
    for r in 0..rows {
        let row = match ctx.get_array_element(outer, r) {
            Value::Object(Some(o)) => o,
            _ => return AesKeySchedule::empty(),
        };
        let mut buf = [0i32; 4];
        let n = ctx.read_int_array_into(row, 0, &mut buf);
        if n < 4 {
            for c in n..4 {
                buf[c] = match ctx.get_array_element(row, c) {
                    Value::Int(v) => v,
                    _ => 0,
                };
            }
        }
        kw.rounds[r] = [buf[0] as u32, buf[1] as u32, buf[2] as u32, buf[3] as u32];
    }
    kw.len = rows;
    kw
}

pub fn bc_aes_bad_key() -> MethodCallFailed {
    RuntimeError::IllegalArgumentException {
        message: "Key length not 128/192/256 bits.".into(),
    }
    .into()
}

pub fn bc_aes_alloc_working_key(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    key_arr: ObjectRef,
    for_enc: bool,
) -> Result<ObjectRef, MethodCallFailed> {
    let klen = ctx.array_length(key_arr);
    let mut kbuf = [0u8; 32];
    let key: &[u8] = if klen <= 32 {
        let n = ctx.read_byte_array_into(key_arr, 0, &mut kbuf[..klen]);
        &kbuf[..n]
    } else {
        &[]
    };

    let w = crate::bc_aes::generate_working_key(key, for_enc).ok_or_else(bc_aes_bad_key)?;

    ctx.set_field_by_name(this, "ROUNDS", Value::Int((w.len() - 1) as i32));

    Ok(bc_aes_schedule_array(ctx, &w))
}

/// The AES round-key schedule as BC's `int[][]` (one `int[4]` row per round).
///
/// gc-common w20-e: both builders (`bc_aes_alloc_working_key` and the
/// `AESEngine.generateWorkingKey` native) stored every row through the outer
/// array's address from before that row's allocation -- a GC point; the old
/// note said a collection "never fires mid-native-call", which is not so. The
/// outer array is rooted here and re-read before every store.
pub fn bc_aes_schedule_array(ctx: &mut dyn NativeContext, w: &[[u32; 4]]) -> ObjectRef {
    let mut scope = cratonvm_native_api::NativeHandleScope::new(ctx);
    let outer = scope.new_array(cratonvm_types::ArrayElementType::Reference, w.len());
    let outer_h = scope.root(outer);
    for (r_idx, cols) in w.iter().enumerate() {
        let row = scope.new_array(cratonvm_types::ArrayElementType::Int, 4);
        for (c, &word) in cols.iter().enumerate() {
            scope.set_array_element(row, c, Value::Int(word as i32));
        }
        let outer: ObjectRef = scope.get(&outer_h);
        scope.set_array_element(outer, r_idx, Value::Object(Some(row)));
    }
    scope.get(&outer_h)
}

pub fn bc_aes_clone_static_sbox(
    ctx: &mut dyn NativeContext,
    aes_class: &str,
    field_name: &str,
) -> Option<ObjectRef> {
    let class_id = ctx.class_id_by_name(aes_class)?;
    let field_index = ctx.static_field_index_by_name(class_id, field_name)?;
    let Value::Object(Some(src)) = ctx.get_static_field(class_id, field_index) else {
        return None;
    };
    // gc-common w20-e: the table is copied out BEFORE the clone's
    // allocation (a GC point); it used to be read through its address from
    // before it.
    let len = ctx.array_length(src);
    let mut bytes = vec![0u8; len];
    ctx.read_byte_array_into(src, 0, &mut bytes);
    let dst = ctx.new_array(cratonvm_types::ArrayElementType::Byte, len);
    ctx.write_byte_array_from(dst, 0, &bytes);
    Some(dst)
}

pub const BC_GOST_PI: [u8; 256] = [
    252, 238, 221, 17, 207, 110, 49, 22, 251, 196, 250, 218, 35, 197, 4, 77, 233, 119, 240, 219,
    147, 46, 153, 186, 23, 54, 241, 187, 20, 205, 95, 193, 249, 24, 101, 90, 226, 92, 239, 33, 129,
    28, 60, 66, 139, 1, 142, 79, 5, 132, 2, 174, 227, 106, 143, 160, 6, 11, 237, 152, 127, 212,
    211, 31, 235, 52, 44, 81, 234, 200, 72, 171, 242, 42, 104, 162, 253, 58, 206, 204, 181, 112,
    14, 86, 8, 12, 118, 18, 191, 114, 19, 71, 156, 183, 93, 135, 21, 161, 150, 41, 16, 123, 154,
    199, 243, 145, 120, 111, 157, 158, 178, 177, 50, 117, 25, 61, 255, 53, 138, 126, 109, 84, 198,
    128, 195, 189, 13, 87, 223, 245, 36, 169, 62, 168, 67, 201, 215, 121, 214, 246, 124, 34, 185,
    3, 224, 15, 236, 222, 122, 148, 176, 188, 220, 232, 40, 80, 78, 51, 10, 74, 167, 151, 96, 115,
    30, 0, 98, 68, 26, 184, 56, 130, 100, 159, 38, 65, 173, 69, 70, 146, 39, 94, 85, 47, 140, 163,
    165, 125, 105, 213, 149, 59, 7, 88, 179, 64, 134, 172, 29, 247, 48, 55, 107, 228, 136, 217,
    231, 137, 225, 27, 131, 73, 76, 63, 248, 254, 141, 83, 170, 144, 202, 216, 133, 97, 32, 113,
    103, 164, 45, 43, 9, 91, 203, 155, 37, 208, 190, 229, 108, 82, 89, 166, 116, 210, 230, 244,
    180, 192, 209, 102, 175, 194, 57, 75, 99, 182,
];

pub const BC_GOST_INV_PI: [u8; 256] = [
    165, 45, 50, 143, 14, 48, 56, 192, 84, 230, 158, 57, 85, 126, 82, 145, 100, 3, 87, 90, 28, 96,
    7, 24, 33, 114, 168, 209, 41, 198, 164, 63, 224, 39, 141, 12, 130, 234, 174, 180, 154, 99, 73,
    229, 66, 228, 21, 183, 200, 6, 112, 157, 65, 117, 25, 201, 170, 252, 77, 191, 42, 115, 132,
    213, 195, 175, 43, 134, 167, 177, 178, 91, 70, 211, 159, 253, 212, 15, 156, 47, 155, 67, 239,
    217, 121, 182, 83, 127, 193, 240, 35, 231, 37, 94, 181, 30, 162, 223, 166, 254, 172, 34, 249,
    226, 74, 188, 53, 202, 238, 120, 5, 107, 81, 225, 89, 163, 242, 113, 86, 17, 106, 137, 148,
    101, 140, 187, 119, 60, 123, 40, 171, 210, 49, 222, 196, 95, 204, 207, 118, 44, 184, 216, 46,
    54, 219, 105, 179, 20, 149, 190, 98, 161, 59, 22, 102, 233, 92, 108, 109, 173, 55, 97, 75, 185,
    227, 186, 241, 160, 133, 131, 218, 71, 197, 176, 51, 250, 150, 111, 110, 194, 246, 80, 255, 93,
    169, 142, 23, 27, 151, 125, 236, 88, 247, 31, 251, 124, 9, 13, 122, 103, 69, 135, 220, 232, 79,
    29, 78, 4, 235, 248, 243, 62, 61, 189, 138, 136, 221, 205, 11, 19, 152, 2, 147, 128, 144, 208,
    36, 52, 203, 237, 244, 206, 153, 16, 68, 64, 146, 58, 1, 38, 18, 26, 72, 104, 245, 129, 139,
    199, 214, 32, 10, 8, 0, 76, 215, 116,
];

pub const BC_GOST_L_FACTORS: [u8; 16] = [
    148, 32, 133, 16, 194, 192, 1, 251, 1, 192, 194, 16, 133, 32, 148, 1,
];

pub fn bc_gost_mul_slow(mut a: u8, mut b: u8) -> u8 {
    let mut p = 0u8;
    for _ in 0..8 {
        if a == 0 || b == 0 {
            break;
        }
        if (b & 1) != 0 {
            p ^= a;
        }
        let hi = a & 0x80;
        a = a.wrapping_shl(1);
        if hi != 0 {
            a ^= 0xc3;
        }
        b = ((b as i8) >> 1) as u8;
    }
    p
}

pub fn bc_gost_lfactor_table() -> &'static [[u8; 256]; 16] {
    static TABLE: std::sync::OnceLock<[[u8; 256]; 16]> = std::sync::OnceLock::new();
    TABLE.get_or_init(|| {
        let mut table = [[0u8; 256]; 16];
        for (i, &factor) in BC_GOST_L_FACTORS.iter().enumerate() {
            for x in 0..256 {
                table[i][x] = bc_gost_mul_slow(x as u8, factor);
            }
        }
        table
    })
}

pub fn bc_gost_l(data: &[u8; 16]) -> u8 {
    let table = bc_gost_lfactor_table();
    let mut x = data[15];
    for i in (0..15).rev() {
        x ^= table[i][data[i] as usize];
    }
    x
}

pub fn bc_gost_r(data: &mut [u8; 16]) {
    let z = bc_gost_l(data);
    data.copy_within(0..15, 1);
    data[0] = z;
}

pub fn bc_gost_inverse_r(data: &mut [u8; 16]) {
    let mut temp = [0u8; 16];
    temp[..15].copy_from_slice(&data[1..]);
    temp[15] = data[0];
    let z = bc_gost_l(&temp);
    data.copy_within(1..16, 0);
    data[15] = z;
}

pub fn bc_gost_l_transform(data: &mut [u8; 16]) {
    for _ in 0..16 {
        bc_gost_r(data);
    }
}

pub fn bc_gost_inverse_l_transform(data: &mut [u8; 16]) {
    for _ in 0..16 {
        bc_gost_inverse_r(data);
    }
}

pub fn bc_gost_x(data: &mut [u8; 16], rhs: &[u8; 16]) {
    for i in 0..16 {
        data[i] ^= rhs[i];
    }
}

pub fn bc_gost_lsx(k: &[u8; 16], a: &[u8; 16]) -> [u8; 16] {
    let mut result = *k;
    bc_gost_x(&mut result, a);
    for b in &mut result {
        *b = BC_GOST_PI[*b as usize];
    }
    bc_gost_l_transform(&mut result);
    result
}

pub fn bc_gost_xsl(k: &[u8; 16], a: &[u8; 16]) -> [u8; 16] {
    let mut result = *k;
    bc_gost_x(&mut result, a);
    bc_gost_inverse_l_transform(&mut result);
    for b in &mut result {
        *b = BC_GOST_INV_PI[*b as usize];
    }
    result
}

pub fn bc_gost_read_subkeys(
    ctx: &dyn NativeContext,
    subkeys_outer: ObjectRef,
) -> Option<[[u8; 16]; 10]> {
    if ctx.array_length(subkeys_outer) != 10 {
        return None;
    }
    let mut subkeys = [[0u8; 16]; 10];
    for (i, slot) in subkeys.iter_mut().enumerate() {
        let row = match ctx.get_array_element(subkeys_outer, i) {
            Value::Object(Some(o)) => o,
            _ => return None,
        };
        if ctx.array_length(row) != 16 || ctx.read_byte_array_into(row, 0, slot) != 16 {
            return None;
        }
    }
    Some(subkeys)
}

pub fn bc_gost_process_block(
    subkeys: &[[u8; 16]; 10],
    for_encryption: bool,
    input: &[u8; 16],
) -> [u8; 16] {
    let mut block = *input;
    if for_encryption {
        for key in subkeys.iter().take(9) {
            block = bc_gost_lsx(key, &block);
        }
        bc_gost_x(&mut block, &subkeys[9]);
    } else {
        for key in subkeys.iter().take(10).skip(1).rev() {
            block = bc_gost_xsl(key, &block);
        }
        bc_gost_x(&mut block, &subkeys[0]);
    }
    block
}

pub fn bc_gost_throw_crypto_exception(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    message: &str,
) -> MethodCallFailed {
    let msg = ctx.create_string(message);
    match ctx.new_object_initialized(
        class_name,
        "(Ljava/lang/String;)V",
        &[Value::Object(Some(msg))],
    ) {
        Ok(Some(Value::Object(Some(exc)))) => MethodCallFailed::ExceptionThrown(exc),
        _ => RuntimeError::IllegalStateException {
            message: format!("{class_name}: {message}"),
        }
        .into(),
    }
}

pub fn bc_aes_bad_state(message: &str) -> MethodCallFailed {
    RuntimeError::IllegalStateException {
        message: message.into(),
    }
    .into()
}

pub fn bc_aes_block_args(
    ctx: &dyn NativeContext,
    args: &[Value],
) -> Option<([u8; 16], ObjectRef, usize, AesKeySchedule)> {
    let in_arr = obj_arg(args, 1).ok()?;
    let in_off = match args.get(2) {
        Some(Value::Int(v)) if *v >= 0 => *v as usize,
        _ => return None,
    };
    let out_arr = obj_arg(args, 3).ok()?;
    let out_off = match args.get(4) {
        Some(Value::Int(v)) if *v >= 0 => *v as usize,
        _ => return None,
    };
    let kw = read_aes_kw(ctx, obj_arg(args, 5).ok()?);
    if kw.len() < 2 {
        return None;
    }
    let mut inb = [0u8; 16];
    if ctx.read_byte_array_into(in_arr, in_off, &mut inb) != 16 {
        return None;
    }
    Some((inb, out_arr, out_off, kw))
}

pub fn bc_aes_native_encrypt_block(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let (inb, out_arr, out_off, kw) = bc_aes_block_args(ctx, args)
        .ok_or_else(|| bc_aes_bad_state("AESEngine native: malformed block/key state"))?;
    let mut outb = [0u8; 16];
    crate::bc_aes::encrypt_block(&kw, &inb, &mut outb);
    ctx.write_byte_array_from(out_arr, out_off, &outb);
    Ok(None)
}

pub fn bc_aes_native_decrypt_block(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let (inb, out_arr, out_off, kw) = bc_aes_block_args(ctx, args)
        .ok_or_else(|| bc_aes_bad_state("AESEngine native: malformed block/key state"))?;
    let mut outb = [0u8; 16];
    crate::bc_aes::decrypt_block(&kw, &inb, &mut outb);
    ctx.write_byte_array_from(out_arr, out_off, &outb);
    Ok(None)
}

pub fn bc_aes_native_generate_working_key(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let key_arr = obj_arg(args, 1)?;
    let for_enc = matches!(args.get(2), Some(Value::Int(v)) if *v != 0);
    let wk = bc_aes_alloc_working_key(ctx, this, key_arr, for_enc)?;
    Ok(Some(Value::Object(Some(wk))))
}

/// Are BouncyCastle services constraints installed?
///
/// Every AES engine entry point that CratonVM replaces natively ends, in
/// BouncyCastle's own bytecode, with a
/// `CryptoServicesRegistrar.checkConstraints(...)` call: the constructors
/// check the algorithm at full strength, `init` checks the key that was
/// actually supplied. That call raises `CryptoServiceConstraintsException`
/// when the process has constraints installed and the service does not meet
/// them, and a native that REPLACES the whole method drops it — an
/// under-strength key then initialises with no error at all, which is exactly
/// the failure a constraints policy exists to prevent
/// (`SymmetricConstraintsTest.testAES`, "no exception!").
///
/// Constraints are off by default and these natives exist for that default
/// path, so ask the registrar and hand the call straight back to the bytecode
/// whenever anything other than the built-in no-op constraints object is
/// installed. The real method then performs the real check with BouncyCastle's
/// own `DefaultServiceProperties`, which carry a per-engine bits-of-security
/// figure and a purpose derived from the direction — not something worth
/// transcribing here, where it would silently rot against the library.
///
/// Every uncertain answer (registrar unloadable, field renamed, accessor
/// missing) reports `true`: declining to the bytecode is always correct, only
/// slower, whereas skipping the check is a security hole.
pub fn bc_services_constraints_active(ctx: &mut dyn NativeContext) -> bool {
    const REGISTRAR: &str = "org/bouncycastle/crypto/CryptoServicesRegistrar";
    let Ok(class_id) = ctx.ensure_class_initialized(REGISTRAR) else {
        return true;
    };
    let Some(idx) = ctx.static_field_index_by_name(class_id, "noConstraintsImpl") else {
        return true;
    };
    let Value::Object(no_constraints) = ctx.get_static_field(class_id, idx) else {
        return true;
    };
    match ctx.invoke(
        REGISTRAR,
        "getServicesConstraints",
        "()Lorg/bouncycastle/crypto/CryptoServicesConstraints;",
        &[],
    ) {
        Ok(Some(Value::Object(current))) => current != no_constraints,
        _ => true,
    }
}

/// `<init>()` for the three AES engines. BouncyCastle's constructors are not
/// empty: each one checks its algorithm against the installed constraints at
/// full strength (`AESEngine` hardcodes 256, the other two ask
/// `bitsOfSecurity()`). With no constraints installed there is genuinely
/// nothing to do — the key schedule is built lazily by `init` /
/// `generateWorkingKey`, and no field initialiser runs — so the native keeps
/// the empty fast path that `newInstance()`'s `alloc_object` path wants, and
/// defers to the bytecode only when the check can actually fire.
pub fn bc_aes_native_ctor(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if bc_services_constraints_active(ctx) {
        let Some(class_name) = ctx.class_name_arc_of_id(ctx.class_id_of_object(this)) else {
            return Ok(None);
        };
        return ctx.invoke_special_bytecode_only(
            &class_name,
            "<init>",
            "()V",
            &[Value::Object(Some(this))],
        );
    }
    Ok(None)
}

pub fn bc_aes_native_init(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    if bc_services_constraints_active(ctx) {
        return ctx.invoke_virtual_bytecode_only(
            this,
            "init",
            "(ZLorg/bouncycastle/crypto/CipherParameters;)V",
            &args[1..],
        );
    }
    let for_enc = matches!(args.get(1), Some(Value::Int(v)) if *v != 0);
    let params = obj_arg(args, 2)?;
    let key_arr = match ctx.get_field_by_name(params, "key") {
        Value::Object(Some(o)) => o,
        _ => {
            return Err(RuntimeError::IllegalArgumentException {
                message: "invalid parameter passed to AES init".into(),
            }
            .into())
        }
    };
    // gc-common w20-e: the working key and the S-box clone both allocate;
    // they used to be installed through the engine's entry address.
    let this_pin = ctx.pin_native_root(this);
    let wk = bc_aes_alloc_working_key(ctx, this, key_arr, for_enc)?;
    let this = ctx.read_native_pin(this_pin, this);
    ctx.set_field_by_name(this, "WorkingKey", Value::Object(Some(wk)));
    ctx.set_field_by_name(this, "forEncryption", Value::Int(i32::from(for_enc)));

    if ctx
        .class_name_arc_of_id(ctx.class_id_of_object(this))
        .as_deref()
        == Some("org/bouncycastle/crypto/engines/AESEngine")
    {
        if let Some(sbox) = bc_aes_clone_static_sbox(
            ctx,
            "org/bouncycastle/crypto/engines/AESEngine",
            if for_enc { "S" } else { "Si" },
        ) {
            let this = ctx.read_native_pin(this_pin, this);
            ctx.set_field_by_name(this, "s", Value::Object(Some(sbox)));
        }
    }
    ctx.unpin_native_roots(this_pin);
    Ok(None)
}

pub fn bc_aes_native_process_block(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let in_arr = obj_arg(args, 1)?;
    let in_off = match args.get(2) {
        Some(Value::Int(v)) if *v >= 0 => *v as usize,
        Some(Value::Int(v)) => {
            // The block access `in[inOff]` raises the array's AIOOBE text
            // (wave 100 L7; was "Array index out of range: n").
            let length = i32::try_from(ctx.array_length(in_arr)).unwrap_or(i32::MAX);
            return Err(RuntimeError::aioobe(*v, length).into());
        }
        _ => 0,
    };
    let out_arr = obj_arg(args, 3)?;
    let out_off = match args.get(4) {
        Some(Value::Int(v)) if *v >= 0 => *v as usize,
        Some(Value::Int(v)) => {
            // The block access `out[outOff]` raises the array's AIOOBE text
            // (wave 100 L7; was "Array index out of range: n").
            let length = i32::try_from(ctx.array_length(out_arr)).unwrap_or(i32::MAX);
            return Err(RuntimeError::aioobe(*v, length).into());
        }
        _ => 0,
    };
    let wk_arr = match ctx.get_field_by_name(this, "WorkingKey") {
        Value::Object(Some(o)) => o,
        _ => return Err(bc_aes_bad_state("AES engine not initialised")),
    };
    if in_off.saturating_add(16) > ctx.array_length(in_arr) {
        return Err(bc_gost_throw_crypto_exception(
            ctx,
            "org/bouncycastle/crypto/DataLengthException",
            "input buffer too short",
        ));
    }
    if out_off.saturating_add(16) > ctx.array_length(out_arr) {
        return Err(bc_gost_throw_crypto_exception(
            ctx,
            "org/bouncycastle/crypto/OutputLengthException",
            "output buffer too short",
        ));
    }
    let kw = read_aes_kw(ctx, wk_arr);
    if kw.len() < 2 {
        return Err(bc_aes_bad_state(
            "AESEngine native: malformed block/key state",
        ));
    }
    let mut inb = [0u8; 16];
    ctx.read_byte_array_into(in_arr, in_off, &mut inb);
    let mut outb = [0u8; 16];
    if matches!(ctx.get_field_by_name(this, "forEncryption"), Value::Int(v) if v != 0) {
        crate::bc_aes::encrypt_block(&kw, &inb, &mut outb);
    } else {
        crate::bc_aes::decrypt_block(&kw, &inb, &mut outb);
    }
    ctx.write_byte_array_from(out_arr, out_off, &outb);
    Ok(Some(Value::Int(16)))
}

pub const BC_SM4_SBOX: [u8; 256] = [
    0xd6, 0x90, 0xe9, 0xfe, 0xcc, 0xe1, 0x3d, 0xb7, 0x16, 0xb6, 0x14, 0xc2, 0x28, 0xfb, 0x2c, 0x05,
    0x2b, 0x67, 0x9a, 0x76, 0x2a, 0xbe, 0x04, 0xc3, 0xaa, 0x44, 0x13, 0x26, 0x49, 0x86, 0x06, 0x99,
    0x9c, 0x42, 0x50, 0xf4, 0x91, 0xef, 0x98, 0x7a, 0x33, 0x54, 0x0b, 0x43, 0xed, 0xcf, 0xac, 0x62,
    0xe4, 0xb3, 0x1c, 0xa9, 0xc9, 0x08, 0xe8, 0x95, 0x80, 0xdf, 0x94, 0xfa, 0x75, 0x8f, 0x3f, 0xa6,
    0x47, 0x07, 0xa7, 0xfc, 0xf3, 0x73, 0x17, 0xba, 0x83, 0x59, 0x3c, 0x19, 0xe6, 0x85, 0x4f, 0xa8,
    0x68, 0x6b, 0x81, 0xb2, 0x71, 0x64, 0xda, 0x8b, 0xf8, 0xeb, 0x0f, 0x4b, 0x70, 0x56, 0x9d, 0x35,
    0x1e, 0x24, 0x0e, 0x5e, 0x63, 0x58, 0xd1, 0xa2, 0x25, 0x22, 0x7c, 0x3b, 0x01, 0x21, 0x78, 0x87,
    0xd4, 0x00, 0x46, 0x57, 0x9f, 0xd3, 0x27, 0x52, 0x4c, 0x36, 0x02, 0xe7, 0xa0, 0xc4, 0xc8, 0x9e,
    0xea, 0xbf, 0x8a, 0xd2, 0x40, 0xc7, 0x38, 0xb5, 0xa3, 0xf7, 0xf2, 0xce, 0xf9, 0x61, 0x15, 0xa1,
    0xe0, 0xae, 0x5d, 0xa4, 0x9b, 0x34, 0x1a, 0x55, 0xad, 0x93, 0x32, 0x30, 0xf5, 0x8c, 0xb1, 0xe3,
    0x1d, 0xf6, 0xe2, 0x2e, 0x82, 0x66, 0xca, 0x60, 0xc0, 0x29, 0x23, 0xab, 0x0d, 0x53, 0x4e, 0x6f,
    0xd5, 0xdb, 0x37, 0x45, 0xde, 0xfd, 0x8e, 0x2f, 0x03, 0xff, 0x6a, 0x72, 0x6d, 0x6c, 0x5b, 0x51,
    0x8d, 0x1b, 0xaf, 0x92, 0xbb, 0xdd, 0xbc, 0x7f, 0x11, 0xd9, 0x5c, 0x41, 0x1f, 0x10, 0x5a, 0xd8,
    0x0a, 0xc1, 0x31, 0x88, 0xa5, 0xcd, 0x7b, 0xbd, 0x2d, 0x74, 0xd0, 0x12, 0xb8, 0xe5, 0xb4, 0xb0,
    0x89, 0x69, 0x97, 0x4a, 0x0c, 0x96, 0x77, 0x7e, 0x65, 0xb9, 0xf1, 0x09, 0xc5, 0x6e, 0xc6, 0x84,
    0x18, 0xf0, 0x7d, 0xec, 0x3a, 0xdc, 0x4d, 0x20, 0x79, 0xee, 0x5f, 0x3e, 0xd7, 0xcb, 0x39, 0x48,
];

pub fn bc_sm4_t(z: u32) -> u32 {
    let b0 = BC_SM4_SBOX[((z >> 24) & 0xff) as usize] as u32;
    let b1 = BC_SM4_SBOX[((z >> 16) & 0xff) as usize] as u32;
    let b2 = BC_SM4_SBOX[((z >> 8) & 0xff) as usize] as u32;
    let b3 = BC_SM4_SBOX[(z & 0xff) as usize] as u32;
    let b = (b0 << 24) | (b1 << 16) | (b2 << 8) | b3;
    b ^ b.rotate_left(2) ^ b.rotate_left(10) ^ b.rotate_left(18) ^ b.rotate_left(24)
}

pub fn bc_sm4_read_rk(
    ctx: &dyn NativeContext,
    this: ObjectRef,
) -> Result<[u32; 32], MethodCallFailed> {
    let rk_arr = match ctx.get_field_by_name(this, "rk") {
        Value::Object(Some(o)) => o,
        _ => {
            return Err(RuntimeError::IllegalStateException {
                message: "SM4 not initialised".into(),
            }
            .into())
        }
    };
    if ctx.array_length(rk_arr) < 32 {
        return Err(RuntimeError::IllegalStateException {
            message: "SM4 native: malformed round-key state".into(),
        }
        .into());
    }
    let mut rk = [0u32; 32];
    for (i, slot) in rk.iter_mut().enumerate() {
        match ctx.get_array_element(rk_arr, i) {
            Value::Int(v) => *slot = v as u32,
            _ => {
                return Err(RuntimeError::IllegalStateException {
                    message: "SM4 native: malformed round-key state".into(),
                }
                .into())
            }
        }
    }
    Ok(rk)
}

pub fn bc_sm4_native_process_block(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let in_arr = obj_arg(args, 1)?;
    let in_off = match args.get(2) {
        Some(Value::Int(v)) if *v >= 0 => *v as usize,
        Some(Value::Int(v)) => {
            // The block access `in[inOff]` raises the array's AIOOBE text
            // (wave 100 L7; was "Array index out of range: n").
            let length = i32::try_from(ctx.array_length(in_arr)).unwrap_or(i32::MAX);
            return Err(RuntimeError::aioobe(*v, length).into());
        }
        _ => 0,
    };
    let out_arr = obj_arg(args, 3)?;
    let out_off = match args.get(4) {
        Some(Value::Int(v)) if *v >= 0 => *v as usize,
        Some(Value::Int(v)) => {
            // The block access `out[outOff]` raises the array's AIOOBE text
            // (wave 100 L7; was "Array index out of range: n").
            let length = i32::try_from(ctx.array_length(out_arr)).unwrap_or(i32::MAX);
            return Err(RuntimeError::aioobe(*v, length).into());
        }
        _ => 0,
    };
    let rk = bc_sm4_read_rk(ctx, this)?;
    if in_off.saturating_add(16) > ctx.array_length(in_arr) {
        return Err(bc_gost_throw_crypto_exception(
            ctx,
            "org/bouncycastle/crypto/DataLengthException",
            "input buffer too short",
        ));
    }
    if out_off.saturating_add(16) > ctx.array_length(out_arr) {
        return Err(bc_gost_throw_crypto_exception(
            ctx,
            "org/bouncycastle/crypto/OutputLengthException",
            "output buffer too short",
        ));
    }

    let mut block = [0u8; 16];
    ctx.read_byte_array_into(in_arr, in_off, &mut block);
    let mut x = [
        u32::from_be_bytes([block[0], block[1], block[2], block[3]]),
        u32::from_be_bytes([block[4], block[5], block[6], block[7]]),
        u32::from_be_bytes([block[8], block[9], block[10], block[11]]),
        u32::from_be_bytes([block[12], block[13], block[14], block[15]]),
    ];
    for i in (0..32).step_by(4) {
        x[0] ^= bc_sm4_t(x[1] ^ x[2] ^ x[3] ^ rk[i]);
        x[1] ^= bc_sm4_t(x[2] ^ x[3] ^ x[0] ^ rk[i + 1]);
        x[2] ^= bc_sm4_t(x[3] ^ x[0] ^ x[1] ^ rk[i + 2]);
        x[3] ^= bc_sm4_t(x[0] ^ x[1] ^ x[2] ^ rk[i + 3]);
    }
    let mut out = [0u8; 16];
    out[0..4].copy_from_slice(&x[3].to_be_bytes());
    out[4..8].copy_from_slice(&x[2].to_be_bytes());
    out[8..12].copy_from_slice(&x[1].to_be_bytes());
    out[12..16].copy_from_slice(&x[0].to_be_bytes());
    ctx.write_byte_array_from(out_arr, out_off, &out);
    Ok(Some(Value::Int(16)))
}

pub fn register_bc_sm4_engine(r: &mut NativeMethodRegistry) {
            bc_register_null_rerun!(
        r,
        "org/bouncycastle/crypto/engines/SM4Engine",
        "processBlock",
        "([BI[BI)I",
        bc_sm4_native_process_block,
    );
    }

pub fn bc_xtea_read_sum(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    field: &str,
) -> Result<[u32; 32], MethodCallFailed> {
    let arr = match ctx.get_field_by_name(this, field) {
        Value::Object(Some(o)) => o,
        _ => {
            return Err(RuntimeError::IllegalStateException {
                message: "XTEA native: missing round-key state".into(),
            }
            .into())
        }
    };
    if ctx.array_length(arr) < 32 {
        return Err(RuntimeError::IllegalStateException {
            message: "XTEA native: malformed round-key state".into(),
        }
        .into());
    }
    let mut out = [0u32; 32];
    for (i, slot) in out.iter_mut().enumerate() {
        match ctx.get_array_element(arr, i) {
            Value::Int(v) => *slot = v as u32,
            _ => {
                return Err(RuntimeError::IllegalStateException {
                    message: "XTEA native: malformed round-key state".into(),
                }
                .into())
            }
        }
    }
    Ok(out)
}

pub fn bc_xtea_native_process_block(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let in_arr = obj_arg(args, 1)?;
    let in_off = match args.get(2) {
        Some(Value::Int(v)) if *v >= 0 => *v as usize,
        Some(Value::Int(v)) => {
            // The block access `in[inOff]` raises the array's AIOOBE text
            // (wave 100 L7; was "Array index out of range: n").
            let length = i32::try_from(ctx.array_length(in_arr)).unwrap_or(i32::MAX);
            return Err(RuntimeError::aioobe(*v, length).into());
        }
        _ => 0,
    };
    let out_arr = obj_arg(args, 3)?;
    let out_off = match args.get(4) {
        Some(Value::Int(v)) if *v >= 0 => *v as usize,
        Some(Value::Int(v)) => {
            // The block access `out[outOff]` raises the array's AIOOBE text
            // (wave 100 L7; was "Array index out of range: n").
            let length = i32::try_from(ctx.array_length(out_arr)).unwrap_or(i32::MAX);
            return Err(RuntimeError::aioobe(*v, length).into());
        }
        _ => 0,
    };

    if !matches!(ctx.get_field_by_name(this, "_initialised"), Value::Int(v) if v != 0) {
        return Err(RuntimeError::IllegalStateException {
            message: "XTEA not initialised".into(),
        }
        .into());
    }
    if in_off.saturating_add(8) > ctx.array_length(in_arr) {
        return Err(bc_gost_throw_crypto_exception(
            ctx,
            "org/bouncycastle/crypto/DataLengthException",
            "input buffer too short",
        ));
    }
    if out_off.saturating_add(8) > ctx.array_length(out_arr) {
        return Err(bc_gost_throw_crypto_exception(
            ctx,
            "org/bouncycastle/crypto/OutputLengthException",
            "output buffer too short",
        ));
    }

    let sum0 = bc_xtea_read_sum(ctx, this, "_sum0")?;
    let sum1 = bc_xtea_read_sum(ctx, this, "_sum1")?;
    let mut block = [0u8; 8];
    ctx.read_byte_array_into(in_arr, in_off, &mut block);
    let mut v0 = u32::from_be_bytes([block[0], block[1], block[2], block[3]]);
    let mut v1 = u32::from_be_bytes([block[4], block[5], block[6], block[7]]);

    if matches!(ctx.get_field_by_name(this, "_forEncryption"), Value::Int(v) if v != 0) {
        for i in 0..32 {
            v0 = v0.wrapping_add(((v1 << 4 ^ v1 >> 5).wrapping_add(v1)) ^ sum0[i]);
            v1 = v1.wrapping_add(((v0 << 4 ^ v0 >> 5).wrapping_add(v0)) ^ sum1[i]);
        }
    } else {
        for i in (0..32).rev() {
            v1 = v1.wrapping_sub(((v0 << 4 ^ v0 >> 5).wrapping_add(v0)) ^ sum1[i]);
            v0 = v0.wrapping_sub(((v1 << 4 ^ v1 >> 5).wrapping_add(v1)) ^ sum0[i]);
        }
    }

    let mut out = [0u8; 8];
    out[0..4].copy_from_slice(&v0.to_be_bytes());
    out[4..8].copy_from_slice(&v1.to_be_bytes());
    ctx.write_byte_array_from(out_arr, out_off, &out);
    Ok(Some(Value::Int(8)))
}

pub fn register_bc_xtea_engine(r: &mut NativeMethodRegistry) {
            bc_register_null_rerun!(
        r,
        "org/bouncycastle/crypto/engines/XTEAEngine",
        "processBlock",
        "([BI[BI)I",
        bc_xtea_native_process_block,
    );
    }

pub fn register_bc_gost3412_engine(r: &mut NativeMethodRegistry) {
            bc_register_null_rerun!(
        r,
        "org/bouncycastle/crypto/engines/GOST3412_2015Engine",
        "processBlock",
        "([BI[BI)I",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let in_arr = obj_arg(args, 1)?;
            let in_off = match args.get(2) {
                Some(Value::Int(v)) if *v >= 0 => *v as usize,
                Some(Value::Int(v)) => {
                    // The block access `in[inOff]` raises the array's AIOOBE text
                    // (wave 100 L7; was "Array index out of range: n").
                    let length = i32::try_from(ctx.array_length(in_arr)).unwrap_or(i32::MAX);
                    return Err(RuntimeError::aioobe(*v, length).into());
                }
                _ => 0,
            };
            let out_arr = obj_arg(args, 3)?;
            let out_off = match args.get(4) {
                Some(Value::Int(v)) if *v >= 0 => *v as usize,
                Some(Value::Int(v)) => {
                    // The block access `out[outOff]` raises the array's AIOOBE text
                    // (wave 100 L7; was "Array index out of range: n").
                    let length = i32::try_from(ctx.array_length(out_arr)).unwrap_or(i32::MAX);
                    return Err(RuntimeError::aioobe(*v, length).into());
                }
                _ => 0,
            };

            let subkeys_outer = match ctx.get_field_by_name(this, "subKeys") {
                Value::Object(Some(o)) => o,
                _ => {
                    return Err(RuntimeError::IllegalStateException {
                        message: "GOST3412_2015 engine not initialised".into(),
                    }
                    .into())
                }
            };
            if in_off.saturating_add(16) > ctx.array_length(in_arr) {
                return Err(bc_gost_throw_crypto_exception(
                    ctx,
                    "org/bouncycastle/crypto/DataLengthException",
                    "input buffer too short",
                ));
            }
            if out_off.saturating_add(16) > ctx.array_length(out_arr) {
                return Err(bc_gost_throw_crypto_exception(
                    ctx,
                    "org/bouncycastle/crypto/OutputLengthException",
                    "output buffer too short",
                ));
            }
            let subkeys = bc_gost_read_subkeys(ctx, subkeys_outer).ok_or_else(|| {
                RuntimeError::IllegalStateException {
                    message: "GOST3412_2015 native: malformed subkey state".into(),
                }
            })?;
            let for_encryption =
                matches!(ctx.get_field_by_name(this, "forEncryption"), Value::Int(v) if v != 0);
            let mut input = [0u8; 16];
            ctx.read_byte_array_into(in_arr, in_off, &mut input);
            let output = bc_gost_process_block(&subkeys, for_encryption, &input);
            ctx.write_byte_array_from(out_arr, out_off, &output);
            Ok(Some(Value::Int(16)))
        },
    );
    }

/// Native fast-path for `org.bouncycastle.crypto.engines.AESEngine`'s private
/// single-block transforms. With `org/bouncycastle/*` JIT-banned, the
/// interpreted T-table AES dominates `AESTest`'s block-cipher Monte-Carlo
/// stress (the documented AES non-finish). These intercept the private
/// `encryptBlock`/`decryptBlock(byte[] in, int inOff, byte[] out, int outOff,
/// int[][] KW)` — which already receive the expanded key schedule and run with
/// all the public `processBlock` checks done — and apply a verbatim,
/// FIPS-197-validated port (see [`crate::bc_aes`]). A registered native fully
/// replaces the body (no decline-to-bytecode path), and `processBlock` has
/// already validated the buffers and non-null key, so the defensive
/// "can't happen" cases surface as an `IllegalStateException` rather than
/// silently no-op'ing the void method.
pub fn register_bc_aes_engine(r: &mut NativeMethodRegistry) {
            const AES: &str = "org/bouncycastle/crypto/engines/AESEngine";
    const DESC: &str = "([BI[BI[[I)V";

    fn block_args(
        ctx: &dyn NativeContext,
        args: &[Value],
    ) -> Option<([u8; 16], usize, ObjectRef, usize, AesKeySchedule)> {
        let in_arr = obj_arg(args, 1).ok()?;
        let in_off = match args.get(2) {
            Some(Value::Int(v)) => *v as usize,
            _ => return None,
        };
        let out_arr = obj_arg(args, 3).ok()?;
        let out_off = match args.get(4) {
            Some(Value::Int(v)) => *v as usize,
            _ => return None,
        };
        let kw = read_aes_kw(ctx, obj_arg(args, 5).ok()?);
        if kw.len() < 2 {
            return None; // malformed schedule (can't happen post-init)
        }
        let mut inb = [0u8; 16];
        if ctx.read_byte_array_into(in_arr, in_off, &mut inb) != 16 {
            return None; // input shorter than a block (processBlock pre-checks this)
        }
        Some((inb, in_off, out_arr, out_off, kw))
    }

    fn bad_state() -> MethodCallFailed {
        RuntimeError::IllegalStateException {
            message: "AESEngine native: malformed block/key state".into(),
        }
        .into()
    }

    bc_register_null_rerun!(r, AES, "encryptBlock", DESC, |ctx, args| {
        let (inb, _in_off, out_arr, out_off, kw) = block_args(ctx, args).ok_or_else(bad_state)?;
        let mut outb = [0u8; 16];
        crate::bc_aes::encrypt_block(&kw, &inb, &mut outb);
        ctx.write_byte_array_from(out_arr, out_off, &outb);
        Ok(None)
    });
    bc_register_null_rerun!(r, AES, "decryptBlock", DESC, |ctx, args| {
        let (inb, _in_off, out_arr, out_off, kw) = block_args(ctx, args).ok_or_else(bad_state)?;
        let mut outb = [0u8; 16];
        crate::bc_aes::decrypt_block(&kw, &inb, &mut outb);
        ctx.write_byte_array_from(out_arr, out_off, &outb);
        Ok(None)
    });

    // KEEP (empty only while no constraints are installed): the key schedule
    // is built lazily by `init`/`generateWorkingKey` (registered below), not at
    // construction, and BouncyCastle's own `AESEngine()` declares no field
    // initialiser — so with the registrar at its default the native body is
    // genuinely nothing, which is what `newInstance()`'s `alloc_object` + the
    // real `MultiBlockCipher` call path want. The constructor is NOT empty
    // otherwise: it checks AES-256 against the installed constraints, so
    // `bc_aes_native_ctor` hands the call back to the bytecode when any are.
    bc_register_null_rerun!(r, AES, "<init>", "()V", bc_aes_native_ctor);
    bc_register_null_rerun!(
        r,
        AES,
        "newInstance",
        "()Lorg/bouncycastle/crypto/MultiBlockCipher;",
        |ctx, _args| {
            let class_id =
                ctx.ensure_class_initialized("org/bouncycastle/crypto/engines/AESEngine")?;
            let field_count = ctx.class_num_total_fields(class_id);
            let obj = ctx.alloc_object(class_id, field_count);
            Ok(Some(Value::Object(Some(obj))))
        },
    );

    // generateWorkingKey(byte[] key, boolean forEncryption) -> int[][]. The
    // private key-schedule expansion; `AESTest.testCounter` churns it via
    // repeated `newCipher()`/`init`, so it became the hot frame once the block
    // transforms above went native. Also sets `this.ROUNDS` (the field's only
    // other readers are the now-native encrypt/decryptBlock).
    bc_register_null_rerun!(r, AES, "generateWorkingKey", "([BZ)[[I", |ctx, args| {
        let this = obj_arg(args, 0)?;
        let key_arr = obj_arg(args, 1)?;
        let for_enc = matches!(args.get(2), Some(Value::Int(v)) if *v != 0);

        let klen = ctx.array_length(key_arr);
        let mut kbuf = [0u8; 32];
        let key: &[u8] = if klen <= 32 {
            let n = ctx.read_byte_array_into(key_arr, 0, &mut kbuf[..klen]);
            &kbuf[..n]
        } else {
            &[]
        };

        let w = match crate::bc_aes::generate_working_key(key, for_enc) {
            Some(w) => w,
            None => {
                return Err(RuntimeError::IllegalArgumentException {
                    message: "Key length not 128/192/256 bits.".into(),
                }
                .into())
            }
        };

        // ROUNDS side effect (KC + 6 == rows - 1), matching BC.
        ctx.set_field_by_name(this, "ROUNDS", Value::Int((w.len() - 1) as i32));

        // Build the int[][] schedule (rooted across the row allocations; see
        // `bc_aes_schedule_array`).
        let outer = bc_aes_schedule_array(ctx, &w);
        Ok(Some(Value::Object(Some(outer))))
    });

    // init(boolean, CipherParameters). `AESTest.testCounter` constructs two
    // fresh AES-CTR engines per verify() call; with BC JIT-banned, the bytecode
    // init scaffolding remains hot even after block encryption and key schedule
    // generation are native.
    bc_register_null_rerun!(
        r,
        AES,
        "init",
        "(ZLorg/bouncycastle/crypto/CipherParameters;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            // Same dropped `CryptoServicesRegistrar.checkConstraints` as in
            // `bc_aes_native_init`; see `bc_services_constraints_active`.
            if bc_services_constraints_active(ctx) {
                return ctx.invoke_virtual_bytecode_only(
                    this,
                    "init",
                    "(ZLorg/bouncycastle/crypto/CipherParameters;)V",
                    &args[1..],
                );
            }
            let for_enc = matches!(args.get(1), Some(Value::Int(v)) if *v != 0);
            let params = obj_arg(args, 2)?;
            let key_arr = match ctx.get_field_by_name(params, "key") {
                Value::Object(Some(o)) => o,
                _ => {
                    return Err(RuntimeError::IllegalArgumentException {
                        message: "invalid parameter passed to AES init".into(),
                    }
                    .into())
                }
            };
            // gc-common w20-e: as in `bc_aes_native_init`, the engine is
            // rooted across the working key's and the S-box's allocations.
            let this_pin = ctx.pin_native_root(this);
            let wk = bc_aes_alloc_working_key(ctx, this, key_arr, for_enc)?;
            let this = ctx.read_native_pin(this_pin, this);
            ctx.set_field_by_name(this, "WorkingKey", Value::Object(Some(wk)));
            ctx.set_field_by_name(this, "forEncryption", Value::Int(i32::from(for_enc)));
            if let Some(sbox) = bc_aes_clone_static_sbox(
                ctx,
                "org/bouncycastle/crypto/engines/AESEngine",
                if for_enc { "S" } else { "Si" },
            ) {
                let this = ctx.read_native_pin(this_pin, this);
                ctx.set_field_by_name(this, "s", Value::Object(Some(sbox)));
            }
            ctx.unpin_native_roots(this_pin);
            Ok(None)
        },
    );

    bc_register_null_rerun!(
        r,
        AES,
        "processBlock",
        "([BI[BI)I",
        bc_aes_native_process_block,
    );

    // One expansion per class, in the order the former loop registered them
    // (wave 107, lane L7: the null-argument wrapper needs the class as a
    // constant, which a loop variable is not).
    macro_rules! register_aes_impl {
        ($aes_impl:literal) => {{
        const AES_IMPL: &str = $aes_impl;
        // Trivial constructor with one caveat. BouncyCastle's block ciphers
        // carry all their state in fields that `init(boolean,
        // CipherParameters)` writes — here `bc_aes_native_init` (registered a
        // few lines below for this same class) sets `ROUNDS`, `WorkingKey`,
        // `forEncryption` and `s`. The no-arg constructor declares no field
        // initializers and only chains to `Object.<init>`, and
        // `bc_aes_native_process_block` refuses to run on an object whose
        // `WorkingKey` is still unset ("AES engine not initialised"), so the
        // real initialiser is provably on the use path. What the body does
        // carry is a constraints check on `bitsOfSecurity()`, so
        // `bc_aes_native_ctor` runs the bytecode whenever constraints are
        // installed and stays empty otherwise. KEEP.
        bc_register_null_rerun!(r, AES_IMPL, "<init>", "()V", bc_aes_native_ctor);
        bc_register_null_rerun!(r, AES_IMPL, "encryptBlock", DESC, bc_aes_native_encrypt_block);
        bc_register_null_rerun!(r, AES_IMPL, "decryptBlock", DESC, bc_aes_native_decrypt_block);
        bc_register_null_rerun!(
            r,
            AES_IMPL,
            "generateWorkingKey",
            "([BZ)[[I",
            bc_aes_native_generate_working_key,
        );
        bc_register_null_rerun!(
            r,
            AES_IMPL,
            "init",
            "(ZLorg/bouncycastle/crypto/CipherParameters;)V",
            bc_aes_native_init,
        );
        bc_register_null_rerun!(
            r,
            AES_IMPL,
            "processBlock",
            "([BI[BI)I",
            bc_aes_native_process_block,
        );
        }};
    }
    register_aes_impl!("org/bouncycastle/crypto/engines/AESLightEngine");
    register_aes_impl!("org/bouncycastle/crypto/engines/AESFastEngine");

    bc_register_null_rerun!(
        r,
        "org/bouncycastle/crypto/modes/SICBlockCipher",
        "reset",
        "()V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let cipher = bc_sic_object_field(ctx, this, "cipher")?;
            let counter_arr = bc_sic_object_field(ctx, this, "counter")?;
            let iv_arr = bc_sic_object_field(ctx, this, "IV")?;
            bc_sic_write_reset_counter(ctx, this, counter_arr, iv_arr)?;
            // `reset()` clears the advance-since-init accumulator and the sticky
            // out-of-range flag. Omitting this made the range bound un-clearable
            // rather than merely unenforced.
            bc_sic_reset_range(ctx, this);
            ctx.invoke_virtual(cipher, "reset", "()V", &[])?;
            Ok(None)
        },
    );

    bc_register_null_rerun!(
        r,
        "org/bouncycastle/crypto/modes/SICBlockCipher",
        "seekTo",
        "(J)J",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let position = match args.get(1) {
                Some(Value::Long(v)) => *v,
                _ => 0,
            };
            if position < 0 {
                return Err(RuntimeError::IllegalArgumentException {
                    message: "position must be non-negative".into(),
                }
                .into());
            }
            let cipher = bc_sic_object_field(ctx, this, "cipher")?;
            let counter_arr = bc_sic_object_field(ctx, this, "counter")?;
            let counter_out_arr = bc_sic_object_field(ctx, this, "counterOut")?;
            let iv_arr = bc_sic_object_field(ctx, this, "IV")?;
            let bs = ctx.array_length(counter_arr);
            if bs == 0 {
                return Err(bc_sic_bad_state("SICBlockCipher: zero block size"));
            }
            let mut counter = bc_sic_write_reset_counter(ctx, this, counter_arr, iv_arr)?;
            // Java `seekTo` is `reset()` then `skip(position)`, and `reset()`
            // clears the range state before the seek re-derives it.
            bc_sic_reset_range(ctx, this);
            let blocks = (position as u64) / (bs as u64);
            let byte_count = ((position as u64) % (bs as u64)) as i32;
            bc_sic_add_blocks(&mut counter, blocks);
            // `checkCounter`, not just the partial-IV prefix half of it — the
            // full-block-IV branch is the one that bounds the advance at 2^64
            // blocks and resyncs `used`.
            let iv_len = ctx.array_length(iv_arr);
            let mut iv = vec![0u8; iv_len];
            ctx.read_byte_array_into(iv_arr, 0, &mut iv);
            let mut range = BcSicRange::read(ctx, this);
            let mut delta = vec![0u8; bs];
            let verdict = range.check_counter(&counter, &iv, &mut delta);
            range.write_back(ctx, this);
            verdict?;
            ctx.write_byte_array_from(counter_arr, 0, &counter);
            ctx.set_field_by_name(this, "byteCount", Value::Int(byte_count));
            bc_sic_encrypt_counter(ctx, cipher, counter_arr, counter_out_arr, &counter)?;
            Ok(Some(Value::Long(position)))
        },
    );

    bc_register_null_rerun!(
        r,
        "org/bouncycastle/crypto/modes/SICBlockCipher",
        "init",
        "(ZLorg/bouncycastle/crypto/CipherParameters;)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let params = obj_arg(args, 2)?;
            let cipher = bc_sic_object_field(ctx, this, "cipher")?;
            let counter_arr = bc_sic_object_field(ctx, this, "counter")?;
            let iv_src = bc_sic_object_field(ctx, params, "iv")?;
            let iv_len = ctx.array_length(iv_src);
            let bs = ctx.array_length(counter_arr);
            if bs < iv_len {
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!("CTR/SIC mode requires IV no greater than: {bs} bytes."),
                }
                .into());
            }
            let max_counter_size = 8usize.min(bs / 2);
            if bs.saturating_sub(iv_len) > max_counter_size {
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!(
                        "CTR/SIC mode requires IV of at least: {} bytes.",
                        bs - max_counter_size
                    ),
                }
                .into());
            }

            // gc-common w20-e: `cipher.init` runs Java and the IV copy's
            // allocation can collect; the receiver, the cipher and the IV
            // source were used through their entry addresses after both.
            // Pinned across them; the IV is copied out before the copy is
            // allocated.
            let this_pin = ctx.pin_native_root(this);
            let cipher_pin = ctx.pin_native_root(cipher);
            let iv_pin = ctx.pin_native_root(iv_src);
            if let Value::Object(Some(inner)) = ctx.get_field_by_name(params, "parameters") {
                ctx.invoke_virtual(
                    cipher,
                    "init",
                    "(ZLorg/bouncycastle/crypto/CipherParameters;)V",
                    &[Value::Int(1), Value::Object(Some(inner))],
                )?;
            }

            let iv_src = ctx.read_native_pin(iv_pin, iv_src);
            let mut iv = vec![0u8; iv_len];
            if iv_len != 0 {
                ctx.read_byte_array_into(iv_src, 0, &mut iv);
            }
            let iv_copy = ctx.new_array(cratonvm_types::ArrayElementType::Byte, iv_len);
            if iv_len != 0 {
                ctx.write_byte_array_from(iv_copy, 0, &iv);
            }
            let this = ctx.read_native_pin(this_pin, this);
            let cipher = ctx.read_native_pin(cipher_pin, cipher);
            ctx.unpin_native_roots(this_pin);
            ctx.set_field_by_name(this, "IV", Value::Object(Some(iv_copy)));
            // The two derived fields the full-block-IV range bound is built on.
            // `fullBlockIV` read back FALSE for a 16-byte IV on a 16-byte block
            // because nothing here wrote it, which disarmed the `used`
            // wrap-around detection for every block size of 8 or less.
            let full_block_iv = iv_len == bs;
            ctx.set_field_by_name(this, "fullBlockIV", Value::Int(i32::from(full_block_iv)));
            let lane_off = match ctx.get_field_by_name(this, "laneOff") {
                Value::Int(v) => v.max(0) as usize,
                _ => 0,
            };
            if full_block_iv && lane_off > 0 {
                let guard = match ctx.get_array_element(iv_copy, lane_off - 1) {
                    Value::Int(v) => (v as u8).wrapping_add(1),
                    _ => 0,
                };
                ctx.set_field_by_name(this, "guardByte", Value::Int(guard as i8 as i32));
            }
            let counter_arr = bc_sic_object_field(ctx, this, "counter")?;
            bc_sic_write_reset_counter(ctx, this, counter_arr, iv_copy)?;
            bc_sic_reset_range(ctx, this);
            ctx.invoke_virtual(cipher, "reset", "()V", &[])?;
            Ok(None)
        },
    );

    }

pub fn bc_stream_read_i32x16(ctx: &dyn NativeContext, arr: ObjectRef) -> Option<[i32; 16]> {
    if ctx.array_length(arr) != 16 {
        return None;
    }
    let mut out = [0i32; 16];
    for (i, slot) in out.iter_mut().enumerate() {
        match ctx.get_array_element(arr, i) {
            Value::Int(v) => *slot = v,
            _ => return None,
        }
    }
    Some(out)
}

pub fn bc_stream_write_i32x16(ctx: &dyn NativeContext, arr: ObjectRef, words: &[i32; 16]) {
    for (i, &word) in words.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(word));
    }
}

pub fn bc_stream_words_to_le_bytes(words: &[i32; 16]) -> [u8; 64] {
    let mut out = [0u8; 64];
    for i in 0..16 {
        out[4 * i..4 * i + 4].copy_from_slice(&(words[i] as u32).to_le_bytes());
    }
    out
}

pub fn bc_stream_generate_key_stream(
    ctx: &mut dyn NativeContext,
    class_name: &str,
    rounds: i32,
    engine_state: &[i32; 16],
    x_arr: ObjectRef,
    key_stream_arr: ObjectRef,
    key_stream: &mut [u8; 64],
) -> Result<(), MethodCallFailed> {
    let mut x = [0i32; 16];
    match class_name {
        "org/bouncycastle/crypto/engines/ChaChaEngine"
        | "org/bouncycastle/crypto/engines/ChaCha7539Engine"
        | "org/bouncycastle/crypto/engines/XChaCha20Engine" => {
            crate::bc_chacha::chacha_core(rounds, engine_state, &mut x);
        }
        "org/bouncycastle/crypto/engines/Salsa20Engine"
        | "org/bouncycastle/crypto/engines/XSalsa20Engine" => {
            crate::bc_chacha::salsa_core(rounds, engine_state, &mut x);
        }
        _ => {
            return Err(RuntimeError::IllegalStateException {
                message: "Salsa20Engine.processBytes: unsupported engine class".into(),
            }
            .into())
        }
    }
    bc_stream_write_i32x16(ctx, x_arr, &x);
    *key_stream = bc_stream_words_to_le_bytes(&x);
    ctx.write_byte_array_from(key_stream_arr, 0, key_stream);
    Ok(())
}

pub fn bc_stream_advance_counter(
    class_name: &str,
    engine_state: &mut [i32; 16],
) -> Result<(), MethodCallFailed> {
    match class_name {
        "org/bouncycastle/crypto/engines/ChaCha7539Engine"
        | "org/bouncycastle/crypto/engines/XChaCha20Engine" => {
            engine_state[12] = engine_state[12].wrapping_add(1);
            if engine_state[12] == 0 {
                return Err(RuntimeError::IllegalStateException {
                    message: "attempt to increase counter past 2^32.".into(),
                }
                .into());
            }
        }
        "org/bouncycastle/crypto/engines/ChaChaEngine" => {
            engine_state[12] = engine_state[12].wrapping_add(1);
            if engine_state[12] == 0 {
                engine_state[13] = engine_state[13].wrapping_add(1);
            }
        }
        "org/bouncycastle/crypto/engines/Salsa20Engine"
        | "org/bouncycastle/crypto/engines/XSalsa20Engine" => {
            engine_state[8] = engine_state[8].wrapping_add(1);
            if engine_state[8] == 0 {
                engine_state[9] = engine_state[9].wrapping_add(1);
            }
        }
        _ => {
            return Err(RuntimeError::IllegalStateException {
                message: "Salsa20Engine.processBytes: unsupported counter layout".into(),
            }
            .into())
        }
    }
    Ok(())
}

pub fn bc_salsa20_process_bytes(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let in_arr = obj_arg(args, 1)?;
    let in_off_i = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let len_i = match args.get(3) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let out_arr = obj_arg(args, 4)?;
    let out_off_i = match args.get(5) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    if !matches!(ctx.get_field_by_name(this, "initialised"), Value::Int(v) if v != 0) {
        return Err(RuntimeError::IllegalStateException {
            message: "Salsa20Engine not initialised".into(),
        }
        .into());
    }
    if in_off_i < 0
        || len_i < 0
        || out_off_i < 0
        || (in_off_i as usize).saturating_add(len_i as usize) > ctx.array_length(in_arr)
    {
        return Err(bc_gost_throw_crypto_exception(
            ctx,
            "org/bouncycastle/crypto/DataLengthException",
            "input buffer too short",
        ));
    }
    if (out_off_i as usize).saturating_add(len_i as usize) > ctx.array_length(out_arr) {
        return Err(bc_gost_throw_crypto_exception(
            ctx,
            "org/bouncycastle/crypto/OutputLengthException",
            "output buffer too short",
        ));
    }

    let class_name = ctx
        .class_name_of_id(ctx.class_id_of_object(this))
        .unwrap_or_default();
    // SECURITY — do NOT restore the `_ => 20` default here.
    //
    // `rounds` is the only thing that makes the ChaCha/Salsa permutation a
    // permutation: `chacha_core(0, ..)` / `salsa_core(0, ..)` is the identity,
    // so the "keystream" is the engine state and the key is recoverable
    // straight out of the ciphertext (see `check_rounds` in
    // `native-builtins-crypto/src/bc_chacha.rs`, which names this call site as
    // one of the two arms that validate nothing).
    //
    // The previous read was `match ctx.get_field_by_name(this, "rounds") {
    // Value::Int(v) => v, _ => 20 }`. That looks like it defaults to 20, but it
    // does not: `get_field_by_name` is not descriptor-aware, so an unwritten
    // slot answers `Value::Int(0)` — which the FIRST arm accepts. The `_ => 20`
    // fallback only ever fires when the field does not resolve at all. So the
    // dangerous input (0) took the "valid value" path and the safe default was
    // unreachable for it. `int_field_strict` reads by resolved index instead
    // (descriptor-decoded) and returns `None` for an absent field, and we
    // refuse outright rather than guess: a round count we cannot read is not a
    // round count we may substitute.
    let rounds = match crate::field_read::int_field_strict(ctx, this, "rounds") {
        Some(v) if v > 0 && v % 2 == 0 => v,
        _ => {
            return Err(RuntimeError::IllegalStateException {
                message: "Salsa20Engine.processBytes: refusing to generate a keystream with an \
                          unreadable or illegal round count"
                    .into(),
            }
            .into())
        }
    };
    let engine_state_arr = match ctx.get_field_by_name(this, "engineState") {
        Value::Object(Some(o)) => o,
        _ => {
            return Err(RuntimeError::IllegalStateException {
                message: "Salsa20Engine.processBytes: missing engineState".into(),
            }
            .into())
        }
    };
    let x_arr = match ctx.get_field_by_name(this, "x") {
        Value::Object(Some(o)) => o,
        _ => {
            return Err(RuntimeError::IllegalStateException {
                message: "Salsa20Engine.processBytes: missing x".into(),
            }
            .into())
        }
    };
    let key_stream_arr = match ctx.get_field_by_name(this, "keyStream") {
        Value::Object(Some(o)) => o,
        _ => {
            return Err(RuntimeError::IllegalStateException {
                message: "Salsa20Engine.processBytes: missing keyStream".into(),
            }
            .into())
        }
    };
    let mut index = match ctx.get_field_by_name(this, "index") {
        Value::Int(v) => v as usize,
        _ => 0,
    } & 63;
    let mut engine_state = bc_stream_read_i32x16(ctx, engine_state_arr).ok_or_else(|| {
        RuntimeError::IllegalStateException {
            message: "Salsa20Engine.processBytes: malformed engineState".into(),
        }
    })?;
    let mut key_stream = [0u8; 64];
    ctx.read_byte_array_into(key_stream_arr, 0, &mut key_stream);

    let len = len_i as usize;
    let mut input = vec![0u8; len];
    ctx.read_byte_array_into(in_arr, in_off_i as usize, &mut input);
    let mut output = vec![0u8; len];
    let mut pos = 0usize;
    while pos < len {
        let take = (64 - index).min(len - pos);
        for i in 0..take {
            output[pos + i] = input[pos + i] ^ key_stream[index + i];
        }
        pos += take;
        index = (index + take) & 63;
        if index == 0 {
            bc_stream_advance_counter(&class_name, &mut engine_state)?;
            bc_stream_generate_key_stream(
                ctx,
                &class_name,
                rounds,
                &engine_state,
                x_arr,
                key_stream_arr,
                &mut key_stream,
            )?;
        }
    }

    bc_stream_write_i32x16(ctx, engine_state_arr, &engine_state);
    ctx.set_field_by_name(this, "index", Value::Int(index as i32));
    ctx.write_byte_array_from(out_arr, out_off_i as usize, &output);
    Ok(Some(Value::Int(len_i)))
}

pub fn bc_vmpc_process_bytes(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let this = obj_arg(args, 0)?;
    let in_arr = obj_arg(args, 1)?;
    let in_off_i = match args.get(2) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let len_i = match args.get(3) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    let out_arr = obj_arg(args, 4)?;
    let out_off_i = match args.get(5) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    };
    if in_off_i < 0
        || len_i < 0
        || out_off_i < 0
        || (in_off_i as usize).saturating_add(len_i as usize) > ctx.array_length(in_arr)
    {
        return Err(bc_gost_throw_crypto_exception(
            ctx,
            "org/bouncycastle/crypto/DataLengthException",
            "input buffer too short",
        ));
    }
    if (out_off_i as usize).saturating_add(len_i as usize) > ctx.array_length(out_arr) {
        return Err(bc_gost_throw_crypto_exception(
            ctx,
            "org/bouncycastle/crypto/OutputLengthException",
            "output buffer too short",
        ));
    }

    let p_arr = match ctx.get_field_by_name(this, "P") {
        Value::Object(Some(o)) => o,
        _ => {
            return Err(RuntimeError::IllegalStateException {
                message: "VMPCEngine not initialised".into(),
            }
            .into())
        }
    };
    if ctx.array_length(p_arr) != 256 {
        return Err(RuntimeError::IllegalStateException {
            message: "VMPCEngine: malformed P".into(),
        }
        .into());
    }
    let mut p = vec![0u8; 256];
    ctx.read_byte_array_into(p_arr, 0, &mut p);
    let mut s = match ctx.get_field_by_name(this, "s") {
        Value::Int(v) => v as u8,
        _ => 0,
    };
    let mut n = match ctx.get_field_by_name(this, "n") {
        Value::Int(v) => v as u8,
        _ => 0,
    };
    let len = len_i as usize;
    let mut input = vec![0u8; len];
    ctx.read_byte_array_into(in_arr, in_off_i as usize, &mut input);
    let mut output = vec![0u8; len];

    for i in 0..len {
        let n_idx = n as usize;
        s = p[s.wrapping_add(p[n_idx]) as usize];
        let s_idx = s as usize;
        let z_idx = p[p[s_idx] as usize].wrapping_add(1) as usize;
        let z = p[z_idx];
        p.swap(n_idx, s_idx);
        n = n.wrapping_add(1);
        output[i] = input[i] ^ z;
    }

    ctx.write_byte_array_from(p_arr, 0, &p);
    ctx.set_field_by_name(this, "s", Value::Int((s as i8) as i32));
    ctx.set_field_by_name(this, "n", Value::Int((n as i8) as i32));
    ctx.write_byte_array_from(out_arr, out_off_i as usize, &output);
    Ok(Some(Value::Int(len_i)))
}

/// Native fast-path for the BouncyCastle ChaCha permutation kernels that
/// dominate SPHINCS-256 (`org.bouncycastle.pqc.crypto.test.RegressionTest`,
/// which otherwise times out >360 s vs HotSpot ~1.3 s). Intercepts the two
/// `public static` pure ChaCha cores:
///   * `ChaChaEngine.chachaCore(int rounds, int[] input, int[] x)` — the PRG
///     block function (`Seed.prg` → `Salsa20Engine.processBytes` →
///     `generateKeyStream`), the profiled hot leaf.
///   * `Permute.permute(int rounds, int[] x)` — the SPHINCS hash permutation
///     (`HashFunctions.hash_2n_n`/`hash_n_n`).
/// With `org/bouncycastle/*` JIT-banned, both run interpreted and their dozens
/// of per-block `Integers.rotateLeft` *method calls* crush the interpreter. The
/// Rust bodies (`crate::bc_chacha`) are verbatim, RFC 8439-validated ports.
/// Both are invoked via invokestatic, so the native registry shadows the
/// bytecode (`execute_invokestatic` `direct_native`). Length/odd-rounds guards
/// mirror BC's `IllegalArgumentException`s (defensive — never fire in the real
/// callers, which always pass length-16 arrays and even rounds).
pub fn register_bc_chacha(r: &mut NativeMethodRegistry) {
        
    // Read a Java int[16] into a Rust array; None when the length isn't 16
    // (→ caller raises IAE, matching BC's `x.length != 16` guard).
    fn read16(ctx: &dyn NativeContext, arr: ObjectRef) -> Option<[i32; 16]> {
        if ctx.array_length(arr) != 16 {
            return None;
        }
        let mut out = [0i32; 16];
        for k in 0..16 {
            match ctx.get_array_element(arr, k) {
                Value::Int(v) => out[k] = v,
                _ => return None,
            }
        }
        Some(out)
    }

    fn iae(msg: &str) -> MethodCallFailed {
        RuntimeError::IllegalArgumentException {
            message: msg.into(),
        }
        .into()
    }

    // One registration per class, in the former loops' order (wave 107, lane
    // L7: the null-argument wrapper needs the class as a constant).
    bc_register_null_rerun!(
        r,
        "org/bouncycastle/crypto/engines/Salsa20Engine",
        "processBytes",
        "([BII[BI)I",
        bc_salsa20_process_bytes,
    );
    bc_register_null_rerun!(
        r,
        "org/bouncycastle/crypto/engines/XSalsa20Engine",
        "processBytes",
        "([BII[BI)I",
        bc_salsa20_process_bytes,
    );
    bc_register_null_rerun!(
        r,
        "org/bouncycastle/crypto/engines/ChaChaEngine",
        "processBytes",
        "([BII[BI)I",
        bc_salsa20_process_bytes,
    );
    bc_register_null_rerun!(
        r,
        "org/bouncycastle/crypto/engines/ChaCha7539Engine",
        "processBytes",
        "([BII[BI)I",
        bc_salsa20_process_bytes,
    );
    bc_register_null_rerun!(
        r,
        "org/bouncycastle/crypto/engines/XChaCha20Engine",
        "processBytes",
        "([BII[BI)I",
        bc_salsa20_process_bytes,
    );

    bc_register_null_rerun!(
        r,
        "org/bouncycastle/crypto/engines/VMPCEngine",
        "processBytes",
        "([BII[BI)I",
        bc_vmpc_process_bytes,
    );
    bc_register_null_rerun!(
        r,
        "org/bouncycastle/crypto/engines/VMPCKSA3Engine",
        "processBytes",
        "([BII[BI)I",
        bc_vmpc_process_bytes,
    );

    // ChaChaEngine.chachaCore: x[i] = permute(input)_i + input[i]. `input` and
    // `x` are distinct arrays (engine state vs keystream buffer).
    bc_register_null_rerun!(
        r,
        "org/bouncycastle/crypto/engines/ChaChaEngine",
        "chachaCore",
        "(I[I[I)V",
        |ctx, args| {
            let rounds = match args.first() {
                Some(Value::Int(v)) => *v,
                _ => return Err(iae("chachaCore: missing rounds")),
            };
            let input_arr = obj_arg(args, 1)?;
            let x_arr = obj_arg(args, 2)?;
            // BC checks input.length, then x.length, then rounds parity.
            let input = read16(ctx, input_arr).ok_or_else(|| iae(""))?;
            if ctx.array_length(x_arr) != 16 {
                return Err(iae(""));
            }
            if rounds % 2 != 0 {
                return Err(iae("Number of rounds must be even"));
            }
            let mut x = [0i32; 16];
            crate::bc_chacha::chacha_core(rounds, &input, &mut x);
            for k in 0..16 {
                ctx.set_array_element(x_arr, k, Value::Int(x[k]));
            }
            Ok(None)
        },
    );

    bc_register_null_rerun!(
        r,
        "org/bouncycastle/crypto/engines/Salsa20Engine",
        "salsaCore",
        "(I[I[I)V",
        |ctx, args| {
            let rounds = match args.first() {
                Some(Value::Int(v)) => *v,
                _ => return Err(iae("salsaCore: missing rounds")),
            };
            let input_arr = obj_arg(args, 1)?;
            let x_arr = obj_arg(args, 2)?;
            let input = read16(ctx, input_arr).ok_or_else(|| iae(""))?;
            if ctx.array_length(x_arr) != 16 {
                return Err(iae(""));
            }
            if rounds % 2 != 0 {
                return Err(iae("Number of rounds must be even"));
            }
            let mut x = [0i32; 16];
            crate::bc_chacha::salsa_core(rounds, &input, &mut x);
            for k in 0..16 {
                ctx.set_array_element(x_arr, k, Value::Int(x[k]));
            }
            Ok(None)
        },
    );

    // Permute.permute: in-place permutation of x, no final input-add.
    bc_register_null_rerun!(
        r,
        "org/bouncycastle/pqc/crypto/sphincs/Permute",
        "permute",
        "(I[I)V",
        |ctx, args| {
            let rounds = match args.first() {
                Some(Value::Int(v)) => *v,
                _ => return Err(iae("permute: missing rounds")),
            };
            let x_arr = obj_arg(args, 1)?;
            let mut x = read16(ctx, x_arr).ok_or_else(|| iae(""))?;
            if rounds % 2 != 0 {
                return Err(iae("Number of rounds must be even"));
            }
            crate::bc_chacha::permute(rounds, &mut x);
            for k in 0..16 {
                ctx.set_array_element(x_arr, k, Value::Int(x[k]));
            }
            Ok(None)
        },
    );

    // Permute.chacha_permute(byte[] out, byte[] in): the SPHINCS hash leaf
    // (HashFunctions.hash_n_n/hash_2n_n), the dominant frame in tree/WOTS
    // signing. Folds in the per-call int[16] allocation + 32 Pack conversions
    // that the bytecode wraps around `permute`. Instance method (invokevirtual,
    // dispatched via the native-override check in execute_invokevirtual_cached):
    // args = [this (Permute), out, in]. `in` and `out` alias in the callers
    // (`chacha_permute(x, x)`); we read `in` fully before writing `out`.
    bc_register_rerun!(
        r,
        "org/bouncycastle/pqc/crypto/sphincs/Permute",
        "chacha_permute",
        "([B[B)V",
        |ctx, args| {
            let out_arr = obj_arg(args, 1)?;
            let in_arr = obj_arg(args, 2)?;
            // The bytecode reads in[0..64) and writes out[0..64); a buffer
            // shorter than 64 would AIOOBE in Pack — mirror that.
            let in_len = ctx.array_length(in_arr);
            let out_len = ctx.array_length(out_arr);
            if in_len < 64 || out_len < 64 {
                let index = if in_len < 64 { in_len } else { out_len } as i32;
                return Err(RuntimeError::aioobe_index_only(index).into());
            }
            let mut inb = [0u8; 64];
            if ctx.read_byte_array_into(in_arr, 0, &mut inb) != 64 {
                return Err(RuntimeError::aioobe_index_only(0).into());
            }
            let mut outb = [0u8; 64];
            crate::bc_chacha::chacha_permute_bytes(&mut outb, &inb);
            ctx.write_byte_array_from(out_arr, 0, &outb);
            Ok(None)
        },
    );

    // SPHINCS hash layer (HashFunctions instance methods, invokevirtual). Once
    // chacha_permute is native, the per-hash byte[64]/byte[32] allocations +
    // copy/XOR loops in hash_n_n/hash_2n_n dominate tree/WOTS signing (millions
    // of calls). Folding them native eliminates that glue. All return int 0
    // (BC). The byte-level logic lives in `crate::bc_chacha` (HotSpot-validated).
    fn ioff(args: &[Value], i: usize) -> usize {
        match args.get(i) {
            Some(Value::Int(v)) => *v as usize,
            _ => 0,
        }
    }
    const HF: &str = "org/bouncycastle/pqc/crypto/sphincs/HashFunctions";

    bc_register_null_rerun!(r, HF, "hash_n_n", "([BI[BI)I", |ctx, args| {
        let out = obj_arg(args, 1)?;
        let out_off = ioff(args, 2);
        let inp = obj_arg(args, 3)?;
        let in_off = ioff(args, 4);
        let mut in32 = [0u8; 32];
        if ctx.read_byte_array_into(inp, in_off, &mut in32) != 32 {
            return Err(iae(""));
        }
        let res = crate::bc_chacha::sphincs_hash_n_n(&in32);
        ctx.write_byte_array_from(out, out_off, &res);
        Ok(Some(Value::Int(0)))
    });

    bc_register_null_rerun!(r, HF, "hash_2n_n", "([BI[BI)I", |ctx, args| {
        let out = obj_arg(args, 1)?;
        let out_off = ioff(args, 2);
        let inp = obj_arg(args, 3)?;
        let in_off = ioff(args, 4);
        let mut in64 = [0u8; 64];
        if ctx.read_byte_array_into(inp, in_off, &mut in64) != 64 {
            return Err(iae(""));
        }
        let res = crate::bc_chacha::sphincs_hash_2n_n(&in64);
        ctx.write_byte_array_from(out, out_off, &res);
        Ok(Some(Value::Int(0)))
    });

    bc_register_null_rerun!(r, HF, "hash_n_n_mask", "([BI[BI[BI)I", |ctx, args| {
        let out = obj_arg(args, 1)?;
        let out_off = ioff(args, 2);
        let inp = obj_arg(args, 3)?;
        let in_off = ioff(args, 4);
        let mask = obj_arg(args, 5)?;
        let mask_off = ioff(args, 6);
        let mut in32 = [0u8; 32];
        let mut m32 = [0u8; 32];
        if ctx.read_byte_array_into(inp, in_off, &mut in32) != 32
            || ctx.read_byte_array_into(mask, mask_off, &mut m32) != 32
        {
            return Err(iae(""));
        }
        for i in 0..32 {
            in32[i] ^= m32[i];
        }
        let res = crate::bc_chacha::sphincs_hash_n_n(&in32);
        ctx.write_byte_array_from(out, out_off, &res);
        Ok(Some(Value::Int(0)))
    });

    bc_register_null_rerun!(r, HF, "hash_2n_n_mask", "([BI[BI[BI)I", |ctx, args| {
        let out = obj_arg(args, 1)?;
        let out_off = ioff(args, 2);
        let inp = obj_arg(args, 3)?;
        let in_off = ioff(args, 4);
        let mask = obj_arg(args, 5)?;
        let mask_off = ioff(args, 6);
        let mut in64 = [0u8; 64];
        let mut m64 = [0u8; 64];
        if ctx.read_byte_array_into(inp, in_off, &mut in64) != 64
            || ctx.read_byte_array_into(mask, mask_off, &mut m64) != 64
        {
            return Err(iae(""));
        }
        for i in 0..64 {
            in64[i] ^= m64[i];
        }
        let res = crate::bc_chacha::sphincs_hash_2n_n(&in64);
        ctx.write_byte_array_from(out, out_off, &res);
        Ok(Some(Value::Int(0)))
    });

    }

/// Native fast-path for the BouncyCastle NewHope (post-quantum) lattice kernels
/// that dominate `NewHopeTest` (1000 key-exchange rounds) once the ChaCha cores
/// are native: the number-theoretic transform `Poly.toNTT`/`fromNTT` (the
/// profiled hot frame — interpreted `short[]` Montgomery butterflies) and the
/// SHAKE128 rejection sampler `Poly.uniform` (interpreted Keccak). All are
/// `public static` over `short[1024]`; the bodies in `crate::bc_newhope` are
/// verbatim ports validated element-for-element against HotSpot. Invoked via
/// invokestatic (registry-shadowed). `short[]` reads/writes go through
/// `get/set_array_element` (sign-extended `Value::Int` <-> i16).
pub fn register_bc_newhope(r: &mut NativeMethodRegistry) {
        
    fn read_poly(ctx: &dyn NativeContext, arr: ObjectRef) -> Option<[i16; 1024]> {
        if ctx.array_length(arr) != 1024 {
            return None;
        }
        let mut out = [0i16; 1024];
        for (k, slot) in out.iter_mut().enumerate() {
            match ctx.get_array_element(arr, k) {
                Value::Int(v) => *slot = v as i16,
                _ => return None,
            }
        }
        Some(out)
    }
    fn write_poly(ctx: &dyn NativeContext, arr: ObjectRef, vals: &[i16; 1024]) {
        for (k, &v) in vals.iter().enumerate() {
            ctx.set_array_element(arr, k, Value::Int(v as i32));
        }
    }
    fn bad() -> MethodCallFailed {
        RuntimeError::aioobe_index_only(1024).into()
    }

    bc_register_rerun!(r, "org/bouncycastle/pqc/crypto/newhope/Poly", "toNTT", "([S)V", |ctx, args| {
        let arr = obj_arg(args, 0)?;
        let mut r = read_poly(ctx, arr).ok_or_else(bad)?;
        crate::bc_newhope::to_ntt(&mut r);
        write_poly(ctx, arr, &r);
        Ok(None)
    });

    bc_register_rerun!(r, "org/bouncycastle/pqc/crypto/newhope/Poly", "fromNTT", "([S)V", |ctx, args| {
        let arr = obj_arg(args, 0)?;
        let mut r = read_poly(ctx, arr).ok_or_else(bad)?;
        crate::bc_newhope::from_ntt(&mut r);
        write_poly(ctx, arr, &r);
        Ok(None)
    });

    bc_register_rerun!(r, "org/bouncycastle/pqc/crypto/newhope/Poly", "uniform", "([S[B)V", |ctx, args| {
        let a_arr = obj_arg(args, 0)?;
        if ctx.array_length(a_arr) != 1024 {
            return Err(bad());
        }
        let seed_arr = obj_arg(args, 1)?;
        let slen = ctx.array_length(seed_arr);
        let mut seed = vec![0u8; slen];
        ctx.read_byte_array_into(seed_arr, 0, &mut seed);
        let mut a = [0i16; 1024];
        crate::bc_newhope::uniform(&mut a, &seed);
        write_poly(ctx, a_arr, &a);
        Ok(None)
    });

    }

pub fn register_bc_cbc_block_cipher(r: &mut NativeMethodRegistry) {
        
    bc_register_null_rerun!(
        r,
        "org/bouncycastle/crypto/modes/CBCBlockCipher",
        "processBlock",
        "([BI[BI)I",
        |ctx, args| {
            // Interpreter round i5 wave 104, lane L7 (`i100-L7-native-shadow-
            // npe-texts`, CBC residue): every case this shadow cannot handle
            // is found here, before it writes anything or runs Java, and runs
            // the library's bytecode instead (AGENTS.md: real class bytes are
            // authoritative), which answers with BouncyCastle's own exception
            // and text (`in.length`'s NPE, the AIOOBE of the first array
            // access at a negative offset, the null `cipher`'s NPE after the
            // XOR loop, ...). The refusal below is kept only for a VM without
            // that bytecode.
            fn cbc_bytecode_or(
                ctx: &mut dyn NativeContext,
                args: &[Value],
                refusal: MethodCallFailed,
            ) -> MethodCallResult {
                // Wave 106 (lane L7): `CRATONVM_DBG_SHADOW_RERUN` evidence line.
                crate::lang_misc::note_shadow_rerun(
                    "org/bouncycastle/crypto/modes/CBCBlockCipher",
                    "processBlock",
                    "([BI[BI)I",
                    || format!("{refusal:?}"),
                );
                match ctx.invoke_special_bytecode_only(
                    "org/bouncycastle/crypto/modes/CBCBlockCipher",
                    "processBlock",
                    "([BI[BI)I",
                    args,
                ) {
                    Err(MethodCallFailed::InternalError(_)) => Err(refusal),
                    other => other,
                }
            }
            let mut this = obj_arg(args, 0)?;
            let (Some(Value::Object(Some(in_arr))), Some(Value::Object(Some(out_arr0)))) =
                (args.get(1).copied(), args.get(3).copied())
            else {
                return cbc_bytecode_or(
                    ctx,
                    args,
                    RuntimeError::NullPointerException { message: None }.into(),
                );
            };
            let mut out_arr = out_arr0;
            let in_off_i = match args.get(2) {
                Some(Value::Int(v)) => *v,
                _ => 0,
            };
            let out_off_i = match args.get(4) {
                Some(Value::Int(v)) => *v,
                _ => 0,
            };
            if in_off_i < 0 {
                return cbc_bytecode_or(ctx, args, RuntimeError::aioobe_index_only(in_off_i).into());
            }
            if out_off_i < 0 {
                return cbc_bytecode_or(ctx, args, RuntimeError::aioobe_index_only(out_off_i).into());
            }
            let in_off = in_off_i as usize;
            let out_off = out_off_i as usize;
            let state_refusal = |what: &str| -> MethodCallFailed {
                RuntimeError::IllegalStateException {
                    message: format!("CBCBlockCipher: {what}"),
                }
                .into()
            };
            let block_size = match ctx.get_field_by_name(this, "blockSize") {
                Value::Int(v) if v > 0 => v as usize,
                _ => return cbc_bytecode_or(ctx, args, state_refusal("malformed block size")),
            };
            let encrypting =
                matches!(ctx.get_field_by_name(this, "encrypting"), Value::Int(v) if v != 0);
            let cipher = match ctx.get_field_by_name(this, "cipher") {
                Value::Object(Some(o)) => o,
                _ => return cbc_bytecode_or(ctx, args, state_refusal("not initialised")),
            };
            let mut cbc_v_arr = match ctx.get_field_by_name(this, "cbcV") {
                Value::Object(Some(o)) => o,
                _ => return cbc_bytecode_or(ctx, args, state_refusal("missing cbcV")),
            };
            let mut cbc_next_v_arr = match ctx.get_field_by_name(this, "cbcNextV") {
                Value::Object(Some(o)) => o,
                _ => return cbc_bytecode_or(ctx, args, state_refusal("missing cbcNextV")),
            };

            if in_off.saturating_add(block_size) > ctx.array_length(in_arr) {
                return Err(bc_gost_throw_crypto_exception(
                    ctx,
                    "org/bouncycastle/crypto/DataLengthException",
                    "input buffer too short",
                ));
            }

            let is_aes = ctx
                .class_name_arc_of_id(ctx.class_id_of_object(cipher))
                .as_deref()
                == Some("org/bouncycastle/crypto/engines/AESEngine");
            let kw = if is_aes {
                match ctx.get_field_by_name(cipher, "WorkingKey") {
                    Value::Object(Some(wk)) => read_aes_kw(ctx, wk),
                    _ => AesKeySchedule::empty(),
                }
            } else {
                AesKeySchedule::empty()
            };
            let use_aes = block_size == 16 && kw.len() >= 2;

            if encrypting {
                let mut cbc_v = vec![0u8; block_size];
                ctx.read_byte_array_into(cbc_v_arr, 0, &mut cbc_v);
                let mut input = vec![0u8; block_size];
                ctx.read_byte_array_into(in_arr, in_off, &mut input);
                for i in 0..block_size {
                    cbc_v[i] ^= input[i];
                }

                if use_aes {
                    if out_off.saturating_add(block_size) > ctx.array_length(out_arr) {
                        return Err(bc_gost_throw_crypto_exception(
                            ctx,
                            "org/bouncycastle/crypto/OutputLengthException",
                            "output buffer too short",
                        ));
                    }
                    let mut block = [0u8; 16];
                    block.copy_from_slice(&cbc_v[..16]);
                    let mut out_block = [0u8; 16];
                    crate::bc_aes::encrypt_block(&kw, &block, &mut out_block);
                    ctx.write_byte_array_from(out_arr, out_off, &out_block);
                    ctx.write_byte_array_from(cbc_v_arr, 0, &out_block);
                    return Ok(Some(Value::Int(16)));
                }

                ctx.write_byte_array_from(cbc_v_arr, 0, &cbc_v);
                // gc-common w20-e: a non-AES engine's `processBlock` is Java;
                // the output and chaining arrays are read and written at
                // their current addresses after it, not their entry ones.
                let out_pin = ctx.pin_native_root(out_arr);
                let v_pin = ctx.pin_native_root(cbc_v_arr);
                let result = ctx.invoke_virtual(
                    cipher,
                    "processBlock",
                    "([BI[BI)I",
                    &[
                        Value::Object(Some(cbc_v_arr)),
                        Value::Int(0),
                        Value::Object(Some(out_arr)),
                        Value::Int(out_off_i),
                    ],
                )?;
                let out_arr = ctx.read_native_pin(out_pin, out_arr);
                let cbc_v_arr = ctx.read_native_pin(v_pin, cbc_v_arr);
                ctx.unpin_native_roots(out_pin);
                let mut out_block = vec![0u8; block_size];
                ctx.read_byte_array_into(out_arr, out_off, &mut out_block);
                ctx.write_byte_array_from(cbc_v_arr, 0, &out_block);
                return Ok(result.or(Some(Value::Int(block_size as i32))));
            }

            if out_off.saturating_add(block_size) > ctx.array_length(out_arr) && use_aes {
                return Err(bc_gost_throw_crypto_exception(
                    ctx,
                    "org/bouncycastle/crypto/OutputLengthException",
                    "output buffer too short",
                ));
            }

            let mut input = vec![0u8; block_size];
            ctx.read_byte_array_into(in_arr, in_off, &mut input);
            ctx.write_byte_array_from(cbc_next_v_arr, 0, &input);

            let result = if use_aes {
                let mut block = [0u8; 16];
                block.copy_from_slice(&input[..16]);
                let mut out_block = [0u8; 16];
                crate::bc_aes::decrypt_block(&kw, &block, &mut out_block);
                ctx.write_byte_array_from(out_arr, out_off, &out_block);
                Some(Value::Int(16))
            } else {
                // gc-common w20-e: as in the encrypting arm, every array and
                // the receiver are used after this Java call; re-read them.
                let this_pin = ctx.pin_native_root(this);
                let out_pin = ctx.pin_native_root(out_arr);
                let v_pin = ctx.pin_native_root(cbc_v_arr);
                let next_pin = ctx.pin_native_root(cbc_next_v_arr);
                let result = ctx.invoke_virtual(
                    cipher,
                    "processBlock",
                    "([BI[BI)I",
                    &[
                        Value::Object(Some(in_arr)),
                        Value::Int(in_off_i),
                        Value::Object(Some(out_arr)),
                        Value::Int(out_off_i),
                    ],
                )?;
                this = ctx.read_native_pin(this_pin, this);
                out_arr = ctx.read_native_pin(out_pin, out_arr);
                cbc_v_arr = ctx.read_native_pin(v_pin, cbc_v_arr);
                cbc_next_v_arr = ctx.read_native_pin(next_pin, cbc_next_v_arr);
                ctx.unpin_native_roots(this_pin);
                result
            };

            let mut cbc_v = vec![0u8; block_size];
            ctx.read_byte_array_into(cbc_v_arr, 0, &mut cbc_v);
            let mut out_block = vec![0u8; block_size];
            ctx.read_byte_array_into(out_arr, out_off, &mut out_block);
            for i in 0..block_size {
                out_block[i] ^= cbc_v[i];
            }
            ctx.write_byte_array_from(out_arr, out_off, &out_block);
            ctx.set_field_by_name(this, "cbcV", Value::Object(Some(cbc_next_v_arr)));
            ctx.set_field_by_name(this, "cbcNextV", Value::Object(Some(cbc_v_arr)));

            Ok(result.or(Some(Value::Int(block_size as i32))))
        },
    );

    }

pub fn bc_pack_offset_arg(args: &[Value], idx: usize) -> Result<usize, MethodCallFailed> {
    match args.get(idx) {
        Some(Value::Int(v)) if *v >= 0 => Ok(*v as usize),
        Some(Value::Int(v)) => Err(RuntimeError::aioobe_index_only(*v).into()),
        _ => Err(RuntimeError::aioobe_index_only(-1).into()),
    }
}

pub fn bc_pack_int_arg(args: &[Value], idx: usize) -> i32 {
    match args.get(idx) {
        Some(Value::Int(v)) => *v,
        _ => 0,
    }
}

pub fn bc_pack_check_range(
    ctx: &dyn NativeContext,
    arr: ObjectRef,
    off: usize,
    len: usize,
) -> Result<(), MethodCallFailed> {
    let end = match off.checked_add(len) {
        Some(v) => v,
        None => return Err(RuntimeError::aioobe_index_only(i32::MAX).into()),
    };
    if ctx.array_length(arr) < end {
        return Err(RuntimeError::aioobe_index_only(end as i32).into());
    }
    Ok(())
}

pub fn bc_pack_read_be_i32(
    ctx: &dyn NativeContext,
    arr: ObjectRef,
    off: usize,
) -> Result<i32, MethodCallFailed> {
    bc_pack_check_range(ctx, arr, off, 4)?;
    let mut bytes = [0u8; 4];
    ctx.read_byte_array_into(arr, off, &mut bytes);
    Ok(i32::from_be_bytes(bytes))
}

pub fn bc_pack_read_le_i32(
    ctx: &dyn NativeContext,
    arr: ObjectRef,
    off: usize,
) -> Result<i32, MethodCallFailed> {
    bc_pack_check_range(ctx, arr, off, 4)?;
    let mut bytes = [0u8; 4];
    ctx.read_byte_array_into(arr, off, &mut bytes);
    Ok(i32::from_le_bytes(bytes))
}

pub fn bc_pack_write_be_i32(
    ctx: &mut dyn NativeContext,
    arr: ObjectRef,
    off: usize,
    value: i32,
) -> Result<(), MethodCallFailed> {
    bc_pack_check_range(ctx, arr, off, 4)?;
    ctx.write_byte_array_from(arr, off, &value.to_be_bytes());
    Ok(())
}

pub fn bc_pack_write_le_i32(
    ctx: &mut dyn NativeContext,
    arr: ObjectRef,
    off: usize,
    value: i32,
) -> Result<(), MethodCallFailed> {
    bc_pack_check_range(ctx, arr, off, 4)?;
    ctx.write_byte_array_from(arr, off, &value.to_le_bytes());
    Ok(())
}

pub fn bc_pack_check_int_array(
    ctx: &dyn NativeContext,
    arr: ObjectRef,
    off: usize,
    len: usize,
) -> Result<(), MethodCallFailed> {
    let end = match off.checked_add(len) {
        Some(v) => v,
        None => return Err(RuntimeError::aioobe_index_only(i32::MAX).into()),
    };
    if ctx.array_length(arr) < end {
        return Err(RuntimeError::aioobe_index_only(end as i32).into());
    }
    Ok(())
}

pub fn register_bc_pack_helpers(r: &mut NativeMethodRegistry) {
        

    bc_register_rerun!(r, "org/bouncycastle/util/Pack", "bigEndianToInt", "([BI[I)V", |ctx, args| {
        let bs = obj_arg(args, 0)?;
        let mut b_off = bc_pack_offset_arg(args, 1)?;
        let ns = obj_arg(args, 2)?;
        let n_len = ctx.array_length(ns);
        bc_pack_check_range(ctx, bs, b_off, n_len.saturating_mul(4))?;
        for i in 0..n_len {
            let word = bc_pack_read_be_i32(ctx, bs, b_off)?;
            ctx.set_array_element(ns, i, Value::Int(word));
            b_off += 4;
        }
        Ok(None)
    });
    bc_register_rerun!(r, "org/bouncycastle/util/Pack", "littleEndianToInt", "([BI[I)V", |ctx, args| {
        let bs = obj_arg(args, 0)?;
        let mut b_off = bc_pack_offset_arg(args, 1)?;
        let ns = obj_arg(args, 2)?;
        let n_len = ctx.array_length(ns);
        bc_pack_check_range(ctx, bs, b_off, n_len.saturating_mul(4))?;
        for i in 0..n_len {
            let word = bc_pack_read_le_i32(ctx, bs, b_off)?;
            ctx.set_array_element(ns, i, Value::Int(word));
            b_off += 4;
        }
        Ok(None)
    });

    bc_register_rerun!(r, "org/bouncycastle/util/Pack", "bigEndianToInt", "([BI[III)V", |ctx, args| {
        let bs = obj_arg(args, 0)?;
        let mut b_off = bc_pack_offset_arg(args, 1)?;
        let ns = obj_arg(args, 2)?;
        let n_off = bc_pack_offset_arg(args, 3)?;
        let count = bc_pack_offset_arg(args, 4)?;
        bc_pack_check_int_array(ctx, ns, n_off, count)?;
        bc_pack_check_range(ctx, bs, b_off, count.saturating_mul(4))?;
        for i in 0..count {
            let word = bc_pack_read_be_i32(ctx, bs, b_off)?;
            ctx.set_array_element(ns, n_off + i, Value::Int(word));
            b_off += 4;
        }
        Ok(None)
    });
    bc_register_rerun!(r, "org/bouncycastle/util/Pack", "littleEndianToInt", "([BI[III)V", |ctx, args| {
        let bs = obj_arg(args, 0)?;
        let mut b_off = bc_pack_offset_arg(args, 1)?;
        let ns = obj_arg(args, 2)?;
        let n_off = bc_pack_offset_arg(args, 3)?;
        let count = bc_pack_offset_arg(args, 4)?;
        bc_pack_check_int_array(ctx, ns, n_off, count)?;
        bc_pack_check_range(ctx, bs, b_off, count.saturating_mul(4))?;
        for i in 0..count {
            let word = bc_pack_read_le_i32(ctx, bs, b_off)?;
            ctx.set_array_element(ns, n_off + i, Value::Int(word));
            b_off += 4;
        }
        Ok(None)
    });


    bc_register_rerun!(r, "org/bouncycastle/util/Pack", "intToBigEndian", "([I[BI)V", |ctx, args| {
        let ns = obj_arg(args, 0)?;
        let bs = obj_arg(args, 1)?;
        let mut b_off = bc_pack_offset_arg(args, 2)?;
        let n_len = ctx.array_length(ns);
        bc_pack_check_range(ctx, bs, b_off, n_len.saturating_mul(4))?;
        for i in 0..n_len {
            let value = match ctx.get_array_element(ns, i) {
                Value::Int(v) => v,
                _ => 0,
            };
            bc_pack_write_be_i32(ctx, bs, b_off, value)?;
            b_off += 4;
        }
        Ok(None)
    });
    bc_register_rerun!(r, "org/bouncycastle/util/Pack", "intToLittleEndian", "([I[BI)V", |ctx, args| {
        let ns = obj_arg(args, 0)?;
        let bs = obj_arg(args, 1)?;
        let mut b_off = bc_pack_offset_arg(args, 2)?;
        let n_len = ctx.array_length(ns);
        bc_pack_check_range(ctx, bs, b_off, n_len.saturating_mul(4))?;
        for i in 0..n_len {
            let value = match ctx.get_array_element(ns, i) {
                Value::Int(v) => v,
                _ => 0,
            };
            bc_pack_write_le_i32(ctx, bs, b_off, value)?;
            b_off += 4;
        }
        Ok(None)
    });

    bc_register_rerun!(r, "org/bouncycastle/util/Pack", "intToBigEndian", "([III[BI)V", |ctx, args| {
        let ns = obj_arg(args, 0)?;
        let n_off = bc_pack_offset_arg(args, 1)?;
        let count = bc_pack_offset_arg(args, 2)?;
        let bs = obj_arg(args, 3)?;
        let mut b_off = bc_pack_offset_arg(args, 4)?;
        bc_pack_check_int_array(ctx, ns, n_off, count)?;
        bc_pack_check_range(ctx, bs, b_off, count.saturating_mul(4))?;
        for i in 0..count {
            let value = match ctx.get_array_element(ns, n_off + i) {
                Value::Int(v) => v,
                _ => 0,
            };
            bc_pack_write_be_i32(ctx, bs, b_off, value)?;
            b_off += 4;
        }
        Ok(None)
    });
    bc_register_rerun!(r, "org/bouncycastle/util/Pack", "intToLittleEndian", "([III[BI)V", |ctx, args| {
        let ns = obj_arg(args, 0)?;
        let n_off = bc_pack_offset_arg(args, 1)?;
        let count = bc_pack_offset_arg(args, 2)?;
        let bs = obj_arg(args, 3)?;
        let mut b_off = bc_pack_offset_arg(args, 4)?;
        bc_pack_check_int_array(ctx, ns, n_off, count)?;
        bc_pack_check_range(ctx, bs, b_off, count.saturating_mul(4))?;
        for i in 0..count {
            let value = match ctx.get_array_element(ns, n_off + i) {
                Value::Int(v) => v,
                _ => 0,
            };
            bc_pack_write_le_i32(ctx, bs, b_off, value)?;
            b_off += 4;
        }
        Ok(None)
    });

    }

pub fn bc_x25519_scalar_mult(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let k_arr = obj_arg(args, 0)?;
    let k_off = bc_pack_offset_arg(args, 1)?;
    let u_arr = obj_arg(args, 2)?;
    let u_off = bc_pack_offset_arg(args, 3)?;
    let r_arr = obj_arg(args, 4)?;
    let r_off = bc_pack_offset_arg(args, 5)?;
    bc_pack_check_range(ctx, k_arr, k_off, 32)?;
    bc_pack_check_range(ctx, u_arr, u_off, 32)?;
    bc_pack_check_range(ctx, r_arr, r_off, 32)?;

    let mut k = [0u8; 32];
    let mut u = [0u8; 32];
    if ctx.read_byte_array_into(k_arr, k_off, &mut k) != 32
        || ctx.read_byte_array_into(u_arr, u_off, &mut u) != 32
    {
        return Err(RuntimeError::aioobe_index_only(-1).into());
    }
    let out = curve25519_dalek::MontgomeryPoint(u).mul_clamped(k);
    if !ctx.write_byte_array_from(r_arr, r_off, out.as_bytes()) {
        return Err(RuntimeError::aioobe_index_only(-1).into());
    }
    Ok(None)
}

pub fn bc_x25519_scalar_mult_base(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let k_arr = obj_arg(args, 0)?;
    let k_off = bc_pack_offset_arg(args, 1)?;
    let r_arr = obj_arg(args, 2)?;
    let r_off = bc_pack_offset_arg(args, 3)?;
    bc_pack_check_range(ctx, k_arr, k_off, 32)?;
    bc_pack_check_range(ctx, r_arr, r_off, 32)?;

    let mut k = [0u8; 32];
    if ctx.read_byte_array_into(k_arr, k_off, &mut k) != 32 {
        return Err(RuntimeError::aioobe_index_only(-1).into());
    }
    let out = curve25519_dalek::MontgomeryPoint::mul_base_clamped(k);
    if !ctx.write_byte_array_from(r_arr, r_off, out.as_bytes()) {
        return Err(RuntimeError::aioobe_index_only(-1).into());
    }
    Ok(None)
}

pub fn register_bc_x25519_field(r: &mut NativeMethodRegistry) {
            bc_register_rerun!(
        r,
        "org/bouncycastle/math/ec/rfc7748/X25519",
        "scalarMult",
        "([BI[BI[BI)V",
        bc_x25519_scalar_mult,
    );
    bc_register_rerun!(
        r,
        "org/bouncycastle/math/ec/rfc7748/X25519",
        "scalarMultBase",
        "([BI[BI)V",
        bc_x25519_scalar_mult_base,
    );
    }

pub fn bc_ed25519_generate_public_key(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let sk_arr = obj_arg(args, 0)?;
    let sk_off = bc_pack_offset_arg(args, 1)?;
    let pk_arr = obj_arg(args, 2)?;
    let pk_off = bc_pack_offset_arg(args, 3)?;
    bc_pack_check_range(ctx, sk_arr, sk_off, 32)?;
    bc_pack_check_range(ctx, pk_arr, pk_off, 32)?;

    let mut sk = [0u8; 32];
    if ctx.read_byte_array_into(sk_arr, sk_off, &mut sk) != 32 {
        return Err(RuntimeError::aioobe_index_only(-1).into());
    }
    let pk = ed25519_dalek::SigningKey::from_bytes(&sk)
        .verifying_key()
        .to_bytes();
    if !ctx.write_byte_array_from(pk_arr, pk_off, &pk) {
        return Err(RuntimeError::aioobe_index_only(-1).into());
    }
    Ok(None)
}

pub fn bc_ed25519_sign_impl(
    ctx: &mut dyn NativeContext,
    sk_arr: ObjectRef,
    sk_off: usize,
    m_arr: ObjectRef,
    m_off: usize,
    m_len: usize,
    sig_arr: ObjectRef,
    sig_off: usize,
) -> MethodCallResult {
    use ed25519_dalek::Signer;

    bc_pack_check_range(ctx, sk_arr, sk_off, 32)?;
    bc_pack_check_range(ctx, m_arr, m_off, m_len)?;
    bc_pack_check_range(ctx, sig_arr, sig_off, 64)?;

    let mut sk = [0u8; 32];
    if ctx.read_byte_array_into(sk_arr, sk_off, &mut sk) != 32 {
        return Err(RuntimeError::aioobe_index_only(-1).into());
    }
    let mut m = vec![0u8; m_len];
    if m_len > 0 && ctx.read_byte_array_into(m_arr, m_off, &mut m) != m_len {
        return Err(RuntimeError::aioobe_index_only(-1).into());
    }
    let sig = ed25519_dalek::SigningKey::from_bytes(&sk)
        .sign(&m)
        .to_bytes();
    if !ctx.write_byte_array_from(sig_arr, sig_off, &sig) {
        return Err(RuntimeError::aioobe_index_only(-1).into());
    }
    Ok(None)
}

pub fn bc_ed25519_sign(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    bc_ed25519_sign_impl(
        ctx,
        obj_arg(args, 0)?,
        bc_pack_offset_arg(args, 1)?,
        obj_arg(args, 2)?,
        bc_pack_offset_arg(args, 3)?,
        bc_pack_offset_arg(args, 4)?,
        obj_arg(args, 5)?,
        bc_pack_offset_arg(args, 6)?,
    )
}

pub fn bc_ed25519_sign_with_pk(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let pk_arr = obj_arg(args, 2)?;
    let pk_off = bc_pack_offset_arg(args, 3)?;
    bc_pack_check_range(ctx, pk_arr, pk_off, 32)?;
    bc_ed25519_sign_impl(
        ctx,
        obj_arg(args, 0)?,
        bc_pack_offset_arg(args, 1)?,
        obj_arg(args, 4)?,
        bc_pack_offset_arg(args, 5)?,
        bc_pack_offset_arg(args, 6)?,
        obj_arg(args, 7)?,
        bc_pack_offset_arg(args, 8)?,
    )
}

pub fn bc_ed25519_verify(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let sig_arr = obj_arg(args, 0)?;
    let sig_off = bc_pack_offset_arg(args, 1)?;
    let pk_arr = obj_arg(args, 2)?;
    let pk_off = bc_pack_offset_arg(args, 3)?;
    let m_arr = obj_arg(args, 4)?;
    let m_off = bc_pack_offset_arg(args, 5)?;
    let m_len = bc_pack_offset_arg(args, 6)?;
    bc_pack_check_range(ctx, sig_arr, sig_off, 64)?;
    bc_pack_check_range(ctx, pk_arr, pk_off, 32)?;
    bc_pack_check_range(ctx, m_arr, m_off, m_len)?;

    let mut sig_bytes = [0u8; 64];
    let mut pk_bytes = [0u8; 32];
    if ctx.read_byte_array_into(sig_arr, sig_off, &mut sig_bytes) != 64
        || ctx.read_byte_array_into(pk_arr, pk_off, &mut pk_bytes) != 32
    {
        return Err(RuntimeError::aioobe_index_only(-1).into());
    }
    let mut m = vec![0u8; m_len];
    if m_len > 0 && ctx.read_byte_array_into(m_arr, m_off, &mut m) != m_len {
        return Err(RuntimeError::aioobe_index_only(-1).into());
    }
    let Ok(vk) = ed25519_dalek::VerifyingKey::from_bytes(&pk_bytes) else {
        return Ok(Some(Value::Int(0)));
    };
    let sig = ed25519_dalek::Signature::from_bytes(&sig_bytes);
    let ok = vk.verify_strict(&m, &sig).is_ok();
    Ok(Some(Value::Int(if ok { 1 } else { 0 })))
}

pub fn register_bc_ed25519(r: &mut NativeMethodRegistry) {
            bc_register_rerun!(
        r,
        "org/bouncycastle/math/ec/rfc8032/Ed25519",
        "generatePublicKey",
        "([BI[BI)V",
        bc_ed25519_generate_public_key,
    );
    bc_register_rerun!(
        r,
        "org/bouncycastle/math/ec/rfc8032/Ed25519",
        "sign",
        "([BI[BII[BI)V",
        bc_ed25519_sign,
    );
    bc_register_rerun!(
        r,
        "org/bouncycastle/math/ec/rfc8032/Ed25519",
        "sign",
        "([BI[BI[BII[BI)V",
        bc_ed25519_sign_with_pk,
    );
    bc_register_rerun!(
        r,
        "org/bouncycastle/math/ec/rfc8032/Ed25519",
        "verify",
        "([BI[BI[BII)Z",
        bc_ed25519_verify,
    );
    }

pub const BC_BLAKE2S_IV: [u32; 8] = [
    0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19,
];

pub const BC_BLAKE2S_SIGMA: [[usize; 16]; 10] = [
    [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15],
    [14, 10, 4, 8, 9, 15, 13, 6, 1, 12, 0, 2, 11, 7, 5, 3],
    [11, 8, 12, 0, 5, 2, 15, 13, 10, 14, 3, 6, 7, 1, 9, 4],
    [7, 9, 3, 1, 13, 12, 11, 14, 2, 6, 5, 10, 4, 0, 15, 8],
    [9, 0, 5, 7, 2, 4, 10, 15, 14, 1, 11, 12, 6, 8, 3, 13],
    [2, 12, 6, 10, 0, 11, 8, 3, 4, 13, 7, 5, 15, 14, 1, 9],
    [12, 5, 1, 15, 14, 13, 4, 10, 0, 7, 6, 3, 9, 2, 8, 11],
    [13, 11, 7, 14, 12, 1, 3, 9, 5, 0, 15, 4, 8, 6, 2, 10],
    [6, 15, 14, 9, 11, 3, 0, 8, 12, 2, 13, 7, 1, 4, 10, 5],
    [10, 2, 8, 4, 7, 6, 1, 5, 15, 11, 9, 14, 3, 12, 13, 0],
];

pub fn bc_blake2s_bad_state(message: &str) -> MethodCallFailed {
    RuntimeError::IllegalStateException {
        message: message.into(),
    }
    .into()
}

pub fn bc_blake2s_int_field(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    name: &str,
) -> Result<u32, MethodCallFailed> {
    match ctx.get_field_by_name(this, name) {
        Value::Int(v) => Ok(v as u32),
        _ => Err(bc_blake2s_bad_state("Blake2sDigest: malformed int field")),
    }
}

pub fn bc_blake2s_int_array_field(
    ctx: &dyn NativeContext,
    this: ObjectRef,
    name: &str,
    min_len: usize,
) -> Result<ObjectRef, MethodCallFailed> {
    let arr = match ctx.get_field_by_name(this, name) {
        Value::Object(Some(o)) => o,
        _ => return Err(bc_blake2s_bad_state("Blake2sDigest: missing array field")),
    };
    if ctx.array_length(arr) < min_len {
        return Err(RuntimeError::aioobe_index_only(min_len as i32).into());
    }
    Ok(arr)
}

pub fn bc_blake2s_read_int_array<const N: usize>(
    ctx: &dyn NativeContext,
    arr: ObjectRef,
    label: &str,
) -> Result<[u32; N], MethodCallFailed> {
    let mut out = [0u32; N];
    for (i, slot) in out.iter_mut().enumerate() {
        match ctx.get_array_element(arr, i) {
            Value::Int(v) => *slot = v as u32,
            _ => {
                return Err(RuntimeError::IllegalStateException {
                    message: format!("Blake2sDigest: malformed {label}"),
                }
                .into())
            }
        }
    }
    Ok(out)
}

pub fn bc_blake2s_write_int_array(ctx: &dyn NativeContext, arr: ObjectRef, words: &[u32]) {
    for (i, &word) in words.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(word as i32));
    }
}

pub fn bc_blake2s_g(
    state: &mut [u32; 16],
    m1: u32,
    m2: u32,
    a: usize,
    b: usize,
    c: usize,
    d: usize,
) {
    state[a] = state[a].wrapping_add(state[b]).wrapping_add(m1);
    state[d] = (state[d] ^ state[a]).rotate_right(16);
    state[c] = state[c].wrapping_add(state[d]);
    state[b] = (state[b] ^ state[c]).rotate_right(12);
    state[a] = state[a].wrapping_add(state[b]).wrapping_add(m2);
    state[d] = (state[d] ^ state[a]).rotate_right(8);
    state[c] = state[c].wrapping_add(state[d]);
    state[b] = (state[b] ^ state[c]).rotate_right(7);
}

pub fn register_bc_blake2s_digest(r: &mut NativeMethodRegistry) {
        
    bc_register_rerun!(
        r,
        "org/bouncycastle/crypto/digests/Blake2sDigest",
        "compress",
        "([BI)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let message_arr = obj_arg(args, 1)?;
            let message_pos = match args.get(2) {
                Some(Value::Int(v)) if *v >= 0 => *v as usize,
                Some(Value::Int(v)) => return Err(RuntimeError::aioobe_index_only(*v).into()),
                _ => return Err(RuntimeError::aioobe_index_only(-1).into()),
            };
            let end = match message_pos.checked_add(64) {
                Some(v) => v,
                None => return Err(RuntimeError::aioobe_index_only(i32::MAX).into()),
            };
            if ctx.array_length(message_arr) < end {
                return Err(RuntimeError::aioobe_index_only(end as i32).into());
            }

            let chain_arr = bc_blake2s_int_array_field(ctx, this, "chainValue", 8)?;
            let state_arr = bc_blake2s_int_array_field(ctx, this, "internalState", 16)?;
            let mut chain = bc_blake2s_read_int_array::<8>(ctx, chain_arr, "chainValue")?;
            let t0 = bc_blake2s_int_field(ctx, this, "t0")?;
            let t1 = bc_blake2s_int_field(ctx, this, "t1")?;
            let f0 = bc_blake2s_int_field(ctx, this, "f0")?;
            let f1 = bc_blake2s_int_field(ctx, this, "f1")?;

            let mut block = [0u8; 64];
            ctx.read_byte_array_into(message_arr, message_pos, &mut block);
            let mut m = [0u32; 16];
            for (i, word) in m.iter_mut().enumerate() {
                let off = i * 4;
                *word = u32::from_le_bytes([
                    block[off],
                    block[off + 1],
                    block[off + 2],
                    block[off + 3],
                ]);
            }

            let mut state = [0u32; 16];
            state[..8].copy_from_slice(&chain);
            state[8..12].copy_from_slice(&BC_BLAKE2S_IV[..4]);
            state[12] = t0 ^ BC_BLAKE2S_IV[4];
            state[13] = t1 ^ BC_BLAKE2S_IV[5];
            state[14] = f0 ^ BC_BLAKE2S_IV[6];
            state[15] = f1 ^ BC_BLAKE2S_IV[7];

            for sigma in BC_BLAKE2S_SIGMA {
                bc_blake2s_g(&mut state, m[sigma[0]], m[sigma[1]], 0, 4, 8, 12);
                bc_blake2s_g(&mut state, m[sigma[2]], m[sigma[3]], 1, 5, 9, 13);
                bc_blake2s_g(&mut state, m[sigma[4]], m[sigma[5]], 2, 6, 10, 14);
                bc_blake2s_g(&mut state, m[sigma[6]], m[sigma[7]], 3, 7, 11, 15);
                bc_blake2s_g(&mut state, m[sigma[8]], m[sigma[9]], 0, 5, 10, 15);
                bc_blake2s_g(&mut state, m[sigma[10]], m[sigma[11]], 1, 6, 11, 12);
                bc_blake2s_g(&mut state, m[sigma[12]], m[sigma[13]], 2, 7, 8, 13);
                bc_blake2s_g(&mut state, m[sigma[14]], m[sigma[15]], 3, 4, 9, 14);
            }

            for i in 0..8 {
                chain[i] ^= state[i] ^ state[i + 8];
            }
            bc_blake2s_write_int_array(ctx, chain_arr, &chain);
            bc_blake2s_write_int_array(ctx, state_arr, &state);
            Ok(None)
        },
    );

    bc_register_rerun!(
        r,
        "org/bouncycastle/crypto/digests/Blake2sDigest",
        "G",
        "(IIIIII)V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let m1 = match args.get(1) {
                Some(Value::Int(v)) => *v as u32,
                _ => 0,
            };
            let m2 = match args.get(2) {
                Some(Value::Int(v)) => *v as u32,
                _ => 0,
            };
            let pos = |idx: usize| -> Result<usize, MethodCallFailed> {
                match args.get(idx) {
                    Some(Value::Int(v)) if (0..16).contains(v) => Ok(*v as usize),
                    Some(Value::Int(v)) => Err(RuntimeError::aioobe_index_only(*v).into()),
                    _ => Err(RuntimeError::aioobe_index_only(-1).into()),
                }
            };
            let pos_a = pos(3)?;
            let pos_b = pos(4)?;
            let pos_c = pos(5)?;
            let pos_d = pos(6)?;
            let state_arr = match ctx.get_field_by_name(this, "internalState") {
                Value::Object(Some(o)) => o,
                _ => {
                    return Err(RuntimeError::IllegalStateException {
                        message: "Blake2sDigest.G: missing internalState".into(),
                    }
                    .into())
                }
            };
            if ctx.array_length(state_arr) < 16 {
                return Err(RuntimeError::aioobe_index_only(16).into());
            }
            let mut state = [0u32; 16];
            for (i, slot) in state.iter_mut().enumerate() {
                match ctx.get_array_element(state_arr, i) {
                    Value::Int(v) => *slot = v as u32,
                    _ => {
                        return Err(RuntimeError::IllegalStateException {
                            message: "Blake2sDigest.G: malformed internalState".into(),
                        }
                        .into())
                    }
                }
            }

            bc_blake2s_g(&mut state, m1, m2, pos_a, pos_b, pos_c, pos_d);

            for (i, &word) in state.iter().enumerate() {
                ctx.set_array_element(state_arr, i, Value::Int(word as i32));
            }
            Ok(None)
        },
    );

    }

/// The eight chaining words `H1..H8`, in order, plus the two remaining fields
/// `processBlock` touches.
pub const BC_SHA256_STATE_FIELDS: [&str; 8] = ["H1", "H2", "H3", "H4", "H5", "H6", "H7", "H8"];

/// Resolved heap slot indices for one `SHA256Digest` class: `(H1..H8, X, xOff)`,
/// each paired with the descriptor byte the class declares for it.
///
/// The descriptors are carried, not assumed, because the batch accessors
/// (`get_fields_typed` / `set_fields_typed`) take the descriptor from the caller
/// and use it to select the coercion: handing over a WRONG one decodes or stores
/// the slot as the wrong type, exactly as if the class had declared it that way.
/// Reading them out of the same `declared_fields` metadata the indices came from
/// is what makes that safe.
#[derive(Clone, Copy)]
pub struct BcSha256Slots {
    /// `(slot, descriptor)` for `H1..H8`, in order, ready to hand to the batch
    /// accessors without rebuilding the array per call.
    pub h: [(usize, u8); 8],
    pub x: usize,
    pub x_off: (usize, u8),
}

/// Slot cache, keyed by the receiver's `ClassId`.
///
/// `get_field_by_name` takes the class-manager read lock and walks the class
/// hierarchy by name on every call; `processBlock` touches ten fields and is
/// called once per 64-byte block, so paying that eighteen times per block would
/// cost more than the bytecode this native replaces. The indices are a property
/// of the class layout, so they are resolved once and reused.
///
/// Keyed on `ClassId` rather than cached unconditionally because the same class
/// name can be loaded by two class loaders (two `ClassId`s, two layouts); a
/// mismatch simply re-resolves rather than reading the wrong slots.
/// LOCK LEVEL (lock-discipline ratchet): `Scratch`. Both acquisitions copy a
/// `Copy` payload out in the same statement; every `NativeContext` call in
/// `bc_sha256_slots` (`class_id_of_object`, `declared_fields`) runs with no
/// guard held.
pub static BC_SHA256_SLOTS: cratonvm_types::lock_order::OrderedPlRwLock<Option<((usize, u32), BcSha256Slots)>> =
    cratonvm_types::lock_order::OrderedPlRwLock::new(
        None,
        cratonvm_types::lock_order::LockLevel::Scratch,
    );

pub fn bc_sha256_slots(
    ctx: &dyn NativeContext,
    this: ObjectRef,
) -> Result<BcSha256Slots, MethodCallFailed> {
    let class_id = ctx.class_id_of_object(this);
    let key = (ctx.vm_identity(), class_id.as_u32());
    if let Some((cached_key, slots)) = *BC_SHA256_SLOTS.read() {
        if cached_key == key {
            return Ok(slots);
        }
    }
    let bad = |what: &str| -> MethodCallFailed {
        RuntimeError::IllegalStateException {
            message: format!("SHA256Digest: cannot resolve field {what}"),
        }
        .into()
    };
    // `declared_fields` reports fields declared BY this class with an absolute
    // heap slot index; H1..H8, X and xOff are all declared on `SHA256Digest`
    // itself, so no super-class walk is needed.
    let fields = ctx.declared_fields(class_id);
    let index_of = |name: &str| -> Option<(usize, u8)> {
        fields
            .iter()
            .find(|f| f.name == name && !f.is_static)
            .map(|f| {
                (
                    f.slot_index,
                    f.descriptor.as_bytes().first().copied().unwrap_or(0),
                )
            })
    };
    let mut h = [(0usize, 0u8); 8];
    for (slot, name) in h.iter_mut().zip(BC_SHA256_STATE_FIELDS) {
        *slot = index_of(name).ok_or_else(|| bad(name))?;
    }
    let slots = BcSha256Slots {
        h,
        x: index_of("X").ok_or_else(|| bad("X"))?.0,
        x_off: index_of("xOff").ok_or_else(|| bad("xOff"))?,
    };
    *BC_SHA256_SLOTS.write() = Some((key, slots));
    Ok(slots)
}

/// Native `org.bouncycastle.crypto.digests.SHA256Digest.processBlock()`.
///
/// # Why this one and not `MessageDigest`
///
/// BouncyCastle's LMS/HSS (`pqc.crypto.lms`) builds `new SHA256Digest()`
/// directly — see that package's `DigestUtil.createDigest` — so this VM's
/// native JCA SHA-256 is on a path the workload never takes, and HotSpot has no
/// intrinsic for BouncyCastle's class either. Both VMs run the round schedule as
/// real bytecode; measurement put CratonVM at ~37x HotSpot on that kernel with
/// the JIT fully engaged and nothing stuck in the interpreter. This replaces the
/// one leaf that owns the cost.
///
/// # Why `processBlock` is the right seam
///
/// It is a `protected` leaf with no arguments and no calls out: every input is a
/// field of the receiver (`H1..H8`, `X`), and the kernel reproduces its exact
/// post-state including the expanded schedule left in `X[16..64]` and the
/// cleared `X[0..16]`. Buffering, padding, length encoding, `reset`, `copy` and
/// `getEncodedState` all stay real bytecode.
///
/// `SHA256Digest` has no subclasses in BouncyCastle, so the superclass walk in
/// `intercept_force_registered_native` cannot divert some other digest's
/// `processBlock` here. Tagged `Intrinsic`, not a stub: it computes the method's
/// exact result rather than standing in for it.
pub fn register_bc_sha256_digest(r: &mut NativeMethodRegistry) {
    // `register_with_kind` rather than the ambient `set_category` the older BC
    // registrars around this one use: it records `kind_stated`, so the census
    // can tell "somebody adjudicated this as an Intrinsic" from "this inherited
    // whatever category was ambient at the registration site".
    r.register(
        "org/bouncycastle/crypto/digests/SHA256Digest",
        "processBlock",
        "()V",
        |ctx, args| {
            let this = obj_arg(args, 0)?;
            let slots = bc_sha256_slots(ctx, this)?;

            // ONE batched read for all nine fields rather than nine trips
            // through the single-slot door. Each of those pays its own
            // canonicalisation, corrupt-cell watch and descriptor resolution,
            // and that prologue -- not the compression, and not the bytecode
            // around it -- is what this kernel's wall was made of. MEASURED
            // 2026-09-22, one 64-byte block: 5 893 ns against HotSpot's 334,
            // of which the sixteen `processWord` decodes are 384. See
            // `NativeContext::set_fields_typed`.
            let mut read = [Value::Int(0); 9];
            let mut read_slots = [(0usize, 0u8); 9];
            read_slots[..8].copy_from_slice(&slots.h);
            // `X` is read in the same batch. Its descriptor byte is `[`, taken
            // from the class metadata like every other slot here.
            read_slots[8] = (slots.x, b'[');
            ctx.get_fields_typed(this, &read_slots, &mut read);

            let mut state = [0u32; 8];
            for (slot, value) in state.iter_mut().zip(read.iter()) {
                match value {
                    Value::Int(v) => *slot = *v as u32,
                    _ => {
                        return Err(RuntimeError::IllegalStateException {
                            message: "SHA256Digest: malformed chaining word".into(),
                        }
                        .into())
                    }
                }
            }

            let x_arr = match read[8] {
                Value::Object(Some(o)) => o,
                _ => {
                    return Err(RuntimeError::IllegalStateException {
                        message: "SHA256Digest: missing X".into(),
                    }
                    .into())
                }
            };

            // One bulk read of the SIXTEEN live words rather than 64
            // `get_array_element` round trips — the per-element path costs a
            // virtual dispatch plus a `Value` box per word, which is most of
            // what this native exists to remove.
            //
            // Sixteen and not 64: `X[16..64]` is scratch the message schedule
            // writes before it reads. `sha256_process_block`'s expansion loop
            // computes `x[t]` from `x[t-2]`, `x[t-7]`, `x[t-15]`, `x[t-16]` for
            // `t` in `16..64`, so the first iteration reads `x[14]`, `x[9]`,
            // `x[1]`, `x[0]` — all live input — and every later one reads only
            // words this same loop has already written. Nothing in the tail is
            // an INPUT, so reading it was copying 192 bytes to overwrite them.
            // The write-back below still restores all 64 words, so everything
            // `copy()` / `reset(Memoable)` / `getEncodedState` can observe is
            // byte-identical to what BouncyCastle's own bytecode leaves.
            //
            // A short read means `X` is not the 64-word `int[]` the class
            // declares, so the LENGTH is still checked against 64.
            //
            // BouncyCastle 1.86 keeps the schedule in locals and declares `X` as a 16-word
            // `int[]`; `processBlock` there reads `X[0..16]`, clears them, and leaves no
            // expanded words behind. That layout is served by the same kernel, writing back
            // only the sixteen cleared words (a 64-word write would run off the array and
            // this native used to refuse the class: `SHA256Digest: X is not a 64-word int[]`
            // in every OCSP responder test run on a 1.86 jar).
            let x_len = ctx.array_length(x_arr);
            if x_len != 64 && x_len != 16 {
                return Err(RuntimeError::IllegalStateException {
                    message: "SHA256Digest: X is not a 64-word int[]".into(),
                }
                .into());
            }
            let mut words = [0i32; 16];
            if ctx.read_int_array_into(x_arr, 0, &mut words) != 16 {
                return Err(RuntimeError::IllegalStateException {
                    message: "SHA256Digest: X is not a 64-word int[]".into(),
                }
                .into());
            }
            let mut w = [0u32; 16];
            for (dst, src) in w.iter_mut().zip(words.iter()) {
                *dst = *src as u32;
            }

            cratonvm_native_builtins_crypto::bc_digest::sha256_compress(&mut state, &w);
            ctx.write_int_array_from(x_arr, 0, &[0i32; 16]);

            // ONE batched write for the eight chaining words and `xOff`.
            // BouncyCastle's `xOff = 0` is what `processWord` reads to decide
            // when the next block is full; omitting it would leave the digest
            // permanently mid-block.
            let mut write = [Value::Int(0); 9];
            let mut write_slots = [(0usize, 0u8); 9];
            write_slots[..8].copy_from_slice(&slots.h);
            write_slots[8] = slots.x_off;
            for (dst, word) in write.iter_mut().zip(state.iter()) {
                *dst = Value::Int(*word as i32);
            }
            write[8] = Value::Int(0);
            ctx.set_fields_typed(this, &write_slots, &write);
            Ok(None)
        },
        cratonvm_native_api::NativeKind::Intrinsic,
    );
}

pub const BC_LONG_DIGEST_STATE_FIELDS: [&str; 8] = ["H1", "H2", "H3", "H4", "H5", "H6", "H7", "H8"];

#[derive(Clone, Copy)]
pub struct BcLongDigestSlots {
    pub h: [(usize, u8); 8],
    pub w: usize,
    pub w_off: (usize, u8),
    pub bc1: (usize, u8),
    pub bc2: (usize, u8),
}

pub static BC_LONG_DIGEST_SLOTS: cratonvm_types::lock_order::OrderedPlRwLock<
    Option<((usize, u32), BcLongDigestSlots)>,
> = cratonvm_types::lock_order::OrderedPlRwLock::new(
    None,
    cratonvm_types::lock_order::LockLevel::Scratch,
);

pub fn bc_long_digest_slots(
    ctx: &dyn NativeContext,
    this: ObjectRef,
) -> Result<BcLongDigestSlots, MethodCallFailed> {
    let class_id = ctx.class_id_of_object(this);
    let key = (ctx.vm_identity(), class_id.as_u32());
    if let Some((cached_key, slots)) = *BC_LONG_DIGEST_SLOTS.read() {
        if cached_key == key {
            return Ok(slots);
        }
    }
    let bad = |what: &str| -> MethodCallFailed {
        RuntimeError::IllegalStateException {
            message: format!("LongDigest: cannot resolve field {what}"),
        }
        .into()
    };
    let mut cur = Some(class_id);
    let mut fields = Vec::new();
    while let Some(cid) = cur {
        let declared = ctx.declared_fields(cid);
        if declared.iter().any(|f| f.name == "H1" && !f.is_static) {
            fields = declared;
            break;
        }
        cur = ctx.superclass_of(cid);
    }
    let index_of = |name: &str| -> Option<(usize, u8)> {
        fields
            .iter()
            .find(|f| f.name == name && !f.is_static)
            .map(|f| {
                (
                    f.slot_index,
                    f.descriptor.as_bytes().first().copied().unwrap_or(0),
                )
            })
    };
    let mut h = [(0usize, 0u8); 8];
    for (slot, name) in h.iter_mut().zip(BC_LONG_DIGEST_STATE_FIELDS) {
        *slot = index_of(name).ok_or_else(|| bad(name))?;
    }
    let slots = BcLongDigestSlots {
        h,
        w: index_of("W").ok_or_else(|| bad("W"))?.0,
        w_off: index_of("wOff").ok_or_else(|| bad("wOff"))?,
        bc1: index_of("byteCount1").ok_or_else(|| bad("byteCount1"))?,
        bc2: index_of("byteCount2").ok_or_else(|| bad("byteCount2"))?,
    };
    *BC_LONG_DIGEST_SLOTS.write() = Some((key, slots));
    Ok(slots)
}

/// Native `org.bouncycastle.crypto.digests.LongDigest.processBlock()`.

pub const BC_MD5_STATE_FIELDS: [&str; 4] = ["H1", "H2", "H3", "H4"];

#[derive(Clone, Copy)]
pub struct BcMd5Slots {
    pub h: [(usize, u8); 4],
    pub x: usize,
    pub x_off: (usize, u8),
}

