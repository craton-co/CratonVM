# WildFly boot: `ClassCastException: java.lang.Object cannot be cast to org.jboss.as.controller.AttributeDefinition` initializing `org.jboss.as.remoting` during parallel-extension-add

Status: **PARTIALLY FIXED 2026-07-19** — see "2026-07-19 session: one real producer found and fixed
(EnhancedQueueExecutor deferred-runnable queue), one residual manifestation confirmed still OPEN" at the
bottom. `08f8190da` (merged to dev as part of `59a3f38c5`) closes a confirmed-live, previously
self-documented-but-unfixed stale-`ObjectRef` producer and measurably drops the live reproduction rate
against the exact known-crashing class set (~2-3% pre-fix → 0.87%, 2/229, post-fix). It does **not**
close the family: a second, structurally different producer (native-collections `compare_via_compare_to`,
fed by a stale receiver from a **genuinely bytecode-executing** `EnhancedQueueExecutor` worker-thread path
that bypasses every native shim) reproduced live post-fix with a full captured stack trace — see that
section for the concrete pickup point. Remains in `docs/known-issues/` accordingly.

**Update, same day, later session:** three unrelated boot blockers (real-vs-synthetic `Module`/
`ModuleClassLoader` field-layout gaps plus a missing native `findClass` overload registration — see
"2026-07-19 session (continued...)" near the bottom) were found and fixed in `6d037508d`
(worktree `wt-remoting-cce-eqe-20260719`, not yet merged to dev as of this writing). These were blocking
bare `standalone.sh` boot from ever reaching `parallel-extension-add` at all on a fresh `dev` checkout —
fixing them is a prerequisite for any further live investigation of root cause #2, and is now verified to
get boot demonstrably past `parallel-extension-add` into real service startup. Root cause #2 itself was
**not** re-reproduced this session (harness now works end-to-end again, but time ran out before enough
repro volume was gathered) — still OPEN, see that section for the exact pickup point.

Prior status (preserved for history): REOPENED 2026-07-18, see "Recurrence 2026-07-18" below. The exact
`LifecycleException: ... exited unexpectedly with code [1]` crash signature reproduced again in a fresh
6-shard full-suite rerun (18 instances / 240 FAIL classes sampled) on a binary built from current dev,
which includes the `70154861` fix as an ancestor. Moved back to `docs/known-issues/` accordingly.

Before that: FIXED — 2026-07-13

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

## New evidence 2026-07-18 (same session) — did not reproduce in 10 isolated attempts; likely low per-attempt probability, not fixed

Follow-up: ran 10 isolated repro attempts (round-7 binary, `org.jboss.as.test.integration.basic` module,
20-30s timeout each, same command as the original repro) specifically trying to catch this exception
live. **0/10 hit it** — every attempt instead showed the companion
[[wildfly-standalone-boot-stw-jit-takeover-hang]]'s STW warning (see that doc's matching new-evidence
section for the mechanism details, which also changed shape this pass: `pending=1`, not `pending=6`,
and no longer a permanent wedge).

