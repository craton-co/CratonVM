# Deep audit of `main`, 2026-07-30 — disposition

The audit was a bad-only sweep of `main`: 20 items across code/architecture,
security, tests, documentation, and performance. This file is its retirement
record. Every item below states what actually changed and, where a claim could
not be verified from this host, says so instead of implying it was.

Landed on `fix/deep-audit-retire-20260730`, merged to `dev`.

## The finding the audit did not make, and should have

`cargo fmt --all --check` is step 1 of CI's `build-and-test` job with no
soft-fail, and it reports **1,050 diffs across 1,008 files** on `origin/dev`.
The job fails before it compiles anything.

Behind that, `cargo test --workspace` did not build at all, in four independent
places, and underneath *that* sat ~48 failing tests including a JIT regression
set bisected to the 2026-07-27 → 2026-07-30 window.

So the audit's recurring complaint — advisory gates, unenforced thresholds,
claims outrunning evidence — understated the problem. The gates were not merely
soft; the two hardest ones were already red, which is why everything else could
drift. Details:
[`../jit-regressions-hidden-by-unbuildable-test-targets-20260730.md`](../jit-regressions-hidden-by-unbuildable-test-targets-20260730.md).

## Critical code and architecture

**P0 — Spring AOT class-loader identity.** `lookup_define.rs` no longer lets the
class manager pick a generated class's superclass by name.
`resolve_lookup_supertypes` resolves the direct supertypes through the *lookup
class's* initiating loader and passes the identities as
`DefineClassOptions::superclass_id_override` / `interface_id_overrides`, which
`define_class_full` validates against the class file's own names (a mismatch is
an `IncompatibleClassChangeError`, never a silent substitution). All three
`Lookup.define*` paths request `force_loader_faithful_linking`. Unit-tested.
**Not** validated against `MockitoSpyBeanAndSpringAopProxyIntegrationTests` —
that needs the Azure host, and the Spring bug list records the outstanding
evidence rather than claiming closure.

**P0 — Redefinition caching. Landed, and measured as insufficient.** The
process-wide `any_class_redefined()` flag is now only a cheap negative fast
path; positive decisions are scoped to an exact class or receiver hierarchy
(`vm/src/runtime/redefine_state.rs`). The JIT dispatch caches, the
Integer/Object/HashMap native caches, the subtype cache, the lambda caches and
the deopt-resume stash all key off the exact class instead of quiescing
globally. `VtableManager::install_vtable` refreshes inherited entries in
already-linked subclasses on reinstall.

Then the audit's own acceptance criterion was run — `SbCostProbe`, the 90-second
reproducer from the known-issue doc, on Mockito 5.21 and JDK 25:

| | before mock | after mock | multiplier |
|---|--:|--:|--:|
| HotSpot 25 | 2 ns/call | 50 ns/call | 25× |
| CratonVM | 1,822 ns/call | 245,653 ns/call | 134× |

The multiplier improved from the original 451×, so the gate narrowing does
something real. The bug is not closed: 245 µs against HotSpot's 50 ns is ~4,900×.
The known-issue doc stays OPEN and now records that "the caches were keyed too
coarsely" is not the whole story.

