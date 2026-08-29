// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The generated **opcode / execution-path coverage matrix**.
//!
//! The C2 review's P0 asks for a machine-readable matrix of which opcodes and
//! operand forms the corpus exercises under each of the VM's execution paths,
//! "so gaps are visible". The essential design decision is where the opcode list
//! comes from: a hand-maintained list is a list of what somebody *remembered*,
//! and it rots silently. This module derives it from the **class files
//! themselves** — the same bytes both VMs execute — so the matrix cannot claim
//! coverage the corpus does not have, and a seed that stops using an opcode
//! shows up as a new gap on the next run.
//!
//! ## What a cell means
//!
//! A cell is `(opcode, operand form) × axis`, and it is covered when **both**
//! sides agree:
//!
//! * **mode side** — a configured [`Mode`] drives that axis
//!   ([`axes_for`]). No configured mode drives the IR pipeline ⇒ the whole
//!   `ir-jit` column is a gap, whatever the corpus contains.
//! * **code side** — the opcode actually occurs somewhere the axis can reach
//!   ([`SiteFacts`]). `osr` needs the opcode to sit in a method with a backward
//!   branch (there is no back-edge to OSR from otherwise); `exception` needs it
//!   to sit in a method with a non-empty exception table, or to *be* `athrow`.
//!
//! Splitting the two is what makes a gap actionable. "`iaload` is not covered
//! under `osr`" has two possible fixes — configure the mode, or write a seed
//! that puts an `iaload` in a loop — and the matrix says which.
//!
//! ## What it deliberately does not claim
//!
//! Running a program under `ir-jit` does not prove the IR pipeline compiled the
//! method containing a given opcode: admission is a conjunction of six terms
//! inside `jit/src/lib.rs` and any of them can decline silently. The matrix
//! therefore reports **opportunity**, not confirmed execution: "the corpus
//! contains this opcode and a mode that drives this path ran it". Confirmed
//! per-method path attribution needs the VM's own compilation report
//! (`jit::metrics`), which the harness cannot see across a process boundary —
//! recorded as an open gap in `docs/testing/differential.md`.
//!
//! ## Three states per cell, not two
//!
//! A boolean cell cannot distinguish "nobody wrote a seed for this" from
//! "nothing *can* write a seed for this". `jsr` is not a coverage gap anybody
//! can close: `javac` has not emitted it since Java 5 and JVMS §4.9.1 forbids it
//! in class files of version ≥ 50.0. Reporting it next to a genuinely missing
//! `dup2_x2` makes the gap list unreadable and, worse, makes it look permanently
//! red. So every `(opcode, axis)` cell is one of [`CellState::Covered`],
//! [`CellState::Uncovered`] — carrying a [`Gap`] that names **which fix it
//! needs** — or [`CellState::UnreachableByConstruction`], carrying the reason.
//!
//! The generator's declarations arrive as a [`GeneratorIndex`]
//! (`crate::opcorpus::generator_index`). The dependency points that way on
//! purpose: this module never learns how programs are produced, so the report
//! still builds for a corpus the generator never touched, and an empty index
//! degrades to "no generator" rather than to silence.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::runner::Mode;

/// The current matrix schema version. Bumped on any breaking change to the
/// emitted JSON.
///
/// **2** adds `opcodes` (the per-opcode × axis tri-state grid), `reconciliation`
/// and the cell counters.
pub const MATRIX_SCHEMA_VERSION: u32 = 2;

// ---------------------------------------------------------------------------
// Path axes
// ---------------------------------------------------------------------------

/// One column of the matrix: a semantic implementation or transition the VM
/// has, and which a fix can land in without landing in the others.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PathAxis {
    /// The interpreter's raw bytecode handlers, including the superinstruction
    /// fusions (`vm/src/runtime/interpreter.rs` ~9280).
    InterpreterFast,
    /// The interpreter's decoded (quickened / full-decode) fallback, selected
    /// when the raw handlers are disabled.
    InterpreterDecoded,
    /// The single-pass x64 emitter (`jit::x64::compile`), a.k.a. the direct
    /// emitter / C1 tier.
    DirectJit,
    /// The optimizing IR pipeline (build → optimize → EA → schedule → lower).
    IrJit,
    /// Back-edge on-stack replacement: entering compiled code mid-loop.
    Osr,
    /// Execution reached from an exception handler, or via `athrow`.
    Exception,
    /// The compiled → interpreted transition (deoptimization).
    Deopt,
}

impl PathAxis {
    /// Every axis, in matrix column order.
    pub fn all() -> &'static [PathAxis] {
        &[
            PathAxis::InterpreterFast,
            PathAxis::InterpreterDecoded,
            PathAxis::DirectJit,
            PathAxis::IrJit,
            PathAxis::Osr,
            PathAxis::Exception,
            PathAxis::Deopt,
        ]
    }

    /// The stable kebab-case label used as a JSON key and a column header.
    pub fn label(self) -> &'static str {
        match self {
            PathAxis::InterpreterFast => "interpreter-fast",
            PathAxis::InterpreterDecoded => "interpreter-decoded",
            PathAxis::DirectJit => "direct-jit",
            PathAxis::IrJit => "ir-jit",
            PathAxis::Osr => "osr",
            PathAxis::Exception => "exception",
            PathAxis::Deopt => "deopt",
        }
    }
}

/// The axes a mode drives.
///
/// Conservative by construction: a mode claims an axis only when its flags make
/// that path reachable. A JIT-enabled mode claims `interpreter-fast` too,
/// because every method is interpreted until it is hot — the interpreter is not
/// bypassed by turning the JIT on, it is bypassed by *no* mode.
pub fn axes_for(mode: Mode) -> &'static [PathAxis] {
    use PathAxis::*;
    match mode {
        // Interpreter only.
        Mode::NoJit | Mode::JdkOnlyNoJit | Mode::RealCompatibleNoJit => {
            &[InterpreterFast, Exception]
        }
        // Interpreter only, raw handlers disabled by `--noverify`.
        Mode::InterpDecoded => &[InterpreterDecoded, Exception],
        // JIT on, IR-first with single-pass fallback.
        Mode::JitOn
        | Mode::NoIntrinsics
        | Mode::MovingGc
        | Mode::LowJitThreshold
        | Mode::JdkOnlyJit
        | Mode::RealCompatibleJit
        | Mode::NoOsr => &[InterpreterFast, DirectJit, IrJit, Exception],
        // IR routing suppressed for the shapes the VM lets us suppress.
        Mode::DirectEmit => &[InterpreterFast, DirectJit, Exception],
        // Every compile request treated as a C2 request.
        Mode::IrJit => &[InterpreterFast, IrJit, Exception],
        Mode::OsrEager => &[InterpreterFast, DirectJit, IrJit, Osr, Exception],
        Mode::ForcedDeopt => &[InterpreterFast, DirectJit, IrJit, Deopt, Exception],
    }
}

// ---------------------------------------------------------------------------
// Class-file scanning
// ---------------------------------------------------------------------------

/// What the corpus's own bytes say about where an `(opcode, form)` occurs.
///
/// The code side of a matrix cell: an axis that needs a loop or a handler can
/// only be covered where one exists.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SiteFacts {
    /// Occurs in a method whose `Code` attribute has a non-empty exception
    /// table, or *is* `athrow`.
    pub in_exception_method: bool,
    /// Occurs in a method containing a backward branch — the precondition for
    /// back-edge OSR.
    pub in_loop_method: bool,
}

impl SiteFacts {
    fn merge(&mut self, other: SiteFacts) {
        self.in_exception_method |= other.in_exception_method;
        self.in_loop_method |= other.in_loop_method;
    }
}

/// One program's opcode inventory, keyed by `(opcode, operand form)`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProgramScan {
    /// The class name (the corpus file stem).
    pub program: String,
    /// How many methods carried a `Code` attribute.
    pub methods: usize,
    /// `("<opcode>:<form>", facts)` — a `BTreeMap` so the emitted JSON is
    /// byte-stable across runs and can be diffed in review.
    pub sites: BTreeMap<String, SiteFacts>,
}

/// The key used in [`ProgramScan::sites`] and rebuilt by [`split_site_key`].
fn site_key(opcode: u8, form: &str) -> String {
    format!("{opcode}:{form}")
}

/// Inverse of [`site_key`]; `None` for a malformed key.
fn split_site_key(key: &str) -> Option<(u8, &str)> {
    let (op, form) = key.split_once(':')?;
    Some((op.parse().ok()?, form))
}

