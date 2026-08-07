// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

//! STUB-RATCHET GATE — a one-way compatibility ratchet on synthetic-stub natives.
//!
//! Project rule (see `docs/synthetic-vs-real-explained.md` and
//! `feedback_no_synthetic_stubs`): **no NEW synthetic stubs**. The native
//! overlay tags every registration with a [`NativeKind`]:
//!
//!   * `Intrinsic` — a correct fast-path for a hot method (kept forever).
//!   * `Bridge`    — a native the VM genuinely needs (OS syscalls, `sun.*`
//!                   internals, classes with no real bytecode). It *is* the
//!                   real behavior.
//!   * `SyntheticStub` — a fake: placeholder / approximate / wrong return
//!                   values, fabricated objects, or "fake main" launcher
//!                   short-circuits. These shadow correct real bytecode and
//!                   are the removal target.
//!
//! This test builds the **default native registry the way the VM does** — all
//! six registration passes `vm/src/vm/vm_init.rs` runs on the real-JDK boot
//! path, in its order, behind its `set_drop_real_layout_synthetic` flag (see
//! [`register_boot_path`]) — censuses how many registrations are tagged
//! `SyntheticStub`, and asserts the count has not RISEN above a frozen
//! [`BASELINE_SYNTHETIC_STUBS`] constant.
//!
//! Until 2026-08-05 it ran `register_essential_natives` and nothing else, under
//! that same claim, and so measured about four fifths of the registry. The
//! number it reported was 165 where the boot registry holds 549.
//!
//! It is a *ratchet*: a change that ADDS a synthetic stub pushes the count over
//! the baseline and fails CI; a change that REMOVES one is welcome and only
//! requires lowering the baseline (see `stub-ratchet.md`).
//!
//! Wire into CI with:
//!
//! ```text
//! cargo test -p cratonvm-native-builtins --test stub_ratchet
//! ```

use cratonvm_native_api::{NativeKind, NativeMethodRegistry};
use cratonvm_native_builtins::register_essential_natives;
use cratonvm_types::compat::CompatibilityMode;

