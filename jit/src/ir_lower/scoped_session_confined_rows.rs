#[allow(unused_imports)]
use super::*;

/// The confined-session classes `lower_scoped_raw` admits inline (nio20 wave
/// 5, lane L2; page
/// `r20-scoped-fold-confined-session-needs-the-current-thread-object-CLOSED-20261012.md`),
/// with the thread-word offset the owner test reads; empty when any piece is
/// missing (the wave-4 behaviour: such a session deopts).
///
/// # Why `owner == current thread && state open` is the whole check
///
/// For both rows, `checkValidStateRaw()` is `if (owner != null && owner !=
/// Thread.currentThread()) throw WRONG_THREAD; if (state not open) throw
/// ALREADY_CLOSED;` -- the JDK's body for `ConfinedSession`, and the model's
/// `p67_session_check_valid`, whose owner test is `current_thread_object() ==
/// owner` (`p67_owner_is_current_thread`), i.e. the `JvmThread::java_thread_obj`
/// word this compares. Requiring a NON-null owner equal to the current thread
/// is a subset of passing, and it also settles the close race the shared-arena
/// audit is about: a confined session closes only through `justClose()`, which
/// runs that same owner test first (the JDK's bytecode; the model's
/// `p67_session_just_close`), so only THIS thread could close it -- and this
/// thread is between the check and the access, with no safepoint, call or
/// allocation in between. A session with a null owner (the model's shared /
/// global / heap / auto sessions) fails the compare and deopts as before --
/// except, since nio21 wave 2 (lane L3, W1L3-2), a model session of the
/// never-closing kind ([`SCOPED_MODEL_STATE_NONCLOSEABLE`], under
/// [`ir_scoped_noncloseable_model_fold_enabled`]), which passes with no
/// thread test (`emit_scoped_confined_session_check`).
///
/// Refused at compile time: no TLS mirror, an armed ZGC read barrier for rows
/// that compare the owner with this thread (shared rows only test null), a
/// narrow-oop owner slot, a `state` that is not a
/// 4-byte int, a class or slot the VM cannot name.
///
/// JIT round 22 (lane memory, MEM20-2 residual): a third, `shared_only` row for
/// the JDK's `SharedSession` when the shared window is available
/// ([`scoped_access_word_tls_disp`]). It never passes the confined test (its
/// owner is always null); its open sessions go to the window.
pub(crate) fn scoped_session_confined_rows(
    helpers: &crate::DirectHelperTable,
) -> (Vec<ScopedConfinedRow>, i32) {
    let none = (Vec::new(), 0);
    let raw = helpers.well_known_class_id;
    let ctx = helpers.static_base_resolver_ctx;
    if raw == 0 || ctx == 0 || !ir_scoped_confined_fold_enabled() {
        return none;
    }
    if crate::x64::jit_thread_tls_disp() == 0 {
        return none;
    }
    let Some(thread_off) = ir_thread_java_obj_offset(helpers) else {
        return none;
    };
    // SAFETY: as in `ir_integer_cache_high`.
    let f: crate::direct_helpers::DirectHelperFnWellKnownClassId =
        unsafe { std::mem::transmute(raw) };
    // Cast: `usize` context to the C ABI's i64 parameter.
    //
    // SAFETY: as in `ir_integer_cache_high`.
    let ask = |which: i64| unsafe { f(ctx as i64, which) };
    let mut rows: Vec<ScopedConfinedRow> = Vec::with_capacity(3);
    // JIT round 22 (lane memory, MEM20-2 residual): the JDK's `SharedSession`
    // joins only when the shared window can take it; without the window it
    // would only add a class compare in front of the same deopt.
    let shared_jdk = scoped_access_word_tls_disp(helpers).is_some();
    for (class_sel, state_sel, owner_sel, open, shared_only) in [
        (
            WELL_KNOWN_SESSION_MODEL,
            WELL_KNOWN_SESSION_MODEL_STATE_SLOT,
            WELL_KNOWN_SESSION_MODEL_OWNER_SLOT,
            ScopedSessionOpen::EqualsOne,
            false,
        ),
        (
            WELL_KNOWN_SESSION_CONFINED,
            WELL_KNOWN_SESSION_CONFINED_STATE_SLOT,
            WELL_KNOWN_SESSION_CONFINED_OWNER_SLOT,
            ScopedSessionOpen::NonNegative,
            false,
        ),
        (
            WELL_KNOWN_SESSION_SHARED,
            WELL_KNOWN_SESSION_SHARED_STATE_SLOT,
            WELL_KNOWN_SESSION_SHARED_OWNER_SLOT,
            ScopedSessionOpen::NonNegative,
            true,
        ),
    ] {
        if (shared_only && !shared_jdk)
            || (!shared_only && crate::x64::zgc_read_barrier_blocks_inline_fields())
        {
            continue;
        }
        let Some(cid) = u32::try_from(ask(class_sel)).ok().filter(|&c| c != 0) else {
            continue;
        };
        let slot = |sel: i64| {
            usize::try_from(ask(sel))
                .ok()
                .and_then(|s| s.checked_sub(1))
        };
        let (Some(state_idx), Some(owner_idx)) = (slot(state_sel), slot(owner_sel)) else {
            continue;
        };
        let Some(state) =
            crate::AtomicIntFieldLayout::new(state_idx, cid).filter(|l| l.class_id == cid)
        else {
            continue;
        };
        let Some((owner_compact, owner_legacy_tag, owner_legacy)) =
            scoped_ref_field_offsets(owner_idx, cid)
        else {
            continue;
        };
        if rows.iter().any(|r| r.class_id == cid) {
            continue;
        }
        rows.push(ScopedConfinedRow {
            class_id: cid,
            state_compact: state.value_compact_offset,
            state_legacy: state.value_legacy_offset,
            owner_compact,
            owner_legacy_tag,
            owner_legacy,
            open,
            shared_only,
        });
    }
    if rows.is_empty() {
        return none;
    }
    (rows, thread_off)
}

/// `CRATONVM_JIT_IR_SCOPED_SHARED_WINDOW_FOLD` (JIT r20 wave 19, lane
/// yFfmShared, MEM20-2 of `docs/feature-designs/jit-ir-optimizations.md`;
/// **default ON** since JIT round 22, `=0` keeps a shared session a deopt):
/// the scoped fold lowers a SHARED session (`Arena.ofShared()`: owner null,
/// state open) inline under this thread's FFM access window
/// (`Lowerer::emit_scoped_window_open`). Needs the confined arm
/// ([`ir_scoped_confined_fold_enabled`], whose rows it reads). Read per site
/// compile.
///
/// History: opt-in after the w17s A/B (bbshared-get 2127 vs 1688 ns): the
/// window then admitted only the session MODEL, and since the `JdkForeign`
/// alignment row every `Arena.ofShared()` session is the JDK's `SharedSession`
/// in both modes, so it never engaged and only cost code. JIT round 22 (lane
/// memory) added the `SharedSession` row: one shared-view row (16 `getInt`s)
/// went from 346 G to 12 G instructions per run (`R22WinRows shared`), its
/// time from 3.6-6.4 us to 0.75-1.3 us; the confined rows kept both of their
/// run-to-run modes in both arms.
pub(crate) fn ir_scoped_shared_window_fold_enabled() -> bool {
    ir_scoped_confined_fold_enabled()
}

/// NOT a class id (JIT r20 wave 19, lane yFfmShared): the segment-relative TLS
/// displacement PLUS ONE (`0` = unavailable) of `native-builtins`'
/// access-word mirror (`ffm_fast::access_word_tls_disp`): the slot holding the
/// address of the running thread's leased `AccessWord::seq`, or `0` before the
/// thread leased one. Thread-invariant. `0` also when the close handshake is
/// off (`CRATONVM_FFM_ACCESS_HANDSHAKE=0`: no closer would probe the word).
pub const WELL_KNOWN_FFM_ACCESS_WORD_TLS_DISP: i64 = 0x5E55_0050;

/// The shared arm of the scoped fold for one site
/// (`Lowerer::scoped_shared_window_for`): the access-word mirror's TLS
/// displacement, the session node, and the model row's compact / legacy
/// `state` addresses (the in-window re-read).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ScopedSharedWindow {
    pub disp: u32,
    pub session: NodeId,
    /// The row whose owner-less open sessions take the window (JIT round 22:
    /// the JDK's `SharedSession` when it is loaded, else the model), and that
    /// row's OPEN encoding, which the in-window re-read tests.
    pub class_id: u32,
    pub open: ScopedSessionOpen,
    pub state_compact: i32,
    pub state_legacy: i32,
}

/// [`WELL_KNOWN_FFM_ACCESS_WORD_TLS_DISP`] for this compile's VM, or `None`
/// (switch off, no VM, no mirror). The emitted load uses the same segment
/// prefix as the JIT's own TLS mirrors (`gs` on Windows, `fs` on Linux), the
/// prefix `native-builtins` probed its slot through.
pub(crate) fn scoped_access_word_tls_disp(helpers: &crate::DirectHelperTable) -> Option<u32> {
    if !ir_scoped_shared_window_fold_enabled() {
        return None;
    }
    let raw = helpers.well_known_class_id;
    let ctx = helpers.static_base_resolver_ctx;
    if raw == 0 || ctx == 0 {
        return None;
    }
    // SAFETY: as in `ir_integer_cache_high`.
    let f: crate::direct_helpers::DirectHelperFnWellKnownClassId =
        unsafe { std::mem::transmute(raw) };
    // Cast: `usize` context to the C ABI's i64 parameter.
    //
    // SAFETY: as in `ir_integer_cache_high`.
    let plus_one = unsafe { f(ctx as i64, WELL_KNOWN_FFM_ACCESS_WORD_TLS_DISP) };
    u32::try_from(plus_one.checked_sub(1)?)
        .ok()
        .filter(|&d| d != 0)
}
/// `CRATONVM_JIT_IR_SCOPED_SESSION_FOLD` (nio20 wave 4, lane L2, **default
/// ON**, `=0` restores the wave-3 deopt on every non-null session): phase A of
/// `docs/feature-designs/deopt-osr.md`.
/// Read per site compile, so it adds no process global.
pub(crate) fn ir_scoped_session_fold_enabled() -> bool {
    true
}

/// The class ids `lower_scoped_raw` admits as a NON-null session without a
/// deopt (nio20 wave 4, lane L2, phase A of the r20 scoped-fold page), or an
/// empty list (the wave-3 behaviour: every non-null session deopts).
///
/// # Why these three, and why a class-id compare is the whole check
///
/// `ScopedMemoryAccess.get*/put*` run `session.checkValidStateRaw()` -- `if
/// (owner != null && owner != Thread.currentThread()) throw WRONG_THREAD; if
/// (state < OPEN) throw ALREADY_CLOSED;` with `OPEN == 0` -- then the access.
/// For an object whose EXACT class is one of these, JDK 25's bytecode makes
/// that check a constant pass (verified with `javap` against
/// 25.0.3+9's `java.base`):
///
/// * `GlobalSession()` passes `owner = null` to the `final` field and sets
///   `state = NONCLOSEABLE` (1); its `acquire0`/`release0` are empty and
///   `justClose()` throws `nonCloseable()`, so `state` is never written again.
///   `GlobalSession$HeapSession` only adds a `final ref` (it inherits all of
///   that).
/// * `ImplicitSession(Cleaner)` reaches `SharedSession()` (`owner = null`),
///   sets `state = 1`, and overrides `acquire0`/`release0` (no state change)
///   and `justClose()` (throws). Its memory is freed only by the `Cleaner`
///   once the session is phantom-reachable, which it is not while this frame
///   holds it as a node input -- and the inline sequence from this check to
///   the access contains no safepoint.
///
/// So no close can precede or interleave with the access (the property the
/// shared-arena close argument,
/// `docs/feature-designs/jit-ir-optimizations.md`,
/// rests on): these sessions never close. `SharedSession` and
/// `ConfinedSession` are NOT admitted (a shared one can close from any thread
/// and needs the `AccessWindow`; a confined one needs an inline owner test
/// against the current `Thread`, which this tier has no load for --
/// `r20-scoped-fold-confined-session-needs-the-current-thread-object-CLOSED-20261012.md`),
/// and neither is CratonVM's own session model (an instance of the abstract
/// `MemorySessionImpl` itself, `p67_memory_session`): an exact compare
/// against the three concrete ids excludes every other class, subclasses
/// included.
/// JIT r23 wave 9 (lane L4): the class id of `GlobalSession$HeapSession` (the
/// scope of `MemorySegment.ofArray`) in this compile's VM, or `0` (no resolver,
/// the scoped session fold off, unknown). Fed to
/// `IrBuilder::set_ffm_heap_session_class`; the same answer
/// [`scoped_session_never_closing_class_ids`] admits first.
pub(crate) fn scoped_heap_session_class_id(helpers: &crate::DirectHelperTable) -> u32 {
    let raw = helpers.well_known_class_id;
    let ctx = helpers.static_base_resolver_ctx;
    if raw == 0 || ctx == 0 || !ir_scoped_session_fold_enabled() {
        return 0;
    }
    // SAFETY: as in `scoped_session_never_closing_class_ids`.
    let f: crate::direct_helpers::DirectHelperFnWellKnownClassId =
        unsafe { std::mem::transmute(raw) };
    // Cast: `usize` context to the C ABI's i64 parameter.
    //
    // SAFETY: as in `scoped_session_never_closing_class_ids`.
    let id = unsafe { f(ctx as i64, WELL_KNOWN_SESSION_HEAP) };
    u32::try_from(id).unwrap_or(0)
}

pub(crate) fn scoped_session_never_closing_class_ids(
    helpers: &crate::DirectHelperTable,
) -> Vec<u32> {
    let raw = helpers.well_known_class_id;
    let ctx = helpers.static_base_resolver_ctx;
    if raw == 0 || ctx == 0 || !ir_scoped_session_fold_enabled() {
        return Vec::new();
    }
    // SAFETY: as in `ir_integer_cache_high`.
    let f: crate::direct_helpers::DirectHelperFnWellKnownClassId =
        unsafe { std::mem::transmute(raw) };
    let mut ids: Vec<u32> = Vec::with_capacity(3);
    for which in [
        WELL_KNOWN_SESSION_HEAP,
        WELL_KNOWN_SESSION_GLOBAL,
        WELL_KNOWN_SESSION_IMPLICIT,
    ] {
        // Cast: `usize` context to the C ABI's i64 parameter.
        //
        // SAFETY: as in `ir_integer_cache_high`.
        let id = unsafe { f(ctx as i64, which) };
        // Class id 0 is `java/lang/Object` (never a session) and the
        // resolver's "unknown"; ids are dense `u32`s.
        if let Ok(id) = u32::try_from(id) {
            if id != 0 && !ids.contains(&id) {
                ids.push(id);
            }
        }
    }
    ids
}


/// The compiled-`getClass()` word table's shape (`DirectHelperTable::
/// getclass_mirror_words`; the VM's `ClassRealm::getclass_mirror_words`
/// allocates it and const-asserts these against its own constants): word 0
/// holds [`GETCLASS_WORDS_CLASS_CAPACITY`], words `1 ..= CAP` the class ids
/// `0 .. CAP`, and since nio20 wave 5 (lane L2, W4-L2-2) the
/// [`GETCLASS_WORDS_LAMBDA_CAPACITY`] words after them the lambda-proxy ids
/// `LAMBDA_BASE ..` (dense: one counter seeded at the base hands them out).
pub const GETCLASS_WORDS_CLASS_CAPACITY: usize = 1 << 16;
/// The first lambda-proxy class id (the VM's `LAMBDA_PROXY_ID_BASE`).
pub const GETCLASS_WORDS_LAMBDA_BASE_ID: u32 = 0x8000_0000;
/// How many lambda-proxy ids, from [`GETCLASS_WORDS_LAMBDA_BASE_ID`], have a
/// word. A proxy id past it keeps the helper call.
pub const GETCLASS_WORDS_LAMBDA_CAPACITY: usize = 1 << 16;
/// nio21 wave 1, lane L5 (W4-L2-1, page
/// `r20-getclass-array-receivers-have-no-word-FIXED-20261008.md`): the word index of
/// the first ONE-DIMENSIONAL PRIMITIVE ARRAY word, after the lambda band. Eight
/// words, one per element type `Boolean ..= Long` (`ArrayElementType` 4..=11),
/// at `GETCLASS_WORDS_PRIM_ARRAY_BASE + (elem - Boolean)`. A primitive array's
/// header carries no class id of its own (`0` plus an element-type tag), so
/// its word is keyed by the tag byte compiled code already reads.
pub const GETCLASS_WORDS_PRIM_ARRAY_BASE: usize =
    1 + GETCLASS_WORDS_CLASS_CAPACITY + GETCLASS_WORDS_LAMBDA_CAPACITY;
