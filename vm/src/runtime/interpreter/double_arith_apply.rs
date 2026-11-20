#[allow(unused_imports)]
use super::*;

/// `a <op> b` exactly as the unfused arms compute it (IEEE arithmetic,
/// `java_drem`, `dcmpl` answering -1 and `dcmpg` 1 for a NaN).
#[inline(always)]
pub(in crate::runtime::interpreter) fn double_arith_apply(op: u8, a: f64, b: f64) -> Option<DoubleArithValue> {
    Some(match op {
        0x63 => DoubleArithValue::Double(java_dadd(a, b)),
        0x67 => DoubleArithValue::Double(java_dsub(a, b)),
        0x6b => DoubleArithValue::Double(java_dmul(a, b)),
        0x6f => DoubleArithValue::Double(java_ddiv(a, b)),
        0x73 => DoubleArithValue::Double(java_drem(a, b)),
        0x97 | 0x98 => DoubleArithValue::Int(if a.is_nan() || b.is_nan() {
            if op == 0x97 {
                -1
            } else {
                1
            }
        } else if a > b {
            1
        } else if a == b {
            0
        } else {
            -1
        }),
        _ => return None,
    })
}

/// Run `dload <first_local>; <operand>; <double op> [; dstore]` as one
/// dispatch, the operand decoded at `operand_pc` (interpreter round i5 wave
/// 102, lane L7, `CRATONVM_INTERP_FUSED_DOUBLE_ARITH`, default off; the
/// `dload_0..3` arm calls it). Returns the pc after the group, or `None` --
/// having written nothing and pushed nothing -- for any shape
/// [`double_arith_group`] declines or a local index at or past
/// `max_locals`, so the plain `dload` runs. The locals are read raw (the
/// `dload` arms push `get_local_compact_unchecked`'s bits, the operation pops
/// them as `f64::from_bits`); the result is pushed with
/// `push_double_unchecked` / `push_int_unchecked` as the unfused operation
/// pushes it, or stored with `set_local_unchecked(Value::Double)` as the
/// `dstore_N` arm stores it. Out of line so the loop frame does not grow.
#[inline(never)]
pub(in crate::runtime::interpreter) fn fused_double_arith(
    frame: &mut Frame,
    code_len: usize,
    operand_pc: usize,
    first_local: usize,
) -> Option<usize> {
    let g = double_arith_group(&frame.code, code_len, operand_pc)?;
    let max_locals = usize::from(frame.max_locals);
    if first_local >= max_locals {
        return None;
    }
    let b = match g.operand {
        DoubleOperand::Local(i) if i < max_locals => {
            f64::from_bits(frame.get_local_compact_unchecked(i).to_bits())
        }
        DoubleOperand::Local(_) => return None,
        DoubleOperand::Const(k) => k,
    };
    if matches!(g.store, Some(z) if z >= max_locals) {
        return None;
    }
    let a = f64::from_bits(frame.get_local_compact_unchecked(first_local).to_bits());
    // A NaN operand: which NaN's payload an arithmetic result keeps depends
    // on the operand order the compiler emitted for the operation (LLVM may
    // commute `fadd` / `fmul`), and this out-of-line copy came out in the
    // other order than the plain `dadd` / `dmul` arms, which match HotSpot
    // (build w102: `NaN(0) + NaN(0x1234)` kept the wrong payload). Decline,
    // having written nothing, so the plain arms compute it. The compares do
    // not look at payloads.
    if (a.is_nan() || b.is_nan()) && !matches!(g.op, 0x97 | 0x98) {
        return None;
    }
    match double_arith_apply(g.op, a, b)? {
        DoubleArithValue::Int(r) => frame.stack.push_int_unchecked(r),
        DoubleArithValue::Double(r) => match g.store {
            Some(z) => frame.set_local_unchecked(z, Value::Double(r)),
            None => frame.stack.push_double_unchecked(r),
        },
    }
    Some(g.next_pc)
}


/// Decode the int-producing bytecode at `pc` as the second operand of a fused
/// compare-and-branch: `iload_0..3`, `iload n`, `iconst_m1..5`, `bipush`,
/// `sipush`, or — when `arrlen` (the dispatch loop's `arraylength_fusion_on`)
/// — the two-bytecode `aload_0..3; arraylength` / `aload n; arraylength`
/// (javac's `i < a.length`). Returns the value and the pc of the bytecode
/// after it, or `None` for any other opcode, for a local that is not
/// int-tagged (the unfused path then keeps its own answer for that slot), for
/// an `iload n` / `aload n` index outside the frame's locals (the same
/// admission as those arms), and for a reference local that is null, not an
/// array or not a plain reference (`field_fast::arraylength_of_local`) — so
/// the unfused `arraylength` raises its NPE at its own pc and nothing in a
/// fused group ever throws.
///
/// # Safety
///
/// `code_ptr` addresses `frame`'s padded bytecode and `pc` is below its
/// unpadded length, so `pc + 1` and `pc + 2` are readable.
///
/// When `ldc_bound` (the dispatch loop's `fused_ldc_bound_on`), an `ldc` /
/// `ldc_w` is admitted too, answered from `ldc_sites` (the thread's
/// `prim_const_sites`) with the probe `constants::execute_ldc` makes first —
/// the same key (`frame.class_id`, the pool index) and the same epoch
/// validation — and only on a held `Int`. A miss, a `Float`, or a reference
/// constant (never held in that table) declines, so the unfused `ldc`
/// resolves, records and fills exactly as before; nothing here resolves or
/// writes. A hit counts as the `ldc` hit it replaces (`LDC_HIT`, under
/// `census`).
///
/// When `getstatic_bound` (the dispatch loop's `fused_getstatic_bound_on`;
/// wave 64, lane L7), a `getstatic` followed by an `if_icmp<cond>` is
/// admitted too, answered from `static_sites` (the thread's
/// `static_field_sites`, the table the quickened `getstatic` probes) and only
/// for an int-typed field (`getstatic_int_site`); `read_static` reads the
/// field (`get_static_shared` in the loop). It declines while a field
/// watchpoint is armed: `field_watch` is the loop's hoisted union when the
/// observers are hoisted (`Some`), else the union is loaded here, as the
/// quickened `getstatic` loads it.
#[inline(always)]
#[allow(clippy::too_many_arguments)]
pub(in crate::runtime::interpreter) unsafe fn fused_int_operand<R: FnOnce(ClassId, usize) -> Value>(
    frame: &Frame,
    code_ptr: *const u8,
    pc: usize,
    arrlen: bool,
    census: bool,
    ldc_bound: bool,
    ldc_sites: &mut site_cache::PrimitiveConstantSiteCache,
    getstatic_bound: bool,
    field_watch: Option<bool>,
    static_sites: &mut FieldSiteCache,
    read_static: R,
) -> Option<(i32, usize)> {
    // SAFETY: the caller's contract above.
    let (op, o1, o2) = unsafe {
        (
            *code_ptr.add(pc),
            *code_ptr.add(pc + 1),
            *code_ptr.add(pc + 2),
        )
    };
    match op {
        0x1a..=0x1d => frame
            .get_local_compact_unchecked((op - 0x1a) as usize) // Cast: bytecode operand decoding
            .as_int()
            .map(|v| (v, pc + 1)),
        0x15 if (o1 as usize) < frame.max_locals as usize => frame
            .get_local_compact_unchecked(o1 as usize) // Cast: bytecode operand decoding
            .as_int()
            .map(|v| (v, pc + 2)),
        0x02..=0x08 => Some((i32::from(op) - 3, pc + 1)),
        0x10 => Some((i32::from(o1 as i8), pc + 2)), // Cast: bytecode operand decoding
        // Cast: bytecode operand decoding
        0x11 => Some((i32::from((((o1 as u16) << 8) | (o2 as u16)) as i16), pc + 3)),
        // A padding byte past the code is 0, never `arraylength` (0xbe), so
        // the `o1` / `o2` tests also keep the group inside the method.
        0x2a..=0x2d if arrlen && o1 == 0xbe => {
            // Cast: bytecode operand decoding
            let local = frame.get_local_compact_unchecked((op - 0x2a) as usize);
            field_fast::arraylength_of_local(local, census).map(|n| (n, pc + 2))
        }
        // Cast: bytecode operand decoding
        0x19 if arrlen && o2 == 0xbe && (o1 as usize) < frame.max_locals as usize => {
            // Cast: bytecode operand decoding
            let local = frame.get_local_compact_unchecked(o1 as usize);
            field_fast::arraylength_of_local(local, census).map(|n| (n, pc + 3))
        }
        // ldc #idx8 / ldc_w #idx16 of a warm int constant (wave 63, lane L7),
        // probed only when an `if_icmp<cond>` follows, so an `iload; ldc;
        // iadd` (a constant in a loop body) pays one byte compare, not a
        // second probe ahead of its own `ldc` arm's. `o1` / `o2` lie in the
        // padded buffer (the contract above), and for `ldc` `o2` IS the
        // following opcode; `ldc_w`'s is read only below `frame.code.len()`
        // (the padded length), so the read stays in the buffer. An index read
        // out of padding only names a site the table does not hold, and
        // `fused_icmp_arm!` re-checks the branch against `code_len` anyway.
        0x12 if ldc_bound && (0x9f..=0xa4).contains(&o2) => {
            ldc_int_site(frame, ldc_sites, u16::from(o1), census).map(|v| (v, pc + 2))
        }
        0x13 if ldc_bound
            && pc + 3 < frame.code.len()
            // SAFETY: `pc + 3` is below the padded buffer's length (tested
            // first), and `code_ptr` addresses that buffer.
            && (0x9f..=0xa4).contains(unsafe { &*code_ptr.add(pc + 3) }) =>
        {
            let index = (u16::from(o1) << 8) | u16::from(o2);
            ldc_int_site(frame, ldc_sites, index, census).map(|v| (v, pc + 3))
        }
        // getstatic #idx16 of a quickened int-typed static (wave 64, lane
        // L7), probed only when an `if_icmp<cond>` follows; the following
        // opcode is read only below the padded length, as for `ldc_w`. A
        // field watch armed (JVMTI or JDWP `FieldAccess`) declines before the
        // probe, so the unfused `getstatic` reports the access.
        0xb2 if getstatic_bound
            && pc + 3 < frame.code.len()
            // SAFETY: `pc + 3` is below the padded buffer's length (tested
            // first), and `code_ptr` addresses that buffer.
            && (0x9f..=0xa4).contains(unsafe { &*code_ptr.add(pc + 3) }) =>
        {
            if field_watch.unwrap_or_else(crate::runtime::jvmti::any_field_watchpoint_active) {
                return None;
            }
            let index = (u16::from(o1) << 8) | u16::from(o2);
            getstatic_int_site(frame, static_sites, index, read_static).map(|v| (v, pc + 3))
        }
        _ => None,
    }
}

/// The int a `getstatic` of pool `index` in `frame`'s class pushes, when the
/// thread's quickened static-site table holds the site and the field is
/// int-typed (`I`, `S`, `B`, `C`, `Z`: ints on the operand stack): what
/// `field_fast::getstatic_site_hit` does on a hit, minus the push. A site is
/// filled only once resolved and its holder initialized, so no
/// initialization check is owed. The field is read on every call through
/// `read_static` (`get_static_shared`), with the `SeqCst` fence after a
/// volatile read the quickened `getstatic` makes, and the value is taken as
/// `push_static_field_value` takes it for a non-reference field: an `Int` as
/// is, a zero-initialized `Object(None)` slot as `0`. Any other shape
/// declines (the unfused `getstatic` answers it). A hit counts as the
/// quickened hit it replaces (`INTERP_STATIC_SITE_GET_HIT`).
#[inline(always)]
pub(in crate::runtime::interpreter) fn getstatic_int_site<R: FnOnce(ClassId, usize) -> Value>(
    frame: &Frame,
    static_sites: &mut FieldSiteCache,
    index: u16,
    read_static: R,
) -> Option<i32> {
    let field = static_sites.get(frame.class_id, index)?;
    if !matches!(field.desc_byte, b'I' | b'S' | b'B' | b'C' | b'Z') {
        return None;
    }
    let (holder, slot, volatile) = (
        field.declaring_class_id,
        field.field_index,
        field.is_volatile,
    );
    let value = read_static(holder, slot);
    if volatile {
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
    }
    let v = match value {
        Value::Int(x) => x,
        Value::Object(None) => 0,
        _ => return None,
    };
    crate::vm::note_interp_static_site(crate::vm::INTERP_STATIC_SITE_GET_HIT);
    Some(v)
}

/// The int an `ldc` / `ldc_w` of pool `index` in `frame`'s class pushes, when
/// the thread's numeric-constant table holds it: the probe
/// `constants::execute_ldc` makes first (same key, same epoch validation), for
/// `fused_int_operand`'s `ldc` bound (wave 63, lane L7). `None` for a miss or
/// a `Float`; a hit counts as the `ldc` hit it replaces (`LDC_HIT`, under
/// `census`).
#[inline(always)]
pub(in crate::runtime::interpreter) fn ldc_int_site(
    frame: &Frame,
    ldc_sites: &mut site_cache::PrimitiveConstantSiteCache,
    index: u16,
    census: bool,
) -> Option<i32> {
    match ldc_sites.get(frame.class_id, index) {
        Some(&site_cache::PrimitiveConstant::Int(v)) => {
            site_cache::site_stats::bump_if(census, site_cache::site_stats::LDC_HIT);
            Some(v)
        }
        _ => None,
    }
}