/// Frozen upper bound on the number of `SyntheticStub`-tagged registrations in
/// the default (real-JDK) **boot** registry — all six passes, not just
/// `register_essential_natives`. See [`register_boot_path`].
///
/// This is the exact current observed count. The ratchet has zero slack: adding
/// one synthetic stub fails, while removing one requires lowering the baseline
/// in the same change to lock in the improvement.
///
/// ## How to (re)compute the baseline
///
/// The exact count is produced at runtime by this very test. Run it once and
/// read the observed value off the assertion / stdout line:
///
/// ```text
/// cargo test -p cratonvm-native-builtins --test stub_ratchet -- --nocapture
/// ```
///
/// The test prints
///
/// ```text
/// stub-ratchet: <N> SyntheticStub registrations (baseline <BASELINE>)
/// ```
///
/// Set this constant to `<N>` and keep [`SLACK`] at zero. See
/// `docs/contributing/stub-ratchet.md`.
///
/// # 157 → 165, 2026-08-05 (JDK-only wave 2, lane L7 item 4)
///
/// The ratchet moved **up**, and the explanation its own failure message asks
/// for is that this change added no fake: it re-labelled eight that were
/// already there and were hidden from this count by the wrong tag.
///
/// The eight are `java/util/function/Function.{identity,compose,andThen}`,
/// `UnaryOperator.identity`, and `Function$Identity.{apply,andThen,compose}`.
/// Every one of them fabricates a `Function$Identity` / `Function$AndThen` /
/// `Function$Compose` stand-in, and **no JDK declares any of those names** —
/// real `Function.identity()` is one line of invokedynamic returning `t -> t`,
/// and `compose`/`andThen` are default methods that return a lambda. They were
/// tagged `Bridge`, which asserts "no working real-bytecode fallback exists".
/// There is one, in `java.base`, and `--jdk-only` now runs it: the
/// `JdkOnlyBreadthProbe` `lambdas` line is byte-identical to HotSpot 25.
///
/// So the count rising is this gate becoming *more* honest, not less: the
/// backlog it exists to measure was under-reported by eight. The direction to
/// be suspicious of is a `SyntheticStub` quietly becoming a `Bridge`, which
/// lowers this number while changing nothing — and which is exactly the shape
/// L6's unadjudicated-`Bridge` ratchet is being built to catch.
///
/// # 165 → 549, 2026-08-05 — a scope fix, not 384 new stubs
///
/// **Nothing was added.** The census stopped measuring `register_essential_natives`
/// alone and started running the boot sequence the VM actually installs, which
/// is 2,279 registrations wider. The 384 extra `SyntheticStub` rows were always
/// in the shipped registry; this gate simply could not see them.
///
/// The bug was found by disagreement, which is the useful part: L7's retag moved
/// 364 registrations `Bridge` → `SyntheticStub`, L6's `bridge-ratchet.sh`
/// counted every one (it censuses a *running VM*), and this gate did not move by
/// a single row. Two ratchets over one VM, 364 apart. Whenever two gates over
/// the same object disagree, at least one is measuring the wrong object.
///
/// **This is the number to compare against `bridge-ratchet.sh` from now on.**
/// If they diverge again, suspect the scope of one of them before suspecting
/// the code.
///
/// # 549 -> 554, 2026-08-05 (catching up with `14145d874`)
///
/// Five, and the change that made them is explicit about why:
/// `fix(jdk-only): the five duplicate registrations that outlived L7 R1's
/// retag` moved `ArrayList.subList`, `Collections.unmodifiableList` and
/// `{List,Set,Map}.copyOf` from the ambient `Bridge` to `SyntheticStub`, so
/// `--jdk-only` drops them and `java.base`'s own bytecode runs. It re-froze
/// L6's Bridge ratchet for the same five (9,705 -> 9,697) and did not re-freeze
/// this one, so `dev` has been failing this gate since — verified by running it
/// on a pristine `5efaa521c`, not inferred.
///
/// The direction is the same as the 157 -> 165 move above and wants the same
/// reading: **no new fake was added.** Five registrations that were mis-tagged
/// `Bridge` are now counted where they always belonged, and the real
/// improvement is on the other gate, which fell by eight.
///
/// # 554 -> 553, 2026-08-05 (a stub deleted, not re-tagged)
///
/// `java.util.Map.entry(K, V)` is no longer registered. A differential run
/// against HotSpot 25 (`probes/ShadowDifferentialProbe.java`) showed the stub
/// answering `java.util.Map$Entry@6c` where the JDK answers `k=7`, and
/// accepting a `setValue` the JDK's immutable `KeyValueHolder` refuses. The
/// method is ordinary bytecode in `java.base`, so deleting the registration is
/// the whole fix. This is the direction the gate exists to encourage: one
/// fewer fake, and the count follows.
///
/// # 553 -> 632, 2026-08-06 (lies removed from the census, not fakes added)
///
/// **No new synthetic stub exists, and no slot the VM dispatches changed kind.**
/// Measured, not asserted: two `--dump-native-registry` censuses from a real
/// JDK 25 boot, before and after, agree on the effective kind of all **10,639**
/// registered triples — zero differences (`scripts/jdk-only-kind-map.py`, and
/// a last-write-wins fold over both files).
///
/// This gate counts *registrations*, superseded rows included, and that is
/// where the 79 moved. Nine registrars had no `set_category` scope over them at
/// all, so each of their callers imposed its own — and registration is
/// last-write-wins, so what shipped was decided by call ORDER while the
/// superseded rows were left claiming a kind nothing ever used.
///
///  * `register_stamped_lock_natives` is called three times. Two callers had
///    `Bridge` in effect and one had none: 62 rows here (`StampedLock` and its
///    two view classes).
///  * `register_service_loader_natives`, `register_url_codec`,
///    `register_p59_bulk_stream_transfer` and the netty/tomcat/slf4j shim
///    registrars: the remaining 17.
///
/// All nine state `SyntheticStub` now, which is what their surviving row always
/// was and what the classes deserve — `java.util.ServiceLoader`,
/// `java.net.URLEncoder`/`URLDecoder`, `java.io.InputStream.transferTo` and
/// `java.util.concurrent.locks.StampedLock` are ordinary bytecode in
/// `java.base` and JDK 25 declares no `ACC_NATIVE` method on any of them, so
/// contract §1.5 cannot call them bridges.
///
/// **`--jdk-only` changed, and this is the fix, not the fallout.** A strict
/// census A/B shows 48 triples that strict mode used to ADMIT and now refuses,
/// and none in the other direction. It admitted them because the drop happens
/// at registration: the later `SyntheticStub` row was refused at the door and
/// the earlier `Bridge` row therefore survived to own the slot. Contract §11's
/// "zero synthetic-stub invocations through any path" was false for 48 triples,
/// by registration order, invisibly. See
/// the retired `native-kind-is-ambient-and-defaults-to-syntheticstub` write-up.
/// # 632 -> 642, 2026-08-06 (ten fakes `--jdk-only` was keeping, by file order)
///
/// The same shape as the re-freeze above, found by asking the question the
/// other way round: which triples does `--real-jdk` call a stub while
/// `--jdk-only` registers something? Twenty did, because a **second file**
/// registers the same triple under a `Bridge` scope, and strict mode refuses
/// the stub at the door — so the copy that survived to own the slot was the
/// one that was not a stub. Ten are now stated `SyntheticStub` at their second
/// site:
///
///  * `Function$Identity.apply` and `UnaryOperator.identity` in
///    `phases_late/streams.rs`. L7 item 4 retagged the `lib.rs` cluster; these
///    two copies kept `--jdk-only` minting a `Function$Identity` /
///    `UnaryOperator$Identity` whose class contract §5 forbids fabricating.
///    That is the "successor defect" the ambient-category record names, still
///    live in a file that lane did not open.
///  * five `CountDownLatch` and two `CyclicBarrier` registrations in a
///    surefire bootstrap block — the same callbacks `util_concurrent_ext`
///    installs and states as stubs.
///  * `java.io.InputStream.transferTo` in `native-io`, whose other copy in
///    `phases_late/zip_streams.rs` is a stub.
///
/// A strict census A/B: 10 triples refused that were admitted, none the other
/// way, and **zero** effective-kind changes in `--real-jdk`. The ten left
/// alone are `java.util.Set.of`, whose strict kind is `Intrinsic` — an
/// intrinsic shadowing concrete bytecode is what an intrinsic is (554 of 678
/// `Intrinsic` rows do), so that is a reclassification question, not a fake.
///
/// # 642 -> 644, 2026-08-06 (JDK-only wave 2, the ThreadPoolExecutor retirement)
///
/// Two, and they are the same shape as the 157 -> 165 move: **no new fake was
/// added.** `native_es_execute` — registered on both
/// `java/util/concurrent/ExecutorService.execute(Runnable)` and
/// `java/util/concurrent/ThreadPoolExecutor.execute(Runnable)` — was inheriting
/// the ambient `Bridge`, and it is not a bridge to anything: it is a
/// compatibility stand-in for CratonVM's synthetic 2-field
/// `Executors.new*ThreadPool()` receiver shape, which real `execute()` bytecode
/// would NPE on.
///
/// The retag is the load-bearing half of a change that DELETED code: eight
/// hand-written "does this receiver's `workers` field hold an object?" probes
/// across four files in `vm`, plus the ninth, receiver-blind
/// `force_native_over_real_jdk_bytecode` arm they existed to override. With the
/// native tagged `SyntheticStub` and `ThreadPoolExecutor` on
/// `real_protected_stub_class_common`'s allow-list, the one centralised
/// arbitration yields it to the real `execute()` body class-scoped, for every
/// receiver — and under `--jdk-only` both registrations are refused outright,
/// which the strict refusal count reflects (up 2).
///
/// This is what "wave 1 is measurement" buys: the count rises because the
/// census finally sees a fake it had mislabelled, at the moment that fake stops
/// being reachable on a real-JDK image.
///
/// # 644 -> 697, 2026-08-06 (giving `--jdk-only` a real `java.lang.ProcessImpl`)
///
/// Fifty-three, and again **no new fake was added** — 53 registrations that
/// were already fakes stopped being labelled `Bridge`. They fall in three
/// groups, and the count is worth reading as three numbers rather than one:
///
///  * **37** on `cratonvm/synthetic/{Process, ProcessPipeInputStream,
///    ProcessPipeOutputStream, ProcessExitWaiter}` and
///    `cratonvm/synthetic/AnonymousObject$2` — receivers this VM mints and no
///    image contains, so §1.5's "what an `ACC_NATIVE` method binds to" has
///    nothing to point at. `Bridge` was keeping a fabricated class reachable
///    under `--jdk-only`, which is what §5 forbids outright.
///  * **11** on `java/lang/ProcessBuilder` — every one shadowing ordinary
///    bytecode (`acc_native: false, has_code: true` for all eleven), so §1.4
///    gives the real method precedence. The cluster has to move together:
///    restating `start()` alone leaves `<init>([Ljava/lang/String;)V` writing a
///    raw `String[]` into the `command` field, and the JDK's own `start()` then
///    dies on `command.toArray(...)`.
///  * **5** `java.io` constructors — `BufferedOutputStream(OutputStream)` and
///    its sized twin, `FilterOutputStream(OutputStream)`, and
///    `FileInputStream`/`FileOutputStream(FileDescriptor)`. Each shim assigns
///    the wrapped stream and stops, while each real constructor also
///    initializes `private final Object closeLock = new Object()` — and every
///    matching `close()` opens with `synchronized (closeLock)`. A stream built
///    through these shims throws NullPointerException, not IOException, on its
///    first close, which `ProcessImpl.destroy`'s `catch (IOException ignored)`
///    cannot absorb.
///
/// What the 53 bought, measured rather than argued: `--jdk-only` now returns a
/// real `java.lang.ProcessImpl` from `ProcessBuilder.start()`, and
/// `probes/RealProcessSurfaceProbe` — 21 lines covering pid, all three streams
/// in both directions, `redirectErrorStream`, file and INHERIT redirects,
/// `onExit`, `destroy`, timed `waitFor`, an empty argument and an unspawnable
/// command — is **byte-identical to HotSpot 25**. Compatible `--real-jdk` mode
/// is byte-identical to the build before the change; it keeps every one of
/// these registrations and still answers with the VM's own process object.
const BASELINE_SYNTHETIC_STUBS: usize = 697;

