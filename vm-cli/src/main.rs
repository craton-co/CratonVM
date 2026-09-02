// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

use anyhow::{bail, Context, Result};
use clap::Parser;

// Round-7 cross-cutting Fix 2: install mimalloc as the process-wide global
// allocator. Gated on the `mimalloc` feature (enabled by default) so musl /
// exotic targets can `--no-default-features` back to the system allocator.
// Without this `#[global_allocator]` declaration the `mimalloc` dependency
// would be linked but never actually used, so the documented speedup on the
// VM's tiny-object workload (Value, ObjectRef, frame locals, Strings) would
// never take effect.
#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;
// Phase accounting (`docs/observability/phase-accounting.md`). Imported as the
// module, not as bare functions: `enter` / `enabled` / `report` are generic
// enough that a call site should say which subsystem it is asking. (The
// `phase: &str` parameter on `trace_jdk_only_violations` is a *value* binding
// and lives in a different namespace, so it does not shadow this path.)
use cratonvm_jfr::phase;
use cratonvm_vm::error::MethodCallFailed;
use cratonvm_vm::types::Value;
use cratonvm_vm::vm::{
    create_java_string, invoke_on_class_shared, invoke_on_class_shared_no_retarget, Vm,
};
use cratonvm_vm::{ClassPath, VmConfig};
use tracing::info;

/// Claim and emit the process-wide shutdown reports once.
///
/// Two of them today: the JIT method summary (`CRATONVM_DBG_JIT_METHOD_STATS`)
/// and the phase-accounting report (`CRATONVM_PHASE_ACCOUNTING`). They share
/// one claim because they share both shutdown routes: `System.exit` never
/// unwinds Rust frames, so it reaches here from the native pre-exit hook, while
/// a normal Java-main return reaches here from the `main-vm` thread. The atomic
/// makes those paths safe to share and prevents future shutdown convergence
/// from printing either report twice.
///
/// The two reports are gated separately *inside* the claim. `jit.method_stats`
/// used to gate the compare-exchange itself; leaving it there would have made
/// the phase report require an unrelated JIT flag. See
/// `docs/observability/phase-accounting.md` §10.3.
/// `--verbose:gc` was passed. Set as soon as the arguments are parsed, because
/// the flag has to be readable from [`maybe_dump_shutdown_reports`], which runs
/// on both exit arms and is handed no `args`. `CRATONVM_GC_STATS` needs no
/// mirror — it is an environment variable and readable from anywhere.
static GC_STATS_REQUESTED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

fn maybe_dump_shutdown_reports() {
    static DUMPED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    if DUMPED
        .compare_exchange(
            false,
            true,
            std::sync::atomic::Ordering::AcqRel,
            std::sync::atomic::Ordering::Acquire,
        )
        .is_err()
    {
        return;
    }

    // The collector's own account of WHAT it did and WHY — the decision
    // histogram, the per-reason moving-young fallback rows, and the two G1
    // JIT-root lines (`g1 root coverage`, `g1 jit publication`).
    //
    // `gc_metrics::collector_decision_report` had no production caller at all
    // until 2026-09-01 — `grep` returned its own unit tests — which quietly
    // voided a claim.
    // `bug-g1-evacuates-live-jit-reference-20260819.md` keeps
    // `G1Collector::empty_jit_publication` as a detector rather than a fix, on
    // the grounds that with the conservative scan always running under G1 an
    // empty publication under a live compiled frame is once again a genuine
    // anomaly, and says of the line that carries it: "the counter is ungated
    // and should now read zero". Ungated it was. Printed it was not.
    //
    // Emitted from HERE rather than beside `print_gc_summary` in the teardown,
    // because that block is only on the normal-return arm and the workloads
    // this number is wanted for end in `System.exit` — `junit.textui.TestRunner`
    // does, which is the page's own repro. That is the same "detector wired to
    // the arm that does not run" shape as W7-90's slot-map sweep, and this
    // function is where W7-90 put its answer.
    if GC_STATS_REQUESTED.load(std::sync::atomic::Ordering::Acquire)
        || std::env::var_os("CRATONVM_GC_STATS").is_some()
    {
        eprintln!("{}", cratonvm_vm::collector_decision_report());
    }

    // `CRATONVM_DBG=ir-isel` — the instruction selector's process totals.
    //
    // Its own switch, not `jit.method_stats`: these are two different
    // measurements and a run that wants one rarely wants the other. At exit
    // rather than per compile because the population is every method the
    // optimizing tier produced a body for — 850 of them across the two Spring
    // Boot classes this was first measured over — and summing that many stderr
    // lines by hand is how a coverage figure gets mis-transcribed.
    if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_IR_ISEL").is_some() {
        let (methods, stats) = cratonvm_jit::x64::isel::shadow_totals();
        if methods != 0 {
            eprintln!(
                "[ir-isel] TOTALS methods={methods} {}",
                stats.summary_line()
            );
        }
        let (mir_methods, tiles, mismatches) = cratonvm_jit::ir_lower::mir_totals::read();
        if mir_methods != 0 {
            // `shadow_tiles` / `arm_bytes` / `enc_bytes` are verify mode's
            // sizing of the increment byte equality cannot cover: what the
            // encoder would have written for the tiles it is not allowed to
            // emit, against what the per-opcode arms did write.
            let (shadow, arm_bytes, enc_bytes) = cratonvm_jit::ir_lower::mir_totals::read_shadow();
            eprintln!(
                "[ir-isel] MIR TOTALS methods={mir_methods} tiles={tiles} \
                 mismatches={mismatches} shadow_tiles={shadow} \
                 arm_bytes={arm_bytes} enc_bytes={enc_bytes}"
            );
        }
        // Increment 2's verdict tally. Printed on its own line and with the
        // three states kept apart on purpose: `verified` methods with a zero
        // `values` total and `nothing_to_cover` methods look identical in any
        // collapsed "ok" count, and only the first is evidence. `rejected` is
        // not a ratio — any non-zero value is a compiler bug.
        let (verified, values, vacuous, indescribable, rejected) =
            cratonvm_jit::ir_lower::mir_totals::read_alloc();
        if verified + vacuous + indescribable + rejected != 0 {
            eprintln!(
                "[ir-isel] MIR ALLOC verified={verified} values={values} \
                 nothing_to_cover={vacuous} indescribable={indescribable} \
                 rejected={rejected}"
            );
        }
    }

    // Final tally for the resolved-field site cache. Self-gated on
    // `CRATONVM_DBG=field-site`; a run that never sets it prints nothing. This
    // is what proves the lever is live before anyone times it — an inert gate
    // reports `hit=0` here rather than hiding inside a timing wash.
    cratonvm_vm::runtime::interpreter::site_cache::site_stats::dump();
    cratonvm_vm::runtime::interpreter::invoke_phases::dump();

    // The G1 live-region memo's tally, self-gated on
    // `CRATONVM_DBG_G1_LIVE_MEMO`. Same argument as the line above, and it is
    // the only usable one for that change: this host has no PMU, so a
    // `perf stat -e instructions` A/B is unavailable, and its load average
    // moves further in an hour than the effect does.
    cratonvm_vm::dump_g1_live_region_memo_stats();

    // The JIT root-scan tally, self-gated the same way. It answers what the
    // method-stats line below cannot: those counters price the COMPILER, and a
    // run where compilation costs 3 ms while the JIT still costs +83% CPU has
    // its cost somewhere the compiler statistics do not reach.
    cratonvm_vm::jit::conservative_roots::scan_prof::dump();

    // The oop-map audit's tally, self-gated the same way
    // (`CRATONVM_DBG_VERIFY_OOP_MAPS`). It answers the one question the
    // moving-young design rests on and that nothing else reports: does the
    // precise map name every live reference in the frames it claims to
    // describe? `never_mapped` is that answer; `below_jit` is the interpreter
    // and native region the map never claimed, split out so it cannot be
    // mistaken for a gap the way the previous oracle's single number was.
    cratonvm_vm::jit::conservative_roots::oop_map_audit::dump();

    // How many native-registry probes one invoke cost, self-gated on
    // `CRATONVM_DBG_NATIVE_LOOKUPS=1`. This is the number
    // `performance/vm-per-call-dispatch-cost-RETIRED-20260817.md` §2 asks for
    // before anyone restructures the dispatch entry points: a profile share can
    // say `slot_for_exact` is 8.5%, but only this says whether a "one lookup
    // per invoke" rewrite would divide it by 1 or by 10.
    cratonvm_native_api::registry::lookup_census::report("exit");

    // Which classes this run DEFINED, hottest first, self-gated on
    // `CRATONVM_DBG=define-census`. Class definition is the only thing that
    // calls `JitCache::invalidate_for_class`, so a profile that shows that
    // symbol long past warm-up is asking this question and nothing else could
    // answer it — `runtime::diagnostics::classes_loaded` reported zero because
    // nothing incremented it.
    cratonvm_classloading::define_census::dump();

    // The lambda tier-up / inline-cache-thunk census, on
    // `CRATONVM_DBG=lambda-jit`.
    //
    // At exit and not merely periodically, because the periodic report fires
    // every 200 000 eligible dispatches (or 100 000 direct calls) and no
    // ordinary application workload comes near either: a census over 24 Tomcat
    // JUnit classes — 129 s of real work, one of them driving 21 HTTP tests
    // against a live connector — printed nothing whatsoever, and "no lambda
    // activity" is not a reading that silence can support. See
    // `runtime::interpreter::report_lambda_census_at_exit`.
    cratonvm_vm::runtime::interpreter::report_lambda_census_at_exit();
    cratonvm_vm::runtime::interpreter::report_stub_door_tally_at_exit();
    cratonvm_vm::runtime::interpreter::report_native_entry_tally_at_exit();

    // The map-view rebuild-elision census, on `CRATONVM_DBG=map-view-cache`.
    // `resync_skipped` is the ENGAGEMENT counter for the keySet-view fast path:
    // a wall-clock number quoted without it cannot say whether the fast path
    // ran at all.
    cratonvm_vm::report_map_view_cache_at_exit();

    // The stale-frame-word oracle's run totals, on `CRATONVM_DBG=remap-residue`.
    // `local_oop` is the count that names a missed root; the per-frame
    // `stale_live` number it replaces is an upper bound that includes dead
    // spill residue, and was twice read as a verdict. `frames` is the
    // engagement counter. See
    // `jit::conservative_roots::report_remap_residue_census_at_exit`.
    cratonvm_vm::jit::conservative_roots::report_remap_residue_census_at_exit();

    // The punned-cell watch census, on `CRATONVM_DBG_WATCH_PUN=<class>:<slot>`.
    // `accessor_reads` / `accessor_stores` are the ENGAGEMENT counters: the
    // experiment this watch exists for turns the JIT off, which also removes
    // every compiled read and write of the slot, so a zero numerator without
    // them cannot be told from "nobody looked".
    cratonvm_vm::report_punned_watch_at_exit();

    // The notification-credit census, on `CRATONVM_DBG=monitor-notify`.
    // `credits_consumed` is the engagement counter for the `Object.wait()`
    // lost-wakeup fix: a run with no stalls says nothing about whether the
    // condition path was exercised, and this is what tells that apart from
    // the switch having been off.
    cratonvm_vm::threading::monitor::report_monitor_notify_census_at_exit();

    if cratonvm_types::flags().jit.method_stats {
        cratonvm_jit::tiered::dump_method_stats_to_stderr();
        // The `getfield` fast-path ENGAGEMENT number, on the same switch. The
        // guarded inline `getfield` is emitted at dozens of sites and can still
        // never take its inline branch; only this counter distinguishes
        // "emitted" from "ran". Pair it with the compile-time
        // `[compact-inline] MISS` census under CRATONVM_DBG_COMPACT_INLINE:
        // MISS names the SITES that cannot inline, this names the ACCESSES that
        // paid the helper's `is_object_address` walk. See
        // fixed-suite-bugs/jit/every-jit-getfield-takes-the-helper-FIXED-20260820.md.
        eprintln!(
            "[cratonvm] getfield helper calls: {} (of which trusted-ref: {}) | CALL sites emitted by arm: {}",
            cratonvm_vm::jit::helpers::jit_getfield_helper_calls()
                .map(|n| n.to_string())
                .unwrap_or_else(|| "<not counted>".to_string()),
            cratonvm_vm::jit::helpers::jit_getfield_trusted_ref_calls(),
            cratonvm_jit::metrics::getfield_arm_emits()
                .iter()
                .map(|(n, c)| format!("{n}={c}"))
                .collect::<Vec<_>>()
                .join(" ")
        );
        // Reference-store barrier gating. BOTH numbers, always: a zero on the
        // left alone cannot distinguish "this collector published no barrier
        // plan" from "this workload compiled no reference stores", and those
        // call for opposite next steps. `gated` counts sites that got the
        // inline SATB/post-barrier gate sequence; `declined` counts sites that
        // asked and kept the full `jit_putfield_object` path.
        //
        // The switch this replaces measured nothing:
        // `CRATONVM_NO_JIT_INLINE_PUTFIELD` was a no-op under the default
        // collector because the path it disabled was already unreachable
        // (`region_bounds_are_live` is false under G1 and ZGC).
        {
            let (gated, declined) = cratonvm_jit::x64::ref_store_site_counts();
            eprintln!(
                "[cratonvm] compiled reference stores: gated={gated} declined={declined}"
            );
            // Optimizing-tier allocation. A zero on the left is the EXPECTED
            // reading under a default configuration -- `c2_alloc_upgrade` is
            // opt-in, so no method containing a `new` reaches that tier -- and
            // printing both is what separates that from "emitted and refused".
            let (bump, stub) = cratonvm_jit::runtime_lowering::ir_alloc_site_counts();
            eprintln!("[cratonvm] optimizing-tier allocations: inline-bump={bump} stub-only={stub}");
        }
        // Reference loads whose slot did NOT hold a reference, contained by
        // `GETFIELD_EXPECT_REFERENCE` instead of being handed to compiled code
        // as a pointer. Printed even when zero, and on the same switch: this
        // number is the difference between "the crash stopped happening" and
        // "the crash stopped being reachable this hour" -- the workload it was
        // found on crashed 3 times in 38 runs one hour and 0 in 129 the next.
        //
        // Non-zero is NOT good news. It counts type-punned reference slots this
        // VM is still producing (the G30-1 species); the guard only stops them
        // becoming wild pointers in compiled code.
        eprintln!(
            "[cratonvm] getfield reference loads that contained a primitive slot: {} \
             (of which the payload word was NON-ZERO, i.e. would have been \
             dereferenced: {})",
            cratonvm_vm::jit::helpers::jit_getfield_primitive_in_ref_slot(),
            cratonvm_vm::jit::helpers::jit_getfield_punned_ref_nonzero()
        );
        // The WRITE side of the same species, and the two ENGAGEMENT counters
        // for the substitutions that could produce it. All three printed even
        // when zero, for the reason the counter above them is:
        //
        // * `unresolved field sites refused` is how often a backend declined a
        //   `getfield`/`putfield`/`getstatic`/`putstatic` because it had no
        //   resolved slot. Those four sites used to substitute slot 0 tagged
        //   `int` and carry on, which is how JDT's `HashtableOfInt.rehash`
        //   wrote an `int[]` into slot 0 as `Value::Int(low32_of_ptr)`. A zero
        //   here says the substitution never fired on this workload -- which is
        //   the observation that has to precede blaming it for anything.
        // * `field sites refused for a descriptor disagreement` is how often
        //   the name-only field resolver found a field whose descriptor is not
        //   the one the constant pool names, so the site's slot index and its
        //   type tag would have described different fields.
        // * `compiled primitive stores into a declared-reference slot` counts
        //   only while `CRATONVM_DBG_WATCH_PUN` is armed; it is `<not armed>`
        //   otherwise rather than `0`, because those are different facts.
        eprintln!(
            "[cratonvm] unresolved field sites refused: {} | field sites refused \
             for a descriptor disagreement: {} | compiled primitive stores into \
             a declared-reference slot: {}",
            cratonvm_jit::x64::bytecode_walk::unresolved_field_site_bails(),
            cratonvm_vm::runtime::interpreter::jit_field_tag_disagreements(),
            cratonvm_vm::jit::helpers::jit_putfield_primitive_into_ref_slot()
                .map(|n| n.to_string())
                .unwrap_or_else(|| "<not armed>".to_string()),
        );
        // Compiled field stores the helper family DISCARDED. Both drops are
        // right in isolation -- writing through an implausible receiver, or
        // past the end of the object, corrupts the neighbouring allocation
        // instead. What they lacked is visibility: a dropped store leaves the
        // field exactly as it was, and for a field assigned once in a
        // constructor that is the zero-filled cell a reference `getfield` reads
        // back as an ordinary `null`. The defect then surfaces arbitrarily far
        // away as "this reference cannot be null", with nothing connecting it
        // to a store that did not happen.
        //
        // Printed even when zero, because zero is the useful reading: it rules
        // the whole mechanism out for a run, which is what a null-reference
        // investigation needs before it starts looking anywhere else.
        // `CRATONVM_DBG_DROPPED_PUTFIELD=1` names the receiver, the slot and
        // the compiled method for the first 32 of each.
        let (dropped_recv, dropped_oob) = cratonvm_vm::jit::helpers::jit_putfield_dropped_stores();
        // ENGAGEMENT counter for the JVMS 5.4.6 reclassification: how many
        // compiled call sites were bound directly because their constant pool
        // resolved to a PRIVATE method, instead of being dispatched from the
        // receiver's class. Without it, "the workload passes now" cannot be
        // told from "no site on this workload was one" -- and the shape (a
        // constructor calling its own `private void init()`, subclassed) is
        // common enough that a zero on a large workload is itself a finding.
        eprintln!(
            "[cratonvm] invokevirtual sites pinned to a private target: {}",
            cratonvm_jit::private_invokevirtual_pinned()
        );
        // Fieldrefs where a field of the NAME exists but not with the
        // constant pool's descriptor. Since 2026-08-28 each of these raises
        // NoSuchFieldError, which is what JVMS 5.4.3.2 says; before it, each
        // was answered by that other field.
        //
        // Deliberately NOT counting the case where the name exists nowhere:
        // that is NoSuchFieldError either way, so counting it would report
        // behaviour changes that did not happen -- as it did on SLF4J's own
        // version sanity check, which probes for a field it expects to be
        // absent and catches the error by design.
        //
        // A non-zero here on a workload that used to pass is the first place
        // to look; `CRATONVM_DBG_FIELD_DESCRIPTOR=1` names each one, and
        // `CRATONVM_FIELD_RESOLUTION_NAME_ONLY=1` restores the old answer for
        // a single-binary A/B.
        eprintln!(
            "[cratonvm] fieldrefs whose name exists but not with the recorded              descriptor (now NoSuchFieldError): {}",
            cratonvm_vm::runtime::resolve::field_resolution_descriptor_fallbacks()
        );
        // And the number that says whether applying the descriptor CHANGED an
        // answer. Every one of these is a field access that used to reach a
        // same-named field of another type -- and in compiled code, to pair
        // that field's slot index with the constant pool's type tag.
        eprintln!(
            "[cratonvm] field resolutions the descriptor key answered differently              from the name-only key: {}",
            cratonvm_vm::runtime::resolve::field_resolution_descriptor_corrections()
        );
        eprintln!(
            "[cratonvm] compiled field stores dropped: implausible receiver={dropped_recv}              slot out of bounds={dropped_oob}"
        );
        // G1 parallel-evacuation CAS losses, i.e. how often a worker found
        // another worker had already forwarded the object it was copying and
        // adopted that target. Until 2026-08-26 that arm returned the target
        // WITHOUT recording `old -> target` in this cycle's `pointer_map`, so
        // no root naming `old` could be remapped and it dangled once the
        // region was reused -- the rare `java/lang/Object`.
        //
        // Printed even when zero, for the reason the counter above it is: a
        // fix to an arm nothing reaches is a no-op dressed as a repair, and
        // this arm needs TWO WORKERS RACING ON ONE OBJECT, so a quiet run
        // means the race did not happen, not that the fix is inert.
        eprintln!(
            "[cratonvm] G1 evacuation CAS losses (forwards this VM would have \
             dropped before the 2026-08-26 fix): {}",
            cratonvm_vm::g1_evacuate_cas_loser_forwards()
        );
        // The `validate_code_ptr` memo's engagement, on the same switch and for
        // the same reason as every counter above it. The memo replaced a global
        // `Mutex` taken on EVERY compiled call; a run where `hits` is 0 has the
        // lock back and would still time within noise of one where it is not.
        {
            let (hits, misses) = cratonvm_jit::code_ptr_memo_stats();
            eprintln!(
                "[cratonvm] code-ptr memo: hits={hits} misses={misses} enabled={} \
                 region_epoch={}",
                cratonvm_jit::code_ptr_memo_enabled(),
                cratonvm_jit::code_ptr_regions_epoch()
            );
        }
        // JIT-side only: these are the sites `helpers.rs` tags by hand. The
        // whole-VM per-caller census that used to print beneath this was
        // retired once it had answered — it cost 3.4x on ZGC, which is how
        // the getfield page's first round of numbers came out wrong.
        eprintln!(
            "[cratonvm] membership walks by JIT site: {}",
            cratonvm_vm::jit::helpers::membership_walks_by_site()
                .iter()
                .map(|(n, c)| format!("{n}={c}"))
                .collect::<Vec<_>>()
                .join(" ")
        );
        // What the SPLICED-CALL emitter actually emitted, per arm. Zeros are
        // printed: a feature measuring "no different from the arm below it" and
        // a feature that never fired look identical in a timing table, and this
        // is the only line that separates them.
        eprintln!(
            "[cratonvm] inline call arms: {}",
            cratonvm_jit::metrics::inline_call_arm_emits()
                .iter()
                .map(|(n, c)| format!("{n}={c}"))
                .collect::<Vec<_>>()
                .join(" ")
        );
        // Compiled local exception handlers. `sites-emitted` counts STUBS, not
        // catches; `entered` is the only number that says a `catch` block ran
        // in compiled code, and `propagated` is its correct-but-not-a-win
        // sibling. Zeros printed, for the same reason as the line above.
        eprintln!(
            "[cratonvm] local handlers: {}",
            cratonvm_jit::metrics::local_handler_counts()
                .iter()
                .map(|(n, c)| format!("{n}={c}"))
                .collect::<Vec<_>>()
                .join(" ")
        );
        // Receiver-type speculation and the per-bci de-spec registry it now
        // consults. `sites-declined` is the only number that says the consult
        // engaged; `guards-emitted` separates "wired and never needed" from
        // "no guarded site compiled at all"; `escalations-spared` is the policy
        // half -- whole-method blacklists withheld from an already-withdrawn
        // speculation. Zeros printed, for the same reason as the lines above.
        eprintln!(
            "[cratonvm] receiver despec: {} escalations-spared={}",
            cratonvm_jit::metrics::receiver_despec_counts()
                .iter()
                .map(|(n, c)| format!("{n}={c}"))
                .collect::<Vec<_>>()
                .join(" "),
            cratonvm_jit::metrics::despec_escalations_spared()
        );
        // How WIDE each surviving blind spill was. `stores-emitted` against
        // `stores-if-full` is the narrowing's engagement AND its size in one
        // ratio; `full-refused` separates "narrowing kept everything" from
        // "narrowing was off".
        eprintln!(
            "[cratonvm] safepoint spill width: {} args-published={}",
            cratonvm_jit::metrics::spill_width_counts()
                .iter()
                .map(|(n, c)| format!("{n}={c}"))
                .collect::<Vec<_>>()
                .join(" "),
            cratonvm_jit::metrics::spill_args_published_count()
        );
        // The per-call safepoint blind spill, and how often the oop-clean-frame
        // proof let a direct call publish the safepoint id alone instead of
        // copying the whole GPR file into the frame. `elided` is the engagement
        // counter; the two refusal rows say why the others did not.
        eprintln!(
            "[cratonvm] direct-call spill: {}",
            cratonvm_jit::metrics::call_spill_counts()
                .iter()
                .map(|(n, c)| format!("{n}={c}"))
                .collect::<Vec<_>>()
                .join(" ")
        );
        // The RECEIVER-SHAPE census: which guard clause each helper call
        // actually failed, counted at EXECUTION on the one path every
        // fall-through crosses. The two lines above count EMISSIONS, which is
        // the question that cost this page four dead hypotheses. Needs
        // `CRATONVM_DBG_GETFIELD_RECEIVERS=1`; all-zero means it was not on.
        eprintln!(
            "[cratonvm] getfield receiver shapes: {}",
            cratonvm_vm::jit::helpers::jit_getfield_receiver_shapes()
                .iter()
                .map(|(n, c)| format!("{n}={c}"))
                .collect::<Vec<_>>()
                .join(" ")
        );
        eprintln!(
            "[cratonvm] getfield out-of-bounds by field kind: {}",
            cratonvm_vm::jit::helpers::jit_getfield_oob_field_kinds()
                .iter()
                .map(|(n, c)| format!("{n}={c}"))
                .collect::<Vec<_>>()
                .join(" ")
        );
        // The ALLOCATION side of the same question, from the path
        // `plan_object_alloc`'s `[compact-legacy]` census cannot see: the TLAB
        // fast path writes a legacy header unconditionally and serves ~99% of
        // allocations, so a class can be 100% of the legacy field receivers
        // above and appear in no allocation census at all. Needs
        // `CRATONVM_DBG_COMPACT_LEGACY`.
        let tlab_legacy = cratonvm_vm::runtime::interpreter::tlab_legacy_object_classes();
        if !tlab_legacy.is_empty() {
            eprintln!(
                "[cratonvm] TLAB-allocated legacy objects by class: {}",
                tlab_legacy
                    .iter()
                    .map(|(n, id, c)| format!("{n}(id={id})={c}"))
                    .collect::<Vec<_>>()
                    .join(" | ")
            );
        }
        let legacy_classes = cratonvm_vm::jit::helpers::jit_getfield_legacy_receiver_classes();
        if !legacy_classes.is_empty() {
            eprintln!(
                "[cratonvm] getfield legacy receivers by class: {}",
                legacy_classes
                    .iter()
                    .map(|(n, id, c)| format!("{n}(id={id})={c}"))
                    .collect::<Vec<_>>()
                    .join(" ")
            );
        }
        eprintln!(
            "[cratonvm] IR-tier inline-getfield refusals: {}",
            cratonvm_jit::metrics::ir_getfield_declines()
                .iter()
                .map(|(n, c)| format!("{n}={c}"))
                .collect::<Vec<_>>()
                .join(" ")
        );
        // The bytecode loop rewriter's admission tally, on the same switch and
        // for the same reason: it is what the compiler did, read at exit. The
        // counters themselves are always collected (they do not consult
        // `metrics::enabled()`), so this prints real numbers from a default
        // run — which is the measurement that retired three of the four gates
        // (`feature-designs/c2/loop-02-planner-admission-gates.md`) and is
        // what would say immediately if one of them got back in the way.
        //
        // The four condition rows OVERLAP: a method with an `invokedynamic`
        // compiled under `deopt_real` is in both. Read each against
        // `loop_xform_compiles`; never sum them. Only `loop_xform_inline_sites`
        // still refuses; the other three are counted and admitted.
        let tally = cratonvm_jit::metrics::loop_xform_counts();
        let row = tally
            .iter()
            .map(|(name, count)| format!("{name}={count}"))
            .collect::<Vec<_>>()
            .join(" ");
        eprintln!("[cratonvm] loop-xform admission: {row}");
    }

    // Phase accounting. Delegate the compile-phase breakdown rather than
    // re-timing it (`docs/observability/phase-accounting.md` §1):
    // `MetricsSummary::phase_totals_ns` is already `Vec<(&'static str, u64)>`,
    // which is exactly `set_compilation_breakdown`'s argument shape, so the
    // report carries `jit::metrics`' own numbers instead of a second set of
    // timers that could disagree with them.
    //
    // The `enabled()` test is not redundant with `emit_configured_sinks`
    // returning `None`: it also keeps a run with accounting off from paying for
    // the `jit::metrics` ring walk that `summary()` performs.
    if phase::enabled() {
        let summary = cratonvm_jit::metrics::summary();
        phase::set_compilation_breakdown(&summary.phase_totals_ns);
        if let Some(outcome) = phase::emit_configured_sinks() {
            // One integer-only line, in the shape
            // `regression-suite/perf/run-cratonbench-gate.sh` already scrapes.
            eprintln!("{}", outcome.report.summary_line());
            // A sink whose path cannot be opened is reported, never raised: a
            // diagnostic that failed a JVM shutdown because a directory is
            // read-only would be a worse defect than the missing artefact.
            if let Some((path, Err(e))) = &outcome.json {
                eprintln!("[cratonvm] phase-accounting JSON sink {path:?} failed: {e}");
            }
            if let Some((path, Err(e))) = &outcome.jfr {
                eprintln!("[cratonvm] phase-accounting JFR sink {path:?} failed: {e}");
            }
        }
    }
}

/// The `--help` long form: the short `about` (the doc comment on [`Args`])
/// plus a summary of the seven mode / JDK-only diagnostic flags.
///
/// Spelled as a `const` rather than extra doc-comment lines because clap
/// reflows a derived doc comment into paragraphs, which would destroy the
/// alignment of the table below. The wording is contract §9 of
/// `docs/feature-designs/jdk-only-mode.md`; keep the two in step.
const LONG_ABOUT: &str = "\
CratonVM - A Java Virtual Machine implemented in Rust.

Executes Java programs by loading and interpreting `.class` files.

Usage: cratonvm [OPTIONS] <CLASS_NAME> [ARGS]...
       cratonvm [OPTIONS] --jar <FILE.jar> [ARGS]...

Class library and compatibility policy:
  --real-jdk                    Real JDK class library with today's compatibility
                                behaviour (bridges, intrinsics AND compatibility
                                shims). This is the default.
  --synthetic-jdk               Standalone synthetic class library (~5,200 Rust
                                stubs). Conflicts with --real-jdk and --jdk-only.
                                Needs a binary built with the `synthetic-jdk`
                                Cargo feature, which is NOT in the default set;
                                without it the launcher exits with an error
                                instead of starting a VM with no class library.
                                `-Xinternalversion` reports whether this build
                                has it (jdk.mode.synthetic_compiled_in).
  --jdk-only                    Real JDK, and real class bytes are authoritative:
                                no fabricated compatibility class and no
                                synthetic-stub native. Implies --real-jdk.

JDK-only diagnostics (all four work in either compatibility mode, so a
default run can be censused before strict mode is switched on):
  --jdk-only-report <FILE>      Write the JSON violation/counter report.
  --dump-class-origins <FILE>   Write the class-origin census (one row per
                                loaded class, with its provenance).
  --trace-jdk-only              Log every recorded violation to stderr.
  --explain-jdk-only            Print the long-form explanation for each
                                violation, and leave absolute paths unredacted
                                in the reports.

Differential mode (docs/testing/diff-hotspot.md):
  --diff-hotspot                Run this same program under CratonVM and under a
                                reference JDK and report the FIRST divergence in
                                stdout / stderr / exit status. Exit 0 identical,
                                1 diverged, 2 no usable reference JDK, 3 the
                                CratonVM side was not self-consistent.
  --diff-ignore <PATTERN>       Mask any output line containing PATTERN on both
                                sides ('*' is a wildcard). Repeatable.
  --diff-runs <N>               CratonVM-side runs used to detect the program's
                                own nondeterminism before blaming HotSpot
                                (default 2; 1 disables the check).
  --diff-timeout <SECONDS>      Per-child wall clock (default 120).
  --diff-java-arg <ARG>         Extra argument for the reference `java` only.
                                Repeatable.
  --diff-strict                 Report a difference that only the built-in
                                nondeterminism maskers explain as a failure.
";

/// CratonVM - A Java Virtual Machine implemented in Rust.
///
/// Executes Java programs by loading and interpreting `.class` files.
///
/// Usage: cratonvm [OPTIONS] <CLASS_NAME> [ARGS]...
///        cratonvm [OPTIONS] --jar <FILE.jar> [ARGS]...
#[derive(Parser, Debug)]
#[command(name = "cratonvm", version, about, long_about = LONG_ABOUT)]
struct Args {
    /// The fully qualified class name to execute (e.g., com.example.Main).
    class_name: Option<String>,

    /// Execute a JAR file. The main class is read from META-INF/MANIFEST.MF.
    /// When -jar is used, the -cp/-classpath flag is ignored; the classpath
    /// comes from the JAR itself and its manifest Class-Path attribute.
    #[arg(long = "jar", value_name = "FILE")]
    jar: Option<String>,

    /// Classpath: directories and JAR files to search for classes.
    // `overrides_with` (self) makes a repeated flag last-wins instead of a hard
    // error, matching the real `java` launcher. Maven Surefire/the WildFly
    // testsuite fork with `-Xmx512m` twice (surefire memory args + jvm.args);
    // without this clap aborts with "cannot be used multiple times" (exit 2),
    // which Surefire reports as "forked VM terminated without saying goodbye".
    #[arg(
        short = 'c',
        long = "classpath",
        alias = "cp",
        overrides_with = "classpath"
    )]
    classpath: Option<String>,

    /// Maximum heap size (e.g., 256m, 1g).
    #[arg(long = "Xmx", value_name = "SIZE", overrides_with = "max_heap")]
    max_heap: Option<String>,

    /// Initial heap size (e.g., 16m, 512m).
    ///
    /// F-16: this used to be accepted and discarded, because the heap was
    /// allocated at `-Xmx` in the collector's constructor and there was nothing
    /// for an initial size to mean. Under G1 the heap is now RESERVED at `-Xmx`
    /// and COMMITTED on demand, so `-Xms` names the prefix committed up front.
    #[arg(long = "Xms", value_name = "SIZE", overrides_with = "initial_heap")]
    initial_heap: Option<String>,

    /// Print verbose class loading information.
    #[arg(long = "verbose:class")]
    verbose_class: bool,

    /// Print verbose GC information.
    #[arg(long = "verbose:gc")]
    verbose_gc: bool,

    /// Boot classpath (overrides JAVA_HOME auto-discovery).
    #[arg(long = "Xbootclasspath", value_name = "PATH")]
    boot_classpath: Option<String>,

    /// JAVA_HOME path for automatic boot/ext classpath discovery.
    #[arg(long = "java-home", value_name = "PATH")]
    java_home: Option<String>,

    /// Skip bytecode verification (like -noverify / -Xverify:none).
    #[arg(long = "noverify")]
    noverify: bool,

    /// Disable JIT compilation (interpreter-only execution).
    ///
    /// Equivalent to setting `CRATONVM_DISABLE_JIT=1` in the environment.
    /// Useful for diagnosing whether a misbehaviour originates in the JIT
    /// versus the interpreter, and as a safety fallback when the JIT is
    /// known to mis-compile a particular library.
    #[arg(long = "nojit")]
    nojit: bool,

    /// Bytecode verification policy (-Xverify:none|remote|all).
    /// `none` skips verification entirely (equivalent to --noverify).
    /// `remote` (HotSpot default) verifies non-boot classes only.
    /// `all` verifies boot classes too.
    #[arg(long = "Xverify", value_name = "MODE")]
    xverify: Option<String>,

    /// CDS shared archive file path (-XX:SharedArchiveFile=path).
    #[arg(long = "XX:SharedArchiveFile", value_name = "PATH")]
    shared_archive_file: Option<String>,

    /// CDS sharing mode (-Xshare:off/on/auto/dump).
    #[arg(long = "Xshare", value_name = "MODE", default_value = "off")]
    xshare: String,

    /// Select the synthetic class library (~5,200 Rust stubs in
    /// `native-builtins`) instead of real JDK bytecode.
    ///
    /// Mutually exclusive with `--real-jdk`. Requires a build with the
    /// `synthetic-jdk` Cargo feature; without it the launcher errors out
    /// rather than booting a VM with no class library (see
    /// `cratonvm_vm::config::require_synthetic_jdk`).
    ///
    /// The launcher default is `--real-jdk`
    /// (`cratonvm_vm::config::LAUNCHER_DEFAULT_JDK_MODE`), fixed at
    /// compile time. It is NOT derived from whether a JDK happens to be
    /// installed — see the determinism note on
    /// `cratonvm_vm::config::JdkMode`.
    #[arg(long = "synthetic-jdk", conflicts_with = "real_jdk")]
    synthetic_jdk: bool,

    /// Select the real JDK class library: `java.base` and friends are
    /// loaded from `$JAVA_HOME/jmods/*.jmod` or the `lib/modules` jimage,
    /// with only the ~300 truly-native methods implemented in Rust.
    ///
    /// This is already the launcher default; the flag exists so the
    /// choice can be stated explicitly (in scripts, CI lanes and bug
    /// reproductions) and so the two modes are symmetric. If no usable
    /// JDK is found the launcher fails with a message naming everything
    /// it searched — it never silently substitutes the synthetic library.
    #[arg(long = "real-jdk")]
    real_jdk: bool,

    /// Strict compatibility policy: real JDK class bytes are authoritative.
    ///
    /// Implies `--real-jdk` (a real runtime image is required — there is no
    /// silent fallback) and additionally forbids the two substitutions the
    /// default mode allows: no fabricated compatibility class
    /// (`ClassOrigin::CompatibilityStub`) may be minted, and no
    /// `NativeKind::SyntheticStub` may be registered. Reviewed bridges and
    /// intrinsics, arrays, hidden classes, lambdas, proxies and reflection
    /// accessors all remain legal — they are products of a conforming JVM,
    /// not compatibility substitutions.
    ///
    /// Conflicts with `--synthetic-jdk`: the synthetic library IS the set of
    /// substitutions this flag forbids, so the pair selects a VM with no
    /// usable class library.
    ///
    /// Wave 1 is measurement-first: violations that cannot yet be enforced
    /// safely are recorded and counted rather than fatal. Pair with
    /// `--jdk-only-report` to see them. See
    /// docs/feature-designs/jdk-only-mode.md.
    #[arg(long = "jdk-only", conflicts_with = "synthetic_jdk")]
    jdk_only: bool,

    /// Write the JDK-only violation/counter report to the given JSON file.
    ///
    /// Schema (`schema_version` 1): `{ mode, jdk_feature, violations[],
    /// counts{}, refusals{} }`, where `counts` carries the four class-origin
    /// buckets and the three per-`NativeKind` invocation totals, and
    /// `refusals` carries the four exact refusal-event tallies that have no
    /// place in the closed `counts` set. Violations are sorted so the file is
    /// diff-stable, and absolute paths are redacted unless
    /// `--explain-jdk-only` is also passed.
    ///
    /// `violations[]` unions all five recording sites: refused native
    /// registrations, compatibility-class requests, JIT compile-time direct
    /// binds, JIT fast-path admissions and interpreter bytecode-wins
    /// observations. A source that is silently omitted would make an empty
    /// array read as "clean" when it means "not measured", so the fold is
    /// all-or-nothing.
    ///
    /// Works in either compatibility mode. In the default `compatible` mode
    /// the report is a census of what strict mode *would* reject, built from
    /// the class-origin side alone: the four other sites only record when
    /// strict policy actually refuses something, so they and every `refusals`
    /// tally are zero there.
    #[arg(long = "jdk-only-report", value_name = "FILE")]
    jdk_only_report: Option<String>,

    /// Write the class-origin census to the given JSON file.
    ///
    /// Schema: `{ schema_version, counts{<origin-tag>: n, total},
    /// classes: [{name, origin, reason, requested_by, real_bytes_found,
    /// loader_id}] }`, one row per class the class manager holds, sorted by
    /// `(name, loader_id, origin)`. The `origin` tags are the stable
    /// `ClassOrigin::as_str()` spellings (`boot-image`, `vm-array`,
    /// `generated-lambda`, `compatibility-stub`, ...).
    #[arg(long = "dump-class-origins", value_name = "FILE")]
    dump_class_origins: Option<String>,

    /// Log each JDK-only violation to stderr as it is picked up.
    ///
    /// All five violation logs are drained once immediately after VM init
    /// (which is when registration refusals happen) and again at shutdown, on
    /// the same schedule, so the trace and `--jdk-only-report` name the same
    /// set. Each drain also prints one refusal-counter delta line when any
    /// counter moved — that is the only way `jit_inline_cache_natives`, which
    /// has no violation object behind it, becomes visible in a traced run.
    ///
    /// Compile-time and dispatch-time refusals cannot have happened yet at the
    /// post-init drain, so in practice they all surface at shutdown.
    #[arg(long = "trace-jdk-only")]
    trace_jdk_only: bool,

    /// Print the long-form, operator-facing explanation for each JDK-only
    /// violation instead of the one-line summary, and leave absolute paths
    /// unredacted in the reports and census files.
    #[arg(long = "explain-jdk-only")]
    explain_jdk_only: bool,

    /// Enable Panama FFI native access (mirrors JDK `--enable-native-access`).
    ///
    /// With native access disabled (the default after the security fix),
    /// Panama downcalls, `MemorySegment.ofAddress`, and `reinterpret` throw
    /// `IllegalCallerException`. Passing this flag opens the process-wide
    /// gate so those restricted FFI operations are permitted.
    ///
    /// The optional value (`ALL-UNNAMED` or a module name) mirrors the JDK
    /// spelling but is accepted-and-ignored: CratonVM's gate is a single
    /// coarse process-wide toggle, not a per-module grant. The flag is also
    /// accepted with no value at all (bare `--enable-native-access`). Optional
    /// values must use `=` so a following main class is not consumed as the
    /// module name. Because clap accepts the long name directly, the JDK
    /// invocation (`--enable-native-access=ALL-UNNAMED`) passes through
    /// unchanged.
    #[arg(
        long = "enable-native-access",
        value_name = "MODULE",
        num_args = 0..=1,
        default_missing_value = "ALL-UNNAMED",
        require_equals = true,
    )]
    enable_native_access: Option<String>,

    /// What to do when a RESTRICTED method is called without a native-access
    /// grant (mirrors JDK `--illegal-native-access=allow|warn|deny`).
    ///
    /// Default `warn`, which is what a JDK 25 launcher does — measured on
    /// Adoptium 25.0.4, `MemorySegment.reinterpret` succeeds with four WARNING
    /// lines and throws only under `deny`. CratonVM used to deny
    /// unconditionally with no way to ask for anything else, which is why this
    /// flag arrives together with the default change: the strict behaviour is
    /// still available, it is just no longer the only behaviour.
    #[arg(
        long = "illegal-native-access",
        value_name = "MODE",
        default_value = "warn"
    )]
    illegal_native_access: String,

    /// Enable preview features (mirrors JDK `--enable-preview`).
    ///
    /// A class file whose `minor_version` is 65535 at the running JVM's major
    /// version is a preview class file (JVMS 4.1), and HotSpot refuses to load
    /// it without this flag — measured on Adoptium 25.0.3.9:
    /// `java.lang.UnsupportedClassVersionError: Preview features are not
    /// enabled for P (class file version 69.65535). Try running with
    /// '--enable-preview'`. CratonVM loaded such a class file unconditionally
    /// until docs/known-issues/jdk-only/W7-28-preview-classfile-gating.md.
    ///
    /// Takes no value, unlike `--enable-native-access` above: HotSpot's flag is
    /// a bare boolean, so it needs no `VALUE_TAKING_OPTS` entry either (see the
    /// comment on that table — boolean flags are copied through verbatim by the
    /// separator inserter).
    #[arg(long = "enable-preview")]
    enable_preview: bool,

    /// AOT compilation mode (-XX:AOTMode=off/training/production).
    #[arg(long = "XX:AOTMode", value_name = "MODE", default_value = "off")]
    aot_mode: String,

    /// AOT cache file path (-XX:AOTCache=path). Used as input in production
    /// mode and as output in training mode (unless AOTCacheOutput is set).
    #[arg(long = "XX:AOTCache", value_name = "PATH")]
    aot_cache: Option<String>,

    /// AOT cache output path (-XX:AOTCacheOutput=path). Overrides AOTCache
    /// for writing in training mode.
    #[arg(long = "XX:AOTCacheOutput", value_name = "PATH")]
    aot_cache_output: Option<String>,

    /// Audit missing native methods: log all ACC_NATIVE methods that were invoked
    /// but had no Rust implementation. Printed on VM shutdown.
    #[arg(long = "XX:AuditMissingNatives")]
    audit_missing_natives: bool,

    /// HotSpot `-XX:±ShowCodeDetailsInExceptionMessages` (JEP 358): route the
    /// non-invoke null-deref opcodes (getfield/putfield/arraylength/array
    /// access/monitor/athrow) through the helpful-NPE message helper. Default
    /// **on**, matching HotSpot (messages verified byte-identical). Tri-state:
    /// absent → use the `VmConfig` default (on); the `-XX:+`/`-XX:-` JDK
    /// spellings are rewritten to `=true`/`=false` (see `rewrite_jvm_args`).
    #[arg(
        long = "XX:ShowCodeDetailsInExceptionMessages",
        num_args = 0..=1,
        default_missing_value = "true"
    )]
    show_code_details_in_exception_messages: Option<bool>,

    /// NEW-10: dump the missing-natives audit log to the given JSON file
    /// on VM shutdown. Implies `--XX:AuditMissingNatives`. The output
    /// schema is `{ "missing_natives": [{class, name, descriptor,
    /// sample_call_site}...] }` with entries sorted so the file is
    /// diff-stable against a committed baseline.
    #[arg(long = "dump-missing-natives", value_name = "FILE")]
    dump_missing_natives: Option<String>,

    /// T2.1.3: dump the missing-natives audit log to the given JSON file,
    /// grouped by JDK module. Implies `--XX:AuditMissingNatives`. Schema:
    /// `{ "version": 1, "modules": { "java.base": [...], "other": [...] } }`.
    /// Entries and module keys are sorted for byte-stable output, so the
    /// file is suitable for committing as a census baseline.
    #[arg(long = "dump-missing-natives-grouped", value_name = "FILE")]
    dump_missing_natives_grouped: Option<String>,

    /// Synthetic-stub census: dump every registered native with its
    /// classification (intrinsic / bridge / synthetic-stub) to the given
    /// JSON file on VM shutdown. Schema (`schema_version` 4):
    /// `{ "mode", "image_adjudication", "counts": {...}, "invocations": {...},
    /// "natives": [{class, name, descriptor, kind, registered_by, overwrote,
    /// invocations, kind_stated, real_declaring_method,
    /// image_declaring_method}...] }`, sorted by
    /// `(class, name, descriptor)` with a stable sort, so duplicate triples
    /// stay in registration order — the overwrite chronology — and the file is
    /// byte-stable across machines. `counts` counts *registrations* (schema 1's
    /// block, unchanged, so the stub ratchet still reads it); `invocations` is
    /// the separate per-kind dispatch total. Use this to verify the default
    /// build is synthetic-stub-free, and (via `invocations`) that no synthetic
    /// stub was dispatched.
    ///
    /// **`invocations` is a LOWER BOUND on calls, not a call count.** It counts
    /// dispatches that resolved the triple by name or id, and misses every
    /// dispatch served from a pre-resolved function pointer — the interpreter's
    /// intrinsic table and the JIT's thin direct-call helpers. MEASURED
    /// 2026-08-17: 100,000 `Math.abs` calls report 1, and the identical run
    /// under `CRATONVM_DISABLE_INTRINSICS=1` reports 100,000; 100,000
    /// `HashMap.get` calls report 1,873 with the JIT on and 100,001 under
    /// `--nojit`. A zero therefore does NOT mean a body is dead. **For a census
    /// whose `invocations` column is exact, run with `--nojit` and
    /// `CRATONVM_DISABLE_INTRINSICS=1`.** `owns_slot` is unaffected and remains
    /// the authoritative answer to "which body would run". Full method,
    /// controls and causal test:
    /// docs/known-issues/jdk-only/G33-1-the-instrument-that-under-reported-20260817.md.
    ///
    /// This flag, like every launcher option, is recognised only BEFORE the
    /// main class; placed after it, it is a program argument. The launcher now
    /// warns when that happens.
    ///
    /// Absolute registration-site paths are redacted, and
    /// `image_declaring_method` — the per-registration adjudication against the
    /// bytes on the class path, which is what tells a real `ACC_NATIVE` bridge
    /// from a registration nobody adjudicated — is `null`, unless
    /// `--explain-jdk-only` is also passed. See
    /// docs/synthetic-vs-real-explained.md and
    /// docs/feature-designs/jdk-only-mode.md §9.
    #[arg(long = "dump-native-registry", value_name = "FILE")]
    dump_native_registry: Option<String>,

    /// Phase accounting: write the whole-run wall-clock partition (startup /
    /// class load / compilation / GC / execution / shutdown, plus an explicit
    /// unattributed remainder) to the given JSON file at shutdown, and print
    /// the one-line `[PHASE-ACCOUNTING] …` summary to stderr. Sugar for
    /// `CRATONVM_PHASE_ACCOUNTING=coarse CRATONVM_PHASE_ACCOUNTING_OUT=<FILE>`;
    /// an explicitly-set `CRATONVM_PHASE_ACCOUNTING` (including the
    /// `CRATONVM_DBG=phase-accounting=fine` token spelling) still chooses the
    /// level, so this can be combined with `fine`. Schema and interpretation:
    /// docs/observability/phase-accounting.md.
    ///
    /// The value is consumed by the launcher before the flag snapshot is
    /// latched (see `launcher_phase_report_path`); this field exists so `clap`
    /// accepts the option and `--help` documents it.
    #[arg(long = "dump-phase-report", value_name = "FILE")]
    dump_phase_report: Option<String>,

    /// Enable JDWP debug server on the given port (e.g., 5005).
    /// Equivalent to -agentlib:jdwp=transport=dt_socket,server=y,address=PORT
    #[arg(long = "jdwp-port", value_name = "PORT")]
    jdwp_port: Option<u16>,

    /// Suspend VM at startup waiting for debugger to attach (requires --jdwp-port).
    #[arg(long = "jdwp-suspend")]
    jdwp_suspend: bool,

    // -----------------------------------------------------------------------
    // JPMS module system flags
    // -----------------------------------------------------------------------
    /// Module path: directories and modular JARs to search for modules.
    /// Format: path1;path2 (Windows) or path1:path2 (Unix).
    #[arg(long = "module-path", alias = "p", value_name = "PATH")]
    module_path: Option<String>,

    /// Add a read edge between modules.
    /// Format: `reader_module=target_module[,target_module2,...]`.
    /// Can be specified multiple times.
    #[arg(long = "add-reads", value_name = "MODULE=TARGET")]
    add_reads: Vec<String>,

    /// Export a package from a module to another module (or ALL-UNNAMED).
    /// Format: `module/package=target_module`.
    /// Can be specified multiple times.
    #[arg(long = "add-exports", value_name = "MODULE/PKG=TARGET")]
    add_exports: Vec<String>,

    /// Open a package for deep reflection from a module to another module.
    /// Format: `module/package=target_module`.
    /// Can be specified multiple times.
    #[arg(long = "add-opens", value_name = "MODULE/PKG=TARGET")]
    add_opens: Vec<String>,

    /// Additional root modules to resolve beyond the initial module.
    /// Can be specified multiple times.  `ALL-MODULE-PATH` resolves all
    /// modules found on the module path.
    #[arg(long = "add-modules", value_name = "MODULE")]
    add_modules: Vec<String>,

    /// Disable container/cgroup support (-XX:-UseContainerSupport).
    /// When set, the JVM ignores cgroup memory/CPU limits.
    #[arg(long = "XX:-UseContainerSupport")]
    disable_container_support: bool,

    /// Garbage-collector selector. Carries the collector name from a HotSpot
    /// `-XX:+Use<name>GC` flag (with the `Use`/`GC` wrapper stripped by
    /// `normalize_java_launcher_argv`), e.g. `G1` for `-XX:+UseG1GC`. CratonVM
    /// honours `G1` and the default `Generational`; any other collector warns
    /// and falls back to Generational. Repeated flags follow HotSpot last-wins
    /// (`overrides_with` self → no `ArgumentConflict` on a second occurrence).
    /// See docs/feature-designs/concurrent-gc-maturation.md §3.1.
    #[arg(long = "XX:UseGc", value_name = "NAME", overrides_with = "gc_selector")]
    gc_selector: Option<String>,

    /// `-XX:InitiatingHeapOccupancyPercent=<n>` → G1 IHOP (honoured under G1).
    #[arg(long = "XX:IHOP", value_name = "PCT", overrides_with = "g1_ihop")]
    g1_ihop: Option<String>,

    /// `-XX:MaxDirectMemorySize=<size>` -> direct (off-heap NIO) buffer
    /// accounting cap. Mirrors real JDK: when absent, the cap defaults to
    /// `-Xmx` instead of a fixed value. See
    /// fixed-suite-bugs/h2-suite-bugs/bug-h2-largeblob-direct-memory-oom.md.
    #[arg(
        long = "XX:MaxDirectMemorySize",
        value_name = "SIZE",
        overrides_with = "max_direct_memory"
    )]
    max_direct_memory: Option<String>,

    /// `-XX:G1HeapRegionSize=<bytes>` → G1 region size (honoured under G1).
    #[arg(
        long = "XX:G1RegionSize",
        value_name = "SIZE",
        overrides_with = "g1_region_size"
    )]
    g1_region_size: Option<String>,

    /// `-XX:MaxGCPauseMillis=<n>` → G1 pause target (honoured under G1).
    #[arg(
        long = "XX:MaxGCPause",
        value_name = "MS",
        overrides_with = "g1_max_pause"
    )]
    g1_max_pause: Option<String>,

    /// `-XX:ParallelGCThreads=<n>` → GC evacuation worker count (honoured
    /// under G1). Absent means the count is derived from the machine.
    #[arg(
        long = "XX:ParallelGCThreads",
        value_name = "N",
        overrides_with = "g1_parallel_gc_threads"
    )]
    g1_parallel_gc_threads: Option<String>,

    /// `-XX:±UseStringDeduplication` → G1 String dedup (honoured under G1).
    #[arg(
        long = "XX:StringDedup",
        value_name = "BOOL",
        overrides_with = "g1_string_dedup"
    )]
    g1_string_dedup: Option<String>,

    /// Unified logging spec (-Xlog:tag[+tag]*[=level][:output[:decorators]]).
    /// Example: --Xlog gc*=info:stdout:time,level,tags
    #[arg(long = "Xlog", value_name = "SPEC")]
    xlog: Option<String>,

    /// T19.H1 — if set, spawn a watchdog thread that, after `SECONDS`,
    /// signals every interpreter thread to dump its frame chain to
    /// stderr and then calls `std::process::abort()`. Used to
    /// diagnose silent-hang bootstraps (Keycloak, WildFly, Quarkus).
    /// The flag is honoured on a best-effort basis — threads stuck
    /// inside Rust native code will not dump (the watchdog still
    /// aborts with a reduced-information banner in that case).
    #[arg(long = "stack-dump-on-timeout", value_name = "SECONDS")]
    stack_dump_on_timeout: Option<u64>,

    /// READ THIS BEFORE AGGREGATING BY METHOD NAME: a leaf frame at
    /// `pc=0 last_pc=0` has executed NOTHING, and the time it represents
    /// belongs to the **invoke that pushed it**, not to its body. The hook
    /// that emits a sample sits at the top of the dispatch loop, and an
    /// invoke pushes the callee frame and `continue`s — so the first
    /// iteration able to observe a re-armed request after an expensive
    /// invoke reports the callee at its entry. Bucket those separately or an
    /// invoke-dense workload reads as "the callee body is slow". Calibrated
    /// by `probes/InvokeAttributionProbe.java`, where a three-bytecode callee
    /// takes 54% of samples at `pc=0` and one sample anywhere else; three
    /// profiles on the Tomcat annotation-scan page were misread this way.
    ///
    /// If set, sample every interpreter thread's Java frame chain to stderr
    /// every `MILLIS` and keep running (no abort). Unlike
    /// `--stack-dump-on-timeout`, which emits one dump per nested interpreter
    /// entry and therefore ranks methods by CALL COUNT, this is a
    /// time-weighted profile: aggregate the leaf frame of each emitted
    /// `T19.H1 stack dump` record to see where wall-clock actually goes.
    /// Diagnostic-only. JIT-compiled frames never reach the dispatch loop and
    /// so are not sampled — pair it with `--nojit`, or read the result as
    /// "of the interpreted time, ...".
    #[arg(long = "stack-sample-ms", value_name = "MILLIS")]
    stack_sample_ms: Option<u64>,

    // -----------------------------------------------------------------------
    // GPU offload (see docs/gpu/cuda-oxide-evaluation.md)
    //
    // Every field below is gated behind the `gpu` Cargo feature. Without
    // the feature the flags are not parsed, not documented in --help,
    // and the CPU execution path is byte-identical to before the GPU
    // work landed.
    // -----------------------------------------------------------------------
    /// Enable GPU offload of eligible static methods. Requires the
    /// CLI to be built with `--features gpu` and a CUDA driver. With
    /// no driver, the flag is honoured but no methods are offloaded.
    #[cfg(feature = "gpu")]
    #[arg(long = "gpu")]
    gpu: bool,

    /// Select CUDA device ordinal when --gpu is on. Defaults to 0.
    #[cfg(feature = "gpu")]
    #[arg(long = "gpu-device", value_name = "N", default_value_t = 0)]
    gpu_device: u32,

    /// Minimum estimated work (array length / loop trip count) before
    /// a method is offloaded. Smaller inputs run on the CPU because
    /// the host↔device round-trip dominates.
    #[cfg(feature = "gpu")]
    #[arg(long = "gpu-min-work", value_name = "N", default_value_t = 4096)]
    gpu_min_work: u32,

    /// Print one line per analyzer verdict (Eligible / Rejected) at
    /// INFO level. Useful for understanding why a method did or did
    /// not offload.
    #[cfg(feature = "gpu")]
    #[arg(long = "print-gpu-decisions")]
    print_gpu_decisions: bool,

    /// Probe the GPU, print device name + compute capability + memory,
    /// then exit. Useful for sanity-checking before a real run.
    #[cfg(feature = "gpu")]
    #[arg(long = "gpu-info")]
    gpu_info: bool,

    /// Arguments passed to the Java program's main method.
    #[arg(trailing_var_arg = true)]
    args: Vec<String>,
}

/// Validate that `name` is a valid Java class name in internal (slash) notation.
///
/// Each segment (split by `/`) must:
/// - Not be empty
/// - Start with a letter, underscore, or dollar sign
/// - Contain only alphanumeric characters, underscores, or dollar signs
fn validate_class_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("Class name must not be empty");
    }
    for (i, segment) in name.split('/').enumerate() {
        if segment.is_empty() {
            bail!(
                "Invalid class name '{}': segment {} is empty (double slash or leading/trailing slash)",
                name,
                i + 1
            );
        }
        let first = match segment.chars().next() {
            Some(ch) => ch,
            None => continue, // skip empty segments (already caught above, but be safe)
        };
        if !first.is_ascii_alphabetic() && first != '_' && first != '$' {
            bail!(
                "Invalid class name '{}': segment '{}' must start with a letter, underscore, or dollar sign",
                name,
                segment
            );
        }
        for ch in segment.chars() {
            if !ch.is_ascii_alphanumeric() && ch != '_' && ch != '$' {
                bail!(
                    "Invalid class name '{}': segment '{}' contains invalid character '{}'",
                    name,
                    segment,
                    ch
                );
            }
        }
    }
    Ok(())
}

/// Expand missing aggregate JAR references into sibling split JARs.
///
/// Some libraries ship as a single "all-in-one" fat JAR (e.g.
/// `netty-all.jar`, `groovy-all.jar`) or, alternatively, as a set of split
/// modules that live next to it (e.g. `netty-common.jar`, `netty-buffer.jar`,
/// …).  When a user passes a classpath entry that points at the aggregate JAR
/// but the repository only contains the split distribution, the classloader
/// silently drops the entry and the program fails with a puzzling
/// `NoClassDefFoundError`.
///
/// This helper detects that situation for a fixed set of known prefixes
/// (currently `netty`) and substitutes the aggregate name with every sibling
/// split JAR that shares the prefix.  The substitution only happens when the
/// aggregate file does *not* exist on disk; if it exists the entry is left
/// untouched.
///
/// Returns a new classpath list with the substitutions applied.  A warning is
/// emitted to stderr when a substitution occurs so the user knows what
/// happened.
/// Cheap sniff for whether `jar_path` looks like a Quarkus fast-jar /
/// runner packaging, used to decide whether the expensive multi-dir
/// classpath walk (the `app/quarkus/lib/...` probe in `run()`) is worth
/// running. A real Quarkus app always ships one of these signature
/// artifacts next to the runner jar:
///
///   * `quarkus-run.jar` — the canonical fast-jar launcher;
///   * `quarkus-app/` — the fast-jar output directory;
///   * a `quarkus/` subdir — holds `quarkus-application.dat` +
///     `generated-bytecode.jar`;
///   * `quarkus-application.dat` — the serialized bootstrap metadata.
///
/// We check the jar's own directory AND its parent (Keycloak puts the
/// runner one level deep in `lib/`), mirroring the two `roots` the walk
/// itself probes. Each check is a single `Path::exists()` stat — far
/// cheaper than the `canonicalize` + `read_dir` of ~10 candidate dirs the
/// walk performs. For a trivial non-Quarkus HelloWorld jar this returns
/// `false` after at most a handful of stats, skipping the walk entirely.
///
/// Conservative by design: any false positive merely re-enables the same
/// walk that previously always ran, so real Quarkus behaviour is never
/// degraded.
fn quarkus_signature_present(jar_path: &std::path::Path) -> bool {
    // Signature file/dir names looked for in each candidate directory.
    const SIGNATURES: &[&str] = &[
        "quarkus-run.jar",
        "quarkus-app",
        "quarkus",
        "quarkus-application.dat",
    ];

    // The runner jar's own dir, plus its parent (one-level-deep packagings
    // such as Keycloak's `lib/quarkus-run.jar`). No canonicalisation: a
    // relative `jar_path` still has a usable parent chain for `.join()`,
    // and `exists()` resolves relative paths against the cwd just fine.
    let mut dirs: Vec<&std::path::Path> = Vec::with_capacity(2);
    if let Some(d) = jar_path.parent() {
        // `Path::parent()` of a bare basename is `Some("")`; treat the
        // empty path as "current directory" so the stats still hit.
        dirs.push(if d.as_os_str().is_empty() {
            std::path::Path::new(".")
        } else {
            d
        });
        if let Some(pp) = d.parent() {
            if !pp.as_os_str().is_empty() {
                dirs.push(pp);
            }
        }
    } else {
        dirs.push(std::path::Path::new("."));
    }

    for dir in dirs {
        for sig in SIGNATURES {
            if dir.join(sig).exists() {
                return true;
            }
        }
    }
    false
}

fn expand_aggregate_jars(entries: Vec<String>) -> Vec<String> {
    // (aggregate_file_name, split_prefix) pairs.  The split prefix is
    // matched case-insensitively against sibling file names.
    const AGGREGATES: &[(&str, &str)] = &[("netty-all.jar", "netty-")];

    let mut out: Vec<String> = Vec::with_capacity(entries.len());
    for entry in entries {
        let path = std::path::Path::new(&entry);
        // PERF: the aggregate-jar case is rare, so do a cheap filename match
        // before touching the filesystem. Entries whose file name is not a
        // known aggregate name can never be substituted, so they skip the
        // `exists()` stat syscall (and the `read_dir` below) entirely.
        let file_name = match path.file_name().and_then(|s| s.to_str()) {
            Some(n) => n.to_ascii_lowercase(),
            None => {
                out.push(entry);
                continue;
            }
        };
        let matched = AGGREGATES.iter().find(|(name, _)| file_name == *name);
        let Some((_, split_prefix)) = matched else {
            out.push(entry);
            continue;
        };
        // Only now (for a candidate aggregate name) pay for the stat syscall.
        // If the entry already exists on disk (or is a directory), keep it.
        if path.exists() {
            out.push(entry);
            continue;
        }
        // Look for split jars next to where the aggregate was expected to be.
        let parent = match path.parent() {
            Some(p) if !p.as_os_str().is_empty() => p.to_path_buf(),
            _ => std::path::PathBuf::from("."),
        };
        let Ok(reader) = std::fs::read_dir(&parent) else {
            out.push(entry);
            continue;
        };
        let mut substitutes: Vec<String> = Vec::new();
        for dirent in reader.flatten() {
            let p = dirent.path();
            if p.extension().map(|e| e == "jar").unwrap_or(false) {
                if let Some(name) = p.file_name().and_then(|s| s.to_str()) {
                    if name.to_ascii_lowercase().starts_with(split_prefix)
                        && name.to_ascii_lowercase() != file_name
                    {
                        substitutes.push(p.to_string_lossy().into_owned());
                    }
                }
            }
        }
        if substitutes.is_empty() {
            // Nothing to substitute — preserve original entry so downstream
            // logging surfaces the missing file.
            out.push(entry);
            continue;
        }
        // Deterministic order helps reproducibility.
        substitutes.sort();
        eprintln!(
            "Note: classpath entry {entry} not found; substituting {n} sibling '{split_prefix}*.jar' files from {parent}",
            n = substitutes.len(),
            parent = parent.display()
        );
        out.extend(substitutes);
    }
    out
}

struct StagedArchiveCleanup {
    path: std::path::PathBuf,
}

static STAGED_ARCHIVE_COPIES: std::sync::OnceLock<std::sync::Mutex<Vec<std::path::PathBuf>>> =
    std::sync::OnceLock::new();

fn staged_archive_copies() -> &'static std::sync::Mutex<Vec<std::path::PathBuf>> {
    STAGED_ARCHIVE_COPIES.get_or_init(|| std::sync::Mutex::new(Vec::new()))
}

fn lock_staged_archive_copies(
    registry: &'static std::sync::Mutex<Vec<std::path::PathBuf>>,
) -> std::sync::MutexGuard<'static, Vec<std::path::PathBuf>> {
    match registry.lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    }
}

fn remove_staged_archive_copy(path: &std::path::Path) {
    if let Err(e) = std::fs::remove_file(path) {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(
                "failed to remove staged archive copy {}: {e}",
                path.display()
            );
        }
    }
}

fn register_staged_archive_copy(path: std::path::PathBuf) {
    lock_staged_archive_copies(staged_archive_copies()).push(path);
}

fn unregister_staged_archive_copy(path: &std::path::Path) {
    if let Some(registry) = STAGED_ARCHIVE_COPIES.get() {
        let mut guard = lock_staged_archive_copies(registry);
        guard.retain(|registered| registered.as_path() != path);
    }
}

fn cleanup_staged_archive_copies() {
    if let Some(registry) = STAGED_ARCHIVE_COPIES.get() {
        let paths: Vec<_> = {
            let mut guard = lock_staged_archive_copies(registry);
            guard.drain(..).collect()
        };
        for path in paths {
            remove_staged_archive_copy(&path);
        }
    }
}

impl Drop for StagedArchiveCleanup {
    fn drop(&mut self) {
        unregister_staged_archive_copy(&self.path);
        remove_staged_archive_copy(&self.path);
    }
}

fn classpath_entry_for_archive(
    archive_path: &std::path::Path,
) -> Result<(std::path::PathBuf, Option<StagedArchiveCleanup>)> {
    if archive_path
        .extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("jar"))
    {
        return Ok((archive_path.to_path_buf(), None));
    }

    // JN3: ClassPath::new only accepts entries whose extension is `.jar`
    // (or `.jmod`/`modules`). WARs, EARs, and other Java archive types are
    // silently dropped, so stage a temporary `.jar` copy that remains alive
    // until the launcher returns.
    let stem = archive_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("app");
    let pid = std::process::id();
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let dir = std::env::temp_dir();
    let mut tmp = dir.join(format!("cratonvm-{pid}-{now_ms}-{stem}.jar"));
    let mut dst_file = None;
    for attempt in 0..16 {
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
        {
            Ok(f) => {
                dst_file = Some(f);
                break;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                tmp = dir.join(format!("cratonvm-{pid}-{now_ms}-{attempt}-{stem}.jar"));
            }
            Err(e) => {
                return Err(anyhow::Error::new(e).context(format!(
                    "failed to stage {} as {} for classpath registration",
                    archive_path.display(),
                    tmp.display()
                )));
            }
        }
    }
    let mut dst_file = dst_file.ok_or_else(|| {
        anyhow::anyhow!(
            "failed to stage {} for classpath registration: \
             could not create a unique temp file in {}",
            archive_path.display(),
            dir.display()
        )
    })?;
    let copy_result = (|| -> Result<()> {
        let mut src_file = std::fs::File::open(archive_path)
            .with_context(|| format!("failed to open {} for staging", archive_path.display()))?;
        std::io::copy(&mut src_file, &mut dst_file).with_context(|| {
            format!(
                "failed to stage {} as {} for classpath registration",
                archive_path.display(),
                tmp.display()
            )
        })?;
        Ok(())
    })();
    drop(dst_file);
    if let Err(e) = copy_result {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }

    tracing::debug!(
        "JN3: staged non-.jar archive {} в†’ {} so ClassPath accepts it",
        archive_path.display(),
        tmp.display()
    );
    register_staged_archive_copy(tmp.clone());
    Ok((tmp.clone(), Some(StagedArchiveCleanup { path: tmp })))
}

/// Launcher options that consume the *following* argv token as their value
/// (the `--opt value` form). Needed by [`insert_program_args_separator`] so a
/// value token (e.g. the classpath string after `-cp`) is not mistaken for the
/// bare main-class name that selects the program. Both the HotSpot spellings
/// (`-jar`, `-classpath`, `-cp`, `-p`, `-mp`) and the clap long spellings
/// (`--jar`, `--classpath`, ...) are listed because this runs before
/// `normalize_java_launcher_argv` rewrites them.
///
/// Options using the inline `--opt=value` form need no entry here -- the value
/// travels in the same token. Boolean flags also need no entry.
const VALUE_TAKING_OPTS: &[&str] = &[
    "-jar",
    "--jar",
    "-classpath",
    "-cp",
    "--classpath",
    "-c",
    "-p",
    "--module-path",
    "-mp",
    // HotSpot single-dash forms used as separate tokens (`-Xmx 256m`,
    // `-Xshare on`, etc.). Most users write them inline (`-Xmx256m`),
    // but Maven Surefire and some test harnesses split them. Listing
    // them here keeps the separator-inserter from mistaking the value
    // token for a bare main-class name.
    "-Xmx",
    "-Xms",
    "-Xshare",
    "-Xverify",
    "-Xbootclasspath",
    "-Xlog",
    "--Xmx",
    "--Xms",
    "--Xbootclasspath",
    "--java-home",
    "--Xverify",
    "--XX:SharedArchiveFile",
    "--XX:UseGc",
    "--Xshare",
    "--XX:AOTMode",
    "--XX:AOTCache",
    "--XX:AOTCacheOutput",
    "--dump-missing-natives",
    "--dump-missing-natives-grouped",
    "--dump-native-registry",
    // JDK-only mode (docs/feature-designs/jdk-only-mode.md §9). Both take a
    // path; `--jdk-only`, `--trace-jdk-only` and `--explain-jdk-only` are
    // booleans and need no entry.
    "--jdk-only-report",
    "--dump-class-origins",
    "--jdwp-port",
    "--add-reads",
    "--add-exports",
    "--add-opens",
    "--add-modules",
    "--Xlog",
    "--stack-dump-on-timeout",
    "--stack-sample-ms",
    "--gpu-device",
    "--gpu-min-work",
];

/// Expand Java argument files (`@<path>`).
///
/// When the JVM launcher sees an argument starting with `@` (not `@@`),
/// it reads the file at that path and expands its contents as additional
/// command-line arguments. This is Java's argument-file feature (JEP 293,
/// available since Java 9). Gradle uses it to pass large classpaths via
/// `@classpath-file.txt` to avoid command-line length limits.
///
/// The file format:
/// - Arguments separated by whitespace (spaces, tabs, newlines)
/// - `"..."` or `'...'` quoted strings (quotes stripped, content preserved)
/// - `#` starts a comment to end of line
/// - Backslash escapes the next character inside quotes
/// - `@@path` is a literal `@path` (single-expansion escape)
///
/// Expansion is NOT recursive (nested `@file` references inside the
/// expanded content are left as-is) to avoid runaway expansion.
/// The `args[0]` element (program name) is never expanded.
fn expand_argfiles(args: Vec<String>) -> Vec<String> {
    if args.is_empty() {
        return args;
    }
    let mut out = vec![args[0].clone()]; // preserve argv[0]
    for arg in &args[1..] {
        if let Some(path_str) = arg.strip_prefix('@') {
            if path_str.starts_with('@') {
                // `@@path` -> literal `@path`
                out.push(path_str.to_string());
                continue;
            }
            match std::fs::read_to_string(path_str) {
                Ok(content) => {
                    // Tokenize the file contents
                    out.extend(tokenize_argfile(&content));
                    if std::env::var_os("CRATONVM_DBG_ARGS").is_some() {
                        eprintln!("[cratonvm] @-expanded {path_str}: {} tokens", out.len());
                    }
                }
                Err(e) => {
                    // If the file can't be read, leave the @arg as-is so
                    // downstream stages can produce a clear error message.
                    if std::env::var_os("CRATONVM_DBG_ARGS").is_some() {
                        eprintln!("[cratonvm] @-file read error {path_str}: {e}");
                    }
                    out.push(arg.clone());
                }
            }
        } else {
            out.push(arg.clone());
        }
    }
    out
}

/// Tokenize the content of a Java argument file.
///
/// Splits on unquoted whitespace; `"..."` and `'...'` preserve whitespace
/// and strip the outer quotes; `#` starts a comment to end of line;
/// backslash inside quotes escapes the following character.
fn tokenize_argfile(content: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut current = String::new();
    let mut chars = content.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '#' => {
                // Comment: skip to end of line
                if current.is_empty() {
                    // Flush any pending token
                } else {
                    tokens.push(std::mem::take(&mut current));
                }
                for ch2 in chars.by_ref() {
                    if ch2 == '\n' {
                        break;
                    }
                }
            }
            '"' | '\'' => {
                // Quoted string: collect until matching quote
                let quote = ch;
                loop {
                    match chars.next() {
                        None => break,
                        Some('\\') if quote == '"' => {
                            // Backslash escape inside double-quotes
                            if let Some(escaped) = chars.next() {
                                current.push(escaped);
                            }
                        }
                        Some(c) if c == quote => break,
                        Some(c) => current.push(c),
                    }
                }
            }
            ' ' | '\t' | '\r' | '\n' => {
                // Whitespace: flush token
                if !current.is_empty() {
                    tokens.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }
    if !current.is_empty() {
        tokens.push(current);
    }
    tokens
}

/// Enforce `java`-launcher positional semantics: every token *after* the
/// program selector is a program argument and must be passed to the Java
/// application verbatim -- even if it starts with `-`/`--` or equals
/// `--help` / `--version` / `--list-modules`.
///
/// The program is selected by either `-jar <jarfile>` or the first bare
/// (non-option) token used as a main-class name. This function scans the
/// leading option section and, as soon as it identifies the selector,
/// inserts a literal `--` separator immediately after it. The downstream
/// stages (`normalize_java_launcher_argv`, `extract_system_properties`,
/// `extract_hotspot_flags`) and clap itself all treat everything past `--`
/// as opaque program args, so launcher options are recognised only in the
/// leading section -- matching the stock `java` launcher.
///
/// If the caller already supplied an explicit `--`, or no program selector
/// is present (e.g. `java --version` / `java --help` with no program), the
/// argv is returned unchanged so the launcher still handles those itself.
fn insert_program_args_separator(args: Vec<String>) -> Vec<String> {
    if args.is_empty() {
        return args;
    }
    let mut out = vec![args[0].clone()];
    let mut i = 1usize;
    while i < args.len() {
        let a = args[i].as_str();
        // An explicit separator already delimits the program args -- respect
        // it and copy the remainder verbatim.
        if a == "--" {
            out.extend_from_slice(&args[i..]);
            return out;
        }
        // `-jar <jar>`: the jar is the selector. Copy `-jar` and its operand,
        // then insert `--` so the rest of argv is program args (unless the
        // caller already placed an explicit `--` there).
        if (a == "-jar" || a == "--jar") && i + 1 < args.len() {
            out.push(args[i].clone());
            out.push(args[i + 1].clone());
            if args.get(i + 2).map(String::as_str) != Some("--") {
                out.push("--".into());
            }
            out.extend_from_slice(&args[i + 2..]);
            return out;
        }
        // Inline `--jar=<jar>` / `-jar=<jar>` form.
        if a.starts_with("-jar=") || a.starts_with("--jar=") {
            out.push(args[i].clone());
            if args.get(i + 1).map(String::as_str) != Some("--") {
                out.push("--".into());
            }
            out.extend_from_slice(&args[i + 1..]);
            return out;
        }
        // An option that consumes the next token as its value -- copy both
        // and keep scanning; the value token is not the main-class name.
        if VALUE_TAKING_OPTS.contains(&a) {
            out.push(args[i].clone());
            if i + 1 < args.len() {
                out.push(args[i + 1].clone());
                i += 2;
            } else {
                i += 1;
            }
            continue;
        }
        // Any other `-`/`--`-prefixed token in the leading section is a
        // launcher option (boolean flag or inline `--opt=value`) -- copy and
        // continue scanning.
        if a.starts_with('-') {
            out.push(args[i].clone());
            i += 1;
            continue;
        }
        // First bare token: the main-class name. It selects the program;
        // insert `--` right after it so all following tokens are program
        // args (unless an explicit `--` already follows).
        out.push(args[i].clone());
        if args.get(i + 1).map(String::as_str) != Some("--") {
            out.push("--".into());
        }
        out.extend_from_slice(&args[i + 1..]);
        return out;
    }
    out
}

/// Launcher options that are **silently discarded** when they appear after the
/// main class, and whose silence is indistinguishable from success.
///
/// # Why this list exists and why it is not "every option"
///
/// `java` positional semantics are that everything after the program selector
/// belongs to the program, and [`insert_program_args_separator`] implements
/// exactly that. For most options a misplacement announces itself: `-cp` in the
/// tail produces a `ClassNotFoundException`, `-Xmx` in the tail produces an
/// `ArrayIndexOutOfBoundsException` from a program that did not expect an extra
/// argument. The options below are the ones where nothing at all happens —
/// exit 0, no file, no diagnostic — because their entire effect is to write a
/// diagnostic artefact or to turn a subsystem off.
///
/// That silence has cost real time in this campaign. `JDK-ONLY-REPORT-CENSUS-20260812`
/// closes with "**Flag order matters and is silent when wrong**" as its last
/// line, and `G33-1` was commissioned partly because it happened again. A
/// measurement lane that gets an empty result cannot tell "the VM says nothing
/// happened" from "the VM never heard me".
///
/// This is a **warning**, never an error, and the argv is never rewritten. A
/// Java program is entitled to an argument spelled `--jdk-only-report`, and
/// silently hoisting it out of the program's own argv would be a far worse bug
/// than the one being reported. Suppress with
/// `CRATONVM_NO_MISPLACED_FLAG_WARNING=1` for a program that really does take
/// one of these names.
const SILENTLY_IGNORED_IF_MISPLACED: &[&str] = &[
    // The census/diagnostic dump family (`docs/feature-designs/jdk-only-mode.md`
    // §9). Every one of these takes a path and its only observable effect is
    // the file, so a discarded flag looks exactly like a clean run.
    "--dump-native-registry",
    "--jdk-only-report",
    "--dump-class-origins",
    "--dump-missing-natives",
    "--dump-missing-natives-grouped",
    "--dump-phase-report",
    // The JDK-only mode switches. A discarded `--jdk-only` runs the whole
    // measurement in Compatible mode, which is the failure that produces a
    // *confidently wrong* result rather than an empty one.
    "--jdk-only",
    "--explain-jdk-only",
    "--trace-jdk-only",
    "--XX:AuditMissingNatives",
    // `--nojit` is on this list for the same reason as `--jdk-only`: a
    // discarded one silently measures the JIT arm and reports it as the
    // interpreter arm. `G20-1` §3 is an entire table of paired JIT/`--nojit`
    // arms; a silent miss there is not recoverable from the output.
    "--nojit",
    // Sampling/diagnostic switches whose absence is a quieter run, not an
    // error.
    "--stack-dump-on-timeout",
    "--stack-sample-ms",
];

/// Environment switch that silences [`misplaced_launcher_flags`]'s warning, for
/// a Java program that genuinely takes one of those names as its own argument.
const MISPLACED_FLAG_WARNING_OFF: &str = "CRATONVM_NO_MISPLACED_FLAG_WARNING";

/// Names from [`SILENTLY_IGNORED_IF_MISPLACED`] that appear in `argv` **after**
/// the program-args separator, i.e. that the launcher will discard.
///
/// Pure and order-preserving so it can be tested without a process: takes the
/// argv as [`insert_program_args_separator`] left it, returns the offending
/// spellings in the order they appear, each at most once. Both the bare
/// `--flag` and the inline `--flag=value` forms are recognised; the reported
/// name is always the bare one, because that is what the user has to move.
///
/// Returns empty when there is no separator at all — `java --version` with no
/// program has no tail, and every token is still a launcher option.
fn misplaced_launcher_flags(argv: &[String]) -> Vec<&'static str> {
    let Some(sep) = argv.iter().position(|a| a == "--") else {
        return Vec::new();
    };
    let mut found: Vec<&'static str> = Vec::new();
    for token in &argv[sep + 1..] {
        // `--flag=value` and `--flag` both report as `--flag`: the fix is the
        // same move either way, and naming the value back at the user only
        // makes the line harder to scan.
        let name = token.split('=').next().unwrap_or(token.as_str());
        if let Some(flag) = SILENTLY_IGNORED_IF_MISPLACED
            .iter()
            .find(|f| **f == name)
            .copied()
        {
            if !found.contains(&flag) {
                found.push(flag);
            }
        }
    }
    found
}

/// Print the [`misplaced_launcher_flags`] warning, if any, to stderr.
///
/// Deliberately loud and deliberately specific: it names each flag, states the
/// consequence in the tense that matters ("was passed to the Java program and
/// the launcher ignored it"), and shows the fix. A warning that says only
/// "check your argument order" leaves the reader doing the work this function
/// already did.
fn warn_about_misplaced_launcher_flags(argv: &[String]) {
    // `std::env::var_os`, deliberately, and this one may NOT be converted.
    // This function runs from `main` BEFORE `install_flags` latches the
    // snapshot (the call is ~37 lines earlier), so reading through
    // `flags::runtime_var_os` here latches it early and `install_flags` then
    // fails with "runtime flags were read before launcher configuration" —
    // every run exits 1. Measured, not reasoned: converting it broke `java
    // Hello`. The name is exempt in `flag_declaration_guard`'s `ALLOWED` as
    // kind 4 for exactly this reason.
    if std::env::var_os(MISPLACED_FLAG_WARNING_OFF).is_some() {
        return;
    }
    let misplaced = misplaced_launcher_flags(argv);
    if misplaced.is_empty() {
        return;
    }
    for flag in &misplaced {
        eprintln!(
            "[cratonvm] WARNING: `{flag}` appears AFTER the main class (or after `-jar <jar>`), \
             so it was passed to the Java program as an argument and the launcher IGNORED it. \
             This flag has no effect where it is."
        );
    }
    eprintln!(
        "[cratonvm] WARNING: move {} before the main class. \
         Launcher options are recognised only ahead of the program selector, exactly as in \
         `java`. If the program really does take {} as its own argument, set {}=1 to silence \
         this.",
        misplaced
            .iter()
            .map(|f| format!("`{f}`"))
            .collect::<Vec<_>>()
            .join(", "),
        if misplaced.len() == 1 {
            "that name"
        } else {
            "those names"
        },
        MISPLACED_FLAG_WARNING_OFF,
    );
}

/// Rewrite common HotSpot launcher spellings so clap can parse them.
///
/// HotSpot uses single-dash flags with idiosyncratic syntax (`-Xmx256m`,
/// `-Xshare:on`, `-XX:AOTMode=off`, `-XX:-UseContainerSupport`, etc.) that
/// clap's "long option" parser cannot handle natively — clap expects
/// `--Xmx 256m`. The `[[bin]] name = "java"` alias (see Cargo.toml) exists
/// specifically so the cratonvm binary is drop-in compatible with stock
/// `java`, so a Maven / Surefire / Gradle invocation like
/// `java -Xmx256m -classpath x Main` MUST parse.
///
/// This function rewrites every HotSpot single-dash spelling we care about
/// to the equivalent double-dash clap form *before* clap sees the argv.
/// `extract_hotspot_flags` handles the few cases that aren't expressible as
/// clap long options (`-XX:+/-Foo` boolean toggles for `HeapDumpOnOutOfMemoryError`,
/// `-agentlib:` / `-agentpath:` / `-javaagent:`).
fn normalize_java_launcher_argv(args: Vec<String>) -> Vec<String> {
    if args.is_empty() {
        return args;
    }
    let mut out = vec![args[0].clone()];
    let mut i = 1usize;
    let mut past_separator = false;
    while i < args.len() {
        let a = args[i].as_str();
        if past_separator {
            out.push(args[i].clone());
            i += 1;
            continue;
        }
        if a == "--" {
            past_separator = true;
            out.push(args[i].clone());
            i += 1;
            continue;
        }
        // JBoss Modules / WildFly use `-mp <modules dir>` after `-jar
        // jboss-modules.jar`. clap parses `-mp` as the short-flag cluster
        // `-m` + `-p`, which errors ("unexpected argument '-m'"). Mirror an
        // explicit `--` so everything from `-mp` onward becomes program args.
        if a == "-mp" {
            out.push("--".into());
            past_separator = true;
            out.push(args[i].clone());
            i += 1;
            continue;
        }
        if a == "-jar" && i + 1 < args.len() {
            out.push("--jar".into());
            out.push(args[i + 1].clone());
            i += 2;
        } else if a == "-version" || a == "-v" {
            out.push("--version".into());
            i += 1;
        } else if (a == "-classpath" || a == "-cp") && i + 1 < args.len() {
            out.push("--classpath".into());
            out.push(args[i + 1].clone());
            i += 2;
        } else if let Some(rest) = a.strip_prefix("-classpath=") {
            out.push("--classpath".into());
            out.push(rest.to_string());
            i += 1;
        } else if let Some(rest) = a.strip_prefix("-cp=") {
            out.push("--classpath".into());
            out.push(rest.to_string());
            i += 1;
        }
        // -------- HotSpot -X compat: inline single-dash spellings --------
        // `-Xmx256m` / `-Xmx 256m` -> `--Xmx 256m`
        else if let Some(rest) = a.strip_prefix("-Xmx") {
            if rest.is_empty() && i + 1 < args.len() {
                out.push("--Xmx".into());
                out.push(args[i + 1].clone());
                i += 2;
            } else {
                out.push("--Xmx".into());
                out.push(rest.to_string());
                i += 1;
            }
        }
        // `-Xshare:on` / `-Xshare:off` -> `--Xshare on`
        else if let Some(rest) = a.strip_prefix("-Xshare:") {
            out.push("--Xshare".into());
            out.push(rest.to_string());
            i += 1;
        }
        // `-Xshare<sp>val` -> `--Xshare val` (rare)
        else if a == "-Xshare" && i + 1 < args.len() {
            out.push("--Xshare".into());
            out.push(args[i + 1].clone());
            i += 2;
        }
        // `-Xverify:none|remote|all` -> `--Xverify none|remote|all`
        else if let Some(rest) = a.strip_prefix("-Xverify:") {
            // `-Xverify:none` is the HotSpot shorthand for `-noverify`.
            // Translate to `--noverify` so the boolean flag fires; the
            // existing CLI also accepts `--Xverify none` as a value flag.
            if rest == "none" {
                out.push("--noverify".into());
            } else {
                out.push("--Xverify".into());
                out.push(rest.to_string());
            }
            i += 1;
        }
        // `-Xverify val` (separate token, rare)
        else if a == "-Xverify" && i + 1 < args.len() {
            out.push("--Xverify".into());
            out.push(args[i + 1].clone());
            i += 2;
        }
        // `-noverify` -> `--noverify` (HotSpot deprecated but still accepted)
        else if a == "-noverify" {
            out.push("--noverify".into());
            i += 1;
        }
        // `-Xbootclasspath:path` / `-Xbootclasspath/a:path` / `-Xbootclasspath/p:path`
        // -> `--Xbootclasspath path`. The /a (append) and /p (prepend) forms
        // are collapsed to a plain replace; cratonvm does not model the three
        // positions separately (boot CP is a single ordered list).
        else if let Some(rest) = a.strip_prefix("-Xbootclasspath/a:") {
            out.push("--Xbootclasspath".into());
            out.push(rest.to_string());
            i += 1;
        } else if let Some(rest) = a.strip_prefix("-Xbootclasspath/p:") {
            out.push("--Xbootclasspath".into());
            out.push(rest.to_string());
            i += 1;
        } else if let Some(rest) = a.strip_prefix("-Xbootclasspath:") {
            out.push("--Xbootclasspath".into());
            out.push(rest.to_string());
            i += 1;
        } else if a == "-Xbootclasspath" && i + 1 < args.len() {
            out.push("--Xbootclasspath".into());
            out.push(args[i + 1].clone());
            i += 2;
        }
        // `-Xlog:spec` -> `--Xlog spec`
        else if let Some(rest) = a.strip_prefix("-Xlog:") {
            out.push("--Xlog".into());
            out.push(rest.to_string());
            i += 1;
        } else if a == "-Xlog" && i + 1 < args.len() {
            out.push("--Xlog".into());
            out.push(args[i + 1].clone());
            i += 2;
        }
        // -------- HotSpot -XX compat: -XX:Foo=val and -XX:+/-Foo --------
        // `-XX:SharedArchiveFile=path` -> `--XX:SharedArchiveFile path`
        else if let Some(rest) = a.strip_prefix("-XX:SharedArchiveFile=") {
            out.push("--XX:SharedArchiveFile".into());
            out.push(rest.to_string());
            i += 1;
        }
        // `-XX:AOTMode=off|training|production` -> `--XX:AOTMode <val>`
        else if let Some(rest) = a.strip_prefix("-XX:AOTMode=") {
            out.push("--XX:AOTMode".into());
            out.push(rest.to_string());
            i += 1;
        }
        // `-XX:AOTCache=path` -> `--XX:AOTCache <path>`
        else if let Some(rest) = a.strip_prefix("-XX:AOTCache=") {
            out.push("--XX:AOTCache".into());
            out.push(rest.to_string());
            i += 1;
        }
        // `-XX:AOTCacheOutput=path` -> `--XX:AOTCacheOutput <path>`
        else if let Some(rest) = a.strip_prefix("-XX:AOTCacheOutput=") {
            out.push("--XX:AOTCacheOutput".into());
            out.push(rest.to_string());
            i += 1;
        }
        // `-XX:+AuditMissingNatives` -> `--XX:AuditMissingNatives`
        // `-XX:-AuditMissingNatives` -> drop (default off).
        else if a == "-XX:+AuditMissingNatives" {
            out.push("--XX:AuditMissingNatives".into());
            i += 1;
        } else if a == "-XX:-AuditMissingNatives" {
            // Default is off; nothing to emit.
            i += 1;
        }
        // `-XX:+ShowCodeDetailsInExceptionMessages` -> the clap toggle (on);
        // `-XX:-...` -> the explicit `=false` form (the default is now on, so
        // opting out must be representable, not merely "absent").
        else if a == "-XX:+ShowCodeDetailsInExceptionMessages" {
            out.push("--XX:ShowCodeDetailsInExceptionMessages=true".into());
            i += 1;
        } else if a == "-XX:-ShowCodeDetailsInExceptionMessages" {
            out.push("--XX:ShowCodeDetailsInExceptionMessages=false".into());
            i += 1;
        }
        // `-XX:-UseContainerSupport` -> `--XX:-UseContainerSupport`
        // (the clap long name literally is `XX:-UseContainerSupport`).
        else if a == "-XX:-UseContainerSupport" {
            out.push("--XX:-UseContainerSupport".into());
            i += 1;
        } else if a == "-XX:+UseContainerSupport" {
            // Default is on; nothing to emit.
            i += 1;
        }
        // `-Xms<size>` (minimum/initial heap) -> `--Xms <size>`.
        //
        // F-16: this arm used to DROP the flag. "CratonVM sizes the heap from
        // `-Xmx` only" was true while the collector allocated its whole arena
        // in the constructor — there was no initial size for the value to name.
        // G1 now reserves `-Xmx` as address space and commits a prefix, so
        // `-Xms` names that prefix and is honoured.
        //
        // Both the inline (`-Xms512m`) and separate-token (`-Xms 512m`) forms
        // must be handled. The separate-token form is listed in
        // `VALUE_TAKING_OPTS`, so `insert_program_args_separator` keeps the
        // value adjacent to the flag here; emitting the value as clap's own
        // argument is what stops a bare `512m` surviving to be mistaken for the
        // main-class positional (which would shift and consume the real class
        // name and the program args after it). Same shape as the `-Xmx` arm
        // above; a bare trailing `-Xms` with no value is dropped, as before.
        else if let Some(rest) = a.strip_prefix("-Xms") {
            if rest.is_empty() {
                if i + 1 < args.len() {
                    out.push("--Xms".into());
                    out.push(args[i + 1].clone());
                    i += 2;
                } else {
                    i += 1;
                }
            } else {
                out.push("--Xms".into());
                out.push(rest.to_string());
                i += 1;
            }
        }
        // GC selector: `-XX:+Use<Name>GC` -> `--XX:UseGc <Name>`. The collector
        // name (`<Name>` between `Use` and `GC`) is forwarded verbatim; the
        // config-apply step (`parse_gc_algorithm`) honours `G1` / `Generational`
        // and warns-and-falls-back for any other collector. Emitting a value
        // option (rather than acting here) means a later `-XX:+Use...GC`
        // overrides an earlier one via clap last-wins, matching HotSpot.
        //
        // `-XX:-UseG1GC` / `-XX:-UseZGC` explicitly turn that collector off ->
        // select Generational. Note this is "off means the copying collector",
        // NOT "off means whatever the default is" — as of 2026-08-10 the
        // default IS ZGC, so resolving `-XX:-UseZGC` to the default would make
        // the flag select the very collector it disables. Other
        // `-XX:-Use<Name>GC` ("do not use collector X") select nothing and fall
        // through to the silent-ignore arm below.
        //
        // Guarded so `-XX:+UseStringDeduplication`, `-XX:+UseCompressedOops`,
        // etc. (no `GC` suffix) do NOT match and keep their existing handling.
        else if let Some(name) = a
            .strip_prefix("-XX:+Use")
            .and_then(|core| core.strip_suffix("GC"))
            .filter(|core| !core.is_empty())
        {
            out.push("--XX:UseGc".into());
            out.push(name.to_string());
            i += 1;
        } else if a == "-XX:-UseG1GC" || a == "-XX:-UseZGC" {
            out.push("--XX:UseGc".into());
            out.push("Generational".into());
            i += 1;
        }
        // G1 tuning knobs (§7 item 4). `-XX:Name=Value` → `--XX:<short> Value`;
        // honoured only under G1 (the config-apply step is G1-gated). Each maps
        // to a `G1CollectorConfig` field via `G1ConfigOverrides`.
        else if let Some(v) = a.strip_prefix("-XX:InitiatingHeapOccupancyPercent=") {
            out.push("--XX:IHOP".into());
            out.push(v.to_string());
            i += 1;
        } else if let Some(v) = a.strip_prefix("-XX:G1HeapRegionSize=") {
            out.push("--XX:G1RegionSize".into());
            out.push(v.to_string());
            i += 1;
        } else if let Some(v) = a.strip_prefix("-XX:MaxGCPauseMillis=") {
            out.push("--XX:MaxGCPause".into());
            out.push(v.to_string());
            i += 1;
        } else if let Some(v) = a.strip_prefix("-XX:MaxDirectMemorySize=") {
            out.push("--XX:MaxDirectMemorySize".into());
            out.push(v.to_string());
            i += 1;
        } else if a == "-XX:+UseStringDeduplication" {
            out.push("--XX:StringDedup".into());
            out.push("true".into());
            i += 1;
        } else if a == "-XX:-UseStringDeduplication" {
            out.push("--XX:StringDedup".into());
            out.push("false".into());
            i += 1;
        }
        // These `-XX` flags are not expressible as clap long names, so keep
        // them verbatim for `extract_hotspot_flags`, which runs after this
        // normalization stage.
        else if a == "-XX:+HeapDumpOnOutOfMemoryError"
            || a == "-XX:-HeapDumpOnOutOfMemoryError"
            || a.starts_with("-XX:HeapDumpPath=")
        {
            out.push(args[i].clone());
            i += 1;
        }
        // Any other `-XX:...` flag is a HotSpot tuning knob CratonVM does not
        // implement (`-XX:MetaspaceSize`, `-XX:MaxMetaspaceSize`,
        // `-XX:+ExitOnOutOfMemoryError`, …).
        // Recognized `-XX:` flags are rewritten by the branches above;
        // everything else is silently ignored so a Maven Surefire / Gradle
        // fork — which passes these unconditionally — launches instead of clap
        // aborting with "unexpected argument '-X'".
        else if a.starts_with("-XX:") {
            if std::env::var_os("CRATONVM_DBG_ARGS").is_some() {
                eprintln!("[cratonvm] ignoring unimplemented HotSpot flag: {a}");
            }
            i += 1;
        }
        // Any other single-dash `-X...` flag is a HotSpot knob CratonVM does
        // not implement (`-Xss<size>` thread stack size — Surefire/Gradle pass
        // this routinely — `-Xint`, `-Xbatch`, `-Xrs`, `-XshowSettings`,
        // `-Xnoclassgc`, …). The recognized value-taking `-X` spellings
        // (`-Xmx`/`-Xms`/`-Xshare`/`-Xverify`/`-Xbootclasspath`/`-Xlog`) are
        // rewritten by the branches above; everything else is accepted-and-
        // ignored here — same as `-XX:` — so a drop-in `java` launches instead
        // of clap aborting with "unexpected argument '-X...'". These remaining
        // `-X` flags are all the inline/no-value form, so dropping just this
        // single token is correct (HotSpot has no separate-token spelling for
        // the ones not handled above).
        //
        // [LOW arg-parse fix (3)] Deliberately drop ONLY this one token
        // (`i += 1`) and never consume the following token. An unrecognized
        // `-X` flag must not swallow the next token when that token is the
        // main-class name: e.g. `java -Xunknown Main` (or, if the separator
        // inserter did not run, `-Xint Main`) must still resolve `Main` as the
        // main class, not silently treat it as the unknown flag's value and
        // shift the real class into the program args. We make that invariant
        // explicit here: when the next token is a bare positional (no leading
        // `-`), it is the main-class candidate and is left for the normal
        // positional/`--` handling to claim — we never absorb it.
        else if a.starts_with("-X") {
            if std::env::var_os("CRATONVM_DBG_ARGS").is_some() {
                eprintln!("[cratonvm] ignoring unimplemented HotSpot flag: {a}");
                // Surface the swallow-avoidance: if a bare positional follows an
                // unknown `-X` flag, it is the main-class candidate and is left
                // untouched (we drop only the flag, never `i += 2`).
                if let Some(next) = args.get(i + 1) {
                    if !next.starts_with('-') && next != "--" {
                        eprintln!(
                            "[cratonvm] keeping following token '{next}' as a \
                             positional (unknown -X flag does not consume it)"
                        );
                    }
                }
            }
            // Drop ONLY this flag token; do NOT consume the next token. This is
            // what keeps an unknown `-X` flag from swallowing the main-class
            // name (see the comment above).
            i += 1;
        }
        // `--sun-misc-unsafe-memory-access=<mode>` (JEP 498). HotSpot's launcher
        // does exactly this rewrite: the flag's only effect is to set the
        // `sun.misc.unsafe.memory.access` system property, and when the flag is
        // absent HotSpot sets NOTHING — the effective mode is still "allow with
        // a warning", but `System.getProperty` answers null.
        //
        // That null is load-bearing for more than tidiness. netty 4.2 disables
        // `sun.misc.Unsafe` by default on Java 25+ *unless* this property is
        // set, so a VM that pins it — which CratonVM did, unconditionally, in
        // `vm_init.rs` — silently puts netty and every other Unsafe-aware
        // library on a different code path than a stock JDK 25 run, and makes
        // any CratonVM-vs-HotSpot comparison over them a comparison of two
        // different code paths rather than two VMs.
        //
        // Rewriting to `-D` here rather than parsing it later gives the
        // property exactly one source: the user.
        else if let Some(mode) = a.strip_prefix("--sun-misc-unsafe-memory-access=") {
            if std::env::var_os("CRATONVM_DBG_ARGS").is_some() {
                eprintln!(
                    "[cratonvm] --sun-misc-unsafe-memory-access={mode} -> \
                     -Dsun.misc.unsafe.memory.access={mode}"
                );
            }
            out.push(format!("-Dsun.misc.unsafe.memory.access={mode}"));
            i += 1;
        }
        // HotSpot VM-selection flags. Modern HotSpot accepts `-server` and
        // `-client` for compatibility (the server VM is effectively the only
        // implementation on current JDKs). WildFly's HostController launch
        // command still passes `-server`; accept-and-ignore it so the `java`
        // shim remains drop-in compatible instead of clap interpreting
        // `-server` as a short-option cluster and aborting on `-s`.
        else if a == "-server" || a == "-client" {
            if std::env::var_os("CRATONVM_DBG_ARGS").is_some() {
                eprintln!("[cratonvm] ignoring HotSpot VM selection flag: {a}");
            }
            i += 1;
        }
        // Assertion control flags: `-ea`/`-enableassertions[:<pkgname>...|:<classname>]`,
        // `-da`/`-disableassertions[...]`, `-esa`/`-enablesystemassertions`,
        // `-dsa`/`-disablesystemassertions`.
        //
        // They are dropped HERE (clap would read `-ea` as the short-option
        // cluster `-e -a` and abort with "unexpected argument '-e'"), but they
        // are no longer *ignored*: `main` scans the same argv with
        // [`launcher_assertions_requested`] before the flag snapshot is
        // latched, and the unscoped spellings set `CRATONVM_ENABLE_ASSERTIONS`.
        // This arm has to stay a pure token filter — see that function for why
        // the switch cannot be flipped from inside this (pure, ~60-test)
        // normaliser.
        //
        // The *scoped* forms (`-ea:some.pkg...`, `-da:some.Class`) really are
        // ignored: `assertion_status_default()` is one global with no
        // per-package granularity, and reading `-ea:some.pkg` as global-enable
        // would switch on assertions for the classes the caller deliberately
        // left out. `CRATONVM_DBG_ARGS` names them so the silence is
        // discoverable.
        else if a == "-ea"
            || a == "-da"
            || a == "-esa"
            || a == "-dsa"
            || a.starts_with("-ea:")
            || a.starts_with("-da:")
            || a.starts_with("-enableassertions")
            || a.starts_with("-disableassertions")
            || a.starts_with("-enablesystemassertions")
            || a.starts_with("-disablesystemassertions")
        {
            if std::env::var_os("CRATONVM_DBG_ARGS").is_some() {
                if assertion_flag_scope(a).is_some() {
                    eprintln!("[cratonvm] assertion flag applied JVM-wide: {a}");
                } else {
                    eprintln!(
                        "[cratonvm] ignoring scoped assertion flag (no per-package \
                         granularity): {a}"
                    );
                }
            }
            i += 1;
        } else {
            out.push(args[i].clone());
            i += 1;
        }
    }
    out
}

/// Pre-process raw command-line arguments to extract `-Dkey=value` system
/// property flags (Java-style) before handing the rest to clap.  Returns
/// `(filtered_args, system_properties)`.
fn extract_system_properties(raw: Vec<String>) -> (Vec<String>, Vec<(String, String)>) {
    let mut filtered = Vec::with_capacity(raw.len());
    let mut props = Vec::new();
    let mut past_separator = false;
    // [LOW arg-parse fix (2)] When the PREVIOUS token was a separate-token
    // value-taking option (`--classpath`, `--Xlog`, `--add-opens`, …), the
    // CURRENT token is that option's VALUE, not an option position. A value
    // may legitimately begin with `-D` (e.g. `--Xlog -Dspecial`, or a
    // classpath/module-path entry on an exotic path), and must NOT be hijacked
    // as a `-Dkey=value` system property — doing so both loses the option's
    // value and fabricates a bogus property. Track the value position and pass
    // it through verbatim. (`-D` itself is never a value-taking option name, so
    // a genuine `-Dkey=value` in an *option* position is still extracted.)
    let mut prev_was_value_opt = false;
    for arg in raw {
        // Anything after `--` is a program argument and must be preserved
        // verbatim, including bare `-Dfoo=bar` tokens that the Java program
        // (e.g. jboss-modules) wants to consume itself.
        if past_separator {
            filtered.push(arg);
            continue;
        }
        if arg == "--" {
            // Mark the separator boundary so subsequent `-D...` tokens are
            // preserved as program arguments. We still emit the `--` to
            // clap so it knows where program args begin (otherwise tokens
            // like `-mp` would be rejected as unknown short flags). The
            // `--` itself is stripped out of the program-args vector
            // after clap parsing, before the String[] is built for
            // Java's main().
            past_separator = true;
            prev_was_value_opt = false;
            filtered.push(arg);
            continue;
        }
        // This token is the value of a preceding value-taking option: emit it
        // unchanged even if it starts with `-D`, and do not treat it as an
        // option position.
        if prev_was_value_opt {
            prev_was_value_opt = false;
            filtered.push(arg);
            continue;
        }
        // Remember whether THIS token is a separate-token value-taking option,
        // so the next iteration knows the following token is its value.
        prev_was_value_opt = VALUE_TAKING_OPTS.contains(&arg.as_str());
        if let Some(kv) = arg.strip_prefix("-D") {
            if let Some((k, v)) = kv.split_once('=') {
                props.push((k.to_string(), v.to_string()));
            } else {
                // `-Dkey` with no value -> set to empty string (matches java behaviour)
                props.push((kv.to_string(), String::new()));
            }
        } else {
            filtered.push(arg);
        }
    }
    (filtered, props)
}

/// T6 CLI compat: HotSpot-style flags extracted from raw argv before clap
/// sees them.  clap's long-option format can't natively parse a `+`/`-`
/// sign inside the option name the way HotSpot's `-XX:+Foo` / `-XX:-Foo`
/// and `-agentlib:` spellings do.
///
/// Fields are populated by [`extract_hotspot_flags`] and consumed inside
/// `run()` to mutate `VmConfig` after clap returns.
#[derive(Debug, Default, Clone)]
struct HotspotFlags {
    /// `-XX:+HeapDumpOnOutOfMemoryError`. `-XX:-...` turns it off.
    heap_dump_on_oom: Option<bool>,
    /// `-XX:HeapDumpPath=<path>` companion.
    heap_dump_path: Option<String>,
    /// obsaudit D12 — `-XX:StartFlightRecording` (bare, `Some("")`) or
    /// `-XX:StartFlightRecording:opt=val,opt=val` (`Some("opt=val,...")`).
    /// Parsed into a `JfrStartRecordingConfig` by
    /// `parse_jfr_start_recording_opts` after clap returns.
    jfr_start_recording: Option<String>,
    /// `-agentlib:<spec>`, `-agentpath:<spec>`, `-javaagent:<spec>` — the
    /// entire token (including prefix) is preserved so the existing
    /// `AgentRegistry::parse_agent_option` can consume it verbatim.
    agent_options: Vec<String>,
}

/// Pull HotSpot-style flags out of raw argv.
///
/// This runs before clap so the bare `-XX:+Foo` / `-agentlib:` spellings
/// don't confuse its parser. Flags we recognize are removed from the
/// returned vector; anything we don't recognize passes through unchanged
/// so clap can still reject unknown flags with a useful error.
fn extract_hotspot_flags(raw: Vec<String>) -> (Vec<String>, HotspotFlags) {
    let mut filtered = Vec::with_capacity(raw.len());
    let mut out = HotspotFlags::default();
    let mut past_separator = false;
    for arg in raw {
        // Tokens after `--` are program arguments — pass through unchanged.
        if past_separator {
            filtered.push(arg);
            continue;
        }
        if arg == "--" {
            past_separator = true;
            filtered.push(arg);
            continue;
        }
        match arg.as_str() {
            // Boolean toggles: -XX:+Foo / -XX:-Foo
            "-XX:+HeapDumpOnOutOfMemoryError" => out.heap_dump_on_oom = Some(true),
            "-XX:-HeapDumpOnOutOfMemoryError" => out.heap_dump_on_oom = Some(false),
            // obsaudit D12 — bare form, no options.
            "-XX:StartFlightRecording" => out.jfr_start_recording = Some(String::new()),
            _ => {
                if let Some(rest) = arg.strip_prefix("-XX:HeapDumpPath=") {
                    out.heap_dump_path = Some(rest.to_string());
                } else if let Some(rest) = arg.strip_prefix("-XX:StartFlightRecording:") {
                    // obsaudit D12 — options form.
                    out.jfr_start_recording = Some(rest.to_string());
                } else if arg.starts_with("-agentlib:")
                    || arg.starts_with("-agentpath:")
                    || arg.starts_with("-javaagent:")
                {
                    out.agent_options.push(arg);
                } else {
                    filtered.push(arg);
                }
            }
        }
    }
    (filtered, out)
}

/// obsaudit D12 (2026-07-26) — parse `-XX:StartFlightRecording[:opts]`'s
/// comma-separated `key=value` options into a `JfrStartRecordingConfig`.
/// `raw` is `""` for the bare flag (all defaults).
///
/// Recognized keys: `filename`, `duration`, `maxage`, `maxevents`,
/// `dumponexit`. Unlike most of the rest of this parser (which silently
/// drops flags it doesn't specifically recognize so clap can report unknown
/// long options), an unrecognized key *inside* `StartFlightRecording:` is a
/// hard error — a typo'd sub-option here has no other layer that will ever
/// catch it, and JFR's whole failure mode in this audit is options that
/// silently do nothing.
/// Collect `+<EventName>#enabled=<bool>` tokens out of a
/// `-XX:StartFlightRecording:` option string.
///
/// B10 (2026-09-01). Split from [`parse_jfr_start_recording_opts`] rather than
/// folded into it because the two answer different questions about the same
/// string — that one builds a `JfrStartRecordingConfig`, this one names events
/// on `VmConfig` — and because the grammar itself belongs to
/// `cratonvm_vm::config::apply_jfr_event_setting`, which the jcmd `JFR.start`
/// surface calls too. One grammar, two callers; the spelling is HotSpot's own.
///
/// Naming an event **narrows** the recording to exactly the names given:
/// `RecordingSettings::enabled_event_names` is simultaneously the producer
/// arming set and the drain whitelist, so there is no way to say "arm this one
/// and keep everything else". `Recording.enable(...)` from Java behaves the
/// same way, so the two routes agree rather than diverging.
fn parse_jfr_event_settings(raw: &str) -> Result<Option<Vec<String>>, String> {
    let mut events: Option<Vec<String>> = None;
    for pair in raw.split(',') {
        let pair = pair.trim();
        if !pair.starts_with('+') {
            continue;
        }
        let (key, value) = pair.split_once('=').ok_or_else(|| {
            format!(
                "-XX:StartFlightRecording: event setting `{pair}` is missing `=value`                  (expected `+<EventName>#enabled=true`)"
            )
        })?;
        cratonvm_vm::config::apply_jfr_event_setting(&mut events, key, value)?;
    }
    Ok(events)
}

fn parse_jfr_start_recording_opts(
    raw: &str,
) -> Result<cratonvm_vm::config::JfrStartRecordingConfig, String> {
    let mut cfg = cratonvm_vm::config::JfrStartRecordingConfig {
        dump_on_exit: true, // HotSpot's default for -XX:StartFlightRecording
        ..Default::default()
    };
    if raw.is_empty() {
        return Ok(cfg);
    }
    for pair in raw.split(',') {
        let pair = pair.trim();
        if pair.is_empty() {
            continue;
        }
        let (key, value) = pair.split_once('=').ok_or_else(|| {
            format!("-XX:StartFlightRecording: option `{pair}` is missing `=value`")
        })?;
        // B10: an event setting, not a recording option. `apply_jfr_event_setting`
        // in `cratonvm_vm::config` owns the grammar; `parse_jfr_event_settings`
        // below makes the second pass that collects them. Skipping here rather
        // than erroring is what lets one comma-separated option string carry both
        // kinds, exactly as HotSpot's does.
        if key.starts_with('+') {
            continue;
        }
        match key {
            "filename" => cfg.filename = Some(value.to_string()),
            "duration" => cfg.duration = Some(parse_jfr_duration(value)?),
            "maxage" => cfg.max_age = Some(parse_jfr_duration(value)?),
            "maxevents" => {
                cfg.max_events = Some(value.parse::<usize>().map_err(|_| {
                    format!("-XX:StartFlightRecording: maxevents=`{value}` is not a number")
                })?);
            }
            "dumponexit" => {
                cfg.dump_on_exit = match value {
                    "true" => true,
                    "false" => false,
                    _ => {
                        return Err(format!(
                            "-XX:StartFlightRecording: dumponexit=`{value}` must be true or false"
                        ))
                    }
                };
            }
            other => {
                return Err(format!(
                    "-XX:StartFlightRecording: unrecognized option `{other}`                      (supported: filename, duration, maxage, maxevents, dumponexit, +<EventName>#enabled=true)"
                ))
            }
        }
    }
    Ok(cfg)
}

/// Parse a HotSpot-style duration: plain digits (seconds) or digits with a
/// trailing `s`/`m`/`h`/`d` unit suffix (seconds/minutes/hours/days).
fn parse_jfr_duration(value: &str) -> Result<std::time::Duration, String> {
    let (digits, mult) = match value.chars().last() {
        Some('s') => (&value[..value.len() - 1], 1u64),
        Some('m') => (&value[..value.len() - 1], 60),
        Some('h') => (&value[..value.len() - 1], 3600),
        Some('d') => (&value[..value.len() - 1], 86400),
        _ => (value, 1),
    };
    let secs: u64 = digits
        .parse()
        .map_err(|_| format!("`{value}` is not a valid duration (e.g. 30s, 5m, 1h)"))?;
    Ok(std::time::Duration::from_secs(secs.saturating_mul(mult)))
}

// ---------------------------------------------------------------------------
// JDK-mode selection and reporting
//
// CratonVM ships two complete, different standard-library implementations
// (real JDK bytecode vs ~5,200 synthetic Rust stubs). Which one ran decides
// which bug set applies, so:
//
//   * selection is explicit and deterministic — never inferred from what is
//     installed on the host (see `cratonvm_vm::config::JdkMode`);
//   * an unavailable mode is a hard launch error, never a silent downgrade;
//   * the active mode is printed by `-version` / `-Xinternalversion` and in
//     the launcher's fatal-error output, so every bug report carries it.
// ---------------------------------------------------------------------------

/// The mode this process actually booted in, published for the diagnostic
/// paths in `main()` (which run after `run()` has returned an error and no
/// longer have the `VmConfig`).
static ACTIVE_JDK_MODE: std::sync::OnceLock<(cratonvm_vm::config::JdkMode, Option<String>)> =
    std::sync::OnceLock::new();

/// One-line "which class library is this" summary for diagnostics.
fn active_jdk_mode_line() -> String {
    match ACTIVE_JDK_MODE.get() {
        Some((mode, Some(home))) => format!("jdk mode: {mode} (java.home={home})"),
        Some((mode, None)) => format!("jdk mode: {mode}"),
        // NOT "during argument parsing". This arm is reached whenever the
        // OnceLock is unset, and the class library is selected LATE: a
        // `--java-home` that clap accepted but that carries no `jmods/` or
        // `lib/modules` lands here too, and that is the common case (a POSIX
        // path spelling on Windows reaches it with rc=1). Naming a phase this
        // function cannot observe sent two separate triage records after the
        // argument parser for a fault that was never in it.
        None => "jdk mode: <not yet resolved — the failure occurred before the class library was selected>"
            .into(),
    }
}

/// Which flavour of version banner the user asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VersionQuery {
    /// `-version` (stderr, exits) / `--version` (stdout, exits).
    Version,
    /// `-fullversion` / `--full-version`: single terse line.
    Full,
    /// `-Xinternalversion`: build + JDK-mode diagnostics.
    Internal,
    /// `-showversion` / `--show-version`: print, then run the program.
    Show,
}

impl VersionQuery {
    /// HotSpot writes the single-dash forms to stderr and the GNU-style
    /// double-dash forms to stdout. Build tools that scrape `java -version`
    /// depend on the stderr side of that.
    fn to_stdout(self, token: &str) -> bool {
        let _ = self;
        token.starts_with("--")
    }

    fn exits(self) -> bool {
        !matches!(self, VersionQuery::Show)
    }
}

/// Scan the launcher section of argv (everything before the `--` that
/// [`insert_program_args_separator`] parks after the program selector) for a
/// version query. Returns the matched token as well so the caller can pick
/// the right output stream.
fn scan_version_query(argv: &[String]) -> Option<(VersionQuery, String)> {
    for a in argv.iter().skip(1) {
        if a == "--" {
            return None;
        }
        let q = match a.as_str() {
            "-version" | "--version" | "-v" | "-V" => VersionQuery::Version,
            "-fullversion" | "--full-version" => VersionQuery::Full,
            "-Xinternalversion" => VersionQuery::Internal,
            "-showversion" | "--show-version" => VersionQuery::Show,
            _ => continue,
        };
        return Some((q, a.clone()));
    }
    None
}

/// Remove the first occurrence of `token` from the launcher section of
/// argv (everything before the `--` separator). Program arguments after
/// `--` are never touched — a Java program is entitled to its own
/// `-showversion`.
fn remove_first_launcher_token(argv: &mut Vec<String>, token: &str) {
    for i in 1..argv.len() {
        if argv[i] == "--" {
            return;
        }
        if argv[i] == token {
            argv.remove(i);
            return;
        }
    }
}

/// Resolve the requested JDK mode from raw argv, for the version banner.
///
/// The authoritative resolution happens after clap parsing
/// ([`resolve_jdk_mode`]); this pre-parse scan exists only because the
/// banner must be printable before clap runs (clap's own `--version`
/// handling would otherwise exit first, printing a banner that says nothing
/// about which standard library is in play).
fn scan_requested_jdk_mode(argv: &[String]) -> cratonvm_vm::config::JdkMode {
    let mut mode = cratonvm_vm::config::LAUNCHER_DEFAULT_JDK_MODE;
    for a in argv.iter().skip(1) {
        if a == "--" {
            break;
        }
        match a.as_str() {
            "--synthetic-jdk" => mode = cratonvm_vm::config::JdkMode::Synthetic,
            // `--jdk-only` selects a *policy*, but it also requires the real
            // class library (contract §9: "--jdk-only implies JdkMode::Real").
            // The banner must say so, or `cratonvm --jdk-only -version` would
            // report whichever library the bare default names.
            "--real-jdk" | "--jdk-only" => mode = cratonvm_vm::config::JdkMode::Real,
            _ => {}
        }
    }
    mode
}

/// Resolve the requested compatibility mode from raw argv, for the version
/// banner. The counterpart of [`scan_requested_jdk_mode`], and equally
/// non-authoritative: [`resolve_compatibility_mode`] decides for the run.
///
/// A conflicting `--jdk-only --synthetic-jdk` pair is not diagnosed here — the
/// banner is a report, not a gate, and the real diagnosis (which names both
/// flags and the fix) happens in `run()`.
fn scan_requested_compatibility_mode(argv: &[String]) -> cratonvm_vm::config::CompatibilityMode {
    for a in argv.iter().skip(1) {
        if a == "--" {
            break;
        }
        if a == "--jdk-only" {
            return cratonvm_vm::config::CompatibilityMode::JdkOnly;
        }
    }
    cratonvm_vm::config::CompatibilityMode::Compatible
}

/// Pick up an explicit `--java-home <PATH>` / `--java-home=<PATH>` from raw
/// argv so the version banner reports the same JDK the run would use.
fn scan_explicit_java_home(argv: &[String]) -> Option<String> {
    let mut it = argv.iter().skip(1);
    while let Some(a) = it.next() {
        if a == "--" {
            return None;
        }
        if let Some(rest) = a.strip_prefix("--java-home=") {
            return Some(rest.to_string());
        }
        if a == "--java-home" {
            return it.next().cloned();
        }
    }
    None
}

/// Render the version banner, always naming the active class library.
///
/// This is the primary fix for "a bug report is uninterpretable without
/// knowing which mode ran": `cratonvm -version` now states it, so the mode
/// travels with every pasted terminal transcript.
fn version_banner(
    query: VersionQuery,
    mode: cratonvm_vm::config::JdkMode,
    compatibility: cratonvm_vm::config::CompatibilityMode,
    explicit_java_home: Option<&str>,
) -> String {
    use cratonvm_vm::config as cfg;
    let version = env!("CARGO_PKG_VERSION");

    if query == VersionQuery::Full {
        return format!(
            "cratonvm full version \"{version}\" ({mode}, {})\n",
            compatibility.as_str()
        );
    }

    let mut out = String::new();
    out.push_str(&format!("cratonvm version \"{version}\"\n"));
    // The `java -version` second line. HotSpot fills this parenthetical from
    // `java.vm.info`, so the execution-mode list here is the one
    // `cratonvm_vm::vm::vm_info_mode_list` will put in that property — the same
    // function, so the banner and the property cannot spell the policy two
    // different ways (they used to: `mixed mode, jdk-only` vs
    // `compatibility=jdk-only`).
    //
    // The banner then adds `compatibility=<mode>` on top, which the property
    // deliberately does NOT carry. Two reasons it belongs here and not there:
    // a banner is grepped by scripts that must not have to parse a comma list,
    // and `-version` prints before any VM exists, so this line cannot read the
    // property it has to agree with. Machine readers grep for
    // `compatibility=jdk-only`. Under `Compatible` the whole line is
    // byte-for-byte what it has always been (jdk-only-mode.md §10).
    out.push_str(&format!(
        "CratonVM (build {version}, {}, sharing, compatibility={})\n",
        cratonvm_vm::vm::vm_info_mode_list(compatibility),
        compatibility.as_str()
    ));
    out.push_str(&format!(
        "JDK class library: {mode} — {}\n",
        mode.describe()
    ));

    match mode {
        cfg::JdkMode::Real => match cfg::require_real_jdk(explicit_java_home) {
            Ok(home) => out.push_str(&format!("JDK class library root: {}\n", home.display())),
            Err(_) => {
                out.push_str(
                    "JDK class library root: NONE FOUND — a program launch in this mode \
                     will fail (run with --real-jdk for the full diagnostic, or pass \
                     --synthetic-jdk to select the other library)\n",
                );
            }
        },
        cfg::JdkMode::Synthetic => {
            if !cfg::SYNTHETIC_JDK_COMPILED_IN {
                out.push_str(
                    "JDK class library root: NOT COMPILED IN — this binary was built \
                     without the `synthetic-jdk` Cargo feature, so a program launch in \
                     this mode will fail\n",
                );
            }
        }
    }

    if query == VersionQuery::Internal {
        out.push('\n');
        out.push_str(&format!("jdk.mode.active                = {mode}\n"));
        out.push_str(&format!(
            "jdk.compatibility.mode          = {}\n",
            compatibility.as_str()
        ));
        out.push_str(&format!(
            "jdk.mode.default.launcher       = {}\n",
            cfg::LAUNCHER_DEFAULT_JDK_MODE
        ));
        out.push_str(&format!(
            "jdk.mode.default.embedded       = {}\n",
            cfg::EMBEDDED_DEFAULT_JDK_MODE
        ));
        out.push_str(
            "jdk.mode.selection              = explicit flag or fixed default \
             (never host-detected)\n",
        );
        out.push_str(&format!(
            "jdk.mode.synthetic_compiled_in  = {}\n",
            cfg::SYNTHETIC_JDK_COMPILED_IN
        ));
        out.push_str("jdk.search:\n");
        out.push_str(&cfg::describe_jdk_search(explicit_java_home));
        out.push('\n');
    }
    out
}

/// Authoritative JDK-mode resolution + availability validation.
///
/// Returns the selected mode and, in real-JDK mode, the validated
/// `JAVA_HOME` root. An unavailable mode is an error — the launcher does
/// **not** fall back to the other class library, because a run whose
/// standard library was chosen by the host is neither reproducible nor
/// reportable.
fn resolve_jdk_mode(
    synthetic_flag: bool,
    real_flag: bool,
    explicit_java_home: Option<&str>,
) -> Result<(cratonvm_vm::config::JdkMode, Option<std::path::PathBuf>)> {
    use cratonvm_vm::config as cfg;

    // clap enforces this via `conflicts_with`; keep the check so a future
    // argv-preprocessing change can't quietly make one flag win.
    if synthetic_flag && real_flag {
        bail!(
            "--synthetic-jdk and --real-jdk are mutually exclusive: they select \
             two different standard-library implementations. Pass exactly one \
             (or neither, for the default {}).",
            cfg::LAUNCHER_DEFAULT_JDK_MODE
        );
    }

    let mode = if synthetic_flag {
        cfg::JdkMode::Synthetic
    } else if real_flag {
        cfg::JdkMode::Real
    } else {
        cfg::LAUNCHER_DEFAULT_JDK_MODE
    };

    match mode {
        cfg::JdkMode::Synthetic => {
            cfg::require_synthetic_jdk().map_err(|e| anyhow::anyhow!("{e}"))?;
            Ok((mode, None))
        }
        cfg::JdkMode::Real => {
            let home =
                cfg::require_real_jdk(explicit_java_home).map_err(|e| anyhow::anyhow!("{e}"))?;
            Ok((mode, Some(home)))
        }
    }
}

// ===========================================================================
// JDK-only mode (docs/feature-designs/jdk-only-mode.md §9).
//
// The launcher owns three things for this feature: resolving the policy flag
// into a `CompatibilityMode`, draining the violation logs for `--trace-jdk-only`
// / `--explain-jdk-only`, and writing the three census artefacts that
// `difftest`'s `census.rs` parses. Everything below is hand-rolled JSON in the
// style of `SharedVm::dump_missing_natives_json`, sorted so the files are
// diff-stable against a committed baseline.
// ===========================================================================

/// Authoritative compatibility-policy resolution.
///
/// Runs **before** [`resolve_jdk_mode`] deliberately. Both flag pairs are
/// mutually exclusive, but they are exclusive for different reasons, and the
/// user deserves the specific one: `--jdk-only --synthetic-jdk` is a *policy*
/// conflict (the synthetic library is made of exactly the substitutions
/// `--jdk-only` forbids), whereas `--real-jdk --synthetic-jdk` is a *library*
/// conflict. Resolving the policy first means the policy diagnosis wins;
/// resolving the library first would report "two different standard-library
/// implementations", which is true but unhelpful.
fn resolve_compatibility_mode(
    jdk_only: bool,
    synthetic_flag: bool,
) -> Result<cratonvm_vm::config::CompatibilityMode> {
    use cratonvm_vm::config::CompatibilityMode;

    // clap enforces this via `conflicts_with`; keep the check so a future
    // argv-preprocessing change can't quietly make one flag win.
    if jdk_only && synthetic_flag {
        bail!(
            "--jdk-only and --synthetic-jdk cannot be combined.\n\
             \n\
             --jdk-only means real JDK class bytes are authoritative: no \
             fabricated compatibility class and no synthetic-stub native may be \
             registered or invoked. The synthetic class library is ~5,200 such \
             stubs, so the combination selects a VM with no usable class \
             library.\n\
             \n\
             Pass --jdk-only against a real JDK (--java-home <PATH>), or drop \
             --jdk-only and keep --synthetic-jdk for the standalone library."
        );
    }

    Ok(if jdk_only {
        CompatibilityMode::JdkOnly
    } else {
        CompatibilityMode::Compatible
    })
}

/// Contract §9: `CRATONVM_REAL=-stubs` keeps working as a native-registry
/// filter, but it cannot express the class-loading or dispatch half of the
/// policy — it drops synthetic-stub *registrations* and says nothing about
/// fabricated classes or about a stub shadowing real bytecode. A run that asked
/// for it almost certainly wanted `--jdk-only`, so say so once.
///
/// Both spellings are checked: the expanded per-knob key (which
/// `flag_groups::expand_process_env` writes back into the environment during
/// `main()`) and the raw grouped token, so the note still fires if the
/// expansion order ever changes or the variable is set after expansion.
fn note_no_stubs_env_without_jdk_only(jdk_only: bool) {
    if jdk_only {
        return;
    }
    let expanded = std::env::var_os("CRATONVM_NO_STUBS").is_some();
    let raw_token = std::env::var("CRATONVM_REAL")
        .map(|v| {
            v.split(',')
                .any(|t| matches!(t.trim(), "-stubs" | "no-stubs" | "stubs=off"))
        })
        .unwrap_or(false);
    if !(expanded || raw_token) {
        return;
    }
    eprintln!(
        "[cratonvm] CRATONVM_REAL=-stubs drops synthetic-stub native \
         registrations only. It cannot reject a fabricated compatibility class, \
         and it cannot stop a registered native from shadowing real bytecode. \
         For the full policy pass --jdk-only (add --jdk-only-report <FILE> to \
         census what it would reject)."
    );
}

/// The JDK feature version of the runtime image, read from
/// `$JAVA_HOME/release`'s `JAVA_VERSION=` line.
///
/// Thin adapter over `cratonvm_vm::vm::jdk_feature_from_release_file`, which is
/// the same function `SharedVm::jdk_feature_version` uses to fill the report's
/// `jdk_feature`. It lives in `vm` so a `--trace-jdk-only` line and the report
/// written at the end of the same run cannot disagree about which JDK they are
/// talking about. `None` rather than a guess: a fabricated number is worse than
/// an absent one — `null` reads as "not measured", a wrong `25` reads as a fact.
fn detect_jdk_feature(java_home: Option<&str>) -> Option<u32> {
    cratonvm_vm::vm::jdk_feature_from_release_file(java_home?)
}

// JDK-ONLY CENSUS WRITERS: there are none here any more, on purpose.
//
// This file used to carry its own schema-2 native census, class-origin census
// and JDK-only report, written independently of the ones in
// `vm/src/vm/vm_init.rs`. Two writers stamping the same `schema_version` with
// two different shapes is a consumer hazard, not a redundancy: a reader that
// works against one silently mis-reads the other. The `vm` writers survived
// (they can fill `real_declaring_method` from already-loaded classes without
// perturbing the census, which the launcher could not), and
// `write_jdk_only_dumps` below now calls them. The launcher's contributions —
// path redaction under `--explain-jdk-only`, the `(kind, summary)` violation
// sort, the `OriginBuckets` partition — moved with them.
//
// The JSON helpers (`json_escape`, `json_string`, `json_opt_string`) and the
// path redaction went the same way. `redact_absolute_paths` is imported back
// from `cratonvm_vm::vm` for the `--trace-jdk-only` lines below, so the trace
// and the artefacts cannot disagree about what counts as a private path.

/// How far each append-only violation log has already been reported.
///
/// **Class-origin violations no longer come through here.** They were the one
/// recording site that fires throughout the run, so polling made every mid-run
/// fabrication surface at shutdown, detached from the code that caused it —
/// contract §9 asks for the opposite. `ClassManager` now carries a VM-scoped
/// `set_violation_sink` that `run()` installs immediately after the `vm-init`
/// drain, and this drain skips their
/// *rendering* (but still advances `origins`) once that sink exists, so each
/// violation is printed exactly once, live.
///
/// The remaining four sites are still polled, and for each of them that is the
/// right answer rather than a deferral:
///
/// * `NativeMethodRegistry::refused_registrations` — every refusal happens
///   inside `Vm::new`, and the `vm-init` drain immediately follows it, so the
///   poll already reports them at their real time of occurrence.
/// * The three process sinks behind `SharedVm::jdk_only_process_violations` —
///   these are JIT/dispatch refusals, and they are *process*-global (retired
///   record: feature-designs/jdk-only-wave2/
///   additional-wave2-markers-not-in-the-original-inventory.md §2). Note that
///   §2's own subject — the JIT compatibility latch — is no longer one of
///   them; what remains process-global here is the violation SINKS, not the
///   policy. Giving them a live sink means giving them a VM first; a per-VM sink
///   hung off process-global state would report another VM's violations as
///   this one's, which is worse than reporting them late.
///
/// The three JIT/dispatch sinks are drained on **the same schedule** as the
/// other two, so a traced run and `--jdk-only-report` name the same set of
/// violations. In practice their `vm-init` drain is almost always empty and
/// everything lands at `shutdown`: nothing has been compiled or dispatched yet
/// when `Vm::new` returns. That is a property of *when refusals happen*, not a
/// limitation of the drain — no sink here is shutdown-only, and a caller that
/// added a mid-run drain point would get the entries recorded so far.
///
/// Each sink is bounded (256 entries) and internally deduplicated, and none of
/// them ever shrinks, so a positional watermark stays valid across drains: once
/// a sink saturates, `len()` stops growing and the drain correctly reports
/// nothing new.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct ViolationWatermark {
    origins: usize,
    registrations: usize,
    /// One watermark per slot of [`cratonvm_vm::vm::JDK_ONLY_PROCESS_SINKS`],
    /// in that constant's documented order. Indexed positionally on purpose:
    /// the array's order is a contract precisely so this stays a plain index.
    process_sinks: [usize; cratonvm_vm::vm::JDK_ONLY_PROCESS_SINKS],
    /// Refusal-event totals as of the last drain, so the summary line reports a
    /// delta rather than a running total that looks like a per-phase count.
    refusals: cratonvm_vm::vm::JdkOnlyRefusalCounts,
}

/// One trace line for a violation.
///
/// `--explain-jdk-only` selects the long-form operator report (which the
/// contract requires to end with the `--real-jdk` fallback hint) and keeps
/// absolute paths; otherwise the one-line summary, redacted.
fn render_violation(
    violation: &cratonvm_types::error::JdkOnlyViolation,
    jdk_feature: Option<u32>,
    explain: bool,
) -> String {
    if explain {
        violation.render(jdk_feature, true)
    } else {
        // The same redaction the census writers apply, from the same function
        // in `vm`: a traced violation and the report row for that violation
        // must not disagree about what counts as a private path.
        cratonvm_vm::vm::redact_absolute_paths(&violation.summary())
    }
}

/// Print every violation recorded since the last drain.
fn trace_jdk_only_violations(
    shared: &cratonvm_vm::SharedVm,
    watermark: &mut ViolationWatermark,
    phase: &str,
    explain: bool,
) {
    let jdk_feature = detect_jdk_feature(shared.config.java_home.as_deref());
    let mut lines: Vec<String> = Vec::new();
    {
        let class_manager = shared.classes.class_manager.read();
        let recorded = class_manager.origin_violations();
        // Skip the rendering — not the watermark — once the live sink is
        // installed: every violation past that point has already been printed
        // at the instant it was recorded, and printing it again here would
        // double-report it. Before the sink exists (the `vm-init` drain, and
        // any run without `--trace-jdk-only`) this is still the only reporter.
        if !class_manager.has_violation_sink() {
            for violation in recorded.iter().skip(watermark.origins) {
                lines.push(render_violation(violation, jdk_feature, explain));
            }
        }
        watermark.origins = recorded.len();
    }
    {
        let refused = shared.natives.native_methods.refused_registrations();
        for violation in refused.iter().skip(watermark.registrations) {
            lines.push(render_violation(violation, jdk_feature, explain));
        }
        watermark.registrations = refused.len();
    }
    // The three process-global sinks the report also folds — JIT compile-time
    // refusals, JIT fast-path refusals, interpreter bytecode-wins observations.
    // Drained here on the same schedule as the two above so a traced run and
    // `--jdk-only-report` cannot disagree about which violations occurred.
    //
    // `jdk_only_process_violations()` returns three empty vectors in
    // `Compatible` mode without touching the sinks, so this loop costs a mode
    // test and three empty iterations on a default `--real-jdk` run.
    let process_sinks = shared.jdk_only_process_violations();
    for (slot, sink) in process_sinks.iter().enumerate() {
        for violation in sink.iter().skip(watermark.process_sinks[slot]) {
            lines.push(render_violation(violation, jdk_feature, explain));
        }
        watermark.process_sinks[slot] = sink.len();
    }
    for line in lines {
        eprintln!("[cratonvm][jdk-only:{phase}] {line}");
    }
    // Refusal counters, as a delta since the last drain. These are *events*,
    // not violations: they are uncapped and undeduplicated, whereas the lines
    // above are one-per-distinct-triple and capped at 256 per sink. Printing
    // them per-event would bury the identities under repetition, and printing
    // nothing would make a traced run silently disagree with the `refusals`
    // block of the report. One summary line per drain is the compromise.
    //
    // `jit_inline_cache_natives` has no violation object behind it at all — the
    // inline-cache publication site has an entry address, not a name triple —
    // so this line is the only place a traced run can see that refusal class.
    let refusals = shared.jdk_only_refusal_counts();
    if refusals != watermark.refusals {
        eprintln!(
            "[cratonvm][jdk-only:{phase}] refusals since last drain: \
             jit-direct-native-binds={} jit-inline-cache-natives={} \
             jit-fastpath-admissions={} interpreter-bytecode-preferred={} \
             interpreter-shadow-unenforced={}",
            refusals
                .jit_direct_native_binds
                .saturating_sub(watermark.refusals.jit_direct_native_binds),
            refusals
                .jit_inline_cache_natives
                .saturating_sub(watermark.refusals.jit_inline_cache_natives),
            refusals
                .jit_fastpath_admissions
                .saturating_sub(watermark.refusals.jit_fastpath_admissions),
            refusals
                .interpreter_bytecode_preferred
                .saturating_sub(watermark.refusals.interpreter_bytecode_preferred),
            // The one term on this line that is not a refusal: a `Bridge` that
            // shadowed real bytes and ran. It rides here because a traced run
            // that reports only what strict policy STOPPED reads as if nothing
            // else happened.
            refusals
                .interpreter_shadow_unenforced
                .saturating_sub(watermark.refusals.interpreter_shadow_unenforced),
        );
    }
    watermark.refusals = refusals;
}

/// Write whichever of the three census artefacts were requested.
///
/// Called from every exit path that has a live VM, including the failing ones:
/// `difftest` categorises a *failing* strict run from these files, so a run that
/// dies on the violation it was launched to find must still leave the census
/// behind. Writes at most once per process (first caller wins — the failure
/// path is the informative one).
/// The absolute path a dump flag's operand actually names, for printing.
///
/// # Why every dump message goes through this
///
/// A dump flag's operand is trusted verbatim and resolved by the OS, and on
/// Windows a POSIX-looking path is neither rejected nor mapped: `/tmp/reg.json`
/// resolves against the current drive to `C:\tmp\reg.json`. The write then
/// succeeds and the caller — typically a Git Bash shell, where `/tmp` means
/// something else entirely — goes looking in the wrong place and reads the
/// absence of the file as the flag having failed. Printing the resolved
/// absolute path turns that into a one-glance answer, on the success line as
/// well as the failure line, because the success case is the one that misleads.
///
/// Falls back to the operand as given if the path cannot be made absolute
/// (empty operand, or a platform error): a diagnostic must never be the thing
/// that fails.
fn absolute_dump_path(path: &str) -> String {
    std::path::absolute(path)
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| path.to_string())
}

/// Why a dump write failed, in the terms that let a caller fix it: the absolute
/// path attempted, and whether the directory it would have gone in exists.
///
/// The OS error alone is not enough. `os error 3` — and its localised text,
/// which on this host is not English — says "the system cannot find the path",
/// which is true of a missing parent directory, a typo'd drive letter and a
/// POSIX path alike. Naming the parent and saying whether it exists separates
/// those three in one line.
fn describe_dump_failure(path: &str, error: &impl std::fmt::Display) -> String {
    let absolute = absolute_dump_path(path);
    let parent = std::path::Path::new(&absolute)
        .parent()
        .map(|p| p.display().to_string());
    match parent {
        Some(dir) if !std::path::Path::new(&dir).is_dir() => format!(
            "{error} (tried to write {absolute}; its directory {dir} does not exist — \
             create it, or pass a path under an existing directory)"
        ),
        Some(dir) => format!("{error} (tried to write {absolute}; its directory {dir} exists)"),
        None => format!("{error} (tried to write {absolute})"),
    }
}

/// The standing caveat on the census's `invocations` column, printed with every
/// successful `--dump-native-registry`.
///
/// MEASURED 2026-08-17 and recorded in
/// `docs/known-issues/jdk-only/G33-1-the-instrument-that-under-reported-20260817.md`:
/// the column counts dispatches that resolved the triple by name or id, and
/// misses every dispatch served from a pre-resolved function pointer — the
/// interpreter's intrinsic table and the JIT's thin direct-call helpers. 100,000
/// `Math.abs` calls report 1; the same run under `CRATONVM_DISABLE_INTRINSICS=1`
/// reports 100,000.
///
/// It is printed on the *success* line, next to the number, because that is
/// where a reader is standing when they decide what the column means. A caveat
/// that lives only in `--help` or only in a design doc is a caveat that gets
/// quoted around; a dozen records in `docs/known-issues/jdk-only/` already quote
/// this tool's output.
fn census_invocations_caveat(nojit: bool, intrinsics_disabled: bool) -> &'static str {
    if nojit && intrinsics_disabled {
        // Both bypass families are off, so the column is a total for
        // everything measured. Say so — a lane that went to the trouble of
        // configuring an exact census should be told it got one.
        "; `invocations` is an exact count in this configuration \
         (--nojit + CRATONVM_DISABLE_INTRINSICS=1)"
    } else {
        "; `invocations` is a LOWER BOUND, not a call count — intrinsic-table and \
         JIT direct-call dispatches are not counted. For an exact census re-run with \
         --nojit and CRATONVM_DISABLE_INTRINSICS=1 (see G33-1)"
    }
}

fn write_jdk_only_dumps(args: &Args, shared: &cratonvm_vm::SharedVm) {
    if args.dump_class_origins.is_none()
        && args.dump_native_registry.is_none()
        && args.jdk_only_report.is_none()
    {
        return;
    }
    if jdk_only_dumps_written() {
        return;
    }

    // `--explain-jdk-only` is the one flag that turns redaction OFF. Every
    // writer below takes it explicitly and redacts when it is false, which is
    // the default; there is no path that reaches a writer without deciding.
    let verbose = args.explain_jdk_only;
    let mode = shared.config.compatibility_mode;

    // Each writer does its own locking and its own row snapshotting, inside
    // `vm`, where the registry and the class manager live. The launcher used to
    // copy the rows out here into local mirror types; that mirror is what
    // allowed the two implementations to drift, and copying twice bought
    // nothing — the writers already release every borrow before touching the
    // filesystem, and the report takes ONE class-manager acquisition for both
    // the census and the violation list so the two describe the same instant.
    if let Some(path) = &args.dump_class_origins {
        match shared.dump_class_origins_json(path, verbose) {
            Ok(n) => eprintln!(
                "[cratonvm] wrote {n} class-origin rows to {}",
                absolute_dump_path(path)
            ),
            Err(e) => eprintln!(
                "[cratonvm] warning: could not write class-origin census: {}",
                describe_dump_failure(path, &e)
            ),
        }
    }

    if let Some(path) = &args.dump_native_registry {
        match shared.dump_native_census_json(path, verbose) {
            // `schema 4`, matching the `"schema_version": 4` the writer in
            // `cratonvm_vm::vm::vm_init` actually emits and the number
            // `--help` documents. This line said `schema 3` until 2026-08-17,
            // which is a bad way for the instrument whose job is to be
            // believed to introduce itself. If the writer's version moves
            // again, this literal is the second place to change.
            Ok((intrinsic, bridge, stub)) => eprintln!(
                "[cratonvm] wrote native registry census (schema 4{}) to {} \
                 (intrinsic={intrinsic}, bridge={bridge}, synthetic-stub={stub}){}",
                if verbose {
                    ", image-adjudicated"
                } else {
                    ", no image adjudication — pass --explain-jdk-only"
                },
                absolute_dump_path(path),
                census_invocations_caveat(
                    cratonvm_types::flags::runtime_var("CRATONVM_DISABLE_JIT")
                        .is_ok_and(|v| !v.is_empty() && v != "0"),
                    cratonvm_types::flags::runtime_var("CRATONVM_DISABLE_INTRINSICS")
                        .is_ok_and(|v| !v.is_empty() && v != "0"),
                ),
            ),
            Err(e) => eprintln!(
                "[cratonvm] warning: could not write native registry JSON: {}",
                describe_dump_failure(path, &e)
            ),
        }
    }

    if let Some(path) = &args.jdk_only_report {
        match shared.dump_jdk_only_report_json(path, verbose) {
            // `violations` is now the union of all five recording sites, so
            // this number moved: it used to count refused registrations plus
            // compatibility-class requests only. The refusal-event total is
            // reported alongside it rather than folded in — they are different
            // units (distinct methods versus events) and adding them would
            // produce a number that means nothing.
            Ok((violations, compatibility_classes)) => {
                eprintln!(
                    "[cratonvm] wrote {} JDK-only report to {} ({violations} violation(s), \
                     {compatibility_classes} compatibility class(es), {} refusal event(s))",
                    mode.as_str(),
                    absolute_dump_path(path),
                    shared.jdk_only_refusal_counts().total(),
                );
                // The line above is the one number most readers stop at, and
                // `{violations}` is a floor whenever the observation sink filled
                // up. Saying so HERE, and not only in the file's
                // `observation_sink` object, is the difference between a caveat a
                // reader has to go looking for and one they cannot miss: the
                // whole failure mode is that a truncated list reads as a
                // complete one.
                if cratonvm_vm::vm::jdk_only_native_shadow_sink_saturated() {
                    // The drop count is what makes this line actionable rather
                    // than merely alarming: it says HOW SHORT the list is, and
                    // it names the knob that fixes it. Before 2026-08-20 the
                    // only advice available here was "narrow the workload",
                    // which is the opposite of what a census run wants and is
                    // how every shadow figure in docs/known-issues/jdk-only/
                    // came to be a floor.
                    let dropped = cratonvm_vm::vm::jdk_only_native_shadow_sink_dropped();
                    let recorded = cratonvm_vm::vm::jdk_only_native_shadow_sink_len();
                    eprintln!(
                        "[cratonvm] warning: the JDK-only observation sink SATURATED at {} \
                         distinct rows and dropped ~{dropped} more — violations[] names \
                         {recorded} of roughly {} shadows, so the rows are a FLOOR and the \
                         TOTAL is the sum. Re-run with \
                         CRATONVM_NATIVE_SHADOW_SINK_CAP={} to name all of them; do NOT \
                         narrow the workload, that is what censors the census.",
                        cratonvm_vm::vm::jdk_only_native_shadow_cap(),
                        recorded as u64 + dropped,
                        // CLAMPED to the sinks' own ceiling (WORKER-5 NOTE-6
                        // N3). Unclamped this advised `(recorded+dropped)*2`,
                        // which exceeds 65,536 for any workload with more than
                        // ~32,768 shadows — so the VM could print a value it
                        // then adjusts. Since 2026-08-22 an over-ceiling value
                        // clamps LOUDLY rather than silently reverting to the
                        // default, but advising a number the run will change is
                        // still worse than advising the right one.
                        (recorded as u64 + dropped)
                            .saturating_mul(2)
                            .max(8192)
                            .min(65_536),
                    );
                }
                // The JIT fast-path sink is a SECOND bounded collection feeding
                // the same `violations[]`, and until 2026-08-20 it had no
                // saturation signal anywhere — not in the file and not on this
                // line. A reader who saw no warning concluded the list was
                // complete; it was complete for one source of three.
                if cratonvm_vm::jit::helpers::jdk_only_jit_helper_sink_saturated() {
                    eprintln!(
                        "[cratonvm] warning: the JDK-only JIT fast-path violation sink \
                         SATURATED at {} rows and dropped {} refusal(s) it could not name — \
                         observation_sink.jit_fastpath in the report carries the same two \
                         numbers. Same remedy: raise CRATONVM_NATIVE_SHADOW_SINK_CAP.",
                        cratonvm_vm::jit::helpers::jdk_only_jit_helper_violation_cap(),
                        cratonvm_vm::jit::helpers::jdk_only_jit_helper_sink_dropped(),
                    );
                }
                // The THIRD bounded collection. It had no counter at all until
                // 2026-08-22, so it could not warn here and rendered `null` in
                // the file — which `run.sh` read as "nothing truncated", on
                // every strict run there has ever been. All three now answer
                // the same two questions.
                if cratonvm_jit::jdk_only_jit_sink_saturated() {
                    eprintln!(
                        "[cratonvm] warning: the JDK-only JIT COMPILE-TIME violation sink \
                         SATURATED at {} rows and dropped {} refusal(s) it could not name — \
                         observation_sink.jit_compile in the report carries the same two \
                         numbers. Same remedy: raise CRATONVM_NATIVE_SHADOW_SINK_CAP.",
                        cratonvm_jit::jdk_only_violation_cap(),
                        cratonvm_jit::jdk_only_jit_sink_dropped(),
                    );
                }
            }
            Err(e) => eprintln!(
                "[cratonvm] warning: could not write JDK-only report: {}",
                describe_dump_failure(path, &e)
            ),
        }
    }
}

/// Claim the right to write the census artefacts; `true` means someone already
/// has.
///
/// Shared by `write_jdk_only_dumps` (the four unwinding exit paths) and
/// `write_jdk_only_dumps_on_exit` (the `System.exit` path), so the two cannot
/// interleave two writes to the same file. First caller wins on purpose — on a
/// failing run that is the failure path, which is the informative one.
fn jdk_only_dumps_written() -> bool {
    static WRITTEN: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    WRITTEN.swap(true, std::sync::atomic::Ordering::AcqRel)
}

/// The `--dump-*` / `--jdk-only-report` paths, readable from the pre-exit hook.
///
/// The hook is a bare `fn(i32)` installed at the top of `run()`, before `Args`
/// is parsed, so it cannot capture them. This cell is published as soon as they
/// are known and read back on the `System.exit` path.
///
/// **This is launcher state, not VM state.** Contract §2's ban on process
/// globals is about the feature's *per-VM* state — a policy or a violation sink
/// that two VMs in one process could confuse. These are the command line of the
/// one process that parsed them, and they sit alongside the equally
/// process-global `PRE_EXIT_HOOK` and `write_jdk_only_dumps`' `WRITTEN` latch
/// that they exist to cooperate with.
static JDK_ONLY_EXIT_DUMP_PATHS: std::sync::OnceLock<JdkOnlyExitDumpPaths> =
    std::sync::OnceLock::new();

#[derive(Debug, Clone)]
struct JdkOnlyExitDumpPaths {
    class_origins: Option<String>,
    native_registry: Option<String>,
    report: Option<String>,
    verbose: bool,
}

/// Write the census from the `System.exit` / `Runtime.exit` path.
///
/// `System.exit(N)` never unwinds, so none of `finish_jdk_only`'s four callers
/// runs. Without this a `--jdk-only --jdk-only-report r.json` run of a program
/// whose error handler exits produced no report at all — and those are the runs
/// the report exists for. See
/// `jdk-only-system-exit-census-FIXED-20260804.md`.
///
/// Shares `write_jdk_only_dumps`' `WRITTEN` latch, so a `System.exit` racing a
/// normal shutdown cannot produce two interleaved writes to the same path;
/// first caller wins, and on this path the first caller is the informative one.
///
/// Never blocks: the VM-side writer uses a non-blocking class-manager
/// acquisition and falls back to labelled partial artefacts. See
/// `SharedVm::try_write_jdk_only_dumps_for_exit` for why a timeout-and-retry
/// would be the wrong shape here.
fn write_jdk_only_dumps_on_exit() {
    let Some(paths) = JDK_ONLY_EXIT_DUMP_PATHS.get() else {
        return;
    };
    if paths.class_origins.is_none() && paths.native_registry.is_none() && paths.report.is_none() {
        return;
    }
    if jdk_only_dumps_written() {
        return;
    }
    let Some(shared) = cratonvm_vm::native::jni::process_vm() else {
        return;
    };
    let (wrote, partial) = shared.try_write_jdk_only_dumps_for_exit(
        paths.class_origins.as_deref(),
        paths.native_registry.as_deref(),
        paths.report.as_deref(),
        paths.verbose,
    );
    if wrote {
        if partial {
            eprintln!(
                "[cratonvm] wrote PARTIAL JDK-only census on System.exit: the class-manager \
                 lock was held, so class-origin rows are absent. Each file records this in its \
                 \"partial_reason\"."
            );
        } else {
            eprintln!("[cratonvm] wrote JDK-only census on System.exit");
        }
    }
}

/// Shutdown half of the JDK-only surface: drain the violation logs one last
/// time, then write the census files.
///
/// Called from the clean shutdown path **and** from each failing path that
/// still has a VM, so a strict-mode failure is categorisable.
fn finish_jdk_only(
    args: &Args,
    shared: &cratonvm_vm::SharedVm,
    watermark: &mut ViolationWatermark,
) {
    if args.trace_jdk_only || args.explain_jdk_only {
        trace_jdk_only_violations(shared, watermark, "shutdown", args.explain_jdk_only);
    }
    write_jdk_only_dumps(args, shared);
}

fn resolve_watchdog_timeout(
    stack_dump_on_timeout: Option<u64>,
    default_watchdog_sec: Option<&str>,
) -> Option<u64> {
    match stack_dump_on_timeout {
        Some(secs) if secs > 0 => Some(secs),
        Some(_) => None,
        None => default_watchdog_sec
            .and_then(|s| s.trim().parse::<u64>().ok())
            .filter(|secs| *secs > 0),
    }
}

fn launcher_nojit_requested(argv: &[String]) -> bool {
    argv.iter()
        .take_while(|arg| arg.as_str() != "--")
        .any(|arg| arg == "--nojit")
}

/// Which assertion scope an argv token switches, and to what.
///
/// `Some((system, enable))` for the four **unscoped** spellings; `None` for
/// everything else, including the scoped forms (`-ea:pkg...`, `-da:Class`) —
/// see [`launcher_assertions_requested`].
fn assertion_flag_scope(arg: &str) -> Option<(bool, bool)> {
    match arg {
        "-ea" | "-enableassertions" => Some((false, true)),
        "-da" | "-disableassertions" => Some((false, false)),
        "-esa" | "-enablesystemassertions" => Some((true, true)),
        "-dsa" | "-disablesystemassertions" => Some((true, false)),
        _ => None,
    }
}

/// The JVM-wide assertion status requested on the command line, or `None` when
/// no unscoped assertion flag was passed.
///
/// `Class.desiredAssertionStatus()` decides whether a class's `<clinit>` stores
/// `$assertionsDisabled = false`, i.e. whether real `assert` bytecode throws.
/// CratonVM has always implemented it — `assertion_status_default()` in
/// `native-builtins` — but the only way to reach the switch was
/// `CRATONVM_ENABLE_ASSERTIONS`, which no Maven Surefire or Gradle fork will
/// ever set. Surefire forks the test JVM with `-ea` by default, so every
/// `assert`-based validation test in the corpus silently did nothing. The
/// recorded case is netty's HTTP/2 flow-controller classes, where HotSpot goes
/// 34/34 with `-ea` and CratonVM stayed at 28/34 — see the retired
/// `ea-flag-ignored-so-assert-never-fires-20260812` write-up.
///
/// **Why it is scanned here and not inside `normalize_java_launcher_argv`.**
/// That function is pure and has ~60 unit tests; `CRATONVM_ENABLE_ASSERTIONS` is
/// a declared flag served from the immutable snapshot `install_flags` latches at
/// the top of `main`. A `set_var` from inside the normaliser would be invisible
/// to the VM (the snapshot is already taken by the time `run()` normalises) and
/// would make those tests order-dependent. Injecting a launcher override is the
/// supported route and the one `--nojit` and `--dump-phase-report` already use.
///
/// **The two scopes are tracked separately, then OR-ed.** HotSpot's `-ea` and
/// `-esa` are independent switches (user classes vs. bootclasspath classes);
/// CratonVM has one global. Reducing the command line by plain last-wins would
/// make `-ea -dsa` resolve to *off* and quietly undo the `-ea` a build tool put
/// there on purpose. Last-wins **within** each scope and OR **across** them
/// keeps every combination that asks for assertions anywhere answering "on".
/// The residual over-breadth is a lone `-esa`, which turns them on for user
/// classes too; that is the direction that fails loudly rather than silently.
///
/// Scoped forms are skipped entirely — see the comment on the strip arm in
/// `normalize_java_launcher_argv`.
fn launcher_assertions_requested(argv: &[String]) -> Option<bool> {
    let mut user: Option<bool> = None;
    let mut system: Option<bool> = None;
    for arg in argv.iter().take_while(|arg| arg.as_str() != "--") {
        if let Some((is_system, enable)) = assertion_flag_scope(arg) {
            if is_system {
                system = Some(enable);
            } else {
                user = Some(enable);
            }
        }
    }
    match (user, system) {
        (None, None) => None,
        (u, s) => Some(u.unwrap_or(false) || s.unwrap_or(false)),
    }
}

/// The `--dump-phase-report <FILE>` path, scanned out of the launcher portion
/// of the expanded argv.
///
/// Scanned here rather than read from the parsed [`Args`] because the three
/// `CRATONVM_PHASE_ACCOUNTING*` names are **declared** flags:
/// `flags::runtime_var_os` serves a declared name from the immutable snapshot
/// that `install_flags` latches, and `phase::level()` latches its own answer on
/// first read — both of which happen before `run()` parses anything. A
/// `set_var` after the parse would be invisible. Injecting the value as a
/// launcher override into `VmFlags::from_env_with_overrides` is the supported
/// route, and the one `--nojit` above already uses.
///
/// Accepts both `--dump-phase-report FILE` and `--dump-phase-report=FILE`, and
/// stops at the `--` separator so a Java program argument of the same spelling
/// is never consumed.
fn launcher_phase_report_path(argv: &[String]) -> Option<String> {
    let end = argv
        .iter()
        .position(|arg| arg == "--")
        .unwrap_or(argv.len());
    let launcher = &argv[..end];
    for (i, arg) in launcher.iter().enumerate() {
        if let Some(path) = arg.strip_prefix("--dump-phase-report=") {
            return Some(path.to_string());
        }
        if arg == "--dump-phase-report" {
            return launcher.get(i + 1).cloned();
        }
    }
    None
}

fn run() -> Result<()> {
    // Phase accounting: everything from here to the `main(String[])` call below
    // is startup. That is wider than `Vm::new` alone, deliberately — the
    // `vm_startup` category is defined as "everything before the application's
    // `main` is entered", and the argument parse, classpath resolution,
    // `-XX:` translation, `args_array` construction and any `-javaagent:`
    // `premain` all happen on this thread before `main` runs. Charging them
    // here is what keeps them out of `unattributed_ns`; the `class_load`,
    // `gc_pause` and `compilation` spans wired inside those paths nest and
    // subtract, so widening the span does not hide them.
    //
    // Closed explicitly with `.end()` immediately before the `main` invocation,
    // not at end of scope. Every `?`/`bail!` between here and there drops it,
    // which charges the startup that did happen and leaves the rest
    // unattributed — the honest reading of "the VM failed to boot".
    let phase_startup = phase::enter(phase::Category::VmStartup);

    // Install the pre-`std::process::exit` hook on `native_system_exit` /
    // `native_runtime_exit`. A silent `System.exit(N)` during real app boot
    // (e.g. Cassandra NodeTool's airline NPE catch path) otherwise tears the
    // process down before any downstream observer can print state. The hook
    // fires immediately before the process exits; when `CRATONVM_DBG_EXIT=1`
    // is set it dumps the dispatch-trace ring so the last Java method run
    // before the exit is visible in stderr for diagnosis. First-installer
    // wins (OnceLock), so installing it once here at the top of `run()` is
    // sufficient. (Installing the hook here only registers the closure; it
    // doesn't run it, so this can safely stay ahead of the tracing-subscriber
    // init below.)
    cratonvm_native_builtins::lang_system::set_pre_exit_hook(|code| {
        cleanup_staged_archive_copies();
        // obsaudit D12 (2026-07-26) — `-XX:StartFlightRecording`'s
        // `dumponexit` (default true). This is the same pre-exit hook
        // `cleanup_staged_archive_copies` already relies on for "run before
        // the process actually exits" — it covers Java-initiated exit
        // (`System.exit`/`Runtime.exit`/`Runtime.halt`, the common case for
        // a real application shutting down cleanly) via `native_system_exit`/
        // `native_runtime_exit`. `process_vm()` resolves the live VM this
        // late without threading it through the hook's own signature.
        if let Some(shared) = cratonvm_vm::native::jni::process_vm() {
            let target = shared.debug.jfr_dump_on_exit.lock().clone();
            if let Some((recording_id, filename)) = target {
                let mut fr = shared.debug.flight_recorder.lock();
                match fr.dump_recording(recording_id, std::path::Path::new(&filename)) {
                    Ok(bytes) => {
                        tracing::info!("wrote {bytes} byte JFR recording to {filename}")
                    }
                    Err(e) => tracing::error!("failed to dump JFR recording on exit: {e}"),
                }
            }
        }
        if std::env::var("CRATONVM_DBG_EXIT").ok().as_deref() == Some("1") {
            eprintln!("=== CRATONVM_DBG_EXIT: System.exit({code}) — dispatch trace ===");
            cratonvm_vm::dispatch_trace::dump_to_stderr_unconditional("pre-system-exit");
        }
        maybe_dump_shutdown_reports();
        write_jdk_only_dumps_on_exit();
    });

    // `java`-launcher positional semantics: insert a `--` separator right
    // after the program selector (`-jar <jar>` or the first bare main-class
    // token) so every token past it is treated as a program argument and
    // passed to the Java application verbatim — even `--help`, `--version`,
    // `--list-modules`. Without this, clap would intercept those anywhere.
    // Runs first so the explicit `--` it parks is honoured by every
    // downstream stage.
    let raw_argv: Vec<String> = expand_argfiles(std::env::args().collect());
    let mut argv: Vec<String> = insert_program_args_separator(raw_argv);

    // Version banners are handled here, ahead of clap, for one reason: the
    // banner must name the active JDK mode. clap's built-in `--version`
    // prints and exits before any of our code runs, so it can only report
    // the crate version — which says nothing about which of the two
    // standard libraries the VM would boot. Since that is the single most
    // important fact for interpreting a bug report, `-version`,
    // `--version`, `-fullversion`, `-showversion` and `-Xinternalversion`
    // all route through `version_banner` instead.
    if let Some((query, token)) = scan_version_query(&argv) {
        let mode = scan_requested_jdk_mode(&argv);
        let compatibility = scan_requested_compatibility_mode(&argv);
        let java_home = scan_explicit_java_home(&argv);
        let banner = version_banner(query, mode, compatibility, java_home.as_deref());
        // HotSpot writes the single-dash forms to stderr (build tools scrape
        // `java -version` from there) and the double-dash forms to stdout.
        if query.to_stdout(&token) {
            print!("{banner}");
            let _ = std::io::Write::flush(&mut std::io::stdout());
        } else {
            eprint!("{banner}");
            let _ = std::io::Write::flush(&mut std::io::stderr());
        }
        if query.exits() {
            std::process::exit(0);
        }
        // `-showversion`: banner printed, now launch the program as normal.
        // Drop the flag so clap doesn't see an unknown option.
        remove_first_launcher_token(&mut argv, &token);
    }

    // Extract -Dkey=value system properties before clap parsing
    let raw_args: Vec<String> = normalize_java_launcher_argv(argv);
    let (filtered_args, system_properties) = extract_system_properties(raw_args);
    // T6 CLI compat: strip HotSpot-style flags before clap so their
    // non-standard spellings (`-XX:+Foo`, `-agentlib:`) don't confuse it.
    let (filtered_args, hotspot_flags) = extract_hotspot_flags(filtered_args);
    let mut args = Args::parse_from(filtered_args);

    // Publish the census output paths for the `System.exit` path, which never
    // returns to `run()` and so cannot see `args`. Done here, immediately after
    // parsing, because a `System.exit` can happen as early as an agent's
    // `premain`. Nothing is written unless at least one of the three flags was
    // given, so a default run publishes three `None`s and the hook returns on
    // its own guard.
    let _ = JDK_ONLY_EXIT_DUMP_PATHS.set(JdkOnlyExitDumpPaths {
        class_origins: args.dump_class_origins.clone(),
        native_registry: args.dump_native_registry.clone(),
        report: args.jdk_only_report.clone(),
        verbose: args.explain_jdk_only,
    });

    // `--dump-phase-report` was already consumed by the launcher — see
    // `launcher_phase_report_path` for why it has to be read that early. What
    // is left to do here is tell the operator when the two disagree: the
    // option is present, but accounting still resolved to off (an explicit
    // `CRATONVM_PHASE_ACCOUNTING=0`, or a `CRATONVM_DBG=-phase-accounting`),
    // so the file they are waiting for will never appear.
    if args.dump_phase_report.is_some() && !phase::enabled() {
        eprintln!(
            "[cratonvm] --dump-phase-report was given but phase accounting resolved to \
             \"off\"; no report will be written"
        );
    }

    // Initialize tracing. B6: route WARN+ diagnostics to stderr so silent
    // swallow sites surface without polluting the program's stdout (which
    // Java's System.out also writes to).
    //
    // This used to be the very first thing in `run()`, ahead of CLI
    // parsing. It's now built after `Args::parse_from` above so the
    // `--print-gpu-decisions` handling below can consult `args`. Nothing in
    // between the old and new init point ever logs through `tracing`
    // (`expand_argfiles`, `insert_program_args_separator`,
    // `normalize_java_launcher_argv`, `extract_system_properties`,
    // `extract_hotspot_flags`, and `set_pre_exit_hook` all checked — the
    // pre-exit hook only *installs* a closure here, it doesn't run it), so
    // moving the subscriber install past them drops no log lines.
    let mut env_filter = tracing_subscriber::EnvFilter::from_default_env()
        .add_directive(tracing::Level::WARN.into());

    // `--gpu --print-gpu-decisions` was a silent no-op: the decision lines
    // it promises (one per analyzer verdict) are emitted via
    // `tracing::info!`/`tracing::debug!` in `vm/src/runtime/offload.rs`
    // (`lookup_or_compile`'s "if self.print_decisions" block, and
    // `try_dispatch`'s "ran on device" / "fell back to CPU" lines) under
    // the module-path target `cratonvm_vm::runtime::offload` (crate name is
    // `cratonvm-vm` per vm/Cargo.toml `[package] name`; Cargo/rustc turns
    // `-` into `_` for the actual crate identifier used as a tracing
    // target). The WARN-only default filter above swallows all of that
    // unless the user separately exports RUST_LOG — the flag shouldn't
    // require knowing that.
    //
    // Add a low-priority default that opens up this one target. This is
    // *not* a global verbosity bump: EnvFilter picks the most specific
    // matching directive per callsite by target length, independent of add
    // order, so any RUST_LOG directive for a different target (crate-wide,
    // a sibling module, or this same target at a different level written
    // with a different target string) composes normally on top of this.
    // The one exception is a RUST_LOG directive for this *exact* target
    // string (e.g. `RUST_LOG=cratonvm_vm::runtime::offload=error`): that
    // ties in specificity with the directive added below, and EnvFilter
    // resolves same-target ties by last-added-wins rather than stacking
    // them — since this runs after `from_default_env()` has already parsed
    // RUST_LOG, this directive would win that narrow case. Not worth extra
    // machinery to special-case an already-obscure override.
    //
    // NOTE: the workspace pins `tracing` with the `release_max_level_info`
    // feature (Cargo.toml ~line 46), which compiles `tracing::debug!` down
    // to a no-op in release builds. So in a release build only the
    // INFO-level analyzer-verdict lines from `lookup_or_compile` can ever
    // appear here, by design/compile-time elision — the DEBUG-level "ran on
    // device" / "fell back to CPU" lines in `try_dispatch` simply aren't in
    // the binary to enable. Asking for `debug` below is still correct: it's
    // the right ceiling for debug builds, and a harmless no-op ceiling in
    // release builds.
    #[cfg(feature = "gpu")]
    {
        if args.print_gpu_decisions {
            env_filter = env_filter.add_directive("cratonvm_vm::runtime::offload=debug".parse()?);
        }
    }

    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_writer(std::io::stderr)
        .init();

    // --enable-native-access: open the process-wide Panama FFI gate
    // (`native-builtins/src/panama.rs::NATIVE_ACCESS_ENABLED`), which is
    // default-CLOSED after the security fix. Mirrors the modern JDK
    // `--enable-native-access` flag; the optional module value is
    // accepted-and-ignored because the gate is a single coarse
    // process-wide toggle rather than a per-module grant.
    //
    // Applied here, immediately after clap parsing and alongside the other
    // global config toggles, so it runs on the main thread well before
    // `Vm::new(config)` executes any bytecode — no Panama downcall,
    // `MemorySegment.ofAddress`, or `reinterpret` can observe the closed
    // gate once the user opted in.
    if args.enable_native_access.is_some() {
        cratonvm_native_builtins::panama::set_native_access_enabled(true);
    }

    // --illegal-native-access: what a RESTRICTED method does without a grant.
    // Set beside the gate above and for the same reason — both must be in place
    // before any bytecode runs, since the first restricted call may come from a
    // static initialiser whose failure JVMS 5.5 makes permanent.
    {
        use cratonvm_native_builtins::panama::IllegalNativeAccess as Ina;
        let mode = match args.illegal_native_access.as_str() {
            "allow" => Ina::Allow,
            "deny" => Ina::Deny,
            "warn" => Ina::Warn,
            other => {
                eprintln!(
                    "Error: Value '{other}' not recognised for --illegal-native-access; \
                     expected one of allow, warn, deny"
                );
                std::process::exit(1);
            }
        };
        cratonvm_native_builtins::panama::set_illegal_native_access(mode);
    }

    // --enable-preview: JVMS 4.1 preview class files (`minor_version == 65535`
    // at the running major version) are refused by the reader unless this is
    // set, matching HotSpot, whose default is also off. Two consumers must see
    // the same bit or the VM would refuse to load a preview class file while
    // telling the class library that preview is enabled:
    //   * cratonvm_reader::set_preview_enabled — the class-file parser's gate;
    //   * jdk/internal/misc/PreviewFeatures.isPreviewEnabled — read by
    //     `Class.isUnnamedClass()` and therefore by JUnit's launcher.
    // Measured on Adoptium 25.0.3.9: `PreviewFeatures.isEnabled` is false
    // without the flag and true with it, so the two really are one bit. Set
    // through `cratonvm_native_builtins` rather than the reader directly
    // because `vm-cli` does not depend on `cratonvm-reader` at all — and
    // because one entry point is what keeps the two consumers agreeing.
    //
    // Applied here for the same timing reason --enable-native-access gives
    // above: main thread, before `Vm::new(config)` parses a single class, so
    // no class file can be checked against the wrong value.
    //
    // Set unconditionally rather than under `if args.enable_preview`, so an
    // embedder that reuses this path cannot inherit a stale `true`.
    cratonvm_native_builtins::set_preview_enabled(args.enable_preview);

    // -XX:+ShowCodeDetailsInExceptionMessages (JEP 358): publish to the
    // env_cache so the interpreter's helpful-NPE opcode gate observes it on
    // first read, before Vm::new(config) runs any bytecode (same OnceLock
    // timing rationale as --nojit above). CRATONVM_HELPFUL_NPE_OPCODES, if set,
    // still overrides it.
    // Tri-state: an absent flag resolves to the default-on (matching
    // `VmConfig::default`); `-XX:-...` (→ `=false`) opts out.
    cratonvm_vm::runtime::env_cache::set_show_code_details_in_exception_messages(
        args.show_code_details_in_exception_messages.unwrap_or(true),
    );

    // GPU handlers — only compiled when the `gpu` Cargo feature is on.
    // Without the feature, the CPU execution path below is reached
    // unconditionally and unchanged.
    #[cfg(feature = "gpu")]
    {
        if args.gpu_info {
            match cuda_bridge::probe() {
                Ok(caps) => {
                    println!(
                        "device {}: {} (sm_{}{}), {:.2} GiB",
                        caps.ordinal,
                        caps.name,
                        caps.compute_major,
                        caps.compute_minor,
                        (caps.total_global_mem as f64) / (1024.0 * 1024.0 * 1024.0)
                    );
                }
                Err(e) => {
                    println!("no CUDA device available: {e}");
                }
            }
            return Ok(());
        }

        if args.gpu {
            match cuda_bridge::probe() {
                Ok(caps) => {
                    info!(
                        "gpu offload enabled on device {}: {} (sm_{}{})",
                        args.gpu_device, caps.name, caps.compute_major, caps.compute_minor
                    );
                }
                Err(e) => {
                    eprintln!(
                        "[cratonvm-cli] --gpu requested but no CUDA driver available ({e}); \
                         running on CPU"
                    );
                    args.gpu = false;
                }
            }
        }
    }

    // Strip a literal `--` separator that clap parked in the trailing
    // positional list. We pass `--` through to clap so it knows where
    // program args begin (so tokens like `-mp` aren't mis-parsed as
    // short flags), but the Java program must not see `--` itself in
    // its String[] args. Without this, jboss-modules' Main.main reads
    // args[0] == "--" instead of "-mp" and fails inside getServiceName
    // with NullPointerException.
    if args.class_name.as_deref() == Some("--") {
        args.class_name = None;
    }
    // Preserve every `--` that survives clap, including a trailing one.
    //
    // clap already consumes the first lone `--` as its option-parsing
    // terminator, so in the normal-class and `-jar` launch modes no launcher
    // `--` ever reaches `args.args` — any `--` left there is genuinely a
    // program argument and MUST be delivered to `main` verbatim (stock
    // `java Main a -- b` gives the program `["a", "--", "b"]`, and stock
    // `java Main a --` gives the program `["a", "--"]`; many CLI tools use
    // their own `--` end-of-options convention).
    //
    // [LOW arg-parse fix (1)] A previous version unconditionally popped a
    // trailing `--` here, on the theory that the JBoss-Modules `-mp` path
    // (`normalize_java_launcher_argv` prepends an extra `--` so `-mp` isn't
    // mis-parsed as the `-m`/`-p` short-flag cluster) could leak a spurious
    // trailing `--` as the LAST element of `args.args`. That artifact no
    // longer occurs: `-mp` is in `VALUE_TAKING_OPTS`, so
    // `insert_program_args_separator` consumes its operand instead of
    // injecting a separator, and the `-mp` token itself lands in
    // `class_name` (not `args.args`) — see the
    // `launcher_trailing_double_dash_artifact_is_popped` regression test.
    // The unconditional pop therefore had no remaining legitimate target and
    // instead silently dropped a genuine user trailing `--` (e.g.
    // `java Main a --`), violating JDK launcher semantics. Leave a trailing
    // `--` in place so it reaches the program's `String[] args` verbatim.

    // Validate: exactly one of class_name or --jar must be provided
    if args.class_name.is_none() && args.jar.is_none() {
        bail!("No class name or --jar specified. Usage: cratonvm <class> or cratonvm --jar <file.jar>");
    }
    // When both --jar and positional arguments are given, treat the
    // positionals as program args (Java-style).  This matches the stock
    // `java -jar foo.jar arg1 arg2` behaviour.
    if args.class_name.is_some() && args.jar.is_some() {
        // Shuffle: move class_name into args[0], then args, then the rest.
        let cn = args.class_name.take().unwrap();
        let mut new_args = vec![cn];
        new_args.extend(std::mem::take(&mut args.args));
        args.args = new_args;
    }

    // Resolve class name and classpath based on launch mode. If a WAR/EAR is
    // staged as a temporary `.jar` copy, keep the cleanup guard alive until
    // `run()` exits so lazy class loading can still read it.
    let mut _staged_archive_cleanup: Option<StagedArchiveCleanup> = None;
    let (class_name, classpath) = if let Some(jar_path_str) = &args.jar {
        // -jar mode: read Main-Class from manifest, build classpath from JAR + manifest Class-Path
        let jar_path = std::path::Path::new(jar_path_str);
        if !jar_path.exists() {
            bail!("JAR file not found: {}", jar_path.display());
        }

        let manifest = ClassPath::read_jar_manifest(jar_path)
            .ok_or_else(|| anyhow::anyhow!("Cannot read manifest from {}", jar_path.display()))?;

        // JN3: ClassPath::new only accepts entries whose extension is `.jar`
        // (or `.jmod`/`modules`). WARs, EARs, and other Java archive types
        // are silently dropped — so `--jar jenkins.war` would never have
        // its contents indexed and `executable.Main` (the launcher class
        // declared in `Main-Class`) could not be resolved.
        //
        // Work around this in the CLI by materialising any non-`.jar`
        // archive as a sibling temp file with a `.jar` extension and
        // adding that path to the classpath instead. The original `jar_path`
        // is still used for manifest parsing (which doesn't care about the
        // extension), and the `Class-Path` manifest header is resolved
        // relative to the original file's parent so sibling lookups still
        // work.
        let (cp_entry_for_archive, staged_cleanup) = classpath_entry_for_archive(jar_path)?;
        _staged_archive_cleanup = staged_cleanup;

        // Build classpath: JAR (or staged .jar copy) itself + manifest Class-Path entries.
        // The manifest's Class-Path header is still resolved relative to the
        // user-supplied path so sibling JARs are found at their real locations.
        let mut cp = vec![cp_entry_for_archive.to_string_lossy().into_owned()];
        cp.extend(manifest.resolve_class_path(jar_path));

        // KC26: For Quarkus applications, the RunnerClassLoader normally loads
        // classes from jars listed in quarkus-application.dat. Since we can't
        // fully emulate that complex bootstrap, add all application jars to
        // the VM classpath so ClassLoader.loadClass can find them. Canonicalise
        // jar_path first so a bare basename (user cd'd into lib/) still resolves
        // its parent. Probe BOTH the jar's own dir AND its parent, since
        // Keycloak packaging puts quarkus-run.jar in lib/ (one level deep),
        // whereas the canonical Quarkus packaging puts it at the project root.
        //
        // PERF: the multi-dir `canonicalize` + `read_dir` walk below is
        // only meaningful for Quarkus packagings, but it used to run on
        // EVERY `-jar` startup — ~10 stat/read_dir syscalls even for a
        // trivial HelloWorld jar. Gate it behind a cheap "is this actually
        // a Quarkus app" sniff so non-Quarkus jars skip the walk entirely.
        // The sniff is a handful of `Path::exists()` stats next to the jar
        // (and one dir-parent up), which is far cheaper than canonicalising
        // and reading 5 candidate dirs across 2 roots. Real Quarkus apps
        // always ship one of these signature artifacts, so their behaviour
        // is unchanged.
        if quarkus_signature_present(jar_path) {
            let canon_jar =
                std::fs::canonicalize(jar_path).unwrap_or_else(|_| jar_path.to_path_buf());
            let jar_dir = canon_jar.parent().map(|p| p.to_path_buf());
            let mut roots = Vec::new();
            if let Some(d) = jar_dir.as_ref() {
                roots.push(d.clone());
                if let Some(pp) = d.parent() {
                    roots.push(pp.to_path_buf());
                }
            }
            let app_dirs = ["app", "quarkus", "lib/main", "lib/boot", "lib/deployment"];
            let mut seen = std::collections::HashSet::new();
            for root in &roots {
                for dir_name in &app_dirs {
                    let dir = root.join(dir_name);
                    let canon_dir = std::fs::canonicalize(&dir).unwrap_or(dir.clone());
                    if !seen.insert(canon_dir.clone()) {
                        continue;
                    }
                    if canon_dir.is_dir() {
                        if let Ok(entries) = std::fs::read_dir(&canon_dir) {
                            for entry in entries.flatten() {
                                let p = entry.path();
                                if p.extension().map_or(false, |e| e == "jar") {
                                    cp.push(p.to_string_lossy().into_owned());
                                }
                            }
                        }
                    }
                }
            }
        }

        let main_class = manifest.main_class.ok_or_else(|| {
            anyhow::anyhow!("no main manifest attribute, in {}", jar_path.display())
        })?;

        if args.classpath.is_some() {
            eprintln!("Warning: -cp/-classpath is ignored when --jar is used");
        }

        let class_name = main_class.replace('.', "/");
        let cp = expand_aggregate_jars(cp);
        (class_name, cp)
    } else {
        // Class name mode
        let cn = args.class_name.as_ref().unwrap().replace('.', "/");
        let cp = if let Some(cp) = args.classpath.as_ref() {
            VmConfig::parse_classpath(cp)
        } else if let Ok(env_cp) = std::env::var("CLASSPATH") {
            VmConfig::parse_classpath(&env_cp)
        } else {
            Vec::new()
        };
        let cp = expand_aggregate_jars(cp);
        (cn, cp)
    };

    // -Xverify:* takes precedence over --noverify when both are present
    // (matches HotSpot, where the more specific flag wins).
    let xverify_mode = if let Some(spec) = args.xverify.as_deref() {
        match cratonvm_vm::config::XverifyMode::parse(spec) {
            // `All` used to parse, be stored, and be read by nothing, so the
            // launcher warned rather than silently telling a user asking for
            // maximum verification that they had it. It is wired now
            // (`ClassManager::set_strict_verification`, from `vm_init`): the
            // boot image is typestate-verified strictly, the Pass-3 deferral
            // for user-loader classes is withdrawn, and the link-time
            // bootstrap skip is off. See `config::XverifyMode`.
            Some(m) => Some(m),
            None => {
                eprintln!(
                    "Warning: ignoring unknown -Xverify mode {spec:?}; expected none|remote|all"
                );
                None
            }
        }
    } else {
        None
    };

    // The launcher's starting config. `for_launcher()` is deterministic:
    // real-JDK mode (`LAUNCHER_DEFAULT_JDK_MODE`) regardless of what is
    // installed on this machine. The actual mode — including validation and
    // the `--synthetic-jdk` / `--real-jdk` override — is resolved below,
    // after `--java-home` has been parsed. The library path
    // (`VmConfig::default`) stays synthetic so embedded callers and the
    // in-tree test suite are unaffected; that split is declared by
    // `EMBEDDED_DEFAULT_JDK_MODE` in `vm/src/config.rs`.
    let mut config = VmConfig::for_launcher()
        .with_classpath(classpath)
        .with_verbose_class_loading(args.verbose_class)
        .with_verbose_gc(args.verbose_gc)
        .with_skip_verification(args.noverify);

    // Record the user-supplied `-jar` path so `java.class.path` is set
    // to the bare jar (HotSpot contract), not the manifest-expanded
    // transitive classpath. See `VmConfig::launcher_jar` for the full
    // rationale — Liberty/Quarkus boot launchers reflect on this.
    if let Some(jar) = args.jar.as_deref() {
        config = config.with_launcher_jar(jar.to_string());
    }

    if let Some(mode) = xverify_mode {
        config = config.with_xverify_mode(mode);
    }

    // Forward the GPU-offload CLI flags into VmConfig. Only compiled
    // when the `gpu` Cargo feature is on; without the feature these
    // fields do not exist on VmConfig (see vm/src/config.rs).
    #[cfg(feature = "gpu")]
    {
        config.gpu_offload_enabled = args.gpu;
        config.gpu_device_ordinal = args.gpu_device;
        config.gpu_min_work = args.gpu_min_work;
        config.print_gpu_decisions = args.print_gpu_decisions;
    }

    if let Some(bcp) = &args.boot_classpath {
        config = config.with_boot_classpath(VmConfig::parse_classpath(bcp));
    }
    if let Some(jh) = &args.java_home {
        // Fail fast when --java-home points at a path that doesn't exist.
        // Without this, `resolve_java_home` silently returns None, the boot
        // classpath ends up empty, and the first JDK-class reference (e.g.
        // `INVOKESTATIC java/lang/Boolean.parseBoolean`) surfaces as a
        // confusing `NoSuchMethodError` instead of a clear configuration error.
        let p = std::path::Path::new(jh);
        if !p.is_dir() {
            // Under `--jdk-only` this must carry the strict framing. Contract §8
            // (docs/feature-designs/jdk-only-mode.md) requires the JdkOnly
            // failure to name "--jdk-only, the searched paths and the accepted
            // JDK layout".
            //
            // MEASURED (G82-1): the sibling branch — a `--java-home` that EXISTS
            // but is not a runtime image — does all three, via
            // `require_jdk_image_for_jdk_only`. This one never reaches it: a
            // nonexistent path fails here, during argument parsing, so the run's
            // own trailer reads `jdk mode: <not yet resolved>` and the message
            // mentions neither the policy nor why there is no fallback. That is
            // the branch a TYPO takes, i.e. the commonest way to arrive here.
            //
            // Both branches refuse and neither fabricates, so closure rule 5 held
            // either way; this is about §8's wording, which only one of them met.
            if args.jdk_only {
                anyhow::bail!(
                    "--jdk-only: --java-home path does not exist or is not a \
                     directory: {jh}\n\
                     Under --jdk-only real class bytes are authoritative, so there \
                     is nothing to fall back to — the run cannot continue without a \
                     real JDK image.\n\
                     An acceptable JDK root must contain either:\n  \
                       * jmods/java.base.jmod   (a full JDK 9+ installation), or\n  \
                       * lib/modules            (a JRE or jlink-trimmed runtime image)\n\
                     Fix by one of:\n  \
                       * pass --java-home <PATH> pointing at a JDK 9+ root;\n  \
                       * set JAVA_HOME (or CRATONVM_JAVA_HOME, which wins over it);\n  \
                       * or drop --jdk-only to run with the default compatibility \
                     behaviour.\n\
                     --synthetic-jdk is not an escape here: it conflicts with \
                     --jdk-only and is rejected before this point."
                );
            }
            anyhow::bail!(
                "--java-home path does not exist or is not a directory: {jh}\n\
                 Provide a valid JDK installation (must contain `jmods/` or `lib/modules`)."
            );
        }
        config = config.with_java_home(jh.clone());
    }

    // Container/cgroup awareness (HotSpot's `-XX:+UseContainerSupport`, on by
    // default). When enabled, read the cgroup memory/CPU limits once so the
    // ergonomic default heap is sized off the container limit and
    // `Runtime.availableProcessors()` honors the CPU quota. `-XX:-UseContainer
    // Support` skips detection entirely, so every limit reverts to host values.
    // On non-Linux hosts `detect_container()` reports "not containerized" with
    // all limits `None`, so this is a no-op there.
    let container_info = if args.disable_container_support {
        None
    } else {
        Some(cratonvm_vm::runtime::container::detect_container())
    };
    let container_mem_limit = container_info.as_ref().and_then(|i| i.memory_limit);
    // CPU count for Runtime.availableProcessors() / the JMX OS bean. Only set
    // when a cgroup quota was actually detected; `None` ⇒ report host count.
    config.container_effective_processors =
        container_info.as_ref().and_then(|i| i.effective_cpu_count);

    if let Some(max_heap_str) = &args.max_heap {
        let size = parse_size(max_heap_str)
            .with_context(|| format!("Invalid heap size: {max_heap_str}"))?;
        config = config.with_max_heap_size(size);
    } else if let Some(ergo) = ergonomic_default_max_heap(container_mem_limit) {
        // No explicit -Xmx: size the heap like a stock JDK (1/4 of host RAM, or
        // of the cgroup limit inside a container) instead of the fixed 256 MB
        // library default, so Spring/Mockito/JUnit workloads don't thrash GC
        // into a pseudo-hang. See `ergonomic_default_max_heap`.
        if args.verbose_gc {
            let basis = if container_mem_limit.is_some() {
                "1/4 container memory limit"
            } else {
                "1/4 physical RAM"
            };
            eprintln!(
                "[cratonvm] ergonomic default max heap: {} MB ({basis}; \
                 set -Xmx or CRATONVM_DEFAULT_HEAP_ERGONOMICS=0 to override)",
                ergo / (1024 * 1024)
            );
        }
        config = config.with_max_heap_size(ergo);
    }

    // F-16 — `-Xms`. Applied AFTER `-Xmx` so it can be clamped against the heap
    // size actually in force, ergonomic default included. Clamping rather than
    // rejecting: HotSpot treats `-Xms` above `-Xmx` as an error, but a VM that
    // refuses to start over a memory hint is a worse drop-in than one that
    // starts with the heap it was told it may have — and a Surefire fork with a
    // stale `-Xms` is exactly the case this has to survive.
    if let Some(initial) = &args.initial_heap {
        let size = parse_size(initial)
            .with_context(|| format!("Invalid initial heap size: {initial}"))?;
        let clamped = size.min(config.max_heap_size);
        if clamped != size && args.verbose_gc {
            eprintln!(
                "[cratonvm] -Xms {} MB exceeds -Xmx {} MB; committing the whole heap",
                size / (1024 * 1024),
                config.max_heap_size / (1024 * 1024)
            );
        }
        config.initial_heap_size = clamped;
    }

    // CDS configuration
    if let Some(archive_path) = &args.shared_archive_file {
        config.shared_archive_file = Some(archive_path.clone());
    }
    config.cds_mode = match args.xshare.as_str() {
        "on" => cratonvm_vm::config::CdsMode::On,
        "auto" => cratonvm_vm::config::CdsMode::Auto,
        "dump" => cratonvm_vm::config::CdsMode::Dump,
        "off" => cratonvm_vm::config::CdsMode::Off,
        other => {
            // Warn on a typo rather than silently defaulting to Off.
            eprintln!(
                "Warning: ignoring unknown -Xshare mode {other:?}; expected on|auto|dump|off"
            );
            cratonvm_vm::config::CdsMode::Off
        }
    };

    // ---------------------------------------------------------------
    // JDK mode: explicit selection, then validation, then publication.
    //
    // Previously this was `use_synthetic_jdk = detect_real_jdk().is_none()`
    // (in `with_host_jdk_default`) plus an ad-hoc "--java-home implies
    // real" rule here. That made the *standard library* — and therefore
    // the set of bugs a run could hit — a function of the host machine,
    // with nothing in the VM's output recording which one was used.
    //
    // Now: the mode comes from `--synthetic-jdk` / `--real-jdk` or the
    // fixed launcher default; an unavailable mode aborts the launch with
    // an actionable message; and the outcome is published for the
    // `-version` banner and the fatal-error path. `--java-home` no longer
    // *selects* real-JDK mode (it is already the default) — it only points
    // the (already selected) real-JDK mode at a specific installation.
    //
    // The compatibility *policy* (`--jdk-only`) is resolved FIRST, so that
    // `--jdk-only --synthetic-jdk` is diagnosed as the policy conflict it is
    // rather than as the generic "two standard libraries" conflict
    // `resolve_jdk_mode` would report for the same argv.
    // ---------------------------------------------------------------
    let compatibility_mode = resolve_compatibility_mode(args.jdk_only, args.synthetic_jdk)?;
    note_no_stubs_env_without_jdk_only(args.jdk_only);
    let (jdk_mode, resolved_java_home) = match resolve_jdk_mode(
        args.synthetic_jdk,
        // Contract §9: `--jdk-only` implies `JdkMode::Real`. It composes with
        // an explicit `--real-jdk` (both select the same library) and is
        // rejected above alongside `--synthetic-jdk`.
        args.real_jdk || args.jdk_only,
        config.java_home.as_deref(),
    ) {
        Ok(resolved) => resolved,
        Err(e) if args.jdk_only => {
            return Err(e.context(
                "--jdk-only requires a real JDK runtime image: real class bytes are \
                 authoritative under this policy, so there is nothing to fall back \
                 to. Point the launcher at an installation with --java-home <PATH>, \
                 or drop --jdk-only to run with the default compatibility behaviour.",
            ));
        }
        Err(e) => return Err(e),
    };
    config = config.with_jdk_mode(jdk_mode);
    config = config.with_compatibility_mode(compatibility_mode);
    // Backstop for the same conflict: `resolve_compatibility_mode` guards the
    // launcher's own flags, `validate_compatibility` guards the assembled
    // config (which an embedder or a future argv rewrite could reach another
    // way). Cheap, and it keeps the invariant with the type that owns it.
    config
        .validate_compatibility()
        .map_err(|e| anyhow::anyhow!("{e}"))?;
    // Pin the validated JDK root onto the config so boot-classpath
    // discovery inside the VM resolves the same installation this launcher
    // validated, instead of re-running the environment probe and possibly
    // landing somewhere else.
    if let Some(home) = resolved_java_home.as_ref() {
        if config.java_home.is_none() {
            config = config.with_java_home(home.to_string_lossy().into_owned());
        }
    }
    let _ = ACTIVE_JDK_MODE.set((
        jdk_mode,
        resolved_java_home.map(|p| p.to_string_lossy().into_owned()),
    ));
    tracing::info!("{}", active_jdk_mode_line());
    tracing::info!("compatibility mode: {}", compatibility_mode.as_str());
    // Remembered for `maybe_dump_shutdown_reports`, which runs on BOTH exit
    // arms and does not have `args`. See `GC_STATS_REQUESTED`.
    if args.verbose_gc {
        GC_STATS_REQUESTED.store(true, std::sync::atomic::Ordering::Release);
    }
    if args.verbose_class || args.verbose_gc {
        eprintln!("[cratonvm] {}", active_jdk_mode_line());
        eprintln!(
            "[cratonvm] compatibility mode: {}",
            compatibility_mode.as_str()
        );
    }
    if compatibility_mode.is_jdk_only() {
        // Wave 1 is measurement-first (contract §10): only class fabrication
        // and stub registration actually enforce. Say so, so a clean run is
        // not mistaken for a fully-enforced one.
        eprintln!(
            "[cratonvm] --jdk-only: real class bytes are authoritative. Wave-1 \
             enforcement covers class fabrication and synthetic-stub \
             registration; remaining violations are recorded and counted. Pass \
             --jdk-only-report <FILE> for the census."
        );
    }

    // AOT configuration
    config.aot_mode = match args.aot_mode.as_str() {
        "training" => cratonvm_vm::config::AotMode::Training,
        "production" => cratonvm_vm::config::AotMode::Production,
        "off" => cratonvm_vm::config::AotMode::Off,
        other => {
            // Warn on a typo rather than silently defaulting to Off.
            eprintln!(
                "Warning: ignoring unknown -XX:AOTMode value {other:?}; \
                 expected off|training|production"
            );
            cratonvm_vm::config::AotMode::Off
        }
    };
    if let Some(cache_path) = &args.aot_cache {
        // AOTCache serves as input in production mode and output in training mode
        match config.aot_mode {
            cratonvm_vm::config::AotMode::Production => {
                config.aot_cache_input = Some(cache_path.clone());
            }
            cratonvm_vm::config::AotMode::Training => {
                if config.aot_cache_output.is_none() {
                    config.aot_cache_output = Some(cache_path.clone());
                }
            }
            _ => {
                config.aot_cache_input = Some(cache_path.clone());
            }
        }
    }
    if let Some(output_path) = &args.aot_cache_output {
        config.aot_cache_output = Some(output_path.clone());
    }

    // Garbage-collector selection (`-XX:+UseG1GC` / `-XX:-UseG1GC` / any other
    // `-XX:+Use*GC`, normalized to `--XX:UseGc <name>`). Absent → keep the
    // default, which is **ZGC** as of 2026-08-10 (see `VmConfig::default` for
    // the measurement). A recognized selector (`g1` | `z`/`zgc` |
    // `generational`) sets `gc_algorithm`; an unsupported collector
    // (Serial/Parallel/Shenandoah/Epsilon) warns and falls back to Generational
    // so a `java` drop-in keeps booting.
    //
    // The unsupported-collector fallback deliberately stays **Generational**
    // rather than following the default: someone who asked for
    // `-XX:+UseSerialGC` asked for a collector this VM does not have, and the
    // conservative copying collector is the safer thing to hand them than
    // whichever backend happens to be default that month.
    if let Some(sel) = &args.gc_selector {
        match cratonvm_vm::config::parse_gc_algorithm(sel) {
            Some(algo) => config.gc_algorithm = algo,
            None => {
                eprintln!(
                    "Warning: unsupported garbage collector -XX:+Use{sel}GC; CratonVM \
                     implements ZGC-real (the default, -XX:+UseZGC), G1 \
                     (-XX:+UseG1GC) and Generational (-XX:+UseGenerationalGC). \
                     Falling back to Generational."
                );
                config.gc_algorithm = cratonvm_vm::config::GcAlgorithm::Generational;
            }
        }
    }
    // `--nojit` used to force Generational here. That override is GONE as of
    // 2026-08-10, and its removal is part of the default flip rather than an
    // aside: with ZGC as the default, leaving it in place would have made
    // `--nojit` silently change the COLLECTOR as well as the compiler. Every
    // A/B that uses `--nojit` to isolate a JIT effect would then have been
    // varying two things at once — and it would have put interpreter-only runs
    // on the arm carrying the 63-class HANG column this flip exists to escape.
    // Its stated reason was a G1 reference-slot concern, which never applied to
    // the generational-vs-ZGC choice; `-XX:+UseGenerationalGC` expresses it
    // explicitly for callers who still want it.

    // G1 tuning knobs (§7 item 4). Parsed from the normalized `--XX:*` value
    // args and stored on the config; applied to `G1CollectorConfig` only when
    // the G1 backend is selected (vm_init via `G1ConfigOverrides`). A malformed
    // value warns and is ignored (keeps the collector default), matching the
    // lenient-with-warning policy used for the GC selector.
    if let Some(s) = &args.g1_ihop {
        match s.parse::<u8>() {
            Ok(p) if (1..=100).contains(&p) => config.g1_ihop_percent = Some(p),
            _ => eprintln!(
                "Warning: ignoring -XX:InitiatingHeapOccupancyPercent={s} (expected 1..=100)"
            ),
        }
    }
    if let Some(s) = &args.g1_region_size {
        match parse_size(s) {
            Some(sz) if sz > 0 => config.g1_region_size = Some(sz),
            _ => eprintln!("Warning: ignoring -XX:G1HeapRegionSize={s} (expected a byte size)"),
        }
    }
    if let Some(s) = &args.g1_max_pause {
        match s.parse::<u64>() {
            Ok(ms) if ms > 0 => config.g1_max_gc_pause_ms = Some(ms),
            _ => eprintln!(
                "Warning: ignoring -XX:MaxGCPauseMillis={s} (expected a positive integer)"
            ),
        }
    }
    if let Some(s) = &args.g1_string_dedup {
        config.g1_string_dedup = Some(s == "true");
    }
    if let Some(s) = &args.g1_parallel_gc_threads {
        match s.parse::<usize>() {
            // Rejecting 0 rather than accepting it: inside the collector 0 is
            // the sentinel for "derive from the machine", so honouring
            // `-XX:ParallelGCThreads=0` would silently do the opposite of what
            // an operator writing it means.
            Ok(n) if n > 0 => config.g1_parallel_gc_threads = Some(n),
            _ => eprintln!(
                "Warning: ignoring -XX:ParallelGCThreads={s} (expected a positive integer)"
            ),
        }
    }
    if let Some(s) = &args.max_direct_memory {
        match parse_size(s) {
            Some(sz) if sz > 0 => config.max_direct_memory_size = Some(sz),
            _ => eprintln!("Warning: ignoring -XX:MaxDirectMemorySize={s} (expected a byte size)"),
        }
    }

    // Missing native audit. NEW-10: `--dump-missing-natives FILE` and
    // T2.1.3: `--dump-missing-natives-grouped FILE` both implicitly
    // enable audit mode so the user doesn't have to pass the flag
    // separately.
    config.audit_missing_natives = args.audit_missing_natives
        || args.dump_missing_natives.is_some()
        || args.dump_missing_natives_grouped.is_some();

    // -XX:±ShowCodeDetailsInExceptionMessages — record on the VmConfig too, so
    // the resolved config reflects the flag (env_cache was already set from
    // `args` above for the interpreter gate). Absent → keep the default-on
    // `VmConfig` value; an explicit flag (`=true`/`=false`) overrides it.
    config.show_code_details_in_exception_messages = args
        .show_code_details_in_exception_messages
        .unwrap_or(config.show_code_details_in_exception_messages);

    // JDWP debug server.
    //
    // `jdwp_port` is only ever read behind `#[cfg(feature = "experimental-debug")]`
    // in `vm_init`, and that feature is not in `cratonvm-vm`'s default set nor
    // enabled by `cratonvm-cli` — so in every shipped launcher binary this is
    // accepted and does nothing. `jdwp_suspend` is not read anywhere at all, in
    // any configuration. Both used to be silent; a debugger that never attaches
    // is not a subtle failure to be left unexplained.
    if let Some(port) = args.jdwp_port {
        if !cratonvm_vm::config::JDWP_SERVER_COMPILED_IN {
            eprintln!(
                "Warning: --jdwp-port {port} has no effect in this build; the JDWP \
                 server is behind the 'experimental-debug' feature, which this \
                 binary was not built with."
            );
        }
        if args.jdwp_suspend {
            eprintln!(
                "Warning: --jdwp-suspend is not implemented; the VM will not wait \
                 for a debugger to attach."
            );
        }
        config = config.with_jdwp(port, args.jdwp_suspend);
    } else if args.jdwp_suspend {
        eprintln!("Warning: --jdwp-suspend has no effect without --jdwp-port.");
    }

    // JPMS module system flags
    if let Some(mp) = &args.module_path {
        config.module_path = VmConfig::parse_classpath(mp);
    }
    for s in &args.add_reads {
        config.add_reads.extend(VmConfig::parse_add_reads(s));
    }
    for s in &args.add_exports {
        if let Some(tuple) = VmConfig::parse_add_exports(s) {
            config.add_exports.push(tuple);
        } else {
            eprintln!(
                "Warning: invalid --add-exports format: {s} (expected module/package=target)"
            );
        }
    }
    for s in &args.add_opens {
        if let Some(tuple) = VmConfig::parse_add_exports(s) {
            config.add_opens.push(tuple);
        } else {
            eprintln!("Warning: invalid --add-opens format: {s} (expected module/package=target)");
        }
    }
    config.add_modules = args.add_modules.clone();

    // System properties from -Dkey=value flags
    config.system_properties = system_properties;

    // `sun.java.command` and `sun.java.launcher`, which HotSpot's launcher sets
    // and this one did not. Measured absent 2026-08-04 by diffing
    // `System.getProperties()` against HotSpot 25.
    //
    // Not cosmetic. `sun.java.command` is how a process identifies itself to
    // itself: Spring Boot's `ApplicationHome`, log4j/logback default file
    // naming, JMX `RuntimeMXBean`, and several agent/attach paths read it, and
    // a `null` there turns into a wrong log path or a silent feature-off rather
    // than an error. The launcher is the only layer that knows the value, which
    // is why the property table in `native-builtins` cannot supply it.
    //
    // Format follows the launcher: the main class (dotted, as typed) or the jar
    // path, then the program arguments, space-separated. An explicit `-D` wins,
    // matching `java -Dsun.java.command=…`.
    if !config
        .system_properties
        .iter()
        .any(|(k, _)| k == "sun.java.command")
    {
        let head = args
            .jar
            .clone()
            .or_else(|| args.class_name.clone())
            .unwrap_or_default();
        if !head.is_empty() {
            let command = if args.args.is_empty() {
                head
            } else {
                format!("{head} {}", args.args.join(" "))
            };
            config
                .system_properties
                .push(("sun.java.command".to_string(), command));
        }
    }
    if !config
        .system_properties
        .iter()
        .any(|(k, _)| k == "sun.java.launcher")
    {
        config
            .system_properties
            .push(("sun.java.launcher".to_string(), "SUN_STANDARD".to_string()));
    }

    // Container support (enabled by default, disabled with --XX:-UseContainerSupport)
    if args.disable_container_support {
        config = config.with_container_support(false);
    }

    // Unified logging (-Xlog)
    if let Some(xlog_spec) = &args.xlog {
        config = config.with_xlog_spec(xlog_spec.clone());
    }

    // T6.1.2 — `-XX:+HeapDumpOnOutOfMemoryError` / `-XX:HeapDumpPath=...`.
    // The interpreter's OOM path already honors these config fields (see
    // `maybe_dump_heap_on_oom` in `vm/src/runtime/interpreter.rs`); we
    // only need to thread the CLI values through.
    if let Some(flag) = hotspot_flags.heap_dump_on_oom {
        config.heap_dump_on_oom = flag;
    }
    if let Some(path) = hotspot_flags.heap_dump_path.clone() {
        config.heap_dump_path = Some(path);
    }

    // obsaudit D12 — `-XX:StartFlightRecording[:opts]`. Parsed here (not in
    // extract_hotspot_flags) so a bad sub-option produces a proper `Err`
    // this function can propagate, instead of a raw panic/exit from deep
    // inside argv scanning.
    if let Some(raw) = &hotspot_flags.jfr_start_recording {
        config.jfr_start_recording =
            Some(parse_jfr_start_recording_opts(raw).map_err(|e| anyhow::anyhow!("{e}"))?);
        // B10: the same string also carries `+<EventName>#enabled=true` tokens.
        // Collected in a second pass so a bad event name is an `Err` here rather
        // than a silently-ignored token — a typo that produces an empty dump is
        // indistinguishable from "the event never fired", which is precisely the
        // ambiguity this event was added to remove.
        config.jfr_enabled_events =
            parse_jfr_event_settings(raw).map_err(|e| anyhow::anyhow!("{e}"))?;
    }

    // T6.3.3 — `-agentlib:`, `-agentpath:`, `-javaagent:`. The options
    // are stashed here and handed to the JVMTI `AgentRegistry` at VM
    // startup; registering them in a dedicated config field lets the
    // startup path load them in the canonical Agent_OnLoad order.
    //
    // WP2.4-C: split out the `-javaagent:` JAR specs into Java-agent
    // descriptors. The two sets are disjoint by syntax (`-javaagent:`
    // points at a JAR + `Premain-Class` manifest attribute, while
    // `-agentlib:` / `-agentpath:` point at native libraries with a
    // C `Agent_OnLoad` entry point) so we can route them independently.
    let mut java_agents: Vec<cratonvm_vm::runtime::agent_loader::LoadedAgent> = Vec::new();
    if !hotspot_flags.agent_options.is_empty() {
        let mut native_agent_opts: Vec<String> = Vec::new();
        for opt in &hotspot_flags.agent_options {
            if opt.starts_with("-javaagent:") {
                match cratonvm_vm::runtime::agent_loader::parse_javaagent_spec(opt) {
                    Ok(agent) => java_agents.push(agent),
                    Err(e) => {
                        // Per the `java.lang.instrument` package spec, a
                        // misconfigured agent should fail loudly enough
                        // that the operator notices, but the VM should
                        // still try to run the application — match the
                        // HotSpot warn-and-continue behaviour.
                        eprintln!("Warning: ignoring {opt}: {e}");
                    }
                }
            } else {
                native_agent_opts.push(opt.clone());
            }
        }
        config.jvmti_agent_options = native_agent_opts;
    }

    // Validate the class name before proceeding
    validate_class_name(&class_name)?;

    info!("Starting CratonVM");
    info!("Class: {class_name}");
    info!("Classpath: {:?}", config.classpath);

    // Create VM and execute main method
    let mut vm = Vm::new(config);

    // JDK-only: drain the violation logs now, not only at shutdown. Native
    // registration refusals all happen inside `Vm::new`, so this is their real
    // time of occurrence — reporting them at shutdown would print them after
    // whatever failure they caused. See `ViolationWatermark` for what this
    // drain still covers now that class-origin violations arrive live.
    let mut jdk_only_watermark = ViolationWatermark::default();
    // `--explain-jdk-only` is admitted here as well as `--trace-jdk-only`.
    // `finish_jdk_only` already accepts either (see its gate), so gating the
    // vm-init drain and the live sink on `trace_jdk_only` ALONE meant
    // `--jdk-only --explain-jdk-only` on its own got only a shutdown batch:
    // every mid-run fabrication reported detached from the code that caused it,
    // which is precisely what `ViolationWatermark`'s own doc says the live sink
    // exists to prevent. Measured 2026-08-12: none of the nine runtime
    // fabrication families appears in `--explain-jdk-only` output, while all
    // thirteen boot-time refusals do.
    if args.trace_jdk_only || args.explain_jdk_only {
        trace_jdk_only_violations(
            &vm.shared,
            &mut jdk_only_watermark,
            "vm-init",
            args.explain_jdk_only,
        );
        // Contract §9 asks `--trace-jdk-only` to *"log every violation as it
        // happens"*. Class-origin violations occur throughout the run, so a
        // shutdown-batched drain reports them detached from the code that
        // caused them. Install a live sink now — after the drain above, so
        // the violations recorded inside `Vm::new` are reported exactly once,
        // by that drain, and everything from here on is reported exactly once,
        // by the sink. `trace_jdk_only_violations` sees the sink and advances
        // its origin watermark without re-printing.
        //
        // The sink is a field on this VM's `ClassManager`, not a process
        // global (contract §2): two VMs in one process each see only their
        // own violations.
        let jdk_feature = detect_jdk_feature(vm.shared.config.java_home.as_deref());
        let explain = args.explain_jdk_only;
        vm.shared
            .classes
            .class_manager_write()
            .set_violation_sink(std::sync::Arc::new(move |violation| {
                eprintln!(
                    "[cratonvm][jdk-only:live] {}",
                    render_violation(violation, jdk_feature, explain)
                );
            }));
    }

    // BUG-03 — publish the main thread's TLAB address now that `vm` is at its
    // final, address-stable location on the `main-vm` thread. This lets the
    // cross-thread STW JIT root scan recover the main thread's un-retired
    // reserved TLAB tail if it is forcibly stopped while executing JIT code
    // (workers / foreign threads publish theirs at their own start). Casting to
    // a raw pointer ends the borrow immediately, so the subsequent registry
    // call does not conflict.
    {
        let main_tlab = &vm.main_thread.tlab as *const _ as usize;
        let main_tid = vm.main_thread.thread_id;
        vm.shared
            .threads
            .thread_registry
            .set_tlab_addr(main_tid, main_tlab);
        // xt-hardening (2026-07-03): publish main's OS thread id for the
        // takeover's counted-set excusal (workers publish at their start).
        vm.shared
            .threads
            .thread_registry
            .set_os_tid_current(main_tid);
    }

    // T19.H1: optional watchdog that dumps interpreter frames and aborts when
    // the user explicitly bounds execution. Normal Java programs may be
    // long-running services, so no watchdog is armed by default. Set
    // `--stack-dump-on-timeout=N` or `CRATONVM_DEFAULT_WATCHDOG_SEC=N` to opt
    // in; `--stack-dump-on-timeout=0` and `CRATONVM_DISABLE_DEFAULT_WATCHDOG=1`
    // disable the env-default path.
    //
    // Native-call and dispatch rings are recorded only for explicit diagnostic
    // requests because they add hot-path work on every native-method entry.
    let explicit_watchdog = matches!(args.stack_dump_on_timeout, Some(s) if s > 0);
    let ring_recording_requested = explicit_watchdog
        || std::env::var("CRATONVM_ENABLE_NATIVE_RING").ok().as_deref() == Some("1");
    let default_watchdog_env = if std::env::var("CRATONVM_DISABLE_DEFAULT_WATCHDOG")
        .ok()
        .as_deref()
        == Some("1")
    {
        None
    } else {
        std::env::var("CRATONVM_DEFAULT_WATCHDOG_SEC").ok()
    };
    let effective_watchdog =
        resolve_watchdog_timeout(args.stack_dump_on_timeout, default_watchdog_env.as_deref());
    // Shared "run() completed" flag for the stack-dump watchdog. When `run()`
    // returns (normally OR via `?`/early-return), the RAII guard below sets
    // this to `true`; the watchdog checks it after its deadline sleep and
    // again immediately before `abort()`, exiting cleanly without aborting a
    // run that finished just after a tight deadline. Only meaningful when a
    // watchdog is actually armed, but harmless otherwise.
    let watchdog_completed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    // RAII guard: its Drop runs on every exit path of `run()` (normal return,
    // early `return`, and `?` propagation), so the completed flag is always
    // set once we leave this function. Instantiated right after the watchdog
    // is spawned (see below).
    struct WatchdogCompletionGuard {
        flag: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }
    impl Drop for WatchdogCompletionGuard {
        fn drop(&mut self) {
            self.flag.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }
    // Holds the guard alive until `run()` returns. `None` when no watchdog is
    // armed (nothing to cancel).
    let mut _watchdog_completion_guard: Option<WatchdogCompletionGuard> = None;

    if ring_recording_requested {
        cratonvm_native_api::native_ring::enable(true);
        vm.shared.natives.native_methods.flush_native_ring_names();
        cratonvm_vm::dispatch_trace::enable();
    }

    // `--stack-sample-ms`: periodic, time-weighted Java-frame profiler. Arms
    // sampling mode (which makes the interpreter CONSUME each dump request
    // instead of latching it once per nested `execute()`), then re-arms the
    // request every interval. Never aborts the process, so it composes with a
    // normal run; the run just gets slower in proportion to the sample rate.
    if let Some(ms) = args.stack_sample_ms.filter(|ms| *ms > 0) {
        vm.shared.enable_stack_sampling();
        let shared_for_sampler = std::sync::Arc::clone(&vm.shared);
        let sampler_completed = std::sync::Arc::clone(&watchdog_completed);
        std::thread::Builder::new()
            .name("cratonvm-stack-sampler".into())
            .spawn(move || {
                eprintln!("=== stack sampler: armed at {ms}ms intervals ===");
                loop {
                    std::thread::sleep(std::time::Duration::from_millis(ms));
                    if sampler_completed.load(std::sync::atomic::Ordering::SeqCst) {
                        return;
                    }
                    shared_for_sampler.request_stack_sample();
                }
            })
            .ok();
    }

    if let Some(secs) = effective_watchdog {
        // Enable the native-call ring buffer so the watchdog's "0 Java
        // threads dumped" fallback can show the last ~64 native methods
        // every thread entered. Without this, the ring's
        // `dump_to_stderr` reports "recording disabled" and a hang in
        // pure Rust runtime code has no actionable diagnostic.
        // Recording cost (single relaxed AtomicBool load on entry, plus
        // a parking_lot::Mutex when set) is negligible per call but adds
        // up across a full run, so we only arm it when a diagnostic was
        // EXPLICITLY requested (see `ring_recording_requested` above). The
        // A watchdog armed through the env default still aborts + dumps Java
        // frames; ring detail is reserved for runs that asked for it.
        if ring_recording_requested {
            cratonvm_native_api::native_ring::enable(true);
            vm.shared.natives.native_methods.flush_native_ring_names();
            // T19.H1 — also enable the dispatch-trace ring. The native-call
            // ring records only opaque fn-pointers from two dispatch sites;
            // the dispatch trace records *named* class.method.desc for every
            // bytecode-method entry and every `safe_native_call`, which is
            // the actionable diagnostic for "main thread is in native code".
            cratonvm_vm::dispatch_trace::enable();
        }
        let shared_for_watchdog = std::sync::Arc::clone(&vm.shared);
        // RKC16N.5 — capture the audit-dump paths into the watchdog
        // thread so a hung run still produces a missing-natives
        // census. Without this, the only flush path is the
        // clean-shutdown branch at the end of `main()`, and every
        // watchdog-killed run loses the JSON we use for KC16/KC26
        // boot debugging.
        let watchdog_dump_path = args.dump_missing_natives.clone();
        let watchdog_dump_grouped_path = args.dump_missing_natives_grouped.clone();
        let watchdog_completed_for_thread = std::sync::Arc::clone(&watchdog_completed);
        std::thread::Builder::new()
            .name("cratonvm-stack-watchdog".into())
            .spawn(move || {
                std::thread::sleep(std::time::Duration::from_secs(secs));
                // Cancellation: if `run()` already finished (e.g. it completed
                // just after a tight deadline), don't dump or abort — exit
                // cleanly. Checked here right after the deadline sleep, and
                // again immediately before `abort()` below.
                if watchdog_completed_for_thread.load(std::sync::atomic::Ordering::SeqCst) {
                    return;
                }
                // Banner first so the user can tell we got this far.
                eprintln!(
                    "=== T19.H1 watchdog: deadline of {secs}s elapsed; \
                     requesting thread stack dumps ==="
                );
                shared_for_watchdog.request_stack_dump();

                // Give interpreter threads a short grace period to hit
                // the hot-loop check and flush their dumps. We poll the
                // ack counter so we exit the grace period as soon as
                // every reachable thread has responded.
                let grace_start = std::time::Instant::now();
                let grace = std::time::Duration::from_secs(3);
                while grace_start.elapsed() < grace {
                    let acks = shared_for_watchdog.stack_dump_ack_count();
                    // We can't know the exact thread count without
                    // holding the registry lock, but the registry only
                    // grows, so reaching any positive ack count and
                    // then stabilising is the signal we want. Short
                    // sleeps keep the abort latency bounded.
                    std::thread::sleep(std::time::Duration::from_millis(100));
                    let acks2 = shared_for_watchdog.stack_dump_ack_count();
                    if acks > 0 && acks == acks2 {
                        break;
                    }
                }

                let total_acks = shared_for_watchdog.stack_dump_ack_count();
                // Print the per-thread summary HERE, not at request time: only
                // now is it known which threads answered, and that is what lets
                // the summary tell a RUNNING-and-dumped thread apart from a
                // RUNNING-and-silent one (JIT-compiled code or a long native
                // call). See `SharedVm::dump_thread_summary_after_dumps`.
                shared_for_watchdog.dump_thread_summary_after_dumps();
                // A stall whose reactor threads are parked in `select` — i.e.
                // working — is not explained by any thread dump: the question
                // is what the selector had to hand them, and `interest_ops` vs
                // `ready_ops` per registered key answers it at the moment of
                // the stall. Costs nothing until this deadline fires.
                cratonvm_native_io::nio_selector::dump_selector_state_to_stderr();
                eprintln!(
                    "=== T19.H1 watchdog: {total_acks} thread(s) dumped; \
                     aborting process ==="
                );

                // KC-watchdog-native: when zero Java threads ack'd a dump,
                // no thread reached an interpreter dispatch point. That is
                // TWO states, not one, and this block used to assert the
                // wrong one of them as fact ("main thread is in native
                // (Rust) code"):
                //
                //   * parked in native (Rust) code — a JNI/native-method
                //     loop or a deadlock on a runtime mutex; or
                //   * RUNNING in JIT-COMPILED code, which the interpreter's
                //     dump hook cannot observe at all because the hook lives
                //     in the dispatch loop the thread is not executing.
                //
                // `--nojit` separates them in one re-run, and the message
                // below now says so. Guessing cost
                // known-issues/netty/brotli-integration-test-hangs-outside-the-interpreter
                // an entire investigation: nothing was blocked, a compiled
                // `ByteBuf.writeByte` loop was simply 200x too slow.
                //
                // Either way the Java-frame dump produces nothing, so fall
                // back to:
                //   1. The watchdog thread's own native backtrace (cheap
                //      and tells you WHERE in the runtime the watchdog
                //      is reached from — usually right after the sleep,
                //      which is uninteresting, but confirms the watchdog
                //      thread didn't itself deadlock).
                //   2. The PID + a hint to attach an external debugger
                //      (cdb / WinDbg / `rust-lldb -p <pid>`) for full
                //      thread coverage. We can't safely walk other
                //      threads' native stacks from a portable Rust
                //      thread without OS-specific facilities (Windows
                //      `MiniDumpWriteDump`, Linux `ptrace`, etc.).
                if total_acks == 0 {
                    let pid = std::process::id();
                    eprintln!(
                        "=== T19.H1 watchdog: no Java thread reached an \
                         interpreter dispatch point. That is EITHER \
                         JIT-compiled code (which this hook cannot observe \
                         — it lives in the dispatch loop) OR native (Rust) \
                         code. Do NOT assume the second: re-run with \
                         --nojit, and if the frame dumps appear there the \
                         thread was in compiled code and was RUNNING, not \
                         stuck. pid={pid}. To settle it on the live process \
                         instead, attach a native debugger before the 3s \
                         post-dump grace ends (Windows: `cdb -p {pid}` then \
                         `~* k`; Linux: `gdb -p {pid}` then `thread apply \
                         all bt`) — a thread burning CPU in \
                         `jit_invoke_*`/compiled frames is the first case, \
                         one parked in a futex/read is the second. ==="
                    );
                    let bt = std::backtrace::Backtrace::force_capture();
                    eprintln!(
                        "--- T19.H1 watchdog native backtrace (watchdog \
                         thread; FYI only) ---\n{bt}\n--- end native \
                         backtrace ---"
                    );
                    // PERF: ring recording is opt-in now (see
                    // `ring_recording_requested`). If it wasn't requested,
                    // the ring dumps below will say "recording disabled";
                    // tell the operator how to capture them next time so
                    // the diagnostic isn't a dead end.
                    if !ring_recording_requested {
                        eprintln!(
                            "=== T19.H1 watchdog: native-call/dispatch \
                             rings were not recording (opt-in). Re-run with \
                             `--stack-dump-on-timeout=N` or \
                             `CRATONVM_ENABLE_NATIVE_RING=1` to capture the \
                             last native methods leading up to the hang. ==="
                        );
                    }
                    // KC-watchdog-native: dump the native-call ring
                    // buffer. The last entry with `STILL-IN-NATIVE`
                    // marks the hang site.
                    cratonvm_native_api::native_ring::dump_to_stderr();
                    // T19.H1: dump the dispatch-trace ring too — it
                    // records *named* class.method.desc for the last
                    // 256 bytecode-method entries and native dispatches
                    // (across all threads), so the last few NAT/BC
                    // entries pinpoint the hung native and its caller.
                    cratonvm_vm::dispatch_trace::dump_to_stderr_unconditional(
                        "watchdog-native-hang",
                    );
                }

                // RKC16N.5 — flush the missing-natives audit BEFORE
                // `process::abort()` so a hung or watchdog-killed run
                // still produces the diagnostic JSON. Errors are
                // logged but never unwrap; abort still happens.
                shared_for_watchdog.dump_missing_natives();
                if let Some(path) = watchdog_dump_path.as_deref() {
                    match shared_for_watchdog.dump_missing_natives_json(path) {
                        Ok(()) => eprintln!(
                            "=== T19.H1 watchdog: missing-natives audit \
                             flushed (json={path}) ==="
                        ),
                        Err(e) => eprintln!(
                            "=== T19.H1 watchdog: failed to flush \
                             missing-natives JSON to {path}: {e} ==="
                        ),
                    }
                }
                if let Some(path) = watchdog_dump_grouped_path.as_deref() {
                    match shared_for_watchdog.dump_missing_natives_grouped_json(path) {
                        Ok(()) => eprintln!(
                            "=== T19.H1 watchdog: missing-natives audit \
                             flushed (grouped json={path}) ==="
                        ),
                        Err(e) => eprintln!(
                            "=== T19.H1 watchdog: failed to flush \
                             grouped missing-natives JSON to {path}: {e} ==="
                        ),
                    }
                }

                // Flush the stderr handle so our banners land before
                // the abort kills the process.
                use std::io::Write;
                let _ = std::io::stderr().flush();

                // Final cancellation check: `run()` may have completed during
                // the post-dump grace window. Don't abort a finished run.
                if watchdog_completed_for_thread.load(std::sync::atomic::Ordering::SeqCst) {
                    return;
                }
                std::process::abort();
            })
            .context("failed to spawn stack-dump watchdog thread")?;
        // Arm the RAII guard so the completed flag is set on every exit path
        // of `run()` once the watchdog is live.
        _watchdog_completion_guard = Some(WatchdogCompletionGuard {
            flag: std::sync::Arc::clone(&watchdog_completed),
        });
        eprintln!("[cratonvm] stack-dump watchdog armed: will dump + abort after {secs}s");
    }

    // T14: Run System.initPhase1() when booting from real JDK classes
    // BEFORE loading the user main class.
    //
    // In HotSpot this is called from Threads::create_vm() after the
    // bootstrap classloader is initialized but before any user class is
    // resolved. It sets up system properties, encodings, and the standard
    // I/O streams (System.in/out/err).
    //
    // Previously the main class was loaded first, which forced eager
    // resolution of `java/lang/Object`, `String`, etc. under an
    // uninitialised system-properties / charset subsystem. A
    // NoClassDefFoundError originating in the half-bootstrapped JDK then
    // surfaced as the confusing "Could not find or load main class …"
    // instead of a clean bootstrap error.
    if vm.shared.config.java_home.is_some() {
        match vm.invoke("java/lang/System", "initPhase1", "()V", &[]) {
            Ok(_) => {
                tracing::info!("System.initPhase1() completed");
                // WP1.3: initPhase1 just finished — advance to level 2.
                vm.shared.set_init_level(2);
            }
            Err(e) => {
                // T14: initPhase1 may fail mid-bootstrap on real JDK 25 due to
                // subsystems we don't fully emulate (e.g. Unsafe accessors hitting
                // uninitialized reference slots).  The fallback path uses our
                // synthetic System.in/out/err so stdout/stderr still work.
                if let cratonvm_vm::error::MethodCallFailed::ExceptionThrown(exc_ref) = &e {
                    let exc_class_id = vm.shared.mem.heap.class_id_of(*exc_ref);
                    let exc_class_name = vm
                        .shared
                        .classes
                        .class_manager
                        .read()
                        .get_class(exc_class_id)
                        .map(|c| c.name.to_string())
                        .unwrap_or_else(|| format!("unknown({})", exc_class_id));
                    tracing::info!(
                        "System.initPhase1() fell back to synthetic streams ({exc_class_name})"
                    );
                } else {
                    tracing::info!("System.initPhase1() fell back to synthetic streams");
                    tracing::debug!("initPhase1 details: {e:?}");
                }
                // initPhase1 may have partially initialized classes and populated stale
                // invoke/resolution cache entries (e.g. real JDK PrintStream bytecode
                // cached for a synthetic 1-slot object). Clear both caches so the next
                // invocation re-resolves cleanly via the native-override registry.
                vm.main_thread.invoke_cache.clear();
                vm.shared.classes.resolution_cache.write().clear();
                // WP1.3: even though initPhase1 threw mid-flight, the
                // early system-properties / stream installation ran
                // before the failure — enough for callers gated on
                // level 2 (e.g. `java.class.path` availability) to
                // proceed.  Bump anyway so downstream `initLevel()`
                // observers don't stall at 1.
                vm.shared.set_init_level(2);
            }
        }

        // Initialise the module system. This is HotSpot's `System.initPhase2`
        // slot in the boot sequence, and it is the only place it belongs.
        //
        // WP1.3: initPhase2 / initPhase3 are pure-Java methods on
        // `java.lang.System`. `initPhase2(boolean, boolean)` is
        // `ModuleBootstrap.boot()` plus `VM.initLevel(2)`; `initPhase3`
        // installs `ClassLoader.scl` and sets the TCCL. We do not invoke
        // either, and the reason for initPhase2 is now a MEASUREMENT rather
        // than a policy — see docs/known-issues/jdk-only/W7-97-initphase2-skipped.md.
        //
        // Measured 2026-08-12, one process per arm, by invoking
        // `System.initPhase2` reflectively inside the VM under test: it
        // reaches `ModuleBootstrap.boot()` → `SystemModuleFinders.ofSystem()`
        // → `ofModuleInfos()` → `ImageReader.getModuleNames()` and dies inside
        // `ImageReader$SharedImageReader.imageFileAttributes()` with
        // `UncheckedIOException` / `NoSuchFileException: ` — an EMPTY path —
        // returning JNI_ERR (-1) and changing NOTHING: the provider counts
        // below stay at 0. So running it is not a fix we merely have not
        // written; it is a route that is currently closed.
        //
        // WHY it is closed, precisely, because that is the durable part:
        // `ImageReaderFactory` is boot-loader-defined, so it builds the
        // runtime-image path with
        // `sun.nio.fs.DefaultFileSystemProvider.theFileSystem().getPath(...)`
        // rather than `FileSystems.getDefault()`. In CratonVM those are two
        // DIFFERENT `WindowsFileSystem` instances (HotSpot: one, identity-equal),
        // and `Path`s minted by the boot-loader one are inert — `Files.exists`
        // answers false, `readAttributes` throws `NoSuchFileException`, and
        // `equals` against the same path from the default filesystem is false,
        // for a `Path` whose own `toString()` is correct. Repairing that is the
        // prerequisite for ever running the real `initPhase2`, and it is not in
        // this file.
        //
        // What DOES initialise the module system here is `ModuleLayer.boot()`.
        // Its native (`register_jboss_jdkspecific`, last-writer-wins over the
        // `phases_late` stub, both `NativeKind::Bridge` so strict mode keeps
        // them) runs `build_boot_layer` → `populate_boot_layer_modules` →
        // `ServicesCatalog.getServicesCatalog(scl).register(module)` for every
        // registered module. That IS this VM's `ModuleBootstrap.boot()`. Like
        // the JDK's it runs exactly once, and it runs HERE — before the level
        // advances below — because `VM.initLevel(4)` wakes every
        // `awaitInitLevel` waiter, and a thread woken at SYSTEM_BOOTED is
        // entitled to assume the module system came up at level 2. Its one
        // externally ordered dependency is `ClassLoader.getSystemClassLoader()`,
        // which is a native with no init-level gate, so it does not need the
        // bump that follows.
        //
        // Until this runs, `ServiceLoader` returns ZERO module-declared
        // providers while classpath `META-INF/services` providers keep working
        // — which is exactly why a `ServiceLoader` probe reads green and this
        // stayed hidden. Measured under `--jdk-only`:
        // `java.nio.file.spi.FileSystemProvider` 2 → 0,
        // `java.util.spi.ToolProvider` 9 → 0, `javax.tools.JavaCompiler` 1 → 0,
        // and `ToolProvider.getSystemJavaCompiler()` null, which sent H2's
        // `SourceCompiler` down a `com.sun.tools.javac` path HotSpot never
        // takes. In `--real-jdk` the `SyntheticStub` `ServiceLoader` natives
        // covered the gap; `--jdk-only` refuses that kind at registration, so
        // the same omission stopped being invisible and became a wrong answer.
        //
        // The failure is NOT swallowed. HotSpot treats a non-zero `initPhase2`
        // as fatal to VM creation. We warn rather than abort because this
        // substitute is narrower than the JDK's phase — an application that
        // never looks up a service is unharmed — but the operator is told,
        // because from here on every module-declared provider is silently
        // absent.
        match vm.invoke(
            "java/lang/ModuleLayer",
            "boot",
            "()Ljava/lang/ModuleLayer;",
            &[],
        ) {
            Ok(Some(Value::Object(Some(_)))) => {
                tracing::info!("module system initialised (ModuleLayer.boot)");
            }
            Ok(_) => {
                tracing::warn!(
                    "module-system init produced no boot layer — every module-declared \
                     ServiceLoader provider will be missing (HotSpot aborts VM creation \
                     when System.initPhase2 returns non-zero)"
                );
            }
            Err(e) => {
                tracing::warn!(
                    "module-system init failed ({e:?}) — every module-declared \
                     ServiceLoader provider will be missing (HotSpot aborts VM creation \
                     when System.initPhase2 returns non-zero)"
                );
            }
        }
    }

    // WP1.3: right before `main()` starts, advance to level 4 —
    // HotSpot's "VM fully initialized" state.  This is the signal
    // that `ClassLoader.getSystemClassLoader()` may return `scl` if
    // it is populated (in cratonvm it usually isn't, so callers fall
    // through to `getBuiltinAppClassLoader()` without harm), that
    // `Thread.currentThread().getName()` is safe, and that every
    // subsystem keyed on `VM.awaitInitLevel(4)` can proceed.  We
    // also briefly pass through level 3 so any probe between 2 and
    // 4 observes a level 3 transition.
    vm.shared.set_init_level(3);
    vm.shared.set_init_level(4);

    // spring-bug-05: advance the REAL `jdk.internal.misc.VM.initLevel` static
    // field. `set_init_level` above only updates CratonVM's internal counter and
    // the `VM.initLevel()` *method* native — but `VM.isModuleSystemInited()`
    // (Proxy.java's gate at ProxyBuilder) reads the static *field* directly
    // (`initLevel >= MODULE_SYSTEM_INITED`). Since we never run the real
    // `System.initPhase2/3` (which would call `VM.initLevel(int)`), the field
    // stays 0 and every JDK dynamic Proxy throws `InternalError: Proxy is not
    // supported until module system is fully initialized`. The setter
    // `VM.initLevel(I)V` is real bytecode (not natively shadowed): invoking it
    // sets the field to SYSTEM_BOOTED (4) and wakes `awaitInitLevel` waiters,
    // exactly as a fully-booted HotSpot would. Real-JDK mode only; errors are
    // swallowed (synthetic mode has no such class/method).
    if vm.shared.config.java_home.is_some() {
        let _ = vm.invoke(
            "jdk/internal/misc/VM",
            "initLevel",
            "(I)V",
            &[Value::Int(4)],
        );
    }

    // The boot module layer is materialised in the `System.initPhase2` slot
    // above, before the level advances — NOT here. It used to be invoked at
    // this point as a behavioural patch; that placement left a window in which
    // `VM.initLevel(4)` had already woken every `awaitInitLevel` waiter while
    // the services catalog was still empty. There is exactly one such call.

    // Pre-allocate the singleton java.lang.OutOfMemoryError while the heap is
    // still fresh, so a later 100%-full-heap OOM (in either user code or a
    // premain) can be thrown without allocating the throwable — which would
    // otherwise hard-abort in the non-fallible String allocator. Idempotent and
    // best-effort: if the class isn't loadable yet it leaves the slot empty and
    // the OOM paths keep their prior behaviour.
    cratonvm_vm::runtime::exceptions::ensure_singleton_oom(&vm.shared, &mut vm.main_thread);

    // WP2.4-C — run every `-javaagent:` agent's `premain(String,
    // Instrumentation)` hook BEFORE the application's `main`. Per the
    // `java.lang.instrument` package spec, agent failures are warnings
    // (logged inside the dispatcher) unless the agent throws a fatal
    // `Error`, in which case we abort here.
    if !java_agents.is_empty() {
        let res = cratonvm_vm::runtime::agent_loader::invoke_premains(
            &vm.shared,
            &mut vm.main_thread,
            &java_agents,
        );
        if let Err(e) = res {
            finish_jdk_only(&args, &vm.shared, &mut jdk_only_watermark);
            bail!("javaagent premain aborted VM: {e}");
        }
    }

    // Resolve and load the user main class.
    //
    // Two orderings constrain this point. It must come after `initPhase1` (see
    // the comment on that block above) — that is why it is not next to the
    // classpath setup. And it must come after `invoke_premains`, because the
    // `java.lang.instrument` contract is that a transformer registered by
    // `premain` is offered *every subsequent definition*, and the application's
    // main class is the first one an agent expects to see. HotSpot starts its
    // agents during VM creation and loads the main class afterwards, from the
    // launcher; loading it first made the main class the one class a
    // `-javaagent:` could never instrument. `load_class_transformed` is the
    // entry point that offers the bytes to the chain (a no-op with no agent).
    if let Err(e) = vm
        .shared
        .load_class_transformed(&mut vm.main_thread, &class_name)
    {
        // JDK-only: this is the likeliest strict-mode failure — the main class
        // (or something it needs) was refused rather than fabricated. The
        // census must survive it, or `difftest` cannot categorise the failure
        // it was launched to produce.
        finish_jdk_only(&args, &vm.shared, &mut jdk_only_watermark);
        // `class_name` is in internal slash form here; the inner error `e`
        // typically embeds that same slash-form name, so render both dotted so
        // the message doesn't show the class two ways (`com.example.Main` then
        // `class not found: com/example/Main`). HotSpot reports the binary
        // (dotted) name throughout.
        bail!(
            "Could not find or load main class {}: {}",
            class_name.replace('/', "."),
            e.to_string().replace('/', ".")
        );
    }

    // Build String[] args array for main(String[]).
    //
    // Built here, not earlier: `args_array` is a bare `ObjectRef` held on the
    // Rust stack, which no GC root provider knows about, so every allocating
    // Java call between its creation and the `main` invoke is a window in which
    // a young collection could move it. Constructing it after `premain` and the
    // main-class load — the two arbitrarily-large stretches of Java on this path
    // — leaves only the invoke itself.
    let java_args: Vec<Value> = args
        .args
        .iter()
        .map(|a| Value::Object(Some(create_java_string(&vm.shared, a))))
        .collect();

    // Resolve the `java/lang/String` class id for the args array's
    // element type. Previously this was hard-coded to `ClassId::new(0)`
    // (which is `java/lang/Object` — the base reference array element
    // type), but JLS / JVMS require `main(String[])` to receive a
    // `[Ljava/lang/String;` array, NOT `[Ljava/lang/Object;`. Code
    // that inspects `args.getClass().getComponentType()` (e.g. test
    // harnesses, generic helpers) observes the wrong component class
    // when ClassId(0) is used.
    //
    // `load_class_concurrent` is idempotent and the boot classloader
    // resolves `String` extremely early, so this is effectively a
    // hash-table lookup.
    let string_array_class_id = vm
        .shared
        .load_class_concurrent("java/lang/String")
        .unwrap_or_else(|_| cratonvm_vm::ClassId::new(0));
    let args_array = vm.shared.mem.heap.alloc_array(
        string_array_class_id,
        cratonvm_vm::memory::heap::ArrayElementType::Reference,
        java_args.len(),
    );
    for (i, val) in java_args.into_iter().enumerate() {
        vm.shared
            .mem
            .heap
            .set_array_element(args_array, i, val)
            .map_err(|idx| {
                anyhow::anyhow!("Failed to set args array element {i} (index {idx} out of bounds)")
            })?;
    }

    // Phase accounting: startup ends here, execution begins. This is the
    // single span the `coarse` level's reconciliation rests on — every other
    // category nests inside it and subtracts (see §10.5 of
    // `docs/observability/phase-accounting.md`). At `fine` it records nothing
    // and the `interpretation` / `jit_execution` spans carry the split instead,
    // which is why it can be opened unconditionally.
    //
    // `catch_unwind` swallows a panic rather than unwinding through these, so
    // both `.end()` calls below are reached on every path out of the invoke.
    phase_startup.end();
    let phase_exec = phase::enter(phase::Category::JavaExecution);

    // Invoke main(String[])
    let main_start = std::time::Instant::now();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        vm.invoke(
            &class_name,
            "main",
            "([Ljava/lang/String;)V",
            &[Value::Object(Some(args_array))],
        )
    }));
    let main_elapsed = main_start.elapsed();
    phase_exec.end();
    // Everything from here to the end of `run()` is teardown: the census
    // dumps, the non-daemon-thread join, the exception renderer. Bound to a
    // named guard held to end of scope so the `bail!` paths below charge it
    // too.
    let _phase_shutdown = phase::enter(phase::Category::VmShutdown);
    tracing::info!("main() completed in {:.2}s", main_elapsed.as_secs_f64());

    // W7-90: the read-side slot-map sweep's PRIMARY trigger.
    //
    // `read_alias::verify_declared_slot_maps` had no caller at all until this
    // line, which is indistinguishable from a detector that reports all-clear —
    // the species this whole campaign is about, sitting inside the instrument
    // built to detect it. Seven `SlotMap`s were published to a sweep that never
    // ran (W7-69 §7.2, W7-75 §7, W7-77 §7).
    //
    // Here rather than at registration because the sweep's one hard requirement
    // is that the class be LOADED, and W7-69 established that at registration
    // time most are not: `java.nio.DirectByteBuffer` is package-private and is
    // not among the 323 classes `bootstrap_core_classes` names, so its
    // `declared_fields` comes back empty and "not loaded" is indistinguishable
    // from "no fields". Immediately after `main` returns is the point in the
    // process with the most classes loaded, and it is where every other
    // self-gated census in this launcher already prints.
    //
    // ABOVE the `match result` deliberately, so a workload that PANICKED out of
    // `main` still produces its census — the same argument the missing-natives
    // dump below makes for being unconditional. `System.exit` never reaches
    // this line; `lang_system::native_system_exit` carries the second trigger
    // for that path.
    //
    // Gated with no `else`: observation only, and the report is printed and
    // dropped. With the flag off this is one `OnceLock` load and a branch, once.
    if cratonvm_native_api::layout_alias::enabled() {
        let _ = vm.sweep_declared_slot_maps("main-returned");
    }

    let result = match result {
        Ok(r) => r,
        Err(panic) => {
            let msg = panic
                .downcast_ref::<&str>()
                .map(|s| s.to_string())
                .or_else(|| panic.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| "unknown panic".into());
            finish_jdk_only(&args, &vm.shared, &mut jdk_only_watermark);
            bail!("main() panicked: {msg}");
        }
    };

    // NEW-10: before reporting the invocation result, write the
    // missing-natives audit log to the user-specified JSON path. We
    // do this unconditionally (regardless of Ok/Err) so a crashing
    // program still produces a census file. Any I/O error surfaces
    // as a warning — the primary invocation result takes precedence.
    if let Some(path) = &args.dump_missing_natives {
        match vm.shared.dump_missing_natives_json(path) {
            Ok(()) => {
                let count = vm.shared.get_missing_natives().len();
                eprintln!("[cratonvm] wrote {count} missing-native entries to {path}");
            }
            Err(e) => {
                eprintln!(
                    "[cratonvm] warning: could not write missing-natives JSON to {path}: {e}"
                );
            }
        }
    }

    // JDK-only census artefacts: the class-origin dump, the schema-2 native
    // registry census and the violation report, all three written by the
    // `SharedVm` methods that own them (`dump_class_origins_json`,
    // `dump_native_census_json`, `dump_jdk_only_report_json`) — the launcher
    // supplies the paths and the `--explain-jdk-only` flag and nothing else.
    // Written unconditionally, exactly like the missing-natives dump above, so
    // a program that threw still produces a census. The failing paths earlier
    // in this function write them too.
    finish_jdk_only(&args, &vm.shared, &mut jdk_only_watermark);

    // T2.1.3: grouped-by-module census.
    if let Some(path) = &args.dump_missing_natives_grouped {
        match vm.shared.dump_missing_natives_grouped_json(path) {
            Ok(()) => {
                let grouped = vm.shared.classify_missing_natives_by_module();
                let total: usize = grouped.values().map(|v| v.len()).sum();
                eprintln!(
                    "[cratonvm] wrote {total} missing-native entries across \
                     {} modules to {path}",
                    grouped.len()
                );
            }
            Err(e) => {
                eprintln!(
                    "[cratonvm] warning: could not write grouped missing-natives \
                     JSON to {path}: {e}"
                );
            }
        }
    }

    // A counter nobody can read is not a diagnostic. This reports under the
    // flag that PRODUCED the numbers, not under `intrinsic-stats`, so
    // `CRATONVM_DBG_SITE_ALIAS=1` alone is enough to get the totals.
    if cratonvm_types::flags::runtime_var_os("CRATONVM_DBG_SITE_ALIAS").is_some() {
        eprintln!(
            "[cratonvm] site-alias: distinct JitSiteKeys={} recycled-key hits={}",
            cratonvm_vm::jit::helpers::site_alias_key_count(),
            cratonvm_vm::jit::helpers::site_alias_hit_count()
        );
    }

    // Interpreter intrinsic-table stats. `CRATONVM_INTRINSIC_STATS=1` prints
    // the steady-state intrinsic-dispatch hit count on shutdown — the
    // counter that verifies acceptance criterion §9 of
    // gaps/feature_roadmap_interpreter_intrinsic_table.md.
    if matches!(
        std::env::var("CRATONVM_INTRINSIC_STATS").as_deref(),
        Ok("1")
    ) {
        eprintln!(
            "[cratonvm] interpreter intrinsic dispatches: {}",
            cratonvm_vm::runtime::interpreter::intrinsic_hit_count()
        );
        // Compiled code's own funnel bypass. Reported beside the interpreter's
        // counter because the two answer the same question for different
        // execution tiers, and the JIT half is the one that was missing: a
        // compiled loop's calls do not reach the interpreter's inline cache, so
        // a run whose first number moves and whose second stays at zero has NOT
        // been sped up where it is hot. See `jit::helpers::LEAF_NATIVE_HITS`.
        eprintln!(
            "[cratonvm] compiled leaf-native dispatches: {}",
            cratonvm_vm::jit::helpers::leaf_native_hit_count()
        );
        // The non-leaf half of the same cache: these skipped `invoke_or_native`
        // but still entered the funnel. Reported separately because it is the
        // larger population and the one that carries `java.util.concurrent`.
        eprintln!(
            "[cratonvm] compiled site-cached native dispatches (non-leaf): {}",
            cratonvm_vm::jit::helpers::site_cached_native_hit_count()
        );
        // The subset of the line above served as a plain field load rather
        // than a native call: signature-polymorphic `VarHandle` read modes on
        // an ordinary instance field. Zero here beside a non-zero site-cached
        // count means every VarHandle site refused the plan, which is a
        // different fault from "no VarHandle site was reached".
        eprintln!(
            "[cratonvm]   of which VarHandle instance-field reads served directly: {}",
            cratonvm_vm::jit::helpers::varhandle_field_read_hit_count()
        );
        // The same read, reached WITHOUT the funnel — a thin direct call baked
        // into compiled code by `VARHANDLE_READ_DIRECT_FNS`. The pair is the
        // engagement evidence for that bind: `served` counts calls that never
        // entered `jit_invoke_dispatch` at all, `declined` counts the ones that
        // did. A non-zero bind count in the "thin direct-helper binds" line with
        // `served=0` here is the specific failure this exists to name — the site
        // compiled and every execution refused.
        {
            let (served, declined) = cratonvm_vm::jit::helpers::varhandle_read_direct_counts();
            eprintln!(
                "[cratonvm] VarHandle read thin direct calls: served={served} declined={declined}",
            );
        }
        // The WRITE direction's two routes, which the read direction has had all
        // along and this one did not. `write thin direct calls` is the bind
        // baked into compiled code; `field CAS in-funnel` is the short circuit
        // inside `jit_invoke_dispatch`, the twin of the read fast path above.
        //
        // The CAS pair is load-bearing in a way the census is NOT: a served CAS
        // still calls `count_jit_native_dispatch`, exactly as a served read
        // does, so `--dump-native-registry` reports the SAME
        // `VarHandle.compareAndSet` count whether the fast path ran or not.
        // These counters are the only way to tell the two apart.
        {
            let (served, declined) = cratonvm_vm::jit::helpers::varhandle_write_direct_counts();
            eprintln!(
                "[cratonvm] VarHandle write thin direct calls: served={served} declined={declined}",
            );
            let (cas_served, cas_declined) =
                cratonvm_vm::jit::helpers::varhandle_field_cas_counts();
            eprintln!(
                "[cratonvm] VarHandle field CAS in-funnel: served={cas_served} declined={cas_declined}",
            );
            // The bound CAS route. Read it TOGETHER with the in-funnel pair
            // above: once `compareAndSet` sites bind, the in-funnel count is
            // supposed to fall to what the interpreter and the declined sites
            // still send through it, and a bind that moved nothing looks
            // exactly like a bind that was never reached unless both are
            // printed.
            let (cd_served, cd_declined) =
                cratonvm_vm::jit::helpers::varhandle_cas_direct_counts();
            let (cd_sp, cd_osr) = cratonvm_jit::varhandle_cas_direct_helper_sites();
            eprintln!(
                "[cratonvm] VarHandle CAS thin direct calls: served={cd_served} declined={cd_declined}                  (sites: singlepass={cd_sp} osr={cd_osr})",
            );
        }
        // The exact-receiver `java/util/regex/Matcher` leaf, which is neither of
        // the two above: it is the one by-name fast path that decides per
        // dispatch rather than at cache-fill time. Reported separately because
        // its number is the only thing that distinguishes "the leaf served this
        // call" from "the leaf declined and the generic tail served it, counting
        // it properly" — the two are indistinguishable in the native census,
        // which is what left the §4 gap this leaf was filed for unfalsifiable
        // for as long as it stood.
        eprintln!(
            "[cratonvm] compiled Matcher-leaf dispatches: {}",
            cratonvm_vm::jit::helpers::matcher_leaf_hit_count()
        );
        // A zero above is ambiguous — "nothing here is a leaf" and "every site
        // was refused for a reason nobody intended" look identical — so the
        // fill-time refusal reasons are reported alongside it.
        for (reason, count) in cratonvm_vm::jit::helpers::leaf_native_refusals() {
            eprintln!("[cratonvm]   leaf sites refused, {reason}: {count}");
        }
        // `Thread.currentThread()` is served one level earlier still: the
        // compilers bake a direct `CALL` to `jit_thread_current_thread_direct`,
        // so those sites never reach the leaf path above, or any dispatch
        // helper at all. The per-door site counts prove the bind is not inert;
        // the denominators are what named the compile door that was missing.
        // See `native-call-funnel-per-call-floor-item2-20260805.md`.
        let (sp_sites, ir_sites, osr_sites) = cratonvm_jit::thread_current_thread_bound_sites();
        let (sp_seen, ir_seen) = cratonvm_jit::static_sites_seen();
        eprintln!(
            "[cratonvm] compiled Thread.currentThread direct calls: {} \
             (sites bound per compile door: single-pass {sp_sites}/{sp_seen}, \
             IR {ir_sites}/{ir_seen}, OSR {osr_sites}; the two denominators are \
             invokestatic sites those ladders examined)",
            cratonvm_vm::jit::helpers::jit_funnel_bypass_count()
        );
        // JNI up-calls that found no thread context. Non-zero means some native
        // called back into Java with the context torn down under it — the shape
        // that lost netty's TLSv1.3 client certificate. Zero is expected for any
        // run whose native calls all originate from Java.
        eprintln!(
            "[cratonvm] JNI up-calls answered with NO thread context: {}",
            cratonvm_vm::native::jni::jni_upcalls_without_context()
        );
        // Why a compiled callee is reached through a Rust helper at all.
        //
        // The netty census (`[DISP_CENSUS]`, `CRATONVM_DBG=mic-prof`) says 98.4%
        // of `jit_invoke_dispatch` calls are `DISPATCH_CACHE` hits — a callee
        // that IS compiled, entered through the helper on every call. These two
        // say why: a statically bound site is offered a direct `CALL` exactly
        // once, while its caller is being compiled, and a callee that is not
        // compiled yet at that instant leaves the site on the helper forever.
        let (bind_hits, bind_misses) = cratonvm_jit::direct_callee_bind_counts();
        eprintln!(
            "[cratonvm] direct callee binds: {bind_hits} bound, {bind_misses} left on the dispatch helper (statically bound sites where a ladder asked for a direct target)"
        );
        // The OSR door's share of that pair. It is a SUBSET of the line above,
        // not a third total.
        //
        // Printed separately because its absence was mistaken for its answer:
        // `osr-door-refused-every-exception-table-callee-FIXED-20260824.md` read
        // `0 bound, 0 left` in both arms of a gate flip and concluded the gate
        // is unreachable from an OSR body. The conclusion was right and the
        // instrument was not -- the OSR ladder was binding and refusing all
        // along and reporting neither, so a door that reported nothing and a
        // door that did nothing printed the same line. A hot loop is always an
        // OSR body, so a zero HERE is the one worth noticing.
        let (osr_hits, osr_misses) = cratonvm_jit::osr_direct_callee_bind_counts();
        eprintln!(
            "[cratonvm]   of which the OSR door: {osr_hits} bound, {osr_misses} left on the dispatch helper"
        );
        // WHICH gate refused. A bare miss total cannot separate a compile-ORDER
        // accident (the callee simply was not compiled yet — repairable by
        // re-binding) from a standing policy refusal (an exception table, a
        // native shadow — which no re-bind touches), and those two want
        // opposite fixes. `unattributed` is the mutator-side door's arms that
        // return a bare `None`; a large value there means this list is the one
        // to extend next, not that the misses are unexplained.
        let reasons = cratonvm_jit::direct_callee_bind_refusal_reasons();
        let attributed: u64 = reasons.iter().map(|(_, c)| *c).sum();
        for (reason, count) in &reasons {
            eprintln!("[cratonvm]   bind refused, {reason}: {count}");
        }
        eprintln!(
            "[cratonvm]   bind refused, unattributed: {}",
            bind_misses.saturating_sub(attributed)
        );
    }

    // JDK-ONLY-WAVE2 §8/§11 census dumps (`CRATONVM_DBG_CHECK_OVERRIDE=1`).
    // Both are no-ops without the flag; they exist to drive the per-family
    // deletion exercise those two records describe but never measured.
    cratonvm_vm::vm::dump_check_override_census();
    cratonvm_vm::vm::dump_canonical_census();
    // The enforcement dial's per-door census (`H17-3` N1). Silent unless
    // `CRATONVM_ENFORCE_NATIVE_SHADOW` is armed or `CRATONVM_DBG_DIAL_DOORS`
    // is set; `reached - yielded` per door is the price the dial is not
    // charging.
    cratonvm_vm::vm::dump_dial_door_census();

    // WS1 diagnostic: final JIT-dispatch-helper profile dump on shutdown
    // (env-gated inside `dump_now` callers; `enabled()` re-checked here).
    if cratonvm_vm::jit::helpers::mic_prof::enabled() {
        cratonvm_vm::jit::helpers::mic_prof::dump_now();
        eprintln!(
            "[MIC_PROF] gc_collections={} total_dispatches={}",
            vm.shared.mem.heap.collection_count(),
            cratonvm_vm::dispatch_trace::total_dispatches()
        );
    }

    // B6: Silent-exit guard. If main() returned Ok but the VM has recorded
    // one or more swallowed errors during class init / invokedynamic / native
    // calls, surface a WARN to stderr so users (and CI) don't mistake a
    // silent exit for a successful run. Exit code stays 0 for compatibility
    // with programs that legitimately produce no stdout.
    if matches!(result, Ok(_)) {
        let swallowed = vm
            .shared
            .debug
            .swallow_counter
            .load(std::sync::atomic::Ordering::Relaxed);
        if swallowed > 0 {
            eprintln!(
                "WARN: main() completed with {swallowed} swallowed VM error(s) \
                 (class-init / invokedynamic / native). Re-run with \
                 RUST_LOG=warn (already default) to see each site, or \
                 CRATONVM_STRICT_SWALLOWS=1 to escalate the first swallow to a \
                 panic for diagnosis."
            );
        }
    }

    // §5 acceptance metric — aggregate G1 pause summary (p50/p99/max young +
    // mixed) to stderr at shutdown when GC stats are requested. Driven by
    // `--verbose:gc` or the `CRATONVM_GC_STATS` env knob so a gauntlet runner
    // can collect the table without `RUST_LOG`. No-op for the generational
    // collector and when no G1 collection ran.
    // `CRATONVM_DBG_G1ACCESSOR` too: the accessor census rides inside
    // `print_gc_summary`, and a census whose only output path is gated behind a
    // DIFFERENT flag prints nothing when you ask for it — which reads as
    // "zero accessor calls" rather than "you never enabled the report".
    let gc_stats_requested = args.verbose_gc
        || std::env::var_os("CRATONVM_GC_STATS").is_some()
        || cratonvm_types::flags::flags().gc.g1_dbg_accessor;
    // A stale or refused arena translation is a correctness event, so it is
    // reported whether or not anyone asked for statistics -- gating one behind
    // a stats flag turns "nobody asked" into "nothing happened". With the flag
    // on, the line prints unconditionally so the DENOMINATOR is available too.
    cratonvm_native_builtins::arena_translation_exit_summary(gc_stats_requested);
    if gc_stats_requested {
        vm.shared.mem.heap.print_gc_summary();
        {
            // Cross-thread STW peer-scan coverage. A non-zero count means the
            // collector swept while a peer it could not classify was still
            // running JIT code, i.e. that cycle marked from an INCOMPLETE root
            // set. See audits/old-sweep-liveness.md.
            use std::sync::atomic::Ordering as O;
            let peers = cratonvm_vm::jit::xt_root_scan::XT_PEERS_UNCLASSIFIED.load(O::Relaxed);
            let cycles =
                cratonvm_vm::jit::xt_root_scan::XT_CYCLES_WITH_UNCLASSIFIED.load(O::Relaxed);
            // H2-CID0 (2026-08-05): UNCONDITIONAL. This used to print only when
            // `peers > 0`, which made "every peer answered" indistinguishable
            // from "the take-over never ran" — and a run that FAILED printed
            // nothing, which reads as the reassuring one and was the other.
            // `taken_over` is what separates them.
            let taken = cratonvm_vm::jit::xt_root_scan::XT_THREADS_TAKEN_OVER.load(O::Relaxed);
            let roots = cratonvm_vm::jit::xt_root_scan::XT_ROOTS_FOUND.load(O::Relaxed);
            let hw = cratonvm_vm::jit::xt_root_scan::XT_HELPER_WINDOWS_SCANNED.load(O::Relaxed);
            let resig = cratonvm_vm::jit::xt_root_scan::XT_PEER_RESIGNALS.load(O::Relaxed);
            let saved =
                cratonvm_vm::jit::xt_root_scan::XT_PEERS_CLASSIFIED_AFTER_RETRY.load(O::Relaxed);
            let hw_pin =
                cratonvm_vm::jit::xt_root_scan::XT_HELPER_WINDOWS_PINNED.load(O::Relaxed);
            let hw_ref =
                cratonvm_vm::jit::xt_root_scan::XT_HELPER_WINDOWS_REFUSED.load(O::Relaxed);
            eprintln!(
                "[GC] xt_peer_scan: unclassified_peers={peers} cycles_with_unclassified={cycles} \
                 taken_over={taken} xt_roots={roots} helper_windows={hw} hw_pinned={hw_pin} hw_refused={hw_ref} \
                 resignals={resig} classified_after_retry={saved} enabled={}",
                cratonvm_vm::jit::xt_root_scan::enabled(),
            );
            // H2-CID0 (2026-08-05): the unregistered-JIT-frame memo's audit.
            // `suppressed` counts times the memo answered "clean" while a scan
            // of the same range found a frame — i.e. oops that went unmarked
            // and a cycle that was never told to avoid moving them.
            // `shortcircuits` is the denominator: without it a zero cannot be
            // told apart from an audit that never ran.
            let sc =
                cratonvm_vm::jit::conservative_roots::UNREG_MEMO_SHORTCIRCUITS.load(O::Relaxed);
            let sup = cratonvm_vm::jit::conservative_roots::UNREG_MEMO_SUPPRESSED.load(O::Relaxed);
            let supa = cratonvm_vm::jit::conservative_roots::UNREG_MEMO_SUPPRESSED_AUTHORITATIVE
                .load(O::Relaxed);
            // `SUPPRESSED_AUTHORITATIVE` is the one that judges the fix: total
            // suppressions are dominated by ordinary per-native-call snapshots,
            // which no collector marks from.
            eprintln!(
                "[GC] unreg_memo: shortcircuits={sc} SUPPRESSED={sup} \
                 SUPPRESSED_AUTHORITATIVE={supa}"
            );
        }
    }

    // Per-segment `getstatic` cost, plus whether the lock-free statics index is
    // actually being hit (`CRATONVM_DBG_GETSTATIC_PROF=1`).
    cratonvm_vm::jit::helpers::gs_prof::dump();

    // T19.K1 — wait for non-daemon threads before exiting.
    //
    // Per the JVM specification, the VM keeps running until every
    // non-daemon thread has terminated. Daemon threads (GC workers,
    // event loops, finalisers) are best-effort: when the last
    // non-daemon thread finishes, the VM exits, abandoning any
    // remaining daemons.
    //
    // For HelloWorld and any program that doesn't `Thread.start()`
    // a user thread, `wait_for_non_daemon_threads` returns
    // immediately (the snapshot is empty). For Quarkus / Keycloak,
    // the embedded HTTP listener and Vert.x worker pool are
    // non-daemon; this loop blocks until they exit.
    //
    // We only wait when `main()` returned cleanly (`Ok`). On
    // exception we propagate to the existing error-printing path
    // which calls `bail!()` and lets the process exit with non-zero
    // status — same as HotSpot's "Exception in thread \"main\"".
    // Waiting for daemons or worker threads after a fatal error
    // would just delay the stack trace.
    //
    // The deadline parameter is `None` (wait indefinitely): a
    // legitimate non-daemon thread could be a long-running service
    // and bounding the wait would surprise users. CI runs that need
    // to bound execution can use the existing
    // `--stack-dump-on-timeout` watchdog which `abort()`s the
    // process from a separate thread — this loop will be
    // interrupted by the watchdog's `process::abort()` call.
    if matches!(result, Ok(_)) {
        // T19.K1 — diagnostic only when the wait is actually
        // observable (i.e. there ARE non-daemon threads). HelloWorld
        // and any program that doesn't `Thread.start()` a user
        // thread skips this message and exits silently. Long-running
        // apps (Quarkus, Keycloak, embedded Jetty) print one line so
        // the user can tell the wait is what's holding the process
        // alive — useful when a CI run mysteriously sits at "main
        // returned" forever.
        let pending = vm
            .shared
            .threads
            .thread_registry
            .alive_non_daemon_thread_ids()
            .len();
        if pending > 0 {
            eprintln!(
                "[cratonvm] main() returned; VM held alive by {pending} \
                 non-daemon thread(s) (JVM-spec behaviour). Send SIGINT/\
                 SIGTERM, use System.exit(), or pass --stack-dump-on-timeout \
                 to bound execution."
            );
        }
        // GC-barrier fix: this thread never runs Java bytecode again once it
        // reaches this wait (it is parked in a raw `pthread_join` loop, not
        // Java-level parking), so it can never cooperatively reach an
        // interpreter safepoint and call `arrive_and_wait`. Publish the main
        // thread's roots and enter the full per-thread blocked-region protocol
        // for the whole wait, mirroring native socket/pipe waits.
        vm.begin_main_thread_blocking_region("vm-main:wait-non-daemon");
        let joined = vm
            .shared
            .threads
            .thread_registry
            .wait_for_non_daemon_threads(None);
        vm.end_main_thread_blocking_region();
        if joined > 0 {
            tracing::info!("cratonvm: joined {joined} non-daemon thread(s) after main() returned");
        }
    }

    // W7-92 — the launcher's two exit paths: `main` returned, and `main` threw.
    // HotSpot runs shutdown hooks on BOTH (measured, 25.0.3+9: the `normal`,
    // `nondaemon` and `uncaught` rows of the record's oracle table), so this
    // sits ABOVE the `match` for the same reason the slot-map census at :4361
    // does. `System.exit` / `Runtime.exit` never reach this line;
    // `lang_system::native_system_exit` carries the trigger for those.
    //
    // AFTER `wait_for_non_daemon_threads`, and that ordering is not cosmetic:
    // HotSpot's `nondaemon` transcript prints `KEEPER-DONE` BEFORE the hook
    // output. Shutdown does not begin until the last non-daemon thread ends.
    //
    // Called directly rather than through `vm.invoke("java/lang/Shutdown",
    // "runHooks", "()V", &[])`. The Java route would depend on
    // `java/lang/Shutdown` resolving, which is a real-JDK-mode assumption —
    // and a bridge that silently does nothing in synthetic-JDK mode is the
    // exact failure shape this record is about. The native registration on
    // that triple still exists for JDK-side callers; both land on the same
    // drained list, so neither can double-run the hooks.
    //
    // KNOWN ORDERING DIVERGENCE, stated rather than discovered: on the
    // uncaught-exception path HotSpot prints the stack trace and THEN runs the
    // hooks. Here the trace is rendered by `bail!` and printed by `main()`
    // after `run()` returns, so CratonVM's hook output lands BEFORE it. Fixing
    // that means restructuring how the launcher renders a fatal exception,
    // which is a change to output every harness in this tree reads; W7-92 §7
    // records it as the follow-up.
    {
        // PARK THE PENDING THROWABLE WHERE THE COLLECTOR CAN SEE IT.
        //
        // `result` holds an `ObjectRef` — a raw heap address in a Rust local.
        // The hooks below are arbitrary Java: they allocate, and they can
        // collect. Until this slot existed the throwable was reachable from
        // nothing the collector scans, so a collection inside a hook reclaimed
        // it and the render further down read a zeroed header — `ClassId(0)`,
        // which IS `java.lang.Object`. That is
        // `bug-h2-testopenclose-throwable-is-java-lang-object-20260829.md`:
        // `Exception in thread "main" java/lang/Object`, no message, no frames,
        // because the object was gone rather than mis-typed.
        //
        // `uncaught_exception_pending` is rooted AND remapped (roots.rs §10,
        // gc.rs), so this both keeps it alive and gives us its post-move
        // address back after the hooks.
        if let Err(MethodCallFailed::ExceptionThrown(exc_ref)) = &result {
            vm.main_thread.uncaught_exception_pending = Some(*exc_ref);
        }
        let mut ctx = cratonvm_vm::vm::NativeContextImpl {
            shared: &vm.shared,
            thread: &mut vm.main_thread,
        };
        let trigger = if result.is_ok() {
            "main-returned"
        } else {
            "uncaught"
        };
        cratonvm_native_builtins::lang_system::run_shutdown_hooks(&mut ctx, trigger);
    }

    // Take the throwable back at whatever address it now lives at. A moving
    // cycle during the hooks rewrote the slot; `result`'s copy is stale from
    // that moment on, so everything below reads THIS one.
    let relocated_exc = vm.main_thread.uncaught_exception_pending.take();
    let result = match (result, relocated_exc) {
        (Err(MethodCallFailed::ExceptionThrown(_)), Some(moved)) => {
            Err(MethodCallFailed::ExceptionThrown(moved))
        }
        (other, _) => other,
    };

    match result {
        Ok(_) => Ok(()),
        Err(MethodCallFailed::InternalError(e)) => {
            bail!("Error in thread \"main\" {e}");
        }
        Err(MethodCallFailed::ExceptionThrown(exc_ref)) => {
            // Try to read exception class name and message, plus the cause chain
            // so users can see the underlying reason for wrapper exceptions like
            // ExceptionInInitializerError or InvocationTargetException.
            //
            // Also render the captured Java-side stack trace from the
            // `stackTrace` field on each Throwable when present. NOTE: in the
            // current cratonvm, `Throwable.fillInStackTrace` (see
            // `native-builtins/src/lang_misc.rs`) only stashes frames into the
            // VM-wide Throwable trace registry keyed by identity
            // hash — it does NOT populate the heap-side `stackTrace` /
            // `backtrace` field. The Java code only writes that field lazily
            // when something calls `Throwable.getStackTrace()`. For unhandled
            // exceptions that escape `main()`, that has typically never
            // happened, so the renderer below will usually find a null array
            // and emit no `\tat ...` lines. Promoting the synthetic capture
            // to populate the heap field (or wiring this CLI to read from
            // `throwable_stacks` directly) is roadmap item T2.2.18 — see
            // `history/roadmap-100.md` line 471.
            //
            // INTENTIONAL (reviewed): omitting the `\tat ...` frames here is an
            // acceptable, honest degradation — NOT a wrong-result stub. The
            // renderer prints the real exception class, message, and the full
            // `Caused by:` cause chain (all read from live heap fields); only
            // the per-frame stack-trace lines are absent when the heap-side
            // `stackTrace` array was never materialised. We never fabricate
            // synthetic frames, so what is printed is always faithful; the
            // missing frames are a known limitation tracked by T2.2.18, not a
            // silent incorrect value.
            let mut cur = exc_ref;
            let mut lines: Vec<String> = Vec::new();
            let mut prefix = "Exception in thread \"main\"";
            // Cap the cause-chain walk so a self-referential or pathologically
            // deep chain can't loop forever. When the cap is hit with an
            // unrendered cause still pending, a marker line is emitted (see the
            // `next_cause` handling at the end of the loop) so deeply-wrapped
            // exceptions are not silently truncated.
            const MAX_CAUSE_DEPTH: usize = 8;
            for depth in 0..MAX_CAUSE_DEPTH {
                let cid = vm.shared.mem.heap.class_id_of(cur);
                // PERF: resolve the class name AND the Throwable field indices
                // under a single read guard. These were two back-to-back
                // `class_manager.read()` calls; both are pure reads with no
                // intervening work, so one guard is behavior-identical and
                // avoids a redundant lock/unlock per cause-chain iteration.
                //
                // Find fields by name so we work regardless of layout.
                // Also probe `target` (used by InvocationTargetException
                // in lieu of Throwable.cause — see its `getCause()` override)
                // so that `Caused by:` chains still walk through the wrapper.
                let (cname, msg_idx, cause_idx, stack_idx, target_idx) = {
                    let cm = vm.shared.classes.class_manager.read();
                    let cname = cm
                        .get_class(cid)
                        .map(|c| c.name.to_string())
                        .unwrap_or_else(|| "unknown".to_string());
                    let mut msg_i: Option<usize> = None;
                    let mut cause_i: Option<usize> = None;
                    let mut stack_i: Option<usize> = None;
                    let mut target_i: Option<usize> = None;
                    // Walk from Throwable down
                    let mut walk = Some(cid);
                    while let Some(k) = walk {
                        if let Some(cls) = cm.get_class(k) {
                            let mut inst = 0usize;
                            for f in &cls.fields {
                                if !f.is_static() {
                                    let abs = cls.first_field_index + inst;
                                    if &*f.name == "detailMessage" && msg_i.is_none() {
                                        msg_i = Some(abs);
                                    }
                                    if &*f.name == "cause" && cause_i.is_none() {
                                        cause_i = Some(abs);
                                    }
                                    if &*f.name == "stackTrace" && stack_i.is_none() {
                                        stack_i = Some(abs);
                                    }
                                    if &*f.name == "target" && target_i.is_none() {
                                        target_i = Some(abs);
                                    }
                                    inst += 1;
                                }
                            }
                            walk = cls.superclass;
                        } else {
                            break;
                        }
                    }
                    (cname, msg_i, cause_i, stack_i, target_i)
                };
                let message = if let Some(i) = msg_idx {
                    let v = vm.shared.mem.heap.get_field(cur, i);
                    if let Value::Object(Some(s)) = v {
                        cratonvm_vm::vm::read_java_string(&vm.shared.mem.heap, s)
                            .unwrap_or_default()
                    } else {
                        String::new()
                    }
                } else {
                    String::new()
                };
                let line = if message.is_empty() {
                    format!("{prefix} {cname}")
                } else {
                    format!("{prefix} {cname}: {message}")
                };
                lines.push(line);

                // Render `\tat ...` frames from `stackTrace[]` if populated.
                // Each element is a `StackTraceElement` with fields, in
                // declaration order: declaringClass (String), methodName
                // (String), fileName (String, nullable), lineNumber (int).
                // We resolve those four field indices the same way as the
                // Throwable fields above so we work regardless of layout.
                let mut emitted_frames = false;
                if let Some(si) = stack_idx {
                    let stack_val = vm.shared.mem.heap.get_field(cur, si);
                    if let Value::Object(Some(arr)) = stack_val {
                        let len = vm.shared.mem.heap.array_length(arr);
                        if len > 0 {
                            emitted_frames = true;
                            // Resolve StackTraceElement field indices once
                            // from the first non-null element's class.
                            let mut ste_idx: Option<(usize, usize, usize, usize)> = None;
                            for i in 0..len {
                                let elem =
                                    vm.shared.mem.heap.get_array_element(arr, i).ok().and_then(
                                        |v| {
                                            if let Value::Object(Some(o)) = v {
                                                Some(o)
                                            } else {
                                                None
                                            }
                                        },
                                    );
                                let Some(elem_ref) = elem else { continue };
                                if ste_idx.is_none() {
                                    let ecid = vm.shared.mem.heap.class_id_of(elem_ref);
                                    let cm = vm.shared.classes.class_manager.read();
                                    let mut dc: Option<usize> = None;
                                    let mut mn: Option<usize> = None;
                                    let mut fn_: Option<usize> = None;
                                    let mut ln: Option<usize> = None;
                                    let mut walk = Some(ecid);
                                    while let Some(k) = walk {
                                        if let Some(cls) = cm.get_class(k) {
                                            let mut inst = 0usize;
                                            for f in &cls.fields {
                                                if !f.is_static() {
                                                    let abs = cls.first_field_index + inst;
                                                    match &*f.name {
                                                        "declaringClass" if dc.is_none() => {
                                                            dc = Some(abs)
                                                        }
                                                        "methodName" if mn.is_none() => {
                                                            mn = Some(abs)
                                                        }
                                                        "fileName" if fn_.is_none() => {
                                                            fn_ = Some(abs)
                                                        }
                                                        "lineNumber" if ln.is_none() => {
                                                            ln = Some(abs)
                                                        }
                                                        _ => {}
                                                    }
                                                    inst += 1;
                                                }
                                            }
                                            walk = cls.superclass;
                                        } else {
                                            break;
                                        }
                                    }
                                    if let (Some(a), Some(b), Some(c), Some(d)) = (dc, mn, fn_, ln)
                                    {
                                        ste_idx = Some((a, b, c, d));
                                    }
                                }
                                let Some((dc, mn, fn_, ln)) = ste_idx else {
                                    continue;
                                };
                                let read_str = |idx: usize| -> Option<String> {
                                    match vm.shared.mem.heap.get_field(elem_ref, idx) {
                                        Value::Object(Some(s)) => {
                                            cratonvm_vm::vm::read_java_string(
                                                &vm.shared.mem.heap,
                                                s,
                                            )
                                        }
                                        _ => None,
                                    }
                                };
                                let class_name =
                                    read_str(dc).unwrap_or_else(|| "<unknown>".to_string());
                                let method_name =
                                    read_str(mn).unwrap_or_else(|| "<unknown>".to_string());
                                let file_name = read_str(fn_);
                                let line_no = match vm.shared.mem.heap.get_field(elem_ref, ln) {
                                    Value::Int(i) => i,
                                    _ => -1,
                                };
                                // HotSpot format:
                                //   \tat <class>.<method>(<file>:<line>)
                                // If fileName is null/empty, use "Unknown Source".
                                // If lineNumber < 0, omit ":<line>".
                                let location = match (file_name.as_deref(), line_no) {
                                    (Some(f), n) if !f.is_empty() && n >= 0 => format!("{f}:{n}"),
                                    (Some(f), _) if !f.is_empty() => f.to_string(),
                                    _ => "Unknown Source".to_string(),
                                };
                                lines.push(format!("\tat {class_name}.{method_name}({location})"));
                            }
                        }
                    }
                }

                // Fallback: when `Throwable.stackTrace[]` was never populated
                // (the array is null or empty — the typical case for an
                // exception that escapes `main()` without anyone calling
                // `getStackTrace()`), pull frames from the VM-wide registry
                // keyed by identity hash
                // — that's where `Throwable.fillInStackTrace` actually
                // stashes the captured frames in this VM. See
                // `vm/src/vm/vm_init.rs::Vm::throwable_stack_for`.
                if !emitted_frames {
                    if let Some(frames) = vm.throwable_stack_for(cur) {
                        for frame in frames {
                            let location = match (frame.file.as_deref(), frame.line) {
                                (Some(f), n) if !f.is_empty() && n >= 0 => format!("{f}:{n}"),
                                (Some(f), _) if !f.is_empty() => f.to_string(),
                                _ => "Unknown Source".to_string(),
                            };
                            lines.push(format!(
                                "\tat {}.{}({})",
                                frame.class, frame.method, location
                            ));
                        }
                    }
                }

                // Follow cause. Throwable.cause is the canonical chain link,
                // but InvocationTargetException stores the wrapped exception
                // in its own `target` field and its `getCause()` override
                // returns that — so the heap-level `cause` is null/self while
                // the real cause lives in `target`. Probe both.
                let mut next_cause = {
                    let mut next = None;
                    if let Some(i) = cause_idx {
                        if let Value::Object(Some(c)) = vm.shared.mem.heap.get_field(cur, i) {
                            if c != cur {
                                next = Some(c);
                            }
                        }
                    }
                    if next.is_none() {
                        if let Some(i) = target_idx {
                            if let Value::Object(Some(t)) = vm.shared.mem.heap.get_field(cur, i) {
                                if t != cur {
                                    next = Some(t);
                                }
                            }
                        }
                    }
                    next
                };
                if next_cause.is_none() && cname == "java/lang/reflect/InvocationTargetException" {
                    let ite_decl = vm
                        .shared
                        .classes
                        .class_manager
                        .read()
                        .get_loaded_class_id("java/lang/reflect/InvocationTargetException");
                    for (meth, desc) in [
                        ("getTargetException", "()Ljava/lang/Throwable;"),
                        ("getCause", "()Ljava/lang/Throwable;"),
                    ] {
                        if next_cause.is_some() {
                            break;
                        }
                        let invoke_res = if let Some(ite_cid) = ite_decl {
                            invoke_on_class_shared_no_retarget(
                                &vm.shared,
                                &mut vm.main_thread,
                                ite_cid,
                                meth,
                                desc,
                                &[Value::Object(Some(cur))],
                            )
                        } else {
                            invoke_on_class_shared(
                                &vm.shared,
                                &mut vm.main_thread,
                                cid,
                                meth,
                                desc,
                                &[Value::Object(Some(cur))],
                            )
                        };
                        match invoke_res {
                            Ok(Some(Value::Object(Some(t)))) if t != cur => {
                                next_cause = Some(t);
                            }
                            Ok(_) => {}
                            Err(e) => {
                                lines.push(format!(
                                    "[cratonvm-cli] InvocationTargetException.{meth}() failed: {e:?}"
                                ));
                            }
                        }
                    }
                }
                // When walking through PropertyBatchUpdateException, also
                // surface the nested PropertyAccessExceptions array contents.
                // The standard `getMessage()` joins their messages with ";",
                // but Spring's BeanCreationException wrapper inlines that
                // joined string before any per-sub-cause framing is rendered,
                // so the actual offending property (and the IAE/NPE thrown by
                // the setter) is lost in plain `Caused by:` chains.  Drill one
                // level so each sub-cause's class + message + cause chain is
                // visible — this is the only stable surface for diagnosing
                // setter-injection failures because PBUE itself doesn't
                // `initCause` the first sub-exception.
                if cname == "org/springframework/beans/PropertyBatchUpdateException" {
                    // Find the propertyAccessExceptions field by name.
                    let arr_idx = {
                        let cm = vm.shared.classes.class_manager.read();
                        let mut found: Option<usize> = None;
                        let mut walk = Some(cid);
                        while let Some(k) = walk {
                            if let Some(cls) = cm.get_class(k) {
                                let mut inst = 0usize;
                                for f in &cls.fields {
                                    if !f.is_static() {
                                        let abs = cls.first_field_index + inst;
                                        if &*f.name == "propertyAccessExceptions" && found.is_none()
                                        {
                                            found = Some(abs);
                                        }
                                        inst += 1;
                                    }
                                }
                                walk = cls.superclass;
                            } else {
                                break;
                            }
                        }
                        found
                    };
                    if let Some(ai) = arr_idx {
                        match vm.shared.mem.heap.get_field(cur, ai) {
                            Value::Object(Some(arr)) => {
                                let n = vm.shared.mem.heap.array_length(arr);
                                lines.push(format!(
                                    "[cratonvm-cli] PropertyBatchUpdateException.propertyAccessExceptions length={n}"
                                ));
                                for i in 0..n {
                                    let elem = vm.shared.mem.heap.get_array_element(arr, i).ok();
                                    if let Some(Value::Object(Some(eref))) = elem {
                                        let ecid = vm.shared.mem.heap.class_id_of(eref);
                                        // PERF: one read guard for the sub-exception class
                                        // name and its field indices (back-to-back reads).
                                        // Read detailMessage and cause from this sub-exception
                                        let (ename, smsg, scause, spname) = {
                                            let cm = vm.shared.classes.class_manager.read();
                                            let ename = cm
                                                .get_class(ecid)
                                                .map(|c| c.name.to_string())
                                                .unwrap_or_else(|| "?".to_string());
                                            let mut mi: Option<usize> = None;
                                            let mut ci: Option<usize> = None;
                                            let mut pn: Option<usize> = None;
                                            let mut walk = Some(ecid);
                                            while let Some(k) = walk {
                                                if let Some(cls) = cm.get_class(k) {
                                                    let mut inst = 0usize;
                                                    for f in &cls.fields {
                                                        if !f.is_static() {
                                                            let abs = cls.first_field_index + inst;
                                                            match &*f.name {
                                                                "detailMessage" if mi.is_none() => {
                                                                    mi = Some(abs)
                                                                }
                                                                "cause" if ci.is_none() => {
                                                                    ci = Some(abs)
                                                                }
                                                                "propertyName" if pn.is_none() => {
                                                                    pn = Some(abs)
                                                                }
                                                                _ => {}
                                                            }
                                                            inst += 1;
                                                        }
                                                    }
                                                    walk = cls.superclass;
                                                } else {
                                                    break;
                                                }
                                            }
                                            let read_s = |idx: Option<usize>| -> String {
                                                idx.and_then(|i| {
                                                    match vm.shared.mem.heap.get_field(eref, i) {
                                                        Value::Object(Some(s)) => {
                                                            cratonvm_vm::vm::read_java_string(
                                                                &vm.shared.mem.heap,
                                                                s,
                                                            )
                                                        }
                                                        _ => None,
                                                    }
                                                })
                                                .unwrap_or_default()
                                            };
                                            (ename, read_s(mi), ci, read_s(pn))
                                        };
                                        lines.push(format!(
                                            "[cratonvm-cli]   [{i}] {ename} property='{spname}' message={smsg:?}"
                                        ));
                                        if let Some(frames) = vm.throwable_stack_for(eref) {
                                            if !frames.is_empty() {
                                                lines.push(format!(
                                                    "[cratonvm-cli]       ({} captured frames)",
                                                    frames.len()
                                                ));
                                                for frame in frames.iter().take(12) {
                                                    let loc =
                                                        match (frame.file.as_deref(), frame.line) {
                                                            (Some(f), n)
                                                                if !f.is_empty() && n >= 0 =>
                                                            {
                                                                format!("{f}:{n}")
                                                            }
                                                            (Some(f), _) if !f.is_empty() => {
                                                                f.to_string()
                                                            }
                                                            _ => "Unknown Source".to_string(),
                                                        };
                                                    lines.push(format!(
                                                        "\t\tat {}.{}({})",
                                                        frame.class, frame.method, loc
                                                    ));
                                                }
                                            }
                                        }
                                        // Follow cause(s) for this sub-exception (one level deep,
                                        // up to 6 deep just in case).
                                        let mut sub_cur = scause.and_then(|ci| {
                                            if let Value::Object(Some(c)) =
                                                vm.shared.mem.heap.get_field(eref, ci)
                                            {
                                                if c != eref {
                                                    Some(c)
                                                } else {
                                                    None
                                                }
                                            } else {
                                                None
                                            }
                                        });
                                        for _d in 0..6 {
                                            let Some(sc) = sub_cur else { break };
                                            let sc_cid = vm.shared.mem.heap.class_id_of(sc);
                                            // PERF: one read guard for the sub-cause class
                                            // name and its field indices (back-to-back reads).
                                            let (sc_name, sc_msg, sc_cause_idx) = {
                                                let cm = vm.shared.classes.class_manager.read();
                                                let sc_name = cm
                                                    .get_class(sc_cid)
                                                    .map(|c| c.name.to_string())
                                                    .unwrap_or_else(|| "?".to_string());
                                                let mut mi: Option<usize> = None;
                                                let mut ci: Option<usize> = None;
                                                let mut walk = Some(sc_cid);
                                                while let Some(k) = walk {
                                                    if let Some(cls) = cm.get_class(k) {
                                                        let mut inst = 0usize;
                                                        for f in &cls.fields {
                                                            if !f.is_static() {
                                                                let abs =
                                                                    cls.first_field_index + inst;
                                                                match &*f.name {
                                                                    "detailMessage"
                                                                        if mi.is_none() =>
                                                                    {
                                                                        mi = Some(abs)
                                                                    }
                                                                    "cause" if ci.is_none() => {
                                                                        ci = Some(abs)
                                                                    }
                                                                    _ => {}
                                                                }
                                                                inst += 1;
                                                            }
                                                        }
                                                        walk = cls.superclass;
                                                    } else {
                                                        break;
                                                    }
                                                }
                                                let m = mi
                                                    .and_then(|i| {
                                                        match vm.shared.mem.heap.get_field(sc, i) {
                                                            Value::Object(Some(s)) => {
                                                                cratonvm_vm::vm::read_java_string(
                                                                    &vm.shared.mem.heap,
                                                                    s,
                                                                )
                                                            }
                                                            _ => None,
                                                        }
                                                    })
                                                    .unwrap_or_default();
                                                (sc_name, m, ci)
                                            };
                                            lines.push(format!("[cratonvm-cli]       Caused by: {sc_name}: {sc_msg}"));
                                            if let Some(frames) = vm.throwable_stack_for(sc) {
                                                if !frames.is_empty() {
                                                    lines.push(format!("[cratonvm-cli]         ({} captured frames)", frames.len()));
                                                    for frame in frames.iter().take(16) {
                                                        let loc = match (
                                                            frame.file.as_deref(),
                                                            frame.line,
                                                        ) {
                                                            (Some(f), n)
                                                                if !f.is_empty() && n >= 0 =>
                                                            {
                                                                format!("{f}:{n}")
                                                            }
                                                            (Some(f), _) if !f.is_empty() => {
                                                                f.to_string()
                                                            }
                                                            _ => "Unknown Source".to_string(),
                                                        };
                                                        lines.push(format!(
                                                            "\t\t\tat {}.{}({})",
                                                            frame.class, frame.method, loc
                                                        ));
                                                    }
                                                }
                                            }
                                            sub_cur = sc_cause_idx.and_then(|ci| {
                                                if let Value::Object(Some(c)) =
                                                    vm.shared.mem.heap.get_field(sc, ci)
                                                {
                                                    if c != sc {
                                                        Some(c)
                                                    } else {
                                                        None
                                                    }
                                                } else {
                                                    None
                                                }
                                            });
                                        }
                                    } else {
                                        lines.push(format!("[cratonvm-cli]   [{i}] <null>"));
                                    }
                                }
                            }
                            Value::Object(None) => {
                                lines.push("[cratonvm-cli] PropertyBatchUpdateException.propertyAccessExceptions = null".into());
                            }
                            _ => {}
                        }
                    } else {
                        lines.push("[cratonvm-cli] PropertyBatchUpdateException.propertyAccessExceptions field NOT FOUND on class".into());
                    }
                }
                if let Some(c) = next_cause {
                    // If this is the last iteration the cap allows, the cause
                    // `c` would never be rendered — emit a marker so deeply
                    // wrapped exceptions are not silently cut off.
                    if depth + 1 >= MAX_CAUSE_DEPTH {
                        lines.push("\t... (deeper causes truncated)".to_string());
                        break;
                    }
                    cur = c;
                    prefix = "Caused by:";
                    continue;
                }
                break;
            }
            let had_caused_by = lines.iter().any(|l| l.starts_with("Caused by:"));
            if !had_caused_by
                && lines
                    .iter()
                    .any(|l| l.contains("java/lang/reflect/InvocationTargetException"))
            {
                if let Some(frames) = vm.throwable_stack_for(exc_ref) {
                    if !frames.is_empty() {
                        lines.push(
                            "[cratonvm-cli] Throwable stack (fillInStackTrace) for InvocationTargetException:"
                                .to_string(),
                        );
                        for frame in frames.iter().take(24) {
                            let location = match (frame.file.as_deref(), frame.line) {
                                (Some(f), n) if !f.is_empty() && n >= 0 => format!("{f}:{n}"),
                                (Some(f), _) if !f.is_empty() => f.to_string(),
                                _ => "Unknown Source".to_string(),
                            };
                            lines.push(format!(
                                "\tat {}.{}({})",
                                frame.class, frame.method, location
                            ));
                        }
                    }
                }
            }
            // Missing-stack-trace diagnostic (2026-05-21).
            //
            // When an exception escapes `main()` and NEITHER the heap-side
            // `Throwable.stackTrace[]` NOR the VM-wide retained trace
            // capture produced a single `\tat ...` frame, the bare
            // `Exception in thread "main" <class>: <msg>` line is useless
            // for diagnosis — exactly the keycloak26 `NullPointerException:
            // charset` symptom. Rather than exit silently, emit an explicit
            // marker so the failure mode is unambiguous, retry the
            // identity-hash trace lookup for the head exception, and point
            // the operator at the live-stack diagnostic env var.
            let emitted_any_frame = lines.iter().any(|l| l.starts_with("\tat "));
            if !emitted_any_frame {
                lines.push(
                    "[cratonvm-cli] (no Java stack frames were captured for this exception)"
                        .to_string(),
                );
                // Last-ditch: dump whatever the head exception's
                // retained trace entry holds, even if the cause-chain
                // walk above skipped it.
                match vm.throwable_stack_for(exc_ref) {
                    Some(frames) if !frames.is_empty() => {
                        lines.push(format!(
                            "[cratonvm-cli] recovered {} captured frame(s) from the trace store:",
                            frames.len()
                        ));
                        for frame in frames.iter().take(40) {
                            let location = match (frame.file.as_deref(), frame.line) {
                                (Some(f), n) if !f.is_empty() && n >= 0 => format!("{f}:{n}"),
                                (Some(f), _) if !f.is_empty() => f.to_string(),
                                _ => "Unknown Source".to_string(),
                            };
                            lines.push(format!(
                                "\tat {}.{}({})",
                                frame.class, frame.method, location
                            ));
                        }
                    }
                    _ => {
                        lines.push(
                            "[cratonvm-cli] the retained trace store has no entry for this \
                             throwable either — the exception was likely thrown on a \
                             non-main thread, or its constructor was shadowed by a native \
                             that skipped fillInStackTrace."
                                .to_string(),
                        );
                        lines.push(
                            "[cratonvm-cli] re-run with CRATONVM_DBG_CHARSET=1 to dump the \
                             full live Java thread stack at throw time (NullPointerException: \
                             charset), or CRATONVM_DBG_ATHROW=1 for every exception throw."
                                .to_string(),
                        );
                    }
                }
            }
            bail!("{}", lines.join("\n"));
        }
    }
}

fn main() {
    // `--diff-hotspot`: run this same program under CratonVM and under a
    // reference JDK and report the FIRST divergence, then exit. Handled here,
    // as the very first statement of `main`, for three reasons:
    //
    //  * this mode boots no VM in *this* process — it spawns one CratonVM child
    //    and one `java` — so it must not install flags, expand the grouped
    //    configuration variables, or latch the immutable snapshot the child
    //    under test will latch for itself;
    //  * `diff_hotspot::scan` removes its own `--diff-*` tokens before
    //    `expand_argfiles` / `insert_program_args_separator` ever see them, so
    //    the pre-clap argv pipeline (and its ~60 positional-semantics tests) is
    //    untouched by this feature;
    //  * on a normal launch it costs one pass over argv and returns `None`.
    //
    // See `docs/testing/diff-hotspot.md`.
    if let Some(code) = diff_hotspot::maybe_run() {
        use std::io::Write as _;
        let _ = std::io::stdout().flush();
        std::process::exit(code);
    }

    // Install the immutable runtime configuration before crash handlers or
    // any other subsystem can read a CratonVM flag. `--nojit` is detected
    // from the launcher portion of the expanded argv (never from Java program
    // arguments) and applied as a typed overlay. `run()` performs the full
    // parse again; this early pass exists solely to close the configuration
    // ordering boundary.
    let early_argv =
        insert_program_args_separator(expand_argfiles(std::env::args().collect::<Vec<_>>()));
    // A launcher flag parked in the program-args tail is discarded in silence
    // — exit 0, no file, no diagnostic. Say so before anything else runs, so
    // the warning is the first thing on stderr rather than the last, and so it
    // is emitted even on the paths that exit before `run()` ever parses argv.
    // This is the ONLY consumer of `early_argv` that does not also change
    // behaviour: nothing is rewritten, `java` positional semantics are intact,
    // and the misplaced token still reaches the Java program verbatim.
    warn_about_misplaced_launcher_flags(&early_argv);
    let mut flag_overrides = cratonvm_types::MapSource::empty();
    if launcher_nojit_requested(&early_argv) {
        flag_overrides = flag_overrides.with("CRATONVM_DISABLE_JIT", "1");
    }
    // `--dump-phase-report <FILE>` is sugar for
    // `CRATONVM_PHASE_ACCOUNTING=coarse CRATONVM_PHASE_ACCOUNTING_OUT=<FILE>`,
    // applied here because a declared flag is served from the snapshot latched
    // three lines below and is immutable afterwards. The level is only
    // *defaulted*: an explicit `CRATONVM_PHASE_ACCOUNTING`, or a
    // `CRATONVM_DBG=phase-accounting=fine` token, still chooses it, so the
    // option composes with `fine` instead of silently downgrading it.
    if let Some(path) = launcher_phase_report_path(&early_argv) {
        flag_overrides = flag_overrides.with(phase::FLAG_JSON_OUT, &path);
        let level_set_explicitly = std::env::var_os(phase::FLAG_ENABLE).is_some()
            || std::env::var(cratonvm_types::flag_groups::Group::DBG.var())
                .is_ok_and(|spec| spec.contains("phase-accounting"));
        if !level_set_explicitly {
            flag_overrides = flag_overrides.with(phase::FLAG_ENABLE, "coarse");
        }
    }
    // `-ea` / `-da` / `-esa` / `-dsa`: the HotSpot spelling of
    // `CRATONVM_ENABLE_ASSERTIONS`, applied here for the same reason as the two
    // overrides above — the flag is declared, so the snapshot latched three
    // lines below is the last point at which it can be set at all.
    let mut flag_unsets: Vec<&str> = Vec::new();
    match launcher_assertions_requested(&early_argv) {
        Some(true) => flag_overrides = flag_overrides.with("CRATONVM_ENABLE_ASSERTIONS", "1"),
        // `-da` has to make the name *absent*, not set it to "0": the flag is
        // parsed with `present`, under which `=0` still reads as enabled. An
        // explicit `-da` therefore also overrides an inherited export, which is
        // what HotSpot does.
        Some(false) => flag_unsets.push("CRATONVM_ENABLE_ASSERTIONS"),
        None => {}
    }
    let runtime_flags =
        cratonvm_types::VmFlags::from_env_with_overrides_and_unsets(flag_overrides, &flag_unsets);
    if cratonvm_types::install_flags(runtime_flags).is_err() {
        eprintln!("[cratonvm] runtime flags were read before launcher configuration");
        std::process::exit(1);
    }

    // Expand the ten grouped configuration variables (`CRATONVM_JIT=-bce,unroll`
    // and friends) into the per-knob keys the rest of the VM reads.
    //
    // This runs first, before the symbolize/SEGV hooks below, because those
    // read `CRATONVM_SYMBOLIZE` and `CRATONVM_TEST_SEGV` — both of which are
    // now tokens (`CRATONVM_DBG=symbolize=...`, `CRATONVM_TEST=segv`) and would
    // otherwise be missed on this run. It is also the only point in the process
    // guaranteed to be single-threaded, which is what makes writing back to
    // `environ` sound: 431 read sites still call `std::env::var` directly and
    // cannot see a resolved source. See `cratonvm_types::flag_groups`.
    let (legacy_direct, unknown_tokens) = cratonvm_types::flag_groups::expand_process_env();
    // An unrecognised token is FATAL, not a warning.
    //
    // It used to print and continue, which is the worst of both worlds: the
    // knob the caller asked for is not applied, the run completes normally, and
    // a scripted A/B that captures only the tail of the output — or only greps
    // for PASS/FAIL — never sees the notice. The experiment then measures
    // nothing while reading as a clean result. That is not hypothetical: it is
    // how `CRATONVM_JIT=no-self-cache-inherit` (the disable spelling is
    // `-self-cache-inherit`) produced a confident, wrong "hypothesis
    // falsified" in
    // `docs/known-issues/jit/math-floormod-long-int-returns-minus-one-20260805.md`.
    //
    // Failing closed makes a typo impossible to mistake for a measurement.
    // `CRATONVM_ALLOW_UNKNOWN_TOKENS=1` restores the old warn-and-continue for
    // anyone who genuinely needs it (e.g. a shared script that must run against
    // several VM revisions whose token sets differ).
    if !unknown_tokens.is_empty() {
        let allow = std::env::var_os("CRATONVM_ALLOW_UNKNOWN_TOKENS").is_some();
        for t in &unknown_tokens {
            eprintln!("[cratonvm] unknown configuration token: {t}");
            // `GROUPVAR=token` — split back so a suggestion can be offered.
            if let Some((var, tok)) = t.split_once('=') {
                let tok = tok.split('=').next().unwrap_or(tok);
                let tok = tok.strip_prefix('-').unwrap_or(tok);
                let tok = tok.strip_prefix('+').unwrap_or(tok);
                if let Some(group) = cratonvm_types::flag_groups::Group::ALL
                    .iter()
                    .find(|g| g.var() == var)
                {
                    if let Some(hint) = cratonvm_types::flag_groups::suggest(*group, tok) {
                        eprintln!("[cratonvm]   {hint}");
                    }
                }
            }
        }
        if allow {
            eprintln!(
                "[cratonvm] continuing anyway: CRATONVM_ALLOW_UNKNOWN_TOKENS is set. \
                 The knob(s) above are NOT applied."
            );
        } else {
            eprintln!(
                "[cratonvm] refusing to start: an unknown token is not applied, so this run \
                 would silently not be the configuration you asked for. Fix the spelling, or \
                 set CRATONVM_ALLOW_UNKNOWN_TOKENS=1 to continue with it ignored."
            );
            std::process::exit(2);
        }
    }

    // Seed the phase-accounting epoch. This is the earliest point it can go:
    // `CRATONVM_PHASE_ACCOUNTING` is a `CRATONVM_DBG` token, so it is not
    // readable until the group expansion immediately above has run, and
    // `phase::level()` latches on first read. Everything before this line —
    // the allocator init, `clap`'s argfile expansion, the flag install — is
    // outside the epoch rather than unattributed inside it, which is
    // deliberate and is item 2 of `docs/observability/phase-accounting.md`
    // §11. A no-op (not even a clock read) when accounting is off.
    //
    // This also provisionally claims the launcher as the reconciliation basis;
    // the `main-vm` thread takes that role back below, because the launcher
    // spends the rest of the process blocked in `join`.
    phase::mark_process_start();

    // `CRATONVM_DBG=-deprecations` expands to `CRATONVM_QUIET_DEPRECATIONS=1`,
    // which the call above has already written back, so this read sees it.
    if !legacy_direct.is_empty() && std::env::var_os("CRATONVM_QUIET_DEPRECATIONS").is_none() {
        // One line, not one per variable: a debugging session routinely exports
        // a dozen of these and a dozen warnings would just train people to
        // ignore them.
        let shown: Vec<&str> = legacy_direct.iter().map(|(_, to)| to.as_str()).collect();
        eprintln!(
            "[cratonvm] {} per-flag variable(s) set directly; the supported \
             spelling is now: {}",
            legacy_direct.len(),
            shown.join(" ")
        );
    }
    // Env tokens that a launcher flag now supersedes — notably
    // `CRATONVM_REAL=-stubs`, which `--jdk-only` covers and extends (contract
    // §9). Printed beside the `legacy_direct` line above because it is the same
    // class of message ("your configuration has a newer spelling"), and once
    // each: a token that supersedes twice is still one fact.
    // …except when the run already passes the flag the note recommends: the
    // only supersession today is `CRATONVM_REAL=-stubs` -> `--jdk-only`.
    if !scan_requested_compatibility_mode(&early_argv).is_jdk_only() {
        let mut noted: Vec<String> = Vec::new();
        for superseded in cratonvm_types::flag_groups::process_env_supersessions() {
            let note = superseded.note().to_string();
            if noted.iter().any(|seen| *seen == note) {
                continue;
            }
            eprintln!("[cratonvm] {note}");
            noted.push(note);
        }
    }

    // Hardware-fault diagnostics. On Windows a SEGV/access violation is a
    // structured exception that bypasses the Rust panic hook below entirely;
    // without this, a native fault (e.g. the JIT-dispatch SEGV) kills the
    // process with empty stderr and a bare STATUS_ACCESS_VIOLATION exit code.
    // This registers a vectored exception handler that prints the faulting PC
    // + a symbolized backtrace and then lets the process die as before. It
    // does NOT install a panic hook, so the visibility-first hook set up just
    // below is preserved. No-op on non-Windows targets.
    cratonvm_vm::runtime::crash_handler::install_hardware_fault_handler();

    // Diagnostic: CRATONVM_SYMBOLIZE=0x11BF183,0xAB10E resolves exe-relative
    // RVAs (as printed by the VEH crash report) against THIS binary's symbols
    // in a clean context, then exits. Used to symbolize a multi-threaded crash
    // whose racy teardown truncated the in-handler symbolization.
    if let Ok(spec) = std::env::var("CRATONVM_SYMBOLIZE") {
        let rvas: Vec<usize> = spec
            .split(',')
            .filter_map(|s| {
                let s = s.trim().trim_start_matches("0x").trim_start_matches("0X");
                usize::from_str_radix(s, 16).ok()
            })
            .collect();
        for (rva, name) in cratonvm_vm::runtime::crash_handler::symbolize_rvas(&rvas) {
            match name {
                Some(n) => println!("0x{:X}\t{}", rva, n),
                None => println!("0x{:X}\t<unresolved>", rva),
            }
        }
        std::process::exit(0);
    }

    // Self-test hook for the hardware-fault handler: when CRATONVM_TEST_SEGV=1,
    // deliberately trigger an access violation right after installing the
    // handler so the VEH path (faulting PC + symbolized backtrace) can be
    // validated without needing to reproduce a real crash. Gated behind an env
    // var so it never affects normal runs.
    if std::env::var("CRATONVM_TEST_SEGV").as_deref() == Ok("1") {
        eprintln!("[cratonvm] CRATONVM_TEST_SEGV=1: forcing an access violation");
        // SAFETY: intentional null/wild dereference to exercise the fault
        // handler. This is dead code on every normal run.
        unsafe {
            let p = 0xdead_beef_usize as *mut u8;
            std::ptr::write_volatile(p, 0);
        }
    }

    // I1 — Visibility-first panic hook.
    //
    // The previous T14 hook silenced **every** Rust panic by routing it to
    // `tracing::debug!`. That worked for the well-known initPhase1
    // bootstrap-path panics (unaligned-pointer reads, transient null
    // dereferences) which the outer `safe_native_call` already logs once
    // via a user-facing warning — but it also silenced *real* panics that
    // escaped a `catch_unwind`. Because the tracing subscriber installed
    // by `run()` filters at WARN+, debug-level panic notices were never
    // written to stderr, and any genuine VM crash showed up in the logs
    // with an empty stderr and an exit code derived from the OS abort
    // (rc=-1 on Windows when the runner kills the process; STATUS_ACCESS_
    // VIOLATION when the abort fires from native code). The CGLIB probe
    // hit exactly this surface: an interpreter loop hung inside
    // `String.indexOf`, the watchdog never fired (the user did not pass
    // `--stack-dump-on-timeout`), and the failure was indistinguishable
    // from a successful run that produced no output.
    //
    // The new hook routes everything to `stderr` directly — the only sink
    // that is guaranteed to survive every other failure mode (tracing
    // subscriber not initialized, WARN-level filter, panic firing from a
    // worker thread before `run()` builds the subscriber). For
    // bootstrap-path panics that are still expected to be quiet, the
    // outer `safe_native_call` continues to swallow them via its own
    // `catch_unwind` — the hook fires *before* `catch_unwind` catches
    // the unwind, but only the recovery path knows the panic was caught,
    // so we always emit at hook time. The cost is a couple of extra
    // stderr lines on the (rare) bootstrap-panic path; the gain is that
    // every crash is now traceable.
    //
    // Honors `RUST_BACKTRACE=1` / `RUST_BACKTRACE=full` the same way the
    // default Rust hook does (we use `std::backtrace::Backtrace::capture`
    // which respects the env var).
    let _default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        use std::io::Write;
        // Handled heap-exhaustion unwind from the native allocators — the VM
        // converts it into a catchable java.lang.OutOfMemoryError, so it is not
        // a panic the user needs to see. (See
        // `cratonvm_vm::runtime::native_oom`.)
        if cratonvm_vm::runtime::native_oom::is_native_oom_panic(info) {
            return;
        }
        let msg = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "<panic payload was not a string>".to_string());
        // NOT `std::thread::current()`. That call PANICS once this thread's
        // thread-local data has been destroyed, and a panic inside a panic
        // hook is a panic-while-panicking: Rust aborts the process on the
        // spot, before this hook has printed a single byte about the panic it
        // was called for. Reading one cosmetic name that way cost 183 of 206
        // Hibernate Reactive classes under `--jdk-only` -- every one of them
        // rc=134 (SIGABRT), and every one of them reporting `thread/
        // current.rs:315:9`, which is the *second* panic and says nothing
        // about the first. See
        // `hibernate-reactive-double-panic-abort-FIXED-20260901` and
        // `crash_handler::current_thread_name`.
        let thread_name_owned =
            cratonvm_vm::runtime::crash_handler::current_thread_name();
        let thread_name = thread_name_owned.as_deref().unwrap_or("<unnamed>");

        // Is this thread's thread-local storage still usable?
        //
        // `tracing`'s dispatcher and its subscriber stack reach several
        // `thread_local!`s and PANIC (they do not degrade) once those are
        // gone, and a panic raised inside a panic hook aborts the process
        // immediately. That is the second half of
        // `hibernate-reactive-double-panic-abort-FIXED-20260901`:
        // after the `std::thread::current()` call above was fixed, this
        // hook printed the real panic correctly and then aborted anyway on
        // its own `tracing::warn!` tail.
        //
        // A never-otherwise-touched, destructor-bearing key is a sound probe
        // for the phase that matters. MEASURED (glibc 2.39, rustc 1.97.1):
        // throughout `__nptl_deallocate_tsd` — the `pthread` thread-specific-
        // data phase this hook is reached from when a JNI library's TSD
        // destructor panics — every `try_with` on the thread answers
        // `Err(AccessError)`, initialized or not; during Rust's own TLS
        // destructors even an untouched key still initializes. So `Ok` here
        // means the tracing stack is safe to enter, and the normal panic
        // keeps its tracing mirror.
        thread_local! {
            static TLS_LIVENESS_PROBE: Box<u8> = Box::new(0);
        }
        let tls_usable = TLS_LIVENESS_PROBE.try_with(|_| ()).is_ok();

        // Mirror `safe_native_call`'s well-known bootstrap-path quiet
        // list (vm/src/vm/vm_exec.rs:253). These are caught one frame
        // up and surfaced via a single user-facing warning; we route
        // them to `tracing::debug!` so the full panic doesn't spam the
        // user's stderr during initPhase1. Anything *not* in this list
        // gets the full visibility treatment.
        // Keycloak Round 71: only quiet the bootstrap-class panics while
        // bootstrap is actually running (init level < 4). Once `main()`
        // is executing, an unaligned/null pointer panic from a native is
        // a real fault that must be visible — silently demoting it to
        // debug produces the "Keycloak exits in 5s with no output"
        // failure mode where 19 caught NPEs corrupt picocli state and
        // `parseAndRun` returns without ever calling `start-dev`.
        let in_bootstrap = cratonvm_native_api::init_level::get_init_level() < 4;
        let is_known_bootstrap_quiet =
            (msg.contains("unaligned pointer") || msg.contains("null pointer")) && in_bootstrap;

        if is_known_bootstrap_quiet {
            if !tls_usable {
                // Quiet by intent and unable to reach `tracing` — say nothing
                // rather than abort the process to log a panic that is caught
                // one frame up anyway.
                return;
            }
            if let Some(loc) = info.location() {
                tracing::debug!(
                    target: "cratonvm::panic",
                    file = loc.file(),
                    line = loc.line(),
                    column = loc.column(),
                    thread = thread_name,
                    "bootstrap-path panic (caught upstream): {msg}",
                );
            } else {
                tracing::debug!(
                    target: "cratonvm::panic",
                    thread = thread_name,
                    "bootstrap-path panic (caught upstream): {msg}",
                );
            }
            return;
        }

        // Visible path: write directly to stderr because tracing may
        // be filtering at WARN+ and we cannot rely on the subscriber
        // having been initialised (e.g. when the panic fires before
        // `run()` builds it).
        let mut stderr = std::io::stderr().lock();
        if let Some(loc) = info.location() {
            let _ = writeln!(
                stderr,
                "thread '{thread_name}' panicked at {}:{}:{}:\n{msg}",
                loc.file(),
                loc.line(),
                loc.column(),
            );
        } else {
            let _ = writeln!(stderr, "thread '{thread_name}' panicked:\n{msg}");
        }
        // Backtrace only when explicitly requested — matches the stock
        // Rust hook semantics so users opting out of backtrace still see
        // the panic message but no overhead.
        // Stamp the crash with the standard library in play. Two complete
        // class-library implementations ship in this binary and they fail
        // differently; a panic report that doesn't say which one ran costs
        // a round trip to triage.
        let _ = writeln!(stderr, "[cratonvm] {}", active_jdk_mode_line());
        let bt = std::backtrace::Backtrace::capture();
        if bt.status() == std::backtrace::BacktraceStatus::Captured {
            let _ = writeln!(stderr, "stack backtrace:\n{bt}");
        } else {
            let _ = writeln!(
                stderr,
                "note: run with `RUST_BACKTRACE=1` environment variable \
                 to display a backtrace",
            );
        }
        let _ = stderr.flush();
        // ALSO mirror through tracing at WARN level so log aggregators
        // that key on tracing still see the panic. We do this *after*
        // the direct stderr write so the user-visible message lands
        // even when the tracing subscriber is unavailable or filtering
        // it out.
        if !tls_usable {
            // The stderr write above already carries the whole panic. Entering
            // `tracing` from a thread whose TLS is gone would panic inside this
            // hook and abort the process, discarding what we just printed.
            let _ = writeln!(
                std::io::stderr(),
                "[cratonvm] (panic raised after this thread's TLS was destroyed                  -- the tracing mirror of this panic is skipped)",
            );
            return;
        }
        if let Some(loc) = info.location() {
            tracing::warn!(
                target: "cratonvm::panic",
                file = loc.file(),
                line = loc.line(),
                column = loc.column(),
                thread = thread_name,
                "panic: {msg}",
            );
        } else {
            tracing::warn!(target: "cratonvm::panic", thread = thread_name, "panic: {msg}");
        }
    }));

    // The interpreter uses recursive Rust calls for Java method invocations.
    // Deep Java call stacks (e.g. Quarkus bootstrap, binary-trees-style
    // recursion under JIT dispatch) can exceed the default 8 MB Rust stack.
    // 64 MB cleared every workload up to and including QuickBenchLong's
    // first four kernels but `binaryTrees(18)` (~524 k recursive
    // invocations through `jit_invoke_dispatch` / interpreter fallback
    // helpers, each adding one Rust frame) drove the main-vm thread past
    // it on some platforms — manifesting as `thread 'main-vm' has
    // overflowed its stack` (rc=139 on Linux) before reaching the GC
    // safepoint that would have triggered an OOME. Bump to 128 MB so
    // even the deepest recursive workloads have headroom; the upper
    // bound is virtual-address-space-only on 64-bit OSes (no commit
    // until the page is touched), so the practical cost is zero.
    // DBG (CRATONVM_DBG_HEARTBEAT=<ms>): forensic liveness heartbeat, armed
    // from the launcher thread (not main-vm) so it keeps writing even if
    // main-vm itself hangs or dies without unwinding. See its own doc
    // comment for what it's for.
    cratonvm_vm::runtime::heartbeat_watch::arm_from_env();

    let builder = std::thread::Builder::new()
        .name("main-vm".into())
        .stack_size(128 * 1024 * 1024);
    let handler = builder
        .spawn(|| {
            // DBG (CRATONVM_DBG_HANGWALK=<secs>): arm the native-stack-walk
            // watchdog on THIS (main-vm) thread — the one that runs the
            // interpreter — not the launcher thread that just joins it.
            cratonvm_vm::runtime::stwhang_watch::arm_from_env();
            // Phase accounting: move the reconciliation basis onto THIS thread.
            // `mark_process_start` had to run on the launcher (it is the first
            // point the `CRATONVM_DBG` token is readable), but the launcher does
            // nothing after this spawn except block in `join` — leaving the
            // basis there would make `reconciles` a verdict about a waiter, and
            // every per-category value on the summary line would be zero. The
            // epoch is untouched, so `process_wall_ns` still covers the
            // launcher prologue even though `basis_wall_ns` starts here.
            phase::claim_reconciliation_basis();
            // Diagnosability (Keycloak Gap 9): the boot can exit SILENTLY — `run()`
            // returns `Ok` (e.g. waitForExit returned / VM main finished) or an `Err`
            // whose `Display` ({e:#}) renders empty, so the prior `eprintln!("{e:#}")`
            // could print nothing before `exit(1)`. Always surface the outcome
            // (Display AND Debug) and flush stderr so a startup failure that ends the
            // process is never invisible. Additive logging only — no behaviour change.
            use std::io::Write as _;
            let result = run();
            // Everything past `run()` is teardown: report emission and outcome
            // rendering. Charging it keeps the tail of the basis thread's
            // timeline out of `unattributed_ns`. Held to the end of the
            // closure, so the report below is taken from *inside* it — which is
            // why `anomalies.open_spans == 1` is the expected reading and not a
            // leak (`docs/observability/phase-accounting.md` §9).
            let _phase_shutdown = phase::enter(phase::Category::VmShutdown);
            // A normal Java-main return never reaches the System.exit hook.
            // Flush controlled-exit diagnostics before rendering the outcome.
            maybe_dump_shutdown_reports();
            // The corrupt-cell census, on every exit arm. See
            // `reclaim_guard::corrupt_cell_exit_summary`: a run that tripped the
            // collector's guard and named nothing must SAY so, because silence
            // from a diagnostic reads as "clean" and is not.
            cratonvm_vm::memory::corrupt_cell_exit_summary();
            // Same reasoning for the post-remap stale-frame-word detector: its
            // `System.exit` printer sits in the shutdown trailer, and this is
            // the arm a program that returns from `main` takes instead.
            cratonvm_types::stale_remap_census::exit_summary();
            // A3 (2026-09-01): the coercion / ref-word degradation censuses,
            // on the arm a program that returns from `main` takes. Same pair as
            // the shutdown trailer in `lang_system.rs`; both are `Once`-guarded,
            // so whichever exit path runs first prints and the other is a no-op.
            cratonvm_types::compact_value::coercion_census::exit_summary();
            cratonvm_types::compact_value::degradation_exit_summary();
            // The JVMS 6.5 uninstantiable-receiver census (A12), on the arm a
            // program that returns from `main` takes. `Once`-guarded and silent
            // unless a native handed back an abstract/interface receiver, so
            // having it on both exit paths is correct.
            cratonvm_native_api::instantiable::exit_summary();
            match result {
                Ok(()) => {
                    cratonvm_vm::jit::conservative_roots::report_a5_engagement();
                    eprintln!("[cratonvm] main-vm run() returned Ok — VM main exiting normally");
                    let _ = std::io::stderr().flush();
                }
                Err(e) => {
                    eprintln!("[cratonvm] main-vm run() returned Err: {e:#}");
                    eprintln!("[cratonvm] main-vm run() Err (debug): {e:?}");
                    // Always stamp the failure with the standard library it
                    // ran against. CratonVM has two of them with different
                    // bug sets, so a stack trace or exception without the
                    // mode is not actionable — this line is what makes a
                    // pasted terminal transcript sufficient for triage.
                    eprintln!("[cratonvm] {}", active_jdk_mode_line());
                    let _ = std::io::stderr().flush();
                    // G11-1: STDOUT too, and only here — this arm ends in
                    // `std::process::exit`, which runs no destructors, and the
                    // fd table's fd-1 entry is a `Mutex<io::Stdout>`, i.e. a
                    // LINE writer. A Java program whose last `System.out`
                    // write had no trailing newline (`System.out.print`, a
                    // partial `write`) and which then died on an uncaught
                    // exception lost those bytes: the stderr flush three lines
                    // up never touched them. HotSpot does not lose them —
                    // MEASURED on Temurin 25.0.3+9, `HookProbe noflush` and
                    // `haltnoflush`, where an unterminated unflushed
                    // `System.out.print` survives both `System.exit(0)` and
                    // `Runtime.halt(6)`.
                    //
                    // Deliberately additive: no output is produced, no
                    // ordering changes (stderr is flushed first, as before),
                    // and the exit code stays 1. `let _` because a broken pipe
                    // on stdout must not turn a Java-level failure into a
                    // different one.
                    let _ = std::io::stdout().flush();
                    std::process::exit(1);
                }
            }
        })
        .expect("failed to spawn main-vm thread");
    handler.join().unwrap_or_else(|e| {
        eprintln!("main-vm thread panicked: {:?}", e);
        // Same reason as the `Err` arm above: this is a `process::exit` path.
        // A VM panic is the case where buffered application output is most
        // worth having, since it is the evidence for where the VM was.
        let _ = std::io::Write::flush(&mut std::io::stderr());
        let _ = std::io::Write::flush(&mut std::io::stdout());
        std::process::exit(1);
    });
}

/// Parse a JVM-style memory size string (e.g., "256m", "1g", "1024k").
fn parse_size(s: &str) -> Option<usize> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }

    let (num_str, multiplier) = match s.as_bytes().last()? {
        b'k' | b'K' => (&s[..s.len() - 1], 1024),
        b'm' | b'M' => (&s[..s.len() - 1], 1024 * 1024),
        b'g' | b'G' => (&s[..s.len() - 1], 1024 * 1024 * 1024),
        _ => (s, 1),
    };

    // Use checked_mul so a huge value (e.g. `99999999999g`) fails the parse
    // rather than wrapping silently in release / panicking in debug. A parsed
    // size of 0 is also rejected as invalid (a 0-byte heap is meaningless).
    let n = num_str.parse::<usize>().ok()?;
    let bytes = n.checked_mul(multiplier)?;
    if bytes == 0 {
        return None;
    }
    Some(bytes)
}

/// Total physical RAM in bytes, or `None` if it can't be determined.
///
/// Re-exported from `cratonvm_vm::runtime::container`, which owns the platform
/// probes now that both the launcher and `SharedVm::new` size the default heap
/// through the same code.
fn physical_ram_bytes() -> Option<u64> {
    cratonvm_vm::runtime::container::physical_ram_bytes()
}

/// HotSpot-style ergonomic default max heap, applied only when the user did
/// not pass an explicit `-Xmx`.
///
/// Approximates a stock JDK's `-XX:MaxRAMPercentage=25` ergonomics: max heap =
/// 1/4 of physical RAM, **floored** at the historical 256 MB default (so this
/// only ever *raises* the heap above the prior baseline) and **capped** at
/// `MAX_ERGONOMIC_HEAP`. Without it, real-world apps (Spring / Mockito /
/// ByteBuddy / JUnit) thrash GC at 256 MB and look like a hang where HotSpot —
/// which auto-sizes — finishes fine (e.g. buildpack `LifecycleTests`).
///
/// The cap exists because CratonVM's generational heap **eagerly commits** its
/// arenas (`Arena::new` → `vec![0u8; cap]`): an uncapped 1/4-of-RAM heap (e.g.
/// 16 GB on a 64 GB host) would charge ~16 GB of commit per process. The cap
/// keeps the default's commit bounded while still giving GC-heavy workloads
/// enough room. (If the heap is ever made lazily-committed, the cap can grow
/// or be removed to fully match HotSpot.)
///
/// When running under `-XX:+UseContainerSupport` inside a memory-constrained
/// container, the basis for the fraction is the **cgroup memory limit** rather
/// than host RAM — HotSpot's `MaxRAMPercentage` applies to the container limit,
/// not the host total, so on a 64 GB host with `--memory=512m` the default heap
/// is sized off 512 MB, not 64 GB. `container_mem_limit` is the detected cgroup
/// limit (or `None` when uncontained / container support is off); the basis is
/// then `min(physical RAM, cgroup limit)`.
///
/// Opt out with `CRATONVM_DEFAULT_HEAP_ERGONOMICS=0` (fixed 256 MB default), or
/// override the cap with `CRATONVM_DEFAULT_HEAP_MAX_MB=<N>`. An explicit `-Xmx`
/// always wins over all of this.
/// The sizing itself lives in `cratonvm_vm::runtime::container` so the launcher
/// and `SharedVm::new` cannot drift apart again — they used to disagree by 2x
/// at the top end and disagree entirely about whether host RAM is a basis.
use cratonvm_vm::runtime::container::{
    clamp_ergonomic_heap, ergonomic_default_max_heap, MAX_ERGONOMIC_HEAP,
};

// ---------------------------------------------------------------------------
// `--diff-hotspot` — one program, two VMs, the first divergence
// ---------------------------------------------------------------------------

/// The differential launcher mode.
///
/// # Why this lives in the launcher and not in `difftest/`
///
/// `cratonvm-difftest` already owns the *corpus* door: it compiles a directory
/// of seeds, fans CratonVM across a mode matrix, diffs eight dimensions against
/// one HotSpot run, and gates the result against a committed ledger. What it
/// does not have — and what an independent audit asked for on 2026-09-01, after
/// 273 differential assertions against HotSpot 25 found zero divergences — is a
/// door anyone can point at **their own** program:
///
/// ```text
/// cratonvm --diff-hotspot -cp build/classes com.example.Main --arg
/// ```
///
/// Two constraints put that door here rather than in the fuzzer. First, the
/// fuzzer's two-VM executor takes a classpath and a main class and **no program
/// arguments**, and knows nothing about `--jar`; a real workload needs both.
/// Second, `cratonvm-cli` must not link `cratonvm-difftest`: the dependency runs
/// the wrong way (the fuzzer drives *this* binary as a subprocess), and the
/// shipped launcher has no business carrying a testing crate. So the comparison
/// is reimplemented here, small and self-contained, and the two doors are
/// documented as siblings in `docs/testing/diff-hotspot.md`.
///
/// # Why both sides are subprocesses
///
/// The CratonVM side is a re-exec of `current_exe()` with the diff flags
/// removed, not an in-process `Vm::new`. Three reasons, all of which showed up
/// in this tree's own differential history:
///
/// 1. **Capture.** A Java program writes through the VM's own fd table to the
///    real stdout. Comparing streams means owning the pipe, which means owning
///    the process.
/// 2. **No configuration skew.** `vm/tests/differential.rs` records a live bug
///    where an in-process embedder that never called a launcher setter produced
///    a "divergence" that was purely configuration. A re-exec runs the *same*
///    launcher through the *same* argv pipeline, so the side under test is by
///    construction the side a user would have run.
/// 3. **A crash is an observation.** A SEGV or an OOM abort in the VM under
///    test has to be reportable, not fatal to the comparison.
///
/// # The nondeterminism problem, and why this mode does not cry wolf
///
/// Identity hash codes, `HashMap` iteration order on some shapes, thread names,
/// wall-clock timestamps and absolute paths differ legitimately between any two
/// JVMs. A tool that reports those as bugs gets switched off, which is worse
/// than not shipping it. Three defences, in order of strength:
///
/// * **Self-consistency first.** CratonVM is run twice by default
///   (`--diff-runs`). A line that differs between two CratonVM runs cannot be a
///   HotSpot divergence — it is the *program* being nondeterministic. Those
///   lines are excluded from the verdict and reported as `unstable`.
/// * **Strict first, relaxed only to explain.** The verdict is byte-exact. Only
///   when it fails is the pair re-compared with the five nondeterminism maskers
///   on; if that makes the difference vanish, the finding is downgraded to
///   `noise` and the masked line is printed with the rule that accounts for it.
///   `--diff-strict` keeps it a failure.
/// * **`--diff-ignore <PATTERN>`**, repeatable, for the residue only the user
///   can name.
///
/// # Bytes, not lossily-decoded text
///
/// The two children are compared on what they actually wrote. HotSpot follows
/// the console's charset on `System.out` while CratonVM emits UTF-8, so the
/// reference side routinely produces bytes that are not valid UTF-8 — and
/// `String::from_utf8_lossy` would replace *HotSpot's own correct output* with
/// U+FFFD and blame the VM for a defect in this harness's decoder. `capture`
/// therefore uses `decode_lossless`, an injective byte-to-text escape, so
/// comparing the decoded strings is exactly as strong as comparing the byte
/// streams. When the two sides then differ only outside ASCII the report says
/// so and names the one-token re-run that settles it — a *hint*, never a
/// masker: a charset difference is a program-observable one, and forgiving it
/// would trade a false positive for a false negative.
///
/// See `docs/testing/diff-hotspot.md`.
mod diff_hotspot {
    use super::*;
    // Explicit (anonymous) trait import for `Args::try_parse_from`. The parent
    // module already has `use clap::Parser`, which `use super::*` re-exports
    // here, but naming it is cheaper than depending on that.
    use clap::Parser as _;
    use std::io::Read as _;
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    /// Both sides agreed on every compared observable.
    pub const EXIT_IDENTICAL: i32 = 0;
    /// A divergence that survived the nondeterminism maskers.
    pub const EXIT_DIVERGED: i32 = 1;
    /// The comparison could not be performed: no usable reference JDK, the
    /// CratonVM child could not be spawned, or the invocation was rejected.
    /// Distinct from `1` so a CI job can tell "HotSpot is missing on this
    /// runner" from "the VM is wrong".
    pub const EXIT_NO_REFERENCE: i32 = 2;
    /// CratonVM disagreed with *itself* across `--diff-runs`, and nothing
    /// outside that instability diverged. No verdict was reached on the
    /// unstable lines; a real divergence elsewhere still reports as `1`.
    pub const EXIT_UNSTABLE: i32 = 3;

    /// Set on the CratonVM child so a `--diff-hotspot` that somehow survived
    /// the argv strip below cannot recurse. Belt to the strip's braces: the
    /// strip is what actually prevents recursion, this is what makes a mistake
    /// in the strip terminate instead of forking forever. The one path that can
    /// still reach it is a nested argument file — `expand_argfiles` is
    /// deliberately non-recursive, so an `@outer` containing `@inner` leaves
    /// `@inner` in the child's argv for the child to expand.
    ///
    /// **Deliberately not spelled `CRATONVM_*`.** That prefix is a declared
    /// configuration surface (`types/src/flag_groups.rs::INVENTORY`, enforced by
    /// `tools/flag-census/check-surface.sh` and `types/tests/flag_surface.rs`),
    /// and this is not configuration — it is one process telling the child it
    /// just spawned that it is under comparison. Adding it to the inventory
    /// would put a piece of private plumbing in `docs/CONFIG.md`, which is how
    /// that surface reached 692 identifiers in the first place.
    const CHILD_GUARD: &str = "CVM_DIFF_HOTSPOT_CHILD";

    /// Diff-mode options that consume the following argv token as their value.
    /// Mirrors [`super::VALUE_TAKING_OPTS`]'s role for the launcher proper —
    /// the scan below has to know these so `--diff-ignore Foo` cannot leave
    /// `Foo` looking like the bare main-class token that ends the launcher
    /// section.
    const DIFF_VALUE_OPTS: &[&str] = &[
        "--diff-ignore",
        "--diff-java-arg",
        "--diff-runs",
        "--diff-timeout",
    ];

    /// Default per-child wall clock. Same number as the fuzzer's
    /// `runner::DEFAULT_TIMEOUT`, for the same reason: a CratonVM run that
    /// never finishes while HotSpot does is itself the finding, and it has to
    /// be *reported*, not waited on forever.
    const DEFAULT_TIMEOUT_SECS: u64 = 120;

    /// A parsed `--diff-hotspot` invocation.
    struct Request {
        /// argv for the CratonVM child, with every `--diff-*` token removed and
        /// everything else — `-XX:`, `-D`, `-cp`, `--jar`, the main class and
        /// the program's own arguments — preserved verbatim and in order.
        child_argv: Vec<String>,
        ignores: Vec<String>,
        java_extra: Vec<String>,
        runs: usize,
        timeout: Duration,
        strict: bool,
    }

    /// One captured child process.
    struct Capture {
        stdout: String,
        stderr: String,
        exit_code: Option<i32>,
        timed_out: bool,
        wall_ms: u64,
    }

    impl Capture {
        /// The exit channel's comparison token. A timeout and a signal kill are
        /// distinct, never-matching words, so neither can ever compare equal to
        /// a clean exit — the failure mode where a hang reads as `0`.
        fn exit_token(&self) -> String {
            if self.timed_out {
                "<timeout>".to_string()
            } else {
                match self.exit_code {
                    Some(c) => c.to_string(),
                    None => "<signal>".to_string(),
                }
            }
        }
    }

    // -----------------------------------------------------------------------
    // Entry point
    // -----------------------------------------------------------------------

    /// Scan argv; if `--diff-hotspot` was requested, perform the whole
    /// comparison and return the process exit code. `None` means "this is a
    /// normal launch, carry on".
    ///
    /// Called as the first statement of `main`, ahead of `install_flags` and
    /// `expand_process_env`, because this mode boots no VM in *this* process
    /// and must not be able to perturb the immutable configuration snapshot the
    /// child under test will latch for itself.
    pub fn maybe_run() -> Option<i32> {
        if std::env::var_os(CHILD_GUARD).is_some() {
            // We are the child of a comparison. Nothing to do — the parent
            // already removed the flags, so reaching here at all would mean the
            // strip missed one, and recursing would be worse than ignoring it.
            return None;
        }
        let raw: Vec<String> = std::env::args().collect();
        let request = match scan(&raw)? {
            Ok(r) => r,
            Err(msg) => {
                eprintln!("[diff-hotspot] {msg}");
                return Some(EXIT_NO_REFERENCE);
            }
        };
        Some(execute(request))
    }

    // -----------------------------------------------------------------------
    // Argument scan
    // -----------------------------------------------------------------------

    /// Extract the `--diff-*` options from the **launcher portion** of argv.
    ///
    /// This deliberately does *not* teach the existing pre-clap pipeline about
    /// the new flags. `insert_program_args_separator` finds the program
    /// selector by walking option tokens and stopping at the first bare one; a
    /// value-taking option it does not know about would make that option's
    /// value look like the main class. Rather than extend `VALUE_TAKING_OPTS` —
    /// which is load-bearing for `java` positional semantics and has ~60 unit
    /// tests behind it — the diff options are removed here, before any of that
    /// runs, so the pipeline sees exactly the argv it would have seen without
    /// this feature.
    ///
    /// The walk mirrors `insert_program_args_separator`'s structure on purpose:
    /// `-jar <x>` and the first bare token both end the launcher section, and
    /// everything after is the program's own — so a Java program is still free
    /// to take an argument spelled `--diff-ignore`.
    fn scan(raw: &[String]) -> Option<Result<Request, String>> {
        // Cheap pre-filter: no `--diff-hotspot` on the command line and no
        // argument file that could contain one means this costs one scan of
        // argv on every normal launch and nothing else.
        let literal = raw.iter().any(|a| a == "--diff-hotspot");
        let argfile = raw.iter().skip(1).any(|a| a.starts_with('@'));
        if !literal && !argfile {
            return None;
        }
        let argv = expand_argfiles(raw.to_vec());
        if argv.is_empty() || !argv.iter().any(|a| a == "--diff-hotspot") {
            return None;
        }

        let mut kept: Vec<String> = vec![argv[0].clone()];
        let mut ignores: Vec<String> = Vec::new();
        let mut java_extra: Vec<String> = Vec::new();
        let mut runs: usize = 2;
        let mut timeout_secs: u64 = DEFAULT_TIMEOUT_SECS;
        let mut strict = false;
        let mut enabled = false;

        let mut i = 1usize;
        while i < argv.len() {
            let a = argv[i].clone();
            let a = a.as_str();

            // An explicit separator: everything past it is the program's.
            if a == "--" {
                kept.extend_from_slice(&argv[i..]);
                break;
            }

            if a == "--diff-hotspot" {
                enabled = true;
                i += 1;
                continue;
            }
            if a == "--diff-strict" {
                strict = true;
                i += 1;
                continue;
            }
            if let Some(name) = DIFF_VALUE_OPTS.iter().copied().find(|n| is_opt(a, n)) {
                let value = match take_value(&argv, &mut i, name) {
                    Ok(v) => v,
                    Err(e) => return Some(Err(e)),
                };
                match name {
                    "--diff-ignore" => ignores.push(value),
                    "--diff-java-arg" => java_extra.push(value),
                    "--diff-runs" => match value.parse::<usize>() {
                        Ok(n) if n >= 1 => runs = n,
                        _ => {
                            return Some(Err(format!(
                                "--diff-runs expects a positive integer, got {value:?}"
                            )))
                        }
                    },
                    "--diff-timeout" => match value.parse::<u64>() {
                        Ok(n) if n >= 1 => timeout_secs = n,
                        _ => {
                            return Some(Err(format!(
                                "--diff-timeout expects a positive number of seconds, \
                                 got {value:?}"
                            )))
                        }
                    },
                    _ => {}
                }
                continue;
            }

            // `-jar <jar>` selects the program: it and everything after it are
            // copied verbatim.
            if (a == "-jar" || a == "--jar") && i + 1 < argv.len() {
                kept.extend_from_slice(&argv[i..]);
                break;
            }
            if a.starts_with("-jar=") || a.starts_with("--jar=") {
                kept.extend_from_slice(&argv[i..]);
                break;
            }
            // A launcher option that consumes the next token: copy both, keep
            // scanning; the value is not the main-class name.
            if VALUE_TAKING_OPTS.contains(&a) {
                kept.push(argv[i].clone());
                if i + 1 < argv.len() {
                    kept.push(argv[i + 1].clone());
                    i += 2;
                } else {
                    i += 1;
                }
                continue;
            }
            if a.starts_with('-') {
                kept.push(argv[i].clone());
                i += 1;
                continue;
            }
            // First bare token: the main class. The launcher section ends.
            kept.extend_from_slice(&argv[i..]);
            break;
        }

        if !enabled {
            // `--diff-hotspot` appeared, but only in the program's own argument
            // tail. It is that program's argument, not ours.
            return None;
        }

        Some(Ok(Request {
            child_argv: kept,
            ignores,
            java_extra,
            runs,
            timeout: Duration::from_secs(timeout_secs),
            strict,
        }))
    }

    /// Whether `tok` is `name` or `name=<value>`.
    fn is_opt(tok: &str, name: &str) -> bool {
        tok == name
            || (tok.len() > name.len()
                && tok.starts_with(name)
                && tok.as_bytes()[name.len()] == b'=')
    }

    /// Read the value of `name` at `argv[*i]`, accepting both the
    /// `--opt=value` and the `--opt value` spellings, and advance `*i` past
    /// every token consumed.
    fn take_value(argv: &[String], i: &mut usize, name: &str) -> Result<String, String> {
        let tok = argv[*i].clone();
        if tok == name {
            return match argv.get(*i + 1) {
                Some(v) => {
                    *i += 2;
                    Ok(v.clone())
                }
                None => {
                    *i += 1;
                    Err(format!("{name} requires a value"))
                }
            };
        }
        *i += 1;
        Ok(tok[name.len() + 1..].to_string())
    }

    // -----------------------------------------------------------------------
    // Reference JDK resolution (docs/CONFIG.md's four-step precedence)
    // -----------------------------------------------------------------------

    /// Where a resolved `java` came from, for the report and for the failure
    /// message that has to name all four steps.
    struct Reference {
        java: PathBuf,
        source: String,
        banner: String,
    }

    fn java_in(home: &str) -> PathBuf {
        let exe = if cfg!(windows) { "java.exe" } else { "java" };
        Path::new(home).join("bin").join(exe)
    }

    /// Resolve the reference `java` exactly as `docs/CONFIG.md` documents the
    /// `--java-home` precedence: the flag, then `CRATONVM_JAVA_HOME`, then
    /// `JAVA_HOME`, then `java` on `PATH`. Inventing a fifth rule here would
    /// make the reference JDK a different JDK from the one the CratonVM side
    /// loads its class library from, which is the one skew this mode can least
    /// afford.
    ///
    /// A candidate is rejected — and the walk continues to the next step —
    /// when `<java> -version` fails, **or** when the banner identifies
    /// CratonVM. That second case is not hypothetical: this repository ships a
    /// `java`-named alias binary (`--features java-bin-alias`) and a Maven
    /// shim tree, and `CRATONVM_JAVA_HOME` exists precisely because `JAVA_HOME`
    /// routinely points at one. Comparing CratonVM against CratonVM would
    /// report a serene, meaningless "no divergence".
    fn resolve_reference(
        explicit_home: Option<&str>,
        timeout: Duration,
    ) -> Result<Reference, String> {
        let mut tried: Vec<String> = Vec::new();
        let mut candidates: Vec<(String, PathBuf)> = Vec::new();
        if let Some(h) = explicit_home {
            candidates.push((format!("--java-home {h}"), java_in(h)));
        }
        if let Ok(h) = std::env::var("CRATONVM_JAVA_HOME") {
            candidates.push((format!("CRATONVM_JAVA_HOME={h}"), java_in(&h)));
        }
        if let Ok(h) = std::env::var("JAVA_HOME") {
            candidates.push((format!("JAVA_HOME={h}"), java_in(&h)));
        }
        candidates.push(("java on PATH".to_string(), PathBuf::from("java")));

        for (source, java) in candidates {
            let mut cmd = Command::new(&java);
            cmd.arg("-version");
            let cap = match capture(cmd, timeout) {
                Ok(c) => c,
                Err(e) => {
                    tried.push(format!("{source}: cannot run {} ({e})", java.display()));
                    continue;
                }
            };
            if cap.timed_out || cap.exit_code != Some(0) {
                tried.push(format!(
                    "{source}: `{} -version` exited {}",
                    java.display(),
                    cap.exit_token()
                ));
                continue;
            }
            // HotSpot prints the banner on stderr; read both and be tolerant.
            let banner_all = format!("{}\n{}", cap.stderr.trim(), cap.stdout.trim());
            if banner_all.to_ascii_lowercase().contains("cratonvm") {
                tried.push(format!(
                    "{source}: {} is a CratonVM launcher, not a reference JDK",
                    java.display()
                ));
                continue;
            }
            let banner = banner_all
                .lines()
                .map(str::trim)
                .find(|l| !l.is_empty())
                .unwrap_or("<no version banner>")
                .to_string();
            return Ok(Reference {
                java,
                source,
                banner,
            });
        }

        Err(format!(
            "no usable reference JDK. The four places looked at, in order:\n  \
             1. --java-home <PATH>\n  \
             2. CRATONVM_JAVA_HOME\n  \
             3. JAVA_HOME\n  \
             4. `java` on PATH\n\
             What each one gave:\n  {}",
            tried.join("\n  ")
        ))
    }

    // -----------------------------------------------------------------------
    // Subprocess capture
    // -----------------------------------------------------------------------

    /// Spawn `cmd` and capture stdout/stderr/exit status under `timeout`.
    ///
    /// Both pipes are drained on their own threads: a child that fills a 64 KiB
    /// pipe buffer would otherwise block forever while the parent polls for an
    /// exit that can never happen. On overrun the child is killed and
    /// `timed_out` is set — which the exit channel renders as a word that never
    /// compares equal to a clean exit.
    fn capture(mut cmd: Command, timeout: Duration) -> std::io::Result<Capture> {
        cmd.stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let start = Instant::now();
        let mut child = cmd.spawn()?;
        let mut out_pipe = child.stdout.take().expect("stdout piped");
        let mut err_pipe = child.stderr.take().expect("stderr piped");
        let out_thread = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = out_pipe.read_to_end(&mut buf);
            buf
        });
        let err_thread = std::thread::spawn(move || {
            let mut buf = Vec::new();
            let _ = err_pipe.read_to_end(&mut buf);
            buf
        });

        let mut timed_out = false;
        let status = loop {
            match child.try_wait()? {
                Some(st) => break st,
                None => {
                    if start.elapsed() > timeout {
                        let _ = child.kill();
                        let st = child.wait()?;
                        timed_out = true;
                        break st;
                    }
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
        };
        let stdout = out_thread.join().unwrap_or_default();
        let stderr = err_thread.join().unwrap_or_default();
        // `decode_lossless`, never `from_utf8_lossy`: the reference JDK's own
        // correct output is routinely not valid UTF-8 (it follows the console
        // charset), and a lossy decode would corrupt it into a divergence the
        // harness invented. See `decode_lossless`.
        Ok(Capture {
            stdout: decode_lossless(&stdout),
            stderr: decode_lossless(&stderr),
            exit_code: status.code(),
            timed_out,
            wall_ms: start.elapsed().as_millis() as u64,
        })
    }

    // -----------------------------------------------------------------------
    // Decoding the captured bytes without damaging either side
    // -----------------------------------------------------------------------

    /// The one character [`decode_lossless`] ever inserts. It introduces a
    /// two-hex-digit escape standing for exactly one raw byte that was not part
    /// of a valid UTF-8 sequence.
    ///
    /// `U+FDD0` is a Unicode *noncharacter*: permanently unassigned, and
    /// specified as never to be interchanged. Nothing a program under
    /// comparison prints is expected to contain it — but "expected" is not
    /// "guaranteed", and this design leans on the escape being unambiguous, so
    /// [`decode_lossless`] escapes it too.
    const BYTE_ESCAPE: char = '\u{FDD0}';

    const HEX_UPPER: &[u8; 16] = b"0123456789ABCDEF";

    /// Append the escape for one raw byte. Uppercase hex, so the rendering a
    /// reader sees (`\xE9`) matches how every hex dump in this tree spells one.
    fn push_byte_escape(out: &mut String, b: u8) {
        out.push(BYTE_ESCAPE);
        out.push(HEX_UPPER[(b >> 4) as usize] as char);
        out.push(HEX_UPPER[(b & 0x0f) as usize] as char);
    }

    /// Copy `s`, escaping any literal [`BYTE_ESCAPE`] the *program* printed as
    /// that character's own three UTF-8 bytes. Without this the sentinel would
    /// be ambiguous and the decode would stop being injective.
    fn push_escaping_sentinel(out: &mut String, s: &str) {
        if !s.contains(BYTE_ESCAPE) {
            out.push_str(s);
            return;
        }
        let mut buf = [0u8; 4];
        let sentinel_bytes = BYTE_ESCAPE.encode_utf8(&mut buf).as_bytes().to_vec();
        for c in s.chars() {
            if c == BYTE_ESCAPE {
                for b in &sentinel_bytes {
                    push_byte_escape(out, *b);
                }
            } else {
                out.push(c);
            }
        }
    }

    /// Decode a child's raw stream into text **without losing a byte**.
    ///
    /// The obvious spelling — `String::from_utf8_lossy` — was the original one,
    /// and it corrupts the *reference* side rather than CratonVM's. HotSpot
    /// derives `stdout.encoding` from the host (JEP 400 pinned `file.encoding`
    /// and deliberately left this one alone), so on a cp1252-style Windows
    /// console an `é` leaves HotSpot as the single byte `0xE9`, which is not
    /// valid UTF-8; `from_utf8_lossy` replaces it with U+FFFD and the harness
    /// then reports a divergence against a line HotSpot never wrote. CratonVM
    /// emits UTF-8 unconditionally, so its side decoded cleanly and only the
    /// reference side was damaged — the least defensible way for a differential
    /// harness to be wrong. Measured on Linux, no Windows box needed:
    /// `java -Dstdout.encoding=ISO-8859-1` writes `e9 3f 3f` where the UTF-8
    /// arm writes `c3 a9 e4 b8 ad f0 9f 98 80`. See
    /// `docs/known-issues/stdout-encoding-differs-from-hotspot-on-windows-20260901.md`.
    ///
    /// **Decoding each side with its own declared charset was the alternative,
    /// and was rejected.** The launcher links no charset library; "its own
    /// declared charset" would have to be discovered by starting a further JVM
    /// to ask; and the verdict would then rest on an *interpretation* of the
    /// bytes, which is exactly what a byte-exact verdict must not do.
    ///
    /// So: every byte that is part of a valid UTF-8 sequence decodes normally,
    /// and every byte that is not becomes a [`BYTE_ESCAPE`] plus two hex
    /// digits. The mapping is **injective**, and that is the whole point —
    /// comparing two decoded strings is exactly as strong as comparing the two
    /// byte streams, so the byte-exact verdict is genuinely byte-exact while
    /// every stage downstream (the line split, the maskers, `--diff-ignore`,
    /// the report) keeps working on `str` and is unchanged.
    ///
    /// Injectivity, spelled out, because it is the load-bearing claim:
    ///
    /// * A stream with no invalid byte and no literal sentinel maps to itself,
    ///   and its image contains no sentinel — so it cannot collide with
    ///   anything the escaping path produces.
    /// * A literal sentinel becomes the three escapes `EF`, `B7`, `90` in a
    ///   row. Three *invalid* bytes can never produce that: `EF B7 90` adjacent
    ///   **is** a valid sequence, so the walk below decodes it rather than
    ///   reaching the escape path.
    /// * Escapes are fixed width, so no escape is a prefix of another.
    ///
    /// The escapes never contain `\r` or `\n` (those bytes are valid UTF-8 and
    /// are never escaped), so `lines_of`'s CRLF and trailing-whitespace
    /// normalisation is unaffected and keeps operating on text exactly as
    /// before.
    fn decode_lossless(bytes: &[u8]) -> String {
        // Fast path: a clean UTF-8 stream with no sentinel — which is every
        // run on a UTF-8 host — costs one validation scan and one copy.
        if let Ok(s) = std::str::from_utf8(bytes) {
            if !s.contains(BYTE_ESCAPE) {
                return s.to_string();
            }
        }
        let mut out = String::with_capacity(bytes.len());
        let mut rest = bytes;
        loop {
            match std::str::from_utf8(rest) {
                Ok(s) => {
                    push_escaping_sentinel(&mut out, s);
                    return out;
                }
                Err(e) => {
                    let good = e.valid_up_to();
                    // `valid_up_to` is a char boundary by construction; the
                    // `unwrap_or` is unreachable and costs nothing to be safe.
                    let head = std::str::from_utf8(&rest[..good]).unwrap_or("");
                    push_escaping_sentinel(&mut out, head);
                    // `error_len() == None` means the input ended mid-sequence.
                    // Escape one byte and let the loop re-derive the rest, so
                    // there is one rule rather than two.
                    let bad = e.error_len().unwrap_or(1).max(1);
                    let end = (good + bad).min(rest.len());
                    for b in &rest[good..end] {
                        push_byte_escape(&mut out, *b);
                    }
                    // `end > good >= 0`, so `rest` shrinks every iteration.
                    rest = &rest[end..];
                }
            }
        }
    }

    /// Render a compared line for human eyes: the raw-byte escapes become
    /// `\xNN`, which is readable, where the sentinel itself would print as a
    /// replacement box and tell the reader nothing.
    ///
    /// Display only. This is deliberately *not* injective — a program that
    /// literally prints the four characters `\xE9` renders identically — and
    /// nothing downstream of this function compares its result. Every
    /// comparison in this module runs on the decoded string, not on this.
    fn for_display(s: &str) -> String {
        if !s.contains(BYTE_ESCAPE) {
            return s.to_string();
        }
        let mut out = String::with_capacity(s.len() + 8);
        for c in s.chars() {
            if c == BYTE_ESCAPE {
                out.push_str("\\x");
            } else {
                out.push(c);
            }
        }
        out
    }

    // -----------------------------------------------------------------------
    // Text hygiene and the nondeterminism maskers
    // -----------------------------------------------------------------------

    /// CRLF -> LF plus a trailing-whitespace trim, then split into lines.
    ///
    /// This is the one transform that is always on, and it is what keeps a
    /// trailing newline or a Windows line ending from ever being reported as a
    /// divergence. Its risk is stated rather than hidden: a program whose last
    /// byte is *deliberately* a bare `\r` or a trailing space cannot be
    /// distinguished from one whose last byte is not.
    fn lines_of(s: &str) -> Vec<String> {
        s.replace("\r\n", "\n")
            .trim_end()
            .lines()
            .map(|l| l.trim_end_matches('\r').to_string())
            .collect()
    }

    /// Whether a captured stderr line is CratonVM narrating itself rather than
    /// program output.
    ///
    /// Not optional, and not a user-facing knob: the launcher prints
    /// `[cratonvm] main-vm run() returned Ok` on **every** clean exit, so
    /// without this the stderr channel would report a divergence on every
    /// single run and the mode would be useless on its first invocation. The
    /// cost is stated in the docs — a program that itself prints `[cratonvm]`
    /// on stderr loses that line from the comparison.
    ///
    /// Same three shapes as the fuzzer's `vm-diagnostics` normalization rule
    /// (`difftest/src/normalize.rs`), kept in step deliberately.
    fn is_vm_diagnostic(line: &str) -> bool {
        let t = line.trim();
        t.contains("[cratonvm]") || t.contains("[NativeBridge]") || t.contains("cratonvm_")
    }

    fn is_hex(c: char) -> bool {
        c.is_ascii_hexdigit()
    }

    fn is_ident(c: char) -> bool {
        c.is_alphanumeric() || c == '_' || c == '$'
    }

    /// `Object.toString` identity hashes: `java.lang.Object@1b6d3586`.
    ///
    /// `System.identityHashCode` is explicitly unspecified — HotSpot derives it
    /// from a thread-local PRNG, CratonVM from the object address — so the two
    /// can never agree and neither number is a semantic observable. Requires at
    /// least four hex digits and a non-identifier terminator so `user@ab` and
    /// `foo@deadbeefzz` are left alone.
    fn mask_identity_hash(s: &str) -> String {
        let ch: Vec<char> = s.chars().collect();
        let mut out = String::with_capacity(s.len());
        let mut i = 0usize;
        while i < ch.len() {
            if ch[i] == '@' && i > 0 && is_ident(ch[i - 1]) {
                let mut j = i + 1;
                while j < ch.len() && is_hex(ch[j]) {
                    j += 1;
                }
                if j - i - 1 >= 4 && (j >= ch.len() || !is_ident(ch[j])) {
                    out.push_str("@<idhash>");
                    i = j;
                    continue;
                }
            }
            out.push(ch[i]);
            i += 1;
        }
        out
    }

    /// Bare `0x…` address blobs, as printed by Unsafe / DirectByteBuffer /
    /// MemorySegment diagnostics and JNI handles. Two VMs with different
    /// allocators can never agree on one.
    fn mask_hex_address(s: &str) -> String {
        let ch: Vec<char> = s.chars().collect();
        let mut out = String::with_capacity(s.len());
        let mut i = 0usize;
        while i < ch.len() {
            let starts_token = i == 0 || !is_ident(ch[i - 1]);
            if starts_token
                && ch[i] == '0'
                && i + 2 < ch.len()
                && (ch[i + 1] == 'x' || ch[i + 1] == 'X')
                && is_hex(ch[i + 2])
            {
                let mut j = i + 2;
                while j < ch.len() && is_hex(ch[j]) {
                    j += 1;
                }
                out.push_str("0x<addr>");
                i = j;
                continue;
            }
            out.push(ch[i]);
            i += 1;
        }
        out
    }

    /// Thread / pool / process ordinals. Two VMs schedule differently, so
    /// `pool-1-thread-3` on one side is `pool-1-thread-2` on the other for
    /// reasons that are not the program's semantics.
    fn mask_thread_id(s: &str) -> String {
        const MARKERS: &[&str] = &[
            "Thread-", "thread-", "worker-", "pool-", "pid=", "tid=", "nid=",
        ];
        let mut out = String::with_capacity(s.len());
        let mut rest = s;
        loop {
            let mut best: Option<(usize, &str)> = None;
            for &m in MARKERS {
                if let Some(k) = rest.find(m) {
                    if best.map_or(true, |(bk, _)| k < bk) {
                        best = Some((k, m));
                    }
                }
            }
            let (k, m) = match best {
                Some(v) => v,
                None => break,
            };
            let after = k + m.len();
            let digits = rest[after..]
                .chars()
                .take_while(char::is_ascii_digit)
                .count();
            out.push_str(&rest[..after]);
            if digits > 0 {
                out.push_str("<n>");
                rest = &rest[after + digits..];
            } else {
                rest = &rest[after..];
            }
        }
        out.push_str(rest);
        out
    }

    fn digits_at(ch: &[char], at: usize, n: usize) -> bool {
        at + n <= ch.len() && (at..at + n).all(|k| ch[k].is_ascii_digit())
    }

    /// ISO-8601-shaped dates and clock times. A wall-clock reading is not a
    /// property of the program under test.
    fn mask_timestamp(s: &str) -> String {
        let ch: Vec<char> = s.chars().collect();
        let mut out = String::with_capacity(s.len());
        let mut i = 0usize;
        while i < ch.len() {
            // yyyy-mm-dd
            if i + 10 <= ch.len()
                && digits_at(&ch, i, 4)
                && ch[i + 4] == '-'
                && digits_at(&ch, i + 5, 2)
                && ch[i + 7] == '-'
                && digits_at(&ch, i + 8, 2)
            {
                out.push_str("<date>");
                i += 10;
                continue;
            }
            // hh:mm:ss[.fff…]
            if i + 8 <= ch.len()
                && digits_at(&ch, i, 2)
                && ch[i + 2] == ':'
                && digits_at(&ch, i + 3, 2)
                && ch[i + 5] == ':'
                && digits_at(&ch, i + 6, 2)
            {
                let mut j = i + 8;
                if j < ch.len() && ch[j] == '.' {
                    let mut k = j + 1;
                    while k < ch.len() && ch[k].is_ascii_digit() {
                        k += 1;
                    }
                    if k > j + 1 {
                        j = k;
                    }
                }
                out.push_str("<time>");
                i = j;
                continue;
            }
            out.push(ch[i]);
            i += 1;
        }
        out
    }

    /// Absolute filesystem paths, reduced to their last segment. The build
    /// directory a program was run from is not an observable of the program.
    fn mask_abs_path(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        for piece in s.split_inclusive(char::is_whitespace) {
            let trimmed = piece.trim_end();
            let (tok, ws) = piece.split_at(trimmed.len());
            out.push_str(&reduce_path(tok));
            out.push_str(ws);
        }
        out
    }

    fn reduce_path(tok: &str) -> String {
        let posix = tok.starts_with('/') && tok.matches('/').count() >= 2;
        let win = {
            let b: Vec<char> = tok.chars().take(3).collect();
            b.len() == 3
                && b[0].is_ascii_alphabetic()
                && b[1] == ':'
                && (b[2] == '\\' || b[2] == '/')
        };
        if !posix && !win {
            return tok.to_string();
        }
        let last = tok
            .rsplit(|c: char| c == '/' || c == '\\')
            .next()
            .unwrap_or(tok);
        if last.is_empty() {
            tok.to_string()
        } else {
            last.to_string()
        }
    }

    /// The maskers, in application order, each with the name the report prints.
    const RELAX_RULES: &[(&str, fn(&str) -> String)] = &[
        ("identity-hash", mask_identity_hash),
        ("hex-address", mask_hex_address),
        ("thread-id", mask_thread_id),
        ("timestamp", mask_timestamp),
        ("absolute-path", mask_abs_path),
    ];

    /// Apply every masker, reporting which ones actually changed the line.
    fn relax_line(line: &str) -> (String, Vec<&'static str>) {
        let mut cur = line.to_string();
        let mut fired: Vec<&'static str> = Vec::new();
        for &(id, f) in RELAX_RULES {
            let next = f(&cur);
            if next != cur {
                fired.push(id);
                cur = next;
            }
        }
        (cur, fired)
    }

    // -----------------------------------------------------------------------
    // Naming a divergence that is shaped like a charset disagreement
    //
    // A hint, never a masker. The five entries in `RELAX_RULES` exist because
    // identity hashes and addresses are *unspecified* observables that two
    // conforming JVMs may legitimately disagree about. The characters a program
    // prints are not in that category — they are precisely what a JVM
    // differential is for — so nothing below ever changes a verdict or an exit
    // code. It only tells the reader which one-token re-run settles it.
    // -----------------------------------------------------------------------

    /// The ASCII skeleton of a compared line, plus whether the line held
    /// anything outside ASCII at all.
    ///
    /// Every maximal run of "this position held something the charset could not
    /// carry" collapses to a single `\u{0}`; everything else is kept verbatim.
    /// Three spellings count as such a position, because three different layers
    /// produce them:
    ///
    /// * a raw-byte escape from [`decode_lossless`] — a cp1252 `é` that reached
    ///   us as the single byte `0xE9`. Its two hex digits are swallowed with
    ///   it, so it counts as **one** position and not as three characters;
    /// * any other non-ASCII character — the UTF-8 side, which carries the
    ///   character intact;
    /// * `?` and `\u{1A}` (SUB) — what a JDK `CharsetEncoder` substitutes when
    ///   it cannot represent a character. `U+FFFD`, what a *decoder*
    ///   substitutes, is already covered by the non-ASCII arm.
    ///
    /// That is what lets HotSpot's `hello, ??? world` line up against
    /// CratonVM's `hello, é中😀 world`.
    fn ascii_skeleton(s: &str) -> (String, bool) {
        let mut out = String::with_capacity(s.len());
        let mut saw_non_ascii = false;
        let mut in_run = false;
        let mut chars = s.chars().peekable();
        while let Some(c) = chars.next() {
            let mark = if c == BYTE_ESCAPE {
                for _ in 0..2 {
                    if matches!(chars.peek(), Some(d) if d.is_ascii_hexdigit()) {
                        chars.next();
                    }
                }
                saw_non_ascii = true;
                true
            } else if !c.is_ascii() {
                saw_non_ascii = true;
                true
            } else {
                c == '?' || c == '\u{1A}'
            };
            if mark {
                if !in_run {
                    out.push('\u{0}');
                    in_run = true;
                }
            } else {
                in_run = false;
                out.push(c);
            }
        }
        (out, saw_non_ascii)
    }

    /// Whether two differing lines differ **only** outside ASCII — the shape a
    /// charset disagreement makes.
    ///
    /// The `saw_non_ascii` guard is what keeps this from firing on an ordinary
    /// ASCII difference: `x?y` against `x??y` has the same skeleton, but
    /// neither side left ASCII, so encoding cannot be the explanation and the
    /// hint stays quiet.
    ///
    /// It is a heuristic and is allowed to be, because it decides nothing. It
    /// over-fires on a genuine character-level divergence — CratonVM printing
    /// `é` where HotSpot prints `ü` is a real bug and this returns `true` for
    /// it — and the cost of that is one extra paragraph in a report that still
    /// says `DIVERGENCE` and still exits `1`. The re-run the paragraph asks for
    /// is what separates the two cases, and it separates them by *proof*: pin
    /// the charset on both sides and a real divergence survives.
    fn differs_only_outside_ascii(cvm: &str, java: &str) -> bool {
        if cvm == java {
            return false;
        }
        let (skel_c, non_ascii_c) = ascii_skeleton(cvm);
        let (skel_j, non_ascii_j) = ascii_skeleton(java);
        (non_ascii_c || non_ascii_j) && skel_c == skel_j
    }

    /// `--diff-ignore` matching: a plain substring, with `*` standing for any
    /// run of characters. Deliberately **not** a regular expression — the
    /// launcher links no regex engine, and promising regex syntax it cannot
    /// honour would be worse than saying plainly what this is.
    fn pattern_matches(pat: &str, line: &str) -> bool {
        if pat.is_empty() {
            return false;
        }
        if !pat.contains('*') {
            return line.contains(pat);
        }
        let mut pos = 0usize;
        for part in pat.split('*') {
            if part.is_empty() {
                continue;
            }
            match line[pos..].find(part) {
                Some(k) => pos += k + part.len(),
                None => return false,
            }
        }
        true
    }

    // -----------------------------------------------------------------------
    // Comparison
    // -----------------------------------------------------------------------

    /// Which line indices differ across two or more runs of the *same* VM.
    ///
    /// This is the load-bearing part of not crying wolf. A line that CratonVM
    /// does not reproduce against itself carries no information about HotSpot,
    /// and reporting it as a divergence is how a differential tool earns a
    /// reputation for noise and stops being run.
    ///
    /// When two runs disagree on line *count* the alignment past that point is
    /// gone, so everything from the first disagreement to the end is marked
    /// unstable — an honest over-approximation, and one the report states.
    fn instability(runs: &[Vec<String>]) -> Vec<bool> {
        let n = runs.iter().map(Vec::len).max().unwrap_or(0);
        let mut unstable = vec![false; n];
        if runs.len() < 2 {
            return unstable;
        }
        let base = &runs[0];
        let mut lengths_differ = false;
        for r in runs.iter().skip(1) {
            if r.len() != base.len() {
                lengths_differ = true;
            }
            for (i, flag) in unstable.iter_mut().enumerate() {
                if base.get(i) != r.get(i) {
                    *flag = true;
                }
            }
        }
        if lengths_differ {
            if let Some(k) = unstable.iter().position(|u| *u) {
                for flag in unstable.iter_mut().skip(k) {
                    *flag = true;
                }
            }
        }
        unstable
    }

    /// One stream, prepared for comparison: VM chatter removed (stderr only),
    /// `--diff-ignore` lines blanked, unstable lines blanked.
    ///
    /// Masking replaces a line rather than deleting it, so both sides keep the
    /// same indices and a reported line number still means something.
    fn prepare(
        lines: &[String],
        drop_vm_chatter: bool,
        ignores: &[String],
        unstable: &[bool],
    ) -> Vec<String> {
        lines
            .iter()
            .enumerate()
            .map(|(i, l)| {
                if drop_vm_chatter && is_vm_diagnostic(l) {
                    return "<vm-diagnostic>".to_string();
                }
                if unstable.get(i).copied().unwrap_or(false) {
                    return "<unstable>".to_string();
                }
                if ignores.iter().any(|p| pattern_matches(p, l)) {
                    return "<ignored>".to_string();
                }
                l.clone()
            })
            .collect()
    }

    /// The first index at which two prepared streams differ.
    fn first_line_diff(a: &[String], b: &[String]) -> Option<usize> {
        for i in 0..a.len().max(b.len()) {
            if a.get(i) != b.get(i) {
                return Some(i);
            }
        }
        None
    }

    fn at(lines: &[String], i: usize) -> String {
        lines
            .get(i)
            .cloned()
            .unwrap_or_else(|| "<no such line>".to_string())
    }

    // -----------------------------------------------------------------------
    // The reference-side command line
    // -----------------------------------------------------------------------

    /// Build the `java` argv for the reference side from the *same* parse the
    /// CratonVM side will perform.
    ///
    /// [`Args`] is reached through the identical pre-clap pipeline the real run
    /// uses (`insert_program_args_separator` -> `normalize_java_launcher_argv`
    /// -> `extract_system_properties` -> `extract_hotspot_flags` -> clap), so
    /// the two sides cannot disagree about which token was the main class or
    /// where the program's own arguments began. That skew is the classic way a
    /// differential harness blames the VM for its own bug.
    ///
    /// Only options that can change *program-observable* behaviour are
    /// forwarded: `-D` system properties, `-ea`, `-Xmx`, the module-system
    /// flags, `--enable-preview` and `--enable-native-access`. CratonVM-only
    /// options and the `-XX:` / `-agentlib:` family that `extract_hotspot_flags`
    /// removes before clap are deliberately *not* forwarded — see
    /// `docs/testing/diff-hotspot.md` — and `--diff-java-arg` is the escape
    /// hatch for the rest.
    fn reference_argv(
        parsed: &Args,
        sysprops: &[(String, String)],
        assertions: Option<bool>,
        extra: &[String],
    ) -> Result<Vec<String>, String> {
        if parsed.synthetic_jdk {
            return Err(
                "--synthetic-jdk cannot be compared against HotSpot: the synthetic class \
                 library is a deliberately different implementation of java.*, so every \
                 difference it produces is expected and the comparison would measure \
                 nothing. Drop --synthetic-jdk (or use --jdk-only, which stays on the real \
                 class library and IS meaningful here)."
                    .to_string(),
            );
        }
        let mut out: Vec<String> = Vec::new();
        for (k, v) in sysprops {
            out.push(format!("-D{k}={v}"));
        }
        if assertions == Some(true) {
            out.push("-ea".to_string());
        }
        if let Some(mx) = &parsed.max_heap {
            out.push(format!("-Xmx{mx}"));
        }
        if let Some(mp) = &parsed.module_path {
            out.push("--module-path".to_string());
            out.push(mp.clone());
        }
        for (flag, values) in [
            ("--add-reads", &parsed.add_reads),
            ("--add-exports", &parsed.add_exports),
            ("--add-opens", &parsed.add_opens),
            ("--add-modules", &parsed.add_modules),
        ] {
            for v in values.iter() {
                out.push(flag.to_string());
                out.push(v.clone());
            }
        }
        if parsed.enable_preview {
            out.push("--enable-preview".to_string());
        }
        if let Some(v) = &parsed.enable_native_access {
            out.push(format!("--enable-native-access={v}"));
        }
        out.extend(extra.iter().cloned());

        // `--jar` wins over `-cp`, exactly as the launcher itself does: the
        // classpath comes from the jar and its manifest `Class-Path`, so both
        // sides get the same jar and neither gets a stray `-cp`.
        if let Some(jar) = &parsed.jar {
            out.push("-jar".to_string());
            out.push(jar.clone());
        } else {
            match &parsed.class_name {
                Some(c) => {
                    if let Some(cp) = &parsed.classpath {
                        out.push("-cp".to_string());
                        out.push(cp.clone());
                    }
                    // Accept the internal slash spelling the launcher allows.
                    out.push(c.replace('/', "."));
                }
                None => {
                    return Err(
                        "--diff-hotspot needs a program to compare: pass a main class \
                         (`-cp <CP> <MainClass>`) or `--jar <FILE.jar>`."
                            .to_string(),
                    )
                }
            }
        }
        out.extend(parsed.args.iter().cloned());
        Ok(out)
    }

    // -----------------------------------------------------------------------
    // Driver
    // -----------------------------------------------------------------------

    #[allow(clippy::too_many_lines)]
    fn execute(req: Request) -> i32 {
        // Parse the child argv through the launcher's own pipeline, so the
        // reference side is built from the identical understanding of argv.
        let staged = insert_program_args_separator(req.child_argv.clone());
        let assertions = launcher_assertions_requested(&staged);
        let normalized = normalize_java_launcher_argv(staged);
        let (normalized, sysprops) = extract_system_properties(normalized);
        let (normalized, _hotspot_flags) = extract_hotspot_flags(normalized);
        let parsed = match Args::try_parse_from(normalized) {
            Ok(a) => a,
            Err(e) => {
                // clap renders `--help` / `--version` as an `Err` whose `Display`
                // *is* the banner, so print it verbatim rather than wrapping a
                // help screen inside a diagnostic sentence, then say why the run
                // stopped.
                eprint!("{e}");
                eprintln!(
                    "[diff-hotspot] those arguments do not parse as a launcher command \
                     line, so there is nothing to compare."
                );
                return EXIT_NO_REFERENCE;
            }
        };

        let ref_args = match reference_argv(&parsed, &sysprops, assertions, &req.java_extra) {
            Ok(a) => a,
            Err(msg) => {
                eprintln!("[diff-hotspot] {msg}");
                return EXIT_NO_REFERENCE;
            }
        };

        let reference = match resolve_reference(parsed.java_home.as_deref(), req.timeout) {
            Ok(r) => r,
            Err(msg) => {
                eprintln!("[diff-hotspot] {msg}");
                return EXIT_NO_REFERENCE;
            }
        };

        let exe = match std::env::current_exe() {
            Ok(p) => p,
            Err(e) => {
                eprintln!("[diff-hotspot] cannot locate this launcher's own binary: {e}");
                return EXIT_NO_REFERENCE;
            }
        };

        let program = match (&parsed.jar, &parsed.class_name) {
            (Some(j), _) => format!("--jar {j}"),
            (None, Some(c)) => c.clone(),
            (None, None) => "<none>".to_string(),
        };

        println!("=== cratonvm --diff-hotspot ===");
        println!("  program        : {program}");
        if let Some(cp) = &parsed.classpath {
            println!("  classpath      : {cp}");
        }
        println!("  cratonvm       : {}", exe.display());
        println!(
            "  reference java : {}  [{}]",
            reference.java.display(),
            reference.source
        );
        println!("                   {}", reference.banner);
        println!(
            "  runs           : cratonvm x{}, java x1, timeout {}s",
            req.runs,
            req.timeout.as_secs()
        );
        if !req.ignores.is_empty() {
            println!("  --diff-ignore  : {}", req.ignores.join(" | "));
        }
        println!();

        // --- CratonVM side, `--diff-runs` times ----------------------------
        let mut cvm: Vec<Capture> = Vec::with_capacity(req.runs);
        for n in 0..req.runs {
            let mut cmd = Command::new(&exe);
            cmd.args(&req.child_argv[1..]);
            cmd.env(CHILD_GUARD, "1");
            match capture(cmd, req.timeout) {
                Ok(c) => cvm.push(c),
                Err(e) => {
                    eprintln!(
                        "[diff-hotspot] could not run the CratonVM side (run {}): {e}",
                        n + 1
                    );
                    return EXIT_NO_REFERENCE;
                }
            }
        }

        // --- Reference side, once ------------------------------------------
        let mut cmd = Command::new(&reference.java);
        cmd.args(&ref_args);
        let hs = match capture(cmd, req.timeout) {
            Ok(c) => c,
            Err(e) => {
                eprintln!(
                    "[diff-hotspot] could not run the reference JDK ({}): {e}",
                    reference.java.display()
                );
                return EXIT_NO_REFERENCE;
            }
        };

        // --- Stability of the CratonVM side --------------------------------
        let cvm_out: Vec<Vec<String>> = cvm.iter().map(|c| lines_of(&c.stdout)).collect();
        let cvm_err: Vec<Vec<String>> = cvm.iter().map(|c| lines_of(&c.stderr)).collect();
        let unstable_out = instability(&cvm_out);
        let unstable_err = instability(&cvm_err);
        let exit_unstable = cvm.iter().any(|c| c.exit_token() != cvm[0].exit_token());
        let any_unstable =
            exit_unstable || unstable_out.iter().any(|u| *u) || unstable_err.iter().any(|u| *u);

        if any_unstable {
            let n_out = unstable_out.iter().filter(|u| **u).count();
            let n_err = unstable_err.iter().filter(|u| **u).count();
            let exit_note = if exit_unstable {
                ", and the exit status"
            } else {
                ""
            };
            println!(
                "unstable: the CratonVM side did not reproduce itself across {} runs \
                 ({n_out} stdout line(s), {n_err} stderr line(s){exit_note}).",
                req.runs
            );
            println!(
                "          Those lines carry no verdict and are excluded below — a line that \
                 differs\n          between two CratonVM runs is the *program* being \
                 nondeterministic, not a\n          HotSpot divergence. Mask them with \
                 --diff-ignore for a clean run."
            );
            for (i, u) in unstable_out.iter().enumerate() {
                if *u {
                    println!(
                        "          stdout {:>5} | {}",
                        i + 1,
                        for_display(&at(&cvm_out[0], i))
                    );
                }
            }
            println!();
        }

        // --- Prepare both sides ---------------------------------------------
        let hs_out_raw = lines_of(&hs.stdout);
        let hs_err_raw = lines_of(&hs.stderr);
        let c_out = prepare(&cvm_out[0], false, &req.ignores, &unstable_out);
        let h_out = prepare(&hs_out_raw, false, &req.ignores, &unstable_out);
        let c_err = prepare(&cvm_err[0], true, &req.ignores, &unstable_err);
        let h_err = prepare(&hs_err_raw, true, &req.ignores, &unstable_err);
        let c_exit = cvm[0].exit_token();
        let h_exit = hs.exit_token();

        let out_diff = first_line_diff(&c_out, &h_out);
        let err_diff = first_line_diff(&c_err, &h_err);
        let exit_diff = !exit_unstable && c_exit != h_exit;

        // Make the decode visible when it did anything. Non-UTF-8 bytes on
        // either side are the norm on a legacy Windows console, and a reader
        // who sees `\xE9` in the report below is owed the sentence that says
        // what it is and that it was compared and not repaired.
        if [&cvm[0].stdout, &cvm[0].stderr, &hs.stdout, &hs.stderr]
            .iter()
            .any(|s| s.contains(BYTE_ESCAPE))
        {
            println!(
                "  note           : one side wrote bytes that are not valid UTF-8. They are \
                 compared\n                   exactly, byte for byte, and shown below as \\xNN."
            );
        }

        let exit_word = if exit_diff {
            format!("DIFFER (cratonvm {c_exit}, java {h_exit})")
        } else {
            format!("agree ({c_exit})")
        };
        println!(
            "  stdout {:<9} stderr {:<9} exit-status {}",
            verdict_word(out_diff.is_none()),
            verdict_word(err_diff.is_none()),
            exit_word
        );
        println!(
            "  wall           : cratonvm {} ms, java {} ms",
            cvm[0].wall_ms, hs.wall_ms
        );
        println!();

        if out_diff.is_none() && err_diff.is_none() && !exit_diff {
            if any_unstable {
                println!(
                    "VERDICT: no divergence on the comparable output, but the CratonVM side was \
                     not\n         self-consistent — see the unstable lines above. Exit {}.",
                    EXIT_UNSTABLE
                );
                return EXIT_UNSTABLE;
            }
            println!(
                "VERDICT: no divergence. CratonVM and the reference JDK agree on stdout, \
                 stderr and exit status."
            );
            return EXIT_IDENTICAL;
        }

        // --- Something differed. Is it only known nondeterminism? -----------
        let relax = |v: &[String]| -> Vec<String> { v.iter().map(|l| relax_line(l).0).collect() };
        let rc_out = relax(&c_out);
        let rh_out = relax(&h_out);
        let rc_err = relax(&c_err);
        let rh_err = relax(&h_err);
        let r_out_diff = first_line_diff(&rc_out, &rh_out);
        let r_err_diff = first_line_diff(&rc_err, &rh_err);

        if r_out_diff.is_none() && r_err_diff.is_none() && !exit_diff {
            let idx = out_diff.or(err_diff).unwrap_or(0);
            let (c, h) = if out_diff.is_some() {
                (&c_out, &h_out)
            } else {
                (&c_err, &h_err)
            };
            let stream = if out_diff.is_some() { "stdout" } else { "stderr" };
            let rules = relax_line(&at(c, idx)).1;
            let rule_list = if rules.is_empty() {
                "masked".to_string()
            } else {
                rules.join(", ")
            };
            println!(
                "VERDICT: no semantic divergence. The output differs only in shapes that are \
                 not\n         program-observable ({rule_list}). First such line, on \
                 {stream}:"
            );
            print_context(idx, c, h);
            println!();
            println!(
                "         Identity hash codes, addresses, thread ordinals, timestamps and \
                 absolute\n         paths differ legitimately between any two JVMs. Pass \
                 --diff-strict to treat\n         this as a failure, or --diff-ignore \
                 <PATTERN> to mask the line outright."
            );
            return if req.strict {
                EXIT_DIVERGED
            } else {
                EXIT_IDENTICAL
            };
        }

        // --- A real divergence. Report the first one, with context. ---------
        //
        // `encoding_shaped` names the cause when the two sides agree on every
        // ASCII character and differ only outside it. It changes neither the
        // verdict nor the exit code — see `print_encoding_hint`.
        println!("VERDICT: DIVERGENCE.");
        let mut encoding_shaped = false;
        if let Some(i) = r_out_diff {
            println!("  first divergence: stdout, line {}", i + 1);
            print_context(i, &c_out, &h_out);
            encoding_shaped = differs_only_outside_ascii(&at(&c_out, i), &at(&h_out, i));
        } else if let Some(i) = r_err_diff {
            println!("  first divergence: stderr, line {}", i + 1);
            print_context(i, &c_err, &h_err);
            encoding_shaped = differs_only_outside_ascii(&at(&c_err, i), &at(&h_err, i));
        } else {
            println!(
                "  first divergence: exit status — cratonvm {c_exit}, reference java {h_exit}; \
                 stdout and stderr agree."
            );
            let tail: Vec<&String> = cvm_err[0].iter().rev().take(10).collect();
            if !tail.is_empty() {
                println!("  last lines of the CratonVM stderr:");
                for l in tail.into_iter().rev() {
                    println!("    {}", for_display(l));
                }
            }
        }
        println!();
        if encoding_shaped {
            print_encoding_hint();
            println!();
        }
        println!(
            "  Before filing this: identity hash codes, HashMap iteration order on some \
             shapes,\n  timestamps, thread interleaving and absolute paths differ legitimately \
             between any\n  two JVMs. The five maskers (identity-hash, hex-address, thread-id, \
             timestamp,\n  absolute-path) were applied and the difference survived them, and \
             {} CratonVM run(s)\n  agreed with each other on this line — but a program-level \
             race can still defeat\n  both. Re-run with --diff-runs 5 if you are unsure.",
            req.runs
        );
        EXIT_DIVERGED
    }

    fn verdict_word(agree: bool) -> String {
        if agree {
            "agree".to_string()
        } else {
            "DIFFER".to_string()
        }
    }

    /// The differing line and the three before it. A wall of diff is what
    /// people already have; one line with its lead-in is what makes a report
    /// usable without opening a second terminal.
    fn print_context(idx: usize, c: &[String], h: &[String]) {
        let start = idx.saturating_sub(3);
        for i in start..idx {
            println!("      {:>5} | {}", i + 1, for_display(&at(c, i)));
        }
        println!("    cratonvm {:>5} | {}", idx + 1, for_display(&at(c, idx)));
        println!("    java     {:>5} | {}", idx + 1, for_display(&at(h, idx)));
    }

    /// Name a divergence whose two sides differ only outside ASCII, and point
    /// at the one re-run that settles whether it is encoding or semantics.
    ///
    /// **This is a hint, not a masker.** It is printed *after* the verdict, the
    /// verdict is still `DIVERGENCE`, and the exit code is still
    /// [`EXIT_DIVERGED`]. Nothing here can turn a red run green — see the
    /// comment above `ascii_skeleton` for why an encoding difference must not
    /// be forgiven the way an identity hash is.
    fn print_encoding_hint() {
        println!(
            "  Those two lines are identical everywhere except outside ASCII. That is the\n  \
             shape a *charset* disagreement makes, not the shape a semantic one makes.\n  \
             HotSpot derives stdout.encoding from the host — JEP 400 pinned file.encoding\n  \
             and deliberately left this one alone — while CratonVM answers UTF-8\n  \
             unconditionally, so on a non-UTF-8 console the two VMs write the same\n  \
             characters as different bytes. Settle it in one run; -D properties are\n  \
             forwarded to both sides, and UTF-8 is the one value the specification\n  \
             blesses for these keys:\n\n      \
             cratonvm --diff-hotspot -Dstdout.encoding=UTF-8 -Dstderr.encoding=UTF-8 \
             <the same arguments>\n\n  \
             If the divergence disappears, it was encoding and the characters agreed all\n  \
             along. If it survives, it is a real finding. This paragraph is a hint and not\n  \
             a mask: the verdict above is still DIVERGENCE and the exit code is still 1.\n  \
             Background: docs/testing/diff-hotspot.md and the known-issue page\n  \
             stdout-encoding-differs-from-hotspot-on-windows-20260901.md."
        );
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn a_trailing_newline_is_not_a_divergence() {
            assert_eq!(lines_of("a\nb\n"), lines_of("a\nb"));
            assert_eq!(lines_of("a\r\nb\r\n"), lines_of("a\nb"));
        }

        #[test]
        fn identity_hashes_are_masked_but_short_at_suffixes_are_not() {
            assert_eq!(
                mask_identity_hash("java.lang.Object@1b6d3586 end"),
                "java.lang.Object@<idhash> end"
            );
            assert_eq!(mask_identity_hash("user@ab"), "user@ab");
            assert_eq!(mask_identity_hash("@1b6d3586"), "@1b6d3586");
        }

        #[test]
        fn addresses_and_thread_ordinals_are_masked() {
            assert_eq!(mask_hex_address("at 0xdeadbeef!"), "at 0x<addr>!");
            assert_eq!(
                mask_thread_id("pool-1-thread-13 ran"),
                "pool-<n>-thread-<n> ran"
            );
        }

        #[test]
        fn timestamps_and_absolute_paths_are_masked() {
            assert_eq!(mask_timestamp("2026-09-01T12:34:56.789Z"), "<date>T<time>Z");
            assert_eq!(
                mask_abs_path("read /home/me/x/Foo.txt ok"),
                "read Foo.txt ok"
            );
            assert_eq!(mask_abs_path("ratio 3/4 ok"), "ratio 3/4 ok");
        }

        /// The bug this decode replaced: `String::from_utf8_lossy` turned
        /// HotSpot's *own correct* cp1252/ISO-8859-1 `é` — the single byte
        /// `0xE9` — into U+FFFD, and the harness then reported a divergence
        /// against a line HotSpot never wrote. Nothing is lost now, and the
        /// mapping is injective, which is what makes the byte-exact verdict
        /// byte-exact.
        #[test]
        fn a_non_utf8_reference_byte_survives_the_decode() {
            assert_eq!(for_display(&decode_lossless(b"h\xE9llo")), "h\\xE9llo");
            // Injective: two different byte streams cannot decode alike.
            assert_ne!(decode_lossless(b"h\xE9llo"), decode_lossless(b"h\xEAllo"));
            // Valid UTF-8 is untouched, so the common path is unchanged.
            assert_eq!(decode_lossless("héllo".as_bytes()), "héllo");
            // A truncated sequence escapes byte by byte and still terminates.
            assert_eq!(for_display(&decode_lossless(b"a\xEF\xB7b")), "a\\xEF\\xB7b");
        }

        /// The sentinel is escaped as its own three bytes, or a program that
        /// printed U+FDD0 would be indistinguishable from a raw byte.
        #[test]
        fn the_escape_sentinel_escapes_itself() {
            let decoded = decode_lossless("a\u{FDD0}b".as_bytes());
            assert_eq!(for_display(&decoded), "a\\xEF\\xB7\\x90b");
            assert_ne!(decoded, "a\u{FDD0}b");
        }

        /// A hint, and only where it belongs: the encoding shape is recognised
        /// on the witness from the known-issue page (both the `?`-substituting
        /// and the raw-byte spellings of the reference side), and an ordinary
        /// ASCII difference does not trip it.
        #[test]
        fn an_encoding_shaped_divergence_is_recognised_and_an_ascii_one_is_not() {
            assert!(differs_only_outside_ascii(
                "hello, é中😀 world",
                "hello, ??? world"
            ));
            // cp1252: `é` reaches us as one raw byte, the rest substitute.
            assert!(differs_only_outside_ascii(
                "hello, é中😀 world",
                &decode_lossless(b"hello, \xE9?? world")
            ));
            // A numeric divergence is not encoding, and neither is a `?` count.
            assert!(!differs_only_outside_ascii("0.3", "0.30000000000000004"));
            assert!(!differs_only_outside_ascii("x?y", "x??y"));
            assert!(!differs_only_outside_ascii("same", "same"));
        }

        #[test]
        fn ignore_patterns_are_substrings_with_a_star_wildcard() {
            assert!(pattern_matches("elapsed", "total elapsed 5ms"));
            assert!(pattern_matches("took*ms", "it took 5 ms"));
            assert!(!pattern_matches("took*ns", "it took 5 ms"));
        }

        #[test]
        fn a_line_that_moves_between_two_cratonvm_runs_is_unstable() {
            let a = vec!["x".to_string(), "1".to_string()];
            let b = vec!["x".to_string(), "2".to_string()];
            assert_eq!(instability(&[a, b]), vec![false, true]);
        }

        /// The whole point of the dedicated scan: a `--diff-hotspot` in the
        /// program's own argument tail belongs to the program, and the flags
        /// this mode owns never reach the launcher's argv pipeline.
        #[test]
        fn the_scan_leaves_a_normal_launch_alone_and_strips_only_its_own_flags() {
            let normal: Vec<String> = ["cratonvm", "-cp", "b", "Main", "--diff-hotspot"]
                .iter()
                .map(|s| (*s).to_string())
                .collect();
            assert!(scan(&normal).is_none());

            let asked: Vec<String> = [
                "cratonvm",
                "--diff-hotspot",
                "--diff-ignore",
                "noise",
                "-cp",
                "b",
                "Main",
                "--diff-hotspot",
            ]
            .iter()
            .map(|s| (*s).to_string())
            .collect();
            let req = scan(&asked).expect("requested").expect("parsed");
            assert_eq!(req.ignores, vec!["noise".to_string()]);
            assert_eq!(req.runs, 2);
            assert_eq!(
                req.child_argv,
                vec![
                    "cratonvm".to_string(),
                    "-cp".to_string(),
                    "b".to_string(),
                    "Main".to_string(),
                    "--diff-hotspot".to_string(),
                ]
            );
        }

        /// `--opt=value` and `--opt value` both work, and a `--jar` selector
        /// ends the launcher section just as it does for the real parse.
        #[test]
        fn inline_values_and_the_jar_selector() {
            let asked: Vec<String> = [
                "cratonvm",
                "--diff-hotspot",
                "--diff-runs=1",
                "--diff-strict",
                "--jar",
                "app.jar",
                "--diff-ignore",
                "mine",
            ]
            .iter()
            .map(|s| (*s).to_string())
            .collect();
            let req = scan(&asked).expect("requested").expect("parsed");
            assert_eq!(req.runs, 1);
            assert!(req.strict);
            // The `--diff-ignore` after the jar is the program's argument.
            assert!(req.ignores.is_empty());
            assert_eq!(
                req.child_argv,
                vec![
                    "cratonvm".to_string(),
                    "--jar".to_string(),
                    "app.jar".to_string(),
                    "--diff-ignore".to_string(),
                    "mine".to_string(),
                ]
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cratonvm_vm::config::{
        CompatibilityMode, JdkMode, EMBEDDED_DEFAULT_JDK_MODE, LAUNCHER_DEFAULT_JDK_MODE,
        SYNTHETIC_JDK_COMPILED_IN,
    };

    // -----------------------------------------------------------------------
    // JDK-mode determinism (2026-07-26)
    //
    // The launcher used to pick its standard library by sniffing the host
    // (`use_synthetic_jdk = detect_real_jdk().is_none()`). These tests pin
    // the replacement contract: fixed default, symmetric explicit flags,
    // no silent fallback, and a mode that is visible in `-version`.
    // -----------------------------------------------------------------------

    fn tokens(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn launcher_default_mode_is_real_jdk() {
        assert_eq!(LAUNCHER_DEFAULT_JDK_MODE, JdkMode::Real);
        assert_eq!(
            scan_requested_jdk_mode(&tokens(&["cratonvm", "Main", "--"])),
            JdkMode::Real
        );
        // ...and the embedding default deliberately differs.
        assert_eq!(EMBEDDED_DEFAULT_JDK_MODE, JdkMode::Synthetic);
    }

    #[test]
    fn explicit_mode_flags_are_symmetric() {
        assert_eq!(
            scan_requested_jdk_mode(&tokens(&["cratonvm", "--synthetic-jdk", "Main", "--"])),
            JdkMode::Synthetic
        );
        assert_eq!(
            scan_requested_jdk_mode(&tokens(&["cratonvm", "--real-jdk", "Main", "--"])),
            JdkMode::Real
        );
    }

    /// A program argument after `--` must never be mistaken for a launcher
    /// mode flag.
    #[test]
    fn mode_flags_after_separator_are_program_args() {
        assert_eq!(
            scan_requested_jdk_mode(&tokens(&["cratonvm", "Main", "--", "--synthetic-jdk"])),
            JdkMode::Real
        );
    }

    #[test]
    fn both_mode_flags_is_an_error_not_a_silent_winner() {
        let err = resolve_jdk_mode(true, true, None).expect_err("both flags must be rejected");
        let msg = format!("{err:#}");
        assert!(msg.contains("mutually exclusive"), "{msg}");
    }

    /// Selecting synthetic mode in a build without the `synthetic-jdk`
    /// Cargo feature must abort the launch. Booting anyway would register
    /// none of the ~5,200 stubs *and* skip boot-classpath discovery,
    /// producing a VM with no class library at all.
    #[test]
    fn synthetic_mode_requires_the_cargo_feature() {
        let result = resolve_jdk_mode(true, false, None);
        assert_eq!(result.is_ok(), SYNTHETIC_JDK_COMPILED_IN);
        if let Err(e) = result {
            let msg = format!("{e:#}");
            assert!(msg.contains("synthetic-jdk"), "{msg}");
        }
    }

    /// The general-bugs TODO's "update the usage docs accordingly": the
    /// `synthetic-jdk` build requirement is a property a user hits at launch,
    /// so `--help` has to state it. The rejection message alone is not
    /// documentation — it only appears after the run has already failed.
    #[test]
    fn the_usage_text_states_the_synthetic_jdk_build_requirement() {
        // `LONG_ABOUT` is hand-wrapped to the help column, so a phrase can be
        // split across lines with the next line's indent in between. Collapse
        // whitespace before matching rather than pinning today's line breaks.
        let flat = LONG_ABOUT.split_whitespace().collect::<Vec<_>>().join(" ");
        assert!(
            flat.contains("synthetic-jdk` Cargo feature"),
            "--help must name the Cargo feature --synthetic-jdk needs"
        );
        assert!(
            flat.contains("jdk.mode.synthetic_compiled_in"),
            "--help must point at the way to check whether THIS build has it"
        );
    }

    /// `--real-jdk` must select the real library outright, including in a
    /// build that does have the synthetic one compiled in. The two flags are
    /// symmetric selections, not a preference the build configuration can
    /// override.
    #[test]
    fn real_jdk_flag_selects_real_mode_regardless_of_the_synthetic_feature() {
        // Point at a nonexistent JAVA_HOME so the mode is decided without
        // depending on whether this machine has a JDK: an `Err` naming the
        // real-JDK search proves real mode was chosen, and an `Ok` carries the
        // mode directly.
        match resolve_jdk_mode(false, true, Some("/definitely/not/a/jdk/anywhere")) {
            Ok((mode, _)) => assert_eq!(mode, cratonvm_vm::config::JdkMode::Real),
            Err(e) => {
                let msg = format!("{e:#}");
                assert!(
                    msg.contains("no usable JDK was found"),
                    "--real-jdk must fail through the REAL-JDK path, not fall \
                     back to synthetic: {msg}"
                );
            }
        }
    }

    /// Real-JDK mode with a bogus `--java-home` must fail loudly, naming
    /// what was searched — never fall through to the synthetic library.
    #[test]
    fn real_mode_without_a_jdk_fails_loudly() {
        let err = resolve_jdk_mode(false, false, Some("/definitely/not/a/jdk/anywhere"))
            .expect_err("a nonexistent --java-home must not be tolerated");
        let msg = format!("{err:#}");
        assert!(msg.contains("no usable JDK was found"), "{msg}");
        assert!(msg.contains("jmods/java.base.jmod"), "{msg}");
        assert!(msg.contains("lib/modules"), "{msg}");
        // The message must offer the other mode explicitly rather than
        // silently taking it.
        assert!(msg.contains("--synthetic-jdk"), "{msg}");
    }

    // ── version banner ───────────────────────────────────────────────

    #[test]
    fn version_query_is_recognised_in_the_launcher_section() {
        for (tok, expected) in [
            ("-version", VersionQuery::Version),
            ("--version", VersionQuery::Version),
            ("-fullversion", VersionQuery::Full),
            ("-Xinternalversion", VersionQuery::Internal),
            ("-showversion", VersionQuery::Show),
        ] {
            let got = scan_version_query(&tokens(&["cratonvm", tok]));
            assert_eq!(got.map(|(q, _)| q), Some(expected), "token {tok}");
        }
        // After the program selector these are program args.
        assert!(scan_version_query(&tokens(&["cratonvm", "Main", "--", "-version"])).is_none());
        assert!(scan_version_query(&tokens(&["cratonvm", "Main", "--"])).is_none());
    }

    #[test]
    fn early_nojit_scan_ignores_java_program_arguments() {
        assert!(launcher_nojit_requested(&tokens(&[
            "cratonvm", "--nojit", "Main", "--"
        ])));
        assert!(!launcher_nojit_requested(&tokens(&[
            "cratonvm", "Main", "--", "--nojit"
        ])));
    }

    /// The launcher-side scan for `--dump-phase-report`. It has to run before
    /// the flag snapshot is latched, so it cannot go through clap; this pins
    /// both accepted spellings and the `--` boundary that keeps a Java program
    /// argument of the same name from being consumed.
    #[test]
    fn phase_report_scan_accepts_both_spellings_and_stops_at_the_separator() {
        assert_eq!(
            launcher_phase_report_path(&tokens(&[
                "cratonvm",
                "--dump-phase-report",
                "/tmp/p.json",
                "Main",
                "--"
            ])),
            Some("/tmp/p.json".to_string())
        );
        assert_eq!(
            launcher_phase_report_path(&tokens(&[
                "cratonvm",
                "--dump-phase-report=/tmp/p.json",
                "Main",
                "--"
            ])),
            Some("/tmp/p.json".to_string())
        );
        // Past the separator it is the Java program's argument, not ours.
        assert_eq!(
            launcher_phase_report_path(&tokens(&[
                "cratonvm",
                "Main",
                "--",
                "--dump-phase-report",
                "/tmp/p.json"
            ])),
            None
        );
        // A trailing option with no value is not a panic and not a guess.
        assert_eq!(
            launcher_phase_report_path(&tokens(&["cratonvm", "--dump-phase-report"])),
            None
        );
        assert_eq!(
            launcher_phase_report_path(&tokens(&["cratonvm", "Main"])),
            None
        );
    }

    /// The launcher consumes the option, but clap must still accept it or the
    /// parse in `run()` would fail with "unexpected argument".
    #[test]
    fn clap_accepts_the_phase_report_option() {
        let parsed = Args::try_parse_from(tokens(&[
            "cratonvm",
            "--dump-phase-report",
            "/tmp/p.json",
            "Main",
        ]))
        .expect("clap must accept --dump-phase-report");
        assert_eq!(parsed.dump_phase_report.as_deref(), Some("/tmp/p.json"));
    }

    /// The whole point of routing version output through our own banner:
    /// it must name the active class library.
    #[test]
    fn version_banner_names_the_jdk_mode() {
        let banner = version_banner(
            VersionQuery::Version,
            JdkMode::Synthetic,
            CompatibilityMode::Compatible,
            None,
        );
        assert!(banner.contains("cratonvm version"), "{banner}");
        assert!(banner.contains("synthetic-jdk"), "{banner}");
        let banner = version_banner(
            VersionQuery::Version,
            JdkMode::Real,
            CompatibilityMode::Compatible,
            None,
        );
        assert!(banner.contains("real-jdk"), "{banner}");
    }

    #[test]
    fn internal_version_reports_both_defaults_and_the_search_path() {
        let banner = version_banner(
            VersionQuery::Internal,
            JdkMode::Real,
            CompatibilityMode::Compatible,
            None,
        );
        for needle in [
            "jdk.mode.active",
            "jdk.compatibility.mode",
            "jdk.mode.default.launcher",
            "jdk.mode.default.embedded",
            "jdk.mode.synthetic_compiled_in",
            "never host-detected",
            "JAVA_HOME",
            "java on PATH",
        ] {
            assert!(banner.contains(needle), "missing {needle}:\n{banner}");
        }
    }

    #[test]
    fn version_stream_matches_hotspot_conventions() {
        // `java -version` → stderr (build tools scrape it there);
        // `java --version` → stdout.
        assert!(!VersionQuery::Version.to_stdout("-version"));
        assert!(VersionQuery::Version.to_stdout("--version"));
        assert!(VersionQuery::Version.exits());
        assert!(!VersionQuery::Show.exits());
    }

    #[test]
    fn showversion_token_is_stripped_before_clap() {
        let mut argv = tokens(&["cratonvm", "-showversion", "Main", "--", "-showversion"]);
        remove_first_launcher_token(&mut argv, "-showversion");
        // Only the launcher-section occurrence is removed; the program arg
        // after `--` survives.
        assert_eq!(argv, tokens(&["cratonvm", "Main", "--", "-showversion"]));
    }

    #[test]
    fn explicit_java_home_is_visible_to_the_banner() {
        assert_eq!(
            scan_explicit_java_home(&tokens(&[
                "cratonvm",
                "--java-home",
                "/opt/jdk",
                "Main",
                "--"
            ])),
            Some("/opt/jdk".to_string())
        );
        assert_eq!(
            scan_explicit_java_home(&tokens(&["cratonvm", "--java-home=/opt/jdk", "Main", "--"])),
            Some("/opt/jdk".to_string())
        );
        assert_eq!(
            scan_explicit_java_home(&tokens(&["cratonvm", "Main", "--", "--java-home", "/x"])),
            None
        );
    }

    /// Both spellings of every JPMS `--add-*` flag must parse, and must not
    /// swallow the following argument.
    ///
    /// HotSpot accepts `--add-opens M/P=T` and `--add-opens=M/P=T` alike. A
    /// launcher that consumed the NEXT token as part of a space-separated
    /// value would eat `-cp` and its directory, and the symptom would be
    /// `Could not find or load main class` — which reads as a broken program
    /// or a bad classpath, not as a mis-parsed flag. That misreading is not
    /// hypothetical: W7-56-infercaller-strict.md reported exactly this defect
    /// and told readers to prefer the `=` spelling on CratonVM. It was wrong;
    /// re-measured 2026-08-12, all eight combinations below already worked, on
    /// the then-current binary AND on the pre-merge control. These assertions
    /// exist so the claim can be settled by running the tests rather than by
    /// re-deriving it, and so a future arg-parsing change cannot make it true.
    ///
    /// The `-cp` and main-class assertions are the load-bearing half: a test
    /// that only checked the flag's own value would pass against a launcher
    /// that swallowed the classpath.
    ///
    /// Parsed through `normalize_java_launcher_argv`, which is what the real
    /// launcher does before clap sees anything. Calling `Args::try_parse_from`
    /// directly is NOT equivalent and would test the wrong thing: clap reads a
    /// bare `-cp /tmp/cp` as short `-c` with the attached value `p`, so the
    /// assertion below fails for a reason that has nothing to do with the flag
    /// under test. Measured while writing this test.
    #[test]
    fn add_star_flags_accept_both_spellings_without_eating_the_next_arg() {
        // (flag, value, accessor label) — one row per JPMS add-* flag.
        let cases: &[(&str, &str)] = &[
            ("--add-opens", "java.logging/java.util.logging=ALL-UNNAMED"),
            ("--add-exports", "java.base/java.lang=ALL-UNNAMED"),
            ("--add-reads", "java.logging=ALL-UNNAMED"),
            ("--add-modules", "java.logging"),
        ];
        for (flag, value) in cases {
            // Space-separated: `--flag VALUE -cp DIR Main`
            let spaced = Args::try_parse_from(normalize_java_launcher_argv(tokens(&[
                "cratonvm", flag, value, "-cp", "/tmp/cp", "Main",
            ])))
            .unwrap_or_else(|e| panic!("clap must accept `{flag} {value}`: {e}"));

            // Joined: `--flag=VALUE -cp DIR Main`
            let joined_flag = format!("{flag}={value}");
            let joined = Args::try_parse_from(normalize_java_launcher_argv(tokens(&[
                "cratonvm",
                &joined_flag,
                "-cp",
                "/tmp/cp",
                "Main",
            ])))
            .unwrap_or_else(|e| panic!("clap must accept `{joined_flag}`: {e}"));

            for (spelling, parsed) in [("space", &spaced), ("equals", &joined)] {
                // The flag's own value survived.
                let got: &[String] = match *flag {
                    "--add-opens" => &parsed.add_opens,
                    "--add-exports" => &parsed.add_exports,
                    "--add-reads" => &parsed.add_reads,
                    "--add-modules" => &parsed.add_modules,
                    other => unreachable!("unlisted flag {other}"),
                };
                assert_eq!(
                    got,
                    &[value.to_string()],
                    "{flag} ({spelling}) lost or mangled its own value"
                );

                // ...and, the half that actually catches the reported defect,
                // the FOLLOWING option was not consumed as part of it.
                assert_eq!(
                    parsed.classpath.as_deref(),
                    Some("/tmp/cp"),
                    "{flag} ({spelling}) swallowed -cp; this is the defect that                      surfaces as `Could not find or load main class`"
                );
                assert_eq!(
                    parsed.class_name.as_deref(),
                    Some("Main"),
                    "{flag} ({spelling}) swallowed the main class"
                );
            }
        }
    }

    /// clap must reject the two mode flags together rather than letting one
    /// silently win.
    #[test]
    fn clap_rejects_both_mode_flags() {
        assert!(
            Args::try_parse_from(tokens(&[
                "cratonvm",
                "--real-jdk",
                "--synthetic-jdk",
                "Main"
            ]))
            .is_err(),
            "--real-jdk and --synthetic-jdk must conflict"
        );
        let parsed = Args::try_parse_from(tokens(&["cratonvm", "--real-jdk", "Main"]))
            .expect("clap must accept --real-jdk");
        assert!(parsed.real_jdk);
        assert!(!parsed.synthetic_jdk);
    }

    // -----------------------------------------------------------------------
    // Ergonomic default-heap clamp — pure math, exercised directly so the
    // container-vs-host basis logic is covered without a real cgroupfs.
    // -----------------------------------------------------------------------

    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * MIB;

    #[test]
    fn ergo_clamp_quarter_of_large_host() {
        // 16 GiB basis → 1/4 = 4 GiB, exactly the default cap.
        assert_eq!(
            clamp_ergonomic_heap(16 * GIB, MAX_ERGONOMIC_HEAP),
            4 * GIB as usize
        );
    }

    #[test]
    fn ergo_clamp_capped_for_huge_host() {
        // 64 GiB basis → 1/4 = 16 GiB, capped to the 4 GiB default.
        assert_eq!(
            clamp_ergonomic_heap(64 * GIB, MAX_ERGONOMIC_HEAP),
            4 * GIB as usize
        );
    }

    #[test]
    fn ergo_clamp_floored_small_basis() {
        // 4 GiB basis → 1/4 = 1 GiB (above the 256 MiB floor).
        assert_eq!(
            clamp_ergonomic_heap(4 * GIB, MAX_ERGONOMIC_HEAP),
            GIB as usize
        );
        // 512 MiB basis → 1/4 = 128 MiB, raised to the 256 MiB floor.
        assert_eq!(
            clamp_ergonomic_heap(512 * MIB, MAX_ERGONOMIC_HEAP),
            (256 * MIB) as usize
        );
    }

    #[test]
    fn ergo_clamp_never_exceeds_basis() {
        // A 256 MiB container: 1/4 = 64 MiB, the floor would push it to
        // 256 MiB — but it must never exceed the basis itself, so it stays
        // at exactly 256 MiB (not above), and a 200 MiB basis stays at 200.
        assert_eq!(
            clamp_ergonomic_heap(256 * MIB, MAX_ERGONOMIC_HEAP),
            (256 * MIB) as usize
        );
        assert_eq!(
            clamp_ergonomic_heap(200 * MIB, MAX_ERGONOMIC_HEAP),
            (200 * MIB) as usize
        );
    }

    #[test]
    fn ergo_clamp_custom_cap_floored() {
        // A tiny cap override can't drop the result below the 256 MiB floor.
        assert_eq!(
            clamp_ergonomic_heap(16 * GIB, 64 * MIB),
            (256 * MIB) as usize
        );
        // A 2 GiB cap bites on a big host (8 GiB → 1/4 = 2 GiB).
        assert_eq!(clamp_ergonomic_heap(8 * GIB, 2 * GIB), (2 * GIB) as usize);
    }

    #[test]
    fn watchdog_is_not_armed_by_default() {
        assert_eq!(resolve_watchdog_timeout(None, None), None);
    }

    #[test]
    fn watchdog_env_default_is_opt_in() {
        assert_eq!(resolve_watchdog_timeout(None, Some("300")), Some(300));
        assert_eq!(resolve_watchdog_timeout(None, Some("0")), None);
        assert_eq!(resolve_watchdog_timeout(None, Some("not-a-number")), None);
    }

    #[test]
    fn watchdog_explicit_timeout_wins_and_zero_disables() {
        assert_eq!(resolve_watchdog_timeout(Some(7), Some("300")), Some(7));
        assert_eq!(resolve_watchdog_timeout(Some(0), Some("300")), None);
    }

    // -----------------------------------------------------------------------
    // insert_program_args_separator tests — `java`-launcher positional
    // semantics: tokens after the program selector are program args.
    // -----------------------------------------------------------------------

    fn argv(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn sep_jar_then_program_help_is_program_arg() {
        // `-jar foo.jar --help`: `--help` must become a program arg.
        let out = insert_program_args_separator(argv(&["java", "-jar", "foo.jar", "--help"]));
        assert_eq!(out, argv(&["java", "-jar", "foo.jar", "--", "--help"]));
    }

    #[test]
    fn sep_jar_with_leading_opts() {
        // Launcher options before `-jar` stay in the leading section.
        let out = insert_program_args_separator(argv(&[
            "java",
            "--java-home",
            "C:/jdk",
            "-jar",
            "app.jar",
            "--list-modules",
            "--version",
        ]));
        assert_eq!(
            out,
            argv(&[
                "java",
                "--java-home",
                "C:/jdk",
                "-jar",
                "app.jar",
                "--",
                "--list-modules",
                "--version",
            ])
        );
    }

    #[test]
    fn sep_bare_main_class_then_program_args() {
        // `-cp bench Main --help 0`: `Main` selects the program; everything
        // after it (including `--help`) is a program arg.
        let out =
            insert_program_args_separator(argv(&["java", "-cp", "bench", "Main", "--help", "0"]));
        assert_eq!(
            out,
            argv(&["java", "-cp", "bench", "Main", "--", "--help", "0"])
        );
    }

    // -----------------------------------------------------------------------
    // misplaced_launcher_flags — the silent-ignore family (G33-1 defect 2).
    // The detector is pure and reads the argv `insert_program_args_separator`
    // produced, so these compose the two functions exactly as `main()` does.
    // -----------------------------------------------------------------------

    #[test]
    fn misplaced_dump_flag_after_main_class_is_detected() {
        // The exact shape that cost this campaign time twice: the census flag
        // parked behind the main class. The launcher discards it, the VM exits
        // 0 and writes nothing, and before this warning nothing said so.
        let out = insert_program_args_separator(argv(&[
            "cratonvm",
            "-cp",
            "build",
            "RJdkHello",
            "--dump-native-registry",
            "reg.json",
        ]));
        assert_eq!(
            misplaced_launcher_flags(&out),
            vec!["--dump-native-registry"]
        );
    }

    #[test]
    fn correctly_placed_flags_are_not_reported() {
        // The whole family, all ahead of the selector. A detector that fires
        // here would train every reader to ignore it, which is worse than not
        // having one.
        let out = insert_program_args_separator(argv(&[
            "cratonvm",
            "--jdk-only",
            "--nojit",
            "--dump-native-registry",
            "reg.json",
            "--jdk-only-report",
            "r.json",
            "-cp",
            "build",
            "Main",
            "5",
        ]));
        assert!(misplaced_launcher_flags(&out).is_empty());
    }

    #[test]
    fn misplaced_flags_are_reported_once_each_in_argv_order() {
        let out = insert_program_args_separator(argv(&[
            "cratonvm",
            "-cp",
            "build",
            "Main",
            "--nojit",
            "--jdk-only",
            "--nojit",
            "--dump-native-registry=reg.json",
        ]));
        assert_eq!(
            misplaced_launcher_flags(&out),
            // Order is argv order, not list order, so the message reads in the
            // order the user typed. Deduplicated, because a repeated flag is
            // one mistake.
            vec!["--nojit", "--jdk-only", "--dump-native-registry"],
            "the inline `--flag=value` form must report as the bare flag"
        );
    }

    #[test]
    fn misplaced_detection_covers_the_jar_form_too() {
        // `-jar app.jar` is the other program selector, and it is the form a
        // build tool is most likely to append flags to.
        let out = insert_program_args_separator(argv(&[
            "cratonvm",
            "-jar",
            "app.jar",
            "--jdk-only-report",
            "r.json",
        ]));
        assert_eq!(misplaced_launcher_flags(&out), vec!["--jdk-only-report"]);
    }

    #[test]
    fn a_program_argument_that_merely_resembles_a_flag_is_not_reported() {
        // Only exact spellings from the list. A program arg that shares a
        // prefix, or a value that happens to look like one, must not fire —
        // the warning has to survive contact with real command lines.
        let out = insert_program_args_separator(argv(&[
            "cratonvm",
            "-cp",
            "build",
            "Main",
            "--dump-native-registry-v2",
            "--jdk-only-reporter",
            "--dump",
            "-nojit",
        ]));
        assert!(misplaced_launcher_flags(&out).is_empty());
    }

    #[test]
    fn no_program_selector_means_nothing_is_misplaced() {
        // `cratonvm --jdk-only --version`: no separator is inserted at all, so
        // every token is still a launcher option and none is discarded.
        let out = insert_program_args_separator(argv(&["cratonvm", "--jdk-only", "--version"]));
        assert!(misplaced_launcher_flags(&out).is_empty());
    }

    #[test]
    fn an_explicit_separator_still_delimits_the_tail() {
        // A caller who writes `--` themselves gets the same treatment: the
        // tail is the program's, and a launcher flag in it is discarded.
        let out = insert_program_args_separator(argv(&[
            "cratonvm",
            "-cp",
            "build",
            "--",
            "Main",
            "--trace-jdk-only",
        ]));
        assert_eq!(misplaced_launcher_flags(&out), vec!["--trace-jdk-only"]);
    }

    // -----------------------------------------------------------------------
    // Dump-path diagnostics — the other half of G33-1 defect 2: a path the
    // caller cannot find is as bad as no file at all.
    // -----------------------------------------------------------------------

    #[test]
    fn absolute_dump_path_resolves_a_relative_operand() {
        let resolved = absolute_dump_path("reg.json");
        let path = std::path::Path::new(&resolved);
        assert!(
            path.is_absolute(),
            "a relative operand must be reported as the absolute path actually written: {resolved}"
        );
        assert!(resolved.ends_with("reg.json"));
    }

    #[test]
    fn absolute_dump_path_never_fails_on_a_degenerate_operand() {
        // A diagnostic that can itself fail is not a diagnostic. The empty
        // operand is the one input `std::path::absolute` rejects.
        assert_eq!(absolute_dump_path(""), "");
    }

    #[test]
    fn dump_failure_names_the_path_and_the_missing_directory() {
        let err = std::io::Error::from(std::io::ErrorKind::NotFound);
        let message = describe_dump_failure(
            "no-such-dir-4b7f1e/deeper/reg.json",
            &format!("{err} (os error 3)"),
        );
        assert!(
            message.contains("no-such-dir-4b7f1e"),
            "the attempted path must appear: {message}"
        );
        assert!(
            message.contains("does not exist"),
            "a missing parent directory is the common cause and must be named: {message}"
        );
        assert!(
            message.contains("os error 3"),
            "the underlying OS error must survive: {message}"
        );
    }

    #[test]
    fn dump_failure_says_so_when_the_directory_does_exist() {
        // Distinguishing "no such directory" from "the directory is there and
        // the write still failed" is the whole point — the second is a
        // permissions or locking problem and sends the reader somewhere else.
        let dir = std::env::temp_dir();
        let path = dir.join("g33-1-existing-dir-probe.json");
        let err = std::io::Error::from(std::io::ErrorKind::PermissionDenied);
        let message = describe_dump_failure(&path.display().to_string(), &err);
        assert!(
            message.contains("exists"),
            "an existing parent must be reported as existing: {message}"
        );
        assert!(!message.contains("does not exist"), "{message}");
    }

    #[test]
    fn the_census_caveat_is_only_dropped_when_both_bypasses_are_off() {
        // Three of the four configurations leave at least one bypass family
        // live, and in all three the column is a floor. Only the fourth may
        // claim an exact count.
        for (nojit, no_intrinsics) in [(false, false), (true, false), (false, true)] {
            let text = census_invocations_caveat(nojit, no_intrinsics);
            assert!(
                text.contains("LOWER BOUND"),
                "nojit={nojit} intrinsics-off={no_intrinsics} must warn: {text}"
            );
        }
        let exact = census_invocations_caveat(true, true);
        assert!(exact.contains("exact count"), "{exact}");
        assert!(!exact.contains("LOWER BOUND"), "{exact}");
    }

    #[test]
    fn sep_no_program_left_unchanged() {
        // `java --version` with no program: nothing to delimit; the
        // launcher must still handle `--version` itself.
        let inp = argv(&["java", "--version"]);
        assert_eq!(insert_program_args_separator(inp.clone()), inp);
        let inp = argv(&["java", "--help"]);
        assert_eq!(insert_program_args_separator(inp.clone()), inp);
    }

    #[test]
    fn normalize_version_short_forms() {
        assert_eq!(
            normalize_java_launcher_argv(argv(&["java", "-version"])),
            argv(&["java", "--version"])
        );
        assert_eq!(
            normalize_java_launcher_argv(argv(&["java", "-v"])),
            argv(&["java", "--version"])
        );
    }

    #[test]
    fn sep_explicit_separator_respected() {
        // An explicit `--` already delimits program args; copy verbatim.
        let inp = argv(&["java", "-jar", "a.jar", "--", "--help"]);
        assert_eq!(insert_program_args_separator(inp.clone()), inp);
    }

    #[test]
    fn sep_value_token_not_mistaken_for_main_class() {
        // The classpath string after `-cp` is a value, not the main class.
        let out = insert_program_args_separator(argv(&["java", "-cp", "lib.jar", "Main", "arg1"]));
        assert_eq!(out, argv(&["java", "-cp", "lib.jar", "Main", "--", "arg1"]));
    }

    #[test]
    fn sep_inline_jar_value() {
        // `--jar=foo.jar` inline form: `--version` after it is a program arg.
        let out = insert_program_args_separator(argv(&["java", "--jar=foo.jar", "--version"]));
        assert_eq!(out, argv(&["java", "--jar=foo.jar", "--", "--version"]));
    }

    // -----------------------------------------------------------------------
    // parse_size tests
    // -----------------------------------------------------------------------

    #[test]
    fn parse_memory_sizes() {
        assert_eq!(parse_size("256m"), Some(256 * 1024 * 1024));
        assert_eq!(parse_size("1g"), Some(1024 * 1024 * 1024));
        assert_eq!(parse_size("1024k"), Some(1024 * 1024));
        assert_eq!(parse_size("1024"), Some(1024));
        assert_eq!(parse_size(""), None);
    }

    #[test]
    fn parse_size_whitespace() {
        assert_eq!(parse_size("  256m  "), Some(256 * 1024 * 1024));
        assert_eq!(parse_size("   "), None);
    }

    #[test]
    fn parse_size_invalid() {
        assert_eq!(parse_size("abc"), None);
        assert_eq!(parse_size("m"), None);
        assert_eq!(parse_size("-1m"), None);
    }

    #[test]
    fn parse_size_overflow() {
        // Multiplying by the suffix factor must not overflow `usize`:
        // an oversized input returns None instead of panicking/wrapping.
        assert_eq!(parse_size("999999999999g"), None);
        assert_eq!(parse_size(&format!("{}g", usize::MAX)), None);
    }

    // -----------------------------------------------------------------------
    // validate_class_name tests
    // -----------------------------------------------------------------------

    #[test]
    fn valid_simple_class_name() {
        assert!(validate_class_name("Main").is_ok());
    }

    #[test]
    fn valid_fully_qualified_class_name() {
        assert!(validate_class_name("com/example/Main").is_ok());
    }

    #[test]
    fn valid_class_name_with_underscore_and_dollar() {
        assert!(validate_class_name("com/_internal/$Helper").is_ok());
        assert!(validate_class_name("$Proxy0").is_ok());
        assert!(validate_class_name("_Private").is_ok());
    }

    #[test]
    fn valid_class_name_with_digits() {
        assert!(validate_class_name("com/example/Test123").is_ok());
    }

    #[test]
    fn invalid_empty_class_name() {
        let err = validate_class_name("").unwrap_err();
        assert!(err.to_string().contains("must not be empty"));
    }

    #[test]
    fn invalid_class_name_starts_with_digit() {
        let err = validate_class_name("com/123bad/Main").unwrap_err();
        assert!(err.to_string().contains("must start with"));
    }

    #[test]
    fn invalid_class_name_double_slash() {
        let err = validate_class_name("com//Main").unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn invalid_class_name_leading_slash() {
        let err = validate_class_name("/com/Main").unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn invalid_class_name_trailing_slash() {
        let err = validate_class_name("com/Main/").unwrap_err();
        assert!(err.to_string().contains("empty"));
    }

    #[test]
    fn invalid_class_name_special_chars() {
        let err = validate_class_name("com/ex@mple/Main").unwrap_err();
        assert!(err.to_string().contains("invalid character"));
    }

    #[test]
    fn invalid_class_name_spaces() {
        let err = validate_class_name("com/my class/Main").unwrap_err();
        assert!(err.to_string().contains("invalid character"));
    }

    #[test]
    fn invalid_class_name_hyphen() {
        let err = validate_class_name("com/my-pkg/Main").unwrap_err();
        assert!(err.to_string().contains("invalid character"));
    }

    // -----------------------------------------------------------------------
    // extract_system_properties tests
    // -----------------------------------------------------------------------

    #[test]
    fn extract_d_properties() {
        let raw = vec![
            "cratonvm".to_string(),
            "-Djboss.home.dir=C:/craton/kc16".to_string(),
            "-Dmy.flag".to_string(),
            "com.example.Main".to_string(),
        ];
        let (filtered, props) = extract_system_properties(raw);
        assert_eq!(filtered, vec!["cratonvm", "com.example.Main"]);
        assert_eq!(
            props,
            vec![
                ("jboss.home.dir".to_string(), "C:/craton/kc16".to_string()),
                ("my.flag".to_string(), String::new()),
            ]
        );
    }

    #[test]
    fn extract_d_no_properties() {
        let raw = vec!["cratonvm".to_string(), "Main".to_string()];
        let (filtered, props) = extract_system_properties(raw);
        assert_eq!(filtered, vec!["cratonvm", "Main"]);
        assert!(props.is_empty());
    }

    #[test]
    fn extract_d_value_with_equals() {
        // -Dkey=val=ue  →  key = "val=ue"
        let raw = vec!["cratonvm".to_string(), "-Dpath=a=b".to_string()];
        let (_, props) = extract_system_properties(raw);
        assert_eq!(props, vec![("path".to_string(), "a=b".to_string())]);
    }

    #[test]
    fn normalize_classpath_and_jar_for_clap() {
        let raw = vec![
            "cratonvm".to_string(),
            "-classpath".to_string(),
            "a;b".to_string(),
            "-cp=c:d".to_string(),
            "-jar".to_string(),
            "app.jar".to_string(),
            "arg1".to_string(),
        ];
        assert_eq!(
            normalize_java_launcher_argv(raw),
            vec![
                "cratonvm",
                "--classpath",
                "a;b",
                "--classpath",
                "c:d",
                "--jar",
                "app.jar",
                "arg1",
            ]
            .into_iter()
            .map(String::from)
            .collect::<Vec<_>>()
        );
    }

    // -----------------------------------------------------------------------
    // T6 extract_hotspot_flags tests
    // -----------------------------------------------------------------------

    #[test]
    fn hotspot_flag_enables_heap_dump_on_oom() {
        let raw = vec![
            "cratonvm".to_string(),
            "-XX:+HeapDumpOnOutOfMemoryError".to_string(),
            "Main".to_string(),
        ];
        let (filtered, flags) = extract_hotspot_flags(raw);
        assert_eq!(filtered, vec!["cratonvm", "Main"]);
        assert_eq!(flags.heap_dump_on_oom, Some(true));
    }

    #[test]
    fn hotspot_flag_disables_heap_dump_on_oom() {
        let raw = vec![
            "cratonvm".to_string(),
            "-XX:-HeapDumpOnOutOfMemoryError".to_string(),
            "Main".to_string(),
        ];
        let (_, flags) = extract_hotspot_flags(raw);
        assert_eq!(flags.heap_dump_on_oom, Some(false));
    }

    #[test]
    fn hotspot_flag_extracts_heap_dump_path() {
        let raw = vec![
            "cratonvm".to_string(),
            "-XX:HeapDumpPath=/tmp/heap.hprof".to_string(),
            "Main".to_string(),
        ];
        let (filtered, flags) = extract_hotspot_flags(raw);
        assert_eq!(filtered, vec!["cratonvm", "Main"]);
        assert_eq!(flags.heap_dump_path.as_deref(), Some("/tmp/heap.hprof"));
    }

    // obsaudit D12 — -XX:StartFlightRecording extraction + option parsing.

    #[test]
    fn hotspot_flag_extracts_bare_start_flight_recording() {
        let raw = vec![
            "cratonvm".to_string(),
            "-XX:StartFlightRecording".to_string(),
            "Main".to_string(),
        ];
        let (filtered, flags) = extract_hotspot_flags(raw);
        assert_eq!(filtered, vec!["cratonvm", "Main"]);
        assert_eq!(flags.jfr_start_recording.as_deref(), Some(""));
    }

    #[test]
    fn hotspot_flag_extracts_start_flight_recording_with_opts() {
        let raw = vec![
            "cratonvm".to_string(),
            "-XX:StartFlightRecording:filename=rec.jfr,duration=30s".to_string(),
            "Main".to_string(),
        ];
        let (filtered, flags) = extract_hotspot_flags(raw);
        assert_eq!(filtered, vec!["cratonvm", "Main"]);
        assert_eq!(
            flags.jfr_start_recording.as_deref(),
            Some("filename=rec.jfr,duration=30s")
        );
    }

    #[test]
    fn jfr_opts_bare_flag_uses_defaults() {
        let cfg = parse_jfr_start_recording_opts("").unwrap();
        assert_eq!(cfg.filename, None);
        assert_eq!(cfg.duration, None);
        assert_eq!(cfg.max_age, None);
        assert_eq!(cfg.max_events, None);
        assert!(cfg.dump_on_exit, "HotSpot defaults dumponexit to true");
    }

    #[test]
    fn jfr_opts_parses_all_recognized_keys() {
        let cfg = parse_jfr_start_recording_opts(
            "filename=out.jfr,duration=30s,maxage=5m,maxevents=500,dumponexit=false",
        )
        .unwrap();
        assert_eq!(cfg.filename.as_deref(), Some("out.jfr"));
        assert_eq!(cfg.duration, Some(std::time::Duration::from_secs(30)));
        assert_eq!(cfg.max_age, Some(std::time::Duration::from_secs(5 * 60)));
        assert_eq!(cfg.max_events, Some(500));
        assert!(!cfg.dump_on_exit);
    }

    #[test]
    fn jfr_opts_bare_duration_digits_mean_seconds() {
        let cfg = parse_jfr_start_recording_opts("duration=45").unwrap();
        assert_eq!(cfg.duration, Some(std::time::Duration::from_secs(45)));
    }

    #[test]
    fn jfr_opts_hour_and_day_suffixes() {
        let cfg = parse_jfr_start_recording_opts("maxage=2h").unwrap();
        assert_eq!(cfg.max_age, Some(std::time::Duration::from_secs(2 * 3600)));
        let cfg = parse_jfr_start_recording_opts("maxage=1d").unwrap();
        assert_eq!(cfg.max_age, Some(std::time::Duration::from_secs(86400)));
    }

    #[test]
    fn jfr_opts_rejects_unrecognized_key() {
        let err = parse_jfr_start_recording_opts("disk=true").unwrap_err();
        assert!(err.contains("disk"), "error should name the bad key: {err}");
    }

    #[test]
    fn jfr_opts_rejects_missing_equals() {
        let err = parse_jfr_start_recording_opts("filename").unwrap_err();
        assert!(err.contains("filename"));
    }

    #[test]
    fn jfr_opts_rejects_bad_duration() {
        assert!(parse_jfr_start_recording_opts("duration=notanumber").is_err());
    }

    #[test]
    fn jfr_opts_rejects_bad_dumponexit_value() {
        assert!(parse_jfr_start_recording_opts("dumponexit=maybe").is_err());
    }

    #[test]
    fn hotspot_flag_preserves_all_agent_tokens() {
        let raw = vec![
            "cratonvm".to_string(),
            "-agentlib:jdwp=transport=dt_socket,server=y,address=5005".to_string(),
            "-agentpath:/opt/myagent.so=trace".to_string(),
            "-javaagent:/opt/bytebuddy.jar".to_string(),
            "Main".to_string(),
        ];
        let (filtered, flags) = extract_hotspot_flags(raw);
        assert_eq!(filtered, vec!["cratonvm", "Main"]);
        assert_eq!(flags.agent_options.len(), 3);
        assert!(flags.agent_options[0].starts_with("-agentlib:jdwp="));
        assert!(flags.agent_options[1].starts_with("-agentpath:/opt/myagent.so"));
        assert!(flags.agent_options[2].starts_with("-javaagent:/opt/bytebuddy.jar"));
    }

    #[test]
    fn hotspot_flag_passes_unknown_through() {
        let raw = vec![
            "cratonvm".to_string(),
            "--foo".to_string(),
            "Main".to_string(),
        ];
        let (filtered, flags) = extract_hotspot_flags(raw);
        assert_eq!(filtered, vec!["cratonvm", "--foo", "Main"]);
        assert!(flags.agent_options.is_empty());
        assert!(flags.heap_dump_on_oom.is_none());
        assert!(flags.heap_dump_path.is_none());
    }

    #[test]
    fn heap_dump_flags_survive_full_launcher_pipeline() {
        let argv0 = argv(&[
            "java",
            "-XX:+HeapDumpOnOutOfMemoryError",
            "-XX:HeapDumpPath=/tmp/heap.hprof",
            "Main",
        ]);
        let stage1 = insert_program_args_separator(argv0);
        let stage2 = normalize_java_launcher_argv(stage1);
        let (stage3, _props) = extract_system_properties(stage2);
        let (stage4, flags) = extract_hotspot_flags(stage3);
        let parsed = Args::try_parse_from(stage4).expect("clap must accept heap dump flags");

        assert_eq!(parsed.class_name.as_deref(), Some("Main"));
        assert_eq!(flags.heap_dump_on_oom, Some(true));
        assert_eq!(flags.heap_dump_path.as_deref(), Some("/tmp/heap.hprof"));
    }

    // -----------------------------------------------------------------------
    // expand_aggregate_jars tests
    // -----------------------------------------------------------------------

    fn unique_temp_dir(label: &str) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id();
        let dir = std::env::temp_dir().join(format!("cratonvm-cli-{label}-{pid}-{id}"));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    #[test]
    fn expand_missing_netty_all_substitutes_split_jars() {
        let dir = unique_temp_dir("netty-all");
        // Create split jars next to where netty-all.jar would live.
        for name in ["netty-common.jar", "netty-transport.jar", "other.jar"] {
            std::fs::write(dir.join(name), b"pk").unwrap();
        }
        let missing = dir.join("netty-all.jar").to_string_lossy().into_owned();
        let expanded = expand_aggregate_jars(vec![missing.clone()]);
        // netty-all.jar should be gone; the two netty split jars should be
        // present; "other.jar" should NOT have been pulled in.
        assert!(
            !expanded.iter().any(|e| e == &missing),
            "missing aggregate jar must be stripped, got {expanded:?}"
        );
        let has = |n: &str| expanded.iter().any(|e| e.to_ascii_lowercase().ends_with(n));
        assert!(
            has("netty-common.jar"),
            "missing netty-common: {expanded:?}"
        );
        assert!(
            has("netty-transport.jar"),
            "missing netty-transport: {expanded:?}"
        );
        assert!(
            !has("other.jar"),
            "unexpected 'other.jar' in result: {expanded:?}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn expand_preserves_existing_aggregate() {
        let dir = unique_temp_dir("netty-all-real");
        let aggregate = dir.join("netty-all.jar");
        std::fs::write(&aggregate, b"pk").unwrap();
        // A sibling split jar also exists; we must NOT add it because the
        // aggregate is present and authoritative.
        std::fs::write(dir.join("netty-common.jar"), b"pk").unwrap();
        let entry = aggregate.to_string_lossy().into_owned();
        let expanded = expand_aggregate_jars(vec![entry.clone()]);
        assert_eq!(expanded, vec![entry]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn non_jar_archive_staging_cleanup_removes_temp_copy() {
        let dir = unique_temp_dir("stage-war");
        let archive = dir.join("app.war");
        std::fs::write(&archive, b"fake archive").unwrap();

        let (entry, cleanup) = classpath_entry_for_archive(&archive).unwrap();
        let cleanup = cleanup.expect("non-.jar archive should be staged");
        assert_ne!(entry, archive);
        assert_eq!(entry.extension().and_then(|e| e.to_str()), Some("jar"));
        assert!(
            entry.exists(),
            "staged copy should exist while guard is live"
        );
        assert_eq!(std::fs::read(&entry).unwrap(), b"fake archive");

        drop(cleanup);
        assert!(
            !entry.exists(),
            "staged copy should be removed on guard drop"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    // -----------------------------------------------------------------------
    // quarkus_signature_present tests — PERF gate for the multi-dir
    // classpath walk. A plain jar must NOT trip the sniff; the canonical
    // Quarkus signature artifacts (and the Keycloak one-level-deep layout)
    // MUST.
    // -----------------------------------------------------------------------

    #[test]
    fn quarkus_sniff_false_for_plain_jar() {
        let dir = unique_temp_dir("qsniff-plain");
        let jar = dir.join("hello.jar");
        std::fs::write(&jar, b"pk").unwrap();
        assert!(
            !quarkus_signature_present(&jar),
            "a plain jar with no Quarkus artifacts must skip the walk"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn quarkus_sniff_true_for_quarkus_run_jar_sibling() {
        let dir = unique_temp_dir("qsniff-run");
        let jar = dir.join("app.jar");
        std::fs::write(&jar, b"pk").unwrap();
        std::fs::write(dir.join("quarkus-run.jar"), b"pk").unwrap();
        assert!(
            quarkus_signature_present(&jar),
            "quarkus-run.jar next to the jar must enable the walk"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn quarkus_sniff_true_for_quarkus_subdir() {
        let dir = unique_temp_dir("qsniff-subdir");
        let jar = dir.join("app.jar");
        std::fs::write(&jar, b"pk").unwrap();
        std::fs::create_dir_all(dir.join("quarkus")).unwrap();
        assert!(
            quarkus_signature_present(&jar),
            "a quarkus/ subdir must enable the walk"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn quarkus_sniff_true_for_parent_dir_signature() {
        // Keycloak packaging puts quarkus-run.jar one level up from the
        // runner jar (which lives in lib/). The sniff probes the parent.
        let dir = unique_temp_dir("qsniff-parent");
        let lib = dir.join("lib");
        std::fs::create_dir_all(&lib).unwrap();
        let jar = lib.join("quarkus-run.jar");
        std::fs::write(&jar, b"pk").unwrap();
        // Signature artifact lives in the PARENT (dir), not lib/.
        std::fs::write(dir.join("quarkus-application.dat"), b"x").unwrap();
        assert!(
            quarkus_signature_present(&jar),
            "a signature in the jar's parent dir must enable the walk"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn expand_keeps_missing_entry_when_no_siblings() {
        let dir = unique_temp_dir("netty-lonely");
        let missing = dir.join("netty-all.jar").to_string_lossy().into_owned();
        let expanded = expand_aggregate_jars(vec![missing.clone()]);
        // No split jars exist, so the entry is kept as-is (so downstream
        // logging still surfaces the missing-jar message).
        assert_eq!(expanded, vec![missing]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn expand_ignores_non_aggregate_names() {
        let dir = unique_temp_dir("no-match");
        let missing = dir
            .join("does-not-exist.jar")
            .to_string_lossy()
            .into_owned();
        let expanded = expand_aggregate_jars(vec![missing.clone()]);
        assert_eq!(expanded, vec![missing]);
        let _ = std::fs::remove_dir_all(dir);
    }

    // -----------------------------------------------------------------------
    // `--sun-misc-unsafe-memory-access=<mode>` (JEP 498).
    //
    // The flag's whole effect is the system property it sets, and the property
    // is the single input netty 4.2 keys its Unsafe-vs-FFM decision on. A
    // default run must leave it UNSET, exactly as HotSpot does — CratonVM used
    // to pin it to `allow` in `vm_init`, which silently put netty and every
    // other Unsafe-aware library on a different code path than a stock JDK 25.
    // -----------------------------------------------------------------------

    #[test]
    fn unsafe_memory_access_flag_becomes_the_system_property() {
        let out = normalize_java_launcher_argv(argv(&[
            "java",
            "--sun-misc-unsafe-memory-access=allow",
            "Main",
        ]));
        assert_eq!(
            out,
            argv(&["java", "-Dsun.misc.unsafe.memory.access=allow", "Main"])
        );
    }

    #[test]
    fn every_unsafe_memory_access_mode_round_trips() {
        for mode in ["allow", "warn", "debug", "deny"] {
            let out = normalize_java_launcher_argv(argv(&[
                "java",
                &format!("--sun-misc-unsafe-memory-access={mode}"),
                "Main",
            ]));
            assert_eq!(
                out,
                argv(&[
                    "java",
                    &format!("-Dsun.misc.unsafe.memory.access={mode}"),
                    "Main"
                ]),
                "mode {mode}"
            );
        }
    }

    #[test]
    fn no_unsafe_memory_access_flag_leaves_the_property_unset() {
        // The load-bearing half: a plain command line must not introduce the
        // property, because `System.getProperty(...) == null` is what makes
        // netty take the same path it takes on a stock JDK 25.
        let out = normalize_java_launcher_argv(argv(&["java", "-Xmx1g", "Main"]));
        assert!(
            !out.iter()
                .any(|a| a.contains("sun.misc.unsafe.memory.access")),
            "a default command line must not mention the property: {out:?}"
        );
    }

    #[test]
    fn an_unsafe_memory_access_flag_after_the_separator_is_the_programs_own() {
        let out = normalize_java_launcher_argv(argv(&[
            "java",
            "Main",
            "--",
            "--sun-misc-unsafe-memory-access=allow",
        ]));
        assert_eq!(
            out,
            argv(&[
                "java",
                "Main",
                "--",
                "--sun-misc-unsafe-memory-access=allow"
            ])
        );
    }

    // -----------------------------------------------------------------------
    // Assertion flags: `-ea` and friends.
    //
    // Two halves that must both hold. `normalize_java_launcher_argv` still
    // *strips* every spelling (clap cannot parse them), and
    // `launcher_assertions_requested` reads the same argv for the switch. A
    // test that only checked the strip would pass with the switch deleted,
    // which is exactly the state this fixed.
    // -----------------------------------------------------------------------

    #[test]
    fn assertion_flags_are_stripped_before_clap() {
        for flag in [
            "-ea",
            "-da",
            "-esa",
            "-dsa",
            "-enableassertions",
            "-disableassertions",
            "-enablesystemassertions",
            "-disablesystemassertions",
            "-ea:io.netty...",
            "-da:some.Class",
        ] {
            let out = normalize_java_launcher_argv(argv(&["java", flag, "Main"]));
            assert_eq!(
                out,
                argv(&["java", "Main"]),
                "flag {flag} survived the strip"
            );
        }
    }

    #[test]
    fn unscoped_ea_enables_and_da_disables() {
        assert_eq!(
            launcher_assertions_requested(&argv(&["java", "-ea", "Main"])),
            Some(true)
        );
        assert_eq!(
            launcher_assertions_requested(&argv(&["java", "-enableassertions", "Main"])),
            Some(true)
        );
        assert_eq!(
            launcher_assertions_requested(&argv(&["java", "-da", "Main"])),
            Some(false)
        );
        assert_eq!(
            launcher_assertions_requested(&argv(&["java", "-disableassertions", "Main"])),
            Some(false)
        );
    }

    #[test]
    fn no_assertion_flag_leaves_the_env_var_in_charge() {
        // `None`, not `Some(false)`: a plain command line must not clear an
        // inherited CRATONVM_ENABLE_ASSERTIONS.
        assert_eq!(
            launcher_assertions_requested(&argv(&["java", "-Xmx1g", "Main"])),
            None
        );
    }

    #[test]
    fn scoped_assertion_flags_are_not_honoured() {
        // One global switch has no per-package granularity, so `-ea:io.netty`
        // must NOT read as global-enable — that would turn assertions on for
        // every class the caller deliberately left out.
        for flag in ["-ea:io.netty...", "-da:io.netty.Foo", "-ea:com.example"] {
            assert_eq!(
                launcher_assertions_requested(&argv(&["java", flag, "Main"])),
                None,
                "scoped flag {flag} was honoured"
            );
        }
    }

    #[test]
    fn last_wins_within_a_scope() {
        assert_eq!(
            launcher_assertions_requested(&argv(&["java", "-ea", "-da", "Main"])),
            Some(false)
        );
        assert_eq!(
            launcher_assertions_requested(&argv(&["java", "-da", "-ea", "Main"])),
            Some(true)
        );
    }

    #[test]
    fn a_system_scope_flag_never_cancels_a_user_scope_enable() {
        // HotSpot's `-ea -dsa` is "user assertions on, system assertions off".
        // Collapsed onto one global switch that has to stay ON, or the `-dsa`
        // silently undoes the `-ea` Surefire put there.
        assert_eq!(
            launcher_assertions_requested(&argv(&["java", "-ea", "-dsa", "Main"])),
            Some(true)
        );
        assert_eq!(
            launcher_assertions_requested(&argv(&["java", "-esa", "-da", "Main"])),
            Some(true)
        );
        assert_eq!(
            launcher_assertions_requested(&argv(&["java", "-dsa", "Main"])),
            Some(false)
        );
        assert_eq!(
            launcher_assertions_requested(&argv(&["java", "-esa", "Main"])),
            Some(true)
        );
    }

    #[test]
    fn an_ea_after_the_program_args_separator_is_the_programs_own() {
        // `java -jar app.jar -- -ea` passes `-ea` to the application; scanning
        // past `--` would let a program argument reconfigure the VM.
        assert_eq!(
            launcher_assertions_requested(&argv(&["java", "Main", "--", "-ea"])),
            None
        );
        assert_eq!(
            launcher_assertions_requested(&argv(&["java", "-da", "Main", "--", "-ea"])),
            Some(false)
        );
    }

    // -----------------------------------------------------------------------
    // HotSpot single-dash compatibility rewrites — C36 acceptance.
    //
    // Stock `java -Xmx256m -classpath x Main` must parse, otherwise the
    // `[[bin]] name = "java"` alias is useless for Maven Surefire / Gradle.
    // -----------------------------------------------------------------------

    #[test]
    fn hotspot_xmx_inline_rewrites_to_clap_long() {
        let raw = argv(&["java", "-Xmx256m", "-classpath", "x", "Main"]);
        let out = normalize_java_launcher_argv(raw);
        assert_eq!(
            out,
            argv(&["java", "--Xmx", "256m", "--classpath", "x", "Main"])
        );
    }

    #[test]
    fn hotspot_xmx_separate_token_rewrites_to_clap_long() {
        let raw = argv(&["java", "-Xmx", "1g", "Main"]);
        let out = normalize_java_launcher_argv(raw);
        assert_eq!(out, argv(&["java", "--Xmx", "1g", "Main"]));
    }

    #[test]
    fn hotspot_xms_inline_rewrites_to_clap_long() {
        // F-16: `-Xms512m` used to be dropped, because the heap was allocated
        // at `-Xmx` in the collector's constructor and an initial size named
        // nothing. G1 now reserves `-Xmx` and commits a prefix, so the value is
        // carried through to clap like `-Xmx`'s.
        let raw = argv(&["java", "-Xms512m", "Main"]);
        let out = normalize_java_launcher_argv(raw);
        assert_eq!(out, argv(&["java", "--Xms", "512m", "Main"]));
    }

    #[test]
    fn a_bare_trailing_xms_with_no_value_is_still_dropped() {
        // Nothing to carry, and emitting `--Xms` with no value would make clap
        // reject a command line HotSpot accepts.
        let raw = argv(&["java", "Main", "-Xms"]);
        let out = normalize_java_launcher_argv(raw);
        assert_eq!(out, argv(&["java", "Main"]));
    }

    #[test]
    fn hotspot_xms_separate_token_keeps_its_value_and_the_class_name() {
        // B1 regression, still: `-Xms 512m` (separate value token) must not
        // leave a bare `512m` to be mistaken for the main-class positional,
        // shifting `Main` into a program arg. Maven Surefire / Gradle forks
        // emit this form. The value sits adjacent here because `-Xms` is in
        // VALUE_TAKING_OPTS, so this mirrors the full pre-clap pipeline.
        //
        // F-16 changed the remedy, not the requirement: the value is now
        // consumed by being handed to clap as `--Xms 512m` rather than by being
        // thrown away.
        let stage1 = insert_program_args_separator(argv(&["java", "-Xms", "512m", "Main"]));
        let out = normalize_java_launcher_argv(stage1);
        assert_eq!(out, argv(&["java", "--Xms", "512m", "Main", "--"]));
    }

    #[test]
    fn hotspot_xms_separate_token_passes_clap_after_full_pipeline() {
        // B1 acceptance: stock `java -Xms512m -Xmx256m -classpath x Main`
        // with the separate-token `-Xms 512m` form must parse end-to-end with
        // `Main` resolved as the main class (not the heap-size value `512m`).
        let argv0: Vec<String> = argv(&[
            "java",
            "-Xms",
            "512m",
            "-Xmx",
            "256m",
            "-classpath",
            "x",
            "Main",
        ]);
        let stage1 = insert_program_args_separator(argv0);
        let stage2 = normalize_java_launcher_argv(stage1);
        let (stage3, _props) = extract_system_properties(stage2);
        let (stage4, _hot) = extract_hotspot_flags(stage3);
        let parsed =
            Args::try_parse_from(stage4).expect("clap must accept HotSpot separate-token -Xms");
        assert_eq!(parsed.max_heap.as_deref(), Some("256m"));
        // F-16: and the initial size ARRIVES, rather than being discarded on
        // the way. (`-Xms` above `-Xmx` is clamped where the config is built,
        // not here — this stage only has to carry the value.)
        assert_eq!(parsed.initial_heap.as_deref(), Some("512m"));
        assert_eq!(parsed.classpath.as_deref(), Some("x"));
        assert_eq!(parsed.class_name.as_deref(), Some("Main"));
    }

    #[test]
    fn hotspot_xshare_colon_rewrites_to_clap_long() {
        let raw = argv(&["java", "-Xshare:on", "Main"]);
        let out = normalize_java_launcher_argv(raw);
        assert_eq!(out, argv(&["java", "--Xshare", "on", "Main"]));
    }

    #[test]
    fn hotspot_xverify_none_collapses_to_noverify_flag() {
        let raw = argv(&["java", "-Xverify:none", "Main"]);
        let out = normalize_java_launcher_argv(raw);
        assert_eq!(out, argv(&["java", "--noverify", "Main"]));
    }

    #[test]
    fn hotspot_xverify_remote_rewrites_to_clap_long() {
        let raw = argv(&["java", "-Xverify:remote", "Main"]);
        let out = normalize_java_launcher_argv(raw);
        assert_eq!(out, argv(&["java", "--Xverify", "remote", "Main"]));
    }

    #[test]
    fn hotspot_xbootclasspath_inline_rewrites_to_clap_long() {
        let raw = argv(&["java", "-Xbootclasspath:/opt/boot", "Main"]);
        let out = normalize_java_launcher_argv(raw);
        assert_eq!(
            out,
            argv(&["java", "--Xbootclasspath", "/opt/boot", "Main"])
        );
    }

    #[test]
    fn hotspot_xbootclasspath_append_and_prepend_normalise() {
        // /a (append) and /p (prepend) collapse to a plain replace — cratonvm
        // does not model the three boot-CP positions separately.
        let raw = argv(&["java", "-Xbootclasspath/a:/opt/extra", "Main"]);
        let out = normalize_java_launcher_argv(raw);
        assert_eq!(
            out,
            argv(&["java", "--Xbootclasspath", "/opt/extra", "Main"])
        );

        let raw = argv(&["java", "-Xbootclasspath/p:/opt/pre", "Main"]);
        let out = normalize_java_launcher_argv(raw);
        assert_eq!(out, argv(&["java", "--Xbootclasspath", "/opt/pre", "Main"]));
    }

    #[test]
    fn hotspot_xlog_colon_rewrites_to_clap_long() {
        let raw = argv(&["java", "-Xlog:gc*=info", "Main"]);
        let out = normalize_java_launcher_argv(raw);
        assert_eq!(out, argv(&["java", "--Xlog", "gc*=info", "Main"]));
    }

    #[test]
    fn hotspot_xx_shared_archive_file_rewrites_to_clap_long() {
        let raw = argv(&["java", "-XX:SharedArchiveFile=app.jsa", "Main"]);
        let out = normalize_java_launcher_argv(raw);
        assert_eq!(
            out,
            argv(&["java", "--XX:SharedArchiveFile", "app.jsa", "Main"])
        );
    }

    #[test]
    fn hotspot_xx_aot_flags_rewrite_to_clap_long() {
        let raw = argv(&[
            "java",
            "-XX:AOTMode=training",
            "-XX:AOTCache=in.aot",
            "-XX:AOTCacheOutput=out.aot",
            "Main",
        ]);
        let out = normalize_java_launcher_argv(raw);
        assert_eq!(
            out,
            argv(&[
                "java",
                "--XX:AOTMode",
                "training",
                "--XX:AOTCache",
                "in.aot",
                "--XX:AOTCacheOutput",
                "out.aot",
                "Main",
            ])
        );
    }

    #[test]
    fn hotspot_xx_use_container_support_toggle() {
        // `-XX:-UseContainerSupport` -> clap long form.
        let out = normalize_java_launcher_argv(argv(&["java", "-XX:-UseContainerSupport", "Main"]));
        assert_eq!(out, argv(&["java", "--XX:-UseContainerSupport", "Main"]));
        // `-XX:+UseContainerSupport` is the default; gets dropped.
        let out = normalize_java_launcher_argv(argv(&["java", "-XX:+UseContainerSupport", "Main"]));
        assert_eq!(out, argv(&["java", "Main"]));
    }

    #[test]
    fn hotspot_xx_useg1gc_normalizes_to_selector() {
        // `-XX:+UseG1GC` -> `--XX:UseGc G1` (value option, adjacent value).
        let out = normalize_java_launcher_argv(argv(&["java", "-XX:+UseG1GC", "Main"]));
        assert_eq!(out, argv(&["java", "--XX:UseGc", "G1", "Main"]));
        // `-XX:-UseG1GC` explicitly reverts to the default Generational.
        let out = normalize_java_launcher_argv(argv(&["java", "-XX:-UseG1GC", "Main"]));
        assert_eq!(out, argv(&["java", "--XX:UseGc", "Generational", "Main"]));
    }

    #[test]
    fn hotspot_xx_unsupported_gc_is_forwarded_not_dropped() {
        // Selectors are forwarded verbatim; support validation happens at
        // config-apply time (`parse_gc_algorithm`), not here.
        for (flag, name) in [
            ("-XX:+UseParallelGC", "Parallel"),
            ("-XX:+UseSerialGC", "Serial"),
            ("-XX:+UseShenandoahGC", "Shenandoah"),
        ] {
            let out = normalize_java_launcher_argv(argv(&["java", flag, "Main"]));
            assert_eq!(out, argv(&["java", "--XX:UseGc", name, "Main"]), "{flag}");
        }
    }

    #[test]
    fn hotspot_xx_usezgc_normalizes_to_selector() {
        let out = normalize_java_launcher_argv(argv(&["java", "-XX:+UseZGC", "Main"]));
        assert_eq!(out, argv(&["java", "--XX:UseGc", "Z", "Main"]));
    }

    #[test]
    fn hotspot_usezgc_reaches_clap_as_selector_after_pipeline() {
        let argv0: Vec<String> = argv(&["java", "-XX:+UseZGC", "Main"]);
        let stage1 = insert_program_args_separator(argv0);
        let stage2 = normalize_java_launcher_argv(stage1);
        let (stage3, _props) = extract_system_properties(stage2);
        let (stage4, _hot) = extract_hotspot_flags(stage3);
        let parsed = Args::try_parse_from(stage4).expect("clap must accept -XX:+UseZGC");
        assert_eq!(parsed.gc_selector.as_deref(), Some("Z"));
    }

    #[test]
    fn hotspot_xx_non_gc_use_flags_not_mistaken_for_selector() {
        // `-XX:+Use*` flags that do NOT end in `GC` must not become a GC
        // selector. Supported non-GC flags keep their own mapping.
        let out =
            normalize_java_launcher_argv(argv(&["java", "-XX:+UseStringDeduplication", "Main"]));
        assert_eq!(out, argv(&["java", "--XX:StringDedup", "true", "Main"]));

        let out =
            normalize_java_launcher_argv(argv(&["java", "-XX:-UseStringDeduplication", "Main"]));
        assert_eq!(out, argv(&["java", "--XX:StringDedup", "false", "Main"]));

        let out = normalize_java_launcher_argv(argv(&["java", "-XX:+UseCompressedOops", "Main"]));
        assert_eq!(out, argv(&["java", "Main"]));
    }

    #[test]
    fn hotspot_useg1gc_reaches_clap_as_selector_after_pipeline() {
        // End-to-end: `-XX:+UseG1GC` survives the full pre-clap pipeline and
        // lands in `Args::gc_selector`, the field the config-apply step reads.
        let argv0: Vec<String> = argv(&["java", "-XX:+UseG1GC", "-classpath", "x", "Main"]);
        let stage1 = insert_program_args_separator(argv0);
        let stage2 = normalize_java_launcher_argv(stage1);
        let (stage3, _props) = extract_system_properties(stage2);
        let (stage4, _hot) = extract_hotspot_flags(stage3);
        let parsed = Args::try_parse_from(stage4).expect("clap must accept -XX:+UseG1GC");
        assert_eq!(parsed.gc_selector.as_deref(), Some("G1"));
        assert_eq!(parsed.class_name.as_deref(), Some("Main"));
    }

    #[test]
    fn hotspot_string_dedup_reaches_clap_after_pipeline() {
        let argv0: Vec<String> = argv(&["java", "-XX:+UseStringDeduplication", "Main"]);
        let stage1 = insert_program_args_separator(argv0);
        let stage2 = normalize_java_launcher_argv(stage1);
        let (stage3, _props) = extract_system_properties(stage2);
        let (stage4, _hot) = extract_hotspot_flags(stage3);
        let parsed = Args::try_parse_from(stage4).expect("clap must accept string dedup flag");
        assert_eq!(parsed.g1_string_dedup.as_deref(), Some("true"));
        assert_eq!(parsed.class_name.as_deref(), Some("Main"));
    }

    #[test]
    fn hotspot_repeated_gc_flags_last_wins() {
        // HotSpot honours the last `-XX:+Use*GC`; clap's `Set` action keeps the
        // last value, so a `-XX:+UseParallelGC -XX:+UseG1GC` pair selects G1.
        let argv0: Vec<String> = argv(&["java", "-XX:+UseParallelGC", "-XX:+UseG1GC", "Main"]);
        let stage1 = insert_program_args_separator(argv0);
        let stage2 = normalize_java_launcher_argv(stage1);
        let (stage3, _props) = extract_system_properties(stage2);
        let (stage4, _hot) = extract_hotspot_flags(stage3);
        let parsed = Args::try_parse_from(stage4).expect("clap must accept repeated GC flags");
        assert_eq!(parsed.gc_selector.as_deref(), Some("G1"));
    }

    #[test]
    fn hotspot_xx_audit_missing_natives_toggle() {
        let out = normalize_java_launcher_argv(argv(&["java", "-XX:+AuditMissingNatives", "Main"]));
        assert_eq!(out, argv(&["java", "--XX:AuditMissingNatives", "Main"]));
        // Disabled form drops the flag (default is off).
        let out = normalize_java_launcher_argv(argv(&["java", "-XX:-AuditMissingNatives", "Main"]));
        assert_eq!(out, argv(&["java", "Main"]));
    }

    #[test]
    fn hotspot_xx_show_code_details_toggle() {
        // `-XX:+ShowCodeDetailsInExceptionMessages` -> the explicit `=true` form.
        let out = normalize_java_launcher_argv(argv(&[
            "java",
            "-XX:+ShowCodeDetailsInExceptionMessages",
            "Main",
        ]));
        assert_eq!(
            out,
            argv(&[
                "java",
                "--XX:ShowCodeDetailsInExceptionMessages=true",
                "Main"
            ])
        );
        // Disabled form -> the explicit `=false` form (the default is now on,
        // so opting out must be representable, not merely dropped).
        let out = normalize_java_launcher_argv(argv(&[
            "java",
            "-XX:-ShowCodeDetailsInExceptionMessages",
            "Main",
        ]));
        assert_eq!(
            out,
            argv(&[
                "java",
                "--XX:ShowCodeDetailsInExceptionMessages=false",
                "Main"
            ])
        );
    }

    #[test]
    fn hotspot_noverify_rewrites_to_clap_long() {
        let raw = argv(&["java", "-noverify", "Main"]);
        let out = normalize_java_launcher_argv(raw);
        assert_eq!(out, argv(&["java", "--noverify", "Main"]));
    }

    #[test]
    fn hotspot_xmx_after_separator_is_program_arg() {
        // Tokens past `--` belong to the Java program, not the launcher.
        let raw = argv(&["java", "Main", "--", "-Xmx256m"]);
        let out = normalize_java_launcher_argv(raw);
        assert_eq!(out, argv(&["java", "Main", "--", "-Xmx256m"]));
    }

    #[test]
    fn hotspot_xmx_passes_clap_after_full_pipeline() {
        // C36 acceptance test: stock `java -Xmx256m -classpath x Main`
        // must parse end-to-end. Exercises the entire pre-clap pipeline
        // exactly as `run()` would.
        let argv0: Vec<String> = argv(&["java", "-Xmx256m", "-classpath", "x", "Main"]);
        let stage1 = insert_program_args_separator(argv0);
        let stage2 = normalize_java_launcher_argv(stage1);
        let (stage3, _props) = extract_system_properties(stage2);
        let (stage4, _hot) = extract_hotspot_flags(stage3);
        let parsed = Args::try_parse_from(stage4).expect("clap must accept HotSpot -Xmx");
        assert_eq!(parsed.max_heap.as_deref(), Some("256m"));
        assert_eq!(parsed.classpath.as_deref(), Some("x"));
        assert_eq!(parsed.class_name.as_deref(), Some("Main"));
    }

    #[test]
    fn nojit_flag_is_accepted_by_clap() {
        // README documents `--nojit`; clap must accept it.
        let parsed = Args::try_parse_from(argv(&["cratonvm", "--nojit", "Main"]))
            .expect("clap must accept --nojit");
        assert!(parsed.nojit);
    }

    #[test]
    fn enable_native_access_bare_flag_does_not_consume_main_class() {
        let argv0: Vec<String> = argv(&["java", "--enable-native-access", "Main", "arg"]);
        let stage1 = insert_program_args_separator(argv0);
        let stage2 = normalize_java_launcher_argv(stage1);
        let (stage3, _props) = extract_system_properties(stage2);
        let (stage4, _hot) = extract_hotspot_flags(stage3);
        let parsed = Args::try_parse_from(stage4).expect("clap must parse bare native-access flag");

        assert_eq!(parsed.enable_native_access.as_deref(), Some("ALL-UNNAMED"));
        assert_eq!(parsed.class_name.as_deref(), Some("Main"));
        assert_eq!(parsed.args, argv(&["arg"]));
    }

    #[test]
    fn enable_preview_is_a_bare_flag_and_does_not_consume_main_class() {
        // Before docs/known-issues/jdk-only/W7-28-preview-classfile-gating.md
        // this was `error: unexpected argument '--enable-preview' found`, with
        // clap suggesting `--enable-native-access`. Run the whole pre-clap
        // pipeline, not just `try_parse_from`: a bare boolean needs no
        // VALUE_TAKING_OPTS entry, and this is what proves it — if it ever
        // acquired one, `Main` would be eaten as the flag's value.
        let argv0: Vec<String> = argv(&["java", "--enable-preview", "Main", "arg"]);
        let stage1 = insert_program_args_separator(argv0);
        let stage2 = normalize_java_launcher_argv(stage1);
        let (stage3, _props) = extract_system_properties(stage2);
        let (stage4, _hot) = extract_hotspot_flags(stage3);
        let parsed = Args::try_parse_from(stage4).expect("clap must parse --enable-preview");

        assert!(parsed.enable_preview);
        assert_eq!(parsed.class_name.as_deref(), Some("Main"));
        assert_eq!(parsed.args, argv(&["arg"]));
    }

    #[test]
    fn enable_preview_defaults_off_like_hotspot() {
        // HotSpot's default is off, measured: plain `java -cp . P` on a
        // 69.65535 class file raises UnsupportedClassVersionError.
        let parsed = Args::try_parse_from(argv(&["cratonvm", "Main"]))
            .expect("clap must parse without the flag");
        assert!(!parsed.enable_preview);
    }

    #[test]
    fn enable_native_access_equals_value_is_still_accepted() {
        let parsed = Args::try_parse_from(argv(&[
            "cratonvm",
            "--enable-native-access=java.base",
            "Main",
        ]))
        .expect("clap must parse native-access value with equals");

        assert_eq!(parsed.enable_native_access.as_deref(), Some("java.base"));
        assert_eq!(parsed.class_name.as_deref(), Some("Main"));
    }

    #[test]
    fn hotspot_xss_inline_is_accepted_and_ignored() {
        // B3 regression: single-dash `-Xss<size>` (thread stack size —
        // Surefire/Gradle pass this routinely) was not in the handled set, so
        // it fell through to the final `else`, reached clap verbatim, and clap
        // rejected it as "unexpected argument '-X'". The catch-all `-X` arm now
        // accepts-and-ignores it (the whole token is dropped), same as `-XX:`.
        let raw = argv(&["java", "-Xss512k", "Main"]);
        let out = normalize_java_launcher_argv(raw);
        assert_eq!(out, argv(&["java", "Main"]));
    }

    #[test]
    fn hotspot_misc_x_flags_are_accepted_and_ignored() {
        // Other no-value `-X` knobs HotSpot accepts: `-Xint`, `-Xbatch`,
        // `-Xrs`, `-XshowSettings`, `-Xnoclassgc`. All drop out, leaving the
        // class name (and any later args) intact.
        let raw = argv(&["java", "-Xint", "-Xbatch", "-Xrs", "-Xnoclassgc", "Main"]);
        let out = normalize_java_launcher_argv(raw);
        assert_eq!(out, argv(&["java", "Main"]));
    }

    #[test]
    fn hotspot_server_flag_passes_clap_after_full_pipeline() {
        // WildFly HostController still launches child JVMs with `-server`.
        // HotSpot accepts it as a VM-selection hint; CratonVM ignores it but
        // must not let clap parse it as short flags (`-s -e ...`).
        let argv0: Vec<String> = argv(&["java", "-server", "-classpath", "x", "Main"]);
        let stage1 = insert_program_args_separator(argv0);
        let stage2 = normalize_java_launcher_argv(stage1);
        let (stage3, _props) = extract_system_properties(stage2);
        let (stage4, _hot) = extract_hotspot_flags(stage3);
        let parsed = Args::try_parse_from(stage4).expect("clap must accept HotSpot -server");
        assert_eq!(parsed.classpath.as_deref(), Some("x"));
        assert_eq!(parsed.class_name.as_deref(), Some("Main"));
    }

    #[test]
    fn hotspot_xss_passes_clap_after_full_pipeline() {
        // B3 acceptance: stock `java -Xss512k -classpath x Main` must parse
        // end-to-end with `Main` resolved as the main class — previously clap
        // aborted on the unrecognized `-Xss512k`.
        let argv0: Vec<String> = argv(&["java", "-Xss512k", "-classpath", "x", "Main"]);
        let stage1 = insert_program_args_separator(argv0);
        let stage2 = normalize_java_launcher_argv(stage1);
        let (stage3, _props) = extract_system_properties(stage2);
        let (stage4, _hot) = extract_hotspot_flags(stage3);
        let parsed =
            Args::try_parse_from(stage4).expect("clap must accept HotSpot -Xss after pipeline");
        assert_eq!(parsed.classpath.as_deref(), Some("x"));
        assert_eq!(parsed.class_name.as_deref(), Some("Main"));
    }

    #[test]
    fn user_double_dash_reaches_program_args() {
        // B2 regression: a user `--` between program args
        // (`java Main a -- b`) must be delivered to the program verbatim as
        // `["a", "--", "b"]`. The launcher's own separator is consumed by clap
        // (it never reaches `parsed.args`), so the interior `--` here is purely
        // user data and must survive the whole pipeline.
        let argv0: Vec<String> = argv(&["java", "Main", "a", "--", "b"]);
        let stage1 = insert_program_args_separator(argv0);
        let stage2 = normalize_java_launcher_argv(stage1);
        let (stage3, _props) = extract_system_properties(stage2);
        let (stage4, _hot) = extract_hotspot_flags(stage3);
        let parsed = Args::try_parse_from(stage4).expect("clap must parse user `--` argv");
        assert_eq!(parsed.class_name.as_deref(), Some("Main"));
        // The interior user `--` is present in the trailing program args.
        assert_eq!(parsed.args, argv(&["a", "--", "b"]));
        // After [LOW arg-parse fix (1)] `run()` no longer pops any trailing
        // `--`; here the last arg is `b` anyway, so the program sees
        // ["a","--","b"] verbatim (the trailing-`--` case is covered by
        // `trailing_user_double_dash_is_preserved_through_pipeline`).
        assert_ne!(parsed.args.last().map(String::as_str), Some("--"));
    }

    #[test]
    fn classpath_class_and_program_args_pipeline() {
        // Regression guard for cli_main_args integration test.
        // `cratonvm --classpath <dir> PrintArgs alpha beta gamma` must produce
        // class_name=PrintArgs and args=["alpha","beta","gamma"].
        let argv0: Vec<String> = argv(&[
            "cratonvm",
            "--classpath",
            "/tmp/dir",
            "PrintArgs",
            "alpha",
            "beta",
            "gamma",
        ]);
        let stage1 = insert_program_args_separator(argv0);
        let stage2 = normalize_java_launcher_argv(stage1);
        let (stage3, _props) = extract_system_properties(stage2);
        let (stage4, _hot) = extract_hotspot_flags(stage3);
        let parsed = Args::try_parse_from(stage4).expect("clap must parse classpath+args argv");
        assert_eq!(parsed.class_name.as_deref(), Some("PrintArgs"));
        assert_eq!(parsed.args, argv(&["alpha", "beta", "gamma"]));
    }

    #[test]
    fn launcher_trailing_double_dash_artifact_is_popped() {
        // B2 regression guard: `-mp` is listed in VALUE_TAKING_OPTS, so
        // `insert_program_args_separator` treats `/modules` as `-mp`'s value
        // and does NOT inject a spurious `"--"` between them.
        //
        // The pipeline result for `java -mp /modules`:
        //   normalize inserts `"--"` before `-mp` → clap sees ["java", "--", "-mp", "/modules"]
        //   class_name = Some("-mp")  (first positional after "--")
        //   args = ["/modules"]       (trailing_var_arg gets the rest)
        //
        // The old trailing `"--"` artifact (from when `/modules` was wrongly
        // treated as the main-class name) no longer occurs. The guard pop in
        // `run()` is a no-op.
        let argv0: Vec<String> = argv(&["java", "-mp", "/modules"]);
        let stage1 = insert_program_args_separator(argv0);
        let stage2 = normalize_java_launcher_argv(stage1);
        let (stage3, _props) = extract_system_properties(stage2);
        let (stage4, _hot) = extract_hotspot_flags(stage3);
        let parsed = Args::try_parse_from(stage4).expect("clap must parse `-mp` argv");
        // `-mp` is consumed as the class_name (first positional after `--`).
        assert_eq!(parsed.class_name.as_deref(), Some("-mp"));
        let mut prog = parsed.args;
        // No trailing "--" artifact — VALUE_TAKING_OPTS prevents the spurious injection.
        assert_ne!(prog.last().map(String::as_str), Some("--"));
        // The guard pop in `run()` is a no-op; args are already clean.
        if prog.last().map(String::as_str) == Some("--") {
            prog.pop();
        }
        // Only `/modules` remains in args; `-mp` went to class_name.
        assert_eq!(prog, argv(&["/modules"]));
    }

    // -----------------------------------------------------------------------
    // [LOW arg-parse fix (1)] Trailing user `--` is preserved as a program arg.
    // -----------------------------------------------------------------------

    #[test]
    fn trailing_user_double_dash_is_preserved_through_pipeline() {
        // `java Main a --` per JDK launcher semantics gives the program
        // `["a", "--"]`. The launcher's own boundary separator is consumed by
        // clap; the interior+trailing user `--` here is genuine program data
        // and the trailing one must NOT be popped (the old unconditional pop in
        // `run()` silently dropped it).
        let argv0: Vec<String> = argv(&["java", "Main", "a", "--"]);
        let stage1 = insert_program_args_separator(argv0);
        let stage2 = normalize_java_launcher_argv(stage1);
        let (stage3, _props) = extract_system_properties(stage2);
        let (stage4, _hot) = extract_hotspot_flags(stage3);
        let parsed = Args::try_parse_from(stage4).expect("clap must parse trailing `--` argv");
        assert_eq!(parsed.class_name.as_deref(), Some("Main"));
        // The trailing user `--` survives clap as the last program arg.
        assert_eq!(parsed.args.last().map(String::as_str), Some("--"));
        // Mirror `run()`'s post-clap handling: the pop is gone, so the trailing
        // `--` reaches the program verbatim.
        assert_eq!(parsed.args, argv(&["a", "--"]));
    }

    // -----------------------------------------------------------------------
    // [LOW arg-parse fix (2)] A value of a value-taking option that starts with
    // `-D` is NOT hijacked as a `-Dkey=value` system property.
    // -----------------------------------------------------------------------

    #[test]
    fn dminus_value_of_value_opt_is_not_a_system_property() {
        // `--Xlog -Dgc` — `-Dgc` is the VALUE of `--Xlog`, not a system
        // property. It must be passed through to clap (as `--Xlog`'s operand)
        // and must NOT appear in the extracted props list.
        let (filtered, props) =
            extract_system_properties(argv(&["java", "--Xlog", "-Dgc", "Main"]));
        assert_eq!(filtered, argv(&["java", "--Xlog", "-Dgc", "Main"]));
        assert!(
            props.is_empty(),
            "value token must not be parsed as -D prop"
        );
    }

    #[test]
    fn dminus_genuine_property_still_extracted_in_option_position() {
        // A genuine `-Dkey=value` in an OPTION position (not following a
        // value-taking option) is still extracted — the fix only protects the
        // value slot, it doesn't disable `-D` handling.
        let (filtered, props) =
            extract_system_properties(argv(&["java", "-Dfoo=bar", "--classpath", "x", "Main"]));
        // `-Dfoo=bar` removed; `--classpath x Main` survive (x is the value of
        // --classpath and is not a -D candidate anyway).
        assert_eq!(filtered, argv(&["java", "--classpath", "x", "Main"]));
        assert_eq!(props, vec![("foo".to_string(), "bar".to_string())]);
    }

    #[test]
    fn dminus_value_starting_with_dminus_for_classpath() {
        // Pathological but legal: a classpath entry literally starting with
        // `-D` (e.g. a directory named `-Dweird`). It is `--classpath`'s value
        // and must survive as-is, not become a fabricated system property.
        let (filtered, props) =
            extract_system_properties(argv(&["java", "--classpath", "-Dweird", "Main"]));
        assert_eq!(filtered, argv(&["java", "--classpath", "-Dweird", "Main"]));
        assert!(props.is_empty());
    }

    #[test]
    fn dminus_after_separator_is_program_arg_not_property() {
        // Unchanged behaviour guard: `-Dfoo=bar` after `--` belongs to the
        // program (jboss-modules style) and is never extracted.
        let (filtered, props) =
            extract_system_properties(argv(&["java", "Main", "--", "-Dfoo=bar"]));
        assert_eq!(filtered, argv(&["java", "Main", "--", "-Dfoo=bar"]));
        assert!(props.is_empty());
    }

    // -----------------------------------------------------------------------
    // [LOW arg-parse fix (3)] An unrecognized `-X` flag must not swallow the
    // following main-class token.
    // -----------------------------------------------------------------------

    #[test]
    fn unknown_x_flag_does_not_swallow_following_main_class() {
        // `-Xunknown Main`: the unknown `-X` flag is dropped (`i += 1`), and
        // the following bare token `Main` is NOT consumed as its value — it
        // survives so it can be resolved as the main class.
        let out = normalize_java_launcher_argv(argv(&["java", "-Xunknown", "Main"]));
        assert_eq!(out, argv(&["java", "Main"]));
    }

    #[test]
    fn unknown_x_flag_before_class_resolves_class_through_pipeline() {
        // End-to-end: an unknown separate-looking `-X` flag immediately before
        // the main class must still resolve `Main` (not absorb it). Exercises
        // the full pre-clap pipeline as `run()` would.
        let argv0: Vec<String> = argv(&["java", "-Xint", "-classpath", "x", "Main"]);
        let stage1 = insert_program_args_separator(argv0);
        let stage2 = normalize_java_launcher_argv(stage1);
        let (stage3, _props) = extract_system_properties(stage2);
        let (stage4, _hot) = extract_hotspot_flags(stage3);
        let parsed =
            Args::try_parse_from(stage4).expect("clap must accept unknown -X before main class");
        assert_eq!(parsed.classpath.as_deref(), Some("x"));
        assert_eq!(parsed.class_name.as_deref(), Some("Main"));
    }

    #[test]
    fn unknown_x_flag_directly_before_class_no_classpath() {
        // The minimal swallow scenario with no intervening options:
        // `java -Xint Main` must resolve `Main` as the main class.
        let argv0: Vec<String> = argv(&["java", "-Xint", "Main"]);
        let stage1 = insert_program_args_separator(argv0);
        let stage2 = normalize_java_launcher_argv(stage1);
        let (stage3, _props) = extract_system_properties(stage2);
        let (stage4, _hot) = extract_hotspot_flags(stage3);
        let parsed = Args::try_parse_from(stage4).expect("clap must accept `-Xint Main`");
        assert_eq!(parsed.class_name.as_deref(), Some("Main"));
    }

    // -----------------------------------------------------------------------
    // JDK-only mode (docs/feature-designs/jdk-only-mode.md §9)
    //
    // These pin the launcher half of the contract: the flag surface other
    // crates (difftest) already invoke, the conflict *ordering* that decides
    // which of two true diagnoses the user sees, the redaction rule, and the
    // shape/order of the census files difftest parses.
    // -----------------------------------------------------------------------

    #[test]
    fn jdk_only_flag_surface_matches_the_contract() {
        let parsed = Args::try_parse_from(argv(&[
            "cratonvm",
            "--jdk-only",
            "--jdk-only-report",
            "report.json",
            "--dump-class-origins",
            "origins.json",
            "--dump-native-registry",
            "natives.json",
            "--trace-jdk-only",
            "--explain-jdk-only",
            "Main",
        ]))
        .expect("the seven §9 flags must parse together");
        assert!(parsed.jdk_only);
        assert!(parsed.trace_jdk_only);
        assert!(parsed.explain_jdk_only);
        assert_eq!(parsed.jdk_only_report.as_deref(), Some("report.json"));
        assert_eq!(parsed.dump_class_origins.as_deref(), Some("origins.json"));
        assert_eq!(parsed.dump_native_registry.as_deref(), Some("natives.json"));
        assert_eq!(parsed.class_name.as_deref(), Some("Main"));

        // Composes with the (redundant but legal) explicit library selection…
        let both = Args::try_parse_from(argv(&["cratonvm", "--jdk-only", "--real-jdk", "Main"]))
            .expect("--jdk-only and --real-jdk select the same library");
        assert!(both.jdk_only && both.real_jdk);

        // …and conflicts with the other library.
        assert!(
            Args::try_parse_from(argv(&["cratonvm", "--jdk-only", "--synthetic-jdk", "Main"]))
                .is_err(),
            "--jdk-only --synthetic-jdk must be rejected by clap"
        );

        // Both value-taking flags must be known to the pre-clap separator
        // inserter, or their path argument is mistaken for the main class.
        assert!(VALUE_TAKING_OPTS.contains(&"--jdk-only-report"));
        assert!(VALUE_TAKING_OPTS.contains(&"--dump-class-origins"));

        // The long help names every one of them.
        for flag in [
            "--real-jdk",
            "--synthetic-jdk",
            "--jdk-only",
            "--jdk-only-report",
            "--dump-class-origins",
            "--trace-jdk-only",
            "--explain-jdk-only",
        ] {
            assert!(LONG_ABOUT.contains(flag), "long_about omits {flag}");
        }
    }

    /// The banner is printed before clap runs, so `--jdk-only` has to be
    /// visible to the raw-argv scan — both as "which class library" (it implies
    /// real) and as "which policy".
    #[test]
    fn jdk_only_is_visible_to_the_pre_clap_banner_scan() {
        assert_eq!(
            scan_requested_jdk_mode(&tokens(&["cratonvm", "--jdk-only", "Main", "--"])),
            JdkMode::Real
        );
        assert_eq!(
            scan_requested_compatibility_mode(&tokens(&["cratonvm", "--jdk-only", "Main", "--"])),
            CompatibilityMode::JdkOnly
        );
        // A program argument is not a launcher flag.
        assert_eq!(
            scan_requested_compatibility_mode(&tokens(&["cratonvm", "Main", "--", "--jdk-only"])),
            CompatibilityMode::Compatible
        );

        let strict = version_banner(
            VersionQuery::Version,
            JdkMode::Real,
            CompatibilityMode::JdkOnly,
            None,
        );
        assert!(
            strict.contains("compatibility=jdk-only"),
            "the build line must name the policy: {strict}"
        );
        // ...and the execution-mode list must be the one `java.vm.info` will
        // carry, so the banner and the property agree on the policy token.
        assert!(
            strict.contains(cratonvm_vm::vm::vm_info_mode_list(
                CompatibilityMode::JdkOnly
            )),
            "the banner must spell the mode list the way java.vm.info does: {strict}"
        );
        let default = version_banner(
            VersionQuery::Version,
            JdkMode::Real,
            CompatibilityMode::Compatible,
            None,
        );
        assert!(default.contains("compatibility=compatible"), "{default}");
        // Compatible mode's banner is byte-for-byte what it always was (§10).
        assert!(default.contains("mixed mode, sharing"), "{default}");
        assert!(!default.contains("jdk-only"), "{default}");
    }

    /// Ordering matters: `--jdk-only --synthetic-jdk` is a policy conflict, and
    /// resolving the policy before the library is what makes the user see that
    /// diagnosis rather than the generic library one.
    #[test]
    fn compatibility_resolution_reports_the_policy_conflict() {
        assert_eq!(
            resolve_compatibility_mode(false, false).expect("default is compatible"),
            CompatibilityMode::Compatible
        );
        assert_eq!(
            resolve_compatibility_mode(true, false).expect("--jdk-only alone is fine"),
            CompatibilityMode::JdkOnly
        );
        let err = resolve_compatibility_mode(true, true).expect_err("the pair must be rejected");
        let msg = format!("{err:#}");
        assert!(msg.contains("--jdk-only"), "{msg}");
        assert!(msg.contains("--synthetic-jdk"), "{msg}");
        // The specific (policy) diagnosis, not `resolve_jdk_mode`'s generic
        // "two different standard-library implementations" one.
        assert!(!msg.contains("mutually exclusive"), "{msg}");
    }

    // The census writers, the JSON helpers, the origin-bucket fold and the
    // path redaction all moved to `vm/src/vm/vm_init.rs` when the two
    // implementations were collapsed to one; their tests moved with them
    // (`cargo test -p cratonvm-vm`). What stays testable here is the launcher's
    // own job: flag parsing, mode resolution, and the banner.
}
