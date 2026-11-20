#[allow(unused_imports)]
use super::*;

/// r21 barriers (BARRIERS21-2): the G1 `aastore` screen, a pure byte sequence
/// so a unit test can execute it.
///
/// Entry: RAX = the array (non-null), RDX = the value. Writes RCX, R10, R11
/// and the flags; RAX and RDX survive. Returns `(bytes, decline, cross)`:
///
/// * a `decline` rel32 edge is taken when the SATB pre-gate byte is armed.
///   The caller lands it on the whole `jit_aastore` call, which logs the old
///   element, stores and records the edge;
/// * a `cross` edge is taken when the value is non-null and lies in another
///   region. The caller stores and then calls the lean barrier;
/// * falling off the end (a null value, or a same-region pair) means store
///   with no barrier. `G1Collector::post_write_barrier_rset` returns on both
///   without touching anything.
///
/// ```text
///   mov r11, imm64 &pre ; cmp byte [r11], 0 ; jne decline
///   test rdx, rdx ; jz plain
///   mov r11, imm64 &JIT_G1_BARRIER
///   mov r10, rax ; sub r10, [r11 + base]
///   mov rcx, rdx ; sub rcx, [r11 + base]
///   xor rcx, r10 ; and rcx, [r11 + mask] ; jnz cross
/// plain:
/// ```
///
/// The base is subtracted from both operands, as in F-08's filter
/// (`x64::objects::emit_g1_barrier_filter`): G1's arena is not region-aligned.
pub(crate) fn g1_aastore_screen_bytes(
    plan: crate::x64::G1AastorePlan,
) -> (Vec<u8>, Vec<usize>, Vec<usize>) {
    let mut a = VhConstAsm::default();
    a.emit(&[0x49, 0xBB]); // MOV R11, imm64 (the SATB pre-gate byte)
    a.emit(&(plan.pre as u64).to_le_bytes()); // Cast: a data address
    a.emit(&[0x41, 0x80, 0x3B, 0x00]); // CMP BYTE [R11], 0
    a.jcc_decline(0x85); // JNE: marking -- the helper logs the old element
    a.emit(&[0x48, 0x85, 0xD2]); // TEST RDX, RDX
    let null_value = a.jcc_forward(0x84); // JZ: a null records no edge
    a.emit(&[0x49, 0xBB]); // MOV R11, imm64 (JIT_G1_BARRIER)
    a.emit(&(plan.table as u64).to_le_bytes()); // Cast: a data address
    a.emit(&[0x49, 0x89, 0xC2]); // MOV R10, RAX
    a.emit(&[0x4D, 0x2B, 0x93]); // SUB R10, [R11 + disp32]
    a.emit(&G1B_ARENA_BASE.to_le_bytes());
    a.emit(&[0x48, 0x89, 0xD1]); // MOV RCX, RDX
    a.emit(&[0x49, 0x2B, 0x8B]); // SUB RCX, [R11 + disp32]
    a.emit(&G1B_ARENA_BASE.to_le_bytes());
    a.emit(&[0x4C, 0x31, 0xD1]); // XOR RCX, R10
    a.emit(&[0x49, 0x23, 0x8B]); // AND RCX, [R11 + disp32]
    a.emit(&G1B_REGION_MASK.to_le_bytes());
    // JIT round 23 wave 3, lane L4 (`CRATONVM_JIT_G1_INLINE_YOUNG_FILTER`):
    // another region, but a holder in an Eden / Survivor region owes no
    // barrier (HotSpot's young-card filter; sound under G1's young-source
    // prerequisite, the only state that publishes the table). R10 still holds
    // the holder's arena offset; R10 and R11 are free past this screen (the
    // stores reload RCX and use RAX / RDX).
    if let Some((bytes, skips)) = crate::x64::g1_young_holder_skip_bytes(R10, None, R11) {
        let same = a.jcc_forward(0x84); // JZ: same region -- no barrier
        let at = a.b.len();
        a.emit(&bytes);
        let mut young: Vec<usize> = skips.into_iter().map(|s| at + s).collect();
        a.emit(&[0xE9]); // JMP: not young -- the lean barrier
        let cross = a.rel32_field();
        for s in young.drain(..) {
            a.bind(s);
        }
        a.bind(same);
        a.bind(null_value);
        return (a.b, a.decline, vec![cross]);
    }
    let cross = a.jcc_forward(0x85); // JNZ: another region -- the lean barrier
    a.bind(null_value);
    (a.b, a.decline, vec![cross])
}



/// `Lowerer::try_emit_ir_integer_value_of_prefix`, the inline out-of-cache
/// `Integer.valueOf` box (`CRATONVM_JIT_IR_VALUEOF_INLINE_BOX`, default on;
/// `0` keeps every bound site on the `jit_integer_value_of_direct` call alone,
/// byte-for-byte what this tier emitted before round 11 wave 17). Read per
/// compile, so it adds no process global.
/// al16 (2026-10-05): may a box prefix's allocation arm bump the TLAB on this
/// collector? Always on a collector that walks chunks. Under ZGC, which must
/// be told about every TLAB object, only when the inline announce is armed
/// (`x64::ir_zgc_inline_announce_enabled`): the arm then announces the box
/// with the start-bit store or undoes the bump and declines to the call
/// (`runtime_lowering::emit_zgc_announce_or_rollback`).
/// `CRATONVM_JIT_BOX_ZGC_ANNOUNCE=0` restores the round-16 refusal (every
/// out-of-cache box under ZGC takes the call).
pub(crate) fn box_tlab_admitted() -> bool {
    !cratonvm_types::jit_tlab_registration_required()
        || (crate::x64::ir_zgc_inline_announce_enabled())
}



/// `which` argument of `DirectHelperTable::well_known_class_id`: NOT a class
/// id, but the `IntegerCache.high` this compile's VM resolved for its
/// `Integer.valueOf` native (`lang_math::integer_cache_high_for`), or `0`
/// while it is unresolved. Answered by the VM's `jit_well_known_class_id`.
///
/// Round 11 wave 17, lane `hashmap`. It sits here rather than beside its three
/// siblings in `direct_helpers.rs` only because that file had no owner in the
/// wave; `ir_integer_cache_high` is its one reader. Moving both next to
/// `DirectHelperTable::integer_class_id` is a pure relocation.
pub const WELL_KNOWN_INTEGER_CACHE_HIGH: i64 = 4;

/// The VM's resolved `IntegerCache.high` (always `>= 127` once resolved), or
/// `None` when it cannot be shown: no resolver, no VM context, or a bound no
/// `Integer.valueOf` has resolved yet. The optimizing tier's inline box
/// allocates only ABOVE it (or below -128), so a wrong `Some` would break
/// `valueOf` identity; every doubt answers `None`, which keeps the call.
/// Also read by the single-pass twin (round 12 wave 1, lane `calls`:
/// `x64/op_invoke.rs`, `try_emit_value_of_box_prefix`), so both tiers bake
/// the same bound.
pub(crate) fn ir_integer_cache_high(helpers: &crate::DirectHelperTable) -> Option<i32> {
    let raw = helpers.well_known_class_id;
    let ctx = helpers.static_base_resolver_ctx;
    if raw == 0 || ctx == 0 {
        return None;
    }
    // SAFETY: the VM fills this field from `jit_well_known_class_id`, an
    // `extern "C" fn(i64, i64) -> i64` with process lifetime; a test fills it
    // from a function of the same signature (as `integer_class_id` reads it).
    let f: crate::direct_helpers::DirectHelperFnWellKnownClassId =
        unsafe { std::mem::transmute(raw) };
    // Cast: `usize` context to the C ABI's i64 parameter.
    //
    // SAFETY: `ctx` is the `SharedVm` published beside `f`, and it outlives
    // every compilation it runs.
    let high = unsafe { f(ctx as i64, WELL_KNOWN_INTEGER_CACHE_HIGH) };
    i32::try_from(high).ok().filter(|&h| h >= 127)
}

/// `CRATONVM_JIT_LONG_VALUEOF_INLINE_BOX` (default on; `0` keeps every
/// `Long.valueOf` site bound to `jit_long_value_of_direct` on the call alone,
/// in both tiers, as before round 12 wave 2). Read per site compile, so it adds
/// no process global. The two tiers' own `*_VALUEOF_INLINE_BOX` switches turn
/// off their `Integer` and `Long` prefixes together.
pub(crate) fn long_value_of_inline_box_enabled() -> bool {
    true
}


/// `which` argument of `DirectHelperTable::well_known_class_id`: NOT a class
/// id, but the address of slot 0 of this compile's VM's `Integer` box table
/// (`lang_math::integer_cache_slots_raw`), or `0` while no `Integer.valueOf`
/// has filed it. JIT round 16, lane callbind; read by
/// [`ir_integer_cache_slots`] only.
pub const WELL_KNOWN_INTEGER_CACHE_SLOTS: i64 = 6;

/// `which` argument of `DirectHelperTable::well_known_class_id`: the slot
/// count of the table [`WELL_KNOWN_INTEGER_CACHE_SLOTS`] names
/// (`IntegerCache.high + 129`), or `0`.
pub const WELL_KNOWN_INTEGER_CACHE_SLOTS_LEN: i64 = 7;

/// `CRATONVM_JIT_IR_VALUEOF_CACHE_READ` (JIT round 16, lane callbind), **default
/// OFF** (not measured; the orchestrator measures it): a site bound to the thin
/// `jit_integer_value_of_direct` helper answers a value inside the box cache
/// from the VM's box table INLINE (`try_emit_ir_integer_value_of_prefix`), and
/// takes the call only for an empty slot or a value outside the cache. Read per
/// compile, so it adds no process global.
pub(crate) fn ir_integer_value_of_cache_read_enabled() -> bool {
    true
}

/// `(address of slot 0, slot count)` of this compile's VM's `Integer` box
/// table, or `None` when it cannot be shown: no resolver, no VM context, a
/// table no `valueOf` has filed, or a count that is not the `-128..=high` a
/// resolved `IntegerCache.high` gives (`high >= 127`, so at least 256 slots,
/// and at most `i32::MAX + 129`), or a misaligned address. Every doubt keeps
/// the call. JIT round 16, lane callbind.
pub(crate) fn ir_integer_cache_slots(helpers: &crate::DirectHelperTable) -> Option<(u64, u32)> {
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
    let len = unsafe { f(ctx as i64, WELL_KNOWN_INTEGER_CACHE_SLOTS_LEN) };
    // SAFETY: as above.
    let base = unsafe { f(ctx as i64, WELL_KNOWN_INTEGER_CACHE_SLOTS) };
    let len = u32::try_from(len)
        .ok()
        .filter(|&n| (256..=0x7FFF_FFFF).contains(&n))?;
    let base = u64::try_from(base).ok().filter(|&b| b != 0 && b % 8 == 0)?;
    Some((base, len))
}