/// Slack added on top of the observed count when (re)freezing the baseline.
/// Documented here so the recount instructions and the constant stay in sync.
const SLACK: usize = 0;

/// Run the registration passes the default (`cfg(not(feature =
/// "synthetic-jdk"))`) boot path in `vm/src/vm/vm_init.rs` runs, in its order.
///
/// # This function is the fix for a blind spot, not a refactor
///
/// It used to be `register_essential_natives` and nothing else, under a doc
/// comment claiming it built "the default native registry exactly as the VM's
/// real-JDK boot path does". That was false, and the gap was large: `vm_init`
/// also installs `register_concurrent_natives`, `register_forkjoin_quiescence`,
/// `register_stamped_lock_natives`, `register_io_natives` and
/// `register_collections_natives`, none of which this census could see.
///
/// Measured 2026-08-05: retagging four `native-collections` registrars moved
/// **364** registrations from `Bridge` to `SyntheticStub`, L6's
/// `bridge-ratchet.sh` (which takes its census from a running VM) counted every
/// one of them — and this gate did not move by a single row. Two ratchets over
/// the same VM, disagreeing by 364, because one of them was looking at a
/// fraction of the registry.
///
/// # What is still not counted, and why that is acceptable
///
/// * The handful of individual `native_methods.register(...)` calls that
///   `vm_init` makes inline between the passes (e.g. `LinkedBlockingQueue
///   .drainTo`). They live in `cratonvm-vm`, which this crate cannot depend on
///   — the dependency runs the other way. They are `Bridge`, and a
///   `SyntheticStub` added there would slip past; if that ever matters, the
///   gate has to move to `vm/tests/`.
/// * The `#[cfg(feature = "synthetic-jdk")]` arm, deliberately: the ratchet
///   guards the shipped `cratonvm-cli` build, which does not enable it.
///
/// `ShimSelection::ALL` where the VM computes a selection from a classpath
/// probe — the widest set, which is the right choice for an upper-bound gate
/// and is what `register_essential_natives` itself passes.
fn register_boot_path(registry: &mut NativeMethodRegistry) {
    // Load-bearing and first, exactly as in `vm_init`: it drops the synthetic
    // `java/util/StringJoiner` natives whose fake 5-field layout corrupts the
    // real 7-field object. Setting it after a pass would leave them in.
    registry.set_drop_real_layout_synthetic(true);
    cratonvm_native_builtins::register_essential_natives_with_shims(
        registry,
        cratonvm_native_builtins::app_shims::ShimSelection::ALL,
    );
    cratonvm_native_builtins::register_concurrent_natives(registry);
    // MUST follow `register_concurrent_natives` — same last-write-wins ordering
    // constraint `vm_init` documents at its call site.
    cratonvm_native_builtins::register_forkjoin_quiescence(registry);
    cratonvm_native_builtins::register_stamped_lock_natives(registry);
    cratonvm_native_io::register_io_natives(registry);
    cratonvm_native_collections::register_collections_natives(registry);
}

