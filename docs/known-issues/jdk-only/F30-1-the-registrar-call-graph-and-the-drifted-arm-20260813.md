# F30 — the registrar call graph: four configurations, one drifted arm, and a gate instead of a comment

**Date:** 2026-08-13 **Lane:** F30
**Owns:** `vm/src/vm/vm_init.rs` (only). Everything else here is a NOMINATION.
**Status:** FIXED-UNVERIFIED-BY-CARGO. This lane did **not** run `cargo`
(build/check/test/clippy) and did **not** execute the CratonVM binary. The
edited file was parse-checked by `rustfmt` on a copy (exit 0, and the new test
module is rustfmt-clean); the new test's *logic* was mutation-checked by a
faithful re-implementation run over eight perturbed copies of the real file
(§5.2). Nothing here is a measured VM behaviour claim.

**Prov:** everything below is read out of this working tree at
`C:\craton\cratonvm\.claude\worktrees\h2-testmultithread-concurrent-timeout-8a2a68`
on 2026-08-13. Line numbers are post-edit unless marked "pre-edit".

---

> **VERIFIED AGAINST A BINARY 2026-09-03. The gate fires.** This record's status
> was **FIXED-UNVERIFIED-BY-CARGO** — *"This lane did not run `cargo`
> (build/check/test/clippy)"*, with the new test's logic mutation-checked only
> *"by a faithful re-implementation run over eight perturbed copies"* (§5.2).
> `cargo` has now run it, on the real file, in both Cargo configurations.
>
> ```text
>                                                          default   --features synthetic-jdk
> the_two_real_jdk_arms_run_the_same_registrars_in_the_same_order   ok        ok
> only_the_synthetic_mode_arm_reaches_register_builtins             ok        ok
> last_write_wins_ordering_holds_inside_both_real_jdk_arms          ok        ok
> the_synthetic_jdk_feature_still_implies_management                ok        ok
> the_feature_off_arm_never_consults_the_runtime_jdk_mode           ok        ok
> ```
>
> **Both configurations really were built.** The two runs report different
> filtered-out totals — 2,734 tests against 4,257 — so this is two compilations
> of two different test sets, not the same build scored twice. For a record
> whose whole subject is that the two arms are compiled by different features,
> that distinction is the point.
>
> **And the gate was mutation-proven against the real file, not a copy.**
> Deleting one registrar call from real-JDK arm A — `register_collections_natives`
> at `vm_init.rs:2534`, which is the shape of this record's original defect —
> turns the witness red and names the drift:
>
> ```text
> the_two_real_jdk_arms_run_the_same_registrars_in_the_same_order ... FAILED
>   left  [ …, "register_classvalue_natives", "register_random_and_securerandom_natives", … ]
>   right [ …, "register_classvalue_natives", "register_collections_natives",
>           "register_random_and_securerandom_natives", … ]
> ```
>
> The mutation was reverted and `vm_init.rs` left byte-identical to `HEAD`.
> §5.2's eight perturbed copies tested a re-implementation of the logic; this
> tests the shipped test against the shipped file, which is the half that was
> missing. Both arms carry **49** registrars, comfortably above the witness's
> own `len() > 40` parse-collapse floor — so the floor is not what is holding
> the assertion up.
>
> **The original defect is visibly closed in the transcript.** This record's
> §0 says the `--features synthetic-jdk` build's real-JDK arm was missing four
> registration passes, *"including the one that stops a seeded
> `java.util.Random` returning all zeros"*.
> `register_random_and_securerandom_natives` appears in both arms above.
>
> **What this does NOT verify.** These are SOURCE-WITNESS tests: they read
> `vm_init.rs` as text at run time and assert about its call graph. A green here
> says the two arms name the same registrars in the same order; it does not say
> either arm registers the right natives, and no VM was run for this note. §1's
> four-configuration table and every NOMINATION outside `vm_init.rs` remain
> unadjudicated — this lane owned that one file, and so does this discharge.

## 0. The one-paragraph version

