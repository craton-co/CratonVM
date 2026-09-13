// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! A small, **typed** class-file assembler for the bytecode differential fuzzer
//! ([`crate::jitfuzz`]).
//!
//! `javac` cannot produce the bytecode shapes a JIT gets wrong on demand: it
//! never emits `swap`, emits `dup2_x2` only in compound assignments to `long[]`
//! elements, and never uses `wide` on a small local. So the fuzzer writes class
//! files directly, and this module is what makes that safe to do without a JVM
//! in the loop.
//!
//! ## Why the assembler tracks types
//!
//! A generated class that fails verification is not a finding, it is noise —
//! and it is noise that looks like a finding (`nojit` throws `VerifyError`).
//! Every emitter here therefore carries the instruction's **type effect**, and
//! the assembler runs a simplified JVMS §4.10.1 type checker as it goes:
//!
//! * every pop is checked against the expected verification type;
//! * every branch records the (locals, stack) state at the jump and checks it is
//!   assignable to the target label's declared frame when the method finishes;
//! * falling through into a label checks the same;
//! * an instruction that follows an unconditional transfer must be a label with
//!   a declared frame (JVMS: a `StackMapTable` frame is mandatory there);
//! * every instruction inside an exception range records its locals, which are
//!   checked against the handler's frame.
//!
//! The first violation is latched and returned by [`Asm::finish`], so a
//! generator bug fails a unit test in `cargo test` instead of failing
//! verification at run time on every mode at once.
//!
//! Frames are always emitted as `full_frame` (tag 255). That is larger than
//! `javac`'s compressed forms, but it is the one encoding with no
//! delta-against-previous-frame state to get wrong.

use std::fmt::Write as _;

// ---------------------------------------------------------------------------
// Opcodes
// ---------------------------------------------------------------------------

/// JVMS opcode values used by the generator.
#[allow(missing_docs)]
pub mod op {
    pub const ACONST_NULL: u8 = 0x01;
    pub const ICONST_M1: u8 = 0x02;
    pub const ICONST_0: u8 = 0x03;
    pub const LCONST_0: u8 = 0x09;
    pub const LCONST_1: u8 = 0x0a;
    pub const FCONST_0: u8 = 0x0b;
    pub const FCONST_1: u8 = 0x0c;
    pub const FCONST_2: u8 = 0x0d;
    pub const DCONST_0: u8 = 0x0e;
    pub const DCONST_1: u8 = 0x0f;
    pub const BIPUSH: u8 = 0x10;
    pub const SIPUSH: u8 = 0x11;
    pub const LDC: u8 = 0x12;
    pub const LDC_W: u8 = 0x13;
    pub const LDC2_W: u8 = 0x14;
    pub const ILOAD: u8 = 0x15;
    pub const LLOAD: u8 = 0x16;
    pub const FLOAD: u8 = 0x17;
    pub const DLOAD: u8 = 0x18;
    pub const ALOAD: u8 = 0x19;
    pub const IALOAD: u8 = 0x2e;
    pub const DALOAD: u8 = 0x31;
    pub const ISTORE: u8 = 0x36;
    pub const LSTORE: u8 = 0x37;
    pub const FSTORE: u8 = 0x38;
    pub const DSTORE: u8 = 0x39;
    pub const ASTORE: u8 = 0x3a;
    pub const IASTORE: u8 = 0x4f;
    pub const DASTORE: u8 = 0x52;
    pub const POP: u8 = 0x57;
    pub const POP2: u8 = 0x58;
    pub const DUP: u8 = 0x59;
    pub const DUP_X1: u8 = 0x5a;
    pub const DUP_X2: u8 = 0x5b;
    pub const DUP2: u8 = 0x5c;
    pub const DUP2_X1: u8 = 0x5d;
    pub const DUP2_X2: u8 = 0x5e;
    pub const SWAP: u8 = 0x5f;
    pub const IADD: u8 = 0x60;
    pub const LADD: u8 = 0x61;
    pub const FADD: u8 = 0x62;
    pub const DADD: u8 = 0x63;
    pub const ISUB: u8 = 0x64;
    pub const LSUB: u8 = 0x65;
    pub const IMUL: u8 = 0x68;
    pub const LMUL: u8 = 0x69;
    pub const FMUL: u8 = 0x6a;
    pub const DMUL: u8 = 0x6b;
    pub const IDIV: u8 = 0x6c;
    pub const LDIV: u8 = 0x6d;
    pub const IREM: u8 = 0x70;
    pub const LREM: u8 = 0x71;
    pub const FREM: u8 = 0x72;
    pub const DREM: u8 = 0x73;
    pub const INEG: u8 = 0x74;
    pub const LNEG: u8 = 0x75;
    pub const FNEG: u8 = 0x76;
    pub const DNEG: u8 = 0x77;
    pub const ISHL: u8 = 0x78;
    pub const LSHL: u8 = 0x79;
    pub const ISHR: u8 = 0x7a;
    pub const LSHR: u8 = 0x7b;
    pub const IUSHR: u8 = 0x7c;
    pub const LUSHR: u8 = 0x7d;
    pub const IAND: u8 = 0x7e;
    pub const LAND: u8 = 0x7f;
    pub const IOR: u8 = 0x80;
    pub const LOR: u8 = 0x81;
    pub const IXOR: u8 = 0x82;
    pub const LXOR: u8 = 0x83;
    pub const IINC: u8 = 0x84;
    pub const I2L: u8 = 0x85;
    pub const I2F: u8 = 0x86;
    pub const I2D: u8 = 0x87;
    pub const L2I: u8 = 0x88;
    pub const L2F: u8 = 0x89;
    pub const L2D: u8 = 0x8a;
    pub const F2I: u8 = 0x8b;
    pub const F2L: u8 = 0x8c;
    pub const F2D: u8 = 0x8d;
    pub const D2I: u8 = 0x8e;
    pub const D2L: u8 = 0x8f;
    pub const D2F: u8 = 0x90;
    pub const I2B: u8 = 0x91;
    pub const I2C: u8 = 0x92;
    pub const I2S: u8 = 0x93;
    pub const LCMP: u8 = 0x94;
    pub const FCMPL: u8 = 0x95;
    pub const FCMPG: u8 = 0x96;
    pub const DCMPL: u8 = 0x97;
    pub const DCMPG: u8 = 0x98;
    pub const IFEQ: u8 = 0x99;
    pub const IFNE: u8 = 0x9a;
    pub const IFLT: u8 = 0x9b;
    pub const IFGE: u8 = 0x9c;
    pub const IFGT: u8 = 0x9d;
    pub const IFLE: u8 = 0x9e;
    pub const IF_ICMPEQ: u8 = 0x9f;
    pub const IF_ICMPNE: u8 = 0xa0;
    pub const IF_ICMPLT: u8 = 0xa1;
    pub const IF_ICMPGE: u8 = 0xa2;
    pub const IF_ICMPGT: u8 = 0xa3;
    pub const IF_ICMPLE: u8 = 0xa4;
    pub const IF_ACMPEQ: u8 = 0xa5;
    pub const IF_ACMPNE: u8 = 0xa6;
    pub const GOTO: u8 = 0xa7;
    pub const TABLESWITCH: u8 = 0xaa;
    pub const LOOKUPSWITCH: u8 = 0xab;
    pub const IRETURN: u8 = 0xac;
    pub const LRETURN: u8 = 0xad;
    pub const RETURN: u8 = 0xb1;
    pub const GETSTATIC: u8 = 0xb2;
    pub const INVOKEVIRTUAL: u8 = 0xb6;
    pub const INVOKESTATIC: u8 = 0xb8;
    pub const NEWARRAY: u8 = 0xbc;
    pub const ARRAYLENGTH: u8 = 0xbe;
    pub const ATHROW: u8 = 0xbf;
    pub const CHECKCAST: u8 = 0xc0;
    pub const WIDE: u8 = 0xc4;
    pub const IFNULL: u8 = 0xc6;
    pub const IFNONNULL: u8 = 0xc7;

    /// `newarray` element-type operands.
    pub const T_DOUBLE: u8 = 7;
    pub const T_INT: u8 = 10;
}

// ---------------------------------------------------------------------------
// Verification types and frames
// ---------------------------------------------------------------------------

/// A JVMS verification type (§4.10.1.2), restricted to what the generator uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VType {
    /// An unusable slot.
    Top,
    Int,
    Float,
    /// Category 2: occupies two local slots / two operand-stack words.
    Long,
    /// Category 2.
    Double,
    /// The type of `aconst_null`.
    Null,
    /// A class or array type by internal name (`java/lang/Object`, `[I`).
    Object(String),
}

impl VType {
    /// A reference type by internal name.
    pub fn obj(name: &str) -> VType {
        VType::Object(name.to_string())
    }

