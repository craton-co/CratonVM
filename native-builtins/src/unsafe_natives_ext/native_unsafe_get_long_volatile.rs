#[allow(unused_imports)]
use super::*;

/// `Unsafe.getLongVolatile`; see [`native_unsafe_get_int_volatile_arena`].
pub fn native_unsafe_get_long_volatile_arena(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let Some(raw) = unsafe_arena_volatile_load(args, 8) {
        return Ok(Some(Value::Long(raw as i64)));
    }
    if let Some(raw) = unsafe_wide_raw_load(ctx, args, cratonvm_types::ArrayElementType::Long, 8) {
        return Ok(Some(Value::Long(raw as i64)));
    }
    // Wave 127 (lane L4): a `double` field's bits, as `getLong` answers; see
    // `native_unsafe_get_int_volatile_arena` (`getAndSetDouble` /
    // `getAndAddDouble`). `getDoubleVolatile` keeps the unwrapped native.
    Ok(native_unsafe_get_long_volatile(ctx, args)?.map(|v| match v {
        Value::Double(d) => Value::Long(d.to_bits() as i64),
        other => other,
    }))
}

/// `Unsafe.putLongVolatile`; see [`native_unsafe_get_int_volatile_arena`].
pub fn native_unsafe_put_long_volatile_arena(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let Some(Value::Long(v)) = args.get(3) {
        if unsafe_arena_volatile_store(args, 8, *v as u64) {
            return Ok(None);
        }
    }
    if unsafe_wide_raw_store(ctx, args, cratonvm_types::ArrayElementType::Long, 8) {
        return Ok(None);
    }
    native_unsafe_put_long_volatile(ctx, args)
}

/// Round nio20 wave 2, lane L4 (proposal L2-4 of
/// `docs/feature-designs/jit-ir-optimizations.md`): an int- or long-width
/// `Unsafe` access whose base is a primitive array of ANOTHER element type
/// (`getLongVolatile(byte[], ..)` from a byte-array view `VarHandle`,
/// `compareAndSetInt(float[], ..)` from `compareAndSetFloat`,
/// `putIntVolatile(long[], ..)`), or that straddles two elements. HotSpot
/// reads and writes the raw little-endian bytes; the width-matched volatile and
/// CAS natives decoded `offset` as an index of the array's own element type and
/// read / compared / stored ONE element (a byte, a float `Value`), so the plain
/// accessors (`*_mb`, already raw) and the volatile / atomic ones disagreed.
///
/// `Some((array, offset))` for that shape; `None` keeps the caller's
/// element-typed path (arena handles, fields, a width-matched aligned element).
pub fn unsafe_wide_raw_target(
    ctx: &dyn NativeContext,
    args: &[Value],
    natural: cratonvm_types::ArrayElementType,
    width: usize,
) -> Option<(cratonvm_types::ObjectRef, usize)> {
    let obj = unsafe_obj(args, 1)?;
    let offset = unsafe_offset(args, 2);
    if unsafe_synthetic_field_offset(&*ctx, obj, offset) {
        return None;
    }
    unsafe_array_access_is_raw(ctx, obj, natural, offset, width).then_some((obj, offset))
}

/// The little-endian raw bits of `width` (4 or 8) bytes for a
/// [`unsafe_wide_raw_target`] load; `None` when out of bounds.
pub fn unsafe_wide_raw_load(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    natural: cratonvm_types::ArrayElementType,
    width: usize,
) -> Option<u64> {
    let (obj, off) = unsafe_wide_raw_target(ctx, args, natural, width)?;
    // A naturally aligned access is one hardware-atomic load of the payload
    // (single-copy atomic against compiled code's stores, as HotSpot's).
    if let Some((cur, _)) = unsafe_wide_payload_atomic(ctx, obj, off, width, &|_| None) {
        return Some(cur);
    }
    let mut b = [0u8; UNSAFE_MAX_ACCESS_WIDTH];
    if !unsafe_read_bytes_into(ctx, obj, off, width, &mut b) {
        return None;
    }
    Some(u64::from_le_bytes(b))
}

/// The store half of [`unsafe_wide_raw_load`]: `args[3]`'s raw bits written as
/// `width` little-endian bytes. `true` when it wrote.
pub fn unsafe_wide_raw_store(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    natural: cratonvm_types::ArrayElementType,
    width: usize,
) -> bool {
    let Some((obj, off)) = unsafe_wide_raw_target(ctx, args, natural, width) else {
        return false;
    };
    let bits = unsafe_wide_arg_bits(args, 3, width);
    if unsafe_wide_payload_atomic(ctx, obj, off, width, &|_| Some(bits)).is_some() {
        return true;
    }
    unsafe_array_write_bytes(ctx, obj, off, &bits.to_le_bytes()[..width])
}

/// The hardware-atomic arm of every raw wide access:
/// `NativeHeapAccess::primitive_array_atomic_update` at payload byte
/// `off - ABASE`. `None` (misaligned, no flat payload, out of range) sends the
/// caller to its byte-copy / locked fallback.
pub fn unsafe_wide_payload_atomic(
    ctx: &mut dyn NativeContext,
    obj: cratonvm_types::ObjectRef,
    off: usize,
    width: usize,
    f: &dyn Fn(u64) -> Option<u64>,
) -> Option<(u64, bool)> {
    const ABASE: usize = 16;
    let rel = off.checked_sub(ABASE)?;
    ctx.primitive_array_atomic_update(obj, rel, width, f)
}

/// `args[i]` as raw bits masked to `width` bytes (an `Int`, `Long`, `Float` or
/// `Double` operand).
pub fn unsafe_wide_arg_bits(args: &[Value], i: usize, width: usize) -> u64 {
    let bits = match args.get(i) {
        Some(Value::Int(v)) => *v as u32 as u64,
        Some(Value::Long(v)) => *v as u64,
        Some(Value::Float(f)) => u64::from(f.to_bits()),
        Some(Value::Double(d)) => d.to_bits(),
        _ => 0,
    };
    if width >= 8 {
        bits
    } else {
        bits & ((1u64 << (width * 8)) - 1)
    }
}

/// A compare-and-set of `width` raw bytes at `off` of a primitive array:
/// `Some((witnessed, swapped))`, `None` when out of bounds. One process-wide
/// lock makes it atomic against itself (and so against every other raw CAS,
/// including the JDK's `getAndAdd*` loops over it), not against a concurrent
/// element-typed store into the same bytes -- the [`SubwordLane`] straddle
/// rule. Not compatibility state: a lock, no data.
pub fn unsafe_wide_raw_cas(
    ctx: &mut dyn NativeContext,
    obj: cratonvm_types::ObjectRef,
    off: usize,
    width: usize,
    expected: u64,
    new: u64,
) -> Option<(u64, bool)> {
    unsafe_wide_raw_update(ctx, obj, off, width, &|cur| {
        (cur == expected).then_some(new)
    })
}

/// Interpreter round i6 wave 127, lane L4: the raw-bytes arm of an int- or
/// long-width `Unsafe.compareAndExchange*` (args `[unsafe, base, offset,
/// expected, new]`), for the natives outside this module that lacked it
/// (`unsafe_natives::cae_int_impl` / `cae_long_impl`, the registrations that
/// win): `compareAndExchangeFloat` over a `float[]` is
/// `compareAndExchangeInt` with the raw bits, and the element-typed path
/// compared those bits against a `Value::Float` element. `Some(witness bits)`
/// when the shape is a raw one ([`unsafe_wide_raw_target`]), else `None`.
pub fn unsafe_wide_raw_cae(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    natural: cratonvm_types::ArrayElementType,
    width: usize,
) -> Option<u64> {
    let (obj, off) = unsafe_wide_raw_target(&*ctx, args, natural, width)?;
    let (ex, nv) = (
        unsafe_wide_arg_bits(args, 3, width),
        unsafe_wide_arg_bits(args, 4, width),
    );
    unsafe_wide_raw_cas(ctx, obj, off, width, ex, nv).map(|(cur, _)| cur)
}

/// `getAndAdd` / `getAndSet` over raw bytes: replaces the `width` bytes with
/// `f(current)` (masked to `width`) and answers the previous value; `None`
/// when out of bounds. Same lock as [`unsafe_wide_raw_cas`].
pub fn unsafe_wide_raw_rmw(
    ctx: &mut dyn NativeContext,
    obj: cratonvm_types::ObjectRef,
    off: usize,
    width: usize,
    f: &dyn Fn(u64) -> u64,
) -> Option<u64> {
    unsafe_wide_raw_update(ctx, obj, off, width, &|cur| Some(f(cur))).map(|(old, _)| old)
}

/// The one locked read-compute-write both raw atomics share: `f(current)`
/// `None` leaves the bytes. Answers `(witnessed, wrote)`.
pub fn unsafe_wide_raw_update(
    ctx: &mut dyn NativeContext,
    obj: cratonvm_types::ObjectRef,
    off: usize,
    width: usize,
    f: &dyn Fn(u64) -> Option<u64>,
) -> Option<(u64, bool)> {
    // Naturally aligned (the only shape a byte-array / ByteBuffer view
    // `VarHandle` admits for an atomic mode -- it throws IllegalStateException
    // on a misaligned one before reaching `Unsafe` -- and every `float[]` /
    // `double[]` element): a real `lock cmpxchg` on the payload, atomic against
    // compiled code's CAS / xadd on the same bytes.
    if let Some(r) = unsafe_wide_payload_atomic(ctx, obj, off, width, f) {
        return Some(r);
    }
    // nio21 wave 6 L3: on x86-64 the arm above now serves a MISALIGNED raw
    // `Unsafe` access too, as HotSpot's `lock cmpxchg` at that address. What
    // reaches here is a payload with no flat address, a split lock (an access
    // crossing a cache line, unless `CRATONVM_UNSAFE_SPLIT_LOCK_CAS=0`),
    // another architecture, or `CRATONVM_UNSAFE_MISALIGNED_HW_CAS=0`: atomic
    // only against other calls of this path. Not compatibility state: a lock,
    // no data.
    static WIDE_RAW: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _g = WIDE_RAW.lock().unwrap_or_else(|e| e.into_inner());
    let mut b = [0u8; UNSAFE_MAX_ACCESS_WIDTH];
    if !unsafe_read_bytes_into(ctx, obj, off, width, &mut b) {
        return None;
    }
    let cur = u64::from_le_bytes(b);
    let Some(new) = f(cur) else {
        return Some((cur, false));
    };
    // JIT round 21, lane atomics (UNSAFE21-3; correctness, unconditional):
    // write back only the bytes the update CHANGES. This lock is atomic
    // against other calls of this path only; a plain element store (an
    // interpreted or compiled `bastore`, `set_array_element`) into the same
    // bytes takes no lock and cannot be made to. Writing all `width` bytes put
    // the stale copy of an UNCHANGED byte back over such a store
    // (`R20AtomSplitLock` under `CRATONVM_UNSAFE_SPLIT_LOCK_CAS=0`: the CAS
    // increments the low 24 bits, the marker in the top byte was lost). Not
    // writing an unchanged byte is a correct linearisation (the update before
    // the store), so only a store into a byte the update itself changes can
    // still be overwritten -- the nature of a lock plain stores do not take;
    // the default split-lock arm above has no such window.
    let Some((first, last)) = unsafe_changed_byte_span(cur, new, width) else {
        return Some((cur, true));
    };
    if !unsafe_array_write_bytes(ctx, obj, off + first, &new.to_le_bytes()[first..=last]) {
        return None;
    }
    Some((cur, true))
}

/// JIT round 21, lane atomics (UNSAFE21-3): the first and last of the `width`
/// little-endian bytes in which `old` and `new` differ, or `None` when the
/// update changes nothing. The process-lock arms write back only that span.
pub fn unsafe_changed_byte_span(old: u64, new: u64, width: usize) -> Option<(usize, usize)> {
    let (o, n) = (old.to_le_bytes(), new.to_le_bytes());
    let width = width.min(8);
    let first = (0..width).find(|&i| o[i] != n[i])?;
    let last = (first..width).rev().find(|&i| o[i] != n[i])?;
    Some((first, last))
}

/// SBR3 lane unsafecas: a BYTE LANE of a heap array, the target of a byte/short
/// `Unsafe` access that is not the array's element width: a byte or short inside
/// an `int[]`/`long[]`/`short[]`/`char[]` element, or a short across two
/// `byte[]`/`boolean[]` elements. HotSpot names it by the byte offset
/// `ABASE + n` (little-endian within an element).
///
/// This VM's heap FIELDS are one slot each (a byte field is already its own
/// `Int` cell, so the int-width paths are exact for it) but its arrays are typed
/// element cells addressed `ABASE + index * scale`: the int-width natives read
/// `offset - ABASE` as an element index for the ELEMENT's width and stored the
/// byte into the wrong lane (`compareAndSetByte(int[], base + 1, 0, 0x7f)` left
/// `0x7f`, HotSpot `0x7f00`), and `getByteVolatile` on raw memory read a side
/// store, which made the JDK's `getAndSetByte` loop (volatile load + CAS) spin.
#[derive(Clone, Copy)]
pub struct SubwordLane {
    pub obj: cratonvm_types::ObjectRef,
    pub elem: cratonvm_types::ArrayElementType,
    /// Element size in bytes.
    pub scale: usize,
    /// Access width in bytes (1 or 2).
    pub width: usize,
    /// Bytes from element 0.
    pub byte_off: usize,
}

impl SubwordLane {
    /// `None` when the call is not that shape: not a heap array, a width other
    /// than 1 or 2, an element of the access width (the width-matched path is
    /// exact), a float/double/reference element, an offset below `ABASE` or past
    /// the end, or a lane straddling two wide elements.
    pub fn locate(
        ctx: &dyn NativeContext,
        obj: cratonvm_types::ObjectRef,
        offset: usize,
        width: usize,
    ) -> Option<Self> {
        use cratonvm_types::ArrayElementType as E;
        if !(1..=2).contains(&width) || ctx.heap_kind_of(obj) != cratonvm_types::ObjectKind::Array {
            return None;
        }
        let elem = ctx.heap_element_type_of(obj);
        let scale = match elem {
            E::Boolean | E::Byte => 1usize,
            E::Char | E::Short => 2,
            E::Int => 4,
            E::Long => 8,
            E::Float | E::Double | E::Reference => return None,
        };
        if scale == width {
            return None;
        }
        // ABASE is 16 (`native_unsafe_array_base_offset`).
        const ABASE: usize = 16;
        let byte_off = offset.checked_sub(ABASE)?;
        if byte_off.checked_add(width)? > ctx.array_length(obj).checked_mul(scale)? {
            return None;
        }
        if scale > width && (byte_off % scale) + width > scale {
            return None;
        }
        Some(SubwordLane {
            obj,
            elem,
            scale,
            width,
            byte_off,
        })
    }

    pub fn mask(&self) -> u64 {
        (1u64 << (self.width * 8)) - 1
    }

    pub fn elem_bits(&self, v: &Value) -> Option<u64> {
        match v {
            Value::Int(i) => Some(*i as u32 as u64 & ((1u64 << (self.scale * 8)) - 1)),
            Value::Long(l) => Some(*l as u64),
            _ => None,
        }
    }

    /// The lane's value, zero-extended.
    pub fn load(&self, ctx: &dyn NativeContext) -> Option<u64> {
        if self.scale > self.width {
            let bits =
                self.elem_bits(&ctx.get_array_element(self.obj, self.byte_off / self.scale))?;
            return Some((bits >> ((self.byte_off % self.scale) * 8)) & self.mask());
        }
        let mut cur = 0u64;
        for i in 0..self.width {
            let b = self.elem_bits(&ctx.get_array_element(self.obj, self.byte_off + i))? & 0xff;
            cur |= b << (8 * i);
        }
        Some(cur)
    }

    /// Atomically replace the lane with `f(current)` (`None` = leave it). Returns
    /// the value witnessed and whether `f` wrote. Over a wide element the swap is
    /// a CAS loop on the whole element (atomic against every other element CAS);
    /// a short over two byte elements takes one process-wide lock (atomic against
    /// itself, not against a concurrent byte store, as for a misaligned word on a
    /// CPU without a locked misaligned RMW).
    pub fn rmw(
        &self,
        ctx: &mut dyn NativeContext,
        f: &dyn Fn(u64) -> Option<u64>,
    ) -> Option<(u64, bool)> {
        use cratonvm_types::ArrayElementType as E;
        if self.scale > self.width {
            let idx = self.byte_off / self.scale;
            let shift = (self.byte_off % self.scale) * 8;
            let mut attempts = 0usize;
            loop {
                let old = ctx.get_array_element(self.obj, idx);
                let bits = self.elem_bits(&old)?;
                let cur = (bits >> shift) & self.mask();
                let Some(upd) = f(cur) else {
                    return Some((cur, false));
                };
                let new_bits = (bits & !(self.mask() << shift)) | ((upd & self.mask()) << shift);
                let new = match self.elem {
                    E::Long => Value::Long(new_bits as i64),
                    E::Int => Value::Int(new_bits as u32 as i32),
                    E::Short => Value::Int(new_bits as u16 as i16 as i32),
                    E::Char => Value::Int(new_bits as u16 as i32),
                    _ => return None,
                };
                if ctx.compare_and_swap_field(self.obj, idx, old, new) {
                    return Some((cur, true));
                }
                attempts = attempts.wrapping_add(1);
                // JIT round 21 (lane unsafe): bounded as `FieldLane::rmw` is --
                // an element whose stored shape never equals what the read
                // answers ends with the plain store, never a spin.
                // JIT round 21 (lane revvm; correctness, unconditional): only
                // when the element's bits did not change since the read --
                // a failure caused by another thread's CAS is its progress,
                // which a blind store would overwrite (a lost update).
                if attempts >= 1 << 16 {
                    if self.elem_bits(&ctx.get_array_element(self.obj, idx)) == Some(bits) {
                        ctx.set_array_element(self.obj, idx, new);
                        return Some((cur, true));
                    }
                    attempts = 0;
                }
                if attempts % 1024 == 0 {
                    std::thread::yield_now();
                }
            }
        }
        // nio21 wave 6 L3: a short over two byte elements is one 2-byte
        // hardware atomic on the payload when the VM can serve it (aligned:
        // a plain `AtomicU16`; misaligned: x86-64 `LOCK CMPXCHG`, as HotSpot),
        // so it is atomic against a concurrent byte store too. The lock below
        // is the fallback (no flat payload, another architecture, a split
        // lock, `CRATONVM_UNSAFE_MISALIGNED_HW_CAS=0`).
        if let Some(r) = ctx.primitive_array_atomic_update(self.obj, self.byte_off, self.width, f) {
            return Some(r);
        }
        static STRADDLE: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _g = STRADDLE.lock().unwrap_or_else(|e| e.into_inner());
        let cur = self.load(ctx)?;
        let Some(upd) = f(cur) else {
            return Some((cur, false));
        };
        for i in 0..self.width {
            let b = (upd >> (8 * i)) & 0xff;
            // JIT round 21, lane atomics (UNSAFE21-3; correctness,
            // unconditional): an unchanged byte is not written back, so a
            // plain store into it (which takes no lock) is not overwritten
            // with the stale copy; see `unsafe_wide_raw_update`.
            if (cur >> (8 * i)) & 0xff == b {
                continue;
            }
            let v = if self.elem == E::Boolean {
                b as i32
            } else {
                b as u8 as i8 as i32
            };
            ctx.set_array_element(self.obj, self.byte_off + i, Value::Int(v));
        }
        Some((cur, true))
    }

    /// The witnessed lane as the `B`/`S` the JDK method returns (sign-extended).
    pub fn signed(&self, v: u64) -> Value {
        Value::Int(if self.width == 1 {
            v as u8 as i8 as i32
        } else {
            v as u16 as i16 as i32
        })
    }
}

/// The byte/short atomics of `Unsafe` (`Cas`, `Cae`, `Add`, `Set`) over an
/// array byte lane; see [`SubwordLane`]. `None`: not that shape, the caller
/// keeps the width-matched path. `args` as in [`unsafe_arena_atomic`].
pub fn unsafe_subword_array_atomic(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    op: ArenaAtomic,
    width: usize,
) -> Option<Value> {
    let obj = unsafe_obj(args, 1)?;
    if width == 2 {
        if let Some(v) = unsafe_char_array_short_atomic(ctx, obj, args, op) {
            return Some(v);
        }
    }
    let lane = SubwordLane::locate(ctx, obj, unsafe_offset(args, 2), width)?;
    let mask = lane.mask();
    let arg = |i: usize| -> u64 {
        match args.get(i) {
            Some(Value::Int(v)) => *v as i64 as u64 & mask,
            Some(Value::Long(v)) => *v as u64 & mask,
            _ => 0,
        }
    };
    let (a, b) = (arg(3), arg(4));
    let (cur, ok) = match op {
        ArenaAtomic::Cas | ArenaAtomic::Cae => lane.rmw(ctx, &|cur| (cur == a).then_some(b))?,
        ArenaAtomic::Add => lane.rmw(ctx, &|cur| Some(cur.wrapping_add(a) & mask))?,
        ArenaAtomic::Set => lane.rmw(ctx, &|_| Some(a))?,
    };
    Some(if op == ArenaAtomic::Cas {
        Value::Int(i32::from(ok))
    } else {
        lane.signed(cur)
    })
}

/// JIT round 21 (lane unsafe): the short-width atomics on a `char[]` ELEMENT.
/// The JDK's `compareAndSetChar` / `getAndAddChar` / `getAndSetChar` reach the
/// `Short` natives with the char narrowed to a SIGNED short, while a char
/// element holds the value ZERO-extended, so the element CAS of a char >=
/// 0x8000 never matched and the JDK's getAndAdd / getAndSet retry loops spun
/// (the instance-field twin is [`char_field_short_atomic`]). Compares the low
/// 16 bits, stores the char zero-extended, answers the witness as a short.
/// Bounded like [`FieldLane::rmw`]. `None` for anything but a `char[]`.
pub fn unsafe_char_array_short_atomic(
    ctx: &mut dyn NativeContext,
    obj: cratonvm_types::ObjectRef,
    args: &[Value],
    op: ArenaAtomic,
) -> Option<Value> {
    if ctx.heap_kind_of(obj) != cratonvm_types::ObjectKind::Array
        || ctx.heap_element_type_of(obj) != cratonvm_types::ArrayElementType::Char
    {
        return None;
    }
    let offset = unsafe_offset(args, 2);
    // Only an element's own (2-aligned) offset; anything else keeps its path.
    if offset < 16 || (offset - 16) % 2 != 0 {
        return None;
    }
    let idx = unsafe_checked_array_index(&*ctx, obj, offset)?;
    let arg16 = |i: usize| -> u64 {
        // Cast: the operand's low 16 bits.
        match args.get(i) {
            Some(Value::Int(v)) => u64::from(*v as u32 & 0xFFFF),
            _ => 0,
        }
    };
    let (a, b) = (arg16(3), arg16(4));
    let mut attempts = 0u32;
    loop {
        let old = ctx.get_array_element(obj, idx);
        let cur = match old {
            Value::Int(v) => u64::from(v as u32 & 0xFFFF),
            _ => return None,
        };
        let upd = match op {
            ArenaAtomic::Cas | ArenaAtomic::Cae => {
                if cur != a {
                    return Some(if op == ArenaAtomic::Cas {
                        Value::Int(0)
                    } else {
                        Value::Int(cur as u16 as i16 as i32)
                    });
                }
                b
            }
            ArenaAtomic::Add => cur.wrapping_add(a) & 0xFFFF,
            ArenaAtomic::Set => a,
        };
        // Cast: the char, zero-extended as a char element holds it.
        let new = Value::Int(upd as u16 as i32);
        attempts += 1;
        // JIT round 21 (lane revvm; correctness, unconditional): the bounded
        // plain store only for an element that still holds `old` (a stuck
        // CAS), and never after a CAS that succeeded; a failure caused by
        // another thread's update is retried, not overwritten.
        let swapped = ctx.compare_and_swap_field(obj, idx, old, new);
        let stuck = !swapped && attempts >= 1 << 16 && ctx.get_array_element(obj, idx) == old;
        if stuck {
            ctx.set_array_element(obj, idx, new);        } else if !swapped && attempts >= 1 << 16 {
            attempts = 0;
        }
        if swapped || stuck {
            return Some(if op == ArenaAtomic::Cas {
                Value::Int(1)
            } else {
                Value::Int(cur as u16 as i16 as i32)
            });
        }
    }
}

