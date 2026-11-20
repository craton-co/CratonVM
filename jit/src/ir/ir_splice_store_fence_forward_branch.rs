#[allow(unused_imports)]
use super::*;

impl SpliceFenceRelax {
    /// The round-11 fence: nothing relaxed.
    pub(crate) const NONE: Self = Self {
        forward_branch: false,
        null_facts: false,
        guarded_call: false,
    };

    /// The three switches, read once per compile (never on a hot path).
    pub(crate) fn from_flags() -> Self {
        use cratonvm_types::flags::runtime_flag_default_on;
        Self {
            forward_branch: true,
            null_facts: true,
            guarded_call: true,
        }
    }

    /// Is any relaxation on?
    pub(crate) fn any(self) -> bool {
        self.forward_branch || self.null_facts || self.guarded_call
    }
}

/// `CRATONVM_JIT_IR_SPLICE_STORE_FENCE_FORWARD_BRANCH` (round 16, lane sync,
/// proposal SY16-2 of `jit-r16-sync-proposals-RETIRED-20261006.md`, the W5-4 the round-12 book
/// left open; **default ON since round 16** (measured: SyncM p 14→8 ns),
/// `0`/`false`/`off`/`no` disables): relaxation 1 of
/// [`SpliceFenceRelax`] -- a FORWARD branch inside pc-walked splices -- also
/// after a committed STORE, in the builder
/// (`IrBuilder::admits_after_committed_splice_store`) and in the planner's
/// store-fence walk (`lib.rs` `ir_splice_store_fence_walk`), which read this
/// one value. Rides `CRATONVM_JIT_IR_SPLICE_FENCE_FORWARD_BRANCH` (off there,
/// off here). The shape it admits is a leaf that writes through its receiver
/// and then branches (`a++; b += a; if ((a & 7) == 0) c ^= b; d += c;`),
/// which the synchronized splice already admits for its own bodies; the plain
/// splice refused it as `ir-splice-trap-after-store` and kept the CALL.
/// Read once per build / per site walk.
pub(crate) fn ir_splice_store_fence_forward_branch_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_INLINE_COLD_ATHROW` -- round 16 wave 7 (lane arraylist,
/// AL16-1 of
/// `docs/feature-designs/jit-ir-optimizations.md`).
/// **Default ON** since round 16 (commit `139a504029`, the w7d A/B: `Itr.next`
/// 19 -> 11 ns, `R16Bench arraylist` 838 -> 731 ms;
/// `jit-optin-switch-triage-r16-RETIRED-20261006.md`); `=0` is the kill
/// switch. This line said "Default OFF" until nio21 wave 5 (lane L2), and the
/// wave-4 splice pages repeated it: the code has read
/// `runtime_flag_default_on` since the flip, and `flag-inventory.md` lists it
/// default-on. A spliced callee's handler runs are masked BEFORE its cold arms
/// are looked for (`jit_bridge.rs`), so a `decode` / `encode` body's
/// `assert false` arms are already masked by default.
///
/// The inline resolver refused every callee containing an `athrow`
/// (`inline-resolve REFUSED ... athrow`), so `ArrayList$Itr.next` -- and with
/// it every JDK collection accessor that checks its arguments -- stayed a real
/// call. On: a body whose every `athrow` ends a cold throw arm
/// ([`cold_throw_arms`]) is admitted to the OPTIMIZING tier's splice. The
/// resolver masks each arm ([`mask_cold_throw_arms`]), so nothing in the
/// planner or the builder resolves, scans or builds the arm's exception
/// construction, and the builder turns the arm's branch into a guard that
/// deopts into the interpreter at that branch
/// (`IrBuilder::prune_cold_throw_arm`), which then throws with the callee's
/// own frame on the stack. The single-pass tier's inliner is untouched.
///
/// Read once per resolved site and once per build, never on a hot path.
pub fn ir_inline_cold_athrow_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_STATIC_FINAL_EXACT_CLASS`, **default ON** (`=0` is the
/// kill switch). A trusted `static final` reference field of an initialised
/// class holds one object forever, so the class of that object is a
/// compile-time fact, HotSpot's constant static final (JLS 17.5.3 lets a
/// compiler ignore reflective and `Unsafe` writes to it). The VM files the
/// class when the declaring class's `<clinit>` completes
/// (`ClassRealm::static_final_ref_classes`); the builder then proves an
/// `Op::LoadStatic` of the field exactly that class
/// ([`IrBuilder::set_static_final_exact_classes`]), and the VM's nested
/// resolver keys an exact-receiver row on it -- `HttpStatusClass.UNKNOWN`, an
/// enum constant with a body, is an `HttpStatusClass$1`, and a row resolved
/// for the constant-pool class `HttpStatusClass` could never be proven. Read
/// by both halves (the filing hook and the compiler's feed), never cached.
/// What the VM files for a trusted `static final` holding a plain
/// `java/lang/Object`: `ClassId(0)` is both Object and the empty-slot sentinel,
/// and `0` already files null, so the value is non-null (a trusted constant,
/// e.g. a lock) with no exact class to prove.
pub const STATIC_FINAL_OBJECT_FILED: u32 = u32::MAX;

pub fn ir_static_final_exact_class_enabled() -> bool {
    true
}

/// Where one operand of a call comes from, as
/// [`splice_call_operand_sources`] reads it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SpliceOperandSource {
    /// A load of local `k` (any `xload`, `wide` included).
    Local(usize),
    /// The value a `getstatic` at this pc pushed.
    GetStatic(usize),
    /// `getfield` (at the second pc) of local `k`.
    LocalField(usize, usize),
    /// `getfield` (at the second pc) of the value a `getstatic` at the first
    /// pc pushed.
    StaticField(usize, usize),
    /// The value an `invokedynamic` at this pc pushed.
    Indy(usize),
    /// Anything else that pushes one value (a constant, a `checkcast`ed value).
    Other,
}