/// How many primitive-array words follow the lambda band (one per primitive
/// element type).
pub const GETCLASS_WORDS_PRIM_ARRAY_CAPACITY: usize = 8;
/// nio21 wave 2, lane L5 (L5W1-1, page
/// `r20-getclass-array-receivers-have-no-word-FIXED-20261008.md`): the word index of
/// the first REFERENCE-ARRAY word, after the primitive-array band. A reference
/// array's header carries its COMPONENT's class id, so its word is keyed by
/// that id: word `GETCLASS_WORDS_REF_ARRAY_BASE + component_id` holds the
/// mirror of the array class over that component (`String[]` under
/// `String`'s id, `int[][]` under `[I`'s id). Only a component a built-in
/// loader defined, whose array class is the unique one of its name, is ever
/// published (the VM's `ref_array_class_mirror_publishing_word` states why
/// that is every arm of `native_object_get_class`'s answer).
pub const GETCLASS_WORDS_REF_ARRAY_BASE: usize =
    GETCLASS_WORDS_PRIM_ARRAY_BASE + GETCLASS_WORDS_PRIM_ARRAY_CAPACITY;
/// How many component ids, from `0`, have a reference-array word. A component
/// id at or above it (a lambda proxy's included) keeps the helper call.
pub const GETCLASS_WORDS_REF_ARRAY_CAPACITY: usize = 1 << 16;
/// JIT round 20 wave 19, lane zAssign (`CRATONVM_JIT_CLASS_IS_ASSIGNABLE_INLINE`):
/// the word index of the first MIRROR-HINT word, after the reference-array
/// band. A direct-mapped `mirror address -> class id` hint per
/// [`mirror_hint_slot`] of the address, holding [`mirror_hint_word`] or `0`.
/// A hint is only a guess: compiled code accepts its class id `c` only when
/// the class word of `c` (word `1 + c`) IS the mirror, which -- a non-zero
/// class word being `class_mirrors[c]` -- proves `c` is the mirror's class
/// whatever the hint's age, a racing write or a moved mirror. The flag bits
/// are a function of the class at `c`; the VM clears the hints naming `c`
/// when `c`'s word is forgotten, so a re-used id never inherits them.
pub const GETCLASS_WORDS_MIRROR_HINT_BASE: usize =
    GETCLASS_WORDS_REF_ARRAY_BASE + GETCLASS_WORDS_REF_ARRAY_CAPACITY;
/// How many mirror-hint words follow the reference-array band (a power of
/// two: [`mirror_hint_slot`] masks with it).
pub const GETCLASS_WORDS_MIRROR_HINT_CAPACITY: usize = 1 << 12;
/// A mirror-hint word's "present" bit; its low 32 bits are the class id.
pub const MIRROR_HINT_PRESENT: u64 = 1 << 32;
/// A mirror-hint word's "a miss against this TARGET may be answered" bit: the
/// class is neither an array nor a primitive-named class nor a
/// `synthetic_implements` target (`cratonvm_types::synthetic_targets`), so
/// the native's name-keyed disjuncts cannot admit a pair the hierarchy
/// refuses (the duplicate-name rule is screened at run time by the latch).
pub const MIRROR_HINT_MISS_OK: u64 = 1 << 33;
/// A mirror-hint word's "this class gets no hint" bit (with the class id in
/// the low 32 bits and [`MIRROR_HINT_PRESENT`] clear): the VM's filler
/// remembers an array, primitive-named or `getClass()`-remapped class so its
/// next native call does not ask again. Compiled code sees no present bit and
/// declines, as for an empty slot.
pub const MIRROR_HINT_DECLINED: u64 = 1 << 34;
/// Total words of the table: the capacity word, the class words, the lambda
/// words, the primitive-array words, the reference-array words, and (JIT
/// round 20 wave 19, lane zAssign) the mirror-hint words.
pub const GETCLASS_WORDS_TOTAL: usize =
    GETCLASS_WORDS_MIRROR_HINT_BASE + GETCLASS_WORDS_MIRROR_HINT_CAPACITY;

/// The multiplier of [`mirror_hint_slot`] (2^64 / phi, Fibonacci hashing:
/// mirrors allocated at a regular stride spread over the band).
pub const MIRROR_HINT_HASH_MUL: u64 = 0x9E37_79B9_7F4A_7C15;
/// The right shift of [`mirror_hint_slot`]: the top 12 bits of the product.
pub const MIRROR_HINT_HASH_SHIFT: u32 = 64 - GETCLASS_WORDS_MIRROR_HINT_CAPACITY.trailing_zeros();

/// JIT round 20 wave 19, lane zAssign: the mirror-hint slot of a mirror at
/// `addr` -- `((addr >> 3) * MUL) >> (64 - log2 CAP)` (wrapping), exactly
/// what `class_mirror_hint_resolve` emits.
pub fn mirror_hint_slot(addr: usize) -> usize {
    // Cast: a 64-bit target (usize is u64), and the shifted product is below
    // the band's capacity.
    (((addr as u64) >> 3).wrapping_mul(MIRROR_HINT_HASH_MUL) >> MIRROR_HINT_HASH_SHIFT) as usize
}

/// JIT round 20 wave 19, lane zAssign: the mirror-hint word naming class id
/// `class_id`, with [`MIRROR_HINT_MISS_OK`] when `miss_ok`.
pub fn mirror_hint_word(class_id: u32, miss_ok: bool) -> u64 {
    let flags = if miss_ok {
        MIRROR_HINT_PRESENT | MIRROR_HINT_MISS_OK
    } else {
        MIRROR_HINT_PRESENT
    };
    flags | u64::from(class_id)
}

/// `CRATONVM_JIT_GETCLASS_REF_ARRAY_WORDS` (nio21 wave 2, lane L5, **default
/// ON**, `=0` off): both tiers' `getClass()` prefix answers a reference-array
/// receiver from the table's reference-array band (keyed by the header's
/// component id), and the VM's thin helper answers and publishes those words.
/// Off: a reference array declines to the call as before and nothing is
/// published. Read per compile / per helper call.
pub fn getclass_ref_array_words_enabled() -> bool {
    true
}

/// The array arm of both tiers' `getClass()` prefix (nio21 wave 2, lane L5):
/// the reference-array lookup (when `refs`) followed by the primitive-array
/// arm [`getclass_prim_array_lookup_bytes`] (when `prims`). Entry: RAX = a
/// NON-NULL receiver whose kind bits are non-zero; R10 = the table. Exit by
/// fall-through: RAX = the published mirror of the receiver's array class.
/// Returns the bytes and the rel32 operands that decline (left zero). Clobbers
/// RAX and the flags only. With neither arm the bytes are one `JMP decline`.
///
/// ```text
///   CMP   BYTE [RAX+KIND_TAGS], Array|Reference<<2 ; JNE prim (or decline)
///   MOV   EAX, [RAX]                    ; the COMPONENT's class id
///   CMP   EAX, REF_CAP ; JAE decline    ; past the band (lambda ids too)
///   MOV   RAX, [R10 + RAX*8 + 8*REF_BASE]
///   TEST  RAX, RAX ; JZ decline         ; not published
///   JMP   done
/// prim:
///   <getclass_prim_array_lookup_bytes>
/// done:
/// ```
///
/// The tag compare is exact for "a reference array": the byte's bits 6..7 are
/// reserved zero on every array (the primitive arm's screen rests on the same
/// fact), a primitive array's element tag is non-zero, and a filler's kind is
/// `2`. A byte that ever carried other bits would only decline.
pub fn getclass_array_arm_bytes(refs: bool, prims: bool) -> (Vec<u8>, Vec<usize>) {
    const REF_TAG: u8 = (cratonvm_types::ObjectKind::Array as u8)
        | ((cratonvm_types::ArrayElementType::Reference as u8) << 2);
    const _: () = assert!(REF_TAG == 0x01);
    const _: () = assert!(cratonvm_types::KIND_TAGS_BYTE_OFFSET < 0x80);
    const _: () = assert!(8 * GETCLASS_WORDS_REF_ARRAY_BASE <= i32::MAX as usize);
    const _: () = assert!(GETCLASS_WORDS_REF_ARRAY_CAPACITY <= i32::MAX as usize);
    const _: () = assert!(
        (GETCLASS_WORDS_LAMBDA_BASE_ID as usize) >= GETCLASS_WORDS_REF_ARRAY_CAPACITY,
        "a lambda component must fall past the reference-array band"
    );
    let mut b: Vec<u8> = Vec::with_capacity(96);
    let mut decline: Vec<usize> = Vec::new();
    let mut to_done: Option<usize> = None;
    let mut not_ref: Option<usize> = None;
    if refs {
        // CMP BYTE [RAX + disp8], imm8 (80 /7 ib) ; JNE not_ref
        // Cast: asserted above to fit a disp8.
        let kind_tags = cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8;
        b.extend_from_slice(&[0x80, 0x78, kind_tags, REF_TAG]);
        b.extend_from_slice(&[0x0F, 0x85]);
        not_ref = Some(b.len());
        b.extend_from_slice(&[0, 0, 0, 0]);
        // MOV EAX, [RAX] -- the component id at offset 0, zero-extended.
        b.extend_from_slice(&[0x8B, 0x00]);
        // CMP EAX, imm32 (3D id) ; JAE decline
        b.push(0x3D);
        // Cast: asserted above to fit.
        b.extend_from_slice(&(GETCLASS_WORDS_REF_ARRAY_CAPACITY as u32).to_le_bytes());
        b.extend_from_slice(&[0x0F, 0x83]);
        decline.push(b.len());
        b.extend_from_slice(&[0, 0, 0, 0]);
        // MOV RAX, [R10 + RAX*8 + disp32] (REX.W+B 8B /r, mod 10, SIB scale 8).
        b.extend_from_slice(&[0x49, 0x8B, 0x84, 0xC2]);
        // Cast: asserted above to fit.
        b.extend_from_slice(&((8 * GETCLASS_WORDS_REF_ARRAY_BASE) as u32).to_le_bytes());
        // TEST RAX, RAX ; JZ decline
        b.extend_from_slice(&[0x48, 0x85, 0xC0, 0x0F, 0x84]);
        decline.push(b.len());
        b.extend_from_slice(&[0, 0, 0, 0]);
        // JMP done
        b.push(0xE9);
        to_done = Some(b.len());
        b.extend_from_slice(&[0, 0, 0, 0]);
    }
    let here = |b: &mut Vec<u8>, at: usize| {
        // Cast: offsets within a ~100-byte vector.
        let rel = b.len() as i32 - (at as i32 + 4);
        b[at..at + 4].copy_from_slice(&rel.to_le_bytes());
    };
    if let Some(at) = not_ref {
        here(&mut b, at);
    }
    if prims {
        let (arm, edges) = getclass_prim_array_lookup_bytes();
        let at = b.len();
        b.extend_from_slice(&arm);
        decline.extend(edges.into_iter().map(|e| at + e));
    } else {
        // JMP decline
        b.push(0xE9);
        decline.push(b.len());
        b.extend_from_slice(&[0, 0, 0, 0]);
    }
    if let Some(at) = to_done {
        here(&mut b, at);
    }
    (b, decline)
}

/// The primitive-array band slot (`0..8`) of a one-dimensional array of
/// `elem`, or `None` for a reference array. The one mapping the VM's publisher
/// and [`getclass_prim_array_lookup_bytes`] share.
pub fn getclass_prim_array_slot(elem: cratonvm_types::ArrayElementType) -> Option<usize> {
    use cratonvm_types::ArrayElementType as E;
    match elem {
        E::Reference => None,
        // Cast: the enum's `repr(u8)` discriminant, 4..=11 (asserted below).
        other => Some(usize::from(other as u8 - E::Boolean as u8)),
    }
}

const _: () = {
    use cratonvm_types::ArrayElementType as E;
    assert!(E::Boolean as u8 == 4 && E::Long as u8 == 11);
    assert!((E::Long as u8 - E::Boolean as u8) as usize + 1 == GETCLASS_WORDS_PRIM_ARRAY_CAPACITY);
};

/// `CRATONVM_JIT_GETCLASS_PRIM_ARRAY_WORDS` (nio21 wave 1, lane L5, **default
/// ON**, `=0` off): both tiers' `getClass()` prefix answers a one-dimensional
/// primitive array receiver from the table's primitive-array band, and the
/// VM's thin helper publishes those words. Off: an array declines to the call
/// as before (and nothing is published). Read per compile / per helper call.
pub fn getclass_prim_array_words_enabled() -> bool {
    true
}

/// The primitive-array arm of both tiers' `getClass()` prefix (nio21 wave 1,
/// lane L5). Entry: RAX = a NON-NULL receiver whose kind bits are non-zero
/// (the plain-object test already failed); R10 = the table. Exit by
/// fall-through: RAX = the published mirror of the receiver's array class.
/// Returns the bytes and the rel32 operands that decline (left zero). Clobbers
/// RAX and the flags only.
///
/// ```text
///   MOVZX EAX, BYTE [RAX+KIND_TAGS]     ; kind (bits 0..1) | elem << 2
///   SUB   EAX, Array|Boolean<<2         ; the scoped fold's one-compare screen
///   TEST  EAX, ~0x1C ; JNZ decline      ; reference array, filler, junk
///   SHR   EAX, 2                        ; elem - Boolean, 0..7
///   MOV   RAX, [R10 + RAX*8 + 8*PRIM_BASE]
///   TEST  RAX, RAX ; JZ decline         ; not published
/// ```
///
/// Exact for "a one-dimensional primitive array": `[[I` is a REFERENCE array
/// (its element tag is `Reference`), so it fails the screen; the tag byte's
/// bits 6..7 are reserved zero on every array, and a forwarding or lock state
/// lives in other mark bits (`MARK_QUARTET_MASK` is disjoint from them).
pub fn getclass_prim_array_lookup_bytes() -> (Vec<u8>, Vec<usize>) {
    const LO: u8 = (cratonvm_types::ObjectKind::Array as u8)
        | ((cratonvm_types::ArrayElementType::Boolean as u8) << 2);
    const SPAN: u32 = ((cratonvm_types::ArrayElementType::Long as u32)
        - (cratonvm_types::ArrayElementType::Boolean as u32))
        << 2;
    const _: () = assert!(LO == 0x11 && SPAN == 0x1C);
    const _: () = assert!(cratonvm_types::KIND_TAGS_BYTE_OFFSET < 0x80);
    const _: () = assert!(8 * GETCLASS_WORDS_PRIM_ARRAY_BASE <= i32::MAX as usize);
    let mut b: Vec<u8> = Vec::with_capacity(40);
    let mut decline: Vec<usize> = Vec::new();
    // MOVZX EAX, BYTE [RAX + disp8] (0F B6 /r, mod 01).
    // Cast: asserted above to fit a disp8.
    b.extend_from_slice(&[
        0x0F,
        0xB6,
        0x40,
        cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8,
    ]);
    // SUB EAX, imm8 (83 /5 ib).
    b.extend_from_slice(&[0x83, 0xE8, LO]);
    // TEST EAX, imm32 (A9 id) ; JNZ decline.
    b.push(0xA9);
    b.extend_from_slice(&(!SPAN).to_le_bytes());
    b.extend_from_slice(&[0x0F, 0x85]);
    decline.push(b.len());
    b.extend_from_slice(&[0, 0, 0, 0]);
    // SHR EAX, 2 (C1 /5 ib).
    b.extend_from_slice(&[0xC1, 0xE8, 0x02]);
    // MOV RAX, [R10 + RAX*8 + disp32] (REX.W+B 8B /r, mod 10, SIB scale 8).
    b.extend_from_slice(&[0x49, 0x8B, 0x84, 0xC2]);
    // Cast: asserted above to fit.
    b.extend_from_slice(&((8 * GETCLASS_WORDS_PRIM_ARRAY_BASE) as u32).to_le_bytes());
    // TEST RAX, RAX ; JZ decline
    b.extend_from_slice(&[0x48, 0x85, 0xC0, 0x0F, 0x84]);
    decline.push(b.len());
    b.extend_from_slice(&[0, 0, 0, 0]);
    (b, decline)
}

