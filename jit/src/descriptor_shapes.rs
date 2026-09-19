// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! Method descriptors and opcode categories: the pure functions that answer
//! "what SHAPE is this method?" from its descriptor string and its bytecode
//! bytes.
//!
//! Split out of `lib.rs` on 2026-09-16, following `jfr_compile_decision`,
//! `osr_entry`, `inline_cache_pic`, `ea_ir_bridge` and `exec_memory`.
//!
//! # Why this is a real boundary
//!
//! Everything in here is a total function of `(descriptor)` or
//! `(code, code_len, descriptor)`. Not one of these functions reads a global,
//! takes a lock, consults an environment variable, allocates executable memory
//! or looks at the state of a compilation. That is not a coincidence of how
//! they were written -- it is what they ARE: a decoder for the two on-disk
//! encodings the JVM specification defines (the method descriptor grammar of
//! JVMS 4.3.3, and the opcode table of JVMS 6.5), plus the handful of
//! classifications this JIT derives from them.
//!
//! That purity is the whole argument for the file. These predicates are the
//! gate conditions on the tier decision -- `method_uses_category2` and
//! `method_uses_fp` are what keep a `long`/`double` method off the IR path,
//! `static_call_shape` is what decides whether a call site can be a direct
//! `Op::Call`, `compute_param_oop_mask` is what tells the GC which incoming
//! argument registers hold references. Read inline among the compilation
//! driver, each one looks like a piece of tier policy that might depend on
//! anything. Read together in a file that imports nothing but `crate::ir` for a
//! type name, they are visibly what they are: a spec decoder, with the policy
//! living at the call sites that use it. Anyone auditing "is this
//! classification right?" now has a file where the only possible input is the
//! arguments.
//!
//! It also means these are the cheapest functions in the crate to test: there
//! is no fixture, no compile, no thread. The suites that exercise them need
//! construct nothing but a `&str`.
//!
//! # What deliberately did NOT come along
//!
//! `selfrec_direct_enabled` and its thread-local test override sat physically
//! between `method_uses_fp` and `static_call_shape` in `lib.rs`, and they were
//! left behind: they read `CRATONVM_JIT_IR_SELFREC_DIRECT` and a per-thread
//! cell, so they are a feature gate, not a descriptor fact. Moving them would
//! have put the first environment read into a file whose entire claim is that
//! it contains none. The cut therefore runs around them, in two pieces, rather
//! than taking a contiguous range that would have been one line cheaper and one
//! invariant poorer.
//!
//! `count_param_slots` likewise stays out: it comes from `cratonvm_jit_api` and
//! is re-exported by the crate root, and this module has no business owning a
//! name it does not define. [`count_param_slots_jvm_spec`] is here because it
//! is a DIFFERENT function -- JVMS two-slot numbering for category-2 locals,
//! against the compact one-register-per-argument ABI the other counts -- and
//! the pair only makes sense read side by side.
//!
//! Glob-re-exported from the crate root, so every path a caller used before the
//! split still resolves -- the code moved, the API did not.

use crate::ir;

/// Iterator over parameter types in a JVM method descriptor.
pub struct DescriptorParamIter<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> DescriptorParamIter<'a> {
    pub fn new(descriptor: &'a str) -> Self {
        let bytes = descriptor.as_bytes();
        let pos = if !bytes.is_empty() && bytes[0] == b'(' {
            1
        } else {
            0
        };
        Self { bytes, pos }
    }
}

impl Iterator for DescriptorParamIter<'_> {
    type Item = u8;
    fn next(&mut self) -> Option<u8> {
        if self.pos >= self.bytes.len() || self.bytes[self.pos] == b')' {
            return None;
        }
        let tag = self.bytes[self.pos];
        match tag {
            b'I' | b'J' | b'F' | b'D' | b'B' | b'C' | b'S' | b'Z' => {
                self.pos += 1;
                Some(tag)
            }
            b'L' => {
                while self.pos < self.bytes.len() && self.bytes[self.pos] != b';' {
                    self.pos += 1;
                }
                self.pos += 1;
                Some(b'L')
            }
            b'[' => {
                while self.pos < self.bytes.len() && self.bytes[self.pos] == b'[' {
                    self.pos += 1;
                }
                if self.pos < self.bytes.len() && self.bytes[self.pos] == b'L' {
                    while self.pos < self.bytes.len() && self.bytes[self.pos] != b';' {
                        self.pos += 1;
                    }
                    self.pos += 1;
                } else if self.pos < self.bytes.len() {
                    self.pos += 1;
                }
                Some(b'[')
            }
            _ => {
                self.pos += 1;
                Some(tag)
            }
        }
    }
}

