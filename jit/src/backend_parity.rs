// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

// REVIEW-NOTE: THE DRIVER IS NOW BUILT. WHAT THIS BLOCK USED TO SAY IS DONE.
//
// This block previously said the module had no production caller and listed
// three obstacles to writing one. Obstacle (1), the un-splittable
// `single_pass_tier`, is cleared. Obstacle (3), the bytes to pass, needed
// nothing and still needs nothing. Obstacle (2) is not an obstacle at all — it
// is an operating instruction, and it is now on the label of the driver itself.
// Recorded here because the old text is what a reader would otherwise trust:
//
//   * `pub mod backend_parity;` is in `jit/src/lib.rs`. Done.
//   * `single_pass_tier` was split (`jit/src/lib.rs`). Its ~2,900 lines of
//     per-site resolver state now live in `build_single_pass_tables`, which
//     returns `SinglePassTables` (what `x64::compile_with_param_slots`
//     consumes) and `SinglePassParts` (the arenas those tables point into, plus
//     what the finished artifact is decorated with). `single_pass_tier` is
//     "build tables, run backend, publish". The default path neither clones nor
//     runs twice.
//   * The driver is `backend_parity_shadow_compile` (`jit/src/lib.rs`), called
//     from `try_compile_inner` on the arm where `ir_tier` SUCCEEDED, behind
//     `backend_parity_enabled()` — `CRATONVM_JIT_BACKEND_PARITY`, default OFF.
//     It runs `single_pass_tier` a second time for the same method, hands both
//     `code_bytes()` to `compare_bodies`, and drops the second artifact.
//   * `single_pass_only_lowering_for` still declines the optimizing tier for
//     exactly the six shapes this harness most wants to compare, so those
//     methods still never reach the driver. That was obstacle (2) and it is not
//     fixable from here: `c1_vector_veto_enabled` caches in a process-wide
//     `OnceLock`. **A corpus run therefore sets
//     `CRATONVM_JIT_C1_VECTOR_VETO=0` for the whole process**, and the driver's
//     own doc comment says so where an operator will read it.
//
// REVIEW-NOTE: TWO FILES THIS CHANGE WAS NOT PERMITTED TO EDIT NEED A ROW EACH.
//
// (1) `types/src/flag_groups.rs`, in the `INVENTORY` table, among the
// `Group::JIT` rows (they are not sorted; put it beside the `c1-vector-veto`
// row at line 1193, which is the flag it is operationally paired with):
//
//     E { group: Group::JIT, token: "backend-parity", on_key: Some("CRATONVM_JIT_BACKEND_PARITY"), off_key: None, off_word: None, since: "2026-09-16" },
//
// `on_key` only, no `off_word`: it is a default-OFF diagnostic read through
// `cratonvm_types::flags::runtime_flag_on`, so "unset" is the off state and
// there is nothing for an off-word to express.
//
// (2) `types/tests/flag-surface.txt`, which is sorted, so the name goes between
// `CRATONVM_JIT_ARM64_SAFEPOINTS` (line 712) and `CRATONVM_JIT_BAND_SKIP`
// (line 713) — `BACKEND` sorts before `BAND` on the third character:
//
//     CRATONVM_JIT_BACKEND_PARITY
//
// Until BOTH land, `types/tests/flag_declaration_guard.rs` fails: it scans for
// exact whole-string `"CRATONVM_*"` literals and requires each to be declared,
// and there are now three such literal sites for this one name: the read in
// `backend_parity_enabled` and the two `with_thread_overrides` arrangements in
// `jit/src/lib.rs`'s test module. The
// tests themselves work either way: `VmFlags::from_env_with_edits` keeps edits
// for undeclared names in `undeclared_edits`, which `runtime_var_os` consults
// while an override is active. (`flag_declaration_guard.rs`'s own doc table
// says an undeclared flag is invisible to `with_thread_overrides`; that row is
// stale as of the `undeclared_edits` field. Not this change's to fix.)
//
// REVIEW-NOTE: `jit/tests/process_global_statics_ratchet.rs` NEEDS `BASELINE`
// RAISED BY EXACTLY ONE, and owes the justifying paragraph its header demands.
//
// By ONE, not to a number quoted here. The ratchet's `BASELINE` reads 721 and
// the tree already counts more than that for reasons outside this change, so a
// literal target written here would be wrong the moment anything else lands —
// which is the drift that file's own comments keep having to repair. Re-run its
// grep and add one:
//
//     grep -rhE '^\s*(pub(\([^)]*\))?\s+)?static\s+(mut\s+)?[A-Za-z_][A-Za-z0-9_]*\s*:' \
//         jit/src --include=*.rs | wc -l
//
// The one line this change adds is `BACKEND_PARITY_CENSUS_LOCK` in
// `jit/src/lib.rs`'s `mod tests` — a `std::sync::Mutex<()>` that serialises the
// two parity census tests against each other. It is not process state any VM
// can observe: it is test-harness serialisation, and this file's own census doc
// explains why it is needed ("a test that wants an exact delta has to serialise
// against other tests in the same binary, because `cargo test` runs them in
// parallel threads on one process"). The ratchet's header already says
// `#[cfg(test)]` statics are counted deliberately — "telling test statics apart
// textually is fragile" — so the count moves and the paragraph is the price.
// The suggested comment:
//
//     // 2026-09-16: +1. `BACKEND_PARITY_CENSUS_LOCK`, the mutex that
//     // serialises the two `CRATONVM_JIT_BACKEND_PARITY` census tests in
//     // `lib.rs`'s test module against each other. Test-harness
//     // serialisation, not VM state; the parity census it guards is the
//     // `backend_parity.rs` counter already argued for above.