`vm_init.rs` has **three** registration arms, not two, and they serve **four**
configurations. Two of the three arms are real-JDK arms that are supposed to be
identical and had silently grown apart: the `--features synthetic-jdk` build's
real-JDK arm was missing four registration passes the shipping arm has,
including the one that stops a seeded `java.util.Random` returning all zeros.
`register()` is last-write-wins and `NativeKind` is ambient, so **which arm** and
**in what order** are semantics. Both facts are now asserted by a source-witness
test at the bottom of `vm_init.rs` rather than described in a comment.

---

## 1. The four configurations (read this table first)

`vm_init.rs` forks twice: on the Cargo feature `synthetic-jdk` (compile time)
and on `config.use_synthetic_jdk` (runtime). Those two forks are **not**
symmetric, and that asymmetry is the whole trap.

| # | JDK mode (runtime) | `synthetic-jdk` feature | arm that runs | source range | passes |
|---|---|---|---|---|---|
| 1 | real-JDK (incl. `--jdk-only`) | **on** | **real-JDK arm A** — the `else` of `if config.use_synthetic_jdk` | L2001–L2608 | 49 |
| 2 | real-JDK (incl. `--jdk-only`) | off | **real-JDK arm B** — the `#[cfg(not(feature = "synthetic-jdk"))]` block | L2613–L3276 | 49 |
| 3 | synthetic-JDK | **on** | **synthetic arm** | L1932–L2000 | 4 |
| 4 | synthetic-JDK *requested* | off | **real-JDK arm B again**, minus the real-layout drop (§9) | L2613–L3276 | 49 |

Facts that make this table non-obvious, each verified in source:

* **Configuration 4 exists and is reachable.** The
  `#[cfg(not(feature = "synthetic-jdk"))]` block reads `config.use_synthetic_jdk`
  in exactly **one** place — since 2026-09-05; it was zero when this record was
  written, and §9 is what changed and why. Every `register_*` pass in the block
  is still unconditional, so a feature-off build asked for synthetic mode still
  gets the *real-JDK* registrar set, not an empty registry. `config::require_synthetic_jdk()`
  rejects that pairing — but its only callers are `libcratonvm/src/lib.rs:479`
  and the CLI, **not** `SharedVm::new`. And `VmConfig::default()`'s JDK mode is
  `EMBEDDED_DEFAULT_JDK_MODE = JdkMode::Synthetic` (`vm/src/config.rs:225`), so
  every embedder and in-tree test that does not set the mode explicitly *is*
  configuration 4.
* **`--jdk-only` never reaches configuration 3.**
  `VmConfig::validate_compatibility` (`vm/src/config.rs:967`) rejects
  `is_jdk_only() && use_synthetic_jdk`. `SharedVm::new` reaches it on every
  boot: `vm_init.rs:1183` calls `require_jdk_image_for_jdk_only`, whose first
  statement (`vm_init.rs:679`) is `config.validate_compatibility()?`, and L1184
  turns any error into a `panic!`. So `--jdk-only` is always arm A or arm B —
  which is why the arm-A drift in §3 was a `--jdk-only` defect, not a curiosity.
* **Arm A is the *default* arm of a feature-enabled build.** Turning
  `synthetic-jdk` on does not select synthetic mode; the mode is a separate,
  explicit request. So every `--features synthetic-jdk` binary that is not
  passed `--synthetic-jdk` runs arm A.

Common tail, outside all three arms and therefore in **every** configuration:

| line | pass | gate |
|---|---|---|
| L3286 | `cratonvm_native_awt::register_awt_natives` | `#[cfg(feature = "awt")]` (default-on) |
| L3783 | `jmx::register_mbean_server` | `#[cfg(feature = "management")]` (default-on) |

`register_mbean_server` is, per its own comment, the last `register_*` before
the registry is moved into `Self`.

---

## 2. The ordered census

### 2.1 Synthetic arm (configuration 3), L1932–L2000

| # | line | pass |
|---|---|---|
| 1 | 1934 | `crate::native::register_builtins` |
| 2 | 1935 | `crate::native::register_io_natives` |
| 3 | 1936 | `crate::native::register_collections_natives` |
| 4 | 1998 | `util_concurrent_ext::register_synthetic_aqs_natives` |

