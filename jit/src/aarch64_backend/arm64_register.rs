#[allow(unused_imports)]
use super::*;


// ---------------------------------------------------------------------------
// Arm64Register
// ---------------------------------------------------------------------------

/// Lightweight register identifier for the backend pipeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Arm64Register(pub u8);

impl Arm64Register {
    // Argument / result registers
    pub const X0: Self = Self(0);
    pub const X1: Self = Self(1);
    pub const X2: Self = Self(2);
    pub const X3: Self = Self(3);
    pub const X4: Self = Self(4);
    pub const X5: Self = Self(5);
    pub const X6: Self = Self(6);
    pub const X7: Self = Self(7);

    // Scratch / temporary registers
    pub const X8: Self = Self(8);
    pub const X9: Self = Self(9);
    pub const X10: Self = Self(10);
    pub const X11: Self = Self(11);
    pub const X12: Self = Self(12);
    pub const X13: Self = Self(13);
    pub const X14: Self = Self(14);
    pub const X15: Self = Self(15);
    pub const X16: Self = Self(16); // IP0
    pub const X17: Self = Self(17); // IP1
    pub const X18: Self = Self(18); // Platform register

    // Callee-saved registers (for locals)
    pub const X19: Self = Self(19);
    pub const X20: Self = Self(20);
    pub const X21: Self = Self(21);
    pub const X22: Self = Self(22);
    pub const X23: Self = Self(23);
    pub const X24: Self = Self(24);
    pub const X25: Self = Self(25);
    pub const X26: Self = Self(26);
    pub const X27: Self = Self(27);
    pub const X28: Self = Self(28);

    // Special registers
    pub const FP: Self = Self(29);
    pub const LR: Self = Self(30);
    pub const SP: Self = Self(31);
    pub const XZR: Self = Self(31); // Context-dependent zero register

    // NEON SIMD registers (encoded as 32+n)
    pub const V0: Self = Self(32);
    pub const V1: Self = Self(33);
    pub const V2: Self = Self(34);
    pub const V3: Self = Self(35);
    pub const V4: Self = Self(36);
    pub const V5: Self = Self(37);
    pub const V6: Self = Self(38);
    pub const V7: Self = Self(39);

    /// Returns `true` if this is a callee-saved general-purpose register (X19-X28).
    pub fn is_callee_saved(self) -> bool {
        (19..=28).contains(&self.0)
    }

    /// Returns `true` if this register is used for integer argument passing.
    pub fn is_arg_reg(self) -> bool {
        self.0 <= 7
    }

    /// Raw numeric index.
    pub fn index(self) -> u8 {
        self.0
    }
}

// ---------------------------------------------------------------------------
// Arm64Condition
// ---------------------------------------------------------------------------

/// ARM64 condition codes mapping to the NZCV flags.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arm64Condition {
    /// Equal (Z=1)
    Eq,
    /// Not equal (Z=0)
    Ne,
    /// Signed less than (N!=V)
    Lt,
    /// Signed less or equal (Z=1 or N!=V)
    Le,
    /// Signed greater than (Z=0 and N=V)
    Gt,
    /// Signed greater or equal (N=V)
    Ge,
    /// Unsigned higher (C=1 and Z=0)
    Hi,
    /// Unsigned lower or same (C=0 or Z=1)
    Ls,
    /// Carry set / unsigned higher or same (C=1)
    Cs,
    /// Carry clear / unsigned lower (C=0)
    Cc,
    /// Minus / negative (N=1). After `FCMP` this is "less than" and is FALSE on
    /// unordered, which is what `fcmpg`/`dcmpg` need (`Lt` is true on it).
    Mi,
    /// Plus / positive or zero (N=0)
    Pl,
    /// Overflow (V=1). After `FCMP`: unordered.
    Vs,
    /// No overflow (V=0)
    Vc,
    /// Always
    Al,
}

// ---------------------------------------------------------------------------
// Arm64Instruction
// ---------------------------------------------------------------------------

/// The access width of an [`Arm64Instruction::MemLoad`] /
/// [`Arm64Instruction::MemStore`] (round 9 wave 10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arm64MemWidth {
    /// One byte: `LDRB`/`STRB`, `LDARB`/`STLRB`.
    B8,
    /// Two bytes: `LDRH`/`STRH`, `LDARH`/`STLRH`.
    H16,
    /// Four bytes: `LDR Wt`/`STR Wt`, `LDAR Wt`/`STLR Wt`.
    W32,
    /// Eight bytes: `LDR Xt`/`STR Xt`, `LDAR Xt`/`STLR Xt`.
    X64,
}

/// A `getstatic` site resolved by the caller (round 9 wave 10).
///
/// The same facts the x64 single-pass tier's inline `getstatic`
/// (`x64/objects.rs` `try_emit_inline_getstatic`) works from:
///
/// * `base_cell` -- the address of the class's never-freed statics-base
///   POINTER cell, i.e. `DirectHelperTable::resolve_static_base(class_id,
///   field_index)`. The compiled read is `[[base_cell] + field_index *
///   SLOT_SIZE + payload]`, exactly x64's two dependent loads plus the cell
///   read. The resolver answers only for a class that is already
///   initialized, and initialization is monotonic, so no class-init check is
///   emitted -- and `class_id` is also recorded on the artifact
///   (`CompiledMethod::static_init_classes`) as x64 does;
/// * `type_tag` -- the field descriptor's first byte. Only primitives lower;
///   anything else refuses the method;
/// * `is_volatile` -- whether the read must be an `LDAR` (JMM acquire) rather
///   than a plain `LDR`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Arm64StaticField {
    pub class_id: u32,
    pub field_index: usize,
    pub base_cell: u64,
    pub type_tag: u8,
    pub is_volatile: bool,
}

/// Every `getstatic` (`0xb2`) and `putstatic` (`0xb3`) site in `bytecode` as
/// `(pc, opcode, constant-pool index)`, found by an instruction-length walk
/// (never a byte scan, which would read an operand byte as an opcode). Stops
/// at the first undecodable instruction. The caller resolves each index into
/// an [`Arm64StaticField`] and hands the map to
/// [`Arm64Backend::set_static_field_info`].
///
/// Both opcodes, since round 9 wave 17: the two need the same four facts, and
/// `base_cell` is what BOTH of them treat as the caller's proof that the
/// declaring class is already initialized — the read because it dereferences
/// that cell, the write because it may not let `<clinit>` run inside its
/// helper.
pub fn static_field_sites(bytecode: &[u8]) -> Vec<(usize, u8, u16)> {
    let mut sites = Vec::new();
    let mut pc = 0usize;
    while pc < bytecode.len() {
        let op = bytecode[pc];
        if op == 0xb2 || op == 0xb3 {
            if let (Some(&hi), Some(&lo)) = (bytecode.get(pc + 1), bytecode.get(pc + 2)) {
                sites.push((pc, op, u16::from_be_bytes([hi, lo])));
            }
        }
        match crate::bytecode_analysis::insn_len(bytecode, pc) {
            Some(len) if len > 0 => pc += len,
            _ => break,
        }
    }
    sites
}

/// A `getfield`/`putfield` site resolved by the caller (round 9 wave 15).
///
/// The instance twin of [`Arm64StaticField`], and deliberately much smaller:
/// a static read is a pair of dependent loads off a never-freed cell this
/// backend can bake as an immediate, while an instance access goes through a
/// runtime helper, which needs only the slot index and the field's type.
///
/// * `field_index` -- the resolved slot, the same number
///   `cp_field_resolver` hands the x64 tier;
/// * `type_tag` -- the field descriptor's first byte. Only primitives lower;
///   a reference field refuses the method, because its store owes the SATB
///   pre-barrier and the card write-barrier that only `jit_putfield_object`
///   runs;
/// * `is_volatile` -- whether the access owes the JMM's fences. For a store
///   THROUGH A HELPER that is a `DMB ISH` on BOTH sides; see
///   [`Arm64Backend::emit_putfield`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Arm64InstanceField {
    pub field_index: usize,
    pub type_tag: u8,
    pub is_volatile: bool,
}

/// Every `getfield` (`0xb4`) and `putfield` (`0xb5`) site in `bytecode` as
/// `(pc, opcode, constant-pool index)`.
///
/// The instance-field twin of [`static_field_sites`], and an instruction-length
/// walk for the same reason: a byte scan would read a `0xb4` that is some
/// other instruction's operand as an opcode and resolve a constant-pool
/// index that site never names. The caller resolves each index into an
/// [`Arm64InstanceField`] and hands the map to
/// [`Arm64Backend::set_instance_field_info`].
pub fn instance_field_sites(bytecode: &[u8]) -> Vec<(usize, u8, u16)> {
    let mut sites = Vec::new();
    let mut pc = 0usize;
    while pc < bytecode.len() {
        let op = bytecode[pc];
        if op == 0xb4 || op == 0xb5 {
            if let (Some(&hi), Some(&lo)) = (bytecode.get(pc + 1), bytecode.get(pc + 2)) {
                sites.push((pc, op, u16::from_be_bytes([hi, lo])));
            }
        }
        match crate::bytecode_analysis::insn_len(bytecode, pc) {
            Some(len) if len > 0 => pc += len,
            _ => break,
        }
    }
    sites
}

/// `CRATONVM_JIT_ARM64_FIELD_SITE_TRANSIENT` -- performance, **default on**
/// (interpreter round i5 wave 109, lane L2; page
/// `docs/known-issues/interpreter/aarch64-unloaded-field-owner-bail-lists-the-method.md`).
/// With it, `try_compile_inner`'s aarch64 block refuses a method with a
/// field site the compile cannot describe YET
/// ([`arm64_first_cold_field_site`]) as a TRANSIENT bail -- before it sets
/// `backend_attempted` -- so the hot door retries and compiles the method once
/// the owner has loaded (and, for a static, initialized), instead of
/// bail-listing it for the life of the VM. Read once per aarch64 compile (a
/// cold path); defined without a `cfg` so every host type-checks it.
pub fn arm64_field_site_transient_enabled() -> bool {
    true
}

/// The first field site (`(pc, opcode)`, in code order) of `bytecode` that a
/// compile right now cannot describe but a later one may, or `None`:
///
/// * a `getstatic` / `putstatic` whose `static_ready(cp_idx)` is `false`:
///   the static resolver answered nothing (owner not loaded: the field peek
///   loads nothing, `field_access.rs` `field_peek_never_loads_enabled`) or
///   `resolve_static_base` answered nothing (owner not initialized);
/// * a `getfield` / `putfield` whose `instance_known(cp_idx)` is `false`: the
///   instance resolver answered nothing (owner not loaded).
///
/// A `None` closure (the request carries no such resolver) counts nothing: no
/// retry can supply it, so the backend's own refusal stays permanent, as do
/// the refusals a later compile would repeat (a reference field, the SATB
/// marker row): those sites RESOLVE, so they are not cold. Pure; the
/// aarch64 block is its only production caller, and the tests run on every
/// host.
pub fn arm64_first_cold_field_site(
    bytecode: &[u8],
    static_ready: Option<&dyn Fn(u16) -> bool>,
    instance_known: Option<&dyn Fn(u16) -> bool>,
) -> Option<(usize, u8)> {
    let cold_static = static_ready.and_then(|ready| {
        static_field_sites(bytecode)
            .into_iter()
            .find(|&(_, _, cp_idx)| !ready(cp_idx))
    });
    let cold_instance = instance_known.and_then(|known| {
        instance_field_sites(bytecode)
            .into_iter()
            .find(|&(_, _, cp_idx)| !known(cp_idx))
    });
    match (cold_static, cold_instance) {
        (Some(s), Some(i)) => Some(if s.0 <= i.0 { (s.0, s.1) } else { (i.0, i.1) }),
        (Some(s), None) => Some((s.0, s.1)),
        (None, Some(i)) => Some((i.0, i.1)),
        (None, None) => None,
    }
}


/// A `new` (`0xbb`) or `anewarray` (`0xbd`) site resolved by the caller
/// (round 9 wave 18).
///
/// The aarch64 half of what `cp_new_resolver` reports as
/// [`crate::JitNewSite::Resolved`]. Only the resolved case is carried: a
/// `Deferred` site names a class nothing has loaded yet, and the CP-indexed
/// helper that resolves it at run time (`new_object_cp`) is a second entry
/// point this backend does not lower.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Arm64NewSite {
    /// For `new`, the class to allocate. For `anewarray`, the COMPONENT
    /// class — the array's own class is derived from it by the helper.
    pub class_id: u32,
    /// The instance field count the header's `num_slots` is set from. Read
    /// only by `new`; an `anewarray`'s length is an operand, not a constant.
    pub num_fields: usize,
}