fn u16_at(b: &[u8], off: usize) -> Option<usize> {
    Some(((*b.get(off)? as usize) << 8) | (*b.get(off + 1)? as usize))
}

fn u32_at(b: &[u8], off: usize) -> Option<usize> {
    let mut v = 0usize;
    for i in 0..4 {
        v = (v << 8) | (*b.get(off + i)? as usize);
    }
    Some(v)
}

fn i32_at(b: &[u8], off: usize) -> Option<i32> {
    u32_at(b, off).map(|v| v as u32 as i32)
}

/// Walk the constant pool, returning the offset just past it plus every
/// `CONSTANT_Utf8` by index (needed to find the `Code` attribute by name).
fn parse_constant_pool(class: &[u8]) -> Option<(usize, BTreeMap<usize, String>)> {
    if class.len() < 10 || class[0..4] != [0xCA, 0xFE, 0xBA, 0xBE] {
        return None;
    }
    let cp_count = u16_at(class, 8)?;
    let mut utf8 = BTreeMap::new();
    let mut off = 10usize;
    let mut index = 1usize;
    while index < cp_count {
        let tag = *class.get(off)?;
        let payload = off + 1;
        let advance = match tag {
            1 => {
                let len = u16_at(class, payload)?;
                let bytes = class.get(payload + 2..payload + 2 + len)?;
                utf8.insert(index, String::from_utf8_lossy(bytes).into_owned());
                3 + len
            }
            3 | 4 => 5,
            5 | 6 => {
                index += 1; // long/double occupy two slots
                9
            }
            7 | 8 | 16 | 19 | 20 => 3,
            9 | 10 | 11 | 12 | 17 | 18 => 5,
            15 => 4,
            _ => return None, // unknown tag — refuse rather than guess
        };
        off = off.checked_add(advance)?;
        if off > class.len() {
            return None;
        }
        index += 1;
    }
    Some((off, utf8))
}

/// Skip an `attributes_count`-prefixed attribute list, returning the offset just
/// past it. `on_attr` is called with `(name_index, payload)` for each.
fn walk_attributes(
    class: &[u8],
    mut off: usize,
    mut on_attr: impl FnMut(usize, &[u8]),
) -> Option<usize> {
    let count = u16_at(class, off)?;
    off += 2;
    for _ in 0..count {
        let name_index = u16_at(class, off)?;
        let len = u32_at(class, off + 2)?;
        let start = off + 6;
        let payload = class.get(start..start.checked_add(len)?)?;
        on_attr(name_index, payload);
        off = start + len;
    }
    Some(off)
}

/// Scan one compiled class for its opcode inventory.
///
/// Returns `None` for anything that is not a well-formed class file — a scan
/// that guessed at a truncated file would report coverage the corpus does not
/// have, which is the one failure mode this whole module exists to prevent.
pub fn scan_class_bytes(program: &str, class: &[u8]) -> Option<ProgramScan> {
    let (cp_end, utf8) = parse_constant_pool(class)?;

    // access_flags, this_class, super_class, interfaces.
    let mut off = cp_end + 6;
    let interfaces = u16_at(class, cp_end + 6)?;
    off += 2 + interfaces * 2;

    // Fields, then methods: same shape, only the methods carry `Code`.
    let field_count = u16_at(class, off)?;
    off += 2;
    for _ in 0..field_count {
        off = walk_attributes(class, off + 6, |_, _| {})?;
    }

    let mut sites: BTreeMap<String, SiteFacts> = BTreeMap::new();
    let mut methods = 0usize;
    let method_count = u16_at(class, off)?;
    off += 2;
    for _ in 0..method_count {
        let mut code_bodies: Vec<Vec<u8>> = Vec::new();
        off = walk_attributes(class, off + 6, |name_index, payload| {
            if utf8.get(&name_index).map(String::as_str) == Some("Code") {
                code_bodies.push(payload.to_vec());
            }
        })?;
        for body in &code_bodies {
            if let Some((count, method_sites)) = scan_code_attribute(body) {
                methods += count;
                for (key, facts) in method_sites {
                    sites.entry(key).or_default().merge(facts);
                }
            }
        }
    }

    Some(ProgramScan {
        program: program.to_string(),
        methods,
        sites,
    })
}

/// Scan one `Code` attribute payload. Returns `(1, sites)` on success.
fn scan_code_attribute(payload: &[u8]) -> Option<(usize, BTreeMap<String, SiteFacts>)> {
    // max_stack(2) max_locals(2) code_length(4) code[] exception_table_length(2)
    let code_len = u32_at(payload, 4)?;
    let code = payload.get(8..8usize.checked_add(code_len)?)?;
    let handlers = u16_at(payload, 8 + code_len)?;

    // First pass: does this method contain a backward branch?
    let has_backward_branch = has_backward_branch(code);
    let facts = SiteFacts {
        in_exception_method: handlers > 0,
        in_loop_method: has_backward_branch,
    };

    let mut sites: BTreeMap<String, SiteFacts> = BTreeMap::new();
    let mut pc = 0usize;
    while pc < code.len() {
        let op = code[pc];
        let len = instruction_len(code, pc)?;
        let mut site = facts;
        // `athrow` reaches the exception axis whether or not this method has a
        // handler: it is how an exception is raised in the first place.
        if op == 0xbf {
            site.in_exception_method = true;
        }
        sites
            .entry(site_key(op, &operand_form(code, pc)))
            .or_default()
            .merge(site);
        pc = pc.checked_add(len)?;
    }
    Some((1, sites))
}

/// Whether the method's code contains a branch to a lower pc.
fn has_backward_branch(code: &[u8]) -> bool {
    let mut pc = 0usize;
    while pc < code.len() {
        let Some(len) = instruction_len(code, pc) else {
            return false;
        };
        let op = code[pc];
        let is_s16_branch = matches!(op, 0x99..=0xa8 | 0xc6 | 0xc7);
        if is_s16_branch {
            if let Some(hi) = code.get(pc + 1) {
                if let Some(lo) = code.get(pc + 2) {
                    let raw = ((*hi as u16) << 8) | (*lo as u16);
                    if (raw as i16) < 0 {
                        return true;
                    }
                }
            }
        } else if matches!(op, 0xc8 | 0xc9) {
            if let Some(delta) = i32_at(code, pc + 1) {
                if delta < 0 {
                    return true;
                }
            }
        }
        pc += len;
    }
    false
}

/// The encoded length of the instruction at `pc`, or `None` for a reserved /
/// truncated opcode.
pub fn instruction_len(code: &[u8], pc: usize) -> Option<usize> {
    let op = *code.get(pc)?;
    match op {
        0xaa => {
            // tableswitch: pad to a 4-byte boundary, default, low, high, offsets
            let base = pc + 1 + switch_pad(pc);
            let low = i32_at(code, base + 4)?;
            let high = i32_at(code, base + 8)?;
            let n = i64::from(high) - i64::from(low) + 1;
            if n < 0 {
                return None;
            }
            Some((base + 12 + (n as usize) * 4) - pc)
        }
        0xab => {
            // lookupswitch: pad, default, npairs, pairs
            let base = pc + 1 + switch_pad(pc);
            let npairs = i32_at(code, base + 4)?;
            if npairs < 0 {
                return None;
            }
            Some((base + 8 + (npairs as usize) * 8) - pc)
        }
        0xc4 => {
            // wide: `wide iinc` carries two extra u16 operands.
            match code.get(pc + 1) {
                Some(0x84) => Some(6),
                Some(_) => Some(4),
                None => None,
            }
        }
        _ => fixed_len(op),
    }
}

/// The pad bytes a switch at `pc` inserts to reach a 4-byte boundary.
fn switch_pad(pc: usize) -> usize {
    (4 - ((pc + 1) % 4)) % 4
}