`register_builtins` is `#[cfg(feature = "synthetic-jdk")]` in
`native-builtins/src/lib.rs:21601` and its body is exactly
`register_essential_natives(registry); register_synthetic_overrides(registry);`.
It is the **only** caller of `register_synthetic_overrides` in the tree
(verified: `grep -rn "register_synthetic_overrides"` returns 20 hits, all of
them prose, tests or the definition, plus this one call). Everything reached
only from inside `register_synthetic_overrides` is therefore
**synthetic-mode-only**, whatever `NativeKind` it is tagged with.

### 2.2 The two real-JDK arms (configurations 1, 2 and 4)

Post-fix these are **sequence-identical**, 49 passes each. Only the `#[cfg]`
column differs, and only for the JMX family (§3.3).

| # | pass | arm A line | arm A cfg | arm B line | arm B cfg |
|---|------|-----------|-----------|-----------|-----------|
| 1 | `register_essential_natives_with_shims` | 2055 | – | 2639 | – |
| 2 | `register_concurrent_natives` | 2061 | – | 2647 | – |
| 3 | `register_forkjoin_quiescence` | 2067 | – | 2653 | – |
| 4 | `register_stamped_lock_natives` | 2068 | – | 2654 | – |
| 5 | `register_p61_file_handler` | 2080 | – | 2666 | – |
| 6 | `register_url_classloader_close_bridge` | 2086 | – | 2668 | – |
| 7 | `register_io_natives` | 2252 | – | 2834 | – |
| 8 | `register_p60_process_handle` | 2255 | – | 2837 | – |
| 9 | **`register_classvalue_natives`** | **2269 (new)** | – | 2844 | – |
| 10 | `register_collections_natives` | 2300 | – | 2865 | – |
| 11 | **`register_random_and_securerandom_natives`** | **2311 (new)** | – | 2878 | – |
| 12 | `register_properties_sidetable` | 2318 | – | 2891 | – |
| 13 | `register_t12_unsafe_natives` | 2322 | – | 2894 | – |
| 14 | `register_t14_system_bootstrap` | 2326 | – | 2897 | – |
| 15 | `register_boot_loader_natives` | 2333 | – | 2901 | – |
| 16 | `register_phase57_nio_file` | 2339 | – | 2904 | – |
| 17 | **`register_phase57_file`** | **2351 (new)** | – | 2914 | – |
| 18 | `register_p59_jar` | 2375 | – | 2923 | – |
| 19 | `register_p59_bulk_stream_transfer` | 2376 | – | 2928 | – |
| 20 | `register_p59_zip_output_primitives` | 2379 | – | 2931 | – |
| 21 | **`register_spring_boot_logback_apply`** | **2390 (new)** | – | 2939 | – |
| 22 | `register_url_codec` | 2407 | – | 3136 | – |
| 23 | `register_charset_natives_pub` | 2411 | – | 3137 | – |
| 24 | `register_p58_charset_coder` | 2413 | – | 3138 | – |
| 25 | `register_real_charset_natives` | 2418 | – | 3139 | – |
| 26 | `register_deprecated_internal_natives` | 2423 | – | 3140 | – |
| 27 | `register_arrays_support_natives` | 2427 | – | 3143 | – |
| 28 | `register_string_latin1_natives` | 2432 | – | 3146 | – |
| 29 | `register_classloader_real_natives` | 2451 | – | 3164 | – |
| 30 | `register_phase54_method_handle` | 2457 | – | 3167 | – |
| 31 | `register_p63_method_handles_lookup` | 2463 | – | 3170 | – |
| 32 | `register_t4_method_handle_invoke` | 2469 | – | 3173 | – |
| 33 | `register_t28_method_handle_completeness` | 2478 | – | 3177 | – |
| 34 | `register_p68_invoke_extras` | 2489 | – | 3189 | – |
| 35 | `init_service_loader_bootstrap` | 2497 | – | 3193 | – |
| 36 | `register_reflect_proxy_natives` | 2505 | – | 3196 | – |
| 37 | `register_instrumentation_natives` | 2510 | – | 3204 | – |
| 38 | `register_self_attach_natives` | 2514 | – | 3209 | – |
| 39 | `jmx::register_vm_management_impl` | 2521 | – | 3214 | `management` |
| 40 | `jmx::register_jmx_natives` | 2561 | `management` | 3248 | `management` |
| 41 | `jmx::register_thread_impl` | 2583 | – | 3253 | `management` |
| 42 | `jmx::register_class_loading_impl` | 2584 | – | 3255 | `management` |
| 43 | `jmx::register_garbage_collector_impl` | 2585 | – | 3257 | `management` |
| 44 | `jmx::register_memory_pool_impl` | 2589 | – | 3261 | `management` |
| 45 | `jmx::register_memory_manager_impl` | 2590 | – | 3263 | `management` |
| 46 | `jmx::register_operating_system_impl` | 2591 | – | 3265 | `management` |
| 47 | `jmx::register_hotspot_diagnostic` | 2592 | – | 3267 | `management` |
| 48 | `jmx::register_flag_impl` | 2593 | – | 3269 | `management` |
| 49 | `register_slf4j_binder_stubs_pub` | 2605 | – | 3273 | – |