/// Every `multianewarray` (`0xc5`) site: `(pc, cp_index)`, h23c.
///
/// An INSTRUCTION walk, like every other site scan here: a `0xc5` byte that is
/// some other instruction's operand is not an allocation, and resolving it
/// would hand the backend a constant-pool index that site never names. The
/// arity byte is deliberately NOT returned -- the backend reads it off the
/// opcode when it lowers, so the two can never disagree about which byte the
/// count came from.
pub fn multianewarray_sites(bytecode: &[u8]) -> Vec<(usize, u16)> {
    let mut sites = Vec::new();
    let mut pc = 0usize;
    while pc < bytecode.len() {
        let op = bytecode[pc];
        if op == 0xc5 {
            if let (Some(&hi), Some(&lo)) = (bytecode.get(pc + 1), bytecode.get(pc + 2)) {
                sites.push((pc, u16::from_be_bytes([hi, lo])));
            }
        }
        match crate::bytecode_analysis::insn_len(bytecode, pc) {
            Some(len) if len > 0 => pc += len,
            _ => break,
        }
    }
    sites
}

/// Every allocation site in `bytecode` as `(pc, opcode, operand)`:
/// `new` (`0xbb`) and `anewarray` (`0xbd`) with their constant-pool index,
/// `newarray` (`0xbc`) with its `atype` byte.
///
/// An instruction-length walk, for the same reason as [`static_field_sites`]
/// and [`instance_field_sites`]: a `0xbb` that is some other instruction's
/// operand is not an allocation, and resolving it would name a class this
/// method never mentions.
///
/// Two callers, and they want different halves of it. `jit/src/lib.rs`
/// resolves the `0xbb`/`0xbd` indices into [`Arm64NewSite`]s; `compile_pass`
/// asks only whether the list is EMPTY, because a method that allocates needs
/// a frame with safepoint homes and an id word whether or not polls are on.
/// Whether any INSTRUCTION in `bytecode` has an opcode `pred` accepts.
///
/// An instruction-length walk, like the three site tables above and for the
/// same reason: a byte that matches inside another instruction's operand is
/// not that instruction. Used by `compile_pass` for the two decisions it must
/// make from the bytecode alone, before the frame is sized -- whether this
/// method takes the VM context, and whether it makes a call that can
/// safepoint.
pub fn any_opcode(bytecode: &[u8], pred: fn(u8) -> bool) -> bool {
    let mut pc = 0usize;
    while pc < bytecode.len() {
        if pred(bytecode[pc]) {
            return true;
        }
        match crate::bytecode_analysis::insn_len(bytecode, pc) {
            Some(len) if len > 0 => pc += len,
            _ => break,
        }
    }
    false
}

pub fn allocation_sites(bytecode: &[u8]) -> Vec<(usize, u8, u16)> {
    let mut sites = Vec::new();
    let mut pc = 0usize;
    while pc < bytecode.len() {
        let op = bytecode[pc];
        match op {
            0xbb | 0xbd => {
                if let (Some(&hi), Some(&lo)) = (bytecode.get(pc + 1), bytecode.get(pc + 2)) {
                    sites.push((pc, op, u16::from_be_bytes([hi, lo])));
                }
            }
            0xbc => {
                if let Some(&atype) = bytecode.get(pc + 1) {
                    sites.push((pc, op, u16::from(atype)));
                }
            }
            _ => {}
        }
        match crate::bytecode_analysis::insn_len(bytecode, pc) {
            Some(len) if len > 0 => pc += len,
            _ => break,
        }
    }
    sites
}

/// Every `invoke*` in `bytecode`, as `(pc, opcode, constant-pool index)`.
///
/// An instruction-length walk, like the other site tables. The three widths
/// matter here more than anywhere else: `invokeinterface` (`0xb9`) is FIVE
/// bytes (index, an argument count, a zero) and `invokedynamic` (`0xba`) is
/// five as well (index, two zeros), so a naive three-byte stride reads the
/// trailing bytes as an opcode.
pub fn invoke_sites(bytecode: &[u8]) -> Vec<(usize, u8, u16)> {
    let mut sites = Vec::new();
    let mut pc = 0usize;
    while pc < bytecode.len() {
        let op = bytecode[pc];
        if matches!(op, 0xb6..=0xba) {
            if let (Some(&hi), Some(&lo)) = (bytecode.get(pc + 1), bytecode.get(pc + 2)) {
                sites.push((pc, op, u16::from_be_bytes([hi, lo])));
            }
        }
        match crate::bytecode_analysis::insn_len(bytecode, pc) {
            Some(len) if len > 0 => pc += len,
            _ => break,
        }
    }
    sites
}

/// Every `checkcast`/`instanceof` in `bytecode`, as `(pc, opcode, cp index)`.
pub fn typecheck_sites(bytecode: &[u8]) -> Vec<(usize, u8, u16)> {
    let mut sites = Vec::new();
    let mut pc = 0usize;
    while pc < bytecode.len() {
        let op = bytecode[pc];
        if matches!(op, 0xc0 | 0xc1) {
            if let (Some(&hi), Some(&lo)) = (bytecode.get(pc + 1), bytecode.get(pc + 2)) {
                sites.push((pc, op, u16::from_be_bytes([hi, lo])));
            }
        }
        match crate::bytecode_analysis::insn_len(bytecode, pc) {
            Some(len) if len > 0 => pc += len,
            _ => break,
        }
    }
    sites
}

/// One `checkcast`/`instanceof` site (round 9 wave 23): the target class as
/// the two things `jit_checkcast` / `jit_instanceof_check` take, a name and a
/// length.
///
/// A NAME rather than a class id, because that is the helpers' ABI -- and
/// deliberately the process-INTERNED name (`intern_typecheck_target_*`), not a
/// string this artifact owns: the helpers memoize on `(ptr, len)` in
/// thread-locals that outlive any one compilation, so a per-artifact copy
/// would miss the memo on every call and dangle in it afterwards.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Arm64TypecheckSite {
    /// Address of the interned name bytes.
    pub name_ptr: u64,
    /// Their length. Zero is "unresolved", which the helper answers `false`
    /// to -- a WRONG answer rather than a missing one, so this backend
    /// refuses such a site instead of passing it on.
    pub name_len: usize,
}

/// One `invoke*` site, as the CALLER resolved it (round 9 wave 22).
///
/// The whole of call-target resolution, reduced to what
/// [`Arm64Backend::emit_invoke`] needs. Nothing here is an entry point: the
/// target is named, not addressed, and `jit_invoke_dispatch` resolves it at
/// run time through the `JitInvokeInfo` this points at. That is the reason
/// this backend can have calls at all without the four pieces x64 needs -- see
/// the wave-22 section of the module header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Arm64InvokeSite {
    /// Address of the [`crate::JitInvokeInfo`] describing this site.
    ///
    /// Baked into the code as an immediate, so the box has to outlive the
    /// artifact: `lib.rs` parks it in `CompiledMethod::_jit_invoke_infos`,
    /// which is what every x64 dispatch site does with its own.
    pub info_ptr: u64,
    /// Operand-stack entries this site consumes: one per declared parameter
    /// (a `long` or `double` is ONE, this VM's compact convention), plus the
    /// receiver for every kind but `invokestatic` and `invokedynamic`.
    ///
    /// The same number `JitInvokeInfo::num_jit_args` carries, because the
    /// helper indexes the argument buffer by it.
    pub num_jit_args: usize,
    /// The descriptor's return tag: `V` for void, `L`/`[` for a reference.
    pub return_type: u8,
}

/// Backend IR instruction set for the ARM64 pipeline.
///
/// These are pseudo-instructions that map 1:1 to real ARM64 ops but carry
/// higher-level label references instead of raw offsets.
#[derive(Debug, Clone)]
pub enum Arm64Instruction {
    // -- Arithmetic --
    Add {
        rd: Arm64Register,
        rn: Arm64Register,
        rm: Arm64Register,
    },
    AddImm {
        rd: Arm64Register,
        rn: Arm64Register,
        imm: i32,
    },
    /// `ADD Xd, Xn, Xm, LSL #shift` -- the scaled-index form (round 9 wave
    /// 13). `shift` is `log2(element size)`, 0..=3 for a Java array.
    AddLsl {
        rd: Arm64Register,
        rn: Arm64Register,
        rm: Arm64Register,
        shift: u8,
    },
    Sub {
        rd: Arm64Register,
        rn: Arm64Register,
        rm: Arm64Register,
    },
    SubImm {
        rd: Arm64Register,
        rn: Arm64Register,
        imm: i32,
    },
    Mul {
        rd: Arm64Register,
        rn: Arm64Register,
        rm: Arm64Register,
    },
    SDiv {
        rd: Arm64Register,
        rn: Arm64Register,
        rm: Arm64Register,
    },
    Neg {
        rd: Arm64Register,
        rn: Arm64Register,
    },
    Madd {
        rd: Arm64Register,
        rn: Arm64Register,
        rm: Arm64Register,
        ra: Arm64Register,
    },
    Msub {
        rd: Arm64Register,
        rn: Arm64Register,
        rm: Arm64Register,
        ra: Arm64Register,
    },

    // -- Logical --
    And {
        rd: Arm64Register,
        rn: Arm64Register,
        rm: Arm64Register,
    },
    Orr {
        rd: Arm64Register,
        rn: Arm64Register,
        rm: Arm64Register,
    },
    Eor {
        rd: Arm64Register,
        rn: Arm64Register,
        rm: Arm64Register,
    },
    Lsl {
        rd: Arm64Register,
        rn: Arm64Register,
        rm: Arm64Register,
    },
    Lsr {
        rd: Arm64Register,
        rn: Arm64Register,
        rm: Arm64Register,
    },
    Asr {
        rd: Arm64Register,
        rn: Arm64Register,
        rm: Arm64Register,
    },

    // -- Compare --
    Cmp {
        rn: Arm64Register,
        rm: Arm64Register,
    },
    CmpImm {
        rn: Arm64Register,
        imm: i32,
    },
    Tst {
        rn: Arm64Register,
        rm: Arm64Register,
    },

    // -- Move --
    Mov {
        rd: Arm64Register,
        rm: Arm64Register,
    },
    MovImm {
        rd: Arm64Register,
        imm: i64,
    },
    MovK {
        rd: Arm64Register,
        imm: u16,
        shift: u8,
    },

    // -- Load / Store --
    Ldr {
        rt: Arm64Register,
        rn: Arm64Register,
        offset: i32,
    },
    /// Zero-extending BYTE load. Distinct from [`Self::Ldr`] because the
    /// safepoint flag is a one-byte `AtomicBool` and the 64-bit form would
    /// fold the `GcBarrier` fields after it into the test.
    Ldrb {
        rt: Arm64Register,
        rn: Arm64Register,
        offset: i32,
    },
    Str {
        rt: Arm64Register,
        rn: Arm64Register,
        offset: i32,
    },
    Ldp {
        rt1: Arm64Register,
        rt2: Arm64Register,
        rn: Arm64Register,
        offset: i32,
    },
    Stp {
        rt1: Arm64Register,
        rt2: Arm64Register,
        rn: Arm64Register,
        offset: i32,
    },
    /// Pre-index STP with writeback: `STP rt1, rt2, [rn, #offset]!`.
    ///
    /// Bug-fix (ARM64 BUG #2): the AAPCS64-idiomatic prologue store. The base
    /// register `rn` is updated to `rn + offset` as part of the instruction,
    /// which both saves the pair AND allocates the (first 16 bytes of the)
    /// stack frame atomically. `offset` uses the small fixed −16, always inside
    /// the imm7 range, so it is robust for arbitrarily large frames (unlike a
    /// signed-offset STP at `frame_size-16`).
    StpPre {
        rt1: Arm64Register,
        rt2: Arm64Register,
        rn: Arm64Register,
        offset: i32,
    },
    /// Post-index LDP with writeback: `LDP rt1, rt2, [rn], #offset`.
    ///
    /// Bug-fix (ARM64 BUG #2): the matching epilogue restore. Loads the pair
    /// from `[rn]` then updates `rn = rn + offset`, mirroring [`StpPre`].
    LdpPost {
        rt1: Arm64Register,
        rt2: Arm64Register,
        rn: Arm64Register,
        offset: i32,
    },
    LdrLiteral {
        rt: Arm64Register,
        label: u32,
    },

