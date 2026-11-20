#[allow(unused_imports)]
use super::*;


/// Was `class_id` ever redefined? [`crate::runtime::redefine_state::class_was_redefined`],
/// memoized per thread (round 12 wave 2, lane rt, proposal R12W1-3). It
/// shadows that function for every caller in this file on purpose: the
/// native-site hit, the object-native and wrapper-class caches, the
/// HashMap/CHM/Matcher receiver guards. Once any class in the process was
/// redefined, each of them took the class-manager read lock per call.
///
/// Sound for the reason [`site_class_was_redefined`] is: a `false` turns
/// `true` only through a redefinition of this VM, which bumps the class's
/// generation under the class-manager writer before
/// `JitCache::bump_redefine_epoch`, and the epoch is read before the verdict.
/// A stale `true` only keeps a fast path off. The class-definition epoch in
/// the stamp retires a `true` whose class id an unload freed and a new class
/// took. Kill switch `CRATONVM_JIT_REDEFINED_CLASS_MEMO=0`.
#[inline]
pub fn class_was_redefined(vm: &SharedVm, class_id: ClassId) -> bool {
    if !crate::classloading::any_class_redefined() {
        return false;
    }
    if !redefined_class_memo_enabled() {
        return crate::runtime::redefine_state::class_was_redefined(vm, class_id);
    }
    let stamp = (
        vm.vm_identity,
        vm.jit.jit_cache.redefine_epoch(),
        cratonvm_classloading::store_epochs(vm.jit.class_layout_domain).class_definition_epoch(),
    );
    let raw = class_id.as_u32();
    let memo = REDEFINED_CLASS_MEMO.with(|m| {
        let m = m.borrow();
        if m.0 == stamp {
            m.1.get(&raw).copied()
        } else {
            None
        }
    });
    if let Some(verdict) = memo {
        return verdict;
    }
    let verdict = crate::runtime::redefine_state::class_was_redefined(vm, class_id);
    REDEFINED_CLASS_MEMO.with(|m| {
        let mut m = m.borrow_mut();
        if m.0 != stamp || m.1.len() >= REDEFINED_CLASS_MEMO_CAP {
            m.0 = stamp;
            m.1.clear();
        }
        m.1.insert(raw, verdict);
    });
    verdict
}

/// `CRATONVM_JIT_REDEFINED_CLASS_MEMO`: default ON; `0` / `false` / `off`
/// asks the class manager on every [`class_was_redefined`] call, as before.
pub fn redefined_class_memo_enabled() -> bool {
    pub static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, || {
        !matches!(
            Err::<String, std::env::VarError>(std::env::VarError::NotPresent).as_deref(),
            Ok("0") | Ok("false") | Ok("off")
        )
    })
}

/// Where the retire count sits in [`cratonvm_jit::jit_cache_generation`].
///
/// `0` means "every bump is a retire", which is exactly the behaviour before
/// the publish/retire split: any generation change drops every site-keyed
/// memo. A nonzero shift is sound only because `jit/src/lib.rs` bumps the high
/// half on every withdrawal and in `Drop for CompiledMethod` (without those, a
/// recycled `JitInvokeInfo` address would keep the previous site's positive
/// memo); both landed together in round 11 wave 9
/// (`r11w9-rt-lib-publish-retire-generation-patch-FIXED-20260924.md`).
pub const JIT_RETIRE_SHIFT: u32 = cratonvm_jit::JIT_CACHE_RETIRE_SHIFT;

/// What a change of the JIT cache generation requires of the site-keyed memos.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SiteMemoFlush {
    /// The generation did not move.
    Nothing,
    /// Only publications moved it: a new key cannot falsify a positive memo,
    /// and no `JitInvokeInfo` address was freed.
    PublicationSensitive,
    /// A body was replaced, withdrawn or freed.
    All,
}

/// Decide the flush for a thread that last saw `previous` and now sees `now`.
///
/// Pure, so both settings of the shift are unit-tested whatever
/// [`JIT_RETIRE_SHIFT`] is in the build. A carry out of the publish half into
/// the retire half reads as a retire: a spurious full flush, which is safe.
#[inline]
pub fn site_memo_flush(previous: u64, now: u64, retire_shift: u32) -> SiteMemoFlush {
    if previous == now {
        SiteMemoFlush::Nothing
    } else if previous.checked_shr(retire_shift).unwrap_or(0)
        != now.checked_shr(retire_shift).unwrap_or(0)
    {
        SiteMemoFlush::All
    } else {
        SiteMemoFlush::PublicationSensitive
    }
}

/// Flush every per-thread memo keyed by [`JitSiteKey`], so a probe cannot serve
/// one call site's resolution to a different one.
///
/// **The key half that makes this mandatory is the `JitInvokeInfo` pointer.**
/// Those boxes are owned by `CompiledMethod::_jit_invoke_infos` and are freed
/// when that method drops (see the field doc in `jit/src/lib.rs`). The allocator
/// is then free to hand the same address to the next compile's `JitInvokeInfo` —
/// at which point `(vm_identity, info_ptr)` names a DIFFERENT call site while
/// every memo still holds the old site's answer. What the old site's answer is
/// decides how the aliasing surfaces:
///
///  * `NATIVE_SITE_CACHE` holds a resolved leaf-native callback, so the reused
///    site CALLS the previous site's native and returns whatever that returns —
///    e.g. a `boolean` 1 landing in a slot the caller then treats as an
///    `Annotation[]`, whose `arraylength` reads `1 + 0xC`. That is the
///    `EXCEPTION_ACCESS_VIOLATION ... read at address 0x000000000000000D`
///    signature of the `OffsetDateTimeTest` discovery crash.
///  * `VIRTUAL_TARGET_CACHE` holds the resolved dispatch CLASS NAME, so the
///    reused site resolves its own (correct) method name against the previous
///    site's class — surfacing as `NoSuchMethodError:
///    java.lang.Object.annotationType()` /
///    `java.lang.Class.annotationType()`, a real method name against a class
///    that never declared it.
///  * `OBJECT_NATIVE_DISPATCH_CACHE` / `INTEGER_NATIVE_DISPATCH_CACHE` hold the
///    same shape of decision for their own fast paths.
///
/// Only `DISPATCH_CACHE` and `VIRTUAL_DISPATCH_CACHE` were flushed here before,
/// because the hazard was framed as "a raw entry pointer can go stale". The
/// address-reuse hazard is broader: a memo does not have to hold a code pointer
/// to be wrong once its key stops identifying its site.
///
/// The JIT cache generation closes it exactly: publishing the new
/// `CompiledMethod` that owns the reused `JitInvokeInfo` is itself an
/// unconditional `JIT_CACHE_GENERATION.fetch_add` (`JitCache::put` /
/// `put_osr`), so an address can never be re-issued without a generation the
/// thread has not yet seen — and this runs before every consult.
///
/// Cost on the steady state is unchanged: two thread-local compares, and the
/// `clear()` calls only ever execute inside the changed-generation branch.
///
/// **Which memos a change drops** is [`site_memo_flush`]'s decision. Once
/// the generation word carries a retire count in its high half
/// ([`JIT_RETIRE_SHIFT`]), a publication that retired nothing drops only the
/// `publication_sensitive` memos, and the positive ones survive a compile
/// storm. The address-reuse argument above then rests on the retire bump in
/// `Drop for CompiledMethod`, which runs before the boxes are freed, instead
/// of on the next publication.
pub fn flush_raw_entry_dispatch_caches() {
    // Every compiled publication/invalidation advances this generation. Flush
    // the site-keyed memos before probing them, to pick up tier replacements,
    // to release their code owners after invalidation, and above all so a
    // recycled `JitInvokeInfo` address cannot inherit the previous site's
    // resolution.
    let generation = cratonvm_jit::jit_cache_generation();
    let previous = DISPATCH_CACHE_JIT_GENERATION.with(|seen| seen.replace(generation));
    match site_memo_flush(previous, generation, JIT_RETIRE_SHIFT) {
        SiteMemoFlush::Nothing => {}
        SiteMemoFlush::PublicationSensitive => clear_publication_sensitive_site_memos(),
        SiteMemoFlush::All => clear_site_keyed_dispatch_memos(),
    }
    // No supersede-epoch check here any more (interpreter round i1 wave 19).
    // It cleared the publication-sensitive memos a second time for an event
    // the generation above had already reported: the epoch's one bumper (the
    // C2 publish in `background_compile_task`) runs after `JitCache::put`,
    // whose unconditional publish-half bump is a `PublicationSensitive` flush
    // here, and a C2 body that REPLACED one marked the old body `superseded`
    // before that bump, so a positive memo on the old body is dropped at its
    // next hit (`DispatchCache::names_a_stale_body`). The epoch is per VM now
    // (`JitRealm::supersede_epoch`) and this function has no VM, so keeping
    // the check would have meant one VM's supersede clearing another VM's
    // memos, or a TLS read and an atomic load per dispatch for nothing.
}

/// `CRATONVM_DBG_SITE_ALIAS=1` — count of dispatch-helper entries whose `JitSiteKey` named a
/// DIFFERENT call site than the one that first used it — i.e. a `JitInvokeInfo`
/// address that was freed with its `CompiledMethod` and re-issued.
///
/// This measures the PRECONDITION of the recycled-address defect rather than
/// its (rare, workload-dependent) visible corruption, which is why it can
/// answer "does this workload alias?" in a single run.
pub static SITE_ALIAS_HITS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
pub static SITE_ALIAS_KEYS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn site_alias_hit_count() -> u64 {
    SITE_ALIAS_HITS.load(std::sync::atomic::Ordering::Relaxed)
}
pub fn site_alias_key_count() -> u64 {
    SITE_ALIAS_KEYS.load(std::sync::atomic::Ordering::Relaxed)
}

thread_local! {
    /// `JitSiteKey -> the (class, method, descriptor) that key FIRST named`.
    /// Diagnostic-only; unbounded on purpose so nothing is missed.
    pub static SITE_IDENTITY: std::cell::RefCell<
        rustc_hash::FxHashMap<JitSiteKey, (String, String, String)>,
    > = std::cell::RefCell::new(rustc_hash::FxHashMap::default());
}

/// Record/verify what this `JitSiteKey` names. Called at the
/// top of both dispatch entry points, so it is independent of WHICH memo a
/// given site would have consulted.
#[cold]
#[inline(never)]
pub fn note_site_identity(info_key: JitSiteKey, info: &JitInvokeInfo) {
    SITE_IDENTITY.with(|m| {
        let mut m = m.borrow_mut();
        match m.get(&info_key) {
            Some((c, mm, d)) => {
                if c != info.class_name || mm != info.method_name || d != info.descriptor {
                    SITE_ALIAS_HITS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    // The running totals ride on the event line, not only on a
                    // shutdown summary: a JUnit runner ends the process with
                    // `System.exit`, so anything printed at VM shutdown is
                    // unreachable in exactly the workloads this exists for.
                    pub static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                    let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    if n < 40 {
                        eprintln!(
                            "[site-alias] #{} of {} keys: key={:#x} WAS {}.{}{} NOW {}.{}{}",
                            n + 1,
                            SITE_ALIAS_KEYS.load(std::sync::atomic::Ordering::Relaxed),
                            info_key.1,
                            c,
                            mm,
                            d,
                            info.class_name,
                            info.method_name,
                            info.descriptor
                        );
                        if n + 1 == 40 {
                            eprintln!("[site-alias] (further hits are counted, not printed)");
                        }
                    }
                    m.insert(
                        info_key,
                        (
                            info.class_name.to_string(),
                            info.method_name.to_string(),
                            info.descriptor.to_string(),
                        ),
                    );
                }
            }
            None => {
                SITE_ALIAS_KEYS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                m.insert(
                    info_key,
                    (
                        info.class_name.to_string(),
                        info.method_name.to_string(),
                        info.descriptor.to_string(),
                    ),
                );
            }
        }
    });
}

pub fn site_alias_detect_enabled() -> bool {
    pub static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    // `runtime_var_os`, not `std::env::var_os`: this name IS declared
    // (`CRATONVM_DBG=site-alias`), and a raw read on a declared name is served
    // by a live `getenv` instead of the latched snapshot — so the grouped
    // spelling silently did nothing here, and `flags::with_thread_overrides`
    // could not arrange the probe in a test. Check 4 of
    // `tools/flag-census/check-surface.sh` names exactly this call site.
    cratonvm_jit::memo_flag::memo_bool(&ON, || false)}

/// May a callee that declares an exception table be published into the
/// machine-code MIC/PIC? **Yes, by default since 2026-08-17.**
///
/// The ban existed because the inline cascade in `jit/src/x64.rs` CALLs the
/// cached entry directly, so an `i64::MIN` deopt/exception sentinel from the
/// callee had no Rust frame to notice it and route it through the *callee's*
/// own exception table — it surfaced at the caller's epilogue as the caller's
/// own deopt.
///
/// That hole is closed. `Compiler::emit_inline_callee_deopt_check` is emitted
/// after **every** inline direct-entry CALL (both PIC slots and the MIC arm,
/// and the megamorphic hashed stub's `emit_callee_deopt_check` twin) and hands
/// the sentinel to `jit_service_callee_deopt`, a thin wrapper over the same
/// [`handle_compiled_callee_deopt_sentinel`] every helper arm uses. It landed
/// later, for the H2 `MVMap`/`DataType.read` case, and the ban was never
/// revisited against it.
///
/// **The mechanism is measured, not assumed.** `probes/CalleeExceptionTable/// SemanticsProbe.java` drives six exception-table callees (implicit AIOOBE /
/// NPE / divide, an explicit athrow, a table that does NOT cover what it
/// throws, and a `finally`) through a monomorphic interface site, with every
/// throwing call INSIDE the hot loop so it goes through the published cache.
/// Four arms, one binary:
///
/// | arm | result |
/// |---|---|
/// | HotSpot 25 (the oracle) | PASS |
/// | ban kept | PASS |
/// | ban lifted | PASS |
/// | ban lifted + `CRATONVM_JIT_SP_IC_DEOPT_CHECK=0`, **pre-interlock binary** | **FAIL 8/8 — `ArithmeticException` escapes `Div.apply`'s own `catch` to `main`** |
/// | the same arm on a binary carrying the interlock below | PASS 8/8 |
///
/// The fourth arm is the point: deleting the sentinel check is the only way to
/// make the lifted ban wrong, which is what says the check is what makes it
/// right. `SP_IC_DEOPT_CHECK` is therefore an INTERLOCK below, not a separate
/// knob — publishing while the check is suppressed is unsound, and `SkipVoid`
/// suppresses it for exactly the void callees whose return register carries no
/// value.
///
/// The fifth row is the interlock working, not the probe going blind, and the
/// difference is visible as a THROUGHPUT reading rather than a claim.
/// `NativeFunnelFloorProbe`'s try/catch rung under `MIC_EXC_TABLE_PUBLISH=1
/// SP_IC_DEOPT_CHECK=0`: **207.04 ns/op** with the interlock (nothing was
/// published — the banned reading) against **24.12 ns/op** without it
/// (published with no check — the unsound state the fourth arm catches).
/// Anyone re-running the red proof must do it on a binary that predates the
/// interlock, or they will read a refusal as a pass.
///
/// **What it buys.** `probes/NativeFunnelFloorProbe.java`, ABBA on one binary,
/// two interleaved rounds, ns/op:
///
/// | rung | ban kept | ban lifted |
/// |---|---:|---:|
/// | interface call, callee has no exception table (control) | 15.75 / 15.91 | 15.65 / 15.60 |
/// | interface call, callee has `try`/`catch` | **125.98 / 125.94** | **14.35 / 14.56** |
///
/// 8.7x, with the control rung unmoved — the whole Rust helper route measured
/// against the inline cascade the callee was barred from. The older note here
/// recorded the opposite ("indistinguishable") from `probes/LazyArmVariants.java`
/// at ten threads; that probe measures a ten-thread lock-contention shape where
/// the dispatch round trip is not the limiter, so it could not see this. Do not
/// re-derive the ban from it.
///
/// `CRATONVM_JIT_MIC_EXC_TABLE_PUBLISH=0` restores the ban.
pub fn mic_publish_exception_table_callees() -> bool {
    pub static G: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&G, || {
        // INTERLOCK. `emit_inline_callee_deopt_check` is the entire reason
        // publishing is sound; if the codegen is not emitting it at every
        // direct-entry CALL, publishing must not happen. Pairing the two here
        // means a future reader cannot turn one off and leave the other on.
        if cratonvm_jit::sp_ic_deopt_check_mode() != cratonvm_jit::SpIcDeoptCheck::On {
            return false;
        }
        !matches!(
            Err::<String, std::env::VarError>(std::env::VarError::NotPresent).as_deref(),
            Ok("0") | Ok("false")
        )
    })
}

/// Opt-out for the Rust-level compiled-entry cache the virtual-MIC helper uses
/// for exception-table callees, so one binary can be A/B'd against itself.
pub fn mic_rust_entry_cache_enabled() -> bool {
    pub static G: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&G, || {
        !false
    })
}

/// Call this site's compiled callee out of the per-thread entry cache the
/// virtual-MIC helper keeps for callees the machine-code caches are barred
/// from holding. `None` means no usable entry — resolve normally.
///
/// # Why this cache exists
///
/// A callee that declares an exception table can never be published into the
/// MIC or the PIC: the inline cascade in `jit/src/x64.rs` machine-CALLs the
/// raw entry, and nothing on that route can run
/// [`handle_compiled_callee_deopt_sentinel`] to send the callee's own trap
/// through the callee's own table. The MIC slot therefore never gets a class
/// id either, so every call to such a callee took the *cache-miss* arm — a
/// compile probe, an exception-table probe, and `invoke_or_native` — forever,
/// and the compiled body that was sitting in the JIT cache was never entered.
///
/// This map is a plain thread-local the inline cascade cannot see, so a hit
/// runs the compiled body *and* keeps the sentinel routing. It is the same
/// trade `jit_invoke_dispatch`'s virtual arm already makes for its own callees
/// (see the long note at its consult site).
///
/// Measured with `probes/LazyArmVariants.java`, whose V7/V8 pair differs only
/// by a never-taken `try`/`catch` in the delegate — the shape of
/// `CharsetCache.getCharset` — at ten threads: 8901 vs 44859 ns/op before,
/// with both delegates confirmed compiled either way.
///
/// SAFETY: same contract as the surrounding helper — `vm`/`info`/`args_slice`
/// are the live values the JIT caller passed, and `thread` is this thread's
/// exclusive borrow.
#[inline]
pub unsafe fn try_mic_rust_cached_entry(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    info_ptr: i64,
    receiver_cid: u32,
    vm_ptr: i64,
    args_slice: &[i64],
    // jr20 wave 9 / gcE: `args_slice` is a forwarded Rust copy (in no root
    // set): root it across the call whatever the site flag says.
    args_forwarded: bool,
) -> Option<i64> {
    if !mic_rust_entry_cache_enabled() || !direct_virtual_compiled_callee_entry_enabled() {
        return None;
    }
    let key = (
        jit_site_key(vm.vm_identity, info_ptr as usize),
        receiver_cid,
    );
    // The owner comes out WITH the entry: it is the keep-alive this map already
    // holds for exactly this raw pointer, so cloning it here spares the callee a
    // code-range registry round trip per call. See
    // `try_call_compiled_entry_reentrant_owned`.
    // A superseded or invalidated body is dropped here and the call takes the
    // resolving path below, which finds the current one. See
    // `DispatchCache::names_a_stale_body`.
    let (entry, needs_ctx, owner, rerun_pins) = VIRTUAL_DISPATCH_CACHE.with(|dc| {
        let mut dc = dc.borrow_mut();
        if dc.get(&key).is_some_and(DispatchCache::names_a_stale_body) {
            dc.remove(&key);
            return None;
        }
        dc.get(&key)
            .map(|c| (c.entry, c.needs_context, c.owner.clone(), c.rerun_pins))
    })?;
    // The sentinel below can re-run the callee at its handler with these
    // arguments, so they are rooted across the call AND its servicing, and
    // read back through the pins (see [`call_compiled_then_route`]; `thread`
    // is this helper's own borrow, so the pins go straight onto it).
    let pins = (rerun_pins || args_forwarded)
        .then(|| CompileArgPins::pin_raw(vm, thread, info, args_slice));
    let called = match owner.as_ref() {
        Some(owner) => {
            try_call_compiled_entry_reentrant_owned(owner, entry, needs_ctx, vm_ptr, args_slice)
        }
        None => try_call_compiled_entry_reentrant(entry, needs_ctx, vm_ptr, args_slice),
    };
    let result = called.map(|rc| {
        if rc == i64::MIN {
            if let Some(v) = handle_rust_armed_callee_sentinel(
                vm,
                thread,
                info,
                ClassId::new(receiver_cid),
                args_slice,
                pins.as_ref(),
            ) {
                return v;
            }
        }
        rc
    });
    if let Some(pins) = pins {
        pins.release(thread);
    }
    result
}

/// Publish a compiled entry into the cache [`try_mic_rust_cached_entry`] reads.
///
/// `pin_jit_entry` is the only keep-alive for the raw pointer stored here, so
/// a failure to pin simply means the entry is not cached — never a dangling
/// one.
///
/// `rerun_pins` is what [`try_mic_rust_cached_entry`] roots the arguments
/// with across the call. The two table-barred callers pass `true` (their
/// callee declares an exception table, so its sentinel can re-run it). The
/// no-room caller in the miss arm (round 11 wave 18, lane hashmap) passes the
/// site's sticky `callee_rerun_pins` flag, which is set before any table
/// callee is published at the site, so it is exact or conservative, never
/// short.
pub fn publish_mic_rust_cached_entry(
    vm_identity: usize,
    info_ptr: i64,
    receiver_cid: u32,
    entry_ptr: usize,
    needs_ctx: bool,
    rerun_pins: bool,
) {
    if !mic_rust_entry_cache_enabled() || entry_ptr == 0 {
        return;
    }
    let Some(owner) = cratonvm_jit::pin_jit_entry(entry_ptr) else {
        return;
    };
    VIRTUAL_DISPATCH_CACHE.with(|dc| {
        dc.borrow_mut().insert(
            (jit_site_key(vm_identity, info_ptr as usize), receiver_cid),
            DispatchCache {
                entry: entry_ptr,
                needs_context: needs_ctx,
                owner: Some(std::rc::Rc::new(owner.into())),
                rerun_pins,
            },
        );
    });
}

/// `CRATONVM_JIT_MEGA_DISPATCH_TABLE` (default on; `0` neither publishes into
/// nor reads the VM's shared megamorphic dispatch table,
/// `cratonvm_jit::MegaDispatchTable`, round 12 wave 2, lane calls, W2-1).
/// Cached once per process: it only selects between two correct routes.
pub fn mega_dispatch_table_enabled() -> bool {
    pub static G: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&G, || {
        true
    })
}

/// An inline-cache install was refused only because a retired way's grace
/// lags: ask for an inline-cache grace handshake if this VM's switch is on
/// (`CRATONVM_JIT_IC_GRACE_HANDSHAKE_MS`, default off, read once per VM by its
/// megamorphic table) and it has not asked within the interval. JIT round 13
/// wave 11 lane mega9's patch (`r13w11-mega9-ic-grace-handshake-patch-FIXED-20260929.md`),
/// applied in round 14 wave 1 by lane codecache. Called on a mutator inside a
/// dispatch helper, past every probe it made, so the thread itself is outside
/// every inline-cache window. `initiator` is the caller's thread id (`None`:
/// unknown, no request).
pub fn request_ic_grace_if_due(vm: &SharedVm, initiator: Option<crate::ThreadId>) {
    let Some(initiator) = initiator else {
        return;
    };
    if !vm.jit.jit_cache.mega_dispatch_table().claim_grace_request() {
        return;
    }
    let _ = crate::runtime::interpreter::request_ic_grace_handshake(vm, initiator);
}