**Note #35.** `init_service_loader_bootstrap` is a registration pass whose name
does not start with `register_`. The first draft of the witness test in §5 was a
`register_*` name scanner and silently missed it — a lane could have deleted it
from one arm and the "arms are identical" gate would have stayed green. The
shipped scanner keys on the **argument** (`&mut native_methods`) instead. This
is the same failure the lane exists to catch, reproduced inside the lane's own
instrument; it is recorded here rather than quietly fixed.

---

## 3. The drift, and what was done about it

### 3.1 Four registration passes present in arm B and absent from arm A (FIXED)

All four were added to arm A **at the position they occupy in arm B**, because
the registry is last-write-wins and position is semantics.

| pass | what arm A lost | evidence it was a defect, not a deliberate difference |
|---|---|---|
| `register_classvalue_natives` | all `java.lang.ClassValue` natives | arm B's own comment: *"needed here explicitly because this real-JDK-mode branch does NOT call `register_synthetic_overrides`"*. Arm A is also real-JDK mode and also does not call it. |
| `register_random_and_securerandom_natives` | **a seeded `java.util.Random` returns 0 from every `nextInt`/`nextLong`/`nextDouble`** | arm B's comment states the mechanism: in real-JDK mode `Random` field 0 is the `AtomicLong seed` *reference*, not a long, so the aliases `register_collections_natives` re-registers read 0. The `securerandom` handlers keep the seed in an identity-hash-keyed side table and are layout-independent. Nothing about that reasoning is feature-dependent. |
| `register_phase57_file` | `java.io.File` constructors and metadata accessors fall back to `FileSystem.normalize` bytecode the interpreter does not run cleanly | the 2026-08-07 comment **in arm A itself** writes the required order out as *"nio_file → file → jar → bulk → zip-output"* and then registers everything in that list except `file`. The order was written down correctly and one step of it was skipped. |
| `register_spring_boot_logback_apply` | nothing today — the callee is an empty `pub fn ...(_registry) {}` since the 2026-07-24 logging batch | inert, but its own doc comment claims it is *"registered unconditionally in real-JDK mode by `vm_init.rs`"*, which was false for arm A. Added so the two arms are sequence-identical and the §5 gate needs **no exception list** — an exception list is the folklore shape this lane exists to remove. |

The second row is the consequential one: any `--features synthetic-jdk` binary
running real-JDK mode or `--jdk-only` had a `java.util.Random` that emitted
zeros. That includes fixture and gate runs built with the feature.

### 3.2 Three inline registration rows present only in arm B (NOT fixed — see §7)

These are `native_methods.register(...)` closures, not registrar calls:

| row | arm B line (post-edit) | arm A |
|---|---|---|
| `java/util/concurrent/CopyOnWriteArrayList.addIfAbsent (Ljava/lang/Object;)Z` | 2772 | absent |
| `java/util/ArrayList.toArray ([Ljava/lang/Object;)[Ljava/lang/Object;` | 3119 | absent |
| `java/util/AbstractCollection.toArray ([Ljava/lang/Object;)[Ljava/lang/Object;` | 3125 | absent |