/// A plain or volatile `put{Byte,Short,Char,Boolean}` over an array byte lane:
/// `bytes` is the value in memory order (1 or 2 bytes). `true` when it wrote.
pub fn unsafe_subword_array_store(
    ctx: &mut dyn NativeContext,
    obj: cratonvm_types::ObjectRef,
    offset: usize,
    bytes: &[u8],
) -> bool {
    let Some(lane) = SubwordLane::locate(ctx, obj, offset, bytes.len()) else {
        return false;
    };
    let v = bytes
        .iter()
        .enumerate()
        .fold(0u64, |acc, (i, b)| acc | (u64::from(*b) << (8 * i)));
    lane.rmw(ctx, &|_| Some(v)).is_some()
}

/// Whether a sub-word access of `width` bytes at `(args[1], args[2])` is raw
/// (arena) memory or an array byte lane, i.e. a shape the width-matched
/// volatile natives (which index an element or read a side store) get wrong.
pub fn unsafe_subword_is_real_bytes(ctx: &dyn NativeContext, args: &[Value], width: usize) -> bool {
    match unsafe_obj(args, 1) {
        None => unsafe_arena_names_block(unsafe_raw_addr(args, 2)),
        Some(o) => SubwordLane::locate(ctx, o, unsafe_offset(args, 2), width).is_some(),
    }
}

/// JIT round 20 wave 12 (lane qUnsafe, fuzz `R20FuzzEUnsafeArr1`): is the
/// base a `boolean[]`? Its 1-byte volatile accesses take the raw byte
/// accessors (`native_unsafe_{get,put}_byte_mb`), not the element natives,
/// whose boolean element store narrows to `v & 1` and whose load
/// zero-extends; HotSpot moves the raw byte.
pub fn unsafe_is_boolean_array(ctx: &dyn NativeContext, args: &[Value]) -> bool {
    unsafe_obj(args, 1).is_some_and(|o| {
        ctx.heap_kind_of(o) == cratonvm_types::ObjectKind::Array
            && matches!(
                ctx.heap_element_type_of(o),
                cratonvm_types::ArrayElementType::Boolean
            )
    })
}

unsafe_subword_volatile!(
    native_unsafe_get_byte_volatile,
    native_unsafe_put_byte_volatile,
    native_unsafe_get_byte_mb,
    native_unsafe_put_byte_mb,
    1,
    b'B'
);
unsafe_subword_volatile!(
    native_unsafe_get_short_volatile,
    native_unsafe_put_short_volatile,
    native_unsafe_get_short_mb,
    native_unsafe_put_short_mb,
    2,
    b'S'
);
unsafe_subword_volatile!(
    native_unsafe_get_char_volatile,
    native_unsafe_put_char_volatile,
    native_unsafe_get_char_mb,
    native_unsafe_put_char_mb,
    2,
    b'C'
);

/// `Unsafe.compareAndSetByte`: [`unsafe_arena_atomic`] over an arena handle,
/// [`unsafe_subword_array_atomic`] over an array lane, else the int-width
/// native (a heap field's slot holds the byte widened to `Int`).
pub fn native_unsafe_cas_byte(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let Some(v) = unsafe_arena_atomic(&*ctx, args, ArenaAtomic::Cas, 1) {
        return Ok(Some(v));
    }
    // Wave 8c: a lane of an instance field under byte field offsets.
    if let Some(v) = unsafe_field_lane_atomic(ctx, args, ArenaAtomic::Cas, 1) {
        return Ok(Some(v));
    }
    if let Some(v) = unsafe_subword_array_atomic(ctx, args, ArenaAtomic::Cas, 1) {
        return Ok(Some(v));
    }
    native_unsafe_cas_int_element(ctx, args)
}

// ---------------------------------------------------------------------------
// The `Unsafe` FIELD VIEW (interpreter round i4 wave 87, lane L3).
//
// HotSpot field offsets are byte offsets, so an access whose width or type
// differs from the field's reads or writes only the bytes it names:
// `putInt(o, offsetOf(long l), v)` replaces the low half of `l` (little-endian)
// and keeps the high half; `getShort(o, offsetOf(char c))` is the 16 bits
// read as a signed short; `getInt(o, offsetOf(float f))` is its bits. CratonVM
// field offsets are slot indices: the access replaced or narrowed the whole
// slot (`l` lost its high half, `getShort` of a char answered 65535). The view
// decodes the field once by offset (`instance_field_descriptor_byte`, the
// cached descriptor the descriptor-aware `get_field` uses) and moves bytes
// only when the access kind differs from the field's; a same-kind access
// (and every array, static, raw-memory and synthetic-offset access) takes the
// unchanged path.
//
// What the slot model cannot give, chosen and recorded:
// * an access WIDER than the field (`getLong` at an `int` field's offset):
//   HotSpot reads the neighbouring bytes, a layout this VM does not have; the
//   bytes past the field read as 0, and a wider store keeps only the field's
//   own bytes (HotSpot would also overwrite the neighbour);
// * `offset + k` inside a field names ANOTHER slot here (a slot index, not a
//   byte); it is not modelled;
// * a primitive store into a REFERENCE field is IGNORED (HotSpot writes bits
//   into an oop and corrupts the heap; ignoring keeps the slot a reference the
//   collector can trace); a primitive read of one keeps its old answer.
// ---------------------------------------------------------------------------

/// Byte width of a primitive descriptor / access kind.
pub fn field_view_width(kind: u8) -> Option<usize> {
    match kind {
        b'B' | b'Z' => Some(1),
        b'S' | b'C' => Some(2),
        b'I' | b'F' => Some(4),
        b'J' | b'D' => Some(8),
        _ => None,
    }
}

/// `(slot, byte within the slot)` of the INSTANCE field `(args[1], args[2])`
/// names: the offset decoded per this VM's offset scheme
/// ([`unsafe_field_slot`]: slot indices, or `16 + slot * 8 + k` under
/// `CRATONVM_UNSAFE_BYTE_FIELD_OFFSETS`). `None` for a null base, an array, a
/// synthetic (minted / static) offset, the `Buffer.address` sentinel, or a
/// slot past the object's fields.
///
/// Interpreter round i4 wave 95, lane L3, and JIT round 21, lane unsafe, fixed
/// this twice (page
/// `interpreter-i94-orch-dev-jit-round-20-regresses-four-interpreter-probes-FIXED-20261128.md`;
/// the round's dev merge kept this decoder and dropped the round's
/// `unsafe_exact_field_slot`): the view and the `char` short-atomics compared
/// the RAW offset with the field count. Once byte field offsets went default ON
/// (JIT round 20, `b0dc8fb375`) every instance offset (`16 + 8 * slot`) failed
/// that test, the view declined, and a narrow read fell to
/// `native_unsafe_get_int`, whose whole-slot answer no return narrows:
/// `getShort` of a `long` answered its low int, `getChar` of a `short` -2,
/// `getByte` of an `int` the int, and `compareAndSetChar` of a `char` >= 0x8000
/// compared 65535 with -1.
pub fn unsafe_instance_field_target(
    ctx: &dyn NativeContext,
    args: &[Value],
) -> Option<(ObjectRef, usize, usize)> {
    let obj = unsafe_obj(args, 1)?;
    if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
        return None;
    }
    let offset = unsafe_offset(args, 2);
    if offset == BUFFER_ADDRESS_SENTINEL || unsafe_synthetic_field_offset(ctx, obj, offset) {
        return None;
    }
    let (slot, k) = unsafe_field_slot(ctx, obj, offset);
    if slot >= ctx.object_num_fields(obj) {
        return None;
    }
    Some((obj, slot, k))
}

/// The field at `(args[1], args[2])` when `access` differs in kind from it
/// (or, for a big-endian `*Unaligned` access, `big_endian`: the bytes are
/// swapped whatever the kinds; JIT round 21, lane unsafe): `(obj, slot, field
/// kind)`. `None` sends the access down the unchanged path. Only the field's
/// own offset (`k == 0`); an offset inside a field is
/// [`unsafe_field_view_get`]'s sub-field arm or the stores' lane path.
pub fn field_view_target(
    ctx: &dyn NativeContext,
    args: &[Value],
    access: u8,
    big_endian: bool,
) -> Option<(ObjectRef, usize, u8)> {
    let (obj, slot, k) = unsafe_instance_field_target(ctx, args)?;
    if k != 0 {
        return None;
    }
    let field = ctx.instance_field_descriptor_byte(obj, slot)?;
    let same = field == access || (matches!(field, b'B' | b'Z') && matches!(access, b'B' | b'Z'));
    (!same || big_endian).then_some((obj, slot, field))
}

/// The field's current bits, zero-extended from its width.
pub fn field_view_bits(value: Value, field: u8) -> u64 {
    let bits = match value {
        Value::Int(i) => i as u32 as u64,
        Value::Long(l) => l as u64,
        Value::Float(f) => f.to_bits() as u64,
        Value::Double(d) => d.to_bits(),
        _ => 0,
    };
    match field_view_width(field) {
        Some(8) | None => bits,
        Some(w) => bits & ((1u64 << (w * 8)) - 1),
    }
}

/// A value of kind `kind` from its low bits.
pub fn field_view_value(bits: u64, kind: u8) -> Value {
    match kind {
        b'B' => Value::Int(bits as u8 as i8 as i32),
        b'Z' => Value::Int(bits as u8 as i32),
        b'S' => Value::Int(bits as u16 as i16 as i32),
        b'C' => Value::Int(bits as u16 as i32),
        b'I' => Value::Int(bits as u32 as i32),
        b'F' => Value::Float(f32::from_bits(bits as u32)),
        b'J' => Value::Long(bits as i64),
        _ => Value::Double(f64::from_bits(bits)),
    }
}

/// Reverse the low `width` bytes (`Unsafe.convEndian` of a big-endian
/// `*Unaligned` access).
pub fn field_view_swap(bits: u64, width: usize) -> u64 {
    let le = bits.to_le_bytes();
    let mut out = [0u8; 8];
    for i in 0..width {
        out[i] = le[width - 1 - i];
    }
    u64::from_le_bytes(out)
}

/// A mixed-kind `get` of `access` (`B Z S C I F J D`) at a field's offset;
/// `big_endian` for an `*Unaligned(.., true)` read. `None`: not a field view.
pub fn unsafe_field_view_get(
    ctx: &dyn NativeContext,
    args: &[Value],
    access: u8,
    big_endian: bool,
) -> Option<Value> {
    // Wave 95 (lane L3): an offset INSIDE a field (byte field offsets, `k !=
    // 0`) reads `width` of its bytes from byte `k` (clamped to the field,
    // zero past it or for a reference field, as `native_unsafe_get_int`'s
    // sub-field arm), typed as the ACCESS: the int-width fallback answered
    // four bytes un-narrowed for `getShort` / `getChar` / `getByte` there too.
    let (obj, slot, k) = unsafe_instance_field_target(ctx, args)?;
    if k != 0 {
        let width = field_view_width(access)?;
        let mut bits = unsafe_sub_field_load(ctx, obj, slot, k, width);
        if big_endian {
            bits = field_view_swap(bits, width);
        }
        return Some(match access {
            b'Z' => Value::Int(i32::from(bits as u8 != 0)),
            _ => field_view_value(bits, access),
        });
    }
    let field = ctx.instance_field_descriptor_byte(obj, slot)?;
    let same = field == access || (matches!(field, b'B' | b'Z') && matches!(access, b'B' | b'Z'));
    // JIT round 21 (lane unsafe): a big-endian `*Unaligned` read of a
    // SAME-kind field is the view's too (its bytes are swapped).
    if same && !big_endian {
        return None;
    }
    // A reference field (`L` / `[`) has no width: its old answer stands.
    field_view_width(field)?;
    let width = field_view_width(access)?;
    let mut bits = field_view_bits(ctx.get_field(obj, slot), field);
    if width < 8 {
        bits &= (1u64 << (width * 8)) - 1;
    }
    if big_endian {
        bits = field_view_swap(bits, width);
    }
    Some(match access {
        // `normalize_for_read(jboolean)`: any non-zero byte is `true`.
        b'Z' => Value::Int(i32::from(bits as u8 != 0)),
        _ => field_view_value(bits, access),
    })
}

/// The store half of [`unsafe_field_view_get`]: the value is `args[3]`, the
/// `*Unaligned` order flag `args[4]`. `true` when handled here (a primitive
/// store into a reference field is handled by ignoring it; see the banner).
pub fn unsafe_field_view_put(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    access: u8,
) -> bool {
    // The `*Unaligned` order flag (a same-kind big-endian store swaps too).
    let big_endian = matches!(args.get(4), Some(Value::Int(z)) if *z != 0);
    let Some((obj, slot, field)) = field_view_target(&*ctx, args, access, big_endian) else {
        return false;
    };
    let Some(width) = field_view_width(access) else {
        return false;
    };
    let Some(field_width) = field_view_width(field) else {
        // `L` / `[`: a reference slot. Ignored, never overwritten with bits.
        return matches!(field, b'L' | b'[');
    };
    let mut val = match args.get(3) {
        Some(Value::Int(i)) => *i as u32 as u64,
        Some(Value::Long(l)) => *l as u64,
        Some(Value::Float(f)) => f.to_bits() as u64,
        Some(Value::Double(d)) => d.to_bits(),
        _ => 0,
    };
    if access == b'Z' {
        // `normalize_for_write(jboolean)`.
        val &= 1;
    }
    if width < 8 {
        val &= (1u64 << (width * 8)) - 1;
    }
    if big_endian {
        val = field_view_swap(val, width);
    }
    // JIT round 21 (lane unsafe): a store NARROWER than the field replaces
    // its low bytes with the lane's CAS loop, so a concurrent CAS / getAndAdd
    // on the field's other bytes (the lane natives, the hardware field
    // atomics) is never undone by this read-modify-write; HotSpot's narrow
    // store never touches those bytes. The plain merge below stays the
    // fallback.
    if width < field_width {
        if let Some(lane) = FieldLane::locate(&*ctx, obj, slot, 0, width) {
            if lane.rmw(ctx, &|_| Some(val)).is_some() {
                return true;
            }
        }
    }
    let new_bits = if width < field_width {
        let mask = (1u64 << (width * 8)) - 1;
        (field_view_bits(ctx.get_field(obj, slot), field) & !mask) | val
    } else {
        val
    };
    ctx.set_field(obj, slot, field_view_value(new_bits, field));
    true
}

/// The descriptor's first byte of the INSTANCE field at heap slot `slot` of
/// `obj` (its class or a superclass), or `None`.
pub fn instance_field_descriptor_at(
    ctx: &dyn NativeContext,
    obj: ObjectRef,
    slot: usize,
) -> Option<u8> {
    let mut cid = Some(ctx.class_id_of_object(obj));
    let mut hops = 0;
    while let Some(c) = cid {
        if let Some(f) = ctx
            .declared_fields(c)
            .into_iter()
            .find(|f| !f.is_static && f.slot_index == slot)
        {
            return f.descriptor.as_bytes().first().copied();
        }
        hops += 1;
        if hops > 64 {
            return None;
        }
        cid = ctx.superclass_of(c);
    }
    None
}

/// The short-width atomics on a `char` INSTANCE field (interpreter round i4
/// wave 86, lane L3). The JDK's `compareAndSetChar` / `compareAndExchangeChar`
/// / `getAndAddChar` / `getAndSetChar` reach the `Short` natives with the char
/// narrowed to a SIGNED short (`c2s`), while a `char` field's slot holds the
/// value ZERO-extended: for a char of `0x8000` or more the int-width element
/// CAS compared `65535` with `-1`, missed, and `compareAndSetChar` (which is
/// `compareAndExchangeChar(..) == expected`, both sides `s2c`'d) still
/// answered `true` with nothing written; a successful one stored the
/// sign-extended short, which `getfield` then read back negative. HotSpot
/// compares and stores the 16 bits. This compares the low 16 bits, stores the
/// new value zero-extended, and answers the witnessed value as a short.
/// `None` for anything that is not a heap object's `C` field.
pub fn char_field_short_atomic(
    ctx: &mut dyn NativeContext,
    args: &[Value],
    op: ArenaAtomic,
) -> Option<MethodCallResult> {
    // Wave 95 (lane L3): the slot decoded from the offset (byte field offsets
    // are `16 + 8 * slot`; the raw offset failed the field-count test and the
    // int-width CAS compared 65535 with -1 again). An offset inside the field
    // is the lane atomics' (`unsafe_field_lane_atomic`).
    let (obj, offset, k) = unsafe_instance_field_target(&*ctx, args)?;
    if k != 0 || instance_field_descriptor_at(&*ctx, obj, offset) != Some(b'C') {
        return None;
    }
    let arg16 = |i: usize| match args.get(i) {
        Some(Value::Int(v)) => (*v as u32 & 0xFFFF) as i32,
        _ => 0,
    };
    let expected = arg16(3);
    // `getAndAddShort(o, off, delta)` / `getAndSetShort(o, off, v)` carry the
    // operand in arg 3; the CAS pair in arg 4.
    let operand = match op {
        ArenaAtomic::Add | ArenaAtomic::Set => arg16(3),
        _ => arg16(4),
    };
    // Bounded: a slot whose stored shape never equals what `get_field`
    // answers would otherwise spin; the int-width path is the fallback.
    for _ in 0..100_000 {
        let raw = match ctx.get_field(obj, offset) {
            Value::Int(v) => v,
            _ => 0,
        };
        let cur = (raw as u32 & 0xFFFF) as i32;
        let as_short = Value::Int(cur as u16 as i16 as i32);
        let new = match op {
            ArenaAtomic::Cas | ArenaAtomic::Cae => {
                if cur != expected {
                    return Some(Ok(Some(match op {
                        ArenaAtomic::Cas => Value::Int(0),
                        _ => as_short,
                    })));
                }
                operand
            }
            ArenaAtomic::Add => (cur + operand) & 0xFFFF,
            ArenaAtomic::Set => operand,
        };
        if ctx.compare_and_swap_field(obj, offset, Value::Int(raw), Value::Int(new)) {
            return Some(Ok(Some(match op {
                ArenaAtomic::Cas => Value::Int(1),
                _ => as_short,
            })));
        }
    }
    None
}

/// `Unsafe.compareAndSetShort`; see [`native_unsafe_cas_byte`].
pub fn native_unsafe_cas_short(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let Some(v) = unsafe_arena_atomic(&*ctx, args, ArenaAtomic::Cas, 2) {
        return Ok(Some(v));
    }
    // Wave 8c: a lane of an instance field under byte field offsets.
    if let Some(v) = unsafe_field_lane_atomic(ctx, args, ArenaAtomic::Cas, 2) {
        return Ok(Some(v));
    }
    if let Some(v) = unsafe_subword_array_atomic(ctx, args, ArenaAtomic::Cas, 2) {
        return Ok(Some(v));
    }
    if let Some(r) = char_field_short_atomic(ctx, args, ArenaAtomic::Cas) {
        return r;    }
    native_unsafe_cas_int_element(ctx, args)
}

/// `Unsafe.compareAndExchangeByte`; see [`native_unsafe_cas_byte`].
pub fn native_unsafe_cae_byte(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let Some(v) = unsafe_arena_atomic(&*ctx, args, ArenaAtomic::Cae, 1) {
        return Ok(Some(v));
    }
    // Wave 8c: a lane of an instance field under byte field offsets.
    if let Some(v) = unsafe_field_lane_atomic(ctx, args, ArenaAtomic::Cae, 1) {
        return Ok(Some(v));
    }
    if let Some(v) = unsafe_subword_array_atomic(ctx, args, ArenaAtomic::Cae, 1) {
        return Ok(Some(v));
    }
    native_unsafe_compare_and_exchange_int_element(ctx, args)
}

/// `Unsafe.compareAndExchangeShort`; see [`native_unsafe_cas_byte`].
pub fn native_unsafe_cae_short(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let Some(v) = unsafe_arena_atomic(&*ctx, args, ArenaAtomic::Cae, 2) {
        return Ok(Some(v));
    }
    // Wave 8c: a lane of an instance field under byte field offsets.
    if let Some(v) = unsafe_field_lane_atomic(ctx, args, ArenaAtomic::Cae, 2) {
        return Ok(Some(v));
    }
    if let Some(v) = unsafe_subword_array_atomic(ctx, args, ArenaAtomic::Cae, 2) {
        return Ok(Some(v));
    }
    if let Some(r) = char_field_short_atomic(ctx, args, ArenaAtomic::Cae) {
        return r;    }
    native_unsafe_compare_and_exchange_int_element(ctx, args)
}

/// `Unsafe.getAndAddByte`; see [`native_unsafe_cas_byte`].
pub fn native_unsafe_get_and_add_byte(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let Some(v) = unsafe_arena_atomic(&*ctx, args, ArenaAtomic::Add, 1) {
        return Ok(Some(v));
    }
    // Wave 8c: a lane of an instance field under byte field offsets.
    if let Some(v) = unsafe_field_lane_atomic(ctx, args, ArenaAtomic::Add, 1) {
        return Ok(Some(v));
    }
    if let Some(v) = unsafe_subword_array_atomic(ctx, args, ArenaAtomic::Add, 1) {
        return Ok(Some(v));
    }
    native_unsafe_get_and_add_int_element(ctx, args)
}

/// `Unsafe.getAndAddShort`; see [`native_unsafe_cas_byte`].
pub fn native_unsafe_get_and_add_short(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let Some(v) = unsafe_arena_atomic(&*ctx, args, ArenaAtomic::Add, 2) {
        return Ok(Some(v));
    }
    // Wave 8c: a lane of an instance field under byte field offsets.
    if let Some(v) = unsafe_field_lane_atomic(ctx, args, ArenaAtomic::Add, 2) {
        return Ok(Some(v));
    }
    if let Some(v) = unsafe_subword_array_atomic(ctx, args, ArenaAtomic::Add, 2) {
        return Ok(Some(v));
    }
    if let Some(r) = char_field_short_atomic(ctx, args, ArenaAtomic::Add) {
        return r;    }
    native_unsafe_get_and_add_int_element(ctx, args)
}