/// The word-table lookup of both tiers' `getClass()` prefix, as bytes (nio20
/// wave 5, lane L2). Entry: RAX = the receiver's class id, zero-extended; R10 =
/// the table. Exit by fall-through: RAX = the published mirror (non-zero).
/// Returns the bytes and the offsets of the rel32 operands that decline (an id
/// with no word, an unpublished word), left zero for the caller to patch.
/// Clobbers RAX and the flags only.
///
/// ```text
///   CMP  RAX, [R10] ; JAE lambda             ; word 0 = class capacity
///   MOV  RAX, [R10 + RAX*8 + 8] ; JMP loaded
/// lambda:
///   SUB  EAX, LAMBDA_BASE                    ; an id in [CAP, BASE) wraps high
///   CMP  EAX, LAMBDA_CAP ; JAE decline
///   MOV  RAX, [R10 + RAX*8 + 8 + 8*CAP]
/// loaded:
///   TEST RAX, RAX ; JZ decline               ; not published
/// ```
pub fn getclass_table_lookup_bytes() -> (Vec<u8>, Vec<usize>) {
    const _: () = assert!(GETCLASS_WORDS_LAMBDA_CAPACITY <= i32::MAX as usize);
    const _: () = assert!(8 * (1 + GETCLASS_WORDS_CLASS_CAPACITY) <= i32::MAX as usize);
    const _: () = assert!(
        GETCLASS_WORDS_LAMBDA_BASE_ID as usize >= GETCLASS_WORDS_CLASS_CAPACITY,
        "the lambda band must not overlap the class band"
    );
    let mut b: Vec<u8> = Vec::with_capacity(48);
    let mut decline: Vec<usize> = Vec::new();
    // CMP RAX, [R10] (REX.W+B 3B /r) ; JAE lambda
    b.extend_from_slice(&[0x49, 0x3B, 0x02, 0x0F, 0x83]);
    let to_lambda = b.len();
    b.extend_from_slice(&[0, 0, 0, 0]);
    // MOV RAX, [R10 + RAX*8 + 8] ; JMP loaded
    b.extend_from_slice(&[0x49, 0x8B, 0x44, 0xC2, 0x08, 0xE9]);
    let to_loaded = b.len();
    b.extend_from_slice(&[0, 0, 0, 0]);
    let here = |b: &mut Vec<u8>, at: usize| {
        // Cast: offsets within a ~50-byte vector.
        let rel = b.len() as i32 - (at as i32 + 4);
        b[at..at + 4].copy_from_slice(&rel.to_le_bytes());
    };
    here(&mut b, to_lambda);
    // SUB EAX, imm32 (2D id): the 32-bit result zero-extends into RAX.
    b.push(0x2D);
    b.extend_from_slice(&GETCLASS_WORDS_LAMBDA_BASE_ID.to_le_bytes());
    // CMP EAX, imm32 (3D id) ; JAE decline
    b.push(0x3D);
    // Cast: asserted above to fit.
    b.extend_from_slice(&(GETCLASS_WORDS_LAMBDA_CAPACITY as u32).to_le_bytes());
    b.extend_from_slice(&[0x0F, 0x83]);
    decline.push(b.len());
    b.extend_from_slice(&[0, 0, 0, 0]);
    // MOV RAX, [R10 + RAX*8 + disp32] (REX.W+B 8B /r, mod 10, SIB scale 8).
    b.extend_from_slice(&[0x49, 0x8B, 0x84, 0xC2]);
    // Cast: asserted above to fit.
    b.extend_from_slice(&((8 * (1 + GETCLASS_WORDS_CLASS_CAPACITY)) as u32).to_le_bytes());
    here(&mut b, to_loaded);
    // TEST RAX, RAX ; JZ decline
    b.extend_from_slice(&[0x48, 0x85, 0xC0, 0x0F, 0x84]);
    decline.push(b.len());
    b.extend_from_slice(&[0, 0, 0, 0]);
    (b, decline)
}

/// JIT round 20 wave 19, lane zAssign (`CRATONVM_JIT_CLASS_IS_ASSIGNABLE_INLINE`):
/// a mirror's class id from the compiled-`getClass()` table's mirror-hint
/// band. Entry: R10 = the table, the mirror (non-null) in RDX (`src_rdx`) or
/// RAX. Fall-through: RCX = the class id (zero-extended), R11 = its hint word.
/// Writes RCX, R11 and the flags; the mirror's register is only read.
///
/// ```text
///   MOV RCX, m ; SHR RCX, 3 ; MOV R11, MUL ; IMUL RCX, R11
///   SHR RCX, 64 - log2 CAP                   ; mirror_hint_slot(m)
///   MOV R11, [R10 + RCX*8 + 8*HINT_BASE]     ; the hint word
///   BT R11, 32 ; JNC decline                 ; no hint
///   MOV ECX, R11D                            ; its class id c
///   CMP RCX, [R10] ; JAE decline             ; past the class band
///   CMP m, [R10 + RCX*8 + 8] ; JNE decline   ; c's class word is not m
/// ```
///
/// The last compare is the proof: a non-zero class word of `c` is
/// `class_mirrors[c]` (the VM's `GetClassMirrorWords` contract: published
/// under the `class_mirrors` lock, re-keyed and withdrawn with every mutator
/// parked), so `c` is the mirror's class however stale or torn the hint was.
/// No GC point lies between it and the use of `c`.
pub(in crate::ir_lower) fn class_mirror_hint_resolve(a: &mut VhConstAsm, src_rdx: bool) {
    const _: () = assert!(8 * GETCLASS_WORDS_MIRROR_HINT_BASE <= i32::MAX as usize);
    const _: () = assert!(GETCLASS_WORDS_MIRROR_HINT_CAPACITY.is_power_of_two());
    const _: () = assert!(MIRROR_HINT_HASH_SHIFT > 0 && MIRROR_HINT_HASH_SHIFT < 64);
    // Cast: asserted in 1..64.
    let shift = MIRROR_HINT_HASH_SHIFT as u8;
    // Cast: asserted above to fit.
    let disp = (8 * GETCLASS_WORDS_MIRROR_HINT_BASE) as u32;
    if src_rdx {
        a.emit(&[0x48, 0x89, 0xD1]); // MOV RCX, RDX
    } else {
        a.emit(&[0x48, 0x89, 0xC1]); // MOV RCX, RAX
    }
    a.emit(&[0x48, 0xC1, 0xE9, 0x03]); // SHR RCX, 3
    a.emit(&[0x49, 0xBB]); // MOV R11, imm64
    a.emit(&MIRROR_HINT_HASH_MUL.to_le_bytes());
    a.emit(&[0x49, 0x0F, 0xAF, 0xCB]); // IMUL RCX, R11
    a.emit(&[0x48, 0xC1, 0xE9, shift]); // SHR RCX, shift
    a.emit(&[0x4D, 0x8B, 0x9C, 0xCA]); // MOV R11, [R10 + RCX*8 + disp32]
    a.emit(&disp.to_le_bytes());
    a.emit(&[0x49, 0x0F, 0xBA, 0xE3, 0x20]); // BT R11, 32 (MIRROR_HINT_PRESENT)
    a.jcc_decline(0x83); // JNC
    a.emit(&[0x44, 0x89, 0xD9]); // MOV ECX, R11D (zero-extends RCX)
    a.emit(&[0x49, 0x3B, 0x0A]); // CMP RCX, [R10] (the class capacity)
    a.jcc_decline(0x83); // JAE
    if src_rdx {
        a.emit(&[0x49, 0x3B, 0x54, 0xCA, 0x08]); // CMP RDX, [R10 + RCX*8 + 8]
    } else {
        a.emit(&[0x49, 0x3B, 0x44, 0xCA, 0x08]); // CMP RAX, [R10 + RCX*8 + 8]
    }
    a.jcc_decline(0x85); // JNE
}

/// JIT round 20 wave 19, lane zAssign: "is the class `s` the class `t` or a
/// subtype of it", from `s`'s supers display, with both answers. Entry: RCX =
/// `s` (zero-extended), RAX = `t`'s mirror-hint word (low 32 bits `t`, bit 33
/// [`MIRROR_HINT_MISS_OK`]). Returns the `JE hit` rel32 fields and the `JMP
/// miss` one; declines go to `a.decline`. Writes RCX, R11 and the flags; RAX
/// is preserved.
///
/// A HIT -- `s == t`, or `t` in `s`'s superclass chain or transitive interface
/// set -- is JVMS 4.10.1.2's answer and the native's `is_subclass` (the same
/// edges, `jit_realm::SupersDisplays`; the `instanceof` display arm's
/// premise). A MISS is the native's `false` only when every other disjunct of
/// `native_class_is_assignable_from` / `native_class_is_instance` is provably
/// false, which is pAlloc2's `typecheck_display_answer_bytes` argument with
/// the target-side screen moved to run time:
///
/// * `s`'s display carries verdict `1` (no instance-admitted shape -- Proxy,
///   AnnotationProxy, `Unmodifiable*` -- and no class whose edges the VM may
///   still rewrite, on its chain or in its interface set), else decline;
/// * `t`'s hint says [`MIRROR_HINT_MISS_OK`] (not an array, not primitive-named,
///   and -- only with `CRATONVM_JIT_TYPECHECK_DISPLAY_MISS_ANY_TARGET=0` --
///   not a `synthetic_implements` target: since JIT r21 tc21b the VM's filler
///   writes the bit for those too, because a verdict-`1` source is declined by
///   every `synthetic_implements` rule, the first bullet), else decline;
/// * the duplicate-class-name latch is clear (the loader-duplication by-name
///   rule then admits only `t` itself, which the display did not list), else
///   decline.
///
/// No display for `s` (an interface, a class not yet published, an id past
/// the table: a lambda proxy) declines.
///
/// ```text
///   CMP EAX, ECX ; JE hit
///   (cell, capacity, display of s: JZ / JAE -> decline)    ; R11 = display
///   MOV ECX, [R11]                                ; len
/// c:  TEST ECX, ECX ; JZ c_done ; CMP [R11 + RCX*4], EAX ; JE hit ; DEC ECX ; JMP c
/// c_done: MOV ECX, [R11] ; LEA R11, [R11 + RCX*4 + 4] ; MOV ECX, [R11]   ; n
/// i:  TEST ECX, ECX ; JZ verdict ; CMP [R11 + RCX*4], EAX ; JE hit ; DEC ECX ; JMP i
/// verdict: MOV ECX, [R11] ; CMP DWORD [R11 + RCX*4 + 4], 1 ; JNE decline
///   BT RAX, 33 ; JNC decline
///   MOV R11, latch ; CMP BYTE [R11], 0 ; JNE decline
///   JMP miss
/// ```
pub(in crate::ir_lower) fn class_assignable_core(a: &mut VhConstAsm, cell: usize, latch: usize) -> (Vec<usize>, usize) {
    let mut hits: Vec<usize> = Vec::with_capacity(3);
    a.emit(&[0x39, 0xC8]); // CMP EAX, ECX
    hits.push(a.jcc_forward(0x84)); // JE hit
    a.emit(&[0x49, 0xBB]); // MOV R11, imm64
    a.emit(&(cell as u64).to_le_bytes()); // Cast: a data address
    a.emit(&[0x4D, 0x8B, 0x1B]); // MOV R11, [R11]
    a.emit(&[0x4D, 0x85, 0xDB]); // TEST R11, R11
    a.jcc_decline(0x84);
    a.emit(&[0x49, 0x3B, 0x0B]); // CMP RCX, [R11]
    a.jcc_decline(0x83); // JAE
    a.emit(&[0x4D, 0x8B, 0x5C, 0xCB, 0x08]); // MOV R11, [R11 + RCX*8 + 8]
    a.emit(&[0x4D, 0x85, 0xDB]); // TEST R11, R11
    a.jcc_decline(0x84); // no display

    // The chain, then the interface set: ids at [R11 + 4*i], i = count ..= 1.
    let scan = |a: &mut VhConstAsm, hits: &mut Vec<usize>| -> usize {
        let top = a.b.len();
        a.emit(&[0x85, 0xC9]); // TEST ECX, ECX
        let done = a.jcc_forward(0x84); // JZ
        a.emit(&[0x41, 0x39, 0x04, 0x8B]); // CMP [R11 + RCX*4], EAX
        hits.push(a.jcc_forward(0x84)); // JE hit
        a.emit(&[0xFF, 0xC9]); // DEC ECX
        a.emit(&[0xE9]); // JMP top

        // Cast: a backward distance inside one short sequence.
        let rel = top as i64 - (a.b.len() as i64 + 4);
        a.emit(&(rel as i32).to_le_bytes());
        done
    };
    a.emit(&[0x41, 0x8B, 0x0B]); // MOV ECX, [R11] (len)
    let chain_done = scan(&mut *a, &mut hits);
    a.bind(chain_done);
    a.emit(&[0x41, 0x8B, 0x0B]); // MOV ECX, [R11] (len)
    a.emit(&[0x4D, 0x8D, 0x5C, 0x8B, 0x04]); // LEA R11, [R11 + RCX*4 + 4] -> n
    a.emit(&[0x41, 0x8B, 0x0B]); // MOV ECX, [R11] (n)
    let iface_done = scan(&mut *a, &mut hits);
    a.bind(iface_done);
    a.emit(&[0x41, 0x8B, 0x0B]); // MOV ECX, [R11] (n)
    a.emit(&[0x41, 0x83, 0x7C, 0x8B, 0x04, 0x01]); // CMP DWORD [R11 + RCX*4 + 4], 1
    a.jcc_decline(0x85); // JNE: not hierarchy-only
    a.emit(&[0x48, 0x0F, 0xBA, 0xE0, 0x21]); // BT RAX, 33 (MIRROR_HINT_MISS_OK)
    a.jcc_decline(0x83); // JNC: the target's name rules may admit
    a.emit(&[0x49, 0xBB]); // MOV R11, imm64
    a.emit(&(latch as u64).to_le_bytes()); // Cast: a static's address
    a.emit(&[0x41, 0x80, 0x3B, 0x00]); // CMP BYTE [R11], 0
    a.jcc_decline(0x85); // JNE: a duplicate class name exists
    a.emit(&[0xE9]); // JMP miss
    let miss = a.rel32_field();
    (hits, miss)
}

/// JIT round 20 wave 19, lane zAssign (`CRATONVM_JIT_CLASS_IS_ASSIGNABLE_INLINE`):
/// `t.isAssignableFrom(s)` with no call. Entry: RAX = `t`'s mirror (the
/// receiver), RDX = `s`'s mirror (the argument). Returns `(bytes, decline,
/// hits, miss)`: rel32 fields to the call, to the caller's `1` and to its
/// `0`. A null mirror declines (the call raises the argument's NPE), and so
/// does every mirror without a validated hint -- an array or primitive
/// mirror is never given one (the VM's filler,
/// `vm::jit::helpers::class_mirror_id_hint_fill`). Then
/// [`class_assignable_core`]. Writes RAX, RCX, R10, R11 and the flags.
pub(crate) fn class_assignable_answer_bytes(
    table: usize,
    cell: usize,
    latch: usize,
) -> (Vec<u8>, Vec<usize>, Vec<usize>, usize) {
    let mut a = VhConstAsm::default();
    a.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
    a.jcc_decline(0x84);
    a.emit(&[0x48, 0x85, 0xD2]); // TEST RDX, RDX
    a.jcc_decline(0x84);
    a.emit(&[0x49, 0xBA]); // MOV R10, imm64
    a.emit(&(table as u64).to_le_bytes()); // Cast: the table's address
    class_mirror_hint_resolve(&mut a, false); // t
    a.emit(&[0x4C, 0x89, 0xD8]); // MOV RAX, R11 (t's hint word)
    class_mirror_hint_resolve(&mut a, true); // s -> RCX
    let (hits, miss) = class_assignable_core(&mut a, cell, latch);
    (a.b, a.decline, hits, miss)
}

/// JIT round 20 wave 19, lane zAssign (`CRATONVM_JIT_CLASS_IS_INSTANCE_SUBTYPE_INLINE`):
/// `t.isInstance(obj)` for a plain `obj` of class `s` whose compiled-
/// `getClass()` word is published but is not `t`. Entry: RCX = `s`
/// (zero-extended), RDX = `t`'s mirror (the receiver). Same returns and
/// writes as [`class_assignable_answer_bytes`].
pub(crate) fn class_instance_subtype_answer_bytes(
    table: usize,
    cell: usize,
    latch: usize,
) -> (Vec<u8>, Vec<usize>, Vec<usize>, usize) {
    let mut a = VhConstAsm::default();
    a.emit(&[0x48, 0x85, 0xD2]); // TEST RDX, RDX
    a.jcc_decline(0x84);
    a.emit(&[0x89, 0xC8]); // MOV EAX, ECX (s)
    a.emit(&[0x49, 0xBA]); // MOV R10, imm64
    a.emit(&(table as u64).to_le_bytes()); // Cast: the table's address
    class_mirror_hint_resolve(&mut a, true); // t -> RCX, R11
    a.emit(&[0x89, 0xC1]); // MOV ECX, EAX (s)
    a.emit(&[0x4C, 0x89, 0xD8]); // MOV RAX, R11 (t's hint word)
    let (hits, miss) = class_assignable_core(&mut a, cell, latch);
    (a.b, a.decline, hits, miss)
}