/// The inline cached-range `Integer.valueOf` read (JIT round 16, lane
/// callbind), as bytes. Entry: ECX = the `int` value. Exit by fall-through:
/// RAX = the canonical box. Clobbers RAX, R10 and the flags; ECX is unchanged.
///
/// ```text
/// LEA  EAX, [RCX + 128]        ; slot index; a 32-bit write zero-extends
/// CMP  EAX, len
/// JAE  outside                 ; unsigned: below -128 wraps past len
/// MOV  R10, base
/// MOV  RAX, [R10 + RAX*8]      ; the slot `integer_cache_peek` loads
/// TEST RAX, RAX
/// JZ   empty                   ; not boxed yet: the call fills it
/// ```
///
/// Returns the bytes and the offsets, within them, of the two rel32 operands
/// (`outside`, `empty`), both left zero for the caller to patch.
pub(crate) fn integer_cache_read_bytes(base: u64, len: u32) -> (Vec<u8>, usize, usize) {
    let mut b = Vec::with_capacity(36);
    b.extend_from_slice(&[0x8D, 0x81, 0x80, 0x00, 0x00, 0x00]); // LEA EAX,[RCX+80h]
    b.push(0x3D); // CMP EAX, imm32
    b.extend_from_slice(&len.to_le_bytes());
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

/// `which` argument of `DirectHelperTable::well_known_class_id`: NOT a class
/// id, but the address of slot 0 of this compile's VM's `Long` box table
/// (`lang_math::long_cache_slots_raw`; always 256 slots, `-128..=127`), or `0`
/// while no `Long.valueOf` has filed it. JIT round 16 wave 7, lane callbind
/// (CB16-3); read by [`ir_long_cache_slots`] only.
pub const WELL_KNOWN_LONG_CACHE_SLOTS: i64 = 8;



/// The address of slot 0 of this compile's VM's `Long` box table, or `None`
/// when it cannot be shown: no resolver, no VM context, a table no `valueOf`
/// has filed, or a misaligned address. The slot count is the fixed 256 (the
/// VM answers `0` for any other). JIT round 16 wave 7, lane callbind.
pub(crate) fn ir_long_cache_slots(helpers: &crate::DirectHelperTable) -> Option<u64> {
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
    let base = unsafe { f(ctx as i64, WELL_KNOWN_LONG_CACHE_SLOTS) };
    u64::try_from(base).ok().filter(|&b| b != 0 && b % 8 == 0)
}

/// `which` arguments of `DirectHelperTable::well_known_class_id` (nio20 wave
/// 4, lane L2): the `ClassId`s of the three JDK session classes whose
/// `checkValidStateRaw()` can never fail, BOOTSTRAP definitions only, or `0`
/// when not loaded / not answerable. Numbered far from the 1..=8 siblings so a
/// concurrently added selector cannot collide. Read by
/// [`scoped_session_never_closing_class_ids`] only.
pub const WELL_KNOWN_SESSION_GLOBAL: i64 = 0x5E55_0001;
/// `jdk/internal/foreign/GlobalSession$HeapSession` (the JDK's
/// `MemorySessionImpl.createHeap`, the session of its `MemorySegment.ofArray` /
/// `ofBuffer` views). NOTE: in CratonVM `createHeap` (with the other session
/// factories) is force-native (`native_override.rs`) and answers the session
/// MODEL (`p67_memory_session`, class `MemorySessionImpl` itself), which this
/// list does not admit; see
/// `r20-scoped-fold-confined-session-needs-the-current-thread-object-CLOSED-20261012.md`.
pub const WELL_KNOWN_SESSION_HEAP: i64 = 0x5E55_0002;
/// `jdk/internal/foreign/ImplicitSession` (`Arena.ofAuto()` when the JDK's own
/// `ArenaImpl` builds it).
pub const WELL_KNOWN_SESSION_IMPLICIT: i64 = 0x5E55_0003;
/// nio20 wave 5, lane L2 (the confined arm of the scoped fold,
/// [`scoped_session_confined_rows`]): `jdk/internal/foreign/ConfinedSession`
/// (bootstrap), the JDK's own confined session, or `0`.
pub const WELL_KNOWN_SESSION_CONFINED: i64 = 0x5E55_0004;
/// `jdk/internal/foreign/MemorySessionImpl` (bootstrap): the class of
/// CratonVM's session MODEL (`p67_memory_session`; the class is abstract, so an
/// instance of exactly this id is a model session), or `0`.
pub const WELL_KNOWN_SESSION_MODEL: i64 = 0x5E55_0005;
/// JIT round 22 (lane memory, MEM20-2 residual): `jdk/internal/foreign/SharedSession`
/// (bootstrap), the class of every `Arena.ofShared()` session the JDK's own
/// `ArenaImpl` builds -- in `--real-jdk`, and in `--compatible` under the
/// `JdkForeign` alignment row -- or `0`. Its `state` / `owner` slots (plus
/// one) are [`WELL_KNOWN_SESSION_SHARED_STATE_SLOT`] /
/// [`WELL_KNOWN_SESSION_SHARED_OWNER_SLOT`]. Read by
/// [`scoped_session_confined_rows`] only while the shared window is on.
pub const WELL_KNOWN_SESSION_SHARED: i64 = 0x5E55_0006;
pub const WELL_KNOWN_SESSION_SHARED_STATE_SLOT: i64 = 0x5E55_0013;
pub const WELL_KNOWN_SESSION_SHARED_OWNER_SLOT: i64 = 0x5E55_0014;
/// NOT class ids: the instance slot index PLUS ONE (`0` = unknown) of
/// `int state` / `Thread owner` as an instance of the named class carries them
/// (resolved by name AND descriptor through the hierarchy, as the model's own
/// `p67_session_slots` resolves them by name).
pub const WELL_KNOWN_SESSION_CONFINED_STATE_SLOT: i64 = 0x5E55_0011;
pub const WELL_KNOWN_SESSION_CONFINED_OWNER_SLOT: i64 = 0x5E55_0012;
pub const WELL_KNOWN_SESSION_MODEL_STATE_SLOT: i64 = 0x5E55_0021;
pub const WELL_KNOWN_SESSION_MODEL_OWNER_SLOT: i64 = 0x5E55_0022;
/// NOT a class id: `offset_of!(JvmThread, java_thread_obj)` PLUS ONE -- the
/// current thread's `java.lang.Thread` word, from the `JvmThread*` the
/// `JIT_THREAD` TLS mirror holds -- for any execution policy. Unlike
/// `DirectHelperTable::thread_java_obj_offset` (which stands in for the
/// `Thread.currentThread()` native and is therefore `0` under `--real-jdk`),
/// the scoped fold only COMPARES it with a session's `owner`, exactly as the
/// session natives do (`p67_owner_is_current_thread`), so no policy gate applies.
pub const WELL_KNOWN_THREAD_JAVA_OBJ_OFFSET: i64 = 0x5E55_0030;
/// NOT a class id (nio21 wave 1 L3): the address of the process's JIT-readable
/// `Unsafe` arena table (`cratonvm_native_builtins::unsafe_arena_jit_table_addr`),
/// or `0`. Read by [`scoped_arena_jit_table`].
pub const WELL_KNOWN_UNSAFE_ARENA_JIT_TABLE: i64 = 0x5E55_0040;

/// The layout of the JIT-readable arena table (nio21 wave 1 L3), as
/// `native-builtins` defines it (`unsafe_natives_ext.rs`, the section comment
/// above `unsafe_arena::JIT_SLOT_SHIFT`); the VM's `jit_well_known_class_id`
/// arm asserts the two copies agree. A SLOTTED handle is `1<<62 | seq << 30`
/// (bit 61 clear); its entry is `table[(handle >> 30) & (2^15 - 1)]`, 32 bytes:
/// `key` (the handle), `len`, `ptr`.
pub const SCOPED_ARENA_SLOT_SHIFT: u32 = 30;
/// See [`SCOPED_ARENA_SLOT_SHIFT`].
pub const SCOPED_ARENA_INDEX_BITS: u32 = 15;
/// See [`SCOPED_ARENA_SLOT_SHIFT`].
pub const SCOPED_ARENA_ENTRY_SHIFT: u32 = 5;
/// See [`SCOPED_ARENA_SLOT_SHIFT`].
pub const SCOPED_ARENA_KEY_OFFSET: usize = 0;
/// See [`SCOPED_ARENA_SLOT_SHIFT`].
pub const SCOPED_ARENA_LEN_OFFSET: usize = 8;
/// See [`SCOPED_ARENA_SLOT_SHIFT`].
pub const SCOPED_ARENA_PTR_OFFSET: usize = 16;

/// `CRATONVM_JIT_IR_SCOPED_ARENA_FOLD` (nio21 wave 1 L3, **default ON**, `=0`
/// restores the nio20 behaviour: the builder refuses a literal-null base and an
/// arena handle deopts): fold a `ScopedMemoryAccess` access whose base is null
/// (a direct buffer, a native segment) and resolve an `Unsafe` arena handle
/// through the JIT-readable table. Read per compile, so it adds no process
/// global.
pub(crate) fn ir_scoped_arena_fold_enabled() -> bool {
    true
}

/// ADDR21-1 Option A (`CRATONVM_UNSAFE_REAL_POINTERS`, default ON since 2026-10-27, `=0` off): the `Unsafe`
/// arena hands out real addresses, so a native segment's `min` is never a
/// slotted handle and the loop-entry arena cache (round 24 B2) would always
/// miss. When on, `Lowerer::lower_arena_cache_entry` answers the sentinel
/// [`ARENA_CACHE_RAW_HIT`] for an untagged positive `min` and the cached access
/// serves it at `min + delta` -- what the full path's raw arm loads, without
/// its per-element session check. Read per compile; off, the emitted bytes
/// are unchanged.
pub(crate) fn scoped_arena_real_pointers() -> bool {
    true
}

/// The [`crate::ir::UnboxOp::ArenaCacheEntry`] of a raw (untagged) `min` in
/// real-pointer mode ([`scoped_arena_real_pointers`]): not a table slot.
pub(in crate::ir_lower) const ARENA_CACHE_RAW_HIT: i32 = 1;

/// The JIT-readable arena table's address for this compile, or `None` (switch
/// off, no resolver / VM, table off, a misaligned answer). When `None` the
/// builder keeps refusing a literal-null base and the lowering keeps deopting
/// on a tagged address.
pub(crate) fn scoped_arena_jit_table(helpers: &crate::DirectHelperTable) -> Option<u64> {
    let raw = helpers.well_known_class_id;
    let ctx = helpers.static_base_resolver_ctx;
    if raw == 0 || ctx == 0 || !ir_scoped_arena_fold_enabled() {
        return None;
    }
    // SAFETY: as in `ir_integer_cache_high`.
    let f: crate::direct_helpers::DirectHelperFnWellKnownClassId =
        unsafe { std::mem::transmute(raw) };
    // Cast: `usize` context to the C ABI's i64 parameter.
    //
    // SAFETY: as in `ir_integer_cache_high`.
    let addr = unsafe { f(ctx as i64, WELL_KNOWN_UNSAFE_ARENA_JIT_TABLE) };
    u64::try_from(addr)
        .ok()
        .filter(|&a| a != 0 && a % (1u64 << SCOPED_ARENA_ENTRY_SHIFT) == 0)
}

// ── nio21 wave 11 L5 (W9L2-2): constant-`VarHandle` field accesses inline ──
//
// Page: `docs/feature-designs/profile-guided-inlining.md`.
//
// HotSpot treats a `VarHandle` held in a `static final` field of an initialised
// class as a constant, and its access-mode call (get / set / CAS / getAndAdd /
// ...) inlines to the field access with the access mode's ordering. Here every
// such call was a thin helper CALL (or the generic dispatch) that re-identified
// the handle by its identity hash on every execution.
//
// What this section serves, shared byte for byte by the single-pass prefixes
// (`x64/op_invoke.rs`, the thin `VarHandle` bind arm and the dispatch arm) and
// the optimizing tier's `Op::Call` prefix (`Lowerer::try_emit_ir_vh_const_prefix`):
//
// * The VM files, when a class finishes `<clinit>`, every `static final
//   VarHandle` field whose handle the natives describe as an INSTANCE-field
//   handle over an `int`, `long` or reference field
//   (`vm::jit::helpers::note_class_initialized_varhandles`), keyed by the
//   static's `(class id, static field index)`. The compile asks for that row
//   through the well-known resolver ([`vh_const_plan`], [`vh_const_ref_facts`]):
//   pure data, the compile thread never reads the handle object.
// * A site is admitted by its access-mode name and call-site descriptor
//   ([`vh_const_site_shape`]): exact `I` / `J` value kinds, or references;
//   anything an `asType` adaptation would convert (a boxed value, a widening)
//   keeps the call.
// * The emitted prefix proves the handle is that static's (the optimizing tier:
//   the operand IS the `getstatic`; single-pass: a run-time compare of the
//   operand with the static's current value), then screens the receiver: not
//   null, a plain object of EXACTLY the coordinate class, COMPACT. A subclass
//   instance, a null, a legacy object all take the call, which answers them as
//   before (NPE, the subclass's field, ...). No deopt anywhere.
// * The access itself: a load (`MOV` is a correct plain / opaque / acquire /
//   volatile load under x86-64 TSO), a store (`MOV`; `XCHG` for `setVolatile`,
//   the StoreLoad a volatile store owes) or one locked instruction for the
//   read-modify-write modes, the `Unsafe` prefixes' own bytes
//   (`direct_helpers::unsafe_inline_rmw_bytes`, `LOCK CMPXCHG` / `LOCK XADD` /
//   `XCHG` / the bitwise `CMPXCHG` loop) at the field's compact displacement.
//   The VM's CAS on a compact `int` / `long` / reference field is one hardware
//   atomic on the same bytes (`vm_exec::compare_and_swap_field_shared`), so the
//   two are atomic against each other.
// * A REFERENCE store (`set*`, CAS, `compareAndExchange*`, `getAndSet*`)
//   stores inline only where the collector's published barrier gates rule both
//   barriers out, decided FIRST, exactly the gates of the gated reference
//   `putfield` (`x64::objects::emit_gated_compact_ref_putfield`, the optimizing
//   tier's `emit_gated_ir_ref_putfield`): the SATB pre-gate byte clear, and a
//   null value, a young receiver or no old objects. Anything else takes the
//   call, whose helper runs the collector's own barriers. A collector that
//   publishes no plan (G1) keeps every reference store on the call -- in the
//   single-pass tier; the optimizing tier serves G1 through its own plan since
//   JIT round 21, lane g1vh ([`VhG1Gates`], `CRATONVM_JIT_VH_CONST_G1_REF`).
// * A stored reference VALUE is admitted the way the natives admit it
//   (`varhandle_plan_admits_value`) where that can be decided without a call:
//   any value for an `Object` field; for a field of class type `T`, null or an
//   instance of exactly `T`, or -- statically -- a value the call site's own
//   descriptor types as `T` when the site lives in the coordinate class and the
//   VM vouched that the name resolves to `T` there (same loader or `java/`, not
//   an interface: the verifier does not type interface operands). HotSpot casts
//   `expected` too (`handle.fieldType.cast(expected)`), so a CAS screens it
//   the same way. Everything else takes the call.

/// `CRATONVM_JIT_VH_CONST_INLINE` (nio21 wave 11 L5, **default ON**, `=0`
/// keeps every `VarHandle` access on its call in both tiers, and stops the VM
/// filing the plans): inline field accesses through a constant `VarHandle`.
/// Read per compile site (and once per class initialisation by the VM), so no
/// cache and no process global.
pub fn vh_const_inline_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_VH_CONST_REF_INLINE` (nio21 wave 11 L5, **default ON**, `=0`
/// keeps every REFERENCE-field access on its call while the `int` / `long`
/// ones stay inline): the reference half of [`vh_const_inline_enabled`] --
/// raw reference loads, gated reference stores and CAS. Read per site.
pub fn vh_const_ref_inline_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_VH_CONST_G1_REF` (JIT round 21, lane g1vh; CF21-1 of
/// `docs/feature-designs/jit-ir-optimizations.md`, page
/// `docs/feature-designs/jit-barrier-lowering.md`;
/// **default ON**, `=0` keeps every reference write mode on its call under G1,
/// as before): the optimizing tier's constant-`VarHandle` prefix serves a
/// reference `set*` / CAS / `compareAndExchange*` / `getAndSet*` under G1,
/// which publishes no generational gate plan, with the G1 conditions of
/// `Lowerer::emit_g1_ir_ref_putfield` ([`VhG1Gates`]). Read per site.
pub fn vh_const_g1_ref_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_EVIDENCE_G1_VH_WRITE` (JIT round 21, lane g1vhsoft; page
/// `docs/feature-designs/jit-barrier-lowering.md`;
/// perf, **default ON**, `=0` restores the gate's old answer): a reference
/// write mode the optimizing tier's constant-`VarHandle` prefix serves under
/// G1 ([`VhConstCore::g1`]) is C1 -> C2 acceptance evidence (counted as a
/// builder expansion, `ir_evidence::note_builder_expansion`). The single-pass
/// twin (`Compiler::emit_sp_vh_const_prefix`) builds against the published
/// generational / ZGC gates only, which G1 does not publish, so under G1 its
/// body keeps the CALL for every reference `set*` / CAS / `getAndSet*`: the
/// prefix replaces a call the baseline keeps, the rule
/// `builder_expansion_is_evidence` states. Without it a method whose only
/// gain is that prefix (`R21VhcaeG1Rows vhSetVolFreshVal`) was refused by the
/// gate (`call_prefix_is_evidence` is opt-in) and ran the single-pass call:
/// 144-216 ns per store under G1 against 15 under ZGC, whose single-pass
/// prefix serves the store. Read per site.
pub(crate) fn ir_evidence_g1_vh_write_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_VH_CONST_RET_SAME_LOADER` (JIT round 21, lane vhcae; page
/// `jr21-vhcae-compare-and-exchange-typed-return-declines-20261014.md`;
/// **default ON**, `=0` proves a typed reference return only at a site in
/// the coordinate class itself, as before): see
/// [`vh_const_ret_proof_caller`]. Read per site.
pub fn vh_const_ret_same_loader_enabled() -> bool {
    true
}

/// JIT round 21, lane vhcae: the class whose loader names a constant-
/// `VarHandle` site's REFERENCE return for [`vh_const_core_with`]'s return
/// proof -- `caller_class_id` itself, or the coordinate class when the two
/// have the same defining loader.
///
/// javac types a polymorphic-signature call's return as the cast around it
/// (`(Node) NEXT.compareAndExchange(h, a, b)` is `(..)LNode;`, no
/// `checkcast` follows), and the handle's own `asType` casts the witness to
/// that class -- named in the CALLER's loader. The witness is the field's
/// value, so a value of the field's declared class `F` (the row's proving
/// name is set only for a class type, never an interface, defined by the
/// coordinate class's loader or living in `java/`). A caller whose defining
/// loader IS the coordinate class's resolves the name of `F` to `F` itself,
/// so the cast cannot fail and the prefix may hand the witness over as is,
/// exactly as for a site in the coordinate class. Any other answer (the
/// switch off, a loader unknown, two loaders) keeps `caller_class_id`, i.e.
/// the coordinate-class-only proof.
pub(crate) fn vh_const_ret_proof_caller(
    helpers: &crate::DirectHelperTable,
    caller_class_id: u32,
    coordinate_class_id: u32,
) -> u32 {
    vh_const_ret_proof_caller_with(caller_class_id, coordinate_class_id, &|cid| {
        vh_const_ask(
            helpers,
            vh_const_query(WELL_KNOWN_VH_CONST_LOADER_TAG, cid, 0),
        )
    })
}

/// [`vh_const_ret_proof_caller`] over a loader oracle (`1 + native id`, `0`
/// unknown).
pub(crate) fn vh_const_ret_proof_caller_with(
    caller_class_id: u32,
    coordinate_class_id: u32,
    loader_of: &dyn Fn(u32) -> i64,
) -> u32 {
    if caller_class_id == 0
        || coordinate_class_id == 0
        || caller_class_id == coordinate_class_id
        || !vh_const_ret_same_loader_enabled()
    {
        return caller_class_id;
    }
    let caller_loader = loader_of(caller_class_id);
    if caller_loader > 0 && caller_loader == loader_of(coordinate_class_id) {
        coordinate_class_id
    } else {
        caller_class_id
    }
}

/// JIT round 21, lane g1vh: the G1 barrier plan of a constant-`VarHandle`
/// reference write ([`VhConstCore::g1`]), the inline G1 `putfield`'s
/// conditions (`Lowerer::emit_g1_ir_ref_putfield`) applied to every write
/// mode. Screened before the access (RAX = the receiver, R10 = `expected`,
/// RDX = the new value), by [`vh_g1_store_screen`]:
///
/// * **SATB pre-barrier.** The pre-gate byte (`helpers.ref_store_pre_gate`,
///   armed in a stop-the-world step BEFORE G1's SATB queue goes active and
///   disarmed AFTER it goes inactive) clear: no mark is running, the old value
///   owes nothing (no safepoint lies between the test and the access). While
///   it is armed, a write is served only when the value it replaces is known
///   to be null: a `set*` whose field reads null (the `putfield` arm's test),
///   a CAS / `compareAndExchange*` whose `expected` is null (on success the
///   replaced value IS `expected`; a failure writes nothing). Everything else
///   (a non-null `expected`, every `getAndSet*`) takes the call, whose
///   helper logs the old value -- HotSpot's G1 logs the pre-read value
///   before the `LOCK CMPXCHG` instead; this tier has no SATB enqueue inline.
/// * **Post barrier.** A null value, or one in the receiver's region
///   (`((recv - base) ^ (val - base)) & region_mask == 0` against
///   `JIT_G1_BARRIER`), records no remembered-set edge
///   (`G1Collector::post_write_barrier_rset` returns for both). A
///   cross-region value takes the post edge when [`Self::post_call`] (the
///   caller performs the access there, then -- unless the collector's edge
///   filter shows it would do nothing, `crate::ir_g1_edge_filter` -- calls
///   the lean `jit_g1_post_write_barrier(vm, recv, val)`, as the gated
///   `putfield` does after its store; HotSpot's G1 post barrier runs after a
///   CAS on the new value whatever the outcome, and so does this one), else
///   declines to the call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct VhG1Gates {
    /// The SATB pre-gate byte's address.
    pub(crate) pre: usize,
    /// `JIT_G1_BARRIER`'s address (word 0 the arena base, word 2 the region
    /// mask), read at run time.
    pub(crate) table: usize,
    /// Serve a cross-region value through the post edge (the caller pays
    /// `jit_g1_post_write_barrier`); `false` declines it to the call.
    pub(crate) post_call: bool,
}

/// JIT round 21, lane g1vh: the G1 plan [`VhG1Gates`] for a compile whose
/// collector publishes no generational gate plan (`ref_store_gates` `None`)
/// and whose G1 geometry is live -- the same admission as the inline G1
/// `putfield` (`CRATONVM_JIT_IR_G1_REF_STORE`, `CRATONVM_G1_INLINE_BARRIER`,
/// a live `JIT_G1_BARRIER`) plus a wired SATB pre-gate byte. The post edge
/// needs the gated `putfield`'s switch (`CRATONVM_JIT_IR_G1_GATED_REF_STORE`)
/// and the lean barrier's address. `None` keeps every reference write on its
/// call. One rule for the prefix and for the planner's [`vh_const_site_served`].
pub(crate) fn vh_g1_gates(
    ref_store_gates: Option<(usize, usize, usize)>,
    g1_barrier_addr: usize,
    pre_gate: usize,
    g1_post_write_barrier: usize,
) -> Option<VhG1Gates> {
    if ref_store_gates.is_some()
        || pre_gate == 0
        || !vh_const_g1_ref_enabled()
        || !ir_g1_ref_store_enabled()
        || !crate::x64::g1_inline_barrier_enabled()
        || !crate::x64::g1_barrier_table_live(g1_barrier_addr)
    {
        return None;
    }
    Some(VhG1Gates {
        pre: pre_gate,
        table: g1_barrier_addr,
        post_call: ir_g1_gated_ref_store_enabled() && g1_post_write_barrier != 0,
    })
}

/// The barrier plan a reference write is built against: the published
/// generational / ZGC gates (and the post-skip mask), or G1's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VhStoreBarriers {
    Gates(Option<(usize, usize, usize)>, Option<u8>),
    G1(VhG1Gates),
}

/// NOT a class id (nio21 wave 11 L5): the well-known resolver's query for the
/// constant-`VarHandle` plan filed for one `static final` field, built by
/// [`vh_const_query`]. The answer is a [`VhConstPlan::to_word`] word, `0` when
/// nothing is filed. The tag sits in bits 48..64, far above every small
/// well-known code, and carries `class id << 16 | static field index`.
pub const WELL_KNOWN_VH_CONST_PLAN_TAG: i64 = 0x5648_i64 << 48;
/// The same query for a reference field's value-admission row: the address of a
/// [`VhConstRefRow`] the VM owns for its whole life, `0` when none.
pub const WELL_KNOWN_VH_CONST_REF_ROW_TAG: i64 = 0x5649_i64 << 48;
/// nio21 wave 12 L2: the address of this VM's supers-display cell
/// ([`VhSubtypeDisplay`]); the class and field halves of the word are ignored.
/// `0` when the VM keeps no displays.
pub const WELL_KNOWN_VH_CONST_DISPLAY_CELL_TAG: i64 = 0x564A_i64 << 48;
/// nio21 wave 12 L2: `1 + depth` of the class named in the word's class half
/// (its index in its own supers display, `java/lang/Object` = 0), `0` when the
/// class has no published display (an interface, or not initialised yet).
pub const WELL_KNOWN_VH_CONST_DEPTH_TAG: i64 = 0x564B_i64 << 48;
/// nio21 wave 14 L4: `1` when the class named in the word's class half is an
/// INTERFACE (answered from the displays' interface sets), else `0`.
pub const WELL_KNOWN_VH_CONST_IFACE_TAG: i64 = 0x564C_i64 << 48;
/// JIT round 21, lane vhcae (VHCAE21-1, `CRATONVM_JIT_G1_EDGE_FILTER`): the
/// address of this VM's G1 collector's `G1JitEdgeFilter`
/// ([`crate::ir_g1_edge_filter`]); the class and field halves of the word are
/// ignored. `0` under any other collector.
pub const WELL_KNOWN_VH_CONST_G1_EDGE_FILTER_TAG: i64 = 0x5672_i64 << 48;

/// JIT round 21, lane dispatch (re-landed t1 w8 DISP21): does a type-check
/// row arm serve this site's target (`op` is `"instanceof"` or
/// `"checkcast"`)? After the r21 w1g A/B only an interface `instanceof`;
/// `CRATONVM_JIT_TYPECHECK_ROWS_ALL` admits class targets and `checkcast`
/// too. The rows themselves still need `CRATONVM_JIT_TYPECHECK_ROWS`
/// (`Lowerer::typecheck_rows_base`).
pub(in crate::ir_lower) fn typecheck_row_arm_admitted(display: VhSubtypeDisplay, op: &str) -> bool {
    (display.interface && op == "instanceof") || crate::typecheck_rows::typecheck_rows_all_enabled()
}
/// JIT round 21 wave 1, lane updaters (`CRATONVM_JIT_VH_CONST_CAST_RETURN`):
/// NOT a [`vh_const_query`] word -- the tag in bits 48..64 and, below it, the
/// address of a [`VhConstClassByName`] the compiler holds for the duration of
/// the call. The answer is the class id the name resolves to for the caller
/// class, only when that is the one loaded class of the name; else `0`.
pub const WELL_KNOWN_VH_CONST_CLASS_BY_NAME_TAG: i64 = 0x5650_i64 << 48;

/// JIT round 21 wave 16 (lane perf-cf-lean, `CRATONVM_JIT_IR_CALL_ENTRY_NULL_GUARD`):
/// NOT a [`vh_const_query`] word -- the tag in bits 48..64 and, below it, the
/// address of a [`CallEntryNullGuardQuery`] the compiler holds for the
/// duration of the call. The answer is [`call_entry_null_guard_answer_word`]
/// when the call's resolved target is a receiver-bound `void` method whose
/// entry returns at once while one reference field of `this` is null, else
/// `0`. See [`Lowerer::try_emit_ir_call_entry_null_guard_prefix`].
pub const WELL_KNOWN_CALL_ENTRY_NULL_GUARD_TAG: i64 = 0x5651_i64 << 48;

/// The request [`WELL_KNOWN_CALL_ENTRY_NULL_GUARD_TAG`] points at: the call
/// site's constant-pool triple (UTF-8, not NUL-terminated), the class whose
/// pool spelled it, and its invoke kind (`0` virtual, `1` special).
#[repr(C)]
pub struct CallEntryNullGuardQuery {
    pub class_ptr: *const u8,
    pub class_len: usize,
    pub method_ptr: *const u8,
    pub method_len: usize,
    pub desc_ptr: *const u8,
    pub desc_len: usize,
    pub caller_class_id: u32,
    pub invoke_kind: u8,
}

/// The VM's answer word for a guardable call: bit 40 set (so it is never
/// `0`), bit 32 = the field is `volatile`, bits 0..32 = the field's absolute
/// compact displacement (non-zero).
pub const fn call_entry_null_guard_answer_word(disp: u32, volatile: bool) -> i64 {
    // Cast: widenings into disjoint bit fields of one positive word.
    (1_i64 << 40) | ((volatile as i64) << 32) | (disp as i64)
}

/// `(displacement, volatile)` of a [`call_entry_null_guard_answer_word`], or
/// `None` for `0` and for any word this version would not have written.
pub fn call_entry_null_guard_decode(word: i64) -> Option<(i32, bool)> {
    if word >> 40 != 1 {
        return None;
    }
    let disp = i32::try_from(word & 0xFFFF_FFFF).ok().filter(|&d| d > 0)?;
    let flags = (word >> 32) & 0xFF;
    if flags > 1 {
        return None;
    }
    Some((disp, flags == 1))
}

/// `CRATONVM_JIT_IR_CALL_ENTRY_NULL_GUARD=1` (JIT round 21 wave 16, lane
/// perf-cf-lean; perf, opt-in, default OFF): the optimizing tier skips a
/// `void` instance call whose target's entry is a pure null test of one
/// reference field of `this` followed by `return`, when that field reads null
/// (`CompletableFuture.postComplete()` with `stack == null`, once per
/// `complete`). Read per site at compile time, here and by the VM's answer
/// (`jit_bridge::call_entry_null_guard_answer`).
pub fn ir_call_entry_null_guard_enabled() -> bool {
    false
}

/// Ask this compile's VM whether `info`'s call is guardable
/// ([`WELL_KNOWN_CALL_ENTRY_NULL_GUARD_TAG`]); `(displacement, volatile)`.
pub(crate) fn call_entry_null_guard_ask(
    helpers: &crate::DirectHelperTable,
    info: &JitInvokeInfo,
) -> Option<(i32, bool)> {
    let query = CallEntryNullGuardQuery {
        class_ptr: info.class_name.as_ptr(),
        class_len: info.class_name.len(),
        method_ptr: info.method_name.as_ptr(),
        method_len: info.method_name.len(),
        desc_ptr: info.descriptor.as_ptr(),
        desc_len: info.descriptor.len(),
        caller_class_id: info.declaring_class_id,
        invoke_kind: info.invoke_kind,
    };
    let addr = &query as *const CallEntryNullGuardQuery as usize; // Cast: an address
    if addr >> 48 != 0 {
        return None;
    }
    // Cast: an address below 2^48, OR-ed under the tag.
    call_entry_null_guard_decode(vh_const_ask(
        helpers,
        WELL_KNOWN_CALL_ENTRY_NULL_GUARD_TAG | addr as i64,
    ))
}

/// The guard bytes of [`Lowerer::try_emit_ir_call_entry_null_guard_prefix`]:
/// on entry RAX = the receiver. Falls through (the call is skipped) only for
/// a non-null, plain, COMPACT receiver whose reference field at `disp` reads
/// null; every other case takes a decline edge to the unchanged call (a null
/// receiver's NPE included). A compact receiver of any subclass holds the
/// field at the same displacement (a parent's compact layout is a prefix of
/// every child's). Writes only the flags. Returns the bytes and the offsets of
/// the rel32 decline fields.
pub(crate) fn call_entry_null_guard_bytes(disp: i32) -> (Vec<u8>, Vec<usize>) {
    const KIND_TAGS: u8 = cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8; // Cast: disp8
    const GC_FLAGS: u8 = cratonvm_types::GC_FLAGS_BYTE_OFFSET as u8; // Cast: disp8
    const _: () = assert!(cratonvm_types::KIND_TAGS_BYTE_OFFSET <= 127);
    const _: () = assert!(cratonvm_types::GC_FLAGS_BYTE_OFFSET <= 127);
    let mut a = VhConstAsm::default();
    a.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
    a.jcc_decline(0x84); // JZ: the call raises the NPE
    a.emit(&[0xF6, 0x40, KIND_TAGS, cratonvm_types::KIND_TAG_BYTE_MASK]);
    a.jcc_decline(0x85); // JNZ: not a plain object
    a.emit(&[0xF6, 0x40, GC_FLAGS, cratonvm_types::GC_FLAG_COMPACT]);
    a.jcc_decline(0x84); // JZ: a legacy instance has no compact displacement
    a.emit(&[0x48, 0x83, 0xB8]); // CMP QWORD [RAX + disp32], imm8
    a.emit(&disp.to_le_bytes());
    a.emit(&[0x00]);
    a.jcc_decline(0x85); // JNE: the field is set -- the callee has work
    (a.b, a.decline)
}

/// The request [`WELL_KNOWN_VH_CONST_CLASS_BY_NAME_TAG`] points at: a UTF-8
/// internal class name and the class whose code names it.
#[repr(C)]
pub struct VhConstClassByName {
    pub name_ptr: *const u8,
    pub name_len: usize,
    pub caller_class_id: u32,
}

/// JIT round 21, lane dispatch (`CRATONVM_JIT_TYPECHECK_ROWS`): the address of
/// this VM's type-check rows (`crate::typecheck_rows`), allocated on the first
/// ask; the class and field halves of the word are ignored. `0` when the VM
/// keeps no displays (or could not allocate the rows).
pub const WELL_KNOWN_VH_CONST_TYPECHECK_ROWS_TAG: i64 = 0x5680_i64 << 48;
/// JIT round 21, lane hash (NATIVE21-1): `1` when the class named in the
/// word's class half provably inherits `java/lang/Object.hashCode` (see
/// `identity_hash_profiled_class`), else `0`.
pub const WELL_KNOWN_VH_CONST_IDENTITY_HASH_TAG: i64 = 0x5681_i64 << 48;
/// JIT round 21, lane vhcae (`CRATONVM_JIT_VH_CONST_RET_SAME_LOADER`): `1 +`
/// the native id (`ClassLoaderId::to_native_id`) of the DEFINING loader of the
/// class named in the word's class half, `0` when unknown (no such class, or
/// the class manager could not be read without waiting).
pub const WELL_KNOWN_VH_CONST_LOADER_TAG: i64 = 0x5683_i64 << 48;
/// JIT round 21, lane tcarray (`CRATONVM_JIT_TYPECHECK_ARRAY_ARM`): `1 +` the
/// component class id (`array_info.component_class_id`) of the
/// reference-array class named in the word's class half, `0` for anything
/// else (a primitive array class, a non-array class, an unknown class, or a
/// class manager that could not be read without waiting).
pub const WELL_KNOWN_VH_CONST_ARRAY_COMPONENT_TAG: i64 = 0x5684_i64 << 48;
/// The tag half of a [`vh_const_query`] word.
pub(in crate::ir_lower) const WELL_KNOWN_VH_CONST_TAG_MASK: i64 = !0xFFFF_FFFF_FFFF_i64;
/// JIT round 21 wave 1 (lane indy, INDY21-2, `CRATONVM_JIT_IR_CLINIT_BARRIER`):
/// NOT a class id -- a [`vh_const_query`]-shaped word (field half ignored)
/// asking for the address of the VM's init-owner cell of the NOT yet
/// initialized class in the class half: a word holding `1 +` the thread id of
/// the thread whose init claim the class is under, `0` with no claim. `0` =
/// no cell (the class is initialized, unknown, or the VM could not answer
/// without waiting).
pub const WELL_KNOWN_CLINIT_OWNER_CELL_TAG: i64 = 0x564E_i64 << 48;
/// INDY21-2: the statics base cell of `(class, static field)` for a class that
/// is NOT initialized yet -- `jit_resolve_static_base`'s answer without its
/// initialized rule (the System stream fields, an index switched off and the
/// `Boolean` repair still refuse) -- or `0`.
pub const WELL_KNOWN_CLINIT_BASE_CELL_TAG: i64 = 0x564F_i64 << 48;
/// INDY21-2: `1 +` the byte offset of `JvmThread::thread_id` (a `u64`) from the
/// `JvmThread*` the `JIT_THREAD` TLS mirror holds, or `0`.
pub const WELL_KNOWN_JVM_THREAD_ID_OFFSET: i64 = 0x5E55_0060;

/// `CRATONVM_JIT_IR_CLINIT_BARRIER` (JIT round 21 wave 1, lane indy, INDY21-2;
/// **default ON (flipped; `=0` kills)**): may the optimizing tier compile a
/// `getstatic` of a class that is not initialized at compile time as
/// HotSpot's class-initialization barrier -- an inline load while the current
/// thread holds the class's init claim (a read from inside its own
/// `<clinit>`), the `jit_getstatic` helper otherwise? See
/// [`Lowerer::emit_clinit_barrier_getstatic`]. Read per site at compile time.
pub fn ir_clinit_barrier_enabled() -> bool {
    // R22-AB-FLIP: default ON since the JIT r22 opt-in A/B (docs/feature-designs/jit-ir-optimizations.md); `=0` is the kill switch.
    true
}

/// JIT round 21, lane dispatch (`CRATONVM_JIT_TYPECHECK_ROWS`): the base of
/// this VM's type-check rows (`crate::typecheck_rows`), or `None` (switch off,
/// no displays, the VM could not allocate them). The optimizing tier's
/// `Lowerer::typecheck_rows_base` and the single-pass tier's row-first arm
/// (JIT r21 tcp21, `CRATONVM_JIT_SP_TYPECHECK_ROW_FIRST`) both ask through it,
/// so the two tiers bake the same table.
pub(crate) fn typecheck_rows_base_for(helpers: &crate::DirectHelperTable) -> Option<usize> {
    if !crate::typecheck_rows::typecheck_rows_enabled()
        || !vh_const_inline_enabled()
        || !vh_const_subtype_enabled()
    {
        return None;
    }
    let rows = vh_const_ask(
        helpers,
        vh_const_query(WELL_KNOWN_VH_CONST_TYPECHECK_ROWS_TAG, 0, 0),
    );
    usize::try_from(rows)
        .ok()
        .filter(|&r| r != 0 && r % crate::typecheck_rows::ROW_BYTES == 0)
}

/// One constant-`VarHandle` query word: `tag | class_id << 16 | field_index`.
pub const fn vh_const_query(tag: i64, class_id: u32, field_index: u16) -> i64 {
    // Cast: widenings into the disjoint bit fields of one positive word.
    tag | ((class_id as i64) << 16) | (field_index as i64)
}

/// The VM's decode of a [`vh_const_query`] word: `(tag, class id, static field
/// index)` for one of the two tags, `None` for every other well-known code.
pub fn vh_const_query_decode(which: i64) -> Option<(i64, u32, u32)> {
    let tag = which & WELL_KNOWN_VH_CONST_TAG_MASK;
    if tag != WELL_KNOWN_VH_CONST_PLAN_TAG
        && tag != WELL_KNOWN_VH_CONST_REF_ROW_TAG
        && tag != WELL_KNOWN_VH_CONST_DISPLAY_CELL_TAG
        && tag != WELL_KNOWN_VH_CONST_DEPTH_TAG
        && tag != WELL_KNOWN_VH_CONST_IFACE_TAG
        && tag != WELL_KNOWN_TYPECHECK_ARRAY_COMPONENT_TAG
        && tag != WELL_KNOWN_TYPECHECK_MISS_MEMO_TAG
        && tag != WELL_KNOWN_FFM_CONST_ROW_TAG

        && tag != WELL_KNOWN_VH_CONST_TYPECHECK_ROWS_TAG
        && tag != WELL_KNOWN_VH_CONST_IDENTITY_HASH_TAG
        && tag != WELL_KNOWN_VH_CONST_IDENTITY_HASH_TABLE_TAG
        && tag != WELL_KNOWN_VH_CONST_LOADER_TAG
        && tag != WELL_KNOWN_VH_CONST_ARRAY_COMPONENT_TAG
        && tag != WELL_KNOWN_VH_CONST_IDENTITY_EQUALS_TAG
        && tag != WELL_KNOWN_VH_CONST_IDENTITY_EQUALS_TABLE_TAG
        && tag != WELL_KNOWN_VH_CONST_G1_EDGE_FILTER_TAG
        && tag != vh_static_const::WELL_KNOWN_VH_CONST_STATIC_CELL_TAG
    {
        return None;
    }
    // Cast: the two masked fields fit their widths exactly.
    let class_id = ((which >> 16) & 0xFFFF_FFFF) as u32;
    let field_index = (which & 0xFFFF) as u32; // Cast: as above
    Some((tag, class_id, field_index))
}

/// What the VM filed for one `static final VarHandle` field (nio21 wave 11 L5):
/// the handle's coordinate class, the field's slot in that class's field array,
/// its value kind (`I`, `J` or `L` for any reference) and whether the handle is
/// read-only (a `final` field: every write mode throws, so only reads inline).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VhConstPlan {
    pub receiver_class_id: u32,
    pub slot: u16,
    pub kind: u8,
    pub read_only: bool,
}

impl VhConstPlan {
    /// `class_id | slot << 32 | kind << 48 | read_only << 56`. Never `0` for a
    /// plan (the class id is not `0`), which is the "nothing filed" answer.
    pub const fn to_word(self) -> i64 {
        // Cast: widenings into disjoint bit fields; bit 63 stays clear.
        ((self.receiver_class_id as u64)
            | ((self.slot as u64) << 32)
            | ((self.kind as u64) << 48)
            | ((self.read_only as u64) << 56)) as i64
    }

    /// The plan a [`Self::to_word`] word names, `None` for `0` and for any word
    /// this version would not have written (an unknown kind or flag).
    pub const fn from_word(word: i64) -> Option<Self> {
        let w = word as u64; // Cast: bit pattern
        let receiver_class_id = (w & 0xFFFF_FFFF) as u32; // Cast: masked field
        let slot = ((w >> 32) & 0xFFFF) as u16; // Cast: masked field
        let kind = ((w >> 48) & 0xFF) as u8; // Cast: masked field
        let flags = w >> 56;
        if receiver_class_id == 0 || !matches!(kind, b'I' | b'J' | b'L') || flags > 1 {
            return None;
        }
        Some(Self {
            receiver_class_id,
            slot,
            kind,
            read_only: flags == 1,
        })
    }
}

/// A reference field's value-admission facts as the VM publishes them (nio21
/// wave 11 L5): `#[repr(C)]`, owned by the VM for its whole life
/// (`JitRealm::vh_const_plans`), read by the compile through the address
/// [`WELL_KNOWN_VH_CONST_REF_ROW_TAG`] answers.
#[repr(C)]
#[derive(Debug)]
pub struct VhConstRefRow {
    /// [`VH_CONST_REF_UNCHECKED`] | [`VH_CONST_REF_NAME_PROVES`].
    pub flags: u32,
    /// The field's declared class (`VarHandleInstanceFieldPlan::ref_type_class_id`),
    /// `0` for an array-typed field or an unresolved type.
    pub type_class_id: u32,
    /// That class's internal name (UTF-8, not NUL-terminated), valid while the
    /// row is, or `0` when [`VH_CONST_REF_NAME_PROVES`] is clear.
    pub name_ptr: usize,
    pub name_len: usize,
}

/// The field is declared `java/lang/Object`: the natives judge no stored value.
pub const VH_CONST_REF_UNCHECKED: u32 = 1;
/// A call-site descriptor naming the row's class, at a site in the coordinate
/// class, types its value as exactly that class: the class is not an interface
/// and is defined by the coordinate class's loader (or lives in `java/`).
pub const VH_CONST_REF_NAME_PROVES: u32 = 2;

/// [`VhConstRefRow`] as the compile uses it (the name copied out).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct VhConstRefFacts {
    pub(crate) unchecked: bool,
    pub(crate) type_class_id: u32,
    pub(crate) proving_name: Option<String>,
}

/// Ask this compile's VM one constant-`VarHandle` question; `0` without a
/// resolver or a VM.
pub(crate) fn vh_const_ask(helpers: &crate::DirectHelperTable, which: i64) -> i64 {
    let raw = helpers.well_known_class_id;
    let ctx = helpers.static_base_resolver_ctx;
    if raw == 0 || ctx == 0 {
        return 0;
    }
    // SAFETY: as in `ir_integer_cache_high`: the VM fills this field from
    // `jit_well_known_class_id`, an `extern "C" fn(i64, i64) -> i64` with
    // process lifetime.
    let f: crate::direct_helpers::DirectHelperFnWellKnownClassId =
        unsafe { std::mem::transmute(raw) };
    // Cast: `usize` context to the C ABI's i64 parameter.
    //
    // SAFETY: `ctx` is the `SharedVm` published beside `f`; it outlives every
    // compilation it runs.
    unsafe { f(ctx as i64, which) }
}

/// The plan the VM filed for the `static final` field `(class_id,
/// field_index)`, or `None` (switch off, nothing filed, no resolver).
pub(crate) fn vh_const_plan(
    helpers: &crate::DirectHelperTable,
    class_id: u32,
    field_index: usize,
) -> Option<VhConstPlan> {
    if class_id == 0 || !vh_const_inline_enabled() {
        return None;
    }
    let field_index = u16::try_from(field_index).ok()?;
    VhConstPlan::from_word(vh_const_ask(
        helpers,
        vh_const_query(WELL_KNOWN_VH_CONST_PLAN_TAG, class_id, field_index),
    ))
}

