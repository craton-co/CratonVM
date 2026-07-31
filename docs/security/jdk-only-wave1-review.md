# JDK-only mode — wave-1 security review

| | |
|---|---|
| **Status** | Review record. Read-only pass over the landed wave-1 code against [`jdk-only-threat-model.md`](jdk-only-threat-model.md). |
| **Date** | 2026-07-31 |
| **Branch** | `feat/jdk-only-mode` (worktree `wt-jdk-only`) |
| **Normative source** | [`../feature-designs/jdk-only-mode.md`](../feature-designs/jdk-only-mode.md) |

## Method and limits

Static review only. The VM was **not built and not run** — no `cargo`
invocation of any kind — so every statement below is derived from the source as
it stands on this branch. Where a claim needs execution to settle, it is filed
under "plausible, unverified".

The code is judged against what wave 1 claims to do *now*: instrumentation and
measurement, with enforcement only in class fabrication (design §5) and
synthetic-native registration (design §4). It is not judged against the
finished feature.

`vm-cli/src/main.rs` was being rewritten by another agent during this review
(it shrank by ~700 lines mid-pass, briefly carrying dangling references to a
`write_native_census_json` that no longer exists, then re-landed delegating to
the writers in `vm/src/vm/vm_init.rs`). Launcher-side observations here reflect
the post-consolidation state. `jfr/src/jdk_only.rs` has no consumer in the tree
yet. Both are mid-edit, not defects.

---

## Confirmed findings, by impact

### 1. The threat model's fabrication invariant is stronger than the landed code — HIGH (documentation)

The threat model states, in the present tense and without a wave qualifier:

* §1 invariant 1 — "`ClassOrigin::CompatibilityStub` is never created."
* §2.2 — "Fabrication under `java/`, `jdk/`, `sun/` is refused categorically."
* §2.2 — "Authorisation becomes **origin-based**."

None of the three is true of wave 1.

**Two fabrication choke points, one of which enforces.** `ClassManager` funnels
all class minting through `admit_compatibility_class`
(`classloading/src/class_manager.rs`), reached from exactly two places:

* `create_synthetic_stub` — the whole `load_class` fabrication chain
  (enterprise prefixes, the `ProcessHandle` exemption, the residual
  "JDK-namespace name with no bytes" case). Under `JdkOnly` this **refuses**
  with `ClassNotFoundException`. This is the path §2.1 and §2.3 describe, and
  it works.
* `fabricate_class`, via `ensure_synthetic_class` — records the violation and
  **fabricates anyway, in both modes**. The source says so plainly
  (`JDK-ONLY-WAVE2:` block: ~70 callers across ~33 files, no error channel on
  the signature). `try_ensure_synthetic_class` is the enforcing sibling and has
  one documented caller family.

So a strict run still creates `CompatibilityStub` classes. Some are minted
during boot from `vm/src/vm/vm_init.rs` (the `cratonvm/internal/Unmodifiable*`
shapes, ~line 1224), and more from `native-builtins` / `native-collections` /
`native-io` allocation helpers.

**The privileged-package guard is unchanged.** The H5 check in
`classloading/src/class_manager.rs` (~line 4264) is still a name-prefix test
with loader/hidden/`privileged_define` exemptions. Nothing in it consults
`ClassOrigin`. §2.2's "authorisation becomes origin-based" describes an
intended end state, not this wave.

**Second-order consequence: the census number is not the breach count.**
`ensure_synthetic_class` hard-codes `ClassOrigin::compatibility_stub(...)` for
*every* class it mints, including shapes that `ClassOrigin::VmInternal` is
documented to cover ("`cratonvm/synthetic/*` allocation shapes"). The
`compatibility-stub` tally in `--dump-class-origins` therefore mixes genuine
fabrications with legitimate VM-internal shapes that simply have not been
migrated to `ensure_generated_class` yet. A reader of a shared census cannot
tell the two apart — which is the exact conflation the feature exists to end,
and the reason the wave-2 migration recipe is per-call-site.

**Why this is the top finding.** The threat model's own §5 says an invariant
that silently does not hold is worse than one that was never claimed. This
document is what a security reader reads; the wave-1 posture lives only in
design §10. Fix is documentation: qualify §1 and §2.2 with the wave-1
enforcement boundary, and state that `ensure_synthetic_class` records without
refusing.