/// JIT r23 wave 8, lane L8 (`CRATONVM_JIT_IR_INLINE_REFLECT_NEW_ARRAY`): the
/// component class of an `Array.newArray(component, n)` call, from the
/// compiled-`getClass()` table's mirror-hint band. Entry: RDX = the component
/// mirror. Fall-through: RCX = its class id `c` (zero-extended). Writes RCX,
/// R10 (the table), R11 and the flags; RDX is only read. Every rel32 in the
/// returned list is a decline edge.
///
/// ```text
///   TEST RDX, RDX ; JZ decline        ; null: the call throws the NPE
///   MOV R10, table
///   class_mirror_hint_resolve(RDX)    ; no hint, a stale hint, a lambda id:
///                                     ; decline; else c's class word IS RDX
/// ```
///
/// Why a present, proven hint is exactly the set of mirrors
/// `jit_reflect_new_array_direct` serves with `jit_anewarray_object_body(c)`:
/// the proof makes `c` the mirror's class (its non-zero class word is
/// `class_mirrors[c]`); no fill ever writes a present bit for an array
/// class, a primitive-named class (`int.class`, `void.class`: the helper's
/// IAE / primitive-array arms) or a `getClass()`-remapped one (each filler
/// refuses them -- `MIRROR_HINT_DECLINED`), so `c` is a non-array reference
/// class: one dimension, never the 255-dimension IAE. A lambda-proxy id is
/// past the class band and declines.
///
/// j27 wave 4 (lane CLONEIR; HotSpot parity, no switch): with a non-zero
/// `jvmti_word` (`DirectHelperTable::jvmti_alloc_events_word`) the bytes first
/// decline while a C JVMTI env listens for `VMObjectAlloc` or
/// `SampledObjectAlloc`, where `jit_reflect_new_array_direct` declines to the
/// native that posts the event
/// (`clone_inline::jvmti_alloc_event_test_bytes`; writes R11 only, before it
/// is used).
pub(crate) fn reflect_new_array_resolve_bytes(
    table: usize,
    jvmti_word: usize,
) -> (Vec<u8>, Vec<usize>) {
    let mut a = VhConstAsm::default();
    if jvmti_word != 0 {
        a.emit(&clone_inline::jvmti_alloc_event_test_bytes(jvmti_word));
        a.jcc_decline(0x85); // JNZ: an allocation listener is armed
    }
    a.emit(&[0x48, 0x85, 0xD2]); // TEST RDX, RDX
    a.jcc_decline(0x84);
    a.emit(&[0x49, 0xBA]); // MOV R10, imm64
    a.emit(&(table as u64).to_le_bytes()); // Cast: the table's address
    class_mirror_hint_resolve(&mut a, true); // c -> RCX
    (a.b, a.decline)
}


/// `which` argument of `DirectHelperTable::well_known_class_id` (nio20 wave 5,
/// lane L2): `java/lang/Class`'s `ClassId` (the BOOTSTRAP definition), or `0`.
/// The receiver guard of the identity-hash arm at a `Class.hashCode()` /
/// `Object.hashCode()` site ([`ir_class_class_id`]). Numbered in its own band
/// like the session selectors.
pub const WELL_KNOWN_CLASS_CLASS: i64 = 0x1D_0001;