Not fixed here: their bodies call helper `fn`s declared **inside** arm B
(`real_jdk_to_array_typed`, arm B L2957), so unifying them is a code
move of ~200 lines that cannot be validated without a build. Nominated in §7.

### 3.3 One `NativeKind` drift (NOT fixed — see §7)

The Quarkus `RunnerClassLoader.close` row is registered in **both** arms:

* arm B L3157: `native_methods.register_with_kind(..., NativeKind::SyntheticStub)`,
  under a comment saying the reliance on the ambient default *"was the only one
  left in the VM crate"*.
* arm A L2441: plain `native_methods.register(...)`, i.e. still ambient.

The comment was wrong about its own denominator. Left alone because changing a
row's `NativeKind` changes what `--jdk-only` refuses, and this lane cannot
measure that.

### 3.4 The `#[cfg(feature = "management")]` difference is DELIBERATE (documented + gated)

Arm A leaves rows 39 and 41–48 ungated; arm B gates every one of them. That is
not drift: `vm/Cargo.toml`'s `synthetic-jdk` feature list **includes
`"management"`**, so in every build that compiles arm A, `management` is on. The
cfg attributes were deliberately **not** mirrored onto arm A: without them, a
future change that drops `management` from the `synthetic-jdk` feature list
breaks arm A's **compile** (loud) instead of silently deleting its JMX surface
(quiet). The load-bearing fact — the feature implication — is now asserted by
`the_synthetic_jdk_feature_still_implies_management` (§5.1).

---

## 4. Four worked examples of the failure mode

Recorded because each cost a lane a manual rediscovery this session, and each
is the *same* mistake: inferring reach from a name or a category instead of
tracing the call.

1. **Category is not reach.** `register_p67_string_template` and the phase-64/67
   registrars live under `register_synthetic_overrides`. A doc comment claimed
   their rows survive `--jdk-only` because their `NativeKind` is `Bridge`. The
   claim about categories is true; the conclusion is false, because the
   registrar never runs in that mode. *(`register_p67_string_template`'s call was
   since deleted by lane F15 —
   `native-builtins/src/phases_late.rs:5803` now carries the tombstone.)*
2. **One call site, one mode.** `register_pe_panama` is called exactly once,
   `native-builtins/src/lib.rs:24180`, inside `register_synthetic_overrides`
   (which spans L21612–~L24390). Real-JDK and `--jdk-only` never registered
   panama's layouts, the two shipping modes ran different `structLayout`
   implementations, and the synthetic-mode tests exercised the one `--jdk-only`
   does not run. Every arithmetic defect in the shipping implementation had no
   test.
3. **Last-write-wins makes one rule resolve two ways.** In synthetic mode
   `register_builtins` runs essentials and *then* overrides, so a triple
   registered in both places resolves to the override; the real-JDK arms run
   essentials only, so the same triple resolves to the essential. Two guards for
   one rule were live in different modes and drifting. The already-landed fix at
   `native-builtins/src/lib.rs:21381` states the pattern exactly: *"Registering
   it LAST is deliberate: `register()` is last-write-wins, and until this call
   existed the phases_late family only ever won in synthetic-jdk mode, so the
   shipping default ran the broken copy."*
4. **Shadowing by position.** `register_io_natives` runs after
   `register_essential_natives_with_shims` in both real-JDK arms (rows 1 and 7
   of §2.2), so `native-io`'s bodies shadow `native-builtins`' aliasing
   implementations. A lane cleared several cells against a body that never runs.

The generalisation for the next lane: **before landing a fix in a `register_*`
function, find its call sites and check which of the four configurations in §1
reach them.** `grep -rn "fn <name>"` for the gate, then `grep -rn "<name>("` for
the callers, then walk up until you hit one of the three arms.

---

## 5. The gate

`vm/src/vm/vm_init.rs`, new module `registrar_call_graph_witness` at the bottom
of the file. It is a **source-witness** test: it reads `vm_init.rs` and
`vm/Cargo.toml` from the working tree at run time (`env!("CARGO_MANIFEST_DIR")`),
not via `include_str!`, so it cannot pass against source that is no longer
there.