thread_local! {
    /// The exact classes of the arguments of the next body
    /// `resolve_inline_site_from` resolves, by the body's local slot. Set by
    /// the resolver's caller immediately before the call and TAKEN at the
    /// resolver's entry, so a value can never reach any other resolution.
    pub(in crate::ir) static SPLICE_ARG_CLASSES: std::cell::RefCell<Vec<Option<u32>>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// `CRATONVM_JIT_IR_STATIC_FINAL_EXACT_CLASS`: hand the next inline
/// resolution the exact classes of its body's arguments, by local slot
/// (`None` = unknown). See [`take_splice_arg_classes`].
pub fn set_splice_arg_classes(classes: Vec<Option<u32>>) {
    SPLICE_ARG_CLASSES.with(|c| *c.borrow_mut() = classes);
}

/// The argument classes [`set_splice_arg_classes`] left for this resolution,
/// leaving none behind. Empty when the caller set none.
pub fn take_splice_arg_classes() -> Vec<Option<u32>> {
    SPLICE_ARG_CLASSES.with(|c| std::mem::take(&mut *c.borrow_mut()))
}

thread_local! {
    /// `CRATONVM_FROZEN_PAGES`: the frozen objects (by address) the arguments
    /// of the next body `resolve_inline_site_from` resolves are expected to
    /// be, by the body's local slot. Same hand-off as [`SPLICE_ARG_CLASSES`].
    pub(in crate::ir) static SPLICE_ARG_OBJECTS: std::cell::RefCell<Vec<Option<usize>>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// Hand the next inline resolution the frozen objects its body's arguments
/// are expected to be, by local slot. Evidence for the planner only: the
/// builder guards before it believes any of it.
pub fn set_splice_arg_objects(objects: Vec<Option<usize>>) {
    crate::frozen_facts::note_planned_objects(&objects);
    SPLICE_ARG_OBJECTS.with(|c| *c.borrow_mut() = objects);
}

/// The objects [`set_splice_arg_objects`] left for this resolution, leaving
/// none behind.
pub fn take_splice_arg_objects() -> Vec<Option<usize>> {
    SPLICE_ARG_OBJECTS.with(|c| std::mem::take(&mut *c.borrow_mut()))
}

thread_local! {
    /// The frozen objects the operands of the compiling method's call site
    /// being planned are expected to be ([`SpliceSiteObjects`]).
    pub(in crate::ir) static SPLICE_SITE_OBJECTS: std::cell::RefCell<Vec<Option<usize>>> =
        const { std::cell::RefCell::new(Vec::new()) };
}

/// The frozen objects the operands of ONE of the compiling method's own call
/// sites are expected to be, for as long as this value lives. A root site
/// asks the VM several times (the constant-pool answer, the exact one, the
/// profiled one), so the one-shot hand-off of the nested sites would be spent
/// on the first; dropping this ends the site and leaves nothing behind.
pub struct SpliceSiteObjects {
    prev: Vec<Option<usize>>,
}

impl SpliceSiteObjects {
    pub fn set(objects: Vec<Option<usize>>) -> Self {
        crate::frozen_facts::note_planned_objects(&objects);
        let prev = SPLICE_SITE_OBJECTS.with(|c| std::mem::replace(&mut *c.borrow_mut(), objects));
        Self { prev }
    }
}

impl Drop for SpliceSiteObjects {
    fn drop(&mut self) {
        let prev = std::mem::take(&mut self.prev);
        SPLICE_SITE_OBJECTS.with(|c| *c.borrow_mut() = prev);
    }
}

/// The objects of the root site being planned; empty outside one.
pub fn splice_site_objects() -> Vec<Option<usize>> {
    SPLICE_SITE_OBJECTS.with(|c| c.borrow().clone())
}

thread_local! {
    /// [`note_cha_watermark`]'s hand-off from the VM's exact-receiver resolver
    /// to the IR planner that called it (`0` = none).
    pub(in crate::ir) static CHA_WATERMARK_HANDOFF: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// JIT round 23 wave 6 (`CRATONVM_JIT_IR_CHA_MONOMORPHIC_SPLICE`): the VM's
/// exact-receiver resolver found that every loaded subtype of the site's
/// constant-pool class selects the body it just resolved, and that every class
/// id below `watermark` existed when it looked
/// (`ClassRealm::cha_unique_target_watermark`). The planner takes it with
/// [`take_cha_watermark`] right after the resolver returns, on the same thread.
pub fn note_cha_watermark(watermark: u32) {
    CHA_WATERMARK_HANDOFF.with(|c| c.set(watermark));
}

/// The watermark [`note_cha_watermark`] left for this resolution, leaving none
/// behind. `0` when the resolver noted none.
pub fn take_cha_watermark() -> u32 {
    CHA_WATERMARK_HANDOFF.with(|c| c.replace(0))
}

/// JIT round 23 wave 6 (proxy-define page, proposal 4f item 2;
/// `CRATONVM_JIT_IR_CHA_MONOMORPHIC_SPLICE`, perf, **opt-in, default OFF**):
/// splice a virtual site's body behind a class-id WATERMARK test when
/// class-hierarchy analysis shows every loaded subtype of the constant-pool
/// class selects that one body. The shape is a getter declared once on an
/// abstract base and inherited by many concrete classes
/// (`AbstractPoolEntry.index()`, ~1000 single-pass samples on the proxy row),
/// which no exact-class or profile guard can cover.
///
/// The test is `0 < class word < watermark`
/// ([`IrBuilder::cha_watermark_receiver_test`]); its miss edge is the site's
/// ordinary call. Sound with no retirement: a class defined after the analysis
/// has a larger id and misses (see the VM's
/// `ClassRealm::cha_unique_target_watermark`). Opt-in until an A/B and a run of
/// the class-redefinition and hidden-class batteries with it on. Read per
/// compile, no static.
pub fn ir_cha_monomorphic_splice_enabled() -> bool {
    false
}

/// The JVM local slot each operand of a call of `desc` binds in the callee,
/// receiver first when `has_receiver` (a `long` / `double` takes two slots).
/// `None` for a malformed descriptor.
pub fn descriptor_operand_slots(desc: &str, has_receiver: bool) -> Option<Vec<usize>> {
    let params = desc.strip_prefix('(')?.split_once(')')?.0.as_bytes();
    let mut slots = Vec::new();
    let mut next = 0usize;
    if has_receiver {
        slots.push(0);
        next = 1;
    }
    let mut i = 0usize;
    while i < params.len() {
        let wide = matches!(params[i], b'J' | b'D');
        while params.get(i) == Some(&b'[') {
            i += 1;
        }
        match params.get(i)? {
            b'L' => i += params[i..].iter().position(|&b| b == b';')? + 1,
            b'B' | b'C' | b'D' | b'F' | b'I' | b'J' | b'S' | b'Z' => i += 1,
            _ => return None,
        }
        slots.push(next);
        next += if wide { 2 } else { 1 };
    }
    Some(slots)
}

/// Where each of the `values` operands of the call at `call_pc` comes from,
/// first operand first, when they are pushed by the straight-line run of
/// instructions right before the call: each a single push (`xload`,
/// `getstatic`, a constant) or a `checkcast` of the value below it. `None`
/// when the run is anything else, or when a branch target or an exception
/// handler (`handler_pcs`) lies inside it -- the stack at the call could then
/// come from another path. Used to key a nested exact-receiver row on the
/// class a trusted `static final` or a known argument proves
/// (`CRATONVM_JIT_IR_STATIC_FINAL_EXACT_CLASS`).
pub fn splice_call_operand_sources(
    code: &[u8],
    code_len: usize,
    call_pc: usize,
    values: usize,
    handler_pcs: &[usize],
) -> Option<Vec<SpliceOperandSource>> {
    splice_call_operand_sources_with_calls(code, code_len, call_pc, values, handler_pcs, &|_| None)
}

/// [`splice_call_operand_sources`] extended with an optional `(pops, pushes)`
/// callback for `invoke*` / `invokedynamic` instructions inside an operand's
/// straight-line expression (`LAMBDA321-5`).
pub fn splice_call_operand_sources_with_calls(
    code: &[u8],
    code_len: usize,
    call_pc: usize,
    values: usize,
    handler_pcs: &[usize],
    call_effect: &dyn Fn(usize) -> Option<(usize, usize)>,
) -> Option<Vec<SpliceOperandSource>> {
    use crate::bytecode_analysis as ba;
    let len = code_len.min(code.len());
    let insns = ba::decode_method(code, len)?;
    let call = insns.iter().position(|i| i.pc == call_pc)?;
    let mut targets: Vec<usize> = handler_pcs.to_vec();
    let mut scratch = Vec::new();
    for insn in &insns {
        scratch.clear();
        if !ba::explicit_targets(code, len, insn.pc, &mut scratch) {
            return None;
        }
        targets.extend(scratch.iter().copied());
    }
    let expressions = splice_operand_expressions_enabled();
    let mut out: Vec<SpliceOperandSource> = Vec::with_capacity(values);
    let mut j = call;
    while out.len() < values {
        // One operand: the run of instructions, walking back, that produces
        // exactly one more value than it consumes. Without
        // `splice_operand_expressions_enabled` only a single push (or a
        // `checkcast` of one) qualifies, as before.
        let mut need = 1usize;
        let mut pushes: Vec<usize> = Vec::new();
        let mut cast = false;
        while need > 0 {
            // Every instruction after the first push of the run up to the
            // call itself must be entered only by falling through.
            if targets.contains(&insns[j].pc) {
                return None;
            }
            j = j.checked_sub(1)?;
            let op = insns[j].op;
            if op == 0xc0 {
                // `checkcast`: one value in, one out.
                cast = true;
                continue;
            }
            let (pops, pushed) = match splice_operand_pops(op) {
                Some(p) => (p, 1),
                None if expressions && matches!(op, 0xb6..=0xba) => {
                    let (p, pushes_count) = call_effect(insns[j].pc)?;
                    if pushes_count != 1 {
                        return None;
                    }
                    (p, 1)
                }
                // An inline varargs array: `anewarray` / `newarray` (count in,
                // array out), `dup` of it, and each element's array store.
                None if expressions => match op {
                    0xbc | 0xbd => (1, 1),
                    0x59 => (1, 2),
                    0x4f..=0x56 => (3, 0),
                    _ => return None,
                },
                None => return None,
            };
            if !expressions && pops > 0 {
                return None;
            }
            // A `dup` whose copies feed this operand and one below it: the
            // operands share a value, so neither has a source of its own.
            if pushed > need {
                return None;
            }
            pushes.push(j);
            need = need + pops - pushed;
        }
        let source = match pushes.as_slice() {
            [k] if !cast => {
                let insn = &insns[*k];
                match insn.op {
                    0x15..=0x19 => SpliceOperandSource::Local(if insn.wide {
                        usize::from(u16::from_be_bytes([
                            *code.get(insn.pc + 2)?,
                            *code.get(insn.pc + 3)?,
                        ]))
                    } else {
                        usize::from(*code.get(insn.pc + 1)?)
                    }),
                    0x1a..=0x2d => SpliceOperandSource::Local(usize::from((insn.op - 0x1a) % 4)),
                    0xb2 => SpliceOperandSource::GetStatic(insn.pc),
                    _ if !cast && pushes.first().is_some_and(|&k| insns[k].op == 0xba) => {
                SpliceOperandSource::Indy(insns[pushes[0]].pc)
            }
            _ => SpliceOperandSource::Other,
                }
            }
            // Walking back: the `getfield`, then the single push under it.
            [field, base] if !cast && insns[*field].op == 0xb4 => {
                let (field_pc, base) = (insns[*field].pc, &insns[*base]);
                match base.op {
                    0x19 if !base.wide => SpliceOperandSource::LocalField(
                        usize::from(*code.get(base.pc + 1)?),
                        field_pc,
                    ),
                    0x2a..=0x2d => {
                        SpliceOperandSource::LocalField(usize::from(base.op - 0x2a), field_pc)
                    }
                    0xb2 => SpliceOperandSource::StaticField(base.pc, field_pc),
                    _ if !cast && pushes.first().is_some_and(|&k| insns[k].op == 0xba) => {
                SpliceOperandSource::Indy(insns[pushes[0]].pc)
            }
            _ => SpliceOperandSource::Other,
                }
            }
            _ if !cast && pushes.first().is_some_and(|&k| insns[k].op == 0xba) => {
                SpliceOperandSource::Indy(insns[pushes[0]].pc)
            }
            _ => SpliceOperandSource::Other,
        };
        out.push(source);
    }
    out.reverse();
    Some(out)
}

/// How many stack VALUES `op` consumes, for an instruction
/// [`splice_call_operand_sources`] may step over: it pushes exactly one value
/// and falls through. `None` for anything else (a store, a call, a stack
/// shuffle, a branch). A push of a constant, a local or a static consumes
/// none; the expression ops consume their operands.
pub(in crate::ir) fn splice_operand_pops(op: u8) -> Option<usize> {
    Some(match op {
        // aconst_null .. ldc2_w, the xload family, getstatic.
        0x01..=0x14 | 0x15..=0x2d | 0xb2 => 0,
        // The negations, the conversions, getfield, arraylength.
        0x74..=0x77 | 0x85..=0x93 | 0xb4 | 0xbe => 1,
        // The array element loads, the binary arithmetic, shifts and logic,
        // the long / FP compares.
        0x2e..=0x35 | 0x60..=0x73 | 0x78..=0x83 | 0x94..=0x98 => 2,
        _ => return None,
    })
}

/// `CRATONVM_JIT_IR_SPLICE_OPERAND_EXPRESSIONS` (the r23 L8 heap-buffer close;
/// perf, **default ON**, `=0` restores single-push operands):
/// [`splice_call_operand_sources`] steps over a whole pure expression per
/// operand, so the receiver of `b.putInt(seed + k * 0x01010101)` is still
/// known to be `aload_0`.
///
/// Why: only a single push qualified, so a call whose LATER operand is
/// computed (`putInt(x + y)`) left its receiver unknown, and in `heapbb`
/// spliced into its caller the exact-receiver row was planned for
/// `getInt()` / `clear()` / `flip()` but not for `putInt(int)`, which stayed a
/// virtual call per element (`nest-profiled ... putInt ... no evidence`). The
/// receiver's source is still a single push; the instructions after it only
/// compute the other operands, which cannot change a value already pushed.
pub fn splice_operand_expressions_enabled() -> bool {
    true
}

/// The locals of `code[..code_len]` written exactly once, each by an `astore`
/// of a `new` the straight-line run before it constructs (`new C; dup; ..;
/// invokespecial C.<init>; astore k`), as `local -> pc of the new`. A bytecode
/// reading; the builder proves the same slots exact during its walk
/// (`exact_receiver_single_store_slots`), which is what a splice relies on.
pub fn write_once_new_locals(code: &[u8], code_len: usize) -> HashMap<usize, usize> {
    use crate::bytecode_analysis as ba;
    let len = code_len.min(code.len());
    let Some(insns) = ba::decode_method(code, len) else {
        return HashMap::new();
    };
    let mut targets: HashSet<usize> = HashSet::new();
    let mut scratch = Vec::new();
    for insn in &insns {
        scratch.clear();
        if !ba::explicit_targets(code, len, insn.pc, &mut scratch) {
            return HashMap::new();
        }
        targets.extend(scratch.iter().copied());
    }
    let slot_of = |insn: &ba::Insn| -> Option<(usize, bool)> {
        let index = |insn: &ba::Insn| -> Option<usize> {
            Some(if insn.wide {
                usize::from(u16::from_be_bytes([*code.get(insn.pc + 2)?, *code.get(insn.pc + 3)?]))
            } else {
                usize::from(*code.get(insn.pc + 1)?)
            })
        };
        match insn.op {
            0x36..=0x3a | 0x84 => Some((index(insn)?, matches!(insn.op, 0x37 | 0x39))),
            0x3b..=0x4e => Some((
                usize::from((insn.op - 0x3b) % 4),
                matches!((insn.op - 0x3b) / 4, 1 | 3),
            )),
            _ => None,
        }
    };
    let mut stores: HashMap<usize, Vec<usize>> = HashMap::new();
    for (i, insn) in insns.iter().enumerate() {
        if let Some((k, wide)) = slot_of(insn) {
            stores.entry(k).or_default().push(i);
            if wide {
                stores.entry(k + 1).or_default().push(usize::MAX);
            }
        }
    }
    let mut out = HashMap::new();
    for (k, at) in stores {
        let [i] = at[..] else {
            continue;
        };
        if i == usize::MAX || !matches!(insns[i].op, 0x3a | 0x4b..=0x4e) || i == 0 {
            continue;
        }
        // `invokespecial <init>` right before the store, and the `new; dup`
        // it consumes earlier in one straight-line run.
        if insns[i - 1].opcode != 0xb7 || targets.contains(&insns[i].pc) {
            continue;
        }
        let mut j = i - 1;
        let new_pc = loop {
            if targets.contains(&insns[j].pc) {
                break None;
            }
            let Some(prev) = j.checked_sub(1) else {
                break None;
            };
            j = prev;
            if insns[j].opcode == 0xbb && insns.get(j + 1).is_some_and(|d| d.opcode == 0x59) {
                break Some(insns[j].pc);
            }
            if !ba::falls_through(insns[j].opcode) || ba::is_offset_branch(insns[j].opcode) {
                break None;
            }
        };
        if let Some(new_pc) = new_pc {
            out.insert(k, new_pc);
        }
    }
    out
}

/// Does `code[..code_len]` write local `k` anywhere (any `xstore`, `wide`
/// included, a two-slot store into `k - 1`, or `iinc`)? `None` when the body
/// does not decode.
pub fn body_writes_local(code: &[u8], code_len: usize, k: usize) -> Option<bool> {
    use crate::bytecode_analysis as ba;
    let len = code_len.min(code.len());
    for insn in ba::decode_method(code, len)? {
        let index = |insn: &ba::Insn| -> Option<usize> {
            Some(if insn.wide {
                usize::from(u16::from_be_bytes([*code.get(insn.pc + 2)?, *code.get(insn.pc + 3)?]))
            } else {
                usize::from(*code.get(insn.pc + 1)?)
            })
        };
        let (slot, wide_value) = match insn.op {
            0x36 | 0x38 | 0x3a | 0x84 => (index(&insn)?, false),
            0x37 | 0x39 => (index(&insn)?, true),
            0x3b..=0x4e => {
                let n = usize::from((insn.op - 0x3b) % 4);
                (n, matches!((insn.op - 0x3b) / 4, 1 | 3))
            }
            _ => continue,
        };
        if slot == k || (wide_value && slot + 1 == k) {
            return Some(true);
        }
    }
    Some(false)
}

fn field_opcode_cp_indices(code: &[u8], code_len: usize, opcode: u8) -> Option<Vec<u16>> {
    if let Some(insns) = crate::bytecode_analysis::decode_method(code, code_len) {
        return Some(
            insns
                .iter()
                .filter(|i| i.opcode == opcode)
                .filter_map(|i| {
                    Some(u16::from_be_bytes([*code.get(i.pc + 1)?, *code.get(i.pc + 2)?]))
                })
                .collect(),
        );
    }
    // The strict decoder refuses a body for reasons that do not move an
    // instruction boundary; the plain length walk finds every opcode as long
    // as each length is known, and refuses at the first one that is not.
    let len = code_len.min(code.len());
    let mut out = Vec::new();
    let mut pc = 0usize;
    while pc < len {
        let n = crate::bytecode_analysis::insn_len(code, pc)?;
        if n == 0 || pc + n > len {
            return None;
        }
        if code[pc] == opcode {
            out.push(u16::from_be_bytes([*code.get(pc + 1)?, *code.get(pc + 2)?]));
        }
        pc += n;
    }
    Some(out)
}

/// The constant-pool indices of every `putstatic` in `code[..code_len]`, for
/// the VM's static-final trust rule on a class file older than 53 (a method
/// other than `<clinit>` may still write a final there; HotSpot's
/// `has_initialized_final_update`). `None` when the body does not decode.
pub fn putstatic_cp_indices(code: &[u8], code_len: usize) -> Option<Vec<u16>> {
    field_opcode_cp_indices(code, code_len, 0xb3)
}

/// The constant-pool indices of every `getfield` in `code[..code_len]`, for
/// selective superclass demand invalidation. `None` when undecodable.
pub fn getfield_cp_indices(code: &[u8], code_len: usize) -> Option<Vec<u16>> {
    field_opcode_cp_indices(code, code_len, 0xb4)
}

/// Compiled local throw edges in the optimizing tier (round nio21 wave 1,
/// lane L2; `CRATONVM_JIT_IR_LOCAL_THROW_EDGES`, **default ON** since wave 2,
/// `=0` off).
///
/// With it on, an `athrow` of an exception this method allocated, whose first
/// covering exception-table row is a catch-all or names exactly that class,
/// branches to the handler block of the same compiled body instead of calling
/// `jit_throw_exception` and leaving the frame for the interpreter's handler
/// search (`IrBuilder::local_throw_target`). Every other throw is unchanged.
///
/// The edge posts neither JVMTI exception event. Wave 2 closed both observers:
/// a compile while an `Exception` / `ExceptionCatch` client may be armed
/// builds no edge (`CompileRequest::exception_events_armed_at_compile`), and a
/// listener armed later holds the VM off compiled code and withdraws it
/// (`JvmtiEventManager::local_throw_edges_need_the_interpreter`, which reads
/// this same switch). The argument is on
/// `docs/feature-designs/jit-ir-optimizations.md`.
/// Spliced throw edges in the optimizing tier
/// (`CRATONVM_JIT_IR_SPLICED_THROW_EDGES`, **default ON**, `=0` off).
///
/// An `athrow` inside a spliced callee whose OUTERMOST `invoke` lies in a
/// `try` of the compiling method branches to that `try`'s handler when the
/// thrown object passes the first covering row's type test, and deopts at the
/// `athrow` otherwise (`IrBuilder::spliced_throw_edge`). The VM's resolver
/// admits a throwing body at such a site only (`set_ir_root_site_in_try`): it
/// keeps a cold arm that throws a `getstatic` value unmasked, and admits a
/// body whose throws are not cold arms. With it, `lib.rs` hands the
/// exception table to the optimizing OSR route too, whose loop body is the
/// only compiled form of a method run once (netty's exhaustive
/// `HttpHeaderValidationUtilTest` loops throw on most iterations). Read per
/// build, per resolved site and per throw.
/// `CRATONVM_JIT_IR_SPLICE_SWITCH`, **default ON** (`=0` restores the
/// refusal): the optimizing tier splices a body with a `tableswitch` /
/// `lookupswitch`. A switch's operands are padded to a 4-byte boundary of
/// the method's OWN code array, so the planner appends such a body at a
/// 4-aligned base ([`body_has_switch`]), where the padding of every combined
/// pc is the callee's, and the builder decodes it within the body's bounds.
/// Read per resolved site, per appended body and per spliced switch.
pub fn ir_splice_switch_enabled() -> bool {
    true
}

/// Does `code[..code_len]` hold a `tableswitch` or `lookupswitch`? `None`
/// when the body does not decode.
pub fn body_has_switch(code: &[u8], code_len: usize) -> Option<bool> {
    Some(
        crate::bytecode_analysis::decode_method(code, code_len)?
            .iter()
            .any(|i| matches!(i.opcode, 0xaa | 0xab)),
    )
}

/// `CRATONVM_JIT_IR_SPLICE_UNRECORDED_CALL_EXIT`, **default ON** (`=0`
/// restores the refusal): a call in a spliced body whose constant-pool owner
/// record is not made yet (the call has never run from this class --
/// `AssertNotEquals.assertNotEquals`'s failure arm) becomes the precise exit
/// an unloaded static callee gets, instead of refusing the whole body. The
/// splice never executes it; the interpreter it exits to runs the `invoke`
/// and makes the record. `invokevirtual` / `invokeinterface` take the same
/// exit (`invokespecial` is still refused). Read per resolved site and per
/// built call.
/// `CRATONVM_JIT_IR_SPLICE_NEVER_RUN_PRIVATE_EXIT=1` (JIT round 21 wave 1,
/// lane updaters, UPDATERS21-3; opt-in, default OFF): the VM lists a spliced
/// body's never-run PRIVATE call (`vm::runtime::interpreter::jit_bridge`
/// `never_run_private_exit_pcs`) in `unloaded_static_call_pcs`, and the
/// builder plants the precise exit there -- the `0xb6` arm already does for
/// such a pc (under [`ir_splice_unrecorded_call_exit_enabled`]), the `0xb7`
/// arm does under this switch. Read per site while building.
pub fn ir_splice_never_run_private_exit_enabled() -> bool {
    false
}

pub fn ir_splice_unrecorded_call_exit_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_SPLICE_LEAF_PAST_DEPTH`, **default ON** (`=0` restores
/// the hard cap): one more nesting level than
/// [`crate::MAX_IR_INLINE_NEST_DEPTH`] for a static LEAF callee
/// ([`ir_leaf_splice_body`]) -- `PlatformDependent.hasUnsafe()` under
/// `AsciiString.byteAt` under a `CharSequence.charAt` six splices deep in
/// `HttpHeaderValidationUtilTest`'s name loop, a full compiled call per
/// character otherwise. A leaf nests nothing itself, so the depth stays
/// bounded. Read per resolved body.
pub fn ir_splice_leaf_past_depth_enabled() -> bool {
    true
}

/// A body [`ir_splice_leaf_past_depth_enabled`] may splice past the nesting
/// cap: at most 16 bytes, and no invoke, allocation, `athrow`, monitor or
/// switch in it.
pub fn ir_leaf_splice_body(code: &[u8], code_len: usize) -> bool {
    let len = code_len.min(code.len());
    if len == 0 || len > 16 {
        return false;
    }
    let mut pc = 0usize;
    while pc < len {
        let op = code[pc];
        if matches!(op, 0xaa | 0xab | 0xb6..=0xbf | 0xc2 | 0xc3 | 0xc4 | 0xc5) {
            return false;
        }
        let step = crate::bytecode_analysis::step(code, pc);
        if step == 0 {
            return false;
        }
        pc += step;
    }
    true
}

/// `CRATONVM_JIT_IR_OSR_PROFILED_SLOT_CHECK`, **default ON** (`=0` off): see
/// [`IrBuilder::set_osr_profiled_slot_classes`]. Read per OSR build.
pub fn ir_osr_profiled_slot_check_enabled() -> bool {
    true
}

pub fn ir_spliced_throw_edges_enabled() -> bool {
    true
}

thread_local! {
    /// `CRATONVM_JIT_IR_SPLICED_THROW_EDGES`: the compiling method's call site
    /// being resolved lies in a `try` whose handlers all follow it. Set by
    /// the planner around each top-level resolution (`lib.rs`), read by the
    /// VM's resolver at every depth of that resolution.
    pub(in crate::ir) static IR_ROOT_SITE_IN_TRY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// See [`ir_root_site_in_try`].
pub fn set_ir_root_site_in_try(in_try: bool) {
    IR_ROOT_SITE_IN_TRY.with(|c| c.set(in_try));
}

/// Is the top-level call site the planner is resolving now covered by a
/// `try` of the compiling method whose handlers all follow it
/// (`CRATONVM_JIT_IR_SPLICED_THROW_EDGES`)? Only there may a spliced body
/// throw by a compiled edge, so only there does the VM's resolver admit a
/// throwing body it would otherwise mask or refuse.
pub fn ir_root_site_in_try() -> bool {
    ir_spliced_throw_edges_enabled() && IR_ROOT_SITE_IN_TRY.with(|c| c.get())
}

pub fn ir_local_throw_edges_enabled() -> bool {
    true
}

/// JIT round 20 wave 11 (lane pIR; books `jit-r21-nio-proposals-w1-L2.md`
/// L2-3 and `-w2-L2.md` L2W2-4): the local throw edges also decide a typed
/// catch row whose class is a loaded SUPERCLASS of the thrown class (or skip
/// one that certainly is not), from the VM's subclass answers
/// (`CompileRequest::class_is_subclass_resolver`). Default ON;
/// `CRATONVM_JIT_IR_LOCAL_THROW_SUPERCLASS_CATCH=0` decides only an exact class
/// or a catch-all, as before.
pub fn ir_local_throw_superclass_catch_enabled() -> bool {
    true
}

/// The catch class of one exception-table row, as the compiled local throw
/// edges see it (round nio21 wave 1, lane L2).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IrCatchClass {
    /// `catch_type == 0`: a catch-all (`finally`, a `synchronized` region's
    /// release handler).
    Any,
    /// The class id the method's own constant pool resolves the catch type to
    /// (through its own loader), already loaded at compile time.
    Class(u32),
    /// Not resolvable at compile time: no edge is decided against this row.
    Unknown,
}

/// One exception-table row for [`IrBuilder::set_local_throw_rows`], in the
/// method's own bytecode pcs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IrLocalThrowRow {
    pub start_pc: usize,
    pub end_pc: usize,
    pub handler_pc: usize,
    pub catch: IrCatchClass,
}

/// The handler pcs a normally reachable `athrow` of `code[..code_len]` may
/// branch to under the compiled local throw edges: for each such `athrow`, the
/// handler of the FIRST row covering it, when that row's class is decidable
/// and the handler lies after the `athrow`. See
/// [`IrBuilder::plan_local_throw_reachability`].
/// `CRATONVM_JIT_IR_SPLICED_THROW_EDGES`: the handler of the FIRST covering
/// row (JVMS 2.10 order) of every normally reachable `invoke` of the
/// compiling method's own code that the build may splice (`sites`), when the
/// row's class is decidable and the handler lies after the `invoke` (the pc
/// walk enters a forward merge only). A candidate no spliced `athrow` enters
/// is stepped over (`local_throw_optional`).
pub(in crate::ir) fn spliced_throw_candidate_handlers(
    verified: &cratonvm_reader::VerifiedCode,
    reachable: &HashSet<usize>,
    code_len: usize,
    rows: &[IrLocalThrowRow],
    sites: &HashMap<usize, IrInlineSite>,
) -> HashSet<usize> {
    let mut out = HashSet::new();
    for insn in verified.instructions() {
        let pc = insn.pc as usize;
        if pc >= code_len || !reachable.contains(&pc) || !sites.contains_key(&pc) {
            continue;
        }
        let Some(row) = rows.iter().find(|r| r.start_pc <= pc && pc < r.end_pc) else {
            continue;
        };
        if matches!(row.catch, IrCatchClass::Unknown)
            || row.handler_pc <= pc
            || row.handler_pc >= code_len
        {
            continue;
        }
        out.insert(row.handler_pc);
    }
    out
}

pub(in crate::ir) fn local_throw_candidate_handlers(
    verified: &cratonvm_reader::VerifiedCode,
    reachable: &HashSet<usize>,
    code: &[u8],
    code_len: usize,
    rows: &[IrLocalThrowRow],
) -> HashSet<usize> {
    let mut out = HashSet::new();
    for insn in verified.instructions() {
        let pc = insn.pc as usize;
        if pc >= code_len || code.get(pc) != Some(&0xbf) || !reachable.contains(&pc) {
            continue;
        }
        let Some(row) = rows.iter().find(|r| r.start_pc <= pc && pc < r.end_pc) else {
            continue;
        };
        if row.catch != IrCatchClass::Unknown && row.handler_pc > pc && row.handler_pc < code_len {
            out.insert(row.handler_pc);
        }
    }
    out
}

/// JIT round 20 wave 11 (lane pIR, L2-3 / L2W2-4): every handler a normally
/// reachable `athrow` of the caller's own code may reach once typed rows the
/// VM answers "certainly not caught" are skipped: the handlers of its covering
/// rows in table order, up to and including the first catch-all, and stopping
/// BEFORE the first `Unknown` row (the search may stop there at run time).
/// Only handlers after the `athrow` qualify, as in
/// [`local_throw_candidate_handlers`]. A superset of what any one throw can
/// enter; [`IrBuilder::local_throw_target`] decides each site.
pub(in crate::ir) fn local_throw_later_row_handlers(
    verified: &cratonvm_reader::VerifiedCode,
    reachable: &HashSet<usize>,
    code: &[u8],
    code_len: usize,
    rows: &[IrLocalThrowRow],
) -> HashSet<usize> {
    let mut out = HashSet::new();
    for insn in verified.instructions() {
        let pc = insn.pc as usize;
        if pc >= code_len || code.get(pc) != Some(&0xbf) || !reachable.contains(&pc) {
            continue;
        }
        for row in rows.iter().filter(|r| r.start_pc <= pc && pc < r.end_pc) {
            if row.catch == IrCatchClass::Unknown {
                break;
            }
            if row.handler_pc > pc && row.handler_pc < code_len {
                out.insert(row.handler_pc);
            }
            if row.catch == IrCatchClass::Any {
                break;
            }
        }
    }
    out
}

/// The reachable set `base` widened by the handler code `candidates` may
/// enter, and the pcs that widening added (the "optional" handler code), or
/// `None` when [`IrBuilder::plan_local_throw_reachability`]'s refusals hold:
/// no candidate, a candidate already normally reachable, a loop header inside
/// the added code, or a rotated loop with or without it. The wave-1 body of
/// that function, lifted out so the call-site edges (nio21 wave 3 L2) can ask
/// it twice.
pub(in crate::ir) fn widen_for_local_handlers(
    verified: &cratonvm_reader::VerifiedCode,
    base: &HashSet<usize>,
    candidates: &HashSet<usize>,
    code_len: usize,
) -> Option<(HashSet<usize>, HashSet<usize>)> {
    if candidates.is_empty() || candidates.iter().any(|h| base.contains(h)) {
        return None;
    }
    let mut extended = base.clone();
    let mut work: Vec<usize> = Vec::new();
    for &h in candidates {
        if h < code_len && extended.insert(h) {
            work.push(h);
        }
    }
    while let Some(pc) = work.pop() {
        for succ in verified.successors(pc) {
            if succ < code_len && extended.insert(succ) {
                work.push(succ);
            }
        }
    }
    let optional: HashSet<usize> = extended.difference(base).copied().collect();
    let all_headers: HashSet<usize> = verified
        .loop_headers()
        .iter()
        .map(|&h| h as usize)
        .collect();
    if all_headers.iter().any(|h| optional.contains(h)) {
        return None;
    }
    let base_headers: HashSet<usize> = all_headers
        .iter()
        .copied()
        .filter(|h| base.contains(h))
        .collect();
    let extended_headers: HashSet<usize> = all_headers
        .iter()
        .copied()
        .filter(|h| extended.contains(h))
        .collect();
    if has_rotated_loop_header(verified, base, &base_headers, code_len)
        || has_rotated_loop_header(verified, &extended, &extended_headers, code_len)
    {
        return None;
    }
    Some((extended, optional))
}

/// Call-site exception edges in the optimizing tier (round nio21 wave 3, lane
/// L2; `CRATONVM_JIT_IR_CALL_SITE_EXCEPTION_EDGES`, **default ON** since JIT
/// r24 with the rest of the CompletableFuture handler family, `=0` off).
///
/// With it on (and [`ir_local_throw_edges_enabled`], whose rows it reads), an
/// `invoke*` inside a `try` whose handlers all lie after it branches to the
/// handler of the same compiled body when the callee throws, instead of
/// leaving the frame for the interpreter's handler search (the reason-9 pad or
/// the shared sentinel exit). See [`Op::CallThrew`] for the graph and
/// `ir_lower::Lowerer::emit_call_throw_pads` for the runtime half, which is
/// `jit_local_handler_lookup` -- the single-pass tier's matcher, JVMS 2.10
/// order, subclass catch types included. A throwable no candidate takes, a
/// deopt, a JVMTI exception client armed (the helper declines), or any
/// lowering shape the pad cannot reproduce keeps today's exit.
///
/// Read once per method-entry compile in `lib.rs` (which hands the builder the
/// catch-type names only when it is on) and once per lowering.
pub fn ir_call_site_exception_edges_enabled() -> bool {
    true
}

/// The most exception-table rows one call site's edges cover (nio21 wave 3
/// L2). Past it the site keeps today's exit: the pad tests candidates with one
/// `CMP`/`JE` each, the single-pass tier's own bound.
pub const IR_CALL_THROW_MAX_CANDIDATES: usize = 8;

/// One call site with exception edges (nio21 wave 3, lane L2): what the
/// lowerer turns into the `JitLocalHandlerSite` its pad hands
/// `jit_local_handler_lookup`. `candidates` are the rows covering `bci`, in
/// table order, as `(catch-type name, handler bci)` with `""` for a
/// catch-all; index `k` is [`Op::CallThrew`]`{ index: k }`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IrCallThrowSite {
    pub bci: u32,
    pub declaring_class_id: u32,
    pub candidates: Vec<(&'static str, u32)>,
    /// nio21 wave 4 L2: the call sits inside a spliced callee body and the
    /// candidates are THAT body's rows ([`IrSpliceHandlerRow`]); `bci` is a
    /// combined-buffer pc. Such a site must be armed (an unarmed one would
    /// leave through the caller's `invoke` and skip the callee's `catch`), and
    /// its helper answer must not decline for a JVMTI client
    /// (`JitLocalHandlerSite::SPLICED_SCOPE_BIT`).
    pub spliced: bool,
}

/// `CRATONVM_JIT_IR_SPLICE_CALLEE_HANDLERS` (round nio21 wave 4, lane L2;
/// **default ON** since JIT r24, `=0` off): the optimizing tier's inline resolver admits a
/// callee whose exception table has real handlers
/// ([`splice_callee_handler_plan`]), and the builder covers the callee's
/// `try` ranges with the call-site exception edges of [`Op::CallThrew`]: a
/// spliced call inside such a range asks `jit_local_handler_lookup` with the
/// CALLEE's rows, and a match enters the callee's handler, which deopts there
/// through the scope chain (the interpreter runs the handler). Needs
/// [`ir_call_site_exception_edges_enabled`], [`ir_local_throw_edges_enabled`]
/// and the splice chain fences; `lib.rs` refuses the site otherwise. See
/// `docs/feature-designs/deopt-osr.md`.
pub fn ir_splice_callee_handlers_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_SPLICE_COLD_BRANCH_BOTH_ARMS` (JIT round 23 wave 10, lane
/// L1, CompletableFuture page; perf, default ON, `=0` restores the old route):
/// a spliced branch whose fall-through is a cold throw arm and whose jump
/// enters a masked tail cold throw arm (both ways out throw) becomes an
/// always-failing guard at the branch, instead of a guard that sends live
/// control into the masked tail arm (whose `null` athrow refused the build:
/// `getNow` -> `reportJoin`'s `ifeq 53`). Sound for the same reason as every
/// cold-arm trap: the interpreter re-executes the branch through the chain
/// snapshot and runs the arm it takes. Acts only under the opt-in
/// [`ir_splice_athrow_exit_enabled`], so the default build is unchanged.
/// Read per such branch.
pub fn ir_splice_cold_branch_both_arms_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_SPLICE_HANDLER_TRAP_THROW` (JIT round 23 wave 10, lane
/// L1, CompletableFuture page; correctness, default ON, `=0` builds the bare
/// guard as before): `IrBuilder::plant_splice_handler_trap` ends the caught
/// path's block with an unreachable `Op::Throw` behind its always-failing
/// guard, the terminator `ir_lower` requires of every reachable block. Acts
/// only under [`ir_splice_callee_handlers_enabled`]. Read per handler entry.
pub fn ir_splice_handler_trap_throw_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_SPLICE_HANDLER_NON_NULL_FIELD_ACCESS` (JIT r24,
/// CompletableFuture page; perf, default ON, `=0` refuses every field access
/// in a covered range as before): `IrBuilder::splice_handler_nodes_armed`
/// admits a field `Op::Load` / `Op::Store` whose base is proven non-null
/// ([`IrBuilder::splice_handler_base_non_null`]) inside a spliced callee's
/// `try` range. Such an access has no exception to deliver. Acts only under
/// [`ir_splice_callee_handlers_enabled`]. Read per covered instruction.
pub fn ir_splice_covered_nested_pure_enabled() -> bool {
    true
}

/// See [`ir_splice_covered_nested_pure_enabled`]'s neighbour: the
/// non-null-field admission of [`IrBuilder::splice_handler_nodes_armed`].
pub fn ir_splice_handler_non_null_field_access_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_OSR_SPLICE_CALLEE_HANDLERS` (JIT round 23 wave 3c, lane L1,
/// L2W3-1 of `docs/feature-designs/jit-ir-optimizations.md`; perf,
/// **default ON**, `=0` kill): a spliced tree with handler rows
/// ([`ir_splice_callee_handlers_enabled`]) is admitted on the optimizing OSR
/// route as well, not only in a method-entry compile.
/// - The planner's other gates still hold: call-site edges, local throw edges,
///   the chain fences, no debugger, no armed exception event.
/// - It also needs the OSR walk to carry the spliced throw rows
///   ([`ir_spliced_throw_edges_enabled`]), the gate that already hands the OSR
///   route the method's own rows. Its argument: the OSR walk is the method-entry
///   walk plus an entry arm, so the planner's rules hold.
///
/// A handler entry inside the splice is code of the body, reached by its own
/// edges, so an OSR body enters it exactly as a method-entry body does. A body
/// whose exits the OSR door cannot resume is refused by the door, as any other
/// would be. This changes nothing unless the opt-in handler switches are on.
/// Read per site.
pub fn ir_osr_splice_callee_handlers_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_SPLICE_ATHROW_EXIT` (JIT round 23 wave 5, lane L1; perf,
/// **default ON** since R23-W11-FLIP, `=0` restores the old refusal): see
/// `IrBuilder::plant_spliced_athrow_exit`. n23-w11b A/B, `R21CfChain`
/// `--real-jdk` medians of 3: alone, completed-future 21 ns against 29, and
/// `CfLoop done` 122 against 146; with the other three switches of the CF
/// family (`ir-splice-prune-low-floor`, `ir-splice-cold-nested-hot-path`,
/// `ir-splice-callee-handler-traps`), completed-future 17, thenapply-done 50
/// against 55, and `CfLoop done` 103 against 146. Read per spliced `athrow`.
pub fn ir_splice_athrow_exit_enabled() -> bool {
    true
}

/// The OSR conjunct of the planner's spliced-handler gate (`lib.rs`,
/// `ir-splice-callee-handlers-not-covered`): a method-entry compile always
/// passes it; an OSR compile (`osr_compile`: the OSR door's mode-exit polls,
/// the optimizing OSR route, or a requested OSR header) only under
/// [`ir_osr_splice_callee_handlers_enabled`] AND
/// [`ir_spliced_throw_edges_enabled`] (without which the OSR walk is never
/// handed the throw rows, `plan_local_throw_reachability`). The other
/// conjuncts of that gate stay at the call site. r23 wave 3 review: a
/// function so the rule is tested by behaviour, not by text.
pub fn ir_splice_callee_handlers_admit_compile(osr_compile: bool) -> bool {
    !osr_compile || (ir_osr_splice_callee_handlers_enabled() && ir_spliced_throw_edges_enabled())
}

/// `CRATONVM_JIT_IR_SPLICE_HANDLERS_POSTING_MODE` (JIT round 23 wave 4, lane
/// L1; perf, **default ON** since JIT r24, `=0` restores the wide refusal):
/// the JVMTI conjunct of the planner's spliced-handler gate.
///
/// The gate refuses on `CompileRequest::exception_events_armed_at_compile`.
/// Since interpreter round i4 wave 92 that field is also `true` whenever the VM
/// merely SERVES `Exception` events from compiled code
/// (`CRATONVM_JVMTI_EXCEPTION_FROM_COMPILED`, default ON), and every VM
/// installs its event manager at boot. So every production compile refuses
/// every spliced tree with handler rows (`ir-splice-callee-handlers-not-covered`),
/// whatever the opt-in handler family says. That is why the family's A/B was
/// flat: on `R21CfChain` with n23-w3 (2026-10-21) the refusal disappears only
/// under `CRATONVM_JVMTI_EXCEPTION_FROM_COMPILED=0`.
///
/// Under this switch only a client armed NOW refuses (`listener_armed`,
/// `CompileRequest::exception_listener_armed_at_compile`); that was the
/// nio21 wave 4 rule. A client armed AFTER the compile meets a spliced site
/// that `jit_local_handler_lookup` must answer
/// (`JitLocalHandlerSite::SPLICED_SCOPE_BIT`); that catch used to post no
/// `ExceptionCatch`, which kept this switch opt-in. JIT r24 closed the window:
/// the site's handler entry resumes the interpreter through an inlined chain,
/// and the chain push posts the catch on the callee frame
/// (vm `interpreter::report_spliced_catch_if_pending`,
/// `CRATONVM_JIT_SPLICED_CATCH_REPORT`; probe
/// `R24CfCatchLateArm.java`).
/// `armed_or_posting` is the wide field. Read per site.
pub fn ir_splice_callee_handlers_admit_events(armed_or_posting: bool, listener_armed: bool) -> bool {
    {
        !listener_armed
    }
}

/// One exception-table row of a spliced callee admitted under
/// [`ir_splice_callee_handlers_enabled`] (nio21 wave 4, lane L2), rebased to
/// combined-buffer pcs. `catch_name` is never empty (a catch-all is refused)
/// and is resolved, at run time, through `declaring_class_id` -- the class
/// that DECLARES the callee body, whose constant pool named it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IrSpliceHandlerRow {
    pub start_pc: usize,
    pub end_pc: usize,
    pub handler_pc: usize,
    pub catch_name: &'static str,
    pub declaring_class_id: u32,
    pub bare_athrow: bool,
}

pub fn splice_handler_is_bare_athrow(code: &[u8], handler_pc: usize) -> bool {
    splice_covered_nested::handler_is_bare_athrow(code, handler_pc)
}

/// What [`splice_callee_handler_plan`] admits: the copy to splice (every
/// handler's code masked to `nop`s) and each handler's `[start, end)` run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpliceCalleeHandlerPlan {
    pub code: Vec<u8>,
    pub runs: Vec<(usize, usize)>,
}

/// May an instruction inside a spliced callee's `try` range be built while
/// the range's handlers are only reachable from call-site edges? Only opcodes
/// that cannot leave the frame with an exception (a guard's deopt is fine: the
/// interpreter re-runs the instruction in the callee's own frame, table and
/// all), plus the three invokes whose exits the edges cover. Anything that
/// raises through an exit of its own -- a field or array access, a division,
/// `new`, `athrow`, a type check, a monitor, `ldc`, `invokespecial` -- would
/// leave through the CALLER's `invoke` and skip the callee's `catch`. (An
/// integer division is admitted beside this list since nio21 wave 12, by
/// [`splice_handler_range_division_admitted`].)
pub(in crate::ir) fn splice_handler_range_opcode_admitted(op: u8) -> bool {
    matches!(
        op,
        0x00..=0x11 // nop, constants, bipush, sipush
            | 0x15..=0x2d // loads of locals
            | 0x36..=0x4e // stores to locals
            | 0x57..=0x5f // stack ops
            | 0x60..=0x6b // add / sub / mul
            | 0x74..=0x84 // neg, shifts, and / or / xor, iinc
            | 0x85..=0x98 // conversions, compares
            | 0x99..=0xa7 // conditional branches, goto
            | 0xc6
            | 0xc7 // ifnull / ifnonnull
            | 0xb6
            | 0xb8
            | 0xb9 // invokevirtual / invokestatic / invokeinterface
    )
}

/// JIT round 21 wave 3 (lane keepset): the opcodes
/// [`splice_callee_handler_plan_with`]'s `wide` form admits in a covered
/// range beyond [`splice_handler_range_opcode_admitted`]: normal returns,
/// array loads, `arraylength`, `new`, `putfield`, `checkcast` and `invokespecial`. A `getstatic`
/// is admitted only under [`ir_splice_handler_lower_proven_enabled`] and only
/// when the builder and lowering agree that the class is initialized and the
/// read is direct. Returns cannot throw into the handler. Array loads get
/// null / bounds check deopts into that callee if no covered edge is armed.
/// `arraylength` likewise deopts at its null check, where the callee's table
/// is preserved. Other exits are checked node by node by the lowering's
/// covered-range rule (`Graph::splice_handler_covered`), which refuses the
/// compile when one is not a guard or an armed edge.
pub(in crate::ir) fn splice_handler_wide_range_opcode_admitted(op: u8) -> bool {
    matches!(op, 0x2e..=0x35 | 0x4f..=0x56 | 0xac..=0xb1 | 0xbe | 0xbb | 0xbc | 0xbd | 0xb4 | 0xb5 | 0xc0 | 0xb7)
        || (op == 0xb2 && ir_splice_handler_lower_proven_enabled())
}

/// nio21 wave 12 (lane L4, W10L2-3): may the integer division at `pc` sit in
/// a spliced callee's `try` range? It has no exit of its own when its divisor
/// is a non-zero constant ([`splice_division_cannot_trap`]: no guard, no
/// trap), and when it is not, its zero guard deopts -- through the chain into
/// the callee's own frame, or by re-running the whole call -- so the
/// interpreter raises `ArithmeticException` with the callee's table in force:
/// the "a guard's deopt is fine" clause of
/// [`splice_handler_range_opcode_admitted`], admitted while that guard may
/// stand inside a splice ([`ir_splice_div_guard_enabled`]). Both under
/// [`ir_splice_screens_const_division_enabled`]. No `ldc` divisor here (the
/// plan has no constant-pool rows): such a division is admitted only through
/// the guard.
pub(in crate::ir) fn splice_handler_range_division_admitted(code: &[u8], code_len: usize, pc: usize) -> bool {
    matches!(code.get(pc), Some(0x6c | 0x6d | 0x70 | 0x71))
        && ir_splice_screens_const_division_enabled()
        && (ir_splice_div_guard_enabled()
            || splice_division_cannot_trap(code, code_len, pc, |_| None))
}

/// The local a load (`Some((slot, false))`) or store (`Some((slot, true))`)
/// of `code` at `pc` names; `iinc` is a load. `None` for anything else.
pub(in crate::ir) fn splice_handler_local_access(code: &[u8], pc: usize) -> Option<(usize, bool)> {
    let op = *code.get(pc)?;
    match op {
        0x15..=0x19 => Some((usize::from(*code.get(pc + 1)?), false)),
        0x1a..=0x2d => Some((usize::from((op - 0x1a) % 4), false)),
        0x36..=0x3a => Some((usize::from(*code.get(pc + 1)?), true)),
        0x3b..=0x4e => Some((usize::from((op - 0x3b) % 4), true)),
        0x84 => Some((usize::from(*code.get(pc + 1)?), false)),
        _ => None,
    }
}

/// `CRATONVM_JIT_IR_SPLICE_CALLEE_HANDLERS_WIDE` (JIT round 21 wave 3, lane
/// keepset; item 3 of the wave-3 section of
/// `docs/feature-designs/deopt-osr.md`; **default ON**
/// since JIT r24, `=0` off, and only on top of [`ir_splice_callee_handlers_enabled`]): the
/// widened admission of [`splice_callee_handler_plan_with`] (`wide == true`)
/// and, in the VM's resolver, a `Throwable` / `Error` catch type. Read per
/// splice decision; no static.
pub fn ir_splice_callee_handlers_wide_enabled() -> bool {
    ir_splice_callee_handlers_enabled()
}

/// `CRATONVM_JIT_IR_SPLICE_CALLEE_HANDLER_TRAPS` (JIT round 23 wave 11, lane
/// L5, CompletableFuture page; perf, **opt-in** `=1`). One switch for a
/// spliced callee whose exception table guards only calls that the splice
/// keeps as calls, such as `CompletableFuture.uniApplyNow`'s
/// `try { f.apply(..) } catch (Throwable)`:
/// - the resolver admits the table, in its widened shape
///   ([`ir_splice_callee_handlers_wide_admitted`]);
/// - every nested row inside a covered `try` stays its call
///   (`lib.rs` `ir_splice_strip_handler_covered_nested`);
/// - each kept call gets the callee's rows as call-site edges
///   ([`Op::CallThrew`] with `spliced`, `jit_local_handler_lookup`);
/// - a taken edge enters the masked handler, which is the wave-10 trap: an
///   always-failing guard plus an unreachable `Op::Throw`
///   (`IrBuilder::plant_splice_handler_trap`,
///   [`ir_splice_handler_trap_throw_enabled`]). The interpreter resumes in
///   the callee at the handler bci with the throwable, through the chain
///   snapshot, and runs the handler with the method's own code.
///
/// It differs from the family it reuses
/// ([`ir_splice_callee_handlers_enabled`], `_WIDE`,
/// [`ir_call_site_exception_edges_enabled`],
/// `CRATONVM_JIT_IR_SPLICE_HANDLERS_POSTING_MODE`) in two ways:
/// - it does not turn on the compiling method's OWN call-site edges. The
///   spliced edges read only the callee's rows (`IrSpliceHandlerRow`), so
///   they need no method-level switch;
/// - its JVMTI conjunct is the posting-mode rule: only an exception listener
///   armed at compile refuses ([`ir_splice_callee_handler_events_admitted`]).
///   The documented cost is posting mode's: a client armed after the compile
///   does not see the `ExceptionCatch` of a throwable caught through such a
///   handler entry.
///
/// Every other conjunct of the planner gate still holds (local throw edges,
/// chain fences, the OSR conjunct, no debugger observing locals), as does
/// the lowering's covered-range check (`Graph::splice_handler_covered`).
/// **Default ON** since R23-W11-FLIP (`=0` turns it off), with the CF family; see
/// [`ir_splice_athrow_exit_enabled`] for the n23-w11b A/B.
/// Read per resolution and per planned site; no static.
pub fn ir_splice_callee_handler_traps_enabled() -> bool {
    true
}

/// May the VM resolver admit a callee with real handlers? The family switch
/// or the wave-11 trap switch ([`ir_splice_callee_handler_traps_enabled`]).
pub fn ir_splice_callee_handlers_admitted() -> bool {
    ir_splice_callee_handlers_enabled() || ir_splice_callee_handler_traps_enabled()
}

/// The resolver's widened admission (`Throwable` / `Error` catch types and
/// the `uniApplyNow` body shape, [`splice_callee_handler_plan_with`]): the
/// family's `_WIDE`, or the wave-11 trap switch.
pub fn ir_splice_callee_handlers_wide_admitted() -> bool {
    ir_splice_callee_handlers_wide_enabled() || ir_splice_callee_handler_traps_enabled()
}

/// The planner gate's edge conjunct for a spliced tree with handler rows
/// (`lib.rs`, `ir-splice-callee-handlers-not-covered`). Under the family, its
/// own switch and the method-level call-site edges; under the wave-11 trap
/// switch, the spliced edges alone.
pub fn ir_splice_callee_handler_edges_admitted() -> bool {
    (ir_splice_callee_handlers_enabled() && ir_call_site_exception_edges_enabled())
        || ir_splice_callee_handler_traps_enabled()
}

/// The planner gate's JVMTI conjunct: under the wave-11 trap switch, the
/// posting-mode rule (only a listener armed now refuses); otherwise
/// [`ir_splice_callee_handlers_admit_events`], unchanged.
pub fn ir_splice_callee_handler_events_admitted(
    armed_or_posting: bool,
    listener_armed: bool,
) -> bool {
    if ir_splice_callee_handler_traps_enabled() {
        !listener_armed
    } else {
        ir_splice_callee_handlers_admit_events(armed_or_posting, listener_armed)
    }
}

/// `CRATONVM_JIT_IR_SPLICE_TAIL_HANDLER` (j27 covered-nested retirement):
/// default ON, `=0` restores the legacy refusal. Admit and mask a handler
/// ending at the body's final `athrow`, and walk its trap after the try's
/// return. Covered constructors now stay spliced, so ByteBuffer.wrap's tail
/// handler no longer strands its constructor/setter chain behind a call.
pub fn ir_splice_tail_handler_enabled() -> bool {
    true
}

/// A verified short backward goto at the end of a spliced body. The walk must
/// retain internal returns until this latch has supplied the loop's back edge.
pub fn splice_ends_in_backward_goto(code: &[u8], code_len: usize) -> bool {
    let Some(code) = code.get(..code_len) else {
        return false;
    };
    let Some(pc) = code_len.checked_sub(3) else {
        return false;
    };
    code[pc] == 0xa7
        && i16::from_be_bytes([code[pc + 1], code[pc + 2]]) < 0
        && cratonvm_reader::verified_code(code).ok().is_some_and(|v| {
            v.instruction_at(pc).is_some_and(|i| i.next_pc as usize == code_len)
        })
}

/// May a handler run of [`splice_callee_handler_plan_with`] end at
/// `run_end`? A run ending in a `goto` may end anywhere inside the body. A run
/// ending in its `athrow` (or, `wide`, falling into the join) must be followed
/// by a normally reachable pc, or end the body
/// ([`ir_splice_tail_handler_enabled`]).
/// Does a masked handler run of [`splice_callee_handler_plan_with`] start
/// right after the body's last `return` (at `last_return_pc`) and run to the
/// end of the body ([`ir_splice_tail_handler_enabled`])? Both halves of the
/// splice ask it of the same masked copy: the VM resolver, instead of its
/// `ir-splice-not-single-trailing-return` refusal, and the builder, which then
/// walks the body as a multi-return one so the walk goes on past the return
/// to the handler's trap (`IrBuilder::plant_splice_handler_trap`) instead of
/// leaving the body there. `handlers` are the rows' handler pcs, body-relative.
pub fn splice_tail_handler_follows_return(
    code: &[u8],
    code_len: usize,
    last_return_pc: usize,
    handlers: impl IntoIterator<Item = usize>,
) -> bool {
    let start = last_return_pc + 1;
    start < code_len
        && ir_splice_tail_handler_enabled()
        && handlers.into_iter().any(|h| h == start)
        && code.get(start..code_len).is_some_and(|run| run.iter().all(|&b| b == 0))
}

pub(in crate::ir) fn splice_handler_run_end_admitted(
    run_end: usize,
    code_len: usize,
    ends_in_goto: bool,
    reachable: &HashSet<usize>,
) -> bool {
    if ends_in_goto || run_end == code_len {
        return run_end <= code_len && (ends_in_goto || ir_splice_tail_handler_enabled());
    }
    run_end < code_len && reachable.contains(&run_end)
}

/// [`splice_callee_handler_plan`], widened under `wide`
/// ([`ir_splice_callee_handlers_wide_enabled`]) to the
/// `CompletableFuture.uniApplyNow` shape:
///
/// * the `try` range may also hold `new`, `putfield`, `checkcast` and
///   `invokespecial` (the `new X(..)` pair). Their exits inside a covered
///   range must be guards or deopts into the callee, which the lowering
///   checks per node (`Graph::splice_handler_covered`: anything else refuses
///   the compile and rebuilds without the splice);
/// * the handler RUNS code and may read locals stored before the `try`
///   (`d`). An entered handler deopts at its first instruction and the
///   interpreter runs it, so its code is masked and never built; the reads
///   are kept alive by the chain-snapshot liveness, which models the
///   handler edges on the original body (`IrInlineFrameSites::handler_rows`,
///   `ea_ir_bridge::ir_prune_dead_snapshot_locals_scoped`), and by the keep
///   set and the colouring, which read those snapshots at every covered pc
///   and the handler entry's at the handler;
/// * its run ends at its `athrow` or return, at a `goto` to a normally reachable pc,
///   or by falling into a normally reachable pc (the join after the `try`).
///   Still straight-line otherwise: no other branch, switch,
///   subroutine or `wide` form inside it.
pub fn splice_callee_handler_plan_with(
    code: &[u8],
    code_len: usize,
    rows: &[(usize, usize, usize)],
    wide: bool,
) -> Option<SpliceCalleeHandlerPlan> {
    if rows.is_empty() || rows.len() > IR_CALL_THROW_MAX_CANDIDATES || code_len == 0 {
        return None;
    }
    let body = code.get(..code_len)?;
    let verified = cratonvm_reader::verified_code(body).ok()?;
    let reachable = normally_reachable_pcs(&verified, code_len);
    let mut runs: Vec<(usize, usize)> = Vec::new();
    for &(start, end, handler) in rows {
        if start >= end || end > code_len || handler < end || handler >= code_len {
            return None;
        }
        // The range: whole instructions, all normally reachable, all admitted.
        let mut pc = start;
        while pc < end {
            let insn = verified.instruction_at(pc)?;
            let op = *body.get(pc)?;
            if !reachable.contains(&pc)
                || !(splice_handler_range_opcode_admitted(op)
                    || splice_handler_range_division_admitted(body, code_len, pc)
                    || (wide && splice_handler_wide_range_opcode_admitted(op)))
            {
                return None;
            }
            pc = insn.next_pc as usize;
        }
        if pc != end {
            return None;
        }
        if runs.iter().any(|&(h, _)| h == handler) {
            continue;
        }
        // The handler's run.
        verified.instruction_at(handler)?;
        if reachable.contains(&handler) {
            return None;
        }
        if wide {
            let mut work = vec![handler];
            let mut seen_h = HashSet::new();
            let mut intervals: Vec<(usize, usize)> = Vec::new();
            while let Some(mut pc) = work.pop() {
                if !seen_h.insert(pc) {
                    continue;
                }
                let block_start = pc;
                let mut ends_in_goto = false;
                let block_end = loop {
                    if pc != handler && pc < code_len && reachable.contains(&pc) {
                        break pc;
                    }
                    if pc >= code_len || reachable.contains(&pc) {
                        return None;
                    }
                    let insn = verified.instruction_at(pc)?;
                    let op = *body.get(pc)?;
                    let next = insn.next_pc as usize;
                    let _ = splice_handler_local_access(body, pc);
                    if op == 0xbf || matches!(op, 0xac..=0xb1) {
                        break next;
                    }
                    if matches!(op, 0xa7 | 0xc8) {
                        let target = crate::bytecode_analysis::offset_branch_target(body, pc)?;
                        if !reachable.contains(&target) {
                            work.push(target);
                        } else {
                            ends_in_goto = true;
                        }
                        break next;
                    }
                    if matches!(op, 0x99..=0xa6 | 0xc6 | 0xc7) {
                        let target = crate::bytecode_analysis::offset_branch_target(body, pc)?;
                        if target < code_len && !reachable.contains(&target) {
                            work.push(target);
                        } else if !reachable.contains(&target) {
                            return None;
                        }
                        if next < code_len && !reachable.contains(&next) {
                            work.push(next);
                        } else if !reachable.contains(&next) {
                            return None;
                        }
                        ends_in_goto = true;
                        break next;
                    }
                    if matches!(op, 0xa8..=0xb1 | 0xc4 | 0xc5 | 0xc9) {
                        return None;
                    }
                    pc = next;
                };
                if block_end == code_len && !ends_in_goto && !ir_splice_tail_handler_enabled() {
                    return None;
                }
                if block_start < block_end {
                    intervals.push((block_start, block_end));
                }
            }
            intervals.sort_unstable();
            for (s, e) in intervals {
                if let Some(last) = runs.last_mut() {
                    if s <= last.1 {
                        last.1 = last.1.max(e);
                        continue;
                    }
                }
                runs.push((s, e));
            }
        } else {
            let mut stored: HashSet<usize> = HashSet::new();
            let mut pc = handler;
            let ends_in_goto = false;
            let run_end = loop {
                if pc >= code_len || reachable.contains(&pc) {
                    return None;
                }
                let insn = verified.instruction_at(pc)?;
                let op = *body.get(pc)?;
                let next = insn.next_pc as usize;
                if let Some((slot, is_store)) = splice_handler_local_access(body, pc) {
                    if is_store {
                        stored.insert(slot);
                    } else if !stored.contains(&slot) {
                        return None;
                    }
                }
                if op == 0xbf {
                    break next;
                }
                if matches!(op, 0x99..=0xb1 | 0xc4..=0xc9) {
                    return None;
                }
                pc = next;
            };
            if !splice_handler_run_end_admitted(run_end, code_len, ends_in_goto, &reachable) {
                return None;
            }
            runs.push((handler, run_end));
        }
    }
    // A run must not overlap a covered range (a handler inside another row's
    // `try`): its masked code is not what the range check above admitted.
    if runs
        .iter()
        .any(|&(h, e)| rows.iter().any(|&(s, re, _)| h < re && s < e))
    {
        return None;
    }
    let mut masked = body.to_vec();
    for &(h, e) in &runs {
        masked.get_mut(h..e)?.fill(0x00);
    }
    Some(SpliceCalleeHandlerPlan { code: masked, runs })
}

/// The invokes of `code[..code_len]` that may take call-site exception edges
/// (nio21 wave 3 L2), as `pc -> handler pcs` in table order: a normally
/// reachable `invoke{virtual,special,static,interface}` covered by at least
/// one row, at most [`IR_CALL_THROW_MAX_CANDIDATES`] of them, EVERY one with
/// its handler after the invoke (the walk enters a forward merge only).
/// All-or-nothing per site: the runtime matcher takes the first matching row
/// of the whole covering list, and once it has taken the throwable there is
/// no way back to the old exit, so a site whose list is cut short could enter
/// the wrong handler. The catch class is not consulted -- the runtime decides.
pub(in crate::ir) fn call_site_throw_plan(
    verified: &cratonvm_reader::VerifiedCode,
    reachable: &HashSet<usize>,
    code: &[u8],
    code_len: usize,
    rows: &[IrLocalThrowRow],
) -> HashMap<usize, Vec<usize>> {
    let mut out = HashMap::new();
    for insn in verified.instructions() {
        let pc = insn.pc as usize;
        if pc >= code_len || !reachable.contains(&pc) {
            continue;
        }
        if !matches!(code.get(pc).copied(), Some(0xb6 | 0xb7 | 0xb8 | 0xb9)) {
            continue;
        }
        let handlers: Vec<usize> = rows
            .iter()
            .filter(|r| r.start_pc <= pc && pc < r.end_pc)
            .map(|r| r.handler_pc)
            .collect();
        if handlers.is_empty()
            || handlers.len() > IR_CALL_THROW_MAX_CANDIDATES
            || handlers.iter().any(|&h| h <= pc || h >= code_len)
        {
            continue;
        }
        out.insert(pc, handlers);
    }
    out
}

/// IMPLICIT exception edges in the optimizing tier (round nio21 wave 5, lane
/// L2; `CRATONVM_JIT_IR_IMPLICIT_EXCEPTION_EDGES`, **default ON** since
/// WALKTHROW21-7, `=0` off).
///
/// With it on -- and the call-site edges ([`ir_call_site_exception_edges_enabled`]),
/// whose catch-type names `lib.rs` hands the builder only under that switch,
/// and the local throw edges ([`ir_local_throw_edges_enabled`]), whose rows
/// both read -- the null and bounds checks of an array access and the
/// zero-divisor check of `idiv`/`ldiv`/`irem`/`lrem` inside a `try` whose
/// handlers all lie after the instruction take a handler of the same compiled
/// body instead of deopting: the check's failing branch goes to a pad that
/// raises the implicit signal exactly as the single-pass tier's stubs do
/// (`jit_npe_with_action` / `jit_throw_aioobe` / `jit_throw_arithmetic`, so the
/// throwable is built by the same `materialize_implicit_signal` body, with the
/// JEP 358 message rebuilt from this method's bytecode and the frame snapshot
/// at the trap's bci) and asks `jit_local_handler_lookup`. A miss (no row
/// takes it, a JVMTI exception client armed, `CRATONVM_JIT_IMPLICIT_LOCAL_HANDLERS=0`)
/// leaves through the reason-9 exceptional frame the call-site edges' miss
/// uses, with the signal pending, and the interpreter's drain builds and
/// routes it at the same bci. See `Lowerer::emit_implicit_throw_pad`.
///
/// Default ON since WALKTHROW21-7 (`docs/feature-designs/jit-ir-optimizations.md`):
/// without the edges, an OSR body compiles an in-`try` implicit check as a
/// guard exit, so every throw leaves the body for the interpreter, which builds
/// the throwable in full (never a fast throw); arming OSR bodies alone is not
/// enough, because the method-entry body of the same method then deopts at
/// that guard on its first throw and is made not entrant, every call.
///
/// Read once per compile, by the planner (`plan_local_throw_reachability`);
/// the lowering only arms the `CallThrew` sites the plan left in the graph.
pub fn ir_implicit_exception_edges_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_IMPLICIT_EDGES_UNPROTECTED_MISS` -- JIT round 20 (lane
/// throw, `r21-ir-implicit-exceptions-inside-a-try-still-leave-the-frame-FIXED-20261012.md`),
/// default ON beneath the umbrella ([`ir_implicit_exception_edges_enabled`]),
/// `=0` restores the wave-5 rule.
///
/// An implicit site whose bci is NOT protected in this compile (no precise
/// exception frames: every handler of the method reads only parameters, the
/// RBC.6 complement) is armed too, and its pad's miss leaves through the
/// shared sentinel exit with the signal pending -- the exit the same compile's
/// unprotected call sites and helper-backed field NPEs already take, and the
/// one the single-pass tier's implicit-only arm (`handler_edges_implicit_only`)
/// uses: the interpreter's drain builds the throwable and routes it through
/// this method's table at the stamped bci with the params-only frame, which
/// is exact for that population. Before, these sites were refused ("no
/// reason-9 miss exit"): 88 of 90 on the m7 `Nio21ImplicitCatch` census.
pub fn ir_implicit_edges_unprotected_miss_enabled() -> bool {
    true
}

/// nio21 wave 19 (lane L3): what the `ThreadLocal.get()` expansion
/// (`IrBuilder::try_thread_local_get_intrinsic`) bakes, read from the VM
/// (`DirectHelperTable::thread_local_ir_layout_words`, the class ids and
/// field indices the VM's own thin helper reads).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ThreadLocalIrLayout {
    /// Byte offset of `JvmThread::java_thread_obj` ([`Op::CurrentThreadMirror`]).
    pub thread_obj_offset: u32,
    pub thread_local_class: u32,
    /// `ThreadLocal$SuppliedThreadLocal`, `0` = not admitted.
    pub supplied_class: u32,
    pub map_class: u32,
    pub entry_class: u32,
    pub thread_locals_index: u32,
    pub table_index: u32,
    pub hash_index: u32,
    pub referent_index: u32,
    pub value_index: u32,
    /// nio21 wave 20: `1 + compact displacement` of `Thread.threadLocals`,
    /// `ThreadLocalMap.table`, `ThreadLocal.threadLocalHashCode` and
    /// `Entry.value` (`0` = no row).
    pub thread_locals_row: u32,
    pub table_row: u32,
    pub hash_row: u32,
    pub value_row: u32,
    /// nio21 wave 20: `ThreadLocal.set` may be expanded (the JDK `set` runs
    /// no tracing code in this VM).
    pub set_admitted: bool,
    /// JIT round 23 wave 2f (lane L4, W19L3-1): `java/lang/InheritableThreadLocal`,
    /// `0` = an `InheritableThreadLocal` site is not expanded. Its `getMap`
    /// reads `Thread.inheritableThreadLocals` instead. Also the gate of tl21's
    /// trap form for a `ThreadLocal`-declared site whose profile names one
    /// inheritable-map class (`IrBuilder::tl_profiled_inheritable_receiver`).
    pub inheritable_class: u32,
    /// `Thread.inheritableThreadLocals`'s field index and `1 + compact
    /// displacement` row (`0` = none).
    pub inheritable_thread_locals_index: u32,
    pub inheritable_thread_locals_row: u32,
    /// TL321-5: `1 + compact displacement` of `Reference.referent` in
    /// `Entry`'s layout (`0` = none).
    pub referent_row: u32,
}

impl ThreadLocalIrLayout {
    /// The layout in the VM's words, or `None` for any word out of range or
    /// a zero where a class id or the thread offset must be.
    pub fn from_words(
        w: &[u64; crate::direct_helpers::THREAD_LOCAL_IR_LAYOUT_WORDS],
    ) -> Option<Self> {
        let u = |i: usize| u32::try_from(w[i]).ok();
        let l = ThreadLocalIrLayout {
            thread_obj_offset: u(0)?,
            thread_local_class: u(1)?,
            supplied_class: u(2)?,
            map_class: u(3)?,
            entry_class: u(4)?,
            thread_locals_index: u(5)?,
            table_index: u(6)?,
            hash_index: u(7)?,
            referent_index: u(8)?,
            value_index: u(9)?,
            thread_locals_row: u(10)?,
            table_row: u(11)?,
            hash_row: u(12)?,
            value_row: u(13)?,
            set_admitted: w[14] == 1,
            inheritable_class: u(15)?,
            inheritable_thread_locals_index: u(16)?,
            inheritable_thread_locals_row: u(17)?,
            referent_row: u(18)?,
        };
        (l.thread_obj_offset != 0
            && l.thread_local_class != 0
            && l.map_class != 0
            && l.entry_class != 0)
            .then_some(l)
    }

    /// This compile's layout: `None` under `CRATONVM_JIT_IR_THREAD_LOCAL_GET=0`
    /// or wherever the VM declines.
    pub fn from_table(table: &crate::DirectHelperTable) -> Option<Self> {
        if !ir_thread_local_get_enabled() {
            return None;
        }
        Self::from_words(&table.thread_local_ir_layout_words()?)
    }
}

/// Compile-time layout for `ByteBuffer.wrap([BII)` and `Charset.decode(ByteBuffer)`
/// pure-IR intrinsics (`DirectHelperTable::nio_charset_ir_layout_words`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NioCharsetIrLayout {
    pub hbb_class: u32,
    pub hbb_num_fields: u32,
    pub hcb_class: u32,
    pub hcb_num_fields: u32,
    pub iso_class: u32,
    pub buf_mark_index: u32,
    pub buf_mark_row: u32,
    pub buf_pos_index: u32,
    pub buf_pos_row: u32,
    pub buf_lim_index: u32,
    pub buf_lim_row: u32,
    pub buf_cap_index: u32,
    pub buf_cap_row: u32,
    pub bb_hb_index: u32,
    pub bb_hb_row: u32,
    pub bb_off_index: u32,
    pub bb_off_row: u32,
    pub bb_be_index: u32,
    pub bb_be_row: u32,
    pub cb_hb_index: u32,
    pub cb_hb_row: u32,
    pub cb_off_index: u32,
    pub cb_off_row: u32,
    pub str_class: u32,
    pub str_num_fields: u32,
    pub str_val_index: u32,
    pub str_val_row: u32,
    pub str_coder_index: u32,
    pub str_coder_row: u32,
    pub compact_strings: u32,
    pub buf_addr_index: u32,
    pub buf_addr_row: u32,
    pub iso_dec_class: u32,
    pub dec_state_index: u32,
    pub dec_state_row: u32,
    pub coder_result_class: u32,
    pub cr_underflow_static_index: u32,
    pub cr_overflow_static_index: u32,
}

impl NioCharsetIrLayout {
    pub fn from_words(w: &[u64; crate::direct_helpers::NIO_CHARSET_IR_LAYOUT_QUERY_WORDS]) -> Option<Self> {
        let u = |v: u64| u32::try_from(v).ok();
        // Little-endian host required so `nativeByteOrder` stays false (0).
        if w[23] != 1 {
            return None;
        }
        let hbb_class = u(w[0])?;
        if hbb_class == 0 {
            return None;
        }
        Some(Self {
            hbb_class,
            hbb_num_fields: u(w[1])?,
            hcb_class: u(w[2])?,
            hcb_num_fields: u(w[3])?,
            iso_class: u(w[4])?,
            buf_mark_index: u(w[5])?,
            buf_mark_row: u(w[6])?,
            buf_pos_index: u(w[7])?,
            buf_pos_row: u(w[8])?,
            buf_lim_index: u(w[9])?,
            buf_lim_row: u(w[10])?,
            buf_cap_index: u(w[11])?,
            buf_cap_row: u(w[12])?,
            bb_hb_index: u(w[13])?,
            bb_hb_row: u(w[14])?,
            bb_off_index: u(w[15])?,
            bb_off_row: u(w[16])?,
            bb_be_index: u(w[17])?,
            bb_be_row: u(w[18])?,
            cb_hb_index: u(w[19])?,
            cb_hb_row: u(w[20])?,
            cb_off_index: u(w[21])?,
            cb_off_row: u(w[22])?,
            str_class: u(w[24])?,
            str_num_fields: u(w[25])?,
            str_val_index: u(w[26])?,
            str_val_row: u(w[27])?,
            str_coder_index: u(w[28])?,
            str_coder_row: u(w[29])?,
            compact_strings: u(w[30])?,
            buf_addr_index: u(w[31])?,
            buf_addr_row: u(w[32])?,
            iso_dec_class: u(w[33])?,
            dec_state_index: u(w[34])?,
            dec_state_row: u(w[35])?,
            coder_result_class: u(w[36])?,
            cr_underflow_static_index: u(w[37])?,
            cr_overflow_static_index: u(w[38])?,
        })
    }

    pub fn from_table(table: &crate::DirectHelperTable) -> Option<Self> {
        if !ir_nio_charset_enabled() {
            return None;
        }
        Self::from_words(&table.nio_charset_ir_layout_words()?)
    }
}

pub fn ir_nio_charset_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_THREAD_LOCAL_GET` (nio21 wave 19, lane L3), **default
/// ON**: the optimizing tier expands `ThreadLocal.get()`'s hit path inline
/// (`IrBuilder::try_thread_local_get_intrinsic`). `=0` keeps every call.
pub fn ir_thread_local_get_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_THREAD_LOCAL_PROFILED_SUBCLASS` (JIT round 21, lane tlsub,
/// TL321-4; page `jr21-tl3-threadlocal-subclass-get-takes-the-thin-helper-FIXED-20261014.md`),
/// **default ON**: a `ThreadLocal.get` / `set` site whose receiver profile names a
/// `ThreadLocal` subclass that reads the plain map (the
/// `new ThreadLocal<>() { initialValue() }` idiom) expands for that class too
/// (`IrBuilder::tl_subclass_admitted`). `=0` keeps such a site on the miss call.
pub fn ir_thread_local_profiled_subclass_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_THREAD_LOCAL_MISS_TRAP` (JIT round 21, lane tlsub, TL321-1;
/// page `jr21-tl3-threadlocal-get-expansion-still-8x-hotspot-FIXED-20261014.md`),
/// **default ON**: a `ThreadLocal.get` / `set` site whose receiver profile saw
/// one admitted class alone builds every miss of its expansion as an uncommon
/// trap at the invoke instead of the call (`IrBuilder::tl_trap_site_admits`),
/// so the loop around it holds no call and no memory merge. `=0` keeps the call.
pub fn ir_thread_local_miss_trap_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_THREAD_LOCAL_UNPROFILED_TRAP` (JIT round 21, lane tlget,
/// TLGET21-1; page `jr21-tl3-threadlocal-subclass-get-takes-the-thin-helper-FIXED-20261014.md`),
/// **default ON**: a `ThreadLocal.get` / `set` site with no settled receiver
/// profile tests the layout's classes as an uncommon trap at the invoke, so a
/// subclass receiver deopts once and the recompile admits it from the
/// profile the interpreter then records (`IrBuilder::tl_unprofiled_site`).
/// `=0` keeps that test a branch to the call.
pub fn ir_thread_local_unprofiled_trap_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_THREAD_LOCAL_PENDING_TRAP` (TLGET21-7; page
/// `jr21-tl3-threadlocal-subclass-get-takes-the-thin-helper-FIXED-20261014.md`),
/// **default ON**: a never-run `ThreadLocal.get` / `set` site compiled while
/// the expansion's layout is pending traps at its invoke
/// (`IrBuilder::tl_pending_layout_trap`). `=0` keeps the call.
pub fn ir_thread_local_pending_trap_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_THREAD_LOCAL_UNPROFILED_FULL_TRAP` (TLGET21-6; page
/// `jr21-tl3-threadlocal-get-expansion-still-8x-hotspot-FIXED-20261014.md`),
/// **default ON**: a site with no settled receiver profile (never run: a
/// `tableswitch` arm of a driver compiled before that arm ran) builds the
/// WHOLE trap form, its class test the layout's own classes, instead of
/// TLGET21-1's call form with a trapping class test. That call form never
/// traps for a layout-class receiver (`withInitial`, a plain
/// `InheritableThreadLocal`), so nothing ever recompiled the site, and its
/// call kept every such loop from the loop predicate
/// (`CRATONVM_JIT_IR_THREAD_LOCAL_LOOP_PREDICATE`). A first miss (an
/// `initialValue`) traps, the interpreter profiles the site, and the recompile
/// decides from the profile; a site that keeps missing is withdrawn by the
/// trap form's de-spec rule. Needs `..._UNPROFILED_TRAP`. `=0` keeps
/// TLGET21-1's form.
pub fn ir_thread_local_unprofiled_full_trap_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_THREAD_LOCAL_TRAP_SEED_FLOOR` (TLGET21-6), **default
/// ON**: a site's profile names its class for the trap form
/// ([`ThreadLocalSitePlan::note_receivers`]) when it saw that class ALONE
/// over the seed floor
/// ([`crate::profile::DOMINANT_RECEIVER_MIN_OBSERVATIONS`], 16), not the
/// guarded splice's [`crate::MIN_GUARDED_RECEIVER_EVIDENCE`] (32). The
/// activation a trap hands back runs the rest of its 16-trip kernel in the
/// interpreter, which is the 16 samples the recompile sees; at 32 that
/// recompile chose the call form for good. The trap form's misses are not in
/// the receiver profile at either floor; its de-spec rule bounds them. `=0`
/// keeps 32.
pub fn ir_thread_local_trap_seed_floor_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_THREAD_LOCAL_TRUSTED_LOADS` (JIT round 21, lane tlget,
/// TLGET21-2; page `jr21-tl3-threadlocal-get-expansion-still-8x-hotspot-FIXED-20261014.md`),
/// **default ON**: the expansion's reference loads take the trusted-receiver
/// form (one "read table live" compare instead of the alignment test and six
/// region compares; [`Graph::tl_trusted_loads`]). `=0` keeps the containment.
pub fn ir_thread_local_trusted_loads_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_THREAD_LOCAL_REFERENT_ROW` (TL321-5; page
/// `jr21-tl3-threadlocal-get-expansion-still-8x-hotspot-FIXED-20261014.md`),
/// **default ON**: the expansion's `Entry.referent` reads carry the referent's
/// compact row ([`ThreadLocalIrLayout::referent_row`]), so a compact `Entry`
/// (every `Entry` the allocator makes compact) is compared inline instead of
/// through `jit_getfield` on every probe. `=0` keeps the field-index arms.
pub fn ir_thread_local_referent_row_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_THREAD_LOCAL_SET_ROW` (page
/// `jr21-tl3-threadlocal-get-expansion-still-8x-hotspot-FIXED-20261014.md`, the
/// `tl-set-home` row), **default ON**: the `set` expansion's `e.value = v`
/// store carries `Entry.value`'s compact row, so the lowering stores a compact
/// `Entry` through the collector's gated inline reference store (the
/// `putfield` arm's) instead of calling `jit_putfield_object` on every `set`.
/// `=0` keeps the helper.
pub fn ir_thread_local_set_row_enabled() -> bool {
    true
}

/// The share of a `ThreadLocal` site's receiver observations its dominant
/// class must hold to be tested for (JIT round 21, lane tlsub): the miss of
/// the call form is a call, so a wrong pick costs one class compare.
pub(in crate::ir) const TL_SITE_DOMINANT_PCT: u32 = 90;

/// JIT round 21 (lane tlsub, TL321-4 / TL321-1): what
/// `IrBuilder::try_thread_local_get_intrinsic` reads beyond the
/// [`ThreadLocalIrLayout`]: each site's receiver profile (the compiling
/// method's own sites by bci, a spliced body's by combined pc from the
/// callee's profile), the VM's one-class query, and this VM's de-spec
/// registry with the compiling method's key.
pub struct ThreadLocalSitePlan {
    /// pc -> (dominant class, the class when the profile saw it alone).
    pub(in crate::ir) receivers: HashMap<usize, (u32, Option<u32>)>,
    /// JIT round 21 (lane tlget): pcs whose profile has enough samples
    /// ([`crate::profile::DOMINANT_RECEIVER_MIN_OBSERVATIONS`]) but no
    /// dominant class -- a polymorphic site, settled, never "unprofiled".
    pub(in crate::ir) unsettled: HashSet<usize>,
    /// TLGET21-7: the VM serves the expansion, but its layout is not
    /// resolvable yet ([`crate::DirectHelperTable::thread_local_layout_pending`]).
    pub(in crate::ir) layout_pending: bool,
    pub(in crate::ir) table: Option<crate::DirectHelperTable>,
    pub(in crate::ir) despec: Option<std::sync::Arc<crate::deopt::DespecRegistry>>,
    /// `"class.name:descriptor"`, the de-spec registry's key.
    pub(in crate::ir) method_key: String,
    /// Per compile: class id -> the VM's answer.
    pub(in crate::ir) kinds: HashMap<u32, Option<(u8, bool)>>,
}

impl ThreadLocalSitePlan {
    /// This compile's plan, or `None` with the expansion or both of its
    /// round-21 switches off.
    pub fn for_compile(
        profile: Option<&crate::profile::MethodProfile>,
        table: Option<crate::DirectHelperTable>,
        despec: Option<std::sync::Arc<crate::deopt::DespecRegistry>>,
        method_key: String,
    ) -> Option<Self> {
        if !ir_thread_local_get_enabled()
            || !(ir_thread_local_profiled_subclass_enabled() || ir_thread_local_miss_trap_enabled())
        {
            return None;
        }
        // TLGET21-7: asked once per compile, and only where the pending trap
        // could use it.
        let layout_pending = ir_thread_local_pending_trap_enabled()
            && table
                .as_ref()
                .is_some_and(|t| t.thread_local_layout_pending());
        let mut plan = ThreadLocalSitePlan {
            receivers: HashMap::new(),
            unsettled: HashSet::new(),
            layout_pending,
            table,
            despec,
            method_key,
            kinds: HashMap::new(),
        };
        if let Some(profile) = profile {
            for (&pc, counts) in &profile.receivers {
                plan.note_receivers(pc, counts);
            }
        }
        Some(plan)
    }

    /// Record the receiver profile of the site at `pc`.
    pub fn note_receivers(&mut self, pc: usize, counts: &crate::profile::ReceiverCounts) {
        let evidence = crate::profile::ReceiverEvidence::of(counts);
        if let Some(dominant) = evidence.seed_a_guarded_cache(TL_SITE_DOMINANT_PCT) {
            // TLGET21-6 (`CRATONVM_JIT_IR_THREAD_LOCAL_TRAP_SEED_FLOOR`): one
            // class alone over the seed floor, not the guarded splice's 32.
            let floor = if ir_thread_local_trap_seed_floor_enabled() {
                crate::profile::DOMINANT_RECEIVER_MIN_OBSERVATIONS
            } else {
                crate::MIN_GUARDED_RECEIVER_EVIDENCE
            };
            let only = evidence.only_class(floor);
            self.receivers.insert(pc, (dominant, only));
        } else if evidence.summary().observations
            >= crate::profile::DOMINANT_RECEIVER_MIN_OBSERVATIONS
        {
            self.unsettled.insert(pc);
        }
    }

    /// JIT round 21 (lane tlget, TLGET21-1): no settled profile at `pc` --
    /// neither a dominant class nor a polymorphic row over the floor.
    pub fn unprofiled(&self, pc: usize) -> bool {
        !self.receivers.contains_key(&pc) && !self.unsettled.contains(&pc)
    }

    /// The VM's `(map kind, set admitted)` for `class_id`, asked once per compile.
    pub(in crate::ir) fn receiver_kind(&mut self, class_id: u32) -> Option<(u8, bool)> {
        if let Some(&known) = self.kinds.get(&class_id) {
            return known;
        }
        let answer = self
            .table
            .and_then(|t| t.thread_local_receiver_kind(class_id));
        self.kinds.insert(class_id, answer);
        answer
    }
}

/// `CRATONVM_JIT_IR_REFERS_TO_INLINE` (JIT round 21, lane tl3), **default ON**:
/// `Reference.refersTo(Object)` and its spliced `refersToImpl` / `refersTo0`
/// are an inline identity compare of the referent
/// (`IrBuilder::try_reference_refers_to_intrinsic`). `=0` keeps every call.
/// Rides the `ThreadLocal` layout for the referent's index, so
/// `CRATONVM_JIT_IR_THREAD_LOCAL_GET=0` (no layout) turns it off too.
pub fn ir_refers_to_inline_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_REFERS_TO_OWN_LAYOUT` (JIT round 21, lane refs2; page
/// `docs/feature-designs/zgc-architecture.md`),
/// **default ON**: the `refersTo` expansion takes `Reference.referent`'s index
/// from the VM's referent-only answer
/// ([`crate::DirectHelperTable::reference_referent_index`]) when the compile
/// has no `ThreadLocal` layout. That layout needs `InheritableThreadLocal`
/// and `TerminatingThreadLocal` loaded, so a program that has not yet run a
/// `ThreadLocal.set` / `setInitialValue` kept every `refersTo` a call
/// (`R21Tl3Rows weakref-refersto` 872 in every arm vs `R21Tl2Rows`' 142).
/// `=0` restores the wait.
pub fn ir_refers_to_own_layout_enabled() -> bool {
    true
}

/// This compile's referent index for [`IrBuilder::set_refers_to_referent_index`]:
/// `None` with either switch off or where the VM declines (`--compatible`).
pub fn refers_to_referent_index_from_table(table: &crate::DirectHelperTable) -> Option<u32> {
    if !ir_refers_to_inline_enabled() || !ir_refers_to_own_layout_enabled() {
        return None;
    }
    table.reference_referent_index()
}

/// JIT round 21 (lane tl3): is `(class, method, descriptor)` a site
/// `IrBuilder::try_reference_refers_to_intrinsic` may answer? `refersTo(T)` on
/// a boot `java.*` class that IS a `java.lang.ref.Reference` subclass (the
/// method is `final` in `Reference`, so every one of them selects
/// `Reference.refersTo`), and `refersToImpl` / `refersTo0` on `Reference` and
/// `PhantomReference`, the only two declarers. No other loader can define a
/// `java.*` name, so the name is the class.
pub fn refers_to_inline_site(class: &str, method: &str, descriptor: &str) -> bool {
    if descriptor != "(Ljava/lang/Object;)Z" {
        return false;
    }
    match method {
        "refersTo" => matches!(
            class,
            "java/lang/ref/Reference"
                | "java/lang/ref/WeakReference"
                | "java/lang/ref/SoftReference"
                | "java/lang/ref/PhantomReference"
                | "java/lang/ThreadLocal$ThreadLocalMap$Entry"
                | "java/util/WeakHashMap$Entry"
        ),
        "refersToImpl" | "refersTo0" => matches!(
            class,
            "java/lang/ref/Reference" | "java/lang/ref/PhantomReference"
        ),
        _ => false,
    }
}

/// `CRATONVM_JIT_IR_REFERENCE_GET_INLINE` (JIT round 21, lane g1vhsoft; page
/// `docs/feature-designs/jit-barrier-lowering.md`),
/// **default ON**: a `WeakReference.get()` / `SoftReference.get()` site whose
/// receiver is exactly that JDK class reads the referent inline
/// (`IrBuilder::try_reference_get_intrinsic`), with the SATB keep-alive gate
/// of the spliced `Reference.get` read. `=0` keeps every call. Read per
/// compile.
pub fn ir_reference_get_inline_enabled() -> bool {
    true
}

/// JIT round 21 (lane g1vhsoft): what the `WeakReference.get()` /
/// `SoftReference.get()` expansion bakes, as the VM answers it
/// (`vm::jit::helpers::jit_thread_local_ir_layout`, query
/// [`crate::direct_helpers::REFERENCE_GET_LAYOUT_QUERY_WORDS`]).
///
/// | word | meaning |
/// |---|---|
/// | 0 | class id of the boot `java/lang/ref/WeakReference` (`0` = its sites keep the call) |
/// | 1 | class id of the boot `java/lang/ref/SoftReference` (`0` = its sites keep the call) |
/// | 2 | field index of `Reference.referent` |
/// | 3 | `1 +` compact displacement of the referent in `WeakReference`'s layout (`0` = none) |
/// | 4 | `1 +` compact displacement of the referent in `SoftReference`'s layout (`0` = none) |
/// | 5 | field index of `SoftReference.timestamp` (`long`) |
/// | 6 | `1 +` compact displacement of `timestamp` in `SoftReference`'s layout (`0` = none) |
/// | 7 | static field index of `SoftReference.clock` (`long`) in `SoftReference`'s statics |
/// | 8 | `1` when `clock` is declared `volatile`, else `0` |
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReferenceGetIrLayout {
    pub weak_class: u32,
    pub soft_class: u32,
    pub referent_index: u32,
    pub weak_referent_row: u32,
    pub soft_referent_row: u32,
    pub timestamp_index: u32,
    pub timestamp_row: u32,
    pub clock_index: u32,
    pub clock_volatile: bool,
}

impl ReferenceGetIrLayout {
    /// The layout in the VM's words, or `None` for a word out of range or no
    /// class admitted at all.
    pub fn from_words(
        w: &[u64; crate::direct_helpers::REFERENCE_GET_LAYOUT_QUERY_WORDS],
    ) -> Option<Self> {
        let u = |i: usize| u32::try_from(w[i]).ok();
        let l = ReferenceGetIrLayout {
            weak_class: u(0)?,
            soft_class: u(1)?,
            referent_index: u(2)?,
            weak_referent_row: u(3)?,
            soft_referent_row: u(4)?,
            timestamp_index: u(5)?,
            timestamp_row: u(6)?,
            clock_index: u(7)?,
            clock_volatile: w[8] == 1,
        };
        (l.weak_class != 0 || l.soft_class != 0).then_some(l)
    }

    /// This compile's layout: `None` under
    /// `CRATONVM_JIT_IR_REFERENCE_GET_INLINE=0` or wherever the VM declines.
    /// The `SoftReference` half is dropped (its class set to `0`) unless
    /// `SoftReference.clock` reads inline in this compile
    /// (`Lowerer::emit_inline_getstatic`'s own two conditions): the expansion
    /// must not carry a `getstatic` helper call, which could run Java.
    pub fn from_table(table: &crate::DirectHelperTable) -> Option<Self> {
        if !ir_reference_get_inline_enabled() {
            return None;
        }
        let mut l = Self::from_words(&table.reference_get_layout_words()?)?;
        if l.soft_class != 0
            && (!crate::x64::inline_getstatic_enabled()
                || table
                    .resolve_static_base(l.soft_class, l.clock_index as usize)
                    .is_none())
        {
            l.soft_class = 0;
        }
        (l.weak_class != 0 || l.soft_class != 0).then_some(l)
    }
}

/// `CRATONVM_JIT_IR_THREAD_LOCAL_SET` (nio21 wave 20, lane L3), **default
/// ON**: `ThreadLocal.set(Object)` takes the same expansion, storing into the
/// found entry. `=0` keeps the call. Needs `CRATONVM_JIT_IR_THREAD_LOCAL_GET`.
pub fn ir_thread_local_set_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_THREAD_LOCAL_GET_ROWS` (nio21 wave 20, lane L3), **default
/// ON**: the expansion's loads carry their own compact rows
/// (`Graph::field_rows`), so a compact `Thread`, map, `ThreadLocal` or entry
/// is read inline instead of through the checked `jit_getfield` helper. `=0`
/// sends compact bodies to the helper, as in wave 19.
pub fn ir_thread_local_get_rows_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_THREAD_LOCAL_PROBE_WALK` (JIT round 21, lane tlocal,
/// `docs/feature-designs/jit-ir-optimizations.md`),
/// **default ON**: the `ThreadLocal.get()` / `set` expansion follows the
/// linear-probe chain for up to [`TL_PROBE_WALK_EXTRA`] slots past the key's
/// home slot (`getEntryAfterMiss` without its expunge), so a displaced key
/// hits inline instead of paying the helper on every call. `=0` probes the
/// home slot only (nio21 wave 19).
pub fn ir_thread_local_probe_walk_enabled() -> bool {
    true
}

/// How many slots past the home slot the `ThreadLocal` expansion probes
/// (JIT round 21, lane tlocal). `ThreadLocalMap` resizes at 2/3 load and its
/// hash spreads consecutive keys by the golden ratio, so chains are short.
pub const TL_PROBE_WALK_EXTRA: usize = 2;

/// [`TL_PROBE_WALK_EXTRA`] under [`ir_thread_local_probe_walk_deep_enabled`].
pub const TL_PROBE_WALK_EXTRA_DEEP: usize = 4;

/// `CRATONVM_JIT_IR_THREAD_LOCAL_PROBE_WALK_DEEP` (page
/// `jr21-tlocal-threadlocal-get-kernel-FIXED-20261014.md`, the `worker-chain-3`
/// row), **default ON**: the probe walk tries [`TL_PROBE_WALK_EXTRA_DEEP`]
/// slots past home instead of [`TL_PROBE_WALK_EXTRA`]. A key the walk does
/// not reach pays the thin helper on every call (~20x the inline hit), and
/// HotSpot's compiled `getEntryAfterMiss` makes a displaced key cost a few
/// loads; the extra probes are on the displaced path only, and with the
/// chain moved out of its loop (`CRATONVM_JIT_IR_THREAD_LOCAL_LOOP_PREDICATE`)
/// they run once per loop entry. `=0` keeps two.
pub fn ir_thread_local_probe_walk_deep_enabled() -> bool {
    true
}

/// The probe walk's depth for this compile: `0` with the walk off, else
/// [`TL_PROBE_WALK_EXTRA_DEEP`] or [`TL_PROBE_WALK_EXTRA`].
pub fn tl_probe_walk_extra() -> usize {
    if !ir_thread_local_probe_walk_enabled() {
        0
    } else if ir_thread_local_probe_walk_deep_enabled() {
        TL_PROBE_WALK_EXTRA_DEEP
    } else {
        TL_PROBE_WALK_EXTRA
    }
}

/// `CRATONVM_JIT_IR_THREAD_LOCAL_IN_SPLICE` (JIT round 21, lane tl2,
/// `docs/feature-designs/native-dispatch-architecture.md`),
/// **default ON**: the `ThreadLocal.get()` / `set` expansion also serves a
/// site inside a spliced callee body (a kernel spliced into its driver, the
/// probe's `run(which, n)` shape), where it used to be refused and the site
/// was a plain call through the inline-cache cascade into the native funnel.
/// `=0` keeps every spliced site a call, as before.
pub fn ir_thread_local_in_splice_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_REFERS_TO_NO_KEEPALIVE` (t1 wave 10, lane REFS3), **default
/// ON**, a correctness fix (page
/// `t1-refs2-refersto-expansion-load-takes-the-referent-keep-alive-20261026`):
/// the `refersTo` expansions' compare-only referent reads
/// ([`Graph::no_keepalive_referent_loads`]) skip the checked `jit_getfield`
/// helper's slot-0 SATB keep-alive (`GETFIELD_NO_KEEPALIVE`), as the
/// interpreter's natives, `jit_refers_to0` and HotSpot's `AS_NO_KEEPALIVE`
/// `refersTo0` load do. Before it, a COMPACT receiver's read (which has no
/// compact row and so takes the helper) logged the referent during a
/// concurrent mark and kept a weakly reachable referent alive one cycle
/// longer than HotSpot. `=0` restores the keep-alive.
pub fn ir_refers_to_no_keepalive_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_REFERS_TO_COMPACT_ROW` (t1 wave 12, lane REFS4), **default
/// ON**, `=0` restores the row-less read: the `refersTo` expansions' referent
/// reads (the `Reference.refersTo` expansion and the `ThreadLocal` expansion's
/// `refersTo(this)` step) carry `Reference.referent`'s compact row
/// (`Graph::field_rows`), so a COMPACT receiver -- every `Reference` the
/// compact layout allocates, i.e. almost all of them -- reads the slot inline
/// instead of calling the checked `jit_getfield` helper. Without it the
/// expansion's read had no row (a synthetic pc), took the legacy-only arm and,
/// on a compact receiver, the helper: host t1 wave 11 `T1W8RlHashrefRefersTo`
/// soft 204.9 / phantom 216.4 ns per 16 calls (`--real-jdk`) vs HotSpot 4.4.
/// The row is the VM's ([`refers_to_referent_row_from_table`]): named only
/// for collectors whose referent reads owe no gate (Gen, G1, and since j27
/// wave 5 a ZGC heap whose gate never arms: [`ir_refers_to_zgc_row_enabled`]),
/// so the inline read is the helper's read minus the call.
pub fn ir_refers_to_compact_row_enabled() -> bool {
    true
}

/// This compile's `1 + compact displacement` of `Reference.referent` for
/// [`IrBuilder::set_refers_to_referent_row`] (t1 wave 12, lane REFS4): `0`
/// with the expansion or the row switch off, without compact reference
/// fields, or where the VM names no row.
pub fn refers_to_referent_row_from_table(table: &crate::DirectHelperTable) -> u32 {
    if !ir_refers_to_inline_enabled()
        || !ir_refers_to_compact_row_enabled()
        || !cratonvm_types::compact_ref_fields_enabled()
    {
        return 0;
    }
    // j27 wave 5 (lane REFS): a `refersTo` read only compares the word, so
    // it takes the VM's COMPARE-ONLY row, which a ZGC heap whose referent
    // gate can never arm names too ([`ir_refers_to_zgc_row_enabled`]).
    if ir_refers_to_zgc_row_enabled() {
        return table.reference_referent_compare_row().unwrap_or(0);
    }
    table.reference_referent_row().map_or(0, |(_, row)| row)
}

/// `CRATONVM_JIT_IR_REFERS_TO_ZGC_ROW` (j27 wave 5, lane REFS), **default
/// ON**, `=0` restores the Gen / G1-only row: the `refersTo` expansion's
/// referent read takes the VM's compare-only row
/// (`DirectHelperTable::reference_referent_compare_row`), named on every
/// collector. Where the heap's referent read gate may arm
/// (`ZgcRealHeap::referent_gate_may_arm`), the lowering reads it inline only
/// for a compare against a non-null or null-tested value
/// (`ir_lower::referent_read_owes_the_checked_helper`); elsewhere the checked
/// helper's no-keep-alive read is the slot word with no gate, which the
/// inline read is too, minus the call. A colored ZGC barrier decodes this
/// compare-only read inline, with the checked no-keep-alive helper on a bad
/// color. Other armed barriers refuse the expansion and empty the read table,
/// so an already compiled plain read falls to the helper.
/// Page `weakref-get-and-refersto-read-a-compact-referent-through-jit-getfield-FIXED-20261029.md`.
pub fn ir_refers_to_zgc_row_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_REFERENT_SPLICE_COMPACT_ROW` (j27 wave 1, lane TYPECHK),
/// **default ON**, `=0` restores the row-less read: the spliced
/// `Reference.get` referent reads (`Graph::referent_load_pcs`) carry
/// `Reference.referent`'s compact row, so a COMPACT receiver reads inline
/// behind the SATB pre-gate instead of calling the checked `jit_getfield`
/// helper (page
/// `weakref-get-and-refersto-read-a-compact-referent-through-jit-getfield-FIXED-20261029.md`).
/// Read once per compile.
pub fn ir_referent_splice_compact_row_enabled() -> bool {
    true
}

/// This compile's `1 + compact displacement` of `Reference.referent` for
/// [`IrBuilder::set_spliced_referent_row`] (j27 wave 1, lane TYPECHK): the
/// same VM answer as [`refers_to_referent_row_from_table`] (named on Gen and
/// G1 only), without that expansion's switches; `0` with
/// [`ir_referent_splice_compact_row_enabled`] off, without compact reference
/// fields, or where the VM names no row.
pub fn spliced_referent_row_from_table(table: &crate::DirectHelperTable) -> u32 {
    if !ir_referent_splice_compact_row_enabled() || !cratonvm_types::compact_ref_fields_enabled() {
        return 0;
    }
    let handed_out = table.reference_referent_row().map_or(0, |(_, row)| row);
    // j27 wave 7 (lane T1ACCEPT, `CRATONVM_JIT_IR_REFERENT_SPLICE_ZGC_ROW`).
    spliced_referent_row_choice(
        handed_out,
        table.reference_referent_compare_row().unwrap_or(0),
        ir_referent_splice_zgc_row_enabled(),
    )
}

/// `CRATONVM_JIT_IR_REFERENT_SPLICE_ZGC_ROW` (j27 wave 7, lane T1ACCEPT),
/// **default ON** (`=0` restores the checked helper): on a ZGC heap whose referent read gate can never arm,
/// the SPLICED `Reference.get` referent read takes the referent's compact
/// row too, as the exact-class `WeakReference` / `SoftReference.get()`
/// expansion already does there (`reference_get_ir_layout_words` names its
/// rows wherever the gate cannot arm). Page
/// `weakref-get-and-refersto-read-a-compact-referent-through-jit-getfield-FIXED-20261029.md`.
/// Enabled after same-target splice admission, exact-source GC stress and
/// HotSpot parity on 2026-10-30; the SATB, colored-load and resurrection
/// gates below remain mandatory.
pub fn ir_referent_splice_zgc_row_enabled() -> bool {
    true
}

/// The spliced `Reference.get` referent row (j27 wave 7, lane T1ACCEPT):
/// the VM's handed-out row (Gen / G1) when it names one; else, under
/// `zgc_row` ([`ir_referent_splice_zgc_row_enabled`]), its compare-only row,
/// which the VM names on every heap whose ZGC referent gate can never arm
/// (and which equals the handed-out row on Gen / G1).
///
/// Why the compare-only row is enough for a HANDED-OUT read there, i.e. what
/// the checked helper owes beyond the slot word and how the inline read
/// covers it (HotSpot's ZGC `Reference.get`: a load barrier, a keep-alive
/// while marking, and `null` for a dead referent while resurrection is
/// blocked):
/// * the resurrection gate (`gate_referent_read`): never armed on such a heap
///   (it arms only with `CRATONVM_ZGC_CONCURRENT_REFERENT_CLEAR`), so a
///   no-op; the STW reference pass leaves no blocked window;
/// * the load barrier: an armed ZGC read barrier declines every volatile
///   reference load at compile time (`emit_inline_compact_getfield`'s colored
///   arm requires `!volatile_access`; referent loads are volatile), and
///   arming clears the read-bounds table, so already-compiled guarded reads
///   take the helper; unarmed, the slot word is a plain address;
/// * the keep-alive (`ZgcRealHeap::referent_keep_alive`): runs only while
///   `mark_active`, and its colour-flip arm only with the barrier armed
///   (above). `set_mark_active` raises the heap's JIT `pre_active` byte BEFORE
///   `mark_active` and lowers it AFTER, and the lowering tests that byte after
///   every `referent_load_pcs` read with a row (helper when armed), exactly
///   the TL221-1 argument the Gen / G1 row rests on.
pub fn spliced_referent_row_choice(handed_out: u32, compare_only: u32, zgc_row: bool) -> u32 {
    if handed_out != 0 || !zgc_row {
        handed_out
    } else {
        compare_only
    }
}

/// How many levels of φ inputs `IrBuilder::strip_trivial_phis` strips before
/// comparing them (nio21 wave 19, lane L3).
pub(in crate::ir) const STRIP_TRIVIAL_PHI_INPUT_DEPTH: u32 = 8;

/// `CRATONVM_JIT_IR_STRIP_TRIVIAL_PHI_INPUTS` (nio21 wave 19, lane L3),
/// **default ON**: see `IrBuilder::strip_trivial_phis`. `=0` compares raw φ
/// input ids, as before.
pub fn ir_strip_trivial_phi_inputs_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_ATHROW_IN_MONITOR_TRAP` (nio21 wave 18, lane L3), **default
/// ON**: see `IrBuilder::trap_athrow_in_monitor`. `=0` restores the refusal of
/// a method whose `athrow` is reached with a `monitorenter` of its own held.
/// Read once per such site.
pub fn ir_athrow_in_monitor_trap_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_IMPLICIT_EDGE_FENCED_HANDLERS` (nio21 wave 6, lane L2),
/// **default ON**: an implicit site arms even when checks in its handler's
/// code were elided, because `ir_check_elim` no longer lets a fact created at
/// an exception-edge anchor reach a handler (`exception_edge_fences`). `=0`
/// restores wave 5's refusal of such sites in
/// `ir_lower::Lowerer::plan_implicit_throw_site`, for an A/B; it is not a
/// soundness switch (the fence is always on).
pub fn ir_implicit_edge_fenced_handlers_enabled() -> bool {
    true
}

/// The nodes some `Op::CallThrew` hangs on (its input 1): the calls and
/// trapping nodes that carry exception edges (nio21 wave 6, lane L2). A pass
/// that would move such a node, or prove its check away by planting a guard
/// ahead of it (the predicate passes of `ir_optimize`), leaves the node alone:
/// moved, its flags no longer sit after it in its block and the lowerer
/// leaves the site unarmed; predicated, its check can no longer fail, so a
/// throw deopts at the predicate instead of entering the handler. Both are
/// sound, but the edge is dead. Empty (and one scan) for a graph built
/// without the default-OFF edge switches.
pub(crate) fn exception_edge_anchors(graph: &Graph) -> std::collections::HashSet<NodeId> {
    graph
        .nodes
        .iter()
        .filter(|n| matches!(n.op, Op::CallThrew { .. }))
        .filter_map(|n| n.inputs.get(1).copied())
        .collect()
}

/// The arrays the array-access anchors among `anchors` (see
/// [`exception_edge_anchors`]) for which `within` holds index (nio21 wave 6,
/// lane L2). A loop predicate that dereferences or length-tests one of them
/// ahead of the loop would decide the anchor's null (or bounds) check there,
/// and its edge would never fire.
pub(crate) fn exception_edge_anchor_arrays(
    graph: &Graph,
    anchors: &std::collections::HashSet<NodeId>,
    within: &dyn Fn(NodeId) -> bool,
) -> std::collections::HashSet<NodeId> {
    anchors
        .iter()
        .copied()
        .filter(|&a| within(a))
        .filter_map(|a| graph.node_opt(a))
        .filter(|n| matches!(n.op, Op::ArrayLoad(_) | Op::ArrayStore(_)))
        .filter_map(|n| n.inputs.get(2).copied())
        .collect()
}

/// What an implicit-exception site of `code` at `pc` can raise, by opcode
/// (nio21 wave 5 L2): `(npe, aioobe, arithmetic)`. `None` for any other
/// opcode. `aastore`'s `ArrayStoreException` comes from its helper's own exit
/// and is not an edge of this kind.
///
/// nio21 wave 6 L2 (proposal L2W5-1): `getfield` / `putfield` raise an NPE on
/// a null receiver; the planner admits them only under
/// [`ir_implicit_field_edges_enabled`].
pub(crate) fn implicit_site_kinds(op: u8) -> Option<(bool, bool, bool)> {
    match op {
        // xaload, xastore
        0x2e..=0x35 | 0x4f..=0x56 => Some((true, true, false)),
        // idiv, ldiv, irem, lrem
        0x6c | 0x6d | 0x70 | 0x71 => Some((false, false, true)),
        // getfield, putfield
        0xb4 | 0xb5 => Some((true, false, false)),
        _ => None,
    }
}

/// Is `op` a field access ([`implicit_site_kinds`]' wave-6 rows)?
pub(in crate::ir) fn implicit_site_is_field(op: u8) -> bool {
    matches!(op, 0xb4 | 0xb5)
}

/// `CRATONVM_JIT_IR_IMPLICIT_FIELD_EDGES` (nio21 wave 6, lane L2, proposal
/// L2W5-1), **default ON** beneath the default-OFF umbrella
/// [`ir_implicit_exception_edges_enabled`]: a `getfield` / `putfield` inside
/// a `try` takes its null receiver into this method's handler as the array
/// accesses do. `=0` plans array and division sites only (wave 5).
pub fn ir_implicit_field_edges_enabled() -> bool {
    true
}

/// Can a row whose catch type is `name` (`""` = catch-all) take one of the
/// implicit exceptions `kinds` names? Decided by NAME, and that is exact: the
/// superclass chains of `NullPointerException`,
/// `ArrayIndexOutOfBoundsException` and `ArithmeticException` are boot
/// classes under `java/lang/`, a package no other loader may define, so a
/// catch type is a supertype of one of them exactly when its name is on that
/// chain. Used only to skip sites no row can take (their pad would always
/// miss); the runtime matcher still decides every throw.
pub(crate) fn implicit_catch_name_takes(name: &str, kinds: (bool, bool, bool)) -> bool {
    let (npe, aioobe, arith) = kinds;
    match name {
        "" | "java/lang/Throwable" | "java/lang/Exception" | "java/lang/RuntimeException" => {
            npe || aioobe || arith
        }
        "java/lang/NullPointerException" => npe,
        "java/lang/IndexOutOfBoundsException" | "java/lang/ArrayIndexOutOfBoundsException" => {
            aioobe
        }
        "java/lang/ArithmeticException" => arith,
        _ => false,
    }
}

/// The implicit-exception sites of `code[..code_len]` that may take implicit
/// exception edges (nio21 wave 5 L2; [`ir_implicit_exception_edges_enabled`]),
/// as `pc -> handler pcs` in table order: a normally reachable array access
/// or integer division ([`implicit_site_kinds`]) covered by at least one row,
/// at most [`IR_CALL_THROW_MAX_CANDIDATES`] of them, EVERY one with its
/// handler after the instruction -- all-or-nothing per site, for the reason
/// [`call_site_throw_plan`] gives -- and at least one of them a catch type
/// that can take what the instruction raises ([`implicit_catch_name_takes`]).
pub(in crate::ir) fn implicit_site_throw_plan(
    verified: &cratonvm_reader::VerifiedCode,
    reachable: &HashSet<usize>,
    code: &[u8],
    code_len: usize,
    rows: &[IrLocalThrowRow],
    names: &[&'static str],
) -> HashMap<usize, Vec<usize>> {
    let mut out = HashMap::new();
    if names.len() != rows.len() {
        return out;
    }
    let fields = ir_implicit_field_edges_enabled();
    for insn in verified.instructions() {
        let pc = insn.pc as usize;
        if pc >= code_len || !reachable.contains(&pc) {
            continue;
        }
        let Some(kinds) = code
            .get(pc)
            .copied()
            .filter(|&op| fields || !implicit_site_is_field(op))
            .and_then(implicit_site_kinds)
        else {
            continue;
        };
        let covering: Vec<(usize, &'static str)> = rows
            .iter()
            .zip(names.iter())
            .filter(|(r, _)| r.start_pc <= pc && pc < r.end_pc)
            .map(|(r, &n)| (r.handler_pc, n))
            .collect();
        if covering.is_empty()
            || covering.len() > IR_CALL_THROW_MAX_CANDIDATES
            || covering.iter().any(|&(h, _)| h <= pc || h >= code_len)
            || !covering
                .iter()
                .any(|&(_, n)| implicit_catch_name_takes(n, kinds))
        {
            continue;
        }
        out.insert(pc, covering.into_iter().map(|(h, _)| h).collect());
    }
    out
}


/// One cold throw arm of a method body (round 16 wave 7, lane arraylist): the
/// javac shape of `if (bad) throw new X(...);`,
///
/// ```text
///     branch_pc:  if<cond> L          ; taken = the check passed
///     start:      new X; dup; ...     ; the arm: straight-line code
///     athrow_pc:  athrow
///     L:          ...                 ; == athrow_pc + 1
/// ```
///
/// All three pcs are body-relative.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ColdThrowArm {
    /// The conditional branch that jumps around the arm.
    pub branch_pc: usize,
    /// The arm's first instruction: the branch's fall-through.
    pub start: usize,
    /// The arm's last instruction, its `athrow`.
    pub athrow_pc: usize,
}

/// A conditional branch EARLIER in the body that jumps straight to a cold
/// throw arm's first instruction (al16, 2026-10-05): the javac shape of
/// `if (a || b) throw ...`,
///
/// ```text
///     side_pc:    if<a> start         ; taken = the first check failed
///                 ...
///     branch_pc:  if<!b> L            ; the arm's own branch
///     start:      ...                 ; the arm
///     athrow_pc:  athrow
///     L:
/// ```
///
/// `Preconditions.checkIndex` (every `ArrayList.get` / `set` / `remove`), the
/// `Objects.check*` family and most JDK argument checks have it. Both pcs are
/// body-relative.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ColdThrowSideEntry {
    /// The conditional branch whose TAKEN edge enters the arm.
    pub side_pc: usize,
    /// The arm's first instruction ([`ColdThrowArm::start`]).
    pub arm_start: usize,
}

/// [`ColdThrowArm::branch_pc`] of a TAIL arm: one with no branch of its own,
/// entered only by earlier conditional branches' taken edges and placed after
/// an instruction that does not fall through -- javac's shape of
///
/// ```text
///     if (x != null) return x;            ifnull L; aload x; areturn
///     throw new IllegalStateException();  L: new ...; athrow
/// ```
///
/// netty's `AdaptiveByteBuf.rootParent()`, under every pooled `ByteBuf`
/// access. Every way in is a [`ColdThrowSideEntry`], which
/// [`IrBuilder::prune_cold_throw_side_entry`] turns into a guard, so the masked
/// arm is never reached; no branch's pc is ever this value.
pub const COLD_THROW_TAIL_ARM: usize = usize::MAX;

/// `CRATONVM_JIT_IR_COLD_THROW_TAIL_ARMS` (netty compression wall, 2026-10-14;
/// default ON, `=0` is the kill switch): admit [`COLD_THROW_TAIL_ARM`] shapes.
/// Needs the side entries ([`ir_cold_throw_side_entries_enabled`]), which are
/// its only ways in. Read once per resolved site / per build.
pub fn ir_cold_throw_tail_arms_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_COLD_THROW_TAIL_ARM_DEAD_SKIP` (JIT round 23 wave 8e, lane
/// W8-L1, proxy-define page; perf, **default ON since j27 2026-10-28**, `=0`
/// kills): the builder steps over
/// a spliced multi-return body's [`COLD_THROW_TAIL_ARM`] when it reaches it
/// with dead control (every way in a guard), instead of refusing the whole
/// method at "no control" (`IrBuilder::pc_in_dead_tail_cold_throw_arm`). Read
/// per pc of dead control in a splice.
///
/// Was opt-in after the n23-w8g A/B (pinned, 3 reps, `Zs9Nd8ProxyCost room 3000`,
/// main-thread µs/define): default 1875/1661/2239 vs `=0` 1848/1953/1890 --
/// neutral on a noisy host.
pub fn ir_cold_throw_tail_arm_dead_skip_enabled() -> bool {
    // WARNING j27-user-flip-20261028 (default ON; =0 restores the old default)
    true
}

/// `CRATONVM_JIT_IR_COLD_THROW_SIDE_ENTRIES` (al16, 2026-10-05; default ON,
/// `=0` is the kill switch): admit [`ColdThrowSideEntry`] shapes to the
/// cold-throw-arm splice. Off, an arm some other branch targets refuses the
/// body, as it did in round 16. Read once per resolved site / per build.
pub fn ir_cold_throw_side_entries_enabled() -> bool {
    true
}

/// Every `athrow` of `code[..code_len]` as a [`ColdThrowArm`], or `None` when
/// any one is not the last instruction of such an arm:
///
/// * the arm is entered ONLY by falling through a conditional branch
///   (`if*`, `if_icmp*`, `if_acmp*`, `ifnull`, `ifnonnull`) -- no branch,
///   switch or anything else targets a pc inside it, and it is not the
///   method's first instruction;
/// * the arm is straight-line: nothing between the branch and the `athrow`
///   transfers control;
/// * the branch's target is the instruction right after the `athrow`.
///
/// `None` too when the body does not decode, or names a target the strict
/// decoder cannot (a `jsr`/`ret`). `Some(empty)` for a body with no `athrow`.
///
/// "Cold" is a property of the SHAPE here, the one C2 assumes for a throw
/// path absent a profile: a wrong guess costs a deopt and a recompile, never a
/// wrong answer, because the arm runs in the interpreter.
pub fn cold_throw_arms(code: &[u8], code_len: usize) -> Option<Vec<ColdThrowArm>> {
    cold_throw_shape(code, code_len, ir_cold_throw_side_entries_enabled()).map(|(arms, _)| arms)
}

/// Cold throw arms for String-intrinsic unpinning: matches any straight-line
/// fall-through throw arm under a forward conditional branch even when the
/// branch skips over a following else-block (`target >= athrow.next_pc()`) or
/// another unrelated `athrow` in the same method has a multi-block shape.
pub fn cold_throw_arms_for_pin(code: &[u8], code_len: usize) -> Vec<ColdThrowArm> {
    cold_throw_shape_impl(code, code_len, ir_cold_throw_side_entries_enabled(), false)
        .map(|(arms, _)| arms)
        .unwrap_or_default()
}

/// [`cold_throw_arms`] plus the arms' side entries ([`ColdThrowSideEntry`]).
///
/// With `side_entries` off this is the round-16 rule: any branch into an arm
/// refuses the body. On, the arm's FIRST instruction may also be the target of
/// conditional branches that sit before the arm's own branch, and only of
/// those; any other way into the arm (a `goto`, a switch, a backward or later
/// branch, a jump into its middle) still refuses the body.
pub fn cold_throw_shape(
    code: &[u8],
    code_len: usize,
    side_entries: bool,
) -> Option<(Vec<ColdThrowArm>, Vec<ColdThrowSideEntry>)> {
    cold_throw_shape_impl(code, code_len, side_entries, true)
}

fn cold_throw_shape_impl(
    code: &[u8],
    code_len: usize,
    side_entries: bool,
    strict: bool,
) -> Option<(Vec<ColdThrowArm>, Vec<ColdThrowSideEntry>)> {
    use crate::bytecode_analysis as ba;
    let len = code_len.min(code.len());
    let insns = ba::decode_method(code, len)?;
    if !insns.iter().any(|i| i.opcode == 0xbf) {
        return Some((Vec::new(), Vec::new()));
    }
    let is_cond = |op: u8| matches!(op, 0x99..=0xa6 | 0xc6 | 0xc7);
    // Every explicit transfer into each pc: (source pc, source opcode).
    let mut sources: Vec<Vec<(usize, u8)>> = vec![Vec::new(); len];
    let mut targets: Vec<usize> = Vec::new();
    for insn in &insns {
        targets.clear();
        if !ba::explicit_targets(code, len, insn.pc, &mut targets) {
            return None;
        }
        for &t in &targets {
            sources.get_mut(t)?.push((insn.pc, insn.opcode));
        }
    }
    let mut arms = Vec::new();
    let mut sides = Vec::new();
    for (k, insn) in insns.iter().enumerate() {
        if insn.opcode != 0xbf {
            continue;
        }
        // Walk back from the `athrow` to the branch the arm falls out of.
        let mut j = k;
        let arm: Option<ColdThrowArm> = loop {
            let (Some(cur), Some(prev)) = (insns.get(j), j.checked_sub(1).and_then(|p| insns.get(p)))
            else {
                if strict { return None; }
                break None;
            };
            let Some(into) = sources.get(cur.pc) else {
                if strict { return None; }
                break None;
            };
            // A tail arm ([`COLD_THROW_TAIL_ARM`]): nothing falls into it, and
            // every way in is an earlier conditional branch's taken edge.
            if side_entries
                && !into.is_empty()
                && !ba::falls_through(prev.opcode)
                && ir_cold_throw_tail_arms_enabled()
            {
                if !into.iter().all(|&(src, op)| is_cond(op) && src < cur.pc) {
                    if strict { return None; }
                    break None;
                }
                sides.extend(into.iter().map(|&(src, _)| ColdThrowSideEntry {
                    side_pc: src,
                    arm_start: cur.pc,
                }));
                break Some(ColdThrowArm {
                    branch_pc: COLD_THROW_TAIL_ARM,
                    start: cur.pc,
                    athrow_pc: insn.pc,
                });
            }
            if !into.is_empty() {
                // Only the arm's first instruction may be entered, and only
                // by an earlier conditional branch (checked once `prev` is
                // known to be the arm's own branch).
                if !side_entries || !is_cond(prev.opcode) {
                    if strict { return None; }
                    break None;
                }
            }
            if is_cond(prev.opcode) {
                if !into.iter().all(|&(src, op)| is_cond(op) && src < prev.pc) {
                    if strict { return None; }
                    break None;
                }
                sides.extend(into.iter().map(|&(src, _)| ColdThrowSideEntry {
                    side_pc: src,
                    arm_start: cur.pc,
                }));
                break Some(ColdThrowArm {
                    branch_pc: prev.pc,
                    start: cur.pc,
                    athrow_pc: insn.pc,
                });
            }
            if !ba::falls_through(prev.opcode) || ba::is_offset_branch(prev.opcode) {
                if strict { return None; }
                break None;
            }
            j -= 1;
        };
        let Some(arm) = arm else {
            continue;
        };
        if arm.branch_pc != COLD_THROW_TAIL_ARM {
            let Some(target) = ba::offset_branch_target(code, arm.branch_pc) else {
                if strict { return None; }
                continue;
            };
            let target_ok = if strict {
                target == insn.next_pc()
            } else {
                target >= insn.next_pc()
            };
            if !target_ok {
                if strict { return None; }
                continue;
            }
        }
        arms.push(arm);
    }
    Some((arms, sides))
}

/// [`mask_cold_throw_arms`], except that with `keep_static_values` an arm
/// that is exactly `getstatic; athrow` -- it throws a value it does not
/// construct -- is left unmasked, for a spliced throw edge to take
/// (`CRATONVM_JIT_IR_SPLICED_THROW_EDGES`; the builder still traps such an
/// arm where no edge can take it). Its `athrow` pc is returned either way.
pub fn mask_cold_throw_arms_keeping(
    code: &mut [u8],
    code_len: usize,
    keep_static_values: bool,
) -> Option<Vec<usize>> {
    let arms = cold_throw_arms(code, code_len)?;
    let mut athrows = Vec::with_capacity(arms.len());
    for arm in &arms {
        if keep_static_values
            && arm.athrow_pc == arm.start + 3
            && code.get(arm.start).copied() == Some(0xb2)
        {
            athrows.push(arm.athrow_pc);
            continue;
        }
        if arm.start < arm.athrow_pc {
            let body = code.get_mut(arm.start..arm.athrow_pc)?;
            body.fill(0x00);
            if let Some(first) = body.first_mut() {
                *first = 0x01;
            }
        }
        athrows.push(arm.athrow_pc);
    }
    Some(athrows)
}

/// `CRATONVM_JIT_IR_SELF_SPLICE_BASE_ARM_KEEPS_CALL=1` -- JIT round 19 wave 4,
/// lane throws4 (THROWS3-4 of `docs/feature-designs/jit-ir-optimizations.md`).
/// **default on.** Read once per resolved IR splice site that has an `athrow`.
///
/// A self-recursive body whose cold throw arm guards the recursion's own
/// base case (`static void down(int d) { if (d <= 1) throw E; down(d - 1); }`)
/// is not cold: every top-level call that reaches the bottom of the recursion
/// enters it. Masked and spliced six deep, each throw then deopts (a precise
/// chain resume) until the per-bci de-spec withdraws the call site after four
/// deopts -- eight compiles and four deopts of warm-up for the call site the
/// policy ends up keeping anyway. On, the inline resolver refuses such a body
/// up front ([`cold_throw_arm_tests_recursion_parameter`]), so the site is the
/// call it ends as from the first compile.
pub fn ir_self_splice_base_arm_keeps_call_enabled() -> bool {
    true
}

/// THROWS3-4 (lane throws4): does one of the cold throw arms of the
/// self-recursive body `code[..code_len]` test the recursion's controlling
/// parameter? All of:
///
/// * the body calls itself (`self_invoke(cp_idx)` answers `true` for one of
///   its `invoke*` instructions);
/// * the arm's branch compares an `int` parameter local (`iload k`, `k <
///   param_slots`) -- alone (`if<cond>`), or against a constant or another
///   local (`if_icmp<cond>`), loaded by the one or two instructions right
///   before the branch;
/// * the body steps that local: `iinc k`, or `iload k` followed by a constant
///   load and an `iadd` / `isub` (the argument `d - 1` of the recursive call).
///
/// A shape test, not a proof: `false` keeps today's masking, `true` costs the
/// site its splice (the call the per-bci de-spec would have left anyway).
pub fn cold_throw_arm_tests_recursion_parameter(
    code: &[u8],
    code_len: usize,
    param_slots: usize,
    self_invoke: &dyn Fn(u16) -> bool,
) -> bool {
    use crate::bytecode_analysis as ba;
    let len = code_len.min(code.len());
    let Some(arms) = cold_throw_arms(code, len) else {
        return false;
    };
    if arms.is_empty() {
        return false;
    }
    let Some(insns) = ba::decode_method(code, len) else {
        return false;
    };
    let calls_itself = insns.iter().any(|i| {
        matches!(i.opcode, 0xb6..=0xb9)
            && code
                .get(i.pc + 1..i.pc + 3)
                .is_some_and(|b| self_invoke(u16::from_be_bytes([b[0], b[1]])))
    });
    if !calls_itself {
        return false;
    }
    // The int local an `iload` instruction reads, if it is one.
    let iload_slot = |i: &ba::Insn| -> Option<usize> {
        match i.opcode {
            0x1a..=0x1d => Some(usize::from(i.opcode - 0x1a)),
            0x15 => code.get(i.pc + 1).map(|&b| usize::from(b)),
            _ => None,
        }
    };
    let is_const = |op: u8| matches!(op, 0x02..=0x08 | 0x10 | 0x11);
    // The body steps local `k` towards the base case.
    let steps = |k: usize| -> bool {
        insns.iter().enumerate().any(|(n, i)| {
            if i.opcode == 0x84 {
                return code.get(i.pc + 1).is_some_and(|&b| usize::from(b) == k);
            }
            iload_slot(i) == Some(k)
                && insns.get(n + 1).is_some_and(|c| is_const(c.opcode))
                && insns
                    .get(n + 2)
                    .is_some_and(|o| matches!(o.opcode, 0x60 | 0x64))
        })
    };
    arms.iter().any(|arm| {
        let Some(b) = insns.iter().position(|i| i.pc == arm.branch_pc) else {
            return false;
        };
        let branch = insns[b].opcode;
        let operands: Vec<&ba::Insn> = match branch {
            0x99..=0x9e => insns.get(b.wrapping_sub(1)).into_iter().collect(),
            0x9f..=0xa4 => [b.wrapping_sub(2), b.wrapping_sub(1)]
                .iter()
                .filter_map(|&n| insns.get(n))
                .collect(),
            _ => return false,
        };
        operands.iter().any(|op| {
            iload_slot(op).is_some_and(|k| k < param_slots && steps(k))
                && operands
                    .iter()
                    .all(|o| iload_slot(o).is_some() || is_const(o.opcode))
        })
    })
}


/// Round nio20 wave 1, lane L1 (NIO19-1): `CRATONVM_JIT_IR_SPLICE_FENCE_FINALLY`,
/// **default ON** (`=0` restores the `callee-exception-table` refusal for
/// every callee with a handler).
///
/// On: the optimizing tier's inline resolver admits a callee whose exception
/// table is exactly a javac `finally { Reference.reachabilityFence(..); }`
/// ([`strip_fence_finally_handler`]): every handler row a catch-all into ONE
/// handler `astore t; (aload s; invokestatic Reference.reachabilityFence)+;
/// aload t; athrow`. Running that handler is propagating the exception (the
/// fence has no code), so the spliced copy drops it and an exception raised in
/// the spliced range takes the route every spliced throw takes: a call's
/// exception exit stamps the caller's `invoke` bci and the caller's own table
/// answers it; a guard deopts through the chain into the callee's own frame,
/// where the interpreter runs the real handler. The `ScopedMemoryAccess`
/// `*Internal` family and every `try { .. } finally { reachabilityFence(x) }`
/// in `java.nio` have this shape. Read once per resolved site.
pub fn ir_splice_fence_finally_enabled() -> bool {
    true
}

/// The most `aload s; invokestatic reachabilityFence` pairs one fence handler
/// may carry (`ByteBuffer.putBuffer` fences two references).
pub(in crate::ir) const FENCE_FINALLY_MAX_FENCES: usize = 8;

/// A callee body with its `reachabilityFence` `finally` handler stripped, for
/// the optimizing tier's splice ([`strip_fence_finally_handler`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FenceFinallyStrip {
    /// The copy to splice, exactly [`Self::code_len`] bytes (no padding). Only
    /// this COPY is stripped: the interpreter, which a deopt resumes into,
    /// keeps running the method's own code and exception table.
    pub code: Vec<u8>,
    /// The copy's length: the original length when the handler sat between a
    /// `goto` and the code it jumps to (the goto and the handler became
    /// `nop`s), or the handler's pc when the handler was the method's tail
    /// (the copy is cut there).
    pub code_len: usize,
    /// The handler's first instruction (its `astore`).
    pub handler_pc: usize,
    /// One past the handler's `athrow`.
    pub handler_end: usize,
    /// The locals the handler fences, in handler order.
    pub fenced_locals: Vec<usize>,
}

/// The local an `aload` / `aload_<n>` / `wide aload` reads, or `None`.
pub(in crate::ir) fn fence_finally_aload_local(code: &[u8], insn: &crate::bytecode_analysis::Insn) -> Option<usize> {
    match (insn.wide, insn.op) {
        (false, 0x19) => code.get(insn.pc + 1).map(|&b| usize::from(b)),
        (true, 0x19) => Some(usize::from(u16::from_be_bytes([
            *code.get(insn.pc + 2)?,
            *code.get(insn.pc + 3)?,
        ]))),
        (false, op @ 0x2a..=0x2d) => Some(usize::from(op - 0x2a)),
        _ => None,
    }
}

/// The local a store (any type, `wide` included) or an `iinc` writes, and
/// whether the store is category 2 (it writes the next slot too), or `None`.
pub(in crate::ir) fn fence_finally_store_local(
    code: &[u8],
    insn: &crate::bytecode_analysis::Insn,
) -> Option<(usize, bool)> {
    match insn.op {
        0x36..=0x3a | 0x84 => {
            let slot = if insn.wide {
                usize::from(u16::from_be_bytes([
                    *code.get(insn.pc + 2)?,
                    *code.get(insn.pc + 3)?,
                ]))
            } else {
                usize::from(*code.get(insn.pc + 1)?)
            };
            Some((slot, matches!(insn.op, 0x37 | 0x39)))
        }
        // istore_<n> .. astore_<n>, four per type: i, l, f, d, a.
        op @ 0x3b..=0x4e => {
            let k = op - 0x3b;
            Some((usize::from(k % 4), matches!(k / 4, 1 | 3)))
        }
        _ => None,
    }
}

/// Recognise a javac `finally { Reference.reachabilityFence(s..); }` as a
/// callee's whole exception table and return the body with the handler
/// stripped ([`FenceFinallyStrip`]), or `None` -- refuse, as before -- for any
/// other table (round nio20 wave 1, lane L1, NIO19-1).
///
/// `rows` is the exception table as `(start_pc, end_pc, handler_pc,
/// catch_type)`; `is_fence_call` answers whether a `Methodref` constant-pool
/// index names `java/lang/ref/Reference.reachabilityFence(Ljava/lang/Object;)V`.
///
/// Admitted only when ALL of these hold, each the reason the strip is exact:
///
/// * every row is a catch-all (`catch_type == 0`) into one `handler_pc`;
/// * the handler is `astore t; (aload s_i; invokestatic fence)+; aload t;
///   athrow` (1 to 8 fences, `s_i != t`): running it is propagating the
///   exception, so a spliced throw may propagate instead;
/// * each row either ends at or before the handler (the protected body) or
///   lies inside the handler (javac's self-covering row for its `astore`), and
///   at least one is of the first kind;
/// * nothing branches into the handler and the instruction before it does not
///   fall through: it is entered by an exception edge only;
/// * the handler is the method's tail (the copy is cut at it), or it sits
///   between a `goto` and exactly the instruction that goto jumps to (the
///   goto and the handler become `nop`s, so the copy falls through where the
///   method jumped); in that case the protected rows end at or before the goto;
/// * every fenced local is LIVE (read before written on some normal path) at
///   every instruction the protected rows cover, in the stripped copy. The
///   handler reads exactly those locals and no other (it writes `t` before
///   reading it), so the snapshot-liveness prune, which models no handler for
///   a spliced body, cannot cut a local the interpreter's real handler reads
///   after a deopt into this frame. javac's duplicated normal-path `finally`
///   is what makes this hold.
pub fn strip_fence_finally_handler(
    code: &[u8],
    code_len: usize,
    rows: &[(usize, usize, usize, u16)],
    is_fence_call: &dyn Fn(u16) -> bool,
) -> Option<FenceFinallyStrip> {
    use crate::bytecode_analysis as ba;
    let len = code_len.min(code.len());
    let &(_, _, h, _) = rows.first()?;
    if len == 0 || rows.iter().any(|&(_, _, hp, ct)| hp != h || ct != 0) {
        return None;
    }
    let insns = ba::decode_method(code, len)?;
    let k = insns.iter().position(|i| i.pc == h)?;
    if k == 0 {
        return None;
    }
    // `astore t`.
    let first = insns.get(k)?;
    let t = match (first.wide, first.op) {
        (false, 0x3a) => usize::from(*code.get(first.pc + 1)?),
        (true, 0x3a) => usize::from(u16::from_be_bytes([
            *code.get(first.pc + 2)?,
            *code.get(first.pc + 3)?,
        ])),
        (false, op @ 0x4b..=0x4e) => usize::from(op - 0x4b),
        _ => return None,
    };
    // `(aload s; invokestatic fence)+; aload t; athrow`.
    let mut fenced: Vec<usize> = Vec::new();
    let mut j = k + 1;
    let athrow = loop {
        let load = insns.get(j)?;
        let next = insns.get(j + 1)?;
        let local = fence_finally_aload_local(code, load)?;
        if local == t {
            if next.opcode != 0xbf {
                return None;
            }
            break *next;
        }
        if next.opcode != 0xb8 {
            return None;
        }
        let cp = u16::from_be_bytes([*code.get(next.pc + 1)?, *code.get(next.pc + 2)?]);
        if !is_fence_call(cp) {
            return None;
        }
        fenced.push(local);
        if fenced.len() > FENCE_FINALLY_MAX_FENCES {
            return None;
        }
        j += 2;
    };
    if fenced.is_empty() {
        return None;
    }
    let h_end = athrow.next_pc();
    let mut covers_body = false;
    for &(start, end, _, _) in rows {
        if start >= end {
            return None;
        }
        if end <= h {
            covers_body = true;
        } else if !(start >= h && end <= h_end) {
            return None;
        }
    }
    if !covers_body {
        return None;
    }
    // Entered by an exception edge only.
    let mut targets: Vec<usize> = Vec::new();
    for insn in &insns {
        targets.clear();
        if !ba::explicit_targets(code, len, insn.pc, &mut targets) {
            return None;
        }
        if targets.iter().any(|&to| to >= h && to < h_end) {
            return None;
        }
    }
    let prev = *insns.get(k - 1)?;
    if ba::falls_through(prev.opcode) {
        return None;
    }
    let (copy, new_len, body_limit) = if h_end == len {
        (code.get(..h)?.to_vec(), h, h)
    } else if prev.opcode == 0xa7
        && prev.next_pc() == h
        && ba::offset_branch_target(code, prev.pc) == Some(h_end)
    {
        let mut copy = code.get(..len)?.to_vec();
        copy.get_mut(prev.pc..h_end)?.fill(0x00);
        (copy, len, prev.pc)
    } else {
        return None;
    };
    if rows
        .iter()
        .any(|&(_, end, _, _)| end <= h && end > body_limit)
    {
        return None;
    }
    // Liveness of each fenced local over the stripped copy's normal CFG.
    let cins = ba::decode_method(&copy, new_len)?;
    let mut index_of = vec![usize::MAX; new_len];
    for (i, insn) in cins.iter().enumerate() {
        *index_of.get_mut(insn.pc)? = i;
    }
    let mut succs: Vec<Vec<usize>> = Vec::with_capacity(cins.len());
    let mut out: Vec<usize> = Vec::new();
    for insn in &cins {
        out.clear();
        if !ba::normal_successors(&copy, new_len, insn.pc, &mut out) {
            return None;
        }
        let mut row = Vec::with_capacity(out.len());
        for &s in &out {
            let i = *index_of.get(s)?;
            if i == usize::MAX {
                return None;
            }
            row.push(i);
        }
        succs.push(row);
    }
    for &s in &fenced {
        let mut live = vec![false; cins.len()];
        // Monotone from all-false: a pass either raises a bit or ends it.
        let mut converged = false;
        for _ in 0..=cins.len() {
            let mut changed = false;
            for i in (0..cins.len()).rev() {
                let insn = &cins[i];
                let uses = fence_finally_aload_local(&copy, insn) == Some(s);
                let defs = fence_finally_store_local(&copy, insn)
                    .is_some_and(|(slot, two)| slot == s || (two && slot + 1 == s));
                let v = uses || (!defs && succs[i].iter().any(|&n| live[n]));
                if v != live[i] {
                    live[i] = v;
                    changed = true;
                }
            }
            if !changed {
                converged = true;
                break;
            }
        }
        if !converged {
            return None;
        }
        for &(start, end, _, _) in rows {
            if end > h {
                continue;
            }
            if cins
                .iter()
                .enumerate()
                .any(|(i, insn)| insn.pc >= start && insn.pc < end && !live[i])
            {
                return None;
            }
        }
    }
    Some(FenceFinallyStrip {
        code: copy,
        code_len: new_len,
        handler_pc: h,
        handler_end: h_end,
        fenced_locals: fenced,
    })
}

/// Round nio20 wave 1, lane L1 (NIO19-3):
/// `CRATONVM_JIT_IR_SPLICE_COLD_UNLOADED_TYPECHECK`, **default ON** (`=0`
/// restores the `ir-splice-typecheck-target-not-loaded` refusal).
///
/// On: a callee whose `checkcast` / `instanceof` names a class not loaded yet
/// is still spliced into an optimizing-tier caller when the callee's own
/// branch profile proves that the instruction sits behind a branch edge that
/// never ran ([`splice_typecheck_arm_unexecuted`]). The site becomes a
/// PRECISE exit -- an `Op::Guard` that always fails, resumed through the
/// splice's chain snapshot into the callee's own frame AT the type check
/// (the unloaded-static-call exit's mechanism) -- so resolution, class loading
/// and the check itself all run in the interpreter. `Buffer.session()` on a
/// heap buffer is the shape (`segment != null ? ((AbstractMemorySegmentImpl)
/// segment).sessionImpl() : null` in a program that never used FFM). Read
/// once per resolved site.
pub fn ir_splice_cold_unloaded_typecheck_enabled() -> bool {
    true
}

/// Round nio20 wave 2, lane L3 (proposal L1-2 of `jit-r20-nio-proposals-L1.md`,
/// page `r20-precise-splice-exits-reinterpret-on-a-live-arm-FIXED-20261007.md`):
/// stamp `DeoptReason::ClassLoading` on the precise splice exits for an
/// unloaded class (static call owner, `checkcast` / `instanceof` target), so
/// the FIRST trap through a live arm evicts and recompiles
/// (`DeoptHistory::recommend_action`) instead of every call leaving compiled
/// code until the charged-trap count escalates. The recompile finds the class
/// loaded (the interpreter ran the site) and splices the arm normally.
/// `CRATONVM_JIT_IR_CLASS_LOADING_SPLICE_EXITS`, **default ON** (`=0` keeps
/// the old `UncommonTrap` stamp). Read once per planted exit.
pub fn ir_class_loading_splice_exits_enabled() -> bool {
    true
}

/// Round nio20 wave 2, lane L3 (`R17NioRows utfencode`; page
/// `r20-utf8-encoder-buffer-loop-per-char-calls-CLOSED-20261012.md`): an
/// `invokeinterface` INSIDE a spliced body whose receivers were one class only
/// -- `StringCharBuffer.get()`'s `str.charAt(..)`, spliced into
/// `UTF_8$Encoder.encodeBufferLoop`, a `String` on every call -- is spliced
/// behind an exact class test whose miss edge deopts through the chain
/// snapshot at the call ([`IrBuilder::nested_trap_guard_plan`]); the VM's
/// resolver produces such a row only under this switch (`jit_bridge.rs`
/// `ir_nested_trap_receiver_class`, HotSpot's trap-on-miss evidence bar).
/// Without it the call stays an inline-cached interface call per char.
/// `CRATONVM_JIT_IR_SPLICE_NESTED_PROFILE_TRAP`, **default OFF** (`=1` opts
/// in; A/B pending: a receiver the profile never saw costs a deopt and an
/// eviction, bounded by the charged-trap policy). Read per resolved nested
/// site and per planned build site.
pub fn ir_splice_nested_profile_trap_enabled() -> bool {
    true
}

/// Round nio20 wave 3, lane L4 (proposal L3W2-3 of
/// `jit-r20-nio-proposals-w2-L3.md`): the same switch also covers a
/// NEST-EXACT `invokevirtual` row (round 13 wave 10, HS-2) whose receiver the
/// builder cannot prove exact. Without it such a row splices only for a
/// proven receiver and lowers the kept call otherwise; with it the unproven
/// site is spliced behind `Guard(ExactClassIs(receiver, C))` whose miss edge
/// deopts through the chain snapshot at the call
/// ([`IrBuilder::nested_trap_guard_plan`]). Under the switch the VM's resolver
/// keeps a nest-exact row only where the callee's receiver profile and its
/// compiled artifact's inline cache name no OTHER class at that bci
/// (`jit_bridge.rs` `ir_nested_receiver_evidence_contradicts`), so a site
/// known to see subclasses stays a call instead of trapping. One switch, not
/// two: both rows are the same mechanism and the same risk (a receiver the
/// evidence never saw traps), and A/B-ing them apart buys nothing the
/// `[ir] nested profile trap guard` lines (which name the opcode) do not.
pub fn ir_splice_nested_exact_trap_enabled() -> bool {
    ir_splice_nested_profile_trap_enabled()
}

/// Is the instruction at `target` of `code[..code_len]` reachable from pc 0
/// only through one conditional-branch edge that `counts` says never ran,
/// while the same branch's other edge ran at least `min_samples` times?
///
/// `counts(pc)` is `(taken, not_taken)` of the conditional branch at `pc` (the
/// interpreter's branch profile, keyed by the method's own bcis), `None` when
/// there is no observation. `false` -- not proven -- for an undecodable body,
/// a `jsr`/`ret`/switch the strict successor function refuses, a target that
/// is not an instruction or not reachable at all, and a branch whose two
/// edges coincide. Normal edges only: the caller asks it of a body with no
/// handler (or one whose handler was stripped, which reaches no type check).
pub fn pc_behind_unexecuted_branch_edge(
    code: &[u8],
    code_len: usize,
    target: usize,
    counts: &dyn Fn(usize) -> Option<(u32, u32)>,
    min_samples: u32,
) -> bool {
    use crate::bytecode_analysis as ba;
    let len = code_len.min(code.len());
    if target >= len {
        return false;
    }
    let Some(insns) = ba::decode_method(code, len) else {
        return false;
    };
    if !insns.iter().any(|i| i.pc == target) {
        return false;
    }
    let mut succ: HashMap<usize, Vec<usize>> = HashMap::with_capacity(insns.len());
    let mut out: Vec<usize> = Vec::new();
    for insn in &insns {
        out.clear();
        if !ba::normal_successors(code, len, insn.pc, &mut out) {
            return false;
        }
        succ.insert(insn.pc, out.clone());
    }
    let reaches_without_all = |cuts: &[(usize, usize)]| -> bool {
        let mut seen = vec![false; len];
        let mut work = vec![0usize];
        while let Some(pc) = work.pop() {
            if pc >= len || seen[pc] {
                continue;
            }
            seen[pc] = true;
            if pc == target {
                return true;
            }
            if let Some(next) = succ.get(&pc) {
                for &n in next {
                    if !cuts.contains(&(pc, n)) {
                        work.push(n);
                    }
                }
            }
        }
        false
    };
    let reaches_without = |cut: Option<(usize, usize)>| -> bool {
        match cut {
            Some(cut) => reaches_without_all(&[cut]),
            None => reaches_without_all(&[]),
        }
    };
    if !reaches_without(None) {
        return false;
    }
    // Round rx27 wave 6 (lane chain, `CRATONVM_JIT_IR_SPLICE_COLD_ARM_EDGE_SET`):
    // every edge the profile proves never ran, cut together.
    let mut all_cuts: Vec<(usize, usize)> = Vec::new();
    for insn in &insns {
        if !matches!(insn.opcode, 0x99..=0xa6 | 0xc6 | 0xc7) {
            continue;
        }
        let Some((taken, not_taken)) = counts(insn.pc) else {
            continue;
        };
        let Some(branch_to) = ba::offset_branch_target(code, insn.pc) else {
            continue;
        };
        let fall = insn.next_pc();
        if branch_to == fall {
            continue;
        }
        let cut = if taken == 0 && not_taken >= min_samples {
            (insn.pc, branch_to)
        } else if not_taken == 0 && taken >= min_samples {
            (insn.pc, fall)
        } else {
            continue;
        };
        if !reaches_without(Some(cut)) {
            return true;
        }
        all_cuts.push(cut);
    }
    // A target reached only through SEVERAL never-run edges
    // (`Pattern.curly`'s `new Curly` at pc 52: the `qtype != GREEDY` edge
    // and the `!(prev instanceof CharProperty)` edge behind the
    // `!(prev instanceof BmpCharProperty)` edge). Each edge alone leaves the
    // other path; together they cut every path. Same evidence per edge, so
    // the same claim: no execution that reached this method's profile took
    // any of them.
    all_cuts.len() > 1 && splice_cold_arm_edge_set_enabled() && !reaches_without_all(&all_cuts)
}

/// Round rx27 wave 6 (lane chain): `CRATONVM_JIT_IR_SPLICE_COLD_ARM_EDGE_SET`,
/// **default ON** since 2026-11-09 (`=0` kills it; `compile` -98 instructions/op). [`pc_behind_unexecuted_branch_edge`] also cuts every proven
/// never-run edge at once, so a target behind two or more such edges (one per
/// path) is proven cold too. Before, it tried one edge at a time, so
/// `Pattern.curly`'s unloaded `new Curly` (reached by two never-run edges)
/// stayed `ir-splice-new-site-unresolved` and `curly` stayed a call. Only the
/// proof's reach changes: the site is still a precise class-loading exit (an
/// unloaded `new`) or a type-check trap, as before. Read per question.
pub fn splice_cold_arm_edge_set_enabled() -> bool {
    true
}

/// Observations of the edge that RAN before [`splice_typecheck_arm_unexecuted`]
/// calls the other edge cold. Lower than the layout hints' 20
/// ([`crate::profile::BRANCH_BIAS_MIN_SAMPLES`]) on purpose: a small callee is
/// recorded only between its nomination and its first published body, often a
/// few dozen executions, and a wrong "never" costs a precise exit into the
/// interpreter (then a recompile), never a wrong answer.
pub const SPLICE_COLD_ARM_MIN_SAMPLES: u32 = 8;

/// [`pc_behind_unexecuted_branch_edge`] over a callee's profile snapshot
/// (`ProfileStore::get_profile`), with [`SPLICE_COLD_ARM_MIN_SAMPLES`] as the
/// bar for the edge that ran. `method_code_len` is the method's own
/// `code_length`: a profile stamped with another length (another version of
/// the method, or a conflict) proves nothing.
pub fn splice_typecheck_arm_unexecuted(
    code: &[u8],
    code_len: usize,
    method_code_len: usize,
    target: usize,
    profile: &crate::profile::MethodProfile,
) -> bool {
    let stamped = profile.recorded_code_len();
    if stamped != 0 && usize::try_from(stamped).ok() != Some(method_code_len) {
        return false;
    }
    pc_behind_unexecuted_branch_edge(
        code,
        code_len,
        target,
        &|pc: usize| profile.branches.get(&pc).map(|c| (c.taken, c.not_taken)),
        SPLICE_COLD_ARM_MIN_SAMPLES,
    )
}

/// Round rx27 wave 1 (lane matcher): `CRATONVM_JIT_IR_SPLICE_COLD_HANDLER_ARM`,
/// **default ON** since the rx27 bundle flip of 2026-10-29 (`=0` kills it;
/// evidence at `crate::rx27_chain_entry::bundle_flip_evidence`). A callee whose exception table has a
/// catch-all row (a `synchronized` block or a `finally`, which the
/// handler splice never admits) is spliced into an optimizing-tier caller
/// when every row lies inside one conditional branch's fall-through arm and
/// the callee's own profile says that arm never ran
/// ([`cold_handler_arm_mask`], [`splice_cold_handler_arm_unexecuted`]).
/// `Pattern.matcher`'s compile guard is the shape: `if (!compiled) {
/// synchronized (this) { if (!compiled) compile(); } }`, `compiled` true
/// after the constructor. Read per resolved site with an exception table.
pub fn ir_splice_cold_handler_arm_enabled() -> bool {
    // rx27: default ON since the 2026-10-29 bundle flip
    // (`rx27_chain_entry::bundle_flip_evidence`), `=0` kills it.
    true
}

/// The arm [`cold_handler_arm_mask`] masked: `branch_pc` is the conditional
/// branch that jumps around it, `[start, end)` its bytes (`start` the
/// branch's fall-through, `end` the branch's target). Body-relative.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ColdHandlerArm {
    pub branch_pc: usize,
    pub start: usize,
    pub end: usize,
}

/// Round rx27 wave 1 (lane matcher, [`ir_splice_cold_handler_arm_enabled`]):
/// the spliced copy of a callee whose exception rows (`(start, end,
/// handler)`, body-relative) all lie inside ONE conditional branch's
/// fall-through arm, with that arm masked to `aconst_null; nop...; athrow`
/// -- a [`cold_throw_arms`] shape, so the resolver's cold-throw pass admits
/// it and [`IrBuilder::prune_cold_throw_arm`] turns its branch into a guard
/// that deopts AT the branch. The copy carries no monitor instruction, no
/// handler and no row; the interpreter, which a failing guard resumes in the
/// callee's own frame at the branch, runs the method's own code and table.
///
/// Shape only: whether the arm is cold is the profile's question
/// ([`splice_cold_handler_arm_unexecuted`], asked by the resolver once its
/// class-manager lock is dropped). `Err` names why there is no such arm:
///
/// * the body does not decode, or has a `jsr` / `ret`;
/// * no conditional branch jumps forward over an arm of at least two bytes
///   that holds every row's range and handler (the innermost such arm is
///   chosen);
/// * something outside the arm transfers into it (only the branch's
///   fall-through may enter);
/// * masked, the body has another `athrow` that is not a cold throw arm, so
///   the resolver would not take the mask (it would refuse the body anyway);
/// * the masked body has a back edge: a loop root keeps its call and its own
///   compile, the W13-6 rule (`splice_anewarray_refuses_loop_callee`);
/// * a handler reads a local whose liveness the arm's absence would drop at
///   some pc outside the arm. A chain snapshot's locals are narrowed by the
///   spliced body's liveness over its UNMASKED code with the ADMITTED
///   handler rows (`lib.rs` `spliced_body_codes`), and this body admits
///   none; the check proves that liveness with no rows equals the one with
///   them everywhere a snapshot can be taken, so the interpreter resuming at
///   the branch has every local the handler may read.
pub fn cold_handler_arm_mask(
    code: &[u8],
    code_len: usize,
    rows: &[(usize, usize, usize)],
    num_params: usize,
    max_locals: usize,
) -> Result<(Vec<u8>, ColdHandlerArm), &'static str> {
    use crate::bytecode_analysis as ba;
    let len = code_len.min(code.len());
    if rows.is_empty() {
        return Err("no exception row");
    }
    let insns = ba::decode_method(code, len).ok_or("undecodable body")?;
    let mut edges: Vec<(usize, usize)> = Vec::new();
    let mut targets: Vec<usize> = Vec::new();
    for insn in &insns {
        targets.clear();
        if !ba::explicit_targets(code, len, insn.pc, &mut targets) {
            return Err("a jsr/ret or a malformed branch");
        }
        edges.extend(targets.iter().map(|&t| (insn.pc, t)));
    }
    let is_start = |pc: usize| insns.iter().any(|i| i.pc == pc);
    let mut best: Option<ColdHandlerArm> = None;
    for insn in &insns {
        if !matches!(insn.opcode, 0x99..=0xa6 | 0xc6 | 0xc7) {
            continue;
        }
        let Some(end) = ba::offset_branch_target(code, insn.pc) else {
            continue;
        };
        let start = insn.next_pc();
        if end < start + 2 || !is_start(end) {
            continue;
        }
        let inside = |pc: usize| pc >= start && pc < end;
        if !rows
            .iter()
            .all(|&(s, e, h)| s >= start && s < e && e <= end && inside(h))
        {
            continue;
        }
        if edges.iter().any(|&(src, t)| inside(t) && !inside(src)) {
            continue;
        }
        if best.is_none_or(|b| end - start < b.end - b.start) {
            best = Some(ColdHandlerArm {
                branch_pc: insn.pc,
                start,
                end,
            });
        }
    }
    let arm = best.ok_or("no branch's fall-through arm holds every exception row")?;
    let mut copy = code.to_vec();
    copy[arm.start] = 0x01;
    copy[arm.start + 1..arm.end - 1].fill(0x00);
    copy[arm.end - 1] = 0xbf;
    let as_throw_arm = ColdThrowArm {
        branch_pc: arm.branch_pc,
        start: arm.start,
        athrow_pc: arm.end - 1,
    };
    if !cold_throw_arms(&copy, len).is_some_and(|arms| arms.contains(&as_throw_arm)) {
        return Err("masked, the body is not all cold throw arms");
    }
    if !ba::back_edges(&copy, len).is_empty() {
        return Err("loop callee (a back edge outside the arm)");
    }
    let live = |handlers: &[(usize, usize, usize)]| {
        crate::regalloc::live_locals_per_pc_all(code, len, num_params, &[], handlers, max_locals)
    };
    let with = live(rows);
    let without = live(&[]);
    let words = without.2;
    if with.2 != words {
        return Err("liveness tables disagree");
    }
    for insn in insns.iter().filter(|i| i.pc < arm.start || i.pc >= arm.end) {
        let pc = insn.pc;
        if !without.1.get(pc).copied().unwrap_or(false) {
            // Not covered: every local stays live there (`local_dead_at`).
            continue;
        }
        if !with.1.get(pc).copied().unwrap_or(false) {
            return Err("liveness tables disagree");
        }
        for w in 0..words {
            let at = pc * words + w;
            let need = with.0.get(at).copied().unwrap_or(u64::MAX);
            let kept = without.0.get(at).copied().unwrap_or(0);
            if need & !kept != 0 {
                return Err("a handler reads a local the arm's absence leaves dead");
            }
        }
    }
    Ok((copy, arm))
}

/// [`cold_handler_arm_mask`]'s arm is cold: the profile of the method
/// (`method_code_len` its own `code_length`, as for
/// [`splice_typecheck_arm_unexecuted`]) saw the branch at `arm.branch_pc`
/// jump at least [`SPLICE_COLD_ARM_MIN_SAMPLES`] times and never fall into
/// the arm. A wrong "never" costs a deopt at the branch (then a recompile,
/// whose profile now counts the fall-through), never a wrong answer.
pub fn splice_cold_handler_arm_unexecuted(
    arm: &ColdHandlerArm,
    method_code_len: usize,
    profile: &crate::profile::MethodProfile,
) -> bool {
    let stamped = profile.recorded_code_len();
    if stamped != 0 && usize::try_from(stamped).ok() != Some(method_code_len) {
        return false;
    }
    profile
        .branches
        .get(&arm.branch_pc)
        .is_some_and(|c| c.not_taken == 0 && c.taken >= SPLICE_COLD_ARM_MIN_SAMPLES)
}


/// r23 L8 heap-buffer close, item 3 residual:
/// `CRATONVM_JIT_IR_SPLICE_UNPROFILED_NONNULL_TYPECHECK`, **default ON** (`=0`
/// keeps the profile as the only admission). An unloaded `checkcast` /
/// `instanceof` whose callee has NO usable profile -- `Buffer.session()` when
/// `absGet` is compiled before `session()` was ever profiled, which made
/// `R23L8OsrTiming`'s absget rows bimodal (about 130 ns or 1000 ns per call,
/// `session()` a call per access) -- is admitted as the same precise
/// `ClassLoading` exit when [`typecheck_operand_nonnull_by_test`] holds. A
/// non-null operand reaching the check makes the interpreter resolve, and so
/// load, the class, so the first trap's recompile splices the check as a
/// loaded one: at most one trap per site, the bound the profile rule buys. A
/// null operand skips resolution and could trap per compile; the test is what
/// keeps it out. Read once per held type check.
pub fn ir_splice_unprofiled_nonnull_typecheck_enabled() -> bool {
    true
}

/// Is the type check at `target` reached only on the non-null edge of an
/// `ifnull` / `ifnonnull` that tested the SAME operand expression the check
/// tests? The expression is `aload n`, `aload n; getfield f` or `getstatic f`,
/// byte for byte before both the branch and the check; a local operand must be
/// a local the body never stores. `false` for anything else (an undecodable
/// body, another operand shape). A field re-read between the test and the
/// check can race a write of null; that costs one more trap, never a wrong
/// answer (the exit re-runs the check in the interpreter).
pub fn typecheck_operand_nonnull_by_test(code: &[u8], code_len: usize, target: usize) -> bool {
    use crate::bytecode_analysis as ba;
    let len = code_len.min(code.len());
    let Some(insns) = ba::decode_method(code, len) else {
        return false;
    };
    let Some(ti) = insns.iter().position(|i| i.pc == target) else {
        return false;
    };
    if !matches!(insns[ti].opcode, 0xc0 | 0xc1) {
        return false;
    }
    // The local an `aload` form reads, `None` for another opcode.
    let aload_local = |i: &ba::Insn| -> Option<u16> {
        match i.opcode {
            0x2a..=0x2d => Some(u16::from(i.opcode - 0x2a)),
            0x19 if i.len == 2 => code.get(i.pc + 1).map(|&b| u16::from(b)),
            _ => None,
        }
    };
    // The operand expression ending just before instruction `at`: its first
    // instruction index and its local (if it reads one).
    let operand = |at: usize| -> Option<(usize, Option<u16>)> {
        let prev = at.checked_sub(1)?;
        match insns[prev].opcode {
            0xb2 => Some((prev, None)),
            0xb4 => {
                let base = prev.checked_sub(1)?;
                aload_local(&insns[base]).map(|n| (base, Some(n)))
            }
            _ => aload_local(&insns[prev]).map(|n| (prev, Some(n))),
        }
    };
    let Some((t_start, t_local)) = operand(ti) else {
        return false;
    };
    if let Some(n) = t_local {
        let stored = insns.iter().any(|i| match i.opcode {
            0x4b..=0x4e => u16::from(i.opcode - 0x4b) == n,
            0x3a if i.len == 2 => code.get(i.pc + 1).map(|&b| u16::from(b)) == Some(n),
            0xc4 => true,
            _ => false,
        });
        if stored {
            return false;
        }
    }
    let t_bytes = &code[insns[t_start].pc..target];
    insns.iter().enumerate().any(|(bi, b)| {
        if !matches!(b.opcode, 0xc6 | 0xc7) {
            return false;
        }
        let Some((b_start, _)) = operand(bi) else {
            return false;
        };
        if &code[insns[b_start].pc..b.pc] != t_bytes {
            return false;
        }
        // Call the NON-null edge "never ran": the check is behind it exactly
        // when cutting it leaves the check unreachable.
        let (taken, not_taken) = if b.opcode == 0xc6 { (1, 0) } else { (0, 1) };
        let at = b.pc;
        pc_behind_unexecuted_branch_edge(
            code,
            len,
            target,
            &|pc: usize| (pc == at).then_some((taken, not_taken)),
            1,
        )
    })
}




/// Round 13 wave 10 (lane iropt8, HS-2): does this body write local slot 0
/// anywhere -- an explicit-index store or `iinc` naming it (wide forms
/// included), or any `xstore_0`? `true` when the body does not decode, the
/// conservative answer. (`lstore`/`dstore k` also write `k + 1`, never 0.)
pub(in crate::ir) fn body_written_locals_mask(code: &[u8], code_len: usize) -> u64 {
    let Some(insns) = crate::bytecode_analysis::decode_method(code, code_len) else {
        return u64::MAX;
    };
    let mut mask = 0u64;
    for insn in &insns {
        let operand = || -> Option<usize> {
            if insn.wide {
                let hi = *code.get(insn.pc + 2)?;
                let lo = *code.get(insn.pc + 3)?;
                Some(usize::from(u16::from_be_bytes([hi, lo])))
            } else {
                code.get(insn.pc + 1).map(|&b| usize::from(b))
            }
        };
        let mut mark = |slot: usize, width: usize| {
            for s in slot..slot + width {
                if s >= 64 {
                    mask = u64::MAX;
                    return;
                }
                mask |= 1u64 << s;
            }
        };
        match insn.op {
            // istore, fstore, astore, iinc
            0x36 | 0x38 | 0x3a | 0x84 => match operand() {
                Some(k) => mark(k, 1),
                None => return u64::MAX,
            },
            // lstore, dstore
            0x37 | 0x39 => match operand() {
                Some(k) => mark(k, 2),
                None => return u64::MAX,
            },
            0x3b..=0x3e => mark(usize::from(insn.op - 0x3b), 1),
            0x3f..=0x42 => mark(usize::from(insn.op - 0x3f), 2),
            0x43..=0x46 => mark(usize::from(insn.op - 0x43), 1),
            0x47..=0x4a => mark(usize::from(insn.op - 0x47), 2),
            0x4b..=0x4e => mark(usize::from(insn.op - 0x4b), 1),
            _ => {}
        }
    }
    mask
}

/// Round 13 wave 10 (lane iropt8, HS-2): does this body write local slot 0
/// anywhere -- an explicit-index store or `iinc` naming it (wide forms
/// included), or any `xstore_0`? `true` when the body does not decode, the
/// conservative answer. (`lstore`/`dstore k` also write `k + 1`, never 0.)
pub(in crate::ir) fn body_writes_local_zero(code: &[u8], code_len: usize) -> bool {
    (body_written_locals_mask(code, code_len) & 1) != 0
}

/// `CRATONVM_JIT_IR_OSR_ARM_SPARES_FRESH_NEW` (al16, 2026-10-05; default ON,
/// `=0` is the kill switch): see `IrBuilder::set_osr_requested_header`.
/// Read once per OSR merge build, no `static` (j27 LATCHES).
pub(crate) fn ir_osr_arm_spares_fresh_new_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_EXACT_RECEIVER_FLOW` (al16, 2026-10-05; default ON, `=0`
/// is the kill switch): the flow half of `IrBuilder::exact_receiver_proof`
/// (`IrBuilder::exact_receiver_flow`). Read once per build.
pub(crate) fn ir_exact_receiver_flow_enabled() -> bool {
    true
}

/// Loop header pc -> the local slots some instruction in `[header, last back
/// edge]` writes (`istore`/`lstore`/... in every form, both slots of a
/// category-2 store, and `iinc`), for every backward offset branch of
/// `code[..code_len]`. Empty when the body does not decode.
pub(in crate::ir) fn loop_written_local_slots(code: &[u8], code_len: usize) -> HashMap<usize, HashSet<usize>> {
    let len = code_len.min(code.len());
    let Some(insns) = crate::bytecode_analysis::decode_method(code, len) else {
        return HashMap::new();
    };
    let written = |pc: usize, op: u8, wide: bool| -> Vec<usize> {
        let operand = || -> Option<usize> {
            if wide {
                Some(usize::from(u16::from_be_bytes([
                    *code.get(pc + 2)?,
                    *code.get(pc + 3)?,
                ])))
            } else {
                code.get(pc + 1).map(|&b| usize::from(b))
            }
        };
        match op {
            0x36 | 0x38 | 0x3a | 0x84 => operand().into_iter().collect(),
            0x37 | 0x39 => operand().map(|k| vec![k, k + 1]).unwrap_or_default(),
            0x3b..=0x4e => {
                let k = usize::from((op - 0x3b) % 4);
                if matches!(op, 0x3f..=0x42 | 0x47..=0x4a) {
                    vec![k, k + 1]
                } else {
                    vec![k]
                }
            }
            _ => Vec::new(),
        }
    };
    let mut last_back_edge: HashMap<usize, usize> = HashMap::new();
    for insn in &insns {
        if crate::bytecode_analysis::is_offset_branch(insn.op) {
            if let Some(t) = crate::bytecode_analysis::offset_branch_target(code, insn.pc) {
                if t <= insn.pc {
                    let e = last_back_edge.entry(t).or_insert(insn.pc);
                    *e = (*e).max(insn.pc);
                }
            }
        }
    }
    let mut out: HashMap<usize, HashSet<usize>> = HashMap::new();
    for (&header, &last) in &last_back_edge {
        let set = out.entry(header).or_default();
        for insn in insns.iter().filter(|i| i.pc >= header && i.pc <= last) {
            set.extend(written(insn.pc, insn.op, insn.wide));
        }
    }
    out
}

/// The local slots of `code[..code_len]` that exactly ONE instruction of the
/// method writes, that instruction an `astore` / `astore_<n>` / `wide astore`,
/// and that are not among the first `param_slots` (a parameter is written on
/// entry). The whole-method half of [`IrBuilder::exact_receiver_proof`].
///
/// Every write counts, reachable or not: `istore`/`fstore`/`astore` and their
/// short and wide forms, `lstore`/`dstore` (both slots), and `iinc`. Written
/// by pc order over the shared decoder, so the answer does not depend on the
/// order the builder walks the method in. A method the decoder rejects has no
/// such slot.
pub(in crate::ir) fn single_astore_local_slots(code: &[u8], code_len: usize, param_slots: usize) -> HashSet<usize> {
    let Some(insns) = crate::bytecode_analysis::decode_method(code, code_len) else {
        return HashSet::new();
    };
    // slot -> (writes, whether the last write was an `astore`)
    let mut writes: HashMap<usize, (u32, bool)> = HashMap::new();
    let mut note = |slot: usize, is_astore: bool| {
        let entry = writes.entry(slot).or_insert((0, false));
        entry.0 = entry.0.saturating_add(1);
        entry.1 = is_astore;
    };
    for insn in &insns {
        let operand = || -> Option<usize> {
            if insn.wide {
                let hi = *code.get(insn.pc + 2)?;
                let lo = *code.get(insn.pc + 3)?;
                Some(usize::from(u16::from_be_bytes([hi, lo])))
            } else {
                code.get(insn.pc + 1).map(|&b| usize::from(b))
            }
        };
        match insn.op {
            // istore, fstore, astore
            0x36 | 0x38 | 0x3a => {
                if let Some(k) = operand() {
                    note(k, insn.op == 0x3a);
                }
            }
            // lstore, dstore: the slot and the one above it
            0x37 | 0x39 => {
                if let Some(k) = operand() {
                    note(k, false);
                    note(k.saturating_add(1), false);
                }
            }
            // istore_<n>, fstore_<n>
            0x3b..=0x3e => note(usize::from(insn.op - 0x3b), false),
            0x43..=0x46 => note(usize::from(insn.op - 0x43), false),
            // lstore_<n>, dstore_<n>
            0x3f..=0x42 => {
                let k = usize::from(insn.op - 0x3f);
                note(k, false);
                note(k + 1, false);
            }
            0x47..=0x4a => {
                let k = usize::from(insn.op - 0x47);
                note(k, false);
                note(k + 1, false);
            }
            // astore_<n>
            0x4b..=0x4e => note(usize::from(insn.op - 0x4b), true),
            // iinc
            0x84 => {
                if let Some(k) = operand() {
                    note(k, false);
                }
            }
            _ => {}
        }
    }
    writes
        .into_iter()
        .filter(|&(slot, (count, astore))| slot >= param_slots && count == 1 && astore)
        .map(|(slot, _)| slot)
        .collect()
}

/// Bounded self-recursive inlining in the optimizing tier. **Default ON**
/// (`CRATONVM_JIT_IR_RECURSIVE_INLINE=0` is the kill switch); see "Default ON
/// again" below.
///
/// # Why it is on (round 11 wave 11)
///
/// `CratonBench fib` (`fib(44)`, ~2.3e9 calls) ran 11.4 s against HotSpot's
/// 2.8 s with every call a real direct self-call (`invoke_kind == 4`; the
/// `ir-direct-call MISSED` lines the trace printed for those sites were a
/// diagnostic that did not know the kind-4 route, not a missed bind). The
/// per-call contract is ~30 instructions and is not the lever
/// (`docs/feature-designs/jit-ir-optimizations.md`: removing
/// four stores per call measured nothing); the number of calls is. Two
/// spliced levels take the call count to about one in seven, and the same
/// shape hand-inlined measured `fib(40)` 1 220 -> ~460 ms. It was default off
/// only because nothing had wired it end to end; the flag also used to turn
/// on multi-return splicing for EVERY callee, and no longer does (see
/// [`ir_splice_multi_return_enabled`]): a multi-return body is admitted under
/// this switch only when it calls itself (the resolver) and only as a copy of
/// the method being compiled (`append_ir_inline_site`), so every other
/// compile is unchanged. A copy is also spliced only when its body can neither
/// trap nor call anything but itself (`lib.rs`'s
/// `ir_self_copy_body_is_trap_free`), or -- round 11 wave 15 -- when a trap in
/// it re-runs nothing a replay could observe (`ir_self_copy_body_replays_safely`:
/// field reads, allocations, constructors that write only their fresh
/// receiver, and self-calls; `CRATONVM_JIT_IR_RECURSIVE_INLINE_REPLAY_SAFE=0`
/// drops it). Anything else, e.g. a guard inside a copy whose leaf calls make
/// the region impure, would refuse the whole method's optimizing compile
/// (`splice_guard_seen`), so such a method keeps its plain direct self-calls.
///
/// Dev flipped the same default independently on 2026-09-24 for binary-trees
/// (`bottomUpTree`, `itemCheck`: 1.54x at depth 18,
/// `docs/feature-designs/profile-guided-inlining.md`), where the
/// flag still implied multi-return splicing for every callee. The merge keeps
/// round 11's narrower multi-return rule; wave 15's replay-safe self-copy
/// admission is what gives binary-trees its two levels back
/// (`r11w15-orch-recursive-inline-admission-drops-the-binary-trees-gain`).
///
/// # What it changes
///
/// Without it a self-recursive static method (`fib`) is never inlined into
/// itself by this tier, and not by a decision anyone made:
///
/// * the splice scanner refuses the body outright
///   (`ir-splice-not-single-trailing-return`: the base case is a second
///   `return`);
/// * with multi-return splicing on, the resolver nests the method into itself
///   [`crate::MAX_IR_INLINE_NEST_DEPTH`] deep — 2 + 4 + … + 64 copies — and
///   the plan is then refused WHOLE: the leaves' self-calls have no
///   `direct_entry` on the first compile (the callee compiler declines the
///   cycle), so `append_ir_inline_site`'s unbindable-call rule refuses the
///   site, and on the C2 recompile the tree overruns the 1 300-byte splice
///   budget by one body (63 x 21 bytes for `fib`). Even had it fitted, its
///   leaves would have been bound to the PREVIOUS artifact, so the recursion
///   would have run in the old body. (With this switch on the resolver now
///   stops a self-nesting at [`IR_RECURSIVE_INLINE_MAX_COPIES`], so it builds
///   the 2 + 4 + 8 bodies the plan can use rather than 126.)
///
/// With it, `lib.rs` splices at most [`IR_RECURSIVE_INLINE_MAX_COPIES`] copies
/// of the compiling method along any inline chain and turns the self-call at
/// the cut into this method's own DIRECT self-call (`invoke_kind == 4`, the
/// row [`IrBuilder::self_recursive_call_row`] finds), so the leaves recurse
/// into the NEW body. `fib` becomes one frame per seven nodes of its call tree
/// with eight direct leaf calls: the shape of HotSpot's recursive inlining,
/// one level deeper (see [`IR_RECURSIVE_INLINE_MAX_COPIES`] for why).
///
/// # Why that is sound
///
/// Nothing new is lowered. A spliced self copy is an ordinary IR splice (the
/// same scanner admitted it, the same builder walks it), and a leaf is an
/// ordinary `Op::Call` whose `info_ptr` is the caller's own kind-4
/// `JitInvokeInfo` — the route `ir_lower::emit_self_recursive_call` already
/// takes for the method's own recursive call sites, which records its own
/// inline-frame row and exception/stack checks and is position-independent.
/// The leaf's deopt/exception bci is the enclosing top-level invoke's, exactly
/// as for any call left behind in a spliced body today.
///
/// # Default ON again (round 11 wave 12, lane fib); `=0` is the kill switch
///
/// Lane calls flipped this to default ON in wave 11 and the orchestrator put
/// it back to opt-in: every spliced `fib` body was refused by the lowerer
/// (`unallocated_value`: "home was dropped and its register is not live at a
/// publish edge"). The refused value was a multi-return splice's result phi
/// that the residency plan gave NO register (it is live across the nested
/// splice's calls), whose home `ir_lower`'s `phi_home_droppable` dropped anyway
/// because no reachable deopt names it — leaving the value nowhere. That
/// predicate now requires an assigned register, as `value_home_droppable`
/// always has
/// (`r11w11-orch-recursive-inline-bodies-refused-unallocated-phi-FIXED-20260924.md`).
/// Gate: `jit/tests/r11_calls_recursive_inline.rs`, which executes both arms.
///
/// Read per call rather than cached, like [`crate::ir_inline_enabled`]: a
/// compile is not a hot path, and a test can then arrange it per thread.
pub fn ir_recursive_inline_enabled() -> bool {
    true
}

/// How many copies of the compiling method a bounded self-recursive splice
/// ([`ir_recursive_inline_enabled`]) may stack along one inline chain, the
/// compiling method's own body not counted. HotSpot's `MaxRecursiveInlineLevel`
/// is 1.
///
/// Two, on a measurement rather than by analogy: `fib(40)` hand-inlined at the
/// source level on the wave-2 binary (`irmid3/src/FibRecInl.java`, interleaved,
/// median of three) runs 1 220 ms plain, ~910 ms with one level (1.34x) and
/// ~460 ms with two (2.6x); HotSpot runs the plain form in ~330 ms. Each level
/// multiplies the spliced bytecode by the method's self-call fan-out (two for
/// `fib`: 2 + 4 = 6 bodies, 126 bytes), and the per-compile splice budget still
/// bounds a wider fan-out — a site that does not fit is rolled back whole and
/// stays a direct self-call.
pub const IR_RECURSIVE_INLINE_MAX_COPIES: usize = 2;

/// Maximum self-recursive copies allowed along an inline chain.
///
/// Bounded to [`IR_RECURSIVE_INLINE_MAX_COPIES`] (2; HotSpot uses 1) so
/// self-recursive helpers stay compact inside caller i-cache budgets.
pub fn ir_recursive_inline_max_copies(_code_len: usize) -> usize {
    if let Some(val) = None::<std::ffi::OsString>
        .and_then(|v| v.to_str().and_then(|s| s.parse::<usize>().ok()))
    {
        return val;
    }
    IR_RECURSIVE_INLINE_MAX_COPIES
}

/// The bounded self-recursive splice plan for one inline chain. See
/// [`ir_recursive_inline_enabled`].
///
/// Built once per compile by `lib.rs` from the compiling method's identity and
/// [`IrBuilder::self_recursive_call_row`], and then threaded down the chain:
/// every nested site is [`classified`](Self::classify) against it, and a self
/// site either takes one more copy ([`IrSelfSplice::Splice`], carrying the
/// deeper plan) or becomes a leaf call ([`IrSelfSplice::Leaf`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IrSelfRecursion {
    pub(in crate::ir) class_name: String,
    pub(in crate::ir) method_name: String,
    pub(in crate::ir) descriptor: String,
    pub(in crate::ir) class_id: u32,
    pub(in crate::ir) leaf_row: (usize, usize, u8),
    pub(in crate::ir) copies: usize,
    pub(in crate::ir) max_copies: usize,
}

/// What a site is to an [`IrSelfRecursion`] plan.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum IrSelfSplice {
    /// Not the compiling method: splice (or not) exactly as without the plan,
    /// passing the SAME plan down to its nested sites.
    NotSelf,
    /// The compiling method, with a copy left: splice it and pass this deeper
    /// plan (one more copy counted) to its nested sites.
    Splice(IrSelfRecursion),
    /// The compiling method, with no copy left: do NOT splice. Install this
    /// `invoke_info` row (`(info_ptr, num_args, ret)`, the caller's own
    /// kind-4 row) at the site's combined-buffer pc, so it lowers as this
    /// method's direct self-call.
    Leaf((usize, usize, u8)),
}

impl IrSelfRecursion {
    /// The plan for the compiling method `class_name.method_name descriptor`
    /// declared in class `class_id`, with `leaf_row` its own kind-4
    /// `invoke_info` row, allowing [`IR_RECURSIVE_INLINE_MAX_COPIES`] copies.
    pub fn new(
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        class_id: u32,
        leaf_row: (usize, usize, u8),
    ) -> IrSelfRecursion {
        Self::new_with_code_len(class_name, method_name, descriptor, class_id, leaf_row, 0)
    }

    /// The plan with a copy budget determined by the bytecode length of the method.
    pub fn new_with_code_len(
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        class_id: u32,
        leaf_row: (usize, usize, u8),
        code_len: usize,
    ) -> IrSelfRecursion {
        IrSelfRecursion {
            class_name: class_name.to_owned(),
            method_name: method_name.to_owned(),
            descriptor: descriptor.to_owned(),
            class_id,
            leaf_row,
            copies: 0,
            max_copies: ir_recursive_inline_max_copies(code_len),
        }
    }

    /// The same plan with a different copy budget. For tests and measurement.
    pub fn with_max_copies(mut self, max_copies: usize) -> IrSelfRecursion {
        self.max_copies = max_copies;
        self
    }

    /// Copies of the compiling method already spliced on this chain.
    pub fn copies(&self) -> usize {
        self.copies
    }

    /// Classify a site whose resolved callee is `class_name.method_name
    /// descriptor` in class `class_id`, static or not.
    ///
    /// A site is "self" only when it is STATIC (the kind-4 route is static-only)
    /// and names the same class id — non-zero, so an unknown id never matches —
    /// and the same name and descriptor. Anything else is [`IrSelfSplice::NotSelf`],
    /// which leaves today's behaviour untouched.
    pub fn classify(
        &self,
        class_name: &str,
        method_name: &str,
        descriptor: &str,
        class_id: u32,
        is_static: bool,
    ) -> IrSelfSplice {
        let is_self = is_static
            && class_id != 0
            && class_id == self.class_id
            && class_name == self.class_name
            && method_name == self.method_name
            && descriptor == self.descriptor;
        if !is_self {
            return IrSelfSplice::NotSelf;
        }
        if self.copies >= self.max_copies {
            return IrSelfSplice::Leaf(self.leaf_row);
        }
        let mut deeper = self.clone();
        deeper.copies += 1;
        IrSelfSplice::Splice(deeper)
    }
}

/// Spliced callee bodies whose several `return`s were funnelled into one
/// continuation merge, and how many return edges that took in total. Both are
/// process-wide and diagnostic only; `interp_census` prints them so "no
/// multi-return body was ever admitted" is distinguishable from "the feature
/// is not wired".
pub(in crate::ir) static MULTI_RETURN_SPLICES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub(in crate::ir) static MULTI_RETURN_EDGES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// `(bodies, return edges)` — see [`MULTI_RETURN_SPLICES`].
pub fn multi_return_splice_census() -> (u64, u64) {
    (
        MULTI_RETURN_SPLICES.load(std::sync::atomic::Ordering::Relaxed),
        MULTI_RETURN_EDGES.load(std::sync::atomic::Ordering::Relaxed),
    )
}

/// May a spliced callee body contain a `getstatic`? **Default ON**;
/// `CRATONVM_JIT_IR_SPLICE_GETSTATIC=0` restores the `ir-splice-static-field`
/// refusal.
///
/// Read by BOTH halves, for the same reason and with the same hazard as
/// [`ir_splice_branch_enabled`]: the splice scanner in `jit_bridge` decides
/// whether to admit such a callee, and [`IrInlineTables::static_field_info`] is
/// what carries the rows the builder's `0xb2` arm then looks up. A body
/// admitted without its rows does not fall back -- the builder bails the whole
/// METHOD at the first spliced `getstatic`, which is how the `ldc` rows
/// behaved for their first hour. Neither half may be flipped alone.
///
/// `putstatic` is NOT covered by this switch in either direction; it has its
/// own, [`ir_splice_putstatic_enabled`] (which needs this one on).
pub fn ir_splice_getstatic_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        !matches!(
            Err::<String, std::env::VarError>(std::env::VarError::NotPresent).as_deref(),
            Ok("0") | Ok("false") | Ok("off") | Ok("no")
        )
    })
}