    // -- Branch --
    B {
        label: u32,
    },
    BCond {
        cond: Arm64Condition,
        label: u32,
    },
    Bl {
        label: u32,
    },
    Br {
        rn: Arm64Register,
    },
    Blr {
        rn: Arm64Register,
    },
    Ret,
    Cbz {
        rt: Arm64Register,
        label: u32,
    },
    Cbnz {
        rt: Arm64Register,
        label: u32,
    },

    // -- Conversion --
    ScvtfDouble {
        vd: Arm64Register,
        rn: Arm64Register,
    },
    FcvtzsInt {
        rd: Arm64Register,
        vn: Arm64Register,
    },

    // -- FP move (bit-pattern transfer between GP and FP registers) --
    /// FMOV Vd, Xn — move GP register to FP register (bit-pattern, no conversion).
    FmovToFp {
        vd: Arm64Register,
        rn: Arm64Register,
    },
    /// FMOV Xd, Vn — move FP register to GP register (bit-pattern, no conversion).
    FmovFromFp {
        rd: Arm64Register,
        vn: Arm64Register,
    },
    /// FMOV Vd, Vn — move between FP registers.
    FmovFp {
        vd: Arm64Register,
        vn: Arm64Register,
    },

    // -- FP negate --
    FnegDouble {
        vd: Arm64Register,
        vn: Arm64Register,
    },

    // -- FP single-precision ops --
    FaddSingle {
        vd: Arm64Register,
        vn: Arm64Register,
        vm: Arm64Register,
    },
    FsubSingle {
        vd: Arm64Register,
        vn: Arm64Register,
        vm: Arm64Register,
    },
    FmulSingle {
        vd: Arm64Register,
        vn: Arm64Register,
        vm: Arm64Register,
    },
    FdivSingle {
        vd: Arm64Register,
        vn: Arm64Register,
        vm: Arm64Register,
    },
    FcmpSingle {
        vn: Arm64Register,
        vm: Arm64Register,
    },
    FnegSingle {
        vd: Arm64Register,
        vn: Arm64Register,
    },

    /// SCVTF Sd, Wn — convert 32-bit int to single-precision float.
    ScvtfSingle {
        vd: Arm64Register,
        rn: Arm64Register,
    },
    /// FCVTZS Wd, Sn — convert single-precision float to 32-bit int (truncate toward zero).
    FcvtzsSingle {
        rd: Arm64Register,
        vn: Arm64Register,
    },
    /// FCVT Dd, Sn — convert single to double.
    FcvtSingleToDouble {
        vd: Arm64Register,
        vn: Arm64Register,
    },
    /// FCVT Sd, Dn — convert double to single.
    FcvtDoubleToSingle {
        vd: Arm64Register,
        vn: Arm64Register,
    },

    /// LDR (FP) — load from [base + offset] to FP register.
    FpLdr {
        vt: Arm64Register,
        rn: Arm64Register,
        offset: i32,
        is_double: bool,
    },
    /// STR (FP) — store FP register to [base + offset].
    FpStr {
        vt: Arm64Register,
        rn: Arm64Register,
        offset: i32,
        is_double: bool,
    },

    // -- NEON SIMD (double precision) --
    FaddDouble {
        vd: Arm64Register,
        vn: Arm64Register,
        vm: Arm64Register,
    },
    FsubDouble {
        vd: Arm64Register,
        vn: Arm64Register,
        vm: Arm64Register,
    },
    FmulDouble {
        vd: Arm64Register,
        vn: Arm64Register,
        vm: Arm64Register,
    },
    FdivDouble {
        vd: Arm64Register,
        vn: Arm64Register,
        vm: Arm64Register,
    },
    FcmpDouble {
        vn: Arm64Register,
        vm: Arm64Register,
    },

    // -- NEON SIMD (integer vector, 4x32) --
    /// LD1 {Vt.4S}, [Xn] — load 128-bit vector from memory.
    NeonLd1_4s {
        vt: Arm64Register,
        rn: Arm64Register,
    },
    /// ST1 {Vt.4S}, [Xn] — store 128-bit vector to memory.
    NeonSt1_4s {
        vt: Arm64Register,
        rn: Arm64Register,
    },
    /// ADD Vd.4S, Vn.4S, Vm.4S — vector integer add (4x i32).
    NeonAdd4s {
        vd: Arm64Register,
        vn: Arm64Register,
        vm: Arm64Register,
    },
    /// MUL Vd.4S, Vn.4S, Vm.4S — vector integer multiply (4x i32).
    NeonMul4s {
        vd: Arm64Register,
        vn: Arm64Register,
        vm: Arm64Register,
    },

    // -- 32-bit (W) integer forms --
    //
    // JVM `int` arithmetic. A W-form result is the JVMS 32-bit wrapped value;
    // the backend follows each producer with `Sxtw` to restore the
    // sign-extended form it keeps every `int` in.
    AddW {
        rd: Arm64Register,
        rn: Arm64Register,
        rm: Arm64Register,
    },
    SubW {
        rd: Arm64Register,
        rn: Arm64Register,
        rm: Arm64Register,
    },
    MulW {
        rd: Arm64Register,
        rn: Arm64Register,
        rm: Arm64Register,
    },
    AndW {
        rd: Arm64Register,
        rn: Arm64Register,
        rm: Arm64Register,
    },
    OrrW {
        rd: Arm64Register,
        rn: Arm64Register,
        rm: Arm64Register,
    },
    EorW {
        rd: Arm64Register,
        rn: Arm64Register,
        rm: Arm64Register,
    },
    /// `LSLV Wd, Wn, Wm`: the amount is taken MOD 32, i.e. `ishl`'s `& 0x1f`.
    LslW {
        rd: Arm64Register,
        rn: Arm64Register,
        rm: Arm64Register,
    },
    /// `LSRV Wd, Wn, Wm`, amount MOD 32.
    LsrW {
        rd: Arm64Register,
        rn: Arm64Register,
        rm: Arm64Register,
    },
    /// `ASRV Wd, Wn, Wm`, amount MOD 32.
    AsrW {
        rd: Arm64Register,
        rn: Arm64Register,
        rm: Arm64Register,
    },
    NegW {
        rd: Arm64Register,
        rn: Arm64Register,
    },
    /// `SDIV Wd, Wn, Wm` (`idiv`, round 9 wave 11). `INT_MIN / -1` is
    /// `INT_MIN`, as JVMS §6.5 `idiv` requires: AArch64 division never traps,
    /// and the lowering branches to the `ArithmeticException` stub on a zero
    /// divisor BEFORE this runs (a zero divisor would otherwise yield 0).
    SDivW {
        rd: Arm64Register,
        rn: Arm64Register,
        rm: Arm64Register,
    },
    /// `MSUB Wd, Wn, Wm, Wa` = `Wa - Wn*Wm` (`irem`'s `a - (a/b)*b`).
    MsubW {
        rd: Arm64Register,
        rn: Arm64Register,
        rm: Arm64Register,
        ra: Arm64Register,
    },
    AddImmW {
        rd: Arm64Register,
        rn: Arm64Register,
        imm: i32,
    },
    SubImmW {
        rd: Arm64Register,
        rn: Arm64Register,
        imm: i32,
    },
    CmpW {
        rn: Arm64Register,
        rm: Arm64Register,
    },
    CmpImmW {
        rn: Arm64Register,
        imm: i32,
    },
    CbzW {
        rt: Arm64Register,
        label: u32,
    },
    CbnzW {
        rt: Arm64Register,
        label: u32,
    },
    /// `SXTW Xd, Wn`.
    Sxtw {
        rd: Arm64Register,
        rn: Arm64Register,
    },
    /// `SXTH Xd, Wn`.
    Sxth {
        rd: Arm64Register,
        rn: Arm64Register,
    },
    /// `SXTB Xd, Wn`.
    Sxtb {
        rd: Arm64Register,
        rn: Arm64Register,
    },
    /// `AND Xd, Xn, #imm`, as a bitmask immediate when encodable and through
    /// IP0 otherwise.
    AndImm {
        rd: Arm64Register,
        rn: Arm64Register,
        imm: u64,
    },
    /// `CSET Xd, cond`: 1 when `cond` holds, else 0.
    Cset {
        rd: Arm64Register,
        cond: Arm64Condition,
    },
    /// `CNEG Xd, Xn, cond`: `-Xn` when `cond` holds, else `Xn`.
    Cneg {
        rd: Arm64Register,
        rn: Arm64Register,
        cond: Arm64Condition,
    },

    // -- FP width forms --
    /// `FMOV Sd, Wn` (bit pattern, no conversion).
    FmovToFpSingle {
        vd: Arm64Register,
        rn: Arm64Register,
    },
    /// `FMOV Wd, Sn` (bit pattern; zero-extends into Xd).
    FmovFromFpSingle {
        rd: Arm64Register,
        vn: Arm64Register,
    },
    /// `FMOV Sd, Sn`.
    FmovFpSingle {
        vd: Arm64Register,
        vn: Arm64Register,
    },
    /// `SCVTF Dd, Wn` (`i2d`).
    ScvtfDoubleW {
        vd: Arm64Register,
        rn: Arm64Register,
    },
    /// `SCVTF Sd, Xn` (`l2f`).
    ScvtfSingleX {
        vd: Arm64Register,
        rn: Arm64Register,
    },
    /// `FCVTZS Wd, Dn` (`d2i`, saturating at the 32-bit bounds).
    FcvtzsIntW {
        rd: Arm64Register,
        vn: Arm64Register,
    },
    /// `FCVTZS Xd, Sn` (`f2l`).
    FcvtzsSingleX {
        rd: Arm64Register,
        vn: Arm64Register,
    },

    // -- Switches --
    /// `tableswitch`: `key - low` into IP1 (X17), an unsigned bounds check to
    /// `default`, then a PC-relative jump table (`ADR X16; LDRSW X17, [X16,
    /// W17, UXTW #2]; ADD X16, X16, X17; BR X16`) followed by one 32-bit
    /// offset per case. X16/X17 are never allocator-managed, so no case can
    /// land on the key's own register -- which the old compare chain, whose
    /// constants came from the scratch allocator, could (`CMP R, R`).
    TableSwitch {
        key: Arm64Register,
        low: i32,
        default: u32,
        targets: Vec<u32>,
    },
    /// `lookupswitch`: `CMP Wkey, #value` (constants too wide for an
    /// immediate go through IP0, never a scratch register), `B.EQ` per pair,
    /// then `B default`.
    LookupSwitch {
        key: Arm64Register,
        pairs: Vec<(i32, u32)>,
        default: u32,
    },

    // -- Shared-memory access (round 9 wave 10) --
    //
    // The heap/statics accesses, as distinct from the frame-slot `Ldr`/`Str`
    // above: the address is exactly `[rn]` (the ordered forms take no offset,
    // so every lowering computes the full address first) and the width and
    // the ordering are explicit, so a reviewer can see from the pseudo-op
    // alone whether a `volatile` access got its acquire/release form.
    /// `LDR{B,H} Wt` / `LDR Wt` / `LDR Xt`, or with `acquire` the matching
    /// `LDAR{B,H}` / `LDAR Wt` / `LDAR Xt`, from `[rn]`. Every form
    /// ZERO-extends into `rt`; a signed or FP result is the lowering's
    /// follow-up instruction's job (`Sxtw`, `FmovToFp*`).
    MemLoad {
        rt: Arm64Register,
        rn: Arm64Register,
        width: Arm64MemWidth,
        acquire: bool,
    },
    /// `STR{B,H} Wt` / `STR Wt` / `STR Xt`, or with `release` the matching
    /// `STLR{B,H}` / `STLR Wt` / `STLR Xt`, to `[rn]`. A `volatile` store owes
    /// a StoreLoad edge on top of the release; see [`Self::DmbIsh`].
    MemStore {
        rt: Arm64Register,
        rn: Arm64Register,
        width: Arm64MemWidth,
        release: bool,
    },
    /// `DMB ISH`: the full inner-shareable barrier (the aarch64 counterpart of
    /// x64's `MFENCE` after a `volatile` store).
    DmbIsh,

    // -- System --
    Nop,
    Brk {
        imm: u16,
    },

    // -- Pseudo-instructions --
    Label(u32),
    Comment(String),
    /// Raw 64-bit constant data embedded in the instruction stream (literal pool).
    /// The label is bound to the position of this data so that LdrLiteral can
    /// reference it.
    ConstantPoolEntry {
        label: u32,
        value: u64,
    },
}