This is **not** strong evidence the bug is fixed — round 7's real 6-shard harness run separately observed
this exact signature at 18/240 (7.5%) in the same time window these isolated attempts were made, so a
10-attempt isolated sample missing it entirely (expected hits at 7.5%: <1) is unsurprising, not
contradictory. Isolated single-process reproduction may simply have a lower probability-per-attempt than
the real harness's 6-concurrent-shard host-load conditions, consistent with this bug's original filing
describing it as inherently non-deterministic (~1/5 in the first investigation, run under different host
load than today). Whoever next chases this should pull the actual failing classes directly from round 7's
output (`/data/data/wt-wildfly-bugbash-20260718-runner/out/round7-s*of6-*/results.tsv`, filter for
`exited unexpectedly with code \[1\]` in the FAIL rows' logs) rather than relying on fresh isolated
repro attempts, since those have now twice failed to reproduce it (this session's 10 attempts, plus the
earlier "did not reproduce" note above) despite the real harness continuing to hit it.

## 2026-07-19 session: one real producer found and fixed (EnhancedQueueExecutor deferred-runnable queue), one residual manifestation confirmed still OPEN

Worktree `/data/data/wt-remoting-cce-20260718`, branch `fix/wildfly-remoting-cce-recur-20260718`, forked
from `origin/dev`. Fix commit `08f8190da`, merged to dev as `59a3f38c5` (pushed). Full session: isolated
standalone.sh probes → discovered a critical harness bug (managed server silently running on real
HotSpot) → root-caused and fixed a real, previously self-documented-but-unfixed producer → confirmed a
second, structurally different producer still reproduces post-fix.

### Methodology dead ends and the harness bug (read this before re-attempting isolated repro)

- **~124 isolated `standalone.sh` boot attempts (matched heap ergonomics, `CRATONVM_DEFAULT_HEAP_MAX_MB=256`
  after discovering the earlier probe script's explicit `-Xmx1536m` silently defeated it — "an explicit
  `-Xmx` always wins," `vm-cli/src/main.rs`) — 0 hits.** Isolated single-boot reproduction of this family
  is apparently near-zero probability regardless of heap sizing; it needs the real multi-class,
  multi-shard Maven/Arquillian harness (`run-suite-linux.sh`) to manifest at any usable rate, consistent
  with round 7's own 7.5% coming from that harness, never from isolated attempts.
- **Critical harness bug, found only after ~254 "successful" but silently-invalid Maven-harness
  attempts:** `run-suite-linux.sh`'s `-Djvm=<wrapper>` mechanism (documented in
  `apps/wildfly-suite-runner/README.md`) makes the **Surefire-forked test-orchestration JVM** run on
  CratonVM — it does **not** make the **Arquillian-managed WildFly server subprocess** run on CratonVM.
  That subprocess's JVM is selected by the **`-Dcontainer.java.home=<dir>`** system property (read by
  WildFly's `CommonManagedDeployableContainer` config, `javaHome` → `${container.java.home}` in
  `arquillian.xml`). Without it, the managed server launches via whatever `java.home` the ambient
  environment resolves to — confirmed live via `ps aux` showing
  `/usr/lib/jvm/java-17-openjdk-amd64/bin/java` actually running the `org.jboss.as.standalone` process,
  **not** the CratonVM binary. Every batch that omitted `-Dcontainer.java.home` (many hours, ~150+
  attempts across this and the prior 2026-07-18 session) was testing real HotSpot and could never have
  reproduced a CratonVM-specific bug — the 0% hit rate in those batches is a methodology artifact, not
  evidence of anything about the bug. Fix: set up a `container.java.home`-pointed directory whose
  `bin/java` **is** the CratonVM binary (`cratonvm-javahome/bin/java`), and pass
  `-Dcontainer.java.home=<that dir>` in `MAVEN_ARGS`. Verified via `ps aux` showing the managed server
  process image switch to the CratonVM binary path once this was added.
- **Round7's `/data/data/cratonvm/apps/wildfly/build/target/wildfly-32.0.1.Final` distribution directory
  vanished mid-session** (a host-wide disk-reclaim sweep that — per a live correction from the user —
  only touches directories literally named `target`). Re-extracted from `/data/tmp/wildfly-32.0.1.Final.zip`
  and, per the same guidance, kept a second copy outside any `target/`-named path
  (`/data/data/wildfly-dist-keep/wildfly-32.0.1.Final`) for `-Djboss.home`/`MAVEN_ARGS`.
- **A crash log's Maven-side `logs/*.log` never contains the actual `ClassCastException`** — Arquillian
  does not echo the managed server's own console/logging output into the Surefire-captured stream for this
  failure family, and the server dies during `parallel-extension-add` itself (activating the logging
  extension is *part of* that step), so even `target/wildfly/standalone/log/server.log` is never written
  for a crash this early — every `server.log` "capture" attempted mid-session was silently returning a
  stale snapshot from an unrelated earlier successful boot. The only reliable capture mechanism: wrap the
  `cratonvm-javahome/bin/java` executable itself in a shell script that `tee`s the real binary's raw
  stdout+stderr to a per-invocation file before the JVM/Arquillian machinery gets a chance to swallow it
  (`"$REAL" "$@" 2>&1 | tee "$capfile"; exit "${PIPESTATUS[0]}"`) — this is what finally captured the full
  stack traces below. A companion background pruner (`grep ClassCastException`, keep matches forever in a
  separate dir, delete non-matches after 60s) keeps this from filling the disk across hundreds of mostly-
  successful boots.
- The 22 classes that showed this doc's crash signature in round 7's own shard logs (see
  `crashclass-regex.txt`-style list built from `grep -l 'exited unexpectedly with code' round7-s*/logs/*.log`)
  reproduce at a much higher rate than a blind full-suite sweep — repeatedly cycling through just that
  set (rather than the full ~1500-class index, which is dominated by `domain`-mode tests using a
  completely different, non-`container.java.home`-controlled server-launch path) is the efficient way to
  hunt this family.