/// The dispatch loop's inline answer to a warm `ldc` / `ldc_w` of pool
/// `index` (`CRATONVM_INTERP_LDC_INLINE_HIT`; interpreter round i2 wave 64,
/// lane L7): the probe `constants::execute_ldc` makes first (same table, key
/// and epoch validation) and, on a held `Int` / `Float`, its push, through
/// the typed checked push (`push_int` / `push_float` write the slot and kind
/// `ValueStack::push(Value::Int | Value::Float)` writes). `true` iff pushed
/// (the hit counted as `LDC_HIT` under `census`, as `execute_ldc` counts it);
/// `false` for a miss, a category-2 site, or a push that would overflow (the
/// checked push returns `Err` having written nothing), and the caller then
/// runs `execute_ldc`, which answers exactly as without the switch. The pc is
/// the caller's to store.
#[inline(always)]
pub(in crate::runtime::interpreter) fn ldc_inline_hit(
    frame: &mut Frame,
    ldc_sites: &mut site_cache::PrimitiveConstantSiteCache,
    index: u16,
    census: bool,
) -> bool {
    let pushed = match ldc_sites.get(frame.class_id, index).copied() {
        Some(site_cache::PrimitiveConstant::Int(v)) => frame.stack.push_int(v).is_ok(),
        Some(site_cache::PrimitiveConstant::Float(v)) => frame.stack.push_float(v).is_ok(),
        _ => false,
    };
    if pushed {
        site_cache::site_stats::bump_if(census, site_cache::site_stats::LDC_HIT);
    }
    pushed
}

/// The dispatch loop's inline answer to a warm `ldc2_w` of pool `index`
/// (`CRATONVM_INTERP_LDC2W_INLINE_HIT`; interpreter round i2 wave 65, lane
/// L7): the probe `constants::execute_ldc2w` makes first (same table, key and
/// epoch validation) and, on a held `Long` / `Double`, exactly its push
/// (`push_long` marks the slot `KIND_LONG` with `CompactValue::long`,
/// `push_double` marks it `KIND_DOUBLE` with `CompactValue::double_raw`, so
/// a collision-shaped long or a NaN payload keeps its bits). A category-2
/// value takes ONE operand-stack slot here, as in `execute_ldc2w`. `true`
/// iff pushed; `false` for a miss, a category-1 site, or a push that would
/// overflow (the checked push returns `Err` having written nothing), and the
/// caller then runs `execute_ldc2w`, which answers exactly as without the
/// switch. `execute_ldc2w` counts no census hit, so neither does this. The
/// pc is the caller's to store.
#[inline(always)]
pub(in crate::runtime::interpreter) fn ldc2w_inline_hit(
    frame: &mut Frame,
    ldc_sites: &mut site_cache::PrimitiveConstantSiteCache,
    index: u16,
) -> bool {
    match ldc_sites.get(frame.class_id, index).copied() {
        Some(site_cache::PrimitiveConstant::Long(v)) => frame.stack.push_long(v).is_ok(),
        Some(site_cache::PrimitiveConstant::Double(v)) => frame.stack.push_double(v).is_ok(),
        _ => false,
    }
}

/// The dispatch loop's inline answer to a quickened `getstatic` of pool
/// `index` (`CRATONVM_INTERP_GETSTATIC_INLINE_HIT`; interpreter round i2
/// wave 65, lane L7): what `field_fast::getstatic_site_hit` does on a hit,
/// through the frame the loop holds. The caller has already tested the kill
/// switch and the field-watch union. The site is probed by reference (no
/// clone); a filled site is resolved, static and its holder `Initialized`, so
/// no resolution or initialization is owed. The field is read through
/// `read_static` (`get_static_shared` in the loop), with the `SeqCst` fence
/// after a volatile read, and pushed exactly as
/// `field_access::push_static_field_value` pushes it:
///
/// * `J`: `KIND_LONG` with `CompactValue::long` of a `Long`, a widened `Int`,
///   or `0` for a zero-initialized `Object(None)` slot;
/// * `D`: `KIND_DOUBLE` with `CompactValue::double_raw` of a `Double`, a
///   `Long`'s bits, a converted `Int`, or `0.0` for `Object(None)`;
/// * a reference: `Int(0)` / `Long(0)` as `null`, through the generic
///   `ValueStack::push` (the same dead-object and vacated-push checks);
///   declined while `remap_trace` (`CRATONVM_DBG_REMAP_TRACE`) is armed, so
///   the call records the push's provenance;
/// * any other descriptor: an `Int` with `push_int`, a `Float` with
///   `push_float`, `Object(None)` as `push_int(0)` -- the slot and kind
///   `ValueStack::push(Value::Int | Value::Float)` writes.
///
/// Any other value shape declines (the call coerces it, or reports it). Every
/// push here is a checked one: on overflow it writes nothing and the call
/// runs (re-reading the field, which no observer can tell apart: a watch is
/// not armed) and raises as before -- the call's own `J` / `D` pushes are
/// unchecked, so this is the one place the two paths differ, and only where
/// the call would overflow the stack. `true` iff pushed, the hit counted as
/// the call counts it (`INTERP_STATIC_SITE_GET_HIT`); the pc is the
/// caller's to store.
#[inline(always)]
pub(in crate::runtime::interpreter) fn getstatic_inline_hit<R: FnOnce(ClassId, usize) -> Value>(
    frame: &mut Frame,
    static_sites: &mut FieldSiteCache,
    index: u16,
    remap_trace: bool,
    read_static: R,
) -> bool {
    let Some(field) = static_sites.get(frame.class_id, index) else {
        return false;
    };
    let (holder, slot, volatile, is_reference, desc) = (
        field.declaring_class_id,
        field.field_index,
        field.is_volatile,
        field.is_reference,
        field.desc_byte,
    );
    let category_two = matches!(desc, b'J' | b'D');
    if is_reference && remap_trace && !category_two {
        return false;
    }
    let value = read_static(holder, slot);
    if volatile {
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
    }
    let pushed = match desc {
        b'J' => {
            let lv = match value {
                Value::Long(x) => x,
                Value::Int(x) => i64::from(x),
                Value::Object(None) => 0,
                _ => return false,
            };
            frame
                .stack
                .push_compact_long_checked(CompactValue::long(lv))
                .is_ok()
        }
        b'D' => {
            let dv = match value {
                Value::Double(x) => x,
                // Cast: integer word reinterpreted as float/double bit pattern
                Value::Long(x) => f64::from_bits(x as u64),
                Value::Int(x) => f64::from(x),
                Value::Object(None) => 0.0,
                _ => return false,
            };
            frame
                .stack
                .push_compact_double_checked(CompactValue::double_raw(dv))
                .is_ok()
        }
        _ if is_reference => {
            let v = match value {
                Value::Int(0) | Value::Long(0) => Value::Object(None),
                other => other,
            };
            frame.stack.push(v).is_ok()
        }
        _ => match value {
            Value::Int(x) => frame.stack.push_int(x).is_ok(),
            Value::Float(x) => frame.stack.push_float(x).is_ok(),
            Value::Object(None) => frame.stack.push_int(0).is_ok(),
            _ => return false,
        },
    };
    if pushed {
        crate::vm::note_interp_static_site(crate::vm::INTERP_STATIC_SITE_GET_HIT);
    }
    pushed
}

/// The dispatch loop's inline store to a quickened `putstatic` of pool
/// `index` (`CRATONVM_INTERP_PUTSTATIC_INLINE_HIT`; interpreter round i2
/// wave 66, lane L7): what `field_fast::putstatic_site_hit` does on a hit,
/// through the frame the loop holds. The caller has already tested the kill
/// switch and the field-watch union (a `FieldModification` watch is in it).
/// The site is probed by reference (no clone); a filled site is resolved,
/// static and its holder `Initialized`. A final field declines, as the call's
/// hit declines it (`check_final_field_put` decides on the full path).
///
/// Every decline leaves the operand stack exactly as it was and the caller
/// runs `op_putstatic`, which answers exactly as without the switch. Beyond
/// the miss and the final field, an `Err` from `pop_static_field_value` (an
/// empty stack, or a `D` slot whose tag no `double` producer leaves) is a
/// decline too: every arm decides before it pops (i66-L7), so the error left
/// the stack untouched and the call reports it. The value is popped by
/// `pop_static_field_value` itself (`J` / a `KIND_DOUBLE` `D` bit-exact, a
/// reference coerced for its descriptor, a sub-int narrowed), the `SeqCst`
/// fences are taken around a volatile store,
/// and the value is stored through `write_static` (`set_static_shared` in
/// the loop, which takes the SATB pre-barrier and the `Release` fence
/// itself; the slot is never written directly). `true` iff stored, the hit
/// counted as the call counts it (`INTERP_STATIC_SITE_PUT_HIT`); the pc is
/// the caller's to store.
#[inline(always)]
pub(in crate::runtime::interpreter) fn putstatic_inline_hit<W: FnOnce(ClassId, usize, Value)>(
    frame: &mut Frame,
    static_sites: &mut FieldSiteCache,
    index: u16,
    write_static: W,
) -> bool {
    let Some(field) = static_sites.get(frame.class_id, index) else {
        return false;
    };
    if field.is_final {
        return false;
    }
    let (holder, slot, volatile, desc) = (
        field.declaring_class_id,
        field.field_index,
        field.is_volatile,
        field.desc_byte,
    );
    // An `Err` popped nothing (see the doc comment): decline.
    let Ok(value) = field_access::pop_static_field_value(&mut frame.stack, Some(desc)) else {
        return false;
    };
    if volatile {
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
    }
    write_static(holder, slot, value);
    if volatile {
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
    }
    crate::vm::note_interp_static_site(crate::vm::INTERP_STATIC_SITE_PUT_HIT);
    true
}

/// The int operand of a fused static increment at `p` of `code` (whose
/// unpadded length is `code_len`): `iconst_m1` .. `iconst_5`, `bipush`,
/// `sipush`, `iload n` and `iload_0` .. `iload_3` over a local below
/// `max_locals` holding an int (the slot `iload` would push and `iadd` pop as
/// an int; any other shape declines, so the unfused pair answers it), with
/// the pc after it. Every byte is read through the slice, below `code_len`.
#[inline(always)]
pub(in crate::runtime::interpreter) fn static_increment_operand(
    frame: &Frame,
    code: &[u8],
    code_len: usize,
    p: usize,
) -> Option<(i32, usize)> {
    let byte = |i: usize| {
        if i < code_len {
            code.get(i).copied()
        } else {
            None
        }
    };
    let op = byte(p)?;
    let local = |n: usize| {
        if n < frame.max_locals as usize {
            frame.get_local_compact_unchecked(n).as_int()
        } else {
            None
        }
    };
    match op {
        0x02..=0x08 => Some((i32::from(op) - 3, p + 1)),
        // Cast: bytecode operand decoding (signed byte)
        0x10 => Some((i32::from(byte(p + 1)? as i8), p + 2)),
        0x11 => {
            let (hi, lo) = (byte(p + 1)?, byte(p + 2)?);
            // Cast: bytecode operand decoding (signed short)
            Some((
                i32::from(((u16::from(hi) << 8) | u16::from(lo)) as i16),
                p + 3,
            ))
        }
        0x15 => Some((local(usize::from(byte(p + 1)?))?, p + 2)),
        0x1a..=0x1d => Some((local(usize::from(op - 0x1a))?, p + 1)),
        _ => None,
    }
}

/// [`fused_static_increment`]'s `mode`: `CRATONVM_INTERP_FUSED_STATIC_INCREMENT_WIDE`
/// (`VmConfig::interp_fused_static_increment_wide`; wave 68, lane L7).
pub(in crate::runtime::interpreter) const STATIC_INC_WIDE: u8 = 1;
/// [`fused_static_increment`]'s `mode`:
/// `CRATONVM_INTERP_FUSED_INCREMENT_EARLY_DECLINE`
/// (`VmConfig::interp_fused_increment_early_decline`; wave 69, lane L7).
pub(in crate::runtime::interpreter) const STATIC_INC_EARLY_DECLINE: u8 = 2;

/// [`fused_static_increment`]'s `mode`:
/// `CRATONVM_INTERP_FUSED_INCREMENT_LDC2W` admitted (the loop's
/// `SLG_FUSED_INCREMENT_LDC2W`; interpreter round i3 wave 83, lane L7): the
/// long operand may also be an `ldc2_w` of a `CONSTANT_Long` the thread's
/// numeric-constant table holds ([`ldc2w_long_operand`]). Read only together
/// with [`STATIC_INC_WIDE`] (the long group's own switch).
pub(in crate::runtime::interpreter) const STATIC_INC_LDC2W: u8 = 4;

/// The `mode` byte [`fused_static_increment`] takes, from its two switches.
#[inline(always)]
pub(in crate::runtime::interpreter) fn static_increment_mode(wide: bool, early_decline: bool) -> u8 {
    (if wide { STATIC_INC_WIDE } else { 0 })
        | (if early_decline {
            STATIC_INC_EARLY_DECLINE
        } else {
            0
        })
}

/// `mode` with [`STATIC_INC_LDC2W`] added when `on` (wave 83, lane L7).
#[inline(always)]
pub(in crate::runtime::interpreter) fn with_static_ldc2w(mode: u8, on: bool) -> u8 {
    if on {
        mode | STATIC_INC_LDC2W
    } else {
        mode
    }
}