/// Publish the body a resolving arm of `jit_invoke_virtual_mic` just put into
/// its site's PIC into the VM's shared megamorphic table as well, under the
/// selector of the site's call record (bound on the site's [`JitPICSlot`] the
/// first time), so every other site that resolves the same method finds it
/// (round 12 wave 2, lane calls, proposal W2-1 step 1).
///
/// The caller has already applied the PIC's own admission (a cacheable,
/// plain-object receiver, a body compiled for its class id, no indy trap, not
/// barred, not forbidden). On top of that the publication is withdrawn when a
/// redefinition landed after `epoch_seen` (as
/// `withdraw_ic_publication_if_redefined` does for the site's own caches).
///
/// A callee that declares an exception table is published exactly when the
/// site's own PIC takes it (`mic_publish_exception_table_callees`, round 12
/// wave 3, lane mega2). Wave 2 refused every one, because only the PUBLISHING
/// site's MIC records that its arms must root the arguments of a callee its
/// sentinel can re-run (`JitMICSlot::callee_rerun_pins`). The table's two
/// readers do not need that record: [`try_mega_dispatch_table_entry`] roots the
/// arguments on every hit, and the hashed stub's machine-code probe services a
/// sentinel exactly as the per-site hashed ways it shares its tail with. The
/// refusal only sent every such receiver past a site's own sixteen ways to the
/// helper, on every call. `CRATONVM_JIT_MEGA_TABLE_EXC_CALLEES=0` restores it.
///
/// # Safety
/// As the surrounding helper: `vm`/`info` are live and `pic` is this site's.
pub unsafe fn publish_mega_dispatch_entry(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    pic: &JitPICSlot,
    receiver_cid: u32,
    entry_ptr: u64,
    needs_ctx: bool,
    epoch_seen: u32,
) {
    if !mega_dispatch_table_enabled() || entry_ptr == 0 {
        return;
    }
    if !(mega_table_exc_callees_enabled() && mic_publish_exception_table_callees())
        && mic_callee_has_exception_table(vm, ClassId::new(receiver_cid), info)
    {
        return;
    }
    install_mega_dispatch_way(
        vm,
        info,
        Some(pic),
        receiver_cid,
        entry_ptr,
        needs_ctx,
        epoch_seen,
    );
}

/// `CRATONVM_JIT_MEGA_TABLE_EXC_CALLEES` (default on; `0` keeps every callee
/// that declares an exception table out of the shared megamorphic table, the
/// round-12-wave-2 rule). See [`publish_mega_dispatch_entry`]. Cached once
/// per process, like [`mega_dispatch_table_enabled`]: it selects between two
/// correct routes.
pub fn mega_table_exc_callees_enabled() -> bool {
    pub static G: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&G, || {
        true
    })
}

/// `CRATONVM_JIT_MEGA_TABLE_PUBLISH_LAMBDA` (default on; `0` leaves lambda
/// adapter thunks in their SAM site's own inline caches only): proposal W2-1
/// step 3, round 12 wave 3, lane mega2. See `install_lambda_inline_cache`.
pub fn mega_table_publish_lambda_enabled() -> bool {
    pub static G: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&G, || {
        true
    })
}

/// `CRATONVM_JIT_MEGA_TABLE_PUBLISH_DISPATCH` (default on; `0` has
/// `jit_invoke_dispatch`'s virtual arm fill only its per-thread memo, as
/// before): proposal W2-1 step 3, round 12 wave 3, lane mega2.
pub fn mega_table_publish_dispatch_enabled() -> bool {
    pub static G: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&G, || {
        true
    })
}

/// `CRATONVM_JIT_MEGA_SELECTOR_BY_LOADER` (default on; `0` keys a site's
/// shared-table selector on its caller's CLASS id, the round-12-wave-3 key):
/// proposal M3-2, round 12 wave 4, lane mega3. See [`mega_selector_context`].
/// Cached once per process, like its neighbours: both keys are sound, the
/// loader key only shares more.
pub fn mega_selector_by_loader_enabled() -> bool {
    pub static G: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&G, || {
        true
    })
}

/// `CRATONVM_JIT_MEGA_CLASS_SLOTS` (default on; `0` binds no column and
/// publishes into no per-class cell, so the hashed stub's class-slot probe
/// finds nothing and falls through, exactly the round-12-wave-3 behaviour):
/// proposal M3-1, round 12 wave 4, lane mega3. See [`mega_class_slot_plan`].
pub fn mega_class_slots_enabled() -> bool {
    pub static G: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&G, || {
        true
    })
}

/// `CRATONVM_JIT_MEGA_CLASS_SLOTS_IFACE` (default on; `0` gives
/// `invokeinterface` sites no column, as `invokevirtual`-only M3-1 did): an
/// interface site's column is the slot of the method in its FIRST publishing
/// receiver's own vtable (proposal W4-1, round 12 wave 4, lane mega3). See
/// [`mega_class_slot_plan`].
pub fn mega_class_slots_iface_enabled() -> bool {
    pub static G: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&G, || {
        true
    })
}

/// The resolution context a site's selector is interned under (proposal M3-2):
/// its caller's DEFINING LOADER, where the round-12-wave-3 key used the
/// caller's class id.
///
/// Why this is the same resolution. Between `jit_invoke_virtual_mic`'s entry
/// and a publication, the helper reads `info.declaring_class_id` in exactly
/// one place, [`jit_site_owner`] (called by [`receiver_selection_retarget`],
/// [`select_for_jit_receiver`] and so by `mic_callee_has_exception_table`),
/// and there only as `find_class_by_name_for_class(info.class_name, caller)`,
/// which is `find_class_by_name_for_loader(name, get_loader_id(caller))`. The
/// compile probe, the dispatch target (`virtual_dispatch_target_for_receiver`)
/// and the indy screen are functions of the receiver and the constant-pool
/// names. `jit_invoke_dispatch`'s virtual arm and the lambda-thunk arm publish
/// through the same resolution. Access checks, the one other input a caller
/// class could contribute, fail (and publish nothing) before a site binds, and
/// a site binds only through its own successful publication. Loader ids come
/// from a process-wide monotonic counter and are never reused, so a selector
/// never outlives the namespace it names; a class id context stays for a site
/// with no caller (`0`, the synthetic sites) or whose caller has no loader
/// record.
pub fn mega_selector_context(vm: &SharedVm, info: &JitInvokeInfo) -> u64 {
    let caller = info.declaring_class_id;
    if caller == 0 || !mega_selector_by_loader_enabled() {
        return u64::from(caller);
    }
    let loader = vm
        .classes
        .class_manager
        .read()
        .get_loader_id(ClassId::new(caller));
    match loader {
        Some(loader) => cratonvm_jit::mega_selector_loader_context(loader.to_native_id()),
        None => u64::from(caller),
    }
}

/// The `(column, columns)` of `receiver_cid`'s cell for this site in the
/// shared table's per-class arrays (proposal M3-1), or `None`. For an
/// `invokevirtual` site the column is its resolved method's vtable slot plus
/// one, looked up once in the vtable of the class the site's constant pool
/// names (through the caller's loader, [`jit_site_owner`]). An
/// `invokeinterface` site has no single slot across its receivers; it takes
/// the slot of the method in its FIRST publishing receiver's own vtable
/// (W4-1), which is every receiver's slot in the common shape of
/// implementations that add the method at one depth under one base, and only
/// with a PIC to bind it on (a blind site's receivers would each pick their
/// own). Either way the column is then bound on the site's PIC; `columns` is
/// the receiver's vtable length, which sizes its array the first time one is
/// made.
///
/// The column is an INDEX, never a target: a cell is called only when its key
/// -- this site's selector and the receiver's class id -- matches
/// (`cratonvm_jit::MegaDispatchTable`, "Per-class columns"), so a vtable slot
/// that does not describe JVMS 5.4.6 selection (package-private overriding
/// across packages, a slot moved by a redefinition, an interface default, a
/// receiver that declares the interface method at another slot) can only cost
/// a miss.
///
/// The selector must already be bound on `pic` (the column is stored after it).
///
/// Takes the class-manager and vtable-manager read locks one after the other,
/// never together and never under a JIT lock; a publication is cold.
pub fn mega_class_slot_plan(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    table: &cratonvm_jit::MegaDispatchTable,
    pic: Option<&JitPICSlot>,
    receiver_cid: u32,
) -> Option<(u32, u32)> {
    let virtual_site = info.invoke_kind == 0;
    let interface_site = info.invoke_kind == 2 && pic.is_some() && mega_class_slots_iface_enabled();
    if !mega_class_slots_enabled() || !(virtual_site || interface_site) {
        return None;
    }
    let bound = pic.map_or(0, JitPICSlot::mega_class_slot);
    let owner = if bound == 0 && virtual_site {
        let cm = vm.classes.class_manager.read();
        Some(jit_site_owner(&cm, info)?)
    } else {
        None
    };
    // Round 15 wave 3 (lane mega2, MG15-5): with
    // `CRATONVM_JIT_MEGA_IFACE_SELECTOR_COLUMN` on, an interface site binds the
    // column its selector's first columned site bound (off: always `None`, and
    // the site binds this receiver's own slot, W4-1). An index, never a target.
    let selector = pic.map_or(0, JitPICSlot::mega_selector);
    // JIT round 21 wave 4, lane ic (IC21-3, `CRATONVM_JIT_MEGA_IFACE_COLUMN_SPACE`):
    // or the selector's own interface-range column, which every receiver of
    // the selector shares (`None` with that switch off, as before).
    let recorded = if bound == 0 && interface_site {
        table
            .recorded_interface_column(selector)
            .or_else(|| {
                let issued = table.interface_space_column(selector);
                // Tier-1 round wave 8, lane IC: IC21-3's census row.
                if issued.is_some() {
                    disp_census::note(disp_census::MIC_IFACE_SPACE_ISSUED);
                }
                issued
            })
    } else {
        None
    };
    let (column, columns) = {
        let vtables = vm.classes.vtable_manager.read();
        let receiver_vtable = vtables.get_vtable(u64::from(receiver_cid));
        let column = if bound != 0 {
            bound
        } else if let Some(recorded) = recorded {
            recorded
        } else {
            let slot = match owner {
                Some(owner) => vtables
                    .get_vtable(u64::from(owner.as_u32()))?
                    .lookup_slot(info.method_name, info.descriptor)?,
                // An interface site: the first receiver's own slot.
                None => receiver_vtable?.lookup_slot(info.method_name, info.descriptor)?,
            };
            u32::try_from(slot).ok()?.checked_add(1)?
        };
        let columns =
            receiver_vtable.map_or(0, |vtable| u32::try_from(vtable.len()).unwrap_or(u32::MAX));
        (column, columns)
    };
    // Outside the vtable lock: the first site of the selector records its
    // column, a racing one adopts it.
    let column = if bound == 0 && interface_site && recorded.is_none() {
        table.selector_interface_column(selector, column)
    } else {
        column
    };
    // Round 17 wave 2 (lane mega2): publish at the column the site is bound to
    // AFTER the bind, which is a racing first binder's when it won
    // (`JitPICSlot::bind_mega_class_slot`, `CRATONVM_JIT_MEGA_CLASS_SLOT_CAS`).
    // For a receiver that declares the method at another depth the cell is then
    // refused (`PastArray` / `Taken`) as a later call's would be.
    let column = if bound == 0 {
        match pic {
            Some(pic) => match pic.bind_mega_class_slot(table, column) {
                0 => column,
                won => won,
            },
            None => column,
        }
    } else {
        column
    };
    Some((column, columns))
}

/// JIT round 21 wave 4, lane ic (IC21-2, `CRATONVM_JIT_MEGA_VTABLE_COLUMN_BAKED`;
/// `CompileRequest::mega_site_binder`): bind a fresh `invokevirtual` site's
/// slot, at COMPILE time, to the shared-table selector and column its first
/// publication would bind -- the selector interned from exactly the inputs
/// [`install_mega_dispatch_way`] interns ([`mega_selector_context`] and the
/// info's own fields), the column [`mega_class_slot_plan`]'s for a virtual
/// site (the resolved method's slot in the site owner's vtable, plus one) --
/// so the emitter can bake them as immediates.
///
/// One thing the compile-time bind changes: the site is bound before any of
/// its own resolutions has SUCCEEDED, and a bound site's code reads the cells
/// every site of its selector publishes. Today a site whose resolution fails
/// its access check never publishes, so never binds, so never reads another
/// site's cells. The selector key names the caller's LOADER, not its class,
/// and access is a property of the caller's class (and module), so the bind
/// is made only where access is certain for this caller
/// ([`mega_bake_access_is_certain`]); every other site keeps the
/// first-publication bind. Binds nothing for a site with no caller, a
/// substituted owner, a bound-site slot, or a slot already bound.
pub fn bake_mega_virtual_site(vm: &SharedVm, info: &JitInvokeInfo, pic: &JitPICSlot) {
    if info.invoke_kind != 0
        || info.declaring_class_id == 0
        || info.owner_class_id != 0
        || !mega_dispatch_table_enabled()
        || !mega_class_slots_enabled()
        || pic.is_bound_site()
        || pic.mega_selector() != 0
        || pic.mega_class_slot() != 0
    {
        return;
    }
    let owner = {
        let cm = vm.classes.class_manager.read();
        let Some(owner) = jit_site_owner(&cm, info) else {
            return;
        };
        if !mega_bake_access_is_certain(&cm, ClassId::new(info.declaring_class_id), owner, info) {
            return;
        }
        owner
    };
    let column = {
        let vtables = vm.classes.vtable_manager.read();
        let Some(slot) = vtables
            .get_vtable(u64::from(owner.as_u32()))
            .and_then(|vtable| vtable.lookup_slot(info.method_name, info.descriptor))
        else {
            return;
        };
        let Some(column) = u32::try_from(slot).ok().and_then(|s| s.checked_add(1)) else {
            return;
        };
        column    };
    let table = vm.jit.jit_cache.mega_dispatch_table();
    let Some(selector) = table.selector_id_in_context(
        info.class_name,
        info.method_name,
        info.descriptor,
        info.invoke_kind,
        mega_selector_context(vm, info),
        info.owner_class_id,
    ) else {
        return;
    };
    pic.bind_mega_dispatch(table, selector);
    if pic.mega_selector() == selector {
        pic.bind_mega_class_slot(table, column);
        // Tier-1 round wave 8, lane IC: IC21-2's engagement row (the emitter
        // bakes only a binding this left whole).
        if pic.baked_column_binding().is_some() {
            disp_census::note(disp_census::MIC_BAKED_BIND);
        }
    }
}

/// [`bake_mega_virtual_site`]'s access rule: may the class `caller` link an
/// `invokevirtual` of `info`'s method on the class `owner` its constant pool
/// names, whatever happens at run time? Only when `owner` is a class (not an
/// interface) in the caller's own RUNTIME package (same defining loader, same
/// package name, hence the same module), and the method it resolves to is an
/// instance, non-private method that is public or declared in that same
/// runtime package (package-private and protected members of one's own
/// runtime package are accessible). Anything else -- another package, another
/// loader, a protected member across packages, an unresolvable reference --
/// answers `false` and keeps the first-publication bind.
pub fn mega_bake_access_is_certain(
    cm: &crate::classloading::ClassManager,
    caller: ClassId,
    owner: ClassId,
    info: &JitInvokeInfo,
) -> bool {
    use cratonvm_reader::class_access_flags::MethodAccessFlags;
    let store = cm.class_store();
    let package = |name: &str| -> String {
        name.rsplit_once('/').map_or(String::new(), |(p, _)| p.to_string())
    };
    let same_runtime_package = |a: ClassId, b: ClassId| -> bool {
        let (Some(ca), Some(cb)) = (store.get(a), store.get(b)) else {
            return false;
        };
        let (Some(la), Some(lb)) = (cm.get_loader_id(a), cm.get_loader_id(b)) else {
            return false;
        };
        la.to_native_id() == lb.to_native_id() && package(&*ca.name) == package(&*cb.name)    };
    if store.get(owner).map_or(true, |c| c.is_interface()) || !same_runtime_package(caller, owner) {
        return false;
    }
    let Some(declaring) = crate::runtime::resolve::selection::resolve_declaring(
        store,
        owner,
        info.method_name,
        info.descriptor,
    ) else {
        return false;
    };
    let Some(method) = store
        .get(declaring)
        .and_then(|c| c.find_method(info.method_name, info.descriptor))
    else {
        return false;
    };
    if method.is_static() || method.access_flags.contains(MethodAccessFlags::PRIVATE) {
        return false;
    }
    method.access_flags.contains(MethodAccessFlags::PUBLIC) || same_runtime_package(caller, declaring)
}

/// Install `(receiver class, the site's selector) -> entry` into the VM's
/// shared megamorphic table, and withdraw it at once if a redefinition moved
/// the epoch since `epoch_seen` (the publisher's half of the table's
/// redefinition rule: `bump_redefine_epoch` empties the table AFTER the bump,
/// so a publication either lands before that clear or sees the new epoch
/// here). A `pic` memoizes the selector and binds the site to the table, which
/// is what lets its hashed stub probe the table in machine code; with none
/// (a blind `jit_invoke_dispatch` site) the selector is interned per
/// publication, which only the cold fills below make.
///
/// Every admission is the caller's.
///
/// # Safety
/// As the surrounding helper: `vm`/`info` are live, and `pic`, if any, is the
/// site's own.
#[allow(clippy::too_many_arguments)]
pub unsafe fn install_mega_dispatch_way(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    pic: Option<&JitPICSlot>,
    receiver_cid: u32,
    entry_ptr: u64,
    needs_ctx: bool,
    epoch_seen: u32,
) {
    if !mega_dispatch_table_enabled() || entry_ptr == 0 {
        return;
    }
    let table = vm.jit.jit_cache.mega_dispatch_table();
    let bound = pic.map_or(0, JitPICSlot::mega_selector);
    let selector = if bound != 0 {
        bound
    } else {
        // Round 12 wave 4 (lane mega3, M3-2): under the caller's loader, not
        // its class, so every site of the method in that namespace shares it.
        let Some(selector) = table.selector_id_in_context(
            info.class_name,
            info.method_name,
            info.descriptor,
            info.invoke_kind,
            mega_selector_context(vm, info),
            info.owner_class_id,
        ) else {
            return;
        };
        if let Some(pic) = pic {
            pic.bind_mega_dispatch(table, selector);
        }
        selector
    };
    // Round 12 wave 4 (lane mega3, M3-1): and the receiver's per-class cell at
    // the site's column, which the hashed stub reads by direct index.
    let class_slot = mega_class_slot_plan(vm, info, table, pic, receiver_cid);
    // t1 wave 5 (lane V4): the shared table's install is an inline-cache
    // publication too (`jit_entry_publishable`), and the blind dispatch
    // helper's arm (`dispatch.rs`) reaches it with no scope open; without one
    // its `JdkOnly` refusal fell into the process sink no VM's report reads.
    let jdk_only = crate::vm::dispatch_policy(vm).is_jdk_only();
    let _jdk_only_scope = jdk_only_ic_publication_scope(vm, jdk_only);
    let report = table.install_with_class_slot(
        receiver_cid,
        selector,
        class_slot,
        entry_ptr,
        needs_ctx,
        jdk_only,
    );
    // Round 15 wave 3 (lane mega2): the census of MG15-3 (ways only a class
    // cell holds) and of W4-1a / MG15-5 (interface cells another key holds).
    if report.cell && !report.hashed {
        disp_census::note(disp_census::MIC_CELL_ONLY_PUBLISH);
    }
    if report.cell_taken && info.invoke_kind == 2 {
        disp_census::note(disp_census::MIC_IFACE_CELL_TAKEN);
    }
    // Round 16 (lane mega, MG2-2 step 1): the two silent cell refusals.
    if report.cell_budget_refused {
        disp_census::note(disp_census::MIC_CELL_BUDGET_REFUSED);
    }
    if report.cell_past_array {
        disp_census::note(disp_census::MIC_CELL_PAST_ARRAY);
    }
    // Tier-1 round wave 8, lane IC: IC21-1's engagement row.
    if report.cell_grown {
        disp_census::note(disp_census::MIC_CELL_GROWN);
    }
    if vm.jit.jit_cache.redefine_epoch() != epoch_seen {
        // Cast: an entry address.
        table.retire_entry(entry_ptr as usize);
    }
}

/// Round 14 wave 7 (lane mega): the shape-4 census of
/// `r12w3-mega2-megamorphic-interface-call-remaining-round-trips`. At a site
/// whose PIC has no bound selector, [`try_mega_dispatch_table_entry`] reads
/// nothing; count the calls for which the table already held a way under the
/// selector the site WOULD bind (`disp_census::MIC_UNBOUND_TABLE_HELD`), i.e.
/// the full resolutions a compile-time bind (M3-5) would save. Only under
/// `CRATONVM_DBG=mic-prof`, and it binds and interns nothing (a peek), so the
/// arm it measures runs exactly as with the census off.
///
/// # Safety
/// Since round 15 wave 3 it also reads a class cell without a lock
/// (`MegaDispatchTable::lookup_site_class_cell`): the caller must be the MIC
/// helper, inside compiled code past its entry quiescent stamp.
pub unsafe fn note_unbound_selector_table_held(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    pic: &JitPICSlot,
    receiver_cid: u32,
) {
    if !mic_prof::enabled() || !mega_dispatch_table_enabled() || pic.mega_selector() != 0 {
        return;
    }
    let Some(table) = vm.jit.jit_cache.mega_dispatch_table_if_any() else {
        return;
    };
    let Some(selector) = table.interned_selector_in_context(
        info.class_name,
        info.method_name,
        info.descriptor,
        info.invoke_kind,
        mega_selector_context(vm, info),
        info.owner_class_id,
    ) else {
        return;
    };
    // Round 15 wave 3 (lane mega2, MG15-3): a way whose hashed set was full
    // lives only in the receiver's class cell; count it too, at the column
    // the site would read (the census undercounted exactly those calls).
    let held = table.lookup(receiver_cid, selector).is_some() || {
        let column = mega_census_column(vm, info, table, selector, receiver_cid);
        // SAFETY: called only from `jit_invoke_virtual_mic_body`'s megamorphic
        // arm, entered from compiled code past the handler's entry quiescent
        // stamp, and the read neither stamps, polls nor blocks.
        unsafe { table.lookup_site_class_cell(receiver_cid, selector, column) }.is_some()
    };
    if held {
        disp_census::note(disp_census::MIC_UNBOUND_TABLE_HELD);
    }
}

/// The column an UNBOUND site would read `receiver_cid`'s class cell at,
/// without binding or interning anything (round 15 wave 3, lane mega2; the
/// census of [`note_unbound_selector_table_held`] only, under mic-prof): what
/// [`mega_class_slot_plan`] would bind -- the resolved method's slot in the
/// site owner's vtable for `invokevirtual`, the selector's recorded column
/// (MG15-5) or else this receiver's own slot for `invokeinterface` -- plus
/// one; `0` when there is none. Takes the class-manager and vtable-manager
/// read locks one after the other.
pub fn mega_census_column(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    table: &cratonvm_jit::MegaDispatchTable,
    selector: u32,
    receiver_cid: u32,
) -> u32 {
    if !mega_class_slots_enabled() {
        return 0;
    }
    let owner = match info.invoke_kind {
        0 => {
            let cm = vm.classes.class_manager.read();
            match jit_site_owner(&cm, info) {
                Some(owner) => owner.as_u32(),
                None => return 0,
            }
        }
        2 if mega_class_slots_iface_enabled() => {
            if let Some(recorded) = table.recorded_interface_column(selector) {
                return recorded;
            }
            receiver_cid
        }
        _ => return 0,
    };
    let vtables = vm.classes.vtable_manager.read();
    vtables
        .get_vtable(u64::from(owner))
        .and_then(|vtable| vtable.lookup_slot(info.method_name, info.descriptor))
        .and_then(|slot| u32::try_from(slot).ok())
        .and_then(|slot| slot.checked_add(1))
        .unwrap_or(0)
}

