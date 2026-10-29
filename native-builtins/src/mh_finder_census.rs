// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! The `Lookup` finder phase census, `CRATONVM_DBG=mh-finder-census`
//! (`CRATONVM_DBG_MH_FINDER_CENSUS`; interpreter round i4 wave 89, lane L4).
//!
//! # Why this exists
//!
//! `L4W88FindConstructorSplitBench` row `find-warm-class` (a `findConstructor`
//! repeated with the same arguments on a class already used) costs 25.7 us
//! against HotSpot's 2.7 us, and that row was measured WITH the method finder
//! mint memo registered (`method-finder-mint` is a default row of
//! `CRATONVM_INTRINSIC_BODY_COMPILES_ROWS`). A repeat is exactly what the memo
//! mints, so either the memo is not consulted (a compiled caller bound to the
//! JDK body skips the native door), or it refuses (and why), or the repeat is
//! minted and the cost sits elsewhere. This census answers which, by count,
//! the way `classloading::define_census` answers where a define's time goes.
//!
//! # What it records
//!
//! * Phase times, nanoseconds summed over calls ([`Phase`]):
//!   - the method finder `Intrinsic` as a whole, its mint, the JDK body it
//!     runs on a miss (`findStatic` / `findConstructor` / ...: `resolveOrFail`,
//!     `checkAccess`, `getDirectMethod`, `DirectMethodHandle.make`), and the
//!     filing of the body's answer;
//!   - the same three for the field finder `Intrinsic`;
//!   - `MethodHandleNatives.resolve` (every call, and the share that ran
//!     inside a finder body: the body minus that share is the JDK's Java);
//!   - the interpreter's `MethodHandle.invoke` / `invokeExact` natives, split
//!     by the first call on a handle (per thread and collection epoch) and a
//!     repeat.
//! * Event counts by reason (`[mh-finder-census] event ...` rows): finder
//!   entries per finder, key refusals by reason, memo line outcomes (hit,
//!   empty slot, another VM, another epoch, another key, holder state, mint
//!   allocation failure), body outcomes, filing outcomes by reason (and the
//!   answer's class name when it is not a `DirectMethodHandle` species), the
//!   resolve memos' hits and misses, and which lane served an invoke.
//!
//! * Wave 90 (lane L4), the split of the ~180 us of a finder body outside
//!   `resolve` and of a first `invoke`:
//!   - class initialization and class link time, each charged to the scope
//!     it started in (finder body, first invoke, elsewhere), timed from the
//!     VM (`vm/src/vm/vm_util.rs`: `initialize_class_shared`,
//!     `link_claimed_class`) through [`work_clock`];
//!   - with `CRATONVM_DBG_INTERP_FRAMES=1` as well, the interpreted frames
//!     pushed inside each scope, by method ([`note_interp_frame`]): a JDK
//!     method that runs interpreted once per finder call shows here with a
//!     count near the finder count, and a scope total of thousands per call
//!     means the body ran interpreted.
//!
//! * Wave 91 (lane L4), where a body's time goes when its ~125 interpreted
//!   frames are too few to explain it:
//!   - Java re-entries (`reentry.<scope>`): every nested
//!     `interpreter::execute` started inside a scope, tallied by method, with
//!     the outermost ones timed. `interpreter::execute` is the door a native's
//!     Java callback and a compiled caller's interpreted callee both take, and
//!     each costs a by-name method lookup and a frame build of its own;
//!   - the JIT's mutator-side doors (`jit-door.<scope>`: the interpreter's
//!     tier-up promotion and a compiled caller binding its callee) and the
//!     compiles they run on the mutator itself (`jit-compile.<scope>`);
//!   - `interp-frames <scope>/in-reentry`: how many of the scope's
//!     interpreted frames were pushed inside a re-entry, not by the body's
//!     own interpreter loop.
//!
//! * Wave 94 (lane L4), which of those shares the MEDIAN body pays (the
//!   phase table's means fold one-time costs into every body; the bench
//!   rows are medians):
//!   - `timed` rows: the classes initialized and linked inside a scope, by
//!     name (`class-init.<scope> <class>`, `class-link.<scope> <class>`, from
//!     [`work_clock_named`]), and the outermost re-entries by method
//!     (`reentry-time.<scope> <method><descriptor>`), each with its count and
//!     time. A count of 1 is a one-time JDK class; a count near the body
//!     count is per-body work;
//!   - `body-dist` rows: per finder body, its time and its class-work,
//!     JIT-door, mutator-compile, re-entry and resolve shares; dumped as the
//!     number of bodies that paid each, the median share, and the median of
//!     the "clean" bodies (no class work, no mutator compile).
//!
//! * Wave 95 (lane L4), the warm `invokeExact` / `invoke` of a REAL handle,
//!   which runs the JDK's invoker linker first (`lang_invoke::mh_linked_invoker`):
//!   `timed` rows `linker <name> <outcome>` (`pair-hit`: the caller-keyed pair
//!   row; `basic-hit`: the basic-type member row with the site-type table's
//!   type, `CRATONVM_MH_LINKER_BASIC_MEMBER`; `miss-shareable`;
//!   `miss-user-site`: a site naming a class outside `java/`, which no pair row
//!   may hold, so every call ran `MethodType.fromMethodDescriptorString`'s table
//!   or factory, `Invokers.methodHandleInvokeLinkerMethod` and three full root
//!   refreshes), the sub-phases of a miss (`linker <name> site-type`,
//!   `linker <name> linker-method`), the LambdaForm chain itself
//!   (`linker <name> link-to`), and the lanes served ahead of the linker
//!   (`lane field-before-linker`, `CRATONVM_MH_REAL_FIELD_BEFORE_LINKER`).
//!
//! Nothing is recorded, and no clock is read, unless the token is set: every
//! entry point is one flag load and a not-taken branch. Java-visible
//! behaviour does not change either way (the census only reads).
//!
//! Process-wide atomics, as `define_census`: this is a diagnostic dumped once
//! at exit, not compatibility state.

use std::sync::atomic::{AtomicU64, Ordering};

/// Is the census on (`CRATONVM_DBG_MH_FINDER_CENSUS`)?
#[inline]
pub(crate) fn enabled() -> bool {
    crate::nbflags().dbg_mh_finder_census
}

