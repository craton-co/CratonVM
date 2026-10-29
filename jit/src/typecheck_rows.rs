// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! JIT round 21, lane dispatch (`CRATONVM_JIT_TYPECHECK_ROWS`, proposals
//! TYPECHECK21-4 / DISPATCH21-1, page
//! `docs/internal/jit/jr21-dispatch-typecheck-rows-FIXED-20261029.md`): a
//! per-VM, fixed-address, class-id-indexed table of **type-check rows**, the
//! compiled `instanceof` / `checkcast` arms' short path. HotSpot's
//! primary-supers display at a fixed offset (one load and one compare for a
//! class target) and a secondary-supers home slot plus a membership bloom for
//! an interface target.
//!
//! The supers DISPLAYS (`VhSubtypeDisplay`, the VM's `SupersDisplays`) answer
//! the same questions through three dependent hops (the table cell, the table
//! with its capacity, the display, then the depth or the interface-set walk).
//! A row is the same facts, laid out so that the compiled test needs only the
//! object's class id: `rows + (class_id << ROW_SHIFT)` is the row, and every
//! word the test reads sits at a displacement the compile knows.
//!
//! # Layout (`ROW_WORDS` `u32` words, `ROW_BYTES` bytes, 64-byte aligned)
//!
//! | word | contents |
//! |---|---|
//! | 0 | byte 0: status, `0` no row yet, [`STATUS_PUBLISHED`], or [`STATUS_HIERARCHY_ONLY`]; byte 1: the class's own depth when 1..=7, else `0` ([`STATUS_DEPTH_SHIFT`], JIT round 21 lane tcmiss) |
//! | 1..=7 | the superclass chain at depth 1..=7 (`0` past the chain's end) |
//! | 8..=9 | a 64-bit bloom of the transitive interface set: bit `iface % 64` |
//! | 10..=15 | interface home slots: interface `I` may sit only in slot `I % 6` |
//!
//! Depth 0 is always `java/lang/Object`, so it is not stored. A class deeper
//! than 7 still has its first seven ancestors right, which is all a target at
//! depth <= 7 asks. An interface whose home slot another interface of the same
//! class took is simply not placed: its hit falls back (the bloom still lists
//! it, so it is never answered as a miss).
//!
//! # Why a row is sound to read without a lock
//!
//! The VM fills a class's row once, from the same `(chain, interfaces,
//! hierarchy_only)` facts it publishes as the class's display, under the
//! displays' mutex: words 1..15 first, the status word LAST with a Release
//! store. A row is never changed afterwards (a class's supertypes are fixed for
//! its life; class ids are never reused within a VM) and the table is never
//! moved or freed while the VM lives, so its address is baked into the code.
//!
//! * A HIT reads one chain word or one home slot and needs no status: an
//!   unpublished row is all zeros and a target id is never `0`, and a word the
//!   VM wrote only ever names a real supertype.
//! * A MISS is answered only after the status word reads
//!   [`STATUS_HIERARCHY_ONLY`], and the word that decides it (the chain word,
//!   the home slot, the bloom) is read AGAIN after that load: x86-64 never
//!   reorders a load with an older load, so a status that reads published
//!   implies every word stored before it is visible to the later loads. (The
//!   hit compare that ran before the status load may have raced the
//!   publication; its `0` is not trusted for the miss.)
//! * The miss also needs the target-side screen of the display miss arm
//!   (`Lowerer::typecheck_display_miss_for`) and the duplicate-class-name latch,
//!   exactly as `typecheck_display_answer_bytes`.
//! * Anything else -- an id at or past [`ROWS_CAP`] (every synthetic id at or
//!   past `0x8000_0000` included), a row not yet published, a verdict-0 class,
//!   an unplaced interface, a raised latch -- FALLS THROUGH to the code after
//!   the arm (the seen ways, the display arm, the helper), which is byte for
//!   byte what the site emitted before.

/// Bytes per row; a row is one cache line.
pub const ROW_BYTES: usize = 64;
/// `log2(ROW_BYTES)`: the compiled arm scales the class id with `SHL ECX, 6`.
pub const ROW_SHIFT: u8 = 6;
/// `u32` words per row.
pub const ROW_WORDS: usize = ROW_BYTES / 4;
/// Class ids the table covers. 64 Ki rows of 64 bytes = 4 MiB of zeroed,
/// lazily committed address space per VM that compiles a type check. An id at
/// or past it (a VM with more classes, a synthetic id) falls back.
pub const ROWS_CAP: u32 = 1 << 16;
/// Total table bytes.
pub const ROWS_BYTES: usize = ROW_BYTES * (ROWS_CAP as usize);

/// Word 0's value once the row is filled for a class whose type-check answers
/// may depend on the instance (verdict `0`): only hits are answered.
pub const STATUS_PUBLISHED: u32 = 1;
/// Word 0's value once the row is filled for a class whose display carries the
/// hierarchy-only verdict: a miss is the helper's `false` too (subject to the
/// target screen and the latch).
pub const STATUS_HIERARCHY_ONLY: u32 = 2;
/// The deepest chain index a row stores (word `d` for depth `d`).
pub const PRIMARY_MAX_DEPTH: u32 = 7;
/// JIT round 21, lane tcmiss (TCARRAY21-2): word 0 carries the class's OWN
/// depth (its index in its own chain) at this bit offset -- byte 1 of the
/// row -- when that depth is 1..=[`PRIMARY_MAX_DEPTH`], else `0`. The status
/// is byte 0, so every arm compares the status as a BYTE. Stored in the same
/// Release store as the status, so a reader that sees the status sees the
/// depth. Read by [`aastore_subtype_bytes`], whose target (the array's
/// component) is known only at run time.
pub const STATUS_DEPTH_SHIFT: u32 = 8;
/// The status byte of word 0.
pub const STATUS_MASK: u32 = 0xFF;
/// First of the two words holding the 64-bit interface bloom (byte 32).
pub const BLOOM_WORD: usize = 8;
/// First interface home slot.
pub const SLOT_WORD: usize = 10;
/// Number of interface home slots.
pub const SLOTS: u32 = 6;

const _: () = assert!(1usize << ROW_SHIFT == ROW_BYTES);
const _: () = assert!(SLOT_WORD + SLOTS as usize == ROW_WORDS);
const _: () = assert!((PRIMARY_MAX_DEPTH as usize) < BLOOM_WORD);
// Every displacement the arm uses fits a disp8.
const _: () = assert!(ROW_BYTES <= 127);
// The scaled class id stays below 2^32 (`SHL ECX` keeps it zero-extended).
const _: () = assert!((ROWS_CAP as u64) << ROW_SHIFT <= u32::MAX as u64);

/// The home slot word of interface `iface`.
pub const fn iface_slot_word(iface: u32) -> usize {
    SLOT_WORD + (iface % SLOTS) as usize // Cast: < SLOTS
}

/// The bloom bit of interface `iface`.
pub const fn bloom_bit(iface: u32) -> u32 {
    iface % 64
}

/// The row of a class whose superclass chain (root first, ending with the
/// class) is `chain` and whose transitive interface set is `interfaces`. The
/// status word is filled too; the VM stores it last.
pub fn row_words(chain: &[u32], interfaces: &[u32], hierarchy_only: bool) -> [u32; ROW_WORDS] {
    let mut w = [0u32; ROW_WORDS];
    w[0] = if hierarchy_only {
        STATUS_HIERARCHY_ONLY
    } else {
        STATUS_PUBLISHED
    };
    // JIT round 21, lane tcmiss: the class's own depth in byte 1.
    // Cast: a chain index, compared against a small bound first.
    let own_depth = chain.len().saturating_sub(1) as u32;
    if (1..=PRIMARY_MAX_DEPTH).contains(&own_depth) {
        w[0] |= own_depth << STATUS_DEPTH_SHIFT;
    }
    for d in 1..=PRIMARY_MAX_DEPTH as usize {
        w[d] = chain.get(d).copied().unwrap_or(0);
    }
    let mut bloom = 0u64;
    for &i in interfaces {
        bloom |= 1u64 << bloom_bit(i);
        let slot = iface_slot_word(i);
        if w[slot] == 0 {
            w[slot] = i;
        }
    }
    // Cast: the two halves of the little-endian 64-bit bloom.
    w[BLOOM_WORD] = bloom as u32;
    w[BLOOM_WORD + 1] = (bloom >> 32) as u32;
    w
}

/// `CRATONVM_JIT_TYPECHECK_ROWS` (JIT round 21, lane dispatch; perf,
/// **default ON** since `3107734ec6` (`WARNING t1-user-flip-20261027`), `0`
/// turns it off): the optimizing tier's `instanceof` against an interface
/// tests the receiver's type-check row after the seen ways, before the display
/// arm. History: opt-in after the r21 w1g A/B, where the row placed IN FRONT
/// of the seen ways lost on the common hit (`R21DispatchTypecheck`
/// instanceof-subclass 378 vs 261 ns, instanceof-interface 471 vs 316) and
/// won only on receivers with seven to nine interfaces (instanceof-wide-iface
/// 1080 vs 1755); the r21 w3a/w3e host A/Bs (row after the seen ways) were
/// the basis of the first default-ON answer. Why the hit
/// path lost is open (page `jr21-dispatch-typecheck-rows-FIXED-20261029.md`). Needs
/// the displays (`CRATONVM_JIT_TYPECHECK_DISPLAY_ARM` and what it needs).
/// Read per site.
pub fn typecheck_rows_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_TYPECHECK_ROWS")
}

/// `CRATONVM_JIT_TYPECHECK_ROWS_ALL` (JIT round 21, lane dispatch; opt-in,
/// only read with [`typecheck_rows_enabled`]): the row arm also serves class
/// targets at depth 1..=7 and `checkcast` (hits only), the coverage of the
/// round-21 first build. Read per site.
pub fn typecheck_rows_all_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_TYPECHECK_ROWS_ALL")
}

/// What a row arm tests the receiver against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RowTarget {
    /// A class at this depth of its own chain (1..=[`PRIMARY_MAX_DEPTH`]).
    Class { depth: u32 },
    /// An interface.
    Interface,
}

/// One emitted row arm: its bytes and the offsets of the rel32 fields of the
/// jumps to the site's `1` (`hit`) and `0` (`miss`; empty when the arm was
/// built without the miss half). Every other outcome falls through past the
/// last byte.
#[derive(Debug)]
pub(crate) struct RowArm {
    pub(crate) bytes: Vec<u8>,
    pub(crate) hit: Vec<usize>,
    pub(crate) miss: Vec<usize>,
}

