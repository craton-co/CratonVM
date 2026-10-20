//! Interpreter round i6 wave 132, lane L6: stage 1 of
//! `docs/internal/proposals/interpreter/i40-L2-proposal-guard-a-pinned-private-interface-call-instead-of-refusing-the-method-20261004.md`.
//!
//! An `invokeinterface` whose resolved method is a PRIVATE method of an
//! interface selects that method, but only for a receiver that implements the
//! interface (JVMS §6.5: otherwise `IncompatibleClassChangeError`). No compiled
//! route checks the receiver, so with the selection checks in force (both
//! modes by default, `HotSpotAlignment::SelectionChecks`) the method-entry and
//! OSR doors refuse every method that holds such a site
//! (`invokeinterface-private-receiver-unchecked`). javac emits the shape for a
//! default method calling its interface's private helper; a static census of
//! the JDK 25 runtime image finds 14 sites in 13 methods, all of that shape:
//! `SegmentAllocator.allocateFrom` (ten overloads, `allocateNoInit`) and
//! `MatchResult.start/end/group(String)` (`groupNumber`). They ran interpreted
//! for the life of the VM.
//!
//! The proof: in an instance method of interface `I` whose local 0 is never
//! stored, local 0 is always the receiver the method was invoked on, and that
//! receiver implements `I` (selection of a default method starts from the
//! receiver's class; an `invokespecial` / `I.super.m()` receiver is the
//! caller's `this`; a private method is reached only by these from inside
//! `I`). So a site whose receiver is pushed by `aload_0`, calling a private
//! method declared by `I` itself, needs no receiver check, and the pinned
//! direct bind is exact. The receiver push is found by walking back from the
//! invoke over the argument words, through a short list of pure,
//! straight-line instructions, with no branch target or handler start between
//! the push and the invoke; anything else answers "not proven" and the
//! refusal stands.
//!
//! `CRATONVM_JIT_PRIVATE_IFACE_THIS_PIN` (`runtime_flag_on`, default OFF,
//! performance only). Off: one environment read per refused compile (the
//! refusal is bail-listed, so once per method).

/// Is the stage-1 proof switched on?
pub(super) fn private_iface_this_pin_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_PRIVATE_IFACE_THIS_PIN")
}

/// The longest run of instructions walked back from an invoke.
const MAX_WINDOW: usize = 32;

/// Argument words of a method descriptor, the receiver excluded (`long` and
/// `double` take two). `None` for a malformed descriptor.
fn arg_words(desc: &str) -> Option<usize> {
    let bytes = desc.as_bytes();
    if bytes.first() != Some(&b'(') {
        return None;
    }
    let mut i = 1usize;
    let mut words = 0usize;
    loop {
        match *bytes.get(i)? {
            b')' => return Some(words),
            b'J' | b'D' => {
                words += 2;
                i += 1;
            }
            b'B' | b'C' | b'F' | b'I' | b'S' | b'Z' => {
                words += 1;
                i += 1;
            }
            b'L' => {
                words += 1;
                i += desc[i..].find(';')? + 1;
            }
            b'[' => {
                while *bytes.get(i)? == b'[' {
                    i += 1;
                }
                if *bytes.get(i)? == b'L' {
                    i += desc[i..].find(';')? + 1;
                } else {
                    i += 1;
                }
                words += 1;
            }
            _ => return None,
        }
    }
}

fn u16_at(code: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*code.get(at)?, *code.get(at + 1)?]))
}

fn i32_at(code: &[u8], at: usize) -> Option<i32> {
    Some(i32::from_be_bytes([
        *code.get(at)?,
        *code.get(at + 1)?,
        *code.get(at + 2)?,
        *code.get(at + 3)?,
    ]))
}

/// Mark `pc + offset` a branch target; `None` when it leaves the code.
fn mark(targets: &mut [bool], pc: usize, offset: i64) -> Option<()> {
    let to = usize::try_from(pc as i64 + offset).ok()?;
    *targets.get_mut(to)? = true;
    Some(())
}

