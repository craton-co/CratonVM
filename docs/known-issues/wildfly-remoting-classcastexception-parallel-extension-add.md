# WildFly boot: `ClassCastException: java.lang.Object cannot be cast to org.jboss.as.controller.AttributeDefinition` initializing `org.jboss.as.remoting` during parallel-extension-add

Status: **REOPENED 2026-07-18** — see "Recurrence 2026-07-18" at the bottom. The exact
`LifecycleException: ... exited unexpectedly with code [1]` crash signature reproduced again in a fresh
6-shard full-suite rerun (18 instances / 240 FAIL classes sampled) on a binary built from current dev,
which includes the `70154861` fix as an ancestor. Moved back to `docs/known-issues/` accordingly.

Prior status (preserved for history): FIXED — 2026-07-13

Date observed: 2026-07-13, Azure worktree `test/wildfly-full-suite-20260707`, dev@e8f36c78 (round-6e binary,
`frozen-cratonvm-wildfly-bugbash-v6-20260713`).

Date fixed: 2026-07-13, isolated worktree `/data/data/wt-remoting-cce-20260713`, branch
`fix/wildfly-remoting-cce-20260713`, forked from `origin/dev @ d706ac4d`.

## Symptom (unchanged from original report)

During WildFly standalone boot's `parallel-extension-add` step (every extension — remoting, undertow,
elytron, connector, clustering, etc. — is added concurrently, ~30 threads), some extension's own
extension-add handler throws:

```text
Caused by: java.util.concurrent.ExecutionException: java.lang.ClassCastException: java.lang.Object cannot be cast to org.jboss.as.controller.AttributeDefinition
	...
Caused by: java.lang.ClassCastException: java.lang.Object cannot be cast to org.jboss.as.controller.AttributeDefinition
```

Because `parallel-extension-add` treats all ~30 extensions as one atomic operation, this rolls back
*every* extension and the server exits via `WFLYSRV0056`/`System.exit(1)`.

## Root cause

**Not specific to `org.jboss.as.remoting` or to `AttributeDefinition`.** A live repro during this
investigation hit the byte-for-byte identical exception shape against `org.wildfly.extension.elytron`
(`frozen-cratonvm-remoting-cce-baseline-v1.bin`, attempt 6 of a 15-attempt batch — see Verification). The
real defect is a generic, JIT-compiled `checkcast`/`instanceof` bug in CratonVM's VM core, in
`vm/src/jit/helpers.rs`:

`jit_checkcast`/`jit_instanceof` (the native helpers a JIT-compiled `checkcast`/`instanceof` bytecode
instruction calls out to) resolve their receiver's `ObjectRef` once at entry, then hand it — and, in
`jit_checkcast`'s case, the caller-supplied raw pointer argument itself — to the shared
`jit_typecheck_resolve` helper. When the checkcast/instanceof **target class has not yet been loaded**
(the common case the very first time any thread checks an object against a given interface/class — e.g.
`org.jboss.as.controller.AttributeDefinition` the first time any extension's registration code runs),
`jit_typecheck_resolve` calls `vm.load_class_concurrent(class_name)` to load it on demand. That call
parses and defines the target class — allocating its `Class` mirror, constant-pool strings, and
method/field metadata, and potentially running a user classloader's `loadClass`/`findClass` bytecode — any
of which can trigger a moving GC.

Unlike the interpreter's own `Checkcast` opcode handler (`vm/src/runtime/interpreter.rs`), which pins the
receiver in the current thread's `native_pin_roots` around this exact class-resolution call and re-reads
the (possibly-updated) address afterward, `jit_checkcast`/`jit_instanceof`/`jit_typecheck_resolve` carried
the receiver's `ObjectRef` across `load_class_concurrent` **unpinned**. If a GC landed during that call —
overwhelmingly likely under WildFly's `parallel-extension-add` boot step, where ~30 extension threads are
all allocating/classloading simultaneously — the receiver's old address could already have been reused by
the time the fallback checks ran, or by the time `jit_checkcast` returned its "successful cast" pointer to
the JIT-compiled caller. Whatever the collector later placed at that stale address (typically a bare,
not-yet-initialized `java.lang.Object`) is what a subsequent read of the "same" reference observed —
exactly matching the reported `java.lang.Object cannot be cast to X` shape, for whatever `X` happened to be
the first-ever checkcast target resolved under that thread's particular GC timing.

This is a member of the same general "stale native `ObjectRef` across a GC-triggering call" bug family
documented in `docs/internal/wildfly-parallel-boot-stale-objectref-residual.md` and
`docs/internal/native-stale-local-family-and-persistent-singleton-roots.md`, but in the **JIT-compiled**
`checkcast`/`instanceof` path rather than a `native-builtins`/`native-collections` native method — none of
that prior sweep's tooling covered `vm/src/jit/helpers.rs`, so this specific site was never flagged.

## Fix

`fix/wildfly-remoting-cce-20260713`, commit `70154861aba90ea5fad145856f87bfb6eab6364c`:

- `jit_typecheck_resolve` now takes `obj_ref: &mut ObjectRef` instead of `obj_ref: ObjectRef`.
- Around the `vm.load_class_concurrent(class_name)` call in the "target not yet loaded" branch, the
  receiver is pushed onto the current thread's `native_pin_roots` (obtained via `jit_get_current_thread()`,
  the existing per-thread TLS accessor JIT helpers use), the class load proceeds, and `*obj_ref` is
  refreshed from the pin and the pin released — mirroring the interpreter's `Checkcast` handler exactly.
  Every subsequent read of `*obj_ref` in the function (the `annotation_proxy_satisfies_target` fallback,
  the trailing array-kind checks) now observes the refreshed address.