/// The row arm's bytes, with RAX = a non-null PLAIN object (the caller has
/// screened null and arrays). `rows` is the table's base (64-byte aligned),
/// `target` the class / interface id, `miss_latch` the duplicate-class-name
/// latch's address when the site may answer a miss (`None`: hits only).
/// Writes RCX, R11 and the flags; RAX is preserved. `None` when the target is
/// not one a row answers (id `0`, depth `0` or past [`PRIMARY_MAX_DEPTH`]).
///
/// ```text
///   MOV ECX, [RAX]                    ; class id (zero-extends RCX)
///   CMP ECX, ROWS_CAP ; JAE fall      ; synthetic ids, a huge VM
///   SHL ECX, 6                        ; the row's offset
///   MOV R11, imm64 rows
///   CMP DWORD [R11+RCX+w], target ; JE hit      ; w = depth*4 / home slot
/// (miss half)
///   CMP DWORD [R11+RCX], 2 ; JNE fall           ; hierarchy-only row
///   CMP DWORD [R11+RCX+w], target ; JE hit      ; re-read after the status
///   (iface) TEST BYTE [R11+RCX+32+b/8], 1<<(b%8) ; JNZ fall   ; bloom
///   MOV R11, imm64 latch ; CMP BYTE [R11], 0 ; JNE fall
///   JMP miss
/// fall:
/// ```
pub(crate) fn typecheck_row_bytes(
    rows: usize,
    target: u32,
    kind: RowTarget,
    miss_latch: Option<usize>,
) -> Option<RowArm> {
    if target == 0 || rows == 0 || rows % ROW_BYTES != 0 {
        return None;
    }
    let word = match kind {
        // Cast: depth <= PRIMARY_MAX_DEPTH, a small index.
        RowTarget::Class { depth } if (1..=PRIMARY_MAX_DEPTH).contains(&depth) => depth as usize,
        RowTarget::Class { .. } => return None,
        RowTarget::Interface => iface_slot_word(target),
    };
    // Cast: < ROW_BYTES <= 127, a disp8.
    let disp = (word * 4) as u8;
    let mut b: Vec<u8> = Vec::with_capacity(96);
    let mut fall: Vec<usize> = Vec::new();
    let mut hit: Vec<usize> = Vec::new();
    let mut miss: Vec<usize> = Vec::new();
    let jcc = |b: &mut Vec<u8>, cc: u8| -> usize {
        b.extend_from_slice(&[0x0F, cc]);
        let at = b.len();
        b.extend_from_slice(&[0, 0, 0, 0]);
        at
    };
    b.extend_from_slice(&[0x8B, 0x08]); // MOV ECX, [RAX]
    b.extend_from_slice(&[0x81, 0xF9]); // CMP ECX, imm32
    b.extend_from_slice(&ROWS_CAP.to_le_bytes());
    fall.push(jcc(&mut b, 0x83)); // JAE fall
    b.extend_from_slice(&[0xC1, 0xE1, ROW_SHIFT]); // SHL ECX, 6
    b.extend_from_slice(&[0x49, 0xBB]); // MOV R11, imm64
    b.extend_from_slice(&(rows as u64).to_le_bytes()); // Cast: a data address
    let cmp_word = |b: &mut Vec<u8>| {
        // CMP DWORD [R11 + RCX*1 + disp8], imm32 (REX.B, 81 /7, SIB: base
        // R11, index RCX).
        b.extend_from_slice(&[0x41, 0x81, 0x7C, 0x0B, disp]);
        b.extend_from_slice(&target.to_le_bytes());
    };
    cmp_word(&mut b);
    hit.push(jcc(&mut b, 0x84)); // JE hit
    if let Some(latch) = miss_latch {
        // CMP BYTE [R11 + RCX*1 + 0], imm8 (80 /7 ib): the status byte
        // (byte 1 holds the class's depth since JIT round 21, lane tcmiss).
        // Cast: the status constant is a small positive imm8.
        b.extend_from_slice(&[0x41, 0x80, 0x7C, 0x0B, 0x00, STATUS_HIERARCHY_ONLY as u8]);
        fall.push(jcc(&mut b, 0x85)); // JNE fall
        cmp_word(&mut b); // the deciding word, read after the status
        hit.push(jcc(&mut b, 0x84)); // JE hit
        if kind == RowTarget::Interface {
            let bit = bloom_bit(target);
            // Cast: byte 32 + bit/8 < 40, a disp8; the mask is one bit.
            let byte = (BLOOM_WORD * 4) as u8 + (bit / 8) as u8;
            let mask = 1u8 << (bit % 8);
            // TEST BYTE [R11 + RCX*1 + disp8], imm8 (F6 /0 ib).
            b.extend_from_slice(&[0x41, 0xF6, 0x44, 0x0B, byte, mask]);
            fall.push(jcc(&mut b, 0x85)); // JNZ fall: listed, not placed
        }
        b.extend_from_slice(&[0x49, 0xBB]); // MOV R11, imm64
        b.extend_from_slice(&(latch as u64).to_le_bytes()); // Cast: a static's address
        b.extend_from_slice(&[0x41, 0x80, 0x3B, 0x00]); // CMP BYTE [R11], 0
        fall.push(jcc(&mut b, 0x85)); // JNE fall
        b.push(0xE9); // JMP miss
        let at = b.len();
        b.extend_from_slice(&[0, 0, 0, 0]);
        miss.push(at);
    }
    let end = b.len();
    for at in fall {
        // Cast: a forward distance inside one short sequence.
        let rel = (end - (at + 4)) as i32;
        b[at..at + 4].copy_from_slice(&rel.to_le_bytes());
    }
    Some(RowArm {
        bytes: b,
        hit,
        miss,
    })
}

/// `CRATONVM_JIT_TYPECHECK_ROW_FIRST` (JIT round 21, lane tcperf; perf,
/// **default ON**, `=0` restores the round's placement): the optimizing tier's
/// `instanceof` / `checkcast` against a CLASS at depth 1..=7 tests the
/// receiver's type-check row FIRST, before the exact-id compare, the seen
/// ways and the display arm, in the status-first form
/// ([`typecheck_row_first_bytes`]): HotSpot's primary-supers shape, one
/// receiver-dependent branch per check. Page
/// `jr21-tcperf-class-target-typecheck-cost-FIXED-20261015.md`.
/// Needs [`typecheck_rows_enabled`] (and what it needs). Read per site.
pub fn typecheck_row_first_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_TYPECHECK_ROW_FIRST")
}

/// `CRATONVM_JIT_TYPECHECK_ROW_FIRST_IFACE` (JIT round 21, lane tcperf; perf,
/// **default ON**, `=0` restores the round's placement): as
/// [`typecheck_row_first_enabled`] for an INTERFACE target: the home-slot hit
/// (and, for `instanceof`, the bloom-proved miss) first, before the exact-id
/// compare and the seen ways; `checkcast` takes the hit half. Read per site.
pub fn typecheck_row_first_iface_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_TYPECHECK_ROW_FIRST_IFACE")
}

/// `CRATONVM_JIT_TYPECHECK_BRANCH_FUSION` (JIT round 21, lane tcfuse,
/// TCPERF21-2 = DISPATCH21-3; perf, **default ON**, `=0` restores the round
/// head): an optimizing-tier `instanceof` whose only use is the fused
/// `Cmp(Eq|Ne, x, 0)` of the `If` ending its block sends its inline `1` / `0`
/// answers straight to that `If`'s two edges instead of materialising the
/// answer, storing it and testing it again. Only the helper path still
/// materialises it (and the `If` tests it as before). Page
/// `jr21-dispatch-typecheck-rows-FIXED-20261029.md`. Read per site.
///
/// It engaged nowhere until 2026-10-26 (its admission counted the check's
/// memory-chain edges as uses and refused the `ifeq`'s own frame state);
/// fixed, it saves 7-19 % of the instructions of every `instanceof` kernel of
/// `R20PAllocTypecheck` / `R21TcfuseRows` (in-process counts, page
/// `jr21-typecheck-lane-perf-rows`, retired).
pub fn typecheck_branch_fusion_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_TYPECHECK_BRANCH_FUSION")
}

/// `CRATONVM_JIT_TYPECHECK_SHARED_CLASS_WORD` (JIT round 21, lane tcfuse,
/// TCPERF21-3; perf, **opt-in** since 2026-10-26, `1` turns it on): two or
/// more optimizing-tier `instanceof` / `checkcast` of the SAME value share one
/// `Op::ClassWordOf` of it (an extra input of each check, planted by
/// `ir_optimize::typecheck_class_word`), and each check's status-first row
/// arm starts from that word instead of re-reading the receiver's header
/// behind its own null and kind screens. Read once per compile (the pass) and
/// per site (the lowering, which only reads an input the pass planted).
///
/// Opt-in after the deterministic in-process instruction counts of
/// 2026-10-26 (`jr21-typecheck-lane-perf-rows`, retired): ON cost 2-11 % MORE
/// instructions per call on every kernel with two to four checks of a value
/// (`R20PAllocTypecheck` instanceof-subclass 5115 vs 4545, -interface 5229 vs
/// 4909; `R21TcfuseRows` -deep 6029 vs 5837, -else-chain 5118 vs 5010,
/// cast-then-instanceof 5524 vs 5232) and saved 0.06 % with six
/// (-wide-iface). The word's own null / kind screens, register copy and home
/// store cost what the per-check screens it replaces save.
pub fn typecheck_shared_class_word_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_on("CRATONVM_JIT_TYPECHECK_SHARED_CLASS_WORD")
}

/// JIT round 21, lane tcperf: the row arm in its STATUS-FIRST form, the one
/// a site emits ahead of everything else ([`typecheck_row_first_enabled`]).
/// Same contract as [`typecheck_row_bytes`] (RAX = a non-null PLAIN object;
/// writes RCX, R11 and the flags; RAX preserved; every other outcome falls
/// through past the last byte), but the row's status word is read BEFORE the
/// deciding word and must read [`STATUS_HIERARCHY_ONLY`] for any answer:
///
/// ```text
///   MOV ECX, [RAX] ; CMP ECX, ROWS_CAP ; JAE fall ; SHL ECX, 6
///   MOV R11, imm64 rows
///   CMP DWORD [R11+RCX], 2 ; JNE fall           ; hierarchy-only row
///   CMP DWORD [R11+RCX+w], target ; JE hit      ; w = depth*4 / home slot
/// (miss half)
///   (iface) TEST BYTE [R11+RCX+32+b/8], 1<<(b%8) ; JNZ fall   ; bloom
///   MOV R11, imm64 latch ; CMP BYTE [R11], 0 ; JNE fall
///   JMP miss
/// fall:
/// ```
///
/// Why: on a verdict-1 receiver (every receiver of an ordinary hierarchy) the
/// check is one row-status compare that always passes and ONE compare that
/// decides, both from the same cache line and independent of each other --
/// HotSpot's `[klass + super_check_offset] == target`. The round's arm read
/// the deciding word first and re-read it after the status (two decisions
/// on the same word), and ran after the exact-id compare and the two seen
/// ways, i.e. three or four receiver-dependent branches before it.
///
/// Soundness, beyond [`typecheck_row_bytes`]'s argument: the status is stored
/// LAST with Release and x86-64 never reorders a load with an older load, so
/// once the status load reads published every later load of the row sees the
/// words stored before it -- the deciding word needs no second read, for the
/// hit as for the miss. A verdict-0 class (status `1`), an unpublished row
/// (`0`), an id at or past [`ROWS_CAP`] fall through to the site's code
/// before this arm, which answers them exactly as before.
pub(crate) fn typecheck_row_first_bytes(
    rows: usize,
    target: u32,
    kind: RowTarget,
    miss_latch: Option<usize>,
) -> Option<RowArm> {
    if target == 0 || rows == 0 || rows % ROW_BYTES != 0 {
        return None;
    }
    let word = match kind {
        // Cast: depth <= PRIMARY_MAX_DEPTH, a small index.
        RowTarget::Class { depth } if (1..=PRIMARY_MAX_DEPTH).contains(&depth) => depth as usize,
        RowTarget::Class { .. } => return None,
        RowTarget::Interface => iface_slot_word(target),
    };
    // Cast: < ROW_BYTES <= 127, a disp8.
    let disp = (word * 4) as u8;
    let mut b: Vec<u8> = Vec::with_capacity(80);
    let mut fall: Vec<usize> = Vec::new();
    let mut hit: Vec<usize> = Vec::new();
    let mut miss: Vec<usize> = Vec::new();
    let jcc = |b: &mut Vec<u8>, cc: u8| -> usize {
        b.extend_from_slice(&[0x0F, cc]);
        let at = b.len();
        b.extend_from_slice(&[0, 0, 0, 0]);
        at
    };
    b.extend_from_slice(&[0x8B, 0x08]); // MOV ECX, [RAX]
    b.extend_from_slice(&[0x81, 0xF9]); // CMP ECX, imm32
    b.extend_from_slice(&ROWS_CAP.to_le_bytes());
    fall.push(jcc(&mut b, 0x83)); // JAE fall
    b.extend_from_slice(&[0xC1, 0xE1, ROW_SHIFT]); // SHL ECX, 6
    b.extend_from_slice(&[0x49, 0xBB]); // MOV R11, imm64
    b.extend_from_slice(&(rows as u64).to_le_bytes()); // Cast: a data address

    // CMP BYTE [R11 + RCX*1 + 0], imm8 (80 /7 ib): the status byte, FIRST
    // (byte 1 holds the class's depth since JIT round 21, lane tcmiss).
    // Cast: the status constant is a small positive imm8.
    b.extend_from_slice(&[0x41, 0x80, 0x7C, 0x0B, 0x00, STATUS_HIERARCHY_ONLY as u8]);
    fall.push(jcc(&mut b, 0x85)); // JNE fall

    // CMP DWORD [R11 + RCX*1 + disp8], imm32 (REX.B, 81 /7, SIB): the
    // deciding word, read once.
    b.extend_from_slice(&[0x41, 0x81, 0x7C, 0x0B, disp]);
    b.extend_from_slice(&target.to_le_bytes());
    hit.push(jcc(&mut b, 0x84)); // JE hit
    if let Some(latch) = miss_latch {
        if kind == RowTarget::Interface {
            let bit = bloom_bit(target);
            // Cast: byte 32 + bit/8 < 40, a disp8; the mask is one bit.
            let byte = (BLOOM_WORD * 4) as u8 + (bit / 8) as u8;
            let mask = 1u8 << (bit % 8);
            // TEST BYTE [R11 + RCX*1 + disp8], imm8 (F6 /0 ib).
            b.extend_from_slice(&[0x41, 0xF6, 0x44, 0x0B, byte, mask]);
            fall.push(jcc(&mut b, 0x85)); // JNZ fall: listed, not placed
        }
        b.extend_from_slice(&[0x49, 0xBB]); // MOV R11, imm64
        b.extend_from_slice(&(latch as u64).to_le_bytes()); // Cast: a static's address
        b.extend_from_slice(&[0x41, 0x80, 0x3B, 0x00]); // CMP BYTE [R11], 0
        fall.push(jcc(&mut b, 0x85)); // JNE fall
        b.push(0xE9); // JMP miss
        let at = b.len();
        b.extend_from_slice(&[0, 0, 0, 0]);
        miss.push(at);
    }
    let end = b.len();
    for at in fall {
        // Cast: a forward distance inside one short sequence.
        let rel = (end - (at + 4)) as i32;
        b[at..at + 4].copy_from_slice(&rel.to_le_bytes());
    }
    Some(RowArm {
        bytes: b,
        hit,
        miss,
    })
}