/// The high bit of a `VIRTUAL_DISPATCH_COUNTER` value: this key was already
/// set to probe once by [`blind_dispatch_late_probe_rewind`] in this memo
/// generation. The blind arm masks it off before comparing the count (round 16
/// wave 17, lane mega5).
pub const BLIND_LATE_PROBE_MARK: u32 = 1 << 31;

/// `CRATONVM_JIT_BLIND_DISPATCH_LATE_PROBE=1` (round 16 wave 17, lane mega5;
/// **default OFF**, unmeasured): shape 6 of
/// `r12w3-mega2-megamorphic-interface-call-remaining-round-trips`, its last
/// warm-up round trip. A blind virtual key probes for a compiled callee when
/// its per-thread miss count reaches `jit_invocation_threshold` (or a doubling
/// of it, [`blind_dispatch_spent_reprobe`]). The count lives in
/// `VIRTUAL_DISPATCH_COUNTER`, a `publication_sensitive` memo: every
/// publication in the VM clears it, so during warm-up, while compiles keep
/// landing, a key keeps restarting from zero and may not reach its threshold
/// for a long time. Every call in between misses the memo, finds
/// `try_jit_virtual_bytecode_callee`'s template standing down for a callee
/// that IS compiled, and takes the by-name tail (`jit_dispatch_virtual_tail`
/// -> `invoke_or_native`). That stand-down is the evidence the probe waits
/// for: this sets the key's counter to `threshold - 1` so its NEXT call runs
/// the threshold's own probe, which then only has to find the published body.
/// Once per key per memo generation (the mark, which the same clears drop), so
/// a probe that still answers nothing (a wrapped or foreign body) costs at
/// most one probe per publication. Only the blind arm's own template call
/// (`blind`) sets it; the MIC's template calls count nothing here.
pub fn blind_dispatch_late_probe_rewind(key: (JitSiteKey, u32), blind: bool) {
    if !blind || !false {
        return;
    }
    let threshold = crate::runtime::env_cache::jit_invocation_threshold();
    if blind_late_probe_rewind_counter(key, threshold) {
        disp_census::note(disp_census::DISP_BLIND_LATE_PROBE);
    }
}

/// The counter half of [`blind_dispatch_late_probe_rewind`], switch-free so a
/// unit test can drive it: `true` when the key was set to probe.
pub fn blind_late_probe_rewind_counter(key: (JitSiteKey, u32), threshold: u32) -> bool {
    if threshold == 0 || threshold >= BLIND_LATE_PROBE_MARK {
        return false;
    }
    VIRTUAL_DISPATCH_COUNTER.with(|dc| {
        let mut counts = dc.borrow_mut();
        let raw = counts.entry(key).or_insert(0);
        if *raw & BLIND_LATE_PROBE_MARK != 0 {
            return false;
        }
        *raw = (threshold - 1) | BLIND_LATE_PROBE_MARK;
        true
    })
}

/// Round 16 wave 8 (lane mega):
/// `r16-mega-blind-dispatch-spent-key-never-reprobes-20261001`. The blind
/// virtual arm of [`jit_invoke_dispatch_body`] probes for a `(site, receiver
/// class)` key's callee once, when the key's per-thread miss count EQUALS the
/// tier-up threshold. A probe that answers no body (the callee's own
/// background compile, queued at the same invocation count, has not landed)
/// is never repeated: the count only grows past the threshold, so every later
/// call of that key on this thread is a generic call for the thread's life,
/// although the body is published moments later. `R16MegaBlindSites` reads
/// a fifth of its steady-state calls on that tail.
///
/// Called on a memo miss with the key's miss count after this one. Past the
/// threshold it counts the miss (`disp_census::DISP_BLIND_SPENT_MISS`) and,
/// under `CRATONVM_JIT_BLIND_DISPATCH_REPROBE` (default ON since the round-16
/// wave-8 A/B, `4bb285359b`; `=0` kills it), answers `true` at every doubling
/// of the threshold (2x, 4x, 8x, ...): at most 23 more probes per key over a
/// `u32` count, each one the probe the threshold ran. The switch is read only
/// at those counts, so it costs no static and no read on any other call.
/// (Round 17 wave 1, lane mega: this doc sat above `BLIND_LATE_PROBE_MARK`,
/// which round 16 wave 17 inserted between it and this function, and said
/// "default OFF".)
pub fn blind_dispatch_spent_reprobe(count: u32, threshold: u32) -> bool {
    if threshold == 0 || count <= threshold {
        return false;
    }
    disp_census::note(disp_census::DISP_BLIND_SPENT_MISS);
    if count % threshold != 0 || !(count / threshold).is_power_of_two() {
        return false;
    }
    if !true {
        return false;
    }
    disp_census::note(disp_census::DISP_BLIND_REPROBE);
    true
}

/// Round 16 wave 8 (lane mega): shape 6 of
/// `r12w3-mega2-megamorphic-interface-call-remaining-round-trips` (proposal
/// MG2-4 of `jit-r15w3-mega2-proposals-RETIRED-20261003.md`, with MG7-3's census). The blind
/// virtual arm of [`jit_invoke_dispatch_body`] (an IR site with no inline
/// cache) memoizes a compiled callee per `(site, receiver class)` in
/// `VIRTUAL_DISPATCH_CACHE`, per thread, and probes for one only once a key
/// has missed `jit_invocation_threshold` times: until then every call is a
/// generic `invoke_virtual`, even when the body was compiled long ago and the
/// VM's shared megamorphic table already holds it for this receiver under
/// the selector every site of the method in this loader shares. A fresh
/// thread, or a recompiled caller, pays that warm-up again per receiver.
///
/// This peeks the table on such a miss (`count`, the key's miss count after
/// this one, below `threshold`). When it holds a way, the miss is counted
/// (`disp_census::DISP_BLIND_TABLE_HELD`) and, under
/// `CRATONVM_JIT_MEGA_BLIND_DISPATCH_HINT=1`
/// ([`cratonvm_jit::MegaDispatchTable::blind_dispatch_hint_enabled`]), the
/// key's counter is moved to `threshold` and `true` answered: the caller runs
/// the probe its threshold would have run, now. The table only cues the
/// probe; what is called is what the probe answers, with every admission the
/// threshold path applies (receiver-derived target, the receiver's own class
/// id, `pin_jit_entry`), so a stale or foreign way can cost one early probe,
/// never a wrong callee. A key whose early probe answers nothing is past its
/// threshold, as after a failed threshold probe: one probe per key either way.
///
/// Free when off: without the switch and without `CRATONVM_DBG=mic-prof` it
/// returns before it reads anything. With either, the peek interns nothing
/// (`MegaDispatchTable::peek_interned_way`) and reads a class cell only at the
/// column the site would bind ([`mega_census_column`]).
///
/// # Safety
/// The class-cell read has [`cratonvm_jit::MegaDispatchTable::lookup_site_class_cell`]'s
/// contract: the caller is `jit_invoke_dispatch_body`, entered from compiled
/// code past its entry quiescent stamp, and the read neither stamps, polls nor
/// blocks.
pub unsafe fn blind_dispatch_table_hint(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    key: (JitSiteKey, u32),
    receiver_cid: u32,
    count: u32,
    threshold: u32,
) -> bool {
    if count >= threshold || !mega_dispatch_table_enabled() {
        return false;
    }
    let Some(table) = vm.jit.jit_cache.mega_dispatch_table_if_any() else {
        return false;
    };
    let hint = table.blind_dispatch_hint_enabled();
    if !hint && !mic_prof::enabled() {
        return false;
    }
    let Some((selector, hashed)) = table.peek_interned_way(
        receiver_cid,
        info.class_name,
        info.method_name,
        info.descriptor,
        info.invoke_kind,
        mega_selector_context(vm, info),
        info.owner_class_id,
    ) else {
        return false;
    };
    // A way whose hashed set was full lives only in the receiver's class cell
    // (MG15-3), at the column a columned site of this selector reads.
    let held = hashed || {
        let column = mega_census_column(vm, info, table, selector, receiver_cid);
        // SAFETY: the caller's contract (see above).
        unsafe { table.lookup_site_class_cell(receiver_cid, selector, column) }.is_some()
    };
    if !held {
        return false;
    }
    disp_census::note(disp_census::DISP_BLIND_TABLE_HELD);
    if !hint {
        return false;
    }
    VIRTUAL_DISPATCH_COUNTER.with(|dc| {
        dc.borrow_mut().insert(key, threshold);
    });
    disp_census::note(disp_census::DISP_BLIND_TABLE_HINT);
    true
}

/// Round 15 wave 2 (lane mega): proposal MG7-1 of `jit-r14-mega-proposals-RETIRED-20260930.md`,
/// shape 4 of `r12w3-mega2-megamorphic-interface-call-remaining-round-trips`.
/// Called by the resolving arms of `jit_invoke_virtual_mic_body` once a
/// receiver-derived target resolved (`cacheable_receiver`: no selection
/// error, the receiver's own class): bind the site's shared-table selector
/// NOW instead of at its first publication
/// (`MegaDispatchTable::bind_site_at_resolution`, switch
/// `CRATONVM_JIT_MEGA_BIND_AT_RESOLUTION`, default off). A site whose first
/// receivers publish nothing (a declined or memo-skipped compile probe, a
/// barred callee) then reads the ways every other site of the method filled,
/// in the hashed stub and in [`try_mega_dispatch_table_entry`].
///
/// The binding argument is [`mega_selector_context`]'s: the selector is
/// interned under the same context a publication would use, and a
/// publication's bind is preceded by exactly this resolution (the compile
/// probe between the two reads only the receiver). An `invokevirtual` site
/// also binds its column, which is the resolved method's slot in the site
/// OWNER's vtable and so the column its first publication would bind; an
/// `invokeinterface` site keeps W4-1's rule (the first PUBLISHING receiver's
/// slot) and binds its column at that publication, as before.
///
/// A table nobody allocated holds nothing to read, so none is made here: the
/// site binds at a later resolution or at its own publication.
pub fn bind_mega_selector_at_resolution(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    pic: &JitPICSlot,
    receiver_cid: u32,
) {
    if pic.mega_selector() != 0 || !mega_dispatch_table_enabled() {
        return;
    }
    let Some(table) = vm.jit.jit_cache.mega_dispatch_table_if_any() else {
        return;
    };
    let selector = table.bind_site_at_resolution(
        pic,
        info.class_name,
        info.method_name,
        info.descriptor,
        info.invoke_kind,
        || mega_selector_context(vm, info),
        info.owner_class_id,
    );
    if selector == 0 {
        return;
    }
    disp_census::note(disp_census::MIC_BOUND_AT_RESOLUTION);
    if info.invoke_kind == 0 {
        // The selector is bound above, so the column may follow it.
        let _ = mega_class_slot_plan(vm, info, table, Some(pic), receiver_cid);
    }
}

/// The megamorphic arm's last resort before the full resolution: the body the
/// VM's shared megamorphic table holds for `(receiver class, this site's
/// selector)`, called with its retained owner (round 12 wave 2, lane calls,
/// proposal W2-1 step 1). `None`: nothing was called.
///
/// Every hit is re-validated before the call -- the owner neither superseded
/// nor retired, no redefinition since `epoch_seen` -- so a table way an
/// invalidation pass has not reached yet costs this call the full resolution,
/// never a stale body. The arguments are rooted across the call and its
/// sentinel service, which can re-run the callee -- except for a body that owns
/// its arguments (gcg g6/jm, [`door_pins_call_args`]).
///
/// # Safety
/// As the surrounding helper: `vm`/`info` are live, `thread` is this thread's
/// exclusive borrow, `pic` is this site's, and `args_slice` is the call's
/// canonicalised argument block.
#[allow(clippy::too_many_arguments)]
pub unsafe fn try_mega_dispatch_table_entry(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    pic: &JitPICSlot,
    receiver_class_id: ClassId,
    vm_ptr: i64,
    args_slice: &[i64],
    epoch_seen: u32,
) -> Option<i64> {
    if !mega_dispatch_table_enabled() {
        return None;
    }
    let selector = pic.mega_selector();
    if selector == 0 {
        return None;
    }
    let table = vm.jit.jit_cache.mega_dispatch_table_if_any()?;
    // Round 15 wave 3 (lane mega2, MG15-3): a way whose hashed set was full
    // lives only in the receiver's class cell, which the hashed stub reads
    // and `lookup` does not; read it at this site's column before paying the
    // full resolution (`CRATONVM_JIT_MEGA_RUST_CELL_READ=0`: hashed ways only).
    let (entry, needs_ctx, owner, from_cell) =
        match table.lookup(receiver_class_id.as_u32(), selector) {
            Some((entry, needs_ctx, owner)) => (entry, needs_ctx, owner, false),
            None => {
                // SAFETY: this is the resolving helper, entered from compiled
                // code (JIT depth > 0) past its entry quiescent stamp, and
                // nothing between here and the read's return stamps again,
                // polls or blocks: the machine probe's grace covers it.
                let (entry, needs_ctx, owner) = table.lookup_site_class_cell(
                    receiver_class_id.as_u32(),
                    selector,
                    pic.mega_class_slot(),
                )?;
                (entry, needs_ctx, owner, true)
            }
        };
    let owner = owner?;
    if owner.superseded.load(std::sync::atomic::Ordering::Acquire)
        || owner.retired.load(std::sync::atomic::Ordering::Acquire)
        || vm.jit.jit_cache.redefine_epoch() != epoch_seen
    {
        return None;
    }
    // gcg g6/jm: not for a body that owns its arguments (`door_pins_call_args`).
    let pins = door_pins_call_args(
        true,
        cratonvm_jit::callee_body_owns_its_arguments(&owner),
        door_owned_args_unpinned_enabled(vm),
    )
    .then(|| CompileArgPins::pin_raw(vm, thread, info, args_slice));
    note_cached_entry_arm(&CACHED_ENTRY_OWNER_REUSE_HITS);
    // Cast: an entry address.
    let called = call_compiled_entry_under_owner(
        Some(&*owner),
        entry as usize,
        needs_ctx,
        vm_ptr,
        args_slice,
    );
    let result = called.map(|rc| {
        if rc == i64::MIN {
            handle_rust_armed_callee_sentinel(
                vm,
                thread,
                info,
                receiver_class_id,
                args_slice,
                pins.as_ref(),
            )
            .unwrap_or(rc)
        } else {
            rc
        }
    });
    if let Some(pins) = pins {
        pins.release(thread);
    }
    if from_cell && result.is_some() {
        disp_census::note(disp_census::MIC_MEGA_CELL_READ);
    }
    result
}



/// Round 12 wave 1 (lane `calls`; proposal W18-3 in
/// `docs/feature-designs/jit-ir-optimizations.md`): enter the body the resolving arm of
/// `jit_invoke_virtual_mic` just obtained from its compile probe, instead of
/// re-resolving the same callee BY NAME through `invoke_or_native` (which
/// re-derives the target, re-checks the JIT cache and re-enters through the
/// interpreter door). `None`: nothing was called, take the by-name tail as
/// before.
///
/// The call is the one the megamorphic PIC arm makes for a hashed way:
///
/// * the arguments are the DECODED ones (`full_args`, re-read from their pins
///   after the probe, which can run `<clinit>` and collect), re-encoded one
///   word per slot the way the entry ABI reads them. A value no JIT slot can
///   carry answers `None`;
/// * the body is pinned for the call, and a body already superseded or
///   retired is not entered (the by-name tail finds the current one), nor is
///   any body once a redefinition moved the epoch the caller read or the VM
///   entered interpreter-only mode;
/// * a callee that can be RE-RUN by its sentinel (an exception-table callee,
///   `rerun_pins`) has its arguments rooted across the call and the servicing,
///   exactly as [`try_mic_rust_cached_entry`] does, and the `i64::MIN`
///   sentinel goes through [`handle_rust_armed_callee_sentinel`].
///
/// The caller owns the rest of the admission: a body only for a cacheable,
/// plain-object receiver, compiled for that receiver's own class id, never one
/// carrying an unconditional indy trap (the same bodies the arm publishes, plus
/// the table-barred ones the per-thread entry cache already calls this way).
///
/// # What the sentinel service re-reads (gch h1/jp)
///
/// The re-encoding `raw` is a Rust buffer in no map and no pin list. Every
/// other arm hands the service the CALLER's argument block, which the
/// caller's frame roots for the whole call (a single-pass block is named in
/// the site's oop map and remapped in place; an optimizing body's staging
/// region lies inside the frame band the conservative scan reads, so its
/// objects stay put -- see `gch_h1jp_both_tiers_root_the_dispatch_staging_block`).
/// `raw` used to go to the service instead, unpinned for a handler-free
/// callee, so a moving collection inside the callee left the declined-stash
/// and frameless re-runs reading from-space words
/// (`gch-h1fr-compiled-frames-and-dispatch-doors-review-findings-FIXED-20261008.md`
/// item 1). Now, without `rerun_pins`, the service reads `caller_args` (the
/// caller's block, `None` when the door read a forwarded Rust copy instead)
/// whenever it names the same objects as `raw`
/// ([`mic_miss_rerun_reads_the_callers_block`]): the exposure of every other
/// arm, and no pin, so no added retention. Only when they differ -- the
/// compile probe or the SATB flush moved an argument -- is `raw` pinned across
/// the call (unless the body owns its arguments and
/// `CRATONVM_JIT_DOOR_OWNED_ARGS_UNPINNED` is on, [`door_pins_call_args`]).
///
/// # Safety
/// As the surrounding helper: `vm`/`info` are live, `thread` is this thread's
/// exclusive borrow, and `entry` came from the probe for `receiver_class_id`.
#[allow(clippy::too_many_arguments)]
pub unsafe fn call_body_the_mic_probe_just_found(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    receiver_class_id: ClassId,
    entry: usize,
    needs_ctx: bool,
    vm_ptr: i64,
    full_args: &[Value],
    caller_args: Option<&[i64]>,
    rerun_pins: bool,
    epoch_seen: u32,
) -> Option<i64> {
    if entry == 0
        || !direct_virtual_compiled_callee_entry_enabled()
        || vm.jit.jit_cache.redefine_epoch() != epoch_seen
        || crate::runtime::interpreter::jvmti_requires_interpreter(vm)
    {
        return None;
    }
    let mut raw: smallvec::SmallVec<[i64; INLINE_JIT_NATIVE_ARGS]> = smallvec::SmallVec::new();
    for value in full_args {
        raw.push(match *value {
            Value::Int(v) => i64::from(v),
            Value::Long(v) => v,
            // Cast: the entry ABI carries a float's bits in the low half.
            Value::Float(f) => i64::from(f.to_bits()),
            // Cast: the entry ABI carries a double's bits verbatim.
            Value::Double(d) => d.to_bits() as i64,
            // Cast: a live object's address.
            Value::Object(Some(obj)) => obj.as_ptr() as i64,
            Value::Object(None) => 0,
            _ => return None,
        });
    }
    // An entry the code-range registry cannot name is not entered either.
    let pinned = cratonvm_jit::pin_jit_code_range_owner(entry);
    let live = pinned.as_deref().is_some_and(|cm| {
        !cm.superseded.load(std::sync::atomic::Ordering::Acquire)
            && !cm.retired.load(std::sync::atomic::Ordering::Acquire)
    });
    if !live {
        return None;
    }
    // gch h1/jp: see "What the sentinel service re-reads" above.
    let callers_block = if rerun_pins {
        None
    } else {
        caller_args.filter(|caller| mic_miss_rerun_reads_the_callers_block(&raw, caller, full_args))    };
    let pin_raw = rerun_pins
        || (callers_block.is_none()
            && door_pins_call_args(
                true,
                pinned
                    .as_deref()
                    .is_some_and(cratonvm_jit::callee_body_owns_its_arguments),
                door_owned_args_unpinned_enabled(vm),
            ));
    let pins = pin_raw.then(|| CompileArgPins::pin_raw(vm, thread, info, &raw));
    note_cached_entry_arm(&CACHED_ENTRY_REGISTRY_PINS);
    let called = call_compiled_entry_under_owner(pinned.as_deref(), entry, needs_ctx, vm_ptr, &raw);
    let result = called.map(|rc| {
        if rc == i64::MIN {
            // `pins` index `raw`, so a pinned call is serviced over `raw`.
            let rerun_args: &[i64] = match callers_block {
                Some(caller) if pins.is_none() => caller,
                _ => &raw[..],
            };
            handle_rust_armed_callee_sentinel(
                vm,
                thread,
                info,
                receiver_class_id,
                rerun_args,
                pins.as_ref(),
            )
            .unwrap_or(rc)
        } else {
            rc
        }
    });
    if let Some(pins) = pins {
        pins.release(thread);
    }
    result
}

/// Bound on [`MIC_TAIL_NO_BODY`]; a full table is cleared (slower, never wrong).
pub const MIC_TAIL_NO_BODY_CAP: usize = 1024;

thread_local! {
    /// `(site, receiver class id)` -> the `JitCache::generation()` at which
    /// [`mic_tail_call_selected_declaring_body`] found no body under the
    /// declaring class's key.
    pub static MIC_TAIL_NO_BODY: std::cell::RefCell<rustc_hash::FxHashMap<(JitSiteKey, u32), u64>> =
        std::cell::RefCell::new(rustc_hash::FxHashMap::default());
}