/// `CRATONVM_JIT_IR_FFM_CRITICAL_CALL` (JIT round 22, lane memory, FFM22-1
/// step (a); **default ON**, `=0` off): the optimizing tier makes the C call
/// of a constant critical bound FFM downcall itself
/// ([`Lowerer::try_emit_ir_ffm_const_prefix`]). `FfmHot llabsc`: 1 390 -> 33
/// instructions per call (HotSpot 36). Read per site at compile time.
pub fn ir_ffm_critical_call_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_FFM_SEGMENT_CALL` (JIT round 22, lane memory, FFM22-1
/// step (c); **default ON**, `=0` off): a constant bound downcall the compiled
/// caller cannot make itself (segment arguments, an `ADDRESS` or group return)
/// calls its row's helper, the bound row's own call, instead of the dispatch
/// helper and the FFM door (`FfmHot seg` 8 200 -> 7 700 instructions per call,
/// `ptr` 11 700 -> 11 300). Read per site at compile time.
pub fn ir_ffm_segment_call_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_FFM_SEGMENT_INLINE` (JIT round 22, lane memory, FFM22-3;
/// **default ON**, `=0` off): under [`ir_ffm_segment_call_enabled`], a constant bound
/// downcall with ONE native segment argument checks that segment in the
/// compiled caller (its class, `min`, `length`, and its session: confined to
/// this thread and open, or the global one) and calls the row's segment
/// helper, which acquires, calls and releases with no field read of its own
/// ([`Lowerer::emit_ffm_seg_call`]); anything else takes step (c)'s route.
/// `FfmHot` instructions per call, `--real-jdk`: `seg` 7 750 -> 2 200, `segg`
/// 5 320 -> 2 050, `ptr` 11 330 -> 5 560 (HotSpot 93 / 72 / 100). Read per
/// site at compile time (and by the frame scan, which reserves the staged
/// words).
pub fn ir_ffm_segment_inline_enabled() -> bool {
    ir_ffm_segment_call_enabled()
}

/// NOT class ids (JIT round 22, lane memory, FFM22-3): the well-known
/// resolver's selectors for the JDK's `NativeMemorySegmentImpl` (bootstrap;
/// its exact class id) and the instance slot index PLUS ONE of its `long min`,
/// `long length` and `MemorySessionImpl scope`, resolved by name and
/// descriptor through the hierarchy; and of `ConfinedSession`'s
/// `int acquireCount`. `0`: unknown.
pub const WELL_KNOWN_SEGMENT_NATIVE: i64 = 0x5E55_0070;
pub const WELL_KNOWN_SEGMENT_NATIVE_MIN_SLOT: i64 = 0x5E55_0071;
pub const WELL_KNOWN_SEGMENT_NATIVE_LENGTH_SLOT: i64 = 0x5E55_0072;
pub const WELL_KNOWN_SEGMENT_NATIVE_SCOPE_SLOT: i64 = 0x5E55_0073;
pub const WELL_KNOWN_SESSION_CONFINED_ACQUIRE_SLOT: i64 = 0x5E55_0015;

/// What [`Lowerer::emit_ffm_seg_call`] bakes (FFM22-3): COMPACT addresses
/// only (a legacy instance takes step (c)'s route), under the layout epoch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FfmSegInlineLayout {
    pub segment_class: u32,
    pub min: i32,
    pub length: i32,
    /// An 8-byte (wide) reference.
    pub scope: i32,
    pub confined_class: u32,
    pub state: i32,
    pub owner: i32,
    pub acquire: i32,
    pub global_class: u32,
    pub thread_off: i32,
}

/// The COMPACT address of field `index` of `class_id` when a compact layout is
/// registered for it with `size` bytes of storage (`reference`: a wide
/// reference), else `None`.
pub(in crate::ir_lower) fn ffm_seg_compact_disp(class_id: u32, index: usize, size: u32, reference: bool) -> Option<i32> {
    if class_id == 0 || !cratonvm_types::compact_ref_fields_enabled() {
        return None;
    }
    let (body_off, storage) = cratonvm_types::compact_field_disp_storage(class_id, index)?;
    if storage.size_runtime() != size
        || reference != (storage == cratonvm_types::FieldStorageKind::Reference)
    {
        return None;
    }
    i32::try_from(body_off).ok()
}

/// [`FfmSegInlineLayout`] for this compile's VM, or `None`: no resolver, no
/// TLS mirror or thread word, an armed ZGC read barrier (the segment's scope
/// and the session's owner are reference fields), a class or slot the VM
/// cannot name, a field with no compact storage of its width. The confined
/// test is the scoped fold's ([`scoped_session_confined_rows`]).
pub(crate) fn ffm_segment_inline_layout(
    helpers: &crate::DirectHelperTable,
) -> Option<FfmSegInlineLayout> {
    let raw = helpers.well_known_class_id;
    let ctx = helpers.static_base_resolver_ctx;
    if raw == 0 || ctx == 0 {
        return None;
    }
    if crate::x64::jit_thread_tls_disp() == 0 || crate::x64::zgc_read_barrier_blocks_inline_fields()
    {
        return None;
    }
    let thread_off = ir_thread_java_obj_offset(helpers)?;
    // SAFETY: as in `ir_integer_cache_high`.
    let f: crate::direct_helpers::DirectHelperFnWellKnownClassId =
        unsafe { std::mem::transmute(raw) };
    // Cast: `usize` context to the C ABI's i64 parameter.
    //
    // SAFETY: as in `ir_integer_cache_high`.
    let ask = |which: i64| unsafe { f(ctx as i64, which) };
    let class = |which: i64| u32::try_from(ask(which)).ok().filter(|&c| c != 0);
    let slot = |which: i64| usize::try_from(ask(which)).ok()?.checked_sub(1);
    let segment_class = class(WELL_KNOWN_SEGMENT_NATIVE)?;
    let confined_class = class(WELL_KNOWN_SESSION_CONFINED)?;
    let global_class = class(WELL_KNOWN_SESSION_GLOBAL)?;
    Some(FfmSegInlineLayout {
        segment_class,
        min: ffm_seg_compact_disp(
            segment_class,
            slot(WELL_KNOWN_SEGMENT_NATIVE_MIN_SLOT)?,
            8,
            false,
        )?,
        length: ffm_seg_compact_disp(
            segment_class,
            slot(WELL_KNOWN_SEGMENT_NATIVE_LENGTH_SLOT)?,
            8,
            false,
        )?,
        scope: ffm_seg_compact_disp(
            segment_class,
            slot(WELL_KNOWN_SEGMENT_NATIVE_SCOPE_SLOT)?,
            8,
            true,
        )?,
        confined_class,
        state: ffm_seg_compact_disp(
            confined_class,
            slot(WELL_KNOWN_SESSION_CONFINED_STATE_SLOT)?,
            4,
            false,
        )?,
        owner: ffm_seg_compact_disp(
            confined_class,
            slot(WELL_KNOWN_SESSION_CONFINED_OWNER_SLOT)?,
            8,
            true,
        )?,
        acquire: ffm_seg_compact_disp(
            confined_class,
            slot(WELL_KNOWN_SESSION_CONFINED_ACQUIRE_SLOT)?,
            4,
            false,
        )?,
        global_class,
        thread_off,
    })
}

/// `CRATONVM_JIT_IR_FFM_LAZY_CALL` (JIT round 22, lane memory, FFM22-1 step
/// (b); **default ON**, `=0` off): the same for a NON-critical handle, whose
/// lazy in-native transition is made by a thin helper on the ordinary call
/// route ([`Lowerer::emit_ffm_lazy_call`]). `FfmHot llabs`: 1 910 -> ~960
/// instructions per call. Read per site at compile time.
pub fn ir_ffm_lazy_call_enabled() -> bool {
    true
}

/// NOT a class id (JIT round 22, FFM22-1): the well-known resolver's query, a
/// [`vh_const_query`] word, for the compiled-call row of the bound FFM
/// downcall handle a `static final` field holds: the address of an
/// [`FfmConstRow`] the VM owns for its whole life, `0` when there is none.
pub const WELL_KNOWN_FFM_CONST_ROW_TAG: i64 = 0x4646_i64 << 48;

/// [`FfmConstRow::flags`]: the handle was linked `Linker.Option.critical`.
pub const FFM_CONST_CRITICAL: u8 = 1;
/// [`FfmConstRow::flags`] (FFM22-1 step (c)): only the row's helper can make
/// the call (segment arguments, an object return); the row's `params` are
/// unused and its `arity` is the staged slot count after the handle.
pub const FFM_CONST_HELPER_ONLY: u8 = 2;
/// [`FfmConstRow::ret`] (step (c)): a reference result.
pub const FFM_CONST_REF: u8 = 6;
/// [`FfmConstRow`] codes (`panama::COMPILED_CALL_*`, which the VM copies):
/// no value, the low 8 / 16 / 32 bits sign-extended, all 64 bits, the low 16
/// bits zero-extended.
pub const FFM_CONST_VOID: u8 = 0;
/// See [`FFM_CONST_VOID`].
pub const FFM_CONST_S8: u8 = 1;
/// See [`FFM_CONST_VOID`].
pub const FFM_CONST_S16: u8 = 2;
/// See [`FFM_CONST_VOID`].
pub const FFM_CONST_S32: u8 = 3;
/// See [`FFM_CONST_VOID`].
pub const FFM_CONST_W64: u8 = 4;
/// See [`FFM_CONST_VOID`].
pub const FFM_CONST_U16: u8 = 5;

/// The SysV x86-64 integer argument registers, in order.
pub(in crate::ir_lower) const FFM_CONST_SYSV_ARGS: [u8; 6] = [7, 6, 2, 1, 8, 9];

/// What the VM files for the bound FFM downcall handle a `static final` field
/// holds (JIT round 22, FFM22-1): the call the optimizing tier may make for an
/// `invokeExact` of it. Rows are only ever inserted and never freed.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct FfmConstRow {
    /// The C function.
    pub fn_addr: u64,
    /// The C parameter count (at most six).
    pub arity: u8,
    /// Each parameter's widening code.
    pub params: [u8; 6],
    /// The return's code.
    pub ret: u8,
    /// [`FFM_CONST_CRITICAL`].
    pub flags: u8,
    /// Addresses of 8-byte words that must each read its
    /// [`Self::guard_values`] entry for the call to be made (`0`: unused).
    pub guard_words: [u64; 2],
    /// See [`Self::guard_words`].
    pub guard_values: [u64; 2],
    /// A non-critical row's thin helper (FFM22-1 step (b)), the jit-dispatch
    /// ABI with the row's address and the site's `JitInvokeInfo` in place of
    /// the info and the argument count: `i64 helper(vm, row, args_ptr,
    /// info_ptr)`. `0` for a critical row.
    pub call_helper: u64,
    /// The `invokeExact` descriptor of the site the stub was filled for, which
    /// is the handle's type (UTF-8, owned beside the row for the VM's life).
    pub desc_ptr: u64,
    /// See [`Self::desc_ptr`].
    pub desc_len: u64,
    /// JIT round 22 (lane memory, FFM22-3): a [`FFM_CONST_HELPER_ONLY`] row
    /// whose one segment argument the compiled caller may check itself
    /// ([`Lowerer::emit_ffm_seg_call`]): that helper, with step (b)'s ABI and
    /// the four words of [`FFM_SEG_STAGE_EXTRA`] staged after the arguments.
    /// `0` for any other row.
    pub seg_call_helper: u64,
    /// The C parameter index of that segment, or [`FFM_CONST_NO_SEGMENT`];
    /// the row's `params` / `ret` then hold the call's codes
    /// ([`FFM_CONST_REF`] for an `ADDRESS` return).
    pub seg_param: u8,
}

/// [`FfmConstRow::seg_param`]: no segment the compiled caller checks.
pub const FFM_CONST_NO_SEGMENT: u8 = 0xFF;

/// The words [`Lowerer::emit_ffm_seg_call`] stages after the call's arguments
/// for [`FfmConstRow::seg_call_helper`]: the segment's `min`, its `length`,
/// its session when it is a confined one (`0` for the global session) and the
/// compact offset of that session's `acquireCount`.
pub const FFM_SEG_STAGE_EXTRA: usize = 4;

impl FfmConstRow {
    /// The descriptor the row was filed for.
    pub fn descriptor(&self) -> Option<&str> {
        if self.desc_ptr == 0 || self.desc_len == 0 {
            return None;
        }
        // SAFETY: `desc_ptr` / `desc_len` describe the boxed `str` the row's
        // owner keeps beside it for the VM's life (see the type's doc).
        let bytes = unsafe {
            // Cast: a length the VM wrote from a `usize`.
            std::slice::from_raw_parts(self.desc_ptr as *const u8, self.desc_len as usize)
        };
        std::str::from_utf8(bytes).ok()
    }
}

/// The compiled-call row the VM filed for the `static final` field
/// `(class_id, field_index)`, or `None`.
pub(crate) fn ffm_const_row(
    helpers: &crate::DirectHelperTable,
    class_id: u32,
    field_index: u32,
) -> Option<(usize, FfmConstRow)> {
    let field_index = u16::try_from(field_index).ok()?;
    if class_id == 0 {
        return None;
    }
    let addr = vh_const_ask(
        helpers,
        vh_const_query(WELL_KNOWN_FFM_CONST_ROW_TAG, class_id, field_index),
    );
    let addr = usize::try_from(addr)
        .ok()
        .filter(|&a| a != 0 && a % std::mem::align_of::<FfmConstRow>() == 0)?;
    // SAFETY: the VM answers with the address of a boxed `FfmConstRow` it
    // never frees or moves while it lives, and the VM outlives every compile.
    Some((addr, unsafe { *(addr as *const FfmConstRow) }))
}

/// The bytes that widen `reg` in place by an [`FfmConstRow`] code: `MOVSX
/// r64, r8` / `MOVSX r64, r16` / `MOVSXD r64, r32` / `MOVZX r32, r16`; nothing
/// for 64 bits or no value.
pub(in crate::ir_lower) fn ffm_const_widen_bytes(reg: u8, code: u8) -> Vec<u8> {
    let ext = u8::from(reg >= 8);
    let modrm = 0xC0 | ((reg & 7) << 3) | (reg & 7);
    match code {
        FFM_CONST_S8 => vec![0x48 | (ext << 2) | ext, 0x0F, 0xBE, modrm],
        FFM_CONST_S16 => vec![0x48 | (ext << 2) | ext, 0x0F, 0xBF, modrm],
        FFM_CONST_S32 => vec![0x48 | (ext << 2) | ext, 0x63, modrm],
        FFM_CONST_U16 if ext == 0 => vec![0x0F, 0xB7, modrm],
        FFM_CONST_U16 => vec![0x45, 0x0F, 0xB7, modrm],
        _ => Vec::new(),
    }
}

/// The reference-value facts filed beside a reference field's plan, or `None`.
pub(crate) fn vh_const_ref_facts(
    helpers: &crate::DirectHelperTable,
    class_id: u32,
    field_index: usize,
) -> Option<VhConstRefFacts> {
    let field_index = u16::try_from(field_index).ok()?;
    let addr = vh_const_ask(
        helpers,
        vh_const_query(WELL_KNOWN_VH_CONST_REF_ROW_TAG, class_id, field_index),
    );
    let addr = usize::try_from(addr)
        .ok()
        .filter(|&a| a != 0 && a % std::mem::align_of::<VhConstRefRow>() == 0)?;
    // SAFETY: the VM answers with the address of a boxed `VhConstRefRow` it
    // never frees or moves while it lives (`JitRealm::vh_const_plans` only
    // inserts), and the VM outlives every compile it runs.
    let row = unsafe { &*(addr as *const VhConstRefRow) };
    let proving_name =
        if row.flags & VH_CONST_REF_NAME_PROVES != 0 && row.name_ptr != 0 && row.name_len != 0 {
            // SAFETY: `name_ptr`/`name_len` describe the boxed `str` the same row
            // entry owns (see above).
            let bytes =
                unsafe { std::slice::from_raw_parts(row.name_ptr as *const u8, row.name_len) };
            std::str::from_utf8(bytes).ok().map(str::to_owned)
        } else {
            None
        };
    Some(VhConstRefFacts {
        unchecked: row.flags & VH_CONST_REF_UNCHECKED != 0,
        type_class_id: row.type_class_id,
        proving_name,
    })
}

/// What one constant-handle site does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VhConstOp {
    /// `get`, `getVolatile`, `getAcquire`, `getOpaque`.
    Get,
    /// `set`, `setOpaque`, `setRelease` (plain `MOV`); `setVolatile` (`XCHG`).
    Set { volatile: bool },
    /// The read-modify-write modes, as the `Unsafe` prefixes' operation.
    Rmw(crate::direct_helpers::UnsafeRmwOp),
}

/// A `VarHandle` access-mode call site the constant-handle prefixes may serve
/// (nio21 wave 11 L5), from its name and call-site descriptor alone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct VhConstShape {
    pub(crate) op: VhConstOp,
    /// `I`, `J` or `L` (any reference).
    pub(crate) kind: u8,
    /// The value operands' descriptors after the receiver coordinate:
    /// `[]`, `[value]` or `[expected, new]`.
    pub(crate) values: Vec<String>,
    /// The return descriptor.
    pub(crate) ret_desc: String,
    /// Its JIT tag: `V`, `Z`, `I`, `J`, `L` or `[`.
    pub(crate) ret: u8,
}

impl VhConstShape {
    /// Does the site pass `(expected, new)`?
    pub(crate) fn has_expected(&self) -> bool {
        self.values.len() == 2
    }
}

/// `(parameter descriptors, return descriptor)` of a method descriptor.
pub(in crate::ir_lower) fn vh_desc_params(descriptor: &str) -> Option<(Vec<&str>, &str)> {
    let inner = descriptor.strip_prefix('(')?;
    let close = inner.find(')')?;
    let (params, ret) = (&inner[..close], &inner[close + 1..]);
    let b = params.as_bytes();
    let mut out = Vec::new();
    let mut i = 0usize;
    while i < b.len() {
        let start = i;
        while b.get(i) == Some(&b'[') {
            i += 1;
        }
        match b.get(i)? {
            b'L' => i += params[i..].find(';')? + 1,
            b'Z' | b'B' | b'C' | b'S' | b'I' | b'J' | b'F' | b'D' => i += 1,
            _ => return None,
        }
        out.push(&params[start..i]);
    }
    if ret.is_empty() {
        return None;
    }
    Some((out, ret))
}

/// `I`, `J`, or `L` for any reference descriptor; `None` otherwise.
pub(in crate::ir_lower) fn vh_const_kind_of(desc: &str) -> Option<u8> {
    match desc.as_bytes().first()? {
        b'I' if desc.len() == 1 => Some(b'I'),
        b'J' if desc.len() == 1 => Some(b'J'),
        b'L' | b'[' => Some(b'L'),
        _ => None,
    }
}

/// `CRATONVM_JIT_VH_CONST_VOID_RMW=1` (JIT round 21 wave 1, lane updaters,
/// wave 3; default ON (flipped; `=0` kills)): [`vh_const_site_shape`] admits a CAS /
/// compareAndExchange / getAndAdd / getAndSet / getAndBitwise site whose
/// result is discarded (`...)V`, javac's descriptor for an expression
/// statement). The access is the same; the prefix writes no result. Before,
/// every such site kept its call. Read per site at compile time.
pub fn vh_const_void_rmw_enabled() -> bool {
    // R22-AB-FLIP: default ON since the JIT r22 opt-in A/B (docs/feature-designs/jit-ir-optimizations.md); `=0` is the kill switch.
    true
}

/// Recognise a `VarHandle` access-mode call the constant-handle prefixes may
/// serve, by method name and call-site descriptor: one reference coordinate
/// (an instance-field handle's shape), then the mode's values, all of ONE kind
/// -- `I`, `J`, or references -- with a return of the same kind (`Z` for a
/// CAS, `V` for a set). `getAndAdd*` / `getAndBitwise*` are numeric only.
/// Anything else (a boxed or widened operand the handle would convert, a
/// static-field or array-element shape, a float) is `None` and keeps its call.
pub(crate) fn vh_const_site_shape(method: &str, descriptor: &str) -> Option<VhConstShape> {
    use crate::direct_helpers::{UnsafeBitOp, UnsafeRmwOp};
    let op = match method {
        "get" | "getVolatile" | "getAcquire" | "getOpaque" => VhConstOp::Get,
        "set" | "setOpaque" | "setRelease" => VhConstOp::Set { volatile: false },
        "setVolatile" => VhConstOp::Set { volatile: true },
        "compareAndSet"
        | "weakCompareAndSet"
        | "weakCompareAndSetPlain"
        | "weakCompareAndSetAcquire"
        | "weakCompareAndSetRelease" => VhConstOp::Rmw(UnsafeRmwOp::Cas),
        "compareAndExchange" | "compareAndExchangeAcquire" | "compareAndExchangeRelease" => {
            VhConstOp::Rmw(UnsafeRmwOp::Cae)
        }
        "getAndAdd" | "getAndAddAcquire" | "getAndAddRelease" => {
            VhConstOp::Rmw(UnsafeRmwOp::GetAndAdd)
        }
        "getAndSet" | "getAndSetAcquire" | "getAndSetRelease" => {
            VhConstOp::Rmw(UnsafeRmwOp::GetAndSet)
        }
        "getAndBitwiseOr" | "getAndBitwiseOrAcquire" | "getAndBitwiseOrRelease" => {
            VhConstOp::Rmw(UnsafeRmwOp::GetAndBitwise(UnsafeBitOp::Or))
        }
        "getAndBitwiseAnd" | "getAndBitwiseAndAcquire" | "getAndBitwiseAndRelease" => {
            VhConstOp::Rmw(UnsafeRmwOp::GetAndBitwise(UnsafeBitOp::And))
        }
        "getAndBitwiseXor" | "getAndBitwiseXorAcquire" | "getAndBitwiseXorRelease" => {
            VhConstOp::Rmw(UnsafeRmwOp::GetAndBitwise(UnsafeBitOp::Xor))
        }
        _ => return None,
    };
    let (params, ret_desc) = vh_desc_params(descriptor)?;
    let (coord, values) = params.split_first()?;
    if !coord.starts_with('L') {
        return None;
    }
    let n_values = match op {
        VhConstOp::Get => 0,
        VhConstOp::Rmw(UnsafeRmwOp::Cas | UnsafeRmwOp::Cae) => 2,
        VhConstOp::Set { .. } | VhConstOp::Rmw(_) => 1,
    };
    if values.len() != n_values {
        return None;
    }
    let kind = match values.first() {
        Some(v) => vh_const_kind_of(v)?,
        None => vh_const_kind_of(ret_desc)?,
    };
    if values.iter().any(|v| vh_const_kind_of(v) != Some(kind)) {
        return None;
    }
    // JIT round 21 wave 1, lane updaters (`CRATONVM_JIT_VH_CONST_VOID_RMW`,
    // opt-in): an RMW whose result is discarded is an expression statement,
    // which javac compiles to a `...)V` descriptor (`COUNT.getAndAdd(this, 1);`,
    // `STATE.compareAndSet(this, a, b);` with the answer unused).
    let void_rmw = matches!(op, VhConstOp::Rmw(_)) && ret_desc == "V" && vh_const_void_rmw_enabled();
    let ret_ok = match op {
        VhConstOp::Set { .. } => ret_desc == "V",
        VhConstOp::Rmw(UnsafeRmwOp::Cas) => ret_desc == "Z" || void_rmw,
        VhConstOp::Rmw(_) => vh_const_kind_of(ret_desc) == Some(kind) || void_rmw,
        VhConstOp::Get => vh_const_kind_of(ret_desc) == Some(kind),
    };
    if !ret_ok {
        return None;
    }
    if kind == b'L'
        && matches!(
            op,
            VhConstOp::Rmw(UnsafeRmwOp::GetAndAdd | UnsafeRmwOp::GetAndBitwise(_))
        )
    {
        return None;
    }
    let ret = match ret_desc.as_bytes().first()? {
        b'V' => b'V',
        b'Z' => b'Z',
        b'I' => b'I',
        b'J' => b'J',
        b'L' => b'L',
        b'[' => b'[',
        _ => return None,
    };
    Some(VhConstShape {
        op,
        kind,
        values: values.iter().map(|v| (*v).to_owned()).collect(),
        ret_desc: ret_desc.to_owned(),
        ret,
    })
}

/// How the prefix admits one reference VALUE operand.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VhRefValueCheck {
    /// No run-time test (a primitive, an `Object` field, a statically typed value).
    Admitted,
    /// Only `null` is admitted inline (an array-typed or unresolved field type).
    NullOnly,
    /// `null`, or a plain object of exactly this class.
    ExactClass(u32),
    /// nio21 wave 12 L2: `null`, or a plain object of this class or of any
    /// loaded subclass, read off the VM's supers display (class types only).
    Subtype(u32, VhSubtypeDisplay),
}

/// nio21 wave 12 L2 (`r21-varhandle-inline-declines-subclass-receivers-and-subtype-values-FIXED-20261012.md`):
/// a JIT-readable supers display, HotSpot's primary-supers check.
///
/// `cell` is the address of the VM's display cell
/// (`JitRealm::supers_displays`, `vm/src/vm/realms/jit_realm.rs`): one word
/// holding the current table, `[cap: u64, entry[0..cap]: u64]`, where `entry[c]`
/// is `0` or the address of class `c`'s display, `[len: u32, ids: u32 * len]`,
/// its superclass chain from `java/lang/Object` (index 0) down to `c` itself.
/// A class `C` is a subclass of a class `K` at depth `d` exactly when
/// `display(C).len > d && display(C).ids[d] == K`. The VM publishes a class's
/// display when its `<clinit>` completes (before which it has no instances
/// the prefixes could see), never changes one, and keeps every table and
/// display for its life, so a reader racing a growth sees an older table that
/// merely lacks the newest classes: a decline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct VhSubtypeDisplay {
    pub(crate) cell: usize,
    pub(crate) depth: u32,
    /// nio21 wave 14 L4: the class asked about is an INTERFACE, which has no
    /// display of its own: a candidate is tested against the interface set
    /// that follows ITS display (`[n, ifaces...]` at `4 + 4 * len`), and
    /// `depth` is unused.
    pub(crate) interface: bool,
}

/// `CRATONVM_JIT_VH_CONST_SUBTYPE` (nio21 wave 12 L2, **default ON**, `=0`
/// keeps the wave-11 exact-class screens): the constant-`VarHandle` prefixes
/// admit a receiver of a subclass of the coordinate class, and a stored value
/// of a subclass of the field's class, through the VM's supers display. Read
/// per site; the VM publishes displays only while it is on.
pub fn vh_const_subtype_enabled() -> bool {
    true
}

/// The supers-display facts for `class_id` in this compile's VM, or `None`
/// (switch off, no display published for the class -- an interface, or a class
/// not yet initialised, which then has no instance either -- or no VM).
pub(crate) fn vh_const_display(
    helpers: &crate::DirectHelperTable,
    class_id: u32,
) -> Option<VhSubtypeDisplay> {
    if class_id == 0 || !vh_const_inline_enabled() || !vh_const_subtype_enabled() {
        return None;
    }
    let cell = || {
        let cell = vh_const_ask(
            helpers,
            vh_const_query(WELL_KNOWN_VH_CONST_DISPLAY_CELL_TAG, 0, 0),
        );
        usize::try_from(cell)
            .ok()
            .filter(|&c| c != 0 && c % std::mem::align_of::<u64>() == 0)
    };
    let depth1 = vh_const_ask(
        helpers,
        vh_const_query(WELL_KNOWN_VH_CONST_DEPTH_TAG, class_id, 0),
    );
    if let Some(depth) = u32::try_from(depth1).ok().and_then(|d| d.checked_sub(1)) {
        // A display's ids sit at `4 + 4 * depth`, a disp32.
        if depth > 0x0FFF_FFFF {
            return None;
        }
        return Some(VhSubtypeDisplay {
            cell: cell()?,
            depth,
            interface: false,
        });
    }
    // nio21 wave 14 L4: an interface answers through the interface sets.
    if vh_const_iface_enabled()
        && vh_const_ask(
            helpers,
            vh_const_query(WELL_KNOWN_VH_CONST_IFACE_TAG, class_id, 0),
        ) == 1
    {
        return Some(VhSubtypeDisplay {
            cell: cell()?,
            depth: 0,
            interface: true,
        });
    }
    None
}

/// JIT round 21, lane tcarray (`CRATONVM_JIT_TYPECHECK_ARRAY_ARM`): the
/// component class id of the reference-array class `array_class_id` in this
/// compile's VM (`Some(0)` for `java/lang/Object[]`), or `None` (not a
/// reference-array class, unknown, no VM).
pub(crate) fn vh_const_array_component(
    helpers: &crate::DirectHelperTable,
    array_class_id: u32,
) -> Option<u32> {
    if array_class_id == 0 {
        return None;
    }
    let component1 = vh_const_ask(
        helpers,
        vh_const_query(WELL_KNOWN_VH_CONST_ARRAY_COMPONENT_TAG, array_class_id, 0),
    );
    u32::try_from(component1)
        .ok()
        .and_then(|c| c.checked_sub(1))
}

/// `CRATONVM_JIT_VH_CONST_IFACE` (nio21 wave 14 L4, **default ON**, `=0` keeps
/// the exact-class screen for an interface-typed field): a stored / expected
/// value of an interface-typed `VarHandle` field is admitted inline when its
/// class's display lists the interface. Read per site.
pub fn vh_const_iface_enabled() -> bool {
    true
}

/// The published reference-store barrier gates (`x64::objects::ref_store_gates_of`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct VhRefGates {
    pub(crate) pre: usize,
    pub(crate) post: usize,
    pub(crate) floor: usize,
    pub(crate) mask: Option<u8>,
}