### 5.1 What it asserts

| test | property | goes red when |
|---|---|---|
| `the_two_real_jdk_arms_run_the_same_registrars_in_the_same_order` | the arm-A and arm-B pass sequences are **equal**, element for element | a pass is added to or removed from one arm; a pass is reordered in one arm; the sequence collapses below 40 (parser drift guard) |
| `only_the_synthetic_mode_arm_reaches_register_builtins` | synthetic arm opens with `register_builtins`; neither real arm calls `register_builtins` or `register_synthetic_overrides`; the synthetic arm does not re-run essentials afterwards | any real-JDK arm gains the synthetic-override family; the synthetic arm stops opening with it |
| `last_write_wins_ordering_holds_inside_both_real_jdk_arms` | 8 ordering pairs, each carrying its incident in the failure message: essentials→io, concurrent→forkjoin, collections→securerandom, collections→properties, and nio_file→file→jar→bulk→zip-output | any of those pairs is inverted, or either member is deleted |
| `the_synthetic_jdk_feature_still_implies_management` | `vm/Cargo.toml`'s `synthetic-jdk` list contains `"management"` | the implication arm A's ungated JMX calls depend on is dropped |
| `the_feature_off_arm_consults_the_runtime_jdk_mode_only_for_the_layout_drop` | **exactly one** non-comment `use_synthetic_jdk` occurrence inside the `cfg(not(synthetic-jdk))` block, **and** it is the three-line `if !config.use_synthetic_jdk { native_methods.set_drop_real_layout_synthetic(true); }` (matched line for line) | someone adds a second branch — i.e. a fourth registration path, invalidating row 4 of §1 — or moves the branch onto something other than the layout drop |

Arm location itself is guarded: the three anchors must each match exactly once
(`if config.use_synthetic_jdk {`, `#[cfg(not(feature = "synthetic-jdk"))]`) or
exactly twice (the closing `"Real JDK mode: …"` tracing line), and the five
positions must be in the expected source order. All anchors are matched against
the **whole trimmed line**, so the constants that name them inside the test
module cannot match themselves.

### 5.2 Mutation check

`cargo` was not run (lane rule). Instead the parser and all five assertions were
re-implemented line-for-line in a throwaway script and run over eight perturbed
copies of the real file. Result — control green, **8/8 mutations detected**:

| mutation | red tests |
|---|---|
| control (unmutated working tree) | *none* ✅ |
| M1 delete `register_phase57_file` from arm A | arms_same_order, lww_ordering |
| M2 move arm B's securerandom above collections | arms_same_order, lww_ordering |
| M3 hoist arm A's `register_io_natives` above essentials | arms_same_order, lww_ordering |
| M4 add `register_builtins` to arm A | arms_same_order, only_syn_reaches_builtins |
| M5 drop `"management"` from `synthetic-jdk` in `vm/Cargo.toml` | synthetic_implies_management |
| M6 add `let _probe = config.use_synthetic_jdk;` to arm B | arm_b_ignores_runtime_mode |
| M7 introduce a third `"Real JDK mode: …"` line | all four source-reading tests, via the anchor-count assertion |
| M8 delete `init_service_loader_bootstrap` from arm A | arms_same_order |

M8 is the one that failed against the first draft (§2.2 note) and passes against
the shipped argument-keyed scanner.

### 5.3 What it does NOT gate

* The inline `native_methods.register(...)` rows (§3.2) — receiver, not argument.
* `NativeKind` (§3.3) — ambient, and not visible to a line-oriented reader.
* Anything in `native-builtins`. In particular it does **not** assert that
  `register_pe_panama` has one call site; that fact lives in another crate and
  other lanes are editing those files right now. See §7 NOM-2.

---

## 6. Verified vs. assumed