/// nio21 wave 19, lane L2 (`r21-by-name-invoke-ladder-is-paid-per-call-CLOSED-20261012.md`
/// item 2): the inline-cache miss of a virtual site whose compile probe found
/// nothing under the RECEIVER's key, while the method §5.4.6 selects for that
/// receiver IS compiled under its DECLARING class's key.
///
/// Host w18 `dispatch-tally` on `Nio20JitRows rlock`: every one of 600 000
/// unlocks ran `AQS.release -> tryRelease` as `jit_mic_tail ->
/// invoke_or_native -> invoke_on_class_shared_inner` (the whole by-name
/// ladder), receiver `ReentrantLock$NonfairSync`, method declared and compiled
/// on `ReentrantLock$Sync`. The probe asks for `(NonfairSync, tryRelease)`.
///
/// This asks the question once more by IDENTITY: the selected method and its
/// declaring class id (`select_for_jit_receiver`, the same §5.4.6 answer the
/// dispatch doors publish under), cross-checked with the lenient template
/// build (`build_bytecode_callee_cached`, which declines a native shadow on
/// any class from the receiver up to the declaring one, a synchronized,
/// native or code-less method), then the JIT cache under the declaring
/// class's own key. A wrapped-entry or indy-trapping body is refused, as the
/// probe arm refuses it. The call goes through
/// [`call_body_the_mic_probe_just_found`] (registry pin, sentinel service,
/// argument pins for a table callee), and the entry is published into the
/// per-thread `(site, receiver class)` entry cache the helper consults first
/// on its next call ([`try_mic_rust_cached_entry`]), whose stale-body test
/// drops it on supersession or invalidation. Nothing is published into the
/// machine-code MIC/PIC: that is the probe arm's decision, unchanged.
///
/// `None`: nothing ran; the caller takes the by-name tail as before.
/// Kill switch `CRATONVM_JIT_MIC_TAIL_DECLARING_BODY=0` (read only once a
/// declaring-class body has been found, i.e. right before it would be entered).
///
/// # Safety
/// As [`call_body_the_mic_probe_just_found`].
#[allow(clippy::too_many_arguments)]
pub unsafe fn mic_tail_call_selected_declaring_body(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    info_ptr: i64,
    receiver_class_id: ClassId,
    vm_ptr: i64,
    full_args: &[Value],
    caller_args: Option<&[i64]>,
    site_rerun_pins: bool,
    epoch_seen: u32,
    ic: Option<(i64, i64, &str)>,
) -> Option<i64> {
    if receiver_class_id.as_u32() == 0 || info.method_name == "<init>" {
        return None;
    }
    // A (site, receiver class) whose declaring-class body was absent from the
    // JIT cache at this generation is absent still: nothing has been published
    // or withdrawn since. Without the memo every call of a site whose body was
    // withdrawn (a callee's deopt takes its raw callers with it) re-ran the
    // selection and a by-name class lookup below, ~25 % of the miss path's
    // samples on `CharBuffer.position` in a loop, until the recompile landed.
    let memo_key = (jit_site_key(vm.vm_identity, info_ptr as usize), receiver_class_id.as_u32());
    let generation = vm.jit.jit_cache.generation();
    if MIC_TAIL_NO_BODY.with(|m| m.borrow().get(&memo_key) == Some(&generation)) {
        return None;
    }
    let (decl_name, decl_id, has_table) = {
        let cm = vm.classes.class_manager.read();
        // An interface- or `Object`-classed receiver is dispatched on the
        // site's class by `virtual_dispatch_target_for_receiver` (S111r12 /
        // COL8-2), not on its own: not this arm's question.
        let receiver_class = cm.get_class(receiver_class_id)?;
        if receiver_class.is_interface() || &*receiver_class.name == "java/lang/Object" {
            return None;
        }
        let (method, decl_id) = select_for_jit_receiver(&cm, receiver_class_id, info)?;
        if method.is_static() || method.is_abstract() || method.is_native() {
            return None;
        }
        let has_table = !method.code()?.exception_table.is_empty();
        let decl_name = cm.class_store().get(decl_id)?.name.clone();
        (decl_name, decl_id, has_table)
    };
    let Some(compiled) =
        vm.jit
            .jit_cache
            .read()
            .get(&decl_name, info.method_name, info.descriptor, decl_id)
    else {
        MIC_TAIL_NO_BODY.with(|m| {
            let mut m = m.borrow_mut();
            if m.len() >= MIC_TAIL_NO_BODY_CAP {
                m.clear();            }
            m.insert(memo_key, generation);        });
        return None;
    };
    if compiled.requires_wrapped_entry || compiled.has_indy_trap {
        return None;
    }
    // The lenient walk must agree with the selection, and no native may
    // shadow the body anywhere from the receiver up to its declaring class.
    let (template, _gate) = crate::runtime::interpreter::build_bytecode_callee_cached(
        vm,
        receiver_class_id,
        info.method_name,
        info.descriptor,
        false,
    )?;
    if template.declaring_class_id != decl_id {
        return None;
    }
    // Read only once a body is in hand: a site this arm serves publishes into
    // the entry cache and does not come back; one it cannot serve never reads.
    // Cast: a code address.
    let entry = compiled.entry_ptr() as usize;
    let needs_ctx = compiled.needs_context();
    let rerun_pins = has_table || site_rerun_pins;
    let rc = call_body_the_mic_probe_just_found(
        vm,
        thread,
        info,
        receiver_class_id,
        entry,
        needs_ctx,
        vm_ptr,
        full_args,
        caller_args,
        rerun_pins,
        epoch_seen,
    )?;
    if vm.jit.jit_cache.redefine_epoch() == epoch_seen {
        publish_mic_rust_cached_entry(
            vm.vm_identity,
            info_ptr,
            receiver_class_id.as_u32(),
            entry,
            needs_ctx,
            rerun_pins,
        );
        // nio21 w20 L2: and, for a callee with no exception table, into the
        // site's own machine-code MIC/PIC exactly as the probe arm publishes a
        // body it found (`update` / `install` retain the owner by entry, the
        // shared table takes it under its selector, a redefinition that
        // landed meanwhile withdraws it). The cascade then CALLs the selected
        // body with no Rust on the path. A table callee stays in the Rust
        // entry cache, whose sentinel service runs its handler.
        if let Some((mic_ptr, pic_ptr, class_name)) = ic {
            if !has_table && !rerun_pins {
                mic_tail_publish_declaring_body(
                    vm,
                    thread,
                    info,
                    receiver_class_id.as_u32(),
                    entry,
                    needs_ctx,
                    epoch_seen,
                    mic_ptr,
                    pic_ptr,
                    class_name,
                );
            }
        }
    }
    drop(compiled);
    Some(rc)
}

/// nio21 w20 L2: the machine-code half of
/// [`mic_tail_call_selected_declaring_body`]'s publication. Kill switch
/// `CRATONVM_JIT_MIC_TAIL_DECLARING_BODY_IC=0` (read once per publication,
/// i.e. once per site and receiver class).
///
/// # Safety
/// `mic_ptr` is the live `JitMICSlot` of the call site whose miss is being
/// served (non-zero), `pic_ptr` its co-allocated `JitPICSlot` or 0.
#[allow(clippy::too_many_arguments)]
pub unsafe fn mic_tail_publish_declaring_body(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    receiver_cid: u32,
    entry: usize,
    needs_ctx: bool,
    epoch_seen: u32,
    mic_ptr: i64,
    pic_ptr: i64,
    class_name: &str,
) {
    if mic_ptr == 0
        || entry == 0
    {
        return;
    }
    let mic = &*(mic_ptr as *const JitMICSlot);
    let jdk_only = crate::vm::dispatch_policy(vm).is_jdk_only();
    let _jdk_only_scope = jdk_only_ic_publication_scope(vm, jdk_only);
    if !mic_publication_skipped(pic_ptr) {
        mic.update(receiver_cid, class_name, entry as u64, needs_ctx, jdk_only);
    }
    if pic_ptr != 0 {
        let pic = &*(pic_ptr as *const JitPICSlot);
        // JIT round 21, lane dispatch (`CRATONVM_JIT_IC_MIC_PLUS_FOUR`).
        if !pic_way_redundant_with_mic(mic, pic, receiver_cid)
            && pic.install(receiver_cid, class_name, entry as u64, needs_ctx, jdk_only)
        {
            disp_census::note(disp_census::MIC_GRACE_LAG_REFUSED);
            request_ic_grace_if_due(vm, Some(thread.thread_id));
        }
        publish_mega_dispatch_entry(
            vm,
            info,
            pic,
            receiver_cid,
            entry as u64,
            needs_ctx,
            epoch_seen,
        );
    }
    withdraw_ic_publication_if_redefined(
        vm,
        epoch_seen,
        mic,
        (pic_ptr != 0).then(|| &*(pic_ptr as *const JitPICSlot)),
    );
}

/// JIT round 20 wave 19, lane yLadder (DISP20-7,
/// `docs/feature-designs/jit-ir-optimizations.md`): the body
/// [`mic_tail_call_selected_declaring_body`] finds, answered in the shape of
/// the receiver-keyed compile probe (`try_jit_compile_callee_for_class`:
/// the pinning `Arc`, the entry, `needs_context`) so the two doors that had
/// only that probe can take it as the probe's own answer.
///
/// The probe asks the JIT cache under the RECEIVER's key, which names an
/// INHERITED method's body only when the slow path compiled a copy for the
/// receiver; the body background tiering published under the DECLARING
/// class's key (`ReentrantLock$Sync.tryRelease` on a `NonfairSync`,
/// `AQS.release` / `Sync.lock` reached on a `NonfairSync`) is invisible to it.
/// w19/w20 taught the MIC's entryless-hit arm to ask for that body; two doors
/// still went to the by-name `invoke_or_native` ladder on every call:
///
/// * the MIC MISS arm (`jit_invoke_virtual_mic_body`'s cache-miss branch),
///   where every call lands for a receiver class other than the one the
///   monomorphic MIC was first populated with -- its declined probe was
///   memoised (`MIC_COMPILE_DECLINED`) and the by-name tail ran per call;
/// * the blind dispatch door (`jit_invoke_dispatch`'s virtual arm, sites
///   with no MIC), whose probe fills `VIRTUAL_DISPATCH_CACHE` only with what
///   the probe answers.
///
/// Same rules as the entryless-hit arm: the method JVMS §5.4.6 selects for
/// the receiver (`select_for_jit_receiver`, by class id), not static,
/// abstract or native; not an interface- or `Object`-classed receiver (the
/// doors dispatch those on the site's class); the body published for the
/// EXACT declaring class id; a wrapped-entry or indy-trapping body refused;
/// and the template build (`build_bytecode_callee_cached`, which declines a
/// native shadow on any class from the receiver up to the declaring one and
/// a synchronized method) must agree on the declaring class. The caller then
/// publishes and calls it exactly as it does a probe answer: the exception
/// table rule (`mic_callee_has_exception_table`), the indy-trap rule, the
/// redefinition withdrawal and the stale-body drop all resolve the callee
/// through the receiver's selection, so they judge this body.
///
/// Switch `CRATONVM_JIT_PROBE_DECLARING_BODY` (default ON, `=0` restores the
/// probe's own answer), read only once a body is in hand -- i.e. on a probe
/// miss that this would serve, never per call.
pub fn probe_selected_declaring_body(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    receiver_class_id: ClassId,
) -> Option<(std::sync::Arc<cratonvm_jit::CompiledMethod>, usize, bool)> {
    if receiver_class_id.as_u32() == 0 || info.method_name == "<init>" {
        return None;
    }
    let (decl_name, decl_id) = {
        let cm = vm.classes.class_manager.read();
        let receiver_class = cm.get_class(receiver_class_id)?;
        if receiver_class.is_interface() || &*receiver_class.name == "java/lang/Object" {
            return None;
        }
        let (method, decl_id) = select_for_jit_receiver(&cm, receiver_class_id, info)?;
        if decl_id == receiver_class_id
            || method.is_static()
            || method.is_abstract()
            || method.is_native()
            || method.is_synchronized()
        {
            return None;
        }
        let decl_name = cm.class_store().get(decl_id)?.name.clone();
        (decl_name, decl_id)
    };
    let compiled =
        vm.jit
            .jit_cache
            .read()
            .get(&decl_name, info.method_name, info.descriptor, decl_id)?;
    if compiled.requires_wrapped_entry || compiled.has_indy_trap {
        return None;
    }
    let (template, _gate) = crate::runtime::interpreter::build_bytecode_callee_cached(
        vm,
        receiver_class_id,
        info.method_name,
        info.descriptor,
        false,
    )?;
    if template.declaring_class_id != decl_id {
        return None;
    }
    // Cast: a code address, as the probe's own arm.
    let entry = compiled.entry_ptr() as usize;
    let needs_ctx = compiled.needs_context();
    Some((compiled, entry, needs_ctx))
}

/// gch h1/jp: may [`call_body_the_mic_probe_just_found`]'s sentinel service
/// read the caller's argument block `caller` instead of the call's re-encoding
/// `raw`? Only when the two are the same length as the decoded `full_args` and
/// every reference slot (`Value::Object`, null included) carries the same word
/// in both: then they name the same objects at the call, and the caller's
/// block is the one its frame keeps current. A primitive slot may differ in
/// its unused upper bits (the re-encoding widens canonically), which the
/// service reads the way every other arm reads the caller's block.
pub fn mic_miss_rerun_reads_the_callers_block(
    raw: &[i64],
    caller: &[i64],
    full_args: &[Value],
) -> bool {
    raw.len() == caller.len()
        && full_args.len() == raw.len()
        && full_args
            .iter()
            .zip(raw.iter().zip(caller.iter()))
            .all(|(value, (&r, &c))| !matches!(value, Value::Object(_)) || r == c)
}


/// [`virtual_dispatch_target_for_receiver`] *plus* the `globally_named`
/// round-trip test, both served from a per-thread memo.
///
/// The uncached pair costs TWO `class_manager` read-lock acquisitions on every
/// call — one to turn the receiver's `ClassId` into a name, one to check that
/// the name resolves back to it — on the path taken by every virtual call
/// whose inline cache holds no compiled entry. That is a third of all MIC
/// calls, and unavoidably *every* call to a native callee, which can never
/// have a compiled entry. At ten threads it was the single largest cost in the
/// VM: reader-reader `parking_lot` contention on one cache line, ~13% of all
/// CPU in `lock_shared_slow` alone, with every workload converging on the same
/// per-op cost regardless of what it actually did
/// (23-charsetcache-pathological-slowdown.md).
///
/// Callers MUST have run [`flush_class_identity_dispatch_memos`] on this
/// thread first — that is what makes a hit as fresh as a locked resolution.
///
/// SAFETY: identical contract to [`virtual_dispatch_target_for_receiver`].
pub unsafe fn virtual_dispatch_target_cached(
    vm: &SharedVm,
    receiver: ObjectRef,
    info: &JitInvokeInfo,
    info_key: JitSiteKey,
) -> CachedDispatchTarget {
    // KC26: array receivers store their COMPONENT class id in the header, so
    // the `(site, class id)` key cannot tell `X[]` from `X`. Resolve them
    // directly — array classes inherit Object's method table (JVMS §4.4.1).
    //
    // An array-typed call SITE gets the same answer without asking the header
    // at all, which is the form that survives a header that lies: a block
    // reclaimed while still referenced and then re-served reads back as a
    // perfectly ordinary object of whatever now occupies it, and the
    // receiver-driven form would then resolve `"[J".clone()` against that
    // occupant's class.
    let dispatch = if info.class_name.starts_with('[') {
        JitDispatchClass::JavaLangObject
    } else {
        jit_dispatch_class(vm, receiver)
    };
    let Some(cid) = dispatch.class_id() else {
        // `--real-jdk`: an ARRAY receiver at an `invokeinterface` implements
        // only `Cloneable` and `Serializable` (interpreter round i1 wave 39,
        // lane L2). Not memoized, as the class-id-0 arm below: a flagged
        // pair throws on every call.
        let selection_error = jit_array_receiver_does_not_implement(vm, receiver, info).is_some();
        return CachedDispatchTarget {
            class_name: std::rc::Rc::from("java/lang/Object"),
            cacheable_receiver: false,
            globally_named: false,
            selection_error,
            selected_cid: 0,
        };
    };
    if cid == ClassId::new(0) {
        // `--real-jdk`: class id 0 is also a plain `java.lang.Object`, which
        // the by-name answer below would run the interface's default method
        // on. Not memoized: every call of such a pair throws.
        if jit_object_receiver_does_not_implement(vm, receiver, info).is_some() {
            return CachedDispatchTarget {
                class_name: std::rc::Rc::from("java/lang/Object"),
                cacheable_receiver: false,
                globally_named: false,
                selection_error: true,
                selected_cid: 0,
            };
        }
        // Not a class at all — a synthetic/unidentified receiver. The answer
        // is a pure function of `info`, so there is nothing worth memoizing.
        return CachedDispatchTarget {
            class_name: if crate::vm::is_object_member(info.method_name, info.descriptor) {
                std::rc::Rc::from("java/lang/Object")
            } else {
                std::rc::Rc::from(info.class_name)
            },
            cacheable_receiver: false,
            globally_named: false,
            selection_error: false,
            selected_cid: 0,
        };
    }

    let key = (info_key, cid.as_u32());
    if let Some(hit) = VIRTUAL_TARGET_CACHE.with(|c| c.borrow().get(&key).cloned()) {
        return hit;
    }

    // Miss. Resolve the name AND its round-trip under ONE read lock, where the
    // uncached path took two.
    //
    // A §5.4.6 retarget (`receiver_selection_retarget`) is receiver-derived
    // too — a pure function of (site, receiver class) — so it is memoized even
    // though it publishes nothing under the receiver's id.
    let mut retargeted = false;
    let resolved = {
        let cm = vm.classes.class_manager.read();
        match cm.get_class(cid) {
            // A lambda proxy is not in the class store. A call that is not its
            // SAM (the lambda arm declined it) is a default method of the
            // functional interface, or an abstract method of one of its
            // super-interfaces that the interface implements with a default
            // (`interface Throwing<F, R> extends Function<F, R> { default R
            // apply(F f) {..} }`). Resolving the by-name tail against the
            // SITE's class (`Function`) invoked the abstract `Function.apply`
            // on the proxy, which answered its first argument; the interpreter
            // selects from the proxy's own interface. Quarkus suite wave 1
            // (`JandexUtilTest`: AssertJ `extracting(..)`).
            None => CachedDispatchTarget {
                class_name: vm
                    .classes
                    .lambda_call_site_for(cid)
                    .filter(|_| !crate::vm::is_object_member(info.method_name, info.descriptor))
                    .map(|site| std::rc::Rc::from(&*site.functional_interface))
                    .unwrap_or_else(|| std::rc::Rc::from(info.class_name)),
                cacheable_receiver: false,
                globally_named: false,
                selection_error: false,
                selected_cid: 0,
            },
            Some(recv_class) => {
                let recv_name = recv_class.name.clone();
                let recv_is_iface = recv_class.is_interface();
                let recv_is_bare_object = recv_name.as_ref() == "java/lang/Object";
                let cp_is_not_object = info.class_name != "java/lang/Object";
                // First: before the by-interface-name answer below, which a
                // plain `Object` receiver would otherwise take, and before any
                // selection (HotSpot checks it first).
                // Before even that (wave 100, lane L2): the site's own
                // reference resolves to nothing, so no receiver selects a
                // body. Flagged and memoized per (site, receiver class); the
                // doors raise it through `jit_selection_failure`.
                let unresolvable = jit_site_unresolvable_owner(vm, &cm, info).is_some();
                let not_implemented = if unresolvable {
                    None
                } else {
                    jit_receiver_does_not_implement(vm, &cm, cid, info)
                };
                if unresolvable {
                    retargeted = true;
                    CachedDispatchTarget {
                        class_name: std::rc::Rc::from(&*recv_name),
                        cacheable_receiver: false,
                        globally_named: false,
                        selection_error: true,
                        selected_cid: 0,
                    }
                } else if let Some(message) = not_implemented {
                    if crate::runtime::env_cache::dbg_jitc() {
                        eprintln!(
                            "[cratonvm-jit] invokeinterface receiver does not implement: {message} \
                             (site {}.{}{})",
                            info.class_name, info.method_name, info.descriptor
                        );
                    }
                    retargeted = true;
                    CachedDispatchTarget {
                        class_name: std::rc::Rc::from(&*recv_name),
                        cacheable_receiver: false,
                        globally_named: false,
                        selection_error: true,
                        selected_cid: 0,
                    }
                } else if (recv_is_iface || recv_is_bare_object)
                    && cp_is_not_object
                    && !crate::vm::is_object_member(info.method_name, info.descriptor)
                    && recv_name.as_ref() != info.class_name
                {
                    // COL8-2 (round 17 wave 2, lane mega2): an instance minted
                    // on a sub-interface of the site's class that declares the
                    // method runs its own, as every cached door does. A pure
                    // function of (site, receiver class), so memoized; it
                    // stays unpublished (not `cacheable_receiver`).
                    if recv_is_iface
                        && interface_receiver_dispatches_on_itself(
                            vm,
                            &cm,
                            cid,
                            jit_site_owner(&cm, info),
                            info.class_name,
                            info.method_name,
                            info.descriptor,
                        )
                    {
                        retargeted = true;
                        CachedDispatchTarget {
                            class_name: std::rc::Rc::from(&*recv_name),
                            cacheable_receiver: false,
                            globally_named: false,
                            selection_error: false,
                            selected_cid: 0,
                        }
                    } else {
                        CachedDispatchTarget {
                            class_name: std::rc::Rc::from(info.class_name),
                            cacheable_receiver: false,
                            globally_named: false,
                            selection_error: false,
                            selected_cid: 0,
                        }
                    }
                } else if let Some((selected, selected_id)) =
                    receiver_selection_retarget(&cm, cid, info)
                {
                    retargeted = true;
                    // MEGA17-3: the selected class's id (MEGA3-17-4: straight
                    // from `receiver_selection_retarget`, not a by-name lookup,
                    // which another loader's copy of the name would defeat), so
                    // its body can be compiled and published under this
                    // receiver's id.
                    let selected_cid = if mega_cache_retargeted_enabled() {
                        selected_id.as_u32()
                    } else {
                        0
                    };
                    CachedDispatchTarget {
                        class_name: std::rc::Rc::from(&*selected),
                        cacheable_receiver: false,
                        globally_named: false,
                        selection_error: false,
                        selected_cid,
                    }
                } else if jit_strict_selection_errs(&cm, cid, info) {
                    retargeted = true;
                    CachedDispatchTarget {
                        class_name: std::rc::Rc::from(&*recv_name),
                        cacheable_receiver: false,
                        globally_named: false,
                        selection_error: true,
                        selected_cid: 0,
                    }
                } else {
                    let globally_named = cm.get_loaded_class_id(&recv_name) == Some(cid);
                    CachedDispatchTarget {
                        class_name: std::rc::Rc::from(&*recv_name),
                        cacheable_receiver: true,
                        globally_named,
                        selection_error: false,
                        selected_cid: 0,
                    }
                }
            }
        }
    };

    // Only the receiver-derived answers are worth keeping. The others are pure
    // functions of `info` that cost one comparison to redo, and memoizing them
    // would fill the map with entries no fast path consults.
    if resolved.cacheable_receiver || retargeted {
        VIRTUAL_TARGET_CACHE.with(|c| {
            let mut map = c.borrow_mut();
            if map.len() >= VIRTUAL_TARGET_CACHE_CAP {
                map.clear();
            }
            map.insert(key, resolved.clone());
        });
    }
    resolved
}

/// Resolve the dispatch class for a virtual/interface bail
/// (`bail_to_interpreter`, kinds 0/2). Null/non-object receivers fall back to
/// the static call-site class so we never dispatch on an empty name.
///
/// SAFETY: `vm` must be a live `SharedVm`; `args[0]` (when present) is the
/// receiver `Value` decoded by `decode_dispatch_values`/`decode_values`.
pub unsafe fn virtual_dispatch_class(
    vm: &SharedVm,
    args: &[Value],
    info: &JitInvokeInfo,
) -> std::sync::Arc<str> {
    let receiver = match args.first() {
        Some(Value::Object(Some(obj))) => *obj,
        _ => return std::sync::Arc::from(info.class_name),
    };
    virtual_dispatch_target_for_receiver(vm, receiver, info).class_name
}

/// BUG-H: does the statically-bound callee declare a non-empty exception
/// table? Resolved from the class manager by `(class, method, descriptor)`.
///
/// Only meaningful for the statically-bound kinds (invokestatic/invokespecial),
/// where `info.class_name`/`method_name`/`descriptor` name the exact callee —
/// the virtual/interface kinds resolve on the runtime receiver type elsewhere.
///
/// SAFETY: `vm` must be a live `SharedVm`; `info` must point to a valid
/// `JitInvokeInfo` whose name fields are live `&str`s.
/// Resolve the callee named by `info` to a `CachedBytecodeMethod`, so its own
/// exception table can be consulted and its handler run without re-executing
/// the method. Mirrors `try_resume_trapped_callee`'s resolution recipe.
///
/// Memoized per `(site, receiver class)` in [`CALLEE_HANDLER_TEMPLATE_CACHE`]:
/// a serviced callee exception used to pay the lock, the hierarchy walk and a
/// copy of the whole method body on every throw.
///
/// SAFETY: same contract as `callee_has_exception_table`.
pub unsafe fn resolve_callee_cached(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    receiver_class_id: ClassId,
) -> Option<std::sync::Arc<crate::classloading::resolution::CachedBytecodeMethod>> {
    if let Some(hit) = callee_handler_template_hit(vm, info, receiver_class_id) {
        return Some(hit);
    }
    let (cached, gate) = resolve_callee_uncached(vm, info, receiver_class_id)?;
    let key = callee_handler_template_key(vm, info, receiver_class_id);
    CALLEE_HANDLER_TEMPLATE_CACHE.with(|c| {
        let mut map = c.borrow_mut();
        if map.len() >= CALLEE_HANDLER_TEMPLATE_CACHE_CAP {
            map.clear();
        }
        map.insert(key, (std::sync::Arc::clone(&cached), gate));
    });
    Some(cached)
}

/// Clear-on-full bound for [`CALLEE_HANDLER_TEMPLATE_CACHE`]; an entry exists
/// only for a (site, receiver class) whose callee actually caught something.
pub const CALLEE_HANDLER_TEMPLATE_CACHE_CAP: usize = 1024;

/// The memo key: the receiver class selects the override only for kinds 0/2.
pub fn callee_handler_template_key(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    receiver_class_id: ClassId,
) -> (JitSiteKey, u32) {
    let receiver = if matches!(info.invoke_kind, 0 | 2) {
        receiver_class_id.as_u32()
    } else {
        0
    };
    (
        jit_site_key(vm.vm_identity, info as *const JitInvokeInfo as usize),
        receiver,
    )
}