    /// Operand-stack words / local slots this type occupies.
    pub fn size(&self) -> usize {
        match self {
            VType::Long | VType::Double => 2,
            _ => 1,
        }
    }

    /// Category 2 (`long` / `double`).
    pub fn is_cat2(&self) -> bool {
        self.size() == 2
    }

    /// A reference (including `null`).
    pub fn is_reference(&self) -> bool {
        matches!(self, VType::Null | VType::Object(_))
    }

    /// Whether a value of type `self` may be used where `to` is expected.
    ///
    /// Deliberately narrower than the JVMS relation: the generator only ever
    /// needs "equal", "anything to `Top`", "`null` to any reference", and "any
    /// reference to `java/lang/Object`". Anything else is reported as a
    /// generator bug rather than accepted on a guess about the class hierarchy.
    pub fn assignable_to(&self, to: &VType) -> bool {
        if self == to || *to == VType::Top {
            return true;
        }
        match (self, to) {
            (VType::Null, VType::Object(_)) => true,
            (VType::Object(_), VType::Object(t)) => t == "java/lang/Object",
            _ => false,
        }
    }
}

/// A declared stack-map frame, in **slot** representation: a category-2 local
/// at slot `n` is `Long`/`Double` at `n` followed by `Top` at `n + 1`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub locals: Vec<VType>,
    pub stack: Vec<VType>,
}

impl Frame {
    /// The JVMS `locals` list: category-2 types listed once, not twice.
    fn locals_list(&self) -> Vec<VType> {
        let mut out = Vec::new();
        let mut i = 0usize;
        while i < self.locals.len() {
            let t = &self.locals[i];
            out.push(t.clone());
            i += t.size();
        }
        // Trailing `Top`s carry no information; dropping them keeps frames
        // for the high-`wide`-slot layout from being mostly padding at the end.
        while out.last() == Some(&VType::Top) {
            out.pop();
        }
        out
    }
}

/// Whether a (locals, stack) state may flow into `frame`.
fn state_assignable(locals: &[VType], stack: &[VType], frame: &Frame) -> Result<(), String> {
    if stack.len() != frame.stack.len() {
        return Err(format!(
            "stack depth {} flows into a frame declaring {} ({:?} vs {:?})",
            stack.len(),
            frame.stack.len(),
            stack,
            frame.stack
        ));
    }
    for (i, (have, want)) in stack.iter().zip(&frame.stack).enumerate() {
        if !have.assignable_to(want) {
            return Err(format!("stack[{i}] is {have:?}, frame declares {want:?}"));
        }
    }
    for (slot, want) in frame.locals.iter().enumerate() {
        let have = locals.get(slot).unwrap_or(&VType::Top);
        if !have.assignable_to(want) {
            return Err(format!("local {slot} is {have:?}, frame declares {want:?}"));
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Constant pool
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum CpEntry {
    Utf8(String),
    Integer(i32),
    Float(u32),
    Long(i64),
    Double(u64),
    Class(u16),
    String(u16),
    NameAndType(u16, u16),
    Fieldref(u16, u16),
    Methodref(u16, u16),
}

/// A deduplicating constant pool. Iteration order is insertion order, so the
/// same generator calls always produce the same bytes.
#[derive(Debug, Default)]
pub struct ConstantPool {
    /// `(entry, index)`; category-2 entries consume the following index too.
    entries: Vec<(CpEntry, u16)>,
    next: u16,
}

impl ConstantPool {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            next: 1,
        }
    }

    fn intern(&mut self, e: CpEntry) -> u16 {
        if let Some((_, idx)) = self.entries.iter().find(|(x, _)| *x == e) {
            return *idx;
        }
        let idx = self.next;
        let width = if matches!(e, CpEntry::Long(_) | CpEntry::Double(_)) {
            2
        } else {
            1
        };
        self.next += width;
        self.entries.push((e, idx));
        idx
    }

    pub fn utf8(&mut self, s: &str) -> u16 {
        assert!(s.is_ascii(), "generator constant pool is ASCII-only: {s:?}");
        self.intern(CpEntry::Utf8(s.to_string()))
    }
    pub fn class(&mut self, internal_name: &str) -> u16 {
        let n = self.utf8(internal_name);
        self.intern(CpEntry::Class(n))
    }
    pub fn string(&mut self, s: &str) -> u16 {
        let n = self.utf8(s);
        self.intern(CpEntry::String(n))
    }
    pub fn integer(&mut self, v: i32) -> u16 {
        self.intern(CpEntry::Integer(v))
    }
    pub fn float(&mut self, v: f32) -> u16 {
        self.intern(CpEntry::Float(v.to_bits()))
    }
    pub fn long(&mut self, v: i64) -> u16 {
        self.intern(CpEntry::Long(v))
    }
    pub fn double(&mut self, v: f64) -> u16 {
        self.intern(CpEntry::Double(v.to_bits()))
    }
    fn name_and_type(&mut self, name: &str, desc: &str) -> u16 {
        let n = self.utf8(name);
        let d = self.utf8(desc);
        self.intern(CpEntry::NameAndType(n, d))
    }
    pub fn field_ref(&mut self, owner: &str, name: &str, desc: &str) -> u16 {
        let c = self.class(owner);
        let nt = self.name_and_type(name, desc);
        self.intern(CpEntry::Fieldref(c, nt))
    }
    pub fn method_ref(&mut self, owner: &str, name: &str, desc: &str) -> u16 {
        let c = self.class(owner);
        let nt = self.name_and_type(name, desc);
        self.intern(CpEntry::Methodref(c, nt))
    }

    /// `constant_pool_count` followed by the entries.
    fn write(&self, out: &mut Vec<u8>) {
        put_u16(out, self.next);
        for (e, _) in &self.entries {
            match e {
                CpEntry::Utf8(s) => {
                    out.push(1);
                    put_u16(out, s.len() as u16);
                    out.extend_from_slice(s.as_bytes());
                }
                CpEntry::Integer(v) => {
                    out.push(3);
                    put_u32(out, *v as u32);
                }
                CpEntry::Float(bits) => {
                    out.push(4);
                    put_u32(out, *bits);
                }
                CpEntry::Long(v) => {
                    out.push(5);
                    put_u32(out, ((*v as u64) >> 32) as u32);
                    put_u32(out, *v as u64 as u32);
                }
                CpEntry::Double(bits) => {
                    out.push(6);
                    put_u32(out, (bits >> 32) as u32);
                    put_u32(out, *bits as u32);
                }
                CpEntry::Class(n) => {
                    out.push(7);
                    put_u16(out, *n);
                }
                CpEntry::String(n) => {
                    out.push(8);
                    put_u16(out, *n);
                }
                CpEntry::Fieldref(c, nt) => {
                    out.push(9);
                    put_u16(out, *c);
                    put_u16(out, *nt);
                }
                CpEntry::Methodref(c, nt) => {
                    out.push(10);
                    put_u16(out, *c);
                    put_u16(out, *nt);
                }
                CpEntry::NameAndType(n, d) => {
                    out.push(12);
                    put_u16(out, *n);
                    put_u16(out, *d);
                }
            }
        }
    }
}

fn put_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_be_bytes());
}
fn put_u32(out: &mut Vec<u8>, v: u32) {
    out.extend_from_slice(&v.to_be_bytes());
}

/// Parse a method descriptor into `(params, return)`; `None` return is `V`.
pub fn parse_method_descriptor(desc: &str) -> Option<(Vec<VType>, Option<VType>)> {
    let b = desc.as_bytes();
    if b.first() != Some(&b'(') {
        return None;
    }
    let mut i = 1usize;
    let mut params = Vec::new();
    while *b.get(i)? != b')' {
        let (t, next) = parse_field_type(b, i)?;
        params.push(t);
        i = next;
    }
    i += 1;
    if b.get(i) == Some(&b'V') && i + 1 == b.len() {
        return Some((params, None));
    }
    let (ret, end) = parse_field_type(b, i)?;
    (end == b.len()).then_some((params, Some(ret)))
}