/// `Unsafe.compareAndSetInt`. An int-width CAS over a primitive array of
/// another element type, or straddling elements (`compareAndSetFloat` over a
/// `float[]` is this call with the raw bits; a byte-array view `VarHandle`), is
/// a CAS on the raw bytes, as HotSpot's; see [`unsafe_wide_raw_target`].
pub fn native_unsafe_cas_int(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let Some((obj, off)) =
        unsafe_wide_raw_target(ctx, args, cratonvm_types::ArrayElementType::Int, 4)
    {
        let (ex, nv) = (
            unsafe_wide_arg_bits(args, 3, 4),
            unsafe_wide_arg_bits(args, 4, 4),
        );
        if let Some((_, ok)) = unsafe_wide_raw_cas(ctx, obj, off, 4, ex, nv) {
            return Ok(Some(Value::Int(i32::from(ok))));
        }
    }
    native_unsafe_cas_int_element(ctx, args)
}

/// [`native_unsafe_cas_int`] without the raw-bytes arm: the element-typed
/// CAS the byte/short natives fall back to (their access is not int-width).
pub fn native_unsafe_cas_int_element(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args: [unsafe, obj, offset(long), expected(int), new(int)]
    // Wave 8c: a lane of an instance field under byte field offsets.
    if let Some(v) = unsafe_field_lane_atomic(ctx, args, ArenaAtomic::Cas, 4) {
        return Ok(Some(v));
    }
    if let Some(v) = unsafe_arena_atomic(&*ctx, args, ArenaAtomic::Cas, 4) {
        return Ok(Some(v));
    }
    let offset = unsafe_offset(args, 2);
    let expected = args.get(3).copied().unwrap_or(Value::Int(0));
    let new_val = args.get(4).copied().unwrap_or(Value::Int(0));
    let obj = match unsafe_obj(args, 1) {
        Some(o) => o,
        None => {
            if let Some(ok) = unsafe_static_cas(ctx, offset, expected, new_val) {
                return Ok(Some(Value::Int(if ok { 1 } else { 0 })));
            }
            if note_unsafe_side_store_offset(ctx, offset, line!(), args.len()) {
                // §4.5's specified refusal, discharged by §21/§23's count.
                // HotSpot SIGSEGVs here; the side store used to answer, and a
                // CAS reported success having written nowhere.
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!(
                        "Unsafe access with a null base and an offset that is \
                         neither an arena handle, a synthetic offset, nor a \
                         registered static field: {offset:#x}"
                    ),
                }
                .into());
            }
            let mut map = lock_static_int_shard(offset);
            let cur = *map.entry((ctx.vm_identity(), offset)).or_insert(0);
            let ex = if let Value::Int(e) = expected { e } else { 0 };
            let nv = if let Value::Int(n) = new_val { n } else { 0 };
            let ok = cur == ex;
            if ok {
                map.insert((ctx.vm_identity(), offset), nv);
            }
            return Ok(Some(Value::Int(if ok { 1 } else { 0 })));
        }
    };
    if unsafe_synthetic_field_offset(&*ctx, obj, offset) {
        // Round-9 Bug 7: pass ctx so the side store can use a GC-stable
        // identity hash instead of a raw pointer.
        let ok = synthetic_cas(ctx, obj, offset, expected, new_val);
        return Ok(Some(Value::Int(if ok { 1 } else { 0 })));
    }
    if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
        // `compare_and_swap_field` on a primitive-array element is one
        // hardware `LOCK CMPXCHG` on the element (nio21 wave 5 L3,
        // `vm_exec::hw_cas_primitive_array_element`), so it is atomic against
        // a plain element store and a compiled CAS / XADD on the same bytes;
        // only `CRATONVM_UNSAFE_ARRAY_HW_CAS=0` puts it back on the CAS lock.
        // Bounds-check the decoded index in-crate before it reaches the
        // host accessor; OOB offset => CAS fails (no heap touch).
        let idx = match unsafe_checked_array_index(ctx, obj, offset) {
            Some(i) => i,
            None => return Ok(Some(Value::Int(0))),
        };
        let result = ctx.compare_and_swap_field(obj, idx, expected, new_val);
        return Ok(Some(Value::Int(if result { 1 } else { 0 })));
    }
    // Wave 8c: the slot (an offset inside a field took the lane above).
    let (offset, _) = unsafe_field_slot(&*ctx, obj, offset);
    let result = ctx.compare_and_swap_field(obj, offset, expected, new_val);
    Ok(Some(Value::Int(if result { 1 } else { 0 })))
}

pub fn native_unsafe_cas_long(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args: [unsafe, obj, offset(long), expected(long), new(long)]
    // Wave 8c: a lane of an instance field under byte field offsets.
    if let Some(v) = unsafe_field_lane_atomic(ctx, args, ArenaAtomic::Cas, 8) {
        return Ok(Some(v));
    }
    if let Some(v) = unsafe_arena_atomic(&*ctx, args, ArenaAtomic::Cas, 8) {
        return Ok(Some(v));
    }
    // Raw bytes for a long-width CAS over another element type (a `double[]`
    // through `compareAndSetDouble`, a byte-array view); see
    // [`native_unsafe_cas_int`].
    if let Some((obj, off)) =
        unsafe_wide_raw_target(ctx, args, cratonvm_types::ArrayElementType::Long, 8)
    {
        let (ex, nv) = (
            unsafe_wide_arg_bits(args, 3, 8),
            unsafe_wide_arg_bits(args, 4, 8),
        );
        if let Some((_, ok)) = unsafe_wide_raw_cas(ctx, obj, off, 8, ex, nv) {
            return Ok(Some(Value::Int(i32::from(ok))));
        }
    }
    let offset = unsafe_offset(args, 2);
    let expected = args.get(3).copied().unwrap_or(Value::Long(0));
    let new_val = args.get(4).copied().unwrap_or(Value::Long(0));
    let obj = match unsafe_obj(args, 1) {
        Some(o) => o,
        None => {
            // t1 wave 8 (lane RL-ATOM, re-land of JIT round 21 lane staticcas
            // `b0af38a380`; correctness, unconditional): ONE atomic step on the
            // static (`unsafe_static_cas`: the VM's hardware CAS of the static
            // cell, else under the class mirror's monitor). This arm was the
            // last unlocked read-compare-write of a registered static: two
            // racing null-base `compareAndSetLong`s could both succeed.
            if let Some(ok) = unsafe_static_cas(ctx, offset, expected, new_val) {
                return Ok(Some(Value::Int(if ok { 1 } else { 0 })));
            }
            if note_unsafe_side_store_offset(ctx, offset, line!(), args.len()) {
                // §4.5's specified refusal, discharged by §21/§23's count.
                // HotSpot SIGSEGVs here; the side store used to answer, and a
                // CAS reported success having written nowhere.
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!(
                        "Unsafe access with a null base and an offset that is \
                         neither an arena handle, a synthetic offset, nor a \
                         registered static field: {offset:#x}"
                    ),
                }
                .into());
            }
            let mut map = lock_static_long_shard(offset);
            let cur = *map.entry((ctx.vm_identity(), offset)).or_insert(0);
            let ex = if let Value::Long(e) = expected { e } else { 0 };
            let nv = if let Value::Long(n) = new_val { n } else { 0 };
            let ok = cur == ex;
            if ok {
                map.insert((ctx.vm_identity(), offset), nv);
            }
            return Ok(Some(Value::Int(if ok { 1 } else { 0 })));
        }
    };
    if unsafe_synthetic_field_offset(&*ctx, obj, offset) {
        // Round-9 Bug 7: pass ctx so the side store can use a GC-stable
        // identity hash instead of a raw pointer.
        let ok = synthetic_cas(ctx, obj, offset, expected, new_val);
        return Ok(Some(Value::Int(if ok { 1 } else { 0 })));
    }
    if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
        // One hardware CAS on the element (see `native_unsafe_cas_int_element`).
        // Bounds-check the decoded index in-crate; OOB offset => CAS fails.
        let idx = match unsafe_checked_array_index(ctx, obj, offset) {
            Some(i) => i,
            None => return Ok(Some(Value::Int(0))),
        };
        let result = ctx.compare_and_swap_field(obj, idx, expected, new_val);
        return Ok(Some(Value::Int(if result { 1 } else { 0 })));
    }
    // CRATONVM_DBG_AQS_TRACE (2026-07-21): full transition ledger for the
    // j.u.c.locks synchronizer family — every state CAS with pre-value and
    // outcome, to catch the acquire/release imbalance behind the WildFly
    // CapabilityRegistry write-lock wedge.
    // Wave 8c: the slot (an offset inside a field took the lane above).
    let (offset, _) = unsafe_field_slot(&*ctx, obj, offset);
    let aqs_trace = aqs_trace_enabled();
    let pre_for_trace = if aqs_trace {
        Some(ctx.get_field_volatile(obj, offset))
    } else {
        None
    };
    let result = ctx.compare_and_swap_field(obj, offset, expected, new_val);
    if aqs_trace {
        let cid = ctx.class_id_of_object(obj);
        if let Some(cls) = ctx.class_name_of_id(cid) {
            if cls.starts_with("java/util/concurrent/locks/") {
                aqs_trace_line(&format!(
                    "[AQS] tid={} cas_long obj={:p} cls={} slot={} pre={:?} exp={:?} new={:?} ok={}",
                    ctx.thread_id(),
                    obj.as_ptr(),
                    cls.rsplit('/').next().unwrap_or(&cls),
                    offset,
                    pre_for_trace.unwrap_or(Value::Object(None)),
                    expected,
                    new_val,
                    result
                ));
            }
        }
    }
    // T19_H6_CAS_DIAG: log a failed CAS on a long instance field ONLY when the
    // slot did not read back as a `Long` — the Double-tagged bit pattern (or
    // `Object(None)`) this probe was written to catch. Rate-limited to 5
    // entries to avoid flooding when a livelock fires the CAS millions of times.
    //
    // It used to warn on the first five failures of ANY kind. A failed CAS
    // whose slot reads back a `Long` is the ordinary outcome of contention —
    // `ConcurrentHashMap.addCount`'s `baseCount`/`CounterCell` retry loop,
    // `AtomicLong`, `ThreadLocalRandom`'s seeder — and every one printed in the
    // strict corpus (SBR-W3 strictgate) was that shape, e.g.
    // `class=java/util/concurrent/ConcurrentHashMap slot=4 current=Long(11)
    // expected=Long(10) new=Long(11)`: another thread got there first. Those
    // WARN lines landed in five probes' merged stdout (`ChmKeySetGrowth`,
    // `ChmShadowSweep`, `JdkOnlyPlatformProbe`, `JdkOnlyCensusLoadProbe`, ...)
    // with a timestamp as their section key, which is why the corpus reported
    // ~15 "divergent sections" that were neither divergences nor stable.
    if !result {
        let current = ctx.get_field_volatile(obj, offset);
        if !matches!(current, Value::Long(_)) {
            use std::sync::atomic::{AtomicU64, Ordering};
            static CAS_DIAG_COUNT: AtomicU64 = AtomicU64::new(0);
            let n = CAS_DIAG_COUNT.fetch_add(1, Ordering::Relaxed);
            if n < 5 {
                let cid = ctx.class_id_of_object(obj);
                let cls = ctx
                    .class_name_of_id(cid)
                    .unwrap_or_else(|| "<?>".to_string());
                tracing::warn!(
                    target: "cas_diag",
                    "T19_H6_CAS_DIAG cas_long FAIL #{} class={} slot={} current={:?} expected={:?} new={:?}",
                    n, cls, offset, current, expected, new_val
                );
            }
        }
    }
    Ok(Some(Value::Int(if result { 1 } else { 0 })))
}

/// WP4.1 closer — recover an Object reference from a tag-lost `Value::Double`.
///
/// Under heavy AQS contention the invoke-argument pop boundary surfaces an
/// untagged 64-bit raw-pointer stack slot as `Value::Double(...)` because
/// `CompactValue::to_value()` decodes any non-NaN-tagged u64 as `Double`.
/// Native methods whose Java signature declares a reference parameter must
/// recognize this and reconstruct the original `ObjectRef`; otherwise the
/// CAS or store is performed against a denormal-shaped double, corrupting
/// the field's value tag and crashing later reads with "expected object
/// reference, got double(...)".
///
/// C20 — heap-membership filter on Java-controlled `ObjectRef::from_raw`
/// reconstruction. Mirrors the `vm::interpreter::pop_object_ref_ctx_with`
/// C7 fix: alignment + 48-bit-range alone are NOT sufficient to prove a
/// Double slot holds a smuggled `ObjectRef`. An honest `f64` whose
/// `to_bits()` value satisfies the predicate (e.g. denormals like
/// `f64::from_bits(0x800)`) would be coerced into a wild `ObjectRef`
/// and dereferenced by the next heap access. The interpreter fix uses
/// `heap.is_heap_addr(bits) -> Option<ObjectRef>` as the gating
/// primitive, but `NativeContext` (the only handle into the runtime
/// from native-builtins) does not currently expose that probe and the
/// function's call sites span both `lib.rs` and `unsafe_natives.rs` so
/// the signature is fixed.
///
/// Conservative resolution until the heap probe is plumbed through:
/// disable the smuggled-pointer reconstruction and return `Object(None)`
/// for the Double-bits arm. This matches the existing miss-idiom (line
/// just below for misaligned bits, and for `Int(0)`/`Long(0)`), and
/// matches `recover_class_mirror_from_slot`'s heap-membership-gated
/// behavior in `lang_invoke.rs`. The AQS smuggle path will instead hit
/// the "expected object reference, got double(...)" error rather than
/// silently feeding a wild pointer to downstream heap accesses — a
/// loud failure is strictly safer than the soundness footgun.
pub fn recover_object_arg(value: Value) -> Value {
    match value {
        Value::Object(_) => value,
        Value::Double(d) => {
            let bits = d.to_bits();
            if bits == 0 {
                Value::Object(None)
            } else {
                // C20: previously this fell through to
                // `ObjectRef::from_raw(bits as *mut u8)` after an
                // alignment+48-bit-range check. Without an
                // `is_heap_addr`-style heap-membership probe on
                // `NativeContext`, we cannot distinguish a legitimate
                // smuggled pointer from an honest scalar that happens to
                // satisfy the predicate. Refuse to fabricate the
                // `ObjectRef`; let the caller observe an explicit
                // null/error rather than silently dereferencing arbitrary
                // Java-controlled bits.
                Value::Object(None)
            }
        }
        Value::Int(0) | Value::Long(0) => Value::Object(None),
        _ => value,
    }
}

pub fn native_unsafe_cas_object(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args: [unsafe, obj, offset(long), expected(Object), new(Object)]
    let offset = unsafe_offset(args, 2);
    // WP4.1 closer — recover Object from a tag-lost Double pattern at the
    // invoke-arg boundary; see `recover_object_arg` for rationale.
    let expected = recover_object_arg(args.get(3).copied().unwrap_or(Value::Object(None)));
    let new_val = recover_object_arg(args.get(4).copied().unwrap_or(Value::Object(None)));
    let obj = match unsafe_obj(args, 1) {
        Some(o) => o,
        None => {
            if let Some(ok) = unsafe_static_cas(ctx, offset, expected, new_val) {
                return Ok(Some(Value::Int(if ok { 1 } else { 0 })));
            }
            if note_unsafe_side_store_offset(ctx, offset, line!(), args.len()) {
                // §4.5's specified refusal, discharged by §21/§23's count.
                // HotSpot SIGSEGVs here; the side store used to answer, and a
                // CAS reported success having written nowhere.
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!(
                        "Unsafe access with a null base and an offset that is \
                         neither an arena handle, a synthetic offset, nor a \
                         registered static field: {offset:#x}"
                    ),
                }
                .into());
            }
            let key = (ctx.vm_identity(), offset);
            let mut map = lock_static_obj_shard(offset);
            let cur = *map.entry(key).or_insert(None);
            let ex = if let Value::Object(e) = expected {
                e
            } else {
                None
            };
            let nv = if let Value::Object(n) = new_val {
                n
            } else {
                None
            };
            let ok = cur == ex;
            if ok {
                map.insert(key, nv);
            }
            return Ok(Some(Value::Int(if ok { 1 } else { 0 })));
        }
    };
    // Synthetic-offset CAS — routed via per-object side store so lazy-init
    // guards on JDK-internal fields (Class.reflectionData, ServicesCatalog,
    // AbstractClassLoaderValue) don't livelock when the field's real heap
    // slot is unknown to our layout.  See `synthetic_offset_for` above.
    if unsafe_synthetic_field_offset(&*ctx, obj, offset) {
        // Round-9 Bug 7: pass ctx so the side store can use a GC-stable
        // identity hash instead of a raw pointer.
        let ok = synthetic_cas(ctx, obj, offset, expected, new_val);
        return Ok(Some(Value::Int(if ok { 1 } else { 0 })));
    }
    // ConcurrentHashMap.casTabAt passes byte offsets into an Object[] array,
    // not field slot indices. The shared-VM `compare_and_swap_field` path
    // dispatches on `kind_of(obj) == Array` internally AND grabs the per-
    // object CAS lock — the prior lock-free load+compare+store here did
    // NOT. Observable on `ChmStress` where 64 worker threads racing through
    // `casTabAt` would interleave inside this native and duplicate / drop
    // bin-list head installs. Pre-converting the byte offset to an element
    // index keeps the contract: `compare_and_swap_field` reuses the second
    // arg directly when `is_array` is true.
    if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
        // Bounds-check the decoded index in-crate; OOB offset => CAS fails.
        let idx = match unsafe_checked_array_index(ctx, obj, offset) {
            Some(i) => i,
            None => return Ok(Some(Value::Int(0))),
        };
        let result = ctx.compare_and_swap_field(obj, idx, expected, new_val);
        return Ok(Some(Value::Int(if result { 1 } else { 0 })));
    }
    // Wave 8c: the slot; an offset inside a field names no reference, so
    // the CAS fails.
    let (offset, k) = unsafe_field_slot(&*ctx, obj, offset);
    if k != 0 {
        return Ok(Some(Value::Int(0)));
    }
    let aqs_trace = aqs_trace_enabled();
    let pre_for_trace = if aqs_trace {
        Some(ctx.get_field_volatile(obj, offset))
    } else {
        None
    };
    let result = ctx.compare_and_swap_field(obj, offset, expected, new_val);
    if aqs_trace {
        let cid = ctx.class_id_of_object(obj);
        if let Some(cls) = ctx.class_name_of_id(cid) {
            if cls.starts_with("java/util/concurrent/locks/") {
                aqs_trace_line(&format!(
                    "[AQS] tid={} cas_obj obj={:p} cls={} slot={} pre={:?} exp={:?} new={:?} ok={}",
                    ctx.thread_id(),
                    obj.as_ptr(),
                    cls.rsplit('/').next().unwrap_or(&cls),
                    offset,
                    pre_for_trace.unwrap_or(Value::Object(None)),
                    expected,
                    new_val,
                    result
                ));
            }
        }
    }
    Ok(Some(Value::Int(if result { 1 } else { 0 })))
}

pub fn native_unsafe_get_int_volatile(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let offset = unsafe_offset(args, 2);
    let obj = match unsafe_obj(args, 1) {
        Some(o) => o,
        None => {
            if let Some(v) = unsafe_static_get(ctx, offset) {
                return Ok(Some(match v {
                    Value::Int(i) => Value::Int(i),
                    Value::Long(l) => Value::Int(l as i32),
                    // Interpreter round i6 wave 127, lane L4: a `float` static
                    // passes through, as the heap branch passes a `float`
                    // field: the `getFloatVolatile` registrations share this
                    // native, and `getIntVolatile`'s wrapper
                    // (`native_unsafe_get_int_volatile_arena`) answers its
                    // bits. It read as 0, so the JDK's `getAndSetFloat` loop
                    // compared 0 against the slot.
                    Value::Float(f) => Value::Float(f),
                    // t1 wave 8 (lane RL-ATOM, re-land of `29522fc209`): a
                    // `double` static's low four bytes, not 0.
                    Value::Double(d) => Value::Int(d.to_bits() as i32),
                    _ => Value::Int(0),
                }));
            }
            if note_unsafe_side_store_offset(ctx, offset, line!(), args.len()) {
                // §4.5's specified refusal, discharged by §21/§23's count.
                // HotSpot SIGSEGVs here; the side store used to answer, and a
                // CAS reported success having written nowhere.
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!(
                        "Unsafe access with a null base and an offset that is \
                         neither an arena handle, a synthetic offset, nor a \
                         registered static field: {offset:#x}"
                    ),
                }
                .into());
            }
            let key = (ctx.vm_identity(), offset);
            let map = lock_static_int_shard(offset);
            return Ok(Some(Value::Int(map.get(&key).copied().unwrap_or(0))));
        }
    };
    if unsafe_synthetic_field_offset(&*ctx, obj, offset) {
        // JIT round 21 (lane unsafe): a wider / float static answers its
        // low four bytes (the `0` this answered made the JDK's sub-word
        // getAndAdd retry loops on a long static spin: the lane CAS never saw
        // the `0` it was handed as the expected value).
        return Ok(Some(match synthetic_get(ctx, obj, offset) {
            Value::Int(v) => Value::Int(v),
            // Wave 127 (lane L4): a `float` static through its mirror base;
            // see the null-base arm above.
            Value::Float(f) => Value::Float(f),

            Value::Long(l) => Value::Int(l as i32),
            Value::Double(d) => Value::Int(d.to_bits() as i32),
            _ => Value::Int(0),
        }));
    }
    if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
        match unsafe_checked_array_index(ctx, obj, offset) {
            Some(idx) => Ok(Some(ctx.get_array_element(obj, idx))),
            None => Ok(Some(Value::Int(0))),
        }
    } else {
        // Wave 8c: an offset INSIDE a field reads its bytes.
        match unsafe_field_slot(&*ctx, obj, offset) {
            // Cast: the low 32 bits of the zero-extended bytes.
            (slot, k) if k != 0 => Ok(Some(Value::Int(
                unsafe_sub_field_load(&*ctx, obj, slot, k, 4) as u32 as i32,
            ))),
            // JIT round 21 (lane unsafe): another kind's slot answers its low
            // four bytes, as `native_unsafe_get_int` does (a `long` field's
            // `Value::Long` reached the `I` return; HotSpot reads the low half).
            (slot, _) => Ok(Some(match ctx.get_field_volatile(obj, slot) {
                Value::Long(l) => Value::Int(l as i32),
                Value::Float(f) => Value::Int(f.to_bits() as i32),
                Value::Double(d) => Value::Int(d.to_bits() as i32),
                other => other,
            })),
        }
    }
}