- `jit_checkcast` and `jit_instanceof` now pass `&mut obj_ref` (a local, mutable copy) instead of the
  original by-value `ObjectRef`. Critically, `jit_checkcast` now returns `obj_ref.as_ptr()` (the
  possibly-refreshed address) on a successful cast, **not** the original `obj_ptr` argument — returning
  the stale argument was the direct cause of the JIT-compiled caller observing a dangling/reused pointer.

## Verification

**Baseline** (`frozen-cratonvm-remoting-cce-baseline-v1.bin`, unfixed): 15 isolated-repro attempts against
`apps/wildfly/testsuite/integration/basic` — 1 confirmed `ClassCastException: ... AttributeDefinition` hit
(attempt 6, against `org.wildfly.extension.elytron`, full stack trace captured), 12 hits of the separate
STW cross-thread-JIT-takeover hang (`docs/known-issues/...stw-jit-takeover-hang.md`, being fixed by another
concurrent session — expected, not a regression), 2 unrelated `OTHER` failures (a `mail`/logging
`rootLogger` NPE unrelated to this bug, and one SIGSEGV consistent with the same STW-takeover family).

**Fixed** (`frozen-cratonvm-remoting-cce-fixed-v2.bin`): 20 isolated-repro attempts —
**0 occurrences of `ClassCastException: ... AttributeDefinition`** (the exact bug this doc tracks; final
tally `CCE=0 HANG=15 OK=0 OTHER=5 / 20`, confirmed via a literal grep for the exact message across every
attempt's captured output). 15 of the 20 hit the separate STW cross-thread-JIT-takeover hang (expected, not
a regression). Of the 5 `OTHER`: 2 were a related-but-distinct `ClassCastException: java.lang.Object cannot
be cast to org.jboss.as.controller.registry.AttributeAccess` (see Related, below — not this fix's target
and not eliminated by it), 2 were SIGSEGVs consistent with the same STW-takeover family, 1 was an unrelated
`rootLogger is null` logging-bootstrap NPE also seen once in the baseline batch.

`cargo test -p cratonvm-vm --lib` (debug, matching project convention): 2198 passed, 12 failed, 111 ignored.
All 12 failures (8 `jit::skip_list::tests::*`, `runtime::interpreter::tests::buffered_input_stream_force_native_covers_constructors_and_io_surface`,
`runtime::interpreter::tests::hot_files_have_no_production_panics`, `vm::vm_init::tests::ensure_system_streams_creates_objects`,
`vm::vm_init::tests::real_jdk_mode_registers_fewer_natives`) reproduce byte-for-byte identically
(`git stash` the fix, rerun just those 12 tests: 42 passed, 12 failed) — confirmed pre-existing and
unrelated to this change, none touch `checkcast`/`instanceof`/JIT typecheck code.

`cargo test -p cratonvm-native-builtins --lib`: 2974 passed, 12 failed, 6 ignored — this crate does not
depend on the modified function; failures are pre-existing by construction.

## Related