/// A live [`CALLEE_HANDLER_TEMPLATE_CACHE`] entry for this site, re-validated.
///
/// Runs both memo flushes first: the generated inline cascade reaches the
/// callee-handler service (`jit_service_callee_deopt`) without passing through
/// a dispatch helper's own flushes, and a recycled `JitInvokeInfo` address or a
/// second loader defining the callee's class name must not be served the old
/// answer. Both are two thread-local compares when nothing changed. A stale
/// `RedefineGate` evicts the entry: the template holds the old body.
pub fn callee_handler_template_hit(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    receiver_class_id: ClassId,
) -> Option<std::sync::Arc<crate::classloading::resolution::CachedBytecodeMethod>> {
    flush_raw_entry_dispatch_caches();
    flush_class_identity_dispatch_memos(vm);
    let key = callee_handler_template_key(vm, info, receiver_class_id);
    let (cached, gate) = CALLEE_HANDLER_TEMPLATE_CACHE.with(|c| c.borrow().get(&key).cloned())?;
    if gate.is_stale() {
        CALLEE_HANDLER_TEMPLATE_CACHE.with(|c| {
            c.borrow_mut().remove(&key);
        });
        return None;
    }
    Some(cached)
}

/// The resolution [`resolve_callee_cached`] memoizes. The redefine gate is
/// snapshotted under the same class-manager read lock the method is read
/// under, so a redefinition cannot slip between the two.
///
/// SAFETY: same contract as `callee_has_exception_table`.
/// The method a JIT call site runs, with its declaring class: the §5.4.6
/// selection for a receiver-class dispatch (what the dispatch doors run,
/// `receiver_selection_retarget`), else the named method looked up from the
/// site's owner (a substituted owner by its recorded id). Shared by the
/// handler template (`resolve_callee_uncached`) and the re-run verdict
/// (`callee_rerun_code`), so both judge the same method.
pub fn jit_site_target_method<'a>(
    cm: &'a crate::classloading::ClassManager,
    info: &JitInvokeInfo,
    receiver_class_id: ClassId,
    substituted: Option<ClassId>,
) -> Option<(&'a cratonvm_reader::method::ClassFileMethod, ClassId)> {
    let receiver_dispatch = matches!(info.invoke_kind, 0 | 2) && receiver_class_id.as_u32() != 0;
    let class_id = match info.invoke_kind {
        0 | 2 if receiver_class_id.as_u32() != 0 => Some(receiver_class_id),
        _ if substituted.is_some() => substituted,
        _ if info.declaring_class_id == 0 => cm.find_bootstrap_class_by_name(info.class_name),
        _ => {
            cm.find_cp_class_for_class(info.class_name, ClassId::new(info.declaring_class_id))
        }
    }?;
    if receiver_dispatch {
        select_for_jit_receiver(cm, class_id, info)
    } else {
        crate::classloading::find_method_recursive(
            class_id,
            info.method_name,
            info.descriptor,
            cm.class_store(),
        )
    }
}

pub unsafe fn resolve_callee_uncached(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    receiver_class_id: ClassId,
) -> Option<(
    std::sync::Arc<crate::classloading::resolution::CachedBytecodeMethod>,
    crate::classloading::resolution::RedefineGate,
)> {
    // A substituted owner by its recorded id, asked before this guard.
    let substituted = jit_substituted_owner(vm, info);
    let cm = vm.classes.class_manager.read();
    let store = cm.class_store();
    let (method, declaring_id) = jit_site_target_method(&cm, info, receiver_class_id, substituted)?;
    let code_attr = method.code()?;
    // JIT round 17 wave 8 (lane syncdoor5): a synchronized callee is served
    // when its compiled body is a self-locking one with a table, i.e. under
    // `CRATONVM_JIT_SELF_LOCKING_SYNC_HANDLERS=1`
    // (`r17-self-locking-handlers-refuse-recursive-exceptional-resume-FIXED-20261005.md`).
    if code_attr.exception_table.is_empty()
        || (method.is_synchronized()
            && !crate::runtime::interpreter::synchronized_callee_handlers_served())
    {
        return None;
    }
    let declaring_class_name = store.get(declaring_id).map(|c| &*c.name)?;
    let gate = crate::classloading::resolution::RedefineGate::snapshot(
        cm.class_redefine_generation_handle(declaring_id),
    );
    let num_params = crate::runtime::interpreter::count_method_params(info.descriptor);
    // The pool generation the code indexes, read under the guard the code is
    // copied under (interpreter round i1 wave 43, lane L2;
    // `CachedBytecodeMethod::pool_generation`).
    let pool_generation = cratonvm_classloading::class_redefinition_count();
    let cached = std::sync::Arc::new(
        crate::classloading::resolution::CachedBytecodeMethod::from_parts(
            cratonvm_jit_api::CachedMethodParts {
                declaring_class_id: declaring_id,
                class_name: std::sync::Arc::from(declaring_class_name),
                method_name: std::sync::Arc::from(info.method_name),
                method_descriptor: std::sync::Arc::from(info.descriptor),
                source_file: store
                    .get(declaring_id)
                    .and_then(|c| c.source_file.as_deref())
                    .map(std::sync::Arc::from),
                // Interned per method identity, as every interpreter frame's
                // code is: `local_liveness` and the quickening table key on
                // the `Arc`'s address.
                code: crate::runtime::frame::padded_bytecode_for_method(
                    declaring_id,
                    info.method_name,
                    info.descriptor,
                    &code_attr.code,
                    &code_attr.exception_table,
                ),
                exception_table: std::sync::Arc::from(code_attr.exception_table.as_slice()),
                max_stack: code_attr.max_stack,
                max_locals: code_attr.max_locals,
                num_params: num_params as u16,
                is_synchronized: method.is_synchronized(),
                is_static: method.is_static(),
            },
        )
        .with_pool_generation(pool_generation),
    );
    Some((cached, gate))
}

/// WHICH implicit signal a compiled callee left behind alongside the `i64::MIN`
/// sentinel — the one place that answers it, for both doors.
///
/// A bounds check, a null array reference and a zero divisor do NOT hand back a
/// throwable. Each sets a flag and returns the sentinel, and the throwable is
/// built on this side ([`materialize_implicit_signal`]). Two functions route
/// such a sentinel through the CALLEE's own exception table —
/// [`route_implicit_exc_through_callee`] for the Rust dispatch helper's
/// compiled-entry fast paths, and [`handle_compiled_callee_deopt_sentinel`] for
/// the machine-code direct `CALL` and the MIC/PIC cascade.
///
/// **They each carried their own copy of the table, and the copies drifted.**
/// The arithmetic arm existed only in the second, so a `/ by zero` arriving at
/// the first read as "no implicit signal here" and the sentinel went out to the
/// compiled CALLER, whose drain threw the `ArithmeticException` through the
/// CALLER's exception table. 198 000 of 200 000 `catch (ArithmeticException)`
/// skipped on `probes/EscapeStaticProbe`; ~8 % of `probes/ExcTableDirectCallOracle`
/// runs dying at `main:156`. Both pages are in
/// `fixed-suite-bugs/jit/`.
///
/// The arm was added where it was missing. This type is the other half: there is
/// now ONE table, and — the load-bearing part — the question *"is there an
/// implicit signal at all?"* is answered by the SAME function that decides
/// *which one*. What made the drift possible was that the first door asked it
/// with a hand-written `aioobe.is_none() && !npe` companion to the `match`, so
/// adding a flag to the `match` and not to the companion was a silent no-op.
///
/// Precedence — AIOOBE, then NPE, then arithmetic — is the order both callers
/// already drained in. It is a precedence and not a set because only one
/// throwable can be delivered, and the compiled body can only have trapped once.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImplicitSignal {
    Aioobe { index: i64, length: i64 },
    Npe,
    Arithmetic,
    None,
}

/// Classify a drained signal triple. Pure, so the table above is testable
/// without a heap — the bug it encodes is entirely an arithmetic property.
pub fn implicit_signal_of(
    aioobe: Option<(i64, i64)>,
    npe: bool,
    arithmetic: bool,
) -> ImplicitSignal {
    match aioobe {
        Some((index, length)) => ImplicitSignal::Aioobe { index, length },
        None if npe => ImplicitSignal::Npe,
        None if arithmetic => ImplicitSignal::Arithmetic,
        None => ImplicitSignal::None,
    }
}

/// Put a classified signal back exactly as it was drained.
///
/// Both doors fall back on this, and it is not hygiene: the compiled caller's
/// own drain builds the throwable FROM the flag, so a signal consumed here and
/// not restored is an exception that never happens at all — the caller sees a
/// bare deopt sentinel and carries on.
pub fn restash_implicit_signal(signal: ImplicitSignal) {
    match signal {
        ImplicitSignal::Aioobe { index, length } => stash_jit_pending_aioobe(index, length),
        // `set_jit_pending_npe_flag_only`, NOT `stash_jit_pending_npe`. This
        // door drains the NPE with `take_jit_pending_npe()`, which takes the
        // FLAG and leaves the compiled-frame snapshot where the helper put it.
        // Re-stashing through the ordinary setter would take a SECOND snapshot
        // here -- one frame shallower, because the callee whose code raised the
        // NPE has already returned -- and silently overwrite the real one. The
        // trace would still be plausible and would be missing exactly the frame
        // it was taken to keep.
        ImplicitSignal::Npe => set_jit_pending_npe_flag_only(),
        ImplicitSignal::Arithmetic => stash_jit_pending_arithmetic(),
        ImplicitSignal::None => {}
    }
}

/// Build the throwable an implicit signal is a *request* for.
///
/// One body, so the message text cannot diverge by dispatch route: a
/// `getMessage()` that depended on which door serviced the trap would be its
/// own wrong answer, and keeping the two arms in step by hand is what this
/// replaces.
///
/// `create_exception_object` allocates, so this is a safepoint; each caller
/// documents what it has pinned across it.
pub fn materialize_implicit_signal(
    vm: &SharedVm,
    thread: &mut JvmThread,
    signal: ImplicitSignal,
    trap_frames: Option<Vec<crate::jit::conservative_roots::ActiveCompiledFrame>>,
    npe_action: u8,
) -> Option<ObjectRef> {
    materialize_implicit_signal_or_frames(vm, thread, signal, trap_frames, npe_action, false).ok()
}

/// [`materialize_implicit_signal`], handing the snapshot BACK when no
/// throwable was built.
///
/// The snapshot is consumed only by the throwable it is attached to. On the
/// one miss -- the class will not load or the allocation failed -- it was never
/// attached, and a caller that must restore the signal exactly as it found it
/// needs it back. Returning it saves that caller cloning the whole snapshot
/// (a `Vec` of frames, an `Arc` bump per label and a `Vec` per inline chain)
/// on EVERY throw just to cover the miss (round 12 wave 3, lane exc:
/// `local_handler_enter_implicit` paid that clone on each compiled catch).
/// The fast-throw door drops the snapshot, exactly as before.
///
/// `live_snapshot` (nio21 wave 18 L1): every frame `trap_frames` names is
/// still on the machine stack and nothing ran since it was taken -- only the
/// compiled local handler's implicit arm builds there. See
/// `exceptions::create_exception_object_with_trap_frames_live`.
pub fn materialize_implicit_signal_or_frames(
    vm: &SharedVm,
    thread: &mut JvmThread,
    signal: ImplicitSignal,
    trap_frames: Option<Vec<crate::jit::conservative_roots::ActiveCompiledFrame>>,
    npe_action: u8,
    live_snapshot: bool,
) -> Result<ObjectRef, Option<Vec<crate::jit::conservative_roots::ActiveCompiledFrame>>> {
    // r9w9 exc9: `OmitStackTraceInFastThrow` (opt-in, default off). A hot site
    // gets a stackless throwable and skips the message, the constructor, the
    // stack walk and the snapshot splice below; `trap_frames` is simply dropped.
    let fast_kind = match &signal {
        ImplicitSignal::Aioobe { .. } => {
            Some(crate::runtime::exceptions::FastThrowKind::ArrayIndexOutOfBounds)
        }
        ImplicitSignal::Npe => Some(crate::runtime::exceptions::FastThrowKind::NullPointer),
        ImplicitSignal::Arithmetic => Some(crate::runtime::exceptions::FastThrowKind::Arithmetic),
        ImplicitSignal::None => None,
    };
    if let Some(kind) = fast_kind {
        let site = crate::runtime::exceptions::fast_throw_site_of(trap_frames.as_deref());
        if let Some(exc) =
            crate::runtime::exceptions::try_fast_throw(vm, thread, kind, site.as_ref())
        {
            return Ok(exc);
        }
    }
    match signal {
        ImplicitSignal::Aioobe { index, length } => {
            // JIT round 20 wave 18 (lane xThrow, THROW20-5): the same text,
            // without a heap `String` under the lean build.
            let mut buf: crate::runtime::exceptions::AioobeMessageBuf = [0; 96];
            let formatted: String;
            let lean_text = if crate::runtime::exceptions::implicit_throw_lean_build_enabled() {
                crate::runtime::exceptions::format_aioobe_message(&mut buf, index, length)
            } else {
                None
            };
            let msg: &str = match lean_text {
                Some(text) => text,
                None => {
                    formatted = format!("Index {index} out of bounds for length {length}");
                    formatted.as_str()
                }
            };
            // The third arm to need the snapshot, and the last. Same defect as
            // the other two: the bounds-check helper flagged the signal and the
            // compiled body ran its epilogue, so a trace `fillInStackTrace`
            // walked here names nothing at all. The snapshot either stands in
            // for that walk or is spliced on after it (round 12 wave 4, W3-2).
            crate::runtime::exceptions::create_exception_object_with_trap_frames_live(
                vm,
                thread,
                "java/lang/ArrayIndexOutOfBoundsException",
                Some(msg),
                trap_frames,
                live_snapshot,
            )
            .map_err(|(_, frames)| frames)
        }
        ImplicitSignal::Npe => {
            // JEP 358, on the door the interpreter's drain does not serve.
            //
            // This arm passed `None` and so produced `getMessage() == null` for
            // every null dereference CAUGHT by a compiled method's own handler
            // — `probes/L2JitNpeProbe.java`'s `caughtHere` shape, where the
            // interpreter and `java -XX:-OmitStackTraceInFastThrow` both name
            // the field. It is the same
            // defect as the drain's and it is fixed from the same place, so the
            // two cannot answer differently: `jit_npe_message` rebuilds the
            // message from the trapping method's bytecode, using the very
            // snapshot this function is about to attach.
            //
            // Built BEFORE the construction, which consumes the snapshot --
            // or, round 15 wave 7 (lane throw2, TH1-5), deferred to
            // `getMessage()` under `--real-jdk` as HotSpot computes it
            // (`exceptions::create_compiled_implicit_npe`, which builds the
            // eager message exactly as here otherwise).
            //
            // The frames the helper snapshotted at the trap. Without them the
            // NPE materialised HERE -- i.e. every implicit NPE routed into a
            // compiled callee's own handler -- keeps the frameless trace a
            // late `fillInStackTrace` walk builds, which is the whole defect
            // the snapshot exists to close. Handing it over in the constructor
            // arm rather than at each door is what stops a fourth door from
            // silently reopening it.
            crate::runtime::exceptions::create_compiled_implicit_npe_live(
                vm,
                thread,
                trap_frames,
                npe_action,
                live_snapshot,
            )
            .map_err(|(_, frames)| frames)
        }
        ImplicitSignal::Arithmetic => {
            // The NPE arm above has carried the snapshot since it landed; this
            // arm had the identical defect and no test that could see it.
            // `pgo02_guarded_virtual_inline`'s stack-trace check could, and
            // did — as a ~20% flake, because whether a given `callDivider`
            // call ENTERS the artifact is timing-dependent, so the same run
            // alternated between a 2-frame interpreted trace and an empty
            // compiled one.
            crate::runtime::exceptions::create_exception_object_with_trap_frames_live(
                vm,
                thread,
                "java/lang/ArithmeticException",
                Some("/ by zero"),
                trap_frames,
                live_snapshot,
            )
            .map_err(|(_, frames)| frames)
        }
        ImplicitSignal::None => Err(trap_frames),
    }
}

/// Run the compiled callee's own handler for an exception that escaped it,
/// resuming AT the handler instead of re-running the method from entry.
///
/// Returns `Some(rc)` when a handler covering the throw site was found and the
/// resumed frame completed; `None` leaves the caller's existing behaviour
/// (propagate, or the conservative whole-method re-run) untouched. The
/// exception is only consumed on the `Some` path.
///
/// A miss carries the throwable to propagate or restore and whether it
/// REPLACED `exc` — a covering row's catch type failed to resolve and its
/// resolution error is now the exception in flight (JVMS §5.4.3, see
/// `search_compiled_frame_rows`). The implicit-signal doors need that bit: they
/// restore a bare flag on a miss, which would rebuild the ORIGINAL exception
/// and drop the error. It is decided by identity against this function's own
/// pin of `exc` (which `run_jit_callee_handler`'s search never writes), so a
/// relocation alone does not read as a replacement.
///
/// SAFETY: same contract as `route_implicit_exc_through_callee`.
pub(crate) unsafe fn try_run_callee_handler(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    receiver_class_id: ClassId,
    args_slice: &[i64],
    exc: cratonvm_types::ObjectRef,
    throw_pc: usize,
) -> Result<
    i64,
    (
        crate::runtime::interpreter::CalleeHandlerMiss,
        cratonvm_types::ObjectRef,
        bool,
    ),
> {
    // `exc` arrives here having already been DRAINED out of
    // `thread.jit_pending_exception` by the caller, so for the length of this
    // function it is a bare Rust local — the one heap reference to a live
    // throwable that no root provider knows about. `run_jit_callee_handler`
    // runs arbitrary Java before it pushes `exc` onto the resumed frame's
    // operand stack. Pin it across that. (gen r4w4/rooting2: an earlier note
    // here said `resolve_callee_cached` could run a user `loadClass`. It
    // cannot -- it is a class-manager READ-lock lookup, `find_class_by_name_*`
    // plus `find_method_recursive`, and loads nothing -- so it is not a GC
    // point, and decoding `args_slice` after it is sound. The pin is kept for
    // the handler run.) `native_pin_roots` is the right home: it is
    // both scanned (`memory/roots.rs`) and remapped (`memory/gc.rs`), so we
    // re-read the slot afterwards to pick up a relocation instead of handing
    // the interpreter a from-space address.
    let pin_base = thread.native_pin_roots.len();
    thread.native_pin_roots.push(exc);
    // The arguments are decoded BEFORE the resolve, while `args_slice` is
    // current (the caller's contract), and rooted across it the same way, for
    // the same reason: decoding after a collection in the resolve read
    // pre-move words (r11w4-rt-callee-rerun-reads-pre-call-argument-words).
    let mut args = decode_dispatch_values(vm, info, args_slice);
    let arg_pins = CompileArgPins::pin_values(vm, thread, &args);
    let resolved = resolve_callee_cached(vm, info, receiver_class_id);
    // Truncates to the argument pins' own watermark, above `exc`'s.
    arg_pins.release_values(thread, &mut args);
    let Some(cached) = resolved else {
        // Re-read through the pin: the resolve above may have collected.
        let exc = thread.native_pin_roots[pin_base];
        thread.native_pin_roots.truncate(pin_base);
        // Nothing was consulted, so nothing is known — keep the caller's
        // pre-existing conservative fallback rather than claiming "not caught".
        // j27 wave 4 (SDR17-1): named apart from a frame `Declined`, for the
        // refusal message (`callee_rerun_refusal_what`).
        return Err((
            crate::runtime::interpreter::CalleeHandlerMiss::Unresolved,
            exc,
            false,
        ));
    };
    let exc = thread.native_pin_roots[pin_base];
    let res = crate::runtime::interpreter::run_jit_callee_handler(
        vm, thread, &cached, throw_pc, exc, &args,
    );
    // The original, as the collector left it: `run_jit_callee_handler` pins its
    // own copy and only that copy can be replaced.
    let original = thread.native_pin_roots[pin_base];
    thread.native_pin_roots.truncate(pin_base);
    let res = res.map_err(|(why, now)| (why, now, now != original))?;
    // The callee's handler ran here, in the interpreter. When the body the
    // cache publishes for it is an optimizing one, that is a catch the body
    // could not run itself (round 12 wave 3, lane exc).
    note_optimizing_callee_catch(vm, &cached);
    Ok(match res {
        Ok(Some(Value::Int(v))) => v as i64,
        Ok(Some(Value::Long(v))) => v,
        Ok(Some(Value::Float(f))) => f.to_bits() as i64,
        Ok(Some(Value::Double(d))) => d.to_bits() as i64,
        Ok(Some(Value::Object(Some(obj)))) => obj.as_ptr() as i64,
        Ok(Some(Value::Object(None))) | Ok(None) => 0,
        Ok(_) => 0,
        Err(e) => handle_jit_dispatch_error(vm, thread, e, info),
    })
}

// SAFETY: `vm` must be live for the call. The body only takes a read lock on
// the class manager and looks the callee up by name; it dereferences no raw
// pointer of its own. The `unsafe` marker is the shared JIT-callback contract
// (the caller is JIT-generated code holding a valid `SharedVm`), not an
// additional obligation of this function.
pub unsafe fn callee_has_exception_table(vm: &SharedVm, info: &JitInvokeInfo) -> bool {
    // A memoized handler template exists only for a callee with a non-empty
    // table, so a hit answers without the lock or the hierarchy walk.
    if callee_handler_template_hit(vm, info, ClassId::new(0)).is_some() {
        return true;
    }
    // The NEGATIVE answer, which the template memo cannot give: every
    // exception crossing a compiled caller asks this about each callee it
    // leaves, and a callee without a table used to pay the lock, a by-name
    // class lookup and a hierarchy walk per crossing, per throw.
    let stamp = callee_table_verdict_stamp(vm);
    if let Some(has_table) = callee_table_verdict_hit(vm, info, false, 0, stamp) {
        return has_table;
    }
    let answer = callee_has_exception_table_uncached(vm, info);
    if let Some(has_table) = answer {
        remember_callee_table_verdict(vm, info, false, 0, stamp, has_table);
    }
    answer.unwrap_or(false)
}

/// [`callee_has_exception_table`]'s resolution. `None` when the class or the
/// method did not resolve, an answer that is not remembered.
///
/// SAFETY: as [`callee_has_exception_table`].
pub unsafe fn callee_has_exception_table_uncached(vm: &SharedVm, info: &JitInvokeInfo) -> Option<bool> {
    // A substituted owner by its recorded id; asked before the guard below,
    // which `jit_substituted_owner` would otherwise take a second time.
    let substituted = jit_substituted_owner(vm, info);
    let cm = vm.classes.class_manager.read();
    let class_id = if substituted.is_some() {
        substituted
    } else if info.declaring_class_id == 0 {
        cm.find_bootstrap_class_by_name(info.class_name)
    } else {
        cm.find_cp_class_for_class(info.class_name, ClassId::new(info.declaring_class_id))
    };
    let class_id = class_id?;
    let store = cm.class_store();
    let (method, _declaring_id) = crate::classloading::find_method_recursive(
        class_id,
        info.method_name,
        info.descriptor,
        store,
    )?;
    Some(
        method
            .code()
            .map_or(false, |c| !c.exception_table.is_empty()),
    )
}