/// `CRATONVM_JIT_IR_SPLICE_PUTSTATIC` (JIT round 21, lane staticfwd, FG16-3)
/// -- default ON (`=0` restores the `ir-splice-putstatic` refusal): may a
/// spliced callee body contain a plain `putstatic`?
///
/// Read by all three halves, which must agree: the splice scanner in
/// `jit_bridge` admits such a body only when every `putstatic` names a
/// non-volatile field of a class initialised at resolve time (monotone, so
/// the `jit_putstatic_*` helper's class-init guard answers from its memo and
/// can neither run `<clinit>` nor return the sentinel); `lib.rs`'s
/// `append_ir_inline_site` rebases its rows; the builder's `0xb3` arm builds
/// the `Op::StoreStatic` and COMMITS it (the replay fence of a caller-visible
/// `putfield`), and the planner's store-fence walk models that commit
/// (`ir_splice_store_fence_walk`). Needs [`ir_splice_getstatic_enabled`] on
/// (the rows travel in the same table). Read per use; no static.
pub fn ir_splice_putstatic_enabled() -> bool {
    ir_splice_getstatic_enabled()
        && true
}

/// May a spliced callee body contain a `checkcast` or an `instanceof`?
/// **Default ON**; `CRATONVM_JIT_IR_SPLICE_TYPECHECK=0` restores the refusal.
///
/// Read by BOTH halves, with the same hazard as [`ir_splice_getstatic_enabled`]
/// and one extra step: the splice scanner in `jit_bridge` decides whether to
/// admit such a callee AND resolves the target class against the CALLEE's
/// constant pool, since `InlineSite` carried no typecheck rows before this and
/// only that pool can name the target. [`IrInlineTables::checkcast_info`]
/// carries what comes out. A body admitted without its rows bails the whole
/// METHOD, so neither half may be flipped alone.
///
/// Every typed read out of an untyped container is a `checkcast`, which is why
/// the survey that motivated the caller-side arms counted 306 events on this
/// pair -- "the largest single whole-method refusal, more than every opcode gap
/// combined". The splice scanner had been refusing the same shape for the same
/// non-reason.
pub fn ir_splice_typecheck_enabled() -> bool {
    use std::sync::OnceLock;
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        !matches!(
            Err::<String, std::env::VarError>(std::env::VarError::NotPresent).as_deref(),
            Ok("0") | Ok("false") | Ok("off") | Ok("no")
        )
    })
}

