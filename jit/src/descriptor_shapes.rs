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

/// True if the method uses any category-2 (long/double) value — as a
/// parameter, return type, or operand/result of a long/double bytecode.
///
/// The int/ref admission clause of the tier gate (`ir_value_shape_admitted`);
/// a `long` method reaches the IR path only through the `ir_emit_long` clause
/// and a `double` one only through `ir_emit_fp`. (Round 11: this used to say
/// the IR types every value as 32-bit `Int`, which stopped being true when
/// `IrType::Long`/`Double` landed.) Note: a `long[]`/`double[]` parameter is a
/// *reference* (category-1) and does NOT count here — only scalar J/D do.
///
/// Purely syntactic: a long produced and consumed only by polymorphic opcodes
/// (`invokestatic ()J` straight into `pop2`, a `getfield J` straight into a
/// `putfield J`) is not seen here; such a value is typed by the builder from
/// the resolved descriptor, not by this gate.
pub(crate) fn method_uses_category2(code: &[u8], code_len: usize, descriptor: &str) -> bool {
    // Scalar long/double parameter? The iterator yields `[` for a whole array
    // type, so `[J` / `[D` (references, category-1) never read as `J` / `D`.
    if DescriptorParamIter::new(descriptor).any(|tag| matches!(tag, b'J' | b'D')) {
        return true;
    }
    // Long/double return type. Through `return_type`, which PARSES the
    // parameter list: the first `)` can sit inside a class name (JVMS 4.2.2
    // bars only `. ; [ /`), so `(LA)B;)J` returns `long` while a scan for the
    // first `)` read its `B` and let a long-returning method through this gate
    // (round 11, irlower -- the same defect `return_type` was fixed for).
    if matches!(return_type(descriptor), b'J' | b'D') {
        return true;
    }
    // Any long/double bytecode in the body.
    let mut pc = 0;
    while pc < code_len {
        if is_category2_opcode(executing_opcode(code, pc)) {
            return true;
        }
        pc += crate::bytecode_analysis::step(code, pc);
    }
    false
}

/// The opcode that EXECUTES at `pc`: the byte after a `wide` prefix (`0xc4`).
///
/// Every classification in this file walks the body with
/// `bytecode_analysis::step`, which skips a widened instruction WHOLE — so a
/// loop reading `code[pc]` alone sees `0xc4` and never the `lload` / `dstore` /
/// `fload` inside it. That is the same blindness `bytecode_analysis::
/// is_wide_ret` exists for one module over, and here it makes the gate answer
/// "no category-2 value" about a method that has one: a method with more than
/// 255 locals can hold its only `long` in a `wide lload` / `wide lstore` pair,
/// whose operands need the two-byte local index and therefore the prefix, while
/// every consumer that would have given it away (`ladd`, `lcmp`, `lreturn`) has
/// no widened form at all.
///
/// A trailing `wide` with no opcode after it is malformed; report the prefix,
/// which no predicate here matches, exactly as before.
#[inline]
fn executing_opcode(code: &[u8], pc: usize) -> u8 {
    let op = code.get(pc).copied().unwrap_or(0);
    if op == 0xc4 {
        code.get(pc + 1).copied().unwrap_or(0xc4)
    } else {
        op
    }
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
    for tag in DescriptorParamIter::new(descriptor) {
        let Some(width) = param_tag_width(tag) else {
            continue;
        };
        slots.push(slot);
        slot += width;
    }
    (slots, slot)
}