/// `CRATONVM_JIT_TYPECHECK_ARRAY_ARM` (JIT round 21, lane tcarray; perf,
/// **default ON**, `=0` restores the round head): the optimizing tier's
/// `checkcast` / `instanceof` against a REFERENCE-ARRAY class `T[]` answers a
/// reference-array receiver inline ([`typecheck_array_prefix_bytes`] plus the
/// component's row / display arms) instead of calling the helper for every
/// array. Page `docs/known-issues/jit/jr21-tcarray-array-target-typecheck-cost-20261014.md`.
/// Read per site.
///
/// j27 wave 2 (lane TCARRAY): dev's array arm is lane typecheck's
/// TYPECHECK21-3 (`CRATONVM_JIT_TYPECHECK_ARRAY_TARGETS`,
/// `ir_lower::typecheck_array_hit_bytes`), not this lane's prefix; what this
/// switch still gates is the r21 addition re-landed on that arm, the miss
/// guard ([`typecheck_array_miss_guard_bytes`], with
/// [`typecheck_array_miss_enabled`]). `=0` keeps dev's hit-only arm.
pub fn typecheck_array_arm_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_TYPECHECK_ARRAY_ARM")
}

/// `CRATONVM_JIT_TYPECHECK_ARRAY_MEMO` (JIT round 21, lane tcarray; perf,
/// **default ON**, `=0` restores the round head): `jit_checkcast` /
/// `jit_instanceof` memoize a proven `true` for a REFERENCE-ARRAY receiver,
/// keyed by its component class id, as they memoize a plain receiver's. Read
/// once per VM (`SupersDisplays::new`), never per call.
pub fn typecheck_array_memo_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_TYPECHECK_ARRAY_MEMO")
}

/// The header byte at `KIND_TAGS_BYTE_OFFSET` of exactly the REFERENCE arrays:
/// kind `Array`, element type `Reference` (`0`), reserved bits zero (always,
/// on an array). A plain object has kind bits `0`, a primitive array a non-zero
/// element type, so one byte compare selects reference arrays and nothing else.
// Cast: two `#[repr(u8)]` enum discriminants packed into the tag byte.
pub const REFERENCE_ARRAY_KIND_BYTE: u8 = (cratonvm_types::ObjectKind::Array as u8)
    | ((cratonvm_types::ArrayElementType::Reference as u8) << 2);

/// The array-target prefix's bytes and the offsets of its rel32 fields.
#[derive(Debug)]
pub(crate) struct ArrayPrefix {
    pub(crate) bytes: Vec<u8>,
    /// Jumps to the site's hit (`1` / the cast succeeds).
    pub(crate) hit: Vec<usize>,
    /// Jumps taken by a receiver that is NOT a reference array: the caller
    /// lands them on the code the site emitted before (its plain-object
    /// screens, which send every other array to the helper).
    pub(crate) not_reference_array: Vec<usize>,
}

/// JIT round 21, lane tcarray (`CRATONVM_JIT_TYPECHECK_ARRAY_ARM`): the first
/// half of the inline test against a reference-array class `T[]`, with RAX = a
/// non-null object. On a reference array the header's class id is its
/// COMPONENT's (`typecheck::array_descriptor_of`), so the component test is the
/// plain class test on the same header word:
///
/// ```text
///   CMP BYTE [RAX+KIND_TAGS_BYTE_OFFSET], REFERENCE_ARRAY_KIND_BYTE
///   JNE not_reference_array
/// (component None: the target is Object[])
///   JMP hit                         ; every reference array is an Object[]
/// (component Some(c))
///   CMP DWORD [RAX], c ; JE hit     ; exactly a T[]
///   (falls through: the caller emits the component's row / display hit arms
///    on the same RAX, then a jump to the helper)
/// ```
///
/// Writes the flags only; RAX is preserved. `None` for a component id `0`
/// with `Some` (`java/lang/Object` is asked as `None`).
pub(crate) fn typecheck_array_prefix_bytes(component: Option<u32>) -> Option<ArrayPrefix> {
    if component == Some(0) {
        return None;
    }
    let mut b: Vec<u8> = Vec::with_capacity(32);
    let mut hit: Vec<usize> = Vec::new();
    let mut other: Vec<usize> = Vec::new();
    let rel32 = |b: &mut Vec<u8>| -> usize {
        let at = b.len();
        b.extend_from_slice(&[0, 0, 0, 0]);
        at
    };
    // CMP BYTE [RAX+disp8], imm8 (80 /7 ib, ModRM 0x78).
    b.extend_from_slice(&[
        0x80,
        0x78,
        cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8, // Cast: x86-64 disp8
        REFERENCE_ARRAY_KIND_BYTE,
    ]);
    b.extend_from_slice(&[0x0F, 0x85]); // JNE not_reference_array
    other.push(rel32(&mut b));
    match component {
        None => {
            b.push(0xE9); // JMP hit
            hit.push(rel32(&mut b));
        }
        Some(c) => {
            // CMP DWORD [RAX+0], imm32 (81 /7 id, ModRM 0x78 disp8 0).
            b.extend_from_slice(&[0x81, 0x78, 0x00]);
            b.extend_from_slice(&c.to_le_bytes());
            b.extend_from_slice(&[0x0F, 0x84]); // JE hit
            hit.push(rel32(&mut b));
        }
    }
    Some(ArrayPrefix {
        bytes: b,
        hit,
        not_reference_array: other,
    })
}

/// `CRATONVM_JIT_TYPECHECK_ARRAY_MISS` (JIT round 21, lane tcmiss, proposal
/// TCARRAY21-1; perf, **default ON**, `=0` restores lane tcarray's hit-only
/// array arm): the optimizing tier's `instanceof T[]` answers its MISSES
/// inline too -- a plain object or a primitive array
/// ([`typecheck_array_not_reference_miss_bytes`]), and a reference array whose
/// hierarchy-only component the component's row / display proves not a subtype
/// of `T`. Page `docs/known-issues/jit/jr21-tcmiss-typecheck-misses-and-noncall-20261014.md`.
/// Needs [`typecheck_array_arm_enabled`]. Read per site.
///
/// j27 wave 2 (lane TCARRAY): re-landed on dev's TYPECHECK21-3 arm as the
/// plain-object / primitive-array half only
/// ([`typecheck_array_miss_guard_bytes`], `ir_lower`'s
/// `emit_typecheck_array_arm`); the component miss rule is not re-landed (a
/// reference array dev's arm does not prove a hit still asks the helper).
pub fn typecheck_array_miss_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_TYPECHECK_ARRAY_MISS")
}

/// Whether an `instanceof T[]` emits [`typecheck_array_miss_guard_bytes`]:
/// both switches, as the miss always needed the arm. Read per site.
pub fn typecheck_array_miss_guard_enabled() -> bool {
    typecheck_array_arm_enabled() && typecheck_array_miss_enabled()
}

/// `CRATONVM_JIT_AASTORE_SUBTYPE_ARM` (JIT round 21, lane tcmiss, proposal
/// TCARRAY21-2; perf, **default ON**, `=0` restores the exact-component test
/// alone): the optimizing tier's `aastore` answers a SUBTYPE value of the
/// array's run-time component inline ([`aastore_subtype_bytes`]) instead of
/// calling `jit_aastore_type_check`. Needs [`typecheck_rows_enabled`] (the
/// rows exist). Read per site.
pub fn aastore_subtype_arm_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_AASTORE_SUBTYPE_ARM")
}

/// `CRATONVM_JIT_TYPECHECK_NONCALL` (JIT round 21, lane tcmiss, proposal
/// TCPERF21-1; perf, **default ON**, `=0` restores the round head): an
/// optimizing-tier `instanceof` / `checkcast` is no longer a CALL for the
/// register allocator's XMM bank (`ir_lower::ir_lower_machine_model` drops the
/// caller-saved XMM clobber at a position only type checks occupy); the
/// helper call, which exists only on the slow path, reloads every FP value
/// resident in a caller-saved XMM across it from its home word after the call,
/// as the back-edge poll does. The checks stay SAFEPOINTS (the slow path still
/// publishes its oop map). Inert under `CRATONVM_JIT_IR_LS_SPLITS=1` (the
/// reload is argued for whole-range residency only). Read per compile.
pub fn typecheck_noncall_enabled() -> bool {
    cratonvm_types::flags::runtime_flag_default_on("CRATONVM_JIT_TYPECHECK_NONCALL")
}

/// The element-type bits (2..=5) of the kind-tags header byte: zero on a
/// reference array, the primitive's tag on a primitive one.
const ELEMENT_TYPE_BITS: u8 = 0x0F << 2;

/// [`typecheck_array_not_reference_miss_bytes`]'s bytes and the offsets of
/// their rel32 fields.
#[derive(Debug)]
pub(crate) struct NotReferenceArrayMiss {
    pub(crate) bytes: Vec<u8>,
    /// Jumps to the helper.
    pub(crate) slow: Vec<usize>,
    /// Jumps to the site's `0`.
    pub(crate) zero: Vec<usize>,
}

