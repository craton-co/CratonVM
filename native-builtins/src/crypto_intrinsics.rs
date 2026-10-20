// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Reviewed `Intrinsic`s for the `@IntrinsicCandidate` kernels of the JDK's
//! own SUN and SunJCE engines (SBR wave 5, lane `cryptoeng`).
//!
//! # Why an `Intrinsic` may win here (AGENTS.md, contract §1.4)
//!
//! Real class bytes are authoritative over registered natives; the one
//! exception is a reviewed `NativeKind::Intrinsic` that computes the JDK
//! method's exact semantics. Every row below is a method the JDK itself marks
//! `@IntrinsicCandidate` and HotSpot replaces with a stub — the same
//! substitution, with the same observable result:
//!
//! | triple | what the Java body does |
//! |---|---|
//! | `SHA` / `SHA2` / `SHA5` / `MD5` / `SHA3` `.implCompress0([BI)V` | one compression of `state` |
//! | `DigestBase.implCompressMultiBlock0([BII)I` | `implCompress` per block up to `limit` |
//! | `AESCrypt.implEncryptBlock` / `implDecryptBlock` `([BI[BI)V` | one AES block with `K` |
//! | `CounterMode.implCrypt([BII[BI)I` | the CTR byte loop over `counter` / `encryptedCounter` / `used` |
//! | `CipherBlockChaining.implEncrypt` / `implDecrypt` `([BII[BI)I` | the CBC block loop over `r` / `k` |
//! | `ElectronicCodeBook.implECBEncrypt` / `implECBDecrypt` `([BII[BI)I` | the ECB block loop |
//! | `GHASH.processBlocks([BII[J[J)V` | GF(2^128) fold of `st` |
//! | `ChaCha20Cipher.implChaCha20Block([I[B)I` | the ChaCha20 block function |
//! | `Poly1305.processMultipleBlocks([BII[J[J)V` | per block `a = (a + n) * r` on 26-bit limbs |
//!
//! Six more rows are NOT HotSpot candidates and say so at their definition:
//! `ByteArrayAccess.i2bBig`/`i2bBig4`/`i2bLittle`/`i2bLittle4`/`l2bBig` and
//! `ChaCha20Cipher.xor`, whose bodies are `byteArrayViewVarHandle` accesses
//! that C2 compiles to plain loads/stores and this VM runs as one generic
//! native call per access. Same exactness rule, same reason to win.
//!
//! Every native reads and writes the SAME fields and arrays the Java body
//! does, so the bytecode around the call (buffering, padding, tags, the
//! `Cipher`/`Mac`/`MessageDigest` state machines — all still real SunJCE/SUN
//! code) cannot tell the difference. The kernels live in
//! `cratonvm_native_builtins_crypto::jdk_intrinsics`, each unit-tested against
//! a line-by-line port of the Java body or a published vector.
//!
//! # Where the Java body and this code are allowed to differ
//!
//! Only in what no caller can observe, and each is what HotSpot's stub does
//! too:
//!
//! * scratch fields the Java body writes and nothing reads before
//!   overwriting: `SHA`/`SHA2`/`SHA5`'s `W`, `Poly1305.n`;
//! * `ChaCha20Cipher.implChaCha20Block` returns up to 16 blocks (1024 bytes)
//!   per call, as HotSpot's AVX-512 stub does. The caller is written for it
//!   ("Intrinsics can do multiple blocks at once", `chaCha20Transform` trims
//!   the key stream at the counter limit); the key stream is the same bytes;
//! * the cipher modes check the whole region up front and throw before
//!   writing anything, where the Java loop would have processed the in-range
//!   prefix first. Their only JDK callers (`CounterMode.crypt`,
//!   `CipherBlockChaining.encrypt/decrypt`, `ElectronicCodeBook.encrypt/
//!   decrypt`) validate the region before calling, so this is reachable only
//!   by reflection.
//!
//! Aliasing is exact: when `in` and `out` are the same array the kernels run
//! on one buffer covering both regions, in the Java body's order, so an
//! overlapping in-place call produces what the bytecode would (including the
//! CBC-decrypt quirk of re-reading the chaining value after the write).
//!
//! # When the embedded cipher is not `AESCrypt`
//!
//! `CounterMode`, `CipherBlockChaining` and `ElectronicCodeBook` also serve
//! DES, DESede, Blowfish and RC2. HotSpot's stubs are AES-only and fall back
//! to the bytecode; a native cannot run the bytecode it shadows, so for those
//! ciphers the native performs the Java loop itself and calls the embedded
//! cipher's own `encryptBlock` / `decryptBlock` bytecode per block (heap-level
//! transcription, roots held across each call). Likewise
//! `DigestBase.implCompressMultiBlock0` calls the receiver's own
//! `implCompress` for a digest it has no kernel for (`MD2`, `MD4`).
//!
//! # Kind
//!
//! Registered under an explicit `NativeKind::Intrinsic` scope (AGENTS.md: a
//! registrar states its kind and restores the ambient category on exit).

use cratonvm_native_api::{NativeContext, NativeHandleScope, NativeKind, NativeMethodRegistry};
use cratonvm_native_builtins_crypto::jdk_intrinsics as kern;
use cratonvm_types::error::{
    out_of_bounds_message, MethodCallFailed, MethodCallResult, RuntimeError,
};
use cratonvm_types::{ObjectRef, Value};

type R<T> = Result<T, MethodCallFailed>;

const AESCRYPT: &str = "com/sun/crypto/provider/AESCrypt";

// ---------------------------------------------------------------------------
// Argument, field and array helpers
// ---------------------------------------------------------------------------

fn arg_obj(args: &[Value], i: usize) -> Option<ObjectRef> {
    args.get(i).and_then(|v| v.as_object())
}

fn arg_int(args: &[Value], i: usize) -> i32 {
    args.get(i).and_then(|v| v.as_int()).unwrap_or(0)
}

fn npe() -> MethodCallFailed {
    RuntimeError::NullPointerException { message: None }.into()
}

fn nn(o: Option<ObjectRef>) -> R<ObjectRef> {
    o.ok_or_else(npe)
}

fn clamp_i32(v: i64) -> i32 {
    v.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
}

fn aioobe(index: i64, len: usize) -> MethodCallFailed {
    RuntimeError::aioobe(clamp_i32(index), clamp_i32(len as i64)).into()
}

/// `Preconditions.checkFromIndexSize(from, size, len, AIOOBE_FORMATTER)`.
fn range_aioobe(from: i64, size: i64, len: usize) -> MethodCallFailed {
    RuntimeError::ArrayIndexOutOfBoundsException {
        index: clamp_i32(from),
        message: Some(out_of_bounds_message::check_from_index_size(
            from, size, len as i64,
        )),
    }
    .into()
}

/// `true` when `[off, off + len)` lies inside an array of `alen`.
fn in_range(off: i64, len: i64, alen: usize) -> bool {
    off >= 0 && len >= 0 && off + len <= alen as i64
}

/// Check `[off, off + len)` against `arr`, the way an element loop over it
/// would fail first: at `off` when `off` is out of range, else at the
/// array's length.
fn check_region(ctx: &dyn NativeContext, arr: ObjectRef, off: i64, len: i64) -> R<()> {
    let alen = ctx.array_length(arr);
    if in_range(off, len, alen) {
        return Ok(());
    }
    if off < 0 || off >= alen as i64 {
        Err(aioobe(off, alen))
    } else {
        Err(aioobe(alen as i64, alen))
    }
}