/// The fused static increment's entry (interpreter round i2 wave 69, lane
/// L7): `mode` packs [`STATIC_INC_WIDE`] (the body's `wide`) and
/// [`STATIC_INC_EARLY_DECLINE`]. Under the latter
/// (`CRATONVM_INTERP_FUSED_INCREMENT_EARLY_DECLINE`) the group's bytes are
/// screened first ([`static_increment_screen`]) and a shape the body could
/// never fuse (`x = y + 1` over two statics, a `putstatic` cut off by the
/// end of the code, an operation other than `iadd` / `isub`) returns `None`
/// before the body is called: no local read, no site probe, and none of the
/// body's prologue. A group the screen passes, and every group without the
/// switch, is decided by [`fused_static_increment_body`] exactly as before.
///
/// Small and out of line; the body is `#[inline(always)]` into it (the
/// wave-69 tail call to a separate body cost each fused hit).
///
/// `tables` is the thread's `JvmThread::site_tables` (interpreter round i3
/// wave 83, lane L7, `i70-L7-proposal-one-argument-for-the-fused-helpers-site-tables`):
/// one argument, in the slot the static-site table alone took before, from
/// which the body reads `static_field_sites` exactly as it read that table.
#[inline(never)]
pub(in crate::runtime::interpreter) fn fused_static_increment<R, W>(
    frame: &Frame,
    code_len: usize,
    pc: usize,
    mode: u8,
    tables: &mut InterpSiteTables,
    read_static: R,
    write_static: W,
) -> Option<usize>
where
    R: FnOnce(ClassId, usize) -> Value,
    W: FnOnce(ClassId, usize, Value),
{
    if mode & STATIC_INC_EARLY_DECLINE != 0
        && !static_increment_screen(
            &frame.code,
            code_len,
            pc,
            mode & STATIC_INC_WIDE != 0,
            mode & STATIC_INC_LDC2W != 0,
        )
    {
        return None;
    }
    fused_static_increment_body(frame, code_len, pc, mode, tables, read_static, write_static)
}

/// The byte-only screen of [`fused_static_increment`] (wave 69, lane L7):
/// `false` when the group at `pc` of `code` (unpadded length `code_len`)
/// cannot be a shape [`fused_static_increment_body`] accepts under `wide`
/// (a `getstatic`, then [`increment_group_screen`] with `putstatic`). `true`
/// is not a promise: the body decodes again and decides. `ldc2w` ([`STATIC_INC_LDC2W`]) lets
/// an `ldc2_w` operand through under `wide`.
#[inline(always)]
pub(in crate::runtime::interpreter) fn static_increment_screen(
    code: &[u8],
    code_len: usize,
    pc: usize,
    wide: bool,
    ldc2w: bool,
) -> bool {
    let at = |i: usize| {
        if i < code_len {
            code.get(i).copied()
        } else {
            None
        }
    };
    at(pc) == Some(0xb2)
        && increment_group_screen(code, code_len, pc + 1, pc + 3, 0xb3, wide, false, ldc2w)
}

/// The tail both fused increments share, screened from opcode bytes alone
/// (interpreter round i2 wave 69, lane L7,
/// `CRATONVM_INTERP_FUSED_INCREMENT_EARLY_DECLINE`): the read's pool index at
/// `index_pc` (two bytes), an operand at `operand_pc` whose length and type
/// come from its opcode (`iconst_m1..5`, `iload_0..3`: 1 byte; `bipush`,
/// `iload`: 2; `sipush`: 3; under `wide` also `lconst_0/1`, `lload_0..3`: 1
/// and `lload`: 2), the matching operation (`iadd` / `isub` after an int,
/// `ladd` / `lsub` after a long), under `wide` an optional `i2b` / `i2c` /
/// `i2s` after an int operation, and `store_op` (`putstatic` or `putfield`)
/// with the same two index bytes; every byte read through the slice, below
/// `code_len`. Exactly the byte conditions of `static_increment_operand`,
/// `static_increment_long_operand` and the two decoders, without the local
/// reads (so `true` may still decline there: a local of another kind or
/// past `max_locals`).
///
/// `value_dup` (interpreter round i3 wave 71, lane L7,
/// `CRATONVM_INTERP_FUSED_FIELD_INCREMENT_VALUE`) also lets a `dup_x1` (after
/// an int operation or its conversion) or a `dup2_x1` (after a long one) sit
/// just before `store_op`: the pre-increment whose value is used. The static
/// screen passes `false`.
///
/// `ldc2w` (interpreter round i3 wave 83, lane L7,
/// `CRATONVM_INTERP_FUSED_INCREMENT_LDC2W`) also takes an `ldc2_w` (3 bytes,
/// a long) as the operand under `wide`; the constant itself is the
/// decoder's to look up.
#[inline(always)]
#[allow(clippy::too_many_arguments)]
pub(in crate::runtime::interpreter) fn increment_group_screen(
    code: &[u8],
    code_len: usize,
    index_pc: usize,
    operand_pc: usize,
    store_op: u8,
    wide: bool,
    value_dup: bool,
    ldc2w: bool,
) -> bool {
    let at = |i: usize| {
        if i < code_len {
            code.get(i).copied()
        } else {
            None
        }
    };
    let screen = || -> Option<bool> {
        let (hi, lo) = (at(index_pc)?, at(index_pc + 1)?);
        let (len, long) = match at(operand_pc)? {
            0x02..=0x08 | 0x1a..=0x1d => (1, false),
            0x10 | 0x15 => (2, false),
            0x11 => (3, false),
            0x09 | 0x0a | 0x1e..=0x21 if wide => (1, true),
            0x16 if wide => (2, true),
            0x14 if wide && ldc2w => (3, true),
            _ => return Some(false),
        };
        let arith_pc = operand_pc + len;
        if !matches!(
            (long, at(arith_pc)?),
            (false, 0x60 | 0x64) | (true, 0x61 | 0x65)
        ) {
            return Some(false);
        }
        let mut put_pc = arith_pc + 1;
        if wide && !long && matches!(at(put_pc)?, 0x91..=0x93) {
            put_pc += 1;
        }
        // `dup_x1` over an int result, `dup2_x1` over a long one.
        let value_dup_op = if long { 0x5d } else { 0x5a };
        if value_dup && at(put_pc)? == value_dup_op {
            put_pc += 1;
        }
        Some(at(put_pc)? == store_op && at(put_pc + 1)? == hi && at(put_pc + 2)? == lo)
    };
    screen().unwrap_or(false)
}

// The fused static int increment (`CRATONVM_INTERP_FUSED_STATIC_INCREMENT`;
// interpreter round i2 wave 67, lane L7): at `pc` of `frame` a `getstatic
// #F`, an int operand (`static_increment_operand`), an `iadd` or `isub`,
// and a `putstatic` whose 16-bit pool index is byte-for-byte the
// `getstatic`'s (never field equality: two Fieldrefs naming one field stay
// unfused), all below `code_len` (the unpadded length). The caller has
// tested `fusion_off`, the admission (both inline hits on, the kill switch
// with them) and the field-watch union, which holds `FieldAccess` and
// `FieldModification` watches alike.
//
// The site is probed once in `static_sites` (the one entry serves both
// opcodes); it must be an `I` field and not final (a final declines as the
// quickened `putstatic` declines it). The field is then read through
// `read_static` with the `SeqCst` fence after a volatile read (what
// `getstatic_int_site` does; a value shape other than `Int` or a
// zero-initialized `Object(None)` declines, nothing written), added to /
// subtracted from with the wrapping `iadd` / `isub` semantics, and stored as
// `Value::Int` through `write_static` between the `SeqCst` fences a volatile
// store takes (what `putstatic_inline_hit` does; `set_static_shared` takes
// the SATB pre-barrier and its own `Release` fence). One get hit and one
// put hit are counted, as the two quickened arms count them. Nothing is
// pushed or popped: the unfused group's net stack effect is zero, and the
// operand stack and locals are left exactly as it would leave them. Returns
// the pc past the `putstatic`; `None` (nothing read into the stack, nothing
// written) for any other shape.
//
// Under `wide` (`CRATONVM_INTERP_FUSED_STATIC_INCREMENT_WIDE`; wave 68,
// lane L7) two more shapes are served, with the same site, fences, final
// rule and declines: a narrow static, `getstatic #F (B / C / S); <int
// operand>; iadd | isub; i2b | i2c | i2s; putstatic #F`, where the
// conversion must be the descriptor's own (`B` + `i2b`, `C` + `i2c`, `S` +
// `i2s`) and the narrowed int is stored; and a `long` static, `getstatic #F
// (J); lconst_0 | lconst_1 | lload; ladd | lsub; putstatic #F`
// (`static_increment_long_operand`), read as the quickened `getstatic`
// reads a `J` (a `Long`, a widened `Int`, `0` for `Object(None)`) and
// stored as `Value::Long`. Without `wide` every one of those declines, and
// the wave-67 `I` group is decided exactly as before.
//
// Out of line: the decode and the site work stay out of the dispatch loop's
// frame; the loop tests only the operand opcode before calling. Since wave
// 69 (lane L7) this is the body behind the small entry
// [`fused_static_increment`], which may screen the group's bytes first.
// Inlined into its entry (wave-69 triage: the tail call cost the fused
// field increment ~12% per hit; the static one has the same shape).
//
// Wave 83 (lane L7): takes the entry's `mode` and the thread's grouped
// tables (`InterpSiteTables`) where it took `wide` and the static-site
// table; `wide` is `mode`'s [`STATIC_INC_WIDE`] as before, the site is
// probed in `tables.static_field_sites` as before, and under
// [`STATIC_INC_LDC2W`] (with `wide`) the long operand may also be an
// `ldc2_w` whose `CONSTANT_Long` `tables.prim_const_sites` holds
// ([`ldc2w_long_operand`]; a miss or a `Double` declines).
#[inline(always)]
pub(in crate::runtime::interpreter) fn fused_static_increment_body<R, W>(
    frame: &Frame,
    code_len: usize,
    pc: usize,
    mode: u8,
    tables: &mut InterpSiteTables,
    read_static: R,
    write_static: W,
) -> Option<usize>
where
    R: FnOnce(ClassId, usize) -> Value,
    W: FnOnce(ClassId, usize, Value),
{
    let wide = mode & STATIC_INC_WIDE != 0;
    let code: &[u8] = &frame.code;
    let byte = |i: usize| {
        if i < code_len {
            code.get(i).copied()
        } else {
            None
        }
    };
    if byte(pc)? != 0xb2 {
        return None;
    }
    let (hi, lo) = (byte(pc + 1)?, byte(pc + 2)?);
    // The operand: an int one always; a long one (`lconst_0/1`, a
    // `KIND_LONG`-marked long local) only under `wide`.
    let (operand, op_pc) = match static_increment_operand(frame, code, code_len, pc + 3) {
        Some((k, p)) => (IncrementOperand::Int(k), p),
        None if wide => {
            let (k, p) = match static_increment_long_operand(frame, code, code_len, pc + 3) {
                Some(found) => found,
                None if mode & STATIC_INC_LDC2W != 0 => ldc2w_long_operand(
                    code,
                    code_len,
                    pc + 3,
                    frame.class_id,
                    &mut tables.prim_const_sites,
                )?,
                None => return None,
            };
            (IncrementOperand::Long(k), p)
        }
        None => return None,
    };
    let subtract = match (operand, byte(op_pc)?) {
        (IncrementOperand::Int(_), 0x60) | (IncrementOperand::Long(_), 0x61) => false,
        (IncrementOperand::Int(_), 0x64) | (IncrementOperand::Long(_), 0x65) => true,
        _ => return None,
    };
    // Under `wide`, an int result may be narrowed (`i2b` / `i2c` / `i2s`)
    // before the store; the descriptor check below pairs it with its field.
    let mut put_pc = op_pc + 1;
    let narrow = match (operand, byte(put_pc)?) {
        (IncrementOperand::Int(_), conv @ 0x91..=0x93) if wide => {
            put_pc += 1;
            Some(conv)
        }
        _ => None,
    };
    if byte(put_pc)? != 0xb3 || byte(put_pc + 1)? != hi || byte(put_pc + 2)? != lo {
        return None;
    }
    let index = (u16::from(hi) << 8) | u16::from(lo);
    let field = tables.static_field_sites.get(frame.class_id, index)?;
    if field.is_final {
        return None;
    }
    // The field each shape stores to: `I` unnarrowed, `B` / `C` / `S` behind
    // their own conversion only, `J` from a long group.
    match (operand, narrow, field.desc_byte) {
        (IncrementOperand::Int(_), None, b'I')
        | (IncrementOperand::Int(_), Some(0x91), b'B')
        | (IncrementOperand::Int(_), Some(0x92), b'C')
        | (IncrementOperand::Int(_), Some(0x93), b'S')
        | (IncrementOperand::Long(_), None, b'J') => {}
        _ => return None,
    }
    let (holder, slot, volatile) = (
        field.declaring_class_id,
        field.field_index,
        field.is_volatile,
    );
    let value = read_static(holder, slot);
    if volatile {
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
    }
    let stored = match operand {
        IncrementOperand::Int(k) => {
            // What the quickened `getstatic` pushes for an int-typed static.
            let current = match value {
                Value::Int(x) => x,
                Value::Object(None) => 0,
                _ => return None,
            };
            let next = if subtract {
                current.wrapping_sub(k)
            } else {
                current.wrapping_add(k)
            };
            Value::Int(narrow_increment_result(next, narrow))
        }
        IncrementOperand::Long(k) => {
            // What the quickened `getstatic` pushes for a `J` static: a
            // `Long`, a widened `Int`, `0` for a zero-initialized slot.
            let current = match value {
                Value::Long(x) => x,
                Value::Int(x) => i64::from(x),
                Value::Object(None) => 0,
                _ => return None,
            };
            Value::Long(if subtract {
                current.wrapping_sub(k)
            } else {
                current.wrapping_add(k)
            })
        }
    };
    if volatile {
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
    }
    write_static(holder, slot, stored);
    if volatile {
        std::sync::atomic::fence(std::sync::atomic::Ordering::SeqCst);
    }
    crate::vm::note_interp_static_site(crate::vm::INTERP_STATIC_SITE_GET_HIT);
    crate::vm::note_interp_static_site(crate::vm::INTERP_STATIC_SITE_PUT_HIT);
    Some(put_pc + 3)
}