/// May the optimizing tier lower the scalar intrinsic families as arithmetic?
/// **Default ON**; `CRATONVM_JIT_IR_SCALAR_INTRINSICS=0` restores the
/// method-level refusal for these families too, which is the A arm of the only
/// A/B that means anything here. (Round 11: this paragraph had drifted onto
/// `ir_splice_branch_enabled`'s doc, leaving this function undocumented.)
/// Compile time only (planner, builder, `pure_expand`, the native-shadow
/// refusal text), so it is read per call and not latched: a per-VM or
/// per-thread override reaches it (j27 wave 4, lane SMALLFIX, IR2-5; no
/// `static`, `process_global_statics_ratchet`).
pub fn ir_scalar_intrinsics_enabled() -> bool {
    true
}

/// Scalar-intrinsic sites lowered as arithmetic, this process.
pub(in crate::ir) static SCALAR_INTRINSICS_LOWERED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// Conditional branches replaced by a guard plus an unconditional jump, this
/// process. See [`IrBuilder::prune_always_taken_branch`].
pub(in crate::ir) static BRANCHES_PRUNED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub(in crate::ir) fn note_branch_pruned() {
    BRANCHES_PRUNED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// How many cold branch arms this tier speculated away. A zero with the feature
/// ON means the profile never showed a branch to be one-sided over the sample —
/// which is a fact about the workload, not about the pass, and is exactly the
/// distinction a bare "it did nothing" cannot make.
pub fn branch_prune_census() -> u64 {
    BRANCHES_PRUNED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Speculative pruning of a branch arm the profile has never seen taken —
/// **default ON**, disable with `CRATONVM_JIT_IR_SPECULATE=0`.
///
/// Needs a branch profile to do anything, so it is only meaningful together
/// with `CRATONVM_TIER_PGO` / `CRATONVM_TIER_PGO_ALWAYS` or inside the C2
/// nomination window. Read once per IR compile attempt (`ir_tier_attempt`), not
/// latched (j27 wave 4, lane SMALLFIX; no `static`,
/// `process_global_statics_ratchet`).
pub fn ir_speculate_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_IR_SPECULATE")
}

/// `CRATONVM_JIT_IR_VH_CONST_FRESH_FIELD` (JIT round 23 wave 9, lane W9-L2,
/// CF-W9-3; perf, **default ON** since JIT r24 with the CompletableFuture
/// handler family, `=0` off): see
/// [`IrBuilder::try_vh_const_fresh_field_access`]. Read per site at build
/// time, no `static` (`process_global_statics_ratchet`).
///
/// Opt-in after the n23-w9c A/B (`R21CfChain`, 3 reps, noisy host):
/// thenapply-done 81 / 57 / 70 ns with it on against 78 / 65 / 217 off, i.e.
/// neutral. On that build it never fired: the site refused at the volatile
/// admission check, which only a method with volatile field pcs sets (fixed
/// in wave 9d: the check now asks the lowering's own switches). Even when it
/// fires, `CfSeq.done` keeps `uniApplyNow` and the pending arm's
/// `UniApply.<init>` / `unipush` calls, so the futures still escape.
pub fn ir_vh_const_fresh_field_enabled() -> bool {
    true
}

/// JIT round 23 wave 10 (lane L2): the publication walk (`vh_fresh_receiver_unpublished_in` in tests) with
/// its reason: `None` when every memory path meets `recv` unpublished, else
/// what stopped the walk (for the `CRATONVM_DBG_JITC` refusal line).
pub(crate) fn vh_fresh_receiver_publication(
    graph: &Graph,
    mem: NodeId,
    recv: NodeId,
) -> Option<String> {
    const MAX_STEPS: usize = 256;
    // JIT round 23 wave 11 (lane L2): the memory state the allocation itself
    // consumed. An `Op::New` need not advance the builder's memory token, so
    // a path may never meet `recv` and instead reach the state it was
    // allocated in (n23-w10b: `CfSeq.done`'s `BOXES[i & 127]` `aaload`, the
    // node right before `completedFuture`'s `new`, read as "not a known memory
    // step"). Every memory operation on a path from the access back to that
    // state ran after the allocation, so the walk stops there too.
    let alloc_mem = graph
        .node_opt(recv)
        .filter(|n| matches!(n.op, Op::New { .. }))
        .and_then(|n| n.inputs.get(1).copied())
        .filter(|&m| m != NO_NODE);
    // Wave 11: the VALUE nodes that may name the same object (a cast, a
    // value phi, ...): any non-memory node of reference type taking `recv`
    // (or one of these) as an input. A store or call that takes one of them
    // publishes `recv` as surely as one taking `recv` itself.
    let mut aliases: HashSet<NodeId> = HashSet::new();
    aliases.insert(recv);
    loop {
        let before = aliases.len();
        for (i, n) in graph.nodes.iter().enumerate() {
            let Ok(id) = NodeId::try_from(i) else {
                break;
            };
            if n.ty == IrType::Ref
                && !matches!(
                    n.op,
                    Op::Load(_) | Op::ArrayLoad(_) | Op::Call { .. } | Op::New { .. }
                )
                && n.inputs.iter().any(|x| aliases.contains(x))
            {
                aliases.insert(id);
            }
        }
        if aliases.len() == before || aliases.len() > MAX_STEPS {
            break;
        }
    }
    let mut work: Vec<NodeId> = vec![mem];
    let mut seen: HashSet<NodeId> = HashSet::new();
    while let Some(cur) = work.pop() {
        if cur == recv || Some(cur) == alloc_mem {
            continue;
        }
        if !seen.insert(cur) {
            continue;
        }
        if seen.len() > MAX_STEPS {
            return Some(format!("more than {MAX_STEPS} memory steps"));
        }
        let Some(n) = graph.node_opt(cur) else {
            return Some(format!("n{cur}: no such node"));
        };
        if matches!(n.op, Op::Phi) {
            // A memory φ: every input path must meet `recv`.
            if n.ty != IrType::Memory {
                return Some(format!("n{cur}: a value phi on the memory chain"));
            }
            work.extend(n.inputs.iter().skip(1).copied());
            continue;
        }
        // JIT r23 wave 13 (lane L5, `CRATONVM_JIT_IR_FRESH_WALK_TYPECHECK_STEPS`):
        // a load or store whose BASE is an alias of `recv` (its `checkcast`)
        // touches the same object's own field, like one based on `recv`.
        let typecheck_steps = ir_fresh_walk_typecheck_steps_enabled();
        let base_is_recv = n.inputs.get(2) == Some(&recv)
            || (typecheck_steps && n.inputs.get(2).is_some_and(|b| aliases.contains(b)));
        let base_only = matches!(n.op, Op::Load(_) | Op::Store(_))
            && base_is_recv
            && n.inputs.get(4) != Some(&recv)
            && !(typecheck_steps && n.inputs.get(4).is_some_and(|v| aliases.contains(v)));
        // Wave 13: a type test of the object (`checkcast` / `instanceof`,
        // `[ctrl, mem, obj]`) reads its header in a runtime helper and stores
        // nothing anywhere; it names `recv` without publishing it.
        let type_test = typecheck_steps && fresh_walk_type_test(&n.op);
        let object_init = match n.op {
            Op::Call { info_ptr } if info_ptr != 0 => {
                // SAFETY: as every `Op::Call` `info_ptr` read in this file: a
                // live `JitInvokeInfo` the compile owns.
                let i: &JitInvokeInfo = unsafe { &*(info_ptr as *const JitInvokeInfo) };
                i.class_name == "java/lang/Object" && i.method_name == "<init>"
            }
            _ => false,
        };
        if !base_only
            && !object_init
            && !type_test
            && n.inputs.iter().skip(2).any(|&i| i == recv)
        {
            return Some(format!("n{cur} {:?} takes the receiver (published)", n.op));
        }
        // Wave 11: or a value naming the same object, in any position but a
        // load/store BASE of `recv` itself (checked above).
        if !object_init
            && !type_test
            && n
                .inputs
                .iter()
                .enumerate()
                .skip(2)
                .any(|(k, &i)| i != recv && aliases.contains(&i) && !(base_only && k == 2))
        {
            return Some(format!("n{cur} {:?} takes an alias of the receiver (published)", n.op));
        }
        let memory_step = n.ty == IrType::Memory
            || matches!(
                n.op,
                Op::Load(_)
                    | Op::Call { .. }
                    | Op::New { .. }
                    | Op::LoadStatic { .. }
                    // Wave 11: array reads. A read publishes nothing, and one
                    // that took `recv` was refused above.
                    | Op::ArrayLoad(_)
                    | Op::ArrayLength
            )
            // Wave 13: a type test on the memory chain, of any object.
            || type_test;
        if !memory_step {
            return Some(format!("n{cur} {:?} ({:?}) is not a known memory step", n.op, n.ty));
        }
        match n.inputs.get(1) {
            Some(&next) if next != NO_NODE => work.push(next),
            _ => return Some(format!("n{cur} {:?} has no memory input", n.op)),
        }
    }
    None
}



/// `CRATONVM_JIT_IR_PRUNE_NEVER_TAKEN` (nio21 wave 17, lane L4; **default
/// ON**, `=0` is the kill switch): build a branch whose TAKEN edge the profile
/// never saw as a guard on its fall-through
/// ([`IrBuilder::prune_never_taken_branch`], HotSpot's `unstable_if`).
/// Independent of [`ir_speculate_enabled`] (the always-taken mirror, default
/// OFF). Read without a `static` (`process_global_statics_ratchet`); asked
/// once per compile by the feed in `lib.rs`.
pub fn ir_prune_never_taken_enabled() -> bool {
    true
}

/// Minimum observations of a branch before its unseen edge may be speculated
/// away.
///
/// A branch executed three times, all one way, says nothing. HotSpot's own
/// uncommon-trap policy wants a comparable sample before it prunes, and the
/// cost of being wrong here — a deopt, then a re-speculation on the next
/// compile — is paid per mistake, so the floor is what keeps a cold method from
/// paying it repeatedly.
pub const MIN_OBSERVATIONS_TO_PRUNE: u32 = 100;

/// Call sites REFUSED because their intrinsic family is loop-shaped and a
/// generic dispatch would be a downgrade.
///
/// Counted beside the lowered ones so the split is readable rather than
/// asserted: a large number here is the work list for whoever extends
/// [`try_ir_scalar_intrinsic`], and it is the only thing that says how much of
/// the original 68-method refusal is still on the table.
pub(in crate::ir) static SCALAR_INTRINSICS_REFUSED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

pub(crate) fn note_scalar_intrinsic_lowered() {
    SCALAR_INTRINSICS_LOWERED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    crate::ir_evidence::note(crate::ir_evidence::Transform::ScalarIntrinsic);
}

pub fn note_scalar_intrinsic_refused() {
    SCALAR_INTRINSICS_REFUSED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// `(lowered, refused)` call-site intrinsic decisions in the optimizing tier.
pub fn scalar_intrinsic_census() -> (u64, u64) {
    use std::sync::atomic::Ordering::Relaxed;
    (
        SCALAR_INTRINSICS_LOWERED.load(Relaxed),
        SCALAR_INTRINSICS_REFUSED.load(Relaxed),
    )
}

/// Why a site was replaced by an uncommon trap instead of refusing the method.
///
/// One variant per CALLER, not per opcode, because the census exists to test
/// each caller's coldness argument separately -- `checkcast` and `instanceof`
/// share a cause because they share an argument (the named class is unloaded),
/// while `invokedynamic` is its own because its argument is different and
/// weaker. See [`IrBuilder::plant_uncommon_trap`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrapCause {
    /// `invokedynamic`. No coldness proof; matches the single-pass backend.
    Indy,
    /// `checkcast` / `instanceof` naming a class that is not loaded.
    UnresolvedTypeCheck,
    /// `new` of a class that is not loaded (`JitNewSite::Deferred`).
    UnresolvedNew,
    /// JIT round 19 wave 2, lane eager2 (EAGER19-1): `new` of a class that
    /// is loaded but owes its initialization (`JitNewSite::Deferred` too),
    /// admitted per site by the VM (`CRATONVM_JIT_UNINIT_NEW_TRAP`). Its
    /// coldness proof: an uninitialized class has never been instantiated, so
    /// no `new` of it has ever run; and unlike `UnresolvedNew` the VM's trap
    /// service asks the recompile, so the trap cannot stay on a live path.
    UninitNew,
    /// Interpreter round i4 wave 94, lane L2 (`CRATONVM_JIT_IR_FIELD_SITE_TRAP`):
    /// a `getstatic` / `putstatic` / `getfield` / `putfield` whose owner is
    /// not loaded, admitted per site by the VM. Coldness proof: a field site
    /// loads its owner when it first runs, so a site whose owner is unloaded
    /// has never run; like `UninitNew`, the VM's trap service asks the
    /// recompile (one shot per site), so the trap cannot stay on a live path.
    UnresolvedField,
}

impl TrapCause {
    pub fn as_str(self) -> &'static str {
        match self {
            TrapCause::Indy => "invokedynamic",
            TrapCause::UnresolvedTypeCheck => "unresolved-typecheck",
            TrapCause::UnresolvedNew => "unresolved-new",
            TrapCause::UninitNew => "uninit-new",
            TrapCause::UnresolvedField => "unresolved-field",
        }
    }
    pub(in crate::ir) const COUNT: usize = 5;
    /// JIT round 19 wave 3, lane eager3 (EAGER2-4): the cause's stable one-byte
    /// spelling on a deopt point (`CompiledMethod::trap_cause_points`); the
    /// inverse is [`Self::from_code`].
    pub fn code(self) -> u8 {
        // Cast: an index below `COUNT` (5) fits a byte.
        self.index() as u8
    }
    /// The cause [`Self::code`] spelled `code`, `None` for any other byte.
    pub fn from_code(code: u8) -> Option<Self> {
        match code {
            0 => Some(TrapCause::Indy),
            1 => Some(TrapCause::UnresolvedTypeCheck),
            2 => Some(TrapCause::UnresolvedNew),
            3 => Some(TrapCause::UninitNew),
            4 => Some(TrapCause::UnresolvedField),
            _ => None,
        }
    }
    pub(in crate::ir) fn index(self) -> usize {
        match self {
            TrapCause::Indy => 0,
            TrapCause::UnresolvedTypeCheck => 1,
            TrapCause::UnresolvedNew => 2,
            TrapCause::UninitNew => 3,
            TrapCause::UnresolvedField => 4,
        }
    }
}