/// Count the number of JVM stack slots consumed by parameters in a method descriptor.
/// True if the method uses any category-2 (long/double) value — as a
/// parameter, return type, or operand/result of a long/double bytecode.
///
/// The IR pipeline ([`ir::IrBuilder`]) types every value as 32-bit
/// `IrType::Int` and lays parameters out by JIT-argument index rather than
/// JVM local slot, so it can model neither 64-bit values nor the two-slot
/// category-2 parameter layout. Such methods must use the single-pass
/// bytecode-x64 backend instead. Note: a `long[]`/`double[]` parameter is a
/// *reference* (category-1) and does NOT count here — only scalar J/D do.
pub(crate) fn method_uses_category2(code: &[u8], code_len: usize, descriptor: &str) -> bool {
    let b = descriptor.as_bytes();
    // Scalar long/double parameter?
    let mut i = 1;
    while i < b.len() && b[i] != b')' {
        match b[i] {
            b'J' | b'D' => return true,
            b'L' => {
                i += 1;
                while i < b.len() && b[i] != b';' {
                    i += 1;
                }
                i += 1;
            }
            b'[' => {
                // Skip the whole array type — arrays are references (cat-1),
                // even `[J` / `[D`.
                i += 1;
                while i < b.len() && b[i] == b'[' {
                    i += 1;
                }
                if i < b.len() && b[i] == b'L' {
                    i += 1;
                    while i < b.len() && b[i] != b';' {
                        i += 1;
                    }
                    i += 1;
                } else if i < b.len() {
                    i += 1;
                }
            }
            _ => i += 1, // I F B C S Z
        }
    }
    // Long/double return type.
    if let Some(rp) = b.iter().position(|&c| c == b')') {
        if matches!(b.get(rp + 1), Some(b'J') | Some(b'D')) {
            return true;
        }
    }
    // Any long/double bytecode in the body.
    let mut pc = 0;
    while pc < code_len {
        if is_category2_opcode(code[pc]) {
            return true;
        }
        pc += crate::bytecode_analysis::step(code, pc);
    }
    false
}

/// Compute, for each incoming JIT argument (in order: `this` for instance
/// methods, then declared parameters), the JVM local slot it occupies, along
/// with the total slot span the parameters consume. A category-2
/// (long/double) parameter occupies TWO JVM local slots but is passed in a
/// single JIT argument register, so its slot index diverges from its argument
/// index — e.g. `static f(long a, long b)` puts `a` at slot 0 and `b` at slot
/// 2, while the JIT passes them as args 0 and 1. The bytecode-x64 prologue
/// uses this to deposit each argument in the slot the body reads. See
/// [`x64::compile_with_param_slots`].
pub fn compute_param_jvm_slots(descriptor: &str, is_static: bool) -> (Vec<usize>, usize) {
    let mut slots = Vec::new();
    let mut slot = 0usize;
    if !is_static {
        slots.push(slot);
        slot += 1; // implicit `this`
    }
    // Every tag, an unrecognised one included, takes a slot: the hand walk
    // this replaced counted a stray byte as one wide, and so does this.
    for tag in DescriptorParamIter::new(descriptor) {
        slots.push(slot);
        slot += if matches!(tag, b'J' | b'D') { 2 } else { 1 };
    }
    (slots, slot)
}