fn field_obj(ctx: &dyn NativeContext, o: ObjectRef, name: &str) -> Option<ObjectRef> {
    ctx.get_field_by_name(o, name).as_object()
}

fn field_int(ctx: &dyn NativeContext, o: ObjectRef, name: &str) -> R<i32> {
    ctx.get_field_by_name(o, name).as_int().ok_or_else(|| {
        RuntimeError::InternalError {
            message: format!("crypto intrinsic: field {name} is not an int"),
        }
        .into()
    })
}

fn class_name_is(ctx: &dyn NativeContext, o: ObjectRef, name: &str) -> bool {
    ctx.class_name_arc_of_id(ctx.class_id_of_object(o))
        .as_deref()
        == Some(name)
}

fn read_bytes(ctx: &dyn NativeContext, arr: ObjectRef, off: usize, len: usize) -> Vec<u8> {
    let mut v = vec![0u8; len];
    if len == 0 {
        return v;
    }
    let n = ctx.read_byte_array_into(arr, off, &mut v);
    for (i, slot) in v.iter_mut().enumerate().skip(n) {
        if let Value::Int(b) = ctx.get_array_element(arr, off + i) {
            *slot = b as u8;
        }
    }
    v
}

fn write_bytes(ctx: &mut dyn NativeContext, arr: ObjectRef, off: usize, data: &[u8]) {
    if data.is_empty() || ctx.write_byte_array_from(arr, off, data) {
        return;
    }
    for (i, b) in data.iter().enumerate() {
        ctx.set_array_element(arr, off + i, Value::Int(i32::from(*b as i8)));
    }
}

fn read_ints(ctx: &dyn NativeContext, arr: ObjectRef) -> Vec<i32> {
    let len = ctx.array_length(arr);
    let mut v = vec![0i32; len];
    if len == 0 {
        return v;
    }
    let n = ctx.read_int_array_into(arr, 0, &mut v);
    for (i, slot) in v.iter_mut().enumerate().skip(n) {
        if let Value::Int(x) = ctx.get_array_element(arr, i) {
            *slot = x;
        }
    }
    v
}

fn write_ints(ctx: &mut dyn NativeContext, arr: ObjectRef, data: &[i32]) {
    if data.is_empty() || ctx.write_int_array_from(arr, 0, data) {
        return;
    }
    for (i, x) in data.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Int(*x));
    }
}

fn read_longs(ctx: &dyn NativeContext, arr: ObjectRef, n: usize) -> Vec<i64> {
    let mut raw = vec![0u8; 8 * n];
    if n > 0 && ctx.read_primitive_array_bytes(arr, 0, &mut raw) == 8 * n {
        return raw
            .chunks_exact(8)
            .map(|c| {
                let mut w = [0u8; 8];
                w.copy_from_slice(c);
                i64::from_ne_bytes(w)
            })
            .collect();
    }
    (0..n)
        .map(|i| match ctx.get_array_element(arr, i) {
            Value::Long(x) => x,
            _ => 0,
        })
        .collect()
}

fn write_longs(ctx: &mut dyn NativeContext, arr: ObjectRef, data: &[i64]) {
    if data.is_empty() {
        return;
    }
    let raw: Vec<u8> = data.iter().flat_map(|x| x.to_ne_bytes()).collect();
    if ctx.write_primitive_array_bytes(arr, 0, &raw) {
        return;
    }
    for (i, x) in data.iter().enumerate() {
        ctx.set_array_element(arr, i, Value::Long(*x));
    }
}

/// One buffer covering an input and an output region, so a kernel can run
/// the Java loop's order even when both regions are in the same array.
struct Region {
    buf: Vec<u8>,
    inp: usize,
    out: usize,
    /// Where `buf[out..]` goes back: (array, offset).
    out_arr: ObjectRef,
    out_off: usize,
    out_len: usize,
}

impl Region {
    /// Both regions must already be range-checked.
    fn load(
        ctx: &dyn NativeContext,
        in_arr: ObjectRef,
        in_off: usize,
        in_len: usize,
        out_arr: ObjectRef,
        out_off: usize,
        out_len: usize,
    ) -> Self {
        if in_arr == out_arr {
            let lo = in_off.min(out_off);
            let hi = (in_off + in_len).max(out_off + out_len);
            Region {
                buf: read_bytes(ctx, in_arr, lo, hi - lo),
                inp: in_off - lo,
                out: out_off - lo,
                out_arr,
                out_off,
                out_len,
            }
        } else {
            let mut buf = read_bytes(ctx, in_arr, in_off, in_len);
            buf.resize(in_len + out_len, 0);
            Region {
                buf,
                inp: 0,
                out: in_len,
                out_arr,
                out_off,
                out_len,
            }
        }
    }

    fn store(&self, ctx: &mut dyn NativeContext) {
        let out = &self.buf[self.out..self.out + self.out_len];
        write_bytes(ctx, self.out_arr, self.out_off, out);
    }
}

fn byte16(v: &[u8]) -> [u8; 16] {
    let mut b = [0u8; 16];
    b.copy_from_slice(&v[..16]);
    b
}

// ---------------------------------------------------------------------------
// AES round keys from an AESCrypt
// ---------------------------------------------------------------------------

/// The keys `AESCrypt.implEncryptBlock` (or `implDecryptBlock`) of `aes`
/// would use: `K` with `limit` (or `ROUNDS_12` / `ROUNDS_14`).
fn aes_keys(ctx: &dyn NativeContext, aes: ObjectRef, decrypt: bool) -> R<kern::AesRoundKeys> {
    let k_arr = nn(field_obj(ctx, aes, "K"))?;
    let k = read_ints(ctx, k_arr);
    let res = if decrypt {
        let r12 = field_int(ctx, aes, "ROUNDS_12")? != 0;
        let r14 = field_int(ctx, aes, "ROUNDS_14")? != 0;
        kern::AesRoundKeys::for_decrypt(&k, r12, r14)
    } else {
        kern::AesRoundKeys::for_encrypt(&k, field_int(ctx, aes, "limit")?)
    };
    res.map_err(|i| aioobe(i as i64, k.len()))
}

// ---------------------------------------------------------------------------
// Digests
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Digest {
    Sha1,
    Sha256,
    Sha512,
    Md5,
    Sha3,
}

impl Digest {
    /// Block size of the kernel; `None` for SHA-3, whose rate is the
    /// receiver's `blockSize` field.
    fn fixed_block(self) -> Option<usize> {
        match self {
            Digest::Sha1 | Digest::Sha256 | Digest::Md5 => Some(64),
            Digest::Sha512 => Some(128),
            Digest::Sha3 => None,
        }
    }
}

/// The digest kernel for a receiver whose `implCompress` is exactly the
/// JDK's (`SHA`, `SHA2`'s and `SHA5`'s nested final classes, `MD5`, `SHA3`'s
/// subclasses). Anything else — `MD2`, `MD4` — gets `None`.
fn classify_digest(ctx: &dyn NativeContext, recv: ObjectRef) -> Option<Digest> {
    let name = ctx.class_name_arc_of_id(ctx.class_id_of_object(recv))?;
    let n: &str = &name;
    match n {
        "sun/security/provider/SHA" => Some(Digest::Sha1),
        "sun/security/provider/SHA2$SHA224" | "sun/security/provider/SHA2$SHA256" => {
            Some(Digest::Sha256)
        }
        "sun/security/provider/SHA5$SHA384"
        | "sun/security/provider/SHA5$SHA512"
        | "sun/security/provider/SHA5$SHA512_224"
        | "sun/security/provider/SHA5$SHA512_256" => Some(Digest::Sha512),
        "sun/security/provider/MD5" => Some(Digest::Md5),
        _ if n.starts_with("sun/security/provider/SHA3$") => Some(Digest::Sha3),
        _ => None,
    }
}