/// One remembered answer of [`callee_has_exception_table`] or
/// [`mic_callee_has_exception_table`] (round 12 wave 3, lane exc).
///
/// Keyed by the `JitInvokeInfo` ADDRESS for the lookup, but validated against
/// the site's CONTENT: an info freed with its body and re-issued at the same
/// address names another site, and must not be served this one's answer --
/// the defect `site_keyed_memos!` exists for. Every input the resolution reads
/// is in the entry, so a hit is exactly the resolution: the three names, the
/// two class-id hints and the kind (what `jit_substituted_owner` and the
/// by-name lookup read), the receiver class (what the selection reads), and a
/// stamp of what can change a name's meaning or a method's table -- the VM's
/// class definition epoch (a class defined or unloaded) and the process's
/// redefinition count. `selected` keeps the two resolutions apart: the
/// receiver-selecting one ([`mic_callee_has_exception_table`]) and the by-name
/// one may answer differently for one site.
pub struct CalleeTableVerdict {
    pub vm_identity: usize,
    pub selected: bool,
    pub receiver: u32,
    pub declaring: u32,
    pub owner: u32,
    pub kind: u8,
    pub class_name: Box<str>,
    pub method_name: Box<str>,
    pub descriptor: Box<str>,
    pub stamp: (u64, u64),
    pub has_table: bool,
    /// JIT round 16 wave 12 (lane throw3, TP16-14): the JIT cache's retire
    /// count ([`callee_verdict_retire_epoch`]) when this entry last proved
    /// it names the info at its address -- made, or content-validated.
    pub retire_epoch: u64,
}

impl CalleeTableVerdict {
    /// Everything but the three names: integer compares.
    pub fn scalars_match(
        &self,
        vm_identity: usize,
        info: &JitInvokeInfo,
        selected: bool,
        receiver: u32,
        stamp: (u64, u64),
    ) -> bool {
        self.stamp == stamp
            && self.vm_identity == vm_identity
            && self.selected == selected
            && self.receiver == receiver
            && self.declaring == info.declaring_class_id
            && self.owner == info.owner_class_id
            && self.kind == info.invoke_kind
    }

    pub fn names(
        &self,
        vm_identity: usize,
        info: &JitInvokeInfo,
        selected: bool,
        receiver: u32,
        stamp: (u64, u64),
    ) -> bool {
        self.scalars_match(vm_identity, info, selected, receiver, stamp)
            && *self.class_name == *info.class_name
            && *self.method_name == *info.method_name
            && *self.descriptor == *info.descriptor
    }

    /// The verdict for `info`, or `None` (a miss), and whether the names had
    /// to be compared and matched under an `epoch` (the caller then stamps
    /// the entry with it).
    ///
    /// JIT round 16 wave 12 (lane throw3, TP16-14): with an `epoch` (the
    /// switch on) an entry proved at that retire epoch answers on its integer
    /// fields alone -- see [`callee_verdict_retire_epoch`] for why the address
    /// then still names the same, unchanged info. Without one, or at another
    /// epoch, the names are compared exactly as before.
    pub fn answer(
        &self,
        vm_identity: usize,
        info: &JitInvokeInfo,
        selected: bool,
        receiver: u32,
        stamp: (u64, u64),
        epoch: Option<u64>,
    ) -> (Option<bool>, bool) {
        if epoch == Some(self.retire_epoch) {
            let hit = self.scalars_match(vm_identity, info, selected, receiver, stamp);
            return (hit.then_some(self.has_table), false);
        }
        let hit = self.names(vm_identity, info, selected, receiver, stamp);
        (hit.then_some(self.has_table), hit && epoch.is_some())
    }
}

/// The JIT cache's retire count: the high half of
/// `cratonvm_jit::jit_cache_generation()` ([`JIT_RETIRE_SHIFT`]; the whole word
/// if the shift were 0, which only makes every publication count as a retire).
///
/// JIT round 16 wave 12 (lane throw3, TP16-14). A `JitInvokeInfo` freed with
/// its `CompiledMethod` is freed only after `Drop for CompiledMethod` bumped
/// this count (see [`flush_raw_entry_dispatch_caches`]), so an address that
/// named site A while the count read `e` still names A -- the same, unchanged
/// allocation -- while it still reads `e`. A verdict made (or re-validated)
/// at `e` therefore needs no name compare at `e`: identity before content.
pub fn callee_verdict_retire_epoch() -> u64 {
    cratonvm_jit::jit_cache_generation()
        .checked_shr(JIT_RETIRE_SHIFT)
        .unwrap_or(0)
}

/// Clear-on-full bound for [`CALLEE_TABLE_VERDICTS`].
pub const CALLEE_TABLE_VERDICT_CAP: usize = 1024;

thread_local! {
    /// `(vm_identity, info address, selected, receiver class or 0)` -> the
    /// verdict. Per thread, like every other dispatch memo: no lock on the
    /// throw path.
    pub static CALLEE_TABLE_VERDICTS: std::cell::RefCell<
        // addr-keyed: not a heap address -- VM identity + `JitInvokeInfo` (retire epoch).
        rustc_hash::FxHashMap<(usize, usize, bool, u32), CalleeTableVerdict>,
    > = std::cell::RefCell::new(rustc_hash::FxHashMap::default());
}

/// The stamp a [`CalleeTableVerdict`] is valid under. Read BEFORE resolving,
/// so a definition that lands during the resolution leaves an entry that is
/// already stale rather than one that looks current.
pub fn callee_table_verdict_stamp(vm: &SharedVm) -> (u64, u64) {
    (
        cratonvm_classloading::store_epochs(vm.jit.class_layout_domain).class_definition_epoch(),
        cratonvm_classloading::class_redefinition_count(),
    )
}

/// `CRATONVM_JIT_CALLEE_TABLE_MEMO` -- default ON; `0` resolves every time, as
/// before. Cached: read on the throw path.
pub fn callee_table_memo_enabled() -> bool {
    pub static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, || {
        true
    })
}

pub fn callee_table_verdict_hit(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    selected: bool,
    receiver: u32,
    stamp: (u64, u64),
) -> Option<bool> {
    if !callee_table_memo_enabled() {
        return None;
    }
    let key = (
        vm.vm_identity,
        info as *const JitInvokeInfo as usize,
        selected,
        receiver,
    );
    // TP16-14 (`CRATONVM_JIT_CALLEE_VERDICT_IDENTITY=1`, default OFF): an
    // entry proved at the current retire epoch answers on its integer fields.
    let epoch = vm
        .threads
        .throwable_layout_cache
        .callee_verdict_identity_enabled()
        .then(callee_verdict_retire_epoch);
    CALLEE_TABLE_VERDICTS.with(|memo| {
        let mut memo = memo.try_borrow_mut().ok()?;
        let entry = memo.get_mut(&key)?;
        let (answer, refresh) =
            entry.answer(vm.vm_identity, info, selected, receiver, stamp, epoch);
        if let (true, Some(epoch)) = (refresh, epoch) {
            entry.retire_epoch = epoch;
        }
        answer
    })
}

pub fn remember_callee_table_verdict(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    selected: bool,
    receiver: u32,
    stamp: (u64, u64),
    has_table: bool,
) {
    if !callee_table_memo_enabled() {
        return;
    }
    let key = (
        vm.vm_identity,
        info as *const JitInvokeInfo as usize,
        selected,
        receiver,
    );
    CALLEE_TABLE_VERDICTS.with(|memo| {
        let Ok(mut memo) = memo.try_borrow_mut() else {
            return;
        };
        if memo.len() >= CALLEE_TABLE_VERDICT_CAP && !memo.contains_key(&key) {
            memo.clear();
        }
        // Read while `info` is live (the caller is crossing its site), so the
        // epoch is one at which the address names this info (TP16-14).
        let retire_epoch = callee_verdict_retire_epoch();
        memo.insert(
            key,
            CalleeTableVerdict {
                retire_epoch,
                vm_identity: vm.vm_identity,
                selected,
                receiver,
                declaring: info.declaring_class_id,
                owner: info.owner_class_id,
                kind: info.invoke_kind,
                class_name: Box::from(info.class_name),
                method_name: Box::from(info.method_name),
                descriptor: Box::from(info.descriptor),
                stamp,
                has_table,
            },
        );
    });
}

/// BUG-H (virtual sibling of [`callee_has_exception_table`]): does the
/// receiver-resolved virtual/interface callee declare a non-empty exception
/// table? The MIC hit path knows the callee only by `(info.method_name,
/// descriptor)` + the runtime receiver class, so resolution starts from the
/// receiver's class id (the real override site).
///
/// SAFETY: as [`callee_has_exception_table`]; `receiver_class_id` must be a
/// live class id obtained from the receiver object.
pub unsafe fn mic_callee_has_exception_table(
    vm: &SharedVm,
    receiver_class_id: ClassId,
    info: &JitInvokeInfo,
) -> bool {
    // As `callee_has_exception_table`. Only for a real receiver class: with
    // none, the memo's entry was resolved by name, which this probe never does.
    if receiver_class_id.as_u32() != 0
        && matches!(info.invoke_kind, 0 | 2)
        && callee_handler_template_hit(vm, info, receiver_class_id).is_some()
    {
        return true;
    }
    // The negative answer, as in `callee_has_exception_table`; keyed on the
    // receiver class too, which is what the selection below reads.
    let stamp = callee_table_verdict_stamp(vm);
    let receiver = receiver_class_id.as_u32();
    if let Some(has_table) = callee_table_verdict_hit(vm, info, true, receiver, stamp) {
        return has_table;
    }
    // The method the receiver's class runs by §5.4.6, as the dispatch doors
    // select it (`receiver_selection_retarget`): the question is about the
    // body a publication would enter.
    let answer = {
        let cm = vm.classes.class_manager.read();
        select_for_jit_receiver(&cm, receiver_class_id, info).map(|(method, _decl)| {
            method
                .code()
                .map_or(false, |c| !c.exception_table.is_empty())
        })
    };
    if let Some(has_table) = answer {
        remember_callee_table_verdict(vm, info, true, receiver, stamp, has_table);
    }
    answer.unwrap_or(false)
}

/// Before the dispatch helper publishes a compiled callee into a site's MIC or
/// PIC: stamp the site's `callee_rerun_pins` flag when that callee declares an
/// exception table, so the helper's own MIC-hit and megamorphic-PIC arms root
/// the call's arguments across a sentinel that can re-run it
/// (r11w4-rt-callee-rerun-reads-pre-call-argument-words).
///
/// The table probe (a class-manager read lock) runs only on a publication, only
/// while table callees are publishable at all, and only until the sticky flag
/// is set, so the hit paths pay one relaxed load. `publishes` is false when
/// there is no entry to publish.
///
/// SAFETY: as [`mic_callee_has_exception_table`].
pub unsafe fn mark_site_rerun_pins_before_publish(
    vm: &SharedVm,
    mic: &JitMICSlot,
    receiver_cid: u32,
    info: &JitInvokeInfo,
    publishes: bool,
) {
    if publishes
        && mic_publish_exception_table_callees()
        && !mic.callee_rerun_pins()
        && mic_callee_has_exception_table(vm, ClassId::new(receiver_cid), info)
    {
        mic.mark_callee_rerun_pins();
    }
}

/// BUG-H fix: route an *implicit* runtime exception thrown by a directly
/// invoked compiled callee through the CALLEE's own exception table.
///
/// When a JIT-compiled callee `B` is invoked directly — either via a
/// machine-code `CALL` to its entry or through one of this helper's
/// `try_call_compiled_entry` fast paths — and it throws an implicit runtime
/// exception (`ArrayIndexOutOfBoundsException` / `NullPointerException`), `B`'s
/// own in-method `catch` is never consulted: `B` returns the `i64::MIN` deopt
/// sentinel with the thread-local pending-exception flag set, and that flag is
/// only drained at the *outermost* interpreter↔JIT boundary — which routes it
/// through the wrong method's exception table (the `TestHexUtils` /
/// `HexUtils.getDec` escape: `T[i-'0']` inside `catch (AIOOBE)` returning -1).
///
/// If `rc` is the deopt sentinel, an implicit AIOOBE/NPE is pending, and the
/// callee declares a non-empty exception table, re-execute the callee in the
/// interpreter with the same args. The interpreter re-throws the same
/// exception at the same bytecode and routes it through the callee's table —
/// running its handler (which may swallow the exception and return a normal
/// value) or re-propagating it to the caller via `handle_jit_dispatch_error`.
/// Re-execution from the start mirrors the existing whole-method deopt
/// semantics; it is confined to the (cold) exceptional path of a method that
/// actually declares a handler region, so straight-line callees are untouched.
///
/// `args_slice` is the call's argument block as the caller read it, and
/// `pins`, when present, roots its references ([`call_compiled_then_route`]).
/// Every re-run below reads the words back through the pins ([`args_now`]) at
/// the point it uses them, because the callee, the throwable this function
/// allocates and a refused deopt-frame rebuild can each collect first.
/// `receiver_class_id` is the receiver's class read BEFORE the call — a class
/// id cannot go stale, a receiver word can — and is used only for the
/// virtual/interface kinds.
///
/// SAFETY: same contract as the surrounding `jit_invoke_dispatch` fast paths —
/// `vm`/`info`/`args_slice` are the live values passed by the JIT caller, and
/// no `jit_thread_mut` borrow is live at the call site.
///
/// HANDLER11-3 (JIT round 19 wave 12, lane handler12): a call this route
/// completes with a value over the stash entry it saw on entry left an orphan
/// ([`note_completed_call_left_stash`], `dispatch-callee-route`).
#[inline]
pub unsafe fn route_implicit_exc_through_callee(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    args_slice: &[i64],
    receiver_class_id: ClassId,
    rc: i64,
    pins: Option<&CompileArgPins>,
) -> i64 {
    if rc != i64::MIN {
        return rc;
    }
    let entry_top = cratonvm_jit::deopt::deopt_stash_top_seq();
    let v =
        route_implicit_exc_through_callee_inner(vm, info, args_slice, receiver_class_id, rc, pins);
    if v != i64::MIN {
        note_completed_call_left_stash(entry_top, "dispatch-callee-route");
    }
    v
}