//! Does the optimizing body use the capabilities the baseline body does?
//!
//! # Why this exists
//!
//! `x64/single_pass_only.rs` is a hand-maintained veto list of six loop
//! lowerings the single-pass backend has and the IR tier does not. Its own
//! header states the limit: it "cannot catch one nobody has written down", and
//! names the mechanism that would — "compile a corpus with both backends and
//! compare the emitted bodies ... flagging any method whose single-pass body
//! contains VEX-prefixed or `REP`-string bytes that its IR body does not".
//!
//! That is what this module does: it takes two finished bodies for one method
//! and compares what they use. It does not produce them — the driver that does
//! lives in `jit/src/lib.rs` (`backend_parity_shadow_compile`, behind
//! `CRATONVM_JIT_BACKEND_PARITY`, default OFF), because producing the second
//! body means running a whole second compile and that is the compile driver's
//! business, not the analyser's.
//!
//! # What a run of the driver can and cannot cover
//!
//! Repeated here because a reader arrives at this module first:
//!
//! * Only methods the OPTIMIZING TIER TAKES are compared. A method the IR tier
//!   declines has no second body to compare, and needs none: its installed body
//!   is the single-pass one.
//! * The six `SinglePassOnly` shapes are, by default, exactly the methods the
//!   IR tier is made to decline — so they are exactly the methods the driver
//!   never sees. A corpus run that wants them must set
//!   `CRATONVM_JIT_C1_VECTOR_VETO=0` **for the whole process**, because that
//!   veto caches its answer in a `OnceLock`.
//! * Of those six, three have a byte signature this module can detect and three
//!   do not; see "Three variants have NO byte signature" below.
//!
//! So the strongest thing a clean corpus run says is: over the methods the
//! optimizing tier took and the decoder could walk, no body lost VEX, a `REP`
//! string instruction or the sieve constants. That is a real statement and it
//! is not "the backends are at parity".
//!
//! The failure being guarded is `cov-02`: `IrBuilder::build` learned to lower
//! `bastore`, `CratonBench.sieve([ZI)I` stopped falling through to the
//! vectorising single-pass backend, and 2,462 ms became 15,823 ms with an
//! unchanged checksum and no failing test. A finding here is that shape, caught
//! by a machine rather than by a bisect.
//!
//! # The scan is a length decode, not a substring search
//!
//! A `0xF3` byte inside a displacement is not a `REP` prefix. `MOV EAX, 0xF3`
//! is `B8 F3 00 00 00`; `MOV EAX, [RAX+0xC5C4]` puts both VEX escape bytes in a
//! displacement. A substring search flags all three, and a harness that cries
//! wolf on ordinary integer code is a harness nobody leaves switched on. So
//! [`profile_body`] walks the instruction stream, deriving each instruction's
//! length from its prefixes, escape bytes, ModRM/SIB/displacement and
//! immediate, and only ever reads a prefix byte that *is* in prefix position.
//!
//! Where the table is incomplete the decoder **fails closed**: the walk stops,
//! [`BodyProfile::undecodable`] carries the offset and the reason, and
//! [`regressions`] reports nothing for that body in either direction. A partial
//! scan that silently reported "no VEX here" would be the worse failure — it
//! would make the harness look like it was watching while it was blind — so
//! blindness is counted separately ([`ParityCensus::bodies_undecodable`]) and
//! is visible to an operator.
//!
//! # The capabilities, and what each one is evidence of
//!
//! Each is named after the [`SinglePassOnly`] variant it corresponds to, where
//! one exists. What follows was derived by reading the emitters —
//! `x64/simd.rs`, `x64/vec_emit.rs`, `x64/emit.rs`, `x64/escape_analysis.rs`,
//! `x64/op_invoke.rs` — not by reading their names.
//!
//! | capability | emitted by | `SinglePassOnly` variant |
//! |---|---|---|
//! | [`BodyCapability::Vex`] | `emit_vex2`/`emit_vex3` (`x64/emit.rs`), the whole of `x64/vec_emit.rs` | `SimdIntArraySum`, `SimdArrayElementWise` |
//! | [`BodyCapability::RepString`] | `F3 AA` in `emit_bulk_zero_byte_fill_preheader` (`x64/simd.rs`:911); also the `REP STOS`/`REP MOVS` array-init and arraycopy sequences in `x64/op_invoke.rs` | `BulkZeroByteFill` |
//! | [`BodyCapability::SwarZeroByteScan`] | `MOVABS` of `0x0101_0101_0101_0101` / `0x8080_8080_8080_8080` in `emit_byte_sieve_preheader` (`x64/simd.rs`:1058-1059) | `ByteSieve` |
//! | [`BodyCapability::Evex`] | **nothing in this tree** — see below | none |
//!
//! ## Three variants have NO byte signature. This is the harness's limit.
//!
//! This belongs on the label, so it is on the label.
//!
//! * **`ByteSieve`** — `emit_byte_sieve_preheader` (`x64/simd.rs`:1007-1127)
//!   emits only ordinary scalar integer instructions: `MOV`, `SUB`, `NOT`,
//!   `AND`, `TEST`, `CMP`, `Jcc`, `INC`, `LEA`, `PUSH`/`POP`. There is no
//!   instruction class in it that an IR body could not also contain. The one
//!   thing that *is* distinctive is the pair of SWAR zero-byte-detection
//!   constants it materialises, and those two 64-bit literals appear at exactly
//!   one emission site in `jit/src` — which is why
//!   [`BodyCapability::SwarZeroByteScan`] exists and what its honest strength
//!   is: a **constant-value** signature, not an instruction-class one. A body
//!   that materialises `0x0101_0101_0101_0101` for an unrelated reason is a
//!   false positive; an IR body that grew its own word-at-a-time byte scan with
//!   different constants is a false negative. Neither is hypothetical-free.
//! * **`BulkSetByteStride`** — despite the family name, and despite this
//!   module's brief, `emit_bulk_set_byte_stride_preheader` (`x64/simd.rs`:932)
//!   emits **no `REP` string instruction at all**. Its body is `MOV byte
//!   ptr [RDI], 1` / `ADD RDI, R9` / `ADD R10D, R9D` / `CMP` / `JLE` — a
//!   register-only scalar loop, because the stride is not 1 and `REP STOSB`
//!   cannot express a stride. Only `BulkZeroByteFill` reaches `F3 AA`. There is
//!   no encoding here that distinguishes this lowering from any other scalar
//!   byte-store loop, so this harness cannot see it.
//! * **`MatrixDot`** — `emit_matrix_dot_preheader` / `emit_matrix_dot_element`
//!   (`x64/simd.rs`:346-593) is a **scalar** unrolled loop: `MOV` / `SHL` /
//!   `TEST` / `IMUL r32, m32` / `ADD`. Not one VEX byte. Its advantage is the
//!   unroll and the hoisted row/column guards, neither of which has an
//!   encoding. This harness cannot see it either.
//!
//! So of the six `SinglePassOnly` variants, three have a signature this module
//! can detect (two strongly, one by constant value) and three do not. A parity
//! run that reports no finding is therefore evidence about VEX, `REP` strings
//! and the sieve constants, and evidence about nothing else.
//!
//! ## `Evex` is a tripwire, not a live signal
//!
//! Nothing in `jit/src` emits an EVEX prefix today. `x64/cpu_features` does not
//! detect AVX-512, `vec_emit.rs` refuses every lowering that would need it
//! (masked tails, `VPMULLQ`), and a grep for `0x62` as an escape byte finds no
//! emitter. It is detected anyway so that the day a backend grows an AVX-512
//! path, the parity harness already knows the encoding rather than silently
//! stepping over it as an unknown opcode. A finding on this capability today
//! means the decoder is out of sync with the stream, not that AVX-512 was
//! emitted.
//!
//! # What a linear sweep cannot do, and what this one does about it
//!
//! A linear sweep assumes the byte after an instruction starts another
//! instruction. `x64/op_control.rs` breaks that assumption exactly once: a
//! `tableswitch` with more than four LIVE (non-default) cases emits an inline
//! jump table —
//! `count * 4` bytes of `i32` data — directly into the code stream, right after
//! the `JMP RCX` that dispatches through it. Sweeping into it decodes data as
//! instructions, and a table entry whose low byte happens to be `0xC5` would
//! then be reported as a VEX instruction. That is precisely the noise this
//! module exists not to generate.
//!
//! So the sweep recognises the dispatch sequence that always precedes such a
//! table — `48 63 0C 82` / `48 01 D1` / `FF E1` (`MOVSXD RCX, [RDX+RAX*4]`;
//! `ADD RCX, RDX`; `JMP RCX`), emitted at one site and nowhere else — and stops
//! with [`DecodeStopReason::InlineJumpTable`]. The body is reported
//! `undecodable`, produces no finding, and is counted as blind. That is worse
//! coverage and better honesty, which is the trade this file is named after.
//!
//! [`SinglePassOnly`]: crate::x64::single_pass_only