/// JIT round 21, lane tcmiss (`CRATONVM_JIT_TYPECHECK_ARRAY_MISS`): what an
/// `instanceof T[]` (`T` a reference type) answers for a non-null receiver
/// that is NOT a reference array, emitted where
/// [`typecheck_array_prefix_bytes`]'s `not_reference_array` edges land. RAX = the
/// receiver; writes RCX, R11 and the flags.
///
/// ```text
///   TEST BYTE [RAX+6], KIND_TAG_BYTE_MASK ; JNE array
///   CMP DWORD [RAX], 0 ; JL slow              ; ids >= 0x8000_0000: the helper's
///   MOV R11, imm64 latch ; CMP BYTE [R11], 0 ; JNE slow     ; screens, as every miss
///   JMP zero                                  ; a plain object is never an array
/// array:
///   MOVZX ECX, BYTE [RAX+6] ; AND ECX, 3 ; CMP ECX, Array ; JNE slow
///   TEST BYTE [RAX+6], ELEMENT_TYPE_BITS ; JZ slow
///   JMP zero                                  ; a primitive array is only its own type
/// ```
///
/// JVMS 6.5: an object is an instance of an array type only when it is an
/// array, and a primitive array only of its own type, `Object`, `Cloneable`
/// and `Serializable` -- never of a reference-array type. The VM's helper
/// agrees in both modes (`array_cast_resolved_verdict` answers a primitive
/// receiver `false` before any name; `array_cast_fast_verdict` compares the
/// primitive's tag with the target's reference component; a plain receiver's
/// class-hierarchy, proxy, stamp and by-name rules never name a `[` class).
/// The synthetic-id and duplicate-name screens are kept for the plain object
/// anyway, as every other inline miss keeps them.
pub(crate) fn typecheck_array_not_reference_miss_bytes(latch: usize) -> NotReferenceArrayMiss {
    // Cast: a `#[repr(u8)]` discriminant.
    let array_kind = cratonvm_types::ObjectKind::Array as u8;
    const KIND_TAGS: u8 = cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8; // Cast: x86-64 disp8
    let mut b: Vec<u8> = Vec::with_capacity(80);
    let mut slow: Vec<usize> = Vec::new();
    let mut zero: Vec<usize> = Vec::new();
    let rel32 = |b: &mut Vec<u8>| -> usize {
        let at = b.len();
        b.extend_from_slice(&[0, 0, 0, 0]);
        at
    };
    // TEST BYTE [RAX+disp8], imm8 (F6 /0 ib) ; JNE array.
    b.extend_from_slice(&[0xF6, 0x40, KIND_TAGS, cratonvm_types::KIND_TAG_BYTE_MASK]);
    b.extend_from_slice(&[0x0F, 0x85]);
    let to_array = rel32(&mut b);
    // CMP DWORD [RAX+0], 0 (83 /7 ib) ; JL slow.
    b.extend_from_slice(&[0x83, 0x78, 0x00, 0x00]);
    b.extend_from_slice(&[0x0F, 0x8C]);
    slow.push(rel32(&mut b));
    b.extend_from_slice(&[0x49, 0xBB]); // MOV R11, imm64
    b.extend_from_slice(&(latch as u64).to_le_bytes()); // Cast: a static's address
    b.extend_from_slice(&[0x41, 0x80, 0x3B, 0x00]); // CMP BYTE [R11], 0
    b.extend_from_slice(&[0x0F, 0x85]); // JNE slow
    slow.push(rel32(&mut b));
    b.push(0xE9); // JMP zero
    zero.push(rel32(&mut b));
    // array:
    let array_at = b.len();
    // Cast: a forward distance inside one short sequence.
    let rel = (array_at - (to_array + 4)) as i32;
    b[to_array..to_array + 4].copy_from_slice(&rel.to_le_bytes());
    b.extend_from_slice(&[0x0F, 0xB6, 0x48, KIND_TAGS]); // MOVZX ECX, BYTE [RAX+disp8]
    b.extend_from_slice(&[0x83, 0xE1, cratonvm_types::KIND_TAG_BYTE_MASK]); // AND ECX, imm8
    b.extend_from_slice(&[0x83, 0xF9, array_kind]); // CMP ECX, imm8
    b.extend_from_slice(&[0x0F, 0x85]); // JNE slow
    slow.push(rel32(&mut b));
    b.extend_from_slice(&[0xF6, 0x40, KIND_TAGS, ELEMENT_TYPE_BITS]); // TEST BYTE [RAX+disp8], imm8
    b.extend_from_slice(&[0x0F, 0x84]); // JZ slow
    slow.push(rel32(&mut b));
    b.push(0xE9); // JMP zero
    zero.push(rel32(&mut b));
    NotReferenceArrayMiss {
        bytes: b,
        slow,
        zero,
    }
}

/// j27 wave 2 (lane TCARRAY; `CRATONVM_JIT_TYPECHECK_ARRAY_MISS`): the
/// not-reference-array miss block ([`typecheck_array_not_reference_miss_bytes`])
/// behind its own reference-array test, so that it can sit IN FRONT of dev's
/// array arm (`ir_lower::typecheck_array_hit_bytes`, whose reference arrays it
/// must reach): RAX = a non-null receiver; a reference array falls through
/// past the last byte, everything else leaves by the block's `slow` / `zero`
/// edges. Writes RCX, R11 and the flags; RAX is preserved.
///
/// ```text
///   CMP BYTE [RAX+KIND_TAGS_BYTE_OFFSET], REFERENCE_ARRAY_KIND_BYTE ; JE past
///   (the not-reference-array miss block, which never falls through)
/// past:
/// ```
pub(crate) fn typecheck_array_miss_guard_bytes(latch: usize) -> NotReferenceArrayMiss {
    let block = typecheck_array_not_reference_miss_bytes(latch);
    let mut b: Vec<u8> = Vec::with_capacity(block.bytes.len() + 10);
    b.extend_from_slice(&[
        0x80,
        0x78,
        cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8, // Cast: x86-64 disp8
        REFERENCE_ARRAY_KIND_BYTE,
    ]);
    b.extend_from_slice(&[0x0F, 0x84]); // JE past (rel32 = the block's length)
    let past = i32::try_from(block.bytes.len()).expect("a short sequence");
    b.extend_from_slice(&past.to_le_bytes());
    let at = b.len();
    b.extend_from_slice(&block.bytes);
    NotReferenceArrayMiss {
        bytes: b,
        slow: block.slow.into_iter().map(|e| at + e).collect(),
        zero: block.zero.into_iter().map(|e| at + e).collect(),
    }
}

/// JIT round 21, lane tcmiss (`CRATONVM_JIT_AASTORE_SUBTYPE_ARM`, proposal
/// TCARRAY21-2): the `aastore` element check for a value whose class is a
/// SUBCLASS of the array's run-time component -- HotSpot's three loads
/// (element klass, its `super_check_offset`, the value klass's word there).
/// Entry: RAX = the value (a non-null PLAIN object), EDX = its class id
/// (zero-extended), R11 = the array (a non-null reference array whose header
/// id -- its COMPONENT's -- is neither `0` nor EDX: the caller's `Object[]` and
/// exact tests came first). Writes RAX, RCX, RDX, R11 and the flags. The
/// returned edges jump to the store (the check passed); every other outcome
/// falls through past the last byte to the helper.
///
/// ```text
///   MOV EAX, [R11]                         ; C, the component id
///   CMP EAX, ROWS_CAP ; JAE fall
///   CMP EDX, ROWS_CAP ; JAE fall           ; synthetic ids, a huge VM
///   MOV ECX, EAX ; SHL ECX, 6 ; SHL EDX, 6
///   MOV R11, imm64 rows
///   MOVZX ECX, BYTE [R11+RCX+1]            ; C's own depth (0: none / unpublished)
///   TEST ECX, ECX ; JZ fall
///   LEA RDX, [RDX+RCX*4]
///   CMP [R11+RDX], EAX ; JE hit            ; the value's chain word at that depth
/// fall:
/// ```
///
/// Only a HIT is answered. A value whose chain holds C at C's depth is an
/// instance of C (a chain word is `0` or a real superclass, the rows'
/// invariant; C's depth byte is stored with its status, so a non-zero byte is
/// the published depth), so the store is legal in JVMS 6.5 and in the helper's
/// verdict in both modes; an interface component, a deep class, an
/// unpublished row and every non-subtype still ask the helper, which alone
/// raises `ArrayStoreException`. `None` for a table address that is not one.
pub(crate) fn aastore_subtype_bytes(rows: usize) -> Option<(Vec<u8>, Vec<usize>)> {
    if rows == 0 || rows % ROW_BYTES != 0 {
        return None;
    }
    // Byte 1 of a row: its class's own depth. Cast: `STATUS_DEPTH_SHIFT / 8`
    // is 1, a disp8.
    let depth_byte = (STATUS_DEPTH_SHIFT / 8) as u8;
    let mut b: Vec<u8> = Vec::with_capacity(64);
    let mut fall: Vec<usize> = Vec::new();
    let jcc = |b: &mut Vec<u8>, cc: u8| -> usize {
        b.extend_from_slice(&[0x0F, cc]);
        let at = b.len();
        b.extend_from_slice(&[0, 0, 0, 0]);
        at
    };
    b.extend_from_slice(&[0x41, 0x8B, 0x03]); // MOV EAX, [R11]
    b.push(0x3D); // CMP EAX, imm32
    b.extend_from_slice(&ROWS_CAP.to_le_bytes());
    fall.push(jcc(&mut b, 0x83)); // JAE fall
    b.extend_from_slice(&[0x81, 0xFA]); // CMP EDX, imm32
    b.extend_from_slice(&ROWS_CAP.to_le_bytes());
    fall.push(jcc(&mut b, 0x83)); // JAE fall
    b.extend_from_slice(&[0x89, 0xC1]); // MOV ECX, EAX
    b.extend_from_slice(&[0xC1, 0xE1, ROW_SHIFT]); // SHL ECX, 6
    b.extend_from_slice(&[0xC1, 0xE2, ROW_SHIFT]); // SHL EDX, 6
    b.extend_from_slice(&[0x49, 0xBB]); // MOV R11, imm64
    b.extend_from_slice(&(rows as u64).to_le_bytes()); // Cast: a data address
    b.extend_from_slice(&[0x41, 0x0F, 0xB6, 0x4C, 0x0B, depth_byte]); // MOVZX ECX, BYTE [R11+RCX+1]
    b.extend_from_slice(&[0x85, 0xC9]); // TEST ECX, ECX
    fall.push(jcc(&mut b, 0x84)); // JZ fall
    b.extend_from_slice(&[0x48, 0x8D, 0x14, 0x8A]); // LEA RDX, [RDX + RCX*4]
    b.extend_from_slice(&[0x41, 0x39, 0x04, 0x13]); // CMP DWORD [R11 + RDX*1], EAX
    let hit = vec![jcc(&mut b, 0x84)]; // JE hit
    let end = b.len();
    for at in fall {
        // Cast: a forward distance inside one short sequence.
        let rel = (end - (at + 4)) as i32;
        b[at..at + 4].copy_from_slice(&rel.to_le_bytes());
    }
    Some((b, hit))
}

/// The arm, EXECUTED over a fake row table: `extern "C" fn(obj) -> 1 | 0 |
/// FELL` with a fake one-word object holding the class id.
#[cfg(all(test, target_arch = "x86_64"))]
mod jr21_dispatch_typecheck_row_tests {
    use super::*;

    const FELL: i64 = -1;
    const OBJECT: u32 = 1;
    const BASE: u32 = 0x10;
    const A1: u32 = 0x11;
    const A2: u32 = 0x12;
    const B1: u32 = 0x13;
    const OTHER: u32 = 0x14;
    const PROXYISH: u32 = 0x15;
    const DEEP: u32 = 0x16;
    const UNPUBLISHED: u32 = 0x17;
    const MANY: u32 = 0x18;
    const SHAPE: u32 = 0x40;
    const NAMED: u32 = 0x41;
    // Same home slot as SHAPE (0x40 % 6 == 4 == 0x46 % 6), another bloom bit.
    const CLASHING: u32 = 0x46;

    /// A 64-byte aligned zeroed table of `ROWS_CAP` rows.
    struct Table(Vec<u64>, usize);

    impl Table {
        fn new() -> Self {
            let words = ROWS_BYTES / 8 + ROW_BYTES / 8;
            let v = vec![0u64; words];
            let base = v.as_ptr() as usize; // Cast: the buffer's address
            let aligned = (base + ROW_BYTES - 1) & !(ROW_BYTES - 1);
            Table(v, aligned)
        }

        fn fill(&mut self, id: u32, chain: &[u32], ifaces: &[u32], hierarchy_only: bool) {
            let words = row_words(chain, ifaces, hierarchy_only);
            let off = self.1 - self.0.as_ptr() as usize + (id as usize) * ROW_BYTES; // Cast: addresses
                                                                                     // SAFETY: `off` is inside the buffer: `id < ROWS_CAP` rows past
                                                                                     // the aligned base, which the extra row of slack keeps in bounds.
            let p = unsafe { (self.0.as_mut_ptr() as *mut u8).add(off) as *mut u32 };
            for (i, w) in words.iter().enumerate() {
                // SAFETY: as above, `i < ROW_WORDS` words into the row.
                unsafe { p.add(i).write(*w) };
            }
        }
    }