/// Every `(class, method, descriptor, kind)` row the boot path leaves in the
/// registry, owned so the borrow of the registry can end.
///
/// `dump_registrations()` is the registry's public census API: one row per
/// surviving registration, in registration order.
fn census_rows() -> Vec<(String, String, String, NativeKind)> {
    let mut registry = NativeMethodRegistry::new();
    register_boot_path(&mut registry);
    registry
        .dump_registrations()
        .into_iter()
        .map(|(c, m, d, k)| (c.to_string(), m.to_string(), d.to_string(), k))
        .collect()
}

/// Build the default native registry the way the VM's real-JDK boot path does,
/// and return `(synthetic_stub_count, total_registrations)`.
fn census() -> (usize, usize) {
    let rows = census_rows();
    let synthetic = rows
        .iter()
        .filter(|(_, _, _, kind)| *kind == NativeKind::SyntheticStub)
        .count();
    (synthetic, rows.len())
}

/// THE GATE FOR STEP 3 OF
/// the retired `native-kind-is-ambient-and-defaults-to-syntheticstub` write-up:
/// *"flip the default last — once every registration states its kind,
/// `current_category` can default to something that fails loudly (or be
/// deleted)."*
///
/// It cannot be phrased as "every registration states its kind", because
/// `kind_stated` is true only for `register_with_kind` (849 of ~11,900) and is
/// deliberately false on every row a `with_category` scope covers on purpose.
/// The answerable form is the one the record actually needs: **no registration
/// may be made with no scope covering it at all**, so the conservative
/// `SyntheticStub` fallback in `NativeMethodRegistry::effective_category` is
/// dead code that can be deleted the day `NativeKind` gains a `Refuse` arm.
///
/// The count was **one** when this gate was written — `MergedAnnotation$Adapt
/// .isIn`, the first `register` in `register_essential_natives_with_shims`,
/// which ran before any `set_category` in the whole boot and therefore took the
/// constructor's default by accident rather than by decision. It is stated now.
///
/// Note what this gate is NOT. It does not say the kinds are right; the bridge
/// ratchet asks that. It does not say anybody adjudicated a row; `kind_stated`
/// and `scripts/jdk-only-kind-map.py` ask that. It says only that the *default*
/// decides nothing any more, which is the specific property step 3 names.
#[test]
fn no_registration_runs_on_the_ambient_default() {
    let mut registry = NativeMethodRegistry::new();
    register_boot_path(&mut registry);
    let rows = registry.census();
    let unchosen: Vec<_> = rows.iter().filter(|r| !r.kind_chosen).collect();

    println!(
        "unchosen: {} of {} registrations were made with no category scope in effect",
        unchosen.len(),
        rows.len()
    );
    for r in unchosen.iter().take(40) {
        println!(
            "  {}.{}{}  kind={}  at {}",
            r.class,
            r.name,
            r.descriptor,
            r.kind.as_str(),
            r.registered_by.as_deref().unwrap_or("<unknown>")
        );
    }
    assert!(
        unchosen.is_empty(),
        "{} registration(s) inherited the registry's conservative default \
         because no `set_category` / `with_category` / `register_with_kind` \
         covered them. That is not a tag, it is the absence of one, and under \
         `--jdk-only` it is the difference between a native being registered \
         and being refused at the door. Wrap the call site in a category scope \
         or state its kind with `register_with_kind`; see the list above.",
        unchosen.len()
    );
}

/// The old, essentials-only census, kept for exactly one purpose: to assert
/// that [`census`] is a strict superset of it.
///
/// Without this, a future edit that quietly narrowed `register_boot_path` back
/// to one pass would lower the count, look like an improvement, and re-open the
/// blind spot this pair exists to close.
fn essentials_only_census() -> (usize, usize) {
    let mut registry = NativeMethodRegistry::new();
    register_essential_natives(&mut registry);
    let rows = registry.dump_registrations();
    let synthetic = rows
        .iter()
        .filter(|(_, _, _, kind)| *kind == NativeKind::SyntheticStub)
        .count();
    (synthetic, rows.len())
}