pub fn native_unsafe_put_int_volatile(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let offset = unsafe_offset(args, 2);
    let val = args.get(3).copied().unwrap_or(Value::Int(0));
    let obj = match unsafe_obj(args, 1) {
        Some(o) => o,
        None => {
            let v = if let Value::Int(i) = val { i } else { 0 };
            if unsafe_static_put(ctx, offset, Value::Int(v)) {
                return Ok(None);
            }
            if note_unsafe_side_store_offset(ctx, offset, line!(), args.len()) {
                // §4.5's specified refusal, discharged by §21/§23's count.
                // HotSpot SIGSEGVs here; the side store used to answer, and a
                // CAS reported success having written nowhere.
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!(
                        "Unsafe access with a null base and an offset that is \
                         neither an arena handle, a synthetic offset, nor a \
                         registered static field: {offset:#x}"
                    ),
                }
                .into());
            }
            lock_static_int_shard(offset).insert((ctx.vm_identity(), offset), v);
            return Ok(None);
        }
    };
    if unsafe_synthetic_field_offset(&*ctx, obj, offset) {
        // JIT round 21 (lane unsafefix; correctness, unconditional): an int
        // store AT a `long` / `double` static's own offset replaces its low
        // four bytes (HotSpot's 4-byte store), not the whole static.
        if let Some(bits) = unsafe_value_bits(val) {
            // Cast: the low four bytes in memory order.
            let bytes = (bits as u32).to_le_bytes();
            if unsafe_narrow_field_store(ctx, obj, offset, &bytes) {
                return Ok(None);
            }
        }
        synthetic_put(ctx, obj, offset, val);
        return Ok(None);
    }
    if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
        if let Some(idx) = unsafe_checked_array_index(ctx, obj, offset) {
            ctx.set_array_element(obj, idx, val);
        }
        // OOB offset => no-op (benign), never reaches the host accessor.
    } else {
        // Wave 8c: an offset INSIDE a field replaces four of its bytes.
        let (offset, k) = unsafe_field_slot(&*ctx, obj, offset);
        if k != 0 {
            let bits = unsafe_value_bits(val).unwrap_or(0);
            unsafe_sub_field_store(ctx, obj, offset, k, 4, bits);
            return Ok(None);
        }
        // JIT round 21 (lane unsafe): into a `long` / `float` / `double`
        // field only its low four bytes (HotSpot's 4-byte store at the
        // field's offset); the whole-slot store replaced a long's high half
        // and turned a float slot into an int. The lane store is a CAS loop,
        // atomic against the field's other accesses.
        if matches!(
            ctx.get_field(obj, offset),
            Value::Long(_) | Value::Float(_) | Value::Double(_)
        ) {
            let bits = unsafe_value_bits(val).unwrap_or(0);
            unsafe_sub_field_store(ctx, obj, offset, 0, 4, bits);
            return Ok(None);
        }
        // uns21 R4: a primitive into a reference field is ignored, as in the field view.
        if matches!(ctx.get_field(obj, offset), Value::Object(_)) {
            return Ok(None);
        }
        ctx.set_field_volatile(obj, offset, val);
    }
    Ok(None)
}

pub fn native_unsafe_get_long_volatile(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let offset = unsafe_offset(args, 2);
    let obj = match unsafe_obj(args, 1) {
        Some(o) => o,
        None => {
            if let Some(value) = unsafe_static_get(ctx, offset) {
                return Ok(Some(match value {
                    Value::Long(v) => Value::Long(v),
                    Value::Int(v) => Value::Long(v as i64),
                    // Wave 127 (lane L4): a `double` static passes through;
                    // see `native_unsafe_get_int_volatile`.
                    Value::Double(d) => Value::Double(d),
                    _ => Value::Long(0),
                }));
            }
            if note_unsafe_side_store_offset(ctx, offset, line!(), args.len()) {
                // §4.5's specified refusal, discharged by §21/§23's count.
                // HotSpot SIGSEGVs here; the side store used to answer, and a
                // CAS reported success having written nowhere.
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!(
                        "Unsafe access with a null base and an offset that is \
                         neither an arena handle, a synthetic offset, nor a \
                         registered static field: {offset:#x}"
                    ),
                }
                .into());
            }
            let key = (ctx.vm_identity(), offset);
            let map = lock_static_long_shard(offset);
            return Ok(Some(Value::Long(map.get(&key).copied().unwrap_or(0))));
        }
    };
    if unsafe_synthetic_field_offset(&*ctx, obj, offset) {
        return Ok(Some(match synthetic_get(ctx, obj, offset) {
            Value::Long(v) => Value::Long(v),
            Value::Int(v) => Value::Long(v as i64),
            // Wave 127 (lane L4): a `double` static through its mirror base;
            // see `native_unsafe_get_int_volatile`.
            Value::Double(d) => Value::Double(d),

            _ => Value::Long(0),
        }));
    }
    if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
        match unsafe_checked_array_index(ctx, obj, offset) {
            Some(idx) => Ok(Some(ctx.get_array_element(obj, idx))),
            None => Ok(Some(Value::Long(0))),
        }
    } else {
        // Wave 8c: an offset INSIDE a field reads its remaining bytes.
        match unsafe_field_slot(&*ctx, obj, offset) {
            // Cast: the zero-extended bytes.
            (slot, k) if k != 0 => Ok(Some(Value::Long(
                unsafe_sub_field_load(&*ctx, obj, slot, k, 8) as i64,
            ))),
            // JIT round 21 (lane unsafe): a `double` field answers its bits.
            (slot, _) => Ok(Some(match ctx.get_field_volatile(obj, slot) {
                Value::Double(d) => Value::Long(d.to_bits() as i64),
                other => other,
            })),
        }
    }
}

pub fn native_unsafe_put_long_volatile(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let offset = unsafe_offset(args, 2);
    let val = args.get(3).copied().unwrap_or(Value::Long(0));
    let obj = match unsafe_obj(args, 1) {
        Some(o) => o,
        None => {
            let v = if let Value::Long(i) = val { i } else { 0 };
            if unsafe_static_put(ctx, offset, Value::Long(v)) {
                return Ok(None);
            }
            if note_unsafe_side_store_offset(ctx, offset, line!(), args.len()) {
                // §4.5's specified refusal, discharged by §21/§23's count.
                // HotSpot SIGSEGVs here; the side store used to answer, and a
                // CAS reported success having written nowhere.
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!(
                        "Unsafe access with a null base and an offset that is \
                         neither an arena handle, a synthetic offset, nor a \
                         registered static field: {offset:#x}"
                    ),
                }
                .into());
            }
            lock_static_long_shard(offset).insert((ctx.vm_identity(), offset), v);
            return Ok(None);
        }
    };
    if unsafe_synthetic_field_offset(&*ctx, obj, offset) {
        synthetic_put(ctx, obj, offset, val);
        return Ok(None);
    }
    if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
        if let Some(idx) = unsafe_checked_array_index(ctx, obj, offset) {
            ctx.set_array_element(obj, idx, val);
        }
        // OOB offset => no-op (benign), never reaches the host accessor.
    } else {
        // Wave 8c: an offset INSIDE a field replaces its remaining bytes.
        let (offset, k) = unsafe_field_slot(&*ctx, obj, offset);
        if k != 0 {
            let bits = unsafe_value_bits(val).unwrap_or(0);
            unsafe_sub_field_store(ctx, obj, offset, k, 8, bits);
            return Ok(None);
        }
        // JIT round 21 (lane unsafe): into a `double` field, its bits (the
        // whole-slot store turned the slot into a long).
        if matches!(ctx.get_field(obj, offset), Value::Double(_)) {
            let bits = unsafe_value_bits(val).unwrap_or(0);
            unsafe_sub_field_store(ctx, obj, offset, 0, 8, bits);
            return Ok(None);
        }
        // uns21 R4: as in `native_unsafe_put_int_volatile`.
        if matches!(ctx.get_field(obj, offset), Value::Object(_)) {
            return Ok(None);
        }
        ctx.set_field_volatile(obj, offset, val);
    }
    Ok(None)
}

pub fn native_unsafe_get_object_volatile(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let offset = unsafe_offset(args, 2);
    let obj = match unsafe_obj(args, 1) {
        Some(o) => o,
        None => {
            if let Some(v) = unsafe_static_get(ctx, offset) {
                return Ok(Some(recover_object_arg(v)));
            }
            if note_unsafe_side_store_offset(ctx, offset, line!(), args.len()) {
                // §4.5's specified refusal, discharged by §21/§23's count.
                // HotSpot SIGSEGVs here; the side store used to answer, and a
                // CAS reported success having written nowhere.
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!(
                        "Unsafe access with a null base and an offset that is \
                         neither an arena handle, a synthetic offset, nor a \
                         registered static field: {offset:#x}"
                    ),
                }
                .into());
            }
            let key = (ctx.vm_identity(), offset);
            let map = lock_static_obj_shard(offset);
            return Ok(Some(Value::Object(map.get(&key).copied().unwrap_or(None))));
        }
    };
    if unsafe_synthetic_field_offset(&*ctx, obj, offset) {
        return Ok(Some(recover_object_arg(synthetic_get(ctx, obj, offset))));
    }
    let val = if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
        match unsafe_checked_array_index(ctx, obj, offset) {
            Some(idx) => ctx.get_array_element(obj, idx),
            None => Value::Object(None),
        }
    } else {
        // Wave 8c: an offset INSIDE a field names no reference.
        match unsafe_field_slot(&*ctx, obj, offset) {
            (_, k) if k != 0 => Value::Object(None),
            (slot, _) => {
                let v = ctx.get_field_volatile(obj, slot);
                slot0_read_keep_alive(ctx, obj, slot, v)
            }
        }
    };
    // T14 + WP4.1 closer: coerce non-reference values to null OR recover an
    // ObjectRef from a tag-lost Double bit pattern.
    Ok(Some(recover_object_arg(val)))
}

pub fn native_unsafe_put_object_volatile(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let offset = unsafe_offset(args, 2);
    // WP4.1 closer — recover Object from a tag-lost Double pattern.
    let val = recover_object_arg(args.get(3).copied().unwrap_or(Value::Object(None)));
    let obj = match unsafe_obj(args, 1) {
        Some(o) => o,
        None => {
            let v = if let Value::Object(o) = val { o } else { None };
            if unsafe_static_put(ctx, offset, Value::Object(v)) {
                return Ok(None);
            }
            if note_unsafe_side_store_offset(ctx, offset, line!(), args.len()) {
                // §4.5's specified refusal, discharged by §21/§23's count.
                // HotSpot SIGSEGVs here; the side store used to answer, and a
                // CAS reported success having written nowhere.
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!(
                        "Unsafe access with a null base and an offset that is \
                         neither an arena handle, a synthetic offset, nor a \
                         registered static field: {offset:#x}"
                    ),
                }
                .into());
            }
            lock_static_obj_shard(offset).insert((ctx.vm_identity(), offset), v);
            return Ok(None);
        }
    };
    if unsafe_synthetic_field_offset(&*ctx, obj, offset) {
        synthetic_put(ctx, obj, offset, val);
        return Ok(None);
    }
    if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
        if let Some(idx) = unsafe_checked_array_index(ctx, obj, offset) {
            ctx.set_array_element(obj, idx, val);
        }
        // OOB offset => no-op (benign), never reaches the host accessor.
    } else {
        // Wave 8c: an offset INSIDE a field names no reference: no store.
        let (offset, k) = unsafe_field_slot(&*ctx, obj, offset);
        if k != 0 {
            return Ok(None);
        }
        ctx.set_field_volatile(obj, offset, val);
    }
    Ok(None)
}

pub fn native_unsafe_get_object(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let offset = unsafe_offset(args, 2);
    let obj = match unsafe_obj(args, 1) {
        Some(o) => o,
        None => {
            if let Some(v) = unsafe_static_get(ctx, offset) {
                return Ok(Some(recover_object_arg(v)));
            }
            if note_unsafe_side_store_offset(ctx, offset, line!(), args.len()) {
                // §4.5's specified refusal, discharged by §21/§23's count.
                // HotSpot SIGSEGVs here; the side store used to answer, and a
                // CAS reported success having written nowhere.
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!(
                        "Unsafe access with a null base and an offset that is \
                         neither an arena handle, a synthetic offset, nor a \
                         registered static field: {offset:#x}"
                    ),
                }
                .into());
            }
            let key = (ctx.vm_identity(), offset);
            let map = lock_static_obj_shard(offset);
            return Ok(Some(Value::Object(map.get(&key).copied().unwrap_or(None))));
        }
    };
    if unsafe_synthetic_field_offset(&*ctx, obj, offset) {
        return Ok(Some(recover_object_arg(synthetic_get(ctx, obj, offset))));
    }
    let val = if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
        match unsafe_checked_array_index(ctx, obj, offset) {
            Some(idx) => ctx.get_array_element(obj, idx),
            None => Value::Object(None),
        }
    } else {
        // Wave 8c: an offset INSIDE a field names no reference.
        match unsafe_field_slot(&*ctx, obj, offset) {
            (_, k) if k != 0 => Value::Object(None),
            (slot, _) => {
                let v = ctx.get_field(obj, slot);
                slot0_read_keep_alive(ctx, obj, slot, v)
            }
        }
    };
    Ok(Some(recover_object_arg(val)))
}

pub fn native_unsafe_put_object(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let offset = unsafe_offset(args, 2);
    // WP4.1 closer — recover Object from a tag-lost Double pattern.
    let val = recover_object_arg(args.get(3).copied().unwrap_or(Value::Object(None)));
    let obj = match unsafe_obj(args, 1) {
        Some(o) => o,
        None => {
            let v = if let Value::Object(o) = val { o } else { None };
            if unsafe_static_put(ctx, offset, Value::Object(v)) {
                return Ok(None);
            }
            if note_unsafe_side_store_offset(ctx, offset, line!(), args.len()) {
                // §4.5's specified refusal, discharged by §21/§23's count.
                // HotSpot SIGSEGVs here; the side store used to answer, and a
                // CAS reported success having written nowhere.
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!(
                        "Unsafe access with a null base and an offset that is \
                         neither an arena handle, a synthetic offset, nor a \
                         registered static field: {offset:#x}"
                    ),
                }
                .into());
            }
            lock_static_obj_shard(offset).insert((ctx.vm_identity(), offset), v);
            return Ok(None);
        }
    };
    if unsafe_synthetic_field_offset(&*ctx, obj, offset) {
        synthetic_put(ctx, obj, offset, val);
        return Ok(None);
    }
    if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
        if let Some(idx) = unsafe_checked_array_index(ctx, obj, offset) {
            ctx.set_array_element(obj, idx, val);
        }
        // OOB offset => no-op (benign), never reaches the host accessor.
    } else {
        // Wave 8c: an offset INSIDE a field names no reference: no store.
        let (offset, k) = unsafe_field_slot(&*ctx, obj, offset);
        if k != 0 {
            return Ok(None);
        }
        ctx.set_field(obj, offset, val);
    }
    Ok(None)
}

/// Multi-byte `Unsafe` accessors for `byte[]`-backed memory.
///
/// `Unsafe.get{Short,Char,Int,Long}` and the `*Unaligned` variants take a
/// *byte* offset into the target object.  When the target is a `byte[]`
/// (the backing store of every `HeapByteBuffer`), a single typed read must
/// assemble 2/4/8 consecutive array elements — it is NOT a single-element
/// access.  The generic `native_unsafe_get_int` returns just one element,
/// which truncates every `ByteBuffer.get{Char,Short,Int,Long}` to its first
/// byte (e.g. ICU's `ICUBinary.readHeader` reading the `nfc.nrm` header).
///
/// The 3-arg `*Unaligned` form carries an explicit `bigEndian` boolean at
/// args[3]; the 2-arg form (and the plain aligned `getChar`/`getInt`/…)
/// uses the platform's native byte order, which on every CratonVM host
/// (x86-64 / aarch64) is little-endian.
///
/// Returns `Some(bytes)` only when `obj` is a primitive array and the
/// requested byte range fits; otherwise `None`, so the caller falls back to
/// the generic element/field path (non-array targets, off-heap, references).
///
/// Works for EVERY primitive element type — not just `byte[]`. A multi-byte
/// `Unsafe` read takes a *byte* offset and assembles `width` consecutive
/// bytes which may SPAN several elements: `getLongUnaligned(char[], …)`
/// reads 4 chars, `getLongUnaligned(int[], …)` reads 2 ints, etc. The JDK
/// lays each element out in the platform's native (little-endian) byte
/// order. The previous version handled only `byte[]`/`boolean[]` and fell
/// back to a single-ELEMENT read for `char[]`/`int[]`/`long[]`, which
/// truncated every cross-element word read to its first element — breaking
/// `jdk.internal.util.ArraysSupport.vectorizedMismatch` and therefore
/// `Arrays.equals(char[]/long[])` (e.g. ecj's `CharOperation.equals`
/// mis-comparing `"Signature"` vs `"Synthetic"`).
/// Widest `Unsafe` scalar access, and so the size of the stack buffer
/// [`unsafe_read_bytes_into`] fills. `getLongUnaligned` / `getDouble` are the
/// only 8-byte shapes; nothing in the family is wider.
pub const UNSAFE_MAX_ACCESS_WIDTH: usize = 8;

/// Non-allocating core of [`unsafe_read_bytes_from_array`]: fills
/// `out[..width]` and returns `true`, or returns `false` and leaves `out`
/// unspecified.
///
/// # Why a `byte[]` fast path exists here
///
/// `byte[]` is not one element type among six on this path — it is the backing
/// store of every `HeapByteBuffer`, so it is what `ByteBuffer.getInt` /
/// `getLong` / `getShort` reach, which is the hot shape in any binary-header
/// or HTTP-codec workload. The generic loop below costs `width` VIRTUAL
/// `get_array_element` calls plus, in the old shape, a `Vec` allocation, for a
/// read the heap can serve with one `memcpy`: `read_byte_array_into` is
/// overridden in the VM to `copy_nonoverlapping` from the array payload (with
/// a per-element fallback of its own for a G1 humongous array, so the fast
/// path does not have to know about that case).
///
/// The generic loop is unchanged and still covers `char[]` / `int[]` / `long[]`
/// and friends, which a multi-byte read may legitimately SPAN — the property
/// `ArraysSupport.vectorizedMismatch` depends on and the reason this function
/// is not simply a `byte[]` reader.
pub fn unsafe_read_bytes_into(
    ctx: &dyn NativeContext,
    obj: cratonvm_types::ObjectRef,
    offset: usize,
    width: usize,
    out: &mut [u8; UNSAFE_MAX_ACCESS_WIDTH],
) -> bool {
    use cratonvm_types::ArrayElementType as Aet;
    if width > UNSAFE_MAX_ACCESS_WIDTH {
        return false;
    }
    if ctx.heap_kind_of(obj) != cratonvm_types::ObjectKind::Array {
        return false;
    }
    let elem_type = ctx.heap_element_type_of(obj);
    let elem_size: usize = match elem_type {
        Aet::Byte | Aet::Boolean => 1,
        Aet::Char | Aet::Short => 2,
        Aet::Int | Aet::Float => 4,
        Aet::Long | Aet::Double => 8,
        // Reference arrays have no byte-addressable element storage.
        Aet::Reference => return false,
    };
    // Array byte offsets always start at ABASE (16); see
    // `unsafe_array_index_from_offset` / `native_unsafe_array_base_offset`.
    const ABASE: usize = 16;
    if offset < ABASE {
        return false;
    }
    let rel = offset - ABASE; // bytes from element 0
    let len = ctx.array_length(obj); // element COUNT
    let Some(total_bytes) = len.checked_mul(elem_size) else {
        return false;
    };
    match rel.checked_add(width) {
        Some(end) if end <= total_bytes => {}
        _ => return false,
    }
    if elem_size == 1 {
        // One `memcpy`. `rel` IS the element index when elements are one byte
        // wide, and the range was bounds-checked against `total_bytes` above,
        // so a short return can only mean the heap declined the bulk route —
        // in which case fall through to the generic loop rather than answer
        // with a partly filled buffer.
        if ctx.read_byte_array_into(obj, rel, &mut out[..width]) == width {
            return true;
        }
    } else if ctx.read_primitive_array_bytes(obj, rel, &mut out[..width]) == width {
        // SBR3 ffmperf: every wider element type is a contiguous native-endian
        // payload too; one `memcpy` instead of `width` element reads.
        return true;
    }
    for (k, slot) in out.iter_mut().enumerate().take(width) {
        let byte_pos = rel + k;
        let elem_idx = byte_pos / elem_size;
        let byte_in_elem = byte_pos % elem_size;
        // Read the element's raw bit pattern, then extract the requested
        // byte in little-endian order (native order on x86-64 / aarch64).
        let elem_bits: u64 = match ctx.get_array_element(obj, elem_idx) {
            Value::Int(v) => v as u32 as u64, // byte/short/char/int (low bits used per elem_size)
            Value::Long(v) => v as u64,
            Value::Float(f) => f.to_bits() as u64,
            Value::Double(d) => d.to_bits(),
            _ => return false,
        };
        *slot = ((elem_bits >> (8 * byte_in_elem)) & 0xFF) as u8;
    }
    true
}

pub fn unsafe_read_bytes_from_array(
    ctx: &dyn NativeContext,
    obj: cratonvm_types::ObjectRef,
    offset: usize,
    width: usize,
) -> Option<Vec<u8>> {
    let mut buf = [0u8; UNSAFE_MAX_ACCESS_WIDTH];
    if !unsafe_read_bytes_into(ctx, obj, offset, width, &mut buf) {
        return None;
    }
    Some(buf[..width].to_vec())
}

/// Decode the trailing `bigEndian` boolean of an `*Unaligned` call.
/// `Some(true)` = big-endian, `Some(false)` = little-endian, `None` = the
/// 2-arg form (no explicit order) → native order = little-endian.
pub fn unsafe_big_endian_arg(args: &[Value]) -> bool {
    match args.get(3) {
        Some(Value::Int(z)) => *z != 0,
        // 2-arg form or non-int trailing slot: native (little-endian).
        _ => false,
    }
}

pub fn scoped_memory_access_unsafe_args(args: &[Value]) -> Vec<Value> {
    let mut adapted = Vec::with_capacity(args.len().saturating_sub(1));
    adapted.push(args.first().copied().unwrap_or(Value::Object(None)));
    if args.len() > 2 {
        adapted.extend_from_slice(&args[2..]);
    }
    adapted
}

/// JIT round 21, lane unsafe (page `i94-orch-dev-jit-round-20-regresses-four-
/// interpreter-probes-20261127.md`): the answer of an `Unsafe` read of kind
/// `access` (`B S C I J`) from the generic int / long fallback natives, which
/// return the whole cell they found (a field slot, an element, a static, the
/// four bytes `unsafe_sub_field_load` assembled at byte `k` of a field)
/// rather than the access's own bytes. Keeps the access's low bytes, swaps
/// them for a big-endian `*Unaligned` form, and re-extends them as the
/// access's type: a `getShort` sign-extends, a `getChar` zero-extends, a
/// `getByte` sign-extends -- what HotSpot's `jshort` / `jchar` / `jbyte`
/// return is. A reference (or any non-primitive answer) passes unchanged.
/// Unconditional correctness: nothing narrow may reach a `S` / `C` / `B`
/// return un-narrowed (the interpreter does not narrow native returns).
pub fn unsafe_narrow_read(read: MethodCallResult, access: u8, big_endian: bool) -> MethodCallResult {
    read.map(|value| {
        value.map(|v| {
            let bits = match v {
                // Casts: bit reinterpretation of each primitive kind.
                Value::Int(i) => i as u32 as u64,
                Value::Long(l) => l as u64,
                Value::Float(f) => u64::from(f.to_bits()),
                Value::Double(d) => d.to_bits(),
                other => return other,
            };
            let Some(width) = field_view_width(access) else {
                return v;
            };
            let mut bits = if width < 8 {
                bits & ((1u64 << (width * 8)) - 1)
            } else {
                bits
            };
            if big_endian {
                bits = field_view_swap(bits, width);
            }
            field_view_value(bits, access)
        })
    })
}