/// Everything the shared bytes need for one admitted site.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct VhConstCore {
    pub(crate) op: VhConstOp,
    pub(crate) kind: u8,
    /// The field's absolute compact displacement.
    pub(crate) disp: i32,
    /// The coordinate class the receiver must be (exactly, or a subclass of it
    /// through [`Self::receiver_display`]).
    pub(crate) class_id: u32,
    /// nio21 wave 12 L2: the coordinate class's supers display; `None` keeps
    /// the exact-class receiver screen.
    pub(crate) receiver_display: Option<VhSubtypeDisplay>,
    /// The header's class-id displacement (`JitRuntimeHelpers::class_id_offset_in_obj`).
    pub(crate) cid_off: i32,
    /// R10's screen (a CAS / compareAndExchange `expected`).
    pub(crate) expected_check: VhRefValueCheck,
    /// RDX's screen (the stored value).
    pub(crate) value_check: VhRefValueCheck,
    /// The barrier gates of a reference store; `None` for every other access.
    pub(crate) gates: Option<VhRefGates>,
    /// JIT round 21, lane g1vh: G1's plan for a reference store instead
    /// ([`VhG1Gates`]); `None` for every other access and collector.
    pub(crate) g1: Option<VhG1Gates>,
    /// JIT round 21 wave 1, lane updaters (`CRATONVM_JIT_VH_CONST_CAST_RETURN`):
    /// RAX's screen AFTER a reference `Get` whose site returns a class
    /// narrower than the field (`(Integer) VH.getAcquire(h)` compiles to a
    /// `...)Ljava/lang/Integer;` descriptor): the invoker's cast. `Admitted`
    /// everywhere else ([`vh_const_core_cast_return`] sets it).
    pub(crate) result_check: VhRefValueCheck,
}

/// The field's absolute compact displacement for `plan`, or `None` when the
/// class has no compact layout, or its storage for the slot is not exactly the
/// plan's kind at the width the access uses (4, 8, or an 8-byte bare pointer).
pub(crate) fn vh_const_field_disp(plan: VhConstPlan) -> Option<i32> {
    use cratonvm_types::FieldStorageKind;
    if !cratonvm_types::compact_ref_fields_enabled() || !cratonvm_types::single_layout_domain() {
        return None;
    }
    let (disp, storage) =
        cratonvm_types::compact_field_disp_storage(plan.receiver_class_id, usize::from(plan.slot))?;
    let (want, width) = match plan.kind {
        b'I' => (FieldStorageKind::Int, 4),
        b'J' => (FieldStorageKind::Long, 8),
        b'L' => (FieldStorageKind::Reference, 8),
        _ => return None,
    };
    if storage != want || storage.size_runtime() != width || disp % width as usize != 0 {
        return None;
    }
    i32::try_from(disp).ok().filter(|&d| d > 0)
}

/// Decide, at compile time, whether one site is served and how (nio21 wave 11
/// L5). `caller_class_id` is the class the call site lives in
/// (`JitInvokeInfo::declaring_class_id`); `gates` / `mask` the published
/// reference-store plan. `None` keeps the call.
///
/// JIT round 21, lane vhcae: both tiers now call [`vh_const_core_with`] (the
/// single-pass prefix too, for its return proof); this coordinate-class-only
/// form stays for the unit tests that pin it.
#[cfg_attr(not(test), allow(dead_code))]
#[allow(clippy::too_many_arguments)]
pub(crate) fn vh_const_core(
    shape: &VhConstShape,
    plan: VhConstPlan,
    ref_facts: Option<&VhConstRefFacts>,
    caller_class_id: u32,
    gates: Option<(usize, usize, usize)>,
    mask: Option<u8>,
    cid_off: i32,
    display: &dyn Fn(u32) -> Option<VhSubtypeDisplay>,
) -> Option<VhConstCore> {
    vh_const_core_with(
        shape,
        plan,
        ref_facts,
        caller_class_id,
        caller_class_id,
        VhStoreBarriers::Gates(gates, mask),
        cid_off,
        display,
    )
}

/// [`vh_const_core`] against either barrier plan (JIT round 21, lane g1vh):
/// a reference write mode is built with the published gates
/// ([`VhConstCore::gates`]) or with G1's ([`VhConstCore::g1`]); every other
/// screen is the same.
///
/// JIT round 21, lane vhcae: `ret_caller_class_id` is the class whose loader
/// names a reference RETURN ([`vh_const_ret_proof_caller`]; pass
/// `caller_class_id` for the coordinate-class-only proof); the value
/// operands keep the `caller_class_id` proof.
pub(crate) fn vh_const_core_with(
    shape: &VhConstShape,
    plan: VhConstPlan,
    ref_facts: Option<&VhConstRefFacts>,
    caller_class_id: u32,
    ret_caller_class_id: u32,
    store: VhStoreBarriers,
    cid_off: i32,
    display: &dyn Fn(u32) -> Option<VhSubtypeDisplay>,
) -> Option<VhConstCore> {
    if shape.kind != plan.kind || (plan.read_only && shape.op != VhConstOp::Get) {
        return None;
    }
    let disp = vh_const_field_disp(plan)?;
    // nio21 wave 12 L2: a subclass instance holds the field at the SAME
    // displacement -- a parent's compact layout is an exact prefix of every
    // child's (`classloading::Class::build_compact_layout`, the parent-prefix
    // property), and a subclass whose layout could not keep it is not compact
    // at all (a compatibility stub in its chain), so the COMPACT test declines
    // it.
    let mut core = VhConstCore {
        op: shape.op,
        kind: shape.kind,
        disp,
        class_id: plan.receiver_class_id,
        receiver_display: display(plan.receiver_class_id).filter(|d| !d.interface),
        cid_off,
        expected_check: VhRefValueCheck::Admitted,
        value_check: VhRefValueCheck::Admitted,
        gates: None,
        g1: None,
        result_check: VhRefValueCheck::Admitted,
    };
    if shape.kind != b'L' {
        return Some(core);
    }
    if !vh_const_ref_inline_enabled() || crate::x64::narrow_oops_block_inline_fields() {
        return None;
    }
    let facts = ref_facts?;
    let proves = |desc: &str| {
        caller_class_id != 0
            && caller_class_id == plan.receiver_class_id
            && facts.proving_name.as_deref().is_some_and(|name| {
                desc.strip_prefix('L').and_then(|d| d.strip_suffix(';')) == Some(name)
            })
    };
    // A reference result reaches the caller typed as the site's return: only
    // `Object`, or the field's own class where the field can hold nothing else.
    if !vh_const_ref_return_proved(shape, plan.receiver_class_id, facts, ret_caller_class_id) {
        return None;
    }
    let check = |desc: &str| {
        if facts.unchecked || proves(desc) {
            VhRefValueCheck::Admitted
        } else if facts.type_class_id != 0 {
            // nio21 wave 12 L2: a subclass value through the display; the
            // VM publishes none for an interface, which keeps the exact test.
            match display(facts.type_class_id) {
                Some(d) => VhRefValueCheck::Subtype(facts.type_class_id, d),
                None => VhRefValueCheck::ExactClass(facts.type_class_id),
            }
        } else {
            VhRefValueCheck::NullOnly
        }
    };
    match shape.values.as_slice() {
        [] => {}
        [value] => core.value_check = check(value),
        [expected, value] => {
            core.expected_check = check(expected);
            core.value_check = check(value);
        }
        _ => return None,
    }
    if shape.op != VhConstOp::Get {
        match store {
            VhStoreBarriers::Gates(gates, mask) => {
                let (pre, post, floor) = gates?;
                if mask.is_none() && floor == 0 {
                    return None;
                }
                core.gates = Some(VhRefGates {
                    pre,
                    post,
                    floor,
                    mask,
                });
            }
            // JIT round 21, lane g1vh: every reference write mode the site
            // recogniser admits (set*, CAS, compareAndExchange*, getAndSet*)
            // is screened by `vh_g1_store_screen`.
            VhStoreBarriers::G1(g1) => core.g1 = Some(g1),
        }
    }
    Some(core)
}

/// [`vh_const_core_with`]'s return screen (JIT round 21, lane vhcae, moved
/// out unchanged but for the proving class): a site with no reference return,
/// an `Object` return, or a return naming the field's declared class read in
/// a class whose loader is the coordinate class's (`ret_caller_class_id ==
/// receiver_class_id`, see [`vh_const_ret_proof_caller`]) hands the value
/// over as is; anything else keeps the call.
pub(crate) fn vh_const_ref_return_proved(
    shape: &VhConstShape,
    receiver_class_id: u32,
    facts: &VhConstRefFacts,
    ret_caller_class_id: u32,
) -> bool {
    if !matches!(shape.ret, b'L' | b'[') || shape.ret_desc == "Ljava/lang/Object;" {
        return true;
    }
    ret_caller_class_id != 0
        && ret_caller_class_id == receiver_class_id
        && facts.proving_name.as_deref().is_some_and(|name| {
            shape
                .ret_desc
                .strip_prefix('L')
                .and_then(|d| d.strip_suffix(';'))
                == Some(name)
        })
}

/// nio21 wave 12 L2: would the optimizing tier's constant-`VarHandle` prefix
/// be emitted for the site `name` + `desc` (living in `caller_class_id`) if
/// its handle operand is the `getstatic` of `(static_class, static_field)`?
/// Every compile-time screen of [`Lowerer::try_emit_ir_vh_const_prefix`]
/// except the graph's: the planner (`jit/src/lib.rs`
/// `ir_vh_const_site_admitted`) asks it before admitting a method it would
/// otherwise hold out of this tier for its `VarHandle` sites.
pub(crate) fn vh_const_site_served(
    helpers: &crate::DirectHelperTable,
    runtime: &JitRuntimeHelpers,
    name: &str,
    desc: &str,
    static_class: u32,
    static_field: usize,
    caller_class_id: u32,
) -> bool {
    if crate::x64::zgc_read_barrier_blocks_inline_fields() {
        return false;
    }
    let Some(shape) = vh_const_site_shape(name, desc) else {
        return false;
    };
    let Some(plan) = vh_const_plan(helpers, static_class, static_field) else {
        return false;
    };
    let facts = if plan.kind == b'L' {
        match vh_const_ref_facts(helpers, static_class, static_field) {
            Some(facts) => Some(facts),
            None => return false,
        }
    } else {
        None
    };
    // JIT round 21, lane g1vh: the prefix's own choice of barrier plan
    // (`Lowerer::try_emit_ir_vh_const_prefix`), so a G1 reference write the
    // prefix serves no longer holds its method out of this tier.
    let gates = crate::x64::ref_store_gates_of(runtime);
    let store = match vh_g1_gates(
        gates,
        runtime.g1_barrier_addr,
        runtime.ref_store_pre_gate,
        runtime.g1_post_write_barrier,
    ) {
        Some(g1) => VhStoreBarriers::G1(g1),
        None => VhStoreBarriers::Gates(gates, crate::x64::ref_store_post_skip_mask_of(runtime)),
    };
    vh_const_core_cast_return(
        helpers,
        &shape,
        plan,
        facts.as_ref(),
        caller_class_id,
        // JIT round 21, lane vhcae: the prefix's own return proof.
        vh_const_ret_proof_caller(helpers, caller_class_id, plan.receiver_class_id),
        store,
        0,
    )
    .is_some()
}

/// A byte sequence with rel32 decline edges and local forward jumps.
#[derive(Default)]
pub(in crate::ir_lower) struct VhConstAsm {
    pub(in crate::ir_lower) b: Vec<u8>,
    pub(in crate::ir_lower) decline: Vec<usize>,
}

impl VhConstAsm {
    pub(in crate::ir_lower) fn emit(&mut self, bytes: &[u8]) {
        self.b.extend_from_slice(bytes);
    }

    pub(in crate::ir_lower) fn rel32_field(&mut self) -> usize {
        let at = self.b.len();
        self.b.extend_from_slice(&[0, 0, 0, 0]);
        at
    }

    /// `Jcc rel32` to the decline target.
    pub(in crate::ir_lower) fn jcc_decline(&mut self, cc: u8) {
        self.emit(&[0x0F, cc]);
        let at = self.rel32_field();
        self.decline.push(at);
    }

    /// `JMP rel32` to the decline target.
    pub(in crate::ir_lower) fn jmp_decline(&mut self) {
        self.emit(&[0xE9]);
        let at = self.rel32_field();
        self.decline.push(at);
    }

    /// `Jcc rel32` forward inside the sequence; bind it with [`Self::bind`].
    pub(in crate::ir_lower) fn jcc_forward(&mut self, cc: u8) -> usize {
        self.emit(&[0x0F, cc]);
        self.rel32_field()
    }

    pub(in crate::ir_lower) fn bind(&mut self, at: usize) {
        // Cast: a forward distance inside one short sequence.
        let rel = (self.b.len() - (at + 4)) as i32;
        self.b[at..at + 4].copy_from_slice(&rel.to_le_bytes());
    }
}

/// The screen of one reference value operand in RDX (`r10 == false`) or R10.
/// `CRATONVM_JIT_VH_CONST_CAST_RETURN=1` (JIT round 21 wave 1, lane
/// updaters, wave 3; default ON (flipped; `=0` kills)): a constant-`VarHandle` reference
/// `get` / `getVolatile` / `getAcquire` / `getOpaque` whose site returns a
/// class NARROWER than the field takes the prefix with the invoker's cast as
/// a screen on the loaded value (null, or an instance of the site's return
/// class through its supers display; anything else declines to the call,
/// which throws the `ClassCastException`). javac gives a signature-polymorphic
/// call in a cast context the cast type as its return
/// (`((Integer) RESULT.getAcquire(h))` is `(LR21CfChain;)Ljava/lang/Integer;`),
/// and [`vh_const_core`] refused every such site, while the same handle's
/// `compareAndSet` (an `Object`-typed result) inlined: `R21CfChain`
/// vh-get-control 425 ns against HotSpot's 0. Read per site at compile time.
pub fn vh_const_cast_return_enabled() -> bool {
    // R22-AB-FLIP: default ON since the JIT r22 opt-in A/B (docs/feature-designs/jit-ir-optimizations.md); `=0` is the kill switch.
    true
}

/// The class id of `name` for `caller_class_id` through the VM's
/// [`WELL_KNOWN_VH_CONST_CLASS_BY_NAME_TAG`] query, or `None` (no resolver,
/// not loaded, not unique, or an address the tag word cannot carry).
pub(crate) fn vh_const_class_by_name(
    helpers: &crate::DirectHelperTable,
    name: &str,
    caller_class_id: u32,
) -> Option<u32> {
    if caller_class_id == 0 || name.is_empty() {
        return None;
    }
    let query = VhConstClassByName {
        name_ptr: name.as_ptr(),
        name_len: name.len(),
        caller_class_id,
    };
    let addr = &query as *const VhConstClassByName as usize; // Cast: an address
    if addr >> 48 != 0 {
        return None;
    }
    // Cast: an address below 2^48, OR-ed under the tag.
    let id = vh_const_ask(helpers, WELL_KNOWN_VH_CONST_CLASS_BY_NAME_TAG | addr as i64);
    u32::try_from(id).ok().filter(|&id| id != 0)
}

/// [`vh_const_core_with`] (JIT round 21's G1 store plan and return proof),
/// and for a reference `Get` it refuses only because the
/// site's return class is narrower than the field
/// ([`vh_const_cast_return_enabled`]), the same access with the cast as
/// [`VhConstCore::result_check`]: the core is built for an `Object`-returning
/// twin of the site (which [`vh_const_core`] admits), then the screen is set
/// from the return class -- `Subtype` through its display, `ExactClass`
/// without one, `NullOnly` for an array return. A return class the VM cannot
/// name uniquely keeps the refusal. Writes into no other field.
#[allow(clippy::too_many_arguments)]
pub(crate) fn vh_const_core_cast_return(
    helpers: &crate::DirectHelperTable,
    shape: &VhConstShape,
    plan: VhConstPlan,
    ref_facts: Option<&VhConstRefFacts>,
    caller_class_id: u32,
    ret_caller_class_id: u32,
    store: VhStoreBarriers,
    cid_off: i32,
) -> Option<VhConstCore> {
    let display = |cid| vh_const_display(helpers, cid);
    if let Some(core) = vh_const_core_with(
        shape,
        plan,
        ref_facts,
        caller_class_id,
        ret_caller_class_id,
        store,
        cid_off,
        &display,
    ) {
        return Some(core);
    }
    // JIT round 21 wave 1, lane updaters (UPDATERS21-6,
    // `CRATONVM_JIT_VH_CONST_CAST_RMW`): a reference getAndSet in a cast
    // context too, its cast decided BEFORE the swap
    // (`vh_const_cast_get_and_set_bytes`).
    // JIT round 21 wave 22, lane vhconst (UPDATERS21-6 completed,
    // `CRATONVM_JIT_VH_CONST_CAST_CAE`): and a reference compareAndExchange
    // in a cast context, its witness screened before any write
    // (`vh_const_cast_compare_and_exchange_bytes`).
    let mode_ok = match shape.op {
        VhConstOp::Get => vh_const_cast_return_enabled(),
        VhConstOp::Rmw(crate::direct_helpers::UnsafeRmwOp::GetAndSet) => vh_const_cast_rmw_enabled(),
        VhConstOp::Rmw(crate::direct_helpers::UnsafeRmwOp::Cae) => vh_const_cast_cae_enabled(),
        _ => false,
    };
    if !mode_ok
        || shape.kind != b'L'
        || !matches!(shape.ret, b'L' | b'[')
        || shape.ret_desc == "Ljava/lang/Object;"
    {
        return None;
    }
    let check = if shape.ret == b'[' {
        VhRefValueCheck::NullOnly
    } else {
        let name = shape
            .ret_desc
            .strip_prefix('L')
            .and_then(|d| d.strip_suffix(';'))?;
        let cid = vh_const_class_by_name(helpers, name, caller_class_id)?;
        match display(cid) {
            Some(d) => VhRefValueCheck::Subtype(cid, d),
            None => VhRefValueCheck::ExactClass(cid),
        }
    };
    let widened = VhConstShape {
        ret_desc: "Ljava/lang/Object;".to_owned(),
        ret: b'L',
        ..shape.clone()
    };
    let mut core = vh_const_core_with(
        &widened,
        plan,
        ref_facts,
        caller_class_id,
        caller_class_id,
        store,
        cid_off,
        &display,
    )?;
    core.result_check = check;
    Some(core)
}

/// [`vh_const_value_screen`] for RAX, the loaded reference of a `Get`
/// (JIT round 21 wave 1, lane updaters): fall through for an admitted value,
/// decline otherwise. Writes RCX, R11 and the flags; RAX is preserved.
pub(in crate::ir_lower) fn vh_const_result_screen(a: &mut VhConstAsm, check: VhRefValueCheck, cid_off: i32) {
    const KIND_TAGS: u8 = cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8; // Cast: disp8
    const _: () = assert!(cratonvm_types::KIND_TAGS_BYTE_OFFSET <= 127);
    match check {
        VhRefValueCheck::Admitted => {}
        VhRefValueCheck::NullOnly => {
            a.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
            a.jcc_decline(0x85); // JNZ: only null is served
        }
        VhRefValueCheck::ExactClass(class_id) => {
            a.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
            let null_ok = a.jcc_forward(0x84); // JZ: null passes every cast
            // TEST BYTE [RAX + KIND_TAGS], KIND_TAG_BYTE_MASK: a plain object.
            a.emit(&[0xF6, 0x40, KIND_TAGS, cratonvm_types::KIND_TAG_BYTE_MASK]);
            a.jcc_decline(0x85);
            a.emit(&[0x81, 0xB8]); // CMP DWORD [RAX + cid_off], class_id
            a.emit(&cid_off.to_le_bytes());
            a.emit(&class_id.to_le_bytes());
            a.jcc_decline(0x85);
            a.bind(null_ok);
        }
        VhRefValueCheck::Subtype(class_id, display) => {
            a.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
            let null_ok = a.jcc_forward(0x84); // JZ: null passes every cast
            a.emit(&[0xF6, 0x40, KIND_TAGS, cratonvm_types::KIND_TAG_BYTE_MASK]);
            a.jcc_decline(0x85); // JNZ: an array (or anything not plain) takes the call
            a.emit(&[0x8B, 0x88]); // MOV ECX, DWORD [RAX + cid_off] (zero-extends RCX)
            a.emit(&cid_off.to_le_bytes());
            let exact = vh_const_class_test(a, class_id);
            if display.interface {
                vh_const_iface_test(a, display, class_id);
            } else {
                vh_const_display_test(a, display, class_id);
            }
            a.bind(exact);
            a.bind(null_ok);
        }
    }
}

pub(in crate::ir_lower) fn vh_const_value_screen(a: &mut VhConstAsm, r10: bool, check: VhRefValueCheck, cid_off: i32) {
    const KIND_TAGS: u8 = cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8; // Cast: disp8
    const _: () = assert!(cratonvm_types::KIND_TAGS_BYTE_OFFSET <= 127);
    let test: &[u8] = if r10 {
        &[0x4D, 0x85, 0xD2]
    } else {
        &[0x48, 0x85, 0xD2]
    };
    match check {
        VhRefValueCheck::Admitted => {}
        VhRefValueCheck::NullOnly => {
            a.emit(test); // TEST r, r
            a.jcc_decline(0x85); // JNZ: only null is served
        }
        VhRefValueCheck::ExactClass(class_id) => {
            a.emit(test); // TEST r, r
            let null_ok = a.jcc_forward(0x84); // JZ: null is admitted
            if r10 {
                a.emit(&[0x41]);
            }
            // TEST BYTE [r + KIND_TAGS], KIND_TAG_BYTE_MASK: a plain object.
            a.emit(&[0xF6, 0x42, KIND_TAGS, cratonvm_types::KIND_TAG_BYTE_MASK]);
            a.jcc_decline(0x85);
            if r10 {
                a.emit(&[0x41]);
            }
            // CMP DWORD [r + cid_off], class_id
            a.emit(&[0x81, 0xBA]);
            a.emit(&cid_off.to_le_bytes());
            a.emit(&class_id.to_le_bytes());
            a.jcc_decline(0x85);
            a.bind(null_ok);
        }
        VhRefValueCheck::Subtype(class_id, display) => {
            a.emit(test); // TEST r, r
            let null_ok = a.jcc_forward(0x84); // JZ: null is admitted
            if r10 {
                a.emit(&[0x41]);
            }
            a.emit(&[0xF6, 0x42, KIND_TAGS, cratonvm_types::KIND_TAG_BYTE_MASK]);
            a.jcc_decline(0x85); // JNZ: an array (or anything not plain) takes the call
            if r10 {
                a.emit(&[0x41]);
            }
            // MOV ECX, DWORD [r + cid_off] (zero-extends RCX)
            a.emit(&[0x8B, 0x8A]);
            a.emit(&cid_off.to_le_bytes());
            let exact = vh_const_class_test(a, class_id);
            if display.interface {
                // nio21 wave 14 L4: an interface-typed field.
                vh_const_iface_test(a, display, class_id);
            } else {
                vh_const_display_test(a, display, class_id);
            }
            a.bind(exact);
            a.bind(null_ok);
        }
    }
}

/// nio21 wave 14 L4: with RCX = a plain object's class id (zero-extended),
/// fall through when that class's display lists the interface `iface_id` in
/// its transitive interface set (see [`VhSubtypeDisplay::interface`]), else
/// decline. Writes RCX, R11 and the flags.
///
/// ```text
///   MOV R11, imm64 cell ; MOV R11, [R11] ; TEST R11, R11 ; JZ decline
///   CMP RCX, [R11] ; JAE decline
///   MOV R11, [R11 + RCX*8 + 8] ; TEST R11, R11 ; JZ decline   ; no display
///   MOV ECX, [R11]                       ; len
///   LEA R11, [R11 + RCX*4 + 4]           ; -> n
///   MOV ECX, [R11]                       ; n
/// top:
///   TEST ECX, ECX ; JZ decline
///   CMP DWORD [R11 + RCX*4], iface_id ; JE found
///   DEC ECX ; JMP top
/// found:
/// ```
pub(in crate::ir_lower) fn vh_const_iface_test(a: &mut VhConstAsm, display: VhSubtypeDisplay, iface_id: u32) {
    a.emit(&[0x49, 0xBB]); // MOV R11, imm64
    a.emit(&(display.cell as u64).to_le_bytes()); // Cast: a data address
    a.emit(&[0x4D, 0x8B, 0x1B]); // MOV R11, [R11]
    a.emit(&[0x4D, 0x85, 0xDB]); // TEST R11, R11
    a.jcc_decline(0x84);
    a.emit(&[0x49, 0x3B, 0x0B]); // CMP RCX, [R11]
    a.jcc_decline(0x83); // JAE
    a.emit(&[0x4D, 0x8B, 0x5C, 0xCB, 0x08]); // MOV R11, [R11 + RCX*8 + 8]
    a.emit(&[0x4D, 0x85, 0xDB]); // TEST R11, R11
    a.jcc_decline(0x84);
    a.emit(&[0x41, 0x8B, 0x0B]); // MOV ECX, [R11] (zero-extends RCX)
    a.emit(&[0x4D, 0x8D, 0x5C, 0x8B, 0x04]); // LEA R11, [R11 + RCX*4 + 4]
    a.emit(&[0x41, 0x8B, 0x0B]); // MOV ECX, [R11]
    let top = a.b.len();
    a.emit(&[0x85, 0xC9]); // TEST ECX, ECX
    a.jcc_decline(0x84); // JZ: not in the set
    a.emit(&[0x41, 0x81, 0x3C, 0x8B]); // CMP DWORD [R11 + RCX*4], imm32
    a.emit(&iface_id.to_le_bytes());
    let found = a.jcc_forward(0x84); // JE
    a.emit(&[0xFF, 0xC9]); // DEC ECX
    a.emit(&[0xE9]); // JMP top
                     // Cast: a backward distance inside one short sequence.
    let rel = top as i64 - (a.b.len() as i64 + 4);
    a.emit(&(rel as i32).to_le_bytes());
    a.bind(found);
}

/// `CMP ECX, class_id ; JE <returned>`: the exact-class hit of a subtype
/// screen, bound by the caller past [`vh_const_display_test`].
pub(in crate::ir_lower) fn vh_const_class_test(a: &mut VhConstAsm, class_id: u32) -> usize {
    a.emit(&[0x81, 0xF9]); // CMP ECX, imm32
    a.emit(&class_id.to_le_bytes());
    a.jcc_forward(0x84)
}

/// nio21 wave 12 L2: with RCX = a plain object's class id (zero-extended),
/// fall through when that class is a subclass of `class_id` per the VM's
/// supers display (see [`VhSubtypeDisplay`]), else decline. Writes R11 and
/// the flags.
///
/// ```text
///   MOV R11, imm64 cell ; MOV R11, [R11]          ; the current table
///   TEST R11, R11 ; JZ decline
///   CMP RCX, [R11] ; JAE decline                   ; past the table's cap
///   MOV R11, [R11 + RCX*8 + 8] ; TEST R11, R11 ; JZ decline   ; no display
///   CMP DWORD [R11], depth ; JBE decline           ; shallower than the class
///   CMP DWORD [R11 + 4 + 4*depth], class_id ; JNE decline
/// ```
pub(in crate::ir_lower) fn vh_const_display_test(a: &mut VhConstAsm, display: VhSubtypeDisplay, class_id: u32) {
    a.emit(&[0x49, 0xBB]); // MOV R11, imm64
    a.emit(&(display.cell as u64).to_le_bytes()); // Cast: a data address
    a.emit(&[0x4D, 0x8B, 0x1B]); // MOV R11, [R11]
    a.emit(&[0x4D, 0x85, 0xDB]); // TEST R11, R11
    a.jcc_decline(0x84);
    a.emit(&[0x49, 0x3B, 0x0B]); // CMP RCX, [R11]
    a.jcc_decline(0x83); // JAE
    a.emit(&[0x4D, 0x8B, 0x5C, 0xCB, 0x08]); // MOV R11, [R11 + RCX*8 + 8]
    a.emit(&[0x4D, 0x85, 0xDB]); // TEST R11, R11
    a.jcc_decline(0x84);
    a.emit(&[0x41, 0x81, 0x3B]); // CMP DWORD [R11], imm32
    a.emit(&display.depth.to_le_bytes());
    a.jcc_decline(0x86); // JBE: len <= depth
                         // Cast: `depth` is bounded by `vh_const_display` (<= 0x0FFF_FFFF).
    let at = (4 + 4 * display.depth) as i32;
    a.emit(&[0x41, 0x81, 0xBB]); // CMP DWORD [R11 + disp32], imm32
    a.emit(&at.to_le_bytes());
    a.emit(&class_id.to_le_bytes());
    a.jcc_decline(0x85);
}

/// JIT r20 wave 11 (lane pAlloc, W14L4-3): the typecheck display arm's bytes,
/// with RCX = a plain object's class id (zero-extended): fall through when
/// that class is `target` or a subtype of it per its supers display -- a
/// class target through [`vh_const_display_test`], an interface through
/// [`vh_const_iface_test`] -- else take one of the returned rel32 decline
/// edges. Writes RCX, R11 and the flags.
pub(crate) fn typecheck_display_hit_bytes(
    display: VhSubtypeDisplay,
    target: u32,
) -> (Vec<u8>, Vec<usize>) {
    let mut a = VhConstAsm::default();
    if display.interface {
        vh_const_iface_test(&mut a, display, target);
    } else {
        vh_const_display_test(&mut a, display, target);
    }
    (a.b, a.decline)
}