**Verified (source read in this tree, today):** the three arm ranges and their
`cfg`/runtime gates; all 49 + 49 + 4 registration passes and their order; the
two common-tail passes; `register_builtins`'s body and its
`#[cfg(feature = "synthetic-jdk")]`; that it is the only caller of
`register_synthetic_overrides`; `register_pe_panama`'s single call site and its
enclosing function; that `register_spring_boot_logback_apply`'s body is empty;
`vm/Cargo.toml`'s feature graph (`default = ["awt","management","zgc"]`,
`synthetic-jdk = [… , "management"]`); `validate_compatibility`'s rejection of
`--jdk-only` + synthetic and its unconditional call at `vm_init.rs:679`;
`require_synthetic_jdk`'s two callers; `EMBEDDED_DEFAULT_JDK_MODE == Synthetic`.

**Verified mechanically:** the file still parses (`rustfmt --emit stdout` on a
copy, exit 0); the new module is rustfmt-clean and the four insertions produce
no rustfmt hunks in L1930–L3300; CRLF preserved (CR count == line count before
and after every edit).

**Assumed / NOT verified:** that the crate *compiles* and the tests *pass* — no
`cargo` was run. That adding the four passes to arm A has no other consequence
in a feature-enabled build; the arguments in §3.1 are read off arm B's own
comments, not measured. That the behavioural claims in those comments (the
all-zero `Random`, the `File.normalize` gap) are still true today — they are
quoted as the *rationale for parity*, not re-measured.

---

## 7. Nominations (files this lane does not own)

**NOM-1 — `vm/src/vm/vm_init.rs` is mine, but this one needs a build.** The
three inline rows in §3.2 and the `NativeKind` drift in §3.3. Concretely:
arm A L2441 should become `register_with_kind(..., NativeKind::SyntheticStub)`
to match arm B L3157, and arm A needs copies of the `CopyOnWriteArrayList
.addIfAbsent` / `ArrayList.toArray` / `AbstractCollection.toArray` closures plus
the `real_jdk_to_array_typed` helper they call. Exact old/new text is not given
because the correct move is to *hoist the helper and the three closures out of
both arms into one place above the fork*, which is a refactor, not a patch.