/// The timed phases. `ALL` is the dump order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    /// `mh_method_finder_mint_intrinsic`, entry to return.
    MethodFinder = 0,
    /// Inside it: a memo hit's mint (`method_finder_mint`).
    MethodFinderMint,
    /// Inside it: the JDK body (`invoke_virtual_bytecode_only`).
    MethodFinderBody,
    /// Inside it: `method_finder_mint_file`.
    MethodFinderFile,
    /// `mh_finder_mint_intrinsic` (field finders), entry to return.
    FieldFinder,
    /// Inside it: a memo hit's mint.
    FieldFinderMint,
    /// Inside it: the JDK body.
    FieldFinderBody,
    /// `MethodHandleNatives.resolve`, every call.
    Resolve,
    /// The `Resolve` calls made inside a finder body (either finder).
    ResolveInFinderBody,
    /// `MethodHandle.invoke` native, the first call on a handle.
    InvokeFirst,
    /// `MethodHandle.invoke` native, a repeat call.
    InvokeRepeat,
    /// `MethodHandle.invokeExact` native, the first call on a handle.
    InvokeExactFirst,
    /// `MethodHandle.invokeExact` native, a repeat call.
    InvokeExactRepeat,
    /// Wave 90: a class initialization (`initialize_class_shared`, the link
    /// and `<clinit>` included) run inside a finder body.
    ClassInitInFinderBody,
    /// A class initialization run inside a first `invoke` / `invokeExact`.
    ClassInitInInvokeFirst,
    /// Any other class initialization (the bench's own `Class.forName`, ...).
    ClassInitElsewhere,
    /// A class link (`link_claimed_class`: verification, preparation, the
    /// deferred type-map walk) run inside a finder body.
    ClassLinkInFinderBody,
    /// A class link run inside a first `invoke` / `invokeExact`.
    ClassLinkInInvokeFirst,
    /// Any other class link.
    ClassLinkElsewhere,
    /// Wave 91: a nested `interpreter::execute` (a Java re-entry from a
    /// native, a compiled caller's interpreted callee, a reflective door)
    /// that started inside a finder body, OUTERMOST re-entries only (the
    /// body's own `execute` is the scope's level 0 and is not counted; a
    /// re-entry nested in another re-entry is inside its outer one's time).
    ReentryInFinderBody,
    /// The same inside a first `invoke` / `invokeExact`.
    ReentryInInvokeFirst,
    /// Wave 91: the JIT's mutator-side compile doors (`try_jit_upgrade_with_gate`,
    /// the interpreter's tier-up promotion, and `try_jit_compile_callee*`,
    /// a compiled caller binding its callee), run inside a finder body.
    /// Inclusive of any synchronous compile they do.
    JitDoorInFinderBody,
    /// The same inside a first `invoke` / `invokeExact`.
    JitDoorInInvokeFirst,
    /// Wave 91: `try_jit_compile_callee_slow` (a compile on the mutator
    /// thread, not on a background worker) inside a finder body. Nested in
    /// [`Phase::JitDoorInFinderBody`]'s time.
    JitCompileInFinderBody,
    /// The same inside a first `invoke` / `invokeExact`.
    JitCompileInInvokeFirst,
}

/// Number of [`Phase`]s.
const PHASES: usize = 25;

impl Phase {
    const ALL: [Phase; PHASES] = [
        Phase::MethodFinder,
        Phase::MethodFinderMint,
        Phase::MethodFinderBody,
        Phase::MethodFinderFile,
        Phase::FieldFinder,
        Phase::FieldFinderMint,
        Phase::FieldFinderBody,
        Phase::Resolve,
        Phase::ResolveInFinderBody,
        Phase::InvokeFirst,
        Phase::InvokeRepeat,
        Phase::InvokeExactFirst,
        Phase::InvokeExactRepeat,
        Phase::ClassInitInFinderBody,
        Phase::ClassInitInInvokeFirst,
        Phase::ClassInitElsewhere,
        Phase::ClassLinkInFinderBody,
        Phase::ClassLinkInInvokeFirst,
        Phase::ClassLinkElsewhere,
        Phase::ReentryInFinderBody,
        Phase::ReentryInInvokeFirst,
        Phase::JitDoorInFinderBody,
        Phase::JitDoorInInvokeFirst,
        Phase::JitCompileInFinderBody,
        Phase::JitCompileInInvokeFirst,
    ];

    fn label(self) -> &'static str {
        match self {
            Phase::MethodFinder => "method-finder",
            Phase::MethodFinderMint => "  mint",
            Phase::MethodFinderBody => "  jdk-body",
            Phase::MethodFinderFile => "  file",
            Phase::FieldFinder => "field-finder",
            Phase::FieldFinderMint => "  mint",
            Phase::FieldFinderBody => "  jdk-body",
            Phase::Resolve => "mhn-resolve",
            Phase::ResolveInFinderBody => "  in-finder-body",
            Phase::InvokeFirst => "invoke-first",
            Phase::InvokeRepeat => "invoke-repeat",
            Phase::InvokeExactFirst => "invokeExact-first",
            Phase::InvokeExactRepeat => "invokeExact-repeat",
            Phase::ClassInitInFinderBody => "class-init.finder-body",
            Phase::ClassInitInInvokeFirst => "class-init.invoke-first",
            Phase::ClassInitElsewhere => "class-init.elsewhere",
            Phase::ClassLinkInFinderBody => "class-link.finder-body",
            Phase::ClassLinkInInvokeFirst => "class-link.invoke-first",
            Phase::ClassLinkElsewhere => "class-link.elsewhere",
            Phase::ReentryInFinderBody => "reentry.finder-body",
            Phase::ReentryInInvokeFirst => "reentry.invoke-first",
            Phase::JitDoorInFinderBody => "jit-door.finder-body",
            Phase::JitDoorInInvokeFirst => "jit-door.invoke-first",
            Phase::JitCompileInFinderBody => "jit-compile.finder-body",
            Phase::JitCompileInInvokeFirst => "jit-compile.invoke-first",
        }
    }
}

static PHASE_NS: [AtomicU64; PHASES] = [const { AtomicU64::new(0) }; PHASES];
static PHASE_COUNT: [AtomicU64; PHASES] = [const { AtomicU64::new(0) }; PHASES];

/// A phase clock: `Some(now)` under the census, else `None` (one flag read,
/// no clock).
#[inline]
pub(crate) fn phase_start() -> Option<std::time::Instant> {
    enabled().then(std::time::Instant::now)
}

/// Charge the time since `start` (from [`phase_start`]) to `phase`.
#[inline]
pub(crate) fn phase_end(phase: Phase, start: Option<std::time::Instant>) {
    if let Some(start) = start {
        let ns = start.elapsed().as_nanos() as u64;
        PHASE_NS[phase as usize].fetch_add(ns, Ordering::Relaxed);
        PHASE_COUNT[phase as usize].fetch_add(1, Ordering::Relaxed);
    }
}