use std::sync::atomic::{AtomicU64, Ordering};

// ---------------------------------------------------------------------------
// Capabilities
// ---------------------------------------------------------------------------

/// One capability an x86-64 body may exhibit, as visible in its encoding.
///
/// "Capability" means *a class of instruction the body actually contains*, not
/// a lowering the compiler believed it performed. The whole point of comparing
/// bodies rather than comparing analyses is that a body cannot be wrong about
/// what it contains.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum BodyCapability {
    /// A VEX-prefixed instruction (`C5 ..` or `C4 ..`): the AVX/AVX2 vector
    /// paths. Emitted by `x64/emit.rs`'s `emit_vex2`/`emit_vex3` for
    /// `SimdIntArraySum` and `SimdArrayElementWise`, and by every encoding in
    /// `x64/vec_emit.rs`.
    ///
    /// `VZEROUPPER` (`C5 F8 77`) counts, and should: it is emitted only after
    /// 256-bit registers have been touched, so its presence is itself evidence
    /// of a vector body.
    Vex,
    /// An EVEX-prefixed instruction (`62 ..`): AVX-512. No emitter in this tree
    /// produces one — see the module header. Detected so that a future one is
    /// not silently invisible.
    Evex,
    /// A `REP`/`REPNE`-prefixed string instruction: `REP STOSB`/`STOSW`/
    /// `STOSD`/`STOSQ`, `REP MOVSB`, and the rest of the `A4..AF` family.
    ///
    /// The prefix must be in prefix position AND the opcode must be a string
    /// opcode. `F3 0F B8` is `POPCNT`, `F3 90` is `PAUSE`, `F3 0F 1E FA` is
    /// `ENDBR64`: in all three `F3` is a mandatory opcode selector, not a
    /// repeat prefix, and none of them is counted here.
    RepString,
    /// A `MOVABS r64, imm64` materialising one of the two SWAR zero-byte-scan
    /// constants (`0x0101_0101_0101_0101`, `0x8080_8080_8080_8080`).
    ///
    /// The closest thing `SinglePassOnly::ByteSieve` has to a signature, and
    /// weaker in kind than the other three: it identifies a *value*, not an
    /// instruction class. Justified because those two literals occur at exactly
    /// one emission site in `jit/src` (`emit_byte_sieve_preheader`,
    /// `x64/simd.rs`:1058-1059) and the sieve is the lowering whose loss cost
    /// 6.4x. See the module header for what it cannot promise.
    SwarZeroByteScan,
}

impl BodyCapability {
    /// Every variant, in census-slot order.
    ///
    /// Exhaustive by construction: [`BodyCapability::ordinal`] is an exhaustive
    /// `match`, so a new variant does not compile until it is given an index,
    /// and `every_capability_is_registered` in the test crate fails until this
    /// array carries it at that index.
    pub const ALL: &'static [BodyCapability] = &[
        BodyCapability::Vex,
        BodyCapability::Evex,
        BodyCapability::RepString,
        BodyCapability::SwarZeroByteScan,
    ];

    /// How many variants there are. The width of every per-capability array.
    pub const COUNT: usize = 4;

    /// Index into [`BodyProfile::counts`] and the census.
    ///
    /// Exhaustive on purpose; see [`BodyCapability::ALL`].
    pub const fn ordinal(self) -> usize {
        match self {
            BodyCapability::Vex => 0,
            BodyCapability::Evex => 1,
            BodyCapability::RepString => 2,
            BodyCapability::SwarZeroByteScan => 3,
        }
    }

    /// What a finding prints. Short, stable, greppable in a perf-gate log.
    pub const fn name(self) -> &'static str {
        match self {
            BodyCapability::Vex => "vex",
            BodyCapability::Evex => "evex",
            BodyCapability::RepString => "rep-string",
            BodyCapability::SwarZeroByteScan => "swar-zero-byte-scan",
        }
    }

    /// The `x64::single_pass_only::SinglePassOnly` variant(s) this capability is
    /// evidence of, spelled as text.
    ///
    /// Text rather than the enum itself because `SinglePassOnly` is
    /// `pub(crate)` inside `x64`, and because one capability maps to two
    /// variants. `None` means the capability corresponds to no variant on the
    /// veto list — today only [`BodyCapability::Evex`], which no emitter
    /// produces at all.
    pub const fn single_pass_only_variants(self) -> Option<&'static str> {
        match self {
            BodyCapability::Vex => Some("SimdIntArraySum, SimdArrayElementWise"),
            BodyCapability::Evex => None,
            BodyCapability::RepString => Some("BulkZeroByteFill"),
            BodyCapability::SwarZeroByteScan => Some("ByteSieve"),
        }
    }
}

// ---------------------------------------------------------------------------
// Profiles
// ---------------------------------------------------------------------------

/// Why the instruction walk stopped short of the end of a body.
///
/// Every variant is a refusal to guess. None of them means "nothing found".
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DecodeStopReason {
    /// The instruction runs off the end of the buffer.
    Truncated,
    /// A one-byte opcode this decoder does not have a shape for — including
    /// every opcode that is invalid in 64-bit mode (`06`, `07`, `0E`, `16`,
    /// `27`, `60`, `61`, `82`, `9A`, `CE`, `D4`..`D6`, `EA`, ...). Reaching one
    /// almost always means the walk is out of sync with the stream, which is
    /// the reason to stop rather than the reason to skip a byte.
    UnknownOpcode(u8),
    /// A two-byte (`0F xx`) opcode with no shape in the table.
    UnknownTwoByteOpcode(u8),
    /// A VEX/EVEX `mmmmm`/`mm` field outside maps 1 (`0F`), 2 (`0F 38`) and
    /// 3 (`0F 3A`) — the AVX-512 FP16 maps 5 and 6, or a reserved value.
    UnsupportedVectorMap(u8),
    /// A `C4`/`C5`/`62` escape byte behind a `REX` or a mandatory-prefix byte.
    /// `#UD` on real hardware, and not something any emitter in this tree
    /// produces, so it is evidence the walk is out of sync.
    VectorEscapeAfterPrefix,
    /// A legacy prefix after a `REX` prefix. Hardware ignores the `REX`; no
    /// emitter here produces it; treat it as loss of sync.
    PrefixAfterRex,
    /// The walk reached the inline jump table of a large `tableswitch`
    /// (`x64/op_control.rs`). Everything from here is `i32` data, not code. See
    /// the module header.
    InlineJumpTable,
}

/// Where and why a body's instruction walk gave up.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct DecodeStop {
    /// Byte offset of the instruction that could not be decoded — i.e. the
    /// walk decoded everything strictly before this offset and nothing at or
    /// after it. "Could not decode past offset N."
    pub offset: usize,
    /// Why.
    pub reason: DecodeStopReason,
}