    fn build(
        rows: usize,
        target: u32,
        kind: RowTarget,
        latch: Option<usize>,
    ) -> crate::ExecutableBuffer {
        let arm = typecheck_row_bytes(rows, target, kind, latch).expect("an arm");
        let mut body: Vec<u8> = Vec::new();
        // RAX = arg0 (the fake object's address).
        #[cfg(target_os = "windows")]
        body.extend_from_slice(&[0x48, 0x89, 0xC8]); // MOV RAX, RCX
        #[cfg(not(target_os = "windows"))]
        body.extend_from_slice(&[0x48, 0x89, 0xF8]); // MOV RAX, RDI
        let at = body.len();
        body.extend_from_slice(&arm.bytes);
        // The fall-through lands here.
        body.extend_from_slice(&[0x48, 0xC7, 0xC0, 0xFF, 0xFF, 0xFF, 0xFF, 0xC3]); // MOV RAX, -1 ; RET
        let hit_at = body.len();
        body.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00, 0xC3]); // MOV EAX, 1 ; RET
        let miss_at = body.len();
        body.extend_from_slice(&[0x31, 0xC0, 0xC3]); // XOR EAX, EAX ; RET
        let mut patch = |field: usize, to: usize| {
            let operand = at + field;
            let rel =
                i32::try_from(to).expect("small") - i32::try_from(operand + 4).expect("small");
            body[operand..operand + 4].copy_from_slice(&rel.to_le_bytes());
        };
        for h in &arm.hit {
            patch(*h, hit_at);
        }
        for m in &arm.miss {
            patch(*m, miss_at);
        }
        let mut buf = crate::ExecutableBuffer::new(1024).expect("executable buffer");
        buf.emit(&body);
        assert!(!buf.overflowed());
        buf.finalize();
        buf
    }

    fn ask(buf: &crate::ExecutableBuffer, id: u32) -> i64 {
        let obj = Box::new(u64::from(id));
        // SAFETY: a complete `extern "C" fn(*const u64) -> i64` that reads the
        // fake object, the table and the latch the test keeps alive and writes
        // RAX, RCX, R11 (caller-saved on both ABIs).
        let f = unsafe {
            std::mem::transmute::<*const u8, extern "C" fn(*const u64) -> i64>(buf.as_ptr())
        };
        f(&*obj)
    }

    fn table() -> Table {
        let mut t = Table::new();
        t.fill(BASE, &[OBJECT, BASE], &[], true);
        t.fill(A1, &[OBJECT, BASE, A1], &[SHAPE], true);
        t.fill(A2, &[OBJECT, BASE, A1, A2], &[SHAPE, NAMED], true);
        t.fill(B1, &[OBJECT, BASE, B1], &[SHAPE, CLASHING], true);
        t.fill(OTHER, &[OBJECT, OTHER], &[], true);
        t.fill(PROXYISH, &[OBJECT, PROXYISH], &[SHAPE], false);
        let deep: Vec<u32> = (0..12)
            .map(|i| if i == 0 { OBJECT } else { 0x100 + i })
            .chain([DEEP])
            .collect();
        t.fill(DEEP, &deep, &[], true);
        // CLASHING is never placed: SHAPE, listed first, holds its home slot.
        t.fill(MANY, &[OBJECT, MANY], &[SHAPE, CLASHING], true);
        t
    }

    #[test]
    fn a_class_row_arm_answers_hits_and_hierarchy_only_misses() {
        let t = table();
        let latch = Box::new(std::sync::atomic::AtomicU8::new(0));
        let latch_addr = latch.as_ptr() as usize; // Cast: the latch's address
        let base_arm = build(t.1, BASE, RowTarget::Class { depth: 1 }, Some(latch_addr));
        for id in [BASE, A1, A2, B1] {
            assert_eq!(ask(&base_arm, id), 1, "{id:#x} is a BASE");
        }
        for id in [OTHER, DEEP, MANY] {
            assert_eq!(ask(&base_arm, id), 0, "{id:#x} is not a BASE");
        }
        // Verdict 0, no row, past the table: fall through.
        for id in [PROXYISH, UNPUBLISHED, ROWS_CAP, 0x8000_0001, u32::MAX] {
            assert_eq!(ask(&base_arm, id), FELL, "{id:#x} falls through");
        }
        let a1_arm = build(t.1, A1, RowTarget::Class { depth: 2 }, Some(latch_addr));
        assert_eq!(ask(&a1_arm, A1), 1);
        assert_eq!(ask(&a1_arm, A2), 1);
        assert_eq!(ask(&a1_arm, B1), 0);
        assert_eq!(ask(&a1_arm, BASE), 0, "shallower than the target");
        // A target at depth 7 against a 13-deep chain reads its 7th ancestor.
        let d7 = build(t.1, 0x107, RowTarget::Class { depth: 7 }, Some(latch_addr));
        assert_eq!(ask(&d7, DEEP), 1);
        assert_eq!(ask(&d7, A2), 0);
        // Hits only: every miss falls through.
        let hit_only = build(t.1, BASE, RowTarget::Class { depth: 1 }, None);
        assert_eq!(ask(&hit_only, A2), 1);
        assert_eq!(ask(&hit_only, OTHER), FELL);
        // A raised latch: hits stand, misses fall through.
        latch.store(1, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(ask(&base_arm, A1), 1);
        assert_eq!(ask(&base_arm, OTHER), FELL);
        drop(latch);
    }

    #[test]
    fn an_interface_row_arm_answers_placed_hits_and_bloom_misses() {
        let t = table();
        let latch = Box::new(std::sync::atomic::AtomicU8::new(0));
        let latch_addr = latch.as_ptr() as usize; // Cast: the latch's address
        let shape = build(t.1, SHAPE, RowTarget::Interface, Some(latch_addr));
        for id in [A1, A2, B1, MANY] {
            assert_eq!(ask(&shape, id), 1, "{id:#x} implements SHAPE");
        }
        for id in [BASE, OTHER, DEEP] {
            assert_eq!(ask(&shape, id), 0, "{id:#x} does not implement SHAPE");
        }
        // Verdict 0 still answers its hit; its miss would fall through.
        assert_eq!(ask(&shape, PROXYISH), 1);
        let named = build(t.1, NAMED, RowTarget::Interface, Some(latch_addr));
        assert_eq!(ask(&named, A2), 1);
        assert_eq!(ask(&named, A1), 0);
        assert_eq!(ask(&named, PROXYISH), FELL, "verdict 0 never misses");
        // CLASHING lost its home slot to SHAPE in MANY's row: listed in the
        // bloom, so neither a hit nor a miss; B1 placed it (SHAPE first, then
        // CLASHING, same slot) -- also unplaced.
        let clashing = build(t.1, CLASHING, RowTarget::Interface, Some(latch_addr));
        assert_eq!(ask(&clashing, MANY), FELL);
        assert_eq!(ask(&clashing, B1), FELL);
        assert_eq!(ask(&clashing, A1), 0);
        for id in [UNPUBLISHED, 0x8000_0001] {
            assert_eq!(ask(&clashing, id), FELL);
        }
        drop(latch);
    }

    #[test]
    fn row_words_lay_out_chain_bloom_and_home_slots() {
        let w = row_words(&[OBJECT, BASE, A1], &[SHAPE, NAMED, CLASHING], true);
        assert_eq!(w[0] & STATUS_MASK, STATUS_HIERARCHY_ONLY);
        assert_eq!(w[0] >> STATUS_DEPTH_SHIFT, 2, "A1's own depth");
        assert_eq!(&w[1..=7], &[BASE, A1, 0, 0, 0, 0, 0]);
        let bloom = u64::from(w[BLOOM_WORD]) | (u64::from(w[BLOOM_WORD + 1]) << 32);
        assert_eq!(
            bloom,
            (1 << bloom_bit(SHAPE)) | (1 << bloom_bit(NAMED)) | (1 << bloom_bit(CLASHING))
        );
        assert_eq!(w[iface_slot_word(SHAPE)], SHAPE);
        assert_eq!(w[iface_slot_word(NAMED)], NAMED);
        assert_eq!(iface_slot_word(SHAPE), iface_slot_word(CLASHING));
        assert_eq!(row_words(&[OBJECT], &[], false)[0], STATUS_PUBLISHED);
        assert!(typecheck_row_bytes(0x1000, BASE, RowTarget::Class { depth: 0 }, None).is_none());
        assert!(typecheck_row_bytes(0x1000, BASE, RowTarget::Class { depth: 8 }, None).is_none());
        assert!(typecheck_row_bytes(0x1008, BASE, RowTarget::Class { depth: 1 }, None).is_none());
        assert!(typecheck_row_bytes(0x1000, 0, RowTarget::Interface, None).is_none());
    }
}

/// JIT round 21, lane tcperf: the status-first row arm
/// ([`typecheck_row_first_bytes`]), EXECUTED over a fake row table, as the
/// neighbouring module executes [`typecheck_row_bytes`].
#[cfg(all(test, target_arch = "x86_64"))]
mod jr21_tcperf_row_first_tests {
    use super::*;

    const FELL: i64 = -1;
    const OBJECT: u32 = 1;
    const BASE: u32 = 0x20;
    const A1: u32 = 0x21;
    const A2: u32 = 0x22;
    const B1: u32 = 0x23;
    const OTHER: u32 = 0x24;
    const VERDICT0: u32 = 0x25;
    const DEEP: u32 = 0x26;
    const UNPUBLISHED: u32 = 0x27;
    const MANY: u32 = 0x28;
    const SHAPE: u32 = 0x40;
    const NAMED: u32 = 0x41;
    // Same home slot as SHAPE (0x40 % 6 == 4 == 0x46 % 6), another bloom bit.
    const CLASHING: u32 = 0x46;

    /// A 64-byte aligned zeroed table of `ROWS_CAP` rows.
    struct Table(Vec<u64>, usize);

    impl Table {
        fn new() -> Self {
            let words = ROWS_BYTES / 8 + ROW_BYTES / 8;
            let v = vec![0u64; words];
            let base = v.as_ptr() as usize; // Cast: the buffer's address
            let aligned = (base + ROW_BYTES - 1) & !(ROW_BYTES - 1);
            Table(v, aligned)
        }

        fn write(&mut self, id: u32, words: &[u32; ROW_WORDS]) {
            let off = self.1 - self.0.as_ptr() as usize + (id as usize) * ROW_BYTES; // Cast: addresses
                                                                                     // SAFETY: `off` is inside the buffer: `id < ROWS_CAP` rows past the
                                                                                     // aligned base, which the extra row of slack keeps in bounds.
            let p = unsafe { (self.0.as_mut_ptr() as *mut u8).add(off) as *mut u32 };
            for (i, w) in words.iter().enumerate() {
                // SAFETY: as above, `i < ROW_WORDS` words into the row.
                unsafe { p.add(i).write(*w) };
            }
        }

        fn fill(&mut self, id: u32, chain: &[u32], ifaces: &[u32], hierarchy_only: bool) {
            let words = row_words(chain, ifaces, hierarchy_only);
            self.write(id, &words);
        }
    }