/// Returns true if the target site class name belongs to a core boot package
/// (`java/*`, `jdk/*`, `sun/*`): the sites whose inline miss arms skip the
/// duplicate-class-name latch (`Some(0)` / `0` latch addresses).
///
/// tcm21 review note: the comment this had said core boot classes "cannot be
/// duplicated with different class IDs by other class loaders". That is
/// JVMS 5.3.5 / `ClassLoader.preDefineClass` for `java/*` only; a user loader
/// MAY define its own `jdk/*` or `sun/*` class. For such a name the compiled
/// miss answers by identity (HotSpot's answer) where the helper's duplicate-name
/// fallback can answer `true`; filed, not changed, on
/// `tcm21-typecheck-miss-and-shape-gates-20261102.md`.
/// (Moved here by tcm21: it used to sit between
/// [`typecheck_display_answer_bytes`]'s doc comment and the function, so rustdoc
/// attached that long comment to this helper.)
#[inline]
pub(crate) fn typecheck_site_is_boot_package(site_name: Option<&str>) -> bool {
    site_name.is_some_and(|n| {
        let s = n.trim_start_matches('[');
        let s = s.strip_prefix('L').unwrap_or(s);
        s.starts_with("java/") || s.starts_with("jdk/") || s.starts_with("sun/")
    })
}

/// JIT r20 wave 17 (lane pAlloc2; `CRATONVM_JIT_TYPECHECK_DISPLAY_MISS`): the
/// display arm with BOTH answers, for an `instanceof` whose target-side
/// admissions the caller has screened. With RCX = a plain object's class id
/// (zero-extended): returns `(bytes, decline, hit, miss)` -- `hit` and `miss`
/// are the rel32 fields of the `JMP`s to the caller's `1` / `0`, `decline`
/// the edges to the helper. Writes RCX, R11 and the flags; RAX is preserved.
///
/// The hit test is [`vh_const_display_test`] / [`vh_const_iface_test`]'s. A
/// MISS is a definite "not a subtype" of the hierarchy only past the same
/// table / capacity / no-display declines (an unpublished display is not a
/// miss): the class display shallower than the target or holding another id
/// at the target's depth, or the interface set without the target. It is the
/// helper's `false` only when, in addition,
///
/// * the receiver's display carries verdict `1` (its trailing word, at
///   `4 * (len + n + 2)`; `vm::jit::helpers::typecheck_miss_is_hierarchy_only`
///   over its chain and interface set): no instance-admitted shape, no class
///   whose edges may still be rewritten -- else decline;
/// * the duplicate-class-name latch (`latch`, the byte the final-class miss
///   arm tests) is clear: the loader-duplication by-name rule can only admit
///   a class with a same-named non-identical supertype -- else decline.
///
/// ```text
///   (table, capacity, display: as the hit tests; each JZ / JAE -> decline)
///   class:  CMP DWORD [R11], depth ; JBE miss0
///           CMP DWORD [R11 + 4 + 4*depth], target ; JNE miss0 ; JMP hit
///   miss0:  MOV ECX, [R11] ; LEA R11, [R11 + RCX*4 + 4] ; JMP verdict
///   iface:  MOV ECX, [R11] ; LEA R11, [R11 + RCX*4 + 4] ; MOV ECX, [R11]
///   top:    TEST ECX, ECX ; JZ verdict
///           CMP DWORD [R11 + RCX*4], target ; JE found ; DEC ECX ; JMP top
///   found:  JMP hit
///   verdict: (R11 -> n) MOV ECX, [R11] ; CMP DWORD [R11 + RCX*4 + 4], 1
///           JNE decline
///           MOV R11, imm64 latch ; CMP BYTE [R11], 0 ; JNE decline
///           JMP miss
/// ```
pub(crate) fn typecheck_display_answer_bytes(
    display: VhSubtypeDisplay,
    target: u32,
    latch: usize,
) -> (Vec<u8>, Vec<usize>, usize, usize) {
    let mut a = VhConstAsm::default();
    a.emit(&[0x49, 0xBB]); // MOV R11, imm64
    a.emit(&(display.cell as u64).to_le_bytes()); // Cast: a data address
    a.emit(&[0x4D, 0x8B, 0x1B]); // MOV R11, [R11]
    a.emit(&[0x4D, 0x85, 0xDB]); // TEST R11, R11
    a.jcc_decline(0x84);
    a.emit(&[0x49, 0x3B, 0x0B]); // CMP RCX, [R11]
    a.jcc_decline(0x83); // JAE
    a.emit(&[0x4D, 0x8B, 0x5C, 0xCB, 0x08]); // MOV R11, [R11 + RCX*8 + 8]
    a.emit(&[0x4D, 0x85, 0xDB]); // TEST R11, R11
    a.jcc_decline(0x84); // no display: not a miss
    let hit;
    let to_verdict;
    if display.interface {
        a.emit(&[0x41, 0x8B, 0x0B]); // MOV ECX, [R11] (len)
        a.emit(&[0x4D, 0x8D, 0x5C, 0x8B, 0x04]); // LEA R11, [R11 + RCX*4 + 4] -> n
        a.emit(&[0x41, 0x8B, 0x0B]); // MOV ECX, [R11] (n)
        let top = a.b.len();
        a.emit(&[0x85, 0xC9]); // TEST ECX, ECX
        to_verdict = a.jcc_forward(0x84); // JZ: not in the set
        a.emit(&[0x41, 0x81, 0x3C, 0x8B]); // CMP DWORD [R11 + RCX*4], imm32
        a.emit(&target.to_le_bytes());
        let found = a.jcc_forward(0x84); // JE
        a.emit(&[0xFF, 0xC9]); // DEC ECX
        a.emit(&[0xE9]); // JMP top
                         // Cast: a backward distance inside one short sequence.
        let rel = top as i64 - (a.b.len() as i64 + 4);
        a.emit(&(rel as i32).to_le_bytes());
        a.bind(found);
        a.emit(&[0xE9]); // JMP hit
        hit = a.rel32_field();
    } else {
        a.emit(&[0x41, 0x81, 0x3B]); // CMP DWORD [R11], imm32
        a.emit(&display.depth.to_le_bytes());
        let shallow = a.jcc_forward(0x86); // JBE: len <= depth
                                           // Cast: `depth` is bounded by `vh_const_display` (<= 0x0FFF_FFFF).
        let at = (4 + 4 * display.depth) as i32;
        a.emit(&[0x41, 0x81, 0xBB]); // CMP DWORD [R11 + disp32], imm32
        a.emit(&at.to_le_bytes());
        a.emit(&target.to_le_bytes());
        let other = a.jcc_forward(0x85); // JNE
        a.emit(&[0xE9]); // JMP hit
        hit = a.rel32_field();
        a.bind(shallow);
        a.bind(other);
        a.emit(&[0x41, 0x8B, 0x0B]); // MOV ECX, [R11] (len)
        a.emit(&[0x4D, 0x8D, 0x5C, 0x8B, 0x04]); // LEA R11, [R11 + RCX*4 + 4] -> n
        a.emit(&[0xE9]); // JMP verdict
        to_verdict = a.rel32_field();
    }
    a.bind(to_verdict);
    a.emit(&[0x41, 0x8B, 0x0B]); // MOV ECX, [R11] (n)
    a.emit(&[0x41, 0x83, 0x7C, 0x8B, 0x04, 0x01]); // CMP DWORD [R11 + RCX*4 + 4], 1
    a.jcc_decline(0x85); // JNE: not hierarchy-only
    if latch != 0 {
        a.emit(&[0x49, 0xBB]); // MOV R11, imm64
        a.emit(&(latch as u64).to_le_bytes()); // Cast: a static's address
        a.emit(&[0x41, 0x80, 0x3B, 0x00]); // CMP BYTE [R11], 0
        a.jcc_decline(0x85); // JNE: a duplicate class name exists
    }
    a.emit(&[0xE9]); // JMP miss
    let miss = a.rel32_field();
    (a.b, a.decline, hit, miss)
}

/// `CRATONVM_JIT_TYPECHECK_DISPLAY_MISS` (JIT r20 wave 17, lane pAlloc2;
/// **default ON**, `=0` = the hit-only display arm of wave 11): the
/// optimizing tier's `Op::InstanceOf` answers a display MISS inline as well
/// as a hit ([`typecheck_display_answer_bytes`]). `R20PAllocTypecheck`'s
/// `instanceof` rows missed on 6 of 14 checks and every miss called
/// `jit_instanceof`. Needs `CRATONVM_JIT_TYPECHECK_DISPLAY_ARM`. Read per
/// site.
pub(crate) fn typecheck_display_miss_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_IR_INSTANCEOF_PROXY_WAYS` (JIT r25, lane regex rows; perf,
/// **default ON** since R25-FLIP, `=0` kills): an optimizing-tier `instanceof` against a resolved class
/// target answers a LAMBDA-PROXY receiver (class id `>= 0x8000_0000`, which
/// no display, final-miss or seen-class arm covers) from two per-target ways
/// in a cell the artifact owns: way 0 the last proxy class the helper
/// answered `1` for, way 1 the last it answered `0` for (a miss only while
/// the duplicate-class-name latch is clear). Every other proxy asks
/// `jit_instanceof`, whose answer refills the way. Measured case:
/// `Pattern.newCharProperty`'s `p instanceof BmpCharPredicate` on the
/// `CharPredicates.ASCII_SPACE` lambda (21 % of the r23 `compile` row in
/// `jit_instanceof`, the r23-w5 page's wave 10). `lib.rs` plants the cells;
/// [`INSTANCEOF_PROXY_CELL_TAG`]. Read per compile.
///
/// Why a way's answer is the helper's (`vm::jit::helpers::jit_typecheck_resolve`
/// over a recorded site, the only kind this tier admits): a proxy id is
/// allocated monotonically and never reused (`alloc_lambda_proxy_id`), is in
/// no class store, so the hierarchy, `synthetic_implements`, the instance
/// admissions (annotation proxy, `Proxy$Instance`, `Unmodifiable*` stamps:
/// all ordinary class ids) and the by-name hierarchy walk never answer it;
/// what does is `lambda_proxy_satisfies`, a function of the proxy's call-site
/// row (functional interface, `FLAG_SERIALIZABLE`, marker interfaces), all
/// fixed when the proxy is spun, before any instance exists, and of the
/// interface's hierarchy, which never changes. The helper already memoizes
/// that `true` per proxy id. Its `false` can become `true` only through the
/// loader-duplication name rule, which needs a duplicate class name, so the
/// miss way is read behind the same latch as the final-class miss arm.
pub(crate) fn ir_instanceof_proxy_ways_enabled() -> bool {
    // R25-FLIP: default ON; `=0` is the kill switch.
    true
}

/// The low bit of an `Op::InstanceOf::seen_cell` that marks the cell as the
/// lambda-proxy ways of [`ir_instanceof_proxy_ways_enabled`] rather than
/// MHIFACE19-1's seen-class ways. A cell is a `Box<[AtomicU32; 2]>`, 4-aligned,
/// so the bit is free; the lowering masks it off before baking the address.
pub const INSTANCEOF_PROXY_CELL_TAG: usize = 1;

/// Byte offsets of the lambda-proxy ways in a `crate::TypecheckSeenCell`:
/// the last proxy the helper answered `1` for, the last it answered `0` for,
/// and (round rx27, [`ir_proxy_two_hit_ways_enabled`]) the hit way's
/// previous proxy.
pub const PROXY_WAY_HIT_DISP: u8 = 0;
pub const PROXY_WAY_MISS_DISP: u8 = 4;
pub const PROXY_WAY_HIT2_DISP: u8 = 8;

/// Round rx27 (lane compile): the bytes of a lambda-proxy HIT-way fill. The
/// cell's address is in RAX (`base_rcx == false`) or RCX, the proxy's class id
/// in R11D. One hit way: `MOV [base], R11D`. Two
/// ([`ir_proxy_two_hit_ways_enabled`]): `XCHG [base], R11D ; MOV [base+8],
/// R11D` -- the new proxy into the hit way, its previous proxy into the second
/// hit way. Clobbers R11 (two ways) and nothing else; the implicit `LOCK` of
/// `XCHG` is paid on the helper path only.
pub(crate) fn proxy_hit_way_fill_bytes(base_rcx: bool, two_ways: bool) -> Vec<u8> {
    // ModRM: reg = R11 (011 with REX.R), rm = RAX (000) or RCX (001).
    let rm = u8::from(base_rcx);
    if two_ways {
        vec![
            0x44,
            0x87,
            0x18 | rm,
            0x44,
            0x89,
            0x58 | rm,
            PROXY_WAY_HIT2_DISP,
        ]
    } else {
        vec![0x44, 0x89, 0x18 | rm]
    }
}

/// `CRATONVM_JIT_IR_PROXY_TWO_HIT_WAYS` (round rx27, lane compile; perf,
/// **opt-in** `=1`): the lambda-proxy ways keep TWO proven-`1` proxies per
/// target instead of one. A helper `1` moves the hit way's proxy into the
/// second hit way (`XCHG [cell], id ; MOV [cell+8], old`) and stores the new
/// one, so a site that alternates between two proxies of one target stops
/// paying a helper call per change. Named case: `regex-split` compiles
/// `",\s*"` per op, and `Pattern.newCharProperty`'s `p instanceof
/// BmpCharPredicate` then sees `Pattern.Single`'s lambda and
/// `CharPredicates.ASCII_SPACE`'s in turn, each answered `1`, so with one hit
/// way each call evicted the other (two `jit_instanceof` calls per op).
///
/// Sound for the reason the hit way is ([`ir_instanceof_proxy_ways_enabled`]):
/// a value stored is either `0` (no proxy id) or an id the helper answered
/// `1` for at this target, and that answer is a function of the proxy class
/// and the target alone. A racing writer stores another such id. Read per
/// compile.
pub(crate) fn ir_proxy_two_hit_ways_enabled() -> bool {
    false
}

/// `CRATONVM_JIT_IR_CHECKCAST_PROXY_WAYS` (round rx27, lane compile; perf,
/// **default ON** since 2026-11-09, `=0` kills it): an optimizing-tier `checkcast` against a resolved class
/// target passes a lambda-proxy receiver (class id `>= 0x8000_0000`) found in
/// the target's proxy HIT ways inline, and sends any other proxy straight to
/// `jit_checkcast` (no display, row or exact compare covers that range); a
/// successful helper call writes the receiver's proxy id into the hit way.
/// The cell is the one the same target's `instanceof`s use in this body
/// (`Graph::checkcast_proxy_cells`).
///
/// Why a hit is the helper's answer: `jit_checkcast` asks
/// `jit_typecheck_resolve` with `lenient = true`, a flag that only labels the
/// trace and keys the memo (since i10-L2 both opcodes answer one rule), so a
/// cast passes exactly when `instanceof` answers `1`, and for a lambda proxy
/// that answer is a function of its class and the target
/// ([`ir_instanceof_proxy_ways_enabled`]; the instance rules all need an
/// ordinary class id). The way is written only from the helper's returned
/// reference, which is non-null on success (a failure throws through the
/// sentinel, and the fail-soft `0` writes nothing). Read per compile.
pub(crate) fn ir_checkcast_proxy_ways_enabled() -> bool {
    true
}

/// The first lambda-proxy class id (`vm::vm::realms::class_realm::LAMBDA_PROXY_ID_BASE`,
/// asserted equal to [`GETCLASS_WORDS_LAMBDA_BASE_ID`] there): an id with the
/// top bit set, which the proxy ways test as a negative `i32`.
const _: () = assert!(GETCLASS_WORDS_LAMBDA_BASE_ID == 0x8000_0000);

/// `CRATONVM_JIT_TYPECHECK_DISPLAY_MISS_ANY_TARGET` (JIT round 21, lane
/// typecheck; perf, **default ON**, `=0` restores the target screen): the
/// display MISS arm ([`typecheck_display_answer_bytes`]) also serves a target
/// the `synthetic_implements` name rules can admit (`List`, `Collection`,
/// `Iterable`, `Comparable`, `Serializable`, `Map$Entry`, ...). Sound since
/// round 21's `synthetic_implements` declines every receiver whose supertype
/// closure is hierarchy-only -- the predicate the receiver's display verdict
/// word (tested at run time by the arm) folds over that same closure -- so
/// for such a receiver the helper's `false` is the hierarchy's. A verdict-0
/// receiver (a stub, a VM-internal stamp, a proxy) still declines to the
/// helper. Needs `CRATONVM_JIT_TYPECHECK_DISPLAY_MISS`. Read per site.
pub(crate) fn typecheck_display_miss_any_target_enabled() -> bool {
    true
}

/// JIT r21 tc21b review: THE target-side rule of a compiled `instanceof`
/// display MISS arm, shared by both tiers (the IR's
/// `Lowerer::typecheck_display_miss_for` and the single-pass tier's
/// `x64/op_object.rs` `instanceof` arm kept two copies of it): the miss arm is
/// on, the site's target name is known, and either
/// `CRATONVM_JIT_TYPECHECK_DISPLAY_MISS_ANY_TARGET` relaxes the screen or the
/// target is not one the `synthetic_implements` rules can admit. The
/// receiver-side half (verdict word, proxy ids, the duplicate-name latch) is
/// tested at run time by the emitted bytes.
pub(crate) fn typecheck_display_miss_admits_target(site_name: Option<&str>) -> bool {
    typecheck_display_miss_enabled()
        && site_name.is_some_and(|n| {
            typecheck_display_miss_any_target_enabled()
                || !cratonvm_types::synthetic_targets::synthetic_implements_may_admit(n)
        })
}

/// `CRATONVM_JIT_TYPECHECK_DISPLAY_ARM` (JIT r20 wave 11, lane pAlloc;
/// W14L4-3; **default ON**, `=0` restores the exact-class / seen-way arms
/// alone): the optimizing tier's `Op::InstanceOf` / `Op::CheckCast` answer a
/// subclass or implementor of the target from the VM's supers displays
/// before calling the helper. Needs the displays themselves
/// (`CRATONVM_JIT_VH_CONST_INLINE`, `CRATONVM_JIT_VH_CONST_SUBTYPE`; an
/// interface target also `CRATONVM_JIT_VH_CONST_IFACE`). Read per site.
pub(crate) fn typecheck_display_arm_enabled() -> bool {
    true
}

/// The supers-display cell of this compile's VM
/// ([`VhSubtypeDisplay::cell`]), or `None` when it keeps no displays
/// (`CRATONVM_JIT_VH_CONST_INLINE` / `CRATONVM_JIT_VH_CONST_SUBTYPE` off) or
/// has not published the cell. Shared by both tiers (JIT r21 wave 1, lane
/// typecheck; was the body of `Lowerer::class_assignable_display_cell`).
pub(crate) fn typecheck_display_cell(helpers: &crate::DirectHelperTable) -> Option<usize> {
    if !vh_const_inline_enabled() || !vh_const_subtype_enabled() {
        return None;
    }
    let cell = vh_const_ask(
        helpers,
        vh_const_query(WELL_KNOWN_VH_CONST_DISPLAY_CELL_TAG, 0, 0),
    );
    usize::try_from(cell)
        .ok()
        .filter(|&c| c != 0 && c % std::mem::align_of::<u64>() == 0)
}

/// `CRATONVM_JIT_INSTANCEOF_FINAL_MISS_VERDICT` (JIT r21 wave 1, lane
/// typecheck; correctness, **default ON**, `=0` restores the defect): both
/// tiers' definite `instanceof` miss for a FINAL target
/// (`CRATONVM_JIT_INSTANCEOF_FINAL_MISS`) is answered inline only for a
/// receiver whose supers display carries the hierarchy-only verdict
/// ([`typecheck_receiver_verdict_bytes`]). An instance-admitted receiver --
/// a `cratonvm/internal/Unmodifiable*` stamp, whose display class may be a
/// final JDK class (`ImmutableCollections$ListN`) -- asks the helper, which
/// admits it as the interpreter does. Page
/// `docs/feature-designs/jit-ir-optimizations.md`.
/// Read per site.
pub(crate) fn instanceof_final_miss_verdict_enabled() -> bool {
    true
}

/// JIT r21 wave 1 (lane typecheck, `CRATONVM_JIT_INSTANCEOF_FINAL_MISS_VERDICT`):
/// with RCX = a plain receiver's class id (zero-extended), fall through when
/// its supers display is published and its trailing verdict word is `1`
/// (`typecheck_miss_is_hierarchy_only` over its chain and interface set: no
/// instance-admitted shape), else take a returned decline edge. Writes RCX,
/// R11 and the flags; RAX is preserved.
///
/// ```text
///   MOV R11, imm64 cell ; MOV R11, [R11] ; TEST R11, R11 ; JZ decline
///   CMP RCX, [R11] ; JAE decline
///   MOV R11, [R11 + RCX*8 + 8] ; TEST R11, R11 ; JZ decline
///   MOV ECX, [R11] ; LEA R11, [R11 + RCX*4 + 4] ; MOV ECX, [R11]   ; n
///   CMP DWORD [R11 + RCX*4 + 4], 1 ; JNE decline                  ; verdict
/// ```
pub(crate) fn typecheck_receiver_verdict_bytes(cell: usize) -> (Vec<u8>, Vec<usize>) {
    let mut a = VhConstAsm::default();
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
    a.emit(&[0x41, 0x8B, 0x0B]); // MOV ECX, [R11] (len)
    a.emit(&[0x4D, 0x8D, 0x5C, 0x8B, 0x04]); // LEA R11, [R11 + RCX*4 + 4] -> n
    a.emit(&[0x41, 0x8B, 0x0B]); // MOV ECX, [R11] (n)
    a.emit(&[0x41, 0x83, 0x7C, 0x8B, 0x04, 0x01]); // CMP DWORD [R11 + RCX*4 + 4], 1
    a.jcc_decline(0x85); // JNE: not hierarchy-only
    (a.b, a.decline)
}

/// JIT r21 wave 1 (lane typecheck, TYPECHECK21-3,
/// [`crate::typecheck_array_targets_enabled`]): the well-known resolver's
/// query for the immediate component of the ARRAY class named in the word's
/// class half: `1 + component id` when the component is a class or an
/// interface (not an array, not a primitive), else `0`.
pub const WELL_KNOWN_TYPECHECK_ARRAY_COMPONENT_TAG: i64 = 0x564D_i64 << 48;

/// JIT r25 wave 8 (W8-2): the well-known resolver's query for the address of
/// the VM's `instanceof` memo of the class named in the word's class half
/// ([`crate::typecheck_miss_memo_cell`]), `0` when the VM keeps none.
pub const WELL_KNOWN_TYPECHECK_MISS_MEMO_TAG: i64 = 0x5654_i64 << 48;

/// The component class of the array class `array_class_id` in this compile's
/// VM ([`WELL_KNOWN_TYPECHECK_ARRAY_COMPONENT_TAG`]), or `None`.
pub(crate) fn typecheck_array_component(
    helpers: &crate::DirectHelperTable,
    array_class_id: u32,
) -> Option<u32> {
    let answer = vh_const_ask(
        helpers,
        vh_const_query(WELL_KNOWN_TYPECHECK_ARRAY_COMPONENT_TAG, array_class_id, 0),
    );
    u32::try_from(answer).ok().and_then(|a| a.checked_sub(1))
}

/// What an `instanceof` / `checkcast` against `[LT;` needs to answer a
/// reference-array receiver inline (TYPECHECK21-3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct TypecheckArrayPlan {
    /// `T`'s class id.
    pub(crate) component: u32,
    /// `T` is `java/lang/Object`: every reference array is a `T[]`.
    pub(crate) object_component: bool,
    /// `T`'s supers display (a class with one, or an interface), for a
    /// receiver whose component is a proper subtype of `T`.
    pub(crate) display: Option<VhSubtypeDisplay>,
}

/// The [`TypecheckArrayPlan`] of the site named `site_name` whose resolved
/// target is the array class `array_class_id`, or `None` (switch off, not a
/// one-dimensional `[L...;` target, no component answer).
pub(crate) fn typecheck_array_plan(
    helpers: &crate::DirectHelperTable,
    site_name: Option<&str>,
    array_class_id: u32,
) -> Option<TypecheckArrayPlan> {
    if !crate::typecheck_array_targets_enabled() {
        return None;
    }
    typecheck_array_plan_any_tier(helpers, site_name, array_class_id)
}

/// [`typecheck_array_plan`] without the IR switch: the single-pass twin
/// (TYPECHECK21-3b) asks it under its own `CRATONVM_JIT_SP_TYPECHECK_ARRAY_TARGETS`.
pub(crate) fn typecheck_array_plan_any_tier(
    helpers: &crate::DirectHelperTable,
    site_name: Option<&str>,
    array_class_id: u32,
) -> Option<TypecheckArrayPlan> {
    if array_class_id == 0 {
        return None;
    }
    let name = site_name?;
    let inner = name.strip_prefix("[L")?.strip_suffix(';')?;
    if inner.is_empty() || inner.starts_with('[') {
        return None;
    }
    let object_component = inner == "java/lang/Object";
    let component = typecheck_array_component(helpers, array_class_id)?;
    let display = if object_component {
        None
    } else {
        vh_const_display(helpers, component)
    };
    Some(TypecheckArrayPlan {
        component,
        object_component,
        display,
    })
}

/// A reference array's `KIND_TAGS` byte, exactly (kind `Array`, element type
/// `Reference` = 0, reserved bits clear): what a 1-D or nested reference array
/// carries. A primitive array's byte differs in its element bits; a plain
/// object's kind bits are `0`.
pub(in crate::ir_lower) const REF_ARRAY_KIND_TAGS: u8 = cratonvm_types::ObjectKind::Array as u8;

/// JIT r21 wave 1 (lane typecheck, TYPECHECK21-3): the array-target arm of
/// `instanceof` / `checkcast` against `[LT;`, with RAX = a non-null
/// receiver. Returns `(bytes, decline, hits)`; the sequence never falls
/// through. Writes RCX, R11 and the flags; RAX is preserved.
///
/// A reference array's header class id is its IMMEDIATE component's
/// (`typecheck::array_descriptor_of`): `S` for an `S[]`, the array class
/// `[LS;` for an `S[][]`. So the receiver is a `T[]` when its byte says
/// "reference array" and its component is `T` or a subtype of `T`
/// (JVMS 6.5: `S[]` is a `T[]` iff `S` is assignable to `T`), proven by `T`'s
/// display test on the component id -- the class chain, or the component's
/// interface set for an interface `T`. A nested receiver's component is an
/// array class, never in a class chain below `Object`, so it only hits
/// through an interface its array class lists (`Cloneable`,
/// `Serializable`), which is JVMS too. `T == Object` hits every reference
/// array. Every non-proof declines: the helper still answers misses.
///
/// ```text
///   CMP BYTE [RAX + KIND_TAGS], REF_ARRAY ; JNE decline
///   (T is Object: JMP hit)
///   MOV ECX, [RAX] ; CMP ECX, T ; JE hit
///   (T's display test on RCX ; JMP hit) | JMP decline
/// ```
pub(crate) fn typecheck_array_hit_bytes(plan: TypecheckArrayPlan) -> (Vec<u8>, Vec<usize>, Vec<usize>) {
    const KIND_TAGS: u8 = cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8; // Cast: disp8
    const _: () = assert!(cratonvm_types::KIND_TAGS_BYTE_OFFSET <= 127);
    let mut a = VhConstAsm::default();
    let mut hits: Vec<usize> = Vec::with_capacity(2);
    a.emit(&[0x80, 0x78, KIND_TAGS, REF_ARRAY_KIND_TAGS]); // CMP BYTE [RAX + disp8], imm8
    a.jcc_decline(0x85); // JNE: not a reference array
    if plan.object_component {
        a.emit(&[0xE9]); // JMP hit
        hits.push(a.rel32_field());
        return (a.b, a.decline, hits);
    }
    a.emit(&[0x8B, 0x08]); // MOV ECX, [RAX] (the component id)
    hits.push(vh_const_class_test(&mut a, plan.component)); // CMP ECX, T ; JE hit
    match plan.display {
        Some(display) => {
            if display.interface {
                vh_const_iface_test(&mut a, display, plan.component);
            } else {
                vh_const_display_test(&mut a, display, plan.component);
            }
            a.emit(&[0xE9]); // JMP hit
            hits.push(a.rel32_field());
        }
        None => a.jmp_decline(),
    }
    (a.b, a.decline, hits)
}