/// Run `digest`'s compression over every block of `data` against `this`'s
/// `state` (and, for the JDK 17/21 SHA-3 layout, `lanes`).
fn compress_state(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    digest: Digest,
    block: usize,
    data: &[u8],
) -> R<()> {
    let st = nn(field_obj(ctx, this, "state"))?;
    let slen = ctx.array_length(st);
    match digest {
        Digest::Sha1 | Digest::Sha256 | Digest::Md5 => {
            let words = match digest {
                Digest::Sha1 => 5,
                Digest::Sha256 => 8,
                _ => 4,
            };
            if slen < words {
                return Err(aioobe(slen as i64, slen));
            }
            let mut s = read_ints(ctx, st);
            match digest {
                Digest::Sha1 => {
                    let mut a = [0u32; 5];
                    for (d, v) in a.iter_mut().zip(&s) {
                        *d = *v as u32;
                    }
                    kern::sha1_blocks(&mut a, data);
                    for (d, v) in s.iter_mut().zip(a) {
                        *d = v as i32;
                    }
                }
                Digest::Sha256 => {
                    let mut a = [0u32; 8];
                    for (d, v) in a.iter_mut().zip(&s) {
                        *d = *v as u32;
                    }
                    kern::sha256_blocks(&mut a, data);
                    for (d, v) in s.iter_mut().zip(a) {
                        *d = v as i32;
                    }
                }
                _ => {
                    let mut a = [0u32; 4];
                    for (d, v) in a.iter_mut().zip(&s) {
                        *d = *v as u32;
                    }
                    kern::md5_blocks(&mut a, data);
                    for (d, v) in s.iter_mut().zip(a) {
                        *d = v as i32;
                    }
                }
            }
            write_ints(ctx, st, &s[..words]);
        }
        Digest::Sha512 => {
            if slen < 8 {
                return Err(aioobe(slen as i64, slen));
            }
            let s = read_longs(ctx, st, 8);
            let mut a = [0u64; 8];
            for (d, v) in a.iter_mut().zip(&s) {
                *d = *v as u64;
            }
            kern::sha512_blocks(&mut a, data);
            let out: Vec<i64> = a.iter().map(|v| *v as i64).collect();
            write_longs(ctx, st, &out);
        }
        Digest::Sha3 => {
            if slen == 25 {
                // JDK 22+: `long[] state`, lanes XORed in directly.
                if block > 200 {
                    return Err(aioobe(25, 25));
                }
                let s = read_longs(ctx, st, 25);
                let mut a = [0u64; 25];
                for (d, v) in a.iter_mut().zip(&s) {
                    *d = *v as u64;
                }
                kern::sha3_blocks(&mut a, data, block);
                let out: Vec<i64> = a.iter().map(|v| *v as i64).collect();
                write_longs(ctx, st, &out);
            } else if slen == 200 {
                // JDK 17/21: `byte[] state` + `long[] lanes` scratch.
                if block > 200 {
                    return Err(aioobe(200, 200));
                }
                let lanes_arr = nn(field_obj(ctx, this, "lanes"))?;
                if ctx.array_length(lanes_arr) < 25 {
                    let l = ctx.array_length(lanes_arr);
                    return Err(aioobe(l as i64, l));
                }
                let mut s = [0u8; 200];
                s.copy_from_slice(&read_bytes(ctx, st, 0, 200));
                let l = read_longs(ctx, lanes_arr, 25);
                let mut lanes = [0u64; 25];
                for (d, v) in lanes.iter_mut().zip(&l) {
                    *d = *v as u64;
                }
                kern::sha3_blocks_bytes(&mut s, &mut lanes, data, block);
                write_bytes(ctx, st, 0, &s);
                let out: Vec<i64> = lanes.iter().map(|v| *v as i64).collect();
                write_longs(ctx, lanes_arr, &out);
            } else {
                return Err(aioobe(slen as i64, slen));
            }
        }
    }
    Ok(())
}

/// Block size of `digest` on `this`: the kernel's, or SHA-3's `blockSize`.
fn digest_block(ctx: &dyn NativeContext, this: ObjectRef, digest: Digest) -> R<usize> {
    match digest.fixed_block() {
        Some(b) => Ok(b),
        None => {
            let b = field_int(ctx, this, "blockSize")?;
            if b <= 0 {
                return Err(RuntimeError::InternalError {
                    message: format!("SHA3 blockSize {b}"),
                }
                .into());
            }
            Ok(b as usize)
        }
    }
}

/// `implCompress0(byte[] buf, int ofs)` of `SHA`/`SHA2`/`SHA5`/`MD5`/`SHA3`.
fn compress0(ctx: &mut dyn NativeContext, args: &[Value], digest: Digest) -> MethodCallResult {
    let this = nn(arg_obj(args, 0))?;
    let buf = nn(arg_obj(args, 1))?;
    let ofs = arg_int(args, 2);
    let block = digest_block(ctx, this, digest)?;
    let len = ctx.array_length(buf);
    if !in_range(i64::from(ofs), block as i64, len) {
        return Err(range_aioobe(i64::from(ofs), block as i64, len));
    }
    let data = read_bytes(ctx, buf, ofs as usize, block);
    compress_state(ctx, this, digest, block, &data)?;
    Ok(None)
}

fn native_sha_compress0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    compress0(ctx, args, Digest::Sha1)
}
fn native_sha2_compress0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    compress0(ctx, args, Digest::Sha256)
}
fn native_sha5_compress0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    compress0(ctx, args, Digest::Sha512)
}
fn native_md5_compress0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    compress0(ctx, args, Digest::Md5)
}
fn native_sha3_compress0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    compress0(ctx, args, Digest::Sha3)
}

/// `DigestBase.implCompressMultiBlock0(byte[] b, int ofs, int limit)`:
/// `for (; ofs <= limit; ofs += blockSize) implCompress(b, ofs); return ofs;`
fn native_digest_multi_block0(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = nn(arg_obj(args, 0))?;
    let b = arg_obj(args, 1);
    let ofs = arg_int(args, 2);
    let limit = arg_int(args, 3);
    let block_field = field_int(ctx, this, "blockSize")?;
    if ofs > limit {
        return Ok(Some(Value::Int(ofs)));
    }
    if block_field <= 0 {
        // The Java loop would never terminate normally; there is nothing
        // faithful to do fast. Hand it to the receiver's own code.
        return multi_block_via_java(ctx, this, b, ofs, limit, block_field);
    }
    let digest = match classify_digest(ctx, this) {
        Some(d) => d,
        None => return multi_block_via_java(ctx, this, b, ofs, limit, block_field),
    };
    let block = digest_block(ctx, this, digest)?;
    if block as i32 != block_field {
        return multi_block_via_java(ctx, this, b, ofs, limit, block_field);
    }
    let b = nn(b)?;
    let bs = block as i64;
    let n = (i64::from(limit) - i64::from(ofs)) / bs + 1;
    let len = ctx.array_length(b) as i64;
    let first = i64::from(ofs);
    // Blocks the Java loop completes before its implCompressCheck throws.
    let n_ok = if first < 0 || first > len {
        0
    } else {
        ((len - first) / bs).min(n)
    };
    if n_ok > 0 {
        let data = read_bytes(ctx, b, first as usize, (n_ok * bs) as usize);
        compress_state(ctx, this, digest, block, &data)?;
    }
    if n_ok < n {
        return Err(range_aioobe(first + n_ok * bs, bs, len as usize));
    }
    Ok(Some(Value::Int((first + n * bs) as i32)))
}