/// The operand of a fused static increment, decoded (since wave 69, lane L7,
/// also of the widened fused field increment).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::runtime::interpreter) enum IncrementOperand {
    Int(i32),
    Long(i64),
}

/// `i2b` (0x91), `i2c` (0x92), `i2s` (0x93) of `v` as the unfused conversion
/// computes it; `v` unchanged for `None` (an `I` store). The result is what
/// `pop_static_field_value` would store for the matching descriptor
/// (`narrow_int_to_field_type` of an already narrowed int is the identity).
#[inline(always)]
pub(in crate::runtime::interpreter) fn narrow_increment_result(v: i32, narrow: Option<u8>) -> i32 {
    match narrow {
        // Cast: JVM i2b / i2c / i2s truncation
        Some(0x91) => i32::from(v as i8),
        Some(0x92) => i32::from(v as u16),
        Some(0x93) => i32::from(v as i16),
        _ => v,
    }
}

/// The long operand of a widened fused static increment
/// (`CRATONVM_INTERP_FUSED_STATIC_INCREMENT_WIDE`; wave 68, lane L7) at `p`
/// of `code` (unpadded length `code_len`): `lconst_0` / `lconst_1`, or
/// `lload n` / `lload_0` .. `lload_3` over a local whose two slots are below
/// `max_locals` and whose first is `KIND_LONG`-marked (the bits `lload`
/// would push and `ladd` read with `as_long_unchecked`; an unmarked slot
/// declines, so the unfused pair decides), with the pc after it. `ldc2_w`
/// is not decoded (its warm constant lives in another per-thread table).
#[inline(always)]
pub(in crate::runtime::interpreter) fn static_increment_long_operand(
    frame: &Frame,
    code: &[u8],
    code_len: usize,
    p: usize,
) -> Option<(i64, usize)> {
    let byte = |i: usize| {
        if i < code_len {
            code.get(i).copied()
        } else {
            None
        }
    };
    let op = byte(p)?;
    let local = |n: usize| {
        if n + 1 < usize::from(frame.max_locals) && frame.local_is_long_marked(n) {
            Some(frame.get_local_compact_unchecked(n).as_long_unchecked())
        } else {
            None
        }
    };
    match op {
        0x09 => Some((0, p + 1)),
        0x0a => Some((1, p + 1)),
        0x16 => Some((local(usize::from(byte(p + 1)?))?, p + 2)),
        0x1e..=0x21 => Some((local(usize::from(op - 0x1e))?, p + 1)),
        _ => None,
    }
}

/// The `ldc2_w` long operand of a fused increment
/// (`CRATONVM_INTERP_FUSED_INCREMENT_LDC2W`; interpreter round i3 wave 83,
/// lane L7, `i69-L7-proposal-ldc2w-long-operand-for-the-fused-increments`) at
/// `p` of `code` (unpadded length `code_len`): an `ldc2_w` (0x14) whose pool
/// index (the two bytes after it, below `code_len`) the frame's class
/// `class_id` has a `Long` for in `consts`, the thread's numeric-constant
/// table, with the pc after it. Exactly the probe the quickened `ldc2_w` makes
/// (`ldc2w_inline_hit`: same table, key and epoch validation); a miss, a
/// `Double` (a `CONSTANT_Double` is never an operand of `ladd` / `lsub`) or
/// any other opcode is `None`, so the group declines and the unfused
/// `ldc2_w` resolves (and fills the table) as before. Nothing is pushed.
#[inline(always)]
pub(in crate::runtime::interpreter) fn ldc2w_long_operand(
    code: &[u8],
    code_len: usize,
    p: usize,
    class_id: ClassId,
    consts: &mut site_cache::PrimitiveConstantSiteCache,
) -> Option<(i64, usize)> {
    let byte = |i: usize| {
        if i < code_len {
            code.get(i).copied()
        } else {
            None
        }
    };
    // Wave 83 host rework: the byte after the constant must be `ladd` /
    // `lsub` before the table is probed, so a `double` group (`ldc2_w;
    // dadd`, which the helpers reach and decline) pays no probe.
    if byte(p)? != 0x14 || !matches!(byte(p + 3)?, 0x61 | 0x65) {
        return None;
    }
    let index = (u16::from(byte(p + 1)?) << 8) | u16::from(byte(p + 2)?);
    match consts.get(class_id, index).copied() {
        Some(site_cache::PrimitiveConstant::Long(v)) => Some((v, p + 3)),
        _ => None,
    }
}

/// The last fast-path dispatch the opcode-pair census saw
/// (`CRATONVM_QUICKEN_STATS=pairs`; see `cratonvm_reader::quickened`).
#[derive(Clone, Copy)]
pub(in crate::runtime::interpreter) struct OpcodePairCursor {
    pub(in crate::runtime::interpreter) frame_idx: usize,
    pub(in crate::runtime::interpreter) code_ptr: *const u8,
    /// The pc of that instruction's fall-through successor; `usize::MAX`
    /// when its length is not fixed by the opcode.
    pub(in crate::runtime::interpreter) next_pc: usize,
    pub(in crate::runtime::interpreter) opcode: u8,
    /// The opcode dispatched just before `opcode` when `opcode` was its
    /// fall-through successor (a counted pair), else `0x100`: the first
    /// opcode of a triple the census counts next (interpreter round i6 wave
    /// 120, lane L8).
    pub(in crate::runtime::interpreter) prev_opcode: u16,
}

impl OpcodePairCursor {
    /// No previous dispatch: pairs with nothing.
    pub(in crate::runtime::interpreter) const NONE: Self = Self {
        frame_idx: usize::MAX,
        code_ptr: std::ptr::null(),
        next_pc: usize::MAX,
        opcode: 0,
        prev_opcode: 0x100,
    };
}

/// Census hook for the dispatch of `opcode` at `pc` of frame `frame_idx`
/// (whose bytecode is at `code_ptr`): count it as a pair with `prev` when it
/// is `prev`'s fall-through successor in the same activation — the pairs a
/// superinstruction could fuse. A call, return, taken branch or unwind breaks
/// the pair. Returns the new cursor. Reached only while the census is armed.
#[cold]
#[inline(never)]
pub(in crate::runtime::interpreter) fn note_opcode_pair(
    prev: OpcodePairCursor,
    frame_idx: usize,
    code_ptr: *const u8,
    pc: usize,
    opcode: u8,
) -> OpcodePairCursor {
    let chained = prev.next_pc == pc && prev.frame_idx == frame_idx && prev.code_ptr == code_ptr;
    if chained {
        cratonvm_reader::quickened::record_opcode_pair(prev.opcode, opcode);
        // Wave 120 (lane L8): and the triple, when `prev` was itself the
        // fall-through successor of the one before it.
        if let Ok(first) = u8::try_from(prev.prev_opcode) {
            cratonvm_reader::quickened::record_opcode_triple(first, prev.opcode, opcode);
        }
    }
    let len = usize::from(cratonvm_reader::quickened::fixed_instruction_length(opcode));
    OpcodePairCursor {
        frame_idx,
        code_ptr,
        next_pc: if len == 0 { usize::MAX } else { pc + len },
        opcode,
        prev_opcode: if chained { u16::from(prev.opcode) } else { 0x100 },
    }
}

/// [`note_opcode_pair`] against the dispatch loop's boxed cursor
/// (interpreter round i4 wave 84, lane L7, frame ratchet): `None` is
/// [`OpcodePairCursor::NONE`], and the first call allocates the box, so a
/// loop that never runs the census keeps one null pointer in its frame
/// instead of the 32-byte cursor. Out of line: it runs only with
/// `CRATONVM_QUICKEN_STATS=pairs` armed.
#[inline(never)]
pub(in crate::runtime::interpreter) fn note_opcode_pair_in(
    slot: &mut Option<Box<OpcodePairCursor>>,
    frame_idx: usize,
    code_ptr: *const u8,
    pc: usize,
    opcode: u8,
) {
    let prev = slot.as_deref().copied().unwrap_or(OpcodePairCursor::NONE);
    let next = note_opcode_pair(prev, frame_idx, code_ptr, pc, opcode);
    *slot = Some(match slot.take() {
        Some(mut cursor) => {
            *cursor = next;
            cursor
        }
        None => Box::new(next),
    });
}

/// The condition of `if_icmpeq`..`if_icmple` (`0x9f..=0xa4`) on `a` (the
/// deeper operand) and `b`. Callers admit only that opcode range.
#[inline(always)]
pub(in crate::runtime::interpreter) fn if_icmp_taken(op: u8, a: i32, b: i32) -> bool {
    match op {
        0x9f => a == b, // if_icmpeq
        0xa0 => a != b, // if_icmpne
        0xa1 => a < b,  // if_icmplt
        0xa2 => a >= b, // if_icmpge
        0xa3 => a > b,  // if_icmpgt
        _ => a <= b,    // if_icmple (0xa4)
    }
}

/// Whether `if<cond>` opcode `op` (`0x99..=0x9e`) branches on `v`, the
/// int-against-zero compares of the `ifeq` .. `ifle` arms, for the fused
/// `getstatic; if<cond>` (`CRATONVM_INTERP_FUSED_GETSTATIC_IFZERO`; wave 66,
/// lane L7).
#[inline(always)]
pub(in crate::runtime::interpreter) fn if_zero_taken(op: u8, v: i32) -> bool {
    match op {
        0x99 => v == 0, // ifeq
        0x9a => v != 0, // ifne
        0x9b => v < 0,  // iflt
        0x9c => v >= 0, // ifge
        0x9d => v > 0,  // ifgt
        _ => v <= 0,    // ifle (0x9e)
    }
}


pub(crate) fn count_method_params(descriptor: &str) -> usize {
    let mut count = 0;
    let bytes = descriptor.as_bytes();
    let mut i = 1; // skip opening '('

    while i < bytes.len() && bytes[i] != b')' {
        match bytes[i] {
            b'B' | b'C' | b'D' | b'F' | b'I' | b'J' | b'S' | b'Z' => {
                count += 1;
                i += 1;
            }
            b'L' => {
                count += 1;
                while i < bytes.len() && bytes[i] != b';' {
                    i += 1;
                }
                i += 1;
            }
            b'[' => {
                while i < bytes.len() && bytes[i] == b'[' {
                    i += 1;
                }
                if i < bytes.len() {
                    if bytes[i] == b'L' {
                        while i < bytes.len() && bytes[i] != b';' {
                            i += 1;
                        }
                        i += 1;
                    } else {
                        i += 1;
                    }
                }
                count += 1;
            }
            _ => i += 1,
        }
    }

    count
}

// -- Arithmetic helpers --

// T10.9.D — direct CompactValue arithmetic helpers.
//
// pop_int/pop_long/pop_float/pop_double already inspect raw compact slots;
// the push side now builds a CompactValue directly and avoids the
// Value-enum round-trip.

pub(in crate::runtime::interpreter) fn int_binop(frame: &mut Frame, op: impl FnOnce(i32, i32) -> i32) -> Result<(), RuntimeError> {
    let b = frame.stack.pop_int()?;
    let a = frame.stack.pop_int()?;
    frame.stack.push_int(op(a, b))
}

pub(in crate::runtime::interpreter) fn long_binop(frame: &mut Frame, op: impl FnOnce(i64, i64) -> i64) -> Result<(), RuntimeError> {
    let b = frame.stack.pop_long()?;
    let a = frame.stack.pop_long()?;
    frame.stack.push_long(op(a, b))
}

pub(in crate::runtime::interpreter) fn float_binop(frame: &mut Frame, op: impl FnOnce(f32, f32) -> f32) -> Result<(), RuntimeError> {
    let b = frame.stack.pop_float()?;
    let a = frame.stack.pop_float()?;
    frame.stack.push_float(op(a, b))
}

pub(in crate::runtime::interpreter) fn double_binop(frame: &mut Frame, op: impl FnOnce(f64, f64) -> f64) -> Result<(), RuntimeError> {
    let b = frame.stack.pop_double()?;
    let a = frame.stack.pop_double()?;
    frame.stack.push_double(op(a, b))
}

// -- Multi-dimensional array allocation --

/// The part of a `multianewarray` site's answer that does not depend on the
/// dimension VALUES: the leaf element type, the descriptor's bracket count, and
/// the per-level component class ids.
///
/// All three are functions of `(referencing class, cp index, dimensions)` only,
/// and that triple is fixed for a given bytecode site. Recomputing them per
/// execution costs a `class_manager` read lock, a `get_class_name`, two `String`
/// builds and up to two loader-aware class resolutions — measured at +227 ns on
/// a 300k-iteration `new String[4][4]` loop (418 → 645 ns/op), which is 55% on
/// top of the allocation itself. Memoized, the site pays that once.
pub(in crate::runtime::interpreter) struct MultiANewArrayPlan {
    pub(in crate::runtime::interpreter) leaf_et: ArrayElementType,
    pub(in crate::runtime::interpreter) total_array_depth: usize,
    /// Component class id per allocated level, outermost first.
    pub(in crate::runtime::interpreter) component_ids: Box<[ClassId]>,
}

/// Per-site memo for [`MultiANewArrayPlan`], keyed by
/// `(vm_identity, referencing_class_id, cp_index, dimensions)`.
///
/// `vm_identity` is part of the key because a `ClassId` is unique only within
/// one VM, and this table is process-wide: two VMs in one process (the unit
/// tests build one per test; `libcratonvm` can host several) mint the same
/// ids for different classes, and a plan keyed on the id alone handed VM B the
/// component class ids VM A resolved for ITS class — arrays stamped with
/// another VM's classes. Same reasoning as the lambda caches'
/// `VmScopedClassPairKey`.
///
/// Only FULLY resolved plans are inserted. A component-class resolution
/// failure now throws before a plan exists (it used to fall back to
/// `ClassId(0)` and allocate), so the one remaining non-cacheable shape is a
/// resolver answering `Ok(ClassId(0))` for a class-typed level, kept out
/// defensively: memoizing it would make a transient answer permanent.
pub(in crate::runtime::interpreter) type MultiANewArrayPlanCache =
    parking_lot::RwLock<rustc_hash::FxHashMap<(usize, u32, u16, u8), Arc<MultiANewArrayPlan>>>;