/// tca21 (TCARRAY21-1, second half): the component miss of an `instanceof T[]`
/// (`T` a class or interface other than `Object`, with the display `display`),
/// for a receiver that is exactly a REFERENCE ARRAY in RAX. Returns `(bytes,
/// decline, hit, miss)` as [`typecheck_display_answer_bytes`] does, after a
/// `MOV ECX, [RAX]` that loads the array's header class id -- on a reference
/// array that is its COMPONENT's id, so the plain-object display answer is the
/// component's answer: `hit` the rel32 of the `JMP` to the `1`, `miss` that of
/// the `JMP` to the `0`, `decline` the edges to the helper. Writes RCX, R11 and
/// the flags; RAX is preserved.
///
/// Why the plain-object miss is a miss for the array. JVMS 6.5: `S[]` is a
/// `T[]` iff `S` is assignable to `T`, and the VM's verdict for a reference
/// array is that component test, by identity under `--real-jdk`
/// (`array_cast_resolved_verdict` -> `array_component_verdict` ->
/// `class_is_subtype`) and by the same hierarchy under `--compatible` unless a
/// same-named class of another loader exists (the duplicate-class-name latch
/// the answer keeps). The display answer's miss needs the component's verdict
/// word `1` (hierarchy-only: real bytes behind every class and interface on its
/// closure), which a nested array's component never has -- an array class is
/// `ClassOrigin::VmArray`, outside `typecheck_miss_is_hierarchy_only` -- so a
/// `String[][]` against `Cloneable[]` (a hit through the array class's own
/// interfaces) is never answered `0`. A component id at or past the display
/// table (a lambda proxy, any synthetic id) or without a display declines.
/// `Object[]` (component id `0`, `java/lang/Object`) is a miss for every `T`
/// but `Object`, which the caller excludes.
pub(crate) fn typecheck_array_component_miss_bytes(
    display: VhSubtypeDisplay,
    target: u32,
    latch: usize,
) -> (Vec<u8>, Vec<usize>, usize, usize) {
    // MOV ECX, DWORD [RAX] (8B /r, ModRM 0x08).
    const LOAD_COMPONENT: [u8; 2] = [0x8B, 0x08];
    let (answer, decline, hit, miss) = typecheck_display_answer_bytes(display, target, latch);
    let shift = LOAD_COMPONENT.len();
    let mut bytes: Vec<u8> = Vec::with_capacity(shift + answer.len());
    bytes.extend_from_slice(&LOAD_COMPONENT);
    bytes.extend_from_slice(&answer);
    (
        bytes,
        decline.into_iter().map(|e| e + shift).collect(),
        hit + shift,
        miss + shift,
    )
}

/// `CRATONVM_JIT_IR_AASTORE_DISPLAY_ARM` (JIT r21 wave 1, lane typecheck;
/// **opt-in**, default OFF): the optimizing tier's reference `aastore` proves
/// a plain value's store legal from the supers displays
/// ([`aastore_display_arm_bytes`]) when its class is a subclass of the array's
/// component class or implements the component interface, instead of calling
/// `jit_aastore_type_check`. Before it, only an exact component match, an
/// `Object[]` and the static proofs skipped the call: `Number[]` /
/// `Comparable[]` / `Node[]`-with-`TreeNode` stores paid it every time. Needs
/// `CRATONVM_JIT_TYPECHECK_DISPLAY_ARM` and the displays themselves. Read per
/// site.
pub(crate) fn ir_aastore_display_arm_enabled() -> bool {
    typecheck_display_arm_enabled()
}

/// JIT r21 wave 1 (lane typecheck, `CRATONVM_JIT_IR_AASTORE_DISPLAY_ARM`):
/// the `aastore` store check's YES from the supers displays, with the
/// component known only at run time (HotSpot's: the array klass's element
/// klass, then the value klass's primary display at the element's depth or
/// its secondary supers). Entry: RCX = the array's component class id `c`
/// (non-zero: an `Object[]` was answered before), RDX = the plain value's
/// class id `v`, both zero-extended. Returns `(bytes, decline, hits)`: the
/// rel32 fields of the declines (to the check call) and of the `JE`s to the
/// store. The sequence never falls through. Writes RCX, RDX, R10 and the
/// flags.
///
/// * `c` has a display (a class): the store is legal when `v`'s display is
///   at least as deep and holds `c` at `c`'s depth (`len(c) - 1`), exactly
///   [`vh_const_display_test`] with the depth read at run time;
/// * `c` has none (an interface, or a class not yet published) or is past
///   the table: legal when `c` is in `v`'s transitive interface set. A
///   class id never appears in an interface set, so a scan that finds `c`
///   proves an interface `v` implements;
/// * `v` has no display (an id past the table: a lambda proxy, an
///   unpublished class) and every non-proof: decline. A decline is never a
///   NO, so the refusal and its `ArrayStoreException` stay the helper's.
///
/// ```text
///   MOV R10, imm64 cell ; MOV R10, [R10] ; TEST R10, R10 ; JZ decline
///   CMP RDX, [R10] ; JAE decline
///   MOV RDX, [R10 + RDX*8 + 8] ; TEST RDX, RDX ; JZ decline   ; v's display
///   CMP RCX, [R10] ; JAE iface
///   MOV R10, [R10 + RCX*8 + 8] ; TEST R10, R10 ; JZ iface     ; c's display
///   MOV R10D, [R10]                         ; len(c)
///   CMP [RDX], R10D ; JB decline            ; v shallower than c
///   CMP [RDX + R10*4], ECX ; JE hit         ; ids(v)[len(c) - 1]
///   JMP decline
/// iface:
///   MOV R10D, [RDX] ; LEA RDX, [RDX + R10*4 + 4] ; MOV R10D, [RDX]   ; n
/// top:
///   TEST R10D, R10D ; JZ decline
///   CMP [RDX + R10*4], ECX ; JE hit ; DEC R10D ; JMP top
/// ```
pub(crate) fn aastore_display_arm_bytes(cell: usize) -> (Vec<u8>, Vec<usize>, Vec<usize>) {
    let mut a = VhConstAsm::default();
    let mut hits: Vec<usize> = Vec::with_capacity(2);
    a.emit(&[0x49, 0xBA]); // MOV R10, imm64
    a.emit(&(cell as u64).to_le_bytes()); // Cast: a data address
    a.emit(&[0x4D, 0x8B, 0x12]); // MOV R10, [R10]
    a.emit(&[0x4D, 0x85, 0xD2]); // TEST R10, R10
    a.jcc_decline(0x84); // JZ: no table yet
    a.emit(&[0x49, 0x3B, 0x12]); // CMP RDX, [R10]
    a.jcc_decline(0x83); // JAE: v past the table
    a.emit(&[0x49, 0x8B, 0x54, 0xD2, 0x08]); // MOV RDX, [R10 + RDX*8 + 8]
    a.emit(&[0x48, 0x85, 0xD2]); // TEST RDX, RDX
    a.jcc_decline(0x84); // JZ: v has no display
    a.emit(&[0x49, 0x3B, 0x0A]); // CMP RCX, [R10]
    let past = a.jcc_forward(0x83); // JAE iface
    a.emit(&[0x4D, 0x8B, 0x54, 0xCA, 0x08]); // MOV R10, [R10 + RCX*8 + 8]
    a.emit(&[0x4D, 0x85, 0xD2]); // TEST R10, R10
    let no_display = a.jcc_forward(0x84); // JZ iface
    a.emit(&[0x45, 0x8B, 0x12]); // MOV R10D, [R10] (len(c), >= 1)
    a.emit(&[0x44, 0x39, 0x12]); // CMP [RDX], R10D
    a.jcc_decline(0x82); // JB: len(v) < len(c)
    a.emit(&[0x42, 0x39, 0x0C, 0x92]); // CMP [RDX + R10*4], ECX
    hits.push(a.jcc_forward(0x84)); // JE hit
    a.jmp_decline();
    a.bind(past);
    a.bind(no_display);
    a.emit(&[0x44, 0x8B, 0x12]); // MOV R10D, [RDX] (len(v))
    a.emit(&[0x4A, 0x8D, 0x54, 0x92, 0x04]); // LEA RDX, [RDX + R10*4 + 4] -> n
    a.emit(&[0x44, 0x8B, 0x12]); // MOV R10D, [RDX] (n)
    let top = a.b.len();
    a.emit(&[0x45, 0x85, 0xD2]); // TEST R10D, R10D
    a.jcc_decline(0x84); // JZ: not in the set
    a.emit(&[0x42, 0x39, 0x0C, 0x92]); // CMP [RDX + R10*4], ECX
    hits.push(a.jcc_forward(0x84)); // JE hit
    a.emit(&[0x41, 0xFF, 0xCA]); // DEC R10D
    a.emit(&[0xE9]); // JMP top
                     // Cast: a backward distance inside one short sequence.
    let rel = top as i64 - (a.b.len() as i64 + 4);
    a.emit(&(rel as i32).to_le_bytes());
    (a.b, a.decline, hits)
}

/// The bytes both tiers' constant-`VarHandle` prefixes emit after loading
/// their operands (nio21 wave 11 L5): on entry RAX = the receiver coordinate,
/// R10 = `expected` (a CAS / compareAndExchange), RDX = the value (`new` for a
/// CAS, the delta, the mask, the stored value). The handle has already been
/// proven. Screens the receiver, the reference values and the barrier gates,
/// then performs the access; on the fall-through RAX holds the site's result
/// (an `int` sign-extended, `0`/`1` for a CAS, the old value for
/// getAndAdd / getAndSet, the witness for compareAndExchange; dead for a set).
/// Returns the bytes and the offsets of the rel32 decline fields (each to be
/// patched to the call). Writes RAX, RCX, RDX, R10, R11 and the flags.
///
/// A G1 core ([`VhConstCore::g1`]) is served here without its post edge: a
/// cross-region value declines to the call (the post edge needs the caller's
/// barrier call, [`vh_const_core_bytes_post`]).
pub(crate) fn vh_const_core_bytes(core: &VhConstCore) -> (Vec<u8>, Vec<usize>) {
    let mut c = *core;
    if let Some(g1) = c.g1.as_mut() {
        g1.post_call = false;
    }
    let (bytes, decline, _post) = vh_const_core_bytes_post(&c);
    (bytes, decline)
}

/// [`vh_const_core_bytes`] with G1's post edge (JIT round 21, lane g1vh):
/// the third value is the offset of the rel32 field of the edge a G1 core
/// with [`VhG1Gates::post_call`] takes for a cross-region value, BEFORE the
/// access, with RAX / R10 / RDX still the operands; the caller patches it to
/// code that performs the access ([`vh_const_access_bytes`]), keeps the
/// result, and calls `jit_g1_post_write_barrier`. `None` for every other core.
pub(crate) fn vh_const_core_bytes_post(core: &VhConstCore) -> (Vec<u8>, Vec<usize>, Option<usize>) {
    vh_const_core_bytes_with(core, false)
}

/// j27 wave 2 (lane VHCOMP, `CRATONVM_JIT_VH_CONST_GEN_CARD_EDGE`):
/// [`vh_const_core_bytes_post`] for a GATED (generational) reference write,
/// whose "a card may be owed" exit is the third value instead of a decline,
/// entered BEFORE the access with RAX / R10 / RDX still the operands. A core
/// without gates answers as [`vh_const_core_bytes_post`].
pub(crate) fn vh_const_core_bytes_gen_edge(
    core: &VhConstCore,
) -> (Vec<u8>, Vec<usize>, Option<usize>) {
    vh_const_core_bytes_with(core, true)
}

pub(in crate::ir_lower) fn vh_const_core_bytes_with(
    core: &VhConstCore,
    gen_card_edge: bool,
) -> (Vec<u8>, Vec<usize>, Option<usize>) {
    const KIND_TAGS: u8 = cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8; // Cast: disp8
    const GC_FLAGS: u8 = cratonvm_types::GC_FLAGS_BYTE_OFFSET as u8; // Cast: disp8
    const _: () = assert!(cratonvm_types::GC_FLAGS_BYTE_OFFSET <= 127);
    let mut a = VhConstAsm::default();
    // The receiver: not null, a plain object (an array header carries its
    // COMPONENT's id), exactly the coordinate class, COMPACT.
    a.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
    a.jcc_decline(0x84); // JZ: the call raises the NPE
    a.emit(&[0xF6, 0x40, KIND_TAGS, cratonvm_types::KIND_TAG_BYTE_MASK]);
    a.jcc_decline(0x85);
    match core.receiver_display {
        None => {
            a.emit(&[0x81, 0xB8]); // CMP DWORD [RAX + cid_off], class_id
            a.emit(&core.cid_off.to_le_bytes());
            a.emit(&core.class_id.to_le_bytes());
            a.jcc_decline(0x85); // JNE: another class takes the call
        }
        Some(display) => {
            // nio21 wave 12 L2: exactly the coordinate class, or a subclass of
            // it through the supers display (an unrelated class declines).
            a.emit(&[0x8B, 0x88]); // MOV ECX, DWORD [RAX + cid_off]
            a.emit(&core.cid_off.to_le_bytes());
            let exact = vh_const_class_test(&mut a, core.class_id);
            vh_const_display_test(&mut a, display, core.class_id);
            a.bind(exact);
        }
    }
    a.emit(&[0xF6, 0x40, GC_FLAGS, cratonvm_types::GC_FLAG_COMPACT]);
    a.jcc_decline(0x84); // JZ: a legacy instance takes the call
                         // The reference values the access stores or compares.
    vh_const_value_screen(&mut a, true, core.expected_check, core.cid_off);
    vh_const_value_screen(&mut a, false, core.value_check, core.cid_off);
    // A reference store: both barriers ruled out first, or the call.
    // JIT round 21 wave 16 (lane perf-cf-lean, `CRATONVM_JIT_VH_CONST_SATB_NULL_OLD`):
    // while marking, a store whose overwritten value is provably null stays
    // inline (see `vh_const_satb_null_old`).
    let mut card_edge: Option<usize> = None;
    if let Some(g) = core.gates {
        let edge = gen_card_edge.then_some(&mut card_edge);
        vh_ref_store_gate_screen_with(&mut a, g, vh_const_satb_null_old(core), edge);
    }
    // JIT round 21, lane g1vh: or G1's (pre-gate / null old value, region).
    let post = core
        .g1
        .and_then(|g1| vh_g1_store_screen(&mut a, core.op, core.disp, g1))
        .or(card_edge);
    vh_const_access(&mut a, core);
    (a.b, a.decline, post)
}

/// The access alone of [`vh_const_core_bytes`] (JIT round 21, lane g1vh: the
/// G1 post edge's caller emits it again on that edge): on entry RAX = the
/// receiver, R10 = `expected`, RDX = the value, all screened; on exit RAX =
/// the site's result. Returns the bytes and the rel32 decline fields of the
/// cast arms' own screens, which all run BEFORE any write. Writes RAX, RCX,
/// RDX, R10, R11 and the flags.
pub(crate) fn vh_const_access_bytes(core: &VhConstCore) -> (Vec<u8>, Vec<usize>) {
    let mut a = VhConstAsm::default();
    vh_const_access(&mut a, core);
    (a.b, a.decline)
}

/// JIT round 21, lane g1vh: [`VhG1Gates`]' screen, after the receiver and
/// value screens (RAX = the receiver, R10 = `expected`, RDX = the value; RCX
/// and R11 free). Falls through when no barrier has work; returns the rel32
/// field of the cross-region post edge when `g.post_call`, else declines it.
/// Writes RCX, R11 and the flags.
///
/// ```text
///   MOV R11, imm64 pre ; CMP BYTE [R11], 0 ; JE no_mark
///   ; marking -- the replaced value must be null:
///   set*:   MOV RCX, [RAX + disp] ; TEST RCX, RCX ; JNZ decline
///   CAS/CAE: TEST R10, R10 ; JNZ decline
///   else:   JMP decline
/// no_mark:
///   TEST RDX, RDX ; JZ done                       ; a null value: no edge
///   MOV R11, imm64 table ; MOV R11, [R11]          ; the arena base
///   MOV RCX, RAX ; SUB RCX, R11                    ; recv - base
///   NEG R11 ; ADD R11, RDX                         ; val - base
///   XOR RCX, R11
///   MOV R11, imm64 table ; AND RCX, [R11 + 16]     ; the region mask
///   JNZ post (or decline)                          ; cross-region
/// done:
/// ```
pub(in crate::ir_lower) fn vh_g1_store_screen(a: &mut VhConstAsm, op: VhConstOp, disp: i32, g: VhG1Gates) -> Option<usize> {
    use crate::direct_helpers::UnsafeRmwOp;
    a.emit(&[0x49, 0xBB]); // MOV R11, imm64 (the SATB pre-gate byte)
    a.emit(&(g.pre as u64).to_le_bytes()); // Cast: a data address
    a.emit(&[0x41, 0x80, 0x3B, 0x00]); // CMP BYTE [R11], 0
    let no_mark = a.jcc_forward(0x84); // JE: no mark running
    match op {
        VhConstOp::Set { .. } => {
            a.emit(&[0x48, 0x8B, 0x88]); // MOV RCX, [RAX + disp32]
            a.emit(&disp.to_le_bytes());
            a.emit(&[0x48, 0x85, 0xC9]); // TEST RCX, RCX
            a.jcc_decline(0x85); // JNZ: the helper logs the old value
        }
        VhConstOp::Rmw(UnsafeRmwOp::Cas | UnsafeRmwOp::Cae) => {
            a.emit(&[0x4D, 0x85, 0xD2]); // TEST R10, R10
            a.jcc_decline(0x85); // JNZ: the replaced value would be `expected`
        }
        _ => a.jmp_decline(),
    }
    a.bind(no_mark);
    a.emit(&[0x48, 0x85, 0xD2]); // TEST RDX, RDX
    let null_value = a.jcc_forward(0x84); // JZ: a null records no edge
    a.emit(&[0x49, 0xBB]); // MOV R11, imm64 (JIT_G1_BARRIER)
    a.emit(&(g.table as u64).to_le_bytes()); // Cast: a data address
    a.emit(&[0x4D, 0x8B, 0x1B]); // MOV R11, [R11]: the arena base
    a.emit(&[0x48, 0x89, 0xC1]); // MOV RCX, RAX
    a.emit(&[0x4C, 0x29, 0xD9]); // SUB RCX, R11
    a.emit(&[0x49, 0xF7, 0xDB]); // NEG R11
    a.emit(&[0x49, 0x01, 0xD3]); // ADD R11, RDX
    a.emit(&[0x4C, 0x31, 0xD9]); // XOR RCX, R11
    a.emit(&[0x49, 0xBB]); // MOV R11, imm64 (JIT_G1_BARRIER)
    a.emit(&(g.table as u64).to_le_bytes()); // Cast: a data address
    a.emit(&[0x49, 0x23, 0x8B]); // AND RCX, [R11 + disp32]
    a.emit(&G1B_REGION_MASK.to_le_bytes());
    let post = if g.post_call {
        Some(a.jcc_forward(0x85)) // JNZ: cross-region, the post edge
    } else {
        a.jcc_decline(0x85); // JNZ: cross-region, the call
        None
    };
    a.bind(null_value);
    post
}

/// The access of [`vh_const_core_bytes`] (moved out unchanged by JIT round
/// 21, lane g1vh).
pub(in crate::ir_lower) fn vh_const_access(a: &mut VhConstAsm, core: &VhConstCore) {
    let w64 = core.kind != b'I';
    let disp = core.disp.to_le_bytes();
    match core.op {
        VhConstOp::Get => {
            if w64 {
                a.emit(&[0x48, 0x8B, 0x80]); // MOV RAX, [RAX + disp32]
            } else {
                a.emit(&[0x48, 0x63, 0x80]); // MOVSXD RAX, DWORD [RAX + disp32]
            }
            a.emit(&disp);
            // JIT round 21 wave 1, lane updaters: the invoker's cast of the
            // loaded reference (a pure load: a decline re-reads in the call).
            vh_const_result_screen(a, core.result_check, core.cid_off);
        }
        VhConstOp::Set { volatile } => {
            if w64 {
                a.emit(&[0x48]);
            }
            // XCHG [RAX + disp32], EDX / RDX (implicitly locked: the volatile
            // store's StoreLoad), or MOV [RAX + disp32], EDX / RDX.
            a.emit(&[if volatile { 0x87 } else { 0x89 }, 0x90]);
            a.emit(&disp);
        }
        VhConstOp::Rmw(crate::direct_helpers::UnsafeRmwOp::GetAndSet)
            if core.result_check != VhRefValueCheck::Admitted =>
        {
            // JIT round 21 wave 1, lane updaters (UPDATERS21-6).
            vh_const_cast_get_and_set_bytes(a, core.disp, core.result_check, core.cid_off);
        }
        VhConstOp::Rmw(crate::direct_helpers::UnsafeRmwOp::Cae)
            if core.result_check != VhRefValueCheck::Admitted =>
        {
            // JIT round 21 wave 22, lane vhconst (UPDATERS21-6, cast CAE).
            vh_const_cast_compare_and_exchange_bytes(
                a,
                core.disp,
                core.result_check,
                core.cid_off,
            );
        }
        VhConstOp::Rmw(op) => {
            a.emit(&[0x41, 0xBB]); // MOV R11D, imm32 (zero-extends): the displacement
            a.emit(&disp);
            let rmw = crate::direct_helpers::UnsafeInlineRmw {
                width: if w64 { 8 } else { 4 },
                op,
            };
            a.emit(&crate::direct_helpers::unsafe_inline_rmw_bytes(rmw));
        }
    }
}

/// JIT round 20 wave 12, lane qUnsafe (AT-4): the published reference-store
/// gate plan as [`VhRefGates`], exactly as [`vh_const_core`] builds it for a
/// reference store: `None` when nothing is published or neither post-barrier
/// shape is (no mask and no floor).
pub(crate) fn vh_ref_store_gates(
    gates: Option<(usize, usize, usize)>,
    mask: Option<u8>,
) -> Option<VhRefGates> {
    let (pre, post, floor) = gates?;
    if mask.is_none() && floor == 0 {
        return None;
    }
    Some(VhRefGates {
        pre,
        post,
        floor,
        mask,
    })
}

/// JIT round 20 wave 12, lane qUnsafe (AT-4): the reference-store gate screen
/// of [`vh_const_core_bytes`] on its own (the same function emits it there),
/// for the `Unsafe.compareAndSetReference` field arm of both tiers. On entry
/// RAX = the holder (non-null), RDX = the stored reference; falls through
/// only when the SATB pre-gate byte is clear AND (the value is null, the
/// holder is young, or there are no old objects). Writes RCX, R11 and the
/// flags. Returns the bytes and the rel32 decline fields.
pub(crate) fn vh_ref_store_gate_screen_bytes(g: VhRefGates) -> (Vec<u8>, Vec<usize>) {
    let mut a = VhConstAsm::default();
    vh_ref_store_gate_screen(&mut a, g);
    (a.b, a.decline)
}

/// `CRATONVM_JIT_VH_CONST_SATB_NULL_OLD=1` (JIT round 21 wave 16, lane
/// perf-cf-lean; perf, default ON (flipped; `=0` kills)): while the SATB pre-gate byte is
/// set (a concurrent mark is running), a constant-`VarHandle` reference store
/// whose OVERWRITTEN value is null stays inline instead of declining to the
/// call. Read per site at compile time, by [`vh_const_satb_null_old`].
///
/// Why it is sound: the pre-barrier's whole obligation is to log the value a
/// store overwrites (`ZgcRealHeap::satb_pre_barrier_slow` and
/// G1's `satb_pre_barrier` in `gc/src/g1.rs` both return at once on
/// `old == 0`), so a null
/// old value owes nothing, and the post gates below still run. This is
/// HotSpot's G1 pre-barrier filter (`G1BarrierSetAssembler::g1_write_barrier_pre`:
/// "Is the previous value null?"), and the single-pass fresh-constructor
/// store already relies on it (`x64::objects::emit_inline_fresh_ctor_compact_ref_putfield`,
/// `CRATONVM_JIT_FRESH_CTOR_OLD_VALUE_TEST`). Which value is "old":
///
/// * a CAS / compareAndExchange overwrites exactly `expected` (R10) when it
///   succeeds and nothing when it fails, so `expected == null` proves it with
///   no race at all (`CompletableFuture.completeValue`'s
///   `RESULT.compareAndSet(this, null, v)`, `tryPushStack`'s first push);
/// * a `set*` reads the field (`[RAX + disp]`) before its store. A racing
///   writer can put a value there between the read and the store, which this
///   store then overwrites unlogged -- the same window HotSpot's pre-barrier
///   has, and harmless for the same reason: a value written after the
///   snapshot was reachable from the snapshot or allocated during the mark,
///   and every deletion of its snapshot path is itself logged
///   (`CompletableFuture(Object)`'s `RESULT.setRelease(this, r)` on a fresh
///   future, `NEXT.set(c, h)` on a fresh completion).
///
/// A getAndSet / getAndBitwise overwrites a value it does not know before the
/// swap, so it keeps the decline.
pub fn vh_const_satb_null_old_enabled() -> bool {
    // R22-AB-FLIP: default ON since the JIT r22 opt-in A/B (docs/feature-designs/jit-ir-optimizations.md); `=0` is the kill switch.
    true
}

/// Where [`vh_ref_store_gate_screen_with`] finds the value a store overwrites
/// (see [`vh_const_satb_null_old_enabled`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VhSatbNullOld {
    /// A `set*`: the field at `[RAX + disp32]` (the receiver's absolute
    /// compact displacement), read before the store.
    Field(i32),
    /// A CAS / compareAndExchange: the `expected` operand in R10.
    Expected,
}

/// The null-old proof [`vh_const_core_bytes`] may use for `core`'s store, or
/// `None` (switch off, or a mode whose overwritten value is unknown).
pub(crate) fn vh_const_satb_null_old(core: &VhConstCore) -> Option<VhSatbNullOld> {
    use crate::direct_helpers::UnsafeRmwOp;
    if core.gates.is_none() || core.kind != b'L' || !vh_const_satb_null_old_enabled() {
        return None;
    }
    match core.op {
        VhConstOp::Set { .. } => Some(VhSatbNullOld::Field(core.disp)),
        VhConstOp::Rmw(UnsafeRmwOp::Cas | UnsafeRmwOp::Cae) => Some(VhSatbNullOld::Expected),
        _ => None,
    }
}

/// The gate screen itself (moved out of [`vh_const_core_bytes`] unchanged by
/// JIT round 20 wave 12, lane qUnsafe).
pub(in crate::ir_lower) fn vh_ref_store_gate_screen(a: &mut VhConstAsm, g: VhRefGates) {
    vh_ref_store_gate_screen_with(a, g, None, None);
}

/// [`vh_ref_store_gate_screen`] with an optional null-old proof
/// ([`VhSatbNullOld`]): with `None` the bytes are exactly the previous
/// screen's; with a proof, a set pre-gate (marking) declines only when the
/// overwritten value is non-null, and falls through to the post gates
/// otherwise. RAX = the holder, R10 = `expected`, RDX = the value on entry;
/// writes RCX, R11 and the flags.
///
/// j27 wave 2 (lane VHCOMP, `CRATONVM_JIT_VH_CONST_GEN_CARD_EDGE`): with
/// `card_edge`, the "a card may be owed" exit (an old holder, a non-null
/// value, old objects exist) is not a decline but an unbound `JMP rel32`
/// whose field offset is stored there, for the caller's card edge
/// (`Lowerer::emit_ir_vh_gen_card_edge`).
pub(in crate::ir_lower) fn vh_ref_store_gate_screen_with(
    a: &mut VhConstAsm,
    g: VhRefGates,
    null_old: Option<VhSatbNullOld>,
    card_edge: Option<&mut Option<usize>>,
) {
    const GC_FLAGS: u8 = cratonvm_types::GC_FLAGS_BYTE_OFFSET as u8; // Cast: disp8
                                                                     // zs s5/bt2: the fused gate in front (ZGC's age-floor plan only:
                                                                     // [`vh_fused_gate_word`]). `MOV R11, imm64; CMP word [R11], 0; JE` past
                                                                     // the whole screen when both gate bytes read zero, which is sufficient
                                                                     // for the screen's own fall-through (pre clear, and null / young / post
                                                                     // clear). Absolute, not RIP-relative: these bytes are built apart from
                                                                     // the code buffer and spliced. RCX is not written on that edge, which no
                                                                     // consumer reads (the screen's contract lists RCX as clobbered only).
    let fused_skip = vh_fused_gate_word(g).map(|word| {
        a.emit(&[0x49, 0xBB]); // MOV R11, imm64 (the fused gate word)
        a.emit(&(word as u64).to_le_bytes()); // Cast: a data address
        a.emit(&[0x66, 0x41, 0x83, 0x3B, 0x00]); // CMP word [R11], 0
        a.jcc_forward(0x84) // JE: no mark and no old objects -- no barrier owed
    });
    {
        a.emit(&[0x49, 0xBB]); // MOV R11, imm64 (the SATB pre-gate byte)
        a.emit(&(g.pre as u64).to_le_bytes()); // Cast: a data address
        a.emit(&[0x41, 0x80, 0x3B, 0x00]); // CMP BYTE [R11], 0
        match null_old {
            None => a.jcc_decline(0x85), // JNE: marking -- the helper logs the old value
            Some(old) => {
                let not_marking = a.jcc_forward(0x84); // JE: no mark running
                match old {
                    VhSatbNullOld::Field(disp) => {
                        a.emit(&[0x48, 0x83, 0xB8]); // CMP QWORD [RAX + disp32], imm8
                        a.emit(&disp.to_le_bytes());
                        a.emit(&[0x00]);
                    }
                    VhSatbNullOld::Expected => a.emit(&[0x4D, 0x85, 0xD2]), // TEST R10, R10
                }
                a.jcc_decline(0x85); // JNE: a non-null old value -- the helper logs it
                a.bind(not_marking);
            }
        }
        a.emit(&[0x48, 0x85, 0xD2]); // TEST RDX, RDX
        let null_value = a.jcc_forward(0x84); // JZ: a null records no edge
        a.emit(&[0x0F, 0xB6, 0x48, GC_FLAGS]); // MOVZX ECX, BYTE [RAX + GC_FLAGS]
        let young = match g.mask {
            Some(mask) => {
                a.emit(&[0xF6, 0xC1, mask]); // TEST CL, mask
                a.jcc_forward(0x84) // JZ: a young receiver needs no card
            }
            None => {
                a.emit(&[0x49, 0xBB]); // MOV R11, imm64 (the young floor byte)
                a.emit(&(g.floor as u64).to_le_bytes()); // Cast: a data address
                a.emit(&[0x41, 0x3A, 0x0B]); // CMP CL, BYTE [R11]
                a.jcc_forward(0x82) // JB: a young receiver needs no card
            }
        };
        a.emit(&[0x49, 0xBB]); // MOV R11, imm64 (the post gate byte)
        a.emit(&(g.post as u64).to_le_bytes()); // Cast: a data address
        a.emit(&[0x41, 0x80, 0x3B, 0x00]); // CMP BYTE [R11], 0
        let no_old = a.jcc_forward(0x84); // JZ: no old objects, no card
        match card_edge {
            // j27 wave 2 (lane VHCOMP): the caller's card edge.
            Some(edge) => {
                a.emit(&[0xE9]); // JMP rel32
                *edge = Some(a.rel32_field());
            }
            None => a.jmp_decline(), // the barrier may be owed: the call's helper runs it
        }
        a.bind(null_value);
        a.bind(young);
        a.bind(no_old);
    }
    if let Some(skip) = fused_skip {
        a.bind(skip);
    }
}