fn parse_field_type(b: &[u8], i: usize) -> Option<(VType, usize)> {
    Some(match *b.get(i)? {
        b'B' | b'C' | b'I' | b'S' | b'Z' => (VType::Int, i + 1),
        b'F' => (VType::Float, i + 1),
        b'J' => (VType::Long, i + 1),
        b'D' => (VType::Double, i + 1),
        b'L' => {
            let end = i + b[i..].iter().position(|&c| c == b';')?;
            let name = std::str::from_utf8(&b[i + 1..end]).ok()?;
            (VType::obj(name), end + 1)
        }
        b'[' => {
            let mut j = i;
            while *b.get(j)? == b'[' {
                j += 1;
            }
            let (_, end) = parse_field_type(b, j)?;
            let name = std::str::from_utf8(&b[i..end]).ok()?;
            (VType::obj(name), end)
        }
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// Assembler
// ---------------------------------------------------------------------------

/// A branch target. Created by [`Asm::new_label`], bound once by
/// [`Asm::bind`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Label(usize);

struct Fixup {
    /// Offset of the offset field in `code`.
    at: usize,
    /// The pc the offset is relative to (the branch instruction's own pc).
    base: usize,
    label: Label,
    wide: bool,
}

struct PendingEdge {
    label: Label,
    locals: Vec<VType>,
    stack: Vec<VType>,
    pc: usize,
}

struct TryRange {
    start: usize,
    end: Option<usize>,
    handler: Label,
    catch_class: String,
    /// Distinct locals states observed inside the range.
    states: Vec<Vec<VType>>,
}

/// A finished `Code` attribute body plus what the class writer needs to wrap it.
#[derive(Debug, Clone)]
pub struct CodeBody {
    pub max_stack: u16,
    pub max_locals: u16,
    pub code: Vec<u8>,
    /// `(start, end, handler, catch_type cp index)`.
    pub handlers: Vec<(u16, u16, u16, u16)>,
    /// `(pc, frame)`, sorted, one per pc.
    pub frames: Vec<(u16, Frame)>,
}

/// The typed assembler for one method. See the module docs for what it checks.
pub struct Asm<'cp> {
    cp: &'cp mut ConstantPool,
    code: Vec<u8>,
    max_locals: usize,
    return_type: Option<VType>,
    locals: Vec<VType>,
    stack: Vec<VType>,
    max_stack: usize,
    reachable: bool,
    labels: Vec<Option<(usize, Frame)>>,
    fixups: Vec<Fixup>,
    edges: Vec<PendingEdge>,
    tries: Vec<TryRange>,
    error: Option<String>,
}

impl<'cp> Asm<'cp> {
    /// Start a static method with descriptor `desc` and `max_locals` slots.
    pub fn new_static(cp: &'cp mut ConstantPool, desc: &str, max_locals: usize) -> Self {
        let (params, ret) = parse_method_descriptor(desc).expect("valid method descriptor");
        let mut locals = Vec::new();
        for p in params {
            let cat2 = p.is_cat2();
            locals.push(p);
            if cat2 {
                locals.push(VType::Top);
            }
        }
        let mut error = None;
        if locals.len() > max_locals {
            error = Some(format!(
                "parameters need {} slots, max_locals is {max_locals}",
                locals.len()
            ));
        }
        locals.resize(max_locals.max(locals.len()), VType::Top);
        Self {
            cp,
            code: Vec::new(),
            max_locals,
            return_type: ret,
            locals,
            stack: Vec::new(),
            max_stack: 0,
            reachable: true,
            labels: Vec::new(),
            fixups: Vec::new(),
            edges: Vec::new(),
            tries: Vec::new(),
            error: None,
        }
        .with_error(error)
    }

    fn with_error(mut self, e: Option<String>) -> Self {
        self.error = e;
        self
    }

    /// The constant pool this method interns into.
    pub fn cp(&mut self) -> &mut ConstantPool {
        self.cp
    }

    /// Current pc.
    pub fn pc(&self) -> usize {
        self.code.len()
    }

    /// The current abstract operand stack (bottom first).
    pub fn stack(&self) -> &[VType] {
        &self.stack
    }

    /// The first latched error, if any.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    fn fail(&mut self, msg: impl Into<String>) {
        if self.error.is_none() {
            let pc = self.code.len();
            self.error = Some(format!("pc {pc}: {}", msg.into()));
        }
    }

    // -- state helpers -------------------------------------------------------

    /// Called before emitting any instruction.
    fn begin_insn(&mut self) {
        if !self.reachable {
            self.fail("instruction after an unconditional transfer without a bound label");
            // Recover so one bug reports once.
            self.reachable = true;
        }
        self.record_try_state();
    }

    fn record_try_state(&mut self) {
        let snapshot = &self.locals;
        for t in self.tries.iter_mut().filter(|t| t.end.is_none()) {
            if !t.states.iter().any(|s| s == snapshot) {
                t.states.push(snapshot.clone());
            }
        }
    }

    fn pop(&mut self, want: &VType) {
        match self.stack.pop() {
            None => self.fail(format!("pop of {want:?} from an empty stack")),
            Some(have) => {
                let ok = match want {
                    // `Object("")` is used as "any reference".
                    VType::Object(n) if n.is_empty() => have.is_reference(),
                    _ => have.assignable_to(want),
                };
                if !ok {
                    self.fail(format!("expected {want:?} on the stack, found {have:?}"));
                }
            }
        }
    }

    fn pop_any(&mut self) -> Option<VType> {
        let v = self.stack.pop();
        if v.is_none() {
            self.fail("pop from an empty stack");
        }
        v
    }

    fn push(&mut self, t: VType) {
        self.stack.push(t);
        let depth: usize = self.stack.iter().map(VType::size).sum();
        self.max_stack = self.max_stack.max(depth);
    }

    fn check_slot(&mut self, slot: usize, t: &VType) -> bool {
        if slot + t.size() > self.max_locals {
            self.fail(format!(
                "local {slot} ({t:?}) is outside max_locals {}",
                self.max_locals
            ));
            return false;
        }
        true
    }

    // -- raw emission --------------------------------------------------------

    fn byte(&mut self, b: u8) {
        self.code.push(b);
    }
    fn u16(&mut self, v: u16) {
        put_u16(&mut self.code, v);
    }

    /// A no-operand instruction with an explicit type effect. `pops` lists the
    /// consumed values bottom-to-top.
    pub fn simple(&mut self, opcode: u8, pops: &[VType], pushes: &[VType]) {
        self.begin_insn();
        for t in pops.iter().rev() {
            self.pop(t);
        }
        for t in pushes {
            self.push(t.clone());
        }
        self.byte(opcode);
    }

    // -- constants -----------------------------------------------------------

    pub fn aconst_null(&mut self) {
        self.simple(op::ACONST_NULL, &[], &[VType::Null]);
    }

    pub fn iconst(&mut self, v: i32) {
        self.begin_insn();
        match v {
            -1..=5 => self.byte((op::ICONST_0 as i32 + v) as u8),
            -128..=127 => {
                self.byte(op::BIPUSH);
                self.byte(v as i8 as u8);
            }
            -32768..=32767 => {
                self.byte(op::SIPUSH);
                self.u16(v as i16 as u16);
            }
            _ => {
                let idx = self.cp.integer(v);
                self.ldc_index(idx);
            }
        }
        self.push(VType::Int);
    }

    pub fn lconst(&mut self, v: i64) {
        self.begin_insn();
        match v {
            0 => self.byte(op::LCONST_0),
            1 => self.byte(op::LCONST_1),
            _ => {
                let idx = self.cp.long(v);
                self.byte(op::LDC2_W);
                self.u16(idx);
            }
        }
        self.push(VType::Long);
    }

    pub fn fconst(&mut self, v: f32) {
        self.begin_insn();
        // Bit comparison, not `==`: `-0.0f == 0.0f`, and `fconst_0` is +0.0.
        match v.to_bits() {
            b if b == 0.0f32.to_bits() => self.byte(op::FCONST_0),
            b if b == 1.0f32.to_bits() => self.byte(op::FCONST_1),
            b if b == 2.0f32.to_bits() => self.byte(op::FCONST_2),
            _ => {
                let idx = self.cp.float(v);
                self.ldc_index(idx);
            }
        }
        self.push(VType::Float);
    }

    pub fn dconst(&mut self, v: f64) {
        self.begin_insn();
        match v.to_bits() {
            b if b == 0.0f64.to_bits() => self.byte(op::DCONST_0),
            b if b == 1.0f64.to_bits() => self.byte(op::DCONST_1),
            _ => {
                let idx = self.cp.double(v);
                self.byte(op::LDC2_W);
                self.u16(idx);
            }
        }
        self.push(VType::Double);
    }

    /// `ldc` of a `String` constant.
    pub fn sconst(&mut self, s: &str) {
        self.begin_insn();
        let idx = self.cp.string(s);
        self.ldc_index(idx);
        self.push(VType::obj("java/lang/String"));
    }

    fn ldc_index(&mut self, idx: u16) {
        if idx <= 0xff {
            self.byte(op::LDC);
            self.byte(idx as u8);
        } else {
            self.byte(op::LDC_W);
            self.u16(idx);
        }
    }

    // -- locals --------------------------------------------------------------

    fn local_op(&mut self, base: u8, short_base: Option<u8>, slot: usize, force_wide: bool) {
        if force_wide || slot > 0xff {
            self.byte(op::WIDE);
            self.byte(base);
            self.u16(slot as u16);
        } else if let (Some(sb), true) = (short_base, slot <= 3) {
            self.byte(sb + slot as u8);
        } else {
            self.byte(base);
            self.byte(slot as u8);
        }
    }

    /// Load `t` from `slot`. `force_wide` emits the `wide` prefix even for a
    /// slot that fits in a byte (legal, and a distinct decoder path).
    pub fn load(&mut self, t: VType, slot: usize, force_wide: bool) {
        self.begin_insn();
        if !self.check_slot(slot, &t) {
            return;
        }
        let have = self.locals[slot].clone();
        let (base, short) = match &t {
            VType::Int => (op::ILOAD, 0x1a),
            VType::Long => (op::LLOAD, 0x1e),
            VType::Float => (op::FLOAD, 0x22),
            VType::Double => (op::DLOAD, 0x26),
            _ => (op::ALOAD, 0x2a),
        };
        let ok = if t.is_reference() {
            have.is_reference()
        } else {
            have == t
        };
        if !ok {
            self.fail(format!("load of {t:?} from local {slot} holding {have:?}"));
        }
        self.local_op(base, Some(short), slot, force_wide);
        // A reference load pushes what the slot holds, not what was asked for.
        self.push(if t.is_reference() { have } else { t });
    }

    /// Store the stack top (of type `t`) into `slot`.
    pub fn store(&mut self, t: VType, slot: usize, force_wide: bool) {
        self.begin_insn();
        if !self.check_slot(slot, &t) {
            return;
        }
        let (base, short) = match &t {
            VType::Int => (op::ISTORE, 0x3b),
            VType::Long => (op::LSTORE, 0x3f),
            VType::Float => (op::FSTORE, 0x43),
            VType::Double => (op::DSTORE, 0x47),
            _ => (op::ASTORE, 0x4b),
        };
        let value = if t.is_reference() {
            match self.pop_any() {
                Some(v) if v.is_reference() => v,
                Some(v) => {
                    self.fail(format!("astore of non-reference {v:?}"));
                    v
                }
                None => VType::Top,
            }
        } else {
            self.pop(&t);
            t.clone()
        };
        // Overwriting the second half of a category-2 local kills it.
        if slot > 0 && self.locals[slot - 1].is_cat2() {
            self.locals[slot - 1] = VType::Top;
        }
        let cat2 = value.is_cat2();
        self.locals[slot] = value;
        if cat2 {
            self.locals[slot + 1] = VType::Top;
        }
        self.local_op(base, Some(short), slot, force_wide);
        // Handlers must accept the state *after* a store too.
        self.record_try_state();
    }

    /// `iinc slot delta`, widened when either operand needs it.
    pub fn iinc(&mut self, slot: usize, delta: i32, force_wide: bool) {
        self.begin_insn();
        if !self.check_slot(slot, &VType::Int) {
            return;
        }
        if self.locals[slot] != VType::Int {
            let have = self.locals[slot].clone();
            self.fail(format!("iinc of local {slot} holding {have:?}"));
        }
        if force_wide || slot > 0xff || !(-128..=127).contains(&delta) {
            if !(-32768..=32767).contains(&delta) {
                self.fail(format!("iinc delta {delta} does not fit in s16"));
            }
            self.byte(op::WIDE);
            self.byte(op::IINC);
            self.u16(slot as u16);
            self.u16(delta as i16 as u16);
        } else {
            self.byte(op::IINC);
            self.byte(slot as u8);
            self.byte(delta as i8 as u8);
        }
    }

    // -- stack shuffles (JVMS §6.5, category-checked) ------------------------

    fn top_cats(&self, n: usize) -> Option<Vec<bool>> {
        if self.stack.len() < n {
            return None;
        }
        Some(
            self.stack[self.stack.len() - n..]
                .iter()
                .map(VType::is_cat2)
                .collect(),
        )
    }

    fn shuffle(&mut self, opcode: u8, take: usize, rebuild: impl Fn(&[VType]) -> Vec<VType>) {
        self.begin_insn();
        if self.stack.len() < take {
            self.fail(format!("{opcode:#04x} needs {take} values on the stack"));
            return;
        }
        let top: Vec<VType> = self.stack.split_off(self.stack.len() - take);
        for t in rebuild(&top) {
            self.push(t);
        }
        self.byte(opcode);
    }

    pub fn pop1(&mut self) {
        match self.top_cats(1).as_deref() {
            Some([false]) => self.shuffle(op::POP, 1, |_| vec![]),
            _ => self.fail("pop needs a category-1 value"),
        }
    }

    pub fn dup(&mut self) {
        match self.top_cats(1).as_deref() {
            Some([false]) => self.shuffle(op::DUP, 1, |v| vec![v[0].clone(), v[0].clone()]),
            _ => self.fail("dup needs a category-1 value"),
        }
    }

    pub fn swap(&mut self) {
        match self.top_cats(2).as_deref() {
            Some([false, false]) => self.shuffle(op::SWAP, 2, |v| vec![v[1].clone(), v[0].clone()]),
            _ => self.fail("swap needs two category-1 values"),
        }
    }

    /// `pop2`: two category-1 values, or one category-2 value.
    pub fn pop2(&mut self) {
        if let Some([true]) = self.top_cats(1).as_deref() {
            self.shuffle(op::POP2, 1, |_| vec![]);
        } else if let Some([false, false]) = self.top_cats(2).as_deref() {
            self.shuffle(op::POP2, 2, |_| vec![]);
        } else {
            self.fail("pop2 form not satisfied");
        }
    }

    /// `dup_x1`: v2 v1 (both cat 1) → v1 v2 v1.
    pub fn dup_x1(&mut self) {
        match self.top_cats(2).as_deref() {
            Some([false, false]) => self.shuffle(op::DUP_X1, 2, |v| {
                vec![v[1].clone(), v[0].clone(), v[1].clone()]
            }),
            _ => self.fail("dup_x1 needs two category-1 values"),
        }
    }

    /// `dup_x2` form 1 (v3 v2 v1 all cat 1) or form 2 (v2 cat 2, v1 cat 1).
    pub fn dup_x2(&mut self) {
        if let Some([false, false, false]) = self.top_cats(3).as_deref() {
            self.shuffle(op::DUP_X2, 3, |v| {
                vec![v[2].clone(), v[0].clone(), v[1].clone(), v[2].clone()]
            });
        } else if let Some([true, false]) = self.top_cats(2).as_deref() {
            self.shuffle(op::DUP_X2, 2, |v| {
                vec![v[1].clone(), v[0].clone(), v[1].clone()]
            });
        } else {
            self.fail("dup_x2 form not satisfied");
        }
    }

    /// `dup2`: form 1 (two cat 1) or form 2 (one cat 2).
    pub fn dup2(&mut self) {
        if let Some([true]) = self.top_cats(1).as_deref() {
            self.shuffle(op::DUP2, 1, |v| vec![v[0].clone(), v[0].clone()]);
        } else if let Some([false, false]) = self.top_cats(2).as_deref() {
            self.shuffle(op::DUP2, 2, |v| {
                vec![v[0].clone(), v[1].clone(), v[0].clone(), v[1].clone()]
            });
        } else {
            self.fail("dup2 form not satisfied");
        }
    }

    /// `dup2_x1`: form 1 (v3 v2 v1 all cat 1) or form 2 (v2 cat 1, v1 cat 2).
    pub fn dup2_x1(&mut self) {
        if let Some([false, true]) = self.top_cats(2).as_deref() {
            self.shuffle(op::DUP2_X1, 2, |v| {
                vec![v[1].clone(), v[0].clone(), v[1].clone()]
            });
        } else if let Some([false, false, false]) = self.top_cats(3).as_deref() {
            self.shuffle(op::DUP2_X1, 3, |v| {
                vec![
                    v[1].clone(),
                    v[2].clone(),
                    v[0].clone(),
                    v[1].clone(),
                    v[2].clone(),
                ]
            });
        } else {
            self.fail("dup2_x1 form not satisfied");
        }
    }

    /// `dup2_x2`, all four JVMS forms.
    pub fn dup2_x2(&mut self) {
        // Form 4: v2 cat 2, v1 cat 2.
        if let Some([true, true]) = self.top_cats(2).as_deref() {
            self.shuffle(op::DUP2_X2, 2, |v| {
                vec![v[1].clone(), v[0].clone(), v[1].clone()]
            });
            return;
        }
        // Form 2: v3 v2 cat 1, v1 cat 2.
        if let Some([false, false, true]) = self.top_cats(3).as_deref() {
            self.shuffle(op::DUP2_X2, 3, |v| {
                vec![v[2].clone(), v[0].clone(), v[1].clone(), v[2].clone()]
            });
            return;
        }
        // Form 3: v3 cat 2, v2 v1 cat 1.
        if let Some([true, false, false]) = self.top_cats(3).as_deref() {
            self.shuffle(op::DUP2_X2, 3, |v| {
                vec![
                    v[1].clone(),
                    v[2].clone(),
                    v[0].clone(),
                    v[1].clone(),
                    v[2].clone(),
                ]
            });
            return;
        }
        // Form 1: four cat 1.
        if let Some([false, false, false, false]) = self.top_cats(4).as_deref() {
            self.shuffle(op::DUP2_X2, 4, |v| {
                vec![
                    v[2].clone(),
                    v[3].clone(),
                    v[0].clone(),
                    v[1].clone(),
                    v[2].clone(),
                    v[3].clone(),
                ]
            });
            return;
        }
        self.fail("dup2_x2 form not satisfied");
    }

    // -- references, arrays, invocations -------------------------------------

    pub fn newarray(&mut self, atype: u8) {
        let name = match atype {
            op::T_INT => "[I",
            op::T_DOUBLE => "[D",
            _ => {
                self.fail(format!("unsupported newarray atype {atype}"));
                return;
            }
        };
        self.begin_insn();
        self.pop(&VType::Int);
        self.push(VType::obj(name));
        self.byte(op::NEWARRAY);
        self.byte(atype);
    }

    pub fn arraylength(&mut self) {
        self.begin_insn();
        match self.pop_any() {
            Some(VType::Object(n)) if n.starts_with('[') => {}
            other => self.fail(format!("arraylength of {other:?}")),
        }
        self.push(VType::Int);
        self.byte(op::ARRAYLENGTH);
    }

    pub fn checkcast(&mut self, internal_name: &str) {
        self.begin_insn();
        self.pop(&VType::obj(""));
        let idx = self.cp.class(internal_name);
        self.push(VType::obj(internal_name));
        self.byte(op::CHECKCAST);
        self.u16(idx);
    }

    pub fn getstatic(&mut self, owner: &str, name: &str, desc: &str) {
        self.begin_insn();
        let Some((t, end)) = parse_field_type(desc.as_bytes(), 0) else {
            self.fail(format!("bad field descriptor {desc}"));
            return;
        };
        if end != desc.len() {
            self.fail(format!("bad field descriptor {desc}"));
        }
        let idx = self.cp.field_ref(owner, name, desc);
        self.push(t);
        self.byte(op::GETSTATIC);
        self.u16(idx);
    }

    fn invoke(&mut self, opcode: u8, owner: &str, name: &str, desc: &str, receiver: bool) {
        self.begin_insn();
        let Some((params, ret)) = parse_method_descriptor(desc) else {
            self.fail(format!("bad method descriptor {desc}"));
            return;
        };
        for p in params.iter().rev() {
            self.pop(p);
        }
        if receiver {
            self.pop(&VType::obj(owner));
        }
        if let Some(r) = ret {
            self.push(r);
        }
        let idx = self.cp.method_ref(owner, name, desc);
        self.byte(opcode);
        self.u16(idx);
    }

    pub fn invokestatic(&mut self, owner: &str, name: &str, desc: &str) {
        self.invoke(op::INVOKESTATIC, owner, name, desc, false);
    }

    pub fn invokevirtual(&mut self, owner: &str, name: &str, desc: &str) {
        self.invoke(op::INVOKEVIRTUAL, owner, name, desc, true);
    }

    // -- control flow --------------------------------------------------------

    pub fn new_label(&mut self) -> Label {
        self.labels.push(None);
        Label(self.labels.len() - 1)
    }

    /// Bind `label` here with its declared frame. A reachable fall-through is
    /// checked against the frame; the frame then becomes the current state.
    pub fn bind(&mut self, label: Label, frame: Frame) {
        if self.code.is_empty() {
            self.fail("a label at pc 0 would need an explicit frame at offset 0");
        }
        if self.labels[label.0].is_some() {
            self.fail(format!("label {} bound twice", label.0));
            return;
        }
        if frame.locals.len() > self.max_locals {
            self.fail("frame declares more locals than max_locals");
        }
        if self.reachable {
            if let Err(e) = state_assignable(&self.locals, &self.stack, &frame) {
                self.fail(format!("fall-through into label {}: {e}", label.0));
            }
        }
        self.labels[label.0] = Some((self.code.len(), frame.clone()));
        let mut locals = frame.locals.clone();
        locals.resize(self.max_locals, VType::Top);
        self.locals = locals;
        self.stack.clear();
        for t in frame.stack {
            self.push(t);
        }
        self.reachable = true;
    }

    fn edge(&mut self, label: Label, pc: usize) {
        self.edges.push(PendingEdge {
            label,
            locals: self.locals.clone(),
            stack: self.stack.clone(),
            pc,
        });
    }

    fn branch16(&mut self, opcode: u8, label: Label) {
        let pc = self.code.len();
        self.edge(label, pc);
        self.byte(opcode);
        self.fixups.push(Fixup {
            at: self.code.len(),
            base: pc,
            label,
            wide: false,
        });
        self.u16(0);
    }

    /// `ifeq` … `ifle`: pops one int.
    pub fn if_int(&mut self, opcode: u8, label: Label) {
        self.begin_insn();
        if !(op::IFEQ..=op::IFLE).contains(&opcode) {
            self.fail(format!("{opcode:#04x} is not a unary int branch"));
        }
        self.pop(&VType::Int);
        self.branch16(opcode, label);
    }

    /// `if_icmpeq` … `if_icmple`: pops two ints.
    pub fn if_icmp(&mut self, opcode: u8, label: Label) {
        self.begin_insn();
        if !(op::IF_ICMPEQ..=op::IF_ICMPLE).contains(&opcode) {
            self.fail(format!("{opcode:#04x} is not a binary int branch"));
        }
        self.pop(&VType::Int);
        self.pop(&VType::Int);
        self.branch16(opcode, label);
    }

    /// `ifnull` / `ifnonnull`.
    pub fn if_ref(&mut self, opcode: u8, label: Label) {
        self.begin_insn();
        if opcode != op::IFNULL && opcode != op::IFNONNULL {
            self.fail(format!("{opcode:#04x} is not a reference branch"));
        }
        self.pop(&VType::obj(""));
        self.branch16(opcode, label);
    }

    pub fn goto(&mut self, label: Label) {
        self.begin_insn();
        self.branch16(op::GOTO, label);
        self.reachable = false;
    }

    fn switch_padding(&mut self) {
        while self.code.len() % 4 != 0 {
            self.byte(0);
        }
    }

    fn switch_target(&mut self, base: usize, label: Label) {
        self.edge(label, base);
        self.fixups.push(Fixup {
            at: self.code.len(),
            base,
            label,
            wide: true,
        });
        put_u32(&mut self.code, 0);
    }

    /// `tableswitch` over `low ..= low + targets.len() - 1`.
    pub fn tableswitch(&mut self, low: i32, default: Label, targets: &[Label]) {
        self.begin_insn();
        if targets.is_empty() || (low as i64) + (targets.len() as i64) - 1 > i32::MAX as i64 {
            self.fail("tableswitch range is empty or overflows int");
            return;
        }
        self.pop(&VType::Int);
        let pc = self.code.len();
        self.byte(op::TABLESWITCH);
        self.switch_padding();
        self.switch_target(pc, default);
        put_u32(&mut self.code, low as u32);
        put_u32(
            &mut self.code,
            (low as i64 + targets.len() as i64 - 1) as i32 as u32,
        );
        for &t in targets {
            self.switch_target(pc, t);
        }
        self.reachable = false;
    }

    /// `lookupswitch`; `pairs` must have strictly ascending keys.
    pub fn lookupswitch(&mut self, default: Label, pairs: &[(i32, Label)]) {
        self.begin_insn();
        if pairs.windows(2).any(|w| w[0].0 >= w[1].0) {
            self.fail("lookupswitch keys must be strictly ascending");
        }
        self.pop(&VType::Int);
        let pc = self.code.len();
        self.byte(op::LOOKUPSWITCH);
        self.switch_padding();
        self.switch_target(pc, default);
        put_u32(&mut self.code, pairs.len() as u32);
        for &(k, t) in pairs {
            put_u32(&mut self.code, k as u32);
            self.switch_target(pc, t);
        }
        self.reachable = false;
    }

    /// Return the method's declared type.
    pub fn ret(&mut self) {
        self.begin_insn();
        let opcode = match self.return_type.clone() {
            None => op::RETURN,
            Some(VType::Int) => {
                self.pop(&VType::Int);
                op::IRETURN
            }
            Some(VType::Long) => {
                self.pop(&VType::Long);
                op::LRETURN
            }
            Some(other) => {
                self.fail(format!("unsupported return type {other:?}"));
                op::RETURN
            }
        };
        self.byte(opcode);
        self.reachable = false;
    }

    /// Open an exception range at the current pc, handled at `handler` (whose
    /// frame must declare `[catch_class]` as its stack).
    pub fn try_begin(&mut self, handler: Label, catch_class: &str) -> usize {
        self.tries.push(TryRange {
            start: self.code.len(),
            end: None,
            handler,
            catch_class: catch_class.to_string(),
            states: vec![self.locals.clone()],
        });
        self.tries.len() - 1
    }

    /// Close the range opened by [`try_begin`](Self::try_begin) (exclusive).
    pub fn try_end(&mut self, id: usize) {
        let pc = self.code.len();
        match self.tries.get_mut(id) {
            Some(t) if t.end.is_none() => t.end = Some(pc),
            _ => self.fail(format!("try range {id} is not open")),
        }
    }

    /// Resolve labels, run the deferred checks, and produce the code body.
    pub fn finish(mut self) -> Result<CodeBody, String> {
        if self.reachable {
            self.fail("control falls off the end of the method");
        }
        if let Some(e) = self.error.take() {
            return Err(e);
        }
        let label_pc =
            |labels: &[Option<(usize, Frame)>], l: Label| -> Result<(usize, Frame), String> {
                labels[l.0]
                    .clone()
                    .ok_or_else(|| format!("label {} used but never bound", l.0))
            };
        for f in &self.fixups {
            let (target, _) = label_pc(&self.labels, f.label)?;
            let delta = target as i64 - f.base as i64;
            if f.wide {
                self.code[f.at..f.at + 4].copy_from_slice(&(delta as i32).to_be_bytes());
            } else {
                let d16 = i16::try_from(delta)
                    .map_err(|_| format!("branch at pc {} out of s16 range", f.base))?;
                self.code[f.at..f.at + 2].copy_from_slice(&d16.to_be_bytes());
            }
        }
        for e in &self.edges {
            let (_, frame) = label_pc(&self.labels, e.label)?;
            state_assignable(&e.locals, &e.stack, &frame)
                .map_err(|m| format!("branch at pc {} to label {}: {m}", e.pc, e.label.0))?;
        }
        let mut handlers = Vec::new();
        for t in &self.tries {
            let end = t.end.ok_or("try range never closed")?;
            if end <= t.start {
                return Err(format!("empty try range at pc {}", t.start));
            }
            let (hpc, frame) = label_pc(&self.labels, t.handler)?;
            if frame.stack != vec![VType::obj(&t.catch_class)] {
                return Err(format!(
                    "handler frame stack {:?} is not [{}]",
                    frame.stack, t.catch_class
                ));
            }
            let exc = [VType::obj(&t.catch_class)];
            for s in &t.states {
                state_assignable(s, &exc, &frame)
                    .map_err(|m| format!("try range at pc {}: {m}", t.start))?;
            }
            let catch = self.cp.class(&t.catch_class);
            handlers.push((t.start as u16, end as u16, hpc as u16, catch));
        }
        if self.code.len() >= 0xffff {
            return Err("method body too large".into());
        }
        let mut frames: Vec<(u16, Frame)> = Vec::new();
        let mut bound: Vec<(usize, Frame)> = self.labels.iter().flatten().cloned().collect();
        bound.sort_by_key(|(pc, _)| *pc);
        for (pc, frame) in bound {
            match frames.last() {
                Some((last, f)) if *last as usize == pc => {
                    if *f != frame {
                        return Err(format!("two labels at pc {pc} declare different frames"));
                    }
                }
                _ => frames.push((pc as u16, frame)),
            }
        }
        Ok(CodeBody {
            max_stack: self.max_stack as u16,
            max_locals: self.max_locals as u16,
            code: self.code,
            handlers,
            frames,
        })
    }
}

// ---------------------------------------------------------------------------
// Class writer
// ---------------------------------------------------------------------------

/// Class-file major version. 52 (Java 8) requires a `StackMapTable`; 49 (Java 5)
/// is verified by type inference and gets none — the fallback if a frame the
/// generator emits is ever rejected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClassVersion {
    /// Major 52 with `StackMapTable`.
    Java8,
    /// Major 49, no `StackMapTable`.
    Java5,
}