pub(in crate::runtime::interpreter) fn multianewarray_plan_cache() -> &'static MultiANewArrayPlanCache {
    static CACHE: std::sync::OnceLock<MultiANewArrayPlanCache> = std::sync::OnceLock::new();
    CACHE.get_or_init(|| parking_lot::RwLock::new(rustc_hash::FxHashMap::default()))
}

/// Drop every memoized [`MultiANewArrayPlan`], of every VM.
///
/// Only for a caller that cannot name a VM: the classloading crate's
/// resolution-invalidate hook (`vm_init::resolution_invalidate_adapter`),
/// which fires on a redefinition or a layout change with a bare class id that
/// any live VM may have minted. A redefinition can renumber a constant pool
/// under the same id, so every VM's plans for that id are suspect. The unload
/// path knows its VM and its class set, and uses
/// [`invalidate_multianewarray_plans_naming`] instead.
pub(crate) fn invalidate_multianewarray_plans() {
    multianewarray_plan_cache().write().clear();
}

/// Drop the plans of `vm_identity` that name a class `is_gone` accepts, on
/// either side: the referencing class of the key, or any level's component
/// class in the value. Every other VM's plans, and this VM's plans that name
/// only live classes, are kept.
///
/// gc-common w10-f: the unload path used to call
/// [`invalidate_multianewarray_plans`], which cleared EVERY VM's plans on one
/// VM's unload (and every plan of the unloading VM, whatever it named). Class
/// ids are not reused within a VM (`ClassStore::add` appends and `remove`
/// leaves a tombstone), so a plan that names no unloaded id is exactly as
/// valid after the unload as before it. A plan that does name one is dropped
/// so no site keeps serving a tombstoned id.
pub(crate) fn invalidate_multianewarray_plans_naming(
    vm_identity: usize,
    is_gone: &dyn Fn(ClassId) -> bool,
) {
    retain_multianewarray_plans(
        &mut multianewarray_plan_cache().write(),
        vm_identity,
        is_gone,
    );
}

/// Drop every plan of `vm_identity`: its class ids mean nothing once it is
/// gone. For `release_vm_native_state` (the exact edit is in
/// `docs/feature-designs/real-jdk-mode.md`).
pub(crate) fn forget_vm_multianewarray_plans(vm_identity: usize) {
    retain_multianewarray_plans(
        &mut multianewarray_plan_cache().write(),
        vm_identity,
        &|_| true,
    );
}

/// The one retain both per-VM forms share, over a map the caller holds, so
/// its unit test can run on a private map rather than on the process-wide
/// cache that parallel tests (and the redefine hook's wholesale clear) touch.
pub(in crate::runtime::interpreter) fn retain_multianewarray_plans(
    plans: &mut rustc_hash::FxHashMap<(usize, u32, u16, u8), Arc<MultiANewArrayPlan>>,
    vm_identity: usize,
    is_gone: &dyn Fn(ClassId) -> bool,
) {
    plans.retain(|&(vm, referencing, _, _), plan| {
        vm != vm_identity
            || !(is_gone(ClassId::new(referencing))
                || plan.component_ids.iter().any(|&id| is_gone(id)))
    });
}


/// Resolve a `multianewarray` site's array class and allocate the array.
///
/// **This is the ONE implementation of JVMS §multianewarray's typing rules in
/// this VM.** The interpreter's `Instruction::Multianewarray` arm and the JIT's
/// `jit_multianewarray_n` helper are both thin callers of it, and that is
/// deliberate: the two used to be separate transcriptions and only the
/// interpreter's carried the component-class resolution below. The JIT's copy
/// allocated every level with `ClassId(0)`, so a JIT-compiled `new String[a][b]`
/// produced an object whose `getClass()` read back `[Ljava.lang.Object;` — the
/// `DSCompiler.getCompiler` `ClassCastException` witness.
///
/// `sizes` is outermost-first and must be non-empty; its length is the
/// `dimensions` operand, which JVMS §4.9.1 allows to be SMALLER than the
/// referenced array class's bracket count (the unspecified inner dimensions
/// stay null).
pub(crate) fn multianewarray_alloc(
    shared: &SharedVm,
    thread: &mut JvmThread,
    referencing_class_id: ClassId,
    cp_index: u16,
    sizes: &[usize],
) -> Result<ObjectRef, MethodCallFailed> {
    multianewarray_alloc_at_site(shared, thread, referencing_class_id, cp_index, None, sizes)
}

/// How a compiled `multianewarray` site names its constant-pool entry
/// (interpreter round i1 wave 44, lane L2;
/// `interpreter-L2-multianewarray-and-type-check-sites-judge-their-index-before-the-pool-read-FIXED-20261008`).
///
/// A compiled body keeps running after its class's redefinition, and its
/// site's index is translated from the pool it was compiled against. The
/// caller's lock-free translation (`probe`, judged at class-redefinition count
/// `judged_at`) keys only the plan-cache probe, and a hit is kept only while no
/// redefinition began since; the pool itself is read at the index `index_in`
/// judges under the class-manager guard the read holds, and every JVMS §5.4.3
/// record and the plan inserted are keyed by that index.
pub(crate) struct CompiledMultiANewArraySite<'a> {
    /// The class-redefinition count the probe index was judged at.
    pub(crate) judged_at: u64,
    /// The site's index in the pool `cm` holds, judged under `cm`.
    pub(crate) index_in: &'a dyn Fn(&crate::classloading::ClassManager) -> u16,
}