/// What a scan found in one body.
#[derive(Clone, Debug)]
pub struct BodyProfile {
    /// Length of the body that was scanned, in bytes.
    pub len: usize,
    /// Instructions successfully decoded before the walk ended.
    pub instructions: usize,
    /// Per-capability occurrence counts, indexed by
    /// [`BodyCapability::ordinal`].
    ///
    /// When [`BodyProfile::undecodable`] is `Some`, these are what was seen
    /// *before the stop*. They are real — the walk was in sync up to that point
    /// — but they are not evidence of ABSENCE, which is why [`regressions`]
    /// refuses to use them.
    pub counts: [u32; BodyCapability::COUNT],
    /// `Some` when the walk stopped before the end of the body.
    pub undecodable: Option<DecodeStop>,
}

impl BodyProfile {
    /// How many instructions with this capability the walk saw.
    pub fn count(&self, cap: BodyCapability) -> u32 {
        self.counts[cap.ordinal()]
    }

    /// Did the walk see this capability at all?
    pub fn has(&self, cap: BodyCapability) -> bool {
        self.count(cap) > 0
    }

    /// Did the walk fail to reach the end of the body?
    pub fn is_undecodable(&self) -> bool {
        self.undecodable.is_some()
    }
}

// ---------------------------------------------------------------------------
// The scan
// ---------------------------------------------------------------------------

/// Scan a finished x86-64 body and report which capabilities it exhibits.
///
/// Walks the instruction stream from offset 0, deriving each instruction's
/// length. Stops at the first instruction it cannot decode and records where
/// and why; see [`DecodeStop`]. An empty body is decodable and exhibits
/// nothing.
pub fn profile_body(code: &[u8]) -> BodyProfile {
    let mut profile = BodyProfile {
        len: code.len(),
        instructions: 0,
        counts: [0; BodyCapability::COUNT],
        undecodable: None,
    };
    let mut at = 0usize;
    while at < code.len() {
        // The one construct that puts data in the code stream. Checked before
        // the decode, because the dispatch sequence itself decodes perfectly
        // well and it is the bytes AFTER it that are not code.
        if code[at..].starts_with(&JUMP_TABLE_DISPATCH_TAIL) {
            profile.undecodable = Some(DecodeStop {
                offset: at + JUMP_TABLE_DISPATCH_TAIL.len(),
                reason: DecodeStopReason::InlineJumpTable,
            });
            break;
        }
        match decode_one(code, at) {
            Ok(insn) => {
                debug_assert!(insn.len > 0, "a zero-length instruction would spin");
                if let Some(cap) = insn.capability {
                    profile.counts[cap.ordinal()] = profile.counts[cap.ordinal()].saturating_add(1);
                }
                profile.instructions += 1;
                at += insn.len;
            }
            Err(reason) => {
                profile.undecodable = Some(DecodeStop { offset: at, reason });
                break;
            }
        }
    }
    CENSUS[census::BODIES_PROFILED].fetch_add(1, Ordering::Relaxed);
    if profile.is_undecodable() {
        CENSUS[census::BODIES_UNDECODABLE].fetch_add(1, Ordering::Relaxed);
    }
    profile
}

/// `MOVSXD RCX, [RDX+RAX*4]` / `ADD RCX, RDX` / `JMP RCX` — the tail of the
/// large-`tableswitch` dispatch in `x64/op_control.rs`, immediately followed in
/// the code stream by the `i32` jump table itself.
const JUMP_TABLE_DISPATCH_TAIL: [u8; 9] = [0x48, 0x63, 0x0C, 0x82, 0x48, 0x01, 0xD1, 0xFF, 0xE1];

/// The two SWAR constants `emit_byte_sieve_preheader` materialises.
const SWAR_ONES: u64 = 0x0101_0101_0101_0101;
const SWAR_HIGH_BITS: u64 = 0x8080_8080_8080_8080;

/// One decoded instruction: how long it is, and what it is evidence of.
struct Insn {
    len: usize,
    capability: Option<BodyCapability>,
}

/// How many bytes of immediate an instruction carries.
///
/// Named for the SDM's operand-type letters so the tables below can be checked
/// against it without a translation step.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Imm {
    /// No immediate.
    Zero,
    /// `ib` — one byte.
    B,
    /// `iw` — two bytes.
    W,
    /// `id` — four bytes, regardless of operand size.
    D,
    /// `iz` — two bytes under a `66` prefix, otherwise four. Never eight:
    /// `REX.W` sign-extends a 32-bit immediate, it does not widen it.
    Z,
    /// `io`/`iv` — eight bytes under `REX.W`, two under `66`, otherwise four.
    /// Only `B8+rd` (`MOV r64, imm64`) uses this.
    V,
    /// A `moffs` absolute address: eight bytes, or four under a `67` prefix.
    Moffs,
    /// `ENTER imm16, imm8` — three bytes.
    Enter,
}

impl Imm {
    fn size(self, opsize16: bool, rex_w: bool, addr32: bool) -> usize {
        match self {
            Imm::Zero => 0,
            Imm::B => 1,
            Imm::W => 2,
            Imm::D => 4,
            Imm::Z => {
                if opsize16 {
                    2
                } else {
                    4
                }
            }
            Imm::V => {
                if rex_w {
                    8
                } else if opsize16 {
                    2
                } else {
                    4
                }
            }
            Imm::Moffs => {
                if addr32 {
                    4
                } else {
                    8
                }
            }
            Imm::Enter => 3,
        }
    }
}