/// Stage A.4 (precise oop maps, B-K fix) — bitmask of JVM local slots that hold
/// a REFERENCE parameter on method entry: bit `k` set ⇒ slot `k` is an oop.
///
/// Covers the implicit `this` (slot 0, instance methods) plus every declared
/// `L…;` / `[…` parameter. Mirrors [`compute_param_jvm_slots`]'s slot walk
/// EXACTLY — category-2 (`J`/`D`) parameters consume two slots and are non-oops,
/// primitives consume one — so the returned bit positions line up with the local
/// slot indices the method body reads. Slots ≥ 64 are out of the dataflow's
/// 64-local model and are dropped (such a method cannot be fully-covered).
///
/// Used only on the precise gate to seed [`x64::compile_with_param_slots`]'s
/// `param_oop_mask`; the caller passes `0` when the gate is off.
///
/// `pub` so the OSR / eager-first-call compile paths in the VM crate (which call
/// `x64::compile_with_param_slots` directly) can seed the same reference-parameter
/// mask the hot-path `try_compile` does — without it, an oop parameter living in a
/// callee-saved register across an early safepoint is invisible to the
/// post-safepoint reload and a moving GC leaves the register stale (HIB-CV-20).
pub fn compute_param_oop_mask(descriptor: &str, is_static: bool) -> u64 {
    let mut mask = 0u64;
    let mut slot = 0usize;
    if !is_static {
        // `this` is always a reference.
        mask |= 1u64 << slot; // slot == 0 here
        slot += 1;
    }
    let b = descriptor.as_bytes();
    let mut i = 1;
    while i < b.len() && b[i] != b')' {
        match b[i] {
            b'J' | b'D' => {
                // category-2 scalar: two slots, non-oop.
                slot += 2;
                i += 1;
            }
            b'L' => {
                if slot < 64 {
                    mask |= 1u64 << slot;
                }
                slot += 1;
                i += 1;
                while i < b.len() && b[i] != b';' {
                    i += 1;
                }
                i += 1;
            }
            b'[' => {
                if slot < 64 {
                    mask |= 1u64 << slot;
                }
                slot += 1;
                i += 1;
                while i < b.len() && b[i] == b'[' {
                    i += 1;
                }
                if i < b.len() && b[i] == b'L' {
                    i += 1;
                    while i < b.len() && b[i] != b';' {
                        i += 1;
                    }
                    i += 1;
                } else if i < b.len() {
                    i += 1;
                }
            }
            _ => {
                // I F B C S Z — category-1 primitive, non-oop.
                slot += 1;
                i += 1;
            }
        }
    }
    mask
}

/// Long/double bytecodes (category-2 operands or results). Used to keep such
/// methods off the int-only IR pipeline.
fn is_category2_opcode(op: u8) -> bool {
    matches!(
        op,
        0x09 | 0x0a            // lconst_0, lconst_1
        | 0x0e | 0x0f          // dconst_0, dconst_1
        | 0x14                 // ldc2_w (long/double constant)
        | 0x16 | 0x18          // lload, dload
        | 0x1e..=0x21          // lload_0..3
        | 0x26..=0x29          // dload_0..3
        | 0x2f | 0x31          // laload, daload
        | 0x37 | 0x39          // lstore, dstore
        | 0x3f..=0x42          // lstore_0..3
        | 0x47..=0x4a          // dstore_0..3
        | 0x50 | 0x52          // lastore, dastore
        | 0x61 | 0x63 | 0x65 | 0x67 | 0x69 | 0x6b | 0x6d | 0x6f | 0x71 | 0x73 // l/d add,sub,mul,div,rem
        | 0x75 | 0x77          // lneg, dneg
        | 0x79 | 0x7b | 0x7d   // lshl, lshr, lushr
        | 0x7f | 0x81 | 0x83   // land, lor, lxor
        | 0x85 | 0x87 | 0x88 | 0x89 | 0x8a | 0x8c | 0x8d | 0x8e | 0x8f | 0x90 // i2l,i2d,l2i,l2f,l2d,f2l,f2d,d2i,d2l,d2f
        | 0x94 | 0x97 | 0x98   // lcmp, dcmpl, dcmpg
        | 0xad | 0xaf          // lreturn, dreturn
    )
}