/// The census must cover strictly more than `register_essential_natives` alone.
///
/// This is the guard on the guard. The number it protects is not arbitrary: on
/// 2026-08-05 the boot registry held 364 more `SyntheticStub` rows than the
/// essentials registry, all of them in `native-collections`, and this gate was
/// blind to every one.
#[test]
fn census_covers_more_than_the_essentials_registrar() {
    let (boot_stubs, boot_total) = census();
    let (ess_stubs, ess_total) = essentials_only_census();

    println!(
        "stub-ratchet(scope): boot {boot_stubs} stubs / {boot_total} rows vs \
         essentials-only {ess_stubs} / {ess_total} \
         (delta {} stubs, {} rows)",
        boot_stubs - ess_stubs,
        boot_total - ess_total,
    );

    assert!(
        boot_total > ess_total && boot_stubs > ess_stubs,
        "the boot census ({boot_stubs} stubs / {boot_total} rows) is no larger than \
         `register_essential_natives` alone ({ess_stubs} / {ess_total}). Either a \
         registration pass was dropped from `register_boot_path`, or the passes it \
         adds have stopped registering anything — both re-open the blind spot that \
         let 364 SyntheticStub rows sit outside this gate until 2026-08-05."
    );
}

/// THE GATE: the synthetic-stub count must not exceed the frozen baseline.
///
/// A failure here means a change ADDED one or more synthetic stubs to the
/// default registry. The fix is to make the new native a real `Bridge`/
/// `Intrinsic` (i.e. correct behavior) — NOT to bump the baseline. Only raise
/// the baseline when a stub is genuinely, unavoidably needed; prefer fixing the
/// underlying VM gap so real bytecode runs (see `feedback_no_synthetic_stubs`).
#[test]
fn synthetic_stub_count_does_not_regress() {
    let (synthetic, total) = census();

    // Always surface the live number (visible with `-- --nocapture`) so the
    // baseline can be (re)frozen to `synthetic + SLACK` without guessing.
    println!(
        "stub-ratchet: {synthetic} SyntheticStub registrations \
         out of {total} total (baseline {BASELINE_SYNTHETIC_STUBS}, slack {SLACK})"
    );

    assert!(
        synthetic <= BASELINE_SYNTHETIC_STUBS,
        "STUB-RATCHET REGRESSION: {synthetic} SyntheticStub natives now registered, \
         exceeding the frozen baseline of {BASELINE_SYNTHETIC_STUBS}. A change added a \
         NEW synthetic stub. Make the new native a real Bridge/Intrinsic (correct \
         behavior) instead of a fake — do NOT just raise the baseline. If the stub is \
         genuinely, unavoidably needed, re-freeze BASELINE_SYNTHETIC_STUBS to \
         {synthetic} + SLACK ({}) and explain why in the PR. See \
         stub-ratchet.md.",
        synthetic + SLACK,
    );
}

/// Sanity guard for the census itself: if `register_essential_natives` ever
/// stops registering anything (a wiring break), the ratchet would pass
/// vacuously. Assert the registry is non-trivially populated so a "0 stubs
/// because 0 registrations" never masquerades as a green ratchet.
#[test]
fn essential_registry_is_populated() {
    let (_synthetic, total) = census();
    // `> 100` was too weak to be a vacuity guard. The ratchet is a ratio
    // argument — "157 of 9,320" — and the denominator was never asserted, so
    // a wiring break that dropped 9,000 registrations would still leave
    // `total` above 100 and the ratchet green with far fewer stubs. This floor
    // sits well below the live count (9,320 as of 2026-07-30) so ordinary
    // churn does not trip it, but a collapse of the surface does.
    // Raised 8,000 -> 11,000 on 2026-08-05 with the census scope. Against the
    // 11,649-row boot registry the old floor would not have noticed losing the
    // whole of `register_collections_natives` (2,279 rows) — the exact failure
    // this test exists to detect. Keep it within ~5% of the live total.
    const MIN_TOTAL_REGISTRATIONS: usize = 11_000;
    assert!(
        total >= MIN_TOTAL_REGISTRATIONS,
        "register_essential_natives produced only {total} registrations, below the \
         {MIN_TOTAL_REGISTRATIONS} floor — the census entrypoint or a whole \
         registration module looks broken, which would make the stub-ratchet pass \
         vacuously. Expected the real-JDK boot path to register ~9,300 natives."
    );
}

// ---------------------------------------------------------------------------
// STRICT (JDK-only) CENSUS — docs/feature-designs/jdk-only-mode.md §4 and §11
//
// Strictness is a *runtime policy* on the registry, not a build feature: the VM
// calls `set_compatibility_mode(CompatibilityMode::JdkOnly)` once at init,
// BEFORE any `register_*` pass (contract §8), and `register()` then refuses a
// `NativeKind::SyntheticStub` outright, recording a
// `JdkOnlyViolation::SyntheticNativeRegistered` so the run can name what went
// missing.
//
// The three tests below measure that same `register_essential_natives` surface
// through the strict policy. They are purely additive: the compatibility-mode
// baseline above is untouched, and nothing here tightens
// `BASELINE_SYNTHETIC_STUBS`. Wave 1 is measurement, not deletion (contract
// §10) — the point of these tests is to make the size and shape of the backlog
// a number CI prints on every run, not to delete anything yet.
//
// The same zero-stub invariant is asserted against a hand-built synthetic mix
// in `native-api/tests/jdk_only_registry.rs`; here it is asserted against the
// real boot-path registrar, which is the one that has 549 stubs in it.
// ---------------------------------------------------------------------------