/// Does the instruction at `pc` store into local 0 (any type, `iinc`
/// included)? `wide` forms too.
fn stores_local_zero(code: &[u8], pc: usize) -> bool {
    match code[pc] {
        // istore, lstore, fstore, dstore, astore <index>
        0x36..=0x3a | 0x84 => code.get(pc + 1) == Some(&0),
        // istore_0, lstore_0, fstore_0, dstore_0, astore_0
        0x3b | 0x3f | 0x43 | 0x47 | 0x4b => true,
        0xc4 => {
            matches!(code.get(pc + 1), Some(0x36..=0x3a | 0x84)) && u16_at(code, pc + 2) == Some(0)
        }
        _ => false,
    }
}

/// `aload_0`, `aload 0` or `wide aload 0`.
fn is_aload_zero(code: &[u8], pc: usize) -> bool {
    match code[pc] {
        0x2a => true,
        0x19 => code.get(pc + 1) == Some(&0),
        0xc4 => code.get(pc + 1) == Some(&0x19) && u16_at(code, pc + 2) == Some(0),
        _ => false,
    }
}

/// `(words popped, words pushed)` of a pure straight-line instruction the
/// walk may step over; `None` for every other instruction.
fn stack_effect(code: &[u8], pc: usize) -> Option<(usize, usize)> {
    let op = code[pc];
    Some(match op {
        0x01..=0x08 | 0x0b..=0x0d | 0x10..=0x13 => (0, 1),
        0x09 | 0x0a | 0x0e | 0x0f | 0x14 => (0, 2),
        0x15 | 0x17 | 0x19 | 0x1a..=0x1d | 0x22..=0x25 | 0x2a..=0x2d => (0, 1),
        0x16 | 0x18 | 0x1e..=0x21 | 0x26..=0x29 => (0, 2),
        // add, sub, mul, div, rem: int, long, float, double
        0x60..=0x73 => match (op - 0x60) % 4 {
            0 | 2 => (2, 1),
            _ => (4, 2),
        },
        0x74 | 0x76 => (1, 1),
        0x75 | 0x77 => (2, 2),
        0x78 | 0x7a | 0x7c => (2, 1),
        0x79 | 0x7b | 0x7d => (3, 2),
        0x7e..=0x83 => {
            if (op - 0x7e) % 2 == 0 {
                (2, 1)
            } else {
                (4, 2)
            }
        }
        0x85 | 0x87 | 0x8c | 0x8d => (1, 2),
        0x86 | 0x8b | 0x91..=0x93 => (1, 1),
        0x88 | 0x89 | 0x8e | 0x90 => (2, 1),
        0x8a | 0x8f => (2, 2),
        // arraylength, checkcast
        0xbe | 0xc0 => (1, 1),
        0xc4 => match *code.get(pc + 1)? {
            0x15 | 0x17 | 0x19 => (0, 1),
            0x16 | 0x18 => (0, 2),
            _ => return None,
        },
        _ => return None,
    })
}

