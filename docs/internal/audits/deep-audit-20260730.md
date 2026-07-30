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
[`../../known-issues/jit-regressions-hidden-by-unbuildable-test-targets-20260730.md`](../../known-issues/jit-regressions-hidden-by-unbuildable-test-targets-20260730.md).

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

**P1 — Experimental features out of the default build.** `vm`'s default set is
`["awt"]`. `synthetic-jdk` still implies `experimental-jmx` because the legacy
surface includes JMX bootstrap classes. A new `experimental-features` CI job
compiles and runs the whole optional surface, because this repository has
already lost 1,522 tests to a configuration nothing compiled. Requests the build
cannot honour (`-XX:AOTMode`, `-XX:SharedArchiveFile`) now warn instead of
silently no-opping.

**P1 — Giant files.** Split at the section banners the files already carried:

| file | before | after |
|---|--:|--:|
| `vm/src/runtime/interpreter.rs` | 50,532 | 24,091 |
| `jit/src/x64.rs` | 44,159 | 36,230 |

Nothing moved between modules and nothing became more public: each child is
`mod x; pub use x::*;`, and a glob re-export caps every item at its declared
visibility. `interpreter/invoke.rs` is still 23,224 lines because method
invocation genuinely is one subsystem. `hot_files_have_no_production_panics`
enumerates both split directories from disk — a gate that kept scanning only the
parent would have converted the refactor into "silently stopped checking 26,000
lines".

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

**P1 — README claim.** "Memory-safe by construction" is gone.

## Tests and coverage

**P0 — Coverage claims.** No percentage is claimed. Generation is blocking, the
artifact upload fails on a missing file, and `docs/COVERAGE.md` says the 85%
figure is not demonstrated.

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
| Markdown link check | added | **98 files** — backed |
| Miri (`cratonvm-types --lib`) | added | **found real UB on its first run**, fixed — see below |
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
`docs/internal/stub-ratchet.md` the source pointed at now exists, with the
maintained copy public at `docs/contributing/stub-ratchet.md`.

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
`docs/internal/stub-ratchet.md` now exist. `tools/check_markdown_links.py`
covers every root and `docs/` Markdown file and passes over 98 of them.

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
and the roadmap says plainly that there is no self-hosted hardware CI.

**P0 — Published CPU regressions.** A `performance.yml` gate runs the HashMap,
String/Regex and Binary Trees budgets against the anchored CratonBench
baselines, and the roadmap orders that work ahead of broader totals.

**P1 — Moving-young.** The audit asked for the optimization work to finish
*before* a default flip. The flip had already happened on `dev` —
`DEFAULT_MOVING_YOUNG` is `true`, decided by footprint (bt18 does not complete
at `-Xmx512m` without compaction), while README, ARCHITECTURE, ROADMAP and the
throughput note all still described it as opt-in. Corrected, with the three
residuals reframed as open work on the default path and the second gate kept
explicit: the flag being on does not mean a cycle compacted.

**P1 — Invalidation costs / P1 — framework throughput.** Covered by the
redefinition work above and by `docs/framework-throughput.md`, which names the
workloads and the budget policy.

**P2 — Roadmap holes.** The roadmap now says the unresolved items are the
platform's ceiling rather than a wish list.

## What a reader should not conclude

A green checklist here is not a green build. `cargo fmt --all --check` and
`cargo test --workspace` both fail on `dev` today, and this branch improves both
without finishing either — 39 failing tests against `dev`'s 48, none of them
new. The tracked residuals are the JIT regression set, the Spring CGLIB
superclass evidence, and the formatting decision.