/// How many JVM local slots a parameter of descriptor tag `tag` occupies (JVMS
/// 2.6.1: two for `J`/`D`, one for every other type), or `None` for a byte that
/// is not a field-type tag.
///
/// The ONE decision every parameter walker in this file makes about a stray
/// byte (round 11 wave 2, lane lower;
/// `r11-irlower-descriptor-walkers-disagree-on-malformed-tags`): it is not a
/// parameter — no slot, no argument, no entry. That is what
/// `cratonvm_jit_api::count_param_slots` (the ABI argument count every caller
/// sizes its marshalling by) and `param_spec_slot_indices` always did; the
/// prologue slot walk, the oop mask and the IR parameter types used to count
/// it as a one-slot `int`, so the four could disagree about where parameter
/// `k` lives. [`static_call_shape`] still REFUSES such a descriptor. A
/// verified class file cannot carry one.
fn param_tag_width(tag: u8) -> Option<usize> {
    match tag {
        b'J' | b'D' => Some(2),
        b'I' | b'F' | b'B' | b'C' | b'S' | b'Z' | b'L' | b'[' => Some(1),
        _ => None,
    }
}

/// One tag per JVM local slot of the parameter region, in slot order: `L` for
/// `this` and for a reference or array parameter, `I` for the int family
/// (`I`/`B`/`C`/`S`/`Z`), `J`/`F`/`D` for the others, and `0` for the second
/// slot of a `J`/`D`. Walks the slots exactly as [`compute_param_jvm_slots`]
/// does (a stray byte is no parameter, `param_tag_width`), so the non-zero
/// positions are its slot list.
///
/// The width source for a parameter the method never loads or stores, which
/// the single-pass backend's load/store kind scan cannot type
/// (`x64::BackendRequest::param_slot_tags`; interpreter round i1 wave 22,
/// lane L1).
pub fn param_slot_kind_tags(descriptor: &str, is_static: bool) -> Vec<u8> {
    let mut tags = Vec::new();
    if !is_static {
        tags.push(b'L');
    }
    for tag in DescriptorParamIter::new(descriptor) {
        match tag {
            b'J' | b'D' => {
                tags.push(tag);
                tags.push(0);
            }
            b'I' | b'B' | b'C' | b'S' | b'Z' => tags.push(b'I'),
            b'F' => tags.push(b'F'),
            b'L' | b'[' => tags.push(b'L'),
            // Not a field-type tag: no slot, as in `param_tag_width`.
            _ => {}
        }
    }
    tags
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
    for tag in DescriptorParamIter::new(descriptor) {
        // A stray byte is no parameter (`param_tag_width`), exactly as in
        // `compute_param_jvm_slots`, so the bit positions stay its slots.
        let Some(width) = param_tag_width(tag) else {
            continue;
        };
        if matches!(tag, b'L' | b'[') && slot < 64 {
            mask |= 1u64 << slot;
        }
        // Category-2 scalars take two slots, primitives one; neither is an oop.
        slot += width;
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
    if !descriptor.starts_with('(') {
        return Vec::new();
    }
    DescriptorParamIter::new(descriptor)
        .filter_map(|tag| match tag {
            b'I' | b'B' | b'C' | b'S' | b'Z' => Some(b'I'),
            b'F' | b'J' | b'D' => Some(tag),
            b'L' | b'[' => Some(b'L'),
            // A stray byte is no argument (`param_tag_width`).
            _ => None,
        })
        .collect()
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
    types.extend(
        DescriptorParamIter::new(descriptor).filter_map(|tag| match tag {
            b'J' => Some(ir::IrType::Long),
            b'D' => Some(ir::IrType::Double),
            b'F' => Some(ir::IrType::Float),
            b'L' | b'[' => Some(ir::IrType::Ref),
            b'I' | b'Z' | b'B' | b'C' | b'S' => Some(ir::IrType::Int),
            // A stray byte is no parameter (`param_tag_width`).
            _ => None,
        }),
    );
    types
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
        let op = executing_opcode(code, pc);
        if is_double_opcode(op) || is_float_opcode(op) {
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
/// Returns `Some((num_args, ret_type))` for every WELL-FORMED descriptor: each
/// parameter is one i64 in the compact JIT ABI -- an int-category primitive, a
/// reference (the raw pointer), or (since inc 28/34) a `long`/`float`/`double`
/// as its raw bits in an INTEGER register -- and every return tag is accepted
/// (`J`/`D`/`F` since inc 29/32/33, riding RAX). `None` for the malformed
/// descriptors it can see (a stray byte, no leading `(`, an unknown return
/// tag). Whether a `J`/`D`/`F` site is actually admitted is the `ir_emit_long`
/// / `ir_emit_fp` gates' decision, not this decoder's.
///
/// (Round 11, irlower: this comment used to say `long`/`float`/`double` were
/// refused here and that references were safe because "the GC is non-moving
/// while a JIT frame is active"; neither is true any more -- the call site
/// publishes an oop map, see `ir_lower`'s `Op::Call` arm.)
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
        // already matches `IrType::Double`). (`D`/`F` *args* have been accepted
        // by the arg loop above since inc 34, as raw bits in a GPR.)
        b'D' => Some((num_args, ret)),
        // `F` (float) return: accepted (inc 33). The 32-bit result rides the low
        // 32 of RAX (the i64 return ABI); the IR builder types the `Op::Call`
        // `IrType::Float` and the call-site `dispatch_threw` peek disambiguates a
        // `+0.0f`-with-stale-upper-bits ↔ `i64::MIN` collision.
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
    if !descriptor.starts_with('(') {
        return 0;
    }
    // Category 2: long / double take TWO local slots (JVMS §2.6.1); a stray
    // byte takes none (`param_tag_width`).
    DescriptorParamIter::new(descriptor)
        .filter_map(param_tag_width)
        .sum()
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
        // An unrecognised byte is skipped without a slot (`param_tag_width`).
        let Some(width) = param_tag_width(tag) else {
            continue;
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
///
/// The parameter list is PARSED rather than scanned for a `)`, because a class
/// name may contain one: JVMS 4.2.2 bars only `. ; [ /` from an internal-form
/// class name, so `(LA)B;)J` is a legal descriptor taking one `LA)B;` argument
/// and returning `long`. Scanning for the first `)` answered `B` for it, and
/// scanning for the last one answers the right byte only when the return type
/// is not itself an `L...;` naming such a class. [`DescriptorParamIter`] stops
/// exactly on the `)` that closes the parameter list, so the byte after it is
/// the return tag — the same reasoning [`narrowed_int_return_tag`] states about
/// reading the descriptor's tail instead of hunting for a `)`.
///
/// `V` when there is no such byte (a malformed or truncated descriptor), which
/// is what the scan answered too.
pub fn return_type(descriptor: &str) -> u8 {
    let mut params = DescriptorParamIter::new(descriptor);
    while params.next().is_some() {}
    params.bytes.get(params.pos + 1).copied().unwrap_or(b'V')
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

#[cfg(test)]
mod shape_decoder_tests {
    //! The two decoding rules a scan cannot get right by looking at one byte:
    //! which `)` closes the parameter list, and which opcode a `wide` prefix
    //! hides. Both are pure functions of a `&str` / `&[u8]`, so these need no
    //! fixture at all — the argument for this file, applied to it.
    use super::*;

    /// inc 25: like `method_uses_category2`, but flags only **double**. It
    /// admitted long-only methods to the IR path while double stayed on
    /// single-pass, and has had no production caller since the FP tier moved
    /// admission to `method_uses_fp` / `fp_in_body`. Round 11 (irlower) moved
    /// it here, beside the decoder tests that are its only readers, rather
    /// than leave dead code in the production module.
    fn method_uses_double(code: &[u8], code_len: usize, descriptor: &str) -> bool {
        let params_have_double = DescriptorParamIter::new(descriptor).any(|tag| tag == b'D');
        // `return_type`, not a scan for the first `)` -- see `method_uses_category2`.
        if params_have_double || return_type(descriptor) == b'D' {
            return true;
        }
        let mut pc = 0;
        while pc < code_len {
            if is_double_opcode(executing_opcode(code, pc)) {
                return true;
            }
            pc += crate::bytecode_analysis::step(code, pc);
        }
        false
    }

    /// A class name may contain `)` (JVMS 4.2.2 bars only `. ; [ /`), so the
    /// return tag is found by PARSING the parameters, not by scanning.
    #[test]
    fn a_parameter_class_name_containing_a_paren_does_not_hide_the_return_type() {
        // One argument of class `A)B`, returning `long`.
        assert_eq!(return_type("(LA)B;)J"), b'J');
        // ...and the same with a reference return, which is the case that
        // matters: read as `J` the call's result is a number, so nothing marks
        // it as a live oop.
        assert_eq!(return_type("(LA)B;)Ljava/lang/Object;"), b'L');
        assert_eq!(
            static_call_shape("(LA)B;)Ljava/lang/Object;"),
            Some((1, b'L'))
        );
    }

    /// Round 11 (irlower): the two tier gates read the return tag through
    /// `return_type` too. They used to take the byte after the FIRST `)`, so a
    /// `long`/`double`-returning method whose parameter class name holds a `)`
    /// read as category-2-free.
    #[test]
    fn r11_a_paren_in_a_class_name_does_not_hide_a_wide_return_from_the_gates() {
        // iconst_0 ; pop ; return -- no category-2 opcode anywhere in the body.
        let code = [0x03, 0x57, 0xb1];
        assert!(method_uses_category2(&code, code.len(), "(LA)B;)J"));
        assert!(method_uses_category2(&code, code.len(), "(LA)B;)D"));
        assert!(method_uses_double(&code, code.len(), "(LA)B;)D"));
        // ...and the controls: an int return is still category-1.
        assert!(!method_uses_category2(&code, code.len(), "(LA)B;)I"));
        assert!(!method_uses_double(&code, code.len(), "(LA)B;)J"));
    }

    /// The ordinary shapes, unchanged.
    #[test]
    fn the_ordinary_descriptors_still_decode() {
        assert_eq!(return_type("()V"), b'V');
        assert_eq!(return_type("(II)I"), b'I');
        assert_eq!(return_type("(J)D"), b'D');
        assert_eq!(return_type("([[I)[Ljava/lang/String;"), b'[');
        assert_eq!(return_type("(Ljava/lang/String;)Z"), b'Z');
        // Malformed / truncated answers `V`, as the scan did.
        assert_eq!(return_type(""), b'V');
        assert_eq!(return_type("(I)"), b'V');
        assert_eq!(return_type("I"), b'V');
    }

    /// `wide lload` / `wide lstore` is the one way a method can hold a `long`
    /// with no un-widened category-2 opcode anywhere, and `step` skips the
    /// whole instruction — so the gate must look past the prefix.
    #[test]
    fn a_widened_lload_is_a_category2_opcode() {
        // wide lload 300 ; wide lstore 302 ; return
        let code = [0xc4, 0x16, 0x01, 0x2C, 0xc4, 0x37, 0x01, 0x2E, 0xb1];
        assert!(
            method_uses_category2(&code, code.len(), "()V"),
            "a `wide lload` body reads as category-2-free"
        );
        // wide dload 300 ; return  — the double-only gate sees it too.
        let dcode = [0xc4, 0x18, 0x01, 0x2C, 0xb1];
        assert!(method_uses_double(&dcode, dcode.len(), "()V"));
        assert!(fp_in_body(&dcode, dcode.len()));
        // wide fload 300 ; return  — float, for the FP gate.
        let fcode = [0xc4, 0x17, 0x01, 0x2C, 0xb1];
        assert!(fp_in_body(&fcode, fcode.len()));
        assert!(method_uses_fp(&fcode, fcode.len(), "()V"));
        assert!(!method_uses_double(&fcode, fcode.len(), "()V"));
    }

    /// A widened INT instruction is not category-2, and neither is `wide iinc`
    /// (six bytes, the one widened form with a second operand).
    #[test]
    fn a_widened_int_instruction_is_not_category2() {
        // wide iload 300 ; wide iinc 300, 1 ; wide istore 300 ; return
        let code = [
            0xc4, 0x15, 0x01, 0x2C, 0xc4, 0x84, 0x01, 0x2C, 0x00, 0x01, 0xc4, 0x36, 0x01, 0x2C,
            0xb1,
        ];
        assert!(!method_uses_category2(&code, code.len(), "()V"));
        assert!(!fp_in_body(&code, code.len()));
    }

    /// Round 11 wave 2 (lane lower,
    /// `r11-irlower-descriptor-walkers-disagree-on-malformed-tags`): every
    /// parameter walker agrees with the ABI argument count
    /// (`cratonvm_jit_api::count_param_slots`) and with each other about how
    /// many parameters there are and which JVM slot each occupies — for
    /// well-formed descriptors and for a stray byte, which is no parameter.
    #[test]
    fn every_parameter_walker_agrees_on_count_and_layout() {
        for d in [
            "()V",
            "(I)V",
            "(JI)J",
            "([[JLx;D)V",
            "(LA)B;)J",
            "(Q)V",
            "(ZQJ[Ljava/lang/String;)V",
        ] {
            let abi_args = cratonvm_jit_api::count_param_slots(d);
            let (slots, span) = compute_param_jvm_slots(d, true);
            assert_eq!(slots.len(), abi_args, "{d}: prologue slots vs ABI args");
            assert_eq!(
                ir_param_types(d, true).len(),
                abi_args,
                "{d}: IR param types"
            );
            assert_eq!(indy_arg_type_tags(d).len(), abi_args, "{d}: indy tags");
            assert_eq!(param_spec_slot_indices(d), slots, "{d}: spec slot indices");
            assert_eq!(count_param_slots_jvm_spec(d), span, "{d}: spec slot span");
            // The oop mask names exactly the reference parameters' slots.
            let mut want = 0u64;
            let refs = DescriptorParamIter::new(d).filter(|t| param_tag_width(*t).is_some());
            for (tag, &slot) in refs.zip(slots.iter()) {
                if matches!(tag, b'L' | b'[') {
                    want |= 1u64 << slot;
                }
            }
            assert_eq!(compute_param_oop_mask(d, true), want, "{d}: oop mask");
            // The per-slot kind tags cover the span, and their non-zero
            // positions are the prologue's slots (interpreter round i1 wave 22).
            let tags = param_slot_kind_tags(d, true);
            assert_eq!(tags.len(), span, "{d}: kind tags span");
            let tagged: Vec<usize> = (0..tags.len()).filter(|&s| tags[s] != 0).collect();
            assert_eq!(tagged, slots, "{d}: kind tag slots");
            // The call-shape decoder is the one that REFUSES a stray byte.
            let malformed = d.contains('Q');
            assert_eq!(static_call_shape(d).is_none(), malformed, "{d}: call shape");
        }
        // An instance method adds `this` at slot 0, as a reference.
        assert_eq!(compute_param_jvm_slots("(JI)V", false), (vec![0, 1, 3], 4));
        assert_eq!(compute_param_oop_mask("(JLx;)V", false), 0b1001);
        assert_eq!(
            param_slot_kind_tags("(JZF[ID)V", false),
            vec![b'L', b'J', 0, b'I', b'F', b'L', b'D', 0]
        );
    }

    /// A trailing `wide` with nothing after it is malformed bytecode; it must
    /// answer like the prefix itself rather than read past the body.
    #[test]
    fn a_truncated_wide_prefix_is_not_a_category2_opcode() {
        let code = [0xc4];
        assert!(!method_uses_category2(&code, code.len(), "()V"));
        assert!(!fp_in_body(&code, code.len()));
    }
}