// `getShort` → signed 16-bit; `getChar` → unsigned 16-bit; `getInt` → 32-bit.
unsafe_multibyte_get!(native_unsafe_get_short_mb, 2, asm_i16, b'S');

unsafe_multibyte_get!(native_unsafe_get_char_mb, 2, asm_u16, b'C');

unsafe_multibyte_get!(native_unsafe_get_int_mb, 4, asm_i32, b'I');

/// 64-bit `Unsafe` get for `byte[]`-backed memory (`getLong`/`getLongUnaligned`).
pub fn native_unsafe_get_long_mb(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // A FIELD of another kind (i4 wave 87, lane L3).
    if let Some(v) = unsafe_field_view_get(&*ctx, args, b'J', unsafe_big_endian_arg(args)) {
        return Ok(Some(v));
    }
    let offset = unsafe_offset(args, 2);
    if unsafe_obj(args, 1).is_none() {
        let addr = unsafe_raw_addr(args, 2);
        let mut b = [0u8; 8];
        if ctx.copy_from_native_memory(addr, &mut b) {
            let big_endian = unsafe_big_endian_arg(args);
            let v = if big_endian {
                i64::from_be_bytes(b)
            } else {
                i64::from_le_bytes(b)
            };
            return Ok(Some(Value::Long(v)));
        }
    }
    if let Some(obj) = unsafe_obj(args, 1) {
        // As the multi-byte `get` macro: a stack buffer, because this is the
        // body of `HeapByteBuffer.getLong`.
        let mut b = [0u8; UNSAFE_MAX_ACCESS_WIDTH];
        if unsafe_read_bytes_into(ctx, obj, offset, 8, &mut b) {
            let big_endian = unsafe_big_endian_arg(args);
            let v = if big_endian {
                i64::from_be_bytes(b)
            } else {
                i64::from_le_bytes(b)
            };
            return Ok(Some(Value::Long(v)));
        }
    }
    // JIT round 21 (lane unsafe): a big-endian `getLongUnaligned` the view
    // did not serve (bytes inside a field, a static) swaps too.
    let big_endian = unsafe_big_endian_arg(args);
    unsafe_narrow_read(native_unsafe_get_long(ctx, args), b'J', big_endian)
}

/// Multi-byte `Unsafe` put for `byte[]`-backed memory. Scatters a typed
/// value into `width` consecutive array elements honoring endianness.
/// Returns `true` if the write was handled here (byte-array target).
pub fn unsafe_write_bytes_to_byte_array(
    ctx: &mut dyn NativeContext,
    obj: cratonvm_types::ObjectRef,
    offset: usize,
    bytes: &[u8],
) -> bool {
    if ctx.heap_kind_of(obj) != cratonvm_types::ObjectKind::Array {
        return false;
    }
    match ctx.heap_element_type_of(obj) {
        cratonvm_types::ArrayElementType::Byte | cratonvm_types::ArrayElementType::Boolean => {}
        _ => return false,
    }
    const ABASE: usize = 16;
    if offset < ABASE {
        return false;
    }
    let start = offset - ABASE;
    let len = ctx.array_length(obj);
    if start + bytes.len() > len {
        return false;
    }
    // One `memcpy` where the heap can give one — the store twin of
    // `unsafe_read_bytes_into`'s fast path, and the same reason: this is the
    // body of `HeapByteBuffer.putInt`/`putLong`, so the per-element loop below
    // was `width` virtual calls per scalar write.
    //
    // Falling through on `false` is safe even though the override's G1
    // humongous arm can stop partway: the loop below rewrites the WHOLE range
    // from `start`, and a byte store is idempotent, so partial progress is
    // overwritten rather than compounded. The bounds were checked above and
    // `write_byte_array_from` re-checks them itself, so neither path can run
    // off the end.
    if ctx.write_byte_array_from(obj, start, bytes) {
        return true;
    }
    for (i, &b) in bytes.iter().enumerate() {
        // Byte-array elements round-trip as sign-extended Int.
        ctx.set_array_element(obj, start + i, Value::Int(b as i8 as i32));
    }
    true
}

/// SBR3 lane ffmperf: does an `Unsafe.put*` of `width` bytes at byte `offset`
/// of a primitive array of element type `et` need the RAW (byte-spanning) store
/// rather than the typed one-element store of the access's own natural element
/// type `natural`? It does when the array's elements are another type
/// (`putLong(int[], ..)`, `putInt(short[], ..)`, `putByte(int[], ..)`: HotSpot
/// stores the bytes whatever the array holds, which is what the JDK's
/// `SegmentBulkOperations` loops over a heap segment of any carrier do) or when
/// the access straddles elements. A byte/boolean array has its own path.
pub fn unsafe_put_needs_raw(
    et: cratonvm_types::ArrayElementType,
    natural: cratonvm_types::ArrayElementType,
    offset: usize,
    width: usize,
) -> bool {
    use cratonvm_types::ArrayElementType as Aet;
    if matches!(et, Aet::Reference | Aet::Byte | Aet::Boolean) {
        return false;
    }
    if et != natural {
        return true;
    }
    offset >= 16 && (offset - 16) % width != 0
}

unsafe_multibyte_put!(
    native_unsafe_put_short_mb,
    2,
    native_unsafe_put_int,
    Short,
    b'S'
);

unsafe_multibyte_put!(
    native_unsafe_put_char_mb,
    2,
    native_unsafe_put_int,
    Char,
    b'C'
);

unsafe_multibyte_put!(
    native_unsafe_put_int_mb,
    4,
    native_unsafe_put_int,
    Int,
    b'I'
);

/// 64-bit `Unsafe` put for `byte[]`-backed memory.
pub fn native_unsafe_put_long_mb(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // A FIELD of another kind (i4 wave 87, lane L3).
    if unsafe_field_view_put(ctx, args, b'J') {
        return Ok(None);
    }
    let offset = unsafe_offset(args, 2);
    let val = match args.get(3) {
        Some(Value::Long(l)) => *l,
        Some(Value::Int(i)) => *i as i64,
        _ => 0,
    };
    let big_endian = matches!(args.get(4), Some(Value::Int(z)) if *z != 0);
    let bytes = if big_endian {
        (val as u64).to_be_bytes()
    } else {
        (val as u64).to_le_bytes()
    };
    if unsafe_obj(args, 1).is_none() {
        if ctx.copy_to_native_memory(unsafe_raw_addr(args, 2), &bytes) {
            return Ok(None);
        }
    }
    if let Some(obj) = unsafe_obj(args, 1) {
        if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array
            && matches!(
                ctx.heap_element_type_of(obj),
                cratonvm_types::ArrayElementType::Byte | cratonvm_types::ArrayElementType::Boolean
            )
        {
            if unsafe_write_bytes_to_byte_array(ctx, obj, offset, &bytes) {
                return Ok(None);
            }
        }
        // SBR3 ffmperf: see `unsafe_put_needs_raw`.
        if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array
            && unsafe_put_needs_raw(
                ctx.heap_element_type_of(obj),
                cratonvm_types::ArrayElementType::Long,
                offset,
                8,
            )
            && unsafe_array_write_bytes(ctx, obj, offset, &bytes)
        {
            return Ok(None);
        }
        // JIT round 21 (lane unsafe): bytes INSIDE an instance field
        // (byte field offsets) take the lane store with these bytes in
        // memory order -- swapped for a big-endian `putLongUnaligned`; the
        // whole-slot native below would store the unswapped long.
        if unsafe_narrow_field_store(ctx, obj, offset, &bytes) {
            return Ok(None);
        }
    }
    native_unsafe_put_long(ctx, args)
}

/// t1 wave 8 (lane RL-ATOM, re-land of JIT round 21 lane unsafe
/// `29522fc209`; correctness, unconditional): an int-width (or narrower)
/// `Unsafe` read of a STATIC answers the static's low four bytes, as HotSpot's
/// 4-byte load at the static's address does: a `long` / `double` static its
/// low half, a `float` static its bits. These answered 0, so a JDK sub-word
/// `getAndAdd` retry loop on a `long` static compared the lane against a 0 it
/// was never holding. A reference (or anything else) still answers 0.
pub fn unsafe_static_low_int(v: Value) -> Value {
    // Casts: the low 32 bits of each primitive kind's bits.
    match v {
        Value::Int(i) => Value::Int(i),
        Value::Long(l) => Value::Int(l as i32),
        Value::Float(f) => Value::Int(f.to_bits() as i32),
        Value::Double(d) => Value::Int(d.to_bits() as i32),
        _ => Value::Int(0),
    }
}

pub fn native_unsafe_get_int(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let offset = unsafe_offset(args, 2);
    let obj = match unsafe_obj(args, 1) {
        Some(o) => o,
        None => {
            if let Some(v) = unsafe_static_get(ctx, offset) {
                return Ok(Some(unsafe_static_low_int(v)));
            }
            if note_unsafe_side_store_offset(ctx, offset, line!(), args.len()) {
                // §4.5's specified refusal, discharged by §21/§23's count.
                // HotSpot SIGSEGVs here; the side store used to answer, and a
                // CAS reported success having written nowhere.
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!(
                        "Unsafe access with a null base and an offset that is \
                         neither an arena handle, a synthetic offset, nor a \
                         registered static field: {offset:#x}"
                    ),
                }
                .into());
            }
            let key = (ctx.vm_identity(), offset);
            let map = lock_static_int_shard(offset);
            return Ok(Some(Value::Int(map.get(&key).copied().unwrap_or(0))));
        }
    };
    if unsafe_synthetic_field_offset(&*ctx, obj, offset) {
        // JIT round 21 (lane unsafe): a wider / float static answers its
        // low four bytes (the `0` this answered made the JDK's sub-word
        // getAndAdd retry loops on a long static spin: the lane CAS never saw
        // the `0` it was handed as the expected value).
        return Ok(Some(match synthetic_get(ctx, obj, offset) {
            Value::Int(v) => Value::Int(v),
            Value::Long(l) => Value::Int(l as i32),
            Value::Float(f) => Value::Int(f.to_bits() as i32),
            Value::Double(d) => Value::Int(d.to_bits() as i32),
            _ => Value::Int(0),
        }));
    }
    if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
        match unsafe_checked_array_index(ctx, obj, offset) {
            Some(idx) => Ok(Some(ctx.get_array_element(obj, idx))),
            None => Ok(Some(Value::Int(0))),
        }
    } else {
        // JIT round 20 wave 7 (lane huntDisp): an int-sized read AT a
        // `float` field's own offset is the field's bits, as HotSpot reads
        // its 4 bytes; the slot's `Value::Float` reached the `I` return and
        // read as 0 (`R20HuntDispUnsafeField` `pun.first`), while compiled
        // code answered the bits.
        // Wave 8c: an offset INSIDE a field (byte field offsets) reads its
        // bytes; the caller's return narrows a byte / short / char read.
        let (offset, k) = unsafe_field_slot(&*ctx, obj, offset);
        if k != 0 {
            // Cast: the low 32 bits of the zero-extended bytes.
            return Ok(Some(Value::Int(
                unsafe_sub_field_load(&*ctx, obj, offset, k, 4) as u32 as i32,
            )));
        }
        Ok(Some(match ctx.get_field(obj, offset) {
            Value::Float(f) => Value::Int(f.to_bits() as i32),
            // Wave 8: a narrower read of a `double` field is its low bytes.
            Value::Double(d) => Value::Int(d.to_bits() as i32),
            other => other,
        }))
    }
}

pub fn native_unsafe_put_int(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let offset = unsafe_offset(args, 2);
    let val = args.get(3).copied().unwrap_or(Value::Int(0));
    let obj = match unsafe_obj(args, 1) {
        Some(o) => o,
        None => {
            let v = if let Value::Int(i) = val { i } else { 0 };
            if unsafe_static_put(ctx, offset, Value::Int(v)) {
                return Ok(None);
            }
            if note_unsafe_side_store_offset(ctx, offset, line!(), args.len()) {
                // §4.5's specified refusal, discharged by §21/§23's count.
                // HotSpot SIGSEGVs here; the side store used to answer, and a
                // CAS reported success having written nowhere.
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!(
                        "Unsafe access with a null base and an offset that is \
                         neither an arena handle, a synthetic offset, nor a \
                         registered static field: {offset:#x}"
                    ),
                }
                .into());
            }
            lock_static_int_shard(offset).insert((ctx.vm_identity(), offset), v);
            return Ok(None);
        }
    };
    if unsafe_synthetic_field_offset(&*ctx, obj, offset) {
        // JIT round 21 (lane unsafefix; correctness, unconditional): an int
        // store AT a `long` / `double` static's own offset replaces its low
        // four bytes (HotSpot's 4-byte store), not the whole static.
        if let Some(bits) = unsafe_value_bits(val) {
            // Cast: the low four bytes in memory order.
            let bytes = (bits as u32).to_le_bytes();
            if unsafe_narrow_field_store(ctx, obj, offset, &bytes) {
                return Ok(None);
            }
        }
        synthetic_put(ctx, obj, offset, val);
        return Ok(None);
    }
    if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
        if let Some(idx) = unsafe_checked_array_index(ctx, obj, offset) {
            ctx.set_array_element(obj, idx, val);
        }
        // OOB offset => no-op (benign), never reaches the host accessor.
    } else {
        // Wave 8c: an offset INSIDE a field (byte field offsets) replaces
        // four of its bytes.
        let (offset, k) = unsafe_field_slot(&*ctx, obj, offset);
        if k != 0 {
            let bits = unsafe_value_bits(val).unwrap_or(0);
            unsafe_sub_field_store(ctx, obj, offset, k, 4, bits);
            return Ok(None);
        }
        ctx.set_field(obj, offset, val);
    }
    Ok(None)
}

/// `Unsafe.getByte(Object, long)` / `getBoolean` — width-correct 1-byte read.
///
/// A NULL base means an off-heap absolute address: route it through the SAME
/// arena-or-raw path the NIO socket / FileChannel I/O uses
/// (`copy_from_native_memory`) so a `DirectByteBuffer` round-trips byte-wise
/// with native reads/writes. The previous handler (`native_unsafe_get_int`)
/// stashed null-base bytes in a separate `static_int_store`, which diverged
/// from the raw/arena memory the socket layer reads — so Tomcat's http-nio
/// byte-wise request parsing saw zeros. A heap base falls back to the generic
/// field/array accessor.
pub fn native_unsafe_get_byte_mb(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    match unsafe_obj(args, 1) {
        None => {
            let addr = unsafe_raw_addr(args, 2);
            let mut b = [0u8; 1];
            ctx.copy_from_native_memory(addr, &mut b);
            // getByte returns a (sign-extended) byte; getBoolean coerces 0/non-0.
            Ok(Some(Value::Int(b[0] as i8 as i32)))
        }
        Some(obj) => {
            // The low byte of a wider FIELD (i4 wave 87, lane L3).
            if let Some(v) = unsafe_field_view_get(&*ctx, args, b'B', false) {
                return Ok(Some(v));
            }
            // A byte inside a wider array element (`int[]`, `long[]`, `short[]`,
            // `char[]`, and `float[]`/`double[]`): the byte lane, not the element
            // (the `SubwordLane` shapes are a subset of this raw read; a load needs
            // no atomicity).
            // JIT round 20 wave 12 (lane qUnsafe, fuzz `R20FuzzEUnsafeArr1`):
            // a `boolean[]` byte too. The element read below zero-extends the
            // byte, where `getByte` sign-extends (HotSpot reads the raw byte).
            if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array
                && !matches!(
                    ctx.heap_element_type_of(obj),
                    cratonvm_types::ArrayElementType::Reference
                )
            {
                // Raw boolean-array bytes are not baload/bastore values:
                // Unsafe.putByte can install any eight-bit pattern.
                let mut b = [0u8; UNSAFE_MAX_ACCESS_WIDTH];
                if unsafe_read_bytes_into(ctx, obj, unsafe_offset(args, 2), 1, &mut b) {
                    return Ok(Some(Value::Int(b[0] as i8 as i32)));
                }
            }
            // JIT round 21 (lane unsafe): the low byte, sign-extended.
            unsafe_narrow_read(native_unsafe_get_int(ctx, args), b'B', false)
        }
    }
}

/// `Unsafe.getBoolean(Object, long)`: [`native_unsafe_get_byte_mb`]'s byte,
/// normalized as HotSpot's `normalize_for_read(jboolean)` does (`x != 0`).
/// JIT round 20 wave 7 (lane huntDisp): the raw byte reached the `Z` return,
/// whose boolean narrowing keeps bit 0 only, so a byte of 2 or -12 read as
/// `false` where HotSpot answers `true` (`R20HuntDispUnsafeField`
/// `pun.after`), in both tiers.
pub fn native_unsafe_get_boolean_mb(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    normalize_unsafe_boolean_read(native_unsafe_get_byte_mb(ctx, args))
}

/// `Unsafe.getBooleanVolatile`: [`native_unsafe_get_byte_volatile`],
/// normalized as [`native_unsafe_get_boolean_mb`].
pub fn native_unsafe_get_boolean_volatile(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    normalize_unsafe_boolean_read(native_unsafe_get_byte_volatile(ctx, args))
}

pub fn normalize_unsafe_boolean_read(read: MethodCallResult) -> MethodCallResult {
    read.map(|value| {
        value.map(|v| match v {
            Value::Int(i) => Value::Int(i32::from(i != 0)),
            other => other,
        })
    })
}

/// Decode a raw off-heap ADDRESS argument with NO field-offset clamping.
///
/// The `base == null` forms of `Unsafe.{get,put}*(Object, long, …)` pass a full
/// native address in the `long` slot, not a field index. `unsafe_offset`'s
/// clamp (which exists to neutralise garbage *field* slot indices from
/// compact-value drift) zeroes any value above `MAX_REASONABLE_OFFSET` that is
/// not a tagged arena handle — which silently rewrites a **real** direct-buffer
/// pointer (e.g. a Jetty `ByteBufferPool` allocation at `0x000002_8…`) to
/// address 0. `DirectByteBuffer.put(byte)`/`get(byte)` (≤6-element transfers via
/// `ScopedMemoryAccess`) then wrote/read at address 0, so e.g. Jetty
/// HttpClient's byte-at-a-time request framing produced an all-zero request no
/// server could parse. `copy_to/from_native_memory` already validate the
/// address (arena-vs-raw routing, bounds, null reject), so passing the raw
/// value through here is safe — the clamp's field-index protection is only
/// meaningful for the `base != null` path.
pub fn unsafe_raw_addr(args: &[Value], pos: usize) -> i64 {
    match args.get(pos) {
        Some(Value::Long(a)) => *a,
        Some(Value::Int(a)) => *a as i64,
        // Compact-value drift can surface a long's bit pattern as Double/Float
        // on the invoke-arg boundary (see `unsafe_offset`); mirror its decode.
        Some(Value::Double(d)) => {
            if d.is_finite() {
                let n = *d as i64;
                if n >= 0 && (n as f64) == *d {
                    return n;
                }
            }
            d.to_bits() as i64
        }
        Some(Value::Float(f)) => {
            if f.is_finite() {
                let n = *f as i64;
                if n >= 0 && (n as f32) == *f {
                    return n;
                }
            }
            f.to_bits() as u32 as i64
        }
        _ => 0,
    }
}

/// JIT round 20, lane rev12: `Unsafe.putBoolean` / `putBooleanVolatile`
/// store `x & 1`, as HotSpot does (`MemoryAccess::normalize_for_write` in
/// `unsafe.cpp`; C2's `T_BOOLEAN` store masks the same way), so a `Z`
/// argument outside `0`/`1` -- which the verifier admits from any `int` --
/// stores its low bit, not its low byte. `None` when the value is already
/// `0`/`1` (every javac-compiled call): the byte natives then run on the
/// caller's own slice.
pub fn unsafe_put_boolean_args(args: &[Value]) -> Option<Vec<Value>> {
    match args.get(3) {
        Some(Value::Int(v)) if *v & !1 != 0 => {
            let mut adapted = args.to_vec();
            adapted[3] = Value::Int(*v & 1);
            Some(adapted)
        }
        _ => None,
    }
}

/// `Unsafe.putBoolean(Object, long, boolean)`: [`native_unsafe_put_byte_mb`]
/// of the normalised value ([`unsafe_put_boolean_args`]).
pub fn native_unsafe_put_boolean_mb(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    match unsafe_put_boolean_args(args) {
        Some(adapted) => native_unsafe_put_byte_mb(ctx, &adapted),
        None => native_unsafe_put_byte_mb(ctx, args),
    }
}

/// `Unsafe.putBooleanVolatile(Object, long, boolean)`:
/// [`native_unsafe_put_byte_volatile`] of the normalised value
/// ([`unsafe_put_boolean_args`]).
pub fn native_unsafe_put_boolean_volatile(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    match unsafe_put_boolean_args(args) {
        Some(adapted) => native_unsafe_put_byte_volatile(ctx, &adapted),
        None => native_unsafe_put_byte_volatile(ctx, args),
    }
}

/// `Unsafe.putByte(Object, long, byte)` / `putBoolean` — width-correct 1-byte
/// write. See [`native_unsafe_get_byte_mb`] for the null-base routing rationale.
pub fn native_unsafe_put_byte_mb(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    match unsafe_obj(args, 1) {
        None => {
            let addr = unsafe_raw_addr(args, 2);
            let v = match args.get(3) {
                Some(Value::Int(i)) => *i as u8,
                _ => 0,
            };
            ctx.copy_to_native_memory(addr, &[v]);
            Ok(None)
        }
        Some(obj) => {
            // A raw byte store must bypass boolean bastore's value & 1
            // conversion. The byte/boolean bulk accessor preserves the wire
            // byte, including its sign bit, across every collector.
            if let Some(Value::Int(v)) = args.get(3) {
                if unsafe_write_bytes_to_byte_array(ctx, obj, unsafe_offset(args, 2), &[*v as u8]) {
                    return Ok(None);
                }
            }
            // The low byte of a wider FIELD (i4 wave 87, lane L3).
            if unsafe_field_view_put(ctx, args, b'B') {
                return Ok(None);
            }
            // A byte lane of a wider element: atomic (`SubwordLane`).
            if let Some(Value::Int(v)) = args.get(3) {
                if unsafe_subword_array_store(ctx, obj, unsafe_offset(args, 2), &[*v as u8]) {
                    return Ok(None);
                }
            }
            // SBR3 ffmperf (whatever the lane declines, e.g. float[]/double[]): a byte store into a short/char/int/long/float/double
            // array lands in one BYTE of an element (`SegmentBulkOperations`'
            // tail bytes over a heap segment of any carrier), not in a whole
            // element.
            if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
                let et = ctx.heap_element_type_of(obj);
                // JIT round 20 wave 12 (lane qUnsafe, fuzz `R20FuzzEUnsafeArr1`):
                // a `boolean[]` takes the raw byte store too. The element store
                // (`set_array_element`) narrows a boolean element to `v & 1`
                // (`bastore`'s rule), so `Unsafe.putByte(boolean[], 2)` stored
                // 0 where HotSpot stores the byte 2 (and `getBoolean` then
                // answers `true`). `putBoolean` masks `x & 1` before it gets
                // here (`native_unsafe_put_boolean_mb`).
                if !matches!(
                    et,
                    cratonvm_types::ArrayElementType::Reference
                        | cratonvm_types::ArrayElementType::Byte
                ) {
                    let v = match args.get(3) {
                        Some(Value::Int(i)) => *i as u8,
                        _ => 0,
                    };
                    if unsafe_array_write_bytes(ctx, obj, unsafe_offset(args, 2), &[v]) {
                        return Ok(None);
                    }
                }
            }
            // JIT round 20 wave 8 (lane huntDisp): a byte stored AT a wider
            // instance field's offset is its low byte only.
            if let Some(Value::Int(v)) = args.get(3) {
                if unsafe_narrow_field_store(ctx, obj, unsafe_offset(args, 2), &[*v as u8]) {
                    return Ok(None);
                }
            }
            native_unsafe_put_int(ctx, args)
        }
    }
}