/// zs s5/bt2: the fused gate word for a [`VhRefGates`] screen -- the same
/// predicate as `x64::zgc_fused_ref_store_gate_of` (an age-FLOOR plan, no
/// mask, `post == pre + 1`: one `JitRefStoreGates` block), under both
/// `CRATONVM_JIT_ZGC_FUSED_REF_STORE_GATE` and
/// `CRATONVM_JIT_ZGC_FUSED_GATE_ALL_SITES`. `None` keeps the screen
/// byte-identical to its s3 shape (every mask plan, G1, either switch off).
pub(in crate::ir_lower) fn vh_fused_gate_word(g: VhRefGates) -> Option<usize> {
    (g.mask.is_none()
        && g.floor != 0
        && g.post == g.pre.wrapping_add(1)
        && crate::x64::zgc_fused_ref_store_gate_enabled()
        && crate::x64::zgc_fused_gate_all_sites_enabled())
    .then_some(g.pre)
}

// ── JIT round 20 wave 18, lane xVhArr (AT-2 of
// `docs/feature-designs/jit-ir-optimizations.md`): the ARRAY-ELEMENT
// arm of the constant-`VarHandle` prefix ──────────────────────────────────────
//
// A `static final` `MethodHandles.arrayElementVarHandle(int[].class)` /
// `(long[].class)` site's `get*` / `set*` / CAS / compareAndExchange /
// getAndAdd / getAndSet / getAndBitwise ran as a generic dispatch
// (`Nio21CasRace` `cas xadd lcas cae` at 1.4-2.4 us against HotSpot's 4 ns).
//
// * The VM files, at `<clinit>` end, every `static final VarHandle` holding a
//   REAL `java/lang/invoke/VarHandleInts$Array` / `VarHandleLongs$Array` (the
//   object the JDK's own factory returns in both `--real-jdk` and
//   `--compatible`) that is not invoke-exact, under the same plan table and
//   key as the field arm, as [`vh_const_array_plan_word`] -- a word the field
//   arm's [`VhConstPlan::from_word`] refuses (class id `0`, flags `2`), so the
//   two arms never read each other's rows.
// * The site shape ([`vh_const_array_site_shape`]) is `([I I values..)` or
//   `([J I values..)`, the values and the return exactly the element kind
//   (what [`vh_const_site_shape`] admits for a field of that kind). Reference
//   arrays are declined: they would need the barrier gates AND the covariant
//   array-store check, which no plan publishes here.
// * The handle proof is the field arm's (the optimizing tier: the operand IS
//   the `getstatic`; single-pass: the run-time compare with the static).
// * The screen: the array is not null, its header's kind/element byte is
//   EXACTLY a one-dimensional `int[]` / `long[]`
//   (`cratonvm_types::primitive_array_kind_tags_byte`), and the index is
//   below the length, UNSIGNED (a negative index is huge). Every miss declines
//   to the call, which raises the NPE / `ArrayIndexOutOfBoundsException` (and
//   its message, through the JDK body) exactly as before. No deopt.
// * The access, per mode, as the field arm: a `MOV` load for every get mode
//   (x86-64 TSO makes it a plain / opaque / acquire / volatile load), a `MOV`
//   store for `set` / `setOpaque` / `setRelease`, `XCHG` for `setVolatile`
//   (the StoreLoad a volatile store owes), and the `Unsafe` prefixes' own
//   locked bytes (`direct_helpers::unsafe_inline_rmw_bytes`: `LOCK CMPXCHG`,
//   `LOCK XADD`, `XCHG`, the bitwise `CMPXCHG` loop) at `16 + index * size`.
//   The natives' element CAS / RMW is one hardware atomic on the same bytes
//   (`vm_exec::hw_cas_primitive_array_element`), so the two are atomic
//   against each other. The element is naturally aligned (the payload starts
//   at 16 of an 8-aligned object).

/// `CRATONVM_JIT_VH_CONST_ARRAY_ELEMENT` (JIT r20 wave 18, lane xVhArr, AT-2;
/// **default ON**, `=0` keeps every array-element `VarHandle` access on its
/// call in both tiers and stops the VM filing array plans). Needs
/// `CRATONVM_JIT_VH_CONST_INLINE`. Read per site (and once per class
/// initialisation by the VM), so no cache and no process global.
pub fn vh_const_array_element_enabled() -> bool {
    true
}

/// The flag bit that marks an array-element plan word (bit 57: the field
/// word's flag byte reads `2`, which [`VhConstPlan::from_word`] refuses).
pub(in crate::ir_lower) const VH_CONST_ARRAY_PLAN_FLAG: i64 = 1_i64 << 57;

/// The plan word the VM files for a `static final` array-element handle of
/// element kind `kind` (`I` or `J`); `0` (nothing filed) for any other kind.
pub const fn vh_const_array_plan_word(kind: u8) -> i64 {
    match kind {
        // Cast: a byte widened into its disjoint bit field.
        b'I' | b'J' | b'L' => VH_CONST_ARRAY_PLAN_FLAG | ((kind as i64) << 48),
        _ => 0,
    }
}

/// The element kind a [`vh_const_array_plan_word`] word names, `None` for
/// every other word (a field plan, `0`).
pub const fn vh_const_array_kind_from_word(word: i64) -> Option<u8> {
    if word == vh_const_array_plan_word(b'I') {
        Some(b'I')
    } else if word == vh_const_array_plan_word(b'J') {
        Some(b'J')
    } else if word == vh_const_array_plan_word(b'L') {
        // JIT round 21 wave 1, lane updaters (UPDATERS21-5): an `Object[]`
        // element handle, filed only under `CRATONVM_JIT_VH_CONST_REF_ARRAY_GET`.
        Some(b'L')
    } else {
        None
    }
}

/// The element kind of the array-element plan the VM filed for the `static
/// final` field `(class_id, field_index)`, or `None` (a switch off, nothing
/// or a field plan filed, no resolver).
pub(crate) fn vh_const_array_plan(
    helpers: &crate::DirectHelperTable,
    class_id: u32,
    field_index: usize,
) -> Option<u8> {
    if class_id == 0 || !vh_const_inline_enabled() || !vh_const_array_element_enabled() {
        return None;
    }
    let field_index = u16::try_from(field_index).ok()?;
    vh_const_array_kind_from_word(vh_const_ask(
        helpers,
        vh_const_query(WELL_KNOWN_VH_CONST_PLAN_TAG, class_id, field_index),
    ))
}

/// Recognise an array-element `VarHandle` access-mode call the array arm may
/// serve: the coordinates `([I I` or `([J I`, then the mode's values with the
/// return, all of the ELEMENT kind exactly as [`vh_const_site_shape`] admits
/// them for a field of that kind. The returned shape's `kind` is the element
/// kind and its `values` the value operands after the index. Anything else (a
/// reference or narrower array, an `Object` / boxed / widened operand, a
/// `long` index) is `None` and keeps its call.
pub(crate) fn vh_const_array_site_shape(method: &str, descriptor: &str) -> Option<VhConstShape> {
    let (params, ret_desc) = vh_desc_params(descriptor)?;
    let (array, rest) = params.split_first()?;
    // JIT round 21 wave 1, lane updaters (UPDATERS21-5): a reference-array
    // coordinate, GET modes only (`vh_const_ref_array_site_shape`).
    if array.starts_with("[L") || array.starts_with("[[") {
        return vh_const_ref_array_site_shape(method, rest, ret_desc);
    }
    let elem = match *array {
        "[I" => b'I',
        "[J" => b'J',
        _ => return None,
    };
    let (index, values) = rest.split_first()?;
    if *index != "I" {
        return None;
    }
    // The field recogniser over the same values behind one reference
    // coordinate: the modes, counts, kinds and returns are identical.
    let field_desc = format!("(LA;{}){ret_desc}", values.concat());
    let shape = vh_const_site_shape(method, &field_desc)?;
    (shape.kind == elem).then_some(shape)
}

/// Everything the array arm's shared bytes need for one admitted site.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct VhConstArrayCore {
    pub(crate) op: VhConstOp,
    /// `I` or `J`: the element kind, and so the screened tag and the scale.
    pub(crate) kind: u8,
}

/// Decide, at compile time, whether one array-element site is served: the
/// filed element kind must be the site's. `None` keeps the call.
pub(crate) fn vh_const_array_core(shape: &VhConstShape, plan_kind: u8) -> Option<VhConstArrayCore> {
    if !matches!(shape.kind, b'I' | b'J') || shape.kind != plan_kind {
        return None;
    }
    Some(VhConstArrayCore {
        op: shape.op,
        kind: shape.kind,
    })
}

// ── JIT round 21 wave 1, lane updaters: UPDATERS21-5 / UPDATERS21-6 ────────
//
// UPDATERS21-5 (`CRATONVM_JIT_VH_CONST_REF_ARRAY_GET`, opt-in): a `static
// final` `MethodHandles.arrayElementVarHandle(Object[].class)` site's GET
// modes (`get` / `getVolatile` / `getAcquire` / `getOpaque`) as the element
// load. Sound only because of what the VM files: a REAL
// `java/lang/invoke/VarHandleReferences$Array` that is not invoke-exact and
// whose `arrayType` is exactly `Object[]` (`vm::jit::helpers::
// vh_const_array_handle_kind`). Its JDK body casts the array to `arrayType`
// (`Object[]`: every reference array passes), checks the index against the
// length and reads the element; so the screen is: the array non-null, its
// header kind/element byte exactly a REFERENCE array (`ArrayElementType::
// Reference`, which a multi-dimensional array is too), `index <u length`;
// then one `MOV` at `16 + index * 8` (x86-TSO: every get mode). A null
// array, a primitive array, an out-of-range index decline to the call (its
// NPE / ClassCastException / AIOOBE and message). The site's return, if
// narrower than `Object` (a cast context), is the invoker's cast as a screen
// on the loaded value, exactly the field arm's `result_check`. No barrier: a
// reference load on Generational / G1 (the arm is refused under an armed ZGC
// read barrier and under narrow oops, where an element is not the 8-byte
// pointer). Sets / CAS / getAndSet are NOT served: they owe the covariant
// array-store check against the array's own component class and both store
// barriers.
//
// UPDATERS21-6 (`CRATONVM_JIT_VH_CONST_CAST_RMW`, opt-in): a reference
// FIELD `getAndSet` in a cast context (`(String) VS.getAndSet(h, v)`, a
// `...)Ljava/lang/String;` site). HotSpot's invoker swaps, then casts the old
// value (a mismatch is a `ClassCastException` AFTER the swap). The prefix
// decides the cast BEFORE writing: load the old value, screen it, then `LOCK
// CMPXCHG` that exact old value to the new one; a failed screen or a lost
// race declines to the call, which swaps and casts exactly as HotSpot. A hit
// is one atomic replacement of the value it returns, so it is a getAndSet.

/// `CRATONVM_JIT_VH_CONST_REF_ARRAY_GET=1` (UPDATERS21-5; default ON (flipped; `=0` kills)): see the section comment. Off, no site is recognised and the VM files
/// no reference-array plan. Read per site and once per class initialisation.
pub fn vh_const_ref_array_get_enabled() -> bool {
    // R22-AB-FLIP: default ON since the JIT r22 opt-in A/B (docs/feature-designs/jit-ir-optimizations.md); `=0` is the kill switch.
    true
}

/// `CRATONVM_JIT_VH_CONST_CAST_RMW=1` (UPDATERS21-6; default ON (flipped; `=0` kills)):
/// see the section comment. Read per site at compile time.
pub fn vh_const_cast_rmw_enabled() -> bool {
    // R22-AB-FLIP: default ON since the JIT r22 opt-in A/B (docs/feature-designs/jit-ir-optimizations.md); `=0` is the kill switch.
    true
}

// ── JIT round 21 wave 22, lane vhconst: the rest of UPDATERS21-5 / -6 ──────
//
// UPDATERS21-6, cast compareAndExchange (`CRATONVM_JIT_VH_CONST_CAST_CAE`,
// opt-in): `(String) VS.compareAndExchange(h, e, v)` on a reference FIELD.
// HotSpot's invoker exchanges, then casts the witness (a mismatch is a
// `ClassCastException` AFTER the exchange). The prefix reads the field once
// (`O`) and decides both the outcome and the cast from that one read:
//
// * `O == expected`: the cast is screened on `O`, then `LOCK CMPXCHG` of
//   exactly `O` to the new value. A hit is the successful exchange, whose
//   witness is `O`; a lost race declines with nothing written.
// * `O != expected`: the exchange fails at the read, a failed
//   compareAndExchange is a volatile read (the field arm's `getVolatile` is
//   the same `MOV` on x86-TSO), and its witness `O` is screened.
//
// A failed screen on either path declines to the call with nothing written,
// which exchanges and casts exactly as HotSpot. `expected` and the new value
// keep the screens they had for the `Object`-returning CAE.
//
// UPDATERS21-5, reference-array stores (`CRATONVM_JIT_VH_CONST_REF_ARRAY_STORE`,
// opt-in): `set*` / CAS / compareAndExchange / getAndSet of a `static final`
// `arrayElementVarHandle(Object[].class)` (the plan of the GET arm; the VM
// files it under either switch), with an `Object`, `boolean` or discarded
// result. The JDK body (`VarHandleReferences$Array`) casts the array to
// `Object[]` (every reference array passes), checks the index, and passes
// the NEW value through `runtimeTypeCheck`: the array's own component class
// must admit it, else `ArrayStoreException` (`expected` is only compared).
// The prefix serves exactly the stores that cannot throw and owe no barrier:
//
// * the array non-null and a reference array (its header kind byte);
// * both store barriers ruled out by the published gates, as the field arm's
//   reference store ([`vh_ref_store_gate_screen_with`]: no SATB mark running,
//   or, under `CRATONVM_JIT_VH_CONST_SATB_NULL_OLD`, a null `expected`; and a
//   null value, a young array or no old objects);
// * the store check proven, as the `aastore` lowering proves it: a null
//   value, an array whose component id is `0` (`Object[]`), a PLAIN value of
//   exactly the component class, or, with the supers displays, of a subclass
//   or implementor ([`aastore_display_arm_bytes`]). An array value into a
//   typed array, and every unproven value, declines;
// * `index <u length`.
//
// Every refusal declines to the call before anything is written, so the NPE,
// the `ArrayIndexOutOfBoundsException` and its message, and the
// `ArrayStoreException`, in the JDK body's own order, are the call's. Not
// served: a cast result (`(String) VOA.getAndSet(..)`), narrow oops, an armed
// ZGC read barrier, a collector without a published gate plan (G1).

/// `CRATONVM_JIT_VH_CONST_CAST_CAE=1` (JIT round 21 wave 22, lane vhconst,
/// UPDATERS21-6; perf, default ON (flipped; `=0` kills)): see the section comment. Read
/// per site at compile time.
pub fn vh_const_cast_cae_enabled() -> bool {
    // R22-AB-FLIP: default ON since the JIT r22 opt-in A/B (docs/feature-designs/jit-ir-optimizations.md); `=0` is the kill switch.
    true
}

/// `CRATONVM_JIT_VH_CONST_REF_ARRAY_STORE=1` (JIT round 21 wave 22, lane
/// vhconst, UPDATERS21-5; perf, default ON (flipped; `=0` kills)): see the section
/// comment. Off, no reference-array store site is recognised. Read per site,
/// and by the VM once per class initialisation (it files the `Object[]` plan
/// under this switch or `CRATONVM_JIT_VH_CONST_REF_ARRAY_GET`).
pub fn vh_const_ref_array_store_enabled() -> bool {
    // R22-AB-FLIP: default ON since the JIT r22 opt-in A/B (docs/feature-designs/jit-ir-optimizations.md); `=0` is the kill switch.
    true
}

/// UPDATERS21-5: the array recogniser's reference arm. `rest` is the
/// descriptor's parameters after the array coordinate: the `int` index, then
/// the mode's values. GET modes (no values, any reference return) under
/// [`vh_const_ref_array_get_enabled`]; JIT round 21 wave 22 (lane vhconst):
/// `set*` / CAS / compareAndExchange / getAndSet with references for values
/// and a `V`, `Z` or exactly `Object` return under
/// [`vh_const_ref_array_store_enabled`]. The returned shape's `kind` is `L`.
pub(in crate::ir_lower) fn vh_const_ref_array_site_shape(
    method: &str,
    rest: &[&str],
    ret_desc: &str,
) -> Option<VhConstShape> {
    let (index, values) = rest.split_first()?;
    if *index != "I" {
        return None;
    }
    let shape = vh_const_site_shape(method, &format!("(LA;{}){ret_desc}", values.concat()))?;
    if shape.kind != b'L' {
        return None;
    }
    match shape.op {
        VhConstOp::Get => vh_const_ref_array_get_enabled().then_some(shape),
        _ => (vh_const_ref_array_store_enabled()
            && (matches!(shape.ret, b'V' | b'Z') || shape.ret_desc == "Ljava/lang/Object;"))
            .then_some(shape),
    }
}

/// What the reference-array arm's bytes need ([`vh_const_ref_array_get_bytes`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct VhConstRefArrayCore {
    /// The site's cast, screened on the loaded element (`Admitted` for an
    /// `Object` return).
    pub(crate) result_check: VhRefValueCheck,
    /// The header's class-id displacement.
    pub(crate) cid_off: i32,
}

/// UPDATERS21-5: decide one reference-array GET site at compile time: the
/// filed plan is the reference one, the access is a reference GET, narrow
/// oops are off, and a narrower return names a class the VM resolves
/// uniquely for the caller ([`vh_const_class_by_name`]). `None` keeps the call.
pub(crate) fn vh_const_ref_array_core(
    helpers: &crate::DirectHelperTable,
    shape: &VhConstShape,
    plan_kind: u8,
    caller_class_id: u32,
    cid_off: i32,
) -> Option<VhConstRefArrayCore> {
    if plan_kind != b'L'
        || shape.kind != b'L'
        || shape.op != VhConstOp::Get
        || !vh_const_ref_array_get_enabled()
        || crate::x64::narrow_oops_block_inline_fields()
    {
        return None;
    }
    let result_check = if shape.ret_desc == "Ljava/lang/Object;" {
        VhRefValueCheck::Admitted
    } else if shape.ret == b'[' {
        VhRefValueCheck::NullOnly
    } else {
        let name = shape
            .ret_desc
            .strip_prefix('L')
            .and_then(|d| d.strip_suffix(';'))?;
        let cid = vh_const_class_by_name(helpers, name, caller_class_id)?;
        match vh_const_display(helpers, cid) {
            Some(d) => VhRefValueCheck::Subtype(cid, d),
            None => VhRefValueCheck::ExactClass(cid),
        }
    };
    Some(VhConstRefArrayCore {
        result_check,
        cid_off,
    })
}

/// UPDATERS21-5: the reference-array GET bytes both tiers emit, with RAX =
/// the array and R11 = the `int` index. On the fall-through RAX = the element.
/// Writes RAX, RCX, R11 and the flags.
///
/// ```text
///   TEST RAX, RAX ; JZ decline                    ; the call's NPE
///   CMP BYTE [RAX + KIND_TAGS], REF_ARRAY ; JNE   ; not a reference array
///   CMP R11D, DWORD [RAX + LEN] ; JAE decline     ; the call's AIOOBE
///   MOV R11D, R11D ; LEA R11, [R11 * 8 + 16]
///   MOV RAX, [RAX + R11]
///   <the site's cast on RAX, or nothing>
/// ```
pub(crate) fn vh_const_ref_array_get_bytes(core: &VhConstRefArrayCore) -> (Vec<u8>, Vec<usize>) {
    const KIND_TAGS: u8 = cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8; // Cast: disp8
    const _: () = assert!(cratonvm_types::KIND_TAGS_BYTE_OFFSET <= 127);
    const LEN: u8 = crate::x64::disp::disp8_const(cratonvm_types::ARRAY_LENGTH_OFFSET as i64) as u8;
    const DATA: i32 = cratonvm_types::ARRAY_DATA_OFFSET as i32; // Cast: a small header size
    const _: () = assert!(cratonvm_types::ARRAY_DATA_OFFSET == 16);
    const REF_TAG: u8 = (cratonvm_types::ObjectKind::Array as u8)
        | ((cratonvm_types::ArrayElementType::Reference as u8) << 2);
    let mut a = VhConstAsm::default();
    a.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
    a.jcc_decline(0x84); // JZ: a null array
    a.emit(&[0x80, 0x78, KIND_TAGS, REF_TAG]); // CMP BYTE [RAX + KIND_TAGS], REF_TAG
    a.jcc_decline(0x85); // JNE: a primitive array or a plain object
    a.emit(&[0x44, 0x3B, 0x58, LEN]); // CMP R11D, DWORD [RAX + LEN]
    a.jcc_decline(0x83); // JAE (unsigned): negative or past the end
    a.emit(&[0x45, 0x89, 0xDB]); // MOV R11D, R11D (zero-extends R11)
    a.emit(&[0x4E, 0x8D, 0x1C, 0xDD]); // LEA R11, [R11 * 8 + disp32]
    a.emit(&DATA.to_le_bytes());
    a.emit(&[0x4A, 0x8B, 0x04, 0x18]); // MOV RAX, [RAX + R11]
    vh_const_result_screen(&mut a, core.result_check, core.cid_off);
    (a.b, a.decline)
}

/// UPDATERS21-6: a reference field getAndSet with its cast decided before the
/// swap. On entry RAX = the holder (screened, its barriers ruled out by the
/// gate screen), RDX = the new value (screened). On the fall-through RAX = the
/// old value, which passed `check`, and the field holds RDX. Writes RAX, RCX,
/// R10, R11 and the flags.
///
/// ```text
///   MOV R10, RAX                      ; the holder
///   MOV RAX, [R10 + disp32]           ; the old value
///   <check on RAX>                    ; a failing cast -> the call (swaps, then throws)
///   LOCK CMPXCHG [R10 + disp32], RDX  ; replace exactly that value
///   JNE decline                       ; another writer won: the call
/// ```
pub(in crate::ir_lower) fn vh_const_cast_get_and_set_bytes(
    a: &mut VhConstAsm,
    disp: i32,
    check: VhRefValueCheck,
    cid_off: i32,
) {
    a.emit(&[0x49, 0x89, 0xC2]); // MOV R10, RAX
    a.emit(&[0x49, 0x8B, 0x82]); // MOV RAX, [R10 + disp32]
    a.emit(&disp.to_le_bytes());
    vh_const_result_screen(a, check, cid_off);
    a.emit(&[0xF0, 0x49, 0x0F, 0xB1, 0x92]); // LOCK CMPXCHG [R10 + disp32], RDX
    a.emit(&disp.to_le_bytes());
    a.jcc_decline(0x85); // JNE
}

/// JIT round 21 wave 22, lane vhconst (UPDATERS21-6,
/// `CRATONVM_JIT_VH_CONST_CAST_CAE`): a reference field compareAndExchange
/// with its cast decided from ONE read of the field, before any write (the
/// section comment above [`vh_const_cast_cae_enabled`]). On entry RAX = the
/// holder (screened, its barriers ruled out), R10 = `expected`, RDX = the new
/// value (both screened). On the fall-through RAX = the witness, which passed
/// `check`. Writes RAX, RCX, R10, R11 and the flags.
///
/// ```text
///   MOV RCX, [RAX + disp32]           ; O, the one read
///   CMP RCX, R10                      ; O == expected?
///   MOV R10, RAX ; MOV RAX, RCX       ; R10 = the holder, RAX = O (flags kept)
///   JNE fail
///   <check on RAX>                    ; a failing cast -> the call
///   LOCK CMPXCHG [R10 + disp32], RDX  ; exchange exactly O
///   JNE decline                       ; another writer won: the call
///   JMP done
/// fail:                               ; the exchange failed at the read
///   <check on RAX>
/// done:
/// ```
pub(in crate::ir_lower) fn vh_const_cast_compare_and_exchange_bytes(
    a: &mut VhConstAsm,
    disp: i32,
    check: VhRefValueCheck,
    cid_off: i32,
) {
    a.emit(&[0x48, 0x8B, 0x88]); // MOV RCX, [RAX + disp32]
    a.emit(&disp.to_le_bytes());
    a.emit(&[0x4C, 0x39, 0xD1]); // CMP RCX, R10
    a.emit(&[0x49, 0x89, 0xC2]); // MOV R10, RAX
    a.emit(&[0x48, 0x89, 0xC8]); // MOV RAX, RCX
    let fail = a.jcc_forward(0x85); // JNE: O is not `expected`
    vh_const_result_screen(a, check, cid_off);
    a.emit(&[0xF0, 0x49, 0x0F, 0xB1, 0x92]); // LOCK CMPXCHG [R10 + disp32], RDX
    a.emit(&disp.to_le_bytes());
    a.jcc_decline(0x85); // JNE: the field changed since the read
    a.emit(&[0xE9]); // JMP done
    let done = a.rel32_field();
    a.bind(fail);
    vh_const_result_screen(a, check, cid_off);
    a.bind(done);
}

/// What the reference-array STORE arm's bytes need (JIT round 21 wave 22,
/// lane vhconst, UPDATERS21-5; [`vh_const_ref_array_store_screen_bytes`] and
/// [`vh_const_ref_array_store_access_bytes`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct VhConstRefArrayStoreCore {
    /// A `Set` or an `Rmw` of `Cas` / `Cae` / `GetAndSet`.
    pub(crate) op: VhConstOp,
    /// The published reference-store gates: both barriers are ruled out
    /// before the store.
    pub(crate) gates: VhRefGates,
    /// `CRATONVM_JIT_VH_CONST_SATB_NULL_OLD` for a CAS / compareAndExchange: a
    /// null `expected` proves the overwritten value null while marking.
    pub(crate) null_old_expected: bool,
    /// The supers-display cell for the store check's subclass / implementor
    /// proof ([`aastore_display_arm_bytes`]); `None` proves exact
    /// components only.
    pub(crate) display_cell: Option<usize>,
    /// The header's class-id displacement.
    pub(crate) cid_off: i32,
}

/// UPDATERS21-5 (JIT round 21 wave 22, lane vhconst): decide one
/// reference-array STORE site at compile time: the filed plan is the
/// `Object[]` one, the mode is a set / CAS / compareAndExchange / getAndSet,
/// [`vh_const_ref_array_store_enabled`], no narrow oops, and a gate plan is
/// published (`gates` / `mask`, as [`vh_ref_store_gates`] reads them).
/// `None` keeps the call.
pub(crate) fn vh_const_ref_array_store_core(
    helpers: &crate::DirectHelperTable,
    shape: &VhConstShape,
    plan_kind: u8,
    gates: Option<(usize, usize, usize)>,
    mask: Option<u8>,
    cid_off: i32,
) -> Option<VhConstRefArrayStoreCore> {
    use crate::direct_helpers::UnsafeRmwOp;
    if plan_kind != b'L'
        || shape.kind != b'L'
        || !vh_const_ref_array_store_enabled()
        || crate::x64::narrow_oops_block_inline_fields()
    {
        return None;
    }
    let null_old_expected = match shape.op {
        VhConstOp::Set { .. } | VhConstOp::Rmw(UnsafeRmwOp::GetAndSet) => false,
        VhConstOp::Rmw(UnsafeRmwOp::Cas | UnsafeRmwOp::Cae) => vh_const_satb_null_old_enabled(),
        _ => return None,
    };
    if !(matches!(shape.ret, b'V' | b'Z') || shape.ret_desc == "Ljava/lang/Object;") {
        return None;
    }
    let gates = vh_ref_store_gates(gates, mask)?;
    let display_cell = if typecheck_display_arm_enabled() {
        typecheck_display_cell(helpers)
    } else {
        None
    };
    Some(VhConstRefArrayStoreCore {
        op: shape.op,
        gates,
        null_old_expected,
        display_cell,
        cid_off,
    })
}