**P0 — Default synthetic runtime surface.** Real ForkJoinPool and real
`java.net` sockets are now the default, with `CRATONVM_SYNTHETIC_FORKJOINPOOL` /
`CRATONVM_SYNTHETIC_NET_SOCKETS` as explicit opt-outs (real AQS and real RAF
already were; the audit's claim there was stale). The stub ratchet is now exact —
157 of 9,320 registrations, zero slack — and CI runs the whole `synthetic_diff`
suite rather than five named cases.

The flip left a live bug behind it. It was **filed here, and has since been
fixed** — see the resolution note at the end of this item.
`vm/src/runtime/env_cache.rs` answers "are we in the real ForkJoinPool lane?" by
testing whether `CRATONVM_REAL_FORKJOINPOOL` is *present*. Once real
ForkJoinPool became the default that variable is normally unset, so the reader
returns false on exactly the configuration that is the real lane — and the GC
root-snapshot cache bypass it guards has not fired on a default run since.

Making the predicate honest is a two-line change and it is the wrong one: the
flag is default-true, so the bypass would fire always, the frozen-frame cache
would be dead on every run, and
`root_snapshot_cache_tests::local_write_invalidates_cached_deep_frame_roots`
fails. That was measured, not predicted — the change was made, the suite caught
it, and it was reverted. Choosing between "the hazard is universal now, retire
the cache" and "the hazard was specific to the opt-in lane, narrow the trigger"
needs GC-stress evidence nobody has.

**RESOLVED 2026-07-31** (`fix/rootsnap-cache-trigger-20260730`): the evidence was
gathered and it chose neither. A new default-inert verifier
(`CRATONVM_DBG_ROOTSNAP_VERIFY`) re-scans every frame the uncached way after each
cached snapshot and reports any root the cached snapshot lacks; across the
real-lane `Fork6`/`Fork6Hard` GC-stress repros it reported none, over 25,600
verified snapshots on the largest run, with the cache engaged on every one of
them. The mechanism: this lane's Bridge natives run every ForkJoinTask inline on
the submitting thread (`COMPUTE_THREADS=[main]` against HotSpot's 15 workers), so
the worker-frame hazard the bypass described has no thread to occur on. The
bypass was removed rather than repointed or replaced with a narrower trigger, and
a regression gate — `env_cache::tests::no_presence_predicate_shadows_a_compound_flag_default`
— now fails the build if any presence predicate names a variable whose flag
default is compound. Full writeup:
[`../rootsnap-cache-bypass-lost-its-trigger-RESOLVED-20260731.md`](../rootsnap-cache-bypass-lost-its-trigger-RESOLVED-20260731.md).

The lesson generalises past this one flag: after a default flip, every
*presence* test on the old opt-in env var is a suspect, because it silently
starts answering "was this requested?" when the caller asks "is this active?" —
two questions with the same answer right up until the flip. The full sweep
(51 `cached_is_set!` predicates, ~209 `runtime_var_os(…).is_some()` sites) is in
the resolution note; this flag was the only live instance, and the gate test now
keeps it that way.

**P1 — Experimental features out of the default build. Partly rejected, with
evidence.** `vm`'s default set is `["awt", "experimental-jmx"]`. A new
`experimental-features` CI job compiles and runs the optional surface, because
this repository has already lost 1,522 tests to a configuration nothing
compiled, and requests the build cannot honour (`-XX:AOTMode`,
`-XX:SharedArchiveFile`) now warn instead of silently no-opping.

The job's first version claimed to cover "the whole optional surface" and did
not: `experimental-t19-diag`, `app-stubs` and `synthetic-quarkus-arc` were
compiled by *nothing in the repository* — the exact rot the job exists to
prevent, reintroduced in the commit that added the job. All three are now in it,
verified compiling locally before the claim was made. `synthetic-jdk` has its
own job; `gpu-offload` needs hardware and is covered by `cuda-bridge.yml` and
the self-hosted `gpu-selfhosted.yml`; `legacy-synthetic-crypto` is now checked
here as well as through the fuzz build.

`experimental-jmx` had to go back. The audit item reasons from the feature's
*name*; what it actually gates is the `sun.management` native surface, and
`java.lang.management.ManagementFactory` is core JDK API that the JDK's own
bootstrap reaches. Dropping it made `getMemoryPoolMXBeans()` fail with
`UnsatisfiedLinkError: sun/management/VMManagementImpl.getVersion0()` —
caught by `vm/tests/wave1_a_jmx_mxbeans.rs`, which is the only reason this was
noticed rather than shipped. Renaming the feature is the honest fix; removing
it from the default build is not. `experimental-tls` is similarly misnamed: it
is a no-op alias and the TLS implementation is always compiled.

**Follow-up landed 2026-07-30.** Both renames are done: `experimental-jmx` →
`management` (it gates the `sun.management` natives behind
`java.lang.management` *and* the `javax.management` beans, so plain `jmx` would
have been too narrow a name in the other direction), and `experimental-tls` →
`deprecated-noop-tls`, which has zero `#[cfg(feature = ...)]` sites anywhere in
the tree. Both old names remain as back-compat aliases — a feature that simply
vanishes breaks downstream builds silently — and CI resolves the aliases in
their own step, so a broken alias fails here rather than downstream. Removal is
slated for 0.4.

Worth stating plainly, since it is the second time on this branch: an audit item
that reasons from a name rather than from what the code does will produce a
change that looks like cleanup and is a regression.

**P1 — Giant files.** Split at the section banners the files already carried
(measured 2026-07-30; "before" is the unsplit parent at the commit the split was
applied to, not at the audit's snapshot):

| file | before | after | children |
|---|--:|--:|--:|
| `vm/src/runtime/interpreter.rs` | 50,640 | 24,199 | 26,598 in 4 files |
| `jit/src/x64.rs` | 44,310 | 36,346 | 8,216 in 10 files |

Nothing moved between modules. On visibility the accurate statement is narrower
than "nothing became more public": 273 items went from private to `pub(super)`,
because a helper the parent used must be reachable from the child. Nothing left
the module and nothing left the crate — no `pub(crate)` became `pub`, no private
item became `pub`. Each child is `mod x; pub use x::*;`, and a glob re-export
caps every item at its declared visibility; the one exception is
`x64/cpu_features.rs`, re-exported through a named list because
`jit/tests/intrinsic_crc32.rs` consumes those seven queries directly.

`interpreter/invoke.rs` is still 23,316 lines because method invocation
genuinely is one subsystem. `hot_files_have_no_production_panics` — a unit test
in `interpreter.rs` itself, not in `vm/tests/` — enumerates both split
directories from disk at strict zero, and asserts it found files in each, so the
enumeration cannot silently cover nothing. A gate that kept scanning only the
parent would have converted the refactor into "silently stopped checking 26,000
lines"; the same trap caught `t11_1_interpreter_safety_comments`, whose reported
coverage the split moved from 84% to 80% without a line of `unsafe` changing.

**P1 — Lint policy.** Partially addressed. `suspicious_open_options` and
`cast_slice_from_raw_parts` are `deny` workspace-wide, and `native-io`,
`native-awt`, `cuda-bridge` and `jfr` deny `undocumented_unsafe_blocks`,
`missing_safety_doc` and `not_unsafe_ptr_arg_deref` at their crate roots. The
broad `dead_code` / `unused_*` / `unwrap_used` / `expect_used` allows remain:
flipping them tree-wide is thousands of sites and would land as an unreviewable
sweep. The per-crate root-level `deny` is the pattern to extend.

## Security and safety

**P0 — Product posture.** `SECURITY.md` and `docs/SECURITY_HARDENING.md` now
agree on one threat model. "Certified profile" is gone;
`CRATONVM_CONFINE_IO` is the confinement profile and `CRATONVM_UNTRUSTED_CODE`
the strict defence-in-depth profile, both fail-closed — the latter used to warn
and continue, which is a silent sandbox gap under a flag with that name.

**P0 — SecurityManager and FFI bypass.** `CRATONVM_UNTRUSTED_CODE` implies
`CRATONVM_REQUIRE_POLICY` and unconditionally rejects host-native access.
`System.loadLibrary`, `Runtime.load*`, Panama/FFM downcalls and `SymbolLookup`
all route through one `check_host_native_access_or_throw` gate. The docs still
say plainly this is not an in-process sandbox.

That last sentence was written before it was true. An independent re-read of
this file against the code found **four natives reaching `find_native_symbol`
with no gate at all**, all of them in the always-compiled essential registry:

| native | why it matters |
|---|---|
| `NativeLibrary.findEntry0` | its `handle` is caller-supplied and is only `lib_index + 1`, a small dense integer — walk 1, 2, 3, … and `dlsym` any library loaded by anyone in the process |
| `SymbolLookup.loaderLookup` | hands back a lookup with `lib_index == -1` |
| `SymbolLookup.find` | `-1` means "search every loaded library", so `loaderLookup().find(x)` is an arbitrary-symbol address oracle |
| `JavaLangAccess.findNative` | the same oracle reached through `SharedSecrets` |

This is address disclosure, not code execution — invoking a downcall still
passes `require_native_access`. It matters under a SecurityManager policy that
withholds `loadLibrary.*`: the load side was closed while the read side stayed
open. All four now pass the same gate as the load paths, which is
default-permissive, so trusted callers are unaffected.

The gate is `native_symbol_lookups_all_pass_the_host_access_gate`, an
all-or-nothing source scan over both capabilities — each of the four natives was
individually plausible and only the *set* was wrong, so the assertion has to be
over the set. It was verified to fail on removal (deleting one gate names that
exact file and line) and carries vacuity guards on both the file count and the
call-site count.

Worth stating because it is the counterpart to the `experimental-jmx` lesson
below: **that item was wrong in the safe direction and this one was wrong in the
unsafe direction, and both came from writing down what the design intended
rather than what the code did.**

**P1 — Duplicated crypto trust paths.** RSA PKCS#1 v1.5 verification is now one
implementation in `native-builtins-crypto::signature`, shared by the JCA path
and the signed-JAR verifier. The audit's description of the JAR signer was
stale: ECDSA P-256/P-384, DSA and RFC 5280 path validation were already
implemented; `SECURITY.md` now matches the code.

**P1 — Unsafe annotation and a UB gate.** The four peripheral crates enforce
`undocumented_unsafe_blocks` at their roots, and CI runs Miri against
`cratonvm-types`. Described as what it is — a focused gate over the core value
and pointer representation, not a claim that Miri can execute the JIT or driver
FFI.

The gate earned its place immediately. Run for the first time, it rejected
`heap_types::tests::field_cell_layout_matches_value_enum`:

```
constructing invalid value of type [u8; 16]: at [8],
encountered uninitialized memory, but expected an integer
```

The test did `transmute::<Value, [u8; 16]>`, and `Value` is an enum with
padding — claiming every byte is an initialised integer when the padding is
not. Fixed by transmuting to `[MaybeUninit<u8>; 16]`, which is always sound,
and only `assume_init`-ing the discriminant and payload ranges the test exists
to pin. That is a live example of the audit's point: a gate added but never run
is not a gate, and this one found something on the first attempt.

Re-run after the fix, it is clean — 63 tests, 0 failures. The job is scoped to
`heap_types` rather than the whole crate because the crate-wide run aborts
partway through on Windows: `intern::tests::concurrent_intern_*` spawns threads
and Miri stops with "can't call foreign function `GetModuleHandleA`", an
unsupported-operation error rather than UB. A gate that cannot be run locally
before pushing is the failure mode this audit is about, so the step is narrowed
to what is verifiable everywhere, with the widening procedure in its comment.

The repository's own `unsafe`-annotation gates were red the whole time, on this
branch and on `dev`: `t11_1_interpreter_safety_comments` at 89/105 and
`t11_1_helpers_safety_comments` at 88/100, against a 90% threshold. Both are now
green at 105/105 and 96/100. Most of the gap was blocks that already stated
their invariant in prose but did not use the literal `// SAFETY:` token — the
six `StopTheWorldToken::new()` sites all argue the STW invariant immediately
above. Two mechanical traps are worth knowing: the gate looks back exactly five
lines, so a *correct* comment longer than five lines scores as absent, and it
matches `contains("unsafe {")`, so mid-expression blocks count. One genuine
defect fell out — the SAFETY contract for `virtual_dispatch_target_for_receiver`
had been orphaned onto `cv_trace_enabled`, a safe fn about an env-var probe,
leaving the `unsafe fn` undocumented and the safe one carrying a
pointer-validity contract it has no pointers for.

**P1 — README claim.** "Memory-safe by construction" is gone from the README —
and from `docs/PRESENTATION.md`, where it had survived verbatim as a
customer-facing bullet ("whole categories of classic VM vulnerabilities are
designed out"), which is the same absolute assurance the finding asked to
retire, just relocated.

## Tests and coverage

**P0 — Coverage claims.** No percentage is claimed. Generation is blocking, the
artifact upload fails on a missing file, and `docs/COVERAGE.md` says the 85%
figure is not demonstrated. Rewriting `COVERAGE.md` alone was not enough: the
85% target survived in `.github/pull_request_template.md` — a checkbox every PR
author ticks — and in `docs/book/src/contributing/testing.md`, the published
contributor page. Both now match. (`docs/jck-compliance.md` keeps its 85%
figures; those are JCK area pass rates, a different metric.)

**P0 / P0 — Gates.** difftest, fuzz-build and coverage lost
`continue-on-error`; Miri and the Markdown link check were added; the stub
census became the exact ratchet; the `synthetic-jdk` compile checks stayed
blocking and an `experimental-features` job was added.

Each promotion was then actually run. Results:

| gate | promoted to blocking | verified |
|---|---|---|
| `Test native-builtins (synthetic-jdk)` | yes | **3,313 pass, 0 fail** — backed |
| Semantic differential gate | yes | failed on a stale ledger; ledger refreshed, now **exit 0** — backed |
| Exact stub ratchet | yes | **157 of 9,320, zero slack** — backed |
| Markdown link check | added | **164 files** — backed |
| Miri (`cratonvm-types --lib heap_types`) | added | **found real UB on its first run**, fixed; **63 pass, 0 fail** — backed |
| Fuzz build smoke | yes | failed; two real misalignments fixed, `cargo check --all-targets` in `fuzz/` now clean — backed as far as a non-Linux host can show |
| `Test vm (synthetic-jdk)` | yes | **not backed — reverted**, see below |

Two of these are worth their own note, because in both cases the *stated reason*
for the gate being advisory was wrong, and running it produced a better
diagnosis than the comment did.

**difftest.** It failed identically on `origin/dev`. The comment said the ledger
might not reproduce off the Windows host that captured it — it fails *on*
Windows. The real cause was staleness: CratonVM's `ClassCastException` message
was fixed from internal to source form (`java/lang/String` →
`java.lang.String`) after the 2026-06-21 capture, so the recorded divergence had
narrowed and the gate correctly flagged the change. Regenerating with the
documented procedure makes it clean.

**fuzz.** "The standalone fuzz workspace and native-builtins feature set are not
aligned" turned out to be two concrete things. `[patch.crates-io]` is not
inherited by a standalone workspace, so the fuzz build resolved stock `rustls`
instead of the vendored CBC copy that supplies the `KeyBlockShape::mac_key_len`
`t27_tls_cbc.rs` needs, and `cratonvm-native-builtins` did not compile at all.
And `legacy-synthetic-crypto` — which the fuzz crate enables for
`fuzz_tls_record` — referenced `crate::crypto::crypto_impl` at five sites, a
module that does not exist: a comment in `lib.rs` promised a `crypto.rs`
re-export shim for exactly that back-compat, the shim was removed, and the
comment and its callers were left behind. The feature had been unbuildable ever
since. Both fixed.

`Test vm (synthetic-jdk)` was made blocking on the assumption that the module's
old failure set had been worked through. It has not:

```
declared   3927 tests
reported   2584 (2523 pass, 61 fail)
then       exit code 1 with NO `test result` line
```

The harness aborts mid-run, so 1,343 tests never execute and 61 is a floor. A
step that cannot report a result cannot be a gate, so it is advisory again —
but now with the measurement, the abort named as the thing to fix first, and an
explicit order for re-promoting it. The honest caveat above still applies: with
CI step 1 red, none of these jobs has ever run to completion anyway.

**P1 — Stub ratchet.** 2000-with-16-slack became 157-with-zero-slack, and the
`docs/internal/stub-ratchet.md` the source pointed at now exists (as a redirect)
with the maintained copy public at `docs/contributing/stub-ratchet.md`.

The denominator needed pinning too. "157 of 9,320" is a ratio argument, and only
the numerator was asserted — the total was printed and never checked, behind a
`total > 100` vacuity guard that a wiring break dropping 9,000 registrations
would still have passed. The floor is now 8,000, low enough not to trip on
churn and high enough to catch a collapse.

**P1 — `native-builtins` integration tests.** `tests/registry_contracts.rs`
adds production-registry contracts and an all-or-nothing surface gate for
`StampedLock` — the shape that would have caught the missing
`tryConvertToOptimisticRead` that deadlocked the Keycloak boot through Agroal.
Still thin for a 550k-line crate; the pattern is the deliverable.

**P1 — Coverage/gating doc drift.** Fixed, including the trigger-branch
mismatch and the README's "the fast regression suite and performance gate are
the merge gates" claim.

## Documentation and scripts

**P0 — Broken doc graph.** `docs/RELEASE_READINESS.md`,
`docs/real-raf-segv-root-cause.md`, `docs/synthetic-vs-real-explained.md` and
`docs/internal/stub-ratchet.md` now exist.

`tools/check_markdown_links.py` originally covered the repository root, the
*top level* of `docs/`, and the mdBook tree — 98 files out of 1,644. Every
`docs/` subdirectory was unchecked, including `docs/contributing/`, which holds
the stub-ratchet procedure this same audit promotes as the maintained copy. The
default scope is now `docs/**` recursively, and it validates **164 files**,
exit 0.

`docs/internal/` stays excluded, deliberately: it is dated archival material
with ~396 broken links, most pointing at documents that were intentionally
deleted, and gating CI on it would pressure people to resurrect dead files.
`--all` still covers everything for an audit pass. Eight real breakages in the
newly-covered scope were fixed by repointing at the moved targets — four GPU
docs and two feature-design docs pointed into `docs/internal/` at paths that had
moved to `docs/internal/fixed-suite-bugs/`, and `docs/feature-designs/deopt-osr.md`
carried an absolute `file:///C:/craton/...` URL from someone's local machine.

**P0 — Link checker in CI.** Added to `build-and-test`.

**P1 — Build/script docs.** 20 → 22 crates; `build-gpu-driver.bat` →
`scripts/build-gpu.bat`; `scripts/README.md` rewritten as a structured index
covering `tools/`; the two Maven shims consolidated with
`scripts/sync-maven-java-shim.ps1` as a thin wrapper. `vm/src/lib.rs`'s
`docs/embedding.md` link fixed to `EMBEDDING.md`.

**P1 — Promote from `docs/internal/`.** `flag-census.md`,
`moving-young-throughput.md` and the divergence log (as
`differential-testing.md`) are public and indexed.

## Performance and product direction

**P0 — GPU claims.** "Transparent GPU offload … no annotations and no API
changes" is now "automatic GPU fast path" with the eligibility subset stated,
and the roadmap says plainly that there is no self-hosted hardware CI. Fixing
the README left the claim standing in the two most reader-facing files after it:
`docs/PRESENTATION.md` ("Java on the GPU. No annotations. No rewrites.",
"CratonVM asks for nothing", "No code changes") and `BENCHMARK.md` ("offload is
transparent … no annotations, no API"), plus `docs/gpu/COMPARISON.md` and
`docs/EMBEDDING.md`. All corrected: a `gpu-driver` build and an explicit `--gpu`
are required, nothing is needed *at the call site*, the eligible shape is stated
(static methods over primitive arrays in counted loops), and ineligible shapes
fall back to the CPU.

**P1 — Crate count and LoC.** "20 → 22 crates" was fixed in the build docs and
missed in three other files. All now state one measured figure with a
reproduction recipe: **1,349,978 lines across 702 `.rs` files in the 22
workspace members**, excluding `target/`, `fuzz/` and vendored code, measured
2026-07-30. `docs/book/src/introduction.md` had been claiming 880,000 — off by
35%. `docs/gc-tuning.md` described `gc/src/` as 24 files and ~29k LOC against an
actual 34 and 62,337.

**P0 — Published CPU regressions.** A `performance.yml` job runs the HashMap,
String/Regex and Binary Trees budgets against the CratonBench baselines, and the
roadmap orders that work ahead of broader totals. Three caveats the first
version of this line elided, all of which matter to anyone reading it as
coverage: only Binary Trees is *anchored* — HashMap and String/Regex are marked
`provisional`, which by the baseline file's own definition may be re-anchored
without an evidence doc. It is not a merge gate: the triggers are
`workflow_dispatch` and a twice-weekly `schedule`, with no `push` or
`pull_request`. And it requires a `[self-hosted, linux, cratonvm-perf]` runner
that `ROADMAP.md` says does not exist, so it has most likely never executed.

**P1 — Moving-young.** The audit asked for the optimization work to finish
*before* a default flip. The flip had already happened on `dev` —
`DEFAULT_MOVING_YOUNG` is `true`, decided by footprint (bt18 does not complete
at `-Xmx512m` without compaction), while README, ARCHITECTURE, ROADMAP and the
throughput note all still described it as opt-in. Corrected, with the three
residuals reframed as open work on the default path and the second gate kept
explicit: the flag being on does not mean a cycle compacted.

Fixing those four left two more behind — `docs/GC.md` still said "opt-in behind
`CRATONVM_MOVING_YOUNG`" (wrong twice over: it is the default, and that is not
the live variable, which is the opt-out `CRATONVM_NO_MOVING_YOUNG`), and
`docs/flag-census.md` recorded it as "opt-in (default OFF)". The census is
generated, so the census fix would have been reverted by the next run:
`tools/flag-census/render.py` hardcoded the stale sentence. The generator is
fixed, not just its output.

A default flip has now produced doc drift in six places and one live bug — the
GC root-snapshot bypass under **P0 default synthetic surface** below. That is
the pattern to watch, not any individual file.

**P1 — Invalidation costs / P1 — framework throughput.** Covered by the
redefinition work above and by `docs/framework-throughput.md`, which names the
workloads and the budget policy.

**P2 — Roadmap holes.** The roadmap now says the unresolved items are the
platform's ceiling rather than a wish list.

## What a reader should not conclude

A green checklist here is not a green build.

`cargo test --workspace --no-fail-fast` on this branch, after merging `origin/dev`
at `0477c8851` and rebuilding from a tree with no stale release binary:
**11 failing tests**, out of a suite whose `cratonvm-vm --lib` leg alone runs
about 2,450. For scale, `dev` plus only the four compile fixes measured 48 at the
start of this work.

Treat that 48 as an order-of-magnitude comparison, not a subtraction. It was
measured before `dev` moved and before the merge, and the earlier "39" quoted
here was measured against a **stale `target/release/cratonvm.exe` from before the
merge** — the test helper prefers a release binary over a debug one, so every
subprocess test in that run was validating pre-merge code. The number was wrong
and is withdrawn rather than adjusted.

One of the 11 is worth naming because it is not what it looks like:
`config_from_args_fails_loudly_when_no_jdk_is_available` passes alone and fails
in-binary. It asserts real behaviour only when it wins the race to initialise
the process-wide flag snapshot, so its harness's environment override is a
no-op the rest of the time. Pre-existing and order-dependent.

**FIXED 2026-07-30**, along with ten more sites in the same family — two of
which were live defects, not merely latent ones. `cratonvm_types::flags` now
carries a scoped override (`with_thread_overrides` / `with_process_overrides`)
that wins over the latched snapshot, and `types/tests/flag_env_mutation_guard.rs`
fails the build if a new `set_var` of a declared flag appears. Write-up:
[`../libcratonvm-no-jdk-test-order-dependent-fixed-20260730.md`](../libcratonvm-no-jdk-test-order-dependent-fixed-20260730.md).

The other 10 are the JIT `skip_list` / `conservative_roots` cluster, two
class-loader-unload cases, two ConcurrentHashMap cases and the attach-listener
socket — none of them touched by this work.

A twelfth failure existed and is gone:
`root_snapshot_cache_tests::local_write_invalidates_cached_deep_frame_roots` was
broken *by a fix made on this branch*, and is green again after reverting it.
See the ForkJoinPool item above. The suite catching a change that reasoned
correctly and acted wrongly is the system working, and it is the reason that
change is filed instead of shipped.

`cargo fmt --all --check` is still red, deliberately, and it is still step 1 of
CI. **Nothing in this file has ever run in CI**, because the job has not reached
a build. That is the single most important sentence here.

Tracked residuals: the formatting decision, the Spring CGLIB superclass
evidence, the `libcratonvm` test-isolation defect, the `Test vm (synthetic-jdk)`
harness abort, and the SbCostProbe gap (245 µs against HotSpot's 50 ns). The
ForkJoinPool bypass question is closed — see the resolution note in the P0 item
above.