/// The Java loop, calling the receiver's own `implCompress` per block.
fn multi_block_via_java(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    b: Option<ObjectRef>,
    mut ofs: i32,
    limit: i32,
    block: i32,
) -> MethodCallResult {
    let mut scope = NativeHandleScope::new(ctx);
    let this_h = scope.root(this);
    let b_h = b.map(|o| scope.root(o));
    while ofs <= limit {
        let recv = scope.get(&this_h);
        let arr = b_h.as_ref().map(|h| scope.get(h));
        scope.invoke_virtual(
            recv,
            "implCompress",
            "([BI)V",
            &[Value::Object(arr), Value::Int(ofs)],
        )?;
        ofs = ofs.wrapping_add(block);
        if block <= 0 && ofs > limit {
            break;
        }
    }
    Ok(Some(Value::Int(ofs)))
}

// ---------------------------------------------------------------------------
// AESCrypt
// ---------------------------------------------------------------------------

fn aes_block(ctx: &mut dyn NativeContext, args: &[Value], decrypt: bool) -> MethodCallResult {
    let this = nn(arg_obj(args, 0))?;
    let inp = nn(arg_obj(args, 1))?;
    let in_off = i64::from(arg_int(args, 2));
    check_region(ctx, inp, in_off, 16)?;
    let keys = aes_keys(ctx, this, decrypt)?;
    let out = nn(arg_obj(args, 3))?;
    let out_off = i64::from(arg_int(args, 4));
    check_region(ctx, out, out_off, 16)?;
    let mut b = byte16(&read_bytes(ctx, inp, in_off as usize, 16));
    keys.apply_block(&mut b);
    write_bytes(ctx, out, out_off as usize, &b);
    Ok(None)
}

fn native_aes_encrypt_block(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    aes_block(ctx, args, false)
}
fn native_aes_decrypt_block(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    aes_block(ctx, args, true)
}

// ---------------------------------------------------------------------------
// Feedback modes
// ---------------------------------------------------------------------------

/// `(embeddedCipher, blockSize)` of a `FeedbackCipher`, and whether the fast
/// AES kernels apply (`embeddedCipher` is exactly `AESCrypt`, 16-byte block).
fn feedback(ctx: &dyn NativeContext, this: ObjectRef) -> R<(ObjectRef, i32, bool)> {
    let emb = nn(field_obj(ctx, this, "embeddedCipher"))?;
    let bs = field_int(ctx, this, "blockSize")?;
    let aes = bs == 16 && class_name_is(ctx, emb, AESCRYPT);
    Ok((emb, bs, aes))
}

/// `CounterMode.implCrypt(byte[] in, int inOff, int len, byte[] out, int outOff)`.
fn native_ctr_impl_crypt(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = nn(arg_obj(args, 0))?;
    let len = arg_int(args, 3);
    if len <= 0 {
        return Ok(Some(Value::Int(len)));
    }
    let inp = nn(arg_obj(args, 1))?;
    let in_off = i64::from(arg_int(args, 2));
    let out = nn(arg_obj(args, 4))?;
    let out_off = i64::from(arg_int(args, 5));
    check_region(ctx, inp, in_off, i64::from(len))?;
    check_region(ctx, out, out_off, i64::from(len))?;
    let (emb, bs, aes) = feedback(ctx, this)?;
    let ctr_arr = nn(field_obj(ctx, this, "counter"))?;
    let ec_arr = nn(field_obj(ctx, this, "encryptedCounter"))?;
    let used = field_int(ctx, this, "used")?;
    let len_u = len as usize;
    let mut region = Region::load(
        ctx,
        inp,
        in_off as usize,
        len_u,
        out,
        out_off as usize,
        len_u,
    );
    if aes && used >= 0 && ctx.array_length(ctr_arr) == 16 && ctx.array_length(ec_arr) == 16 {
        let keys = aes_keys(ctx, emb, false)?;
        let mut counter = byte16(&read_bytes(ctx, ctr_arr, 0, 16));
        let mut ec = byte16(&read_bytes(ctx, ec_arr, 0, 16));
        let mut u = used;
        let ks = kern::ctr_keystream(&keys, &mut counter, &mut ec, &mut u, len_u);
        xor_keystream(&mut region, &ks);
        region.store(ctx);
        write_bytes(ctx, ctr_arr, 0, &counter);
        write_bytes(ctx, ec_arr, 0, &ec);
        ctx.set_field_by_name(this, "used", Value::Int(u));
        return Ok(Some(Value::Int(len)));
    }
    ctr_via_java(
        ctx,
        this,
        emb,
        ctr_arr,
        ec_arr,
        used,
        bs,
        &mut region,
        len_u,
    )?;
    Ok(Some(Value::Int(len)))
}

/// `out[i] = in[i] ^ ks[i]` in the Java loop's order over `region`.
fn xor_keystream(region: &mut Region, ks: &[u8]) {
    let (inp, out) = (region.inp, region.out);
    let n = ks.len();
    if kern::bulk_equals_sequential(inp, out, n) {
        let src: Vec<u8> = region.buf[inp..inp + n].to_vec();
        for i in 0..n {
            region.buf[out + i] = src[i] ^ ks[i];
        }
    } else {
        for i in 0..n {
            region.buf[out + i] = region.buf[inp + i] ^ ks[i];
        }
    }
}

/// The CTR byte loop for a non-AES embedded cipher, calling its own
/// `encryptBlock(counter, 0, encryptedCounter, 0)`.
#[allow(clippy::too_many_arguments)]
fn ctr_via_java(
    ctx: &mut dyn NativeContext,
    this: ObjectRef,
    emb: ObjectRef,
    ctr_arr: ObjectRef,
    ec_arr: ObjectRef,
    mut used: i32,
    bs: i32,
    region: &mut Region,
    len: usize,
) -> R<()> {
    let mut scope = NativeHandleScope::new(ctx);
    let this_h = scope.root(this);
    let emb_h = scope.root(emb);
    let ctr_h = scope.root(ctr_arr);
    let ec_h = scope.root(ec_arr);
    let out_h = scope.root(region.out_arr);
    let mut counter = read_bytes(&*scope, ctr_arr, 0, scope.array_length(ctr_arr));
    let mut ec = read_bytes(&*scope, ec_arr, 0, scope.array_length(ec_arr));
    for i in 0..len {
        if used >= bs {
            let (c, e, m) = (scope.get(&ctr_h), scope.get(&ec_h), scope.get(&emb_h));
            write_bytes(&mut *scope, c, 0, &counter);
            scope.invoke_virtual(
                m,
                "encryptBlock",
                "([BI[BI)V",
                &[
                    Value::Object(Some(c)),
                    Value::Int(0),
                    Value::Object(Some(e)),
                    Value::Int(0),
                ],
            )?;
            let e = scope.get(&ec_h);
            ec = read_bytes(&*scope, e, 0, scope.array_length(e));
            kern::ctr_increment(&mut counter);
            used = 0;
        }
        let k = *ec
            .get(used as usize)
            .ok_or_else(|| aioobe(i64::from(used), ec.len()))?;
        region.buf[region.out + i] = region.buf[region.inp + i] ^ k;
        used += 1;
    }
    let c = scope.get(&ctr_h);
    write_bytes(&mut *scope, c, 0, &counter);
    let t = scope.get(&this_h);
    scope.set_field_by_name(t, "used", Value::Int(used));
    region.out_arr = scope.get(&out_h);
    region.store(&mut *scope);
    Ok(())
}