/// JIT round 20 wave 8 (lane huntDisp,
/// `jr20-unsafe-sub-field-offsets-alias-neighbouring-slots-FIXED-20261012.md`): an
/// `Unsafe` store of `bytes` (in memory order, as the put natives lay them
/// out) AT the offset of an instance field WIDER than the store. HotSpot
/// writes those bytes at `base + offset`, i.e. replaces the field's low
/// `bytes.len()` bytes (little-endian) and keeps the rest; the whole-slot
/// store replaced the entire value (`putByte` of `0x11` into a `short` of -2
/// gave 17, HotSpot -239). Read-modify-write of the slot's value by the
/// field's declared descriptor, re-encoded as that descriptor's `Value`.
/// `false` (nothing written) for an array, a synthetic offset, an
/// unresolvable slot, a reference field, or a store as wide as the field or
/// wider -- the caller's path then runs as before. Not atomic against a
/// concurrent writer of the same field's other bytes (a plain store; the
/// volatile and CAS natives keep their own paths).
pub fn unsafe_narrow_field_store(
    ctx: &mut dyn NativeContext,
    obj: cratonvm_types::ObjectRef,
    offset: usize,
    bytes: &[u8],
) -> bool {
    if bytes.is_empty() || ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
        return false;
    }
    if unsafe_synthetic_field_offset(&*ctx, obj, offset) {
        // Wave 11: byte `k != 0` of a static (byte field offsets), at the
        // store's own width; every other synthetic offset keeps its path.
        // JIT round 21 (lane unsafefix; correctness, unconditional): and the
        // static's OWN offset (`k = 0`, either offset encoding) when the store
        // is narrower than the static ([`unsafe_static_exact_narrow_target`]).
        // `putByte(staticFieldBase, offsetOf(short S), b)` went to the
        // whole-value `synthetic_put` and replaced the short with the byte
        // (HotSpot keeps the high byte: -2403, not -99).
        let Some((class_id, field_index, k)) = unsafe_static_sub_target(&*ctx, Some(obj), offset)
            .or_else(|| unsafe_static_exact_narrow_target(&*ctx, obj, offset, bytes.len()))
        else {
            return false;
        };
        let bits = bytes
            .iter()
            .rev()
            .fold(0u64, |acc, b| (acc << 8) | u64::from(*b));
        let _ = unsafe_static_lane_rmw(
            ctx,
            offset - k,
            class_id,
            field_index,
            k,
            bytes.len(),
            &|_| Some(bits),
        );
        return true;
    }
    // Wave 8c: under byte field offsets, a store INSIDE a field (`k != 0`)
    // replaces those bytes of it, whatever its width; it is never the
    // whole-slot store of the caller's fallback.
    let (offset, k) = unsafe_field_slot(&*ctx, obj, offset);
    if k != 0 {
        let bits = bytes
            .iter()
            .rev()
            .fold(0u64, |acc, b| (acc << 8) | u64::from(*b));
        unsafe_sub_field_store(ctx, obj, offset, k, bytes.len(), bits);
        return true;
    }
    let Some(desc) = ctx.instance_field_descriptor_byte(obj, offset) else {
        return false;
    };
    let field_width = match desc {
        b'B' | b'Z' => 1,
        b'S' | b'C' => 2,
        b'I' | b'F' => 4,
        b'J' | b'D' => 8,
        _ => return false,
    };
    if bytes.len() >= field_width {
        return false;
    }
    let bits: u64 = match ctx.get_field(obj, offset) {
        Value::Int(i) => i as u32 as u64,
        Value::Long(l) => l as u64,
        Value::Float(f) => f.to_bits() as u64,
        Value::Double(d) => d.to_bits(),
        _ => return false,
    };
    let mut le = bits.to_le_bytes();
    le[..bytes.len()].copy_from_slice(bytes);
    let merged = u64::from_le_bytes(le);
    // Casts: each field's value is its low `field_width` bytes, re-extended
    // as the descriptor's Java type.
    let value = match desc {
        b'S' => Value::Int(merged as u16 as i16 as i32),
        b'C' => Value::Int(merged as u16 as i32),
        b'I' => Value::Int(merged as u32 as i32),
        b'F' => Value::Float(f32::from_bits(merged as u32)),
        b'J' => Value::Long(merged as i64),
        b'D' => Value::Double(f64::from_bits(merged)),
        _ => return false,
    };
    ctx.set_field(obj, offset, value);
    true
}

pub fn native_unsafe_get_long(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let offset = unsafe_offset(args, 2);
    // C32: Buffer.address sentinel. `objectFieldOffset(Buffer.address)` returns
    // this sentinel; `getLong(buffer, sentinel)` must return that buffer's REAL
    // native address — Netty's `PlatformDependent0.directBufferAddress()` uses
    // exactly this path to compute `memoryAddress()` for *every* direct buffer,
    // not just the `<clinit>` availability probe. Returning a constant `1` made
    // every direct buffer's address `0x1`, so the first write SIGSEGV'd
    // (crash-03: rsocket/Netty PooledByteBuf). Read the receiver's `address`
    // field; fall back to `1` (non-zero, so the init probe still passes) only
    // when there is no readable native address.
    if offset == BUFFER_ADDRESS_SENTINEL {
        if let Some(obj) = unsafe_obj(args, 1) {
            if let Value::Long(addr) = ctx.get_field_by_name(obj, "address") {
                if addr != 0 {
                    return Ok(Some(Value::Long(addr)));
                }
            }
        }
        return Ok(Some(Value::Long(1)));
    }
    let obj = match unsafe_obj(args, 1) {
        Some(o) => o,
        None => {
            if let Some(value) = unsafe_static_get(ctx, offset) {
                return Ok(Some(match value {
                    Value::Long(v) => Value::Long(v),
                    Value::Int(v) => Value::Long(v as i64),
                    _ => Value::Long(0),
                }));
            }
            if note_unsafe_side_store_offset(ctx, offset, line!(), args.len()) {
                // §4.5's specified refusal, discharged by §21/§23's count.
                // HotSpot SIGSEGVs here; the side store used to answer, and a
                // CAS reported success having written nowhere.
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!(
                        "Unsafe access with a null base and an offset that is \
                         neither an arena handle, a synthetic offset, nor a \
                         registered static field: {offset:#x}"
                    ),
                }
                .into());
            }
            let key = (ctx.vm_identity(), offset);
            let map = lock_static_long_shard(offset);
            return Ok(Some(Value::Long(map.get(&key).copied().unwrap_or(0))));
        }
    };
    if unsafe_synthetic_field_offset(&*ctx, obj, offset) {
        return Ok(Some(match synthetic_get(ctx, obj, offset) {
            Value::Long(v) => Value::Long(v),
            Value::Int(v) => Value::Long(v as i64),
            // JIT round 21 (lane unsafe): a double static answers its bits.
            Value::Double(d) => Value::Long(d.to_bits() as i64),
            _ => Value::Long(0),
        }));
    }
    let v = if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
        match unsafe_checked_array_index(ctx, obj, offset) {
            Some(idx) => ctx.get_array_element(obj, idx),
            None => Value::Long(0),
        }
    } else {
        // Wave 8c: an offset INSIDE a field (byte field offsets) reads its
        // remaining bytes.
        match unsafe_field_slot(&*ctx, obj, offset) {
            // Cast: the zero-extended bytes.
            (slot, k) if k != 0 => {
                Value::Long(unsafe_sub_field_load(&*ctx, obj, slot, k, 8) as i64)
            }
            (slot, _) => ctx.get_field(obj, slot),
        }
    };
    // Coerce non-Long values to Long(0) so stack type stays valid; PD0$4's
    // `lstore` would otherwise see an Object and mis-interpret the slot.
    match v {
        Value::Long(_) => Ok(Some(v)),
        Value::Int(i) => Ok(Some(Value::Long(i as i64))),
        // JIT round 20 wave 7 (lane huntDisp): a `getLong` AT a `double`
        // field's own offset is the field's bits (HotSpot reads its 8
        // bytes), not 0 (`R20HuntDispUnsafeField` `pun.*`).
        Value::Double(d) => Ok(Some(Value::Long(d.to_bits() as i64))),
        _ => Ok(Some(Value::Long(0))),
    }
}

pub fn native_unsafe_put_long(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let offset = unsafe_offset(args, 2);
    let val = args.get(3).copied().unwrap_or(Value::Long(0));
    let obj = match unsafe_obj(args, 1) {
        Some(o) => o,
        None => {
            let v = if let Value::Long(i) = val { i } else { 0 };
            if unsafe_static_put(ctx, offset, Value::Long(v)) {
                return Ok(None);
            }
            if note_unsafe_side_store_offset(ctx, offset, line!(), args.len()) {
                // §4.5's specified refusal, discharged by §21/§23's count.
                // HotSpot SIGSEGVs here; the side store used to answer, and a
                // CAS reported success having written nowhere.
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!(
                        "Unsafe access with a null base and an offset that is \
                         neither an arena handle, a synthetic offset, nor a \
                         registered static field: {offset:#x}"
                    ),
                }
                .into());
            }
            lock_static_long_shard(offset).insert((ctx.vm_identity(), offset), v);
            return Ok(None);
        }
    };
    if unsafe_synthetic_field_offset(&*ctx, obj, offset) {
        synthetic_put(ctx, obj, offset, val);
        return Ok(None);
    }
    if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
        if let Some(idx) = unsafe_checked_array_index(ctx, obj, offset) {
            ctx.set_array_element(obj, idx, val);
        }
        // OOB offset => no-op (benign), never reaches the host accessor.
    } else {
        // Wave 8c: an offset INSIDE a field replaces its remaining bytes.
        let (offset, k) = unsafe_field_slot(&*ctx, obj, offset);
        if k != 0 {
            let bits = unsafe_value_bits(val).unwrap_or(0);
            unsafe_sub_field_store(ctx, obj, offset, k, 8, bits);
            return Ok(None);
        }
        ctx.set_field(obj, offset, val);
    }
    Ok(None)
}

pub fn native_unsafe_allocate_instance(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args[0] = Unsafe this, args[1] = Class mirror
    // A null class. HotSpot 25.0.4+7 SIGSEGVs in `Unsafe_AllocateInstance`
    // rather than refusing (measured -- it is why the null-argument rows of
    // this lane run one call per process), so again the oracle has no answer
    // and the choice is ours. CratonVM returned a null REFERENCE, which moves
    // the failure to the caller's next dereference and names nothing.
    if matches!(args.get(1), Some(Value::Object(None))) {
        return Err(RuntimeError::NullPointerException {
            message: Some("Unsafe.allocateInstance: null class".to_string()),
        }
        .into());
    }
    // MEASURED, HotSpot 25.0.4+7: an interface, an abstract class, a primitive
    // class, `void` and an array class are each an `InstantiationException`.
    // CratonVM allocated something for all five. An "instance" of an interface
    // is the fabricated-receiver shape this whole campaign exists to remove --
    // it is handed back with the interface's own name and fails arbitrarily far
    // away, which is exactly what Phase 1 was about.
    let Some(Value::Object(Some(class_obj))) = args.get(1).copied() else {
        return Ok(Some(Value::Object(None)));
    };
    // J8 (round 15 wave 5, lane jdkonly7): the mirror is resolved to its class
    // id ONCE (every constructor `MethodHandle` allocates through here). This
    // resolved it three times: inside `mirror_class_name`, for the flag test
    // and again for the allocation. The name is `mirror_class_name`'s answer
    // exactly -- `class_name_of_id` of the resolved id, its slot-1 reading
    // only for a mirror the reverse map does not know.
    let direct_cid = ctx.class_id_from_mirror(class_obj);
    let cid_opt = direct_cid.or_else(|| match ctx.get_field(class_obj, 0) {
        Value::Int(c) if c >= 0 => Some(cratonvm_types::ClassId::new(c as u32)),
        _ => None,
    });
    let mirror_name = match direct_cid {
        Some(cid) => ctx.class_name_of_id(cid),
        None => crate::lang_class::mirror_class_name(ctx, class_obj),
    };
    if let Some(name) = mirror_name.as_ref() {
        let is_primitive = matches!(
            name.as_str(),
            "boolean" | "byte" | "char" | "short" | "int" | "long" | "float" | "double" | "void"
        );
        if is_primitive {
            return unsafe_instantiation_exception(ctx, None);
        }
        if name.starts_with('[') {
            // An array `Klass` fails `check_valid_for_instantiation`
            // (lane jdkonly6: named, `[I` / `[Ljava.lang.String;`).
            let external = true.then(|| name.class_text(&*ctx));
            return unsafe_instantiation_exception(ctx, external.as_deref());
        }
    }
    let Some(class_id) = cid_opt else {
        return Ok(Some(Value::Object(None)));
    };
    let ACC_INTERFACE = cratonvm_types::access_flags::ACC_INTERFACE;
    let ACC_ABSTRACT = cratonvm_types::access_flags::ACC_ABSTRACT;
    let flags = ctx.class_access_flags(class_id);
    // Round 15 wave 4, lane jdkonly6: HotSpot's
    // `InstanceKlass::check_valid_for_instantiation` names the class
    // (`THROW_MSG(.., external_name())`) for an interface / abstract
    // class, and refuses `java.lang.Class` itself with an
    // `IllegalAccessException` naming it; a class `Klass` that does not
    // exist (a primitive) throws the bare exception above. Both
    // answers were CratonVM's bare `InstantiationException` / an
    // allocated `Class`. `CRATONVM_UNSAFE_ALLOCATE_INSTANCE_CHECKS=0`
    // restores them (read on the refusal path only).
    let refusal = flags & (ACC_INTERFACE | ACC_ABSTRACT) != 0;
    let class_itself = !refusal
        && mirror_name.as_deref() == Some("java/lang/Class");
    if refusal || class_itself {
        let external = true
            .then(|| ctx.class_name_of_id(class_id))
            .flatten()
            .map(|name| name.class_text(&*ctx));
        if class_itself {
            return Err(RuntimeError::IllegalAccessException {
                message: external.unwrap_or_else(|| "java.lang.Class".to_string()),
            }
            .into());
        }
        return unsafe_instantiation_exception(ctx, external.as_deref());
    }
    // Allocate without calling `<init>`. Round 15 wave 4, lane jdkonly6
    // (`r15w4-jdkonly5-native-allocation-skips-class-init-FIXED-20260930.md`, JO5-1):
    // through the one native-instantiate funnel, which initializes the class
    // first as HotSpot's `InstanceKlass::allocate_instance(java_class)` does
    // (`<clinit>` side effects are seen by the first allocation; a failing
    // `<clinit>` is the answer -- `ExceptionInInitializerError`, then
    // `NoClassDefFoundError`), and allocates the mirror's EXACT class. This
    // re-resolved the class BY NAME (`allocate_instance`, a class-manager
    // write lock per call), which answers the first same-named definer, and
    // never initialized. `DirectMethodHandle.allocateInstance` (every
    // constructor method handle) relies on the initialization.
    //
    // Interpreter round i2 wave 58, lane L3: an uninitialized class is
    // initialized first, its failure listed under the JDK frames HotSpot
    // raises it in (`allocate_instance_initialize`).
    if !ctx.is_class_initialized(class_id) {
        allocate_instance_initialize(ctx, args.first().copied(), class_id)?;
    }
    // zr z8/nr (`zgc-zr7rv6-near-full-floor-runs-the-heap-dry-before-non-collecting-native-doors-20261013.md`):
    // on every collector (ZGC; Generational and G1 since gci i48/zl2) a
    // refused allocation collects before the error, as HotSpot's
    // `Unsafe_AllocateInstance` does (every constructor `MethodHandle` and
    // `Constructor.newInstance` allocate here). The native holds no heap
    // reference from here on (`args` is not read again), so the ladder's
    // collection cannot dangle anything. `CRATONVM_ZGC_NATIVE_DOORS_COLLECT=0`
    // keeps the one-attempt `instantiate`.
    if ctx.native_doors_collect_on_refusal() {
        if let Some(obj) = ctx.try_instantiate_no_constructor(class_id)? {
            return Ok(Some(Value::Object(Some(obj))));
        }
        let made = crate::util_concurrent_ext::retry_after_reclaim(ctx, |ctx| {
            ctx.try_instantiate_no_constructor(class_id).transpose()
        });
        if let Some(made) = made {
            return Ok(Some(Value::Object(Some(made?))));
        }
        // Every rung spent: the one-attempt door below raises the error.
    }
    let built = ctx.instantiate(
        class_id,
        cratonvm_native_api::registry::NativeConstruct::NoConstructor,
    )?;
    Ok(Some(Value::Object(built)))
}

/// `Unsafe.allocateInstance`'s initialization of `class_id`, its failure
/// listed as HotSpot raises it: inside `jdk.internal.misc.Unsafe
/// .allocateInstance (Native Method)`, under `sun.misc.Unsafe.allocateInstance`
/// when the receiver is a `sun.misc.Unsafe` (measured, JDK 25.0.3). Interpreter
/// round i2 wave 58, lane L3.
#[cold]
#[inline(never)]
pub fn allocate_instance_initialize(
    ctx: &mut dyn NativeContext,
    receiver: Option<Value>,
    class_id: cratonvm_types::ClassId,
) -> Result<(), cratonvm_types::error::MethodCallFailed> {
    use cratonvm_native_api::{ServedRaiseChain, ServedRaiseHop, ServedRaiseHopSite};
    const DESC: &str = "(Ljava/lang/Class;)Ljava/lang/Object;";
    let sun_misc = match receiver {
        Some(Value::Object(Some(r))) => {
            ctx.class_name_of_id(ctx.class_id_of_object(r)).as_deref() == Some("sun/misc/Unsafe")
        }
        _ => false,
    };
    let mut hops = Vec::with_capacity(2);
    if sun_misc {
        hops.push(ServedRaiseHop {
            class: "sun/misc/Unsafe",
            name: "allocateInstance",
            descriptor: DESC,
            site: ServedRaiseHopSite::LastCall {
                name: "allocateInstance",
                descriptor: Some(DESC),
            },
        });
    }
    hops.push(ServedRaiseHop {
        class: "jdk/internal/misc/Unsafe",
        name: "allocateInstance",
        descriptor: DESC,
        site: ServedRaiseHopSite::Raise,
    });
    ctx.initialize_class_served(
        class_id,
        ServedRaiseChain {
            throwable_class: "java/lang/ExceptionInInitializerError",
            hops,
        },
    )
}



pub fn native_unsafe_fence(
    _ctx: &mut dyn NativeContext,
    _args: &[Value],
) -> MethodCallResult {
    std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
    Ok(None)
}

pub fn native_unsafe_park(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // args: [unsafe, isAbsolute(bool), time(long)]
    // JDK spec: if interrupted, park returns immediately (no exception, flag
    // NOT cleared). Wave 79, lane L6: that return is taken inside the park
    // door below (`park_with_state` returns first thing on the flag), so the
    // `jdk.ThreadPark` event is posted for it, as HotSpot's `Parker::park`
    // returns inside `Unsafe_Park`'s event.
    let is_absolute = match args.get(1) {
        Some(Value::Int(v)) => *v != 0,
        _ => false,
    };
    // CLUSTER-A: long argument may arrive as Value::Double due to operand-stack
    // tag-loss for category-2 longs (CompactValue::long stores raw i64 untagged,
    // and `to_value()` decodes untagged bits as Double). Recover via bit-reinterpret.
    let time: i64 = match args.get(2) {
        Some(Value::Long(t)) => *t,
        Some(Value::Double(d)) => d.to_bits() as i64,
        Some(Value::Int(t)) => *t as i64,
        _ => 0,
    };

    // `unsafe_park_timeout` answers `Parker::park`'s `time < 0 || (isAbsolute
    // && time == 0)` early return as `Some(ZERO)` (gci i21/qh); interpreter
    // round i5 wave 102, lane L6 (`L6W102ParkUntilZero`) fixed the same
    // `parkUntil(0)` hang on dev, folded into it at the 2026-10-16 merge.
    let timeout = if is_absolute {
        // time is an absolute deadline in milliseconds since the epoch
        let now_ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
            .unwrap_or(0);
        unsafe_park_timeout(true, time, now_ms)
    } else {
        unsafe_park_timeout(false, time, 0)
    };

    // gci i22/rv (finding 3 of
    // `gci-i22rv-i21-default-path-review-findings-FIXED-20261016`): an elapsed or
    // non-positive deadline (`Some(ZERO)`) returns at once through the door
    // (the permit taken, the event posted), as `Parker::park` returns for it.
    // It never reaches `try_yield_virtual_park`, which reads `Some(ZERO)` as
    // `wake_after_nanos: 0` -- the UNTIMED yield -- so a VM-scheduled virtual
    // thread (`CRATONVM_VT_CONTINUATIONS=0`) parked forever on a past deadline.
    if ctx.is_interrupted(false) || unsafe_park_returns_at_once(timeout) {
        unsafe_park_door(ctx, timeout, is_absolute, time);
        return Ok(None);
    }
    if let Some(yielded) = try_yield_virtual_park(ctx, timeout) {
        return yielded;
    }
    // Interpreter round i5 wave 101, lane L2 (`CRATONVM_VTHREAD_COMPILED_LEARNED`,
    // default off): a JDK virtual thread parked here from
    // `VirtualThread.parkOnCarrierThread` because a compiled frame (or a VM
    // door's pin) pinned its yield, where HotSpot unmounts: the scheduler
    // compensates for the carrier, as for a pinned `Object.wait`.
    //
    // gci i17/vt (`CRATONVM_VT_PINNED_PARK_COMPENSATE`, opt-in): a mounted JDK
    // virtual thread parks here only when its yield was refused
    // (`VirtualThread.parkOnCarrierThread`), holding its carrier for the
    // park; the scheduler adds one for the time of it, as `Object.wait`'s
    // native does (`monitor_wait_compensated`). The compensation of
    // b056db72a1 went into `native_unsafe_park_with_blocker`, an overload no
    // supported image declares (unregistered since 2026-08-29), so this,
    // the live `park(ZJ)V`, never compensated: a scheduler whose every
    // carrier holds a pinned park starves its queue. HotSpot 25 does not
    // compensate a pinned park either, but pins far less, hence opt-in.
    // Either switch compensates (merge of dev into the gci round,
    // 2026-10-16); dev's predicate is asked first so its census counts.
    //
    // Interpreter round i5 wave 108, lane L6 (page `i91-L6`, "Wave 108"):
    // the learned arm's compensation only for a park that could starve its
    // carrier's queue -- untimed, or timed past
    // `LEARNED_COMPENSATION_MIN_PARK`. A short timed pinned park ends by
    // itself; compensating it made the scheduler add a carrier per
    // concurrently pinned park (up to its max pool size), and those extra
    // carriers outlive the burst by the pool's keep-alive (30 s), so every
    // later rep ran oversubscribed: a run-to-run bimodal `helpers virtual`
    // row, by reading. HotSpot compensates no pinned park.
    let short_timed = timeout.is_some_and(|t| t <= LEARNED_COMPENSATION_MIN_PARK);
    let compensated = ((!short_timed && ctx.vm_pinned_park_compensates())
        || (ctx.blocks_jdk_carrier()))
        && ctx.begin_carrier_compensation();
    unsafe_park_door(ctx, timeout, is_absolute, time);
    if compensated {
        ctx.end_carrier_compensation(true);
    }
    Ok(None)
}