/// Vacuity floor for the *strict* registry, mirroring `MIN_TOTAL_REGISTRATIONS`
/// in [`essential_registry_is_populated`].
///
/// This is a collapse detector, not a measurement. Strict mode is expected to
/// shed the stub registrations plus whatever aliases hang off them (see
/// [`strict_registry_drops_only_the_stubs`] for why that fallout is real), so
/// the floor sits well below the compatibility-mode floor. If the strict
/// registry ever drops under it, `set_compatibility_mode` is refusing far more
/// than the stubs, and "zero synthetic stubs" would be true only because the
/// registry is empty.
const STRICT_MIN_TOTAL_REGISTRATIONS: usize = 10_500;

/// Build the default native registry the way `--jdk-only` does: set the
/// VM-scoped strict policy *first*, then run the same boot sequence
/// `vm/src/vm/vm_init.rs` runs (see [`register_boot_path`]).
///
/// Ordering is load-bearing and mirrors contract §8: a mode set *after*
/// registration would leave every stub already in the table and make this whole
/// section pass for the wrong reason.
///
/// Returns `(synthetic_stub_count, total_registrations, refused_triples)`. The
/// refusals are returned as triples, not a count: a triple registered by two
/// passes is refused twice, so the count alone cannot be compared against
/// surviving rows — see `strict_registry_drops_only_the_stubs`.
fn strict_census() -> (usize, usize, Vec<(String, String, String)>) {
    let mut registry = NativeMethodRegistry::new();
    registry.set_compatibility_mode(CompatibilityMode::JdkOnly);
    // The same full boot sequence the compatibility census runs (see
    // `register_boot_path`), not `register_essential_natives` alone — otherwise
    // "strict mode refuses N registrations" is a count over a fraction of the
    // registry, and the number this prints is not the number an operator sees
    // in `--jdk-only-report`.
    register_boot_path(&mut registry);

    let rows = registry.dump_registrations();
    let synthetic = rows
        .iter()
        .filter(|(_, _, _, kind)| *kind == NativeKind::SyntheticStub)
        .count();
    let total = rows.len();
    drop(rows);
    let refused = registry
        .refused_registrations()
        .iter()
        .filter_map(|v| match v {
            cratonvm_types::error::JdkOnlyViolation::SyntheticNativeRegistered {
                class,
                method,
                descriptor,
                ..
            } => Some((class.clone(), method.clone(), descriptor.clone())),
            _ => None,
        })
        .collect();
    (synthetic, total, refused)
}

/// Every registration the strict boot makes, as `census_rows` does for the
/// compatible one.
fn strict_rows() -> Vec<(String, String, String, NativeKind)> {
    let mut registry = NativeMethodRegistry::new();
    registry.set_compatibility_mode(CompatibilityMode::JdkOnly);
    register_boot_path(&mut registry);
    registry
        .dump_registrations()
        .into_iter()
        .map(|(c, m, d, k)| (c.to_string(), m.to_string(), d.to_string(), k))
        .collect()
}

/// **A fake must not outlive `--jdk-only` because a SECOND file called it a
/// bridge.** Contract §11's "zero synthetic-stub invocations through any path",
/// asserted per triple instead of per row count.
///
/// `strict_registry_drops_only_the_stubs` bounds row *counts*, and its own
/// comment names the reason that cannot express this: "a triple can be
/// registered with two different kinds". When it is, the drop happening **at
/// registration** decides the outcome — strict mode refuses the `SyntheticStub`
/// row at the door, so the `Bridge` row registered by the other file survives to
/// own the slot, and the method the mode exists to remove goes on being served
/// by the fake. Compatible mode, where nothing is refused, keeps the stub
/// instead: **the two modes run different implementations of the same method**,
/// and which is which is a property of the order two files happen to run in.
///
/// Measured on JDK 25 / linux, 2026-08-06, by diffing a `--jdk-only` census
/// against a `--real-jdk` one: 58 triples, in two families —
/// `StampedLock`/`ServiceLoader`/`URLCodec` (the registrar had no category at
/// all, so its callers disagreed) and `Function$Identity.apply` /
/// `UnaryOperator.identity` / `CountDownLatch` / `CyclicBarrier` /
/// `InputStream.transferTo` (a second file under a `Bridge` scope). The first
/// two of those are the successor defect the retired
/// `native-kind-is-ambient-and-defaults-to-syntheticstub` write-up names: a
/// surviving `Bridge` whose receiver class contract §5 forbids fabricating.
///
/// `Intrinsic` is not flagged. A triple that is a stub in compatible mode and an
/// intrinsic in strict is a *different implementation*, not a surviving fake —
/// `java.util.Set.of` is the whole of that set today — and an intrinsic
/// shadowing concrete bytecode is what an intrinsic is.
#[test]
fn no_fake_survives_strict_mode_as_someone_elses_bridge() {
    let mut compat: std::collections::BTreeMap<(String, String, String), NativeKind> =
        std::collections::BTreeMap::new();
    for (c, m, d, k) in census_rows() {
        // Last write wins, exactly as the registry's slot does.
        compat.insert((c, m, d), k);
    }

    let mut survivors: Vec<String> = Vec::new();
    for (c, m, d, k) in strict_rows() {
        if k != NativeKind::Bridge {
            continue;
        }
        if compat.get(&(c.clone(), m.clone(), d.clone())) == Some(&NativeKind::SyntheticStub) {
            survivors.push(format!("{c}.{m}{d}"));
        }
    }
    survivors.sort();
    survivors.dedup();

    println!(
        "stub-ratchet(strict): {} triple(s) are a SyntheticStub in compatible mode \
         and a Bridge in strict",
        survivors.len()
    );
    assert!(
        survivors.is_empty(),
        "{} triple(s) are tagged `SyntheticStub` where compatible mode dispatches \
         them and `Bridge` where strict mode does, so `--jdk-only` runs the fake \
         the mode exists to remove — and it does so only because the stub \
         registration is refused at the door, leaving the other file's `Bridge` \
         row to own the slot. Decide the kind ONCE, at both sites:\n  {}",
        survivors.len(),
        survivors.join("\n  ")
    );
}