### Root cause #1 (FIXED): `EnhancedQueueExecutor`'s deferred-runnable queue held unrooted `ObjectRef`s across GC

`native-builtins/src/wildfly_core.rs`'s `native_exec_execute` — CratonVM's Rust-native shim for
`EnhancedQueueExecutor.execute(Runnable)` — does not run the Runnable synchronously (Round 69 did; Round
89 changed this to avoid a boot-ordering bug). Instead it enqueues the Runnable's raw `ObjectRef` into a
process-global `EQE_PENDING: Mutex<HashMap<ObjectRef, VecDeque<ObjectRef>>>`, to be drained later —
whenever `AsyncFutureTask.await()` is called — by `drain_all_pending_runnables`, which pops entries and
calls `ctx.invoke_virtual(r, "run", "()V", &[])` directly on the stored `ObjectRef`. The comment on
`EQE_PENDING` **already flagged this as `KNOWN-UNSOUND across GCs`** back on 2026-07-06 — neither the
queued Runnable refs nor the map key were rooted or remapped, so a moving GC landing in the window between
`execute()` and the later drain left the stored `ObjectRef` dangling; `drain_all_pending_runnables` would
then `invoke_virtual` on whatever the collector had since placed at that address. Every extension's
activation task during WildFly's `parallel-extension-add` boot step goes through exactly this queue, with
~30 extensions allocating/classloading concurrently — precisely the load pattern most likely to have a GC
land in that window. This is a live, concrete mechanism for the `ClassCastException: X cannot be cast to
Y` family this doc tracks, where the reported `Y` is essentially arbitrary (whatever class the collector's
new occupant of that address happens to report) — matching every prior session's "arbitrary cast-target
menagerie" observation.

**Fix** (`08f8190da`): root each Runnable at enqueue time via the same `register_var_handle_root`
(keyed by `identity_hash_code`) / `read_var_handle_root` pattern already used by
`classloader_value_sidetable.rs`'s `rooted_entry`/`resolve_entry` for an identical "native Rust static
holding a transient `ObjectRef`" shape. `EQE_PENDING`'s value type changed from `VecDeque<ObjectRef>` to
`VecDeque<(i32, ObjectRef)>` (identity key + fallback ref); the drain re-resolves to the current address
via `read_var_handle_root(identity_key)` before dispatching. The map *key* (`this`, the EQE instance) is
still a raw, unrooted address and can itself go stale across a move — left as a documented, lower-severity
follow-up, since the only consequence is a given EQE's tasks splitting across two map buckets after its
object relocates, and `drain_all_pending_runnables` drains every bucket unconditionally regardless of key,
so no task is lost or misdispatched by that particular staleness.

**Verification:**
- `cargo test -p cratonvm-native-builtins --lib`: 3027 passed, 0 failed, 6 ignored (clean; no
  fix-adjacent regressions).
- Live batch reproduction against the exact 22-class known-crashing set, correct
  `-Dcontainer.java.home` in place both before and after: pre-fix batches (`gfix`/`outdbg` tags, ~173
  samples across two runs) showed 4 instances of this doc's exact crash signature
  (`exited unexpectedly with code [1]`, ~2.3%); the post-fix verification batch (`outverify-*` tags, 229
  samples, same class set, same harness, same host) showed 2 (~0.87%). A roughly 2-3× reduction, not full
  closure — see the residual below for why.

### Root cause #2 (confirmed live, OPEN — NOT fixed by the above): a structurally different `EnhancedQueueExecutor` path that runs genuine WildFly bytecode, not the native shim

A post-fix verification run (`outverify-a/verify-rep4`, deployed binary confirmed by MD5 match against
the exact post-fix build) captured, via the raw-stdout `tee` wrapper, a complete, unambiguous crash:

```
ERROR [org.jboss.as.controller.management-operation] WFLYCTL0013: Operation ("parallel-extension-add") failed
    java.lang.RuntimeException: WFLYCTL0079: Failed initializing module org.jboss.as.clustering.infinispan
        at org.jboss.as.controller.AbstractControllerService$1.run(AbstractControllerService.java:362)
        ...
    Caused by: java.util.concurrent.ExecutionException: java.lang.ClassCastException: class java.lang.Object cannot be cast to class java.lang.Comparable
        ...
    Caused by: java.lang.ClassCastException: class java.lang.Object cannot be cast to class java.lang.Comparable
        at org.jboss.threads.EnhancedQueueExecutor$ThreadBody.run(EnhancedQueueExecutor.java:1377)
        at org.jboss.threads.EnhancedQueueExecutor$ThreadBody.doRunTask(EnhancedQueueExecutor.java:1486)
        at org.jboss.threads.EnhancedQueueExecutor.safeRun(EnhancedQueueExecutor.java:1990)
        at org.jboss.threads.ContextClassLoaderSavingRunnable.run(ContextClassLoaderSavingRunnable.java:35)
        at java.util.concurrent.FutureTask.run(FutureTask.java:328)
        at org.jboss.as.controller.extension.ParallelExtensionAddHandler$ExtensionInitializeTask.call(ParallelExtensionAddHandler.java:111)
        ...
        at org.jboss.as.clustering.infinispan.subsystem.InfinispanSubsystemResourceDefinition.register(InfinispanSubsystemResourceDefinition.java:39)
        at org.jboss.as.clustering.controller.ResourceDescriptor.addCapabilities(ResourceDescriptor.java:257)
```
(Full capture preserved at `/data/data/wt-remoting-cce-20260718/probes/runner-cce/raw-stdout-hits/boot-20260719-091652-177890499-2978858.log` on the Azure build host — copy it out before that worktree/probe dir is cleaned up.)