/// [`multianewarray_alloc`] for a site whose index may need judging under the
/// pool read's guard: `compiled` is `None` for the interpreter's own arm (its
/// frame's code indexes the pool it runs against, `obsolete_frames`), and a
/// [`CompiledMultiANewArraySite`] for the JIT's `jit_multianewarray_n`, whose
/// `cp_index` is then only the plan-cache probe.
pub(crate) fn multianewarray_alloc_at_site(
    shared: &SharedVm,
    thread: &mut JvmThread,
    referencing_class_id: ClassId,
    cp_index: u16,
    compiled: Option<&CompiledMultiANewArraySite<'_>>,
    sizes: &[usize],
) -> Result<ObjectRef, MethodCallFailed> {
    if sizes.is_empty() {
        return Err(VmError::Internal {
            message: "multianewarray: dimensions must be >= 1".to_string(),
        }
        .into());
    }
    // `dimensions` is a single bytecode operand, so it never exceeds u8::MAX;
    // the guard below rejects anything past the descriptor's bracket count
    // anyway, and 255 is the JVMS ceiling on that.
    let dims_key = u8::try_from(sizes.len()).unwrap_or(u8::MAX);
    let cache_key = (
        shared.vm_identity,
        referencing_class_id.as_u32(),
        cp_index,
        dims_key,
    );

    // Clone the `Arc` and DROP THE LOCK before allocating. `alloc_multi_array`
    // can trigger a GC, and the GC's own `unload_user_classes` path is what
    // calls `invalidate_multianewarray_plans` — i.e. it takes this lock for
    // WRITE. Holding the read guard across the allocation would let a thread
    // wait for a collection that is waiting for this reader, through a
    // `parking_lot::RwLock` that is neither reentrant nor writer-starving. One
    // refcount bump is the whole cost of not having that edge.
    let hit = multianewarray_plan_cache().read().get(&cache_key).cloned();
    // A compiled site's probe index was judged lock-free: its plan is kept
    // only while no redefinition began since (the seqlock's second read, as
    // the JIT's `CpSite::verdict_still_holds`); otherwise the site is judged
    // again below, under the guard its pool is read under.
    let hit = hit.filter(|_| {
        compiled.is_none_or(|site| {
            std::sync::atomic::fence(std::sync::atomic::Ordering::Acquire);
            crate::classloading::class_redefinition_count() == site.judged_at
        })
    });
    if let Some(plan) = hit {
        return alloc_multi_array_collecting(
            shared,
            thread,
            sizes,
            plan.leaf_et,
            plan.total_array_depth,
            &plan.component_ids,
        );
    }

    // Resolve the leaf element type AND total array depth from the array class
    // descriptor. The `dimensions` operand may be less than the total `[`
    // count, in which case the unspecified inner dimensions stay null and the
    // deepest *allocated* array must hold references (not the leaf type) — see
    // `alloc_multi_array`.
    // BEFORE the pool read: a failure below is recorded under it
    // (`record_resolution_failure_as_of`; i22-L5, interpreter round i1 wave 24).
    let fill_as_of = crate::classloading::resolution::ResolutionCache::fill_snapshot();
    let (cp_index, plan_as_of, leaf_et, total_array_depth, leaf_desc, array_class_name) = {
        let cm = shared.classes.class_manager.read();
        // The class-redefinition count of the pool this guard holds: a
        // redefinition advances it under the class-manager writer before it
        // replaces the pool and drops the plans naming the class
        // (`invalidate_multianewarray_plans_naming`), so the plan built below
        // is filed only while it has not moved (interpreter round i1 wave 44,
        // lane L2).
        let plan_as_of = crate::classloading::class_redefinition_count();
        // A compiled site's index in the pool this guard holds: every record
        // below, and the plan inserted, are keyed by it (interpreter round i1
        // wave 44, lane L2; `CompiledMultiANewArraySite`).
        let cp_index = match compiled {
            Some(site) => {
                let pool_guard: &crate::classloading::ClassManager = &cm;
                (site.index_in)(pool_guard)
            }
            None => cp_index,
        };
        let class = cm
            .get_class(referencing_class_id)
            .ok_or_else(|| VmError::Internal {
                message: "current class not found".to_string(),
            })?;
        let array_class_name = class
            .constant_pool
            .get_class_name(cp_index)
            .ok_or_else(|| VmError::Internal {
                message: format!("invalid class ref at cp#{cp_index}"),
            })?;
        // Strip leading '[' to find the leaf type descriptor; the count of
        // stripped `[`s is the total array depth.
        let total_depth = array_class_name
            .as_bytes()
            .iter()
            .take_while(|&&b| b == b'[')
            .count();
        let leaf = &array_class_name.as_bytes()[total_depth..];
        let et = match leaf.first() {
            Some(b'I') => ArrayElementType::Int,
            Some(b'J') => ArrayElementType::Long,
            Some(b'F') => ArrayElementType::Float,
            Some(b'D') => ArrayElementType::Double,
            Some(b'B') => ArrayElementType::Byte,
            Some(b'C') => ArrayElementType::Char,
            Some(b'S') => ArrayElementType::Short,
            Some(b'Z') => ArrayElementType::Boolean,
            _ => ArrayElementType::Reference,
        };
        (
            cp_index,
            plan_as_of,
            et,
            total_depth,
            array_class_name[total_depth..].to_string(),
            array_class_name.to_string(),
        )
    };
    // The plan is filed under the index the pool was read at.
    let cache_key = (cache_key.0, cache_key.1, cp_index, cache_key.3);

    // JVMS §4.9.1 static constraint: `dimensions` must not exceed the number of
    // leading `[` in the referenced array class.
    //
    // SECURITY (defense-in-depth, same policy as `execute_ldc`'s
    // `ClassFormatError` conversion): the type-state verifier does enforce this
    // (`verify_insn.rs`, `Instruction::Multianewarray`), but that pass does NOT
    // run for every class. Pass 3 is deferred wholesale for any class defined
    // by a user-defined loader while `loader_aware_resolution()` is on — which
    // is the default, and covers every Spring / WildFly / H2 application class
    // — and the structural-only substitute
    // (`verifier::verify_method_structural`) never looks at this operand.
    // `-Xverify:none` removes it too.
    //
    // Without this guard `total_array_depth - d - 1` below underflows: in a
    // release build (overflow-checks off) it wraps to `usize::MAX`, and
    // `"[".repeat(usize::MAX)` then asks the allocator for `usize::MAX` bytes,
    // which aborts the process rather than raising anything Java can catch.
    // It sits in front of the cache insert as well as the subtraction: a site
    // that throws here must throw every time, never be memoized into a plan.
    if sizes.len() > total_array_depth {
        let (cls, mname) = match thread.frames.last() {
            Some(f) => (f.class_name().to_string(), f.method_name().to_string()),
            None => (String::new(), String::new()),
        };
        return Err(crate::runtime::exceptions::throw_linkage_error(
            shared,
            thread,
            LinkageError::VerifyError {
                class_name: cls,
                method_name: mname,
                message: format!(
                    "multianewarray: dimensions {} exceeds array bracket count {} \
                     of type at cp#{cp_index}",
                    sizes.len(),
                    total_array_depth
                ),
            },
        ));
    }

    // Resolve the *component* class id for each allocated array level so the
    // array objects carry their precise class (e.g. the outer level of
    // `new String[8][8]` is a `[[Ljava/lang/String;` whose component is
    // `[Ljava/lang/String;`). Without this every multi-dim array was allocated
    // with `ClassId(0)` and `getClass().getName()` collapsed to
    // `[Ljava/lang/Object;`. Each level d's component descriptor is
    // `[`×(total_depth-d-1) followed by the leaf descriptor; a primitive leaf
    // (`I`, `C`, …) needs no class (the element type drives naming).
    //
    // A resolution FAILURE is the instruction's failure (JVMS §6.5
    // `multianewarray`: the class reference is resolved per §5.4.3.1, which
    // for an array class resolves its element class), so it propagates before
    // anything is allocated — HotSpot's `new Missing[2][2]` with `Missing`
    // gone throws `NoClassDefFoundError: Missing`. This used to be
    // `.unwrap_or(ClassId(0))`: the arrays were allocated anyway, mis-typed,
    // and a Java exception thrown by a user loader's `loadClass` on the way
    // was dropped on the floor. Both callers (the decoded arm and the JIT's
    // `jit_multianewarray_n`) already route an `Err` through their throw paths.
    let leaf_is_reference = leaf_desc.starts_with('L') && leaf_desc.ends_with(';');
    // JVMS §5.4.3: an entry whose resolution already failed fails the same way
    // again without asking any loader (the record `ldc`/`new`/`checkcast`/
    // `anewarray` keep, `constants::record_resolution_failure`). A failed site
    // has no plan, so this is reached on every retry.
    if let Some(recorded) =
        recorded_resolution_failure(shared, thread, referencing_class_id, cp_index)
    {
        return Err(recorded);
    }
    // The element class, which the §5.4.4 access check below is about. The
    // `NoClassDefFoundError` names the ARRAY class the entry names
    // (`[[Lp/Gone;`), with the element as its `ClassNotFoundException` cause:
    // HotSpot 25 (probe `tools/probes/interp/L5/L5W24MissingClassCause.java`;
    // this said "the element class" before wave 24, which HotSpot does not do).
    let ncdfe_name = if leaf_is_reference {
        leaf_desc[1..leaf_desc.len() - 1].to_string()
    } else {
        leaf_desc.clone()
    };
    // Round 15 wave 5, lane classsite2 (`r15w5-classsite-class-entry-shared-
    // by-every-instruction-patch-20260930.md`, part E): for an isolated URL
    // loader's class the ENTRY is resolved once, to the array class it names,
    // and every level's component is read off that class's `array_info`
    // chain (`multianewarray_entry_components`). Resolving each level by name
    // below asked the loader for the element twice on the first execution
    // (the `[..` level's element drive, then the leaf: `R15ClasssiteEntries`
    // `MA=2`, HotSpot 1). `None` keeps the by-name loop: switch
    // (`CRATONVM_CLASS_SITE_ISOLATED_STICKY`) off, any other loader shape, a
    // primitive leaf, a one-level class, or a chain without `array_info`.
    //
    // JIT round 16, lane classsite (R3 of
    // `r15w5-classsite2-review-findings-FIXED-20261001.md`, proposal CS2-4): the
    // walk is taken for EVERY referencing class with a reference leaf
    // (`CRATONVM_MULTIANEWARRAY_ENTRY_WALK`, default on), so `m`'s component
    // and the innermost level's element are one answer by construction
    // instead of two independent by-name lookups. The entry's record stays
    // isolated-only (`record`).
    let entry_components = if leaf_is_reference && total_array_depth >= 2 {
        let record = constants::class_entry_record_applies(shared, thread, referencing_class_id);
        if record || true {
            multianewarray_entry_components(
                shared,
                thread,
                (referencing_class_id, cp_index),
                &array_class_name,
                (sizes.len(), record),
                fill_as_of,
            )?
        } else {
            None
        }
    } else {
        None
    };
    let mut component_ids: Vec<ClassId> = match entry_components {
        Some(ids) => ids,
        None => Vec::with_capacity(sizes.len()),
    };
    // Defensive only, now that failures propagate: a resolver answering
    // `Ok(ClassId(0))` for a class-typed level must still not be memoized.
    let mut fully_resolved = true;
    // Empty when the entry's chain answered every level above.
    for d in component_ids.len()..sizes.len() {
        let comp_brackets = total_array_depth - d - 1;
        let cid = if comp_brackets > 0 {
            // Component is itself an array class — resolve `[…`.
            //
            // JVMS §5.3.3: that inner array class is defined by the defining
            // loader of ITS component, so it must be resolved loader-faithfully.
            // This `ClassId` is stamped into the allocated array object's header
            // and is what a later `getClass()` / `getComponentType()` reads back,
            // so collapsing two loaders' `[Lp/X;` here would make
            // `new p.X[2][2]` report the wrong loader's element type.
            let comp_desc = format!("{}{}", "[".repeat(comp_brackets), leaf_desc);
            resolve_class_or_array_loader_aware(shared, thread, referencing_class_id, &comp_desc)
                .map_err(|e| {
                    // The site's class, not the top frame: the JIT's
                    // `jit_multianewarray_n` shares this function.
                    crate::runtime::exceptions::convert_class_not_found_for(
                        shared,
                        thread,
                        Some(referencing_class_id),
                        &array_class_name,
                        e,
                    )
                })
                .map_err(|e| {
                    record_resolution_failure_as_of(
                        shared,
                        referencing_class_id,
                        cp_index,
                        e,
                        fill_as_of,
                    )
                })?
        } else if leaf_is_reference {
            // Reference leaf — component is the element class itself.
            let comp_name = &leaf_desc[1..leaf_desc.len() - 1];
            resolve_class_loader_aware(shared, thread, referencing_class_id, comp_name)
                .map_err(|e| {
                    // The site's class, not the top frame: the JIT's
                    // `jit_multianewarray_n` shares this function.
                    crate::runtime::exceptions::convert_class_not_found_for(
                        shared,
                        thread,
                        Some(referencing_class_id),
                        &array_class_name,
                        e,
                    )
                })
                .map_err(|e| {
                    record_resolution_failure_as_of(
                        shared,
                        referencing_class_id,
                        cp_index,
                        e,
                        fill_as_of,
                    )
                })?
        } else {
            // Primitive leaf: element type carries the descriptor.
            ClassId::new(0)
        };
        if cid == ClassId::new(0) && (comp_brackets > 0 || leaf_is_reference) {
            fully_resolved = false;
        }
        component_ids.push(cid);
    }
    // JVMS §5.4.3: a failure another thread recorded for the entry meanwhile
    // is its outcome (interpreter round i1 wave 38, lane L5; `--real-jdk`).
    if let Some(recorded) =
        recorded_resolution_failure_after_success(shared, thread, referencing_class_id, cp_index)
    {
        return Err(recorded);
    }
    // JVMS §5.4.4 on the element class (i9-L2), before the plan is cached:
    // a cached plan is what every later execution, in both tiers, serves.
    if leaf_is_reference {
        constants::check_class_constant_access_as_of(
            shared,
            thread,
            (referencing_class_id, cp_index),
            &ncdfe_name,
            None,
            "multianewarray",
            fill_as_of,
        )?;
    }

    if fully_resolved {
        let mut plans = multianewarray_plan_cache().write();
        // Filed only while no redefinition began since the pool read
        // (`plan_as_of`), checked under the plan cache's own write lock,
        // which the redefinition's sweep takes after advancing the count:
        // either the sweep runs later and drops this plan, or the count has
        // moved and the plan (built from the replaced pool) is not filed.
        // Filing it after the sweep left a plan of the OLD pool's entry at an
        // index the new pool gives another class (interpreter round i1 wave
        // 44, lane L2; probe `L2W44CompiledMultiArrayAcrossRenumbering`). A
        // compatibility stub's upgrade replaces the pool without advancing
        // this count, and needs none: a stub has no bytecode (every method
        // `ACC_NATIVE`, no `Code`) and a one-tombstone pool, so no plan is
        // ever keyed on a stub or built from its pool (wave 45,
        // `interpreter-L2-a-stub-upgrade-can-leave-a-multianewarray-plan-of-the-replaced-pool-FIXED-20261009`).
        if crate::classloading::class_redefinition_count() == plan_as_of {
            plans.insert(
                cache_key,
                Arc::new(MultiANewArrayPlan {
                    leaf_et,
                    total_array_depth,
                    component_ids: component_ids.clone().into_boxed_slice(),
                }),
            );
        }
    }

    alloc_multi_array_collecting(
        shared,
        thread,
        sizes,
        leaf_et,
        total_array_depth,
        &component_ids,
    )
}

/// The component class of each of the `levels` allocated levels of a
/// `multianewarray` whose `CONSTANT_Class` entry `(referencing_class_id,
/// cp_index)` names the array class `array_class_name`, read off the
/// `array_info` chain of the class the ENTRY resolves to (JVMS §5.4.3.1: the
/// array class is resolved once, through its element; the intermediate array
/// classes are its components, not fresh resolutions). The entry's record
/// (`constants::recorded_class_entry`) answers first; else the entry is
/// resolved through the door the by-name loop's array levels use
/// ([`resolve_class_or_array_loader_aware`], which asks the loader for the
/// element at most once) and recorded, first wins. A failure is the
/// instruction's, converted and recorded as the loop converts and records
/// it. `Ok(None)`: a link of the chain has no `array_info` (an array class
/// minted before it was populated), so the caller resolves by name. Round 15
/// wave 5, lane classsite2. `record`: the referencing class is an isolated URL
/// loader's (`constants::class_entry_record_applies`), whose entry keeps a
/// durable record; for every other class (JIT round 16, lane classsite,
/// `CRATONVM_MULTIANEWARRAY_ENTRY_WALK`) the entry is resolved and walked
/// without one, as `new` / casts resolve such a class's entries.
pub(in crate::runtime::interpreter) fn multianewarray_entry_components(
    shared: &SharedVm,
    thread: &mut JvmThread,
    (referencing_class_id, cp_index): (ClassId, u16),
    array_class_name: &str,
    (levels, record): (usize, bool),
    fill_as_of: u64,
) -> Result<Option<Vec<ClassId>>, MethodCallFailed> {
    let recorded = if record {
        constants::recorded_class_entry(shared, referencing_class_id, cp_index)
    } else {
        None
    };
    let full = match recorded {
        Some(id) => id,
        None => {
            let resolved = resolve_class_or_array_loader_aware(
                shared,
                thread,
                referencing_class_id,
                array_class_name,
            )
            .map_err(|e| {
                crate::runtime::exceptions::convert_class_not_found_for(
                    shared,
                    thread,
                    Some(referencing_class_id),
                    array_class_name,
                    e,
                )
            })
            .map_err(|e| {
                record_resolution_failure_as_of(
                    shared,
                    referencing_class_id,
                    cp_index,
                    e,
                    fill_as_of,
                )
            })?;
            if !record {
                resolved
            } else {
                // A racing resolver's earlier record is the entry's answer.
                let resolver = crate::runtime::resolve::MemberResolver::new(shared);
                resolver
                    .record_class_entry_as_of(
                        resolver.scope(referencing_class_id),
                        cp_index,
                        resolved,
                        fill_as_of,
                    )
                    .unwrap_or(resolved)
            }
        }
    };
    let cm = shared.classes.class_manager.read();
    let mut ids = Vec::with_capacity(levels);
    let mut current = full;
    for _ in 0..levels {
        let Some(component) = cm
            .get_class(current)
            .and_then(|class| class.array_info.as_ref())
            .map(|info| info.component_class_id)
            .filter(|id| *id != ClassId::new(0))
        else {
            return Ok(None);
        };
        ids.push(component);
        current = component;
    }
    Ok(Some(ids))
}


/// `alloc_multi_array` with the allocation-failure ladder every other
/// allocation site has: forced collection, retry, last-ditch reclaim, retry,
/// heap dump, `OutOfMemoryError`.
///
/// gen r4w3/hunter (2026-09-23): `alloc_multi_array` deliberately never
/// collects — it holds the dimension arrays it has already built in Rust
/// locals across its recursion — so `new long[a][b]` threw `OutOfMemoryError`
/// the first time the young generation (and, through the `_full` spill, the
/// old generation) had no room, on a heap that could be almost entirely
/// garbage, and without the `-XX:+HeapDumpOnOutOfMemoryError` dump. The retry
/// has to happen HERE, above the recursion: once `alloc_multi_array` has
/// returned its error every partially built level is unreachable garbage, and
/// the only live state is `sizes` (plain integers) and the class ids. Both
/// callers (the interpreter's `multianewarray` arm and the JIT's
/// `jit_multianewarray_n`) already reach a collection through this function —
/// component-class resolution can run a user `ClassLoader` — so a collection
/// here is not a new kind of event for either.
///
/// Only a HEAP-exhaustion `OutOfMemoryError` is retried; every other error is
/// returned as-is. See
/// `docs/feature-designs/generative-gc-architecture.md`.
///
/// gc-common w3-c: the escalation is `gc_and_alloc::collect_and_retry`, the one
/// every other interpreter ladder shares, instead of a transcription of it. The
/// transcription had drifted in two ways. (1) It retried
/// `Requested array size exceeds VM limit` too -- that refusal is also an
/// `OutOfMemoryError`, so `new int[Integer.MAX_VALUE][1]` ran a forced
/// collection and a last-ditch cycle before giving HotSpot's answer, which
/// HotSpot (`ObjArrayKlass::multi_allocate`) and `gc_alloc_array` both give
/// before any collection. (2) On the GC-overhead limit it returned at once,
/// skipping the rung that clears every `SoftReference` before an
/// `OutOfMemoryError` (`last_ditch_clear_soft_refs`), which the shared ladder
/// runs. It also now retires the TLAB and flushes the allocation batch before
/// collecting, as the other ladders do.
pub(in crate::runtime::interpreter) fn alloc_multi_array_collecting(
    shared: &SharedVm,
    thread: &mut JvmThread,
    sizes: &[usize],
    leaf_et: ArrayElementType,
    total_array_depth: usize,
    component_ids: &[ClassId],
) -> Result<ObjectRef, MethodCallFailed> {
    fn is_heap_oom(r: &Result<ObjectRef, MethodCallFailed>) -> bool {
        matches!(
            r,
            Err(MethodCallFailed::InternalError(VmError::Runtime(
                RuntimeError::OutOfMemoryError { message }
            ))) if message.as_str() != ARRAY_SIZE_EXCEEDS_VM_LIMIT
        )
    }
    // zr z2/a2: the first attempt carries the executing thread, so on ZGC each
    // level that fits a TLAB is bumped from its VM TLAB (see
    // `multi_array_level`). The retry below keeps the thread-less path: it is
    // the rare arm, and `collect_and_retry` holds the thread.
    let first = alloc_multi_array_on(
        shared,
        Some(&mut *thread),
        sizes,
        0,
        leaf_et,
        total_array_depth,
        component_ids,
    );
    if !is_heap_oom(&first) {
        // gen r4w4/oom (2026-09-24): the VM-limit refusal is an
        // `OutOfMemoryError` too, and HotSpot's `report_java_out_of_memory`
        // (which `check_array_allocation_length` calls before throwing) writes
        // the `-XX:+HeapDumpOnOutOfMemoryError` dump for it, as
        // `gc_alloc_array`'s refusal arm already does here. No collection.
        if let Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::OutOfMemoryError { message },
        ))) = &first
        {
            gc_and_alloc::maybe_dump_heap_on_oom_for(shared, thread, message);
        }
        return first;
    }
    // The failed attempt's partial tree is unreachable (only Rust locals held
    // it, and no root set sees them), so the collection reclaims it and the
    // retry rebuilds the whole tree from `sizes` and the class ids.
    let retried = gc_and_alloc::collect_and_retry(shared, thread, "multianewarray", |s| {
        let attempt = alloc_multi_array(s, sizes, 0, leaf_et, total_array_depth, component_ids);
        // `None` asks the ladder for its next rung; anything else -- success
        // or a non-heap error -- ends it.
        if is_heap_oom(&attempt) {
            None
        } else {
            Some(attempt)
        }
    });
    retried.unwrap_or_else(|| {
        gc_and_alloc::maybe_dump_heap_on_oom(shared, thread);
        first
    })
}