/// The census tallies (one lock, census-only, so a lock is affordable):
/// event counts by label; the class-name tally of answers that were not a
/// `DirectMethodHandle` species; and (wave 90) the interpreted frames pushed
/// inside a census scope, by `"<scope> <class>.<method><descriptor>"`, with
/// the total per scope. Wave 94 (lane L4): the TIMED name tally
/// (`"<what>.<scope> <name>"` -> (count, ns): the class initialized or
/// linked, the method re-entered) and the per-body samples
/// ([`BodySample`]).
#[allow(clippy::type_complexity)]
type Tallies = (
    std::collections::BTreeMap<&'static str, u64>,
    std::collections::BTreeMap<String, u64>,
    std::collections::HashMap<String, u64>,
    std::collections::BTreeMap<&'static str, u64>,
    std::collections::BTreeMap<String, (u64, u64)>,
    Vec<BodySample>,
);

/// Wave 94 (lane L4): one finder body's time and the shares of it this
/// thread spent in each kind of work, outermost occurrences only within a
/// kind ([`BodyShare`]). The kinds OVERLAP one another: a class
/// initialization runs `<clinit>` through `interpreter::execute`, which is
/// also a re-entry, and a mutator compile sits inside its JIT door. So the
/// shares are not additive; the dump reads them as "how many bodies paid
/// this kind at all" and "the median body that paid none of the one-time
/// kinds".
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct BodySample {
    /// The body, entry to return (ns).
    pub total: u64,
    /// The per-kind shares (ns), indexed by [`BodyShare`].
    pub share: [u64; BODY_SHARES],
}

/// The kinds of work a [`BodySample`] splits out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum BodyShare {
    /// Class initialization or link (`initialize_class_shared`,
    /// `link_claimed_class`), the outermost one: a link inside an
    /// initialization is inside its time.
    ClassWork = 0,
    /// A JIT mutator-side door (`try_jit_upgrade_with_gate`,
    /// `try_jit_compile_callee*`), the outermost one.
    JitDoor,
    /// `try_jit_compile_callee_slow` on this thread (inside a door).
    JitCompile,
    /// A level-1 Java re-entry (`interpreter::execute` nested in the body).
    Reentry,
    /// `MethodHandleNatives.resolve`.
    Resolve,
}

/// Number of [`BodyShare`]s.
pub(crate) const BODY_SHARES: usize = 5;

/// Bound on the per-body samples kept (the bench rows run a few hundred
/// bodies; past the bound the samples stop and the phases keep counting).
const BODY_SAMPLES_CAP: usize = 1 << 16;

thread_local! {
    /// Wave 94: the shares of the finder body this thread is in, and its
    /// start (`None` outside a body or with the census off).
    static BODY_ACC: std::cell::Cell<(Option<std::time::Instant>, [u64; BODY_SHARES])> =
        const { std::cell::Cell::new((None, [0; BODY_SHARES])) };
    /// Wave 94: how many class works (initialization or link) and JIT doors
    /// timed in a census scope are live on this thread, so only the
    /// outermost one is charged to a [`BodySample`] share.
    static WORK_DEPTH: std::cell::Cell<(u32, u32)> = const { std::cell::Cell::new((0, 0)) };
}

/// Add `ns` to `share` of the finder body this thread is in (no-op outside
/// one).
fn body_share_add(share: BodyShare, ns: u64) {
    BODY_ACC.with(|acc| {
        let (start, mut shares) = acc.get();
        if start.is_some() {
            shares[share as usize] = shares[share as usize].saturating_add(ns);
            acc.set((start, shares));
        }
    });
}

/// Charge `ns` of `MethodHandleNatives.resolve` run inside a finder body to
/// the body's sample (wave 94; census only, called by the resolve wrapper).
pub(crate) fn note_resolve_in_body(ns: u64) {
    body_share_add(BodyShare::Resolve, ns);
}

/// Wave 94 (lane L4): is the census on, for the VM's `invokedynamic` door
/// (`vm/src/runtime/invokedynamic.rs::execute_invokedynamic_at`), which then
/// times each execution and files it through [`note_indy`]? One flag read.
#[inline]
pub fn indy_census_on() -> bool {
    enabled()
}

/// Wave 94: charge the `invokedynamic` execution started at `start` to the
/// timed row `indy <path>` (`path`: `interp shared-hit`, ...; see the VM's
/// `indy_census_path`).
#[cold]
pub fn note_indy(path: &str, start: std::time::Instant) {
    timed_named(format!("indy {path}"), start.elapsed().as_nanos() as u64);
}

/// Interpreter round i5 wave 101 (lane L4): charge one phase of an
/// `invokedynamic` site's native LINK (the VM's slow path,
/// `execute_invokedynamic_at_uncensused`) to the timed row
/// `indy-link <phase>` (`info`, `impl-resolves`, `site-refusal`,
/// `bootstrap-lambda`, `concat-checks`, `concat-link`). The caller holds
/// `start` only while [`indy_census_on`].
#[cold]
pub fn note_indy_link(phase: &str, start: std::time::Instant) {
    timed_named(format!("indy-link {phase}"), start.elapsed().as_nanos() as u64);
}

/// Interpreter round i4 wave 95 (lane L4): charge the span started at `start`
/// to the timed row `<what> <detail>`: the JDK invoker linker's outcomes and
/// sub-phases (`linker invokeExact pair-hit`, `linker invokeExact link-to`,
/// ...) and the lanes that serve ahead of it (`lane field-before-linker`).
/// The caller checks [`enabled`] (it holds `start` only then).
#[cold]
pub(crate) fn note_timed(what: &str, detail: &str, start: std::time::Instant) {
    timed_named(format!("{what} {detail}"), start.elapsed().as_nanos() as u64);
}

/// Add `ns` under `key` to the timed name tally (census only).
fn timed_named(key: String, ns: u64) {
    let mut guard = events().lock();
    if let Some((n, t)) = guard.4.get_mut(&key) {
        *n += 1;
        *t = t.saturating_add(ns);
    } else if guard.4.len() < NAME_CAP {
        guard.4.insert(key, (1, ns));
    }
}

static EVENTS: std::sync::OnceLock<cratonvm_types::lock_order::OrderedPlMutex<Tallies>> =
    std::sync::OnceLock::new();

/// Bound on distinct names in the name tally (1024 since wave 93, which added
/// the `jit-callee-door.*` and `reentry-callee.*` rows beside the re-entry
/// tally).
const NAME_CAP: usize = 1024;