/// Acceptance criterion (contract §11): **the final native registry in strict
/// mode contains zero `SyntheticStub` entries.**
///
/// This passes *today*, and it is worth being precise about why: not because
/// the 549 stubs are gone, but because `register()` refuses them at the door
/// under `JdkOnly`. That is exactly the property CI's zero-stub census asserts
/// against a booted VM, so it is worth pinning here too — it is the cheap,
/// hermetic version of the same check, with no JDK image and no subprocess.
///
/// A failure means a `SyntheticStub` reached the live table despite the strict
/// policy: either a registrar bypasses `register()`, or the mode is being set
/// after registration rather than before it.
#[test]
fn strict_registry_has_zero_synthetic_stubs() {
    let (strict_stubs, strict_total, refused) = strict_census();

    println!(
        "stub-ratchet(strict): {strict_stubs} SyntheticStub registrations out of \
         {strict_total} total; {} registrations refused by JdkOnly",
        refused.len()
    );

    assert_eq!(
        strict_stubs, 0,
        "acceptance criterion (contract §11) violated: {strict_stubs} SyntheticStub \
         natives are in the STRICT registry. Under CompatibilityMode::JdkOnly, \
         register() must refuse every SyntheticStub, so a non-zero count means a \
         registrar reached the slot table without going through register(), or \
         set_compatibility_mode was applied after registration instead of before it."
    );

    assert!(
        strict_total >= STRICT_MIN_TOTAL_REGISTRATIONS,
        "the strict registry holds only {strict_total} registrations, below the \
         {STRICT_MIN_TOTAL_REGISTRATIONS} floor. Zero synthetic stubs is then a \
         statement about an empty registry, not about the stub backlog."
    );
}