/// `CRATONVM_JIT_IR_IDENTITY_HASH_INLINE` (nio20 wave 5, lane L2, **default
/// ON**, `=0` keeps every identity-hash site a call): the optimizing tier's
/// inline read of an identity hash already installed in the receiver's header
/// (page `r20-identity-hash-is-a-native-call-in-compiled-code-FIXED-20261012.md`).
/// Read per site compile, so it adds no process global.
pub(crate) fn ir_identity_hash_inline_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_OBJECT_HASH_IDENTITY_ARM` (JIT round 21, lane hash,
/// NATIVE21-1; page `jr21-hash-object-hashcode-identity-arm-20261014.md`;
/// **default ON**, `=0` keeps every `Object.hashCode()` receiver but
/// `Integer` / `String` / `Class` on its call): the identity arm of both tiers'
/// box-hash prefix (`Lowerer::try_emit_ir_box_hash_equals_prefix`, the
/// single-pass `Compiler::try_emit_box_hash_equals_prefix`). At an
/// `Object`-declared `invokevirtual hashCode()I`, a plain receiver of exactly
/// `java/lang/Object` (class id 0, which dispatches every `Object` member on
/// `java/lang/Object`) answers its installed identity hash from the header
/// ([`identity_hash_read_bytes`]); every other receiver, and every decline of
/// the read, keeps the call. Also off under
/// `CRATONVM_JIT_IR_IDENTITY_HASH_INLINE=0`. Read per site compile.
pub(crate) fn object_hash_identity_arm_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_OBJECT_HASH_PROFILED_IDENTITY` (JIT round 21, lane hash,
/// NATIVE21-1; **default ON**, `=0` keeps only the exact-`Object` compare of
/// the identity arm): the identity arm also answers for the ONE class the
/// site's inline cache was seeded with (its receiver profile's dominant class)
/// when the VM proves that class's `hashCode()I` selection is
/// `Object.hashCode` ([`identity_hash_profiled_class`]). Off with
/// [`object_hash_identity_arm_enabled`]. Read per site compile.
pub(crate) fn object_hash_profiled_identity_enabled() -> bool {
    object_hash_identity_arm_enabled()
        && true
}

/// The class id the inline cache at `mic` (a [`crate::JitMICSlot`] this
/// compile owns, `0` = none) was seeded with, when the VM answers that the
/// class's virtual `hashCode()I` selection is `java/lang/Object.hashCode`
/// ([`WELL_KNOWN_VH_CONST_IDENTITY_HASH_TAG`]): a concrete class with real
/// bytes along its whole superclass chain, none of which declares or
/// registers a `hashCode()I`. `None` under the switch, for an empty slot,
/// class id 0 (the arm's own exact compare), a lambda proxy or sentinel id
/// (`>= 0x8000_0000`), and every class the VM cannot prove.
///
/// The seed is only a HINT for which class to test: the compiled compare is
/// exact and its miss is the call, and the VM's answer is a property of the
/// class (class ids are never reused within a VM; JVMTI redefinition cannot
/// add a method), so a stale seed costs one compare, never a wrong hash.
pub(crate) fn identity_hash_profiled_class(
    helpers: &crate::DirectHelperTable,
    mic: usize,
) -> Option<u32> {
    if mic == 0 || !object_hash_profiled_identity_enabled() {
        return None;
    }
    // SAFETY: `mic` is the address of a `JitMICSlot` boxed by this compile's
    // planner (`ir_mic_boxes` / the single-pass `mic_slots`), alive for the
    // whole compile; `cached_class_id` is an atomic, read once.
    // Cast: the address back to the slot it names.
    let slot = unsafe { &*(mic as *const crate::JitMICSlot) };
    let cid = slot
        .cached_class_id
        .load(std::sync::atomic::Ordering::Relaxed);
    if cid == 0 || cid >= 0x8000_0000 {
        return None;
    }
    (vh_const_ask(
        helpers,
        vh_const_query(WELL_KNOWN_VH_CONST_IDENTITY_HASH_TAG, cid, 0),
    ) == 1)
        .then_some(cid)
}

/// JIT round 21, lane hash2: the number of class ids the per-VM
/// "inherits `Object.hashCode`" byte table answers for
/// ([`identity_hash_class_table_test_bytes`]); a larger id declines. The VM
/// crate's `jit_realm::IdentityHashClassBytes` allocates exactly
/// [`IDENTITY_HASH_CLASS_TABLE_HEADER`] + this many bytes.
pub const IDENTITY_HASH_CLASS_TABLE_CAPACITY: usize = 1 << 18;
/// The table's header: one little-endian `u64`, the capacity. Byte
/// `HEADER + class_id` is the class's verdict.
pub const IDENTITY_HASH_CLASS_TABLE_HEADER: usize = 8;
/// A class verdict byte: not decided yet (the compiled test declines).
pub const IDENTITY_HASH_CLASS_UNKNOWN: u8 = 0;
/// A class verdict byte: the class's `hashCode()I` selection is proven
/// `java/lang/Object.hashCode` (the VM's `class_hash_code_identity_verdict`),
/// so an exact instance answers its installed identity hash from the header.
pub const IDENTITY_HASH_CLASS_PROVEN: u8 = 1;
/// A class verdict byte: the VM could not prove it (an override somewhere on
/// the chain, a registered native, a stub or proxy, an abstract class, ...).
pub const IDENTITY_HASH_CLASS_REFUSED: u8 = 2;
/// JIT round 21, lane hash2: the address of this VM's
/// [`IDENTITY_HASH_CLASS_TABLE_CAPACITY`]-entry verdict byte table, allocated
/// on the first ask; the class and field halves of the word are ignored. `0`
/// when the VM keeps none.
pub const WELL_KNOWN_VH_CONST_IDENTITY_HASH_TABLE_TAG: i64 = 0x5682_i64 << 48;

/// `CRATONVM_JIT_OBJECT_HASH_CLASS_TABLE` (JIT round 21, lane hash2; page
/// `jr21-hash2-object-hashcode-class-table-20261014.md`; **default ON**, `=0`
/// keeps the identity arm to its exact-`Object` and profiled-class compares):
/// the identity arm of both tiers' `Object.hashCode()` box-hash prefix also
/// answers a plain receiver of ANY class the VM has proven inherits
/// `Object.hashCode`, by one byte of the per-VM class table
/// ([`identity_hash_class_table`]). Off with
/// [`object_hash_identity_arm_enabled`]. Read per site compile.
pub(crate) fn object_hash_class_table_enabled() -> bool {
    object_hash_identity_arm_enabled()
        && true
}

/// JIT round 21, lane hash2: the address of this compile's VM's identity-hash
/// class table ([`WELL_KNOWN_VH_CONST_IDENTITY_HASH_TABLE_TAG`]), or `None`
/// (switch off, no resolver, or the VM keeps no table). Stable for the VM's
/// life: the compiled test bakes it.
pub(crate) fn identity_hash_class_table(helpers: &crate::DirectHelperTable) -> Option<u64> {
    if !object_hash_class_table_enabled() {
        return None;
    }
    let addr = vh_const_ask(
        helpers,
        vh_const_query(WELL_KNOWN_VH_CONST_IDENTITY_HASH_TABLE_TAG, 0, 0),
    );
    u64::try_from(addr).ok().filter(|&a| a != 0)
}

/// JIT round 21, lane hash2: the class-table test of the identity arm, as
/// bytes. Entry: RAX = a non-null PLAIN object (the prefix's kind test ran:
/// an array header carries its component's id). Falls through when the
/// receiver's class verdict byte is [`IDENTITY_HASH_CLASS_PROVEN`]; every
/// other case takes one of the returned rel32 operands (left zero for the
/// caller to patch) to the call. Clobbers RCX, RDX and the flags.
///
/// ```text
///   MOV ECX, [RAX + cid_off]       ; the class id, zero-extended
///   MOV RDX, imm64 table
///   CMP RCX, [RDX] ; JAE decline   ; no byte for this id
///   CMP BYTE [RDX + RCX + 8], 1 ; JNE decline
/// ```
///
/// The byte is written once per class by the VM and never rewritten (a
/// class's verdict is a property of the class: ids are never reused and a
/// redefinition cannot add a method), so a racy read sees `0` (decline) or
/// the final verdict.
pub(crate) fn identity_hash_class_table_test_bytes(
    table: u64,
    cid_off: i32,
) -> (Vec<u8>, Vec<usize>) {
    const _: () = assert!(IDENTITY_HASH_CLASS_TABLE_HEADER == 8);
    let mut b: Vec<u8> = Vec::with_capacity(32);
    let mut decline: Vec<usize> = Vec::new();
    // MOV ECX, [RAX + disp32] (8B /r, ModRM mod=10 reg=ECX rm=RAX).
    b.extend_from_slice(&[0x8B, 0x88]);
    b.extend_from_slice(&cid_off.to_le_bytes());
    // MOV RDX, imm64.
    b.extend_from_slice(&[0x48, 0xBA]);
    b.extend_from_slice(&table.to_le_bytes());
    // CMP RCX, [RDX] (REX.W 3B /r, ModRM mod=00 reg=RCX rm=RDX) ; JAE decline.
    b.extend_from_slice(&[0x48, 0x3B, 0x0A]);
    b.extend_from_slice(&[0x0F, 0x83]);
    decline.push(b.len());
    b.extend_from_slice(&[0, 0, 0, 0]);
    // CMP BYTE [RDX + RCX*1 + 8], PROVEN (80 /7 ib, SIB base=RDX index=RCX).
    b.extend_from_slice(&[0x80, 0x7C, 0x0A, 0x08, IDENTITY_HASH_CLASS_PROVEN]);
    b.extend_from_slice(&[0x0F, 0x85]);
    decline.push(b.len());
    b.extend_from_slice(&[0, 0, 0, 0]);
    (b, decline)
}

/// JIT round 21, lane hashlock (HASH2-21-1): `1` when the class named in the
/// word's class half provably inherits `java/lang/Object.equals(Object)` (see
/// [`identity_equals_profiled_class`]), else `0`. The VM files the answer in
/// its equals verdict table as it answers.
pub const WELL_KNOWN_VH_CONST_IDENTITY_EQUALS_TAG: i64 = 0x5668_i64 << 48;
/// JIT round 21, lane hashlock (HASH2-21-1): the address of this VM's
/// "inherits `Object.equals`" verdict byte table, laid out exactly as the
/// identity-hash class table ([`IDENTITY_HASH_CLASS_TABLE_HEADER`] +
/// [`IDENTITY_HASH_CLASS_TABLE_CAPACITY`] bytes, the same verdict values), so
/// [`identity_hash_class_table_test_bytes`] tests it. Allocated on the first
/// ask; the class and field halves of the word are ignored. `0` when the VM
/// keeps none.
pub const WELL_KNOWN_VH_CONST_IDENTITY_EQUALS_TABLE_TAG: i64 = 0x5669_i64 << 48;

/// `CRATONVM_JIT_OBJECT_EQUALS_IDENTITY_ARM` (JIT round 21, lane hashlock,
/// HASH2-21-1; page `jr21-native-identity-hash-crossings-pay-the-full-funnel-FIXED-20261029.md`;
/// **default ON**, `=0` keeps every `Object.equals(Object)` receiver but
/// `Integer` on its call): both tiers' box-hash prefix answers an
/// `Object`-declared `invokevirtual equals(Ljava/lang/Object;)Z` inline as
/// `this == arg` for a plain receiver of exactly `java/lang/Object`, of the
/// class the site's inline cache was seeded with, or of any class whose
/// per-VM verdict byte is PROVEN -- a class whose virtual `equals(Object)`
/// selection is `Object.equals` (vm `class_equals_identity_verdict`). Every
/// other receiver keeps the call. Read per site compile; `pub` because the VM
/// allocates the verdict table beside the identity-hash one under it.
pub fn object_equals_identity_arm_enabled() -> bool {
    true
}

/// The class id the inline cache at `mic` (a [`crate::JitMICSlot`] this
/// compile owns, `0` = none) was seeded with, when the VM answers that the
/// class's virtual `equals(Ljava/lang/Object;)Z` selection is
/// `java/lang/Object.equals` ([`WELL_KNOWN_VH_CONST_IDENTITY_EQUALS_TAG`]).
/// `None` under the switch, for an empty slot, class id 0 (the arm's own exact
/// compare), a lambda proxy or sentinel id, and every class the VM cannot
/// prove. As for the hash twin ([`identity_hash_profiled_class`]) the seed is
/// only a hint: the compare is exact and its miss is the call.
pub(crate) fn identity_equals_profiled_class(
    helpers: &crate::DirectHelperTable,
    mic: usize,
) -> Option<u32> {
    if mic == 0 || !object_equals_identity_arm_enabled() {
        return None;
    }
    // SAFETY: as in `identity_hash_profiled_class` -- `mic` is the address of
    // a `JitMICSlot` boxed by this compile's planner, alive for the compile.
    // Cast: the address back to the slot it names.
    let slot = unsafe { &*(mic as *const crate::JitMICSlot) };
    let cid = slot
        .cached_class_id
        .load(std::sync::atomic::Ordering::Relaxed);
    if cid == 0 || cid >= 0x8000_0000 {
        return None;
    }
    (vh_const_ask(
        helpers,
        vh_const_query(WELL_KNOWN_VH_CONST_IDENTITY_EQUALS_TAG, cid, 0),
    ) == 1)
        .then_some(cid)
}

/// JIT round 21, lane hashlock (HASH2-21-1): the address of this compile's
/// VM's equals verdict table ([`WELL_KNOWN_VH_CONST_IDENTITY_EQUALS_TABLE_TAG`]),
/// or `None` (switch off, no resolver, or the VM keeps no table). Stable for
/// the VM's life: the compiled test bakes it.
pub(crate) fn identity_equals_class_table(helpers: &crate::DirectHelperTable) -> Option<u64> {
    if !object_equals_identity_arm_enabled() {
        return None;
    }
    let addr = vh_const_ask(
        helpers,
        vh_const_query(WELL_KNOWN_VH_CONST_IDENTITY_EQUALS_TABLE_TAG, 0, 0),
    );
    u64::try_from(addr).ok().filter(|&a| a != 0)
}

/// `CRATONVM_JIT_IR_CURRENT_THREAD_PREFIX` (nio21 wave 1, lane L5, **default
/// ON**, `=0` keeps every optimizing-tier `Thread.currentThread()` a call):
/// the optimizing tier's twin of the single-pass current-thread prefix
/// (`Lowerer::try_emit_ir_current_thread_prefix`). Only effective where the
/// VM published the per-VM thread word (`--compatible`; the word is `0` under
/// `--real-jdk` and under `CRATONVM_JIT_SP_CURRENT_THREAD_PREFIX=0`). Read per
/// site compile, so it adds no process global.
pub(crate) fn ir_current_thread_prefix_enabled() -> bool {
    true
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ByteArrayViewOp {
    pub width: u8,
    pub is_put: bool,
    pub signed: bool,
}

pub fn byte_array_view_site_shape_parts(
    class: &str,
    name: &str,
    desc: &str,
) -> Option<ByteArrayViewOp> {
    if class != "jdk/internal/util/ByteArray"
    {
        return None;
    }
    match (name, desc) {
        ("getShort", "([BI)S") => Some(ByteArrayViewOp {
            width: 2,
            is_put: false,
            signed: true,
        }),
        ("getUnsignedShort", "([BI)I") | ("getChar", "([BI)C") => Some(ByteArrayViewOp {
            width: 2,
            is_put: false,
            signed: false,
        }),
        ("getInt", "([BI)I") => Some(ByteArrayViewOp {
            width: 4,
            is_put: false,
            signed: true,
        }),
        ("getLong", "([BI)J") => Some(ByteArrayViewOp {
            width: 8,
            is_put: false,
            signed: true,
        }),
        ("setShort", "([BIS)V") | ("setUnsignedShort", "([BII)V") | ("setChar", "([BIC)V") => {
            Some(ByteArrayViewOp {
                width: 2,
                is_put: true,
                signed: false,
            })
        }
        ("setInt", "([BII)V") => Some(ByteArrayViewOp {
            width: 4,
            is_put: true,
            signed: false,
        }),
        ("setLong", "([BIJ)V") => Some(ByteArrayViewOp {
            width: 8,
            is_put: true,
            signed: false,
        }),
        _ => None,
    }
}

pub(crate) fn ir_spliced_prefix_counts_bound(t: &crate::InlineInvokeTarget) -> bool {
    t.invoke_kind == 3
        && t.direct_entry.is_none()
        && crate::ir_thin_prefix_enabled()
        && (byte_array_view_site_shape_parts(&t.class_name, &t.method_name, &t.descriptor)
            .is_some()
            || (t.class_name == "java/lang/Thread"
                && t.descriptor == "()Ljava/lang/Thread;"
                && ((t.method_name == "currentThread" )
                    || (t.method_name == "currentCarrierThread"
                        && crate::direct_helpers::carrier_thread_direct_enabled()))))
}

impl<'a> Lowerer<'a> {
    pub(in crate::ir_lower) fn try_emit_ir_byte_array_view_prefix(&mut self, id: NodeId, info_ptr: usize) {
        if info_ptr == 0 || !crate::ir_thin_prefix_enabled() {
            return;
        }
        // SAFETY: the planner keeps each nonzero call row alive through lowering.
        let info: &JitInvokeInfo = unsafe { &*(info_ptr as *const JitInvokeInfo) };
        if info.invoke_kind != 3 {
            return;
        }
        let Some(op) =
            byte_array_view_site_shape_parts(info.class_name, info.method_name, info.descriptor)
        else {
            return;
        };
        let graph = self.graph;
        let Some(node) = graph.nodes.get(id as usize) else {
            return;
        };
        let want_inputs = if op.is_put { 5 } else { 4 };
        let want_ty = if op.is_put {
            IrType::Void
        } else if op.width == 8 {
            IrType::Long
        } else {
            IrType::Int
        };
        if node.inputs.len() != want_inputs || node.ty != want_ty {
            return;
        }
        if node.inputs[2..].iter().any(|&i| self.carry_names(i)) {
            return;
        }
        let slot = if op.is_put {
            None
        } else {
            if self.carry_of.get(id as usize).copied().flatten().is_some()
                || self.home_dropped.get(id as usize).copied().unwrap_or(false)
            {
                return;
            }
            match self.planned_slot_off(id) {
                Ok(slot) => Some(slot),
                Err(_) => return,
            }
        };
        let arr = node.inputs[2];
        let off = node.inputs[3];
        let val = if op.is_put {
            Some(node.inputs[4])
        } else {
            None
        };
        self.note_rcx_written();
        let mut decline: Vec<usize> = Vec::with_capacity(4);
        // RAX = byte[] array; null declines to the call (throws NPE).
        self.gp_load_value(RAX, arr);
        self.buf.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
        decline.push(self.emit_jcc_rel32(0x84)); // JZ decline
        // ECX = offset (zero-extended into RCX); negative declines (throws AIOOBE).
        self.gp_load_value(RCX, off);
        self.buf.emit(&[0x89, 0xC9]); // MOV ECX, ECX
        self.buf.emit(&[0x85, 0xC9]); // TEST ECX, ECX
        decline.push(self.emit_jcc_rel32(0x88)); // JS decline

        // EDX = array.length; require array.length >= width && offset <= array.length - width.
        const ARRAY_LENGTH_DISP: u8 =
            crate::x64::disp8_const(cratonvm_types::ARRAY_LENGTH_OFFSET as i64) as u8;
        const ARRAY_DATA_DISP: u8 =
            crate::x64::disp8_const(cratonvm_types::HEADER_SIZE as i64) as u8;
        self.buf.emit(&[0x8B, 0x50, ARRAY_LENGTH_DISP]); // MOV EDX, [RAX+8]
        self.buf.emit(&[0x83, 0xEA, op.width]); // SUB EDX, width
        decline.push(self.emit_jcc_rel32(0x82)); // JB decline
        self.buf.emit(&[0x39, 0xD1]); // CMP ECX, EDX
        decline.push(self.emit_jcc_rel32(0x87)); // JA decline
        let hdr = ARRAY_DATA_DISP;
        if let Some(val_node) = val {
            self.gp_load_value(RDX, val_node);
            match op.width {
                2 => {
                    self.buf.emit(&[0x66, 0xC1, 0xC2, 0x08]); // ROL DX, 8
                    self.buf.emit(&[0x66, 0x89, 0x54, 0x08, hdr]); // MOV [RAX+RCX+hdr], DX
                }
                4 => {
                    self.buf.emit(&[0x0F, 0xCA]); // BSWAP EDX
                    self.buf.emit(&[0x89, 0x54, 0x08, hdr]); // MOV [RAX+RCX+hdr], EDX
                }
                _ => {
                    self.buf.emit(&[0x48, 0x0F, 0xCA]); // BSWAP RDX
                    self.buf.emit(&[0x48, 0x89, 0x54, 0x08, hdr]); // MOV [RAX+RCX+hdr], RDX
                }
            }
        } else {
            match op.width {
                2 => {
                    self.buf.emit(&[0x0F, 0xB7, 0x44, 0x08, hdr]); // MOVZX EAX, WORD [RAX+RCX+hdr]
                    self.buf.emit(&[0x66, 0xC1, 0xC0, 0x08]); // ROL AX, 8
                    if op.signed {
                        self.buf.emit(&[0x48, 0x0F, 0xBF, 0xC0]); // MOVSX RAX, AX
                    } else {
                        self.buf.emit(&[0x0F, 0xB7, 0xC0]); // MOVZX EAX, AX
                    }
                }
                4 => {
                    self.buf.emit(&[0x8B, 0x44, 0x08, hdr]); // MOV EAX, [RAX+RCX+hdr]
                    self.buf.emit(&[0x0F, 0xC8]); // BSWAP EAX
                    self.buf.emit(&[0x48, 0x63, 0xC0]); // MOVSXD RAX, EAX
                }
                _ => {
                    self.buf.emit(&[0x48, 0x8B, 0x44, 0x08, hdr]); // MOV RAX, [RAX+RCX+hdr]
                    self.buf.emit(&[0x48, 0x0F, 0xC8]); // BSWAP RAX
                }
            }
            if let Some(reg) = self.assigned_gpr(id) {
                self.emit_mov_reg_reg64(reg, RAX);
            }
            if let Some(slot) = slot {
                let mut bytes = FrameAccess::new();
                enc_frame_store(RAX, slot, &mut bytes);
                self.buf.emit(bytes.as_slice());
            }
        }
        let hit = self.emit_jmp_rel32();
        for p in decline {
            self.patch_rel32_to_here(p);
        }
        self.call_prefix_join = Some(hit);
    }
}

/// `java/lang/Class`'s id in this compile's VM ([`WELL_KNOWN_CLASS_CLASS`]),
/// or `None` (no resolver, no VM, not answerable without blocking). Class id
/// 0 (`java/lang/Object`) is never an answer.
pub(crate) fn ir_class_class_id(helpers: &crate::DirectHelperTable) -> Option<u32> {
    let raw = helpers.well_known_class_id;
    let ctx = helpers.static_base_resolver_ctx;
    if raw == 0 || ctx == 0 {
        return None;
    }
    // SAFETY: as in `ir_integer_cache_high`.
    let f: crate::direct_helpers::DirectHelperFnWellKnownClassId =
        unsafe { std::mem::transmute(raw) };
    // Cast: `usize` context to the C ABI's i64 parameter.
    //
    // SAFETY: as in `ir_integer_cache_high`.
    let id = unsafe { f(ctx as i64, WELL_KNOWN_CLASS_CLASS) };
    u32::try_from(id).ok().filter(|&id| id != 0)
}

/// Is the `Op::Call` site `(class, name, desc, invoke_kind)` one whose answer
/// is the receiver's (argument's) IDENTITY hash for the receivers the
/// optimizing tier's identity-hash prefix tests for
/// ([`Lowerer::try_emit_ir_identity_hash_prefix`])? `Some(true)` for
/// `System.identityHashCode(Object)` (static; every non-null object),
/// `Some(false)` for `hashCode()I` declared on `java/lang/Class` (a final class
/// that does not override `Object.hashCode`, so only an exact `Class` receiver
/// is answered), `None` for anything else. An `Object`-declared `hashCode()`
/// site is the box-hash prefix's, which carries its own `Class` arm.
pub(crate) fn ir_identity_hash_site(
    class: &str,
    name: &str,
    desc: &str,
    invoke_kind: u8,
) -> Option<bool> {
    match (class, name, desc) {
        ("java/lang/System", "identityHashCode", "(Ljava/lang/Object;)I") if invoke_kind == 3 => {
            Some(true)
        }
        ("java/lang/Class", "hashCode", "()I") if matches!(invoke_kind, 0 | 2) => Some(false),
        _ => None,
    }
}

/// The inline identity-hash READ (nio20 wave 5, lane L2), as bytes. Entry: RAX
/// = a non-null object. Exit by fall-through: RAX = the identity hash the
/// header already holds, sign-extended (the IR's int convention). Every other
/// case takes one of the returned rel32 operands (left zero for the caller to
/// patch) to the call, which mints, consults the monitor, or answers exactly as
/// before. Clobbers RCX, RDX and the flags.
///
/// It decodes the header exactly as `ObjectHeader::identity_hash` /
/// `installed_identity_hash` (`types/src/heap_types.rs`) do, and the executed
/// test `nio20w5_identity_hash_inline_tests` pins the two together:
///
/// * a FORWARDED mark declines (the second word is the forwarding target);
/// * a long header (array, legacy instance: kind bits non-zero, or no
///   `GC_FLAG_COMPACT`) answers its aux word at `IDENTITY_HASH_OFFSET` in any
///   lock state, and `0` (never hashed) declines;
/// * a compact instance answers only from a NEUTRAL (or, below, a hash-carrying
///   INFLATED) mark with non-zero hash
///   bits -- the 14 payload bits plus the 6 `MARK_SHORT_HASH_HI` bits, mixed
///   with the class id's 11 bits (`short_hash_value`); a thin-locked or
///   inflated compact instance keeps its hash in the monitor and declines, as
///   does an unhashed one. JIT round 21 lane hashlock: an INFLATED compact
///   word that carries its hash bits (`ObjectHeader::inflated_hash_bits`)
///   answers them exactly as a NEUTRAL one; a thin-locked one still declines.
///
/// ```text
///   MOV  EDX, [RAX+MARK]
///   MOV  ECX, EDX ; AND ECX, 3 ; CMP ECX, FORWARDED ; JE decline
///   TEST EDX, KIND<<16 ; JNZ long
///   TEST EDX, COMPACT<<24 ; JZ long
///   TEST CL, 1 ; JNZ decline                 ; thin-locked
///   MOV  ECX, EDX ; SHR ECX, 2 ; AND ECX, 3FFFh
///   SHR  EDX, 18 ; AND EDX, 3Fh ; SHL EDX, 14 ; OR ECX, EDX ; JZ decline
///   MOV  EDX, [RAX] ; IMUL EDX, EDX, 9E3779B9h ; SHR EDX, 21
///   AND  EDX, 7FFh ; SHL EDX, 20 ; OR ECX, EDX ; AND ECX, 7FFFFFFFh
///   JMP  have
/// long:
///   MOV  ECX, [RAX+12] ; TEST ECX, ECX ; JZ decline
/// have:
///   MOVSXD RAX, ECX
/// ```
pub(crate) fn identity_hash_read_bytes() -> (Vec<u8>, Vec<usize>) {
    use cratonvm_types::{
        GC_FLAGS_BYTE_OFFSET, GC_FLAG_COMPACT, IDENTITY_HASH_OFFSET, KIND_TAGS_BYTE_OFFSET,
        KIND_TAG_BYTE_MASK, MARK_FORWARDED, MARK_HASH_MASK, MARK_HASH_SHIFT,
        MARK_SHORT_HASH_HI_MASK, MARK_SHORT_HASH_HI_SHIFT, MARK_STATE_MASK, MARK_WORD_OFFSET,
        SHORT_HASH_BITS,
    };
    // Compile-time pins of the shape the bytes below assume.
    const _: () = assert!(MARK_WORD_OFFSET < 0x80 && IDENTITY_HASH_OFFSET < 0x80);
    const _: () = assert!(KIND_TAGS_BYTE_OFFSET >= MARK_WORD_OFFSET);
    const _: () = assert!(GC_FLAGS_BYTE_OFFSET >= MARK_WORD_OFFSET);
    const _: () = assert!(MARK_STATE_MASK < 0x80 && MARK_FORWARDED < 0x80);
    const _: () = assert!(MARK_SHORT_HASH_HI_SHIFT < 32 && MARK_HASH_SHIFT < 32);
    let kind_bits: u32 =
        (KIND_TAG_BYTE_MASK as u32) << ((KIND_TAGS_BYTE_OFFSET - MARK_WORD_OFFSET) * 8);
    let compact_bit: u32 =
        (GC_FLAG_COMPACT as u32) << ((GC_FLAGS_BYTE_OFFSET - MARK_WORD_OFFSET) * 8);
    let low_mask: u32 = MARK_HASH_MASK >> MARK_HASH_SHIFT; // 0x3FFF
    let low_width: u32 = low_mask.count_ones(); // 14
    let hi_mask: u32 = MARK_SHORT_HASH_HI_MASK >> MARK_SHORT_HASH_HI_SHIFT; // 0x3F
                                                                            // Cast: the shift counts are < 32 (asserted above), x86 imm8.
    let (hash_shift, hi_shift, low_width8, bits8) = (
        MARK_HASH_SHIFT as u8,
        MARK_SHORT_HASH_HI_SHIFT as u8,
        low_width as u8,
        SHORT_HASH_BITS as u8,
    );

    let mut b: Vec<u8> = Vec::with_capacity(112);
    let mut decline: Vec<usize> = Vec::new();
    fn jcc(b: &mut Vec<u8>, cc: u8) -> usize {
        b.extend_from_slice(&[0x0F, cc]);
        let at = b.len();
        b.extend_from_slice(&[0, 0, 0, 0]);
        at
    }
    fn patch_here(b: &mut [u8], at: usize) {
        // Cast: offsets within a ~100-byte vector.
        let rel = b.len() as i32 - (at as i32 + 4);
        b[at..at + 4].copy_from_slice(&rel.to_le_bytes());
    }
    // MOV EDX, [RAX + MARK]
    b.extend_from_slice(&[0x8B, 0x50, MARK_WORD_OFFSET as u8]); // Cast: disp8, asserted
                                                                // MOV ECX, EDX ; AND ECX, STATE ; CMP ECX, FORWARDED ; JE decline
    b.extend_from_slice(&[0x89, 0xD1]);
    b.extend_from_slice(&[0x83, 0xE1, MARK_STATE_MASK as u8]); // Cast: imm8, asserted
    b.extend_from_slice(&[0x83, 0xF9, MARK_FORWARDED as u8]); // Cast: imm8, asserted
    decline.push(jcc(&mut b, 0x84));
    // TEST EDX, kind bits ; JNZ long  (an array, or any non-plain kind)
    b.extend_from_slice(&[0xF7, 0xC2]);
    b.extend_from_slice(&kind_bits.to_le_bytes());
    let long_kind = jcc(&mut b, 0x85);
    // TEST EDX, compact bit ; JZ long  (a legacy instance)
    b.extend_from_slice(&[0xF7, 0xC2]);
    b.extend_from_slice(&compact_bit.to_le_bytes());
    let long_legacy = jcc(&mut b, 0x84);
    // Compact instance. TEST CL, 1 ; JNZ decline (thin-locked: the payload is
    // an owner, not a hash; FORWARDED left above). NEUTRAL and, since JIT
    // round 21 lane hashlock (`CRATONVM_INFLATED_MARK_HASH`), INFLATED go on:
    // an INFLATED compact word carries the same 20 hash bits in the same
    // places when the monitor recorded them there
    // (`ObjectHeader::inflated_hash_bits`), and none otherwise (-> JZ below).
    const _: () = assert!(cratonvm_types::MARK_THIN_LOCKED == 0b01);
    const _: () = assert!(cratonvm_types::MARK_INFLATED == 0b10);
    b.extend_from_slice(&[0xF6, 0xC1, 0x01]);
    decline.push(jcc(&mut b, 0x85));
    // ECX = (mark >> 2) & 0x3FFF
    b.extend_from_slice(&[0x89, 0xD1, 0xC1, 0xE9, hash_shift]);
    b.extend_from_slice(&[0x81, 0xE1]);
    b.extend_from_slice(&low_mask.to_le_bytes());
    // EDX = ((mark >> 18) & 0x3F) << 14
    b.extend_from_slice(&[0xC1, 0xEA, hi_shift]);
    b.extend_from_slice(&[0x81, 0xE2]);
    b.extend_from_slice(&hi_mask.to_le_bytes());
    b.extend_from_slice(&[0xC1, 0xE2, low_width8]);
    // OR ECX, EDX ; JZ decline (no hash installed)
    b.extend_from_slice(&[0x09, 0xD1]);
    decline.push(jcc(&mut b, 0x84));
    // EDX = ((class_id * 0x9E3779B9) >> 21) & 0x7FF, << 20
    b.extend_from_slice(&[0x8B, 0x10]); // MOV EDX, [RAX]
    b.extend_from_slice(&[0x69, 0xD2]); // IMUL EDX, EDX, imm32
    b.extend_from_slice(&0x9E37_79B9u32.to_le_bytes());
    b.extend_from_slice(&[0xC1, 0xEA, 21]); // SHR EDX, 21
    b.extend_from_slice(&[0x81, 0xE2]);
    b.extend_from_slice(&0x7FFu32.to_le_bytes()); // AND EDX, 0x7FF
    b.extend_from_slice(&[0xC1, 0xE2, bits8]); // SHL EDX, 20
                                               // OR ECX, EDX ; AND ECX, i32::MAX
    b.extend_from_slice(&[0x09, 0xD1, 0x81, 0xE1]);
    b.extend_from_slice(&0x7FFF_FFFFu32.to_le_bytes());
    // JMP have
    b.push(0xE9);
    let to_have = b.len();
    b.extend_from_slice(&[0, 0, 0, 0]);
    // long: MOV ECX, [RAX + 12] ; TEST ECX, ECX ; JZ decline
    patch_here(&mut b, long_kind);
    patch_here(&mut b, long_legacy);
    b.extend_from_slice(&[0x8B, 0x48, IDENTITY_HASH_OFFSET as u8]); // Cast: disp8, asserted
    b.extend_from_slice(&[0x85, 0xC9]);
    decline.push(jcc(&mut b, 0x84));
    // have: MOVSXD RAX, ECX
    patch_here(&mut b, to_have);
    b.extend_from_slice(&[0x48, 0x63, 0xC1]);
    (b, decline)
}

/// The inline cached-range `Long.valueOf` read (JIT round 16 wave 7, lane
/// callbind, CB16-3), as bytes: [`integer_cache_read_bytes`] at 64 bits over
/// the fixed 256-slot table. Entry: RCX = the `long` value. Exit by
/// fall-through: RAX = the canonical box. Clobbers RAX, R10 and the flags; RCX
/// is unchanged.
///
/// ```text
/// LEA  RAX, [RCX + 128]        ; slot index
/// CMP  RAX, 256
/// JAE  outside                 ; unsigned: below -128 (and a wrap near
///                              ; i64::MAX) lands past 256
/// MOV  R10, base
/// MOV  RAX, [R10 + RAX*8]      ; the slot `long_cache_peek` loads
/// TEST RAX, RAX
/// JZ   empty                   ; not boxed yet: the call fills it
/// ```
///
/// Returns the bytes and the offsets, within them, of the two rel32 operands
/// (`outside`, `empty`), both left zero for the caller to patch.
pub(in crate::ir_lower) fn long_cache_read_bytes(base: u64) -> (Vec<u8>, usize, usize) {
    let mut b = Vec::with_capacity(40);
    b.extend_from_slice(&[0x48, 0x8D, 0x81, 0x80, 0x00, 0x00, 0x00]); // LEA RAX,[RCX+80h]
    b.extend_from_slice(&[0x48, 0x3D]); // CMP RAX, imm32
    b.extend_from_slice(&256u32.to_le_bytes());
    b.extend_from_slice(&[0x0F, 0x83]); // JAE rel32
    let outside = b.len();
    b.extend_from_slice(&[0, 0, 0, 0]);
    b.extend_from_slice(&[0x49, 0xBA]); // MOV R10, imm64
    b.extend_from_slice(&base.to_le_bytes());
    b.extend_from_slice(&[0x49, 0x8B, 0x04, 0xC2]); // MOV RAX,[R10+RAX*8]
    b.extend_from_slice(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
    b.extend_from_slice(&[0x0F, 0x84]); // JZ rel32
    let empty = b.len();
    b.extend_from_slice(&[0, 0, 0, 0]);
    (b, outside, empty)
}

/// `java/lang/Long`'s `ClassId` in this compile's VM, or `None` when it cannot
/// be shown (no resolver, no VM context, a name absent or defined more than
/// once, a class manager the VM could not read without waiting). Class id 0 is
/// never an answer. The `Long` twin of `DirectHelperTable::integer_class_id`.
pub(crate) fn ir_long_class_id(helpers: &crate::DirectHelperTable) -> Option<u32> {
    let raw = helpers.well_known_class_id;
    let ctx = helpers.static_base_resolver_ctx;
    if raw == 0 || ctx == 0 {
        return None;
    }
    // SAFETY: as in `ir_integer_cache_high`.
    let f: crate::direct_helpers::DirectHelperFnWellKnownClassId =
        unsafe { std::mem::transmute(raw) };
    // Cast: `usize` context to the C ABI's i64 parameter.
    //
    // SAFETY: as in `ir_integer_cache_high`.
    let id = unsafe { f(ctx as i64, WELL_KNOWN_CLASS_LONG) };
    u32::try_from(id).ok().filter(|&id| id != 0)
}





/// How many optimizing-tier inline-cache call sites were compiled WITH the
/// frame-record republish.
///
/// Bumped at COMPILE time, not per call, so it costs a workload nothing. It is
/// the engagement counter for that repair: a claim that the republish did or
/// did not move a workload is worth nothing while this reads zero, because a
/// zero means the optimizing tier never lowered an inline cache in that run and
/// the arms differed only by noise. Printed on the `[jitroots]` line beside the
/// verdict it is supposed to explain.
pub static IC_FRAME_REPUBLISH_SITES: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

/// Read [`IC_FRAME_REPUBLISH_SITES`].
pub fn ic_frame_republish_sites() -> usize {
    IC_FRAME_REPUBLISH_SITES.load(std::sync::atomic::Ordering::Relaxed)
}


/// `CRATONVM_JIT_IR_SYNTHETIC_STORE_FIELD_ROW` (JIT round 23 wave 11b, lane
/// L5, CompletableFuture page; perf, **default ON**, `=0` kill). An
/// `Op::Store` at a [`ir::Graph::synthetic_field_access_pcs`] pc takes the
/// compact row its builder named for that NODE ([`ir::Graph::field_rows`]),
/// as the inline load arm already does (nio21 wave 20). Without it, such a
/// store always took the helper (`jit_putfield_object` for a reference).
///
/// The one producer today is the VH fresh-field `set*`
/// (`CRATONVM_JIT_IR_VH_CONST_FRESH_FIELD`, opt-in). On n23-w11a,
/// `CfLoop done` spent about half of all samples in that helper and its field
/// layout cache (`jit_putfield_object`, `record_object_ref_payload_slow`,
/// `ObjectHeader::num_slots`, `VersionCache`): 129 against 80 ns/op without
/// the VH switch. `R21CfChain cf-thenapply-done` went from 51-57 to 92-111.
///
/// Sound as the load arm's rule is: the row is the exact field's, from the
/// VM's layout (`vh_const_field_disp`), for exactly the node it names. The
/// emitters still pick the shape per OBJECT (compact cell or legacy 16-byte
/// cell, with its slot bound), guard the layout epoch, and keep every
/// barrier gate and the helper fallback. Read per store; no `static`.
pub(crate) fn ir_synthetic_store_field_row_enabled() -> bool {
    true
}


/// JIT round 25 (item 4 of the r24 regex-rows redesign,
/// `CRATONVM_JIT_IR_PROVEN_RECEIVER_FIELDS`): a receiver's compactness is
/// proven once, by a deopting guard, and the field accesses that proof covers
/// read and write with one `MOV`.


// JIT round 25, item 1b (`CRATONVM_JIT_IR_LS_CALLER_SAVED`): block-local values
// in caller-saved GPRs. See [`plan_caller_saved_residency`].


// ── JIT round 25 wave 6, lane W6-A: the fresh-frame clear as a store cover ──
//
// Page: `docs/feature-designs/real-frame-deopt.md`, "Wave 6
// (W6-A)".
//
// The fresh-frame clear (`Lowerer::emit_fresh_frame_clear_sse`) zeroes every
// word of the new frame its skip mask does not name. The mask is chosen for
// BYTES: each round-25 claim, the written / unrooted skips and the lazy block
// words take words out of it. The emitter then clears each maximal run of
// remaining words on its own (`emit_zero_frame_words` / `emit_zero_run`): a
// 32-byte store per four words, a 16- and an 8-byte store for each run's odd
// tail, and above `ZERO_FRAME_SSE_STRAIGHT_BYTES` a loop that retires seven
// instructions per 128 bytes. So a word left out of the clear saves an
// instruction only when it shortens a run past a store boundary, and a mask
// that splits one run into three (an interleaved claim) can cost MORE stores
// than the unsplit run. That can explain why the claims of waves 2-3 took
// bytes out of the clear without a measured instruction gain; the census line
// below says, per body, how many runs and instructions the mask costs.
//
// `CRATONVM_JIT_IR_FRESH_CLEAR_COVER` (default ON since j27 2026-10-28) emits the clear as a minimum
// set of straight-line stores instead ([`fresh_clear_cover_stores`]): from the
// top of the frame down, the first word that must be cleared starts one store
// of the widest width (32 bytes with the AVX form, 16 without), which covers
// the next three (one) words too, skipped or not; the store is narrowed to the
// fewest words that still cover every required word of its window, and shifted
// down to end at the frame's deep end when it would overhang it. Greedy
// placement of fixed-length windows from one end is the minimum number of
// windows covering a set of points, and every run-wise clear is one such
// cover, so this never retires more instructions than the run-wise form; it
// retires fewer whenever the mask is fragmented or a run is long enough to
// loop. A frame needing more than [`FRESH_CLEAR_COVER_MAX_STORES`] stores
// keeps the run-wise clear (the loop's smaller code).
//
// # Soundness
//
// The cover writes zero to every word the run-wise clear writes, and possibly
// to words the mask left out. Zeroing a word of the fresh frame is always
// correct at this point: the clear runs right after `SUB RSP` (the prologue,
// `emit_prologue`, and every OSR entry stub), before the saves, the parameter
// stores and every other write of the activation, so no word of `[rsp, rbp)`
// holds anything this activation, its caller or the VM reads -- the baseline
// argument of the clear itself (the lone-word re-inclusion of the skip mask
// already relies on it). Every skip class is a word that need NOT be zeroed;
// none is a word that must keep its old value. Every store lies inside `[rbp -
// frame_size, rbp)`, and the stores run in descending address order, as the
// run-wise clear's do (a stack that grows through a guard page meets each new
// page from above; the skipped spans can only shrink). Scratch: XMM5 (YMM5)
// and the flags only -- no R10/R11 at all. The returned skip mask is unchanged:
// a skipped word the cover happened to zero keeps its later per-word zero store
// (`fresh_clear_skipped`), which stores zero over zero.
//
// The census (`CRATONVM_DBG_JITC=1`, prologue only, printed with the switch
// off too, for the A/B's denominator): `[ir-frame-clear] method=.. frame=..
// words=.. cleared=.. by-region: <region>=<words> ..; run-wise runs=.. instrs=..;
// cover stores=.. instrs=.. (on|off)` (or `cover refused ..`). `instrs` counts
// the executed instructions of the clear per entry, loop iterations included.


/// The most stores [`fresh_clear_cover_stores`] emits straight-line: 1280
/// bytes of a dense frame with 32-byte stores, 640 with 16-byte ones (at most
/// about 360 bytes of code). A frame that needs more keeps the run-wise clear.



/// The cover's executed instructions for `stores` (one per store, plus the
/// `PXOR` / `VPXOR` and, with `avx`, the `VZEROUPPER`); 0 for no store. Pure.

/// The regions [`Lowerer::fresh_clear_word_region`] names, in print order.

/// The fresh-frame clear of one body, word by word (the `[ir-frame-clear]`
/// census line, section above).



// ── JIT round 25 wave 6 (W6-D): leaf entries ─────────────────────────────
//
// Page: `docs/feature-designs/jit-ir-optimizations.md`,
// section "Wave 6 (W6-D)", which carries the instruction-by-instruction
// inventory of an optimizing-tier prologue and epilogue.
//
// Two switches (opt-in when written, default ON since j27 2026-10-28, `=0`
// kills), read per compile (never cached):
//
// * `CRATONVM_JIT_IR_LEAF_ENTRY_POLL_ELIDE=1`: no method-entry poll in a body
//   whose block CFG has no cycle and whose graph has no Java-dispatching op
//   (`Op::Call`, `Op::IndyBridge`, `Op::LambdaIntToDouble`). The entry poll is
//   a time-to-safepoint device only (no mode exit, no forced exit site); such a
//   body runs a bounded number of its own instructions, every helper it calls
//   either returns or parks in a safepoint-safe state, and any Java code a
//   helper runs (a `<clinit>`, a deopt's interpreter) polls on its own. HotSpot
//   C2 emits no entry poll at all.
// * `CRATONVM_JIT_IR_LEAF_FRAME_ELIDE=1`: a LEAF FRAME -- the shape above,
//   every graph op in the pure allow-list `op_is_leaf_frame_pure` (no load,
//   store, allocation, division, guard, type check or call of any kind), and no
//   self-lock -- additionally skips the entry poll, the fresh-frame clear, the
//   unset-local and bookkeeping-word zeroing, the safepoint-id zero, the frame
//   record (innermost-RBP / compile-id mirrors), the thread fetch, the
//   reference-home zeroing and every exit's shadow-top restore. Sound because
//   nothing ever reads those words while such a frame is live: it has no GC
//   point (no collection runs on this thread inside it), no call (no stack walk
//   starts below it, no callee reads the mirrors), and no exit (no deopt, no
//   exception pad). The mirrors keep naming the caller, a live frame, which is
//   what they name after the return anyway. A cross-thread take-over freezing
//   the thread inside the leaf scans its stack conservatively and marks the
//   cycle non-moving, as for any frozen peer. The prediction is checked after
//   the body (`Lowerer::verify_leaf_entry`): a poll, a safepoint map, a shadow
//   push, a deopt / call-exception / rethrow exit, an exception pad or a frame
//   block refuses the attempt, and the ladder lowers once more without it.
//
// `CRATONVM_DBG_JITC=1` prints one `[cratonvm-jitc] ir-leaf-entry` line per
// lowering attempt, with both switches off too (the A/B's denominator).




/// What [`plan_leaf_entry`] decided for one lowering attempt.

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(in crate::ir_lower) struct LeafEntryPlan {
    /// The block CFG has no cycle (reachable or not).
    pub(in crate::ir_lower) loop_free: bool,
    /// Graph nodes that dispatch to Java (`op_dispatches_java`).
    pub(in crate::ir_lower) java_calls: usize,
    /// Graph nodes outside the leaf-frame allow-list (`op_is_leaf_frame_pure`).
    pub(in crate::ir_lower) impure_ops: usize,
    /// The method-entry poll is not emitted.
    pub(in crate::ir_lower) elide_entry_poll: bool,
    /// The prologue's GC-only parts and the exits' shadow-top restores are
    /// not emitted; verified after the body.
    pub(in crate::ir_lower) leaf_frame: bool,
}

/// Per-attempt W6-D state on the `Lowerer`.
#[derive(Clone, Copy, Debug, Default)]
pub(in crate::ir_lower) struct LeafEntryState {
    pub(in crate::ir_lower) plan: LeafEntryPlan,
    /// Safepoint polls emitted (any kind); a leaf frame admits none.
    pub(in crate::ir_lower) polls_seen: usize,
    /// Deopt-stub plus call-exception patches, snapshotted before the stubs
    /// consume them.
    pub(in crate::ir_lower) stub_exits: usize,
    /// Prologue sections a leaf frame skipped (census only).
    pub(in crate::ir_lower) prologue_parts_skipped: usize,
}

/// Does the directed graph `successors` (block -> successor blocks) contain a
/// cycle? An out-of-range successor answers `true` (fail closed). Iterative
/// three-colour DFS from every block, so an unreachable cycle counts too.
pub(in crate::ir_lower) fn block_cfg_has_cycle(successors: &[Vec<usize>]) -> bool {
    let n = successors.len();
    // 0 = unvisited, 1 = on the DFS stack, 2 = finished.
    let mut state = vec![0u8; n];
    for root in 0..n {
        if state.get(root).copied() != Some(0) {
            continue;
        }
        state[root] = 1;
        let mut stack: Vec<(usize, usize)> = vec![(root, 0)];
        while let Some(&(b, i)) = stack.last() {
            let next = successors.get(b).and_then(|s| s.get(i)).copied();
            match next {
                Some(s) => {
                    if let Some(top) = stack.last_mut() {
                        top.1 = i + 1;
                    }
                    match state.get(s).copied() {
                        None | Some(1) => return true,
                        Some(0) => {
                            state[s] = 1;
                            stack.push((s, 0));
                        }
                        _ => {}
                    }
                }
                None => {
                    if let Some(st) = state.get_mut(b) {
                        *st = 2;
                    }
                    stack.pop();
                }
            }
        }
    }
    false
}

/// Ops that run Java code through this body's own call sequence. A body with
/// none of them (and no loop) cannot keep a thread away from a poll for
/// longer than its own straight-line code plus helper calls.
pub(in crate::ir_lower) fn op_dispatches_java(op: &Op) -> bool {
    matches!(
        op,
        Op::Call { .. } | Op::IndyBridge { .. } | Op::LambdaIntToDouble
    )
}

/// The leaf-frame allow-list: ops whose lowering emits no call of any kind
/// (helper, stub or Java), no GC point, no deopt exit and no exception pad --
/// control, constants, parameters, phis, integer / FP add-sub-mul-negate,
/// bitwise ops and shifts, compares, and the integer narrowing / widening
/// conversions. Deliberately excluded although some lower inline today:
/// division and remainder (`ArithmeticException`), every load, store and
/// array op (null / bounds checks, barriers, helper slow paths), the FP-to-int
/// conversions (NaN fix-ups), switches, type checks, guards, allocations,
/// monitors and `KeepAlive`. Adding an op here needs its lowering audited for
/// calls and exits; `verify_leaf_entry` catches the recorded ones only.
pub(in crate::ir_lower) fn op_is_leaf_frame_pure(op: &Op) -> bool {
    matches!(
        op,
        Op::Start
            | Op::Return
            | Op::If
            | Op::Merge
            | Op::Region
            | Op::Proj(_)
            | Op::Const(_)
            | Op::ConstF(_)
            | Op::Param(_)
            | Op::Phi
            | Op::Add
            | Op::Sub
            | Op::Mul
            | Op::Neg
            | Op::And
            | Op::Or
            | Op::Xor
            | Op::Shl
            | Op::Shr
            | Op::UShr
            | Op::Cmp(_)
            | Op::LCmp
            | Op::FCmp { .. }
            | Op::I2L
            | Op::L2I
            | Op::I2B
            | Op::I2C
            | Op::I2S
            | Op::Dead
    )
}

/// The W6-D decision for one lowering attempt. `poll_switch` /
/// `frame_switch` are the two switches (the caller folds the attempt's own
/// admission into `frame_switch`); `self_locks`: the body takes its method
/// monitor itself.
pub(in crate::ir_lower) fn plan_leaf_entry(
    graph: &Graph,
    schedule: &Schedule,
    poll_switch: bool,
    frame_switch: bool,
    self_locks: bool,
) -> LeafEntryPlan {
    let successors: Vec<Vec<usize>> = schedule
        .blocks
        .iter()
        .map(|b| b.successors.clone())
        .collect();
    let loop_free = !block_cfg_has_cycle(&successors);
    let java_calls = graph
        .nodes
        .iter()
        .filter(|n| op_dispatches_java(&n.op))
        .count();
    let impure_ops = graph
        .nodes
        .iter()
        .filter(|n| !op_is_leaf_frame_pure(&n.op))
        .count();
    let leaf_shape = loop_free && java_calls == 0;
    let leaf_frame = frame_switch && leaf_shape && impure_ops == 0 && !self_locks;
    LeafEntryPlan {
        loop_free,
        java_calls,
        impure_ops,
        elide_entry_poll: leaf_frame || (poll_switch && leaf_shape),
        leaf_frame,
    }
}

pub(in crate::ir_lower) fn fresh_allocation_unfenced_volatile_stores(
    graph: &Graph,
    blocks: &[FenceBlockView<'_>],
    node_to_block: &[usize],
) -> std::collections::HashSet<NodeId> {
    let mut out = std::collections::HashSet::new();
    // Every scheduled node's (block, index); a terminator sits after the
    // block's data nodes.
    let mut pos: HashMap<NodeId, (usize, usize)> = HashMap::new();
    for (b, blk) in blocks.iter().enumerate() {
        for (i, &n) in blk.nodes.iter().enumerate() {
            pos.insert(n, (b, i));
        }
        if let Some(t) = blk.terminator {
            pos.insert(t, (b, blk.nodes.len()));
        }
    }
    // The candidate stores, by the allocation they write into.
    let mut by_alloc: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
    for (idx, n) in graph.nodes.iter().enumerate() {
        if !n.is_volatile_store() {
            continue;
        }
        let Some(base) = n.input_opt(2) else {
            continue;
        };
        if matches!(graph.node_opt(base).map(|b| &b.op), Some(Op::New { .. })) {
            by_alloc.entry(base).or_default().push(idx as NodeId);
        }
    }
    'alloc: for (alloc, stores) in by_alloc {
        let Some(&(alloc_block, _)) = pos.get(&alloc) else {
            continue;
        };
        // Where each escape starts: `(block, Some(i))` right after the node
        // at index `i`, `(block, None)` at the block's entry.
        let mut starts: Vec<(usize, Option<usize>)> = Vec::new();
        for (idx, n) in graph.nodes.iter().enumerate() {
            if matches!(n.op, Op::Dead) {
                continue;
            }
            let mut at = n
                .inputs
                .iter()
                .enumerate()
                .filter(|&(_, &i)| i == alloc)
                .map(|(k, _)| k)
                .peekable();
            if at.peek().is_none() {
                continue;
            }
            if matches!(n.op, Op::Load(_) | Op::Store(_)) && at.all(|k| k == 2) {
                continue;
            }
            if matches!(n.op, Op::Phi) {
                match node_to_block.get(idx) {
                    Some(&b) if b < blocks.len() => starts.push((b, None)),
                    _ => continue 'alloc,
                }
            } else {
                match pos.get(&(idx as NodeId)) {
                    Some(&(b, i)) => starts.push((b, Some(i))),
                    None => continue 'alloc,
                }
            }
        }
        for s in stores {
            let Some(&store_at) = pos.get(&s) else {
                continue;
            };
            if starts
                .iter()
                .all(|&start| !fence_escape_reaches(blocks, start, alloc_block, store_at))
            {
                out.insert(s);
            }
        }
    }
    out
}

/// Can control flow from `start` (an escape) to the store at
/// `(store_block, store_idx)` without entering `alloc_block` (which would
/// allocate a fresh object first)? An out-of-range successor answers `true`.
pub(in crate::ir_lower) fn fence_escape_reaches(
    blocks: &[FenceBlockView<'_>],
    start: (usize, Option<usize>),
    alloc_block: usize,
    (store_block, store_idx): (usize, usize),
) -> bool {
    let mut work: Vec<usize> = Vec::new();
    match start {
        (b, Some(i)) => {
            if b == store_block && store_idx > i {
                return true;
            }
            match blocks.get(b) {
                Some(blk) => work.extend(blk.successors.iter().copied()),
                None => return true,
            }
        }
        (b, None) => work.push(b),
    }
    let mut seen = vec![false; blocks.len()];
    while let Some(x) = work.pop() {
        let Some(blk) = blocks.get(x) else {
            return true;
        };
        if std::mem::replace(&mut seen[x], true) {
            continue;
        }
        if x == alloc_block {
            continue;
        }
        if x == store_block {
            return true;
        }
        work.extend(blk.successors.iter().copied());
    }
    false
}


impl<'a> Lowerer<'a> {
    pub(in crate::ir_lower) fn plan_leaf_entry(&mut self, _frame_allowed: bool) {
        let plan = plan_leaf_entry(
            self.graph,
            self.schedule,
            false,
            false,
            self.self_lock.is_some(),
        );
        self.leaf_entry = LeafEntryState {
            plan,
            ..LeafEntryState::default()
        };
    }

    /// W6-D: what `zero_ref_phi_slots` records without its stores -- every
    /// located `Ref` phi counts as defined. Nothing of a leaf frame reads the
    /// mark (it has no safepoint map); kept so the bookkeeping matches.
    pub(in crate::ir_lower) fn mark_ref_phi_slots_defined(&mut self) {
        let graph = self.graph;
        for (id, n) in graph.nodes.iter().enumerate() {
            if !matches!(n.op, Op::Phi) || n.ty != IrType::Ref {
                continue;
            }
            if self.node_slot.get(id).copied().flatten().is_none() {
                continue;
            }
            if let Some(d) = self.defined_nodes.get_mut(id) {
                *d = true;
            }
        }
    }

    /// W6-D: refuse an attempt whose leaf frame emitted anything a skipped
    /// prologue part exists for; print the census line.
    pub(in crate::ir_lower) fn verify_leaf_entry(&mut self) {
        let st = self.leaf_entry;
        let why: Option<&'static str> = if !st.plan.leaf_frame {
            None
        } else if st.polls_seen != 0 {
            Some("a safepoint poll")
        } else if !self.oop_maps.is_empty() {
            Some("a safepoint map")
        } else if self.shadow_pushed_any {
            Some("a shadow push")
        } else if st.stub_exits != 0 {
            Some("a deopt or call-exception exit")
        } else if !self.deopt_boxes.is_empty() {
            Some("a deoptimization point")
        } else if !self.rethrow_points.is_empty() {
            Some("a rethrow point")
        } else if self.call_throw_pads_emitted != 0 || self.implicit_throw_pads_emitted != 0 {
            Some("an exception pad")
        } else if !self.frame_block.is_empty() {
            Some("a frame block")
        } else {
            None
        };
        if crate::x64::dbg_jitc_enabled() {
            let frame = match (st.plan.leaf_frame, why) {
                (false, _) => "off".to_string(),
                (true, None) => "on".to_string(),
                (true, Some(w)) => format!("refused({w})"),
            };
            eprintln!(
                "[cratonvm-jitc] ir-leaf-entry: method={} loop_free={} java_calls={} \
                 impure_ops={} self_lock={} entry_poll={} leaf_frame={} prologue_parts_skipped={}",
                self.graph.scope_method_key,
                st.plan.loop_free,
                st.plan.java_calls,
                st.plan.impure_ops,
                self.self_lock.is_some(),
                if st.plan.elide_entry_poll {
                    "elided"
                } else {
                    "kept"
                },
                frame,
                st.prologue_parts_skipped,
            );
        }
        if let Some(why) = why {
            self.latch_bailout(Bailout::with_context(
                BailoutReason::Internal(
                    "ir_lower: a leaf frame (CRATONVM_JIT_IR_LEAF_FRAME_ELIDE) emitted a GC point, call or exit",
                ),
                why,
            ));
        }
    }
}


// JIT round 25 wave 7, lane W7-A (`CRATONVM_JIT_IR_REG_ALLOC`, opt-in): the
// first increment of the IR-tier register allocator. Design and order of work:
// `docs/feature-designs/jit-ir-optimizations.md`.
//
// An `int` / `long` value the residency plan gives a callee-saved register is
// kept there for its whole range, and its home word is written only where a
// reader of homes needs it: a call, allocation, monitor or safepointing helper
// that names it (`Lowerer::emit_inspection_spills`), a back-edge poll's slow
// path (`poll_slow_spills`), and nowhere else; a deopt pad or a post-call exit
// names its register (`Lowerer::site_location_of`), the OSR entry seeds its
// register (`emit_osr_seed`), and an exception handler that would read its
// home refuses the attempt, which is retried without the value. That engine is
// the item-1a / W6-B site naming; this increment turns it on as one switch and
// widens its population twice:
//
// * single-use `int` / `long` values the RAX carry cannot reach keep the
//   register the scan gave them (they were refused as "does not pay" because
//   write-through made a register cost a publish and save only a reload), under
//   the same reservation price as the crossblock admissions
//   ([`single_use_candidate`]);
// * a loop phi's coalesced back-edge source (`i + 1` in the phi's register) is
//   site-named: at its block's outgoing edge the register holds it by the
//   coalescing argument, so the fall-through poll's slow path stores it and its
//   mode exit names the register ([`Lowerer::reg_alloc_coalesced_live_at_edge`]).
//
// No new `static` or `thread_local!`: every read is per compile.
/// `CRATONVM_JIT_NO_ZERO_UNSET_LOCALS=1` (opt-in kill switch, unset = the
/// zeroing runs): stop zeroing the local slots a method did not receive as
/// parameters. Separate from [`prologue_zero_reserved_tail_disabled`] so the
/// two prologue changes stay independently attributable.
pub(in crate::ir_lower) fn prologue_zero_unset_locals_disabled() -> bool {
    // t1 wave 3 (lane Y): read per call, no process `OnceLock` latch.
    false
}

/// `CRATONVM_JIT_NO_ZERO_RESERVED_TAIL=1` (opt-in kill switch): stop zeroing
/// the two reserved bookkeeping words the prologue used to leave holding the
/// previous frame's stack (`shadow_savebase`, `phi_copy_scratch`).
///
/// Set, it restores the old behaviour, so the effect is one binary's A/B. This
/// is a codegen change on every prologue, which is exactly the kind that has to
/// ship with a switch.
pub(in crate::ir_lower) fn prologue_zero_reserved_tail_disabled() -> bool {
    // t1 wave 3 (lane Y): read per call, no process `OnceLock` latch.
    false
}

/// `CRATONVM_JIT_NO_IC_FRAME_REPUBLISH=1` (opt-in kill switch) — stop
/// republishing the innermost-frame mirror after an optimizing-tier
/// inline-cache hit: the bisect lever for the republish folded into
/// `emit_call_loaded_ic_entry`. With it set, one binary reproduces the
/// stale-mirror `ACTIVE_FRAME_MAP` refusals this fixed.
pub(in crate::ir_lower) fn ic_frame_republish_disabled() -> bool {
    // t1 wave 3 (lane Y): read per call, no process `OnceLock` latch.
    false
}

/// The cover (the section above): `(disp, width)` per store, `[rbp + disp]`
/// for `width` bytes (8, 16, or 32 only with `avx`), in emission order
/// (descending addresses). `skip` is the clear's mask (word `w` =
/// `[rbp - 8(w + 1)]`; empty means "none"): every word it does not name is
/// covered. Empty when nothing is to be cleared; `None` for an invalid frame
/// size or when more than [`FRESH_CLEAR_COVER_MAX_STORES`] stores are needed.
/// Pure.
impl FreshClearCensus {
    /// The `[ir-frame-clear]` line.
    pub(in crate::ir_lower) fn line(&self, method: &str, frame_size: i32) -> String {
        let regions: Vec<String> = FRESH_CLEAR_REGIONS
            .iter()
            .zip(self.by_region.iter())
            .filter(|(_, n)| **n > 0)
            .map(|(name, n)| format!("{name}={n}"))
            .collect();
        let cover = match self.cover {
            Some((stores, instrs)) => format!(
                "cover stores={stores} instrs={instrs} ({})",
                if self.cover_on { "on" } else { "off" }
            ),
            None => format!("cover refused (over {FRESH_CLEAR_COVER_MAX_STORES} stores)"),
        };
        format!(
            "[ir-frame-clear] method={method} frame={frame_size} words={} cleared={} \
             by-region: {}; run-wise runs={} instrs={}; {cover}",
            self.words,
            self.cleared,
            regions.join(" "),
            self.runwise.0,
            self.runwise.1,
        )
    }
}

pub(in crate::ir_lower) fn fresh_clear_cover_stores(frame_size: i32, skip: &[bool], avx: bool) -> Option<Vec<(i32, i32)>> {
    if frame_size <= 0 || frame_size % 8 != 0 {
        return None;
    }
    // Cast: a positive frame size over 8 (at most 32 KiB / 8).
    let words = (frame_size / 8) as usize;
    let required = |w: usize| !skip.get(w).copied().unwrap_or(false);
    // Store widths in words, narrowest first.
    let widths: &[usize] = if avx { &[1, 2, 4] } else { &[1, 2] };
    let widest = widths.last().copied().unwrap_or(1);
    let mut stores: Vec<(i32, i32)> = Vec::new();
    let mut w = 0usize;
    while w < words {
        if !required(w) {
            w += 1;
            continue;
        }
        // The required words a widest store from `w` would cover end at `last`.
        let window_end = (w + widest).min(words);
        let last = (w..window_end).rev().find(|&v| required(v)).unwrap_or(w);
        let need = last - w + 1;
        // The narrowest width that covers them and fits the frame: at `w`, or
        // shifted down so it ends at the frame's deep end (it still covers
        // `w ..= last`, both below `words`).
        let fitted = widths
            .iter()
            .copied()
            .find(|&k| k >= need && k <= words)
            .map(|k| {
                if w + k <= words {
                    (w, k)
                } else {
                    (words - k, k)
                }
            });
        // `None` only when the frame is narrower than every width that covers
        // the window (three words, all required, 32-byte stores): cover `w`
        // with the widest width that fits from `w`; the loop meets the rest.
        let (top, k) = fitted.unwrap_or_else(|| {
            let k = widths
                .iter()
                .copied()
                .filter(|&k| w + k <= words)
                .max()
                .unwrap_or(1);
            (w, k)
        });
        // Cast: word indices below 4096 (a frame is at most 32 KiB).
        let disp = -8 * ((top + k) as i32);
        // Cast: a width of at most four words.
        stores.push((disp, 8 * (k as i32)));
        if stores.len() > FRESH_CLEAR_COVER_MAX_STORES {
            return None;
        }
        w = top + k;
    }
    Some(stores)
}


pub(in crate::ir_lower) fn plan_register_residency(
    graph: &Graph,
    schedule: &Schedule,
    plan: &SlotPlan,
) -> CompileResult<Option<RegResidency>> {
    let mut live = crate::regalloc::build_live_model(graph, schedule);
    plan_register_residency_with(
        graph,
        schedule,
        plan,
        &mut live,
        r25_w7a_reg_alloc::effective(false),
        None,
    )
}


// ---------------------------------------------------------------------------
// JIT round 25 wave 7 (lane W7-C): the tail-call republish sink
// ---------------------------------------------------------------------------
//
// `CRATONVM_JIT_IR_TAIL_CALL_REPUBLISH_SINK` (default ON since j27 2026-10-28; page
// `docs/feature-designs/jit-ir-optimizations.md`,
// "Wave 7 (W7-C)"). A JIT->JIT call is followed on its hot path by the frame
// republish (`emit_post_call_frame_record`: the innermost-RBP and compile-id
// mirrors, two TLS stores), because the callee's prologue overwrote both. Its
// readers are Rust code this frame reaches while it is the innermost compiled
// frame: a helper, a GC point, a poll's slow path, a deopt or exception exit,
// a native entry (`push_entry_full`'s snapshot, the held-thread copy). A call
// whose continuation reaches the method's return through
// `op_is_leaf_frame_pure` ops only -- no GC point, no helper, no exit, no loop
// (so no back-edge poll) -- reaches none of them on its normal path, so its
// hot path skips the republish; every sentinel side of the site (the
// callee-deopt service, the exception exit, its pads) republishes before it
// runs. After this frame returns, the mirrors name a dead frame (the callee)
// until the caller republishes, exactly as they named this dead frame before:
// every caller republishes after its call (both tiers, every IR route, the
// shared stubs) or restores through the Rust dispatch bracket
// (`try_call_compiled_entry`, the chain pop).
//
// One cross-frame rule is not local: a direct self-call republishes only the
// RBP half (`emit_post_self_call_frame_record`), relying on every activation of
// this same compile restoring its own identity before a normal return. A sunk
// call would break that, so a compile with a direct self-call plans no sink.
// A self-locking body (its monitor exit at the return) plans none either.
//
// `CRATONVM_DBG_JITC=1` prints one `[cratonvm-jitc] ir-tail-republish` line
// per lowering attempt that has a call, with the switch off too.


/// Per-attempt W7-C state on the `Lowerer`.
#[derive(Clone, Debug, Default)]
pub(in crate::ir_lower) struct TailRepublishSink {
    /// Indexed by node id: the `Op::Call` may sink its republish.
    pub(in crate::ir_lower) admitted: Vec<bool>,
    /// The `Op::Call` being lowered sinks (set by its arm, cleared at every
    /// node's start and after the body).
    pub(in crate::ir_lower) cur: bool,
    /// Scheduled `Op::Call` nodes.
    pub(in crate::ir_lower) calls: usize,
    /// Of those, the ones whose continuation admits the sink (switch or not).
    pub(in crate::ir_lower) tail_calls: usize,
    /// Hot paths emitted without a republish.
    pub(in crate::ir_lower) sunk_sites: usize,
    /// The compile-level gate: `on`, `off`, or the refusal.
    pub(in crate::ir_lower) gate: &'static str,
}

impl TailRepublishSink {
    pub(in crate::ir_lower) fn admits(&self, id: NodeId) -> bool {
        self.admitted.get(id as usize).copied().unwrap_or(false)
    }
}

/// One scheduled block as [`tail_sink_calls`] reads it.
pub(in crate::ir_lower) struct TailSinkBlock<'a> {
    pub(in crate::ir_lower) ctrl: Option<NodeId>,
    pub(in crate::ir_lower) nodes: &'a [NodeId],
    pub(in crate::ir_lower) terminator: Option<NodeId>,
    pub(in crate::ir_lower) successors: &'a [usize],
}

/// Is `id` a node whose lowering reads nothing of the frame record (the
/// leaf-frame allow-list)? A missing node answers no.
pub(in crate::ir_lower) fn tail_sink_node_pure(graph: &Graph, id: NodeId) -> bool {
    graph
        .nodes
        .get(id as usize)
        .is_some_and(|n| op_is_leaf_frame_pure(&n.op))
}

/// Is every node of `block` (control, data, terminator) pure, and a block
/// with no successor one that returns?
pub(in crate::ir_lower) fn tail_sink_block_pure(graph: &Graph, block: &TailSinkBlock<'_>) -> bool {
    block.ctrl.is_none_or(|c| tail_sink_node_pure(graph, c))
        && block.nodes.iter().all(|&n| tail_sink_node_pure(graph, n))
        && tail_sink_block_exit_ok(graph, block)
}

/// The terminator is pure, and a block without successors ends in a return.
pub(in crate::ir_lower) fn tail_sink_block_exit_ok(graph: &Graph, block: &TailSinkBlock<'_>) -> bool {
    match block.terminator {
        Some(t) => {
            tail_sink_node_pure(graph, t)
                && (!block.successors.is_empty()
                    || graph
                        .nodes
                        .get(t as usize)
                        .is_some_and(|n| n.op == Op::Return))
        }
        None => !block.successors.is_empty(),
    }
}

/// May the continuation of the call at `blocks[b].nodes[pos]` skip the frame
/// republish? Everything after it in its block is pure, and every block
/// reachable from its block is pure, the reachable region has no cycle (so
/// no back-edge poll) and never re-enters `b`, and every path ends in a
/// return. An out-of-range successor fails closed.
pub(in crate::ir_lower) fn tail_sink_continuation_ok(
    graph: &Graph,
    blocks: &[TailSinkBlock<'_>],
    b: usize,
    pos: usize,
) -> bool {
    let Some(home) = blocks.get(b) else {
        return false;
    };
    let rest_pure = home
        .nodes
        .get(pos + 1..)
        .is_some_and(|rest| rest.iter().all(|&n| tail_sink_node_pure(graph, n)));
    if !rest_pure || !tail_sink_block_exit_ok(graph, home) {
        return false;
    }
    // Iterative three-colour DFS from `b`: 1 = on the stack, 2 = done.
    let mut state = vec![0u8; blocks.len()];
    state[b] = 1;
    let mut stack: Vec<(usize, usize)> = vec![(b, 0)];
    while let Some(&(blk, i)) = stack.last() {
        let next = blocks.get(blk).and_then(|x| x.successors.get(i)).copied();
        let Some(s) = next else {
            if let Some(st) = state.get_mut(blk) {
                *st = 2;
            }
            stack.pop();
            continue;
        };
        if let Some(top) = stack.last_mut() {
            top.1 = i + 1;
        }
        match state.get(s).copied() {
            // Out of range, a cycle, or back into the call's own block.
            None | Some(1) => return false,
            Some(0) => {
                if !blocks
                    .get(s)
                    .is_some_and(|x| tail_sink_block_pure(graph, x))
                {
                    return false;
                }
                state[s] = 1;
                stack.push((s, 0));
            }
            _ => {}
        }
    }
    true
}

/// The `Op::Call` nodes whose continuation admits the sink, as
/// `(call, admitted)` for every scheduled call.
pub(in crate::ir_lower) fn tail_sink_calls(graph: &Graph, blocks: &[TailSinkBlock<'_>]) -> Vec<(NodeId, bool)> {
    let mut out = Vec::new();
    for (b, block) in blocks.iter().enumerate() {
        for (pos, &n) in block.nodes.iter().enumerate() {
            if graph
                .nodes
                .get(n as usize)
                .is_some_and(|x| matches!(x.op, Op::Call { .. }))
            {
                out.push((n, tail_sink_continuation_ok(graph, blocks, b, pos)));
            }
        }
    }
    out
}

impl<'a> Lowerer<'a> {

    pub(in crate::ir_lower) fn plan_tail_republish_sink(&mut self) {
        let graph = self.graph;
        let schedule = self.schedule;
        let views: Vec<TailSinkBlock<'_>> = schedule
            .blocks
            .iter()
            .map(|b| TailSinkBlock {
                ctrl: Some(b.ctrl),
                nodes: &b.nodes,
                terminator: b.terminator,
                successors: &b.successors,
            })
            .collect();
        let calls = tail_sink_calls(graph, &views);
        let gate = if false {
            "off"
        } else if graph.nodes.iter().any(Self::is_direct_self_call) {
            "refused(direct self-call)"
        } else if self.self_lock.is_some() {
            "refused(self-lock)"
        } else if !crate::x64::precise_jit_maps_enabled() || self.frame_record == 0 {
            "refused(no frame record)"
        } else {
            "on"
        };
        let mut sink = TailRepublishSink {
            admitted: vec![false; graph.nodes.len()],
            calls: calls.len(),
            tail_calls: calls.iter().filter(|&&(_, ok)| ok).count(),
            gate,
            ..TailRepublishSink::default()
        };
        if gate == "on" {
            for &(n, ok) in &calls {
                if let Some(cell) = sink.admitted.get_mut(n as usize) {
                    *cell = ok;
                }
            }
        }
        self.tail_sink = sink;
    }

    /// W7-C: the body is lowered; nothing emitted after it is a sunk call's
    /// hot path. Prints the census line.
    /// The census of this frame's clear under `skip` (the section above).
    /// `cover_on`: the clear is emitted as the cover when the cover is not
    /// refused.
    pub(in crate::ir_lower) fn fresh_clear_census(&self, skip: &[bool], avx: bool, cover_on: bool) -> FreshClearCensus {
        let plan = self.slot_plan;
        let mut colour_kind = vec![0u8; plan.slots];
        for (id, class) in plan.class.iter().enumerate() {
            let Some(class) = *class else {
                continue;
            };
            let Some(Some(colour)) = plan.node_color.get(id).copied() else {
                continue;
            };
            let is_ref = self
                .graph
                .nodes
                .get(id)
                .is_some_and(|n| n.ty == IrType::Ref);
            let k = match class {
                SlotClass::Prim => 1u8,
                SlotClass::Ref => 3,
                SlotClass::Pinned if is_ref => 4,
                SlotClass::Pinned => 2,
            };
            if let Some(slot) = colour_kind.get_mut(colour as usize) {
                // A reference occupant names the colour (`verify_slot_colouring`
                // refuses a colour shared across classes).
                *slot = (*slot).max(k);
            }
        }
        let words = if self.frame_size > 0 && self.frame_size % 8 == 0 {
            // Cast: a positive frame size over 8.
            (self.frame_size / 8) as usize
        } else {
            0
        };
        let mut by_region = [0u32; 18];
        let mut cleared = 0u32;
        for w in 0..words {
            if skip.get(w).copied().unwrap_or(false) {
                continue;
            }
            cleared += 1;
            // Cast: a word index below 4096.
            let off = 8 * (w as i32 + 1);
            let region = self.fresh_clear_word_region(off, &colour_kind);
            if let Some(i) = FRESH_CLEAR_REGIONS.iter().position(|r| *r == region) {
                by_region[i] += 1;
            }
        }
        let cover = fresh_clear_cover_stores(self.frame_size, skip, avx).map(|stores| {
            // Cast: at most `FRESH_CLEAR_COVER_MAX_STORES` stores.
            (
                stores.len() as u32,
                fresh_clear_cover_instructions(&stores, avx),
            )
        });
        FreshClearCensus {
            // Cast: at most 4096 words.
            words: words as u32,
            cleared,
            by_region,
            runwise: fresh_clear_runwise_cost(self.frame_size, skip, avx),
            cover,
            cover_on: cover_on && cover.is_some(),
        }
    }


    pub(in crate::ir_lower) fn end_tail_republish_sink_body(&mut self) {
        self.tail_sink.cur = false;
        let s = &self.tail_sink;
        if s.calls != 0 && crate::x64::dbg_jitc_enabled() {
            eprintln!(
                "[cratonvm-jitc] ir-tail-republish: method={} calls={} tail_calls={} \
                 admitted={} sunk_sites={} gate={}",
                self.graph.scope_method_key,
                s.calls,
                s.tail_calls,
                s.admitted.iter().filter(|&&a| a).count(),
                s.sunk_sites,
                s.gate,
            );
        }
    }

    /// W7-C: [`Self::emit_call_return_check_after_note`] for a sunk call:
    ///
    /// ```text
    ///   <shadow reload> ; CMP RAX, 1 ; JNO .keep
    ///   <frame republish> ; <sentinel tail>      -- cold: RAX == i64::MIN
    ///   .keep: <result store>
    /// ```
    ///
    /// The reload neither reads the mirrors nor writes RAX (its doc), so it
    /// runs first on both paths, as it does after the republish today; the
    /// sentinel tail and every exit it reaches run after the republish.
    pub(in crate::ir_lower) fn emit_sunk_call_return_check(&mut self, slot: i32, ty: IrType) {
        self.tail_sink.sunk_sites += 1;
        self.emit_shadow_reload();
        self.emit_cmp_rax_sentinel();
        let keep = self.emit_jcc_rel32(0x81); // JNO .keep
        self.emit_post_call_frame_record();
        self.emit_call_return_sentinel_tail(ty);
        self.patch_rel32_to_here(keep);
        self.store_rax(slot);
    }
}



// ── rx27: frame descriptor records (`CRATONVM_JIT_IR_FRAME_BLOCK_DESCRIPTOR`) ──
//
// The frame block (`Lowerer::plan_frame_block`) publishes each reference home
// of an activation by address, one shadow-stack entry per home: two
// instructions per home on every activation that reaches a safepoint (18
// homes, ~40 instructions, 5.9 % of `regex-matches` in one body). Under the
// switch it publishes ONE record instead, `[rbp | 2, header, packed u16
// home offsets / 8 ...]` -- the layout `cratonvm_gc::shadow_stack` documents
// ("Frame descriptor records") and every window reader expands through
// `shadow_stack::walk_window` -- so a block of n homes costs 2 + ceil(n/4)
// stores. Same words named, same contract (each holds a reference or null
// while the record is below `top`, the frame outlives it, every exit
// retracts `top` to the saved base), so nothing else changes: the exits, the
// overflow bail (the record's size is the guard's `need`) and the
// `[rbp - savebase]` claim are the block's own. A block a record cannot carry
// (an offset not a positive multiple of 8 below 8 * 0xFFFF, or more homes than
// the record's ceiling) publishes the indirect entries as before. The record
// is constant per body, so the publication normally stores only `[rbp | 2,
// address]`, the address of the record interned once in the GC's table
// (`shadow_stack::intern_descriptor`); a full table copies the record inline.



/// `cratonvm_gc::shadow_stack::DESCRIPTOR_TAG`, as the `LEA` displacement.
pub(in crate::ir_lower) const FRAME_BLOCK_DESCRIPTOR_TAG: i32 = 2;
/// `cratonvm_gc::shadow_stack::DESCRIPTOR_MAGIC`.
pub(in crate::ir_lower) const FRAME_BLOCK_DESCRIPTOR_MAGIC: u64 = 0xF0B1_0C4D;
/// `cratonvm_gc::shadow_stack::DESCRIPTOR_MAX_HOMES`.
pub(in crate::ir_lower) const FRAME_BLOCK_DESCRIPTOR_MAX_HOMES: usize = 1024;

/// The record words after word 0 for a frame block of home offsets `block`
/// (each home is `[rbp - off]`): the header, then the offsets / 8 packed four
/// to a word, low half first. `None` when a home cannot be carried. Pure.
pub(in crate::ir_lower) fn frame_block_descriptor_words(block: &[i16]) -> Option<Vec<u64>> {
    if block.is_empty() || block.len() > FRAME_BLOCK_DESCRIPTOR_MAX_HOMES {
        return None;
    }
    let mut out = Vec::with_capacity(1 + block.len().div_ceil(4));
    out.push((FRAME_BLOCK_DESCRIPTOR_MAGIC << 32) | block.len() as u64);
    let mut packed = 0u64;
    for (k, &off) in block.iter().enumerate() {
        if off <= 0 || off % 8 != 0 {
            return None;
        }
        // Cast: off > 0, so off / 8 < 0x1000.
        packed |= (off as u64 / 8) << (16 * (k % 4));
        if k % 4 == 3 || k + 1 == block.len() {
            out.push(packed);
            packed = 0;
        }
    }
    Some(out)
}