/// Maximum recursion depth for multianewarray to prevent stack overflow.
/// The JVM spec allows at most 255 dimensions, but we cap at 255 to be safe.
pub(in crate::runtime::interpreter) const MAX_MULTI_ARRAY_DEPTH: usize = 255;

/// `CRATONVM_ZGC_MULTIARRAY_TLAB` (zr z2/a2, **default ON**; `0` / `off` /
/// `false` / `no` restores the per-sub-array `try_alloc_array_full`): see
/// [`multi_array_level`]. A performance switch, not compatibility state. gci
/// i27/kd: read once per VM (`SharedVm::zgc_multiarray_tlab`); it was a
/// process `OnceLock` (the per-VM statics ratchet).
pub(in crate::runtime::interpreter) fn zgc_multiarray_tlab_enabled(shared: &SharedVm) -> bool {
    shared.zgc_multiarray_tlab
}

/// Allocate one level of a `multianewarray` tree.
///
/// # zr z2/a2: the VM TLAB on ZGC
///
/// Every sub-array used to be one `VmHeap::try_alloc_array_full`. On ZGC that
/// is `ZgcRealHeap::try_alloc_array` -> `alloc_raw_tlab`: the heap's OWN TLAB
/// cell (a thread-local handle lookup, a `Mutex` lock, a start-bit `fetch_or`
/// and an `allocated.fetch_add` on the cache line every allocating thread
/// shares) -- per sub-array. `new int[a][3]` is `a` of those, and
/// `Rbc6ArrayAllocProbe`'s `multianewarrayOuter` row asks for ~2 x 10^10 of
/// them at its default `n`. The interpreter's and the JIT's own `newarray`
/// take the thread's VM TLAB instead (`gc_and_alloc::tlab_alloc_array`: a
/// plain bump, the start bit announced through `note_tlab_object`, the bytes
/// charged once per chunk), so this does the same for each level that fits a
/// TLAB, and falls back to the old call for the rest (a level above
/// `tlab_max_alloc`, a refused refill).
///
/// Why this is safe here: `tlab_alloc_array` refills with
/// `refill_needs_young_room = false`, i.e. it NEVER collects -- the contract
/// this recursion needs, because it holds the levels already built in Rust
/// locals no root set sees (the same contract `try_alloc_array_full` honours
/// and the native array door relies on). The thread is the executing one
/// (the interpreter's, or `jit_thread_mut()`'s for the compiled helper), which
/// is what `refill_tlab`'s owned-chunk record assumes.
///
/// Side effect, toward HotSpot: a level served this way is charged to the
/// thread's allocated-bytes counter by the TLAB cursor
/// (`getThreadAllocatedBytes`); the `try_alloc_array_full` arm charges no
/// thread counter at all.
///
/// ZGC only, so Generational and G1 are unchanged byte for byte.
pub(in crate::runtime::interpreter) fn multi_array_level(
    shared: &SharedVm,
    thread: Option<&mut JvmThread>,
    class_id: ClassId,
    element_type: ArrayElementType,
    length: usize,
) -> Option<ObjectRef> {
    if let Some(thread) = thread {
        if shared.mem.heap.is_zgc()
            && zgc_multiarray_tlab_enabled(shared)
        {
            if let Some(arr) =
                gc_and_alloc::tlab_alloc_array(thread, shared, class_id, element_type, length)
            {
                return Some(arr);
            }
        }
    }
    shared
        .mem
        .heap
        .try_alloc_array_full(class_id, element_type, length)
}

pub(in crate::runtime::interpreter) fn alloc_multi_array(
    shared: &SharedVm,
    sizes: &[usize],
    depth: usize,
    leaf_et: ArrayElementType,
    total_array_depth: usize,
    component_ids: &[ClassId],
) -> Result<ObjectRef, MethodCallFailed> {
    alloc_multi_array_on(
        shared,
        None,
        sizes,
        depth,
        leaf_et,
        total_array_depth,
        component_ids,
    )
}

/// [`alloc_multi_array`] with the executing thread, when the caller has it
/// (zr z2/a2; see [`multi_array_level`]).
pub(in crate::runtime::interpreter) fn alloc_multi_array_on(
    shared: &SharedVm,
    mut thread: Option<&mut JvmThread>,
    sizes: &[usize],
    depth: usize,
    leaf_et: ArrayElementType,
    total_array_depth: usize,
    component_ids: &[ClassId],
) -> Result<ObjectRef, MethodCallFailed> {
    // Component class id for the array allocated at this depth (so it carries
    // its precise array class). Falls back to `ClassId(0)` when unresolved.
    let level_class_id = component_ids.get(depth).copied().unwrap_or(ClassId::new(0));
    if depth >= MAX_MULTI_ARRAY_DEPTH {
        return Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::NotImplemented {
                feature: format!(
                    "multianewarray: dimension depth {} exceeds maximum {}",
                    depth, MAX_MULTI_ARRAY_DEPTH
                ),
            },
        )));
    }

    let length = sizes[depth];

    // HotSpot's array-length limit, checked per dimension as it is allocated
    // (`ObjArrayKlass::multi_allocate` -> `check_array_allocation_length`), so
    // an inner dimension under a zero-length outer one is never checked --
    // exactly as here, where the loop below never reaches it. Refused with
    // HotSpot's own message rather than attempted and reported as
    // "Java heap space" (gc-common w2-d, the `multianewarray` arm of
    // `common-f-array-vm-limit-outside-interpreter`; `newarray` got the same
    // check in w1-f, see `array_length_exceeds_vm_limit`).
    if array_length_exceeds_vm_limit(length) {
        return Err(MethodCallFailed::InternalError(VmError::Runtime(
            RuntimeError::OutOfMemoryError {
                message: ARRAY_SIZE_EXCEEDS_VM_LIMIT.to_string(),
            },
        )));
    }

    if depth == sizes.len() - 1 {
        // Innermost SPECIFIED dimension. Per JVM spec for multianewarray, the
        // array type descriptor can have more leading `[` than the `dimensions`
        // operand; unspecified inner dimensions are left null/uninitialized.
        // So the last-allocated array's element type is the descriptor's leaf
        // type ONLY when `sizes.len() == total_array_depth`. Otherwise the
        // element type must be Reference (each slot would hold an array-of-
        // arrays-of-...-of-leaf that we are NOT instantiating here).
        //
        // Example: `multianewarray [[[[C, 3` (descriptor depth=4, sizes.len()=3)
        // → outer Ref[N], mid Ref[N], inner Ref[N] (null slots). Using `Char`
        // here would build a `char[N]` leaf and crash on the subsequent
        // `aaload`/`aastore` that walks the unallocated 4th dim
        // (e.g. Eclipse ecj `CharDeduplication.charArray_length`).
        let element_type = if sizes.len() == total_array_depth {
            leaf_et
        } else {
            ArrayElementType::Reference
        };
        // SB-LOADER-ZIPCONTENT (2026-08-04): `_full`, not the young-only
        // variant. `alloc_multi_array` holds already-allocated dimension arrays
        // in Rust locals across the recursion, so it deliberately never forces
        // a GC — which leaves it with no second chance at all. Spilling into
        // old gen is that second chance, and it relocates nothing (see
        // `try_alloc_array_humongous`), so it is safe from exactly here.
        let level = thread.as_deref_mut();
        let arr = multi_array_level(shared, level, level_class_id, element_type, length)
            .ok_or_else(|| {
                MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::OutOfMemoryError {
                    message: format!(
                        "Java heap space (multianewarray leaf dim, length={})",
                        length
                    ),
                }))
            })?;
        Ok(arr)
    } else {
        // Intermediate dimensions: always Reference (array of arrays)
        // `_full` for the same reason as the leaf dimension above.
        let level = thread.as_deref_mut();
        let element_type = ArrayElementType::Reference;
        let arr = multi_array_level(shared, level, level_class_id, element_type, length)
            .ok_or_else(|| {
                MethodCallFailed::InternalError(VmError::Runtime(RuntimeError::OutOfMemoryError {
                    message: format!(
                        "Java heap space (multianewarray dim {}, length={})",
                        depth, length
                    ),
                }))
            })?;
        for i in 0..length {
            let sub_array = alloc_multi_array_on(
                shared,
                thread.as_deref_mut(),
                sizes,
                depth + 1,
                leaf_et,
                total_array_depth,
                component_ids,
            )?;
            shared
                .mem
                .heap
                .set_array_element(arr, i, Value::Object(Some(sub_array)))
                .map_err(|idx| {
                    RuntimeError::array_store_fault(idx, shared.mem.heap.array_length(arr) as i32)
                })?;
        }
        Ok(arr)
    }
}

// -- `ireturn` narrowing (JVMS §6.5 ireturn) --

/// Narrow an `ireturn` value to the method's declared return type: `& 1` for
/// `boolean`, sign-extending `i2b` / `i2s` for `byte` / `short`, zero-extending
/// `i2c` for `char`; the identity for every other tag. The baseline JIT's
/// `emit_narrow_int_return` applies the same four rules.
#[inline]
pub(in crate::runtime::interpreter) fn narrow_ireturn(v: i32, ret_tag: u8) -> i32 {
    match ret_tag {
        b'Z' => v & 1,
        b'B' => v as i8 as i32,  // Cast: JVMS i2b narrowing
        b'C' => v as u16 as i32, // Cast: JVMS i2c narrowing
        b'S' => v as i16 as i32, // Cast: JVMS i2s narrowing
        _ => v,
    }
}

/// [`narrow_ireturn`] over a decoded return `Value`; non-int values pass
/// through unchanged.
#[inline]
pub(in crate::runtime::interpreter) fn narrow_ireturn_value(v: Value, ret_tag: u8) -> Value {
    match v {
        Value::Int(i) => Value::Int(narrow_ireturn(i, ret_tag)),
        other => other,
    }
}

// -- Float/double to int/long conversions (JVM spec 2.8.3) --

pub(in crate::runtime::interpreter) fn float_to_int(v: f32) -> i32 {
    if v.is_nan() {
        0
    // JVM spec: f2i bounds check
    } else if v >= i32::MAX as f32 {
        i32::MAX
    // JVM spec: f2i bounds check
    } else if v <= i32::MIN as f32 {
        i32::MIN
    } else {
        v as i32 // JVM spec: bounded float-to-int conversion
    }
}

pub(in crate::runtime::interpreter) fn float_to_long(v: f32) -> i64 {
    if v.is_nan() {
        0
    // JVM spec: f2l bounds check
    } else if v >= i64::MAX as f32 {
        i64::MAX
    // JVM spec: f2l bounds check
    } else if v <= i64::MIN as f32 {
        i64::MIN
    } else {
        v as i64 // JVM spec: bounded float-to-long conversion
    }
}

/// JVMS `drem`, bit-exact with HotSpot. The value is C `fmod` (Rust `%`), but
/// an INVALID remainder — a finite or infinite dividend over `±0`, or an
/// infinite dividend — must be the x86 "default NaN" `0xFFF8_0000_0000_0000`
/// (sign bit set) that HotSpot's `drem` produces on both Windows and Linux
/// (the hardware result of `0/0`-style operations). The Windows CRT `fmod`
/// behind Rust's `%` answers the positive `0x7FF8…`, so
/// `Double.doubleToRawLongBits(1.0 % 0.0)` differed (interpreter round i4
/// wave 98, lane L7; probe `L7W98ArithCorners.java`).
/// A single NaN operand propagates as `%` does (both VMs agree there). Two
/// NaN operands: see below.
///
/// JIT round 22 wave 6b (lane loops; `docs/feature-designs/real-jdk-mode.md`):
/// a NaN operand is now answered explicitly by [`rem_nan_operand_bits`]
/// (HotSpot 25's table, `FpNaN2`): the result is always QUIET, and its sign is
/// cleared unless the divisor is the NaN and the dividend is finite (wave 7:
/// an infinite dividend clears it too). `%` returned a signalling
/// operand unquieted and kept a negative dividend's sign.
#[inline]
pub(crate) fn java_drem(a: f64, b: f64) -> f64 {
    if a.is_nan() || b.is_nan() {
        f64::from_bits(rem_nan_operand_bits(
            a.to_bits(),
            b.to_bits(),
            a.is_finite(),
            b.is_nan(),
            D_QUIET_BIT,
            D_SIGN_BIT,
        ))
    } else if a.is_infinite() || b == 0.0 {
        f64::from_bits(0xFFF8_0000_0000_0000)
    } else {
        a % b
    }
}