    fn build(
        rows: usize,
        target: u32,
        kind: RowTarget,
        latch: Option<usize>,
    ) -> crate::ExecutableBuffer {
        let arm = typecheck_row_first_bytes(rows, target, kind, latch).expect("an arm");
        let mut body: Vec<u8> = Vec::new();
        // RAX = arg0 (the fake object's address).
        #[cfg(target_os = "windows")]
        body.extend_from_slice(&[0x48, 0x89, 0xC8]); // MOV RAX, RCX
        #[cfg(not(target_os = "windows"))]
        body.extend_from_slice(&[0x48, 0x89, 0xF8]); // MOV RAX, RDI
        let at = body.len();
        body.extend_from_slice(&arm.bytes);
        // The fall-through lands here.
        body.extend_from_slice(&[0x48, 0xC7, 0xC0, 0xFF, 0xFF, 0xFF, 0xFF, 0xC3]); // MOV RAX, -1 ; RET
        let hit_at = body.len();
        body.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00, 0xC3]); // MOV EAX, 1 ; RET
        let miss_at = body.len();
        body.extend_from_slice(&[0x31, 0xC0, 0xC3]); // XOR EAX, EAX ; RET
        let mut patch = |field: usize, to: usize| {
            let operand = at + field;
            let rel =
                i32::try_from(to).expect("small") - i32::try_from(operand + 4).expect("small");
            body[operand..operand + 4].copy_from_slice(&rel.to_le_bytes());
        };
        for h in &arm.hit {
            patch(*h, hit_at);
        }
        for m in &arm.miss {
            patch(*m, miss_at);
        }
        let mut buf = crate::ExecutableBuffer::new(1024).expect("executable buffer");
        buf.emit(&body);
        assert!(!buf.overflowed());
        buf.finalize();
        buf
    }

    fn ask(buf: &crate::ExecutableBuffer, id: u32) -> i64 {
        let obj = Box::new(u64::from(id));
        // SAFETY: a complete `extern "C" fn(*const u64) -> i64` that reads the
        // fake object, the table and the latch the test keeps alive and writes
        // RAX, RCX, R11 (caller-saved on both ABIs).
        let f = unsafe {
            std::mem::transmute::<*const u8, extern "C" fn(*const u64) -> i64>(buf.as_ptr())
        };
        f(&*obj)
    }

    fn table() -> Table {
        let mut t = Table::new();
        t.fill(BASE, &[OBJECT, BASE], &[], true);
        t.fill(A1, &[OBJECT, BASE, A1], &[SHAPE], true);
        t.fill(A2, &[OBJECT, BASE, A1, A2], &[SHAPE, NAMED], true);
        t.fill(B1, &[OBJECT, BASE, B1], &[SHAPE, CLASHING], true);
        t.fill(OTHER, &[OBJECT, OTHER], &[], true);
        t.fill(VERDICT0, &[OBJECT, BASE, VERDICT0], &[SHAPE], false);
        let deep: Vec<u32> = (0..12)
            .map(|i| if i == 0 { OBJECT } else { 0x100 + i })
            .chain([DEEP])
            .collect();
        t.fill(DEEP, &deep, &[], true);
        t.fill(MANY, &[OBJECT, MANY], &[SHAPE, CLASHING], true);
        t
    }

    #[test]
    fn a_status_first_class_arm_answers_hits_and_misses_in_one_decision() {
        let t = table();
        let latch = Box::new(std::sync::atomic::AtomicU8::new(0));
        let latch_addr = latch.as_ptr() as usize; // Cast: the latch's address
        let base_arm = build(t.1, BASE, RowTarget::Class { depth: 1 }, Some(latch_addr));
        // The target's own class answers through its own chain word: the arm
        // needs no exact-id compare in front of it.
        for id in [BASE, A1, A2, B1] {
            assert_eq!(ask(&base_arm, id), 1, "a BASE");
        }
        for id in [OTHER, DEEP, MANY] {
            assert_eq!(ask(&base_arm, id), 0, "not a BASE");
        }
        // Verdict 0 falls through even for a HIT (the status is read first);
        // no row and ids past the table fall through.
        for id in [VERDICT0, UNPUBLISHED, ROWS_CAP, 0x8000_0001, u32::MAX] {
            assert_eq!(ask(&base_arm, id), FELL, "falls through");
        }
        let a1_arm = build(t.1, A1, RowTarget::Class { depth: 2 }, Some(latch_addr));
        assert_eq!(ask(&a1_arm, A1), 1);
        assert_eq!(ask(&a1_arm, A2), 1);
        assert_eq!(ask(&a1_arm, B1), 0);
        assert_eq!(ask(&a1_arm, BASE), 0, "shallower than the target");
        let d7 = build(t.1, 0x107, RowTarget::Class { depth: 7 }, Some(latch_addr));
        assert_eq!(ask(&d7, DEEP), 1);
        assert_eq!(ask(&d7, A2), 0);
        // Hits only (a cast): every miss falls through to the helper.
        let hit_only = build(t.1, BASE, RowTarget::Class { depth: 1 }, None);
        assert_eq!(ask(&hit_only, A2), 1);
        assert_eq!(ask(&hit_only, OTHER), FELL);
        assert_eq!(ask(&hit_only, VERDICT0), FELL);
        // A raised duplicate-name latch: hits stand, misses fall through.
        latch.store(1, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(ask(&base_arm, A1), 1);
        assert_eq!(ask(&base_arm, OTHER), FELL);
        drop(latch);
    }

    #[test]
    fn a_status_first_interface_arm_answers_placed_hits_and_bloom_misses() {
        let t = table();
        let latch = Box::new(std::sync::atomic::AtomicU8::new(0));
        let latch_addr = latch.as_ptr() as usize; // Cast: the latch's address
        let shape = build(t.1, SHAPE, RowTarget::Interface, Some(latch_addr));
        for id in [A1, A2, B1, MANY] {
            assert_eq!(ask(&shape, id), 1, "implements SHAPE");
        }
        for id in [BASE, OTHER, DEEP] {
            assert_eq!(ask(&shape, id), 0, "does not implement SHAPE");
        }
        assert_eq!(
            ask(&shape, VERDICT0),
            FELL,
            "verdict 0: the old arms answer it"
        );
        let clashing = build(t.1, CLASHING, RowTarget::Interface, Some(latch_addr));
        assert_eq!(
            ask(&clashing, MANY),
            FELL,
            "listed in the bloom, not placed"
        );
        assert_eq!(ask(&clashing, B1), FELL);
        assert_eq!(ask(&clashing, A1), 0);
        for id in [UNPUBLISHED, 0x8000_0001] {
            assert_eq!(ask(&clashing, id), FELL);
        }
        let named_hit_only = build(t.1, NAMED, RowTarget::Interface, None);
        assert_eq!(ask(&named_hit_only, A2), 1);
        assert_eq!(ask(&named_hit_only, A1), FELL);
        drop(latch);
    }

    /// The race the status-first order closes without a second read: a row
    /// whose deciding word is visible but whose status is not yet published
    /// (a filler between its Relaxed word stores and its Release status store)
    /// answers nothing.
    #[test]
    fn a_row_with_words_but_no_status_answers_nothing() {
        let mut t = table();
        let mut words = row_words(&[OBJECT, BASE, A1, UNPUBLISHED], &[SHAPE], true);
        words[0] = 0;
        t.write(UNPUBLISHED, &words);
        let latch = Box::new(std::sync::atomic::AtomicU8::new(0));
        let latch_addr = latch.as_ptr() as usize; // Cast: the latch's address
        let a1_arm = build(t.1, A1, RowTarget::Class { depth: 2 }, Some(latch_addr));
        assert_eq!(ask(&a1_arm, UNPUBLISHED), FELL);
        let other_arm = build(t.1, OTHER, RowTarget::Class { depth: 1 }, Some(latch_addr));
        assert_eq!(ask(&other_arm, UNPUBLISHED), FELL);
        let shape = build(t.1, SHAPE, RowTarget::Interface, Some(latch_addr));
        assert_eq!(ask(&shape, UNPUBLISHED), FELL);
        drop(latch);
    }

    #[test]
    fn the_status_first_builder_refuses_what_the_rows_cannot_answer() {
        let class1 = RowTarget::Class { depth: 1 };
        assert!(
            typecheck_row_first_bytes(0x1000, BASE, RowTarget::Class { depth: 0 }, None).is_none()
        );
        assert!(
            typecheck_row_first_bytes(0x1000, BASE, RowTarget::Class { depth: 8 }, None).is_none()
        );
        assert!(typecheck_row_first_bytes(0x1008, BASE, class1, None).is_none());
        assert!(typecheck_row_first_bytes(0x1000, 0, RowTarget::Interface, None).is_none());
        assert!(typecheck_row_first_bytes(0, BASE, class1, None).is_none());
        // No miss half without a latch; one decision (no re-read) with one.
        let arm = typecheck_row_first_bytes(0x1000, BASE, class1, None).expect("an arm");
        assert!(arm.miss.is_empty());
        assert_eq!(arm.hit.len(), 1);
        let arm = typecheck_row_first_bytes(0x1000, SHAPE, RowTarget::Interface, Some(0x2000))
            .expect("an arm");
        assert_eq!(arm.miss.len(), 1);
        assert_eq!(arm.hit.len(), 1);
    }
}

/// JIT round 21, lane tcarray (`CRATONVM_JIT_TYPECHECK_ARRAY_ARM`): the
/// array-target prefix ([`typecheck_array_prefix_bytes`]) followed by the
/// component's status-first row arm, EXECUTED over a fake row table on fake
/// headers (`class id | kind-tags byte << 48`), as the lowering emits them.
#[cfg(all(test, target_arch = "x86_64"))]
mod jr21_tcarray_array_arm_tests {
    use super::*;

    const HIT: i64 = 1;
    const SLOW: i64 = -2;
    const PLAIN: i64 = -3;
    const OBJECT: u32 = 1;
    const BASE: u32 = 0x30;
    const A1: u32 = 0x31;
    const A2: u32 = 0x32;
    const OTHER: u32 = 0x33;
    const VERDICT0: u32 = 0x34;
    const NESTED: u32 = 0x35;
    const SHAPE: u32 = 0x40;
    const INT_ARRAY_BYTE: u8 = 1 | (10 << 2);
    const HASHED_PLAIN_BYTE: u8 = 0b1111_1100;

    /// A 64-byte aligned zeroed table of `ROWS_CAP` rows.
    struct Table(Vec<u64>, usize);

    impl Table {
        fn new() -> Self {
            let words = ROWS_BYTES / 8 + ROW_BYTES / 8;
            let v = vec![0u64; words];
            let base = v.as_ptr() as usize; // Cast: the buffer's address
            let aligned = (base + ROW_BYTES - 1) & !(ROW_BYTES - 1);
            Table(v, aligned)
        }

        fn fill(&mut self, id: u32, chain: &[u32], ifaces: &[u32], hierarchy_only: bool) {
            let words = row_words(chain, ifaces, hierarchy_only);
            // Cast: addresses.
            let off = self.1 - self.0.as_ptr() as usize + (id as usize) * ROW_BYTES;
            // SAFETY: `off` is inside the buffer: `id < ROWS_CAP` rows past the
            // aligned base, which the extra row of slack keeps in bounds.
            let p = unsafe { (self.0.as_mut_ptr() as *mut u8).add(off) as *mut u32 };
            for (i, w) in words.iter().enumerate() {
                // SAFETY: as above, `i < ROW_WORDS` words into the row.
                unsafe { p.add(i).write(*w) };
            }
        }
    }

    fn table() -> Table {
        let mut t = Table::new();
        t.fill(BASE, &[OBJECT, BASE], &[], true);
        t.fill(A1, &[OBJECT, BASE, A1], &[SHAPE], true);
        t.fill(A2, &[OBJECT, BASE, A1, A2], &[SHAPE], true);
        t.fill(OTHER, &[OBJECT, OTHER], &[], true);
        t.fill(VERDICT0, &[OBJECT, BASE, VERDICT0], &[SHAPE], false);
        t
    }