/// Per-argument JVM type tag, one entry per COMPACT stack slot (mirrors
/// `count_param_slots`' one-slot-per-parameter counting — a `long`/`double`
/// occupies a single entry here, not two), in descriptor (left-to-right,
/// push) order. Tag is one of `I` (int/boolean/byte/char/short, collapsed to
/// a single non-oop-int tag), `J` (long), `F` (float), `D` (double), or `L`
/// (object or array reference — the caller already has a precise oop mask
/// for these; the tag exists for completeness, not because it's read).
///
/// Written for the OSR-exit/invokedynamic-uncommon-trap deopt snapshot
/// (`build_and_record_deopt_point`'s operand-stack loop in `x64.rs`): the
/// snapshot's generic per-method `wide_fp` gate marks EVERY non-oop stack
/// slot `Unsupported` once a method touches any `long`/`float`/`double`
/// ANYWHERE, even when the specific slot at THIS bci is provably a plain
/// `int` (the abstract stack has no per-entry width source otherwise — see
/// `uses_long_float_double`'s doc comment). An invokedynamic call site is
/// the one place stack shape IS known precisely without a full stack-map
/// simulation: its bootstrap descriptor fixes exactly how many arguments are
/// live directly beneath it and their types, in order. Using these tags to
/// override `Unsupported` just for the indy call's own arguments — instead of
/// the coarse method-level gate — is what let the OSR-exit snapshot at
/// `getstatic System.out` + an `if`/`else`-computed `makeConcatWithConstants`
/// argument decode its operand stack precisely; see
/// `testoutputbuffer-writespeed-content-length-mismatch-FIXED.md`.
pub fn indy_arg_type_tags(descriptor: &str) -> Vec<u8> {
    let bytes = descriptor.as_bytes();
    let mut tags = Vec::new();
    if bytes.is_empty() || bytes[0] != b'(' {
        return tags;
    }
    let mut i = 1;
    while i < bytes.len() && bytes[i] != b')' {
        match bytes[i] {
            b'I' | b'B' | b'C' | b'S' | b'Z' => {
                tags.push(b'I');
                i += 1;
            }
            b'F' => {
                tags.push(b'F');
                i += 1;
            }
            b'J' => {
                tags.push(b'J');
                i += 1;
            }
            b'D' => {
                tags.push(b'D');
                i += 1;
            }
            b'L' => {
                while i < bytes.len() && bytes[i] != b';' {
                    i += 1;
                }
                i += 1;
                tags.push(b'L');
            }
            b'[' => {
                i += 1;
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
                tags.push(b'L');
            }
            _ => {
                i += 1;
            }
        }
    }
    tags
}

/// inc 25: the IR-builder parameter types in JIT-arg order (one per parameter,
/// `this` first for an instance method). Drives `IrBuilder::set_param_types`,
/// which lays the params out across JVM local slots with the category-2 two-slot
/// convention. Mirrors `count_param_slots`' one-slot-per-parameter counting.
pub(crate) fn ir_param_types(descriptor: &str, is_static: bool) -> Vec<ir::IrType> {
    let mut types = Vec::new();
    if !is_static {
        types.push(ir::IrType::Ref); // implicit `this`
    }
    let b = descriptor.as_bytes();
    let mut i = 1; // skip '('
    while i < b.len() && b[i] != b')' {
        match b[i] {
            b'J' => {
                types.push(ir::IrType::Long);
                i += 1;
            }
            b'D' => {
                types.push(ir::IrType::Double);
                i += 1;
            }
            b'F' => {
                types.push(ir::IrType::Float);
                i += 1;
            }
            b'L' => {
                types.push(ir::IrType::Ref);
                i += 1;
                while i < b.len() && b[i] != b';' {
                    i += 1;
                }
                i += 1;
            }
            b'[' => {
                types.push(ir::IrType::Ref);
                i += 1;
                while i < b.len() && b[i] == b'[' {
                    i += 1;
                }
                if i < b.len() && b[i] == b'L' {
                    i += 1;
                    while i < b.len() && b[i] != b';' {
                        i += 1;
                    }
                    i += 1;
                } else if i < b.len() {
                    i += 1;
                }
            }
            _ => {
                types.push(ir::IrType::Int); // I Z B C S
                i += 1;
            }
        }
    }
    types
}

/// inc 25: like `method_uses_category2`, but flags only **double** (the part the
/// long slice does NOT yet handle). A `D` parameter/return or any double-typed
/// opcode disqualifies the method; `long` and `float` do not (long is handled,
/// float bails at the builder's opcode catch-all). Used to admit long-only
/// methods to the IR path while keeping double/XMM on single-pass.
fn method_uses_double(code: &[u8], code_len: usize, descriptor: &str) -> bool {
    let b = descriptor.as_bytes();
    let mut i = 1;
    while i < b.len() && b[i] != b')' {
        match b[i] {
            b'D' => return true,
            b'L' => {
                i += 1;
                while i < b.len() && b[i] != b';' {
                    i += 1;
                }
                i += 1;
            }
            b'[' => {
                i += 1;
                while i < b.len() && b[i] == b'[' {
                    i += 1;
                }
                if i < b.len() && b[i] == b'L' {
                    i += 1;
                    while i < b.len() && b[i] != b';' {
                        i += 1;
                    }
                    i += 1;
                } else if i < b.len() {
                    i += 1;
                }
            }
            _ => i += 1,
        }
    }
    if let Some(rp) = b.iter().position(|&c| c == b')') {
        if matches!(b.get(rp + 1), Some(b'D')) {
            return true;
        }
    }
    let mut pc = 0;
    while pc < code_len {
        if is_double_opcode(code[pc]) {
            return true;
        }
        pc += crate::bytecode_analysis::step(code, pc);
    }
    false
}