/// Strict mode drops the stubs — and, modulo aliasing, *only* the stubs.
///
/// The tempting assertion is exact subtraction:
/// `strict_total == compat_total - compat_stubs`. It is wrong, in a way worth
/// writing down because it will look like a bug to the next reader.
///
/// `NativeMethodRegistry::alias_class` (used by the JDBC, JBoss-MSC and
/// `net_phase_e` registrars) copies a class's natives to a second name by
/// walking the **live registration log** and re-`register()`ing each row it
/// finds. Under `JdkOnly` a refused stub never enters that log, so the alias
/// pass finds nothing to copy and the alias disappears too — one refusal can
/// remove more than one row. Hence:
///
/// * `compat_total - strict_total >= compat_stubs` — every stub is gone, plus
///   any aliases derived from one.
/// * `refused <= compat_stubs` — the mirror image. An alias copy that was
///   itself stub-tagged in compatible mode is counted in `compat_stubs`, but in
///   strict mode `alias_class` never attempts it, so no refusal is recorded for
///   it. `refused` counts refusals, not stubs.
///
/// Both bounds are one-sided on purpose. Pinning the alias fallout to an exact
/// number would make this test fail every time a registrar adds or removes an
/// `alias_class` call, which is churn, not regression.
#[test]
fn strict_registry_drops_only_the_stubs() {
    let compat_rows = census_rows();
    let compat_total = compat_rows.len();
    let compat_stubs = compat_rows
        .iter()
        .filter(|(_, _, _, kind)| *kind == NativeKind::SyntheticStub)
        .count();
    let (_strict_stubs, strict_total, refused_triples) = strict_census();

    let dropped = compat_total.saturating_sub(strict_total);

    println!(
        "stub-ratchet(strict): compatible {compat_total} rows ({compat_stubs} stubs) \
         -> strict {strict_total} rows; {dropped} rows dropped, {} refusals recorded",
        refused_triples.len()
    );

    assert!(
        strict_total <= compat_total,
        "strict mode registered MORE than compatible mode ({strict_total} > \
         {compat_total}). JdkOnly only ever refuses; it must never add a registration."
    );

    assert!(
        dropped >= compat_stubs,
        "strict mode dropped only {dropped} registrations but compatible mode has \
         {compat_stubs} SyntheticStub rows. Every stub row must be absent from the \
         strict registry (plus possibly some alias_class fallout, which is why this \
         is a lower bound and not equality). A shortfall means some stubs survive \
         the strict policy."
    );

    // The property is CONTAINMENT, not a count comparison. `refused <=
    // compat_stubs` was the old form and it is wrong once the census runs the
    // real boot sequence: a triple registered by two passes — which the boot
    // path does deliberately, `register_forkjoin_quiescence` overriding
    // `register_concurrent_natives` by last-write-wins — yields ONE surviving
    // row in compatible mode and TWO refusals in strict. On 2026-08-05 that read
    // 598 refusals against 549 stub rows and failed a gate that was measuring
    // nothing wrong.
    //
    // THE THIRD BOUND WAS DELETED ON 2026-08-05, DELIBERATELY. Do not re-add it
    // without reading this.
    //
    // It asserted `refused <= compat_stubs`, justified as "a refusal with no
    // corresponding stub means JdkOnly is rejecting a Bridge or an Intrinsic".
    // The property is real — contract §4 — but it is **not observable from a
    // differential census**, and the boot-path scope fix made that impossible to
    // ignore. Three independent reasons, each measured:
    //
    //  1. **Refusal is per-ATTEMPT; the registry is per-TRIPLE.** A triple
    //     registered by two passes yields ONE surviving compat row and TWO
    //     refusals, so the counts are not comparable at all — 598 refusals
    //     against 549 stub rows here, with nothing wrong.
    //  2. **Compatible mode has drop rules of its own, and they run AFTER the
    //     JdkOnly refusal in `register()`.** `drop_real_layout_synthetic` (which
    //     `vm_init` sets, and which this census now mirrors) drops every
    //     `java/util/EnumSet` native outright, so strict *records* a refusal for
    //     `EnumSet.noneOf` while compat holds no row for it — neither surviving
    //     nor overwritten. Both behaviours are correct.
    //  3. **A triple can be registered with two different kinds.**
    //     `java/nio/ByteBuffer.allocate(I)` is registered `SyntheticStub` by one
    //     pass and non-stub by another, so it is simultaneously refused (the
    //     stub attempt) and present in the strict registry (the other one).
    //     Even "a refused triple is absent from the strict registry" is false.
    //
    // Reasons 2 and 3 are not fixable by a cleverer query: the registry does not
    // retain the kind of every attempt, only of survivors plus one level of
    // `overwrote`. A gate that cannot mean what it claims is decoration, and
    // this feature has shipped three of those already — so it goes, rather than
    // being weakened until it passes.
    //
    // The property is enforced where it belongs: structurally in `register()`,
    // which refuses on `!current_category.allowed_in(JdkOnly)` and so can refuse
    // no other kind by construction, and hermetically in
    // `native-api/tests/jdk_only_registry.rs`, which asserts it against a
    // hand-built kind mix where no drop rule can confound the answer.
    //
    // The two bounds above survive because both are counts over survivors on
    // both sides, which duplicate registration and compat-side drops cannot
    // skew in the direction they assert.

    assert!(
        strict_total >= STRICT_MIN_TOTAL_REGISTRATIONS,
        "the strict registry holds only {strict_total} registrations, below the \
         {STRICT_MIN_TOTAL_REGISTRATIONS} floor — strict mode is shedding whole \
         registration modules, not just stubs."
    );
}

/// THE END-STATE GATE, deliberately `#[ignore]`d.
///
/// [`strict_registry_has_zero_synthetic_stubs`] passes today for a weak reason:
/// `register()` refuses the stubs at the door. The 549 registrations still
/// exist in `native-builtins/src/`, still run on every boot, and are still what
/// an ordinary `--real-jdk` run dispatches into. **Refused is not retired.**
///
/// This test asserts the strong property — strict mode has *nothing to refuse*
/// — and stays ignored until all three of the following have landed:
///
/// 1. **Reclassify or delete the 549 `SyntheticStub` registrations** in
///    `native-builtins/src/`, subsystem by subsystem: each one becomes a real
///    `Bridge`/`Intrinsic` because it genuinely crosses a VM boundary, or it
///    goes away so the real JDK bytecode runs. This is explicitly *not* wave 1
///    work (contract §8: "do not edit `native-builtins/src/lib.rs`; the
///    549-stub reclassification is a separate wave with its own
///    subsystem-per-PR discipline").
/// 2. **Drive [`BASELINE_SYNTHETIC_STUBS`] to 0 in the same change** that
///    removes the last one. The ratchet is slack-free by design; leaving the
///    baseline at 549 after the stubs are gone would silently re-admit 549 new
///    ones.
/// 3. **Un-ignore this test** (delete the `#[ignore]`) so the zero is held,
///    and promote the CI `jdk-only` job from advisory to blocking, which is the
///    posture contract §11 calls for.
///
/// Until then it is run on demand:
/// `cargo test -p cratonvm-native-builtins --test stub_ratchet -- --ignored --nocapture`
#[test]
#[ignore = "wave 1 is measurement: the 549 stubs are refused at registration, not yet retired"]
fn strict_mode_refuses_nothing() {
    let (_strict_stubs, _strict_total, refused) = strict_census();
    let refused = refused.len();

    assert_eq!(
        refused, 0,
        "{refused} SyntheticStub registrations still have to be refused at VM init. \
         Zero refusals is the real end state: it means the stubs were reclassified \
         or deleted at the source, not merely filtered out of the table on the way \
         in. See this test's doc comment for the three steps that must land first."
    );
}