    /// `extern "C" fn(*const u64) -> HIT | SLOW | PLAIN`: the prefix for
    /// `component`, then (with `element`) the component's hit-only row-first
    /// arm, then the jump to the helper.
    fn build(
        component: Option<u32>,
        element: Option<(usize, RowTarget)>,
    ) -> crate::ExecutableBuffer {
        let prefix = typecheck_array_prefix_bytes(component).expect("a prefix");
        let mut body: Vec<u8> = Vec::new();
        // RAX = arg0 (the fake header's address).
        #[cfg(target_os = "windows")]
        body.extend_from_slice(&[0x48, 0x89, 0xC8]); // MOV RAX, RCX
        #[cfg(not(target_os = "windows"))]
        body.extend_from_slice(&[0x48, 0x89, 0xF8]); // MOV RAX, RDI
        let at = body.len();
        body.extend_from_slice(&prefix.bytes);
        let mut hits: Vec<usize> = prefix.hit.iter().map(|e| at + e).collect();
        if let Some((rows, kind)) = element {
            let target = component.expect("a component");
            let arm = typecheck_row_first_bytes(rows, target, kind, None).expect("an arm");
            let arm_at = body.len();
            body.extend_from_slice(&arm.bytes);
            hits.extend(arm.hit.iter().map(|e| arm_at + e));
        }
        // What falls through reaches the helper; then the two other outcomes.
        body.extend_from_slice(&[0x48, 0xC7, 0xC0, 0xFE, 0xFF, 0xFF, 0xFF, 0xC3]); // MOV RAX, -2 ; RET
        let plain_at = body.len();
        body.extend_from_slice(&[0x48, 0xC7, 0xC0, 0xFD, 0xFF, 0xFF, 0xFF, 0xC3]); // MOV RAX, -3 ; RET
        let hit_at = body.len();
        body.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00, 0xC3]); // MOV EAX, 1 ; RET
        let mut patch = |operand: usize, to: usize| {
            let rel =
                i32::try_from(to).expect("small") - i32::try_from(operand + 4).expect("small");
            body[operand..operand + 4].copy_from_slice(&rel.to_le_bytes());
        };
        for h in hits {
            patch(h, hit_at);
        }
        for p in &prefix.not_reference_array {
            patch(at + p, plain_at);
        }
        let mut buf = crate::ExecutableBuffer::new(1024).expect("executable buffer");
        buf.emit(&body);
        assert!(!buf.overflowed());
        buf.finalize();
        buf
    }

    fn ask(buf: &crate::ExecutableBuffer, id: u32, kind_byte: u8) -> i64 {
        let header = Box::new(u64::from(id) | (u64::from(kind_byte) << 48));
        // SAFETY: a complete `extern "C" fn(*const u64) -> i64` that reads the
        // fake header and the table the test keeps alive and writes RAX, RCX,
        // R11 (caller-saved on both ABIs).
        let f = unsafe {
            std::mem::transmute::<*const u8, extern "C" fn(*const u64) -> i64>(buf.as_ptr())
        };
        f(&*header)
    }

    #[test]
    fn the_reference_array_byte_is_the_header_layouts() {
        assert_eq!(cratonvm_types::KIND_TAGS_BYTE_OFFSET, 6);
        assert_eq!(REFERENCE_ARRAY_KIND_BYTE, 1);
        assert_eq!(
            REFERENCE_ARRAY_KIND_BYTE & cratonvm_types::KIND_TAG_BYTE_MASK,
            cratonvm_types::ObjectKind::Array as u8
        );
        assert_eq!(
            cratonvm_types::primitive_array_kind_tags_byte("[I"),
            Some(INT_ARRAY_BYTE)
        );
        assert!(typecheck_array_prefix_bytes(Some(0)).is_none());
        let object = typecheck_array_prefix_bytes(None).expect("Object[]");
        assert_eq!((object.hit.len(), object.not_reference_array.len()), (1, 1));
    }

    #[test]
    fn an_object_array_target_admits_every_reference_array_and_nothing_else() {
        let buf = build(None, None);
        for id in [OBJECT, BASE, NESTED, 0x8000_0001, u32::MAX] {
            assert_eq!(ask(&buf, id, REFERENCE_ARRAY_KIND_BYTE), HIT, "{id:#x}[]");
        }
        // int[] (header id 0), plain objects (hashed or not), a humongous
        // filler: the site's other code answers them.
        assert_eq!(ask(&buf, 0, INT_ARRAY_BYTE), PLAIN);
        assert_eq!(ask(&buf, OBJECT, 0), PLAIN);
        assert_eq!(ask(&buf, A1, HASHED_PLAIN_BYTE), PLAIN);
        assert_eq!(ask(&buf, 0, 2), PLAIN);
    }

    #[test]
    fn a_class_component_answers_exact_and_subtype_hits_and_sends_the_rest_to_the_helper() {
        let t = table();
        let base = build(Some(BASE), Some((t.1, RowTarget::Class { depth: 1 })));
        for id in [BASE, A1, A2] {
            assert_eq!(
                ask(&base, id, REFERENCE_ARRAY_KIND_BYTE),
                HIT,
                "{id:#x}[] is a Base[]"
            );
        }
        // Not a Base[] (OTHER[], Object[]), a verdict-0 component, a synthetic
        // component, a component with no row (a nested array): the helper.
        for id in [OTHER, OBJECT, VERDICT0, 0x8000_0001, NESTED] {
            assert_eq!(ask(&base, id, REFERENCE_ARRAY_KIND_BYTE), SLOW, "{id:#x}[]");
        }
        // A plain BASE (its header id IS the component) is not a Base[].
        assert_eq!(ask(&base, BASE, 0), PLAIN);
        assert_eq!(ask(&base, A1, HASHED_PLAIN_BYTE), PLAIN);
        assert_eq!(ask(&base, 0, INT_ARRAY_BYTE), PLAIN);
        // The exact compare alone (a nested component, or no display).
        let exact = build(Some(A1), None);
        assert_eq!(ask(&exact, A1, REFERENCE_ARRAY_KIND_BYTE), HIT);
        assert_eq!(ask(&exact, A2, REFERENCE_ARRAY_KIND_BYTE), SLOW);
        assert_eq!(ask(&exact, A1, 0), PLAIN);
        drop(t);
    }

    #[test]
    fn an_interface_component_answers_placed_implementors() {
        let t = table();
        let shape = build(Some(SHAPE), Some((t.1, RowTarget::Interface)));
        for id in [A1, A2, SHAPE] {
            assert_eq!(
                ask(&shape, id, REFERENCE_ARRAY_KIND_BYTE),
                HIT,
                "{id:#x}[] is a Shape[]"
            );
        }
        for id in [BASE, OTHER, VERDICT0] {
            assert_eq!(
                ask(&shape, id, REFERENCE_ARRAY_KIND_BYTE),
                SLOW,
                "{id:#x}[]"
            );
        }
        assert_eq!(ask(&shape, A1, 0), PLAIN);
        drop(t);
    }
}

/// JIT round 21, lane tcmiss: the array-target MISS half
/// ([`typecheck_array_not_reference_miss_bytes`], the component's row-first arm
/// with its miss) and the `aastore` subtype arm ([`aastore_subtype_bytes`]),
/// EXECUTED over a fake row table on fake headers (`class id | kind-tags byte
/// << 48`), as the lowering emits them.
#[cfg(all(test, target_arch = "x86_64"))]
mod jr21_tcmiss_array_miss_and_aastore_tests {
    use super::*;

    const HIT: i64 = 1;
    const MISS: i64 = 0;
    const SLOW: i64 = -2;
    const OBJECT: u32 = 1;
    const BASE: u32 = 0x50;
    const A1: u32 = 0x51;
    const A2: u32 = 0x52;
    const OTHER: u32 = 0x53;
    const VERDICT0: u32 = 0x54;
    const SUB_OF_VERDICT0: u32 = 0x55;
    const UNPUBLISHED: u32 = 0x56;
    const DEEP: u32 = 0x57;
    const SHAPE: u32 = 0x60;
    const INT_ARRAY_BYTE: u8 = 1 | (10 << 2);
    const HASHED_PLAIN_BYTE: u8 = 0b1111_1100;

    /// A 64-byte aligned zeroed table of `ROWS_CAP` rows.
    struct Table(Vec<u64>, usize);

    impl Table {
        fn new() -> Self {
            let words = ROWS_BYTES / 8 + ROW_BYTES / 8;
            let v = vec![0u64; words];
            let base = v.as_ptr() as usize; // Cast: the buffer's address
            let aligned = (base + ROW_BYTES - 1) & !(ROW_BYTES - 1);
            Table(v, aligned)
        }

        fn fill(&mut self, id: u32, chain: &[u32], ifaces: &[u32], hierarchy_only: bool) {
            let words = row_words(chain, ifaces, hierarchy_only);
            // Cast: addresses.
            let off = self.1 - self.0.as_ptr() as usize + (id as usize) * ROW_BYTES;
            // SAFETY: `off` is inside the buffer: `id < ROWS_CAP` rows past the
            // aligned base, which the extra row of slack keeps in bounds.
            let p = unsafe { (self.0.as_mut_ptr() as *mut u8).add(off) as *mut u32 };
            for (i, w) in words.iter().enumerate() {
                // SAFETY: as above, `i < ROW_WORDS` words into the row.
                unsafe { p.add(i).write(*w) };
            }
        }
    }

    fn deep_chain() -> Vec<u32> {
        (0..12)
            .map(|i| if i == 0 { OBJECT } else { 0x100 + i })
            .chain([DEEP])
            .collect()
    }

    fn table() -> Table {
        let mut t = Table::new();
        t.fill(BASE, &[OBJECT, BASE], &[], true);
        t.fill(A1, &[OBJECT, BASE, A1], &[SHAPE], true);
        t.fill(A2, &[OBJECT, BASE, A1, A2], &[SHAPE], true);
        t.fill(OTHER, &[OBJECT, OTHER], &[], true);
        t.fill(VERDICT0, &[OBJECT, BASE, VERDICT0], &[SHAPE], false);
        t.fill(
            SUB_OF_VERDICT0,
            &[OBJECT, BASE, VERDICT0, SUB_OF_VERDICT0],
            &[SHAPE],
            false,
        );
        let deep = deep_chain();
        t.fill(DEEP, &deep, &[], true);
        // The depth-7 ancestor of DEEP, with its own row.
        t.fill(0x107, &deep[..8], &[], true);
        t
    }

    fn finish(body: Vec<u8>) -> crate::ExecutableBuffer {
        let mut buf = crate::ExecutableBuffer::new(1024).expect("executable buffer");
        buf.emit(&body);
        assert!(!buf.overflowed());
        buf.finalize();
        buf
    }

    fn patch(body: &mut [u8], operand: usize, to: usize) {
        let rel = i32::try_from(to).expect("small") - i32::try_from(operand + 4).expect("small");
        body[operand..operand + 4].copy_from_slice(&rel.to_le_bytes());
    }