fn events() -> &'static cratonvm_types::lock_order::OrderedPlMutex<Tallies> {
    EVENTS.get_or_init(|| {
        cratonvm_types::lock_order::OrderedPlMutex::new(
            Default::default(),
            cratonvm_types::lock_order::LockLevel::Scratch,
        )
    })
}

/// Count one `label` event. A no-op unless the census is on.
#[inline]
pub(crate) fn event(label: &'static str) {
    if enabled() {
        event_slow(label);
    }
}

#[cold]
fn event_slow(label: &'static str) {
    *events().lock().0.entry(label).or_insert(0) += 1;
}

/// Count one `label` event and tally `name` under it. A no-op unless the
/// census is on.
#[inline]
pub(crate) fn event_named(label: &'static str, name: &str) {
    if enabled() {
        event_named_slow(label, name);
    }
}

#[cold]
fn event_named_slow(label: &'static str, name: &str) {
    let mut guard = events().lock();
    *guard.0.entry(label).or_insert(0) += 1;
    let key = format!("{label} {name}");
    if let Some(n) = guard.1.get_mut(&key) {
        *n += 1;
    } else if guard.1.len() < NAME_CAP {
        guard.1.insert(key, 1);
    }
}

thread_local! {
    /// How many finder bodies this thread is inside (nesting allowed).
    static FINDER_BODY_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    /// How many FIRST `invoke` / `invokeExact` calls this thread is inside
    /// (wave 90; census only, kept by `InvokeClock`).
    static INVOKE_FIRST_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    /// The handles this thread has invoked, for the first / repeat split:
    /// `(vm, collection epoch, addresses)`. Cleared when the epoch moves (an
    /// address may then name another object) and bounded.
    // addr-keyed: VACATED when the collection epoch moves (cleared wholesale); census only.
    static INVOKED: std::cell::RefCell<(usize, u64, std::collections::HashSet<usize>)> =
        std::cell::RefCell::new((0, 0, std::collections::HashSet::new()));
}

/// Bound on the handles remembered per thread for the first / repeat split.
const INVOKED_CAP: usize = 1 << 16;

/// Mark entry to a finder body (census only; pair with [`body_exit`]).
#[inline]
pub(crate) fn body_enter() {
    if enabled() {
        let outermost = FINDER_BODY_DEPTH.with(|d| {
            let v = d.get();
            d.set(v + 1);
            v == 0
        });
        // Wave 94: the outermost body opens a per-body sample.
        if outermost {
            BODY_ACC.with(|acc| acc.set((Some(std::time::Instant::now()), [0; BODY_SHARES])));
        }
    }
}

/// Mark the return of a finder body.
#[inline]
pub(crate) fn body_exit() {
    if enabled() {
        let outermost = FINDER_BODY_DEPTH.with(|d| {
            let v = d.get().saturating_sub(1);
            d.set(v);
            v == 0
        });
        if outermost {
            body_sample_close();
        }
    }
}

/// Close the outermost finder body's sample and file it (wave 94).
#[cold]
fn body_sample_close() {
    let (start, share) = BODY_ACC.with(|acc| acc.replace((None, [0; BODY_SHARES])));
    let Some(start) = start else {
        return;
    };
    let total = start.elapsed().as_nanos() as u64;
    let mut guard = events().lock();
    if guard.5.len() < BODY_SAMPLES_CAP {
        guard.5.push(BodySample { total, share });
    }
}

/// Is this thread inside a finder body?
#[inline]
pub(crate) fn in_finder_body() -> bool {
    FINDER_BODY_DEPTH.with(|d| d.get() != 0)
}

/// Is this the first `invoke` / `invokeExact` this thread makes on the handle
/// at `addr` in VM `vm`, collection epoch `epoch`? Census only.
pub(crate) fn first_invoke(vm: usize, epoch: u64, addr: usize) -> bool {
    INVOKED.with(|cell| {
        let mut guard = cell.borrow_mut();
        let (seen_vm, seen_epoch, set) = &mut *guard;
        if *seen_vm != vm || *seen_epoch != epoch || set.len() >= INVOKED_CAP {
            *seen_vm = vm;
            *seen_epoch = epoch;
            set.clear();
        }
        set.insert(addr)
    })
}

/// The clock of one `MethodHandle.invoke` / `invokeExact` native call: charged
/// to its phase when dropped, so every return of the native is timed. A nested
/// invoke (a handle whose target invokes another) is inside its caller's time.
pub(crate) struct InvokeClock {
    start: std::time::Instant,
    phase: Phase,
    /// A first call: `INVOKE_FIRST_DEPTH` was raised for it.
    first: bool,
}

impl Drop for InvokeClock {
    fn drop(&mut self) {
        phase_end(self.phase, Some(self.start));
        if self.first {
            INVOKE_FIRST_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
        }
    }
}

thread_local! {
    /// Interpreter round i5 wave 110 (lane L4): the innermost NAMED census
    /// scope this thread is in ([`named_scope`]): `"iwa-body"` (the JDK body
    /// of `MethodHandle.invokeWithArguments` under `CRATONVM_MH_IWA_JDK_BODY`)
    /// and `"shape-mint"` (the method finder shape mint). Census only: never
    /// set with the census off.
    static NAMED_SCOPE: std::cell::Cell<Option<&'static str>> =
        const { std::cell::Cell::new(None) };
}

/// A named census scope, open until dropped ([`named_scope`]).
pub(crate) struct NamedScope {
    prev: Option<&'static str>,
    label: &'static str,
    start: std::time::Instant,
}

impl Drop for NamedScope {
    fn drop(&mut self) {
        NAMED_SCOPE.with(|s| s.set(self.prev));
        note_timed("scope", self.label, self.start);
    }
}

/// Open the named census scope `label` (wave 110, lane L4): until the guard
/// drops, [`scope_label`] answers `label` (a finder body still wins), so the
/// per-method rows the VM files inside a scope (`reentry-time.<label>`,
/// `class-init.<label>` / `class-link.<label>`, `interp-frames <label>` with
/// `CRATONVM_DBG_INTERP_FRAMES=1`) name it, and the scope's own time is the
/// `timed` row `scope <label>`. The aggregated phase rows count it with
/// `invoke-first`. `None`, after one flag read, with the census off.
#[inline]
pub(crate) fn named_scope(label: &'static str) -> Option<NamedScope> {
    if !enabled() {
        return None;
    }
    let prev = NAMED_SCOPE.with(|s| s.replace(Some(label)));
    Some(NamedScope {
        prev,
        label,
        start: std::time::Instant::now(),
    })
}