impl Arm64Instruction {
    /// An upper bound on the bytes this pseudo-op encodes to.
    ///
    /// `Label` and `Comment` emit nothing, a literal emits 8 bytes, immediates
    /// and far frame accesses expand into a materialization through IP0, and
    /// a switch carries its own table. See `emit_machine_code_inner`.
    pub fn max_encoded_bytes(&self) -> usize {
        match self {
            Arm64Instruction::Label(_) | Arm64Instruction::Comment(_) => 0,
            Arm64Instruction::ConstantPoolEntry { .. } => 8,
            Arm64Instruction::MovImm { .. } => 16,
            Arm64Instruction::AddImm { .. }
            | Arm64Instruction::SubImm { .. }
            | Arm64Instruction::AddImmW { .. }
            | Arm64Instruction::SubImmW { .. }
            | Arm64Instruction::CmpImm { .. }
            | Arm64Instruction::CmpImmW { .. }
            | Arm64Instruction::AndImm { .. } => 20,
            Arm64Instruction::Ldr { .. }
            | Arm64Instruction::Str { .. }
            | Arm64Instruction::FpLdr { .. }
            | Arm64Instruction::FpStr { .. } => 24,
            // SUB + CMP (each up to 20 through IP0), B.HI, ADR, LDRSW, ADD, BR,
            // and a word per case.
            Arm64Instruction::TableSwitch { targets, .. } => 60 + 4 * targets.len(),
            // A CMP (up to 20) and a B.EQ per pair, then B.
            Arm64Instruction::LookupSwitch { pairs, .. } => 24 * pairs.len() + 4,
            _ => 4,
        }
    }
}

// ---------------------------------------------------------------------------
// Arm64EntryConvention
// ---------------------------------------------------------------------------

/// The convention a compiled method is ENTERED with on aarch64 — which is
/// AAPCS64 restricted to integer registers, and not AAPCS64 (finding A9).
///
/// It was called `Arm64CallingConvention` and documented as "AAPCS64 calling
/// convention constants and helpers", and it is not that. AAPCS64 §6.4 assigns
/// the first eight floating-point and SIMD arguments to `V0`–`V7`; this type
/// names no FP argument registers at all, and `emit_argument_homing` puts
/// argument `i` in `INT_ARG_REGS[i]` whatever its Java type — a `float` arrives
/// as its IEEE-754 single bit pattern in an X register and a `double` as its
/// bits. Anything written against the old name and the old doc would have
/// passed a `double` in `D0` and this prologue would never have read it.
///
/// That is sound because nothing outside this VM calls a compiled method: the
/// interpreter's entry trampoline is the only producer of these arguments and
/// it marshals every one of them into an integer slot. It is a PRIVATE
/// convention that happens to agree with AAPCS64 wherever it says anything.
///
/// # What here really is AAPCS64
///
/// * [`Self::CALLEE_SAVED`] — X19–X28, and `STACK_ALIGNMENT` = 16, because the
///   compiled frame must be unwindable and re-entrant from ordinary C code (the
///   safepoint slow path is a `BLR` into the runtime).
/// * [`Self::LINK_REG`] / [`Self::FRAME_POINTER`] — X30 / X29, and the frame
///   record `Arm64FrameLayout` builds around them.
/// * [`Self::RED_ZONE`] = 0: AAPCS64 defines none, unlike System V on x86-64.
///
/// # What is NOT
///
/// * no FP/SIMD argument registers, as above;
/// * no stack argument area — `emit_argument_homing` refuses a ninth argument
///   outright rather than reading it from the caller's frame;
/// * D8–D15 are callee-saved under AAPCS64 and this prologue does not save
///   them, which is exactly why `regalloc::ARM64_LOCAL_FPS` must not be used to
///   home a float local here (see the note at the FP-local seeding loop).
///
/// Asserted by `the_entry_convention_passes_every_argument_in_an_x_register`.
pub struct Arm64EntryConvention;

impl Arm64EntryConvention {
    /// Argument registers X0-X7 — for EVERY argument, not only integers. See
    /// the type's doc: a `float`/`double` argument arrives here as its bit
    /// pattern, because this VM's entry trampoline marshals it that way.
    ///
    /// The name is kept for its call sites and because the registers really are
    /// AAPCS64's integer argument registers; what is not AAPCS64 is that they
    /// are the only ones.
    pub const INT_ARG_REGS: &'static [Arm64Register] = &[
        Arm64Register::X0,
        Arm64Register::X1,
        Arm64Register::X2,
        Arm64Register::X3,
        Arm64Register::X4,
        Arm64Register::X5,
        Arm64Register::X6,
        Arm64Register::X7,
    ];

    /// Callee-saved registers: X19-X28.
    pub const CALLEE_SAVED: &'static [Arm64Register] = &[
        Arm64Register::X19,
        Arm64Register::X20,
        Arm64Register::X21,
        Arm64Register::X22,
        Arm64Register::X23,
        Arm64Register::X24,
        Arm64Register::X25,
        Arm64Register::X26,
        Arm64Register::X27,
        Arm64Register::X28,
    ];

    pub const RETURN_REG: Arm64Register = Arm64Register::X0;
    pub const FRAME_POINTER: Arm64Register = Arm64Register::FP;
    pub const LINK_REG: Arm64Register = Arm64Register::LR;
    pub const STACK_POINTER: Arm64Register = Arm64Register::SP;

    /// Stack alignment requirement (16 bytes on ARM64).
    pub const STACK_ALIGNMENT: usize = 16;

    /// Red-zone size (ARM64 AAPCS64 does not define a red zone).
    pub const RED_ZONE: usize = 0;

    /// Get the register for the n-th integer argument, if available.
    pub fn int_arg_reg(n: usize) -> Option<Arm64Register> {
        Self::INT_ARG_REGS.get(n).copied()
    }

    /// Map the n-th local variable to a callee-saved register, if available.
    pub fn local_reg(n: usize) -> Option<Arm64Register> {
        Self::CALLEE_SAVED.get(n).copied()
    }
}

// ---------------------------------------------------------------------------
// Arm64FrameLayout
// ---------------------------------------------------------------------------

/// Describes the stack frame geometry for a compiled method.
pub struct Arm64FrameLayout {
    /// Total frame size in bytes (16-byte aligned).
    pub frame_size: i32,
    /// Byte offset from FP where callee-saved registers are stored.
    pub callee_save_offset: i32,
    /// Byte offset from FP where spill slots begin.
    pub spill_offset: i32,
    /// Number of spill slots.
    pub num_spills: usize,
    /// Which callee-saved registers must be preserved.
    pub saved_regs: Vec<Arm64Register>,
    /// How many locals got a dedicated register.
    pub num_reg_locals: usize,
}

impl Arm64FrameLayout {
    /// Compute the frame layout given method metadata.
    ///
    /// ```text
    /// [FP + 8]     saved LR          \  the AAPCS64 frame record, pushed by
    /// [FP]         saved caller FP   /  `STP X29, X30, [SP, #-16]!`
    /// [FP - 8] ..  callee-saved GPRs   (callee_save_offset = -callee_save_bytes)
    /// ..           spill area          (spill_offset = callee_save_offset - spill_bytes)
    /// [SP]         frame bottom        (FP - (frame_size - 16))
    /// ```
    ///
    /// `frame_size` counts the 16-byte record as well, and is 16-byte aligned.
    /// Everything this frame owns is BELOW FP. The record used to sit at
    /// `[FP-16]`/`[FP-8]` with FP equal to the caller's SP, which is not the
    /// frame record any unwinder or frame-pointer walk expects.
    pub fn compute(_num_locals: usize, num_spills: usize, saved_regs: &[Arm64Register]) -> Self {
        let num_reg_locals = saved_regs.len();

        // Callee-saved regs: round count up to even for STP pairing.
        let num_saved = saved_regs.len();
        let callee_save_bytes = ((num_saved + 1) / 2) * 16; // pairs of 8-byte regs

        let spill_bytes = num_spills * 8;

        // Total = frame record (16) + callee-save area + spill area, aligned.
        let raw = 16 + callee_save_bytes + spill_bytes;
        let frame_size = align_up(raw, 16) as i32;

        // Offsets are negative from FP, which points at the frame record.
        let callee_save_offset = -(callee_save_bytes as i32);
        let spill_offset = callee_save_offset - spill_bytes as i32;

        Self {
            frame_size,
            callee_save_offset,
            spill_offset,
            num_spills,
            saved_regs: saved_regs.to_vec(),
            num_reg_locals: num_reg_locals,
        }
    }
}

/// Round `value` up to the next multiple of `align`.
pub(in crate::aarch64_backend) fn align_up(value: usize, align: usize) -> usize {
    (value + align - 1) & !(align - 1)
}

// ---------------------------------------------------------------------------
// Arm64SpillArea
// ---------------------------------------------------------------------------

/// How the spill area below [`Arm64FrameLayout::spill_offset`] is partitioned,
/// named once instead of four times (finding A11).
///
/// The area holds four regions, laid end to end from `spill_offset` upwards:
///
/// ```text
/// word 0                       .. locals                  frame-homed locals
/// locals                       .. locals+operands          operand stack
/// locals+operands              .. +safepoint_homes         safepoint homes
/// locals+operands+homes        .. +sp_id                   the safepoint-id word
/// ...+sp_id                    .. +context                 the VM context word
/// ```
///
/// # Why this is a type and not four additions
///
/// Before it was, each of the four accessors re-derived its own base by adding
/// up the sizes of the regions before it —
/// `local_spill_count() + max_stack + k` in `safepoint_home_for_reg_local`,
/// `local_spill_count() + depth` in `spill_offset_for_depth`,
/// `num_spills - 1` for the id word — and the total was restated a fifth time
/// where the frame was sized. Five expressions that must agree, with nothing
/// checking that they do, and every one of them bounds-checked only against the
/// GRAND total (`slot >= frame.num_spills`). An accessor that took the wrong
/// base therefore did not refuse: it read or wrote a word belonging to a
/// different region, which for the operand-versus-locals pair means a local
/// silently aliasing an operand.
///
/// The region bases are computed once here, each accessor is bounds-checked
/// against ITS OWN region, and `total()` is derived rather than restated.
///
/// # Why the counts are captured, not re-read
///
/// `local_spill_count()` walks `local_regs` on every call. The frame is sized
/// from one reading of it and then indexed against later readings; they agree
/// today only because nothing edits `local_regs` after the prologue. Capturing
/// the four counts at the moment the frame is sized makes that an invariant of
/// the type rather than an ordering property of the compile.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::aarch64_backend) struct Arm64SpillArea {
    /// Words held by locals with no register home.
    pub(in crate::aarch64_backend) locals: usize,
    /// Words held by the operand stack — `max_stack`, one word per JVM slot.
    pub(in crate::aarch64_backend) operands: usize,
    /// One word per register-homed local, so a safepoint can publish it to the
    /// collector. Zero when this compilation does not poll.
    pub(in crate::aarch64_backend) safepoint_homes: usize,
    /// The word each poll stamps its safepoint id into: 1, or 0 when this
    /// compilation does not poll.
    pub(in crate::aarch64_backend) sp_id: usize,
    /// The word the prologue homes the VM context pointer into: 1 when this
    /// compilation needs the context ABI (round 9 wave 16), else 0.
    ///
    /// LAST, so that adding it moved no existing word. Everything below it is
    /// addressed by an index this type computes, so the position is not load-
    /// bearing -- but a reader comparing two frame dumps across the change
    /// should not have to check that.
    pub(in crate::aarch64_backend) context: usize,
}

impl Arm64SpillArea {
    /// Total words, which is what [`Arm64FrameLayout::compute`] is sized from.
    pub(in crate::aarch64_backend) fn total(&self) -> usize {
        self.locals + self.operands + self.safepoint_homes + self.sp_id + self.context
    }

    /// Word index of frame-homed local slot `i`, or `None` when it is outside
    /// the locals region.
    pub(in crate::aarch64_backend) fn local_word(&self, i: usize) -> Option<usize> {
        (i < self.locals).then_some(i)
    }

    /// Word index of operand-stack depth `depth`, or `None` when it is outside
    /// the operand region.
    ///
    /// Bounded by `operands` and not by the total: a depth at or past
    /// `max_stack` would otherwise land on a safepoint home.
    pub(in crate::aarch64_backend) fn operand_word(&self, depth: usize) -> Option<usize> {
        (depth < self.operands).then_some(self.locals + depth)
    }