impl ClassVersion {
    pub fn major(self) -> u16 {
        match self {
            ClassVersion::Java8 => 52,
            ClassVersion::Java5 => 49,
        }
    }
    pub fn from_major(m: u16) -> Option<Self> {
        match m {
            52 => Some(ClassVersion::Java8),
            49 => Some(ClassVersion::Java5),
            _ => None,
        }
    }
}

/// One `public static` method.
pub struct MethodDef {
    pub name: String,
    pub descriptor: String,
    pub body: CodeBody,
}

fn write_vtype(cp: &mut ConstantPool, out: &mut Vec<u8>, t: &VType) {
    match t {
        VType::Top => out.push(0),
        VType::Int => out.push(1),
        VType::Float => out.push(2),
        VType::Double => out.push(3),
        VType::Long => out.push(4),
        VType::Null => out.push(5),
        VType::Object(n) => {
            out.push(7);
            let idx = cp.class(n);
            put_u16(out, idx);
        }
    }
}

fn stack_map_table(cp: &mut ConstantPool, frames: &[(u16, Frame)]) -> Vec<u8> {
    let mut out = Vec::new();
    put_u16(&mut out, frames.len() as u16);
    let mut prev: Option<u16> = None;
    for (pc, frame) in frames {
        let delta = match prev {
            None => *pc,
            Some(p) => pc - p - 1,
        };
        prev = Some(*pc);
        out.push(255);
        put_u16(&mut out, delta);
        let locals = frame.locals_list();
        put_u16(&mut out, locals.len() as u16);
        for t in &locals {
            write_vtype(cp, &mut out, t);
        }
        put_u16(&mut out, frame.stack.len() as u16);
        for t in &frame.stack {
            write_vtype(cp, &mut out, t);
        }
    }
    out
}