    /// `instanceof T[]`: the prefix, the component's status-first row arm WITH
    /// its miss half (when `element`), the jump to the helper, then the
    /// not-reference-array miss block where the prefix's other edges land.
    fn build_instanceof(
        component: u32,
        element: Option<(usize, RowTarget)>,
        latch: usize,
    ) -> crate::ExecutableBuffer {
        let prefix = typecheck_array_prefix_bytes(Some(component)).expect("a prefix");
        let mut body: Vec<u8> = Vec::new();
        // RAX = arg0 (the fake header's address).
        #[cfg(target_os = "windows")]
        body.extend_from_slice(&[0x48, 0x89, 0xC8]); // MOV RAX, RCX
        #[cfg(not(target_os = "windows"))]
        body.extend_from_slice(&[0x48, 0x89, 0xF8]); // MOV RAX, RDI
        let at = body.len();
        body.extend_from_slice(&prefix.bytes);
        let mut hits: Vec<usize> = prefix.hit.iter().map(|e| at + e).collect();
        let mut zeros: Vec<usize> = Vec::new();
        let mut slows: Vec<usize> = Vec::new();
        if let Some((rows, kind)) = element {
            let arm =
                typecheck_row_first_bytes(rows, component, kind, Some(latch)).expect("an arm");
            let arm_at = body.len();
            body.extend_from_slice(&arm.bytes);
            hits.extend(arm.hit.iter().map(|e| arm_at + e));
            zeros.extend(arm.miss.iter().map(|e| arm_at + e));
        }
        body.push(0xE9); // JMP slow (what the arms did not answer)
        slows.push(body.len());
        body.extend_from_slice(&[0, 0, 0, 0]);
        // The prefix's not-reference-array edges land on the miss block.
        let miss_block_at = body.len();
        for p in &prefix.not_reference_array {
            patch(&mut body, at + p, miss_block_at);
        }
        let nr = typecheck_array_not_reference_miss_bytes(latch);
        body.extend_from_slice(&nr.bytes);
        slows.extend(nr.slow.iter().map(|e| miss_block_at + e));
        zeros.extend(nr.zero.iter().map(|e| miss_block_at + e));
        let slow_at = body.len();
        body.extend_from_slice(&[0x48, 0xC7, 0xC0, 0xFE, 0xFF, 0xFF, 0xFF, 0xC3]); // MOV RAX, -2 ; RET
        let zero_at = body.len();
        body.extend_from_slice(&[0x31, 0xC0, 0xC3]); // XOR EAX, EAX ; RET
        let hit_at = body.len();
        body.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00, 0xC3]); // MOV EAX, 1 ; RET
        for h in hits {
            patch(&mut body, h, hit_at);
        }
        for z in zeros {
            patch(&mut body, z, zero_at);
        }
        for s in slows {
            patch(&mut body, s, slow_at);
        }
        finish(body)
    }

    fn ask(buf: &crate::ExecutableBuffer, id: u32, kind_byte: u8) -> i64 {
        let header = Box::new(u64::from(id) | (u64::from(kind_byte) << 48));
        // SAFETY: a complete `extern "C" fn(*const u64) -> i64` that reads the
        // fake header, the table and the latch the test keeps alive and writes
        // RAX, RCX, R11 (caller-saved on both ABIs).
        let f = unsafe {
            std::mem::transmute::<*const u8, extern "C" fn(*const u64) -> i64>(buf.as_ptr())
        };
        f(&*header)
    }

    #[test]
    fn a_class_array_target_answers_every_miss_the_hierarchy_proves() {
        let t = table();
        let latch = Box::new(std::sync::atomic::AtomicU8::new(0));
        let latch_addr = latch.as_ptr() as usize; // Cast: the latch's address
        let r = REFERENCE_ARRAY_KIND_BYTE;
        let base = build_instanceof(BASE, Some((t.1, RowTarget::Class { depth: 1 })), latch_addr);
        for id in [BASE, A1, A2] {
            assert_eq!(ask(&base, id, r), HIT, "{id:#x}[] is a Base[]");
        }
        // A hierarchy-only component that is not a Base: answered `false`.
        assert_eq!(ask(&base, OTHER, r), MISS, "OTHER[] is not a Base[]");
        assert_eq!(ask(&base, DEEP, r), MISS, "DEEP[] is not a Base[]");
        // Verdict 0, unpublished, synthetic, past the table: the helper.
        for id in [VERDICT0, UNPUBLISHED, 0x8000_0001, ROWS_CAP] {
            assert_eq!(ask(&base, id, r), SLOW, "{id:#x}[]");
        }
        // Not a reference array: a plain object (hashed or not) and a
        // primitive array are never a Base[].
        assert_eq!(ask(&base, BASE, 0), MISS, "a plain Base is not a Base[]");
        assert_eq!(ask(&base, A1, HASHED_PLAIN_BYTE), MISS);
        assert_eq!(ask(&base, 0, INT_ARRAY_BYTE), MISS, "int[] is not a Base[]");
        // A synthetic plain receiver and a humongous filler: the helper.
        assert_eq!(ask(&base, 0x8000_0001, 0), SLOW);
        assert_eq!(ask(&base, 0, 2), SLOW);
        // A raised duplicate-class-name latch: hits stand, every plain or
        // component miss asks the helper; a primitive array is still `false`.
        latch.store(1, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(ask(&base, A1, r), HIT);
        assert_eq!(ask(&base, OTHER, r), SLOW);
        assert_eq!(ask(&base, BASE, 0), SLOW);
        assert_eq!(ask(&base, 0, INT_ARRAY_BYTE), MISS);
        drop(latch);
        drop(t);
    }

    #[test]
    fn an_interface_array_target_answers_bloom_proved_misses() {
        let t = table();
        let latch = Box::new(std::sync::atomic::AtomicU8::new(0));
        let latch_addr = latch.as_ptr() as usize; // Cast: the latch's address
        let r = REFERENCE_ARRAY_KIND_BYTE;
        let shape = build_instanceof(SHAPE, Some((t.1, RowTarget::Interface)), latch_addr);
        for id in [A1, A2, SHAPE] {
            assert_eq!(ask(&shape, id, r), HIT, "{id:#x}[] is a Shape[]");
        }
        for id in [BASE, OTHER, DEEP] {
            assert_eq!(ask(&shape, id, r), MISS, "{id:#x}[] is not a Shape[]");
        }
        assert_eq!(ask(&shape, VERDICT0, r), SLOW);
        assert_eq!(ask(&shape, A1, 0), MISS);
        assert_eq!(ask(&shape, 0, INT_ARRAY_BYTE), MISS);
        // No element arm (a nested component, or no display): the plain and
        // primitive misses stand, every reference array but the exact one
        // asks the helper.
        let exact = build_instanceof(A1, None, latch_addr);
        assert_eq!(ask(&exact, A1, r), HIT);
        assert_eq!(ask(&exact, A2, r), SLOW);
        assert_eq!(ask(&exact, A1, 0), MISS);
        drop(latch);
        drop(t);
    }

    #[test]
    fn the_not_reference_array_block_refuses_odd_array_bytes() {
        let latch = Box::new(std::sync::atomic::AtomicU8::new(0));
        let latch_addr = latch.as_ptr() as usize; // Cast: the latch's address
        let nr = typecheck_array_not_reference_miss_bytes(latch_addr);
        assert_eq!((nr.slow.len(), nr.zero.len()), (4, 2));
        let exact = build_instanceof(A1, None, latch_addr);
        // An array byte with the Reference element but a reserved bit set is
        // not one the prefix admitted; it is not a primitive either: the helper.
        assert_eq!(ask(&exact, A1, REFERENCE_ARRAY_KIND_BYTE | 0x40), SLOW);
        // Every primitive element type is a miss.
        for elem in 1u8..=10 {
            assert_eq!(ask(&exact, 0, 1 | (elem << 2)), MISS, "element {elem}");
        }
        drop(latch);
    }

    /// The reference-array result of [`build_guard`]: the guard fell through.
    const CONTINUE: i64 = 7;

    /// j27 w2 TCARRAY: [`typecheck_array_miss_guard_bytes`] alone, as dev's
    /// array arm emits it ahead of its reference-array hits: a fall-through
    /// answers [`CONTINUE`].
    fn build_guard(latch: usize) -> crate::ExecutableBuffer {
        let guard = typecheck_array_miss_guard_bytes(latch);
        let mut body: Vec<u8> = Vec::new();
        #[cfg(target_os = "windows")]
        body.extend_from_slice(&[0x48, 0x89, 0xC8]); // MOV RAX, RCX
        #[cfg(not(target_os = "windows"))]
        body.extend_from_slice(&[0x48, 0x89, 0xF8]); // MOV RAX, RDI
        let at = body.len();
        body.extend_from_slice(&guard.bytes);
        body.extend_from_slice(&[0xB8, 0x07, 0x00, 0x00, 0x00, 0xC3]); // MOV EAX, CONTINUE ; RET
        let slow_at = body.len();
        body.extend_from_slice(&[0x48, 0xC7, 0xC0, 0xFE, 0xFF, 0xFF, 0xFF, 0xC3]); // MOV RAX, -2 ; RET
        let zero_at = body.len();
        body.extend_from_slice(&[0x31, 0xC0, 0xC3]); // XOR EAX, EAX ; RET
        for s in &guard.slow {
            patch(&mut body, at + s, slow_at);
        }
        for z in &guard.zero {
            patch(&mut body, at + z, zero_at);
        }
        finish(body)
    }

    #[test]
    fn the_array_miss_guard_passes_reference_arrays_and_answers_the_rest() {
        let latch = Box::new(std::sync::atomic::AtomicU8::new(0));
        let latch_addr = latch.as_ptr() as usize; // Cast: the latch's address
        let guard = build_guard(latch_addr);
        let r = REFERENCE_ARRAY_KIND_BYTE;
        // Every reference array goes on to the hit arms, whatever its component.
        for id in [0, BASE, A1, 0x8000_0001] {
            assert_eq!(ask(&guard, id, r), CONTINUE, "{id:#x}[]");
        }
        // A plain object (hashed or not) and a primitive array are never a `T[]`.
        assert_eq!(ask(&guard, BASE, 0), MISS);
        assert_eq!(ask(&guard, A1, HASHED_PLAIN_BYTE), MISS);
        assert_eq!(ask(&guard, 0, INT_ARRAY_BYTE), MISS);
        // The screens every inline miss keeps: synthetic ids, odd kind bytes.
        assert_eq!(ask(&guard, 0x8000_0001, 0), SLOW);
        assert_eq!(ask(&guard, 0, 2), SLOW);
        assert_eq!(ask(&guard, A1, r | 0x40), SLOW);
        // A raised duplicate-class-name latch: a plain miss asks the helper.
        latch.store(1, std::sync::atomic::Ordering::Relaxed);
        assert_eq!(ask(&guard, BASE, 0), SLOW);
        assert_eq!(ask(&guard, 0, INT_ARRAY_BYTE), MISS);
        assert_eq!(ask(&guard, A1, r), CONTINUE);
        drop(latch);
    }

    /// `extern "C" fn(array: *const u64, value: *const u64) -> HIT | SLOW`:
    /// R11 = the array, RAX = the value, EDX = its class id, then the arm.
    fn build_aastore(rows: usize) -> crate::ExecutableBuffer {
        let (arm, hit) = aastore_subtype_bytes(rows).expect("an arm");
        let mut body: Vec<u8> = Vec::new();
        #[cfg(target_os = "windows")]
        {
            body.extend_from_slice(&[0x49, 0x89, 0xCB]); // MOV R11, RCX
            body.extend_from_slice(&[0x48, 0x89, 0xD0]); // MOV RAX, RDX
        }
        #[cfg(not(target_os = "windows"))]
        {
            body.extend_from_slice(&[0x49, 0x89, 0xFB]); // MOV R11, RDI
            body.extend_from_slice(&[0x48, 0x89, 0xF0]); // MOV RAX, RSI
        }
        body.extend_from_slice(&[0x8B, 0x10]); // MOV EDX, [RAX]
        let at = body.len();
        body.extend_from_slice(&arm);
        body.extend_from_slice(&[0x48, 0xC7, 0xC0, 0xFE, 0xFF, 0xFF, 0xFF, 0xC3]); // MOV RAX, -2 ; RET
        let hit_at = body.len();
        body.extend_from_slice(&[0xB8, 0x01, 0x00, 0x00, 0x00, 0xC3]); // MOV EAX, 1 ; RET
        for h in hit {
            patch(&mut body, at + h, hit_at);
        }
        finish(body)
    }

    fn store(buf: &crate::ExecutableBuffer, component: u32, value: u32) -> i64 {
        let array = Box::new(u64::from(component) | (u64::from(REFERENCE_ARRAY_KIND_BYTE) << 48));
        let value = Box::new(u64::from(value));
        // SAFETY: a complete `extern "C" fn(*const u64, *const u64) -> i64`
        // that reads the two fake headers and the table the test keeps alive
        // and writes RAX, RCX, RDX, R11 (caller-saved on both ABIs).
        let f = unsafe {
            std::mem::transmute::<*const u8, extern "C" fn(*const u64, *const u64) -> i64>(
                buf.as_ptr(),
            )
        };
        f(&*array, &*value)
    }

    #[test]
    fn an_aastore_of_a_subclass_value_passes_inline() {
        let t = table();
        let buf = build_aastore(t.1);
        for (component, value) in [
            (BASE, A1),
            (BASE, A2),
            (A1, A2),
            (BASE, VERDICT0),
            // A verdict-0 component still has its depth: a subclass is a
            // subclass whatever the instance rules.
            (VERDICT0, SUB_OF_VERDICT0),
            // The deepest depth a row stores.
            (0x107, DEEP),
        ] {
            assert_eq!(
                store(&buf, component, value),
                HIT,
                "{value:#x} into {component:#x}[]"
            );
        }
        for (component, value) in [
            // Not a subclass: the helper raises ArrayStoreException.
            (BASE, OTHER),
            (A1, BASE),
            (A2, A1),
            // An interface component has no row (no display): the helper.
            (SHAPE, A1),
            // A component deeper than the rows store, or with no row yet.
            (DEEP, DEEP + 1),
            (UNPUBLISHED, A1),
            // Synthetic ids on either side, an id past the table.
            (BASE, 0x8000_0001),
            (0x8000_0001, A1),
            (ROWS_CAP, A1),
            (BASE, ROWS_CAP),
            // A value with no row (its class's row is all zeros).
            (BASE, UNPUBLISHED),
        ] {
            assert_eq!(
                store(&buf, component, value),
                SLOW,
                "{value:#x} into {component:#x}[]"
            );
        }
        assert!(aastore_subtype_bytes(0).is_none());
        assert!(aastore_subtype_bytes(0x1008).is_none());
        drop(t);
    }

    #[test]
    fn a_row_carries_its_own_depth_in_byte_one_only_when_the_rows_store_it() {
        assert_eq!(row_words(&[OBJECT], &[], true)[0], STATUS_HIERARCHY_ONLY);
        assert_eq!(
            row_words(&[OBJECT, BASE], &[], true)[0],
            STATUS_HIERARCHY_ONLY | (1 << STATUS_DEPTH_SHIFT)
        );
        assert_eq!(
            row_words(&deep_chain()[..8], &[], false)[0],
            STATUS_PUBLISHED | (7 << STATUS_DEPTH_SHIFT)
        );
        assert_eq!(
            row_words(&deep_chain(), &[], true)[0],
            STATUS_HIERARCHY_ONLY
        );
        assert_eq!(STATUS_DEPTH_SHIFT % 8, 0, "a byte of its own");
    }
}