/// UPDATERS21-5 (JIT round 21 wave 22, lane vhconst): the reference-array
/// store arm's FIRST half, with RAX = the array, RDX = the new value and
/// R10 = `expected` (a CAS / compareAndExchange). Screens the array, both
/// barrier gates and the covariant store check; falls through only when the
/// store can neither throw (but for the index, screened by the second half)
/// nor owe a barrier. RAX is preserved; RCX, RDX, R10 and R11 are DEAD on
/// the fall-through (the caller reloads the index, `expected` and the
/// value), and the flags are written.
///
/// ```text
///   TEST RAX, RAX ; JZ decline                    ; the call's NPE
///   CMP BYTE [RAX + KIND_TAGS], REF_ARRAY ; JNE   ; not a reference array
///   <the reference-store gate screen>             ; a barrier may be owed
///   TEST RDX, RDX ; JZ ok                         ; null: always storable
///   MOV ECX, [RAX + cid_off] ; TEST ECX, ECX ; JZ ok   ; an Object[]
///   TEST BYTE [RDX + KIND_TAGS], MASK ; JNZ decline    ; an array value
///   CMP ECX, [RDX + cid_off] ; JE ok              ; exactly the component
///   (MOV EDX, [RDX + cid_off] ; <aastore display arm: hits -> ok>) | JMP decline
/// ok:
/// ```
pub(crate) fn vh_const_ref_array_store_screen_bytes(
    core: &VhConstRefArrayStoreCore,
) -> (Vec<u8>, Vec<usize>) {
    const KIND_TAGS: u8 = cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8; // Cast: disp8
    const _: () = assert!(cratonvm_types::KIND_TAGS_BYTE_OFFSET <= 127);
    const REF_TAG: u8 = (cratonvm_types::ObjectKind::Array as u8)
        | ((cratonvm_types::ArrayElementType::Reference as u8) << 2);
    let mut a = VhConstAsm::default();
    a.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
    a.jcc_decline(0x84); // JZ: a null array
    a.emit(&[0x80, 0x78, KIND_TAGS, REF_TAG]); // CMP BYTE [RAX + KIND_TAGS], REF_TAG
    a.jcc_decline(0x85); // JNE: a primitive array or a plain object
    let proof = core.null_old_expected.then_some(VhSatbNullOld::Expected);
    vh_ref_store_gate_screen_with(&mut a, core.gates, proof, None);
    let mut ok: Vec<usize> = Vec::with_capacity(5);
    a.emit(&[0x48, 0x85, 0xD2]); // TEST RDX, RDX
    ok.push(a.jcc_forward(0x84)); // JZ: a null value passes every store check
    a.emit(&[0x8B, 0x88]); // MOV ECX, DWORD [RAX + cid_off] (the component id)
    a.emit(&core.cid_off.to_le_bytes());
    a.emit(&[0x85, 0xC9]); // TEST ECX, ECX
    ok.push(a.jcc_forward(0x84)); // JZ: an Object[] admits every value
    // TEST BYTE [RDX + KIND_TAGS], KIND_TAG_BYTE_MASK: an array value's header
    // names ITS component, which an id compare would mistake for a class.
    a.emit(&[0xF6, 0x42, KIND_TAGS, cratonvm_types::KIND_TAG_BYTE_MASK]);
    a.jcc_decline(0x85);
    a.emit(&[0x3B, 0x8A]); // CMP ECX, DWORD [RDX + cid_off]
    a.emit(&core.cid_off.to_le_bytes());
    ok.push(a.jcc_forward(0x84)); // JE: exactly the component class
    match core.display_cell {
        Some(cell) => {
            a.emit(&[0x8B, 0x92]); // MOV EDX, DWORD [RDX + cid_off] (zero-extends RDX)
            a.emit(&core.cid_off.to_le_bytes());
            let (bytes, edges, hits) = aastore_display_arm_bytes(cell);
            let at = a.b.len();
            a.emit(&bytes);
            a.decline.extend(edges.into_iter().map(|e| at + e));
            ok.extend(hits.into_iter().map(|h| at + h));
        }
        None => a.jmp_decline(),
    }
    for at in ok {
        a.bind(at);
    }
    (a.b, a.decline)
}

/// UPDATERS21-5 (JIT round 21 wave 22, lane vhconst): the reference-array
/// store arm's SECOND half, with RAX = the array (screened by
/// [`vh_const_ref_array_store_screen_bytes`]), R11 = the `int` index, R10 =
/// `expected` (a CAS / compareAndExchange), RDX = the new value. Screens the
/// index, then stores; on the fall-through RAX = the site's result (`0` / `1`
/// for a CAS, the witness for compareAndExchange, the old value for
/// getAndSet; dead for a set). Writes RAX, RDX, R10, R11 and the flags.
///
/// ```text
///   CMP R11D, DWORD [RAX + LEN] ; JAE decline     ; the call's AIOOBE
///   MOV R11D, R11D ; LEA R11, [R11 * 8 + 16]
///   MOV / XCHG [RAX + R11], RDX                   ; set / setVolatile
///   | <unsafe_inline_rmw_bytes, 8 bytes wide>     ; CAS / CAE / getAndSet
/// ```
pub(crate) fn vh_const_ref_array_store_access_bytes(
    core: &VhConstRefArrayStoreCore,
) -> (Vec<u8>, Vec<usize>) {
    const LEN: u8 = crate::x64::disp::disp8_const(cratonvm_types::ARRAY_LENGTH_OFFSET as i64) as u8;
    const DATA: i32 = cratonvm_types::ARRAY_DATA_OFFSET as i32; // Cast: a small header size
    const _: () = assert!(cratonvm_types::ARRAY_DATA_OFFSET == 16);
    let mut a = VhConstAsm::default();
    a.emit(&[0x44, 0x3B, 0x58, LEN]); // CMP R11D, DWORD [RAX + LEN]
    a.jcc_decline(0x83); // JAE (unsigned): negative or past the end
    a.emit(&[0x45, 0x89, 0xDB]); // MOV R11D, R11D (zero-extends R11)
    a.emit(&[0x4E, 0x8D, 0x1C, 0xDD]); // LEA R11, [R11 * 8 + disp32]
    a.emit(&DATA.to_le_bytes());
    match core.op {
        VhConstOp::Set { volatile } => {
            // XCHG [RAX + R11], RDX (the volatile store's StoreLoad), or MOV.
            a.emit(&[0x4A, if volatile { 0x87 } else { 0x89 }, 0x14, 0x18]);
        }
        VhConstOp::Rmw(op) => {
            let rmw = crate::direct_helpers::UnsafeInlineRmw { width: 8, op };
            a.emit(&crate::direct_helpers::unsafe_inline_rmw_bytes(rmw));
        }
        VhConstOp::Get => {
            // Not a store core (`vh_const_ref_array_store_core` refuses it).
            a.jmp_decline();
        }
    }
    (a.b, a.decline)
}

/// The bytes both tiers' array arms emit after loading their operands: on
/// entry RAX = the array, R11 = the `int` index (only its low 32 bits are
/// read), R10 = `expected` (a CAS / compareAndExchange), RDX = the value
/// (`new`, the delta, the mask, the stored value). The handle has already
/// been proven. Screens the array (non-null, exactly `int[]` / `long[]`) and
/// the index (`index <u length`), then performs the access; on the
/// fall-through RAX holds the site's result (an `int` sign-extended, `0`/`1`
/// for a CAS, the old value for getAndAdd / getAndSet / getAndBitwise, the
/// witness for compareAndExchange; dead for a set). Returns the bytes and the
/// offsets of the rel32 decline fields. Writes RAX, RDX, R11 and the flags
/// (a getAndBitwise also RCX and R10; a CAS also R10).
///
/// ```text
///   TEST RAX, RAX ; JZ decline                      ; the call's NPE
///   CMP BYTE [RAX + KIND_TAGS], tag ; JNE decline   ; not exactly int[] / long[]
///   CMP R11D, DWORD [RAX + LEN] ; JAE decline       ; the call's AIOOBE
///   MOV R11D, R11D                                  ; zero-extend the index
///   LEA R11, [R11 * size + 16]                      ; the element's offset
///   <the access at [RAX + R11]>
/// ```
pub(crate) fn vh_const_array_core_bytes(core: &VhConstArrayCore) -> (Vec<u8>, Vec<usize>) {
    const KIND_TAGS: u8 = cratonvm_types::KIND_TAGS_BYTE_OFFSET as u8; // Cast: disp8
    const _: () = assert!(cratonvm_types::KIND_TAGS_BYTE_OFFSET <= 127);
    // The checked signed-disp8 narrowing every other `ir_lower.rs` length
    // load uses: a layout that pushed the length past 127 fails the build
    // here instead of addressing backwards (JIT r20 wave 20, lane inv20).
    const LEN: u8 = crate::x64::disp::disp8_const(cratonvm_types::ARRAY_LENGTH_OFFSET as i64) as u8;
    const DATA: i32 = cratonvm_types::ARRAY_DATA_OFFSET as i32; // Cast: a small header size
    const _: () = assert!(cratonvm_types::ARRAY_DATA_OFFSET == 16);
    // The whole kind/element byte of exactly a one-dimensional `int[]` /
    // `long[]` (`cratonvm_types::primitive_array_kind_tags_byte`).
    const ARRAY_KIND: u8 = cratonvm_types::ObjectKind::Array as u8;
    const INT_TAG: u8 = ARRAY_KIND | ((cratonvm_types::ArrayElementType::Int as u8) << 2);
    const LONG_TAG: u8 = ARRAY_KIND | ((cratonvm_types::ArrayElementType::Long as u8) << 2);
    let w64 = core.kind == b'J';
    let tag = if w64 { LONG_TAG } else { INT_TAG };
    let mut a = VhConstAsm::default();
    a.emit(&[0x48, 0x85, 0xC0]); // TEST RAX, RAX
    a.jcc_decline(0x84); // JZ: a null array, the call's NPE
    a.emit(&[0x80, 0x78, KIND_TAGS, tag]); // CMP BYTE [RAX + KIND_TAGS], tag
    a.jcc_decline(0x85); // JNE: any other array (or a plain object)
    a.emit(&[0x44, 0x3B, 0x58, LEN]); // CMP R11D, DWORD [RAX + LEN]
    a.jcc_decline(0x83); // JAE (unsigned): negative or past the end, the call's AIOOBE
    a.emit(&[0x45, 0x89, 0xDB]); // MOV R11D, R11D (zero-extends R11)
                                 // LEA R11, [R11 * size + disp32] (no base: SIB base 101, mod 00)
    a.emit(&[0x4E, 0x8D, 0x1C, if w64 { 0xDD } else { 0x9D }]);
    a.emit(&DATA.to_le_bytes());
    match core.op {
        VhConstOp::Get => {
            if w64 {
                a.emit(&[0x4A, 0x8B, 0x04, 0x18]); // MOV RAX, [RAX + R11]
            } else {
                a.emit(&[0x4A, 0x63, 0x04, 0x18]); // MOVSXD RAX, DWORD [RAX + R11]
            }
        }
        VhConstOp::Set { volatile } => {
            // XCHG [RAX + R11], EDX / RDX (implicitly locked: the volatile
            // store's StoreLoad), or MOV [RAX + R11], EDX / RDX.
            a.emit(&[
                if w64 { 0x4A } else { 0x42 },
                if volatile { 0x87 } else { 0x89 },
                0x14,
                0x18,
            ]);
        }
        VhConstOp::Rmw(op) => {
            let rmw = crate::direct_helpers::UnsafeInlineRmw {
                width: if w64 { 8 } else { 4 },
                op,
            };
            a.emit(&crate::direct_helpers::unsafe_inline_rmw_bytes(rmw));
        }
    }
    (a.b, a.decline)
}


/// nio21 wave 3 L3 (proposal W2L3-3, the single-pass arena fold): the bytes of
/// [`Lowerer::emit_scoped_arena_resolve`] with every deopt replaced by a
/// DECLINE edge, for a tier whose miss is a call rather than a deopt (the
/// single-pass `ScopedMemoryAccess` / `Unsafe` prefixes in
/// `x64/op_invoke.rs`). R11 holds a tagged handle (bit 62 set, positive); on
/// fall-through RDX is the real address of an access of `w` bytes inside a
/// live SLOTTED block. Writes RAX, RDX, R10; R11 is preserved. The second
/// vector holds the offset of each `Jcc rel32` displacement (in the returned
/// bytes) the caller patches to its decline. Same protocol as the optimizing
/// tier's resolve (the two key compares; see `native-builtins`
/// `unsafe_natives_ext.rs`, the section above `unsafe_arena::JIT_SLOT_SHIFT`):
///
/// ```text
///   MOV R10,R11 ; SHR R10,30 ; AND R10D,0x7FFF ; SHL R10,5
///   MOV RAX,table ; ADD R10,RAX                   ; R10 = &entry
///   MOV RAX,R11 ; SHR RAX,30 ; SHL RAX,30 ; CMP RAX,[R10] ; JNE decline
///   MOV EDX,R11D ; AND EDX,0x3FFF_FFFF            ; offset in the block
///   LEA RAX,[RDX+w] ; CMP RAX,[R10+8] ; JA decline
///   ADD RDX,[R10+16]
///   MOV RAX,R11 ; SHR RAX,30 ; SHL RAX,30 ; CMP RAX,[R10] ; JNE decline
/// ```
pub(crate) fn scoped_arena_resolve_decline_bytes(table: u64, w: u8) -> (Vec<u8>, Vec<usize>) {
    const _: () = assert!(SCOPED_ARENA_SLOT_SHIFT == 30 && SCOPED_ARENA_ENTRY_SHIFT == 5);
    const _: () = assert!(
        SCOPED_ARENA_KEY_OFFSET == 0
            && SCOPED_ARENA_LEN_OFFSET == 8
            && SCOPED_ARENA_PTR_OFFSET == 16
    );
    let index_mask: u32 = (1u32 << SCOPED_ARENA_INDEX_BITS) - 1;
    let offset_mask: u32 = (1u32 << SCOPED_ARENA_SLOT_SHIFT) - 1;
    let mut b: Vec<u8> = Vec::with_capacity(96);
    let mut decline: Vec<usize> = Vec::with_capacity(3);
    let key_check = |b: &mut Vec<u8>, decline: &mut Vec<usize>| {
        b.extend_from_slice(&[0x4C, 0x89, 0xD8]); // MOV RAX, R11
        b.extend_from_slice(&[0x48, 0xC1, 0xE8, 0x1E]); // SHR RAX, 30
        b.extend_from_slice(&[0x48, 0xC1, 0xE0, 0x1E]); // SHL RAX, 30
        b.extend_from_slice(&[0x49, 0x3B, 0x02]); // CMP RAX, [R10]  (key)
        b.extend_from_slice(&[0x0F, 0x85]); // JNE decline
        decline.push(b.len());
        b.extend_from_slice(&[0, 0, 0, 0]);
    };
    b.extend_from_slice(&[0x4D, 0x89, 0xDA]); // MOV R10, R11
    b.extend_from_slice(&[0x49, 0xC1, 0xEA, 0x1E]); // SHR R10, 30
    b.extend_from_slice(&[0x41, 0x81, 0xE2]); // AND R10D, imm32 (zero-extends)
    b.extend_from_slice(&index_mask.to_le_bytes());
    b.extend_from_slice(&[0x49, 0xC1, 0xE2, 0x05]); // SHL R10, 5
    b.extend_from_slice(&[0x48, 0xB8]); // MOV RAX, imm64
    b.extend_from_slice(&table.to_le_bytes());
    b.extend_from_slice(&[0x49, 0x01, 0xC2]); // ADD R10, RAX
    key_check(&mut b, &mut decline);
    b.extend_from_slice(&[0x44, 0x89, 0xDA]); // MOV EDX, R11D (zero-extends)
    b.extend_from_slice(&[0x81, 0xE2]); // AND EDX, imm32
    b.extend_from_slice(&offset_mask.to_le_bytes());
    b.extend_from_slice(&[0x48, 0x8D, 0x42, w]); // LEA RAX, [RDX + w]
    b.extend_from_slice(&[0x49, 0x3B, 0x42, 0x08]); // CMP RAX, [R10 + 8]  (len)
    b.extend_from_slice(&[0x0F, 0x87]); // JA decline: offset + w > len
    decline.push(b.len());
    b.extend_from_slice(&[0, 0, 0, 0]);
    b.extend_from_slice(&[0x49, 0x03, 0x52, 0x10]); // ADD RDX, [R10 + 16]  (ptr)
    key_check(&mut b, &mut decline);
    (b, decline)
}

/// `CRATONVM_JIT_IR_UNSAFE_OFFHEAP_PREFIX` (nio21 wave 4, lane L3, proposal
/// W3L3-2; **default ON**, `=0` restores the nio20 prefix exactly: a null base
/// declines to the call): the optimizing tier's `Unsafe` primitive-array
/// prefix (`Lowerer::try_emit_ir_unsafe_access_prefix`) also serves a NULL
/// base, as the single-pass prefix does since nio21 wave 3
/// (`CRATONVM_JIT_SP_UNSAFE_OFFHEAP_PREFIX`): a raw off-heap address, or an
/// `Unsafe.allocateMemory` arena handle resolved through the JIT-readable
/// arena table (absent under `CRATONVM_JIT_IR_SCOPED_ARENA_FOLD=0` /
/// `CRATONVM_UNSAFE_ARENA_JIT_TABLE=0`, when a handle declines). Read per
/// site compile, so it adds no process global.
pub(crate) fn ir_unsafe_offheap_prefix_enabled() -> bool {
    true
}

/// `CRATONVM_JIT_UNSAFE_STATIC_VOLATILE_INLINE` (t1 wave 9, lane ATOM2;
/// JIT round 21 proposal ATOMICS21-1, book
/// `docs/feature-designs/jit-ir-optimizations.md`;
/// default ON since j27 2026-10-28, `=0` kills): the `{get,put}{Int,Long}Volatile` (and `Acquire` / `Opaque` /
/// `Release`) accessor prefixes of BOTH tiers also serve a static -- a
/// `staticFieldBase` mirror base with a static-band offset -- through the
/// STATICCAS21-2 static entries (`direct_helpers::unsafe_inline_static_cell_bytes`):
/// a get is one `MOV` of the static's payload word, a volatile put one
/// `XCHG` (an ordered put one `MOV`). The same word the natives' hardware
/// static CAS (`vm_exec::hw_static_cell_update`) and the compiled static RMW
/// arm update, so the load / store is single-copy atomic with every atomic of
/// the static. Entries exist only for a static an atomic already ran on
/// (published by `synthetic_cas`), so a static that is only read stays on its
/// call. `None` (no static arm): the switch at `=0`, an access that is not a
/// naturally aligned int / long volatile row, or no static half
/// (`unsafe_static_arm_table`: no cell table, `CRATONVM_JIT_UNSAFE_STATIC_RMW_INLINE=0`,
/// an armed ZGC read barrier). Read per site compile, so no process global.
pub(crate) fn unsafe_volatile_static_arm_table(
    table: &crate::DirectHelperTable,
    access: crate::direct_helpers::UnsafeInlineAccess,
) -> Option<usize> {
    if !access.volatile
        || access.unaligned
        || access.big_endian_operand
        || access.value_kind != b'I'
        || !matches!(access.width, 4 | 8)
        // WARNING j27-user-flip-20261028 (default ON; =0 restores the old default)
        || !true
    {
        return None;
    }
    crate::direct_helpers::unsafe_static_arm_table(table)
}

/// nio21 wave 4 L3 (W3L3-2): the address half of an `Unsafe` access whose
/// base is null, as pure bytes with DECLINE edges (the second vector: the
/// offset of each `Jcc rel32` displacement the caller patches to its
/// decline). R11 holds the address operand; on fall-through RDX is the real
/// address of a `w`-byte access. Writes RAX, RDX, R10; R11 is preserved.
/// The single-pass `emit_sp_offheap_address` emits the same sequence:
///
/// ```text
///   TEST R11,R11 ; JLE decline            ; addr <= 0
///   LEA RDX,[R11-0x2000_0000] ; CMP RDX,0x1FFF_FFFF ; JBE decline   ; minted static offset
///   MOV RAX,1<<62 ; TEST R11,RAX
///   table:  JZ untagged ; <scoped_arena_resolve_decline_bytes> ; JMP done
///           untagged: MOV RDX,R11 ; done:
///   none:   JNZ decline ; MOV RDX,R11     ; a handle with no table declines
/// ```
///
/// A positive untagged address is below `2^62`, so `addr + w` cannot wrap.
/// A legacy handle carries bit 62 too (`ARENA_BASE` in `native-builtins`
/// `unsafe_natives_ext.rs`), and its key compare declines.
pub(crate) fn unsafe_offheap_address_decline_bytes(
    table: Option<u64>,
    w: u8,
) -> (Vec<u8>, Vec<usize>) {
    let mut b: Vec<u8> = Vec::with_capacity(128);
    let mut decline: Vec<usize> = Vec::with_capacity(5);
    let patch_to_here = |b: &mut Vec<u8>, operand: usize| {
        // Cast: a displacement inside a buffer of at most a few hundred bytes.
        let rel = (b.len() as i32) - (operand as i32 + 4);
        b[operand..operand + 4].copy_from_slice(&rel.to_le_bytes());
    };
    b.extend_from_slice(&[0x4D, 0x85, 0xDB]); // TEST R11, R11
    b.extend_from_slice(&[0x0F, 0x8E]); // JLE decline: addr <= 0
    decline.push(b.len());
    b.extend_from_slice(&[0, 0, 0, 0]);
    // A minted static offset (`[0x2000_0000, 0x3FFF_FFFF]`, a null base) is not a
    // pointer: the natives resolve it as the static, so the call must.
    b.extend_from_slice(&[0x49, 0x8D, 0x93, 0x00, 0x00, 0x00, 0xE0]); // LEA RDX, [R11 - 0x2000_0000]
    b.extend_from_slice(&[0x48, 0x81, 0xFA, 0xFF, 0xFF, 0xFF, 0x1F]); // CMP RDX, 0x1FFF_FFFF
    b.extend_from_slice(&[0x0F, 0x86]); // JBE decline: inside the band
    decline.push(b.len());
    b.extend_from_slice(&[0, 0, 0, 0]);
    b.extend_from_slice(&[0x48, 0xB8]); // MOV RAX, imm64
    b.extend_from_slice(&(1u64 << 62).to_le_bytes());
    b.extend_from_slice(&[0x49, 0x85, 0xC3]); // TEST R11, RAX
    match table {
        Some(table) => {
            b.extend_from_slice(&[0x0F, 0x84]); // JZ untagged
            let untagged = b.len();
            b.extend_from_slice(&[0, 0, 0, 0]);
            let (resolve, edges) = scoped_arena_resolve_decline_bytes(table, w);
            let at = b.len();
            b.extend_from_slice(&resolve);
            decline.extend(edges.into_iter().map(|e| at + e));
            b.push(0xE9); // JMP done
            let done = b.len();
            b.extend_from_slice(&[0, 0, 0, 0]);
            patch_to_here(&mut b, untagged);
            b.extend_from_slice(&[0x4C, 0x89, 0xDA]); // MOV RDX, R11
            patch_to_here(&mut b, done);
        }
        None => {
            b.extend_from_slice(&[0x0F, 0x85]); // JNZ decline: a handle
            decline.push(b.len());
            b.extend_from_slice(&[0, 0, 0, 0]);
            b.extend_from_slice(&[0x4C, 0x89, 0xDA]); // MOV RDX, R11
        }
    }
    (b, decline)
}


/// `CRATONVM_JIT_IR_SCOPED_CONFINED_FOLD` (nio20 wave 5, lane L2, **default
/// ON**, `=0` keeps a confined session a deopt): the confined arm of the scoped
/// session fold ([`scoped_session_confined_rows`]). Also off under
/// `CRATONVM_JIT_IR_SCOPED_SESSION_FOLD=0`. Read per site compile.
pub(crate) fn ir_scoped_confined_fold_enabled() -> bool {
    ir_scoped_session_fold_enabled()
}

/// nio21 wave 2 (lane L3, W1L3-2): the session MODEL's never-closing state
/// word, `native-builtins` `foreign_ffm.rs` `P67_SESSION_STATE_NONCLOSEABLE`
/// (`1` is open-and-closeable, `0` closed). The VM's `jit_well_known_class_id`
/// arm for the model's state slot asserts the two copies agree.
pub const SCOPED_MODEL_STATE_NONCLOSEABLE: u8 = 2;

/// `CRATONVM_JIT_IR_SCOPED_NONCLOSEABLE_MODEL_FOLD` (nio21 wave 2, lane L3,
/// W1L3-2), **default ON**, `=0` keeps a model session with a null owner a
/// deopt: admit a session-MODEL instance whose state reads
/// [`SCOPED_MODEL_STATE_NONCLOSEABLE`] with a null owner, beside the confined
/// test of [`scoped_session_confined_rows`]' model row. Inert while the model
/// mints no such session (`CRATONVM_FFM_SESSION_NONCLOSEABLE_KIND=0`). Read
/// per site compile, so it adds no process global.
pub(crate) fn ir_scoped_noncloseable_model_fold_enabled() -> bool {
    true
}

/// How an admitted confined session's `state` word reads OPEN.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ScopedSessionOpen {
    /// The JDK's encoding (`MemorySessionImpl.OPEN == 0`, `CLOSED == -1`, a
    /// confined session's acquire count above 0): `state >= 0`.
    NonNegative,
    /// The model's encoding (`p67_memory_session`): `1` open, `0` closed.
    EqualsOne,
}

/// One confined-session class the scoped fold admits inline (nio20 wave 5,
/// lane L2): its exact class id, the per-object COMPACT / LEGACY addresses of
/// its `state` int and `owner` reference, and the OPEN encoding.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ScopedConfinedRow {
    pub class_id: u32,
    pub state_compact: i32,
    pub state_legacy: i32,
    /// The owner's 8-byte pointer in a compact instance.
    pub owner_compact: i32,
    /// The owner cell's tag word and 8-byte payload in a legacy instance.
    pub owner_legacy_tag: i32,
    pub owner_legacy: i32,
    pub open: ScopedSessionOpen,
    /// JIT round 22 (lane memory, MEM20-2 residual): the JDK's
    /// `SharedSession` row. Its owner is always null, so the confined test
    /// never passes it: the row exists only to classify an open session into
    /// the shared window ([`Lowerer::scoped_shared_window_for`]), and every
    /// other consumer skips it.
    pub shared_only: bool,
}

/// The compact / legacy addresses of reference field `index` of `class_id`
/// (the 64-bit twin of `AtomicIntFieldLayout::new` for a reference): `None`
/// for a compact slot that is not an 8-byte (wide) reference -- a narrow oop
/// would need decoding, so it keeps the deopt.
pub(in crate::ir_lower) fn scoped_ref_field_offsets(index: usize, class_id: u32) -> Option<(i32, i32, i32)> {
    if class_id == 0 {
        return None;
    }
    let cell =
        cratonvm_types::HEADER_SIZE.checked_add(index.checked_mul(cratonvm_types::SLOT_SIZE)?)?;
    let tag = i32::try_from(cell + cratonvm_types::FIELD_CELL_TAG_OFFSET).ok()?;
    let legacy = i32::try_from(cell + cratonvm_types::FIELD_CELL_PAYLOAD64_OFFSET).ok()?;
    let mut compact = legacy;
    if cratonvm_types::compact_ref_fields_enabled() {
        if let Some((body_off, storage)) =
            cratonvm_types::compact_field_disp_storage(class_id, index)
        {
            if storage != cratonvm_types::FieldStorageKind::Reference || storage.size_runtime() != 8
            {
                return None;
            }
            compact = i32::try_from(body_off).ok()?;
        }
    }
    Some((compact, tag, legacy))
}

/// The thread-word offset ([`WELL_KNOWN_THREAD_JAVA_OBJ_OFFSET`]) for this
/// compile's VM, or `None`.
pub(crate) fn ir_thread_java_obj_offset(helpers: &crate::DirectHelperTable) -> Option<i32> {
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
    let plus_one = unsafe { f(ctx as i64, WELL_KNOWN_THREAD_JAVA_OBJ_OFFSET) };
    i32::try_from(plus_one.checked_sub(1)?)
        .ok()
        .filter(|&off| off >= 0)
}