/// Serialize a `public` class extending `java/lang/Object` whose methods are
/// all `public static`. No constructor: the class is never instantiated.
pub fn write_class(
    mut cp: ConstantPool,
    name: &str,
    methods: Vec<MethodDef>,
    version: ClassVersion,
) -> Vec<u8> {
    let this_class = cp.class(name);
    let super_class = cp.class("java/lang/Object");
    let code_name = cp.utf8("Code");
    let mut method_bytes = Vec::new();
    for m in &methods {
        let name_idx = cp.utf8(&m.name);
        let desc_idx = cp.utf8(&m.descriptor);
        let mut code_attr = Vec::new();
        put_u16(&mut code_attr, m.body.max_stack);
        put_u16(&mut code_attr, m.body.max_locals);
        put_u32(&mut code_attr, m.body.code.len() as u32);
        code_attr.extend_from_slice(&m.body.code);
        put_u16(&mut code_attr, m.body.handlers.len() as u16);
        for (s, e, h, c) in &m.body.handlers {
            put_u16(&mut code_attr, *s);
            put_u16(&mut code_attr, *e);
            put_u16(&mut code_attr, *h);
            put_u16(&mut code_attr, *c);
        }
        let with_smt = version == ClassVersion::Java8 && !m.body.frames.is_empty();
        if with_smt {
            // Interned only when a table is written: a pre-50 class must not
            // even name the attribute.
            let smt_name = cp.utf8("StackMapTable");
            let smt = stack_map_table(&mut cp, &m.body.frames);
            put_u16(&mut code_attr, 1);
            put_u16(&mut code_attr, smt_name);
            put_u32(&mut code_attr, smt.len() as u32);
            code_attr.extend_from_slice(&smt);
        } else {
            put_u16(&mut code_attr, 0);
        }
        put_u16(&mut method_bytes, 0x0009); // ACC_PUBLIC | ACC_STATIC
        put_u16(&mut method_bytes, name_idx);
        put_u16(&mut method_bytes, desc_idx);
        put_u16(&mut method_bytes, 1);
        put_u16(&mut method_bytes, code_name);
        put_u32(&mut method_bytes, code_attr.len() as u32);
        method_bytes.extend_from_slice(&code_attr);
    }

    let mut out = Vec::new();
    put_u32(&mut out, 0xCAFE_BABE);
    put_u16(&mut out, 0);
    put_u16(&mut out, version.major());
    cp.write(&mut out);
    put_u16(&mut out, 0x0021); // ACC_PUBLIC | ACC_SUPER
    put_u16(&mut out, this_class);
    put_u16(&mut out, super_class);
    put_u16(&mut out, 0); // interfaces
    put_u16(&mut out, 0); // fields
    put_u16(&mut out, methods.len() as u16);
    out.extend_from_slice(&method_bytes);
    put_u16(&mut out, 0); // class attributes
    out
}