**Not a code defect.** The design explicitly chose this split, and the
`JDK-ONLY-WAVE2:` markers make it mechanically findable.

### 2. The prose path redactor misses the shapes the fields it protects will carry — MEDIUM (latent)

`redact_absolute_paths` in `vm/src/vm/vm_init.rs` (~line 4610) is the redactor
for `--dump-class-origins`' `name` / `reason` / `requested_by` and for every
violation body in `--jdk-only-report` and `--trace-jdk-only`. Four gaps,
all confirmed by reading the scanner:

* **URLs are untouched.** `:` is not in the `boundary` set, and neither is `/`.
  So the `/` in `file:/opt/…`, the `/` in `file:///home/…`, and the `C` in
  `jar:file:/C:/Users/victor/.m2/repository/…!/…` are all preceded by
  non-boundary characters and never start a candidate run. A jar URL is a path
  by another name, and it is exactly what
  `ClassOrigin::{BootImage, ApplicationClassPath, UserDefined}::source` holds —
  including, for a remote or shared repository, an internal hostname.
* **UNC paths are not recognised as rooted at all.** The rooted test is `/` or
  `X:` + separator; a leading `\` matches neither. `\\build-01\share\jdk\…`
  survives verbatim, hostname included. Note the asymmetry: `types`'
  `is_absolute_path` *does* match `[b'\\', ..]`, and `redact_registration_site`
  normalises `\`→`/` before testing, so it catches UNC too. **Three redactors,
  two behaviours.** The prose one is the odd one out and is the one on the
  fields most likely to grow paths.
* **Only the first entry of a path list is redacted.** Neither `;` nor `:` is a
  boundary character, so in a `-cp`-shaped string every entry after the first
  is preceded by a separator that does not open a new run.
* **Drive-relative** (`C:app.jar`, no separator) and paths whose interesting
  component contains a space are partially or wholly missed; `types`'
  whitespace-token redactor has the same limit from the other direction
  (`C:\Program Files\Java\jdk-25` → `<redacted> Files\Java\jdk-25`).

**Not currently reachable.** Today nothing path-bearing lands in a
prose-redacted field: `ClassOriginEntry` carries no `source`;
`requested_by` / `requester` are hard-coded `None` with a `JDK-ONLY-NOTE`
saying the interpreter must supply them in a later wave; all four stub
`reason`s are path-free constants; and `JdkOnlyViolation::MissingBootClass` —
the one variant with a `searched_image` — has no producer outside tests. The
one field that *does* carry a source location, the native census'
`registered_by`, uses the robust `redact_registration_site` instead.

So this is a hole in a safety net, in the shape of the things wave 2 is
scheduled to drop into it. It matters because these files are designed to be
shared: the failure mode is a dump that *looks* redacted (`<redacted>/app.jar`
elsewhere in the same file) while a jar URL two lines down still names a home
directory or a build host.

### 3. The redactor is not JSON-escape-aware and can emit invalid JSON — MEDIUM (latent)

`json_string(value, verbose)` escapes first and redacts second
(`redact_absolute_paths(&json_escape(value))`). The scanner treats every `\`
byte as a path separator with no knowledge that some of them are escape
introducers. Worked example:

```
value        C:\a\b"x
json_escape  "C:\\a\\b\"x"
run scanned  C:\\a\\b\        (stops at the " terminator, taking the lone \ with it)
emitted      "<redacted>/b"x"
```

The `\` that was escaping the quote is consumed into the redaction, and the
quote becomes structural. Same class of failure for any odd trailing run of
backslashes adjacent to a path.

Unreachable today for the same reason as finding 2, and harmless in Compatible
mode. It is worth fixing before the fields are populated because the dumps are
machine-parsed — `difftest/src/census.rs`, the CI gates in
`.github/workflows/ci.yml`, `scripts/jdk-only-census.sh` — so a corrupted
artifact is a silent loss of the whole signal, not a visible error. Redacting
*before* escaping removes the class of bug; the `\\`-counting the current
ordering exists for is already handled by `redact_one_path`'s
consecutive-separator collapse.

### 4. `"violations": []` does not mean "no violations", and the JIT log is a process global — MEDIUM

Two problems in one place.

**Coverage.** `SharedVm::dump_jdk_only_report_json` unions exactly two sources:
`NativeMethodRegistry::refused_registrations()` and
`ClassManager::origin_violations()`. It does not read:

* `cratonvm_jit::jdk_only_jit_violations()` — the JIT's
  `NativeShadowsBytecode` records. That function's own doc says "Read by the
  launcher when building `--jdk-only-report`"; `grep` finds **no caller**.
* Any interpreter-time `Reject` from `resolve_dispatch` /
  `resolve_native_dispatch_wave1`. Those become `VmError::JdkOnly` and are
  surfaced (through JNI as a `java/lang/InternalError`), but are never appended
  to a log the report reads.

Net effect: `native-shadows-bytecode` and `synthetic-native-invocation` cannot
appear in the report today, and `"violations": []` means "no fabrication and no
refused registration this run" — a narrower statement than the field name
implies. On a shared artifact that is the most likely misreading. Either wire
the other two sources in, or say so in the schema doc next to the field.

**Process globals.** `jit/src/lib.rs` holds
`JDK_ONLY_VIOLATIONS: OnceLock<Mutex<Vec<JdkOnlyViolation>>>` plus two static
`AtomicU64` refusal counters. Design §2 is explicit: "There are no process
globals for this feature… A process global would break multi-VM-in-one-process
runs." In a multi-VM process these merge across VMs, and the `contains` dedupe
is cross-VM. This repo has a standing record of exactly that failure mode for
native caches.

### 5. `cratonvm_compatibility_mode`'s `-1` contract invites an unsound use — LOW (C ABI documentation)

The doc says `-1` is returned "on a null or **otherwise unusable** handle". The
only unusable handles `with_vm` actually detects are `NULL` and a re-entrant
borrow of the same handle; anything else is dereferenced. A destroyed handle is
a use-after-free.

That is **not worse than the existing entry points** — `cratonvm_load_class`,
`cratonvm_invoke_static`, `cratonvm_release_ref` all validate identically, and
`cratonvm_create_with_compatibility` adds no new handle lifecycle. The new part
is the wording. A cheap, read-back-shaped accessor with a sentinel return is
precisely the function a host will reach for as a handle-validity probe, and
that is the one use it cannot serve.

Two smaller notes on the same function:

* It is not side-effect-free. `with_vm` calls
  `set_jni_context_arc(handle.vm.shared.get_arc())`, so a "read the mode" call
  rebinds the calling thread's JNI TLS context.
* `cratonvm_compatibility_mode_supported` clears the thread's last error on
  entry and never sets one (documented). The natural host idiom — create fails,
  probe support, then read `cratonvm_last_error` — therefore discards the
  create's message. The two doc blocks currently recommend that sequence.

Suggested wording: "`-1` on a `NULL` handle or a re-entrant call. The handle
must be live; a destroyed handle is undefined behaviour, as for every other
entry point."

**The rest of the new ABI is clean** — see the clean-surfaces section.

### 6. The strict-boot failure is a panic, outside the redaction switch — LOW (documentation gap)

`require_jdk_image_for_jdk_only` builds an actionable `InvalidConfiguration`
message that wraps `require_real_jdk`'s per-probe account — i.e. the searched
JDK paths and the resolved `JAVA_HOME`. `SharedVm::new` surfaces it as
`panic!("{e}")` (deliberately; the constructor is infallible by signature).

Panic output goes to stderr unconditionally. It is not written to an
operator-chosen file, and it is not covered by `--explain-jdk-only` — there is
no redacted variant of it. In CI that puts the JDK layout and the build agent's
home directory in a log that is frequently public. The disclosure is deployment
layout only, and the operator generally *needs* those paths, so impact is low;
but the threat model's diagnostic-leakage row says "Reports are process-local
and written only where the operator asks", which does not describe this path.

Reachable from the launcher and from hand-built `VmConfig` embedders.
**Not** reachable from `libcratonvm`: `config_from_args_with_compatibility` →
`finish_config` → `validate_jdk_mode` runs `require_real_jdk` during argument
parsing and returns a clean `JdkModeUnavailable` first. (Worth noting the flip
side: if it *were* reachable, `create_vm`'s `catch_unwind` would replace the
whole actionable message with `"…: panic during VM bootstrap"`.)

### 7. The CI mitigation reads as landed when half of it is not — LOW (documentation gap)

Threat model §4 "False assurance in CI" mitigation: "Every strict job passes
`--jdk-only`, asserts the mode it ran in, and never retries under the fallback."

The no-fallback half holds: `scripts/jdk-only-census.sh` runs `real` and
`strict` as two separately labelled legs and never retries one as the other,
and the gates read the policy-suffixed filenames.

The blocking half does not. The `jdk-only` job in `.github/workflows/ci.yml`
carries `continue-on-error: true`, which design §11 requires it eventually not
to. The workflow states this explicitly, with both reasons and the promotion
condition, so this is not a hidden gap — but the threat model's mitigation
column should say "advisory during preview" rather than implying a live gate.
Note that the zero-compatibility-class gate is the one that finding 1 predicts
currently fails, and `continue-on-error` is why nobody sees it.

---

## Surfaces that are clean

Stated positively, because a short list of real findings is only useful next to
what was checked and found sound.

**Telemetry label cardinality — clean, and structurally so.** This was the
surface most likely to leak and it is the best-defended thing in the wave.
`jfr/src/jdk_only.rs`: every label-producing function
(`violation_label`, `class_origin_label`, `native_kind_label`,
`generator_label`, `module_label`) returns `&'static str` sourced from a `const`
table in that file, so the borrow checker refuses to let a caller-owned
`String` become a label. Counters are fixed-size arrays indexed by closed
vocabularies. The one input that arrives as a free `String` — `module` — is
collapsed through a 66-entry JDK allow-list to `other` / `unknown`, so an
application module name, a `-D` value, or a path mistakenly passed as a module
all come out as four characters. `NativeCensusSample` **omits** `class`,
`name`, `descriptor`, `registered_by` and `overwrote` rather than carrying and
ignoring them, so no later edit can start emitting them by accident. Opt-in is
by CLI flag only; `enabled_by_args` does not read `std::env`, so an embedded VM
cannot be switched on by the host process's command line, and
`types/src/flag_groups.rs` asserts JDK-only adds no `CRATONVM_*` variable. I
could not find a path from any workload-controlled string to a label.

**`registered_by` redaction.** `redact_registration_site` normalises `\`→`/`
before testing rootedness, so it covers POSIX, drive-qualified and UNC forms
uniformly and collapses to `<redacted>/<file>:<line>` — enough to find the
registration, nothing about the machine. `class` / `name` / `descriptor` are
correctly *not* redacted: program identity is the entire point of that artifact.

**The C ABI's policy plumbing.** `compatibility_mode_from_abi` rejects an
unknown integer rather than clamping it to the loose default, so a host built
against a newer header is told instead of silently getting `Compatible`.
`cratonvm_compatibility_mode_supported` needs no VM, so a host can probe before
paying for a failed create, and `dlsym` covers libraries predating the symbol.
`CRATONVM_COMPATIBILITY_COMPATIBLE` together with a `--jdk-only` option string
is a contradiction **error** rather than a precedence rule — the right call,
because a silent winner means running under a policy neither half of a
split configuration asked for. `ignoreUnrecognized` does not soften it.
`DEFAULT_COMPATIBILITY_MODE` is never inferred from a Cargo feature, from
`CRATONVM_REAL` / `CRATONVM_NO_STUBS`, or from what the host machine has
installed. Ordering in `finish_config` — coherence before availability — is
also right: a flag conflict should not be reported as a missing JDK.

**The last-error string carries nothing sensitive from the new paths.** The two
new variants are `UnknownCompatibilityMode` (an integer and two constants) and
`InvalidCompatibility` (the `VmError::InvalidConfiguration` text verbatim,
which names flags, not the host). The entry-point prefix is a function name.
Pre-existing variants can echo an option string (`UnrecognizedOption`) or a JDK
search account (`JdkModeUnavailable`); neither is new in this wave and neither
is reached by the new entry points in a way the old ones were not.

**`resolve_dispatch`.** Compatible mode is a pure function of the call site's
pre-existing decision (`compat_native_wins`), never inspects the kind and never
produces a `Reject`, so a default run cannot observe that the resolver exists
and strict mode is never *less* strict than compatible. The one documented
deviation from design §7's literal order — a `Code`-less method with a
registered native dispatches to it — is genuinely a no-shadowing case
(`method.code()` is `None`, so §1.4 has nothing to protect) and still refuses
`SyntheticStub` under `JdkOnly`. The wave-2 exception lists are marked
`JDK-ONLY-WAVE2:` throughout, as the contract asks.

**The registry refusal path.** `register()` checks policy *before* insertion, so
a refused triple never enters `registrations` / `categories` / `provenance` /
`slots` and `generation()` does not move — the census cannot show a native that
was refused. The `#[track_caller]` provenance string is built only on the
refusal path, not per registration. The `CRATONVM_NO_STUBS` arm is left
byte-for-byte alone and kept separable, with distinct debug tags, which is
correct: the two mechanisms answer different questions.

**The JNI-visible error.** `raise_jdk_only_violation`
(`vm/src/native/jni.rs`) throws `java/lang/InternalError` carrying
`JdkOnlyViolation::summary()`. Of the seven variants only `MissingBootClass`
puts a path in its summary, and that variant has no producer; every
dispatch-reachable variant carries class, method and descriptor only. So the
string application code can catch and read discloses VM-internal *naming*, not
machine state. That is an acceptable channel, and it is the right message to
give a program that has to act on the refusal.

---

## Plausible, unverified

Each needs a build or a run, which this review could not do.

* **Does a strict run's census actually show `compatibility-stub > 0`?**
  Finding 1 predicts yes, from reading `ensure_synthetic_class` and its boot
  callers. The CI gate that asserts zero in `classes-strict.json` should
  therefore fail today; `continue-on-error: true` means it would not be
  noticed. Confirming this is one `scripts/jdk-only-census.sh` run.
* **Does `core::panic::Location::file()` ever yield an absolute path here?**
  All ~3,100 registration sites are in-workspace, where cargo normally passes a
  relative path, so `registered_by` is probably always workspace-relative and
  the redaction never fires. A vendored build, an absolute `--manifest-path`,
  or a future out-of-workspace registrar would change that.
  `redact_registration_site` covers it either way; I could not measure whether
  it fires.
* **Can any `ensure_synthetic_class` caller be steered to a Java-chosen class
  name?** Most of the ~70 sites pass a `const`. Several do not — e.g.
  `alloc_object_or_synthetic` in `vm/src/vm/vm_exec.rs` (two sites),
  `native-collections/src/lib.rs`, `native-io/src/lib.rs`, and
  `atomic_updater::alloc_impl`. If any of those names is derived from
  application input, a strict run can be induced to mint a `CompatibilityStub`
  under a name the application chose — which is the privileged-package question
  §2.2 raises, arriving through the non-enforcing door. I traced two sites and
  did not settle it. This is the per-call-site audit wave 2 already schedules;
  it should be treated as security-relevant, not only as a migration chore.
* **Is `record_invocation` called on every dispatch path?** The census'
  `invocations` column and the report's `synthetic_stub_invocations: 0` are
  only as strong as that coverage. An undercount makes a zero mean less than it
  reads. Not audited.

## Not checked

* Anything requiring execution: real redaction output, dump file permissions
  (`std::fs::File::create`, so umask default — a shared CI workspace gets
  `0644`), whether the JNI refusal path is reachable in practice.
* `vm-cli/src/main.rs` beyond the post-consolidation dump wiring, because it was
  changing during the pass.
* `difftest`, `regression-suite/run.sh` and the blocker tooling, except where
  they consume a dump schema named above.
* The `native-builtins` 157-stub reclassification, which design §8 defers to its
  own wave.

---

## Verdict

The mechanisms the wave actually built are sound: the telemetry label surface
is closed by construction, the new C ABI rejects rather than clamps and never
infers strictness, the dispatch resolver preserves Compatible bit-for-bit, and
the registration refusal is correctly placed before insertion. The threat
model's *negative* claims — §3's list of what the mode does not secure — match
the code exactly, and the document's refusal of isolation vocabulary is
well-judged and should stay.

The gap is in the threat model's *positive* claims. §1 and §2.2 describe the
finished feature in the present tense, and wave 1 closes one of two fabrication
routes and leaves privileged-package authorisation entirely name-based. The
correction is documentary, not a code change — but it should land before this
document is read as a statement about a shipping flag.