/// The instruction starts of `code`, and the branch-target / handler-start
/// marks (indexed by pc). `None` when local 0 is stored anywhere, the code
/// holds a `jsr` / `ret` (subroutine targets are not decoded), or a branch
/// leaves the code.
fn decode(code: &[u8], handler_pcs: &[u16]) -> Option<(Vec<usize>, Vec<bool>)> {
    let mut starts = Vec::new();
    let mut targets = vec![false; code.len() + 1];
    for &h in handler_pcs {
        *targets.get_mut(usize::from(h))? = true;
    }
    let mut pc = 0usize;
    while pc < code.len() {
        starts.push(pc);
        let op = code[pc];
        if stores_local_zero(code, pc) {
            return None;
        }
        match op {
            0xa8 | 0xa9 | 0xc9 => return None,
            0xc4 if code.get(pc + 1) == Some(&0xa9) => return None,
            0x99..=0xa7 | 0xc6 | 0xc7 => {
                mark(&mut targets, pc, i64::from(u16_at(code, pc + 1)? as i16))?;
            }
            0xc8 => mark(&mut targets, pc, i64::from(i32_at(code, pc + 1)?))?,
            0xaa | 0xab => {
                let base = pc + 1 + (4 - (pc + 1) % 4) % 4;
                mark(&mut targets, pc, i64::from(i32_at(code, base)?))?;
                if op == 0xaa {
                    let low = i64::from(i32_at(code, base + 4)?);
                    let high = i64::from(i32_at(code, base + 8)?);
                    let n = usize::try_from(high - low + 1).ok()?;
                    for k in 0..n {
                        mark(&mut targets, pc, i64::from(i32_at(code, base + 12 + 4 * k)?))?;
                    }
                } else {
                    let n = usize::try_from(i32_at(code, base + 4)?).ok()?;
                    for k in 0..n {
                        mark(&mut targets, pc, i64::from(i32_at(code, base + 12 + 8 * k)?))?;
                    }
                }
            }
            _ => {}
        }
        // The JIT's one decoder, as `CpResolvers::names_an_invoke_of` walks.
        pc += cratonvm_jit::bytecode_insn_len(code, pc).max(1);
    }
    Some((starts, targets))
}

/// Is the receiver of the `invokeinterface` at `starts[at]` pushed by an
/// `aload_0` in the same straight-line run?
fn receiver_pushed_by_aload_zero(
    code: &[u8],
    starts: &[usize],
    targets: &[bool],
    at: usize,
    words: usize,
) -> bool {
    // The receiver sits `depth` words below the top of the stack at the
    // instruction after `j`.
    let mut depth = words;
    let floor = at.saturating_sub(MAX_WINDOW);
    let mut j = at;
    while j > floor {
        // Control must reach `starts[j]` only from `starts[j - 1]`.
        if targets[starts[j]] {
            return false;
        }
        j -= 1;
        let Some((pop, push)) = stack_effect(code, starts[j]) else {
            return false;
        };
        if depth < push {
            return push == 1 && depth == 0 && is_aload_zero(code, starts[j]);
        }
        depth = depth - push + pop;
    }
    false
}

/// Stage 1's question for one method: is every `invokeinterface` naming
/// `cp_idx` in `code` (only the one at `only_pc`, when given) a call whose
/// receiver is local 0, with local 0 never stored? The caller has already
/// checked that the method is an instance method of the interface that
/// declares the private target. `callee_desc` is the target's descriptor.
pub(super) fn every_site_receiver_is_this(
    code: &[u8],
    handler_pcs: &[u16],
    cp_idx: u16,
    only_pc: Option<usize>,
    callee_desc: &str,
) -> bool {
    let Some(words) = arg_words(callee_desc) else {
        return false;
    };
    let Some((starts, targets)) = decode(code, handler_pcs) else {
        return false;
    };
    let mut sites = 0usize;
    for (at, &pc) in starts.iter().enumerate() {
        if code[pc] != 0xb9 || u16_at(code, pc + 1) != Some(cp_idx) {
            continue;
        }
        if only_pc.is_some_and(|only| only != pc) {
            continue;
        }
        if !receiver_pushed_by_aload_zero(code, &starts, &targets, at, words) {
            return false;
        }
        sites += 1;
    }
    sites != 0
}