/// Double-typed opcodes (subset of `is_category2_opcode` that is double, not
/// long). `ldc2_w` (0x14) is deliberately omitted — it is long/double-ambiguous,
/// but the IR builder does not lower it (bails to single-pass), and any double
/// *constant* must be consumed by a double opcode listed here, so a double
/// `ldc2_w` method is caught regardless.
fn is_double_opcode(op: u8) -> bool {
    matches!(
        op,
        0x0e | 0x0f          // dconst_0, dconst_1
        | 0x18 | 0x26..=0x29 // dload, dload_0..3
        | 0x31 | 0x52        // daload, dastore
        | 0x39 | 0x47..=0x4a // dstore, dstore_0..3
        | 0x63 | 0x67 | 0x6b | 0x6f | 0x73 // dadd, dsub, dmul, ddiv, drem
        | 0x77               // dneg
        | 0x87 | 0x8a | 0x8d // i2d, l2d, f2d
        | 0x8e | 0x8f | 0x90 // d2i, d2l, d2f
        | 0x97 | 0x98        // dcmpl, dcmpg
        | 0xaf               // dreturn
    )
}

/// inc 30: float-typed opcodes (the cat-1 FP opcodes, complementing
/// `is_double_opcode`). Together they enumerate the full FP opcode space the IR
/// builder can now lower (or must bail on). `f2d`/`d2f` (0x8d/0x90) appear in
/// both sets — harmless, the union is OR'd.
fn is_float_opcode(op: u8) -> bool {
    matches!(
        op,
        0x0b..=0x0d          // fconst_0..2
        | 0x17 | 0x22..=0x25 // fload, fload_0..3
        | 0x30 | 0x51        // faload, fastore
        | 0x38 | 0x43..=0x46 // fstore, fstore_0..3
        | 0x62 | 0x66 | 0x6a | 0x6e | 0x72 // fadd, fsub, fmul, fdiv, frem
        | 0x76               // fneg
        | 0x86 | 0x89        // i2f, l2f
        | 0x8b | 0x8c | 0x8d // f2i, f2l, f2d
        | 0x90               // d2f
        | 0x95 | 0x96        // fcmpl, fcmpg
        | 0xae               // freturn
    )
}

/// inc 30: any float OR double opcode in the body. A method containing one is an
/// FP method and (when FP-free in its signature) is admitted to the IR path only
/// via the `ir_emit_fp` gate clause — so with the gate off no FP opcode ever
/// reaches the IR builder.
pub(crate) fn fp_in_body(code: &[u8], code_len: usize) -> bool {
    let mut pc = 0;
    while pc < code_len {
        if is_double_opcode(code[pc]) || is_float_opcode(code[pc]) {
            return true;
        }
        pc += crate::bytecode_analysis::step(code, pc);
    }
    false
}

/// inc 30: a `float`/`double` appears in the method's signature (any `F`/`D`
/// parameter or the return type). Such a method needs XMM-register argument /
/// return marshalling the IR prologue/epilogue do not yet emit, so it is kept
/// off the FP IR path (a follow-on). Mirrors `method_uses_double`'s descriptor
/// walk but matches both `F` and `D`.
fn fp_in_descriptor(descriptor: &str) -> bool {
    DescriptorParamIter::new(descriptor).any(|tag| matches!(tag, b'F' | b'D'))
        || matches!(return_type(descriptor), b'F' | b'D')
}

/// inc 30: the method uses `float`/`double` anywhere — signature OR body. Used by
/// the int/long IR-path clauses to stay FP-free.
pub(crate) fn method_uses_fp(code: &[u8], code_len: usize, descriptor: &str) -> bool {
    fp_in_descriptor(descriptor) || fp_in_body(code, code_len)
}