/// Encoded length of every fixed-length opcode (JVMS §6.5).
fn fixed_len(op: u8) -> Option<usize> {
    Some(match op {
        0x00..=0x0f => 1, // nop … dconst_1
        0x10 => 2,        // bipush
        0x11 => 3,        // sipush
        0x12 => 2,        // ldc
        0x13 | 0x14 => 3, // ldc_w, ldc2_w
        0x15..=0x19 => 2, // iload … aload
        0x1a..=0x35 => 1, // iload_0 … saload
        0x36..=0x3a => 2, // istore … astore
        0x3b..=0x83 => 1, // istore_0 … lxor
        0x84 => 3,        // iinc
        0x85..=0x98 => 1, // i2l … dcmpg
        0x99..=0xa8 => 3, // ifeq … jsr
        0xa9 => 2,        // ret
        0xac..=0xb1 => 1, // ireturn … return
        0xb2..=0xb8 => 3, // getstatic … invokestatic
        0xb9 | 0xba => 5, // invokeinterface, invokedynamic
        0xbb => 3,        // new
        0xbc => 2,        // newarray
        0xbd => 3,        // anewarray
        0xbe | 0xbf => 1, // arraylength, athrow
        0xc0 | 0xc1 => 3, // checkcast, instanceof
        0xc2 | 0xc3 => 1, // monitorenter, monitorexit
        0xc5 => 4,        // multianewarray
        0xc6 | 0xc7 => 3, // ifnull, ifnonnull
        0xc8 | 0xc9 => 5, // goto_w, jsr_w
        _ => return None, // 0xaa/0xab/0xc4 are variable; 0xca.. are reserved
    })
}

/// The **operand form** of the instruction at `pc` — the second half of a matrix
/// row key.
///
/// The opcode alone is too coarse: `newarray` on a `boolean[]` and on a
/// `double[]` are different code paths in every one of the VM's four executors,
/// and `ldc` of a `String` and of an `int` resolve through different constant-
/// pool machinery. Forms are chosen to be **bounded** (so the matrix stays a
/// matrix): an operand that could take thousands of values, like a constant-pool
/// index or a branch offset, is summarized by its *shape*, not its value.
pub fn operand_form(code: &[u8], pc: usize) -> String {
    let op = code.get(pc).copied().unwrap_or(0);
    match op {
        0x10 => "s8".to_string(),
        0x11 => "s16".to_string(),
        0x12 => "cp8".to_string(),
        0x13 | 0x14 => "cp16".to_string(),
        0x15..=0x19 | 0x36..=0x3a | 0xa9 => "local:u8".to_string(),
        0x84 => "local:u8,s8".to_string(),
        0x99..=0xa8 | 0xc6 | 0xc7 => "branch:s16".to_string(),
        0xc8 | 0xc9 => "branch:s32".to_string(),
        0xaa => "table".to_string(),
        0xab => "lookup".to_string(),
        0xb2..=0xb8 | 0xbb | 0xbd | 0xc0 | 0xc1 => "cp16".to_string(),
        0xb9 => "cp16,count".to_string(),
        0xba => "cp16,zero16".to_string(),
        0xbc => format!("atype:{}", array_type_name(code.get(pc + 1).copied())),
        // multianewarray is `c5 indexbyte1 indexbyte2 dimensions` — the
        // dimension count is the FOURTH byte, past both constant-pool index
        // bytes. Reading `pc + 2` returned the low half of the CP index.
        0xc5 => format!("cp16,dims:{}", code.get(pc + 3).copied().unwrap_or(0)),
        0xc4 => format!(
            "wide:{}",
            code.get(pc + 1)
                .and_then(|o| opcode_name(*o))
                .unwrap_or("?")
        ),
        // Everything else encodes its operand in the opcode itself.
        _ => "implicit".to_string(),
    }
}

/// `newarray`'s `atype` operand, spelled as the Java primitive type.
fn array_type_name(atype: Option<u8>) -> &'static str {
    match atype {
        Some(4) => "boolean",
        Some(5) => "char",
        Some(6) => "float",
        Some(7) => "double",
        Some(8) => "byte",
        Some(9) => "short",
        Some(10) => "int",
        Some(11) => "long",
        _ => "?",
    }
}

/// The JVMS mnemonic for `op`, or `None` for a reserved opcode.
///
/// Present so the matrix can list what the corpus does **not** exercise: a gap
/// report that could only name opcodes it had already seen would be useless.
pub fn opcode_name(op: u8) -> Option<&'static str> {
    const NAMES: [&str; 202] = [
        "nop",
        "aconst_null",
        "iconst_m1",
        "iconst_0",
        "iconst_1",
        "iconst_2",
        "iconst_3",
        "iconst_4",
        "iconst_5",
        "lconst_0",
        "lconst_1",
        "fconst_0",
        "fconst_1",
        "fconst_2",
        "dconst_0",
        "dconst_1",
        "bipush",
        "sipush",
        "ldc",
        "ldc_w",
        "ldc2_w",
        "iload",
        "lload",
        "fload",
        "dload",
        "aload",
        "iload_0",
        "iload_1",
        "iload_2",
        "iload_3",
        "lload_0",
        "lload_1",
        "lload_2",
        "lload_3",
        "fload_0",
        "fload_1",
        "fload_2",
        "fload_3",
        "dload_0",
        "dload_1",
        "dload_2",
        "dload_3",
        "aload_0",
        "aload_1",
        "aload_2",
        "aload_3",
        "iaload",
        "laload",
        "faload",
        "daload",
        "aaload",
        "baload",
        "caload",
        "saload",
        "istore",
        "lstore",
        "fstore",
        "dstore",
        "astore",
        "istore_0",
        "istore_1",
        "istore_2",
        "istore_3",
        "lstore_0",
        "lstore_1",
        "lstore_2",
        "lstore_3",
        "fstore_0",
        "fstore_1",
        "fstore_2",
        "fstore_3",
        "dstore_0",
        "dstore_1",
        "dstore_2",
        "dstore_3",
        "astore_0",
        "astore_1",
        "astore_2",
        "astore_3",
        "iastore",
        "lastore",
        "fastore",
        "dastore",
        "aastore",
        "bastore",
        "castore",
        "sastore",
        "pop",
        "pop2",
        "dup",
        "dup_x1",
        "dup_x2",
        "dup2",
        "dup2_x1",
        "dup2_x2",
        "swap",
        "iadd",
        "ladd",
        "fadd",
        "dadd",
        "isub",
        "lsub",
        "fsub",
        "dsub",
        "imul",
        "lmul",
        "fmul",
        "dmul",
        "idiv",
        "ldiv",
        "fdiv",
        "ddiv",
        "irem",
        "lrem",
        "frem",
        "drem",
        "ineg",
        "lneg",
        "fneg",
        "dneg",
        "ishl",
        "lshl",
        "ishr",
        "lshr",
        "iushr",
        "lushr",
        "iand",
        "land",
        "ior",
        "lor",
        "ixor",
        "lxor",
        "iinc",
        "i2l",
        "i2f",
        "i2d",
        "l2i",
        "l2f",
        "l2d",
        "f2i",
        "f2l",
        "f2d",
        "d2i",
        "d2l",
        "d2f",
        "i2b",
        "i2c",
        "i2s",
        "lcmp",
        "fcmpl",
        "fcmpg",
        "dcmpl",
        "dcmpg",
        "ifeq",
        "ifne",
        "iflt",
        "ifge",
        "ifgt",
        "ifle",
        "if_icmpeq",
        "if_icmpne",
        "if_icmplt",
        "if_icmpge",
        "if_icmpgt",
        "if_icmple",
        "if_acmpeq",
        "if_acmpne",
        "goto",
        "jsr",
        "ret",
        "tableswitch",
        "lookupswitch",
        "ireturn",
        "lreturn",
        "freturn",
        "dreturn",
        "areturn",
        "return",
        "getstatic",
        "putstatic",
        "getfield",
        "putfield",
        "invokevirtual",
        "invokespecial",
        "invokestatic",
        "invokeinterface",
        "invokedynamic",
        "new",
        "newarray",
        "anewarray",
        "arraylength",
        "athrow",
        "checkcast",
        "instanceof",
        "monitorenter",
        "monitorexit",
        "wide",
        "multianewarray",
        "ifnull",
        "ifnonnull",
        "goto_w",
        "jsr_w",
    ];
    NAMES.get(op as usize).copied()
}

// ---------------------------------------------------------------------------
// The matrix
// ---------------------------------------------------------------------------

/// One row: an `(opcode, operand form)` the corpus contains, and which axes
/// reach it under the configured modes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MatrixRow {
    pub opcode: u8,
    /// The JVMS mnemonic, or `"reserved"` for an opcode with no name (which can
    /// only appear if a mutated class file is in the corpus).
    pub name: String,
    pub form: String,
    /// The corpus programs containing this `(opcode, form)`.
    pub programs: Vec<String>,
    /// Whether the corpus lets each axis reach this row, keyed by
    /// [`PathAxis::label`].
    pub axes: BTreeMap<String, bool>,
    /// The merged code-side facts across every occurrence.
    pub facts: SiteFacts,
}