/// Decode the instruction starting at `at`, returning its length.
///
/// Semantics are not decoded — only lengths, and the prefix/escape bytes that
/// make a capability. Every path that cannot account for a byte returns `Err`;
/// none guesses a length.
fn decode_one(code: &[u8], at: usize) -> Result<Insn, DecodeStopReason> {
    let byte = |i: usize| -> Result<u8, DecodeStopReason> {
        code.get(i).copied().ok_or(DecodeStopReason::Truncated)
    };

    let mut i = at;
    let mut opsize16 = false;
    let mut addr32 = false;
    let mut rep = false;
    let mut repne = false;
    let mut have_rex = false;
    let mut rex_w = false;

    // ---- prefixes ---------------------------------------------------------
    //
    // Legacy prefixes may appear in any order and any number; `REX` must be the
    // last prefix before the opcode. `66 F3 AB` (REP STOSW) and `F3 48 AB`
    // (REP STOSQ) are both emitted by `x64/op_invoke.rs`, so both orderings of
    // operand-size / repeat / REX have to work.
    loop {
        let b = byte(i)?;
        if have_rex && is_legacy_prefix(b) {
            // The REX would be ignored by hardware. No emitter here does this.
            return Err(DecodeStopReason::PrefixAfterRex);
        }
        match b {
            0xF0 => {}                                    // LOCK
            0xF2 => repne = true,                         // REPNE / SD selector
            0xF3 => rep = true,                           // REP / SS selector
            0x2E | 0x36 | 0x3E | 0x26 | 0x64 | 0x65 => {} // segment
            0x66 => opsize16 = true,                      // operand size
            0x67 => addr32 = true,                        // address size
            0x40..=0x4F => {
                have_rex = true;
                rex_w = b & 0x08 != 0;
            }
            _ => break,
        }
        i += 1;
    }

    let op = byte(i)?;

    // ---- VEX / EVEX -------------------------------------------------------
    //
    // In 64-bit mode `C4`, `C5` and `62` are unconditionally escape bytes:
    // their 32-bit meanings (`LES`, `LDS`, `BOUND`) do not exist here, so there
    // is nothing to disambiguate. They are `#UD` behind a `REX` or a mandatory
    // prefix, which is treated as loss of sync rather than decoded anyway.
    if matches!(op, 0xC4 | 0xC5 | 0x62) {
        if have_rex || opsize16 || rep || repne {
            return Err(DecodeStopReason::VectorEscapeAfterPrefix);
        }
        return decode_vector(code, at, i, op);
    }

    // ---- legacy opcode maps ----------------------------------------------
    if op == 0x0F {
        let op2 = byte(i + 1)?;
        let (map_len, shape) = match op2 {
            // `0F 38` — every instruction the SDM defines in this map is
            // ModRM-with-no-immediate. The entry is the rule, not a list.
            0x38 => {
                byte(i + 2)?;
                (3usize, (true, Imm::Zero))
            }
            // `0F 3A` — every instruction the SDM defines in this map takes an
            // `imm8`, the `/is4` register selectors included (also one byte).
            0x3A => {
                byte(i + 2)?;
                (3usize, (true, Imm::B))
            }
            _ => (
                2usize,
                two_byte_shape(op2).ok_or(DecodeStopReason::UnknownTwoByteOpcode(op2))?,
            ),
        };
        let (has_modrm, imm) = shape;
        let mut len = (i - at) + map_len;
        if has_modrm {
            len += modrm_len(code, i + map_len)?;
        }
        len += imm.size(opsize16, rex_w, addr32);
        // The last byte must exist for the instruction to be present at all.
        byte(at + len - 1)?;
        return Ok(Insn {
            len,
            capability: None,
        });
    }

    // Group 3 (`F6`/`F7`) is the one place the immediate depends on the ModRM
    // `reg` field: `/0` and `/1` are `TEST r/m, imm`, `/2`..`/7` (NOT, NEG,
    // MUL, IMUL, DIV, IDIV) have none.
    let (has_modrm, imm) = if op == 0xF6 || op == 0xF7 {
        let modrm = byte(i + 1)?;
        let reg = (modrm >> 3) & 7;
        let imm = if reg <= 1 {
            if op == 0xF6 {
                Imm::B
            } else {
                Imm::Z
            }
        } else {
            Imm::Zero
        };
        (true, imm)
    } else {
        one_byte_shape(op).ok_or(DecodeStopReason::UnknownOpcode(op))?
    };

    let mut len = (i - at) + 1;
    if has_modrm {
        len += modrm_len(code, i + 1)?;
    }
    let imm_at = at + len;
    let imm_len = imm.size(opsize16, rex_w, addr32);
    len += imm_len;
    byte(at + len - 1)?;

    // ---- capability, from bytes that are in the right position ------------
    let capability = if (rep || repne) && is_string_opcode(op) {
        Some(BodyCapability::RepString)
    } else if rex_w && (0xB8..=0xBF).contains(&op) && imm_len == 8 {
        let mut raw = [0u8; 8];
        raw.copy_from_slice(&code[imm_at..imm_at + 8]);
        let value = u64::from_le_bytes(raw);
        if value == SWAR_ONES || value == SWAR_HIGH_BITS {
            Some(BodyCapability::SwarZeroByteScan)
        } else {
            None
        }
    } else {
        None
    };

    Ok(Insn { len, capability })
}

/// The VEX/EVEX half of [`decode_one`]. `i` points at the escape byte `op`.
fn decode_vector(code: &[u8], at: usize, i: usize, op: u8) -> Result<Insn, DecodeStopReason> {
    let byte = |i: usize| -> Result<u8, DecodeStopReason> {
        code.get(i).copied().ok_or(DecodeStopReason::Truncated)
    };

    // (payload bytes after the escape byte, opcode map, capability)
    let (payload, map, capability) = match op {
        // Two-byte VEX: `C5 [R vvvv L pp]`. Map is always 1 (`0F`).
        0xC5 => {
            byte(i + 1)?;
            (1usize, 1u8, BodyCapability::Vex)
        }
        // Three-byte VEX: `C4 [R X B mmmmm] [W vvvv L pp]`.
        0xC4 => {
            let p0 = byte(i + 1)?;
            byte(i + 2)?;
            (2usize, p0 & 0x1F, BodyCapability::Vex)
        }
        // EVEX: `62 [R X B R' 0 0 m m] [W vvvv 1 pp] [z L'L b V' aaa]`.
        0x62 => {
            let p0 = byte(i + 1)?;
            byte(i + 2)?;
            byte(i + 3)?;
            (3usize, p0 & 0x07, BodyCapability::Evex)
        }
        // Unreachable by construction — the caller matches on exactly those
        // three bytes — but returned as a refusal rather than a panic, because
        // nothing in this module is allowed to abort a compile it was only
        // supposed to observe.
        _ => return Err(DecodeStopReason::UnknownOpcode(op)),
    };
    if !matches!(map, 1 | 2 | 3) {
        return Err(DecodeStopReason::UnsupportedVectorMap(map));
    }

    let opcode_at = i + 1 + payload;
    let opcode = byte(opcode_at)?;

    // `VZEROUPPER`/`VZEROALL` (map 1, opcode `77`) is the only VEX-encoded
    // instruction without a ModRM byte. Every EVEX instruction has one.
    let has_modrm = !(map == 1 && opcode == 0x77 && capability == BodyCapability::Vex);

    // Map 1 carries an `imm8` for exactly the shuffle / shift-group / compare /
    // insert / extract opcodes below; map 2 never does; map 3 always does.
    let imm = match map {
        1 => {
            if matches!(
                opcode,
                0x70 | 0x71 | 0x72 | 0x73 | 0xC2 | 0xC4 | 0xC5 | 0xC6
            ) {
                1usize
            } else {
                0
            }
        }
        2 => 0,
        _ => 1,
    };

    let mut len = (opcode_at + 1) - at;
    if has_modrm {
        len += modrm_len(code, opcode_at + 1)?;
    }
    len += imm;
    byte(at + len - 1)?;
    Ok(Insn {
        len,
        capability: Some(capability),
    })
}