/// Gap B (inc 22): classify a static-call descriptor for the `Op::Call` slice.
/// Returns `Some((num_args, ret_type))` iff EVERY parameter is a single-slot
/// value the marshaller can pass as one i64 — an int-category primitive
/// (`I`/`Z`/`B`/`C`/`S`) OR a reference (`L…`/`[…`, passed as the raw pointer) —
/// and the return is int-category, `void`, or a reference. `None` for any
/// `long`/`float`/`double` parameter or return (category-2 / XMM register, not
/// handled by the int/pointer-only GPR marshalling), keeping the method on
/// single-pass.
///
/// Reference args/returns are GC-sound across the call: while a JIT frame is
/// active the GC is non-moving (`gc_quiescence`), and the conservative root
/// scan of the IR spill frame finds (and pins) any pointer in a slot — so a
/// reference live across the call is neither relocated nor reclaimed, with no
/// precise oop map required (inc 22 — see `activate-ir-optimizer.md`).
pub fn static_call_shape(descriptor: &str) -> Option<(usize, u8)> {
    if !descriptor.starts_with('(') {
        return None;
    }
    let mut num_args = 0usize;
    for tag in DescriptorParamIter::new(descriptor) {
        match tag {
            b'I' | b'Z' | b'B' | b'C' | b'S' => num_args += 1,
            // A `long`/`double`/`float` arg is ONE i64 slot in the compact JIT
            // ABI: the VM marshals every value (FP as `to_bits() as i64`) into
            // one INTEGER arg register, not XMM. `J` args: inc 28; `D`/`F`
            // args: inc 34. Only reachable under `ir_emit_long`/`ir_emit_fp`.
            b'J' | b'D' | b'F' => num_args += 1,
            b'L' | b'[' => num_args += 1,
            // Any other byte is a malformed descriptor.
            _ => return None,
        }
    }
    let ret = return_type(descriptor);
    match ret {
        b'I' | b'Z' | b'B' | b'C' | b'S' | b'V' | b'L' | b'[' => Some((num_args, ret)),
        // `J` (long) return: accepted post-inc-29. The result is one i64 slot in
        // RAX (the compact JIT ABI); the IR builder types the `Op::Call` node as
        // `IrType::Long`, and the call-site post-invoke check disambiguates a
        // legitimate `Long.MIN_VALUE` return from the `i64::MIN` deopt sentinel
        // via the out-of-band `dispatch_threw` peek. Only reachable under
        // `ir_emit_long` (consuming a long result needs a category-2 opcode), so
        // inert for the default int/ref path.
        b'J' => Some((num_args, ret)),
        // `D` (double) return: accepted (inc 32). The result rides RAX as a clean
        // 64-bit bit pattern (the i64 return ABI); the IR builder types the
        // `Op::Call` node `IrType::Double`, and the call-site post-invoke check
        // disambiguates a real `-0.0`/other double whose bits == `i64::MIN` from
        // the deopt sentinel via the out-of-band `dispatch_threw` peek (the check
        // already matches `IrType::Double`). `D`/`F` *args* are still rejected
        // (the arg loop above) — they ride XMM the marshaller does not yet emit.
        b'D' => Some((num_args, ret)),
        // `F` (float) return: accepted (inc 33). The 32-bit result rides the low
        // 32 of RAX (the i64 return ABI); the IR builder types the `Op::Call`
        // `IrType::Float` and the call-site `dispatch_threw` peek disambiguates a
        // `+0.0f`-with-stale-upper-bits ↔ `i64::MIN` collision. `D`/`F` *args* are
        // still rejected (the arg loop) — they ride XMM the marshaller lacks.
        b'F' => Some((num_args, ret)),
        _ => None,
    }
}