// ---------------------------------------------------------------------------
// Structural sanity check over finished bytes
// ---------------------------------------------------------------------------

/// Re-parse a written class and check the code shapes a verifier rejects
/// before it ever looks at types: every method body decodes to whole
/// instructions (via [`crate::matrix::instruction_len`]), every branch and
/// switch target and every handler pc is an instruction boundary, and — for a
/// class with `StackMapTable`s — every one of those targets and every
/// instruction after an unconditional transfer carries a frame.
///
/// Independent of [`Asm`]'s bookkeeping on purpose: it reads the bytes that
/// will actually be loaded.
pub fn check_class_shape(class: &[u8]) -> Result<(), String> {
    let r = Reader { b: class };
    if r.u32(0)? != 0xCAFE_BABE {
        return Err("bad magic".into());
    }
    let major = r.u16(6)?;
    let cp_count = r.u16(8)?;
    let mut off = 10usize;
    let mut utf8: Vec<(usize, String)> = Vec::new();
    let mut idx = 1usize;
    while idx < cp_count {
        let tag = r.u8(off)?;
        let adv = match tag {
            1 => {
                let len = r.u16(off + 1)?;
                let s = class.get(off + 3..off + 3 + len).ok_or("truncated utf8")?;
                utf8.push((idx, String::from_utf8_lossy(s).into_owned()));
                3 + len
            }
            3 | 4 => 5,
            5 | 6 => {
                idx += 1;
                9
            }
            7 | 8 => 3,
            9 | 10 | 12 => 5,
            t => return Err(format!("unexpected constant tag {t}")),
        };
        off += adv;
        idx += 1;
    }
    let name_of = |i: usize| utf8.iter().find(|(k, _)| *k == i).map(|(_, s)| s.as_str());
    off += 6;
    let ifaces = r.u16(off)?;
    off += 2 + 2 * ifaces;
    let fields = r.u16(off)?;
    if fields != 0 {
        return Err("generated classes have no fields".into());
    }
    off += 2;
    let methods = r.u16(off)?;
    off += 2;
    for _ in 0..methods {
        let attrs = r.u16(off + 6)?;
        off += 8;
        let mut saw_code = false;
        for _ in 0..attrs {
            let name = r.u16(off)?;
            let len = r.u32(off + 2)? as usize;
            let payload = class
                .get(off + 6..off + 6 + len)
                .ok_or("truncated attribute")?;
            if name_of(name) == Some("Code") {
                saw_code = true;
                check_code(payload, major >= 50, &name_of)?;
            }
            off += 6 + len;
        }
        if !saw_code {
            return Err("method without Code".into());
        }
    }
    if r.u16(off)? != 0 || off + 2 != class.len() {
        return Err("trailing bytes after the method table".into());
    }
    Ok(())
}