/// The census scope this thread is in: `"finder-body"` inside a finder
/// body, else `"invoke-first"` inside a first `invoke` / `invokeExact`,
/// else `None` (and always `None` with the census off). Wave 90.
pub fn scope_label() -> Option<&'static str> {
    if !enabled() {
        return None;
    }
    if in_finder_body() {
        Some("finder-body")
    } else if let Some(named) = NAMED_SCOPE.with(|s| s.get()) {
        Some(named)
    } else if INVOKE_FIRST_DEPTH.with(|d| d.get() != 0) {
        Some("invoke-first")
    } else {
        None
    }
}

/// The VM-side work a [`WorkClock`] times (wave 90).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Work {
    /// `initialize_class_shared` (link and `<clinit>` included).
    ClassInit,
    /// `link_claimed_class`.
    ClassLink,
    /// Wave 91: a JIT mutator-side compile door (`try_jit_upgrade_with_gate`,
    /// `try_jit_compile_callee*`). Timed only inside a census scope.
    JitDoor,
    /// Wave 91: `try_jit_compile_callee_slow` on a mutator thread. Timed only
    /// inside a census scope (a background worker has no scope).
    JitCompile,
}

/// The clock of one [`Work`] item, charged on drop to the phase of the scope
/// it STARTED in. A nested item (a superclass's initialization inside a
/// class's) is inside its outer item's time too.
pub struct WorkClock {
    start: std::time::Instant,
    phase: Phase,
    /// Wave 94: the timed-name key (`"class-init.<scope> <class>"`, ...),
    /// for a class work inside a census scope whose caller named the class.
    name: Option<String>,
    /// Wave 94: the per-body share this item is charged to (the outermost
    /// item of its kind inside a finder body), if any.
    share: Option<BodyShare>,
    /// Wave 94: which [`WORK_DEPTH`] counter this item raised (`Some(true)`
    /// class work, `Some(false)` JIT door), to lower at the drop.
    depth: Option<bool>,
}

impl Drop for WorkClock {
    fn drop(&mut self) {
        let ns = self.start.elapsed().as_nanos() as u64;
        PHASE_NS[self.phase as usize].fetch_add(ns, Ordering::Relaxed);
        PHASE_COUNT[self.phase as usize].fetch_add(1, Ordering::Relaxed);
        if let Some(class_work) = self.depth {
            WORK_DEPTH.with(|d| {
                let (c, j) = d.get();
                d.set(if class_work {
                    (c.saturating_sub(1), j)
                } else {
                    (c, j.saturating_sub(1))
                });
            });
        }
        if let Some(share) = self.share {
            body_share_add(share, ns);
        }
        if let Some(key) = self.name.take() {
            timed_named(key, ns);
        }
    }
}

/// [`work_clock`] for a class initialization or link whose class the caller
/// can name: inside a census scope the item is also tallied, with its time,
/// under `class-init.<scope> <class>` / `class-link.<scope> <class>`
/// (`[mh-finder-census] timed` rows). `name` runs only then (it may take the
/// class-manager read lock, so the caller must hold no class-manager lock).
/// Wave 94 (lane L4): which classes a finder body or a first invoke
/// initializes, and whether each is a one-time JDK class (count 1) or a
/// per-body one (count near the body count).
#[inline]
pub fn work_clock_named(work: Work, name: impl FnOnce() -> Option<String>) -> Option<WorkClock> {
    let mut clock = work_clock(work)?;
    let what = match work {
        Work::ClassInit => "class-init",
        Work::ClassLink => "class-link",
        Work::JitDoor | Work::JitCompile => return Some(clock),
    };
    if let Some(scope) = scope_label() {
        if let Some(class) = name() {
            clock.name = Some(format!("{what}.{scope} {class}"));
        }
    }
    Some(clock)
}

/// Start timing `work` (`None`, one flag read, unless the census is on).
/// Called from the VM's class initialization and link paths.
#[inline]
pub fn work_clock(work: Work) -> Option<WorkClock> {
    if !enabled() {
        return None;
    }
    work_clock_slow(work)
}

#[cold]
fn work_clock_slow(work: Work) -> Option<WorkClock> {
    let scope = scope_label();
    let phase = match (work, scope) {
        (Work::ClassInit, Some("finder-body")) => Phase::ClassInitInFinderBody,
        (Work::ClassInit, Some(_)) => Phase::ClassInitInInvokeFirst,
        (Work::ClassInit, None) => Phase::ClassInitElsewhere,
        (Work::ClassLink, Some("finder-body")) => Phase::ClassLinkInFinderBody,
        (Work::ClassLink, Some(_)) => Phase::ClassLinkInInvokeFirst,
        (Work::ClassLink, None) => Phase::ClassLinkElsewhere,
        (Work::JitDoor, Some("finder-body")) => Phase::JitDoorInFinderBody,
        (Work::JitDoor, Some(_)) => Phase::JitDoorInInvokeFirst,
        (Work::JitCompile, Some("finder-body")) => Phase::JitCompileInFinderBody,
        (Work::JitCompile, Some(_)) => Phase::JitCompileInInvokeFirst,
        (Work::JitDoor | Work::JitCompile, None) => return None,
    };
    // Wave 94: the per-body share and the outermost-of-its-kind depth, for
    // items inside a census scope only (the depth is raised in any scope, so
    // an invoke-first item nested in nothing still counts as outermost).
    let (share, depth) = match (work, scope) {
        (_, None) => (None, None),
        (Work::ClassInit | Work::ClassLink, Some(scope)) => {
            let outer = WORK_DEPTH.with(|d| {
                let (c, j) = d.get();
                d.set((c + 1, j));
                c == 0
            });
            ((outer && scope == "finder-body").then_some(BodyShare::ClassWork), Some(true))
        }
        (Work::JitDoor, Some(scope)) => {
            let outer = WORK_DEPTH.with(|d| {
                let (c, j) = d.get();
                d.set((c, j + 1));
                j == 0
            });
            ((outer && scope == "finder-body").then_some(BodyShare::JitDoor), Some(false))
        }
        (Work::JitCompile, Some(scope)) => {
            ((scope == "finder-body").then_some(BodyShare::JitCompile), None)
        }
    };
    Some(WorkClock {
        start: std::time::Instant::now(),
        phase,
        name: None,
        share,
        depth,
    })
}