The `class X cannot be cast to class java.lang.Comparable` message shape (both sides prefixed `class `,
distinct from the interpreter checkcast's bare `X cannot be cast to Y`) fingerprints this as
**`native-collections/src/lib.rs`'s `compare_via_compare_to`** — the natural-ordering comparator used by
sorted collections (`TreeMap`/`TreeSet`/`Arrays.sort` etc.) — throwing because
`implements_comparable(ctx, *ao)` is false for the receiver it was handed. The interesting part is
**`EnhancedQueueExecutor$ThreadBody.run`/`doRunTask` are real WildFly/jboss-threads bytecode method
names, appearing as genuine frames in the exception's own stack trace** — meaning the task that hit this
CCE was **not** dispatched through `drain_all_pending_runnables` (the path fixed above) at all. Checked
directly: `wildfly_core.rs` only natively overrides `EnhancedQueueExecutor.execute`, `.shutdown` (both
overloads), `.isShutdown`, `.isTerminated`, and `.awaitTermination` — **`submit(Callable)` has no native
shim**. `ParallelExtensionAddHandler` submits `Callable`s (visible in the trace: `FutureTask.run` ←
`ExtensionInitializeTask.call`), and `EnhancedQueueExecutor` — unlike `AbstractExecutorService`-based
implementations — apparently implements its own `submit()` that does not merely wrap-and-call the shimmed
`execute()`; it runs its own real internal worker-thread machinery (`ThreadBody`) as actual interpreted/
JIT'd bytecode, on genuine JVM threads created through the normal thread-creation path — **not** through
`native-builtins`'s Rust-side `spawn_worker`/`worker_loop` at all.

This puts root cause #2 squarely in the same territory as the project's long-running, still-open
**A4 cross-thread-STW-JIT-root-scan gap** (`docs/feature-designs/precise-jit-maps-default.md`, "Step 8 —
GC-root family retirement status": A1–A3 closed, A4 explicitly tracked OPEN). A4's own canonical
`Fork6`/`Fork6Hard` `ForkJoinPool`-worker repro was fixed 2026-07-09
(`docs/internal/fixed-suite-bugs/fork6-fjp-multithread-jit-root-reclamation-FIXED.md`), but that fix's
three components (live-blocked-thread STW accounting, `Unsafe.compareAndExchange*` witness correctness,
real-FJP non-moving-snapshot reference-local scanning) are explicitly scoped to `ForkJoinPool` workers —
nothing in that fix or its validation touches `EnhancedQueueExecutor`'s own genuinely-bytecode-executing
worker threads, which is a structurally different thread-pool implementation with its own internal
queue/thread-management bytecode. **Whoever picks this up next should treat this as a new instance of the
A4 family scoped to `EnhancedQueueExecutor` specifically**, not attempt another native-shim site-fix —
the receiver reaching `compare_via_compare_to` stale is a downstream symptom, not the producer; the
producer is wherever a `EnhancedQueueExecutor$ThreadBody` worker thread's own JIT-compiled frame holds a
register-resident or otherwise GC-invisible root across a moving collection. Start from:
- Confirming whether `CRATONVM_DISABLE_JIT=1`/`--nojit` changes the reproduction rate (the A4 doc's own
  method for the FJP case) — if it does, this really is the JIT-frame-root-visibility mechanism and not
  a native-collections stale-store; if it doesn't, look for an unpinned stale-store site the way the
  2026-07-16/17 sessions found ~85 in stream/collections/MSC/EnumMap/Properties natives (this may simply
  be one more, in whatever caller of `compare_via_compare_to` is reached from
  `ResourceDescriptor.addCapabilities`/`InfinispanSubsystemResourceDefinition.register` — likely a sorted
  `RuntimeCapability`/name collection built during subsystem registration).
- The `boot-20260719-091652-*.log` capture above for the exact call chain and timing (fires ~2.3s after
  `Controller Boot Thread` starts, right as ~40 `org.jboss.threads` worker threads are mid-startup —
  visible in the same capture as a burst of `T19_H6_CAS_DIAG cas_long FAIL` retry-noise warnings on
  `ReentrantReadWriteLock$NonfairSync`, corroborating heavy concurrent contention at the exact failure
  moment, though that diagnostic itself is unrelated instrumentation, not a lead).
- The raw-stdout `tee`-wrapper capture technique above — it is now the only reliable way to catch this
  family's actual exception detail live; reuse it rather than rediscovering the `server.log`-is-always-
  stale trap.


## 2026-07-19 session (continued, worktree `wt-remoting-cce-eqe-20260719`): three real, verified boot-blocker fixes; root cause #2 investigation resumed but not yet closed

Worktree `/data/wt-remoting-cce-eqe-20260719`, branch `fix/wildfly-remoting-cce-eqe-20260719`, forked
from `origin/dev`. Fix commit `6d037508d` (pre-merge). Picked up from the "2026-07-19 session" entry
above with the explicit goal of reproducing and fixing root cause #2 (the confirmed-live
`EnhancedQueueExecutor$ThreadBody`/`compare_via_compare_to` producer). That goal is **not yet reached** —
but getting there required discovering and fixing three separate, previously-undocumented regressions
that blocked bare `standalone.sh` boot from ever reaching `parallel-extension-add` at all on a fresh
worktree built from current `dev`. Documenting all three here since they were real, verified bugs in
their own right, independent of whether root cause #2 gets closed this session.

### Methodology note: harness bugs found and fixed along the way

- The prior session's `wt-remoting-cce-20260718` worktree had accumulated ~13G of disposable probe/scratch
  data (old WildFly standalone install copies, capture logs) that was cleaned up (after preserving the
  one load-bearing capture, `boot-20260719-091652-*.log`, referenced above) to free disk space on an
  already-96%-full host — see [[azure-host-disk-full-flapping-20260715]].
- **`ssh ... "pkill -f <pattern>; echo ..."` self-kills the invoking shell.** `pkill -f` matches against
  full command lines, including the very `bash -c "pkill -f X; echo ..."` process sshd spawns to run the
  command — if the pattern string `X` appears verbatim in that command line (which it does, trivially,
  since you just typed it), `pkill` kills its own invoking shell before the `echo` ever runs, producing a
  silent `exit-signal` (not `exit-status`) with zero output. Symptom: an SSH command that should print
  something prints nothing and returns a mysterious 255. Fix: the classic bracket trick,
  `pkill -f '[p]attern'` — the literal search string then no longer appears in the invoking command line's
  own text, only the regex matches the target process.
- **A `python3 - <<'EOF' ... EOF` heredoc containing backticks inside an outer `ssh host "..."`
  double-quoted Bash-tool command breaks.** The outer double quotes are consumed by the LOCAL shell before
  the whole string ever reaches `ssh`/the remote heredoc, so backticks in code comments (`` `Module.run()` ``
  etc.) trigger local command substitution regardless of the heredoc's own `'EOF'` quoting. Fix: write the
  Python patch script to a local file, `scp` it to the host, then run `python3 /path/to/script.py`
  remotely — sidesteps all nested-quoting hazards entirely.
- **A `java` wrapper script that backgrounds its own cleanup (`(sleep 60 && rm -f "$capfile") &`) without
  redirecting the child's stdin/stdout/stderr hangs every caller of the wrapper.** The backgrounded
  subshell inherits the wrapper's own stdout/stderr file descriptors; since nothing closes them, any
  process capturing the wrapper's output (a pipe, `$(...)`, a test harness) blocks waiting for EOF until
  that backgrounded sleep finally exits 60s later — even though the wrapped real binary returned
  immediately. Symptom: `java -version` (or any single boot attempt) hangs for exactly the cleanup delay
  with zero visible cause. Fix: don't background the cleanup inside the wrapper at all — write straight to
  a per-invocation capture file, `cat` it to reproduce the old "tee to caller" behavior, and delete it
  inline (no subshell, no orphaned FDs) when it doesn't contain the string being hunted.
- **Real-JDK mode needs `CRATONVM_JAVA_HOME` (or `JAVA_HOME`) pointed at an actual JDK class-library
  install** (`/home/victor/jdk25` on this host) **separately from whatever `JAVA_HOME` a test harness uses
  to locate the `java` launcher executable itself** — `vm-cli`'s host-JDK autodetect
  (`VmConfig::with_host_jdk_default`) checks `JAVA_HOME` / `CRATONVM_JAVA_HOME` / `java` on `PATH`, in that
  order; when a wrapper script masquerading as `java` is the only thing on `JAVA_HOME`, real-JDK mode
  silently falls back to synthetic stubs unless `CRATONVM_JAVA_HOME` is set explicitly to the real JDK
  install.

### Fix #1 (in `6d037508d`): `ModuleClassLoader.module` field never populated — real-field-layout gap

**Symptom:** every bare `standalone.sh` boot attempt against a binary built from a clean `dev` checkout
NPE'd immediately after `JBoss Modules version 2.1.5.Final` prints, before any WARN/INFO line:
`NullPointerException: Cannot invoke "org.jboss.modules.Module.loadModuleClass(String, boolean)" because
"module" is null`, `at org/jboss/modules/Main.main(Main.java:603)`.

**Root cause, confirmed via `javap -p -c -l` disassembly of the real `Main.class`/`Module.class`/
`ModuleClassLoader.class` extracted from the target WildFly's own `jboss-modules.jar`, cross-referenced
against a `CRATONVM_DBG_ATHROW=1` raw-frame capture (which gives the correct innermost-first ordering,
unlike the default printed trace — see the "stack trace misattribution" pattern this codebase already
tracks elsewhere) and a `CRATONVM_DBG_WF=1` trace:** `build_module_object()` in
`native-builtins/src/jboss_module_loader.rs` allocates a synthetic `ModuleClassLoader` (`mcl`) and writes
its back-reference to the owning `Module` only via the synthetic slot index `MCL_SLOT_MODULE`
(`ctx.set_field(mcl, MCL_SLOT_MODULE, ...)`) — with no matching name-based write. But the *real*
`org.jboss.modules.ModuleClassLoader` class (loaded from real jboss-modules bytecode in real-JDK mode)
declares `private final org.jboss.modules.Module module;` at a real, `javac`-assigned field offset that
generally does not coincide with the synthetic slot index. Real bytecode inside `ModuleClassLoader`
(reached via `Class.forName(name, resolve, mcl)` → `mcl.loadClass()` → real fallback) reads `this.module`
and gets the zero/null default, producing exactly this NPE. Every *other* field this same function
populates already gets a defensive name-based write alongside its index-based one (see the pre-existing
`"moduleLoader"`/`"name"`/`"moduleClassLoader"` writes just below it, each with its own comment citing
this exact real-vs-synthetic-layout hazard) — this one field was simply missed.

**Fix:** add `ctx.set_field_by_name(mcl, "module", Value::Object(Some(module)));` alongside the existing
index-based write.

### Fix #2 (same commit): `Module.linkage` never populated — first attempt (`Linkage.NONE`) caused a genuine single-thread self-deadlock

Fixing #1 alone did not unblock boot — it advanced the failure to a *different* NPE one layer deeper:
`Cannot invoke "org.jboss.modules.Linkage.getState()" because "oldLinkage" is null`, now with a real stack
trace reaching `Module.loadModuleClass` → `Module.getPathsUnchecked` → `Module.getPaths`. Real
`org.jboss.modules.Module` declares `private volatile Linkage linkage;`, likewise never populated by our
synthetic `build_module_object`.

**First attempt — WRONG, caused a hang, not a crash:** seeded `module.linkage` with the real package-private
static `Linkage.NONE` (read via `ensure_class_initialized` + `static_field_index_by_name` +
`get_static_field`, matching the pattern used elsewhere in this codebase for e.g.
`Thread$State`). Disassembling `Linkage`'s own `static {}` initializer shows `NONE = new
Linkage(Linkage$State.NEW)`. Disassembling `Module.getPaths()` shows real bytecode of the shape:

```
Linkage state = this.linkage;
if (state.getState() == LINKED) return state.getPaths();          // fast path
synchronized (this) {
    while ((state = this.linkage).getState() == LINKING || state.getState() == NEW) {
        this.wait();                                               // <-- pc=55
    }
    ...
}
```

Seeding `NEW` unconditionally routes every module through the `wait()` branch — and since CratonVM's
synthetic module construction never runs the real dependency-linking machinery that would eventually
`notify()` this monitor from another thread, the *lone single thread in the whole process* (boot hadn't
spawned any workers yet) parks on its own `wait()` forever. Confirmed live: a
`CRATONVM_DEFAULT_WATCHDOG_SEC=20` run produced a clean T19.H1 thread dump showing exactly one registered
thread (`"main"`), stuck at `Module.getPaths@55` / `Object.wait()`, for the full 20s deadline before the
watchdog aborted the process. This is a good concrete illustration of why "seed it with whatever the real
static sentinel is called" is not automatically safe for these real-vs-synthetic backstops — the sentinel
matters semantically, not just type-wise.

**Actual fix:** construct a real `Linkage` object already in the **`LINKED`** terminal state via its real
1-argument constructor (`new Linkage(Linkage$State.LINKED)`, via `ctx.new_object_initialized(...)` —
disassembly confirms this constructor internally defaults `dependencySpecs`/`dependencies` to the real
empty `NO_DEPENDENCY_SPECS`/`NO_DEPENDENCIES` statics and `allPaths` to `Collections.emptyMap()`, which is
exactly correct for a module whose resources/paths we resolve and serve entirely through our own native
machinery rather than real dependency-linkage bytecode). This takes `getPaths()`'s fast path
unconditionally and never reaches the wait loop.

### Fix #3 (same commit): `ModuleClassLoader.findClass`'s native shim only covered the 1-arg overload

Fixing #1+#2 advanced boot past the self-deadlock into a clean, *expected* `ClassNotFoundException:
org.jboss.as.server.Main from [Module "org.jboss.as.standalone" ...]` — expected because
`org.jboss.as.standalone`'s own `module.xml` has a genuinely empty `<resources>` block (its main class
lives in the re-exported `org.jboss.as.server` dependency), and real bytecode class-loading was never
walking that dependency closure. Investigating *why* real bytecode was involved here at all (rather than
this codebase's existing native `findClass` shim, which already correctly walks the dependency closure via
`module_visibility_closure`) found the actual root gap: the native registry only registered
`ModuleClassLoader.findClass` for the JDK's own 1-arg `ClassLoader.findClass(String)` signature. Real
`ConcurrentClassLoader.performLoadClassUnchecked` (jboss-modules' own class-loading entry point) calls the
**3-arg** `findClass(String, boolean exportsOnly, boolean resolve)` overload that `ModuleClassLoader`
itself declares — an overload with no native shim at all, so every real class-load fell straight through
to real bytecode, which is what exposed the field-layout gaps in fixes #1/#2 in the first place (and would
presumably keep surfacing more such gaps indefinitely, since a from-scratch synthetic construction can
never fully replicate everything real `Module`/`ModuleClassLoader` construction bytecode sets up).

**Fix:** register the 3-arg overload too, delegating to the same `native_module_classloader_find_class`
(trimming the two trailing booleans, which only affect resolve/export-visibility bookkeeping the closure
walk doesn't need). This is the higher-leverage fix of the three — it routes real boot class-loading
through the already-correct native closure walk instead of continuing to chase individual missing
real-bytecode field dependencies one at a time.

### Verification

- `cargo test -p cratonvm-native-builtins jboss_module` (release): 65/65 pass.
- `cargo test -p cratonvm-native-builtins` (release, full crate): 3033 passed, 2 failed — both confirmed
  pre-existing and unrelated (`jca::key_factory::tests::keyfactory_unproducible_key_throws_not_dead_key`,
  a JCA/crypto test; `phases_late::p57_win_path_tests::trailing_separator_is_removed_only_from_non_roots`,
  a Windows-path-string test asserting a literal `"C:/work/one/two"` — expected to be
  environment/platform-sensitive on this Linux host, nothing to do with `jboss_module_loader.rs`).
- **Live repro, most important evidence:** isolated `standalone.sh` boot against the same frozen WildFly
  32.0.1.Final dist this whole investigation has used throughout, with the fix binary
  (`cvm-remoting-cce-eqe-20260719.bin`) wired in via `JAVA_HOME`/`CRATONVM_JAVA_HOME` exactly as described
  in the "Methodology dead ends" section above. Pre-fix: crashes before the first WARN line, every time.
  Post-fix (`CRATONVM_DEFAULT_WATCHDOG_SEC=45` run, `fixtest8.log`): a T19.H1 thread dump at the 45s mark
  shows **94 registered threads**, with the `Controller Boot Thread` inside
  `ParallelBootOperationStepHandler$2.execute` and *zero* threads still piled on the earlier
  `operationPrepared` contention point — plus genuinely new, much-later-boot thread names present for the
  first time all session: `IdleRemover`, `ConnectionValidator` (JCA connection-pool management),
  `Transaction Expired Entry Monitor`, `Periodic Recovery`, `Transaction Reaper`/`Transaction Reaper Worker
  0` (Narayana/JTA), `Timer-0`/`Timer-1`, `cratonvm-xnio-accept-1` (XNIO/remoting networking acceptor).
  This is unambiguous evidence boot now proceeds from an immediate pre-`parallel-extension-add` crash all
  the way into real, later-stage service startup (transaction manager, connection pools, remoting/XNIO) —
  territory that was completely unreachable before this session's three fixes.

### Still open: root cause #2 not yet reproduced/fixed this session; a separate, real serialization/timing issue surfaced along the way

With the three boot blockers fixed, `parallel-extension-add` itself became reproducible under this
harness for the first time — but proved to have a **separate, genuine timing/contention issue independent
of anything this doc previously tracked**: an isolated 240s run (`fixtest7`, no watchdog) produced *zero*
stdout output beyond the startup banner and never created `standalone/log/server.log`, i.e. it did not
demonstrably progress past the same early `parallel-extension-add` region within 240 wall-clock seconds on
this (heavily host-contended — 30+ concurrent unrelated `cargo build`/test sessions were running throughout
this whole investigation) machine. A companion run with the watchdog armed every 45s
(`fixtest8`, described above) DID progress past that exact point within 45s. The most likely explanation is
raw host contention (this Azure box was extremely oversubscribed all session — confirmed via `ps aux`
showing dozens of concurrent `cargo build`/`cargo test` invocations from unrelated sessions throughout),
not a new deterministic bug, but this was not conclusively distinguished from a genuine intermittent
livelock in the `operationPrepared` serialization point (~40 threads funnel through one
`ModelController`-level lock there by design — real, expected serialization, not obviously a bug — but
worth re-checking on a quieter host before assuming it's purely contention).

**Root cause #2 itself — the `EnhancedQueueExecutor$ThreadBody`-driven `compare_via_compare_to` stale
receiver reaching `class X cannot be cast to class java.lang.Comparable`, documented in the "2026-07-19
session" entry above — was not reproduced again this session.** No isolated attempt reached far enough
into (or past) `parallel-extension-add` reliably enough, within the time available, to give it a fair
chance to fire (its own prior confirmed reproduction rate was already low: 2/229 samples, ~0.87%, even
under a working harness). The path is clear for whoever continues this: the harness now works end-to-end
(binary `cvm-remoting-cce-eqe-20260719.bin`, worktree `/data/wt-remoting-cce-eqe-20260719`, dist at
`probes/wf-slot0` cloned from `/data/data/wildfly-dist-keep/wildfly-32.0.1.Final`, java wrapper at
`probes/javahome/bin/java` — see the "Methodology" section above for exact env vars) — what's needed now
is simply a longer/higher-volume batch of live repro attempts (ideally on a quieter host, or using the
6-shard Maven/Arquillian harness from the 2026-07-19 session above rather than bare isolated
`standalone.sh`, to get back to that session's much higher per-batch hit rate) with
`CRATONVM_DBG_BLOCKGC=1`/`CRATONVM_DBG_STALE_OBJREF=1` armed, to finally get a live capture of the
producer mechanism this doc's own "Root cause #2" section already scoped: is it JIT-frame-root-visibility
(test `--nojit`/`CRATONVM_DISABLE_JIT=1` first, per that section's own suggested next step) or one more
unpinned native store reachable from `InfinispanSubsystemResourceDefinition.register`.