/// Shared shape of the CBC/ECB natives: `(this, in, inOff, len, out, outOff)`.
struct BlockCall {
    this: ObjectRef,
    emb: ObjectRef,
    bs: i32,
    aes: bool,
    region: Region,
    n: usize,
}

/// Validate and load a CBC/ECB call; `None` when the Java loop runs zero
/// times. `ceil` picks CBC's `offset < end` loop over ECB's `i >= blockSize`.
fn block_call(ctx: &mut dyn NativeContext, args: &[Value], ceil: bool) -> R<Option<BlockCall>> {
    let this = nn(arg_obj(args, 0))?;
    let len = arg_int(args, 3);
    let (emb, bs, aes) = feedback(ctx, this)?;
    if len <= 0 || bs <= 0 || (!ceil && len < bs) {
        return Ok(None);
    }
    let n = if ceil {
        (len as usize).div_ceil(bs as usize)
    } else {
        len as usize / bs as usize
    };
    let span = (n * bs as usize) as i64;
    let inp = nn(arg_obj(args, 1))?;
    let in_off = i64::from(arg_int(args, 2));
    check_region(ctx, inp, in_off, span)?;
    let out = nn(arg_obj(args, 4))?;
    let out_off = i64::from(arg_int(args, 5));
    check_region(ctx, out, out_off, span)?;
    let region = Region::load(
        ctx,
        inp,
        in_off as usize,
        span as usize,
        out,
        out_off as usize,
        span as usize,
    );
    Ok(Some(BlockCall {
        this,
        emb,
        bs,
        aes,
        region,
        n,
    }))
}

fn cbc(ctx: &mut dyn NativeContext, args: &[Value], decrypt: bool) -> MethodCallResult {
    let len = arg_int(args, 3);
    let Some(mut c) = block_call(ctx, args, true)? else {
        return Ok(Some(Value::Int(len)));
    };
    let r_arr = nn(field_obj(ctx, c.this, "r"))?;
    let k_arr = nn(field_obj(ctx, c.this, "k"))?;
    if c.aes && ctx.array_length(r_arr) == 16 && ctx.array_length(k_arr) == 16 {
        let keys = aes_keys(ctx, c.emb, decrypt)?;
        let mut r = byte16(&read_bytes(ctx, r_arr, 0, 16));
        let mut k = byte16(&read_bytes(ctx, k_arr, 0, 16));
        let (inp, out) = (c.region.inp, c.region.out);
        if decrypt {
            kern::cbc_decrypt(&keys, &mut r, &mut k, &mut c.region.buf, inp, out, c.n);
        } else {
            kern::cbc_encrypt(&keys, &mut r, &mut k, &mut c.region.buf, inp, out, c.n);
        }
        c.region.store(ctx);
        write_bytes(ctx, r_arr, 0, &r);
        write_bytes(ctx, k_arr, 0, &k);
        return Ok(Some(Value::Int(len)));
    }
    cbc_via_java(ctx, &mut c, r_arr, k_arr, decrypt)?;
    Ok(Some(Value::Int(len)))
}

/// The CBC loop for a non-AES embedded cipher, block by block through the
/// heap exactly as the Java body does it.
fn cbc_via_java(
    ctx: &mut dyn NativeContext,
    c: &mut BlockCall,
    r_arr: ObjectRef,
    k_arr: ObjectRef,
    decrypt: bool,
) -> R<()> {
    let bs = c.bs as usize;
    let mut scope = NativeHandleScope::new(ctx);
    let emb_h = scope.root(c.emb);
    let r_h = scope.root(r_arr);
    let k_h = scope.root(k_arr);
    let out_h = scope.root(c.region.out_arr);
    let mut r = read_bytes(&*scope, r_arr, 0, bs.min(scope.array_length(r_arr)));
    if r.len() < bs {
        return Err(aioobe(r.len() as i64, r.len()));
    }
    // A scratch Java array for one block of the cipher's output (encrypt) or
    // input (decrypt); the Java body uses `cipher` / `k` in place, which this
    // reproduces through the region buffer.
    for j in 0..c.n {
        let ip = c.region.inp + j * bs;
        let op = c.region.out + j * bs;
        // A scratch Java array stands in for the `cipher` block the Java body
        // hands the embedded cipher (the region buffer holds its bytes).
        let tmp = scope.new_array(cratonvm_types::ArrayElementType::Byte, bs);
        let tmp_h = scope.root(tmp);
        if !decrypt {
            // k[i] = plain[i] ^ r[i]; encryptBlock(k, 0, cipher, off);
            // r = cipher block.
            let kb: Vec<u8> = (0..bs).map(|i| c.region.buf[ip + i] ^ r[i]).collect();
            let k = scope.get(&k_h);
            write_bytes(&mut *scope, k, 0, &kb);
            let (k, t, m) = (scope.get(&k_h), scope.get(&tmp_h), scope.get(&emb_h));
            scope.invoke_virtual(
                m,
                "encryptBlock",
                "([BI[BI)V",
                &[
                    Value::Object(Some(k)),
                    Value::Int(0),
                    Value::Object(Some(t)),
                    Value::Int(0),
                ],
            )?;
            let t = scope.get(&tmp_h);
            let cb = read_bytes(&*scope, t, 0, bs);
            c.region.buf[op..op + bs].copy_from_slice(&cb);
            r.copy_from_slice(&c.region.buf[op..op + bs]);
        } else {
            // decryptBlock(cipher, off, k, 0); plain = k ^ r; r = cipher
            // block (re-read after the write, as the Java body does).
            let src = c.region.buf[ip..ip + bs].to_vec();
            let t = scope.get(&tmp_h);
            write_bytes(&mut *scope, t, 0, &src);
            let (k, t, m) = (scope.get(&k_h), scope.get(&tmp_h), scope.get(&emb_h));
            scope.invoke_virtual(
                m,
                "decryptBlock",
                "([BI[BI)V",
                &[
                    Value::Object(Some(t)),
                    Value::Int(0),
                    Value::Object(Some(k)),
                    Value::Int(0),
                ],
            )?;
            let k = scope.get(&k_h);
            let kb = read_bytes(&*scope, k, 0, bs);
            for i in 0..bs {
                c.region.buf[op + i] = kb[i] ^ r[i];
            }
            r.copy_from_slice(&c.region.buf[ip..ip + bs]);
        }
    }
    let ra = scope.get(&r_h);
    write_bytes(&mut *scope, ra, 0, &r);
    c.region.out_arr = scope.get(&out_h);
    c.region.store(&mut *scope);
    Ok(())
}

fn native_cbc_encrypt(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    cbc(ctx, args, false)
}
fn native_cbc_decrypt(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    cbc(ctx, args, true)
}