/// Bytes consumed by the ModRM byte at `i` plus its SIB and displacement.
///
/// Identical under a `67` address-size prefix: 32-bit addressing in 64-bit mode
/// uses the same ModRM/SIB layout and the same displacement widths, so only
/// `moffs` operands change size.
fn modrm_len(code: &[u8], i: usize) -> Result<usize, DecodeStopReason> {
    let byte = |i: usize| -> Result<u8, DecodeStopReason> {
        code.get(i).copied().ok_or(DecodeStopReason::Truncated)
    };
    let modrm = byte(i)?;
    let md = modrm >> 6;
    let rm = modrm & 7;
    let mut n = 1usize;
    if md != 3 && rm == 4 {
        let sib = byte(i + 1)?;
        n += 1;
        // A SIB base field of 101 with mod=00 means "no base, disp32".
        if md == 0 && (sib & 7) == 5 {
            n += 4;
        }
    }
    if md == 0 && rm == 5 {
        // RIP-relative (EIP-relative under `67`): always disp32.
        n += 4;
    } else if md == 1 {
        n += 1;
    } else if md == 2 {
        n += 4;
    }
    Ok(n)
}

/// Is `b` a legacy (non-`REX`) prefix byte?
fn is_legacy_prefix(b: u8) -> bool {
    matches!(
        b,
        0xF0 | 0xF2 | 0xF3 | 0x2E | 0x36 | 0x3E | 0x26 | 0x64 | 0x65 | 0x66 | 0x67
    )
}

/// Is `op` one of the string instructions a `REP`/`REPNE` prefix repeats?
///
/// `INS`/`OUTS` (`6C`..`6F`), `MOVS` (`A4`/`A5`), `CMPS` (`A6`/`A7`), `STOS`
/// (`AA`/`AB`), `LODS` (`AC`/`AD`), `SCAS` (`AE`/`AF`). This is the whole list;
/// an `F3` in front of anything else is a mandatory prefix, not a repeat.
fn is_string_opcode(op: u8) -> bool {
    matches!(op, 0x6C..=0x6F | 0xA4..=0xA7 | 0xAA..=0xAF)
}

/// `(has ModRM, immediate kind)` for a one-byte opcode, or `None` to fail
/// closed.
///
/// `None` covers both "invalid in 64-bit mode" (`06`, `07`, `0E`, `16`, `17`,
/// `1E`, `1F`, `27`, `2F`, `37`, `3F`, `60`, `61`, `82`, `9A`, `CE`, `D4`,
/// `D5`, `D6`, `EA`) and "handled by the caller" (`0F` escape, `F6`/`F7` group
/// 3, prefixes, `REX`, `C4`/`C5`/`62`). Reaching either from a linear sweep is
/// evidence the sweep is out of sync, which is why neither is skipped over.
fn one_byte_shape(op: u8) -> Option<(bool, Imm)> {
    use Imm::*;
    Some(match op {
        // The ALU block: ADD/OR/ADC/SBB/AND/SUB/XOR/CMP, in the classic
        // `+0 r/m8,r8  +1 r/m,r  +2 r8,r/m8  +3 r,r/m  +4 AL,imm8  +5 eAX,imz`
        // pattern. `+6`/`+7` are the segment PUSH/POPs, gone in 64-bit mode.
        0x00..=0x03
        | 0x08..=0x0B
        | 0x10..=0x13
        | 0x18..=0x1B
        | 0x20..=0x23
        | 0x28..=0x2B
        | 0x30..=0x33
        | 0x38..=0x3B => (true, Zero),
        0x04 | 0x0C | 0x14 | 0x1C | 0x24 | 0x2C | 0x34 | 0x3C => (false, B),
        0x05 | 0x0D | 0x15 | 0x1D | 0x25 | 0x2D | 0x35 | 0x3D => (false, Z),

        0x50..=0x5F => (false, Zero),  // PUSH/POP r64
        0x63 => (true, Zero),          // MOVSXD
        0x68 => (false, Z),            // PUSH imm
        0x69 => (true, Z),             // IMUL r, r/m, imm
        0x6A => (false, B),            // PUSH imm8
        0x6B => (true, B),             // IMUL r, r/m, imm8
        0x6C..=0x6F => (false, Zero),  // INS/OUTS
        0x70..=0x7F => (false, B),     // Jcc rel8
        0x80 => (true, B),             // group 1, r/m8, imm8
        0x81 => (true, Z),             // group 1, r/m, imm
        0x83 => (true, B),             // group 1, r/m, imm8
        0x84..=0x8F => (true, Zero),   // TEST/XCHG/MOV/LEA/POP r/m
        0x90..=0x99 => (false, Zero),  // XCHG eAX,r / NOP / CBW / CWD
        0x9B..=0x9F => (false, Zero),  // WAIT / PUSHF / POPF / SAHF / LAHF
        0xA0..=0xA3 => (false, Moffs), // MOV AL/eAX, moffs
        0xA4..=0xA7 => (false, Zero),  // MOVS / CMPS
        0xA8 => (false, B),            // TEST AL, imm8
        0xA9 => (false, Z),            // TEST eAX, imm
        0xAA..=0xAF => (false, Zero),  // STOS / LODS / SCAS
        0xB0..=0xB7 => (false, B),     // MOV r8, imm8
        0xB8..=0xBF => (false, V),     // MOV r, imm (imm64 under REX.W)
        0xC0 | 0xC1 => (true, B),      // shift group, imm8
        0xC2 => (false, W),            // RET imm16
        0xC3 => (false, Zero),         // RET
        0xC6 => (true, B),             // MOV r/m8, imm8 (and XABORT)
        0xC7 => (true, Z),             // MOV r/m, imm (and XBEGIN)
        0xC8 => (false, Enter),        // ENTER imm16, imm8
        0xC9 => (false, Zero),         // LEAVE
        0xCA => (false, W),            // RETF imm16
        0xCB | 0xCC => (false, Zero),  // RETF / INT3
        0xCD => (false, B),            // INT imm8
        0xCF => (false, Zero),         // IRET
        0xD0..=0xD3 => (true, Zero),   // shift group, 1 / CL
        0xD7 => (false, Zero),         // XLAT
        0xD8..=0xDF => (true, Zero),   // x87
        0xE0..=0xE3 => (false, B),     // LOOPNE/LOOPE/LOOP/JRCXZ
        0xE4..=0xE7 => (false, B),     // IN/OUT imm8
        0xE8 | 0xE9 => (false, D),     // CALL/JMP rel32
        0xEB => (false, B),            // JMP rel8
        0xEC..=0xEF => (false, Zero),  // IN/OUT DX
        0xF1 => (false, Zero),         // INT1
        0xF4 | 0xF5 => (false, Zero),  // HLT / CMC
        0xF8..=0xFD => (false, Zero),  // CLC/STC/CLI/STI/CLD/STD
        0xFE | 0xFF => (true, Zero),   // INC/DEC group, group 5
        _ => return None,
    })
}