/// The longest timed park `CRATONVM_VTHREAD_COMPILED_LEARNED`'s pinned-park
/// compensation leaves uncompensated (interpreter round i5 wave 108, lane
/// L6; [`native_unsafe_park`]): it ends by itself within this, so it cannot
/// starve the carrier's queue for long.
pub const LEARNED_COMPENSATION_MIN_PARK: std::time::Duration = std::time::Duration::from_millis(20);

/// The wait of `Unsafe.park(isAbsolute, time)` as HotSpot 25's
/// `Parker::park` reads its arguments (gci i21/qh, page
/// `gci-i21qh-g1-virtual-thread-quiet-hang-park-unpark-audit-FIXED-20261017`):
///
/// * `time < 0`, or an ABSOLUTE `time == 0`: return at once (`Some(ZERO)`;
///   the permit is still taken first by the door, as `Parker::park` takes
///   `_counter` before this test). Before i21 an absolute `0` (`parkUntil(0)`)
///   parked FOREVER here -- the relative "0 = untimed" rule was applied to
///   both forms -- and only an `unpark` could end it;
/// * a relative `0`: untimed (`None`);
/// * a relative `time > 0`: `time` nanoseconds;
/// * an absolute `time > 0`: until that epoch millisecond, `Some(ZERO)` once
///   it has passed. `time - now_ms` saturates: before i21 an absolute
///   `Long.MIN_VALUE` overflowed (a debug panic; in release a wrap to a
///   ~292-million-year park).
pub fn unsafe_park_timeout(
    is_absolute: bool,
    time: i64,
    now_ms: i64,
) -> Option<std::time::Duration> {
    if time < 0 || (is_absolute && time == 0) {
        return Some(std::time::Duration::ZERO);
    }
    if time == 0 {
        return None;
    }
    if is_absolute {
        let remaining = time.saturating_sub(now_ms).max(0) as u64;
        return Some(std::time::Duration::from_millis(remaining));
    }
    Some(std::time::Duration::from_nanos(time as u64))
}

/// gci i22/rv: does [`unsafe_park_timeout`]'s answer return at once (an
/// elapsed or non-positive deadline)?
#[inline]
pub fn unsafe_park_returns_at_once(timeout: Option<std::time::Duration>) -> bool {
    timeout == Some(std::time::Duration::ZERO)
}

/// The `jdk.ThreadPark` event's `until` field for an ABSOLUTE
/// `Unsafe.park(true, time)`, as HotSpot 25's `Unsafe_Park` posts it: the
/// deadline, except `time == 0`, which it posts as an untimed park
/// (`timeout = until = Long.MIN_VALUE`, its `if (time == 0)` arm comes before
/// the absolute one). gci i22/rv (finding 4 of
/// `gci-i22rv-i21-default-path-review-findings-FIXED-20261016`): i21/qh's
/// absolute door posted `until = 0` for `parkUntil(0)`.
#[inline]
pub fn unsafe_park_absolute_until_field(time: i64) -> i64 {
    if time == 0 {
        i64::MIN
    } else {
        time
    }
}

/// `Unsafe.park`'s Java-level park door (interpreter round i3 wave 79, lane
/// L6): an absolute park carries the caller's deadline for the
/// `jdk.ThreadPark` event's `until` (HotSpot: `timeout = Long.MIN_VALUE`,
/// `until = time`); every other park the relative door. gci i21/qh: every
/// absolute park takes the absolute door, an absolute `0` included (it
/// returns at once, `unsafe_park_timeout`), as `Unsafe_Park` posts it --
/// gci i22/rv: with `until = Long.MIN_VALUE` for it
/// ([`unsafe_park_absolute_until_field`]).
pub fn unsafe_park_door(
    ctx: &mut dyn NativeContext,
    timeout: Option<std::time::Duration>,
    is_absolute: bool,
    time: i64,
) {
    if is_absolute {
        ctx.park_until_reporting(timeout, unsafe_park_absolute_until_field(time));
    } else if let Some(raw) = unsafe_park_relative_raw_timeout_field(time) {
        ctx.park_relative_reporting_raw(timeout, raw);
    } else {
        ctx.park_reporting_timeout(timeout);
    }
}

/// gci i23/pk (finding 6 of
/// `gci-i22rv-i21-default-path-review-findings-FIXED-20261016`): the
/// `jdk.ThreadPark` `timeout` a RELATIVE `Unsafe.park(false, time)` needs
/// carried raw: a negative `time`, which HotSpot 25's `Unsafe_Park` posts as
/// is (`timeout = time`) and which [`unsafe_park_timeout`]'s `Some(ZERO)`
/// cannot carry. `None` for `time >= 0`: `0` posts as untimed and a positive
/// time as its nanoseconds, both already right through the `Duration`.
#[inline]
pub fn unsafe_park_relative_raw_timeout_field(time: i64) -> Option<i64> {
    (time < 0).then_some(time)
}

pub fn native_unsafe_unpark(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // args: [unsafe, thread_obj]
    if let Some(Value::Object(Some(thread_obj))) = args.get(1) {
        ctx.unpark(*thread_obj);
    } else {
    }
    Ok(None)
}

/// `Unsafe.getAndAddInt`; an int-width add over another element type is a
/// raw-bytes read-modify-write ([`unsafe_wide_raw_rmw`]).
pub fn native_unsafe_get_and_add_int(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    if let Some((obj, off)) =
        unsafe_wide_raw_target(ctx, args, cratonvm_types::ArrayElementType::Int, 4)
    {
        let d = unsafe_wide_arg_bits(args, 3, 4);
        if let Some(old) = unsafe_wide_raw_rmw(ctx, obj, off, 4, &|cur| cur.wrapping_add(d)) {
            return Ok(Some(Value::Int(old as u32 as i32)));
        }
    }
    native_unsafe_get_and_add_int_element(ctx, args)
}

/// [`native_unsafe_get_and_add_int`] without the raw-bytes arm (the
/// byte/short adds' fallback).
pub fn native_unsafe_get_and_add_int_element(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // args: [unsafe, obj, offset(long), delta(int)]
    // Wave 8c: a lane of an instance field under byte field offsets.
    if let Some(v) = unsafe_field_lane_atomic(ctx, args, ArenaAtomic::Add, 4) {
        return Ok(Some(v));
    }
    if let Some(v) = unsafe_arena_atomic(&*ctx, args, ArenaAtomic::Add, 4) {
        return Ok(Some(v));
    }
    let offset = unsafe_offset(args, 2);
    let delta = match args.get(3) {
        Some(Value::Int(d)) => *d,
        _ => 0,
    };
    let obj = match unsafe_obj(args, 1) {
        Some(o) => o,
        None => {
            if let Some(old) = unsafe_static_get_and_add_int(ctx, offset, delta) {
                return Ok(Some(Value::Int(old)));
            }
            if note_unsafe_side_store_offset(ctx, offset, line!(), args.len()) {
                // §4.5's specified refusal, discharged by §21/§23's count.
                // HotSpot SIGSEGVs here; the side store used to answer, and a
                // CAS reported success having written nowhere.
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!(
                        "Unsafe access with a null base and an offset that is \
                         neither an arena handle, a synthetic offset, nor a \
                         registered static field: {offset:#x}"
                    ),
                }
                .into());
            }
            let mut map = lock_static_int_shard(offset);
            let slot = map.entry((ctx.vm_identity(), offset)).or_insert(0);
            let old = *slot;
            *slot = old.wrapping_add(delta);
            return Ok(Some(Value::Int(old)));
        }
    };
    if unsafe_synthetic_field_offset(&*ctx, obj, offset) {
        for attempt in 0.. {
            let current = synthetic_get(ctx, obj, offset);
            if let Value::Int(old) = current {
                let new_val = Value::Int(old.wrapping_add(delta));
                if synthetic_cas(ctx, obj, offset, current, new_val) {
                    return Ok(Some(Value::Int(old)));
                }
            } else {
                return Ok(Some(Value::Int(0)));
            }
            if attempt > 0 && attempt % CAS_MAX_RETRIES == 0 {
                std::thread::yield_now();
            }
        }
        unreachable!();
    }
    if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
        // OOB offset => return 0 without touching the heap.
        let idx = match unsafe_checked_array_index(ctx, obj, offset) {
            Some(i) => i,
            None => return Ok(Some(Value::Int(0))),
        };
        // ATOMIC -- see the note in `native_unsafe_get_and_set_object`. A
        // read-modify-write on an array element loses concurrent increments;
        // the field arm below has always been a CAS loop.
        for attempt in 0.. {
            let current = ctx.get_array_element(obj, idx);
            let Value::Int(old) = current else {
                return Ok(Some(Value::Int(0)));
            };
            if ctx.compare_and_swap_field(obj, idx, current, Value::Int(old.wrapping_add(delta))) {
                return Ok(Some(Value::Int(old)));
            }
            if attempt > 0 && attempt % CAS_MAX_RETRIES == 0 {
                std::thread::yield_now();
            }
        }
        unreachable!();
    }
    // Wave 8c: the slot (an offset inside a field took the lane above).
    let (offset, _) = unsafe_field_slot(&*ctx, obj, offset);
    // CAS loop with bounded retries and yield on contention
    for attempt in 0.. {
        let current = ctx.get_field_volatile(obj, offset);
        if let Value::Int(old) = current {
            let new_val = Value::Int(old.wrapping_add(delta));
            if ctx.compare_and_swap_field(obj, offset, current, new_val) {
                return Ok(Some(Value::Int(old)));
            }
        } else {
            return Ok(Some(Value::Int(0)));
        }
        if attempt > 0 && attempt % CAS_MAX_RETRIES == 0 {
            std::thread::yield_now();
        }
    }
    unreachable!()
}

pub fn native_unsafe_get_and_set_int(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // args: [unsafe, obj, offset(long), new_val(int)]
    // Wave 8c: a lane of an instance field under byte field offsets.
    if let Some(v) = unsafe_field_lane_atomic(ctx, args, ArenaAtomic::Set, 4) {
        return Ok(Some(v));
    }
    if let Some(v) = unsafe_arena_atomic(&*ctx, args, ArenaAtomic::Set, 4) {
        return Ok(Some(v));
    }
    if let Some((obj, off)) =
        unsafe_wide_raw_target(ctx, args, cratonvm_types::ArrayElementType::Int, 4)
    {
        let nv = unsafe_wide_arg_bits(args, 3, 4);
        if let Some(old) = unsafe_wide_raw_rmw(ctx, obj, off, 4, &|_| nv) {
            return Ok(Some(Value::Int(old as u32 as i32)));
        }
    }
    let offset = unsafe_offset(args, 2);
    let new_val = args.get(3).copied().unwrap_or(Value::Int(0));
    let obj = match unsafe_obj(args, 1) {
        Some(o) => o,
        None => {
            let nv = if let Value::Int(n) = new_val { n } else { 0 };
            if let Some(prev) = unsafe_static_get_and_set_int(ctx, offset, nv) {
                return Ok(Some(Value::Int(prev)));
            }
            if note_unsafe_side_store_offset(ctx, offset, line!(), args.len()) {
                // §4.5's specified refusal, discharged by §21/§23's count.
                // HotSpot SIGSEGVs here; the side store used to answer, and a
                // CAS reported success having written nowhere.
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!(
                        "Unsafe access with a null base and an offset that is \
                         neither an arena handle, a synthetic offset, nor a \
                         registered static field: {offset:#x}"
                    ),
                }
                .into());
            }
            let mut map = lock_static_int_shard(offset);
            let prev = map.insert((ctx.vm_identity(), offset), nv).unwrap_or(0);
            return Ok(Some(Value::Int(prev)));
        }
    };
    if unsafe_synthetic_field_offset(&*ctx, obj, offset) {
        let prev = synthetic_get(ctx, obj, offset);
        synthetic_put(ctx, obj, offset, new_val);
        return Ok(Some(prev));
    }
    if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
        // OOB offset => return Int(0) without touching the heap.
        let idx = match unsafe_checked_array_index(ctx, obj, offset) {
            Some(i) => i,
            None => return Ok(Some(Value::Int(0))),
        };
        // ATOMIC, and it was not until 2026-09-10. A plain read-then-write
        // here is a lost update: two threads reading the same slot both see a
        // non-null `prev` and both report having taken it. The FIELD arm below
        // has always been a CAS retry loop; this arm was a bare
        // `get_array_element` + `set_array_element` pair, so the atomicity of
        // `getAndSet*` depended on whether the receiver happened to be an
        // array.
        //
        // The cost is not theoretical and not confined to `Unsafe`'s own
        // callers. `java.util.concurrent.ForkJoinPool$WorkQueue` claims a
        // queued task with exactly this call --
        // `U.getAndSetReference(a, slotOffset(k), null)` -- so a doubled claim
        // is a task EXECUTED TWICE. Measured with `L5CasRace.java`
        // on the Azure host: 2000 slots, 4 threads, HotSpot 2000 claims and
        // this VM **2035**; and with the real `ForkJoinPool` bytecode running
        // (`CRATONVM_ENFORCE_NATIVE_SHADOW=java/util/concurrent/ForkJoinPool,
        // java/util/concurrent/ForkJoinTask`)
        // `L5CountedCompleter.java` read 64..128 leaves for a
        // 64-leaf tree across eight runs. That is lane 5's
        // `RJdkForkJoin  AssertionError: CountedCompleter leaves: 128`.
        for attempt in 0.. {
            let prev = ctx.get_array_element(obj, idx);
            if ctx.compare_and_swap_field(obj, idx, prev, new_val) {
                return Ok(Some(prev));
            }
            if attempt > 0 && attempt % CAS_MAX_RETRIES == 0 {
                std::thread::yield_now();
            }
        }
        unreachable!();
    }
    // Wave 8c: the slot (an offset inside a field took the lane above).
    let (offset, _) = unsafe_field_slot(&*ctx, obj, offset);
    for attempt in 0.. {
        let current = ctx.get_field_volatile(obj, offset);
        if ctx.compare_and_swap_field(obj, offset, current, new_val) {
            return Ok(Some(current));
        }
        if attempt > 0 && attempt % CAS_MAX_RETRIES == 0 {
            std::thread::yield_now();
        }
    }
    unreachable!()
}

/// `Unsafe.getFloat(Object, long)` with the NULL-BASE arm routed off-heap.
///
/// A null base means an absolute address, and `getFloat(long)` is literally
/// `getFloat(null, address)` in the JDK -- so a write through one spelling has
/// to be visible through the other. It was not: the null-base arm of
/// `native_unsafe_get_float` reads a private static-field side map keyed by the
/// address, which round-trips perfectly WITHIN that door and shares no storage
/// with the arena the 1-arg form uses. MEASURED, HotSpot 25.0.4+7:
///
///   putFloat(null, addr, 1.5f) then getFloat(addr)   HotSpot 1.5   CratonVM 0.0
///   putFloat(addr, 2.5f) then getFloat(null, addr)   HotSpot 2.5   CratonVM 0.0
///
/// int, long, byte, short and char already have `_mb` handlers that route a
/// null base through `copy_from_native_memory`; float and double were the two
/// widths that never got one. That is the same population as
/// `a-null-base-unsafe-access-means-off-heap-not-a-static-field` (2026-08-24) --
/// which fixed the ONE-ARG forms and left the two-arg null-base forms behind.
pub fn native_unsafe_get_float_mb(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // A FIELD of another kind (i4 wave 87, lane L3).
    if let Some(v) = unsafe_field_view_get(&*ctx, args, b'F', false) {
        return Ok(Some(v));
    }
    if unsafe_obj(args, 1).is_none() {
        let addr = unsafe_raw_addr(args, 2);
        let mut b = [0u8; 4];
        if ctx.copy_from_native_memory(addr, &mut b) {
            return Ok(Some(Value::Float(f32::from_le_bytes(b))));
        }
    }
    // SBR3 ffmperf: the raw bits of any other primitive array (`getFloat(int[]
    // .., off)` is the bit pattern, not an element converted).
    if let Some(obj) = unsafe_obj(args, 1) {
        if unsafe_array_access_is_raw(
            ctx,
            obj,
            cratonvm_types::ArrayElementType::Float,
            unsafe_offset(args, 2),
            4,
        ) {
            let mut b = [0u8; UNSAFE_MAX_ACCESS_WIDTH];
            if unsafe_read_bytes_into(ctx, obj, unsafe_offset(args, 2), 4, &mut b) {
                return Ok(Some(Value::Float(f32::from_le_bytes([
                    b[0], b[1], b[2], b[3],
                ]))));
            }
        }
    }
    native_unsafe_get_float(ctx, args)
}

/// SBR3 lane ffmperf: is `obj` a primitive array whose element type is not
/// `natural`, or whose `width`-byte access at `offset` straddles elements? Then
/// an `Unsafe.get*` / `put*` of that width is a read/write of raw bytes.
pub fn unsafe_array_access_is_raw(
    ctx: &dyn NativeContext,
    obj: cratonvm_types::ObjectRef,
    natural: cratonvm_types::ArrayElementType,
    offset: usize,
    width: usize,
) -> bool {
    if ctx.heap_kind_of(obj) != cratonvm_types::ObjectKind::Array {
        return false;
    }
    let et = ctx.heap_element_type_of(obj);
    if et == cratonvm_types::ArrayElementType::Reference {
        return false;
    }
    et != natural || (offset >= 16 && (offset - 16) % width != 0)
}

/// Write half of [`native_unsafe_get_float_mb`].
pub fn native_unsafe_put_float_mb(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // A FIELD of another kind (i4 wave 87, lane L3).
    if unsafe_field_view_put(ctx, args, b'F') {
        return Ok(None);
    }
    if unsafe_obj(args, 1).is_none() {
        let addr = unsafe_raw_addr(args, 2);
        let bits = match args.get(3) {
            Some(Value::Float(f)) => f.to_bits(),
            Some(Value::Int(i)) => *i as u32,
            _ => 0,
        };
        if ctx.copy_to_native_memory(addr, &bits.to_le_bytes()) {
            return Ok(None);
        }
    }
    // SBR3 ffmperf: the raw store into any other primitive array.
    if let Some(obj) = unsafe_obj(args, 1) {
        if unsafe_array_access_is_raw(
            ctx,
            obj,
            cratonvm_types::ArrayElementType::Float,
            unsafe_offset(args, 2),
            4,
        ) {
            let bits = match args.get(3) {
                Some(Value::Float(f)) => f.to_bits(),
                Some(Value::Int(i)) => *i as u32,
                _ => 0,
            };
            if unsafe_array_write_bytes(ctx, obj, unsafe_offset(args, 2), &bits.to_le_bytes()) {
                return Ok(None);
            }
        }
    }
    native_unsafe_put_float(ctx, args)
}

/// `Unsafe.getDouble(Object, long)` with the null-base arm routed off-heap.
/// See [`native_unsafe_get_float_mb`].
pub fn native_unsafe_get_double_mb(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // A FIELD of another kind (i4 wave 87, lane L3).
    if let Some(v) = unsafe_field_view_get(&*ctx, args, b'D', false) {
        return Ok(Some(v));
    }
    if unsafe_obj(args, 1).is_none() {
        let addr = unsafe_raw_addr(args, 2);
        let mut b = [0u8; 8];
        if ctx.copy_from_native_memory(addr, &mut b) {
            return Ok(Some(Value::Double(f64::from_le_bytes(b))));
        }
    }
    // SBR3 ffmperf: see `native_unsafe_get_float_mb`.
    if let Some(obj) = unsafe_obj(args, 1) {
        if unsafe_array_access_is_raw(
            ctx,
            obj,
            cratonvm_types::ArrayElementType::Double,
            unsafe_offset(args, 2),
            8,
        ) {
            let mut b = [0u8; UNSAFE_MAX_ACCESS_WIDTH];
            if unsafe_read_bytes_into(ctx, obj, unsafe_offset(args, 2), 8, &mut b) {
                return Ok(Some(Value::Double(f64::from_le_bytes(b))));
            }
        }
    }
    native_unsafe_get_double(ctx, args)
}

/// Write half of [`native_unsafe_get_double_mb`].
pub fn native_unsafe_put_double_mb(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    // A FIELD of another kind (i4 wave 87, lane L3).
    if unsafe_field_view_put(ctx, args, b'D') {
        return Ok(None);
    }
    if unsafe_obj(args, 1).is_none() {
        let addr = unsafe_raw_addr(args, 2);
        let bits = match args.get(3) {
            Some(Value::Double(d)) => d.to_bits(),
            Some(Value::Long(l)) => *l as u64,
            _ => 0,
        };
        if ctx.copy_to_native_memory(addr, &bits.to_le_bytes()) {
            return Ok(None);
        }
    }
    // SBR3 ffmperf: see `native_unsafe_put_float_mb`.
    if let Some(obj) = unsafe_obj(args, 1) {
        if unsafe_array_access_is_raw(
            ctx,
            obj,
            cratonvm_types::ArrayElementType::Double,
            unsafe_offset(args, 2),
            8,
        ) {
            let bits = match args.get(3) {
                Some(Value::Double(d)) => d.to_bits(),
                Some(Value::Long(l)) => *l as u64,
                _ => 0,
            };
            if unsafe_array_write_bytes(ctx, obj, unsafe_offset(args, 2), &bits.to_le_bytes()) {
                return Ok(None);
            }
        }
    }
    native_unsafe_put_double(ctx, args)
}