/// JVM-spec param slot count: longs and doubles take 2 slots each (per
/// JVMS §2.6.1), unlike `count_param_slots` which counts each parameter
/// as exactly one slot (matching our compact ABI representation where
/// every primitive fits in one i64 register).
///
/// Used for **local-variable layout** in the JIT prologue and register
/// allocator: javac emits `lload_2` / `dstore_3` etc. assuming JVM-spec
/// slot numbering (e.g. for a `(JJ)V` method, the second long lives at
/// local 2, not local 1). Without this distinction the JIT prologue
/// stores the second long arg into local 1 and `lload_2` reads
/// uninitialized memory — see TransactionCounter @Test bug.
pub fn count_param_slots_jvm_spec(descriptor: &str) -> usize {
    let bytes = descriptor.as_bytes();
    if bytes.is_empty() || bytes[0] != b'(' {
        return 0;
    }
    let mut i = 1;
    let mut slots = 0;
    while i < bytes.len() && bytes[i] != b')' {
        match bytes[i] {
            b'I' | b'F' | b'B' | b'C' | b'S' | b'Z' => {
                slots += 1;
                i += 1;
            }
            b'J' | b'D' => {
                // Category 2: long / double take TWO local slots (JVMS §2.6.1).
                slots += 2;
                i += 1;
            }
            b'L' => {
                while i < bytes.len() && bytes[i] != b';' {
                    i += 1;
                }
                i += 1;
                slots += 1;
            }
            b'[' => {
                i += 1;
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
                slots += 1;
            }
            _ => {
                i += 1;
            }
        }
    }
    slots
}

/// Per-parameter destination local slot under JVM-spec layout. Returns a
/// vector whose `i`-th entry is the spec-slot index for the `i`-th
/// parameter (compact ABI ordering — one entry per ABI register slot).
///
/// For descriptor `(JI)V` the result is `[0, 2]`: the first arg is a
/// long (occupies slots 0+1), so the second arg lands at slot 2. The
/// JIT prologue uses this map to copy each ABI register to the
/// matching JVM-spec local slot (instead of the compact-index slot
/// `i`, which silently wrote longs into the wrong slot).
pub fn param_spec_slot_indices(descriptor: &str) -> Vec<usize> {
    let mut out = Vec::new();
    if !descriptor.starts_with('(') {
        return out;
    }
    let mut slot = 0usize;
    for tag in DescriptorParamIter::new(descriptor) {
        // An unrecognised byte is skipped without a slot, as before.
        let width = match tag {
            b'J' | b'D' => 2,
            b'I' | b'F' | b'B' | b'C' | b'S' | b'Z' | b'L' | b'[' => 1,
            _ => continue,
        };
        out.push(slot);
        slot += width;
    }
    out
}

/// The int-category return tag that JVMS §6.5 `ireturn` narrows (`Z`, `B`,
/// `C` or `S`), or `None` when the return needs no narrowing (`I`, or not an
/// int-category return at all).
///
/// Accepts a bare descriptor or a `"Class.method:descriptor"` method key. It
/// reads only the last two bytes: an int-category return is the single byte
/// after the closing `)`, while a reference return ends in `;` and a
/// primitive-array return has `[` before its element byte, so neither can
/// match. That also keeps it immune to a `)` inside a class name, which a
/// scan for the first or last `)` is not.
///
/// The interpreter bridge narrows the same four tags (`narrow_int_return` in
/// `jit_bridge.rs`); both compiled tiers now narrow at `ireturn` too, so a
/// compiled caller never sees a `boolean` of 2.
pub fn narrowed_int_return_tag(descriptor: &str) -> Option<u8> {
    match descriptor.as_bytes() {
        [.., b')', tag @ (b'Z' | b'B' | b'C' | b'S')] => Some(*tag),
        _ => None,
    }
}

/// Parse the return type from a method descriptor. Returns 'I', 'J', 'V', etc.
pub fn return_type(descriptor: &str) -> u8 {
    let bytes = descriptor.as_bytes();
    for i in 0..bytes.len() {
        if bytes[i] == b')' && i + 1 < bytes.len() {
            return bytes[i + 1];
        }
    }
    b'V'
}

/// Whether an `invokestatic` self-call can safely use the backend raw tail jump.
///
/// Non-tail recursive compiled calls grow the native stack and bypass the
/// dispatch helpers' stack-depth guard. Tail self-calls are different: x64 lowers
/// them to a jump back to the method body, so they do not consume another native
/// frame and can keep the raw path.
#[inline]
pub fn invokestatic_self_call_uses_tail_jump(code: &[u8], code_len: usize, pc: usize) -> bool {
    pc + 3 < code_len && pc + 3 < code.len() && matches!(code[pc + 3], 0xac..=0xb0)
}