**NOM-2 — `native-api/tests/guarded_slot_maps.rs` (source-witness suite that
already asserts `register_synthetic_overrides`' shape).** Add a companion
assertion that `register_pe_panama` has exactly one call site in
`native-builtins/src/lib.rs` and that it is inside `register_synthetic_overrides`
— i.e. freeze the §4.2 finding so the next lane cannot rediscover it. Exact
text not supplied: that file is a live edit target this session, and the
existing helpers there (`fn_body`, the preceding-line check) are the right
building blocks but their current signatures should be read at apply time.

**NOM-3 — `native-builtins/src/logging_shims.rs:2678.** `register_spring_boot_logback_apply`
is `pub fn ...(_registry: &mut NativeMethodRegistry) {}`. Either delete it and
its two call sites, or keep it and say in the doc comment that it is
intentionally inert so a reader does not spend a second lane discovering it
again. This lane chose to *keep both call sites* so the arms stay identical; a
deletion must remove **both**.

---

## 8. Left undone

* No `cargo` verification of any kind (lane rule). The four inserted calls and
  the new test module are unbuilt.
* `native-builtins`' own internal ordering — `register_essential_natives`
  (`lib.rs:7102`) is 14,000 lines of registration whose internal
  last-write-wins order this lane did not census. The §5 gate stops at the
  `vm_init` boundary.
* Whether the four passes added to arm A change any currently-green
  synthetic-feature test. They restore parity with the shipping arm, so a
  regression here would mean the *shipping* arm is wrong — worth knowing, and
  not knowable without a run.
* The `register_p64_hex_format` pattern (§4.3) suggests a systematic sweep:
  **which other `phases_late` families win only in synthetic mode because their
  only call site is inside `register_synthetic_overrides`?** That is a
  `native-builtins` census, out of this lane's file scope, and it is the highest
  value follow-on from this record.

---

## 9. 2026-09-05 — configuration 4 gets one runtime branch, and why it had to

§1 row 4 is not a curiosity: it is the configuration every in-tree test and
every embedder that writes `Vm::new(VmConfig::new())` actually runs. This
record said so on 2026-08-13 and stopped there. What it did not say is what
that configuration can and cannot *do*, and the answer turned out to be: it
could not evaluate `"abcdef".length()`.

**The mechanism, in three facts that are each individually reasonable.**

1. `VmConfig::default()` is `JdkMode::Synthetic`, so `SharedVm::new` skips
   boot-classpath discovery. `java/lang/String` is a VM-minted carrier
   declaring five methods, with no `Code` attribute anywhere.
2. `register_builtins` / `register_synthetic_overrides` — the ~5,200 stubs that
   *are* the synthetic class library — are `#[cfg(feature = "synthetic-jdk")]`,
   so in a feature-off build they do not exist to be called;
   `vm/src/native/builtins.rs` supplies no-op stubs. §1 already notes this.
3. Arm B opened with `native_methods.set_drop_real_layout_synthetic(true)`,
   unconditionally. `NativeMethodRegistry::register` then drops every
   `java/lang/String` `Bridge` — the policy
   `vm/tests/wp8_10_9_string_contains_native.rs` documents, and a correct one
   whenever there is real bytecode behind it.

Together: no bytecode (fact 1), no synthetic override (fact 2), and the
essential bridge dropped (fact 3). `String.length()` / `isEmpty()` / `charAt()`
/ `equals()` resolved nowhere and raised `NoSuchMethodError`; `hashCode()`
survived only by falling through to `Object`. Fact 3 is the one that can be
corrected without resurrecting compiled-out code, and it is the one answering
the wrong question: the drop exists because *"a fake 5-field layout corrupts the
real 7-field object"*, which presupposes a real object. In configuration 4 there
is none to protect, and the drop is pure loss.

**The change.** Arm B now reads

```rust
if !config.use_synthetic_jdk {
    native_methods.set_drop_real_layout_synthetic(true);
}
```

That is the arm's only runtime-mode branch, and the §5.1 gate row pins both
halves of it: exactly one occurrence, and that occurrence being these three
lines. No registration pass became conditional, so §3's arm-parity census and
`the_two_real_jdk_arms_run_the_same_registrars_in_the_same_order` are untouched
— what they compare is call sequences, and configurations 2 and 4 still run the
same one.

**One registrar moved with it.**
`service_loader::register_service_loader_natives`' body was
`#[cfg(feature = "synthetic-jdk")]` for the same "real-JDK mode should run the
real bytecode" reason, and had the same hole: configuration 4 has no
`java/util/ServiceLoader` bytecode either, so `ServiceLoader.load(Driver.class)`
raised `NoSuchMethodError` with nothing behind it to raise it for. It now reads
`NativeMethodRegistry::drops_real_layout_synthetic()`, whose own doc comment
already said a `#[cfg]` guard "is NOT equivalent and must not be used for this".
`vm_init` sets that flag before every caller of the registrar — arm A at the top
of its real-JDK `else`, arm B immediately before
`register_essential_natives_with_shims`, which is what reaches
`jdbc::register_jdbc_service_loader`, the second caller the gate lives inside the
function for.

**Witnesses.**
`vm/tests/wp7_2_jdbc_core_types_reachable.rs::connection_methods_carry_signatures`
(the `String.length()` half) and
`vm/tests/wp1_8_real_jar_serviceloader.rs::driver_discovered_from_jar_on_classpath`
(the ServiceLoader half). Both were red on `dev` before this change and green
after; both are green under `--features synthetic-jdk` on either side of it,
which is what identified the defect as configuration-4-only rather than as a
JDBC or class-loading defect.

**Three places that were asking about a mode they were not in**, all now saying
which mode they mean instead of inheriting it from the build:
`wp8_10_9_string_contains_native.rs` booted `VmConfig::default()` and called the
result "a real-JDK registry" — it now boots `JdkMode::Real`, and skips loudly
when no JDK image is reachable rather than asserting nothing in silence;
`wp7_1_jdbc_driver_loader.rs::jdbc_driver_natives_export_service_loader` and
`jdbc.rs`'s two `service_loader_*` unit tests used bare registries with one
`#[cfg]` polarity each, and are now one pair compiled in both builds, each
setting the flag it is about.