struct Reader<'a> {
    b: &'a [u8],
}

impl Reader<'_> {
    fn u8(&self, o: usize) -> Result<usize, String> {
        self.b
            .get(o)
            .map(|&v| v as usize)
            .ok_or_else(|| format!("truncated at {o}"))
    }
    fn u16(&self, o: usize) -> Result<usize, String> {
        Ok((self.u8(o)? << 8) | self.u8(o + 1)?)
    }
    fn u32(&self, o: usize) -> Result<u32, String> {
        Ok(((self.u16(o)? as u32) << 16) | self.u16(o + 2)? as u32)
    }
    fn i32(&self, o: usize) -> Result<i64, String> {
        Ok(self.u32(o)? as i32 as i64)
    }
}

fn check_code<'n>(
    payload: &[u8],
    needs_frames: bool,
    name_of: &dyn Fn(usize) -> Option<&'n str>,
) -> Result<(), String> {
    let r = Reader { b: payload };
    let code_len = r.u32(4)? as usize;
    let code = payload.get(8..8 + code_len).ok_or("truncated code")?;
    let cr = Reader { b: code };
    let mut boundaries = Vec::new();
    let mut targets: Vec<usize> = Vec::new();
    let mut after_transfer: Vec<usize> = Vec::new();
    let mut pc = 0usize;
    while pc < code.len() {
        boundaries.push(pc);
        let opc = code[pc];
        let len = crate::matrix::instruction_len(code, pc)
            .ok_or_else(|| format!("undecodable instruction {opc:#04x} at pc {pc}"))?;
        let jump = |d: i64| -> Result<usize, String> {
            usize::try_from(pc as i64 + d).map_err(|_| format!("negative target from pc {pc}"))
        };
        match opc {
            0x99..=0xa7 | 0xc6 | 0xc7 => {
                let d = cr.u16(pc + 1)? as u16 as i16 as i64;
                targets.push(jump(d)?);
            }
            0xaa | 0xab => {
                let base = (pc + 4) & !3;
                targets.push(jump(cr.i32(base)?)?);
                if opc == 0xaa {
                    let low = cr.i32(base + 4)?;
                    let high = cr.i32(base + 8)?;
                    for i in 0..(high - low + 1) as usize {
                        targets.push(jump(cr.i32(base + 12 + 4 * i)?)?);
                    }
                } else {
                    let n = cr.u32(base + 4)? as usize;
                    for i in 0..n {
                        targets.push(jump(cr.i32(base + 12 + 8 * i)?)?);
                    }
                }
            }
            0xa8 | 0xa9 | 0xc8 | 0xc9 => return Err(format!("unexpected opcode {opc:#04x}")),
            _ => {}
        }
        if matches!(opc, 0xa7 | 0xaa | 0xab | 0xac..=0xb1 | 0xbf) {
            after_transfer.push(pc + len);
        }
        pc += len;
    }
    if pc != code.len() {
        return Err("last instruction overruns the code array".into());
    }
    let last_op = boundaries.last().and_then(|&p| code.get(p).copied());
    if !matches!(last_op, Some(0xa7 | 0xaa | 0xab | 0xac..=0xb1 | 0xbf)) {
        return Err("code can fall off the end".into());
    }
    let mut off = 8 + code_len;
    let n_handlers = r.u16(off)?;
    off += 2;
    for i in 0..n_handlers {
        let start = r.u16(off + 8 * i)?;
        let end = r.u16(off + 8 * i + 2)?;
        let handler = r.u16(off + 8 * i + 4)?;
        if start >= end || end > code_len || !boundaries.contains(&start) {
            return Err(format!("bad handler range {start}..{end}"));
        }
        if end != code_len && !boundaries.contains(&end) {
            return Err(format!("handler end {end} is not an instruction boundary"));
        }
        targets.push(handler);
    }
    off += 8 * n_handlers;
    for &t in &targets {
        if !boundaries.contains(&t) {
            return Err(format!("target {t} is not an instruction boundary"));
        }
    }
    // StackMapTable frame pcs.
    let attrs = r.u16(off)?;
    off += 2;
    let mut frame_pcs: Vec<usize> = Vec::new();
    for _ in 0..attrs {
        let name = r.u16(off)?;
        let len = r.u32(off + 2)? as usize;
        if name_of(name) == Some("StackMapTable") {
            let s = Reader {
                b: payload
                    .get(off + 6..off + 6 + len)
                    .ok_or("truncated StackMapTable")?,
            };
            let n = s.u16(0)?;
            let mut p = 2usize;
            let mut prev: Option<usize> = None;
            for _ in 0..n {
                if s.u8(p)? != 255 {
                    return Err("only full_frame is emitted".into());
                }
                let delta = s.u16(p + 1)?;
                let at = match prev {
                    None => delta,
                    Some(q) => q + delta + 1,
                };
                prev = Some(at);
                frame_pcs.push(at);
                p += 3;
                for _ in 0..2 {
                    let count = s.u16(p)?;
                    p += 2;
                    for _ in 0..count {
                        p += if s.u8(p)? == 7 { 3 } else { 1 };
                    }
                }
            }
            if p != len {
                return Err("StackMapTable length mismatch".into());
            }
        }
        off += 6 + len;
    }
    if needs_frames {
        for &t in targets
            .iter()
            .chain(after_transfer.iter().filter(|&&p| p < code_len))
        {
            if !frame_pcs.contains(&t) {
                return Err(format!("pc {t} needs a StackMapTable frame"));
            }
        }
        for &f in &frame_pcs {
            if !boundaries.contains(&f) {
                return Err(format!("frame at pc {f} is not an instruction boundary"));
            }
        }
    } else if !frame_pcs.is_empty() {
        return Err("a pre-50 class must not carry StackMapTable".into());
    }
    Ok(())
}