impl MatrixRow {
    /// The axes this row does **not** cover — the row's contribution to the gap
    /// list.
    pub fn uncovered(&self) -> Vec<&str> {
        PathAxis::all()
            .iter()
            .map(|a| a.label())
            .filter(|l| !self.axes.get(*l).copied().unwrap_or(false))
            .collect()
    }
}

/// An opcode the corpus never uses at all.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnexercisedOpcode {
    pub opcode: u8,
    pub name: String,
}

// ---------------------------------------------------------------------------
// What the corpus generator declares
// ---------------------------------------------------------------------------

/// What the corpus generator says about one opcode.
///
/// Kept to the two facts the report needs. Anything richer would couple this
/// module to a particular generator, and the matrix has to remain runnable over
/// a corpus of hand-written seeds, promoted app classes or mutants.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum GeneratorStatus {
    /// A generator emits a program whose focus is this opcode.
    Generated,
    /// Nothing can generate it, and why.
    Unreachable { reason: String },
}

/// The generator's declarations, keyed by opcode. An **absent** key means "no
/// generator knows about this opcode", which is a different (and more
/// actionable) statement than "unreachable".
pub type GeneratorIndex = BTreeMap<u8, GeneratorStatus>;

// ---------------------------------------------------------------------------
// The per-opcode × axis grid
// ---------------------------------------------------------------------------

/// Why a cell is not covered — and therefore which fix closes it.
///
/// This is the whole point of splitting the gap out of the boolean: "`iaload`
/// is not covered under `osr`" has four possible causes with four different
/// owners, and a report that does not say which one burns the reader's time.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Gap {
    /// No configured mode drives the axis at all.
    NoModeDrivesAxis,
    /// A generator exists but its opcode is absent from the scanned corpus.
    NotInCorpusButGenerated,
    /// The corpus lacks the opcode and nothing generates it.
    NoGenerator,
    /// Present, but never in a method with a backward branch.
    NoLoopSite,
    /// Present, but never in a method with a handler (and it is not `athrow`).
    NoHandlerSite,
}

impl Gap {
    /// The one-line fix, written as an instruction rather than a diagnosis.
    pub fn fix(self) -> &'static str {
        match self {
            Gap::NoModeDrivesAxis => {
                "add a mode that drives this axis to --modes (see Mode::execution_paths)"
            }
            Gap::NotInCorpusButGenerated => {
                "regenerate the corpus: `cratonvm-difftest gen-opcodes`, then re-run `matrix` \
                 against it"
            }
            Gap::NoGenerator => {
                "write a generator recipe for this opcode in difftest/src/opcorpus.rs, or add a \
                 seed containing it"
            }
            Gap::NoLoopSite => {
                "put the opcode inside a method with a backward branch — there is no back-edge to \
                 OSR from otherwise"
            }
            Gap::NoHandlerSite => "put the opcode inside a method with a non-empty exception table",
        }
    }
}

/// One `(opcode, axis)` cell.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "kebab-case")]
pub enum CellState {
    /// A mode drives the axis and the corpus puts the opcode somewhere the axis
    /// reaches.
    Covered,
    /// Not covered, with the reason and the fix.
    Uncovered { gap: Gap, fix: String },
    /// Nothing can cover it — no seed, no generator, no mode change. Carries
    /// the reason so the claim can be argued with.
    UnreachableByConstruction { reason: String },
}

impl CellState {
    fn uncovered(gap: Gap) -> Self {
        CellState::Uncovered {
            gap,
            fix: gap.fix().to_string(),
        }
    }

    pub fn is_covered(&self) -> bool {
        matches!(self, CellState::Covered)
    }

    pub fn is_unreachable(&self) -> bool {
        matches!(self, CellState::UnreachableByConstruction { .. })
    }

    /// The short label used in the rendered summary.
    pub fn label(&self) -> &'static str {
        match self {
            CellState::Covered => "covered",
            CellState::Uncovered { .. } => "uncovered",
            CellState::UnreachableByConstruction { .. } => "unreachable",
        }
    }
}

/// One opcode's row of the grid: every axis, always, for all 202 named opcodes.
///
/// Emitted even when the corpus contains no occurrence at all — an opcode that
/// vanished from the report would read as covered to anyone scanning for red.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpcodeReport {
    pub opcode: u8,
    pub name: String,
    /// The operand forms the corpus actually contains for this opcode.
    pub forms: Vec<String>,
    /// The corpus programs containing it.
    pub programs: Vec<String>,
    /// Whether a generator claims this opcode.
    pub generator: Option<GeneratorStatus>,
    /// Axis label → state, in [`PathAxis::all`] order.
    pub cells: BTreeMap<String, CellState>,
}

impl OpcodeReport {
    /// Every axis covered.
    pub fn fully_covered(&self) -> bool {
        self.cells.values().all(CellState::is_covered)
    }

    /// Every axis unreachable — the opcode is out of reach entirely.
    pub fn fully_unreachable(&self) -> bool {
        !self.cells.is_empty() && self.cells.values().all(CellState::is_unreachable)
    }

    /// The axes that are neither covered nor unreachable, with their gaps.
    pub fn gaps(&self) -> Vec<(&str, Gap)> {
        self.cells
            .iter()
            .filter_map(|(axis, state)| match state {
                CellState::Uncovered { gap, .. } => Some((axis.as_str(), *gap)),
                _ => None,
            })
            .collect()
    }
}

/// A mismatch between what the generator claims and what the compiled corpus
/// contains.
///
/// The reconciliation is what keeps the generator honest. Its per-opcode
/// witness is a *declaration*; only this step — compile the generated corpus
/// with the same `javac` the differential run uses, then walk the bytes —
/// proves the recipe still works. A `javac` release that changes a lowering
/// shows up here instead of quietly inflating the coverage number.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reconciliation {
    pub opcode: u8,
    pub name: String,
    pub note: String,
}

/// How many cells are in each state.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct CellCounts {
    pub covered: usize,
    pub uncovered: usize,
    pub unreachable: usize,
}

impl CellCounts {
    pub fn total(&self) -> usize {
        self.covered + self.uncovered + self.unreachable
    }
}

/// The generated matrix: machine-readable, byte-stable, and diffable in review.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoverageMatrix {
    pub schema_version: u32,
    /// The `--modes` list the matrix was computed for. Changing it changes the
    /// axis columns, so it is part of the artifact.
    pub modes: Vec<String>,
    /// Axis columns, in [`PathAxis::all`] order.
    pub axes: Vec<String>,
    /// The axes no configured mode drives at all — a *mode-side* gap, fixable
    /// by adding a mode rather than by writing a seed.
    pub axes_without_a_mode: Vec<String>,
    /// One entry per corpus program that scanned successfully.
    pub programs: Vec<ProgramScan>,
    /// Programs whose bytes could not be scanned, with the reason. Surfaced,
    /// never silently dropped: a class that failed to parse is coverage the
    /// matrix is *not* reporting.
    pub unscannable: Vec<(String, String)>,
    /// `(opcode, form)` rows, sorted by opcode then form.
    pub rows: Vec<MatrixRow>,
    /// Opcodes with a JVMS mnemonic that appear nowhere in the corpus.
    pub unexercised_opcodes: Vec<UnexercisedOpcode>,
    /// The per-opcode × axis grid: **all 202 named opcodes**, always, each cell
    /// covered / uncovered-with-a-fix / unreachable-with-a-reason.
    pub opcodes: Vec<OpcodeReport>,
    /// Cell-state totals over [`CoverageMatrix::opcodes`].
    pub cells: CellCounts,
    /// Where the generator's claims and the compiled corpus disagree.
    pub reconciliation: Vec<Reconciliation>,
}