/// Wave 93 (lane L7): tally one JIT callee-door outcome inside a census scope,
/// by method, under `jit-callee-door.<scope>`: `compile-on-mutator` (the door
/// compiled in place, `try_jit_compile_callee_slow` on this thread),
/// `deferred-queued` / `deferred-not-queued` (the door handed the compile to
/// the worker, `CRATONVM_JIT_CALLEE_DOOR_DEFERS_TO_WORKER`, and the manager
/// queued it or refused it). A no-op, after one flag read, unless the census
/// is on and this thread is in a scope (a background worker never is).
pub fn note_jit_callee_door(outcome: &str, class: &str, method: &str, descriptor: &str) {
    if !enabled() {
        return;
    }
    let label = match scope_label() {
        Some("finder-body") => "jit-callee-door.finder-body",
        Some("iwa-body") => "jit-callee-door.iwa-body",
        Some("shape-mint") => "jit-callee-door.shape-mint",
        Some(_) => "jit-callee-door.invoke-first",
        None => return,
    };
    event_named(label, &format!("{outcome} {class}.{method}{descriptor}"));
}

/// Wave 93 (lane L7): is the `interpreter::execute` whose [`ExecuteClock`] was
/// just started a Java RE-ENTRY of a census scope (level 1 or deeper)? `false`
/// with the census off or outside a scope. The VM asks it to classify the
/// re-entered callee ([`note_reentry_callee_state`]) only then.
pub fn in_reentry_now() -> bool {
    enabled() && scope_label().is_some() && SCOPE_EXEC_DEPTH.with(|d| d.get() >= 2)
}

/// Wave 93 (lane L7): tally the JIT state of a re-entered callee, by method,
/// under `reentry-callee.<scope>`. The VM's states: `published` (a method-entry
/// body is published under the callee's own key, so the compiled caller
/// re-entered although compiled code exists: it holds no binding to it, or a
/// binding to a retired body), `bail-listed`, `hot-unpublished` (invoked at
/// least the tier-up threshold times, but no body: retired and not compiled
/// again yet, or queued), `cold` (below the threshold: never compiled). Asked
/// only when [`in_reentry_now`] answered `true`.
pub fn note_reentry_callee_state(state: &str, class: &str, method: &str, descriptor: &str) {
    let label = match scope_label() {
        Some("finder-body") => "reentry-callee.finder-body",
        Some("iwa-body") => "reentry-callee.iwa-body",
        Some("shape-mint") => "reentry-callee.shape-mint",
        Some(_) => "reentry-callee.invoke-first",
        None => return,
    };
    event_named(label, &format!("{state} {class}.{method}{descriptor}"));
}