    /// Word index of the safepoint home for the `k`-th register-homed local,
    /// or `None` when it is outside the home region.
    pub(in crate::aarch64_backend) fn safepoint_home_word(&self, k: usize) -> Option<usize> {
        (k < self.safepoint_homes).then_some(self.locals + self.operands + k)
    }

    /// Word index of the safepoint-id word, or `None` when none is reserved.
    pub(in crate::aarch64_backend) fn sp_id_word(&self) -> Option<usize> {
        (self.sp_id != 0).then_some(self.locals + self.operands + self.safepoint_homes)
    }

    /// Word index of the VM context word, or `None` when this compilation does
    /// not take a context.
    pub(in crate::aarch64_backend) fn context_word(&self) -> Option<usize> {
        (self.context != 0)
            .then_some(self.locals + self.operands + self.safepoint_homes + self.sp_id)
    }
}

// ---------------------------------------------------------------------------
// Arm64CodeBuffer
// ---------------------------------------------------------------------------

/// Accumulates `Arm64Instruction`s and manages labels.
pub struct Arm64CodeBuffer {
    pub(in crate::aarch64_backend) instructions: Vec<Arm64Instruction>,
    pub(in crate::aarch64_backend) labels: HashMap<u32, usize>,
    pub(in crate::aarch64_backend) next_label: u32,
}

impl Arm64CodeBuffer {
    pub fn new() -> Self {
        Self {
            instructions: Vec::new(),
            labels: HashMap::new(),
            next_label: 0,
        }
    }

    /// Append an instruction.
    pub fn emit(&mut self, inst: Arm64Instruction) {
        self.instructions.push(inst);
    }

    /// Allocate a fresh label id.
    pub fn new_label(&mut self) -> u32 {
        let id = self.next_label;
        self.next_label += 1;
        id
    }

    /// Bind `label` to the current instruction index and emit a `Label` pseudo-op.
    pub fn bind_label(&mut self, label: u32) {
        self.labels.insert(label, self.instructions.len());
        self.instructions.push(Arm64Instruction::Label(label));
    }

    pub fn instruction_count(&self) -> usize {
        self.instructions.len()
    }

    pub fn instructions(&self) -> &[Arm64Instruction] {
        &self.instructions
    }

    /// An UPPER BOUND on the encoded size, in bytes.
    ///
    /// Not `len * 4`: this is a pseudo-op stream, in which a `Label` or
    /// `Comment` encodes to nothing and a `MovImm`, a wide immediate, a far
    /// frame access or a switch expands to several words. See
    /// [`Arm64Instruction::max_encoded_bytes`].
    pub fn estimated_size(&self) -> usize {
        self.instructions
            .iter()
            .map(Arm64Instruction::max_encoded_bytes)
            .sum()
    }
}

// ---------------------------------------------------------------------------
// Arm64CompileResult
// ---------------------------------------------------------------------------

/// Is the aarch64 JIT on at all (`CRATONVM_JIT_ARM64`, **default-OFF**)?
///
/// `docs/PLATFORMS.md` says the JIT is disabled off x86-64, and until this
/// switch existed that was not true on aarch64: `try_compile_inner` compiled
/// with this backend unconditionally, publishing machine code that no host in
/// this repository had executed. It stays opt-in until the backend has run on
/// hardware. Read by the `#[cfg(target_arch = "aarch64")]` block of
/// `try_compile_inner`, and defined here without a `cfg` so every host
/// type-checks it.
///
/// Read once per compile, no `static` (j27 LATCHES). The strict on-word set is
/// kept: `runtime_flag_on` would also turn an unknown word on.
pub fn arm64_jit_enabled() -> bool {
    matches!(
        None::<String>
            .as_deref()
            .map(str::trim)
            .map(str::to_ascii_lowercase)
            .as_deref(),
        Some("1") | Some("true") | Some("on") | Some("yes")
    )
}

/// Does the aarch64 backend emit GC safepoint polls
/// (`CRATONVM_JIT_ARM64_SAFEPOINTS`, **default-OFF; opt-in**)?
///
/// # Why this one is opt-in when every sibling switch is default-on
///
/// It emits MACHINE CODE FOR AN ARCHITECTURE NO CI RUNNER OR DEVELOPER HOST
/// HERE CAN EXECUTE. Every other codegen switch in this workspace ships
/// default-on with a kill switch because a regression run can execute it and
/// say so; this one cannot be run at all until someone builds on an aarch64
/// host. The encodings below are asserted against known-good instruction words
/// and the structure is asserted against the emitted pseudo-op stream, which is
/// everything a non-aarch64 host can honestly prove -- and it is not the same
/// as "it works". Default-on would be publishing an unexecuted calling
/// sequence into a GC's stop-the-world protocol.
///
/// Turning it on does two things: a poll at method entry and at every loop
/// header, and -- because that is what the refusal was FOR -- it lifts
/// `label_for_pc`'s blanket refusal of backward branches, so loops compile
/// again. With it off, this backend is byte-identical to before.
pub(crate) fn arm64_safepoints_enabled() -> bool {
    use std::sync::OnceLock;
    static G: OnceLock<bool> = OnceLock::new();
    *G.get_or_init(|| {
        matches!(
            None::<String>
                .as_deref()
                .map(str::trim)
                .map(str::to_ascii_lowercase)
                .as_deref(),
            Some("1") | Some("true") | Some("on") | Some("yes")
        )
    })
}

/// A safepoint's oop map as the COMPILER can know it: the frame slots are
/// final, but the PC is a PSEUDO-OP INDEX, not a byte offset.
///
/// The two cannot be the same value on this backend and that is the whole
/// reason this type exists. `Arm64Instruction` is a pseudo-op stream, not a
/// fixed-width one: `Label` and `Comment` emit nothing, `ConstantPoolEntry`
/// emits 8 bytes, and `MovImm` / `AddImm` / `CmpImm` / out-of-range `Ldr`/`Str`
/// expand to one to four words (`mov_imm64`, `emit_addsub_imm_safe`,
/// `emit_addr_into_ip0`). So the `instruction_count * 4` the writer used to
/// record was wrong for any method containing one of those, and a map keyed by
/// a wrong PC is worse than no map -- the GC reads the WRONG FRAME SLOTS at a
/// real safepoint and either misses a live reference or rewrites a primitive.
///
/// Keeping the unresolved form in its own type means an unresolved PC cannot be
/// mistaken for a resolved one by a later reader: there is no `OopMapEntry`
/// anywhere until [`emit_machine_code_with_oop_maps`] has run the encoder and
/// can say what the byte offset actually is.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Arm64PendingOopMap {
    /// Index into [`Arm64CompileResult::instructions`] of the pseudo-op that
    /// FOLLOWS the safepoint -- the same "return address" convention the x64
    /// backend uses for `OopMapEntry::native_pc_offset`.
    pub pseudo_index: u32,
    /// Frame-slot offsets (relative to FP) holding object references here.
    pub frame_slot_offsets: Vec<i16>,
    /// The safepoint id this map belongs to -- the value the frame's sp-id slot
    /// holds while this safepoint is the active one. Becomes
    /// `OopMapEntry::bytecode_pc`, which is what
    /// `find_oop_map_for_safepoint_id` matches on.
    pub safepoint_id: u32,
}

/// Output of the compilation pipeline.
pub struct Arm64CompileResult {
    pub instructions: Vec<Arm64Instruction>,
    pub frame: Arm64FrameLayout,
    pub labels: HashMap<u32, usize>,
    pub success: bool,
    /// T1.1.3 — this compilation's safepoint oop maps, PC-UNRESOLVED.
    ///
    /// Each entry records the frame-slot offsets (relative to FP on AArch64;
    /// x86-64 uses RBP) that hold object references at a GC-capable safepoint,
    /// keyed by pseudo-op index. [`emit_machine_code_with_oop_maps`] turns
    /// these into `crate::OopMapEntry` values keyed by real byte offsets --
    /// see [`Arm64PendingOopMap`] for why the compiler cannot do that itself.
    ///
    /// Empty unless `CRATONVM_JIT_ARM64_SAFEPOINTS` is on: the safepoint polls
    /// are this backend's only GC-capable points (it lowers no allocation, call
    /// or monitor), and each poll records exactly one map.
    pub pending_oop_maps: Vec<Arm64PendingOopMap>,
    /// Frame offset (positive) of the safepoint-id slot, or 0 when none was
    /// reserved. Published onto `CompiledMethod::sp_id_slot_off`, which the
    /// runtime reads as `[frame_base - off]`.
    pub sp_id_slot_off: i32,
    /// How many safepoints this compilation published a map for, and how many
    /// of those could not describe everything live at their site. The terms
    /// `fully_oop_covered` is computed from -- see `publish_compiled_method`.
    pub safepoint_count: usize,
    pub incomplete_oop_maps: usize,
    /// Declaring class ids of the `getstatic` sites this body reads directly
    /// (round 9 wave 10), sorted and deduplicated. Published as
    /// `CompiledMethod::static_init_classes`, the ensure-initialized
    /// obligation the x64 tier records for the same reason.
    pub static_init_classes: Vec<u32>,
    /// Whether this body takes the VM context pointer as an extra LEADING
    /// argument (round 9 wave 16). Published by
    /// [`publish_compiled_method`] as `CompiledMethod::needs_context`, which
    /// is what makes the VM call the context entry.
    ///
    /// It is not an extra argument the body may ignore: the context OCCUPIES
    /// X0, so a body compiled without one and called with one reads its first
    /// Java argument from the context pointer. The flag and the prologue must
    /// agree, which is why both come from the same field.
    pub needs_context: bool,
    /// Whether this compilation emitted safepoint POLLS (round 9 wave 18).
    ///
    /// It NO LONGER gates `fully_oop_covered` -- round 9 wave 21 widened that
    /// claim to an allocation-only method, see [`publish_compiled_method`].
    /// Kept because it is the one bit that distinguishes the two ways a frame
    /// here becomes safepoint-shaped, and a test that means to exercise the
    /// allocation route has to be able to say which one it got.
    pub polls_enabled: bool,
    /// Whether the prologue published this frame's BASE, through
    /// `helpers.frame_record` (round 9 wave 21).
    ///
    /// The fifth term of `fully_oop_covered`, and the one whose absence is
    /// silent. Everything precise this backend emits is addressed from the
    /// frame base: the runtime reads the safepoint id at
    /// `[frame_base - sp_id_slot_off]` and each map's slots at
    /// `[frame_base - offset]`, and it learns `frame_base` only from
    /// [`Arm64Backend::emit_frame_record`]. A compilation that emits maps but
    /// no frame record leaves `PreciseFrameInfo::exact_rbp` at `0`, which
    /// every precise path declines -- so its maps exist and cannot be read,
    /// and a coverage claim over them is a claim about nothing.
    ///
    /// It is a separate term rather than an assertion because the emitter can
    /// decline for a reason that is not this compilation's fault:
    /// `helpers.frame_record` is `0` when the VM was started with
    /// `CRATONVM_NO_PRECISE_JIT_MAPS=1` and no moving young generation. The
    /// method still compiles and still runs; it just may not claim coverage.
    pub frame_base_published: bool,
}

// ---------------------------------------------------------------------------
// Arm64Backend
// ---------------------------------------------------------------------------

/// The kind of value an operand-stack entry holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperandKind {
    /// JVM `int` (and `boolean`/`byte`/`char`/`short`), held SIGN-EXTENDED in
    /// its 64-bit register. See "The `int` representation" in the module
    /// header.
    I32,
    /// JVM `long`.
    I64,
    /// An object reference.
    Ref,
    /// JVM `float`: an S register, or the low four bytes of a frame word.
    F32,
    /// JVM `double`: a D register, or a whole frame word.
    F64,
}

impl OperandKind {
    /// Lives in V0-V7 rather than X9-X15.
    pub fn is_fp(self) -> bool {
        matches!(self, OperandKind::F32 | OperandKind::F64)
    }

    /// Category 2 (JVMS 2.11.1): one entry here, two JVM stack slots.
    pub fn is_category2(self) -> bool {
        matches!(self, OperandKind::I64 | OperandKind::F64)
    }
}

/// Where an operand-stack entry is right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperandLoc {
    /// A scratch register: X9-X15 for integer kinds, V0-V7 for FP kinds.
    Reg(Arm64Register),
    /// The frame word reserved for the entry's DEPTH, as an FP-relative
    /// offset (see [`Arm64Backend::spill_offset_for_depth`]).
    Slot(i32),
}