/// The quiet bit of an `f64` NaN.
pub(in crate::runtime::interpreter) const D_QUIET_BIT: u64 = 0x0008_0000_0000_0000;
/// The sign bit of an `f64`.
pub(in crate::runtime::interpreter) const D_SIGN_BIT: u64 = 0x8000_0000_0000_0000;
/// The quiet bit of an `f32` NaN (widened).
pub(in crate::runtime::interpreter) const F_QUIET_BIT: u64 = 0x0040_0000;
/// The sign bit of an `f32` (widened).
pub(in crate::runtime::interpreter) const F_SIGN_BIT: u64 = 0x8000_0000;

/// HotSpot 25's `drem` / `frem` with a NaN operand (x86_64, measured on
/// Windows, `FpNaN2` every tier identical; consistent with every Linux row of
/// `L7W102DoubleArithSemantics`, whose NaNs were all positive and quiet): the
/// divisor's NaN when it is one -- with its sign kept when the dividend is
/// FINITE, cleared when the dividend is infinite or a NaN too -- else the
/// dividend's NaN with its sign cleared; always quieted. (`(1.5 % -NaN)` and
/// `(-0.0 % -NaN)` are `-NaN`; `(Inf % -NaN)`, `(-NaN % 1.5)` and
/// `(NaN % -NaN)` are `+NaN` with the payload of the NaN chosen. The infinite
/// dividend row was corrected in wave 7 from the HotSpot 25.0.3 table, every
/// tier.) Bits widened to `u64` for both widths.
#[inline]
pub(in crate::runtime::interpreter) fn rem_nan_operand_bits(a: u64, b: u64, a_finite: bool, b_nan: bool, quiet: u64, sign: u64) -> u64 {
    if b_nan {
        let q = b | quiet;
        if a_finite {
            q
        } else {
            q & !sign
        }
    } else {
        (a | quiet) & !sign
    }
}

/// JIT round 22 wave 6b (lane loops; `r22-w6-nan-payload-parity-FIXED-20261020.md`):
/// the NaN a JVM `dadd` / `dsub` / `dmul` / `ddiv` (and the `float` twins)
/// returns when an operand is a NaN, as HotSpot 25's INTERPRETER returns it
/// (`FpNaN2`): x86 SSE answers its DESTINATION operand's NaN, quieted, when
/// both are NaNs, and HotSpot places value2 there for `add` / `mul` and
/// value1 for `sub` / `div` (its C2 keeps value1 for all four, which our
/// compiled code follows unless `CRATONVM_JIT_FP_NAN_OPERAND_ORDER=1`).
/// So: the preferred operand's NaN if it is one,
/// else the other's, quieted. `r` (the hardware result) is returned unchanged
/// when it is not a NaN, or when neither operand is (an invalid operation's
/// default NaN). Rust's `a + b` leaves the order to LLVM, which commuted the
/// `float` arms (and the out-of-line double copy) the other way.
#[inline(always)]
pub(in crate::runtime::interpreter) fn nan_operand_result(r: u64, r_nan: bool, pref: (u64, bool), other: (u64, bool), quiet: u64) -> u64 {
    if !r_nan {
        r
    } else if pref.1 {
        pref.0 | quiet
    } else if other.1 {
        other.0 | quiet
    } else {
        r
    }
}

/// `dadd` / `dmul` (`commutative`: value2's NaN first) or `dsub` / `ddiv`
/// (value1's NaN first), with HotSpot's NaN-operand rule ([`nan_operand_result`]).
#[inline(always)]
pub(crate) fn java_d_nan_rule(r: f64, a: f64, b: f64, commutative: bool) -> f64 {
    if !r.is_nan() {
        return r;
    }
    let (pa, pb) = ((a.to_bits(), a.is_nan()), (b.to_bits(), b.is_nan()));
    let (pref, other) = if commutative { (pb, pa) } else { (pa, pb) };
    f64::from_bits(nan_operand_result(r.to_bits(), true, pref, other, D_QUIET_BIT))
}

/// The `float` twin of [`java_d_nan_rule`].
#[inline(always)]
pub(crate) fn java_f_nan_rule(r: f32, a: f32, b: f32, commutative: bool) -> f32 {
    if !r.is_nan() {
        return r;
    }
    let pa = (u64::from(a.to_bits()), a.is_nan());
    let pb = (u64::from(b.to_bits()), b.is_nan());
    let (pref, other) = if commutative { (pb, pa) } else { (pa, pb) };
    let bits = nan_operand_result(u64::from(r.to_bits()), true, pref, other, F_QUIET_BIT);
    // Cast: a `float`'s bits, widened above.
    f32::from_bits(bits as u32)
}

#[inline(always)]
pub(crate) fn java_dadd(a: f64, b: f64) -> f64 {
    java_d_nan_rule(a + b, a, b, true)
}
#[inline(always)]
pub(crate) fn java_dsub(a: f64, b: f64) -> f64 {
    java_d_nan_rule(a - b, a, b, false)
}
#[inline(always)]
pub(crate) fn java_dmul(a: f64, b: f64) -> f64 {
    java_d_nan_rule(a * b, a, b, true)
}
#[inline(always)]
pub(crate) fn java_ddiv(a: f64, b: f64) -> f64 {
    java_d_nan_rule(a / b, a, b, false)
}
#[inline(always)]
pub(crate) fn java_fadd(a: f32, b: f32) -> f32 {
    java_f_nan_rule(a + b, a, b, true)
}
#[inline(always)]
pub(crate) fn java_fsub(a: f32, b: f32) -> f32 {
    java_f_nan_rule(a - b, a, b, false)
}
#[inline(always)]
pub(crate) fn java_fmul(a: f32, b: f32) -> f32 {
    java_f_nan_rule(a * b, a, b, true)
}
#[inline(always)]
pub(crate) fn java_fdiv(a: f32, b: f32) -> f32 {
    java_f_nan_rule(a / b, a, b, false)
}

/// JVMS `frem`, the `float` twin of [`java_drem`]: an invalid remainder is
/// the default NaN `0xFFC0_0000` (HotSpot measured; Rust's `f32 %` already
/// answers it on the Windows host, this pins it on every target).
///
/// A NaN operand is answered explicitly, on every target: the divisor's NaN
/// when it is one, else the dividend's (HotSpot on Windows and Linux, build
/// w102d `L7W102DoubleArithSemantics` rows `Infinity NaN/7fc01234` and
/// `NaN/7fc00000 NaN/7fc01234`). The Windows CRT `fmodf` behind `f32 %`
/// answered the default NaN `0xFFC0_0000` for `Infinity % NaN` and the
/// dividend for two NaNs; glibc's already matched this rule.
///
/// JIT round 22 wave 6b: the NaN operand's bits follow [`rem_nan_operand_bits`]
/// (quieted; the sign kept only for a NaN divisor over a finite dividend), which keeps
/// every w102d row above and adds HotSpot's quieting and sign rule.
#[inline]
pub(crate) fn java_frem(a: f32, b: f32) -> f32 {
    if a.is_nan() || b.is_nan() {
        let bits = rem_nan_operand_bits(
            u64::from(a.to_bits()),
            u64::from(b.to_bits()),
            a.is_finite(),
            b.is_nan(),
            F_QUIET_BIT,
            F_SIGN_BIT,
        );
        // Cast: a `float`'s bits, widened above.
        f32::from_bits(bits as u32)
    } else if a.is_infinite() || b == 0.0 {
        f32::from_bits(0xFFC0_0000)
    } else {
        a % b
    }
}

pub(in crate::runtime::interpreter) fn double_to_int(v: f64) -> i32 {
    if v.is_nan() {
        0
    // JVM spec: d2i bounds check
    } else if v >= i32::MAX as f64 {
        i32::MAX
    // JVM spec: d2i bounds check
    } else if v <= i32::MIN as f64 {
        i32::MIN
    } else {
        v as i32 // JVM spec: bounded float-to-int conversion
    }
}

pub(in crate::runtime::interpreter) fn double_to_long(v: f64) -> i64 {
    if v.is_nan() {
        0
    // JVM spec: d2l bounds check
    } else if v >= i64::MAX as f64 {
        i64::MAX
    // JVM spec: d2l bounds check
    } else if v <= i64::MIN as f64 {
        i64::MIN
    } else {
        v as i64 // JVM spec: bounded float-to-long conversion
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

/// DBG (`CRATONVM_DBG_IMSE`): dump the complete `ReentrantReadWriteLock$Sync`
/// read-hold bookkeeping at the moment an `IllegalMonitorStateException` is
/// thrown from `tryReleaseShared` — names WHICH invariant broke (firstReader
/// identity, cachedHoldCounter tid/count, or the current thread's `readHolds`
/// ThreadLocalMap entry) for the ES `testAllEqual` hold-count-loss face.
pub(in crate::runtime::interpreter) fn dump_imse_holdcount_state(shared: &SharedVm, thread: &JvmThread, exc: ObjectRef) {
    let exc_class = {
        let cm = shared.classes.class_manager.read();
        cm.get_class(shared.mem.heap.class_id_of(exc))
            .map(|c| c.name.to_string())
            .unwrap_or_default()
    };
    if !exc_class.contains("IllegalMonitorStateException") {
        return;
    }
    // Find the tryReleaseShared frame; local 0 is the Sync receiver.
    let sync = thread.frames.iter().rev().find_map(|f| {
        if f.method_name() == "tryReleaseShared" && f.class_name().contains("ReadWriteLock") {
            match f.get_local(0) {
                Value::Object(Some(o)) => Some(o),
                _ => None,
            }
        } else {
            None
        }
    });
    let Some(sync) = sync else {
        eprintln!("[imse] IllegalMonitorStateException thrown but no tryReleaseShared frame");
        return;
    };
    let cm = shared.classes.class_manager.read();
    let field = |obj: ObjectRef, name: &str| -> Value {
        let cid = shared.mem.heap.class_id_of(obj);
        match crate::vm::vm_exec::resolve_field_index_in_hierarchy(cid, name, &cm.class_store) {
            Some(idx) => shared.mem.heap.get_field(obj, idx),
            None => Value::Uninitialized,
        }
    };
    let me = thread.java_thread_obj;
    let first_reader = field(sync, "firstReader");
    let first_count = field(sync, "firstReaderHoldCount");
    let cached = field(sync, "cachedHoldCounter");
    let read_holds = field(sync, "readHolds");
    eprintln!(
        "[imse] tid={} me={:?} sync={:p} firstReader={:?} firstReaderHoldCount={:?}",
        thread.thread_id.0,
        me.map(|o| o.as_ptr()),
        sync.as_ptr(),
        first_reader,
        first_count,
    );
    if let Value::Object(Some(rh)) = cached {
        eprintln!(
            "[imse]   cachedHoldCounter={:p} count={:?} tid={:?}",
            rh.as_ptr(),
            field(rh, "count"),
            field(rh, "tid"),
        );
    } else {
        eprintln!("[imse]   cachedHoldCounter={cached:?}");
    }
    // Walk the CURRENT thread's ThreadLocalMap for the readHolds entry.
    let Value::Object(Some(read_holds)) = read_holds else {
        eprintln!("[imse]   readHolds={read_holds:?} (field missing?)");
        return;
    };
    let Some(me) = me else {
        eprintln!("[imse]   no java_thread_obj for current thread");
        return;
    };
    let tl_map = field(me, "threadLocals");
    let Value::Object(Some(tl_map)) = tl_map else {
        eprintln!(
            "[imse]   readHolds={:p} but thread has NO threadLocals map",
            read_holds.as_ptr()
        );
        return;
    };
    let table = field(tl_map, "table");
    let Value::Object(Some(table)) = table else {
        eprintln!(
            "[imse]   threadLocals map {:p} has NO table",
            tl_map.as_ptr()
        );
        return;
    };
    let len = shared.mem.heap.array_length(table);
    let mut found = false;
    for i in 0..len {
        let Ok(Value::Object(Some(entry))) = shared.mem.heap.get_array_element(table, i) else {
            continue;
        };
        // Entry extends WeakReference<ThreadLocal>; referent is field 0.
        let referent = shared.mem.heap.get_field(entry, 0);
        let is_ours =
            matches!(referent, Value::Object(Some(r)) if r.as_ptr() == read_holds.as_ptr());
        if is_ours {
            found = true;
            let value = field(entry, "value");
            let vdesc = if let Value::Object(Some(hc)) = value {
                format!(
                    "HoldCounter@{:p} count={:?} tid={:?}",
                    hc.as_ptr(),
                    field(hc, "count"),
                    field(hc, "tid"),
                )
            } else {
                format!("{value:?}")
            };
            eprintln!(
                "[imse]   readHolds={:p}: table[{i}] entry={:p} value={vdesc}",
                read_holds.as_ptr(),
                entry.as_ptr(),
            );
        }
    }
    if !found {
        eprintln!(
            "[imse]   readHolds={:p}: NO ThreadLocalMap entry in current thread's table (len={len}) — the entry is GONE",
            read_holds.as_ptr(),
        );
    }
}