/// [`route_implicit_exc_through_callee`] for a sentinel, without the
/// HANDLER11-3 orphan note.
///
/// SAFETY: as [`route_implicit_exc_through_callee`].
pub unsafe fn route_implicit_exc_through_callee_inner(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    args_slice: &[i64],
    receiver_class_id: ClassId,
    rc: i64,
    pins: Option<&CompileArgPins>,
) -> i64 {
    // The callee has returned. Any `athrow` bci the pending exception carries is
    // ITS bci, and every path out of here either re-runs the callee interpreted
    // (which regenerates the exception) or hands the sentinel to the compiled
    // CALLER, whose drain would treat that bci as its own. See
    // `clear_jit_athrow_bci`.
    //
    // CAPTURE IT FIRST. Being the callee's own bci is exactly what makes it
    // right for the KCFULL-13 branch below, which routes through THAT callee's
    // own exception table — the one lookup in this function for which the pc is
    // not foreign at all. Clearing it before the branch read it left
    // `throw_pc = usize::MAX`, so `find_jit_exception_handler` took its
    // pc-unknown path, which deliberately skips a catch-all whose region does
    // not span the whole method — i.e. every javac `finally`. The resume-at-
    // handler fix (`run_jit_callee_handler`) could then never fire: measured
    // 4213 entries to the branch and 0 handlers found, with the fall-through
    // re-running the callee from entry and leaking one increment per throw
    // (`CallPathProbe` IFACE-class-delegating, `FinallyBalanceProbe`).
    //
    // The clear itself stays, so every path that propagates outward or re-runs
    // the callee behaves exactly as before; only the branch that consumes the
    // bci for the callee's OWN table sees it.
    let callee_throw_bci = peek_jit_athrow_bci();
    clear_jit_athrow_bci();
    // gce e1/f: the pending throwable already left a not-entrant re-dispatch
    // of this callee (see `handle_compiled_callee_deopt_sentinel`). Then the
    // general-exception branch below must neither search the callee's table
    // again nor re-run the callee from entry.
    let escaped_redispatch = take_redispatch_escape_for(info);
    // Taken from the caller, which read it before the call. Re-deriving it
    // from `args_slice[0]` here read a receiver word that a collection inside
    // the callee may have moved -- unrooted when the callee has no exception
    // table -- and a wrong class here picks the handler table and the resume
    // identity of a different method.
    let receiver_class_id = if matches!(info.invoke_kind, 0 | 2) {
        receiver_class_id
    } else {
        ClassId::new(0)
    };
    let callee_has_handler = if matches!(info.invoke_kind, 0 | 2) {
        mic_callee_has_exception_table(vm, receiver_class_id, info)
    } else {
        callee_has_exception_table(vm, info)
    };
    if rbc6_dbg() {
        eprintln!(
            "[rbc6-dbg] route_implicit_exc_through_callee ENTER {}.{}{} has_last_deopt={} pending_exc={} pending_npe_or_aioobe_unread=?",
            info.class_name,
            info.method_name,
            info.descriptor,
            cratonvm_jit::deopt::has_last_deopt(),
            jit_pending_exception_is_set(),
        );
    }
    // jit-invokedynamic-groovy-regression fix — FIRST chance: a frame-stashing
    // deopt (the unconditional invokedynamic reason-8 trap, or a precise guard
    // bail) in the compiled callee THIS helper just invoked. Resume the callee
    // precisely at its trapping bci and hand the real result back to the
    // compiled caller — no side-effect re-run, no sentinel escape. Refusals
    // (identity mismatch / unmappable / no stash) leave the stash + sentinel
    // to propagate exactly as before. The cheap `has_last_deopt` pre-check
    // keeps the common exception path free of the thread-guard acquire.
    if cratonvm_jit::deopt::has_last_deopt() {
        if let Some((thread, _guard)) = jit_thread_mut() {
            if let Some(v) = try_resume_trapped_callee(vm, thread, info, receiver_class_id, None) {
                return v;
            }
        }
    }
    // Did the callee raise an *implicit* runtime exception?
    //
    // THREE flags, not two. `/ by zero` is stashed by the same machinery as
    // the other two (`stash_jit_pending_arithmetic`, from the compiled `idiv`
    // /`irem` guard and from `handle_compiled_callee_deopt_sentinel`'s
    // fall-through), and this function used to consult only AIOOBE and NPE:
    // an ArithmeticException fell into the `aioobe.is_none() && !npe` branch
    // below, found no *general* pending exception either (the flag is not one),
    // and left through the bare `return rc` — the callee's own exception table
    // never consulted, the sentinel handed to the next frame out.
    //
    // Measured on `probes/EscapeStaticProbe.java`: from the iteration where the
    // caller is compiled to the end of the loop, 198 000 of 200 000 `catch
    // (ArithmeticException)` in the CALLER were skipped and the throw surfaced
    // one frame too high. `route_implicit_exc_through_callee ENTER … pending_exc
    // =false has_last_deopt=false` was printed 198 000 times, which is this
    // function being handed a signal it had no arm for. The sibling door
    // (`handle_compiled_callee_deopt_sentinel`) has always had the arm; the two
    // doors simply disagreed.
    let aioobe = take_jit_pending_aioobe();
    let npe = if aioobe.is_none() {
        take_jit_pending_npe()
    } else {
        false
    };
    let arithmetic = if aioobe.is_none() && !npe {
        take_jit_pending_arithmetic()
    } else {
        false
    };
    // ONE classification, used for both questions this function asks of the
    // three flags — which signal, and whether there is one. See
    // [`ImplicitSignal`]: the second question used to be a separate,
    // hand-written test, and that is what let the arm above go missing here
    // while the sibling door had it.
    let implicit = implicit_signal_of(aioobe, npe, arithmetic);
    if rbc6_dbg() {
        eprintln!(
            "[rbc6-dbg] route_implicit_exc_through_callee IMPLICIT {implicit:?} \
             callee_has_handler={callee_has_handler} {}.{}{}",
            info.class_name, info.method_name, info.descriptor,
        );
    }
    // Re-stash the consumed flag and propagate the sentinel unchanged. Used for
    // the pure-deopt case (no flag) and the no-local-handler case (the callee
    // cannot catch it, so existing outward propagation is already correct).
    let restash_and_return = || {
        restash_implicit_signal(implicit);
        rc
    };
    if implicit == ImplicitSignal::None {
        // KCFULL-13 — *general* pending exception (an explicit `athrow`, or a
        // native-raised throwable, originating in this compiled callee's own
        // callee chain). The original bug-H assumption — "methods-with-tables
        // that `athrow` are never compiled, so a general `JIT_PENDING_EXCEPTION`
        // can't reach a direct compiled call" — only holds for a method that
        // throws *itself*. A method that merely *catches* (declares an
        // exception table but contains no `athrow`) passes the `has_athrow`
        // compile gate and IS compiled; yet the JIT still cannot dispatch to
        // its in-method handler, so an exception propagating up from an
        // (interpreted) sub-callee silently skips its `catch` and escapes to
        // the outermost frame. Canonical victim: keycloak's
        // `CryptoIntegration` chain — JIT'd `getSelectedProvider` wraps an
        // interpreted `CryptoIntegration.getProvider()` (kept interpreted by
        // the string-ldc gate) in `try { ... } catch (IllegalStateException)`,
        // and the ISE escaped the catch under JIT.
        //
        // Mirror the implicit path: if the compiled callee declares a handler,
        // re-execute it in the interpreter so its exception table runs. The
        // interpreter re-run regenerates and routes the exception, so the
        // stale stashed copy is dropped first to avoid a double-drain.
        if jit_pending_exception_is_set() && callee_has_handler && !escaped_redispatch {
            if let Some((thread, _guard)) = jit_thread_mut() {
                // FIRST: resume the callee AT its own handler. The re-run below
                // double-executes everything the compiled attempt already did
                // before the throw -- for `try { n++; mayThrow(); } finally
                // { n--; }` that is one leaked increment per throw, because the
                // compiled body ran `n++` and skipped the `finally`, and the
                // re-run then adds its own balanced pass. Witnessed by
                // `CallPathProbe`/`FinallyShapeProbe`
                // (docs/known-issues/repros/jitban-remaining-20260726/).
                let signals = take_all_jit_signals(thread);
                if let Some(exc) = signals.exception {
                    // `signals.athrow_bci` is always -1 here: the entry clear
                    // above ran before this drain. Fall back to the value
                    // captured before it — the callee's own throw site, which
                    // is what this lookup against the callee's own table needs.
                    let stamped_bci = if signals.athrow_bci >= 0 {
                        signals.athrow_bci
                    } else {
                        callee_throw_bci
                    };
                    // i105 L2: an explicit throwable with no stamp has
                    // already left the callee (`JIT_THROW_PC_UNSTAMPED_EXPLICIT`).
                    let throw_pc = if stamped_bci >= 0 {
                        stamped_bci as usize
                    } else {
                        crate::runtime::interpreter::JIT_THROW_PC_UNSTAMPED_EXPLICIT
                    };
                    let throw_pc_unknown =
                        throw_pc >= crate::runtime::interpreter::JIT_THROW_PC_UNSTAMPED_EXPLICIT;
                    let handler_args = args_now(pins, thread, args_slice);
                    // j27 wave 4 (SDR17-1): the only arm that does not return
                    // answers what the replay refusal below names.
                    let refusal_what = match try_run_callee_handler(
                        vm,
                        thread,
                        info,
                        receiver_class_id,
                        &handler_args,
                        exc,
                        throw_pc,
                    ) {
                        Ok(v) => {
                            // The handler ran and the call is complete, so any
                            // exceptional frame the callee's compiled body
                            // published describes a FINISHED attempt. Drop it here
                            // as well as on the fall-through below. Leaving it
                            // stashed keeps a heap reference alive for an unbounded
                            // time, and lets a later drain for the same method
                            // claim it — the match compares method names only.
                            clear_own_exceptional_frame(vm, thread, info);
                            return v;
                        }
                        // Only trustworthy with a KNOWN throw pc. The
                        // pc-unknown search deliberately under-reports — it
                        // skips a catch-all whose region does not span the whole
                        // method, i.e. every javac `finally` — so "no handler"
                        // there means "cannot tell", and propagating on it would
                        // skip cleanup the re-run does perform. bc-java's
                        // `SymmetricConstraintsTest` is the witness: its
                        // `finally` restores a PROCESS-WIDE
                        // `CryptoServicesRegistrar` constraint, and losing it
                        // failed all 14 `HPKETestVectors` cases afterwards with
                        // "service does not provide 192 bits of security".
                        //
                        // `run_jit_callee_handler` makes that distinction
                        // itself (`Unknown` vs `NotCaught`), against the pc its
                        // lookup ACTUALLY used. It used to be made here from
                        // `throw_pc`, the athrow stamp -- which is always
                        // unknown for a callee compiled with precise exception
                        // frames, because its protected throw sites exit
                        // through the reason-9 stub and publish a frame instead
                        // of stamping. The lookup ran against that frame's
                        // exact bci, found nothing, and the answer was
                        // discarded for a re-run from entry. See
                        // `CalleeHandlerMiss`.
                        // Every miss hands back the throwable to propagate or
                        // restore, re-read from a pin (the search can collect)
                        // and possibly a catch type's resolution error
                        // (JVMS §5.4.3), so it shadows the drained `exc`.
                        Err((
                            crate::runtime::interpreter::CalleeHandlerMiss::NotCaught,
                            exc,
                            _,
                        )) => {
                            // The callee's own table does not cover this throw:
                            // the JVM answer is to propagate to the caller, whose
                            // table has not been consulted yet. Falling through to
                            // the whole-method re-run below would execute the
                            // callee's prefix a SECOND time — and for a prefix
                            // that is not idempotent that does not merely
                            // duplicate work, it can swallow the exception
                            // outright. `CipherInputStream.nextChunk` is the
                            // witness: `finaliseCipher()` sets `finalized = true`
                            // and THEN throws on a bad AEAD tag, so the re-run
                            // takes the `if (finalized) return -1` exit and the
                            // caller reads a clean EOF over tampered ciphertext
                            // (bc-java `CipherStreamTest`).
                            if throw_pc_unknown {
                                set_jit_pending_exception(thread, exc);
                            } else {
                                set_jit_pending_exception_with_bci(thread, exc, throw_pc as i64);
                            }
                            // The callee is gone; its exceptional frame can never
                            // be claimed and must not be left for a later drain.
                            clear_own_exceptional_frame(vm, thread, info);
                            return rc;
                        }
                        Err((why, exc, _)) => {
                            // A handler matched but could not be resumed with
                            // correct locals, or the throw pc is unknown so the
                            // "not caught" answer above cannot be trusted.
                            // Restore the signal exactly as it was found and fall
                            // through to the whole-method re-run, which is this
                            // branch's pre-existing answer.
                            if throw_pc_unknown {
                                set_jit_pending_exception(thread, exc);
                            } else {
                                set_jit_pending_exception_with_bci(thread, exc, throw_pc as i64);
                            }
                            callee_rerun_refusal_what(&why)                        }                    };
                    // Round 13 wave 8 (lane replay4): the re-run below calls
                    // the callee's throwing sub-call again. Counted in the
                    // door census (`callee-exception`, uncounted before) and
                    // refused when the replay rule calls it unsound
                    // (`CRATONVM_JIT_CALLEE_EXCEPTION_RERUN_RAISES`).
                    let callee = callee_rerun_code(vm, info, receiver_class_id);
                    let exact =
                        callee_exception_rerun_is_exact(vm, callee.as_ref(), info, throw_pc);
                    let cause = crate::runtime::interpreter::DoorRerunCause::CalleeException;
                    crate::runtime::interpreter::note_door_rerun(cause, exact);
                    if crate::runtime::interpreter::unsound_rerun_refused(cause, exact, || {
                        callee_rerun_class_redefined(vm, callee.as_ref())
                    }) {
                        let _ = take_jit_pending_exception(thread);
                        clear_own_exceptional_frame(vm, thread, info);
                        return refuse_callee_rerun(
                            vm,
                            thread,
                            info,
                            callee.as_ref(),
                            cause,
                            refusal_what,
                        );
                    }
                }
                let _ = take_jit_pending_exception(thread);
                // The callee is about to be re-executed from its entry in the
                // interpreter, which regenerates and routes the exception
                // itself. Any exceptional frame its compiled body published
                // describes the abandoned attempt — drop it with the stashed
                // exception rather than leave it to be mis-claimed.
                clear_own_exceptional_frame(vm, thread, info);
                let bail_args =
                    decode_dispatch_values(vm, info, &args_now(pins, thread, args_slice));
                if rbc6_dbg() {
                    eprintln!(
                        "[rbc6-dbg] route_implicit_exc_through_callee KCFULL-13 bail_to_interpreter {}.{}{}",
                        info.class_name, info.method_name, info.descriptor
                    );
                }
                return bail_to_interpreter(vm, thread, info, &bail_args);
            }
        }
        // JIT round 17 wave 3 (lane syncdoor3,
        // `r17-syncdoor2-dropped-exceptional-frames-keep-a-handed-method-monitor-FIXED-20261005.md`):
        // the throwable already left a not-entrant re-dispatch of this callee
        // (gce e1/f), so no activation of the callee will ever claim a frame
        // naming it: one still on top is an orphan of that re-dispatch, and
        // what it names (a handed method monitor, a block lock) would be
        // released by nobody once a later take discarded it beneath a newer
        // frame. Take it and release, under the exceptional hand-over's
        // switches only; off, the frame stays for the caller's sinks as before.
        if escaped_redispatch
            && jit_pending_exception_is_set()
            && crate::runtime::interpreter::CompiledLocksOfAStash::dropped_frame_holds_released()
        {
            if let Some((thread, _guard)) = jit_thread_mut() {
                let _ = clear_own_exceptional_frame(vm, thread, info);
            }
        }
        // A FRAMELESS trap in the callee: `jit_uncommon_trap` raised the deopt
        // flag and returned the sentinel with nothing stashed, no implicit
        // signal and no throwable. Passing it through bails the compiled
        // CALLER's whole activation, and the door that entered the caller can
        // only re-run it from entry (`run_jit_body`'s `Deopt`) or, for an OSR'd
        // caller, replay every iteration it committed (`try_osr`'s "no frame"
        // safe reject). The callee is the one frame that lost its work, and the
        // one frame this helper can re-run cheaply: its arguments are still
        // here. Re-run it interpreted and hand its result — value or pending
        // throwable — to the caller as an ordinary return, which is what an
        // interpreted caller does with the same trap. The flag is per-call
        // fresh (`call_compiled_entry_under_owner` retires it before the callee
        // is entered), so it cannot be a stale flag beside a genuine
        // `Long.MIN_VALUE`. A stash left here (one `try_resume_trapped_callee`
        // refused) is NOT frameless and keeps propagating as before.
        // Interpreter round i1 wave 5, L7:
        // `i4-L7-proposal-service-bare-callee-traps-at-the-dispatch-helper`.
        if frameless_callee_trap_pending() {
            if let Some((thread, _guard)) = jit_thread_mut() {
                return service_frameless_callee_trap(
                    vm,
                    thread,
                    info,
                    receiver_class_id,
                    args_slice,
                    pins,
                );
            }
        }
        // The stash `try_resume_trapped_callee` declined above, when it is this
        // site's callee's: re-run the callee here rather than hand its frame to
        // a caller sink that can only replay more
        // (r11w12-tier-helpers-declined-callee-rerun-patch).
        if !jit_pending_exception_is_set() && cratonvm_jit::deopt::has_last_deopt() {
            if let Some((thread, _guard)) = jit_thread_mut() {
                if let Some(v) = rerun_declined_callee_from_entry(
                    vm,
                    thread,
                    info,
                    receiver_class_id,
                    args_slice,
                    pins,
                ) {
                    return v;
                }
            }
        }
        return rc;
    }
    if !callee_has_handler {
        return restash_and_return();
    }
    // Round 12 wave 3 (lane exc): the callee's OWN throw pc, stamped by the pad
    // that raised the signal (`jit_throw_aioobe`'s bci, the NPE / `/ by zero`
    // stubs' `set_throw_bci`) or by the exit stub of the frame it propagated
    // out of. This arm used to hand `run_jit_callee_handler` `usize::MAX`, and
    // the pc-unknown search then matched a TYPED row by exception class alone,
    // whatever its range: `int v = a[i]; try { .. } catch
    // (ArrayIndexOutOfBoundsException e) { return -1; }` returned -1 for an
    // out-of-range `i` instead of throwing, and a narrow `finally` covering the
    // trap was skipped (`R12ExcCalleeThrowPc`). The
    // MIC door (`handle_compiled_callee_deopt_sentinel`) has always range-tested
    // with the stamp; the two doors now agree.
    let callee_pc = implicit_callee_throw_pc(callee_throw_bci);
    // A pc no row of the callee covers cannot be caught there. Answer that
    // BEFORE building the throwable: this arm used to build one, find no
    // handler, discard it and let the caller-side drain build it again.
    if let Some(pc) = callee_pc {
        if implicit_throw_is_uncovered(vm, info, receiver_class_id, pc) {
            return restash_and_return();
        }
    }
    // Materialize the implicit signal and resume the callee at its handler.
    // Re-entering at bytecode 0 replays every prefix side effect (and was the
    // source of the old finally/counter leak).
    if let Some((thread, _guard)) = jit_thread_mut() {
        // The exceptional frame the compiled attempt published is KEPT across
        // the allocation below. It used to be dropped here, and that is what
        // made RBC.6's `getfield`/`putfield` admission a miscompile: the
        // compiled body publishes a correct reason-9/10 frame at the trapping
        // bci, this line threw it away, and `run_jit_callee_handler` — finding
        // nothing — refused the handler and let the exception propagate past a
        // `catch` that catches it.
        //
        // The reason it was dropped no longer holds. `create_exception_object`
        // does allocate, and an allocation is a safepoint, but a
        // `ReconstructedFrame` in `LAST_EXCEPTIONAL` **is** a GC root now: the
        // scan half runs in `memory/roots.rs` §10
        // (`for_each_stashed_deopt_object`) and the remap half in
        // `memory/gc.rs` (`remap_stashed_deopt_objects`), paired by a debug
        // assertion that refuses one without the other. Both land on the thread
        // that owns the stash, which is this thread. `docs/jit/
        // deopt-thread-local-roots.md` names THIS window as the shortest
        // instance of the hazard it closed — a reason-9 frame published by
        // `emit_post_invoke_exception_check` whose sink allocates the throwable
        // before draining it — so keeping the frame here is precisely what that
        // wiring was for.
        //
        // Dropping it was never the "conservative" answer its old comment
        // claimed. Refusing to enter a handler is not a safe subset of entering
        // it: JVMS requires the handler to run, so the refusal is a wrong
        // answer that happens to be loud (an escaping exception) instead of
        // quiet (zeroed locals). The three OTHER exceptional-frame drops in
        // this function are different and stay: each of them runs on a path
        // where the compiled attempt is FINISHED or ABANDONED, so its frame can
        // never be legitimately claimed and must not be left for a later drain
        // to mis-match. Since N6 all three go through
        // `clear_own_exceptional_frame`, which drops the frame only if it
        // belongs to the callee this helper invoked -- being entitled to drop
        // a frame on this path is not the same as being entitled to drop
        // somebody else's.
        // The message text cannot diverge from
        // `handle_compiled_callee_deopt_sentinel`'s or from the interpreter's
        // own `idiv`, because there is one body — a `getMessage()` that changed
        // with the dispatch route would be its own wrong answer, and this used
        // to be kept true by hand.
        // This door drained the FLAG only (`take_jit_pending_npe`), so the
        // snapshot is still in the signal record; take it here so it is
        // consumed by exactly the throwable it belongs to.
        //
        // Taken for `Arithmetic` as well as `Npe` since 2026-09-05: those are
        // the two signals whose setters record a snapshot, and gating the take
        // on `Npe` alone left the arithmetic snapshot in the cell — where the
        // next take, belonging to a different throwable, would have found it.
        // So this was never only a missing feature; it was also a stale-frames
        // hazard, and the drain contract in
        // `take_jit_pending_trap_frames` says why that is the worse
        // half ("a trace that is confidently wrong is worse than one that is
        // short").
        let trap_frames = if matches!(
            implicit,
            ImplicitSignal::Npe | ImplicitSignal::Arithmetic | ImplicitSignal::Aioobe { .. }
        ) {
            take_jit_pending_trap_frames()
        } else {
            None
        };
        // The action code is PEEKED, not taken: this door drained the flag
        // only, and the drain that eventually services this NPE reads the same
        // cell.
        let npe_action = peek_jit_pending_npe_action();
        // Cloned for the discard path below. A snapshot is a handful of small
        // structs and this is the cold exception path; the alternative is
        // materialising the throwable twice to find out whether it is needed.
        let unused_frames = trap_frames.clone();
        let exc = materialize_implicit_signal(vm, thread, implicit, trap_frames, npe_action);
        if let Some(exc) = exc {
            // Read AFTER the allocation above, which can collect.
            let handler_args = args_now(pins, thread, args_slice);
            match try_run_callee_handler(
                vm,
                thread,
                info,
                receiver_class_id,
                &handler_args,
                exc,
                callee_pc.unwrap_or(usize::MAX),
            ) {
                Ok(v) => {
                    // The trap is SERVICED: its handler ran and `v` is the call's
                    // result. Retire the two cells this door read without taking —
                    // the out-of-band deopt flag the trapping stub raised beside
                    // the implicit one, and the peeked NPE action code — exactly as
                    // the sibling door's `take_all_jit_signals` does. A deopt flag
                    // left set is not inert: the compiled caller carries on, and
                    // the next `J`/`D` callee that legitimately returns
                    // `Long.MIN_VALUE` reads it through `jit_dispatch_threw` as a
                    // sentinel and bails its WHOLE activation — which an OSR'd
                    // caller's `try_osr` then safe-rejects, re-running every
                    // iteration since entry (interpreter round i1 wave 3, L7;
                    // `L7JitDoorOutcomes.java`).
                    let _ = take_jit_deopt_pending();
                    let _ = take_jit_pending_npe_action();
                    return v;
                }
                Err((_, error, true)) => {
                    // A covering row's catch type failed to resolve, and its
                    // resolution error REPLACED the implicit exception (JVMS
                    // §5.4.3; the interpreter unwinder and every other compiled
                    // route answer the same). Restashing the flag below would
                    // have the caller-side drain rebuild the ORIGINAL exception
                    // and drop the error, so the error escapes as an ordinary
                    // pending exception instead, exactly as the general arm
                    // above propagates a `NotCaught` miss. The implicit signal
                    // is retired with it: its flag and snapshot were taken
                    // above (the snapshot's clone goes unused), and the two
                    // cells the `Ok` arm retires are retired here too.
                    let _ = take_jit_deopt_pending();
                    let _ = take_jit_pending_npe_action();
                    set_jit_pending_exception(thread, error);
                    clear_own_exceptional_frame(vm, thread, info);
                    return rc;
                }
                Err(_) => {}
            }
        }
        // The callee's table did not take it, so the throwable built above is
        // DISCARDED and the caller-side drain will build another from the flag
        // `restash_and_return` is about to put back. Everything the discarded
        // one consumed has to go back with it — above all the compiled-frame
        // snapshot, which `materialize_implicit_signal` moved out. Without this
        // the second throwable is built from a bare flag and names only the
        // frames the interpreter still holds: `len=1 [main]` where HotSpot
        // reads five.
        restash_jit_pending_trap_frames(unused_frames);
    }
    restash_and_return()
}

/// The callee's stamped throw pc for [`route_implicit_exc_through_callee`]'s
/// implicit arm, or `None` for "unknown" (nothing stamped, or the switch off,
/// which restores the historical pc-unknown search).
pub fn implicit_callee_throw_pc(stamped: i64) -> Option<usize> {
    if stamped < 0 || !implicit_callee_throw_pc_enabled() {
        return None;
    }
    usize::try_from(stamped).ok()
}

/// `CRATONVM_JIT_IMPLICIT_CALLEE_THROW_PC` -- default ON; `0` restores the
/// pc-unknown handler search (and the build-then-discard) in
/// [`route_implicit_exc_through_callee`]'s implicit arm. Cached: throw path.
pub fn implicit_callee_throw_pc_enabled() -> bool {
    pub static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, || {
        true
    })
}

/// Does no row of the callee's exception table cover `pc`, so that an
/// implicit exception raised there certainly propagates out of it?
///
/// `false` whenever that cannot be known without building the throwable: the
/// callee does not resolve to a handler template ([`resolve_callee_uncached`]:
/// no exception table, or `ACC_SYNCHRONIZED` while
/// `synchronized_callee_handlers_served()` is off -- a synchronized callee
/// with a table resolves under `CRATONVM_JIT_SELF_LOCKING_SYNC_HANDLERS`;
/// j27 wave 3 lane BACKLOG, SDR17-3), or a reason-9 frame of this callee is
/// stashed -- `run_jit_callee_handler`
/// range-tests THAT frame's bci in preference to the stamp, so the stamp alone
/// does not decide.
///
/// SAFETY: as [`route_implicit_exc_through_callee`].
pub unsafe fn implicit_throw_is_uncovered(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    receiver_class_id: ClassId,
    pc: usize,
) -> bool {
    let own_frame_stashed =
        cratonvm_jit::deopt::peek_exceptional_identity().is_some_and(|(key, _)| {
            exceptional_frame_key_names(&key, info.method_name, info.descriptor)
        });
    if own_frame_stashed {
        return false;
    }
    let Some(callee) = resolve_callee_cached(vm, info, receiver_class_id) else {
        return false;
    };
    !callee
        .exception_table
        .iter()
        .any(|row| pc >= usize::from(row.start_pc) && pc < usize::from(row.end_pc))
}

/// jit-invokedynamic-groovy-regression fix — does the JIT cache hold a
/// compiled artifact for `(class, method, descriptor)` whose body contains an
/// unconditional `invokedynamic` uncommon trap (`has_indy_trap`)?
///
/// Entry-publication gates consult this: such an artifact must NEVER be
/// published where MACHINE CODE calls it directly (a baked JIT→JIT direct
/// call, a MIC/PIC inline-cache entry), because the trap's `i64::MIN`
/// sentinel + stashed frame would bail through the compiled CALLER's
/// epilogue, past the only point (the dispatch helper) that can resume the
/// callee precisely. Keyed exactly like the compile probes at each gate site
/// (the receiver class for MIC/PIC, the CP class for static direct calls) so
/// the lookup hits the same cache row `try_jit_compile_callee` filled.
pub fn compiled_entry_has_indy_trap(
    vm: &SharedVm,
    class_name: &str,
    method_name: &str,
    descriptor: &str,
) -> bool {
    // `&str`-only and resolved globally by name, so not loader-aware. A site
    // that holds the artifact it is about to publish reads `has_indy_trap`
    // off it instead (the MIC arms and the OSR direct bind do, since
    // interpreter round i1 wave 7); this probe remains for the MIC miss arm
    // when no body was compiled, where nothing callable is published.
    let class_id = vm
        .classes
        .class_manager
        .read()
        .get_loaded_class_id(class_name)
        .unwrap_or(cratonvm_types::ClassId::new(0));
    let jit_cache = vm.jit.jit_cache.read();
    jit_cache
        .get(class_name, method_name, descriptor, class_id)
        .map_or(false, |c| c.has_indy_trap)
}

/// jit-invokedynamic-groovy-regression fix — precise, in-place resolution of a
/// compiled callee that deopted with a stashed frame, at the dispatch-helper
/// call site that invoked it.
///
/// When a compiled callee returns the `i64::MIN` sentinel because it hit a
/// frame-stashing deopt (the unconditional `invokedynamic` reason-8 trap, or a
/// precise guard bail), the stash describes the CALLEE's own frame at the
/// trapping bci. THIS helper — sitting directly between the compiled caller
/// and the callee — is the only place the callee can be resumed without
/// losing any caller's continuation: rebuild the callee's interpreter frame
/// from the stash, run it to completion, and hand the REAL result back to the
/// compiled caller as if the callee had returned normally. No side effect is
/// re-run, no sentinel escapes.
///
/// Refusal (returns `None`, stash left in place / restored) when:
///   * no stash, or the superseded-artifact sentinel (`bci == u32::MAX`);
///   * the stash's baked `method_key` does not name the SAME method this
///     helper invoked (`info`'s name+descriptor; the declaring class may be a
///     supertype of the call-site class for virtual dispatch, so the class is
///     taken from the key and verified to declare the method) — e.g. a
///     deeper machine-called method's guard deopt propagating through;
///   * the method is `ACC_SYNCHRONIZED` (conservative — monitor accounting
///     for the compiled prologue/epilogue vs the interpreter continuation is
///     not audited for this path);
///   * the frame is unmappable (`build_deopt_frame_inner` bails).
/// On refusal the sentinel propagates exactly as before — the outer sinks'
/// identity checks then despeculate + fall back to the safe re-run.
///

/// Two method descriptors name the same parameter list, ignoring the return
/// type.
///
/// The covariant-return BRIDGE case, and the reason `try_resume_trapped_callee`
/// cannot demand descriptor equality. `javac` emits, for
/// `class RowDataType implements DataType<SearchRow>`:
///
/// ```text
/// public Object      read(ByteBuffer)  { return read(b); }   // the bridge
/// public SearchRow   read(ByteBuffer)  { ...the real body... }
/// ```
///
/// A call site typed through the interface names the bridge's descriptor,
/// while the method that actually runs — and therefore the one that traps and
/// stashes a frame — is the covariant target. Requiring full descriptor
/// equality refuses precise resume for every such trap, and the stash then
/// reaches a sink that can only raise `InternalError`.
///
/// Matching on the parameter list alone keeps the guard's actual job (reject a
/// stash from an unrelated method) while admitting the one shape the JVM
/// guarantees is the same call: same name, same arity, same parameter types.
/// Nothing downstream trusts `info` — the callee is re-resolved from the
/// stash's own key and cross-checked against its declaring class.
/// Exceptional frames a sink declined to drop because they were not its own.
///
/// **Expected to stay at zero**, and the number that says whether the rule this
/// enforces was ever being broken. Non-zero means a dispatch helper abandoned
/// its compiled attempt while a DIFFERENT method's pending-exception frame was
/// stashed — a nested callee's, which the abandoning caller never published and
/// is not entitled to discard. Before N6 those were dropped silently.
pub static FOREIGN_EXCEPTIONAL_FRAMES_KEPT: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// See [`FOREIGN_EXCEPTIONAL_FRAMES_KEPT`].
pub fn foreign_exceptional_frames_kept() -> u64 {
    FOREIGN_EXCEPTIONAL_FRAMES_KEPT.load(std::sync::atomic::Ordering::Relaxed)
}

/// `CRATONVM_DBG_DEOPT`, read ONCE — `runtime_flag_on`'s value rule, the same
/// as `runtime::interpreter::deopt_resume::dbg_deopt_enabled` (which is not
/// reachable from here).
///
/// Round 11 (lane `rt`): the eight deopt / trap / exceptional-frame sites in
/// this file re-read the variable on every event (`runtime_flag_on` is
/// uncached: a declared-name set probe plus a string rebuild), and a soft
/// `OsrExit` re-runs that path on every call.
#[inline]
pub fn dbg_deopt_on() -> bool {
    pub static ON: cratonvm_types::flags::MemoSlot = cratonvm_types::flags::MemoSlot::new();
    cratonvm_jit::memo_flag::memo_bool(&ON, || false)}

/// Frameless callee traps `route_implicit_exc_through_callee` serviced by
/// re-running only the callee, where the sentinel used to bail the compiled
/// caller's whole activation. The census the proposal asked for: nonzero on a
/// workload means callers (and OSR'd loops) that used to replay.
pub static BARE_CALLEE_TRAPS_SERVICED: std::sync::atomic::AtomicU64 =
    std::sync::atomic::AtomicU64::new(0);

/// See [`BARE_CALLEE_TRAPS_SERVICED`].
pub fn bare_callee_traps_serviced() -> u64 {
    BARE_CALLEE_TRAPS_SERVICED.load(std::sync::atomic::Ordering::Relaxed)
}