/// One simulated operand-stack entry: what it is, and where it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Operand {
    pub kind: OperandKind,
    pub loc: OperandLoc,
    /// Holds an object reference the GC must see. Set for every `Ref` entry --
    /// `aconst_null`, `aload*`, `aaload`, `new`/`anewarray`, a
    /// reference-returning `invoke*`, a reference `getfield` and a reference
    /// `getstatic` all produce one -- and carried by the stack shuffles, so
    /// the marks are exact by construction.
    pub oop: bool,
}

impl Operand {
    /// An entry that lives in `reg`.
    pub fn in_reg(kind: OperandKind, reg: Arm64Register) -> Self {
        Self {
            kind,
            loc: OperandLoc::Reg(reg),
            oop: kind == OperandKind::Ref,
        }
    }
}

/// Kinds of the `<x>load`/`<x>store` families, in opcode order: `i l f d a`.
pub(in crate::aarch64_backend) const LOCAL_KINDS: [OperandKind; 5] = [
    OperandKind::I32,
    OperandKind::I64,
    OperandKind::F32,
    OperandKind::F64,
    OperandKind::Ref,
];

/// Conditions of `ifeq..ifle` and `if_icmpeq..if_icmple`, in opcode order.
pub(in crate::aarch64_backend) const IF_CONDS: [Arm64Condition; 6] = [
    Arm64Condition::Eq,
    Arm64Condition::Ne,
    Arm64Condition::Lt,
    Arm64Condition::Ge,
    Arm64Condition::Gt,
    Arm64Condition::Le,
];

/// The bytecode byte at `at`, or `None` past the end.
pub(in crate::aarch64_backend) fn bc_u8(code: &[u8], at: usize) -> Option<u8> {
    code.get(at).copied()
}

/// The big-endian `i16` at `at`, or `None` if it runs past the end.
pub(in crate::aarch64_backend) fn bc_i16(code: &[u8], at: usize) -> Option<i16> {
    Some(i16::from_be_bytes([*code.get(at)?, *code.get(at + 1)?]))
}