/// Traps planted, by cause. Process-wide; read by `ir_trap_census`.
pub(in crate::ir) static TRAPS_PLANTED: [std::sync::atomic::AtomicU64; TrapCause::COUNT] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];

/// Traps REFUSED because the trap would not have been resumable, by cause.
///
/// A planted count on its own cannot tell "this workload has no such site"
/// from "every such site was declined": both read as a low number of hard
/// failures, and only one of them means the guard is doing anything. This row
/// is what makes the refusal falsifiable — and it is the price tag of
/// `ir_trap_replay_guard_enabled`, since every refusal here is one optimizing
/// body given back to the single-pass tier.
pub(in crate::ir) static TRAPS_REFUSED_UNRESUMABLE: [std::sync::atomic::AtomicU64; TrapCause::COUNT] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];

pub(in crate::ir) fn note_trap_planted(cause: TrapCause) {
    TRAPS_PLANTED[cause.index()].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

pub(in crate::ir) fn note_trap_refused(cause: TrapCause) {
    TRAPS_REFUSED_UNRESUMABLE[cause.index()].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// `(cause, refused)` for every trap cause — the companion row to
/// [`ir_trap_census`]. See [`TRAPS_REFUSED_UNRESUMABLE`].
pub fn ir_trap_refusal_census() -> [(&'static str, u64); TrapCause::COUNT] {
    use std::sync::atomic::Ordering::Relaxed;
    [
        (
            TrapCause::Indy.as_str(),
            TRAPS_REFUSED_UNRESUMABLE[0].load(Relaxed),
        ),
        (
            TrapCause::UnresolvedTypeCheck.as_str(),
            TRAPS_REFUSED_UNRESUMABLE[1].load(Relaxed),
        ),
        (
            TrapCause::UnresolvedNew.as_str(),
            TRAPS_REFUSED_UNRESUMABLE[2].load(Relaxed),
        ),
        (
            TrapCause::UninitNew.as_str(),
            TRAPS_REFUSED_UNRESUMABLE[3].load(Relaxed),
        ),
        (
            TrapCause::UnresolvedField.as_str(),
            TRAPS_REFUSED_UNRESUMABLE[4].load(Relaxed),
        ),
    ]
}

/// Must an uncommon trap be resumable before it may be planted? **Default
/// ON**; `CRATONVM_JIT_IR_TRAP_REPLAY_GUARD=0` is the kill switch and restores
/// the pre-2026-09-07 behaviour (plant regardless, and let the interpreter's
/// deopt sink raise `InternalError` when it cannot replay).
///
/// The guard exists because this backend publishes no resumable deopt on a
/// production artifact — see [`IrBuilder::plant_uncommon_trap`] for the full
/// argument and the javac shape that proved it. The kill switch is here so the
/// cost of the refusal (optimizing bodies handed back to the single-pass tier)
/// can be measured as an A/B in one binary rather than argued about. Read per
/// trap site at build time, not latched (j27 wave 4, lane SMALLFIX; no
/// `static`, `process_global_statics_ratchet`).
pub fn ir_trap_replay_guard_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_TRAP_CAUSE_ON_POINT` (JIT round 19 wave 3, lane eager3,
/// EAGER2-4 of `jit-r19-eager2-proposals.md`; **default on**): does the
/// lowerer record the builder's [`TrapCause`] beside each trap / class-init
/// guard deopt point it boxes (`CompiledMethod::trap_cause_points`), so the
/// VM's deopt service names the cause from the point that fired instead of
/// recognising an uninitialized-`new` site by `(method, bci)`? Read per such
/// guard at lowering (cold: compile time).
pub fn trap_cause_on_point_enabled() -> bool {
    true
}


pub fn reachable_pcs_without_pruned_branches(
    code: &[u8],
    code_len: usize,
    pruned_always_taken: &HashSet<usize>,
    pruned_never_taken: &HashSet<usize>,
) -> Option<HashSet<usize>> {
    if pruned_always_taken.is_empty() && pruned_never_taken.is_empty() {
        return None;
    }
    let verified = cratonvm_reader::verified_code(&code[..code_len]).ok()?;
    let mut removed = Vec::new();
    for insn in verified.instructions() {
        let pc = insn.pc as usize;
        if pc >= code_len {
            break;
        }
        let next_pc = insn.next_pc as usize;
        if pruned_always_taken.contains(&pc)
            && next_pc < code_len
            && verified
                .successors(pc)
                .iter()
                .any(|&s| s > pc && s != next_pc)
        {
            removed.push((pc, next_pc));
        }
        if pruned_never_taken.contains(&pc) {
            for succ in verified.successors(pc) {
                if succ > next_pc && succ < code_len {
                    removed.push((pc, succ));
                }
            }
        }
    }
    if removed.is_empty() {
        return None;
    }
    Some(reachable_pcs_without_edges(&verified, code_len, &removed))
}