/// `(has ModRM, immediate kind)` for a `0F xx` opcode, or `None` to fail
/// closed.
///
/// The `0F 38` and `0F 3A` escapes are handled by the caller and never reach
/// here. `0F 0F` (3DNow!) is deliberately absent: its immediate is a *suffix*
/// opcode selector rather than an operand, the extension is dead on every CPU
/// this JIT targets, and refusing is cheaper than carrying a table for it.
fn two_byte_shape(op: u8) -> Option<(bool, Imm)> {
    use Imm::*;
    Some(match op {
        0x00..=0x03 => (true, Zero),         // group 6 / group 7 / LAR / LSL
        0x05..=0x09 | 0x0B => (false, Zero), // SYSCALL / CLTS / SYSRET / INVD / WBINVD / UD2
        0x0D => (true, Zero),                // prefetch group
        0x0E => (false, Zero),               // FEMMS
        0x10..=0x17 => (true, Zero),         // SSE moves
        0x18..=0x1F => (true, Zero),         // hint-NOP / prefetch / ENDBR
        0x20..=0x23 => (true, Zero),         // MOV to/from CR/DR
        0x28..=0x2F => (true, Zero),         // SSE converts and compares
        0x30..=0x37 => (false, Zero),        // WRMSR / RDTSC / RDMSR / RDPMC / SYS* / GETSEC
        0x40..=0x4F => (true, Zero),         // CMOVcc
        0x50..=0x6F => (true, Zero),         // SSE packed
        0x70..=0x73 => (true, B),            // PSHUF* and the shift groups
        0x74..=0x76 => (true, Zero),         // PCMPEQ
        0x77 => (false, Zero),               // EMMS
        0x78 | 0x79 => (true, Zero),         // VMREAD / VMWRITE
        0x7C..=0x7F => (true, Zero),         // HADD / HSUB / MOVD / MOVQ / MOVDQ
        0x80..=0x8F => (false, D),           // Jcc rel32
        0x90..=0x9F => (true, Zero),         // SETcc
        0xA0..=0xA2 => (false, Zero),        // PUSH FS / POP FS / CPUID
        0xA3 => (true, Zero),                // BT
        0xA4 => (true, B),                   // SHLD imm8
        0xA5 => (true, Zero),                // SHLD CL
        0xA8..=0xAA => (false, Zero),        // PUSH GS / POP GS / RSM
        0xAB => (true, Zero),                // BTS
        0xAC => (true, B),                   // SHRD imm8
        0xAD..=0xAF => (true, Zero),         // SHRD CL / group 15 / IMUL
        0xB0..=0xB9 => (true, Zero),         // CMPXCHG / LSS / BTR / MOVZX / POPCNT / UD1
        0xBA => (true, B),                   // group 8, BT/BTS/BTR/BTC imm8
        0xBB..=0xBF => (true, Zero),         // BTC / BSF / BSR / MOVSX
        0xC0 | 0xC1 => (true, Zero),         // XADD
        0xC2 => (true, B),                   // CMPPS/CMPSS imm8
        0xC3 => (true, Zero),                // MOVNTI
        0xC4..=0xC6 => (true, B),            // PINSRW / PEXTRW / SHUFPS imm8
        0xC7 => (true, Zero),                // group 9
        0xC8..=0xCF => (false, Zero),        // BSWAP
        0xD0..=0xFF => (true, Zero),         // SSE packed, and UD0
        _ => return None,
    })
}

// ---------------------------------------------------------------------------
// Comparison
// ---------------------------------------------------------------------------

/// The method two bodies belong to, for the text of a finding.
///
/// Borrowed rather than owned: every caller already holds these as `&str` on a
/// `CachedBytecodeMethod`, and a parity check must not allocate three strings
/// per compile to say nothing.
#[derive(Clone, Copy, Debug)]
pub struct MethodId<'a> {
    pub class_name: &'a str,
    pub method_name: &'a str,
    pub descriptor: &'a str,
}

impl std::fmt::Display for MethodId<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{}.{}{}",
            self.class_name, self.method_name, self.descriptor
        )
    }
}

/// Capabilities the baseline body has and the optimizing body does not.
///
/// **Asymmetric on purpose.** A capability the OPTIMIZING body has and the
/// baseline does not is the optimizing tier doing its job, and reporting it
/// would make the harness fire on every successful vectorisation. Only the loss
/// direction is a finding.
///
/// **Blind bodies yield nothing.** If either profile is
/// [`undecodable`](BodyProfile::is_undecodable), the result is empty — in both
/// directions. A partial scan of the baseline can under-count (so a real loss
/// would be missed, which is a silent miss rather than a false alarm) and a
/// partial scan of the optimizing body can under-count (so an absence is not
/// established, which would be a false alarm). Neither is worth a finding; both
/// are worth a counter, which [`compare_bodies`] keeps as
/// [`ParityCensus::comparisons_blind`].
///
/// Results come back in [`BodyCapability::ALL`] order, so a diff of two runs is
/// stable.
pub fn regressions(baseline: &BodyProfile, optimizing: &BodyProfile) -> Vec<BodyCapability> {
    if baseline.is_undecodable() || optimizing.is_undecodable() {
        return Vec::new();
    }
    BodyCapability::ALL
        .iter()
        .copied()
        .filter(|&cap| baseline.has(cap) && !optimizing.has(cap))
        .collect()
}

/// Everything one comparison produced.
#[derive(Clone, Debug)]
pub struct ParityVerdict {
    /// The single-pass (baseline) body's profile.
    pub baseline: BodyProfile,
    /// The optimizing (IR) body's profile.
    pub optimizing: BodyProfile,
    /// Capabilities the optimizing body lost. Empty when either side is blind.
    pub lost: Vec<BodyCapability>,
}

impl ParityVerdict {
    /// Did the harness go blind on this method rather than see nothing?
    ///
    /// The distinction the whole module turns on: `lost.is_empty()` alone does
    /// NOT mean the bodies agree.
    pub fn is_blind(&self) -> bool {
        self.baseline.is_undecodable() || self.optimizing.is_undecodable()
    }
}

/// Profile both bodies for one method, report any lost capability, and record
/// the outcome in the census.
///
/// `baseline` is the single-pass body, `optimizing` is the IR body. Getting
/// them the right way round is the caller's job and the comparison is
/// asymmetric, so getting it wrong produces a harness that reports every
/// vectorised method as a regression — which is why [`ParityVerdict`] carries
/// both profiles rather than only a verdict.
///
/// Findings are reported with [`tracing::warn!`], **not** `debug!` or `trace!`.
/// The workspace pins `tracing` with `release_max_level_info` (see the comment
/// on the `tracing` dependency in the workspace `Cargo.toml`), which compiles
/// every `debug!`/`trace!` site in a release binary to a no-op. A parity
/// finding that only appeared at debug level would be invisible on exactly the
/// binaries a perf gate runs.
pub fn compare_bodies(method: MethodId<'_>, baseline: &[u8], optimizing: &[u8]) -> ParityVerdict {
    let baseline = profile_body(baseline);
    let optimizing = profile_body(optimizing);
    let lost = regressions(&baseline, &optimizing);

    CENSUS[census::COMPARISONS].fetch_add(1, Ordering::Relaxed);
    if baseline.is_undecodable() || optimizing.is_undecodable() {
        CENSUS[census::COMPARISONS_BLIND].fetch_add(1, Ordering::Relaxed);
        tracing::warn!(
            target: "cratonvm::jit::backend_parity",
            "backend-parity: {} not compared — baseline {}, optimizing {}; \
             this method is UNCHECKED, not clean",
            method,
            describe(&baseline),
            describe(&optimizing),
        );
    }

    for &cap in &lost {
        CENSUS[census::FINDINGS_BASE + cap.ordinal()].fetch_add(1, Ordering::Relaxed);
        tracing::warn!(
            target: "cratonvm::jit::backend_parity",
            "backend-parity: {} — the optimizing body has no `{}` where the \
             single-pass body has {} ({} bytes / {} insns optimizing vs {} bytes / \
             {} insns single-pass). Single-pass-only lowering(s) at risk: {}",
            method,
            cap.name(),
            baseline.count(cap),
            optimizing.len,
            optimizing.instructions,
            baseline.len,
            baseline.instructions,
            cap.single_pass_only_variants().unwrap_or("none enumerated"),
        );
    }

    ParityVerdict {
        baseline,
        optimizing,
        lost,
    }
}