/// The big-endian `i32` at `at`, or `None` if it runs past the end.
pub(in crate::aarch64_backend) fn bc_i32(code: &[u8], at: usize) -> Option<i32> {
    let b = code.get(at..at.checked_add(4)?)?;
    Some(i32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

/// The bytecode pc a branch at `start_pc` with displacement `offset` targets.
///
/// Cast: a target before pc 0 wraps to a huge pc. No label is ever bound
/// there, so `emit_machine_code` refuses the method -- the same outcome as any
/// other branch into the middle of nowhere.
pub(in crate::aarch64_backend) fn branch_target(start_pc: usize, offset: i32) -> usize {
    (start_pc as i64 + i64::from(offset)) as usize
}

// ── Shared memory and the ordering instructions this backend does not have ──

/// Does this backend's PRODUCTION code emit the acquire/release instructions
/// a `volatile` access lowers to?
///
/// **`true` since round 9 wave 10**, and like its sibling below this is a
/// statement about the code rather than a switch: the
/// [`Arm64Instruction::MemLoad`] / [`Arm64Instruction::MemStore`] encoding
/// arms call `aarch64.rs`'s `ldar*` / `stlr*` encoders, and `getstatic`'s
/// lowering reaches them with `acquire: true` for a `volatile` static.
/// [`the_ordering_constants_agree_with_the_code`] checks it against the file.
///
/// # What replaced the single `ARM64_CAN_ORDER_MEMORY` (round 9 wave 14)
///
/// There used to be ONE constant, `ARM64_CAN_ORDER_MEMORY`, and the gate in
/// `compile_pass` read it as a blanket permission: while it was `false` every
/// shared-memory opcode was refused, and flipping it to `true` would un-gate
/// all of them at once. Its own documentation said it could flip only once
/// BOTH the acquire/release and the exclusive-access halves had production
/// call sites -- i.e. once `monitorenter` lowered a lock-word CAS.
///
/// That coupling turned out to be wrong in both directions.
///
/// * It was too WEAK at the top: a single `bool` is exactly the kind of thing
///   a later change flips to unblock itself, and flipping it would have
///   allowed a `getfield` arm with a plain `LDR` -- the precise accident the
///   gate exists to prevent.
/// * It was too STRONG at the bottom: the exclusive half cannot arrive the
///   way it was expected to. Every `monitorenter` javac emits comes from a
///   `synchronized` block, and a `synchronized` block always carries the
///   compiler-generated `any -> monitorexit; athrow` handler, so the method's
///   exception table is NOT empty -- and every direct-throw lowering on this
///   backend required an empty one, because the interpreter's drain threw
///   with an unknown pc. A monitor lowering would therefore have been dead
///   code, and the constant it was supposed to unlock would have stayed
///   `false` forever while the acquire/release half it also governs had been
///   real since wave 10. (h23, 2026-09-22: the exception-table restriction is
///   gone -- `emit_stamp_throw_bci` -- and `monitorenter` IS lowered now. It
///   still does not flip the exclusive constant, because the CAS is the
///   helper's; see [`ARM64_LOWERS_EXCLUSIVE_ACCESS`]. The coupling was wrong
///   on its own terms either way.)
///
/// So the blanket permission is gone. The gate now consults ONLY
/// [`opcode_has_ordered_lowering`], a per-opcode list each entry of which is a
/// reviewed claim about ONE lowering. There is no longer any single edit that
/// opens the gate for an opcode nobody has written an ordered lowering for,
/// which is strictly stronger than what the boolean gave. These two constants
/// remain because the QUESTION they answer is still worth answering -- "which
/// ordering instructions does this backend actually emit?" -- and because
/// `docs/jit/aarch64-parity.md` §2.2's capability table is checked against
/// them.
pub(crate) const ARM64_LOWERS_ACQUIRE_RELEASE: bool = true;

/// Does this backend's PRODUCTION code emit the exclusive-access instructions
/// (`LDAXR`/`STLXR`, or the LSE `CASAL`)?
///
/// **`false`, and for a different reason since h23 (2026-09-22).**
///
/// The encoders exist (round 9 wave 9) and nothing calls them. Until h23 the
/// reason was that `monitorenter` could not be reached at all: a
/// `synchronized` block's javac-generated `any -> monitorexit; athrow`
/// handler makes the method's exception table non-empty, and every trapping
/// lowering here required an empty one. Both halves of that have since
/// changed -- `emit_stamp_throw_bci` removed the exception-table restriction,
/// and `emit_monitor_op` lowers both opcodes.
///
/// It stays `false` because that lowering calls `jit_monitor_enter` /
/// `jit_monitor_exit` and the compare-and-swap happens INSIDE the helper,
/// which is what `ir_lower`'s arm does on x64 too. An inline uncontended
/// thin-lock CAS with the helper as its fallback is the thing that would flip
/// this, and it is an open performance item, not a capability gap.
///
/// `Unsafe`'s atomics are the other caller. They arrive through an `invoke*`,
/// which this backend lowers as of round 9 wave 22 -- but through
/// `jit_invoke_dispatch`, so the exclusive access is the VM's, not this
/// backend's.
pub(crate) const ARM64_LOWERS_EXCLUSIVE_ACCESS: bool = false;

/// Does executing `opcode` read or write memory another thread can observe?
///
/// The single predicate every shared-memory lowering on this backend must
/// consult. It is deliberately an ALLOW-LIST OF THE HAZARD rather than of the
/// safe ops: an opcode nobody has classified is not in it, so the failure
/// direction for a *new* opcode is "allowed through", which is why the gate is
/// paired with the module header's coverage test rather than trusted alone.
///
/// Membership follows the JVM's own notion of the heap, not the narrower one of
/// "an instruction with a memory operand":
///
/// * array loads and stores, and `arraylength` — the array header and its
///   elements live in the shared heap;
/// * every field accessor, static or instance — the `volatile` and `final`
///   cases are the whole reason this predicate exists;
/// * every `invoke*` — a callee may do any of the above, so a barrier the
///   caller owes cannot be deferred past the call;
/// * every allocation — publication of a new object's header and its `final`
///   fields is the JMM's freeze action;
/// * `monitorenter` / `monitorexit` — acquire and release by definition;
/// * `athrow` — it transfers to a handler that may observe anything the
///   throwing frame published;
/// * `checkcast` / `instanceof` — both read the object header and may run a
///   class initialiser.
pub(crate) fn opcode_touches_shared_memory(opcode: u8) -> bool {
    matches!(
        opcode,
        0x2e..=0x35        // iaload .. saload
        | 0x4f..=0x56      // iastore .. sastore
        | 0xb2..=0xba      // getstatic, putstatic, getfield, putfield, invoke*
        | 0xbb..=0xbe      // new, newarray, anewarray, arraylength
        | 0xbf             // athrow
        | 0xc0 | 0xc1      // checkcast, instanceof
        | 0xc2 | 0xc3      // monitorenter, monitorexit
        | 0xc5 // multianewarray
    )
}

/// The shared-memory opcodes whose aarch64 lowering is COMPLETE -- **the whole
/// gate**, since round 9 wave 14 removed the blanket constant that used to sit
/// in front of it (see [`ARM64_LOWERS_ACQUIRE_RELEASE`]).
///
/// Every entry is a reviewed claim about ONE lowering: that it orders every
/// access it makes, and that it refuses -- in its own arm, with a named
/// reason -- every site it cannot settle. Adding an opcode here is the ONLY
/// way to let it past the gate, and doing so without the lowering fails
/// [`every_shared_memory_opcode_without_an_ordered_lowering_is_refused`],
/// which requires each listed opcode to name the refusal its own arm emits
/// when nothing is wired for it.
///
/// * `getstatic` (`0xb2`, round 9 wave 10), for a site the caller resolved
///   into an [`Arm64StaticField`]: a primitive static of an
///   already-initialized class, read as `[[base_cell] + cell]`, with `LDAR`
///   for a `volatile` field (the JMM acquire; ARMv8's `STLR`->`LDAR` is RCsc,
///   so no trailing `DMB` is owed by a READ) and a plain `LDR` otherwise. No
///   null check (a static has no receiver), no class-init check (the resolver
///   answers only for initialized classes), no call, no safepoint. An
///   unresolved site, a reference-typed static or a missing table refuses in
///   the `0xb2` arm.
/// * `arraylength` (`0xbe`, round 9 wave 12): a null check against the
///   per-action NPE stub ([`Arm64Backend::emit_npe_throw_stubs`]) and a plain
///   32-bit load of the header's length word.
/// * the array element accesses (`0x2e..=0x35`, `0x4f..=0x56`, round 9 wave
///   13) other than `aaload`/`aastore`, which have arms but always refuse: a
///   null check, an unsigned bounds check against the length word, a scaled
///   address and one plain access of the element's own width, plus
///   `bastore`'s runtime `boolean[]` mask.
///
/// The plain loads and stores in the last two are the ORDERED lowering, not an
/// omission. An array's length is written once before publication and its
/// elements are ordinary memory the JMM owes nothing about; and every address
/// in both is derived from the array reference, so ARMv8's address-dependency
/// rule (ARM ARM §B2.3.2, "dependency-ordered before") already orders the
/// access after the load that produced the reference. That is the guarantee
/// the JMM freeze action needs, and it is why no `LDAR` and no `DMB ISHLD` is
/// owed -- not because x64 emits none.
///
/// NOT listed, and why. Since wave 13 the reason is the SAME reason for every
/// one of them, which is itself the finding: each needs a helper call in the
/// MIDDLE of a method -- either as the fallback for a guard it cannot settle
/// at compile time, or as the operation itself -- and this backend has no such
/// call path. Every call it does emit (the safepoint poll, the three throw
/// stubs) either happens with an empty operand stack or is on its way OUT of
/// the frame, where nothing has to survive.
///
/// * `putstatic` -- a raw payload store races `set_static_shared`'s `grow_to`,
///   which copies the statics block under the write lock and republishes it,
///   so a compiled store into the old block between the copy and the
///   republish is LOST. x64 keeps the helper for this, and for the SATB
///   pre-barrier a reference store owes.
/// * `getfield`/`putfield` -- the NPE throw path exists now, but an instance
///   field has TWO layouts per OBJECT (the packed compact one and the legacy
///   16-byte tagged `Value` cell), chosen at allocation time, plus a
///   replaceable layout epoch. x64 emits a runtime branch whose every declined
///   path -- a compact receiver at a site with no baked compact offset, a
///   replaced layout, `field_index >= num_slots` -- falls back to
///   `jit_getfield` / `jit_putfield_*`. Without that fallback the lowering
///   would have to be total for every layout, and it cannot be: a compact
///   receiver at a site the resolver gave no compact offset for has no
///   computable address.
/// * the five `invoke*` forms (`0xb6..=0xba`, round 9 wave 22), through
///   `jit_invoke_dispatch`. The ordering claim is the SHAPE of this backend
///   rather than anything about the callee: no lowering here ever leaves a
///   fence owed. A `volatile` field or static emits its `LDAR`/`STLR` and its
///   `DMB ISH`s inline and completes; an array access owes nothing (ARM ARM
///   §B2.3.2); every reference store is inside the helper that runs the
///   barriers. There is no deferred-barrier state a call could outrun,
///   because there is no deferred-barrier state. The callee's own ordering is
///   the callee's, and `jit_invoke_dispatch` opens with
///   `jit_safepoint_flush_satb` -- the round-7 audit's requirement for a
///   helper whose callee may enter the GC barrier.
/// * `monitorenter`/`monitorexit` (`0xc2`/`0xc3`, h23 2026-09-22), through
///   `jit_monitor_enter` / `jit_monitor_exit`. The acquire and the release
///   are the HELPER's, exactly as they are for `ir_lower`'s arm on x64: the
///   lock word is CAS'd inside it, under its own barriers. Same ordering
///   claim as the calls above -- nothing here leaves a fence owed across a
///   `BLR` -- and see [`ARM64_LOWERS_EXCLUSIVE_ACCESS`], which stays `false`
///   because this lowering emits no exclusive-access instruction of its own.
/// * `checkcast`/`instanceof` (`0xc0`/`0xc1`, round 9 wave 23), through
///   `jit_checkcast` / `jit_instanceof_check`. Same ordering claim as the
///   calls above, and for the same reason: nothing here leaves a fence owed.
/// * `multianewarray` (`0xc5`, h23c 2026-09-22), through
///   `multianewarray_n`. Same ordering claim as the calls above -- an
///   allocation publishes a new object's header and its `final` fields, which
///   is the JMM's freeze action, and the helper performs it; nothing here
///   leaves a fence owed across the `BLR`.
pub(crate) fn opcode_has_ordered_lowering(opcode: u8) -> bool {
    matches!(opcode, 0x2e..=0x35 | 0x4f..=0x56 | 0xb2..=0xc3 | 0xc5)
}

/// The main ARM64 compilation pipeline.
///
/// Translates JVM bytecode into `Arm64Instruction` sequences using a
/// simulated operand stack (compile-time stack mapping).
pub struct Arm64Backend {
    pub(in crate::aarch64_backend) buffer: Arm64CodeBuffer,
    pub(in crate::aarch64_backend) frame: Option<Arm64FrameLayout>,
    /// Register assignment for each local variable (GPR); `None` means the
    /// local is frame-homed.
    pub(in crate::aarch64_backend) local_regs: Vec<Option<Arm64Register>>,
    /// FP register assignment for float/double locals. Always `None`: see the
    /// comment on the loop that fills it in `compile_pass`.
    pub(in crate::aarch64_backend) float_local_regs: Vec<Option<Arm64Register>>,
    /// The simulated operand stack, bottom first -- ONE stack for every kind.
    ///
    /// Each entry records where ITS value is: a scratch register, or the frame
    /// word reserved for its depth. This replaces a register-only stack plus a
    /// `register -> spill slot` map, which could not describe two entries that
    /// had used the same register: when the round-robin allocator wrapped onto
    /// a live register it spilled it, recorded `spill_map[R]`, and pushed R
    /// again for the NEW value, so popping the new value reloaded the OLD one.
    /// `a - (b+1+2+3+4)` returned -4 for `(100, 5)`. Floats lived on a second
    /// stack whose allocator had no liveness check and no spill at all.
    pub(in crate::aarch64_backend) operand_stack: Vec<Operand>,
    /// Scratch registers the bytecode being lowered has popped and still reads.
    /// The allocator never hands one out, so a result register cannot overwrite
    /// an operand the same instruction has yet to consume. Cleared per bytecode.
    pub(in crate::aarch64_backend) held: Vec<Arm64Register>,
    /// The operand-stack SHAPE (kind and oop mark per entry) at each branch
    /// target, recorded by the branches. Every path into a target leaves each
    /// entry in its depth slot, so the shape is all a target needs to rebuild
    /// the model. Recorded by pass 1 and read by pass 2, so a backward target
    /// is known before the walk reaches it.
    pub(in crate::aarch64_backend) label_states: HashMap<usize, Vec<(OperandKind, bool)>>,
    /// Whether control falls into the instruction being lowered from the
    /// previous one (false after `goto`, `*return` and the switches).
    pub(in crate::aarch64_backend) reachable: bool,
    /// Per-bci operand-stack kinds from the shared analysis. Consulted only to
    /// rebuild the model at a pc that no recorded branch describes.
    ///
    /// Populated with EMPTY metadata apart from the resolved `getstatic`
    /// types (round 9 wave 10), which costs nothing here: the analysis needs
    /// field types, call arities and constant-pool tags, and this backend
    /// refuses every method containing any other field access, a call of any
    /// kind, or any `ldc`.
    pub(in crate::aarch64_backend) stack_kinds: crate::x64::stack_kinds::StackKindMap,
    /// Bytecode PC -> label mapping for branch targets.
    pub(in crate::aarch64_backend) pc_labels: HashMap<usize, u32>,
    /// Bytecode PC of the instruction currently being lowered. Read by
    /// [`Arm64Backend::label_for_pc`] to tell a back-edge from a forward
    /// branch, and by the safepoint poll for its id and oop-local mask.
    pub(in crate::aarch64_backend) cur_bytecode_pc: usize,
    /// Per-bytecode-pc "must be oop" local masks, and whether the dataflow
    /// reached each pc. Shared with x64 (`compute_local_oop_masks`). Empty when
    /// unsupported (>64 locals), which this backend treats as "no claim".
    pub(in crate::aarch64_backend) local_oop_masks: Vec<u64>,
    pub(in crate::aarch64_backend) local_oop_reached: Vec<bool>,
    /// Which parameter slots hold references on entry. The ENTRY poll answers
    /// from this. Zero unless [`Arm64Backend::set_method_descriptor`] was called.
    pub(in crate::aarch64_backend) param_oop_mask: u64,
    /// The JVM local slot of each incoming argument, from the descriptor
    /// (`compute_param_jvm_slots`), or `None` for the identity layout
    /// `0..num_params` when no descriptor was supplied.
    pub(in crate::aarch64_backend) param_jvm_slots: Option<Vec<usize>>,
    /// JVMS §6.5 `ireturn` narrowing for this method's declared return type
    /// (`Z`/`B`/`C`/`S`, from [`crate::narrowed_int_return_tag`]), or `None`.
    /// Set by [`Arm64Backend::set_method_descriptor`]; x64 applies the same
    /// narrowing (`emit_narrow_int_return`), so a compiled body never hands
    /// back a `boolean` of 2 or a `byte` of 300.
    pub(in crate::aarch64_backend) return_narrow: Option<u8>,
    /// Resolved `getstatic` sites, keyed by the bytecode pc of the `0xb2`
    /// (round 9 wave 10). Empty unless [`Arm64Backend::set_static_field_info`]
    /// was called, in which case every `getstatic` refuses the method.
    pub(in crate::aarch64_backend) static_fields: HashMap<usize, Arm64StaticField>,
    /// Resolved `getfield`/`putfield` sites, keyed by the bytecode pc of the
    /// `0xb4`/`0xb5` (round 9 wave 15). Empty unless
    /// [`Arm64Backend::set_instance_field_info`] was called, in which case
    /// every instance field access refuses the method.
    pub(in crate::aarch64_backend) instance_fields: HashMap<usize, Arm64InstanceField>,
    /// Whether this compilation takes the VM context pointer (round 9 wave
    /// 16). Decided ONCE per pass, from the bytecode alone -- it must be the
    /// same in both passes, and it must be known before the frame is sized,
    /// because the context word is part of the frame.
    pub(in crate::aarch64_backend) needs_context: bool,
    /// Whether this method contains an allocation (round 9 wave 18). Decided
    /// the same way and for the same reason: an allocation calls a helper that
    /// can SAFEPOINT, which needs the safepoint homes and the id word in the
    /// frame -- whether or not `CRATONVM_JIT_ARM64_SAFEPOINTS` reserved them
    /// for polling.
    pub(in crate::aarch64_backend) allocates: bool,
    /// Resolved `new`/`anewarray` sites, keyed by the pc of the opcode (round
    /// 9 wave 18). Empty unless [`Arm64Backend::set_new_site_info`] was
    /// called, in which case every `new` and `anewarray` refuses the method.
    pub(in crate::aarch64_backend) new_sites: HashMap<usize, Arm64NewSite>,
    /// Resolved `multianewarray` sites, keyed by the pc of the opcode (h23c):
    /// the packed `(holder_class_id | cp_idx << 32)` descriptor the helper
    /// resolves the array class from. A site absent from the map refuses the
    /// method, which is what an unwired caller gets for free.
    pub(in crate::aarch64_backend) multianewarray_sites: HashMap<usize, i64>,
    /// Declaring class ids of every `getstatic` this compilation lowered,
    /// published as `CompiledMethod::static_init_classes`. Reset per pass.
    pub(in crate::aarch64_backend) static_init_classes: Vec<u32>,
    /// Whether the caller vouched that this method's exception table is EMPTY
    /// (round 9 wave 11), via [`Arm64Backend::set_exception_table_empty`].
    /// `false` until told -- "unknown" must refuse, not assume. Gates the
    /// direct-throw lowerings (`idiv`/`irem`/`ldiv`/`lrem`): with no handler in
    /// this frame, the `ArithmeticException` the stub raises can only
    /// propagate to the caller, so no throw bci has to be recorded.
    pub(in crate::aarch64_backend) exception_table_empty: bool,
    /// One out-of-line `ArithmeticException` stub per distinct bci (see
    /// [`Arm64Backend::arith_throw_label`]), emitted after the epilogue by
    /// [`Arm64Backend::emit_arith_throw_stub`]. Reset per pass.
    pub(in crate::aarch64_backend) arith_throw_labels: Vec<(usize, u32)>,
    /// One out-of-line `ArrayIndexOutOfBoundsException` stub per SITE (round 9
    /// wave 13), emitted after the epilogue by
    /// [`Arm64Backend::emit_aioobe_stubs`]. Per site, not per action: the
    /// helper takes the index, the length, the array and the bci, and two
    /// sites share none of them.
    pub(in crate::aarch64_backend) aioobe_stubs: Vec<Arm64AioobeStub>,
    /// One out-of-line `NullPointerException` stub per JEP-358 action code
    /// this pass needed (round 9 wave 12), emitted after the epilogue by
    /// [`Arm64Backend::emit_npe_throw_stubs`].
    ///
    /// A `Vec`, not a `HashMap`: the stubs are EMITTED from it, so their order
    /// is machine-code layout, and a `HashMap`'s iteration order would make
    /// two compilations of the same method produce different bytes.
    pub(in crate::aarch64_backend) npe_throw_labels: Vec<(u8, usize, u32)>,
    /// Frame offsets of reference LOCALS at the safepoint being emitted, folded
    /// into the map by `emit_oop_map_for_safepoint`. Taken, not copied.
    pub(in crate::aarch64_backend) pending_local_oop_slots: Vec<i32>,
    /// Frame offsets of reference OPERANDS the poll stored for its call. Taken,
    /// not copied, like the locals.
    pub(in crate::aarch64_backend) pending_operand_oop_slots: Vec<i32>,
    /// Label for the shared epilogue.
    pub(in crate::aarch64_backend) epilogue_label: u32,
    /// Number of parameter SLOTS for the current method.
    pub(in crate::aarch64_backend) num_params: usize,
    /// Method invoke metadata: maps constant pool index to argument count.
    pub(in crate::aarch64_backend) method_info: HashMap<u16, usize>,
    /// Set to true if a compilation error occurred (e.g. stack underflow).
    pub failed: bool,
    /// T1.1.3 — collected oop maps, PC-unresolved; see [`Arm64PendingOopMap`].
    pub pending_oop_maps: Vec<Arm64PendingOopMap>,
    /// Runtime helper addresses. Zeroed until [`Arm64Backend::set_helpers`] is
    /// called; `safepoint_flag_addr == 0` means "not wired" and the poll emits
    /// nothing, the same contract x64 uses.
    pub(in crate::aarch64_backend) helpers: crate::JitRuntimeHelpers,
    /// Bytecode PCs that are the target of a BACKWARD branch -- loop headers.
    /// Discovered by pass 1 and read by pass 2, which polls at each one.
    pub(in crate::aarch64_backend) back_edge_targets: std::collections::HashSet<usize>,
    /// Frame offset (POSITIVE; the slot is at `[FP - sp_id_slot_off]`) of the
    /// word each safepoint stamps its id into, or 0 when none is reserved.
    pub(in crate::aarch64_backend) sp_id_slot_off: i32,
    /// How the spill area is partitioned — filled in when the frame is sized,
    /// and the single source of every word index below `spill_offset`. See
    /// [`Arm64SpillArea`]. All-zero before the prologue runs, which makes every
    /// accessor refuse rather than compute an offset into a frame that does not
    /// exist yet.
    pub(in crate::aarch64_backend) spill_area: Arm64SpillArea,
    /// Safepoints this compilation published a map for, and how many of those
    /// maps could NOT describe everything live at their site. A count, not a
    /// set of pcs: two safepoints can share one bci.
    pub(in crate::aarch64_backend) safepoint_count: usize,
    pub(in crate::aarch64_backend) incomplete_oop_maps: usize,
    /// Set by the poll when it could not describe this site; consumed (taken)
    /// by the map writer.
    pub(in crate::aarch64_backend) pending_map_incomplete: bool,
    /// Resolved `invoke*` sites, by bytecode pc (round 9 wave 22). A site
    /// absent from this map refuses the method in its own arm.
    pub(in crate::aarch64_backend) invoke_sites: HashMap<usize, Arm64InvokeSite>,
    /// Resolved `checkcast`/`instanceof` sites, by bytecode pc (round 9 wave
    /// 23). A site absent from this map refuses the method.
    pub(in crate::aarch64_backend) typecheck_sites: HashMap<usize, Arm64TypecheckSite>,
    /// Whether this method contains any `invoke*` (round 9 wave 22). Decided
    /// from the BYTECODE, not from `invoke_sites`, so the frame is the same
    /// shape whether or not the caller resolved the site -- an unresolved one
    /// refuses the method anyway, and both compile passes must agree on the
    /// frame before either knows that.
    pub(in crate::aarch64_backend) calls: bool,
    /// Set by [`Self::emit_frame_record`] when it actually emitted the call
    /// that publishes FP. See `Arm64CompileResult::frame_base_published`.
    pub(in crate::aarch64_backend) frame_base_published: bool,
    /// Whether this compilation emits safepoint polls, seeded from
    /// [`arm64_safepoints_enabled`] in `new()`. A field so a test can reach
    /// both arms of a process-latched switch.
    pub(in crate::aarch64_backend) safepoints_enabled: bool,
}

/// Scratch registers available for the operand stack (X9-X15, 7 regs).
pub(in crate::aarch64_backend) const SCRATCH_REGS: [Arm64Register; 7] = [
    Arm64Register::X9,
    Arm64Register::X10,
    Arm64Register::X11,
    Arm64Register::X12,
    Arm64Register::X13,
    Arm64Register::X14,
    Arm64Register::X15,
];

/// Float scratch registers for the operand stack (V0-V7, 8 regs).
pub(in crate::aarch64_backend) const FLOAT_SCRATCH_REGS: [Arm64Register; 8] = [
    Arm64Register::V0,
    Arm64Register::V1,
    Arm64Register::V2,
    Arm64Register::V3,
    Arm64Register::V4,
    Arm64Register::V5,
    Arm64Register::V6,
    Arm64Register::V7,
];

/// Page size the stack bang probes at. AArch64 Linux, Windows and Darwin all
/// place at least this much guard below a thread's stack.
pub(in crate::aarch64_backend) const STACK_BANG_PAGE_BYTES: i32 = 4096;

/// Probes beyond this many refuse the method (a 2 MiB frame), for the same
/// code-size reason as x64's `MAX_STACK_BANG_PROBES`.
pub(in crate::aarch64_backend) const MAX_STACK_BANG_PROBES: usize = 512;

/// SP-relative distances (subtracted from SP) the prologue probes before it
/// moves SP down by `frame_below` bytes, or `None` when the frame needs more
/// probes than [`MAX_STACK_BANG_PROBES`].
///
/// The same scheme as x64's `stack_bang_frame_probe_disps`: one probe per page
/// the new frame crosses, plus the exact frame bottom when it is not
/// page-aligned. A frame smaller than a page needs none: it cannot step over
/// a guard page, so its own first store already lands on the guard.
pub(in crate::aarch64_backend) fn stack_bang_probe_offsets(frame_below: i32) -> Option<Vec<i32>> {
    if frame_below < 0 {
        return None;
    }
    let mut offsets = Vec::new();
    let mut off = STACK_BANG_PAGE_BYTES;
    while off <= frame_below {
        if offsets.len() >= MAX_STACK_BANG_PROBES {
            return None;
        }
        offsets.push(off);
        off = off.checked_add(STACK_BANG_PAGE_BYTES)?;
    }
    if frame_below >= STACK_BANG_PAGE_BYTES && frame_below % STACK_BANG_PAGE_BYTES != 0 {
        if offsets.len() >= MAX_STACK_BANG_PROBES {
            return None;
        }
        offsets.push(frame_below);
    }
    Some(offsets)
}

/// One deferred `ArrayIndexOutOfBoundsException` site (round 9 wave 13).
///
/// The guard's `B.HS` is the only way in, so the three register fields name
/// where that guard left the helper's arguments -- not where they live in
/// general, which is a question with no answer once the bytecode is over.
#[derive(Debug, Clone, Copy)]
pub(in crate::aarch64_backend) struct Arm64AioobeStub {
    pub(in crate::aarch64_backend) label: u32,
    pub(in crate::aarch64_backend) index: Arm64Register,
    pub(in crate::aarch64_backend) length: Arm64Register,
    pub(in crate::aarch64_backend) array: Arm64Register,
    pub(in crate::aarch64_backend) bci: usize,
}

/// What one primitive array element opcode needs to know (round 9 wave 13).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::aarch64_backend) struct Arm64ArrayShape {
    /// The operand-stack kind the element loads to, or stores from.
    pub(in crate::aarch64_backend) kind: OperandKind,
    /// The access width, i.e. the element's byte size.
    pub(in crate::aarch64_backend) width: Arm64MemWidth,
    /// `log2(element size)`, the `LSL` amount the index is scaled by.
    pub(in crate::aarch64_backend) shift: u8,
    /// The JEP-358 action code a null receiver reports.
    pub(in crate::aarch64_backend) action: u8,
    /// Whether a sub-word LOAD sign-extends (`baload`, `saload`) rather than
    /// zero-extends (`caload`). Meaningless for a store, which truncates.
    pub(in crate::aarch64_backend) signed: bool,
}

/// The array LOAD opcodes this backend lowers, and what each one is.
///
/// `aaload` (`0x32`) is deliberately absent: a reference element is a narrow
/// oop when compressed oops are on, must be marked as an oop on the operand
/// stack, and becomes a GC root this backend would then owe a map for at every
/// later safepoint. None of that is an ordering question, which is why it is a
/// separate piece of work rather than a row here.
pub(in crate::aarch64_backend) fn array_element_load_shape(opcode: u8) -> Option<Arm64ArrayShape> {
    let (kind, width, shift, action, signed) = match opcode {
        // iaload
        0x2e => (
            OperandKind::I32,
            Arm64MemWidth::W32,
            2,
            npe_action::ALOAD_INT,
            true,
        ),
        // laload
        0x2f => (
            OperandKind::I64,
            Arm64MemWidth::X64,
            3,
            npe_action::ALOAD_LONG,
            true,
        ),
        // faload
        0x30 => (
            OperandKind::F32,
            Arm64MemWidth::W32,
            2,
            npe_action::ALOAD_FLOAT,
            true,
        ),
        // daload
        0x31 => (
            OperandKind::F64,
            Arm64MemWidth::X64,
            3,
            npe_action::ALOAD_DOUBLE,
            true,
        ),
        // baload -- `byte[]` sign-extends; a `boolean[]` element is 0 or 1, so
        // the two narrowings agree and one lowering serves both.
        0x33 => (
            OperandKind::I32,
            Arm64MemWidth::B8,
            0,
            npe_action::ALOAD_BYTE,
            true,
        ),
        // caload -- `char` is unsigned, so the zero-extending load IS the value.
        0x34 => (
            OperandKind::I32,
            Arm64MemWidth::H16,
            1,
            npe_action::ALOAD_CHAR,
            false,
        ),
        // saload
        0x35 => (
            OperandKind::I32,
            Arm64MemWidth::H16,
            1,
            npe_action::ALOAD_SHORT,
            true,
        ),
        _ => return None,
    };
    Some(Arm64ArrayShape {
        kind,
        width,
        shift,
        action,
        signed,
    })
}

/// The array STORE opcodes this backend lowers, and what each one is.
///
/// `aastore` (`0x53`) is deliberately absent, and for more reasons than
/// `aaload`: a reference store owes the SATB pre-write barrier (round-7's
/// use-after-free fix), a card mark, and the runtime component-type check that
/// raises `ArrayStoreException` -- three mechanisms this backend has none of.
pub(in crate::aarch64_backend) fn array_element_store_shape(opcode: u8) -> Option<Arm64ArrayShape> {
    let (kind, width, shift, action) = match opcode {
        // iastore
        0x4f => (
            OperandKind::I32,
            Arm64MemWidth::W32,
            2,
            npe_action::ASTORE_INT,
        ),
        // lastore
        0x50 => (
            OperandKind::I64,
            Arm64MemWidth::X64,
            3,
            npe_action::ASTORE_LONG,
        ),
        // fastore
        0x51 => (
            OperandKind::F32,
            Arm64MemWidth::W32,
            2,
            npe_action::ASTORE_FLOAT,
        ),
        // dastore
        0x52 => (
            OperandKind::F64,
            Arm64MemWidth::X64,
            3,
            npe_action::ASTORE_DOUBLE,
        ),
        // bastore
        0x54 => (
            OperandKind::I32,
            Arm64MemWidth::B8,
            0,
            npe_action::ASTORE_BYTE,
        ),
        // castore
        0x55 => (
            OperandKind::I32,
            Arm64MemWidth::H16,
            1,
            npe_action::ASTORE_CHAR,
        ),
        // sastore
        0x56 => (
            OperandKind::I32,
            Arm64MemWidth::H16,
            1,
            npe_action::ASTORE_SHORT,
        ),
        _ => return None,
    };
    Some(Arm64ArrayShape {
        kind,
        width,
        shift,
        action,
        // A store truncates whatever the width is; there is no extension to
        // choose. Fixed `true` so two shapes never differ on a dead field.
        signed: true,
    })
}

/// The operand-stack kind a PRIMITIVE field of `type_tag` is read and written
/// as, or `None` for a reference field (`L`/`[`) or an unrecognised tag.
///
/// The sub-`int` tags all answer `I32`: JVMS keeps `boolean`, `byte`, `char`
/// and `short` on the operand stack as `int`s, and their narrower declared
/// widths matter only at the store ([`Arm64Backend::emit_narrow_to_field_tag`])
/// and at the load's sign/zero extension.
pub(in crate::aarch64_backend) fn primitive_field_operand_kind(type_tag: u8) -> Option<OperandKind> {
    match type_tag {
        b'Z' | b'B' | b'C' | b'S' | b'I' => Some(OperandKind::I32),
        b'J' => Some(OperandKind::I64),
        b'F' => Some(OperandKind::F32),
        b'D' => Some(OperandKind::F64),
        _ => None,
    }
}

/// One argument to a mid-method helper call (round 9 wave 15).
///
/// The register form names a SCRATCH register (X9-X15) and nothing else, which
/// [`Arm64Backend::emit_helper_call`] enforces rather than assumes: the
/// arguments land in X0-X7, so a source that was itself an argument register
/// could be overwritten by an earlier argument's move, and the call would pass
/// a value that was correct when the emitter looked at it and wrong by the
/// time the `BLR` ran. Every value this backend can hand a helper already
/// lives in the scratch pool -- the operand stack is allocated out of it and
/// nothing else is poppable -- so the check costs nothing and closes the case
/// permanently.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(in crate::aarch64_backend) enum Arm64HelperArg {
    /// A value already in a scratch register.
    Reg(Arm64Register),
    /// A compile-time constant, materialized with `MOVZ`/`MOVK`.
    Imm(i64),
}