/// A one-line-per-instruction listing of a code body, for triage reports.
pub fn disassemble(code: &[u8]) -> String {
    let mut s = String::new();
    let mut pc = 0usize;
    while pc < code.len() {
        let Some(len) = crate::matrix::instruction_len(code, pc) else {
            let _ = writeln!(s, "{pc:5}: <undecodable {:#04x}>", code[pc]);
            break;
        };
        let name = crate::matrix::opcode_name(code[pc]).unwrap_or("?");
        let operands: Vec<String> = code[pc + 1..pc + len]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let _ = writeln!(s, "{pc:5}: {name} {}", operands.join(" "));
        pc += len;
    }
    s
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(locals: Vec<VType>, stack: Vec<VType>) -> Frame {
        Frame { locals, stack }
    }

    #[test]
    fn descriptor_parsing() {
        let (p, r) = parse_method_descriptor("(IJFD[I[[Ljava/lang/String;)J").unwrap();
        assert_eq!(
            p,
            vec![
                VType::Int,
                VType::Long,
                VType::Float,
                VType::Double,
                VType::obj("[I"),
                VType::obj("[[Ljava/lang/String;")
            ]
        );
        assert_eq!(r, Some(VType::Long));
        assert_eq!(
            parse_method_descriptor("([Ljava/lang/String;)V").unwrap().1,
            None
        );
        assert!(parse_method_descriptor("I)V").is_none());
    }

    #[test]
    fn constant_pool_dedupes_and_reserves_two_slots_for_wide_constants() {
        let mut cp = ConstantPool::new();
        let a = cp.long(5);
        let b = cp.integer(7);
        assert_eq!(b, a + 2, "a long constant consumes two indices");
        assert_eq!(cp.long(5), a);
        // -0.0 and 0.0 are distinct constants.
        assert_ne!(cp.float(0.0), cp.float(-0.0));
    }

    #[test]
    fn a_counted_loop_assembles_and_passes_the_shape_check() {
        let mut cp = ConstantPool::new();
        let mut a = Asm::new_static(&mut cp, "(I)I", 2);
        a.iconst(0);
        a.store(VType::Int, 1, false);
        let head = a.new_label();
        let exit = a.new_label();
        let f = frame(vec![VType::Int, VType::Int], vec![]);
        a.bind(head, f.clone());
        a.load(VType::Int, 1, false);
        a.load(VType::Int, 0, false);
        a.if_icmp(op::IF_ICMPGE, exit);
        a.iinc(1, 1, true);
        a.goto(head);
        a.bind(exit, f);
        a.load(VType::Int, 1, true);
        a.ret();
        let body = a.finish().expect("assembles");
        assert_eq!(body.frames.len(), 2);
        assert_eq!(body.max_stack, 2);
        // `wide iinc 1 1` is c4 84 0001 0001.
        assert!(body.code.windows(6).any(|w| w == [0xc4, 0x84, 0, 1, 0, 1]));
        let class = write_class(
            cp,
            "L",
            vec![MethodDef {
                name: "t".into(),
                descriptor: "(I)I".into(),
                body,
            }],
            ClassVersion::Java8,
        );
        check_class_shape(&class).expect("shape");
        assert!(crate::matrix::scan_class_bytes("L", &class).is_some());
    }

    #[test]
    fn type_errors_are_latched() {
        let mut cp = ConstantPool::new();
        let mut a = Asm::new_static(&mut cp, "(J)J", 2);
        a.load(VType::Long, 0, false);
        a.simple(op::IADD, &[VType::Int, VType::Int], &[VType::Int]);
        a.ret();
        let err = a.finish().unwrap_err();
        assert!(err.contains("expected Int"), "{err}");
    }

    #[test]
    fn code_after_goto_needs_a_label() {
        let mut cp = ConstantPool::new();
        let mut a = Asm::new_static(&mut cp, "()V", 0);
        let l = a.new_label();
        a.goto(l);
        a.aconst_null();
        a.pop1();
        a.bind(l, frame(vec![], vec![]));
        a.ret();
        assert!(a.finish().unwrap_err().contains("unconditional transfer"));
    }

    #[test]
    fn branch_state_must_match_the_target_frame() {
        let mut cp = ConstantPool::new();
        let mut a = Asm::new_static(&mut cp, "(I)I", 1);
        let l = a.new_label();
        a.iconst(1);
        a.load(VType::Int, 0, false);
        a.if_int(op::IFEQ, l); // leaves an int on the stack
        a.iconst(2);
        a.simple(op::IADD, &[VType::Int, VType::Int], &[VType::Int]);
        a.bind(l, frame(vec![VType::Int], vec![]));
        a.ret();
        let err = a.finish().unwrap_err();
        assert!(err.contains("stack depth"), "{err}");
    }

    #[test]
    fn stack_shuffles_follow_the_jvms_forms() {
        let mut cp = ConstantPool::new();
        let mut a = Asm::new_static(&mut cp, "(IJ)J", 3);
        // dup_x2 form 2: J I -> I J I
        a.load(VType::Long, 1, false);
        a.load(VType::Int, 0, false);
        a.dup_x2();
        assert_eq!(a.stack(), &[VType::Int, VType::Long, VType::Int]);
        a.simple(op::I2L, &[VType::Int], &[VType::Long]);
        a.simple(op::LSUB, &[VType::Long, VType::Long], &[VType::Long]);
        // dup2_x2 form 3: J I I -> I I J I I
        a.load(VType::Int, 0, false);
        a.load(VType::Int, 0, false);
        a.dup2_x2();
        assert_eq!(
            a.stack(),
            &[
                VType::Int,
                VType::Int,
                VType::Int,
                VType::Long,
                VType::Int,
                VType::Int
            ]
        );
        a.pop2();
        a.simple(op::L2I, &[VType::Long], &[VType::Int]);
        a.swap();
        a.simple(op::IXOR, &[VType::Int, VType::Int], &[VType::Int]);
        a.simple(op::IXOR, &[VType::Int, VType::Int], &[VType::Int]);
        a.simple(op::IADD, &[VType::Int, VType::Int], &[VType::Int]);
        a.simple(op::I2L, &[VType::Int], &[VType::Long]);
        a.ret();
        a.finish().expect("assembles");
    }

    #[test]
    fn switches_pad_to_four_and_resolve_32_bit_offsets() {
        let mut cp = ConstantPool::new();
        let mut a = Asm::new_static(&mut cp, "(I)I", 1);
        a.load(VType::Int, 0, false); // pc 0 (iload_0), switch at pc 1
        let d = a.new_label();
        let c0 = a.new_label();
        a.tableswitch(i32::MAX - 1, d, &[c0, c0]);
        let f = frame(vec![VType::Int], vec![]);
        a.bind(c0, f.clone());
        a.iconst(1);
        a.ret();
        a.bind(d, f);
        a.lookupswitch_default_only_helper();
        let body = a.finish().expect("assembles");
        // opcode at 1, padding to 4 ⇒ default offset field at 4.
        assert_eq!(body.code[1], op::TABLESWITCH);
        assert_eq!(&body.code[2..4], &[0, 0]);
    }

    impl Asm<'_> {
        fn lookupswitch_default_only_helper(&mut self) {
            let out = self.new_label();
            self.load(VType::Int, 0, false);
            self.lookupswitch(out, &[(i32::MIN, out), (i32::MAX, out)]);
            self.bind(out, frame(vec![VType::Int], vec![]));
            self.iconst(0);
            self.ret();
        }
    }

    #[test]
    fn handler_frames_are_checked_against_every_state_in_range() {
        let mut cp = ConstantPool::new();
        let mut a = Asm::new_static(&mut cp, "(I)I", 2);
        a.iconst(0);
        a.store(VType::Int, 1, false);
        let h = a.new_label();
        let join = a.new_label();
        let t = a.try_begin(h, "java/lang/ArithmeticException");
        a.load(VType::Int, 0, false);
        a.load(VType::Int, 0, false);
        a.simple(op::IDIV, &[VType::Int, VType::Int], &[VType::Int]);
        a.store(VType::Int, 1, false);
        a.try_end(t);
        a.goto(join);
        a.bind(
            h,
            frame(
                vec![VType::Int, VType::Int],
                vec![VType::obj("java/lang/ArithmeticException")],
            ),
        );
        a.pop1();
        a.bind(join, frame(vec![VType::Int, VType::Int], vec![]));
        a.load(VType::Int, 1, false);
        a.ret();
        let body = a.finish().expect("assembles");
        assert_eq!(body.handlers.len(), 1);
    }

    #[test]
    fn java5_classes_carry_no_stack_map_table() {
        let mut cp = ConstantPool::new();
        let mut a = Asm::new_static(&mut cp, "(I)I", 1);
        let l = a.new_label();
        a.load(VType::Int, 0, false);
        a.if_int(op::IFEQ, l);
        a.iconst(1);
        a.ret();
        a.bind(l, frame(vec![VType::Int], vec![]));
        a.iconst(2);
        a.ret();
        let body = a.finish().unwrap();
        let class = write_class(
            cp,
            "V5",
            vec![MethodDef {
                name: "t".into(),
                descriptor: "(I)I".into(),
                body,
            }],
            ClassVersion::Java5,
        );
        check_class_shape(&class).expect("shape");
        assert!(!class.windows(13).any(|w| w == b"StackMapTable"));
    }
}