/// Record a comparison that could not be made, and say why.
///
/// [`compare_bodies`] handles the case where both bodies exist and one of them
/// cannot be decoded. This is the case one step earlier: the driver could not
/// PRODUCE one of the two bodies at all — the shadow single-pass compile
/// declined, or panicked and was contained.
///
/// It bumps [`ParityCensus::comparisons`] as well as
/// [`ParityCensus::comparisons_blind`], and that is deliberate: `comparisons`
/// counts attempts, so `comparisons_sighted()` stays "attempts that produced a
/// usable answer" and a method whose baseline body never existed lands in the
/// blind column rather than vanishing from the denominator. A harness that
/// quietly dropped such methods would report a smaller, cleaner-looking corpus
/// the more often its second compile failed — which is the exact direction of
/// wrongness this module refuses.
///
/// [`tracing::warn!`] for the same reason [`compare_bodies`] uses it: the
/// workspace pins `release_max_level_info`, so a `debug!` here would be a no-op
/// on precisely the binaries a perf gate runs.
pub fn note_comparison_blind(method: MethodId<'_>, why: &str) {
    CENSUS[census::COMPARISONS].fetch_add(1, Ordering::Relaxed);
    CENSUS[census::COMPARISONS_BLIND].fetch_add(1, Ordering::Relaxed);
    tracing::warn!(
        target: "cratonvm::jit::backend_parity",
        "backend-parity: {} not compared — {}; this method is UNCHECKED, not clean",
        method,
        why,
    );
}

/// One profile, as a phrase for a log line.
fn describe(p: &BodyProfile) -> String {
    match p.undecodable {
        Some(stop) => format!(
            "undecodable past offset {} of {} ({:?})",
            stop.offset, p.len, stop.reason
        ),
        None => format!("{} bytes decoded", p.len),
    }
}

// ---------------------------------------------------------------------------
// Census
// ---------------------------------------------------------------------------

/// Index into [`CENSUS`], in [`ParityCensus`] field order.
mod census {
    pub const BODIES_PROFILED: usize = 0;
    pub const BODIES_UNDECODABLE: usize = 1;
    pub const COMPARISONS: usize = 2;
    pub const COMPARISONS_BLIND: usize = 3;
    /// The first of [`super::BodyCapability::COUNT`] per-capability slots,
    /// indexed by [`super::BodyCapability::ordinal`].
    pub const FINDINGS_BASE: usize = 4;
    pub const SLOTS: usize = FINDINGS_BASE + super::BodyCapability::COUNT;
}

/// Process-wide parity counters since process start.
///
/// One `static` for eight counters, deliberately — not eight statics.
///
/// `jit/tests/process_global_statics_ratchet.rs` counts `static` DECLARATION
/// LINES under `jit/src` and fails when the total passes its baseline, so the
/// cost of this census to that budget is exactly one line regardless of how
/// many buckets it grows. A new bucket goes in [`census`] and widens the array;
/// it does not add a declaration. (No number is quoted here on purpose: the
/// count moves with every unrelated commit, and a snapshot in a comment is the
/// drift this repository keeps having to repair. Run the ratchet.)
///
/// That ratchet's own header lists "metrics counters" among the statics it
/// considers legitimate, and this is one: a diagnostic tally, not per-VM
/// compatibility state. Two VMs in one process sharing it is the intended
/// behaviour, not the bug the ratchet exists to prevent — a parity finding is a
/// fact about a compiled body, and the same body is the same body whichever VM
/// asked for it.
///
/// Monotone and process-global, so a caller reads a DELTA around the work it
/// cares about — and a test that wants an exact delta has to serialise against
/// other tests in the same binary, because `cargo test` runs them in parallel
/// threads on one process.
static CENSUS: [AtomicU64; census::SLOTS] = [
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
    AtomicU64::new(0),
];

/// The process-wide backend-parity census since process start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ParityCensus {
    /// Bodies handed to [`profile_body`], decodable or not. Two per
    /// [`compare_bodies`] call, and none for a [`note_comparison_blind`] one —
    /// which is why this is not `2 * comparisons`.
    pub bodies_profiled: u64,
    /// Bodies whose instruction walk stopped short. The harness's blind spot,
    /// counted so it can be seen rather than inferred from a quiet log.
    pub bodies_undecodable: u64,
    /// Comparisons ATTEMPTED: one per [`compare_bodies`] call plus one per
    /// [`note_comparison_blind`] call. Attempts, not successes — see
    /// [`ParityCensus::comparisons_sighted`].
    pub comparisons: u64,
    /// Attempts that produced no answer in either direction: either both bodies
    /// existed and at least one was undecodable ([`compare_bodies`]), or one of
    /// the two bodies could not be produced at all
    /// ([`note_comparison_blind`]).
    pub comparisons_blind: u64,
    /// Findings per capability, indexed by [`BodyCapability::ordinal`].
    pub findings: [u64; BodyCapability::COUNT],
}

impl ParityCensus {
    /// Findings for one capability.
    pub fn findings_for(&self, cap: BodyCapability) -> u64 {
        self.findings[cap.ordinal()]
    }

    /// Comparisons that produced a usable answer — that is, comparisons the
    /// harness was not blind on.
    ///
    /// The number an operator should read before believing a zero in
    /// [`ParityCensus::findings`]: zero findings out of zero sighted
    /// comparisons says nothing at all.
    pub fn comparisons_sighted(&self) -> u64 {
        self.comparisons.saturating_sub(self.comparisons_blind)
    }
}

/// Read the process-wide census.
pub fn backend_parity_census() -> ParityCensus {
    let mut findings = [0u64; BodyCapability::COUNT];
    for (slot, out) in findings.iter_mut().enumerate() {
        *out = CENSUS[census::FINDINGS_BASE + slot].load(Ordering::Relaxed);
    }
    ParityCensus {
        bodies_profiled: CENSUS[census::BODIES_PROFILED].load(Ordering::Relaxed),
        bodies_undecodable: CENSUS[census::BODIES_UNDECODABLE].load(Ordering::Relaxed),
        comparisons: CENSUS[census::COMPARISONS].load(Ordering::Relaxed),
        comparisons_blind: CENSUS[census::COMPARISONS_BLIND].load(Ordering::Relaxed),
        findings,
    }
}