/// Is the thread's signal state exactly a compiled callee's FRAMELESS trap —
/// the deopt flag, and nothing else: no pending throwable, no implicit
/// NPE/AIOOBE/`/ by zero` flag, no stashed frame (a stash a resume refused is
/// NOT frameless and keeps propagating)?
///
/// Only meaningful right after a RUST-armed compiled call, where
/// `call_compiled_entry_under_owner` retired the flag before the callee ran,
/// so a set flag is the callee's own and not a stale one beside a genuine
/// `Long.MIN_VALUE`. The inline machine-code cascade has no such retirement,
/// which is why `jit_service_callee_deopt` does not ask this.
#[inline]
pub fn frameless_callee_trap_pending() -> bool {
    !jit_pending_exception_is_set()
        && !cratonvm_jit::deopt::has_last_deopt()
        && JIT_SIGNALS.with(|s| {
            s.deopt.get() && !s.npe.get() && s.aioobe.get().is_none() && !s.arithmetic.get()
        })
}

/// Service a frameless callee trap ([`frameless_callee_trap_pending`]): take
/// the flag and re-run only the callee interpreted, handing its result — value
/// or pending throwable — to the compiled caller as an ordinary return. Passed
/// through instead, the sentinel bails the CALLER's whole activation, whose
/// door can only re-run it from entry or, for an OSR'd caller, replay every
/// iteration it committed. See
/// `i4-L7-proposal-service-bare-callee-traps-at-the-dispatch-helper`.
///
/// # Safety
/// As the dispatch helpers — `vm` live, `info` a live `JitInvokeInfo`,
/// `args_slice` the caller's outgoing argument slots.
///
/// Round 13 wave 8 (lane replay4): `receiver_class_id` (the receiver's class
/// read before the call, `ClassId::new(0)` for none) selects the override the
/// re-run dispatches to, so the replay verdict is asked of the method that
/// actually re-runs; and a re-run the verdict calls unsound is REFUSED
/// ([`refuse_callee_rerun`]) instead of committing the callee's prefix again.
#[cold]
pub unsafe fn service_frameless_callee_trap(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    receiver_class_id: ClassId,
    args_slice: &[i64],
    pins: Option<&CompileArgPins>,
) -> i64 {
    let _ = take_jit_deopt_pending();
    BARE_CALLEE_TRAPS_SERVICED.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    // Round 12 wave 5 (lane replay3): the door census's `callee-frameless`
    // row. Round 13 wave 8 (lane replay4): judged by the trap site the stub
    // recorded, over the method the re-run dispatches to
    // (`frameless_callee_rerun_is_exact`); it used to be the whole-body rule
    // over the method the SITE names, which for a virtual site was the
    // interface's abstract method and read unsound whatever ran.
    let callee = callee_rerun_code(vm, info, receiver_class_id);
    let exact = frameless_callee_rerun_is_exact(vm, callee.as_ref(), info);
    let cause = crate::runtime::interpreter::DoorRerunCause::CalleeFrameless;
    crate::runtime::interpreter::note_door_rerun(cause, exact);
    if dbg_deopt_on() {
        eprintln!(
            "[cratonvm-deopt] frameless callee trap serviced at the dispatch \
             helper: re-running {}.{}{} interpreted, replay={} (site {:?})",
            info.class_name,
            info.method_name,
            info.descriptor,
            if exact { "exact" } else { "UNSOUND" },
            last_frameless_trap_site(),
        );
    }
    if crate::runtime::interpreter::unsound_rerun_refused(cause, exact, || {
        callee_rerun_class_redefined(vm, callee.as_ref())
    }) {
        return refuse_callee_rerun(vm, thread, info, callee.as_ref(), cause, "a frameless trap");
    }
    // Through the pins, like every other re-run here: a collection during the
    // call may have moved the arguments (round 11, `CompileArgPins`).
    let bail_args = decode_dispatch_values(vm, info, &args_now(pins, thread, args_slice));
    bail_to_interpreter(vm, thread, info, &bail_args)
}

/// The method a call-site service's re-run from entry dispatches to, with
/// its padded bytecode: `(declaring class id, declaring class name, code)`
/// (round 13 wave 8, lane replay4). The resolution
/// [`resolve_callee_uncached`] makes for the handler template (receiver
/// selection for kinds 0/2, the substituted owner, the site's loader), without
/// its "has a table, not synchronized" filter. `None` when nothing resolves,
/// or the method has no code.
///
/// SAFETY: as [`resolve_callee_uncached`].
pub unsafe fn callee_rerun_code(
    vm: &SharedVm,
    info: &JitInvokeInfo,
    receiver_class_id: ClassId,
) -> Option<(ClassId, std::sync::Arc<str>, std::sync::Arc<[u8]>)> {
    // Asked before the guard below, which it would otherwise take twice.
    let substituted = jit_substituted_owner(vm, info);
    let cm = vm.classes.class_manager.read();
    let store = cm.class_store();
    let (method, declaring_id) = jit_site_target_method(&cm, info, receiver_class_id, substituted)?;
    let code_attr = method.code()?;
    let class_name: std::sync::Arc<str> = std::sync::Arc::from(&*store.get(declaring_id)?.name);
    let code = crate::runtime::frame::padded_bytecode_for_method(
        declaring_id,
        info.method_name,
        info.descriptor,
        &code_attr.code,
        &code_attr.exception_table,
    );
    Some((declaring_id, class_name, code))
}

/// The replay verdict for a call-site service re-running a callee after its
/// bare sentinel (round 13 wave 8, lane replay4): the trap site the thread
/// recorded (`deopt_resume::frameless_rerun_is_exact` with no entered body;
/// a stub's own stamp answers for its body), else the published body's stamp
/// and the whole-body rule over `callee`'s code. A callee that did not
/// resolve is answered `false`: a census must not call exact a re-run it
/// could not check.
///
/// Round 13 wave 9 (lane replay5): except a trap site that carries its own
/// verdict, which needs no code: a stub stamped `exact` by the single-pass
/// finalizer, or a dropped frame judged exact at its own bci. With the
/// wave-8 refusal a `false` here is an `InternalError`, so answering it for a
/// re-run the trapping body's own stamp proves exact was a spurious one.
pub fn frameless_callee_rerun_is_exact(
    vm: &SharedVm,
    callee: Option<&(ClassId, std::sync::Arc<str>, std::sync::Arc<[u8]>)>,
    info: &JitInvokeInfo,
) -> bool {
    let Some((class_id, class_name, code)) = callee else {
        return unresolved_callee_trap_site_is_exact(last_frameless_trap_site());
    };
    let site = last_frameless_trap_site();
    if matches!(site, FramelessTrapSite::Unknown) {
        let artifact =
            vm.jit
                .jit_cache
                .read()
                .get(class_name, info.method_name, info.descriptor, *class_id);
        return crate::runtime::interpreter::frameless_rerun_is_exact(
            code,
            artifact.as_deref(),
            site,
        );
    }
    // Round 13 wave 13 (lane jitfix): a stub's stamp answers only for the
    // method this service re-runs.
    crate::runtime::interpreter::frameless_rerun_is_exact_for_callee(
        code,
        site,
        frameless_trap_method_hash(class_name, info.method_name, info.descriptor),
    )
}

/// [`frameless_callee_rerun_is_exact`] for a callee that did not resolve
/// (round 13 wave 9, lane replay5): `true` only for a trap site whose own
/// verdict is exact, the two `deopt_resume::frameless_rerun_is_exact` trusts
/// for a service without reading any code. Under
/// `CRATONVM_JIT_FRAMELESS_TRAP_IDENTITY=0` (no site is read) it stays `false`.
pub fn unresolved_callee_trap_site_is_exact(site: FramelessTrapSite) -> bool {
    matches!(
        site,
        FramelessTrapSite::Stub { exact: true, .. }
            | FramelessTrapSite::Dropped { exact: true }
            // gce e1/f: an owned-arguments hand-off judged exact.
            | FramelessTrapSite::Handoff { exact: true }
    ) && true}


/// The replay verdict for the KCFULL-13 re-run of `callee` after an exception
/// escaped it at `throw_pc` (`usize::MAX`: unknown; round 13 wave 8, lane
/// replay4). The attempt ran through the throwing instruction, so the prefix
/// rule applies only to an `athrow` there (which commits nothing itself, and
/// re-executes); a throw out of a sub-call ran that call, which a re-run calls
/// again, so anything else is the whole-body rule. The spliced-body half is
/// the published body's (`false` without one).
pub fn callee_exception_rerun_is_exact(
    vm: &SharedVm,
    callee: Option<&(ClassId, std::sync::Arc<str>, std::sync::Arc<[u8]>)>,
    info: &JitInvokeInfo,
    throw_pc: usize,
) -> bool {
    let Some((class_id, class_name, code)) = callee else {
        return false;
    };
    let body =
        vm.jit
            .jit_cache
            .read()
            .get(class_name, info.method_name, info.descriptor, *class_id);
    // 0xbf: athrow.
    let bci = match u32::try_from(throw_pc) {
        Ok(bci) if code.get(throw_pc) == Some(&0xbf) => bci,
        _ => u32::MAX,
    };
    // Round 13 wave 12 (lane replay6): the body's local handlers may have run
    // a `catch` block placed before the throw on the way here, so the extent
    // follows their edges too.
    crate::runtime::interpreter::replay_from_entry_is_observably_equivalent_after(
        code,
        body.as_deref(),
        bci,
    )
}

/// Was the class of the method a call-site service would re-run redefined?
/// Any redefinition in this VM when the method did not resolve: the
/// conservative side here keeps the re-run (the owner decision for a hot
/// swap).
pub fn callee_rerun_class_redefined(
    vm: &SharedVm,
    callee: Option<&(ClassId, std::sync::Arc<str>, std::sync::Arc<[u8]>)>,
) -> bool {
    match callee {
        Some((class_id, _, _)) => class_was_redefined(vm, *class_id),
        None => crate::classloading::any_class_redefined(),
    }
}

/// A call-site service's answer to a re-run of `info`'s callee the replay
/// rule refused ([`crate::runtime::interpreter::unsound_rerun_refused`];
/// round 13 wave 8, lane replay4): count the refusal and raise
/// `InternalError` at the call, as the method-entry doors raise it. The
/// callee's compiled attempt committed a prefix that no sink can resume, and
/// running the callee again would commit it twice.
#[cold]
#[inline(never)]
pub(crate) fn refuse_callee_rerun(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    callee: Option<&(ClassId, std::sync::Arc<str>, std::sync::Arc<[u8]>)>,
    cause: crate::runtime::interpreter::DoorRerunCause,
    what: &str,
) -> i64 {
    crate::runtime::interpreter::note_door_rerun_refused(cause);
    // Retired, as a door retires the method whose re-run it refused, so the
    // next call does not raise again.
    if let Some((class_id, class_name, _)) = callee {
        crate::runtime::interpreter::retire_callee_after_refused_rerun(
            vm,
            *class_id,
            class_name,
            info.method_name,
            info.descriptor,
        );
    }
    if dbg_deopt_on() {
        eprintln!(
            "[cratonvm-deopt] call-site service REFUSED re-running {}.{}{} from entry \
             ({what}, cause={cause:?})",
            info.class_name, info.method_name, info.descriptor,
        );
    }
    let message = format!(
        "precise deoptimization unavailable for {}.{}{} after {what} (call-site service: the \
         callee's frame could not be resumed); refusing side-effecting replay",
        info.class_name, info.method_name, info.descriptor,
    );
    let failed = crate::runtime::exceptions::throw_runtime_error(
        vm,
        thread,
        crate::error::RuntimeError::InternalError { message },
    );
    handle_jit_dispatch_error(vm, thread, failed, info)
}

/// j27 wave 4 (lane MONROWS, SDR17-1 of
/// `jit-r17-syncdoor5-proposals`): what a call-site service's refused re-run
/// ([`refuse_callee_rerun`]) names after a callee-handler miss. An
/// [`Unresolved`](crate::runtime::interpreter::CalleeHandlerMiss::Unresolved)
/// template consulted no table, so it says so; every other miss keeps the
/// message it always had (a handler that could not be resumed, or a throw pc
/// that could not be trusted). Diagnosis only: both arms refuse alike.
pub(crate) fn callee_rerun_refusal_what(
    why: &crate::runtime::interpreter::CalleeHandlerMiss,
) -> &'static str {
    match why {
        crate::runtime::interpreter::CalleeHandlerMiss::Unresolved => {
            "an exception whose callee handler template did not resolve"
        }
        _ => "an exception its handler could not resume",
    }
}


/// [`handle_compiled_callee_deopt_sentinel`] for a callee a RUST arm of
/// `jit_invoke_virtual_mic` entered (the Rust-cached entry, the megamorphic
/// PIC probe, the MIC hit): the same stash / handler service, then the
/// frameless-trap service `route_implicit_exc_through_callee` gives the
/// dispatch helper's arms. Those three arms call through
/// `call_compiled_entry_under_owner`, so the flag is per-call fresh there too;
/// they were left out of stage 2 only because they share the sentinel handler
/// with the inline cascade, which is not. Interpreter round i1 wave 6, L7.
///
/// # Safety
/// As [`handle_compiled_callee_deopt_sentinel`].
pub unsafe fn handle_rust_armed_callee_sentinel(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
    receiver_class_id: ClassId,
    args_slice: &[i64],
    pins: Option<&CompileArgPins>,
) -> Option<i64> {
    if let Some(v) = handle_compiled_callee_deopt_sentinel(
        vm,
        thread,
        info,
        receiver_class_id,
        args_slice,
        pins,
        false,
    ) {
        return Some(v);
    }
    if frameless_callee_trap_pending() {
        return Some(service_frameless_callee_trap(
            vm,
            thread,
            info,
            receiver_class_id,
            args_slice,
            pins,
        ));
    }
    None
}

/// Drop the stashed pending-exception frame **only if it belongs to the callee
/// this dispatch helper invoked** — N6.
///
/// # Why the unchecked drop was wrong
///
/// `cratonvm_jit::deopt::clear_exceptional_frame`'s own doc states the rule —
/// *"sinks that merely pass an exception along must NOT call this"* — and then
/// drops whatever is there, including a nested callee's frame that the
/// abandoning caller never published. That made the rule documentation rather
/// than enforcement, and the two cases are indistinguishable from inside the
/// function: it is handed no identity at all.
///
/// The three call sites in `route_implicit_exc_through_callee` are entitled to
/// drop a frame on the paths they take — the compiled attempt there is FINISHED
/// or ABANDONED, so its frame can never be legitimately claimed and leaving it
/// stashed lets a later drain for the same method mis-claim it. What they are
/// not entitled to do is drop somebody else's.
///
/// # The identity rule
///
/// Method name and descriptor, not the class — the same rule
/// `try_resume_trapped_callee`'s gate applies, and for the same reason: for a
/// virtual dispatch the frame's baked key names the RESOLVED declaring class,
/// which may be a supertype of the class the call site was written with.
/// Descriptors are compared modulo the return type because a call site's
/// descriptor and the resolved method's can differ there (covariant returns).
///
/// A frame we decline to drop is COUNTED
/// ([`foreign_exceptional_frames_kept`]) rather than left silent: the whole
/// point is to find out whether this was ever happening.
///
/// # What the dropped frame holds (JIT round 17 wave 3, lane syncdoor3)
///
/// A frame the attempt published may name locks its compiled code still holds:
/// a block lock, or, under the exceptional hand-over, a self-locking body's
/// METHOD monitor. Only a sink that takes the frame ever releases them, so a
/// plain drop kept them held for good, and the re-run from entry that follows
/// on the `Declined` arm entered the method monitor AGAIN
/// (`r17-syncdoor2-dropped-exceptional-frames-keep-a-handed-method-monitor-FIXED-20261005.md`).
/// Under the hand-over's switches the frame is TAKEN and what it names is
/// released, as a propagation would ([`drop_exceptional_frame_of`]).
pub fn clear_own_exceptional_frame(
    vm: &SharedVm,
    thread: &mut JvmThread,
    info: &JitInvokeInfo,
) -> bool {
    let Some((key, bci)) = cratonvm_jit::deopt::peek_exceptional_identity() else {
        return false;
    };
    if !exceptional_frame_key_names(&key, info.method_name, info.descriptor) {
        FOREIGN_EXCEPTIONAL_FRAMES_KEPT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if dbg_deopt_on() {
            eprintln!(
                "[cratonvm-deopt] keeping a foreign exceptional frame: stash={key} bci={bci} \
                 vs call site {}.{}{}",
                info.class_name, info.method_name, info.descriptor,
            );
        }
        return false;
    }
    drop_exceptional_frame_of(vm, thread, &key, bci, "abandoned callee attempt")
}

/// Drop the innermost stashed exceptional frame when it is the one named by
/// `(method_key, bci)`, keeping every frame beneath it. Returns whether a
/// frame was dropped.
///
/// JIT round 17 wave 3, lane syncdoor3: under
/// [`crate::runtime::interpreter::CompiledLocksOfAStash::dropped_frame_holds_released`]
/// (`CRATONVM_JIT_SELF_LOCKING_SYNC_HANDLERS=1`, the exceptional hand-over not
/// `0`) the frame is TAKEN (`take_exceptional_frame_above_if`, the take twin
/// of `clear_exceptional_frame_of`: same identity, same top-only pop) and the
/// locks it names are released
/// ([`crate::runtime::interpreter::CompiledLocksOfAStash::release_for_a_dropped_frame`]).
/// Off, exactly `clear_exceptional_frame_of`. An empty key never matches.
pub fn drop_exceptional_frame_of(
    vm: &SharedVm,
    thread: &mut JvmThread,
    method_key: &str,
    bci: u32,
    site: &'static str,
) -> bool {
    use crate::runtime::interpreter::CompiledLocksOfAStash;
    if !CompiledLocksOfAStash::dropped_frame_holds_released() {
        return cratonvm_jit::deopt::clear_exceptional_frame_of(method_key, bci);
    }
    if method_key.is_empty() {
        return false;
    }
    let Some((rframe, _cause, _point)) =
        cratonvm_jit::deopt::take_exceptional_frame_above_if(0, |frame, _point| {
            frame.method_key == method_key && frame.bci == bci
        })
    else {
        return false;
    };
    // Released right after the take, before anything that can allocate: the
    // stash rooted the frame's words until now.
    let _ = CompiledLocksOfAStash::release_for_a_dropped_frame(vm, thread, &rframe, site);
    true
}

/// Does a stashed exceptional frame's baked `method_key` name this call site's
/// callee? The identity rule [`clear_own_exceptional_frame`] applies, split out
/// so it can be tested without a live `JitInvokeInfo`.
///
/// Method name and descriptor only. The class is deliberately NOT compared: for
/// a virtual dispatch the key names the RESOLVED declaring class, which may be
/// a supertype of the class the call site was written with — the same widening
/// `try_resume_trapped_callee`'s gate makes, for the same reason. The
/// descriptor is compared modulo the return type, because a call site's
/// descriptor and the resolved method's can differ there.
///
/// An unparseable or empty key names no method at all, so it is nobody's to
/// drop — the fail-closed direction, and the same one
/// `deopt_frame_matches_method` takes.
pub fn exceptional_frame_key_names(key: &str, method_name: &str, descriptor: &str) -> bool {
    let Some((rest, key_desc)) = key.rsplit_once(':') else {
        return false;
    };
    let Some((_key_class, key_method)) = rest.rsplit_once('.') else {
        return false;
    };
    key_method == method_name && descriptors_match_modulo_return(key_desc, descriptor)}

pub fn descriptors_match_modulo_return(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    // The `)` that CLOSES the parameter list, not the first one: a class name
    // may contain `)`, and `(LA)B;)Z` vs `(LA)C;)Z` cut at the first `)`
    // compared `(LA)` with `(LA)` and matched two different parameter lists.
    let params = cratonvm_jit_api::descriptor_param_list_end;
    match (params(a), params(b)) {
        (Some(ia), Some(ib)) => a.as_bytes()[..=ia] == b.as_bytes()[..=ib],
        _ => false,
    }
}

/// Service a compiled callee's `i64::MIN` sentinel returned by a GENERATED
/// inline call (the MIC/PIC cascade in `jit/src/x64.rs`).
///
/// The inline cascade calls the cached compiled entry directly, so unlike every
/// dispatch-helper arm there is no Rust frame in between to notice the callee
/// trapped. Without this, the sentinel reached the caller's generated code as
/// if the CALLER had deopted, and the callee's reconstructed frame stayed in
/// the thread's one stash slot until an unrelated sink consumed it — which
/// de-speculates the wrong method and then, correctly, refuses to resume a
/// frame that is not its own (`InternalError: precise deoptimization
/// unavailable`). Reproduced on H2 `TestReopen` / `TestPageStoreCoverage`,
/// where `MVMap` reads every row through the megamorphic `DataType.read`.
///
/// Returns the resumed result, or `i64::MIN` unchanged when the sentinel must
/// keep propagating (no stash of ours, an implicit exception with no local
/// handler, …) — i.e. the pre-existing behaviour on every path this does not
/// service.
///
/// # Safety
/// Same contract as the other JIT dispatch helpers: `vm_ptr` a live `SharedVm`,
/// `info_ptr` a live `JitInvokeInfo`, `args_ptr` the caller's outgoing argument
/// slots (`num_args` of them).
#[no_mangle]
pub unsafe extern "C" fn jit_service_callee_deopt(
    vm_ptr: i64,
    info_ptr: i64,
    args_ptr: i64,
    num_args: i64,
) -> i64 {
    let f = || jit_service_callee_deopt_body(vm_ptr, info_ptr, args_ptr, num_args);
    contain(
        "jit_service_callee_deopt",
        OnPanic::Throw { vm_ptr },
        i64::MIN,
        f,
    )
}

/// JIT round 19 wave 4, lane throws4 (THROWS3-3 of
/// `docs/feature-designs/jit-ir-optimizations.md`,
/// `CRATONVM_JIT_SELF_CALL_SENTINEL_LEAN`): would
/// [`handle_compiled_callee_deopt_sentinel`] do nothing but take the signal
/// record and put it back? If so, apply its one net effect (the `athrow_bci`
/// stamp is reset, as `take_all_jit_signals` + `set_jit_pending_exception`
/// leave it) and answer `true`; the service then returns `i64::MIN`.
///
/// Every arm of the full service is ruled out term by term:
///
/// * no deopt frame stashed (`try_resume_trapped_callee`, the declined-stash
///   re-run), and no exceptional frame (the "MIC door miss" drop);
/// * a throwable or an implicit-exception flag is pending, so the frameless
///   re-run (a bare deopt flag) is not this case;
/// * no not-entrant escape recorded (`take_redispatch_escape_for`; it must be
///   `0` here, which is also what the full service leaves);
/// * a statically bound site (the receiver-based table probe needs the
///   arguments) whose callee has no exception table (the memoized
///   `callee_has_exception_table`), so no handler arm runs.
///
/// What the full service would then do is restore every drained field as it
/// was except `athrow_bci`, reset to `-1` (the restash is the general setter),
/// and re-raise the deopt flag it drained. Reads TLS and the memoized table
/// verdict only; takes no thread guard and allocates nothing.
///
/// SAFETY: as [`jit_service_callee_deopt`] (`vm` live, `info` a live
/// `JitInvokeInfo`).
pub unsafe fn service_has_nothing_to_do_but_propagate(vm: &SharedVm, info: &JitInvokeInfo) -> bool {
    if matches!(info.invoke_kind, 0 | 2)
        || cratonvm_jit::deopt::has_last_deopt()
        || cratonvm_jit::deopt::has_exceptional_frame()
    {
        return false;
    }
    let (signal_pending, escape) = JIT_SIGNALS.with(|s| {
        (
            s.npe.get() || s.aioobe.get().is_some() || s.arithmetic.get(),
            s.redispatch_escape.get(),
        )
    });
    if escape != 0 || !(signal_pending || jit_pending_exception_is_set()) {
        return false;
    }
    if callee_has_exception_table(vm, info) {
        return false;
    }
    JIT_SIGNALS.with(|s| {
        s.athrow_bci.set(-1);
        s.athrow_owner.set(0);
    });
    true
}