thread_local! {
    /// Wave 91: how many `interpreter::execute` calls that started inside a
    /// census scope are live on this thread. Level 0 is the scope's own entry
    /// (a finder body's `findStatic`, ...); 1 and up are Java re-entries.
    /// Census only: [`execute_clock`] returns `None` without a scope, so this
    /// stays 0 when the census is off.
    static SCOPE_EXEC_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// The clock of one `interpreter::execute` call inside a census scope (wave
/// 91). Dropped at `execute`'s return: lowers the depth and, for an outermost
/// re-entry, charges its time.
pub struct ExecuteClock {
    start: std::time::Instant,
    /// `Some` for an outermost re-entry (level 1), the phase it is charged to.
    phase: Option<Phase>,
    /// Wave 94: an outermost re-entry's timed-name key
    /// (`"reentry-time.<scope> <method><descriptor>"`).
    name: Option<String>,
    /// Interpreter round i5 wave 114, lane L7: the scope of a re-entry at ANY
    /// level (1 and deeper; `None` for a scope's own level-0 entry), for the
    /// `reentry-prologue.<scope>` / `reentry-epilogue.<scope>` split of
    /// `execute`'s own time around the callee's body ([`Self::mark_body`],
    /// [`Self::mark_returned`]). The body excludes nested re-entries' own
    /// prologues only through their own rows, so the sum of the two rows over
    /// every re-entry is `execute`'s self time, never double counted.
    reentry_scope: Option<&'static str>,
    /// When the callee's body (compiled or interpreted) started.
    body_at: std::cell::Cell<Option<std::time::Instant>>,
    /// When it returned to `execute`.
    returned_at: std::cell::Cell<Option<std::time::Instant>>,
    /// Census bookkeeping inside the prologue ([`Self::excluding`]), not
    /// charged to it.
    excluded_ns: std::cell::Cell<u64>,
}

impl ExecuteClock {
    /// Wave 114 (lane L7): the callee's body starts now (the first mark
    /// counts; a re-run from entry keeps the first).
    pub fn mark_body(&self) {
        if self.reentry_scope.is_some() && self.body_at.get().is_none() {
            self.body_at.set(Some(std::time::Instant::now()));
        }
    }

    /// Wave 114 (lane L7): the callee's body returned to `execute` now (the
    /// last mark counts).
    pub fn mark_returned(&self) {
        if self.reentry_scope.is_some() {
            self.returned_at.set(Some(std::time::Instant::now()));
        }
    }

    /// Wave 114 (lane L7): run census bookkeeping `f` inside the prologue
    /// without charging its time to it.
    pub fn excluding<R>(&self, f: impl FnOnce() -> R) -> R {
        let at = std::time::Instant::now();
        let result = f();
        let ns = at.elapsed().as_nanos() as u64;
        self.excluded_ns.set(self.excluded_ns.get().saturating_add(ns));
        result
    }
}

impl Drop for ExecuteClock {
    fn drop(&mut self) {
        SCOPE_EXEC_DEPTH.with(|d| d.set(d.get().saturating_sub(1)));
        // Read once, before any bookkeeping below, so the wave-114 rows'
        // tally does not lengthen the level-1 phase time.
        let now = std::time::Instant::now();
        // Wave 114 (lane L7): `execute`'s self time around the body. A
        // re-entry that never reached a body (an early error, the no-`Code`
        // rescue) is charged whole to `reentry-nobody.<scope>`.
        if let Some(scope) = self.reentry_scope {
            let excluded = self.excluded_ns.get();
            match self.body_at.get() {
                Some(body_at) => {
                    let prologue = (body_at.saturating_duration_since(self.start).as_nanos()
                        as u64)
                        .saturating_sub(excluded);
                    let epilogue = self.returned_at.get().map_or(0, |returned_at| {
                        now.saturating_duration_since(returned_at).as_nanos() as u64
                    });
                    timed_named(format!("reentry-prologue.{scope}"), prologue);
                    timed_named(format!("reentry-epilogue.{scope}"), epilogue);
                }
                None => {
                    let whole = (now.saturating_duration_since(self.start).as_nanos() as u64)
                        .saturating_sub(excluded);
                    timed_named(format!("reentry-nobody.{scope}"), whole);
                }
            }
        }
        if let Some(phase) = self.phase {
            let ns = now.saturating_duration_since(self.start).as_nanos() as u64;
            PHASE_NS[phase as usize].fetch_add(ns, Ordering::Relaxed);
            PHASE_COUNT[phase as usize].fetch_add(1, Ordering::Relaxed);
            if phase == Phase::ReentryInFinderBody {
                body_share_add(BodyShare::Reentry, ns);
            }
            if let Some(key) = self.name.take() {
                timed_named(key, ns);
            }
        }
    }
}

/// Start the clock of an `interpreter::execute` of `method` + `descriptor`
/// (wave 91). `None`, after one flag read, unless the census is on and this
/// thread is in a census scope. Every re-entry (level 1 and deeper) is
/// tallied by method under `reentry.<scope>`; level-0 entries under
/// `execute.level0.<scope>`; only level-1 re-entries are timed, so the phase
/// total has no double counting.
#[inline]
pub fn execute_clock(method: &str, descriptor: &str) -> Option<ExecuteClock> {
    if !enabled() {
        return None;
    }
    execute_clock_slow(method, descriptor)
}

#[cold]
fn execute_clock_slow(method: &str, descriptor: &str) -> Option<ExecuteClock> {
    let scope = scope_label()?;
    let level = SCOPE_EXEC_DEPTH.with(|d| {
        let v = d.get();
        d.set(v + 1);
        v
    });
    let finder = scope == "finder-body";
    let name = format!("{method}{descriptor}");
    let phase = match level {
        0 => {
            event_named(
                match scope {
                    "finder-body" => "execute.level0.finder-body",
                    "iwa-body" => "execute.level0.iwa-body",
                    "shape-mint" => "execute.level0.shape-mint",
                    _ => "execute.level0.invoke-first",
                },
                &name,
            );
            None
        }
        _ => {
            event_named(
                match scope {
                    "finder-body" => "reentry.finder-body",
                    "iwa-body" => "reentry.iwa-body",
                    "shape-mint" => "reentry.shape-mint",
                    _ => "reentry.invoke-first",
                },
                &name,
            );
            (level == 1).then_some(if finder {
                Phase::ReentryInFinderBody
            } else {
                Phase::ReentryInInvokeFirst
            })
        }
    };
    // Wave 94: an outermost re-entry is also timed by method.
    let timed = phase.map(|_| format!("reentry-time.{scope} {name}"));
    Some(ExecuteClock {
        start: std::time::Instant::now(),
        phase,
        name: timed,
        reentry_scope: (level >= 1).then_some(scope),
        body_at: std::cell::Cell::new(None),
        returned_at: std::cell::Cell::new(None),
        excluded_ns: std::cell::Cell::new(0),
    })
}

/// Is this thread inside a Java re-entry of a census scope (level 1 or
/// deeper)? Census only (wave 91).
fn in_scope_reentry() -> bool {
    SCOPE_EXEC_DEPTH.with(|d| d.get() >= 2)
}

/// Bound on distinct rows in the scoped frame tally (counting continues in
/// the per-scope totals past it).
const SCOPED_FRAMES_CAP: usize = 8192;

/// Record one interpreted frame push when this thread is in a census scope
/// (wave 90). Called from the VM's `CRATONVM_DBG_INTERP_FRAMES` census
/// (`vm/src/runtime/interp_census.rs::record_interp_frame`), so it costs
/// nothing unless BOTH that census and this one are on.
#[cold]
pub fn note_interp_frame(class: &str, method: &str, descriptor: &str) {
    let Some(scope) = scope_label() else {
        return;
    };
    let key = format!("{scope} {class}.{method}{descriptor}");
    let mut guard = events().lock();
    *guard.3.entry(scope).or_insert(0) += 1;
    // Wave 91: the share of the scope's frames pushed inside a Java re-entry
    // (a nested `execute`), against those of the scope's own interpreter loop.
    if in_scope_reentry() {
        let label = if scope == "finder-body" {
            "finder-body/in-reentry"
        } else {
            "invoke-first/in-reentry"
        };
        *guard.3.entry(label).or_insert(0) += 1;
    }
    if let Some(n) = guard.2.get_mut(&key) {
        *n += 1;
    } else if guard.2.len() < SCOPED_FRAMES_CAP {
        guard.2.insert(key, 1);
    }
}

/// Rows of the scoped interpreted-frame tally printed per scope.
const SCOPED_FRAMES_TOP: usize = 80;

/// Print the scoped interpreted-frame tally (the caller holds the lock).
fn dump_scoped_frames(guard: &Tallies) {
    for (scope, total) in &guard.3 {
        eprintln!("[mh-finder-census] interp-frames {scope} total {total}");
        let prefix = format!("{scope} ");
        let mut rows: Vec<(&String, &u64)> =
            guard.2.iter().filter(|(k, _)| k.starts_with(&prefix)).collect();
        rows.sort_by(|a, b| b.1.cmp(a.1).then(a.0.cmp(b.0)));
        for (k, n) in rows.into_iter().take(SCOPED_FRAMES_TOP) {
            eprintln!("[mh-finder-census] frames {n:>9} {k}");
        }
    }
}

/// Start the clock of an `invoke` (`exact == false`) or `invokeExact` call on
/// the handle at `addr`, of class `class_name`; `None` unless the census is
/// on. A first call also tallies the handle's class name.
pub(crate) fn invoke_clock(
    vm: usize,
    epoch: u64,
    addr: usize,
    exact: bool,
    class_name: Option<&str>,
) -> Option<InvokeClock> {
    if !enabled() {
        return None;
    }
    let first = first_invoke(vm, epoch, addr);
    let phase = match (exact, first) {
        (false, true) => Phase::InvokeFirst,
        (false, false) => Phase::InvokeRepeat,
        (true, true) => Phase::InvokeExactFirst,
        (true, false) => Phase::InvokeExactRepeat,
    };
    if first {
        event_named(
            if exact { "invokeExact.first.handle-class" } else { "invoke.first.handle-class" },
            class_name.unwrap_or("?"),
        );
        INVOKE_FIRST_DEPTH.with(|d| d.set(d.get() + 1));
    }
    Some(InvokeClock {
        start: std::time::Instant::now(),
        phase,
        first,
    })
}

/// Dump the phase times and the event tallies. No-op unless
/// `CRATONVM_DBG_MH_FINDER_CENSUS` is set.
pub fn dump() {
    if !enabled() {
        return;
    }
    eprintln!("[mh-finder-census] {:<23}{:>6}{:>14}{:>10}", "phase", "count", "total-us", "mean-ns");
    for phase in Phase::ALL {
        let ns = PHASE_NS[phase as usize].load(Ordering::Relaxed);
        let n = PHASE_COUNT[phase as usize].load(Ordering::Relaxed);
        let mean = if n == 0 { 0 } else { ns / n };
        eprintln!(
            "[mh-finder-census] {:<20} {:>8} {:>12} {:>9}",
            phase.label(),
            n,
            ns / 1000,
            mean
        );
    }
    let Some(cell) = EVENTS.get() else {
        eprintln!("[mh-finder-census] no events");
        return;
    };
    let guard = cell.lock();
    for (label, n) in &guard.0 {
        eprintln!("[mh-finder-census] event {n:>9} {label}");
    }
    for (name, n) in &guard.1 {
        eprintln!("[mh-finder-census] name  {n:>9} {name}");
    }
    // Wave 94: the timed name tally, `count total-us mean-ns key`.
    for (key, (n, ns)) in &guard.4 {
        let mean = if *n == 0 { 0 } else { ns / n };
        eprintln!("[mh-finder-census] timed {n:>9} {:>12} {mean:>10} {key}", ns / 1000);
    }
    dump_body_samples(&guard.5);
    dump_scoped_frames(&guard);
}

/// The `q`-quantile (0..=1) of `xs`, which must be sorted; 0 when empty.
fn quantile(xs: &[u64], q: f64) -> u64 {
    if xs.is_empty() {
        return 0;
    }
    let i = ((xs.len() - 1) as f64 * q).round() as usize;
    xs[i.min(xs.len() - 1)]
}

/// The median of one column of `samples` (0 when empty).
fn median_of(samples: &[&BodySample], column: impl Fn(&BodySample) -> u64) -> u64 {
    let mut xs: Vec<u64> = samples.iter().map(|s| column(*s)).collect();
    xs.sort_unstable();
    quantile(&xs, 0.5)
}

/// Print the per-body distribution (wave 94). The phase table's means mix
/// one-time costs (a JDK class's first initialization, a compile on the
/// mutator) into every body; the bench rows report MEDIANS. These rows say
/// which shares the median body pays:
///
/// * `body-dist all`: bodies, median and p90 of the body time, and how many
///   bodies paid any class work, any mutator compile, any JIT door, any
///   re-entry;
/// * `body-dist share-median`: the median of each share over all bodies;
/// * `body-dist clean`: the bodies that paid no class work and no mutator
///   compile, with their median time and the medians of their re-entry,
///   JIT-door and resolve shares. That median is the per-body cost a fresh
///   class pays in steady state.
fn dump_body_samples(samples: &[BodySample]) {
    if samples.is_empty() {
        return;
    }
    let all: Vec<&BodySample> = samples.iter().collect();
    let mut totals: Vec<u64> = all.iter().map(|s| s.total).collect();
    totals.sort_unstable();
    let paid = |share: BodyShare| all.iter().filter(|s| s.share[share as usize] != 0).count();
    eprintln!(
        "[mh-finder-census] body-dist all bodies {} median-ns {} p90-ns {} with-class-work {} \
         with-jit-compile {} with-jit-door {} with-reentry {}",
        all.len(),
        quantile(&totals, 0.5),
        quantile(&totals, 0.9),
        paid(BodyShare::ClassWork),
        paid(BodyShare::JitCompile),
        paid(BodyShare::JitDoor),
        paid(BodyShare::Reentry),
    );
    let share = |s: BodyShare| median_of(&all, move |b| b.share[s as usize]);
    eprintln!(
        "[mh-finder-census] body-dist share-median class-work {} jit-door {} jit-compile {} \
         reentry {} resolve {}",
        share(BodyShare::ClassWork),
        share(BodyShare::JitDoor),
        share(BodyShare::JitCompile),
        share(BodyShare::Reentry),
        share(BodyShare::Resolve),
    );
    let clean: Vec<&BodySample> = all
        .iter()
        .copied()
        .filter(|s| {
            s.share[BodyShare::ClassWork as usize] == 0 && s.share[BodyShare::JitCompile as usize] == 0
        })
        .collect();
    eprintln!(
        "[mh-finder-census] body-dist clean bodies {} median-ns {} reentry {} jit-door {} \
         resolve {}",
        clean.len(),
        median_of(&clean, |b| b.total),
        median_of(&clean, |b| b.share[BodyShare::Reentry as usize]),
        median_of(&clean, |b| b.share[BodyShare::JitDoor as usize]),
        median_of(&clean, |b| b.share[BodyShare::Resolve as usize]),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The first / repeat split: the first call on an address is first, a
    /// second is not, and a moved epoch forgets.
    #[test]
    fn first_invoke_splits_by_address_and_epoch() {
        assert!(first_invoke(7, 1, 0x1000));
        assert!(!first_invoke(7, 1, 0x1000));
        assert!(first_invoke(7, 1, 0x2000));
        assert!(first_invoke(7, 2, 0x1000));
        assert!(first_invoke(8, 2, 0x1000));
    }

    /// Wave 94: the dump's quantile reads a sorted sample.
    #[test]
    fn quantiles_of_sorted_samples() {
        assert_eq!(quantile(&[], 0.5), 0);
        assert_eq!(quantile(&[1, 2, 3], 0.5), 2);
        assert_eq!(quantile(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10], 0.9), 9);
        let a = BodySample { total: 30, share: [0; BODY_SHARES] };
        let b = BodySample { total: 10, share: [0; BODY_SHARES] };
        let c = BodySample { total: 20, share: [0; BODY_SHARES] };
        assert_eq!(median_of(&[&a, &b, &c], |s| s.total), 20);
    }

    /// Wave 94: a body share is charged only while a body sample is open.
    #[test]
    fn a_body_share_is_charged_only_inside_an_open_sample() {
        body_share_add(BodyShare::Reentry, 5);
        assert_eq!(BODY_ACC.with(|a| a.get().1), [0; BODY_SHARES]);
        BODY_ACC.with(|a| a.set((Some(std::time::Instant::now()), [0; BODY_SHARES])));
        body_share_add(BodyShare::Reentry, 5);
        body_share_add(BodyShare::Resolve, 7);
        let (_, shares) = BODY_ACC.with(|a| a.replace((None, [0; BODY_SHARES])));
        assert_eq!(shares[BodyShare::Reentry as usize], 5);
        assert_eq!(shares[BodyShare::Resolve as usize], 7);
        assert_eq!(shares[BodyShare::ClassWork as usize], 0);
    }

    /// Every phase has a distinct slot and a label.
    #[test]
    fn phases_are_dense_and_labelled() {
        for (i, phase) in Phase::ALL.iter().enumerate() {
            assert_eq!(*phase as usize, i);
            assert!(!phase.label().trim().is_empty());
        }
    }
}