impl CoverageMatrix {
    /// Build the matrix from scanned programs, the configured mode list, and
    /// what the corpus generator declares.
    ///
    /// Pass an empty [`GeneratorIndex`] for a corpus no generator produced: every
    /// absent opcode is then reported as [`Gap::NoGenerator`], which is the
    /// honest reading — *nothing here knows how to produce it* — rather than a
    /// silent omission.
    pub fn build(
        scans: Vec<ProgramScan>,
        modes: &[Mode],
        unscannable: Vec<(String, String)>,
        generators: &GeneratorIndex,
    ) -> Self {
        // Which axes any configured mode drives.
        let mut driven: BTreeSet<PathAxis> = BTreeSet::new();
        for m in modes {
            driven.extend(axes_for(*m).iter().copied());
        }

        // Merge every program's sites into rows.
        let mut merged: BTreeMap<(u8, String), (SiteFacts, BTreeSet<String>)> = BTreeMap::new();
        for scan in &scans {
            for (key, facts) in &scan.sites {
                let Some((opcode, form)) = split_site_key(key) else {
                    continue;
                };
                let entry = merged
                    .entry((opcode, form.to_string()))
                    .or_insert_with(|| (SiteFacts::default(), BTreeSet::new()));
                entry.0.merge(*facts);
                entry.1.insert(scan.program.clone());
            }
        }

        let rows: Vec<MatrixRow> = merged
            .into_iter()
            .map(|((opcode, form), (facts, programs))| {
                let axes = PathAxis::all()
                    .iter()
                    .map(|axis| {
                        let mode_side = driven.contains(axis);
                        let code_side = match axis {
                            PathAxis::Osr => facts.in_loop_method,
                            PathAxis::Exception => facts.in_exception_method,
                            _ => true,
                        };
                        (axis.label().to_string(), mode_side && code_side)
                    })
                    .collect();
                MatrixRow {
                    opcode,
                    name: opcode_name(opcode).unwrap_or("reserved").to_string(),
                    form,
                    programs: programs.into_iter().collect(),
                    axes,
                    facts,
                }
            })
            .collect();

        let seen: BTreeSet<u8> = rows.iter().map(|r| r.opcode).collect();
        let unexercised_opcodes = (0u8..=0xc9)
            .filter(|op| !seen.contains(op))
            .filter_map(|op| {
                opcode_name(op).map(|name| UnexercisedOpcode {
                    opcode: op,
                    name: name.to_string(),
                })
            })
            .collect();

        let axes_without_a_mode = PathAxis::all()
            .iter()
            .filter(|a| !driven.contains(*a))
            .map(|a| a.label().to_string())
            .collect();

        let (opcodes, cells, reconciliation) = build_opcode_grid(&rows, &driven, generators);

        Self {
            schema_version: MATRIX_SCHEMA_VERSION,
            modes: modes.iter().map(|m| m.label().to_string()).collect(),
            axes: PathAxis::all()
                .iter()
                .map(|a| a.label().to_string())
                .collect(),
            axes_without_a_mode,
            programs: scans,
            unscannable,
            rows,
            unexercised_opcodes,
            opcodes,
            cells,
            reconciliation,
        }
    }

    /// How many `(opcode, form)` rows cover every axis.
    pub fn fully_covered_rows(&self) -> usize {
        self.rows
            .iter()
            .filter(|r| r.uncovered().is_empty())
            .count()
    }

    /// How many of the 202 named opcodes are covered on every axis.
    pub fn fully_covered_opcodes(&self) -> usize {
        self.opcodes.iter().filter(|o| o.fully_covered()).count()
    }

    /// How many are out of reach entirely (every axis unreachable).
    pub fn unreachable_opcodes(&self) -> usize {
        self.opcodes
            .iter()
            .filter(|o| o.fully_unreachable())
            .count()
    }

    /// Pretty JSON, for the committed artifact.
    pub fn to_json(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string_pretty(self)
    }

    /// A short human summary for the CLI. The JSON is the artifact; this is the
    /// line a reviewer reads first.
    pub fn render(&self) -> String {
        use std::fmt::Write as _;
        let mut s = String::new();
        let _ = writeln!(
            s,
            "opcode/path matrix — {} program(s), {} distinct (opcode, form) row(s), \
             {} row(s) covering every axis",
            self.programs.len(),
            self.rows.len(),
            self.fully_covered_rows()
        );
        let _ = writeln!(s, "  modes: {}", self.modes.join(","));
        if !self.axes_without_a_mode.is_empty() {
            let _ = writeln!(
                s,
                "  AXIS GAP (no configured mode drives these): {}",
                self.axes_without_a_mode.join(", ")
            );
        }
        for (program, reason) in &self.unscannable {
            let _ = writeln!(s, "  UNSCANNED {program} — {reason}");
        }
        let _ = writeln!(
            s,
            "  {} of 202 named opcodes never appear in the corpus",
            self.unexercised_opcodes.len()
        );
        let _ = writeln!(
            s,
            "  opcodes: {} covered on every axis, {} unreachable by construction, {} partial",
            self.fully_covered_opcodes(),
            self.unreachable_opcodes(),
            self.opcodes.len() - self.fully_covered_opcodes() - self.unreachable_opcodes()
        );
        let _ = writeln!(
            s,
            "  cells:   {} covered, {} uncovered, {} unreachable (of {} = {} opcodes × {} axes)",
            self.cells.covered,
            self.cells.uncovered,
            self.cells.unreachable,
            self.cells.total(),
            self.opcodes.len(),
            self.axes.len()
        );
        for r in &self.reconciliation {
            let _ = writeln!(s, "  RECONCILE {} ({:#04x}) — {}", r.name, r.opcode, r.note);
        }
        s
    }

    /// The per-opcode gap list, one line per uncovered cell, grouped by opcode
    /// and naming the fix. This is what `--show-gaps` prints.
    pub fn render_gaps(&self) -> String {
        use std::fmt::Write as _;
        let mut s = String::new();
        for report in &self.opcodes {
            if report.fully_covered() {
                continue;
            }
            if report.fully_unreachable() {
                let reason = report
                    .cells
                    .values()
                    .find_map(|c| match c {
                        CellState::UnreachableByConstruction { reason } => Some(reason.as_str()),
                        _ => None,
                    })
                    .unwrap_or("(no reason recorded)");
                let _ = writeln!(
                    s,
                    "  UNREACHABLE {} ({:#04x}) — {reason}",
                    report.name, report.opcode
                );
                continue;
            }
            // One line per distinct gap, listing the axes it applies to, so an
            // opcode missing from the corpus does not print seven identical
            // lines.
            let mut by_gap: BTreeMap<&'static str, (Gap, Vec<&str>)> = BTreeMap::new();
            for (axis, gap) in report.gaps() {
                by_gap
                    .entry(gap_key(gap))
                    .or_insert_with(|| (gap, Vec::new()))
                    .1
                    .push(axis);
            }
            for (_, (gap, axes)) in by_gap {
                let _ = writeln!(
                    s,
                    "  GAP {} ({:#04x}) [{}] — {} — fix: {}",
                    report.name,
                    report.opcode,
                    axes.join(", "),
                    gap_key(gap),
                    gap.fix()
                );
            }
        }
        s
    }
}

/// Stable key/label for a [`Gap`], used to group gap lines.
fn gap_key(gap: Gap) -> &'static str {
    match gap {
        Gap::NoModeDrivesAxis => "no-mode-drives-axis",
        Gap::NotInCorpusButGenerated => "not-in-corpus-but-generated",
        Gap::NoGenerator => "no-generator",
        Gap::NoLoopSite => "no-loop-site",
        Gap::NoHandlerSite => "no-handler-site",
    }
}