/// Interpreter round i6 wave 133, lane L4 (page
/// `docs/known-issues/interpreter/i132-L6-interface-invokespecial-skips-the-receiver-subtype-check-20261230.md`):
/// the same proof for the `invokespecial` sites of one method of an
/// interface. Is the receiver of every `invokespecial` in `code` whose
/// constant-pool index satisfies `names_target` pushed by `aload_0`, with local
/// 0 never stored? `true` when `code` holds no such site; `false` for any site
/// of a STATIC method (`is_static`: local 0 is not a receiver there).
/// `callee_desc` is the target's descriptor. Such a receiver is the method's
/// own `this`, which implements the interface, so HotSpot's interface-sender
/// receiver check (`IllegalAccessError`, which no compiled route makes) cannot
/// fail at that site.
pub(super) fn every_special_site_receiver_is_this(
    code: &[u8],
    handler_pcs: &[u16],
    is_static: bool,
    callee_desc: &str,
    names_target: impl Fn(u16) -> bool,
) -> bool {
    let is_site = |pc: usize| code[pc] == 0xb7 && u16_at(code, pc + 1).is_some_and(&names_target);
    let mut pc = 0usize;
    let mut any = false;
    while pc < code.len() {
        if is_site(pc) {
            any = true;
            break;
        }
        pc += cratonvm_jit::bytecode_insn_len(code, pc).max(1);
    }
    if !any {
        return true;
    }
    if is_static {
        return false;
    }
    let Some(words) = arg_words(callee_desc) else {
        return false;
    };
    let Some((starts, targets)) = decode(code, handler_pcs) else {
        return false;
    };
    starts.iter().enumerate().all(|(at, &pc)| {
        !is_site(pc) || receiver_pushed_by_aload_zero(code, &starts, &targets, at, words)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Wave 133 (lane L4): the `invokespecial` form of the proof.
    #[test]
    fn special_sites_on_this_are_proven_and_on_an_argument_are_not() {
        let proven = |code: &[u8], is_static: bool, cp_idx: u16| {
            every_special_site_receiver_is_this(code, &[], is_static, "()Ljava/lang/String;", |cp| {
                cp == cp_idx
            })
        };
        // aload_0 invokespecial #7 priv()Ljava/lang/String; areturn
        let on_this = [0x2a, 0xb7, 0, 7, 0xb0];
        assert!(proven(&on_this, false, 7));
        // A static method's site is never proven.
        assert!(!proven(&on_this, true, 7));
        // Another constant-pool index: no site, nothing to prove.
        assert!(proven(&on_this, true, 8));
        // aload_1 invokespecial #7 areturn: the i132-L6 `special-bad` shape.
        let on_arg = [0x2b, 0xb7, 0, 7, 0xb0];
        assert!(!proven(&on_arg, false, 7));
        // aload_1 astore_0 first: local 0 is no longer `this`.
        let stored = [0x2b, 0x4b, 0x2a, 0xb7, 0, 7, 0xb0];
        assert!(!proven(&stored, false, 7));
    }

    /// `invokeinterface #cp, count, 0`.
    fn invokeinterface(cp: u16, count: u8) -> [u8; 5] {
        let [hi, lo] = cp.to_be_bytes();
        [0xb9, hi, lo, count, 0]
    }

    #[test]
    fn arg_words_counts_categories_and_arrays() {
        assert_eq!(arg_words("()I"), Some(0));
        assert_eq!(arg_words("(Ljava/lang/String;)I"), Some(1));
        assert_eq!(arg_words("(J)Ljava/lang/Object;"), Some(2));
        assert_eq!(arg_words("(Ljava/lang/foreign/MemoryLayout;J)V"), Some(3));
        assert_eq!(arg_words("([[J[Ljava/lang/Object;D)V"), Some(4));
        assert_eq!(arg_words("I"), None);
        assert_eq!(arg_words("(Ljava/lang/String"), None);
    }

    #[test]
    fn match_result_group_shape_is_proven() {
        // aload_0 aload_0 aload_1 invokeinterface #1 groupNumber(String)I
        // invokeinterface #2 start(I)I ireturn
        let mut code = vec![0x2a, 0x2a, 0x2b];
        code.extend_from_slice(&invokeinterface(1, 2));
        code.extend_from_slice(&invokeinterface(2, 2));
        code.push(0xac);
        assert!(every_site_receiver_is_this(&code, &[], 1, None, "(Ljava/lang/String;)I"));
        // The outer call's receiver is also `this`, but it is not asked.
        assert!(!every_site_receiver_is_this(&code, &[], 9, None, "()I"));
    }

    #[test]
    fn allocate_from_string_shape_is_proven() {
        // aload_0 iload 5 i2l iload_3 i2l ladd invokeinterface #40 (J)
        let mut code = vec![0x2a, 0x15, 5, 0x85, 0x1d, 0x85, 0x61];
        code.extend_from_slice(&invokeinterface(40, 3));
        code.push(0xb0);
        assert!(every_site_receiver_is_this(
            &code,
            &[],
            40,
            Some(7),
            "(J)Ljava/lang/foreign/MemorySegment;"
        ));
        assert!(!every_site_receiver_is_this(
            &code,
            &[],
            40,
            Some(0),
            "(J)Ljava/lang/foreign/MemorySegment;"
        ));
    }

    #[test]
    fn another_receiver_a_stored_local_zero_or_a_join_is_not_proven() {
        let desc = "(Ljava/lang/String;)I";
        // aload_1 aload_2 invokeinterface: the receiver is local 1.
        let mut other = vec![0x2b, 0x2c];
        other.extend_from_slice(&invokeinterface(1, 2));
        other.push(0xac);
        assert!(!every_site_receiver_is_this(&other, &[], 1, None, desc));
        // aload_1 astore_0 aload_0 aload_1 invokeinterface: local 0 reassigned.
        let mut stored = vec![0x2b, 0x4b, 0x2a, 0x2b];
        stored.extend_from_slice(&invokeinterface(1, 2));
        stored.push(0xac);
        assert!(!every_site_receiver_is_this(&stored, &[], 1, None, desc));
        // aload_0 aload_1 invokeinterface, with a handler starting at the
        // `aload_1`: control may reach the invoke with another stack.
        let mut joined = vec![0x2a, 0x2b];
        joined.extend_from_slice(&invokeinterface(1, 2));
        joined.push(0xac);
        assert!(every_site_receiver_is_this(&joined, &[], 1, None, desc));
        assert!(!every_site_receiver_is_this(&joined, &[1], 1, None, desc));
        // A `dup` in the window is not stepped over.
        let mut dup = vec![0x2a, 0x2b, 0x59, 0x57];
        dup.extend_from_slice(&invokeinterface(1, 2));
        dup.push(0xac);
        assert!(!every_site_receiver_is_this(&dup, &[], 1, None, desc));
        // `jsr` anywhere refuses the method.
        let mut jsr = vec![0x2a, 0x2b];
        jsr.extend_from_slice(&invokeinterface(1, 2));
        jsr.extend_from_slice(&[0xa8, 0x00, 0x03, 0xac]);
        assert!(!every_site_receiver_is_this(&jsr, &[], 1, None, desc));
    }

    #[test]
    fn a_branch_into_the_window_is_not_proven() {
        let desc = "(Ljava/lang/String;)I";
        // 0: aload_0  1: iload_1  2: ifeq +6 (-> 8)  5: aload_0  6: aload_2
        // 7: nop      8: ... invoke at 8 reached from the branch with other stack
        let mut code = vec![0x2a, 0x1b, 0x99, 0x00, 0x06, 0x2a, 0x2c, 0x00];
        code.extend_from_slice(&invokeinterface(1, 2));
        code.push(0xac);
        // `nop` is not in the step list either, so this is refused twice over;
        // without the nop the branch target is the invoke itself.
        assert!(!every_site_receiver_is_this(&code, &[], 1, None, desc));
        let mut direct = vec![0x2a, 0x1b, 0x99, 0x00, 0x05, 0x2a, 0x2c];
        direct.extend_from_slice(&invokeinterface(1, 2));
        direct.push(0xac);
        assert!(!every_site_receiver_is_this(&direct, &[], 1, None, desc));
    }
}