pub fn native_unsafe_get_float(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let offset = unsafe_offset(args, 2);
    let obj = match unsafe_obj(args, 1) {
        Some(o) => o,
        None => {
            return Ok(Some(match unsafe_static_get(ctx, offset) {
                Some(Value::Float(f)) => Value::Float(f),
                Some(Value::Int(bits)) => Value::Float(f32::from_bits(bits as u32)),
                _ => Value::Float(0.0),
            }));
        }
    };
    if unsafe_synthetic_field_offset(&*ctx, obj, offset) {
        let val = synthetic_get(ctx, obj, offset);
        return Ok(Some(match val {
            Value::Float(_) => val,
            Value::Int(bits) => Value::Float(f32::from_bits(bits as u32)),
            _ => Value::Float(0.0),
        }));
    }
    // An ARRAY base is an ELEMENT access, not a field access — the same screen
    // `native_unsafe_put_int` / `native_unsafe_get_long` have always carried,
    // and the four float/double natives were the only ones without it.
    //
    // `Unsafe.{get,put}{Float,Double}(Object, long, …)` over an array reached
    // `ctx.{get,set}_field(obj, offset)` with the BYTE OFFSET as a slot index.
    // An array mirrors its LENGTH into `num_slots`, so the index check admits
    // it, and the accessor then addresses a 16-byte `Value` cell at
    // `HEADER_SIZE + offset * 16` — for Hazelcast's
    // `UnsafeUtil.checkUnsafeInstance`, a `putFloat(new byte[32], 16, 3f)` that
    // computed byte 272 of a 32-byte body, i.e. 240 bytes past the allocation.
    // Found 2026-08-23 by the array-receiver screen at the heap accessors
    // (`corrupt-value-cell-array-receiver-species-CLOSED-20260823`), which is
    // what turned a silent out-of-bounds write into a named producer.
    let val = if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
        // The read half of `native_unsafe_put_float`'s byte-addressed write.
        // Restricted to `byte[]`/`boolean[]` on purpose: for a typed array the
        // element accessor is the authority on how that element is stored, and
        // re-assembling it from bytes would be a second, unvalidated decoding.
        if matches!(
            ctx.heap_element_type_of(obj),
            cratonvm_types::ArrayElementType::Byte | cratonvm_types::ArrayElementType::Boolean
        ) {
            match unsafe_read_bytes_from_array(ctx, obj, offset, 4) {
                Some(b) => Value::Float(f32::from_le_bytes([b[0], b[1], b[2], b[3]])),
                None => Value::Float(0.0),
            }
        } else {
            match unsafe_checked_array_index(ctx, obj, offset) {
                Some(idx) => ctx.get_array_element(obj, idx),
                None => Value::Float(0.0),
            }
        }
    } else {
        // Wave 8c: an offset INSIDE a field reads its bytes as the float.
        match unsafe_field_slot(&*ctx, obj, offset) {
            // Cast: the low 32 bits of the zero-extended bytes.
            (slot, k) if k != 0 => {
                Value::Int(unsafe_sub_field_load(&*ctx, obj, slot, k, 4) as u32 as i32)
            }
            (slot, _) => ctx.get_field(obj, slot),
        }
    };
    match val {
        Value::Float(_) => Ok(Some(val)),
        Value::Int(bits) => Ok(Some(Value::Float(f32::from_bits(bits as u32)))),
        _ => Ok(Some(Value::Float(0.0))),
    }
}

pub fn native_unsafe_put_float(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let offset = unsafe_offset(args, 2);
    let val = args.get(3).copied().unwrap_or(Value::Float(0.0));
    let obj = match unsafe_obj(args, 1) {
        Some(o) => o,
        None => {
            let _ = unsafe_static_put(ctx, offset, val);
            return Ok(None);
        }
    };
    if unsafe_synthetic_field_offset(&*ctx, obj, offset) {
        synthetic_put(ctx, obj, offset, val);
        return Ok(None);
    }
    // An ARRAY base is an ELEMENT access, not a field access — the same screen
    // `native_unsafe_put_int` / `native_unsafe_get_long` have always carried,
    // and the four float/double natives were the only ones without it.
    //
    // `Unsafe.{get,put}{Float,Double}(Object, long, …)` over an array reached
    // `ctx.{get,set}_field(obj, offset)` with the BYTE OFFSET as a slot index.
    // An array mirrors its LENGTH into `num_slots`, so the index check admits
    // it, and the accessor then addresses a 16-byte `Value` cell at
    // `HEADER_SIZE + offset * 16` — for Hazelcast's
    // `UnsafeUtil.checkUnsafeInstance`, a `putFloat(new byte[32], 16, 3f)` that
    // computed byte 272 of a 32-byte body, i.e. 240 bytes past the allocation.
    // Found 2026-08-23 by the array-receiver screen at the heap accessors
    // (`corrupt-value-cell-array-receiver-species-CLOSED-20260823`), which is
    // what turned a silent out-of-bounds write into a named producer.
    if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
        // A `byte[]` base is a BYTE-ADDRESSED write of the full 4-byte value,
        // not a one-element store. `unsafe_multibyte_put!` says the same thing
        // for short/char/int/long; float and double were not wired to it, and
        // routing them to `set_array_element` alone would truncate a float to
        // one byte -- no longer an out-of-bounds write, but still not HotSpot's
        // answer. This is the idiom `Bits.writeIntL([BII)` (one frame below
        // Hazelcast's probe in the stack that found this) depends on.
        let bits = match val {
            Value::Float(f) => f.to_bits(),
            Value::Int(i) => i as u32,
            _ => 0,
        };
        if unsafe_write_bytes_to_byte_array(ctx, obj, offset, &bits.to_le_bytes()) {
            return Ok(None);
        }
        // Any other primitive array: the offset names one typed element.
        if let Some(idx) = unsafe_checked_array_index(ctx, obj, offset) {
            ctx.set_array_element(obj, idx, val);
        }
        // An out-of-range offset is a no-op, matching the int/long siblings:
        // never reaches the host accessor.
    } else {
        // Wave 8c: an offset INSIDE a field replaces four of its bytes.
        let (offset, k) = unsafe_field_slot(&*ctx, obj, offset);
        if k != 0 {
            let bits = unsafe_value_bits(val).unwrap_or(0);
            unsafe_sub_field_store(ctx, obj, offset, k, 4, bits);
            return Ok(None);
        }
        ctx.set_field(obj, offset, val);
    }
    Ok(None)
}

pub fn native_unsafe_get_double(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let offset = unsafe_offset(args, 2);
    let obj = match unsafe_obj(args, 1) {
        Some(o) => o,
        None => {
            return Ok(Some(match unsafe_static_get(ctx, offset) {
                Some(Value::Double(d)) => Value::Double(d),
                Some(Value::Long(bits)) => Value::Double(f64::from_bits(bits as u64)),
                _ => Value::Double(0.0),
            }));
        }
    };
    if unsafe_synthetic_field_offset(&*ctx, obj, offset) {
        let val = synthetic_get(ctx, obj, offset);
        return Ok(Some(match val {
            Value::Double(_) => val,
            Value::Long(bits) => Value::Double(f64::from_bits(bits as u64)),
            _ => Value::Double(0.0),
        }));
    }
    // An ARRAY base is an ELEMENT access, not a field access — the same screen
    // `native_unsafe_put_int` / `native_unsafe_get_long` have always carried,
    // and the four float/double natives were the only ones without it.
    //
    // `Unsafe.{get,put}{Float,Double}(Object, long, …)` over an array reached
    // `ctx.{get,set}_field(obj, offset)` with the BYTE OFFSET as a slot index.
    // An array mirrors its LENGTH into `num_slots`, so the index check admits
    // it, and the accessor then addresses a 16-byte `Value` cell at
    // `HEADER_SIZE + offset * 16` — for Hazelcast's
    // `UnsafeUtil.checkUnsafeInstance`, a `putFloat(new byte[32], 16, 3f)` that
    // computed byte 272 of a 32-byte body, i.e. 240 bytes past the allocation.
    // Found 2026-08-23 by the array-receiver screen at the heap accessors
    // (`corrupt-value-cell-array-receiver-species-CLOSED-20260823`), which is
    // what turned a silent out-of-bounds write into a named producer.
    let val = if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
        // Byte-addressed over a `byte[]` only — see `native_unsafe_get_float`.
        if matches!(
            ctx.heap_element_type_of(obj),
            cratonvm_types::ArrayElementType::Byte | cratonvm_types::ArrayElementType::Boolean
        ) {
            match unsafe_read_bytes_from_array(ctx, obj, offset, 8) {
                Some(b) => Value::Double(f64::from_le_bytes([
                    b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
                ])),
                None => Value::Double(0.0),
            }
        } else {
            match unsafe_checked_array_index(ctx, obj, offset) {
                Some(idx) => ctx.get_array_element(obj, idx),
                None => Value::Double(0.0),
            }
        }
    } else {
        // Wave 8c: an offset INSIDE a field reads its bytes as the double.
        match unsafe_field_slot(&*ctx, obj, offset) {
            // Cast: the zero-extended bytes.
            (slot, k) if k != 0 => {
                Value::Long(unsafe_sub_field_load(&*ctx, obj, slot, k, 8) as i64)
            }
            (slot, _) => ctx.get_field(obj, slot),
        }
    };
    match val {
        Value::Double(_) => Ok(Some(val)),
        Value::Long(bits) => Ok(Some(Value::Double(f64::from_bits(bits as u64)))),
        _ => Ok(Some(Value::Double(0.0))),
    }
}

pub fn native_unsafe_put_double(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let offset = unsafe_offset(args, 2);
    let val = args.get(3).copied().unwrap_or(Value::Double(0.0));
    let obj = match unsafe_obj(args, 1) {
        Some(o) => o,
        None => {
            let _ = unsafe_static_put(ctx, offset, val);
            return Ok(None);
        }
    };
    if unsafe_synthetic_field_offset(&*ctx, obj, offset) {
        synthetic_put(ctx, obj, offset, val);
        return Ok(None);
    }
    // An ARRAY base is an ELEMENT access, not a field access — the same screen
    // `native_unsafe_put_int` / `native_unsafe_get_long` have always carried,
    // and the four float/double natives were the only ones without it.
    //
    // `Unsafe.{get,put}{Float,Double}(Object, long, …)` over an array reached
    // `ctx.{get,set}_field(obj, offset)` with the BYTE OFFSET as a slot index.
    // An array mirrors its LENGTH into `num_slots`, so the index check admits
    // it, and the accessor then addresses a 16-byte `Value` cell at
    // `HEADER_SIZE + offset * 16` — for Hazelcast's
    // `UnsafeUtil.checkUnsafeInstance`, a `putFloat(new byte[32], 16, 3f)` that
    // computed byte 272 of a 32-byte body, i.e. 240 bytes past the allocation.
    // Found 2026-08-23 by the array-receiver screen at the heap accessors
    // (`corrupt-value-cell-array-receiver-species-CLOSED-20260823`), which is
    // what turned a silent out-of-bounds write into a named producer.
    if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
        // Byte-addressed over a `byte[]`, one typed element otherwise — see
        // `native_unsafe_put_float`.
        let bits = match val {
            Value::Double(d) => d.to_bits(),
            Value::Long(l) => l as u64,
            _ => 0,
        };
        if unsafe_write_bytes_to_byte_array(ctx, obj, offset, &bits.to_le_bytes()) {
            return Ok(None);
        }
        if let Some(idx) = unsafe_checked_array_index(ctx, obj, offset) {
            ctx.set_array_element(obj, idx, val);
        }
    } else {
        // Wave 8c: an offset INSIDE a field replaces its remaining bytes.
        let (offset, k) = unsafe_field_slot(&*ctx, obj, offset);
        if k != 0 {
            let bits = unsafe_value_bits(val).unwrap_or(0);
            unsafe_sub_field_store(ctx, obj, offset, k, 8, bits);
            return Ok(None);
        }
        ctx.set_field(obj, offset, val);
    }
    Ok(None)
}

pub fn native_unsafe_get_and_add_long(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Wave 8c: a lane of an instance field under byte field offsets.
    if let Some(v) = unsafe_field_lane_atomic(ctx, args, ArenaAtomic::Add, 8) {
        return Ok(Some(v));
    }
    if let Some(v) = unsafe_arena_atomic(&*ctx, args, ArenaAtomic::Add, 8) {
        return Ok(Some(v));
    }
    if let Some((obj, off)) =
        unsafe_wide_raw_target(ctx, args, cratonvm_types::ArrayElementType::Long, 8)
    {
        let d = unsafe_wide_arg_bits(args, 3, 8);
        if let Some(old) = unsafe_wide_raw_rmw(ctx, obj, off, 8, &|cur| cur.wrapping_add(d)) {
            return Ok(Some(Value::Long(old as i64)));
        }
    }
    let delta = match args.get(3) {
        Some(Value::Long(d)) => *d,
        _ => 0,
    };
    let offset = unsafe_offset(args, 2);
    let obj = match unsafe_obj(args, 1) {
        Some(o) => o,
        None => {
            if let Some(old) = unsafe_static_get_and_add_long(ctx, offset, delta) {
                return Ok(Some(Value::Long(old)));
            }
            // Static-field semantics (null receiver). Maintain a per-offset
            // counter so callers like Thread$ThreadIdentifiers.next() get
            // monotonically-increasing values rather than a VM panic.
            if note_unsafe_side_store_offset(ctx, offset, line!(), args.len()) {
                // §4.5's specified refusal, discharged by §21/§23's count.
                // HotSpot SIGSEGVs here; the side store used to answer, and a
                // CAS reported success having written nowhere.
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!(
                        "Unsafe access with a null base and an offset that is \
                         neither an arena handle, a synthetic offset, nor a \
                         registered static field: {offset:#x}"
                    ),
                }
                .into());
            }
            let mut map = lock_static_long_shard(offset);
            let slot = map.entry((ctx.vm_identity(), offset)).or_insert(0);
            let old = *slot;
            *slot = old.wrapping_add(delta);
            return Ok(Some(Value::Long(old)));
        }
    };
    if unsafe_synthetic_field_offset(&*ctx, obj, offset) {
        for attempt in 0.. {
            let current = synthetic_get(ctx, obj, offset);
            if let Value::Long(old) = current {
                let new_val = Value::Long(old.wrapping_add(delta));
                if synthetic_cas(ctx, obj, offset, current, new_val) {
                    return Ok(Some(Value::Long(old)));
                }
            } else {
                return Ok(Some(Value::Long(0)));
            }
            if attempt > 0 && attempt % CAS_MAX_RETRIES == 0 {
                std::thread::yield_now();
            }
        }
        unreachable!();
    }
    if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
        // OOB offset => return 0 without touching the heap.
        let idx = match unsafe_checked_array_index(ctx, obj, offset) {
            Some(i) => i,
            None => return Ok(Some(Value::Long(0))),
        };
        // ATOMIC -- see the note in `native_unsafe_get_and_set_object`.
        for attempt in 0.. {
            let current = ctx.get_array_element(obj, idx);
            let Value::Long(old) = current else {
                return Ok(Some(Value::Long(0)));
            };
            if ctx.compare_and_swap_field(obj, idx, current, Value::Long(old.wrapping_add(delta))) {
                return Ok(Some(Value::Long(old)));
            }
            if attempt > 0 && attempt % CAS_MAX_RETRIES == 0 {
                std::thread::yield_now();
            }
        }
        unreachable!();
    }
    // Wave 8c: the slot (an offset inside a field took the lane above).
    let (offset, _) = unsafe_field_slot(&*ctx, obj, offset);
    for attempt in 0.. {
        let current = ctx.get_field_volatile(obj, offset);
        if let Value::Long(old) = current {
            let new_val = Value::Long(old.wrapping_add(delta));
            if ctx.compare_and_swap_field(obj, offset, current, new_val) {
                return Ok(Some(Value::Long(old)));
            }
        } else {
            return Ok(Some(Value::Long(0)));
        }
        if attempt > 0 && attempt % CAS_MAX_RETRIES == 0 {
            std::thread::yield_now();
        }
    }
    unreachable!()
}

pub fn native_unsafe_get_and_set_long(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Wave 8c: a lane of an instance field under byte field offsets.
    if let Some(v) = unsafe_field_lane_atomic(ctx, args, ArenaAtomic::Set, 8) {
        return Ok(Some(v));
    }
    if let Some(v) = unsafe_arena_atomic(&*ctx, args, ArenaAtomic::Set, 8) {
        return Ok(Some(v));
    }
    if let Some((obj, off)) =
        unsafe_wide_raw_target(ctx, args, cratonvm_types::ArrayElementType::Long, 8)
    {
        let nv = unsafe_wide_arg_bits(args, 3, 8);
        if let Some(old) = unsafe_wide_raw_rmw(ctx, obj, off, 8, &|_| nv) {
            return Ok(Some(Value::Long(old as i64)));
        }
    }
    let offset = unsafe_offset(args, 2);
    let new_val = args.get(3).copied().unwrap_or(Value::Long(0));
    let obj = match unsafe_obj(args, 1) {
        Some(o) => o,
        None => {
            let nv = if let Value::Long(n) = new_val { n } else { 0 };
            if let Some(prev) = unsafe_static_get_and_set_long(ctx, offset, nv) {
                return Ok(Some(Value::Long(prev)));
            }
            if note_unsafe_side_store_offset(ctx, offset, line!(), args.len()) {
                // §4.5's specified refusal, discharged by §21/§23's count.
                // HotSpot SIGSEGVs here; the side store used to answer, and a
                // CAS reported success having written nowhere.
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!(
                        "Unsafe access with a null base and an offset that is \
                         neither an arena handle, a synthetic offset, nor a \
                         registered static field: {offset:#x}"
                    ),
                }
                .into());
            }
            let mut map = lock_static_long_shard(offset);
            let prev = map.insert((ctx.vm_identity(), offset), nv).unwrap_or(0);
            return Ok(Some(Value::Long(prev)));
        }
    };
    if unsafe_synthetic_field_offset(&*ctx, obj, offset) {
        let prev = synthetic_get(ctx, obj, offset);
        synthetic_put(ctx, obj, offset, new_val);
        return Ok(Some(prev));
    }
    if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
        // OOB offset => return Long(0) without touching the heap.
        let idx = match unsafe_checked_array_index(ctx, obj, offset) {
            Some(i) => i,
            None => return Ok(Some(Value::Long(0))),
        };
        // ATOMIC, and it was not until 2026-09-10. A plain read-then-write
        // here is a lost update: two threads reading the same slot both see a
        // non-null `prev` and both report having taken it. The FIELD arm below
        // has always been a CAS retry loop; this arm was a bare
        // `get_array_element` + `set_array_element` pair, so the atomicity of
        // `getAndSet*` depended on whether the receiver happened to be an
        // array.
        //
        // The cost is not theoretical and not confined to `Unsafe`'s own
        // callers. `java.util.concurrent.ForkJoinPool$WorkQueue` claims a
        // queued task with exactly this call --
        // `U.getAndSetReference(a, slotOffset(k), null)` -- so a doubled claim
        // is a task EXECUTED TWICE. Measured with `L5CasRace.java`
        // on the Azure host: 2000 slots, 4 threads, HotSpot 2000 claims and
        // this VM **2035**; and with the real `ForkJoinPool` bytecode running
        // (`CRATONVM_ENFORCE_NATIVE_SHADOW=java/util/concurrent/ForkJoinPool,
        // java/util/concurrent/ForkJoinTask`)
        // `L5CountedCompleter.java` read 64..128 leaves for a
        // 64-leaf tree across eight runs. That is lane 5's
        // `RJdkForkJoin  AssertionError: CountedCompleter leaves: 128`.
        for attempt in 0.. {
            let prev = ctx.get_array_element(obj, idx);
            if ctx.compare_and_swap_field(obj, idx, prev, new_val) {
                return Ok(Some(prev));
            }
            if attempt > 0 && attempt % CAS_MAX_RETRIES == 0 {
                std::thread::yield_now();
            }
        }
        unreachable!();
    }
    // Wave 8c: the slot (an offset inside a field took the lane above).
    let (offset, _) = unsafe_field_slot(&*ctx, obj, offset);
    for attempt in 0.. {
        let current = ctx.get_field_volatile(obj, offset);
        if ctx.compare_and_swap_field(obj, offset, current, new_val) {
            return Ok(Some(current));
        }
        if attempt > 0 && attempt % CAS_MAX_RETRIES == 0 {
            std::thread::yield_now();
        }
    }
    unreachable!()
}

pub fn native_unsafe_get_and_set_object(
    ctx: &mut dyn NativeContext,
    args: &[Value],
) -> MethodCallResult {
    let offset = unsafe_offset(args, 2);
    let new_val = args.get(3).copied().unwrap_or(Value::Object(None));
    let obj = match unsafe_obj(args, 1) {
        Some(o) => o,
        None => {
            let nv = if let Value::Object(n) = new_val {
                n
            } else {
                None
            };
            if let Some(prev) = unsafe_static_get_and_set_object(ctx, offset, nv) {
                return Ok(Some(Value::Object(prev)));
            }
            if note_unsafe_side_store_offset(ctx, offset, line!(), args.len()) {
                // §4.5's specified refusal, discharged by §21/§23's count.
                // HotSpot SIGSEGVs here; the side store used to answer, and a
                // CAS reported success having written nowhere.
                return Err(RuntimeError::IllegalArgumentException {
                    message: format!(
                        "Unsafe access with a null base and an offset that is \
                         neither an arena handle, a synthetic offset, nor a \
                         registered static field: {offset:#x}"
                    ),
                }
                .into());
            }
            let key = (ctx.vm_identity(), offset);
            let mut map = lock_static_obj_shard(offset);
            let prev = map.insert(key, nv).unwrap_or(None);
            return Ok(Some(Value::Object(prev)));
        }
    };
    if unsafe_synthetic_field_offset(&*ctx, obj, offset) {
        let prev = synthetic_get(ctx, obj, offset);
        synthetic_put(ctx, obj, offset, new_val);
        return Ok(Some(prev));
    }
    if ctx.heap_kind_of(obj) == cratonvm_types::ObjectKind::Array {
        // OOB offset => return null without touching the heap.
        let idx = match unsafe_checked_array_index(ctx, obj, offset) {
            Some(i) => i,
            None => return Ok(Some(Value::Object(None))),
        };
        // ATOMIC, and it was not until 2026-09-10. A plain read-then-write
        // here is a lost update: two threads reading the same slot both see a
        // non-null `prev` and both report having taken it. The FIELD arm below
        // has always been a CAS retry loop; this arm was a bare
        // `get_array_element` + `set_array_element` pair, so the atomicity of
        // `getAndSet*` depended on whether the receiver happened to be an
        // array.
        //
        // The cost is not theoretical and not confined to `Unsafe`'s own
        // callers. `java.util.concurrent.ForkJoinPool$WorkQueue` claims a
        // queued task with exactly this call --
        // `U.getAndSetReference(a, slotOffset(k), null)` -- so a doubled claim
        // is a task EXECUTED TWICE. Measured with `L5CasRace.java`
        // on the Azure host: 2000 slots, 4 threads, HotSpot 2000 claims and
        // this VM **2035**; and with the real `ForkJoinPool` bytecode running
        // (`CRATONVM_ENFORCE_NATIVE_SHADOW=java/util/concurrent/ForkJoinPool,
        // java/util/concurrent/ForkJoinTask`)
        // `L5CountedCompleter.java` read 64..128 leaves for a
        // 64-leaf tree across eight runs. That is lane 5's
        // `RJdkForkJoin  AssertionError: CountedCompleter leaves: 128`.
        for attempt in 0.. {
            let prev = ctx.get_array_element(obj, idx);
            if ctx.compare_and_swap_field(obj, idx, prev, new_val) {
                return Ok(Some(prev));
            }
            if attempt > 0 && attempt % CAS_MAX_RETRIES == 0 {
                std::thread::yield_now();
            }
        }
        unreachable!();
    }
    // Wave 8c: the slot; an offset inside a field names no reference.
    let (offset, k) = unsafe_field_slot(&*ctx, obj, offset);
    if k != 0 {
        return Ok(Some(Value::Object(None)));
    }
    for attempt in 0.. {
        let current = ctx.get_field_volatile(obj, offset);
        if ctx.compare_and_swap_field(obj, offset, current, new_val) {
            return Ok(Some(current));
        }
        if attempt > 0 && attempt % CAS_MAX_RETRIES == 0 {
            std::thread::yield_now();
        }
    }
    unreachable!()
}