/// Build the per-opcode × axis grid over **every** named opcode.
///
/// Precedence inside a cell, and the order matters:
///
/// 1. **Corpus evidence wins over any declaration.** If the bytes contain the
///    opcode, no "unreachable" claim can override that — a `mutate`d class
///    really can carry a `swap`, and a report that called it unreachable while
///    the corpus executed it would be lying. Such a contradiction is recorded in
///    the reconciliation list instead.
/// 2. **Unreachable-by-construction**, from the generator's declaration.
/// 3. **Not in the corpus**, split by whether a generator exists — because
///    "regenerate" and "write a recipe" are different jobs for different people.
/// 4. **No mode drives the axis** — a whole dead column, fixable in `--modes`.
/// 5. **Code-side**: no loop site (`osr`) or no handler site (`exception`).
fn build_opcode_grid(
    rows: &[MatrixRow],
    driven: &BTreeSet<PathAxis>,
    generators: &GeneratorIndex,
) -> (Vec<OpcodeReport>, CellCounts, Vec<Reconciliation>) {
    // Collapse the (opcode, form) rows down to per-opcode facts.
    let mut per_opcode: BTreeMap<u8, (SiteFacts, Vec<String>, BTreeSet<String>)> = BTreeMap::new();
    for row in rows {
        let entry = per_opcode
            .entry(row.opcode)
            .or_insert_with(|| (SiteFacts::default(), Vec::new(), BTreeSet::new()));
        entry.0.merge(row.facts);
        entry.1.push(row.form.clone());
        entry.2.extend(row.programs.iter().cloned());
    }

    let mut reports = Vec::new();
    let mut counts = CellCounts::default();
    let mut reconciliation = Vec::new();

    for opcode in 0u8..=0xc9 {
        let Some(name) = opcode_name(opcode) else {
            continue;
        };
        let generator = generators.get(&opcode).cloned();
        let present = per_opcode.get(&opcode);
        let (facts, forms, programs) = match present {
            Some((f, forms, progs)) => (*f, forms.clone(), progs.iter().cloned().collect()),
            None => (SiteFacts::default(), Vec::new(), Vec::new()),
        };

        // Contradictions, both directions, before any cell is decided.
        match (&generator, present.is_some()) {
            (Some(GeneratorStatus::Unreachable { reason }), true) => {
                reconciliation.push(Reconciliation {
                    opcode,
                    name: name.to_string(),
                    note: format!(
                        "declared unreachable-by-construction ({reason}) but the scanned corpus \
                         contains it — the corpus wins; the declaration is stale"
                    ),
                });
            }
            (Some(GeneratorStatus::Generated), false) => {
                reconciliation.push(Reconciliation {
                    opcode,
                    name: name.to_string(),
                    note: "a generator claims this opcode, but the compiled corpus does not \
                           contain it — the recipe no longer produces it, or this corpus was not \
                           generated"
                        .to_string(),
                });
            }
            _ => {}
        }

        let unreachable_reason = match (&generator, present.is_some()) {
            // Rule 1: evidence beats declaration.
            (_, true) => None,
            (Some(GeneratorStatus::Unreachable { reason }), false) => Some(reason.clone()),
            _ => None,
        };

        let cells: BTreeMap<String, CellState> = PathAxis::all()
            .iter()
            .map(|axis| {
                let state = if let Some(reason) = &unreachable_reason {
                    CellState::UnreachableByConstruction {
                        reason: reason.clone(),
                    }
                } else if present.is_none() {
                    CellState::uncovered(match &generator {
                        Some(GeneratorStatus::Generated) => Gap::NotInCorpusButGenerated,
                        _ => Gap::NoGenerator,
                    })
                } else if !driven.contains(axis) {
                    CellState::uncovered(Gap::NoModeDrivesAxis)
                } else {
                    match axis {
                        PathAxis::Osr if !facts.in_loop_method => {
                            CellState::uncovered(Gap::NoLoopSite)
                        }
                        PathAxis::Exception if !facts.in_exception_method => {
                            CellState::uncovered(Gap::NoHandlerSite)
                        }
                        _ => CellState::Covered,
                    }
                };
                match &state {
                    CellState::Covered => counts.covered += 1,
                    CellState::Uncovered { .. } => counts.uncovered += 1,
                    CellState::UnreachableByConstruction { .. } => counts.unreachable += 1,
                }
                (axis.label().to_string(), state)
            })
            .collect();

        reports.push(OpcodeReport {
            opcode,
            name: name.to_string(),
            forms,
            programs,
            generator,
            cells,
        });
    }

    (reports, counts, reconciliation)
}

// ---------------------------------------------------------------------------
// Corpus scanning
// ---------------------------------------------------------------------------

