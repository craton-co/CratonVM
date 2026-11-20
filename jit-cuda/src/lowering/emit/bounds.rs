use super::*;

pub(super) fn int_param_index_for_local(sig: &KernelSignature, slot: u16) -> Option<u32> {
    let mut s = 0u16;
    for (i, k) in sig.param_kinds.iter().enumerate() {
        if s == slot {
            return match k {
                ParamKind::I32 => u32::try_from(i).ok(),
                _ => None,
            };
        }
        s += match k {
            ParamKind::I64 | ParamKind::F64 => 2,
            ParamKind::Void => 0,
            _ => 1,
        };
    }
    None
}

/// Whether any instruction in the method writes local `slot`.
///
/// Deliberately whole-method and opcode-level rather than
/// reaching-definitions: the question is "is this local still the
/// incoming parameter value", and one store anywhere is enough to make
/// the answer "not provably". `iinc` counts as a store.
pub(super) fn local_is_ever_stored(bytes: &[u8], slot: u16) -> Result<bool, LoweringError> {
    let mut pc = 0usize;
    while pc < bytes.len() {
        let op = bytes[pc];
        let size = instr_size(bytes, pc)?;
        if pc + size > bytes.len() {
            return Err(LoweringError::UnsupportedNode(format!(
                "instruction at pc={pc} runs past the end of the bytecode \
                 ({} bytes) - truncated Code attribute",
                bytes.len()
            )));
        }
        // `(base slot, how many slots it covers)`. A `lstore`/`dstore`
        // writes TWO slots, and a later method body is free to reuse a
        // dead parameter's slot as the upper half of one - so counting
        // only the named slot would report "never stored" for a slot
        // that is in fact overwritten.
        let stored: Option<(u16, u16)> = match op {
            // istore / fstore / astore
            0x36 | 0x38 | 0x3A => bytes.get(pc + 1).map(|b| (*b as u16, 1)),
            // lstore / dstore
            0x37 | 0x39 => bytes.get(pc + 1).map(|b| (*b as u16, 2)),
            // istore_<n>, fstore_<n>, astore_<n>
            0x3B..=0x3E | 0x43..=0x46 | 0x4B..=0x4E => Some((((op - 0x3B) % 4) as u16, 1)),
            // lstore_<n>, dstore_<n>
            0x3F..=0x42 | 0x47..=0x4A => Some((((op - 0x3B) % 4) as u16, 2)),
            // iinc
            0x84 => bytes.get(pc + 1).map(|b| (*b as u16, 1)),
            // wide istore/fstore/astore and wide iinc: one slot
            0xC4 if matches!(
                bytes.get(pc + 1),
                Some(0x36) | Some(0x38) | Some(0x3A) | Some(0x84)
            ) =>
            {
                Some((u16::from_be_bytes([bytes[pc + 2], bytes[pc + 3]]), 1))
            }
            // wide lstore/dstore: two
            0xC4 if matches!(bytes.get(pc + 1), Some(0x37) | Some(0x39)) => {
                Some((u16::from_be_bytes([bytes[pc + 2], bytes[pc + 3]]), 2))
            }
            _ => None,
        };
        if let Some((base, width)) = stored {
            if slot >= base && slot - base < width {
                return Ok(true);
            }
        }
        pc += size;
    }
    Ok(false)
}

pub(super) fn param_len_for_local(
    sig: &KernelSignature,
    slot: u16,
) -> Result<BoundSource, LoweringError> {
    let mut s = 0u16;
    for (i, k) in sig.param_kinds.iter().enumerate() {
        if s == slot {
            if !k.is_array() {
                return Err(LoweringError::UnsupportedNode(
                    "arraylength of a non-array parameter".into(),
                ));
            }
            return Ok(BoundSource::ParamLen(i));
        }
        s += match k {
            ParamKind::I64 | ParamKind::F64 => 2,
            ParamKind::Void => 0,
            _ => 1,
        };
    }
    Err(LoweringError::UnsupportedNode(format!(
        "arraylength source local {slot} is not a parameter"
    )))
}

/// Where the loop bound comes from.
#[derive(Clone, Debug)]
pub(crate) enum BoundSource {
    /// `pN_len` for parameter index N.
    ParamLen(usize),
    /// A literal compile-time constant.
    Literal(i32),
    /// `pN` - an unmodified `int` parameter used directly as the bound.
    /// See [`crate::emitter::WorkBound::ParamScalar`] for the obligation
    /// this one puts on the host that the other two do not.
    ParamScalar(u32),
}