fn ecb_call(ctx: &mut dyn NativeContext, args: &[Value], decrypt: bool) -> MethodCallResult {
    let len = arg_int(args, 3);
    let Some(mut c) = block_call(ctx, args, false)? else {
        return Ok(Some(Value::Int(len)));
    };
    if c.aes {
        let keys = aes_keys(ctx, c.emb, decrypt)?;
        let (inp, out) = (c.region.inp, c.region.out);
        kern::ecb(&keys, &mut c.region.buf, inp, out, c.n);
        c.region.store(ctx);
        return Ok(Some(Value::Int(len)));
    }
    // Non-AES: the Java loop, one embedded-cipher call per block on a
    // scratch pair of arrays, in the loop's order over the region buffer.
    let bs = c.bs as usize;
    let mut scope = NativeHandleScope::new(ctx);
    let emb_h = scope.root(c.emb);
    let out_h = scope.root(c.region.out_arr);
    let method = if decrypt {
        "decryptBlock"
    } else {
        "encryptBlock"
    };
    for j in 0..c.n {
        let ip = c.region.inp + j * bs;
        let op = c.region.out + j * bs;
        let src = scope.new_array(cratonvm_types::ArrayElementType::Byte, bs);
        let sh = scope.root(src);
        let dst = scope.new_array(cratonvm_types::ArrayElementType::Byte, bs);
        let dh = scope.root(dst);
        let s = scope.get(&sh);
        let bytes = c.region.buf[ip..ip + bs].to_vec();
        write_bytes(&mut *scope, s, 0, &bytes);
        let (s, d, m) = (scope.get(&sh), scope.get(&dh), scope.get(&emb_h));
        scope.invoke_virtual(
            m,
            method,
            "([BI[BI)V",
            &[
                Value::Object(Some(s)),
                Value::Int(0),
                Value::Object(Some(d)),
                Value::Int(0),
            ],
        )?;
        let d = scope.get(&dh);
        let ob = read_bytes(&*scope, d, 0, bs);
        c.region.buf[op..op + bs].copy_from_slice(&ob);
    }
    c.region.out_arr = scope.get(&out_h);
    c.region.store(&mut *scope);
    Ok(Some(Value::Int(len)))
}

fn native_ecb_encrypt(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    ecb_call(ctx, args, false)
}
fn native_ecb_decrypt(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    ecb_call(ctx, args, true)
}

// ---------------------------------------------------------------------------
// GHASH, ChaCha20, Poly1305
// ---------------------------------------------------------------------------

/// `GHASH.processBlocks(byte[] data, int inOfs, int blocks, long[] st, long[] subH)`.
fn native_ghash_process_blocks(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let blocks = arg_int(args, 2);
    if blocks <= 0 {
        return Ok(None);
    }
    let data = nn(arg_obj(args, 0))?;
    let ofs = i64::from(arg_int(args, 1));
    let st = nn(arg_obj(args, 3))?;
    let sub_h = nn(arg_obj(args, 4))?;
    for a in [st, sub_h] {
        let l = ctx.array_length(a);
        if l < 2 {
            return Err(aioobe(l as i64, l));
        }
    }
    let len = ctx.array_length(data) as i64;
    let n = i64::from(blocks);
    let n_ok = if ofs < 0 || ofs > len {
        0
    } else {
        ((len - ofs) / 16).min(n)
    };
    if n_ok > 0 {
        let bytes = read_bytes(ctx, data, ofs as usize, (16 * n_ok) as usize);
        let s = read_longs(ctx, st, 2);
        let h = read_longs(ctx, sub_h, 2);
        let mut s2 = [s[0] as u64, s[1] as u64];
        kern::ghash_blocks(&mut s2, [h[0] as u64, h[1] as u64], &bytes);
        write_longs(ctx, st, &[s2[0] as i64, s2[1] as i64]);
    }
    if n_ok < n {
        return Err(range_aioobe(ofs + 16 * n_ok, 8, len as usize));
    }
    Ok(None)
}

/// ChaCha20 blocks per call: HotSpot's AVX-512 stub produces 16 (1024 bytes,
/// `KS_MAX_LEN`), and `ChaCha20Cipher` is written to accept any multiple.
const CHACHA_MAX_BLOCKS: usize = 16;

/// `ChaCha20Cipher.implChaCha20Block(int[] initState, byte[] result)`.
fn native_chacha20_block(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let init = nn(arg_obj(args, 0))?;
    let ilen = ctx.array_length(init);
    if ilen < 16 {
        return Err(aioobe(ilen.max(4) as i64, ilen));
    }
    let v = read_ints(ctx, init);
    let mut st = [0u32; 16];
    for (d, s) in st.iter_mut().zip(&v) {
        *d = *s as u32;
    }
    let result = nn(arg_obj(args, 1))?;
    let rlen = ctx.array_length(result);
    let nblocks = (rlen / 64).min(CHACHA_MAX_BLOCKS);
    if nblocks == 0 {
        // The Java body writes the block's sixteen words in order and fails
        // at the first that does not fit.
        let mut one = [0u8; 64];
        kern::chacha20_blocks(&st, &mut one);
        let fit = (rlen / 4) * 4;
        write_bytes(ctx, result, 0, &one[..fit]);
        return Err(aioobe(fit as i64, rlen));
    }
    let mut out = vec![0u8; 64 * nblocks];
    kern::chacha20_blocks(&st, &mut out);
    write_bytes(ctx, result, 0, &out);
    Ok(Some(Value::Int((64 * nblocks) as i32)))
}