/// Scan a directory of compiled `.class` files.
///
/// `.java` sources are **not** compiled here: the caller (the CLI) compiles the
/// corpus once with the same `javac` the run uses, so the matrix describes the
/// exact bytes the differential run executed rather than a second, possibly
/// differently-compiled copy.
pub fn scan_class_dir(dir: &Path) -> (Vec<ProgramScan>, Vec<(String, String)>) {
    let mut scans = Vec::new();
    let mut failed = Vec::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return (
            scans,
            vec![(dir.display().to_string(), "unreadable directory".into())],
        );
    };
    let mut paths: Vec<std::path::PathBuf> = entries
        .filter_map(Result::ok)
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|s| s.to_str()) == Some("class"))
        .collect();
    paths.sort();
    for path in paths {
        let name = path
            .file_stem()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        match std::fs::read(&path) {
            Ok(bytes) => match scan_class_bytes(&name, &bytes) {
                Some(scan) => scans.push(scan),
                None => failed.push((name, "not a well-formed class file".to_string())),
            },
            Err(e) => failed.push((name, e.to_string())),
        }
    }
    (scans, failed)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal but *real* class file: a constant pool with the one
    /// `Utf8` the scanner needs (`"Code"`), no fields, and one method whose
    /// `Code` attribute carries `body`.
    ///
    /// Hand-built rather than compiled so the matrix generator's own test does
    /// not need `javac` on PATH — and so a "known corpus" is known exactly.
    fn class_with_code(body: &[u8], handlers: u16) -> Vec<u8> {
        let mut b: Vec<u8> = vec![0xCA, 0xFE, 0xBA, 0xBE, 0x00, 0x00, 0x00, 0x41];
        // constant_pool_count = 2 ⇒ one entry, #1 = Utf8 "Code".
        b.extend_from_slice(&2u16.to_be_bytes());
        b.push(1);
        b.extend_from_slice(&4u16.to_be_bytes());
        b.extend_from_slice(b"Code");
        // access_flags, this_class, super_class, interfaces_count = 0
        b.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 0]);
        // fields_count = 0
        b.extend_from_slice(&0u16.to_be_bytes());
        // methods_count = 1
        b.extend_from_slice(&1u16.to_be_bytes());
        // access_flags, name_index, descriptor_index
        b.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
        // attributes_count = 1, name_index = #1 ("Code")
        b.extend_from_slice(&1u16.to_be_bytes());
        b.extend_from_slice(&1u16.to_be_bytes());

        // Code payload.
        let mut code_attr: Vec<u8> = Vec::new();
        code_attr.extend_from_slice(&1u16.to_be_bytes()); // max_stack
        code_attr.extend_from_slice(&1u16.to_be_bytes()); // max_locals
        code_attr.extend_from_slice(&(body.len() as u32).to_be_bytes());
        code_attr.extend_from_slice(body);
        code_attr.extend_from_slice(&handlers.to_be_bytes());
        for _ in 0..handlers {
            code_attr.extend_from_slice(&[0; 8]);
        }
        code_attr.extend_from_slice(&0u16.to_be_bytes()); // attributes_count

        b.extend_from_slice(&(code_attr.len() as u32).to_be_bytes());
        b.extend_from_slice(&code_attr);
        b
    }

    #[test]
    fn opcode_names_cover_the_whole_jvms_range() {
        assert_eq!(opcode_name(0x00), Some("nop"));
        assert_eq!(opcode_name(0x60), Some("iadd"));
        assert_eq!(opcode_name(0xb6), Some("invokevirtual"));
        assert_eq!(opcode_name(0xba), Some("invokedynamic"));
        assert_eq!(opcode_name(0xc9), Some("jsr_w"));
        // 0xca (breakpoint) and up are reserved and must not be named.
        assert_eq!(opcode_name(0xca), None);
        assert_eq!(opcode_name(0xff), None);
        // Every named opcode must have a decodable length (except the three
        // variable-length ones, which `instruction_len` handles specially).
        for op in 0u8..=0xc9 {
            if matches!(op, 0xaa | 0xab | 0xc4) {
                continue;
            }
            assert!(fixed_len(op).is_some(), "no length for opcode {op:#04x}");
        }
    }

    #[test]
    fn instruction_len_handles_the_variable_length_opcodes() {
        // tableswitch at pc 0: pad = 3, default+low+high = 12, 2 offsets = 8.
        let mut code = vec![0xaa, 0, 0, 0];
        code.extend_from_slice(&0i32.to_be_bytes()); // default
        code.extend_from_slice(&1i32.to_be_bytes()); // low
        code.extend_from_slice(&2i32.to_be_bytes()); // high
        code.extend_from_slice(&0i32.to_be_bytes());
        code.extend_from_slice(&0i32.to_be_bytes());
        assert_eq!(instruction_len(&code, 0), Some(24));

        // lookupswitch at pc 0: pad = 3, default + npairs = 8, 1 pair = 8.
        let mut look = vec![0xab, 0, 0, 0];
        look.extend_from_slice(&0i32.to_be_bytes());
        look.extend_from_slice(&1i32.to_be_bytes());
        look.extend_from_slice(&[0; 8]);
        assert_eq!(instruction_len(&look, 0), Some(20));

        // wide iload = 4 bytes, wide iinc = 6.
        assert_eq!(instruction_len(&[0xc4, 0x15, 0, 1], 0), Some(4));
        assert_eq!(instruction_len(&[0xc4, 0x84, 0, 1, 0, 1], 0), Some(6));
        // A reserved opcode has no length rather than a guessed one.
        assert_eq!(instruction_len(&[0xca], 0), None);
    }

    #[test]
    fn operand_forms_are_bounded_and_specific() {
        assert_eq!(operand_form(&[0x60], 0), "implicit"); // iadd
        assert_eq!(operand_form(&[0x10, 5], 0), "s8"); // bipush
        assert_eq!(operand_form(&[0x12, 3], 0), "cp8"); // ldc
        assert_eq!(operand_form(&[0x15, 2], 0), "local:u8"); // iload
        assert_eq!(operand_form(&[0xa7, 0xff, 0xf0], 0), "branch:s16"); // goto
        assert_eq!(operand_form(&[0xbc, 10], 0), "atype:int"); // newarray int
        assert_eq!(operand_form(&[0xbc, 7], 0), "atype:double");
        assert_eq!(operand_form(&[0xc5, 0, 3, 2], 0), "cp16,dims:2");
        assert_eq!(operand_form(&[0xc4, 0x84, 0, 1, 0, 1], 0), "wide:iinc");
        assert_eq!(operand_form(&[0xb9, 0, 3, 1, 0], 0), "cp16,count");
    }

    #[test]
    fn scans_a_known_class_and_reports_it_correctly() {
        // `iload_0; bipush 7; iadd; ireturn` — four opcodes, three forms.
        let class = class_with_code(&[0x1a, 0x10, 0x07, 0x60, 0xac], 0);
        let scan = scan_class_bytes("Known", &class).expect("well-formed class");
        assert_eq!(scan.program, "Known");
        assert_eq!(scan.methods, 1);
        assert_eq!(scan.sites.len(), 4);
        assert!(scan.sites.contains_key("26:implicit")); // iload_0
        assert!(scan.sites.contains_key("16:s8")); // bipush
        assert!(scan.sites.contains_key("96:implicit")); // iadd
        assert!(scan.sites.contains_key("172:implicit")); // ireturn
                                                          // No handlers, no back-edge.
        for facts in scan.sites.values() {
            assert!(!facts.in_exception_method);
            assert!(!facts.in_loop_method);
        }
    }

    #[test]
    fn a_backward_branch_marks_the_method_osr_eligible() {
        // `iload_0; ifne -2; return` — the `ifne` jumps backwards.
        let class = class_with_code(&[0x1a, 0x9a, 0xff, 0xfe, 0xb1], 0);
        let scan = scan_class_bytes("Loop", &class).expect("well-formed");
        assert!(scan.sites.values().all(|f| f.in_loop_method));
        // A forward branch does not.
        let fwd = class_with_code(&[0x1a, 0x9a, 0x00, 0x03, 0xb1], 0);
        let scan = scan_class_bytes("Fwd", &fwd).expect("well-formed");
        assert!(scan.sites.values().all(|f| !f.in_loop_method));
    }

    #[test]
    fn a_handler_table_and_athrow_both_reach_the_exception_axis() {
        let with_handler = class_with_code(&[0x01, 0xb1], 1);
        let scan = scan_class_bytes("Handled", &with_handler).expect("well-formed");
        assert!(scan.sites.values().all(|f| f.in_exception_method));

        // No handler table, but `athrow` is itself how an exception is raised.
        let throws = class_with_code(&[0x01, 0xbf], 0);
        let scan = scan_class_bytes("Throws", &throws).expect("well-formed");
        assert!(scan.sites["191:implicit"].in_exception_method, "athrow");
        assert!(!scan.sites["1:implicit"].in_exception_method, "aconst_null");
    }

    #[test]
    fn a_malformed_class_is_reported_not_guessed_at() {
        assert!(scan_class_bytes("X", b"not a class").is_none());
        assert!(scan_class_bytes("X", &[0xCA, 0xFE, 0xBA, 0xBE]).is_none());
    }

    /// A generator index that knows exactly the opcodes in the hand-built
    /// fixtures below, plus one declared-unreachable opcode — so the three cell
    /// states are all reachable from a corpus of one tiny class.
    fn known_generators() -> GeneratorIndex {
        let mut g = GeneratorIndex::new();
        for op in [0x1a, 0x10, 0x60, 0xac] {
            g.insert(op, GeneratorStatus::Generated);
        }
        g.insert(
            0xa8,
            GeneratorStatus::Unreachable {
                reason: "javac has not emitted `jsr` since Java 5, and JVMS 4.9.1 forbids it in \
                         class files >= 50.0"
                    .to_string(),
            },
        );
        g
    }

    #[test]
    fn the_matrix_reports_a_known_corpus_correctly() {
        let class = class_with_code(&[0x1a, 0x10, 0x07, 0x60, 0xac], 0);
        let scan = scan_class_bytes("Known", &class).expect("well-formed");
        let modes = vec![Mode::NoJit, Mode::IrJit];
        let m = CoverageMatrix::build(vec![scan], &modes, Vec::new(), &known_generators());

        assert_eq!(m.schema_version, MATRIX_SCHEMA_VERSION);
        assert_eq!(m.modes, vec!["nojit", "ir-jit"]);
        assert_eq!(m.rows.len(), 4);
        assert_eq!(m.programs.len(), 1);

        let iadd = m.rows.iter().find(|r| r.opcode == 0x60).expect("iadd row");
        assert_eq!(iadd.name, "iadd");
        assert_eq!(iadd.form, "implicit");
        assert_eq!(iadd.programs, vec!["Known".to_string()]);
        // `nojit` drives interpreter-fast; `ir-jit` drives ir-jit. Neither
        // drives the decoded interpreter, the direct emitter, OSR or deopt.
        assert_eq!(iadd.axes["interpreter-fast"], true);
        assert_eq!(iadd.axes["ir-jit"], true);
        assert_eq!(iadd.axes["interpreter-decoded"], false);
        assert_eq!(iadd.axes["direct-jit"], false);
        assert_eq!(iadd.axes["osr"], false);
        assert_eq!(iadd.axes["deopt"], false);
        // The exception axis is mode-driven but code-gated: this method has no
        // handler and no athrow.
        assert_eq!(iadd.axes["exception"], false);

        assert!(m.fully_covered_rows() == 0);
        assert!(iadd.uncovered().contains(&"direct-jit"));

        // Mode-side gaps are named separately from code-side ones.
        assert_eq!(
            m.axes_without_a_mode,
            vec![
                "interpreter-decoded".to_string(),
                "direct-jit".to_string(),
                "osr".to_string(),
                "deopt".to_string()
            ]
        );

        // Everything the corpus does not contain is listed by name.
        assert!(m
            .unexercised_opcodes
            .iter()
            .any(|u| u.name == "invokedynamic"));
        assert!(!m.unexercised_opcodes.iter().any(|u| u.name == "iadd"));
        assert_eq!(m.unexercised_opcodes.len(), 202 - 4);

        // -- the per-opcode grid, exactly ----------------------------------
        // Every named opcode has a row, whether or not the corpus contains it.
        assert_eq!(m.opcodes.len(), 202);
        assert_eq!(m.cells.total(), 202 * PathAxis::all().len());

        let iadd = m.opcodes.iter().find(|o| o.opcode == 0x60).expect("iadd");
        assert_eq!(iadd.forms, vec!["implicit".to_string()]);
        assert_eq!(iadd.programs, vec!["Known".to_string()]);
        assert!(iadd.cells["interpreter-fast"].is_covered());
        assert!(iadd.cells["ir-jit"].is_covered());
        // Four axes have no mode; two are code-side gaps under the two that do.
        assert_eq!(
            iadd.cells["direct-jit"],
            CellState::uncovered(Gap::NoModeDrivesAxis)
        );
        assert_eq!(
            iadd.cells["osr"],
            CellState::uncovered(Gap::NoModeDrivesAxis)
        );
        assert_eq!(
            iadd.cells["exception"],
            CellState::uncovered(Gap::NoHandlerSite)
        );
        assert!(!iadd.fully_covered());

        // `jsr` is declared unreachable: every axis carries the reason, and it
        // is never mixed in with the ordinary gaps.
        let jsr = m.opcodes.iter().find(|o| o.opcode == 0xa8).expect("jsr");
        assert!(jsr.fully_unreachable());
        assert!(jsr.gaps().is_empty(), "unreachable is not a gap");
        match &jsr.cells["ir-jit"] {
            CellState::UnreachableByConstruction { reason } => assert!(reason.contains("4.9.1")),
            other => panic!("jsr must be unreachable, got {other:?}"),
        }
        assert_eq!(m.unreachable_opcodes(), 1);
        assert_eq!(m.cells.unreachable, PathAxis::all().len());

        // An opcode the generator claims but the corpus lacks is a *different*
        // gap from one nothing can produce — different fix, different owner.
        let ireturn_ok = m.opcodes.iter().find(|o| o.opcode == 0xac).unwrap();
        assert!(ireturn_ok.cells["interpreter-fast"].is_covered());
        let dup2_x2 = m.opcodes.iter().find(|o| o.opcode == 0x5e).unwrap();
        assert_eq!(
            dup2_x2.cells["interpreter-fast"],
            CellState::uncovered(Gap::NoGenerator)
        );

        // The gap render names the fix, not just the fact.
        let gaps = m.render_gaps();
        assert!(gaps.contains("fix: write a generator recipe"));
        assert!(gaps.contains("UNREACHABLE jsr"));
        assert!(m.render().contains("cells:"));
    }

    #[test]
    fn an_opcode_with_no_generator_is_reported_uncovered_not_omitted() {
        // The failure this guards against: a report that only lists what it has
        // seen. With an empty generator index and an empty corpus, all 202
        // opcodes must still appear, each uncovered on every axis, each naming
        // "no generator" as the reason.
        let m = CoverageMatrix::build(
            Vec::new(),
            Mode::execution_paths(),
            Vec::new(),
            &GeneratorIndex::new(),
        );
        assert_eq!(m.opcodes.len(), 202);
        assert_eq!(m.fully_covered_opcodes(), 0);
        assert_eq!(m.unreachable_opcodes(), 0);
        assert_eq!(m.cells.uncovered, 202 * PathAxis::all().len());
        assert_eq!(m.cells.covered, 0);
        for report in &m.opcodes {
            assert!(report.generator.is_none());
            for (axis, gap) in report.gaps() {
                assert_eq!(gap, Gap::NoGenerator, "{} / {axis}", report.name);
                assert!(gap.fix().contains("opcorpus.rs"));
            }
        }
        // Including the ones that are unreachable *in practice*: without a
        // declaration the matrix must not invent one.
        let swap = m.opcodes.iter().find(|o| o.opcode == 0x5f).unwrap();
        assert!(!swap.fully_unreachable());
    }

    #[test]
    fn corpus_evidence_overrides_an_unreachable_declaration() {
        // A mutated class really can carry a `swap`. If the bytes have it, the
        // report must say covered and flag the stale declaration, rather than
        // insisting the opcode cannot exist.
        let class = class_with_code(&[0x5f, 0xb1], 0);
        let scan = scan_class_bytes("Mutant", &class).expect("well-formed");
        let mut generators = GeneratorIndex::new();
        generators.insert(
            0x5f,
            GeneratorStatus::Unreachable {
                reason: "javac never emits swap".to_string(),
            },
        );
        let m = CoverageMatrix::build(vec![scan], Mode::execution_paths(), Vec::new(), &generators);
        let swap = m.opcodes.iter().find(|o| o.opcode == 0x5f).unwrap();
        assert!(swap.cells["interpreter-fast"].is_covered());
        assert!(!swap.fully_unreachable());
        assert_eq!(m.reconciliation.len(), 1);
        assert!(m.reconciliation[0].note.contains("stale"));
        assert!(m.render().contains("RECONCILE swap"));
    }

    #[test]
    fn a_generator_whose_opcode_is_missing_from_the_corpus_is_reconciled() {
        // The other direction: the recipe stopped working (a javac lowering
        // changed), so the claim is louder than the evidence.
        let mut generators = GeneratorIndex::new();
        generators.insert(0xba, GeneratorStatus::Generated);
        let m = CoverageMatrix::build(Vec::new(), Mode::execution_paths(), Vec::new(), &generators);
        assert_eq!(m.reconciliation.len(), 1);
        assert_eq!(m.reconciliation[0].name, "invokedynamic");
        let indy = m.opcodes.iter().find(|o| o.opcode == 0xba).unwrap();
        assert_eq!(
            indy.cells["ir-jit"],
            CellState::uncovered(Gap::NotInCorpusButGenerated)
        );
        assert!(m.render_gaps().contains("fix: regenerate the corpus"));
    }

    #[test]
    fn the_full_execution_path_list_drives_every_axis() {
        // The point of the mode axis: with the contract's `--modes` list, no
        // axis is a *mode-side* gap, so every remaining gap is the corpus's.
        let class = class_with_code(&[0x1a, 0x60, 0xac], 0);
        let scan = scan_class_bytes("K", &class).expect("well-formed");
        let m = CoverageMatrix::build(
            vec![scan],
            Mode::execution_paths(),
            Vec::new(),
            &GeneratorIndex::new(),
        );
        assert!(
            m.axes_without_a_mode.is_empty(),
            "execution_paths() must drive all seven axes, missing {:?}",
            m.axes_without_a_mode
        );
        // With every axis driven, no cell can carry a mode-side gap.
        for report in &m.opcodes {
            for (_, gap) in report.gaps() {
                assert_ne!(gap, Gap::NoModeDrivesAxis, "{}", report.name);
            }
        }
    }

    #[test]
    fn the_matrix_round_trips_through_json() {
        let class = class_with_code(&[0x1a, 0x60, 0xac], 0);
        let scan = scan_class_bytes("K", &class).expect("well-formed");
        let m = CoverageMatrix::build(vec![scan], &[Mode::NoJit], Vec::new(), &known_generators());
        let json = m.to_json().expect("serialize");
        let back: CoverageMatrix = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(back.rows.len(), m.rows.len());
        assert_eq!(back.modes, m.modes);
        assert_eq!(back.opcodes.len(), m.opcodes.len());
        assert_eq!(back.cells, m.cells);
        assert_eq!(back.reconciliation, m.reconciliation);
        assert!(m.render().contains("opcode/path matrix"));
        assert!(m.render().contains("AXIS GAP"));
    }

    #[test]
    fn unscannable_programs_are_surfaced_in_the_artifact() {
        let m = CoverageMatrix::build(
            Vec::new(),
            &[Mode::NoJit],
            vec![("Broken".into(), "not a well-formed class file".into())],
            &GeneratorIndex::new(),
        );
        assert_eq!(m.unscannable.len(), 1);
        assert!(m.render().contains("UNSCANNED Broken"));
        // A corpus that scanned nothing must not read as "everything covered".
        assert_eq!(m.rows.len(), 0);
        assert_eq!(m.unexercised_opcodes.len(), 202);
        assert_eq!(m.fully_covered_opcodes(), 0);
    }

    #[test]
    fn axes_for_is_conservative_about_the_interpreter() {
        // Every JIT-enabled mode still interprets until a method is hot, so it
        // claims interpreter-fast. Only `interp-decoded` claims the decoded
        // fallback, and only it passes `--noverify`.
        for m in Mode::all() {
            let axes = axes_for(*m);
            if *m == Mode::InterpDecoded {
                assert!(
                    axes.contains(&PathAxis::InterpreterDecoded),
                    "{}",
                    m.label()
                );
                assert!(!axes.contains(&PathAxis::InterpreterFast), "{}", m.label());
            } else {
                assert!(axes.contains(&PathAxis::InterpreterFast), "{}", m.label());
                assert!(
                    !axes.contains(&PathAxis::InterpreterDecoded),
                    "{} must not claim the decoded fallback",
                    m.label()
                );
            }
        }
        assert!(axes_for(Mode::ForcedDeopt).contains(&PathAxis::Deopt));
        assert!(axes_for(Mode::OsrEager).contains(&PathAxis::Osr));
        assert!(!axes_for(Mode::NoOsr).contains(&PathAxis::Osr));
    }
}