[[wildfly-standalone-managed-server-boot-fails-under-surefire-fork]] — the original, less-precise
documentation of this whole failure family.
[[wildfly-standalone-boot-stw-jit-takeover-hang]] — the dominant sibling failure mode hit during the same
boot step, a hang rather than a crash; fixed separately/concurrently by another session.
`docs/known-issues/wildfly-parallel-boot-stale-objectref-residual.md` — the broader stale-`ObjectRef`
bug-class sweep this fix's root cause belongs to, extended here to the JIT `checkcast`/`instanceof` helpers
in `vm/src/jit/helpers.rs`, which that sweep's tooling (scoped to `native-builtins`/`native-io`/
`classloading`) did not cover.

A distinct `ClassCastException: java.lang.Object cannot be cast to org.jboss.as.controller.registry.AttributeAccess`
was also observed once against the fixed binary during verification, with no accompanying STW-takeover
warning. `AttributeAccess` is a foundational WildFly-core class loaded far earlier than boot's
`parallel-extension-add` step, so its checkcast almost certainly takes `jit_typecheck_resolve`'s **fast**
path (target already loaded) — which has no `load_class_concurrent` call and so is not protected by this
fix.

**2026-07-13 follow-up investigation ([[wildfly-standalone-boot-attributeaccess-cce-register-invisible-root]],
`docs/known-issues/wildfly-standalone-boot-attributeaccess-cce-register-invisible-root.md`): CONFIRMED**
as a separate instance of the already-tracked "register-invisible JIT root" bug family (the `SB-CRASH-04`
residual within the default-on precise-JIT-oop-map machinery), not a new bug and not this fix's scope.
Confirmed from source that `jit_typecheck_resolve`'s fast path makes zero GC-triggering calls (so
staleness must originate upstream of the checkcast helper) and that Generational's young collector
never relocates objects while any JIT frame is active (so a moving-GC-evacuation mechanism is
structurally excluded for a JIT-required bug) — leaving the non-moving sweep's marking phase missing a
live root at a cooperative safepoint as the only mechanism consistent with the symptom, matching three
other independent 2026-07-10 occurrences of the same family (DoHead, `TestSwallowAbortedUploads`,
`TestAccessLogValve`). Live reproduction was inconclusive this session (host-contention-blocked, and a
separate, real "Family 1" stale-`ObjectRef` bug — see the new doc's "Separate finding" — dominated every
attempt instead), so this remains a code-analysis-based conclusion, not a fresh capture. Not fixed, per
this bug family's established policy — see the new doc for full reasoning and the fresh "Family 1"
finding, flagged separately as its own follow-up.

**⚠️ The "JIT-required" reasoning in the paragraph above is SUPERSEDED — see 2026-07-15 update below.**
The `AttributeAccess` doc's own later session disproved it with a live `--nojit` repro.

## 2026-07-15 update: investigation chain continued, `AttributeAccess` CCE generalized to `WFLYCTL0079`/any extension, four more real (but non-closing) bugs fixed along the way

The 2026-07-13 follow-up above was the start, not the end, of chasing this residual. Two further sessions
(2026-07-14/15) continued the live-repro side of the investigation the 2026-07-13 session couldn't
complete, on the same `docs/known-issues/wildfly-standalone-boot-attributeaccess-cce-register-invisible-root.md`
doc (now the authoritative, actively-maintained living doc for this whole failure family — read it directly
for full detail rather than this summary). Headline findings, most-recent-first:

- **The "JIT is required" claim is DISPROVED.** A `CRATONVM_DISABLE_JIT=1` repro batch hit the byte-for-byte
  identical `ClassCastException: java.lang.Object cannot be cast to X` shape 4/8 times — with zero JIT
  frames anywhere in the process. This directly contradicts the 2026-07-13 conclusion's central premise
  ("Generational never relocates while any JIT frame is active, so a moving-GC mechanism is excluded *for a
  JIT-required bug*") — since the bug isn't JIT-required, that exclusion doesn't apply. The residual is now
  characterized as a likely **cross-thread GC-root-visibility/timing race** among `parallel-extension-add`'s
  ~37-42 concurrently-executing mutator threads, not (solely) the JIT-specific `SB-CRASH-04` precise-oop-map
  gap originally named. Confirmed NOT just "one more unpinned site": live diagnostic instrumentation showed
  the panicking reads go through the codebase's own *pinned* path (`via_pin=true`) and still observe a stale
  address.
- **The bug is not `AttributeAccess`- or `remoting`-specific either** — it's the same generic
  `parallel-extension-add`-time CCE this doc already generalized once (from `remoting`/`AttributeDefinition`
  to "any extension/any target"). A later sighting surfaced as `WFLYCTL0079: Failed initializing module
  org.wildfly.extension.io`; a 12-attempt batch hit it against seven different extensions (elytron, jaxrs,
  undertow, infinispan, connector, plus `io` itself twice) with seven different cast targets. The failing
  module/exception target is circumstantial — whichever thread reads a corrupted address first.
- **Four separate, real, narrow bugs were found and fixed while chasing this residual — none of them close
  it, but all are genuine, verified, merged fixes in their own right:**
  1. TreeMap/TreeSet binary search, bulk ops, and submap/subset views held `owner`/`data`/`comparator`
     `ObjectRef`s unpinned across user-`Comparator` dispatch (`native-collections/src/lib.rs`) — commit
     `a4f3db9d`/`220ebb7d`.
  2. Stream `map`/`flatMap`/`collect` accumulation and `Comparator.thenComparing`/`comparing` construction
     had the same unpinned-across-dispatch pattern — commit `671c8df3`/`a969b18f`.
  3. **A genuinely unrelated regression, not this bug family at all**: `d8092acb` (2026-07-14) accidentally
     unmasked a previously-known-and-deliberately-shadowed gap where `ObjectName.getCanonicalKeyPropertyListString`
     (and sibling pattern methods) ran real bytecode against a synthetic-object model that never populated
     the fields those methods read, NPE-ing on the very first JMX MBean registration — this one **blocked
     WildFly boot entirely**, before it could ever reach `parallel-extension-add`. Bisected and fixed;
     `docs/internal/fixed-suite-bugs/wildfly-standalone-boot-objectname-ca-array-npe-FIXED.md` (commit
     `974c0838`/`32b6a2f1`, merged `78f17a93`/`317e4738`).
  4. `invoke_virtual`'s lambda-dispatch decision point (`vm/src/vm/vm_exec.rs`) read `receiver`/lambda
     `args` again after a SAM-compatibility check that can itself trigger class loading, without pinning
     across it — `docs/internal/fixed-suite-bugs/wildfly-invoke-virtual-lambda-sam-compat-stale-locals-FIXED.md`
     (commit `d64fab85`/`2ba6d7f3`).
- **Still OPEN.** Every fix above was verified not to change the residual's reproduction rate (matched
  before/after batches, ~5/12 both times for the lambda-dispatch fix specifically). Per this bug family's
  established policy, no further speculative per-site patch was attempted — the real fix is completing the
  precise-oop-map/shadow-stack infrastructure (`docs/feature-designs/precise-jit-maps-default.md`) and/or
  diagnosing the cross-thread pin-visibility race directly in the GC's cross-thread suspend/scan protocol.
  Whoever picks this up next should start from the `via_pin=true` finding in the `AttributeAccess` doc's
  2026-07-15 section, not re-chase individual unpinned-local sites — that avenue has now been tried
  repeatedly and each time found real-but-non-closing bugs.

## Recurrence 2026-07-18 — reopened

Fresh full-suite 6-shard rerun ("round 7", worktree `test/wildfly-full-suite-20260718`, `dev@7a939ec0`
base + a local fix for an unrelated same-day compile break in `native-io/src/socket_channel.rs`, binary
`frozen-cratonvm-wildfly-bugbash-v7-20260718`) reproduced this bug's exact crash signature:

- Broader sample (252/1548 classes completed before this check): of 240 FAIL classes, 18 (7.5%) show
  `LifecycleException: ... exited unexpectedly with code [1]` — the fast-crash path this doc documents,
  as opposed to the companion [[wildfly-standalone-boot-stw-jit-takeover-hang]]'s dominant "Could not
  start container" hang (199/240, 83%).

Not re-diagnosed to a specific extension/call site this pass (no per-class log inspection done yet to
confirm whether this is `org.jboss.as.remoting` again, `org.wildfly.extension.io.IOExtension` per the
2026-07-14 addendum's `CCE_CRASH` finding, or a new site) — reopening on the strength of the reproduction
count alone, consistent with `docs/known-issues/wildfly-parallel-boot-stale-objectref-residual.md`'s
"long-tail" characterization of this bug family (new sites keep surfacing after each fix). Whoever picks
this up next should pull the specific failing classes' logs from this round's output
(`/data/data/wt-wildfly-bugbash-20260718-runner/out/round7-s*of6-*/`) and grep for
`ClassCastException.*AttributeDefinition` to identify which extension is implicated this time before
assuming it's a reopened instance of the original `org.jboss.as.remoting` site.