/// `Poly1305.processMultipleBlocks(byte[] input, int offset, int length,
/// long[] aLimbs, long[] rLimbs)`. The Java body works on `this.a` and
/// `this.r` (`aLimbs`/`rLimbs` are those elements' `limbs`, passed for the
/// benefit of HotSpot's stub), so this reads the fields, as the body does.
fn native_poly1305_blocks(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let this = nn(arg_obj(args, 0))?;
    let length = arg_int(args, 3);
    if length < 16 {
        return Ok(None);
    }
    let input = nn(arg_obj(args, 1))?;
    let offset = i64::from(arg_int(args, 2));
    let a = nn(field_obj(ctx, this, "a"))?;
    let r = nn(field_obj(ctx, this, "r"))?;
    let a_limbs = nn(field_obj(ctx, a, "limbs"))?;
    let r_limbs = nn(field_obj(ctx, r, "limbs"))?;
    for l in [a_limbs, r_limbs] {
        let n = ctx.array_length(l);
        if n < 5 {
            return Err(aioobe(n as i64, n));
        }
    }
    let len = ctx.array_length(input) as i64;
    let n = i64::from(length / 16);
    let n_ok = if offset < 0 || offset > len {
        0
    } else {
        ((len - offset) / 16).min(n)
    };
    if n_ok > 0 {
        let bytes = read_bytes(ctx, input, offset as usize, (16 * n_ok) as usize);
        let av = read_longs(ctx, a_limbs, 5);
        let rv = read_longs(ctx, r_limbs, 5);
        let mut p = kern::Poly1305Limbs {
            a: [av[0], av[1], av[2], av[3], av[4]],
            a_num_adds: field_int(ctx, a, "numAdds")?,
            r: [rv[0], rv[1], rv[2], rv[3], rv[4]],
            r_num_adds: field_int(ctx, r, "numAdds")?,
            r_changed: false,
        };
        kern::poly1305_blocks(&mut p, &bytes);
        write_longs(ctx, a_limbs, &p.a);
        ctx.set_field_by_name(a, "numAdds", Value::Int(p.a_num_adds));
        if p.r_changed {
            write_longs(ctx, r_limbs, &p.r);
            ctx.set_field_by_name(r, "numAdds", Value::Int(p.r_num_adds));
        }
    }
    if n_ok < n {
        let from = offset + 16 * n_ok;
        return Err(RuntimeError::IndexOutOfBoundsException {
            message: Some(out_of_bounds_message::check_from_index_size(from, 16, len)),
        }
        .into());
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// Byte-order helpers the engines finish every digest / key stream with
// ---------------------------------------------------------------------------
//
// NOT `@IntrinsicCandidate`s. Their bodies are `byteArrayViewVarHandle`
// accesses, which C2 compiles to single loads and stores; in this VM each
// such access is a generic native call through `lang_invoke`'s byte-view
// path (measured ~1 us per access on the host, cvm-sbr-cryptoeng-1:
// `ChaCha20Cipher.xor` over 1 MiB = 340 ms, a 64-byte SHA-256
// update+digest = 9 us). They are reviewed `Intrinsic`s for the same reason
// the kernels above are — exact semantics, the same observable result — and
// the day VarHandle byte views compile to plain accesses they can go.

/// `ByteArrayAccess.{i2bBig,i2bLittle}(int[] in, int inOfs, byte[] out, int
/// outOfs, int len)` (`big`) and the `long` twin `l2bBig` (`width` 8):
/// `len += outOfs; while (outOfs < len) { VH.set(out, outOfs, in[inOfs++]);
/// outOfs += width; }`.
fn words_to_bytes(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    width: usize,
    big: bool,
) -> MethodCallResult {
    let out_ofs0 = i64::from(arg_int(args, 3));
    let end = out_ofs0 + i64::from(arg_int(args, 4));
    if out_ofs0 >= end {
        return Ok(None);
    }
    let inp = nn(arg_obj(args, 0))?;
    let out = nn(arg_obj(args, 2))?;
    let in_ofs0 = i64::from(arg_int(args, 1));
    let in_len = ctx.array_length(inp);
    let out_len = ctx.array_length(out);
    let words = ((end - out_ofs0) as usize).div_ceil(width);
    // How many words the Java loop completes before its first throw.
    let mut done = 0usize;
    let mut err = None;
    for j in 0..words {
        let io = in_ofs0 + j as i64;
        if io < 0 || io >= in_len as i64 {
            err = Some(aioobe(io, in_len));
            break;
        }
        let oo = out_ofs0 + (j * width) as i64;
        // VarHandle byte views check `index` against `length - (width - 1)`.
        let limit = out_len as i64 - (width as i64 - 1);
        if oo < 0 || oo >= limit {
            err = Some(RuntimeError::aioobe(clamp_i32(oo), clamp_i32(limit)).into());
            break;
        }
        done += 1;
    }
    if done > 0 {
        let mut bytes = Vec::with_capacity(done * width);
        if width == 4 {
            let mut v = vec![0i32; done];
            let n = ctx.read_int_array_into(inp, in_ofs0 as usize, &mut v);
            for (i, slot) in v.iter_mut().enumerate().skip(n) {
                if let Value::Int(x) = ctx.get_array_element(inp, in_ofs0 as usize + i) {
                    *slot = x;
                }
            }
            for w in v {
                bytes.extend_from_slice(&if big {
                    w.to_be_bytes()
                } else {
                    w.to_le_bytes()
                });
            }
        } else {
            let mut raw = vec![0u8; 8 * done];
            let got = ctx.read_primitive_array_bytes(inp, 8 * in_ofs0 as usize, &mut raw);
            for i in 0..done {
                let w = if got == raw.len() {
                    let mut b = [0u8; 8];
                    b.copy_from_slice(&raw[8 * i..8 * i + 8]);
                    i64::from_ne_bytes(b)
                } else {
                    match ctx.get_array_element(inp, in_ofs0 as usize + i) {
                        Value::Long(x) => x,
                        _ => 0,
                    }
                };
                bytes.extend_from_slice(&if big {
                    w.to_be_bytes()
                } else {
                    w.to_le_bytes()
                });
            }
        }
        write_bytes(ctx, out, out_ofs0 as usize, &bytes);
    }
    match err {
        Some(e) => Err(e),
        None => Ok(None),
    }
}

fn native_i2b_big(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    words_to_bytes(ctx, args, 4, true)
}
fn native_i2b_little(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    words_to_bytes(ctx, args, 4, false)
}
fn native_l2b_big(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    words_to_bytes(ctx, args, 8, true)
}

/// `ByteArrayAccess.{i2bBig4,i2bLittle4}(int val, byte[] out, int outOfs)`.
fn int4(ctx: &mut dyn NativeContext, args: &[Value], big: bool) -> MethodCallResult {
    let val = arg_int(args, 0);
    let out = nn(arg_obj(args, 1))?;
    let ofs = i64::from(arg_int(args, 2));
    let limit = ctx.array_length(out) as i64 - 3;
    if ofs < 0 || ofs >= limit {
        return Err(RuntimeError::aioobe(clamp_i32(ofs), clamp_i32(limit)).into());
    }
    let b = if big {
        val.to_be_bytes()
    } else {
        val.to_le_bytes()
    };
    write_bytes(ctx, out, ofs as usize, &b);
    Ok(None)
}

fn native_i2b_big4(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    int4(ctx, args, true)
}
fn native_i2b_little4(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    int4(ctx, args, false)
}

/// `ChaCha20Cipher.xor(byte[] in1, int off1, byte[] in2, int off2, byte[]
/// out, int outOff, int len)`: eight bytes at a time through a
/// little-endian long view (both reads, then the write), then byte by byte.
/// Run literally over buffers loaded per distinct array, so any aliasing
/// among the three arrays behaves as the loop does.
fn native_chacha_xor(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let len0 = arg_int(args, 6);
    if len0 <= 0 {
        return Ok(None);
    }
    let arrs = [
        nn(arg_obj(args, 0))?,
        nn(arg_obj(args, 2))?,
        nn(arg_obj(args, 4))?,
    ];
    let offs = [
        i64::from(arg_int(args, 1)),
        i64::from(arg_int(args, 3)),
        i64::from(arg_int(args, 5)),
    ];
    let len = i64::from(len0);
    let alens = [
        ctx.array_length(arrs[0]),
        ctx.array_length(arrs[1]),
        ctx.array_length(arrs[2]),
    ];
    // Distinct arrays, each loaded over the union of the ranges its operands
    // can touch (clamped to the array).
    let mut owner = [0usize; 3];
    let mut bufs: Vec<(ObjectRef, i64, Vec<u8>)> = Vec::new();
    let mut ranges: Vec<(i64, i64)> = Vec::new();
    for k in 0..3 {
        let lo = offs[k].clamp(0, alens[k] as i64);
        let hi = (offs[k] + len).clamp(0, alens[k] as i64);
        match bufs.iter().position(|(a, _, _)| *a == arrs[k]) {
            Some(i) => {
                owner[k] = i;
                ranges[i].0 = ranges[i].0.min(lo);
                ranges[i].1 = ranges[i].1.max(hi);
            }
            None => {
                owner[k] = bufs.len();
                bufs.push((arrs[k], 0, Vec::new()));
                ranges.push((lo, hi));
            }
        }
    }
    for (i, (a, base, buf)) in bufs.iter_mut().enumerate() {
        let (lo, hi) = ranges[i];
        *base = lo;
        *buf = read_bytes(ctx, *a, lo as usize, (hi - lo) as usize);
    }
    let (mut o1, mut o2, mut oo) = (offs[0], offs[1], offs[2]);
    let mut rem = len;
    let mut err: Option<MethodCallFailed> = None;
    let get = |bufs: &Vec<(ObjectRef, i64, Vec<u8>)>, k: usize, at: i64| -> u8 {
        let (_, base, buf) = &bufs[owner[k]];
        buf[(at - base) as usize]
    };
    while rem >= 8 {
        let lim1 = alens[0] as i64 - 7;
        if o1 < 0 || o1 >= lim1 {
            err = Some(RuntimeError::aioobe(clamp_i32(o1), clamp_i32(lim1)).into());
            break;
        }
        let lim2 = alens[1] as i64 - 7;
        if o2 < 0 || o2 >= lim2 {
            err = Some(RuntimeError::aioobe(clamp_i32(o2), clamp_i32(lim2)).into());
            break;
        }
        let mut v = [0u8; 8];
        for (i, slot) in v.iter_mut().enumerate() {
            *slot = get(&bufs, 0, o1 + i as i64) ^ get(&bufs, 1, o2 + i as i64);
        }
        let limo = alens[2] as i64 - 7;
        if oo < 0 || oo >= limo {
            err = Some(RuntimeError::aioobe(clamp_i32(oo), clamp_i32(limo)).into());
            break;
        }
        let (_, base, buf) = &mut bufs[owner[2]];
        let at = (oo - *base) as usize;
        buf[at..at + 8].copy_from_slice(&v);
        o1 += 8;
        o2 += 8;
        oo += 8;
        rem -= 8;
    }
    if err.is_none() {
        while rem > 0 {
            if o1 < 0 || o1 >= alens[0] as i64 {
                err = Some(aioobe(o1, alens[0]));
                break;
            }
            if o2 < 0 || o2 >= alens[1] as i64 {
                err = Some(aioobe(o2, alens[1]));
                break;
            }
            let b = get(&bufs, 0, o1) ^ get(&bufs, 1, o2);
            if oo < 0 || oo >= alens[2] as i64 {
                err = Some(aioobe(oo, alens[2]));
                break;
            }
            let (_, base, buf) = &mut bufs[owner[2]];
            buf[(oo - *base) as usize] = b;
            o1 += 1;
            o2 += 1;
            oo += 1;
            rem -= 1;
        }
    }
    // Write back the output array's loaded range (bytes the loop did not
    // write are written back unchanged).
    let (a, base, buf) = &bufs[owner[2]];
    let (a, base, buf) = (*a, *base, buf.clone());
    write_bytes(ctx, a, base as usize, &buf);
    match err {
        Some(e) => Err(e),
        None => Ok(None),
    }
}

// ---------------------------------------------------------------------------
// Registration
// ---------------------------------------------------------------------------

/// Every triple this module registers, in registration order. The unit test
/// below and the lane record count these. The first [`LEAF_COUNT`] never
/// allocate on the Java heap, never call Java and never block, so they are
/// registered as leaves (the funnel without its GC bookkeeping; see
/// `NativeMethodRegistry::set_leaf` for the contract). The rest can call the
/// embedded cipher's or the digest's own bytecode (their non-AES / non-kernel
/// fallbacks) and take the full funnel.
pub const JDK_CRYPTO_INTRINSIC_TRIPLES: &[(&str, &str, &str)] = &[
    // leaves
    ("sun/security/provider/SHA", "implCompress0", "([BI)V"),
    ("sun/security/provider/SHA2", "implCompress0", "([BI)V"),
    ("sun/security/provider/SHA5", "implCompress0", "([BI)V"),
    ("sun/security/provider/MD5", "implCompress0", "([BI)V"),
    ("sun/security/provider/SHA3", "implCompress0", "([BI)V"),
    (
        "com/sun/crypto/provider/AESCrypt",
        "implEncryptBlock",
        "([BI[BI)V",
    ),
    (
        "com/sun/crypto/provider/AESCrypt",
        "implDecryptBlock",
        "([BI[BI)V",
    ),
    (
        "com/sun/crypto/provider/GHASH",
        "processBlocks",
        "([BII[J[J)V",
    ),
    (
        "com/sun/crypto/provider/ChaCha20Cipher",
        "implChaCha20Block",
        "([I[B)I",
    ),
    (
        "com/sun/crypto/provider/Poly1305",
        "processMultipleBlocks",
        "([BII[J[J)V",
    ),
    (
        "sun/security/provider/ByteArrayAccess",
        "i2bBig",
        "([II[BII)V",
    ),
    (
        "sun/security/provider/ByteArrayAccess",
        "i2bBig4",
        "(I[BI)V",
    ),
    (
        "sun/security/provider/ByteArrayAccess",
        "i2bLittle",
        "([II[BII)V",
    ),
    (
        "sun/security/provider/ByteArrayAccess",
        "i2bLittle4",
        "(I[BI)V",
    ),
    (
        "sun/security/provider/ByteArrayAccess",
        "l2bBig",
        "([JI[BII)V",
    ),
    (
        "com/sun/crypto/provider/ChaCha20Cipher",
        "xor",
        "([BI[BI[BII)V",
    ),
    // full funnel
    (
        "sun/security/provider/DigestBase",
        "implCompressMultiBlock0",
        "([BII)I",
    ),
    (
        "com/sun/crypto/provider/CounterMode",
        "implCrypt",
        "([BII[BI)I",
    ),
    (
        "com/sun/crypto/provider/CipherBlockChaining",
        "implEncrypt",
        "([BII[BI)I",
    ),
    (
        "com/sun/crypto/provider/CipherBlockChaining",
        "implDecrypt",
        "([BII[BI)I",
    ),
    (
        "com/sun/crypto/provider/ElectronicCodeBook",
        "implECBEncrypt",
        "([BII[BI)I",
    ),
    (
        "com/sun/crypto/provider/ElectronicCodeBook",
        "implECBDecrypt",
        "([BII[BI)I",
    ),
];

/// How many of [`JDK_CRYPTO_INTRINSIC_TRIPLES`] are leaves.
const LEAF_COUNT: usize = 16;

type NativeFn = fn(&mut dyn NativeContext, &[Value]) -> MethodCallResult;

const HANDLERS: &[NativeFn] = &[
    native_sha_compress0,
    native_sha2_compress0,
    native_sha5_compress0,
    native_md5_compress0,
    native_sha3_compress0,
    native_aes_encrypt_block,
    native_aes_decrypt_block,
    native_ghash_process_blocks,
    native_chacha20_block,
    native_poly1305_blocks,
    native_i2b_big,
    native_i2b_big4,
    native_i2b_little,
    native_i2b_little4,
    native_l2b_big,
    native_chacha_xor,
    native_digest_multi_block0,
    native_ctr_impl_crypt,
    native_cbc_encrypt,
    native_cbc_decrypt,
    native_ecb_encrypt,
    native_ecb_decrypt,
];

/// Register the reviewed `Intrinsic`s. States its own kind (and leaf claim)
/// and restores the ambient category on exit (AGENTS.md).
pub fn register_jdk_crypto_intrinsics(registry: &mut NativeMethodRegistry) {
    registry.with_category(NativeKind::Intrinsic, |registry| {
        for (i, ((class, name, desc), f)) in JDK_CRYPTO_INTRINSIC_TRIPLES
            .iter()
            .zip(HANDLERS)
            .enumerate()
        {
            let leaf = i < LEAF_COUNT;
            registry.with_leaf(leaf, |registry| {
                registry.register(class, name, desc, *f);
            });
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_triple_has_a_handler_and_is_an_intrinsic() {
        assert_eq!(JDK_CRYPTO_INTRINSIC_TRIPLES.len(), HANDLERS.len());
        let mut r = NativeMethodRegistry::new();
        let before = r.current_category();
        let leaf_before = r.current_leaf();
        register_jdk_crypto_intrinsics(&mut r);
        assert_eq!(r.current_category(), before, "ambient category restored");
        assert_eq!(r.current_leaf(), leaf_before, "leaf claim restored");
        for (c, m, d) in JDK_CRYPTO_INTRINSIC_TRIPLES {
            assert_eq!(
                r.kind_of(c, m, d),
                Some(NativeKind::Intrinsic),
                "{c}.{m}{d} must be a reviewed Intrinsic"
            );
        }
    }
}
