# WildFly standalone boot: `ClassCastException: java.lang.Object cannot be cast to org.jboss.as.controller.registry.AttributeAccess` during `parallel-extension-add` — register-invisible JIT root family, confirmed occurrence

Status: OPEN — two independent 2026-07-14/2026-07-15 follow-ups (both below)
narrow this further without closing it. The 2026-07-14 revalidation ruled out
the default-on full-GPR safepoint spill (`02b91823`), both JIT/root-scan
caches, and broadened STW peer scanning as fixes. The 2026-07-15 follow-up
generalizes the symptom to `WFLYCTL0079: Failed initializing module <any
extension>` (not limited to `AttributeAccess` or to this doc's
originally-named extensions), **disproves this doc's own "JIT is required"
claim** (reproduces identically under `--nojit`), and — consistent with the
2026-07-14 finding that broadened STW scanning didn't help — found via live
capture that the stale read goes through the *pinned* path (not a
missed-pin fallback), pointing at a cross-thread GC-root-visibility race
rather than a simple unpinned-local or JIT-register-visibility bug. One
genuine, narrow contributing site (unrelated to the ruled-out mechanisms
above) was found and FIXED (see
`docs/internal/fixed-suite-bugs/wildfly-invoke-virtual-lambda-sam-compat-stale-locals-FIXED.md`)
but does not close the residual. Still documented here, not further patched
— see both follow-up sections below for the full evidence chain.

Investigated 2026-07-13, worktree
`C:/craton/cratonvm/.claude/worktrees/attrib-cce-investigate`, branch
`investigate/wildfly-attributeaccess-cce-20260713`, forked from `origin/dev @
a7680d77`.

## 2026-07-14 revalidation and residual isolation

The supplied Azure host built cratonvm-cli --release from ab423500 with
CARGO_TARGET_DIR=/data/target-wildfly-attributeaccess-rootfix-20260714 and
used the uniquely named binary
/data/target-wildfly-attributeaccess-rootfix-20260714/release/java-wildfly-attributeaccess-rootfix-20260714.
Each probe cloned the WildFly standalone installation into
/data/wildfly-attributeaccess-rootfix-20260714-probe, cleared standalone/data/tmp,
and ran the real jboss-modules standalone boot with a 45-second limit. Logs are
under /data/wildfly-attributeaccess-rootfix-20260714-probe/logs/ on that host.

### Control result: the issue still reproduces on current dev

Ten clean boots of the current dev binary produced one AttributeAccess CCE,
five sibling AttributeDefinition CCEs, two complete boots, one SIGSEGV
(exit 139), and one STW hang. The target failure is in boot_1.log; sibling
failures are in boot_3.log, boot_4.log, boot_6.log, boot_7.log, and boot_9.log.

This is direct counter-evidence to treating 02b91823 as a complete fix for
this document. Its full-GPR safepoint spill remains present in the build, but
both CCE forms still occur.

### Root-scanning experiments that did not fix it

* CRATONVM_DBG_FULLSTACK_SCAN=1 was used for nine completed-or-timed probe
  attempts. A sibling AttributeDefinition CCE still occurred. This batch did
  not sample an AttributeAccess CCE, so it neither proves nor disproves
  suppression of the target alone; it does disprove a complete family fix.
* Disabling both the JIT scan cache and root-snapshot cache with
  CRATONVM_NO_JIT_SCAN_CACHE=1 CRATONVM_ROOTSNAP_CACHE=0 produced two
  AttributeAccess CCEs, two AttributeDefinition CCEs, one complete boot, two
  STW hangs, and one timeout in eight attempts. Neither cache is the cause of
  this residual.
* An isolated source experiment expanded STW peer scanning to include every
  alive peer rather than only the blocked JIT-return window. Ten boots still
  produced one AttributeAccess CCE, five AttributeDefinition CCEs, one
  complete boot, one SIGSEGV, one STW hang, and one timeout. The experiment
  was reverted and is not part of the commit.

### Current conclusion

The CCE family remains reproducible, but the tested explanations are now
excluded: default full-GPR safepoint spilling, the two root-scan caches, and
broadened STW peer scanning do not eliminate it. The existing fast-path
analysis below remains useful for locating where the stale reference is
observed, but it is not sufficient to attribute the corruption to a
register-invisible root. The upstream stale-object/reference corruption point
is still unknown. There are no newly fixed items to remove or move to
docs/internal.

## Background

[[wildfly-remoting-classcastexception-parallel-extension-add]]
(`docs/internal/fixed-suite-bugs/wildfly-remoting-classcastexception-parallel-extension-add.md`,
dev `70154861`) fixed a JIT `checkcast`/`instanceof` bug: `jit_typecheck_resolve`'s
**slow path** (target class not yet loaded) called `vm.load_class_concurrent(...)`
— a GC-triggering call — while holding the receiver's `ObjectRef` unpinned.
That fix's own verification run flagged one un-investigated residual: 2 of 20
fixed-binary attempts hit a structurally identical but distinct exception,
`ClassCastException: java.lang.Object cannot be cast to
org.jboss.as.controller.registry.AttributeAccess`, with no accompanying
STW-takeover warning. `AttributeAccess` is a foundational WildFly-core class
loaded long before `parallel-extension-add`, so its checkcast/instanceof
almost certainly takes `jit_typecheck_resolve`'s **fast** path (target
already resolved) — which has no `load_class_concurrent` call, so the
just-landed fix's mechanism cannot apply to it. This doc is the follow-up
investigation into that residual.

## Code-level analysis (confirmed from source, current `dev`)

### The fast path has zero GC-triggering calls

`vm/src/jit/helpers.rs::jit_typecheck_resolve` (~line 3478), fast path (target
class already resolved via `JIT_TYPECHECK_TARGET_CACHE` or
`class_manager.read().find_class_by_name`):

```rust
if let Some(target_class_id) = target_class_id_opt {
    if obj_class_id == target_class_id { return true; }
    let is_subclass = vm.class_manager.read().is_subclass_of(obj_class_id, target_class_id);
    if is_subclass { return true; }
    if crate::runtime::interpreter::lambda_proxy_satisfies_public(vm, obj_class_id, target_class_id) {
        return true;
    }
}
```

Every operation here is a `class_manager.read()` (RwLock read guard,
non-GC-triggering) or a pure hierarchy-table lookup. There is genuinely
**zero** allocation or class-loading call between `jit_checkcast`/
`jit_instanceof` reading the receiver's class id (`vm.heap.class_id_of(obj_ref)`,
itself just a header read) and returning a result. `jit_checkcast` (~line
3699) and `jit_instanceof` (~line 3763) do no allocation either — they
resolve the receiver via `vm.heap.is_object_address(obj_ptr)` (a lock-free
structural check) and hand it straight to `jit_typecheck_resolve`.

**Conclusion: staleness cannot originate inside this function on the fast
path.** If the receiver observed here is stale, it was already stale
*before* the JIT-compiled caller ever loaded it into the register/stack slot
it passed as `obj_ptr` — the corruption event is external to this helper, on
whatever code path last legitimately held a live reference to the object.

### The STW cross-thread JIT takeover is a conservative, inclusive scan — not the mechanism here

`vm/src/jit/xt_root_scan.rs` (BUG-03): when a stop-the-world GC initiator
needs to freeze a peer thread mid-execution inside JIT-compiled code (one
that never reaches a cooperative interpreter safepoint on its own), it
`SuspendThread`+`GetThreadContext`s the peer (Windows) and conservatively
scans **all 16 integer registers and the peer's entire used stack** for
plausible heap-pointer bit patterns (`VmHeap::is_object_address`), adding
every match as a root. This is a full, inclusive fallback — false positives
only over-retain; true roots are never structurally missed. This mechanism
closes exactly the "a live oop sits only in a register, invisible to a
stack-only scan" gap, but only for *frozen, forcibly-stopped* peers. It is
not where this residual lives; that lives in the *cooperative* precise-map
path described next.

### Generational GC's "frozen-peer-during-moving-GC is impossible" claim — CONFIRMED REAL, not aspirational

`gc/src/gen_heap.rs::collect_garbage_inner` (~line 2993-3111): whenever any
JIT frame is active anywhere (`gc_quiescence::is_active()`, true almost
continuously once JIT-compiled code is running) and the experimental
`CRATONVM_MOVING_YOUNG` flag is off (its default), the young generation
collector **unconditionally diverts to the non-moving mark-sweep +
selective-promotion path**, never the moving (Cheney semispace) collector —
verified directly in source. The code's own comment: "a semispace cannot pin
a conservative JIT root nor rewrite a register-resident one, so some live
nodes go stale after the swap" — Generational deliberately never relocates
objects while any thread could hold a JIT-resident reference to one. This
confirms, as a real implemented invariant (not aspirational), the same claim
the G1-backend "INT-3" design doc makes about Generational's inherent
immunity ("Generational gets this free — frozen cycles force the non-moving
sweep").

**This matters for root-cause attribution.** WildFly boots under the default
`GcAlgorithm::Generational` backend (`vm/src/config.rs:409`; no
`-XX:+UseG1GC`/ZGC flag anywhere in the repro), and JIT is required to hit
this bug. Since the young collector never moves objects while this bug's
preconditions hold, the classic "moving GC evacuated the object out from
under a frozen peer, leaving a dangling old-address pointer" mechanism is
**structurally excluded** here.

**What remains, and is fully consistent with every observed symptom, is the
other half of the same family: the non-moving sweep's *marking* phase
missing a live root.** The non-moving sweep's own source comment: it "relies
on conservative over-marking it does NOT have" outside the conservative-scan
path — correctness for a reclaim-in-place (never relocated) collector
depends entirely on the mark phase finding every live root. If a live
reference is invisible to that mark phase at exactly the wrong cooperative
safepoint (see next section), the sweep reclaims the object's memory *while
a mutator still holds a "live" reference to it*; that memory is then handed
to a fresh allocation (WildFly's `parallel-extension-add` step is ~30-40
threads allocating/classloading concurrently — about as hostile a window as
this codebase has), and the next read of the stale reference sees a
legitimate, fully-valid header for a *different*, freshly-allocated
`java.lang.Object` — exactly the `ClassCastException: java.lang.Object
cannot be cast to X` shape, for whatever `X` the checkcast happened to
target.

### The precise-JIT-oop-map machinery has known, documented, currently-open gaps that produce exactly this shape

`CRATONVM_PRECISE_JIT_MAPS` has been default-on since dev `32649b56`
(2026-06-17) and closes the *original* "A3" register-invisibility gap (a
live oop in a **caller frame's** callee-saved register, invisible to a
stack-only conservative scan — `docs/known-issues/README.md`'s A1-A5 table,
`docs/feature-designs/precise-jit-maps-default.md`).

**Two independent 2026-07-10 investigations — unconnected to this one,
working entirely different Tomcat test classes — found the default-on
precise-map machinery itself still has real, un-closed gaps**, tracked under
the identifier `SB-CRASH-04` in `jit/src/x64.rs`'s own source comment on
`emit_pre_safepoint_spill`:

> "SB-CRASH-04 (register-invisibility) — blind-spill the CURRENT value of
> every used callee-saved GPR into its reserved frame slot. The local flush
> above only covers register-resident *locals*; an oop can also live in a
> callee-saved register as an operand-stack temporary that survives the
> call, **or via a value the per-slot oop tracker fails to tag**."

The default-on local-oop dataflow (`compute_local_oop_masks`) is a forward
must-analysis that **intersects (AND) at control-flow merge points** — sound
for "definitely oop" but unsound for "actually still live oop": any CFG
shape where a slot is oop-typed on one incoming edge and not on another
under-reports that slot as non-oop at the merge. Operand-stack
*temporaries* (not named locals) that outlive a call are explicitly
uncovered by the default path too. The blind-spill mitigation
(`CRATONVM_JIT_SAFEPOINT_REG_SPILL`) exists but is **default off** and, per
the DoHead investigation, empirically insufficient even when turned on
(3/6 crashes, indistinguishable from the `=0` baseline's 2/6).

Three independent occurrences, all root-caused via live gdb core-dump
register analysis, all showing a stale register/stack-slot holding a
non-tagged-pointer garbage value read past a null-check that itself passed:

- `docs/internal/fixed-suite-bugs/dohead-jit-heap-corruption-register-invisibility-FIXED.md`
  — "Layer 1 (register-invisible roots → survivable all-zero-header
  stale-receiver flood) is UNCHANGED — the real fix remains precise oop maps
  / shadow stack." An explicitly accepted, unfixed residual, even after that
  doc's own FATAL-layer (young from-space walk-desync) fix landed.
- `docs/internal/tomcat-08-07/swallowabortedupploads-unexpected-socketexception-RESOLVED.md`
  ("2026-07-10: blank-response follow-up" section) — a SIGSEGV reproduced
  10-11 invocations into a tight loop of 3 `AbortedPOSTClient` tests, JIT-on
  only (`--nojit` never crashes), register signature `rsi=0xffffffff94a08430`
  / `0x198ca4f8` (non-tagged-pointer garbage read after a passing null
  check, at what disassembles to a class/identity-comparison — an
  `instanceof`/inline-cache-shaped fast path). Its own "register-invisible
  root diagnostic pass" section directly confirmed, from source, that
  `CRATONVM_PRECISE_JIT_MAPS` was default-on on the exact `dev` tip tested
  and that the collector in play was the non-moving mark-sweep (not
  moving) — the same setup as this doc's finding.
- `docs/known-issues/README.md`'s 2026-07-10 entry retiring
  `accesslogvalve-rewritevalve-connection-failures-RESOLVED.md`: "The sixth
  — a SIGSEGV around `TestAccessLogValve` test #8 — is a confirmed,
  byte-for-byte register-signature match with the already-tracked,
  currently-OPEN 'register-invisible JIT root' bug family."

**This investigation's finding is a fourth independent occurrence of the
same family, in a fourth structurally different call shape** (a
checkcast/instanceof fast path against an already-resolved target, rather
than AQS-park, `ConcurrentLinkedQueue`, or a socket-processor lock read) —
consistent with the swallow-uploads doc's own observation that the
register-invisibility gap is "broader than just the `park()` call shape
already documented."

## Reproduction attempt (this session)

**Environment note, important for interpreting these numbers:** this shared
Windows machine was under sustained, externally-driven CPU load throughout
(`wmic cpu get loadpercentage` read 84-100% continuously; confirmed via
`ps -ef` to be driven by *other* concurrent sessions' cargo builds and a
Spring Boot `spring-suite-runner` HotSpot baseline run — none of it
self-inflicted after an early cleanup of a redundant own-batch). This
measurably changed the outcome distribution from the original investigation's
(a healthy mix of CCE / STW-hang / OK outcomes across its 15-20 attempt
batches). Under today's contention, every one of this session's 46 plain
(no extra diagnostic env var) boot attempts stalled completely rather than
reproducing either CCE. A `--stack-dump-on-timeout`-style live thread/frame
dump (`CRATONVM_DEFAULT_WATCHDOG_SEC=40`) on one hung attempt showed all ~36
`parallel-extension-add` worker threads still `alive=true, blocked=false`
(genuinely still running, not parked at a safepoint) after 40 real seconds
with zero progress; one attempt extended to a 180-second timeout never
resolved and never printed the documented STW-takeover warning line either.
This is consistent with severe CPU starvation preventing the boot from ever
reaching the timing-sensitive window this bug needs, not with a change in
the bug itself — see "Separate finding" below for a concrete, better
explanation of the hang pattern found during this same session.

Tallies (60s timeout per attempt unless noted; JIT on, real JDK25, default
Generational GC — matching the task's own recipe):

| Batch | N | AttributeDefinition CCE | AttributeAccess CCE | SIGSEGV | STW warn printed | Hung, no further output | OK boot |
|---|---|---|---|---|---|---|---|
| sanity (`sanity01-05`) | 5 | 0 | 0 | 0 | 0 | 5 | 0 |
| main batch (`main001-030`) | 30 | 0 | 0 | 0 | 0 | 30 | 0 |
| main2 batch (`main2_001-010`) | 10 | 0 | 0 | 0 | 0 | 10 | 0 |
| extended timeout (180s, 1 attempt) | 1 | 0 | 0 | 0 | 0 | 1 | 0 |
| **Total (no diagnostic flag)** | **46** | **0** | **0** | **0** | **0** | **46** | **0** |

**Neither CCE reproduced live in this session's 46 full-boot attempts.** The
`AttributeDefinition` sanity check passed as expected (0/46 — confirms the
`70154861` fix still holds; no regression). The `AttributeAccess` CCE this
doc targets did **not** reproduce live this session, for the environmental
reasons above — an honest inconclusive-by-environment result for the *live
capture* component specifically. It does not weaken the code-level analysis
above, which is independent of a fresh capture. The original fix doc's own
verification run (a different session, `frozen-cratonvm-remoting-cce-fixed-v2.bin`,
20 attempts) already captured this exact CCE twice (2/20); this doc's
root-cause analysis is built on that occurrence plus the code reading above.

## `CRATONVM_DBG_STALE_OBJREF` diagnostic run

Confirmed present in source (`gc/src/stale_objref_debug.rs`,
`gc/src/gen_heap.rs::get_header`) before running.

**Predicted outcome, from source, before running:** unlikely to fire for
*this specific* bug, because this assertion's mechanism
(`docs/internal/wildfly-stale-objectref-debug-assertion-scoping.md`, "Not
covered" section) is scoped to the **moving-GC evacuation** case — it panics
when `get_header` reads a header carrying a forwarding pointer (an object
the *moving* collector relocated, whose old address a native local kept
pointing at for one extra quarantined cycle). Since Generational never moves
objects while any JIT frame is active (confirmed above), and this bug's
precondition is exactly "JIT is active," the non-moving mark-sweep that
actually runs during this bug's window never produces a forwarding pointer
at all — there is nothing for this assertion to catch for the
premature-reclamation-by-marking-gap failure mode.

**Actual result: the assertion DID fire, at a real but partial rate (9 of 20
attempts with the flag set, across three sub-batches — see "Separate
finding" below) — but every single firing was on a DIFFERENT, unrelated
bug**, not the `AttributeAccess` checkcast fast path (confirmed: the panic
message identifies the *forwarded* object's class id/kind, and none of the
firings correlate with a checkcast/instanceof call site — they fire
immediately as the `parallel-extension-add` worker threads spin up, well
before any checkcast against `AttributeAccess` would even be attempted).
This is itself informative: it confirms the assertion's mechanism works
exactly as scoped (it catches genuine moving-GC-evacuation staleness, which
does happen elsewhere during this same boot step, in a plain native — not
JIT — function), and by elimination further supports that the
`AttributeAccess` bug is *not* a moving-GC-evacuation case — if it were,
this same assertion would very likely have caught *it* too, at a comparable
rate, rather than exclusively catching a different, structurally unrelated
site every time it fired.

## Separate finding: a fresh occurrence of the "Family 1" stale-`ObjectRef`-across-GC bug class, also firing during `parallel-extension-add`

Not this doc's target, but significant enough to flag prominently. With
`CRATONVM_DBG_STALE_OBJREF=1`, 9 of 20 attempts (across three sub-batches,
including one rebuilt with a small diagnostic enhancement to identify the
stale object's class — see below) hit:

```
thread 'Thread' panicked at gc\src\gen_heap.rs:1514:13:
CRATONVM_DBG_STALE_OBJREF: stale ObjectRef detected at <OLD> — this object
was evacuated by a moving GC to <NEW> (class_id=<N> kind=<K>), but
native/interpreter code dereferenced the OLD address. This means a raw
ObjectRef local was held across a GC-triggering call without
pin_native_root/read_native_pin. See
docs/known-issues/wildfly-parallel-boot-stale-objectref-residual.md.
```

immediately after the ~37-42 `parallel-extension-add` worker threads spin
up — the same boot step as this doc's target, but a **structurally
different** bug: a genuine native (non-JIT) function somewhere in the
classloading/extension-loading path holds a raw `ObjectRef` local across a
GC-triggering call, unpinned — the exact "Family 1" pattern
`docs/internal/wildfly-parallel-boot-stale-objectref-residual.md`
extensively chronicles fixing ~40+ instances of across 5 prior sessions,
whose header now reads "Status: FIXED - 2026-07-13" (the same day as this
investigation).

A small, diagnostic-only enhancement was added to
`gc/src/gen_heap.rs::get_header`'s panic message (gated behind the same
`CRATONVM_DBG_STALE_OBJREF` flag, zero cost/behavior change when off) to
also report the *forwarded* object's `class_id`/`kind` — read from the live,
valid header at the new address — since the object's OLD-address header is
by then just a forwarding marker. One rebuild + capture with this
enhancement showed **three distinct class ids hit within a single run**
(`class_id=373` dominant — 95 of 105 total header reads on the panicking
thread landed on this one stale class before the process was torn down —
plus `class_id=1299` and `class_id=6`, `kind=Object` in all three cases).
Class ids are assigned in load order within a single VM run and are not
stable across runs, so these numbers don't identify a specific JDK/WildFly
class by name without further cross-referencing — not done here, out of
scope for this doc's time budget — but the fact that **multiple distinct
classes** are affected within one run argues this is either one call site
reached with many different receiver types (e.g. a generic
`Class`/`Constructor`/`ServiceLoader` reflection helper — several of that
shape were fixed in the referenced doc's sessions 2-3) or several call
sites, not a single narrow one-class bug.

Since this fires on a **fresh `origin/dev` checkout forked after that
"FIXED" date**, this is either a genuinely new/regressed site, a gap that
doc's own sweep didn't cover (its own text: "this is a long-tail bug class,
not a small fixed set of sites" / "~37 more confirmed sites fixed... the
full JSON candidate list is not [exhausted]"), or a difference between this
session's full-extension-set Windows WildFly distribution and that doc's
narrower Linux/Azure validation config. **Not root-caused to an exact call
site this session** — `RUST_BACKTRACE=1` captured frames but every frame
resolved as `<unknown>` even after copying the matching `.pdb` alongside the
renamed `java.exe` binary (Windows symbol resolution did not pick it up;
not investigated further, out of scope for the time budget here).

**This plausibly explains why this session's 46 plain (non-diagnostic) boot
attempts all hung rather than reproducing either CCE**: if this same
corruption event happens silently (assertion off) at a comparable rate,
whatever object it corrupts plausibly feeds the `parallel-extension-add`
orchestration itself (a shared future/queue/executor/classloader-cache
state), and a native function silently reading a wrong-but-plausible-looking
object there could easily explain ~36 worker threads spinning forever making
no progress. This is a hypothesis, not proven, but it is a considerably more
economical explanation for the "42 threads then dead silence, never resolves
even at 180s" pattern than assuming the already-tracked STW-takeover hang
(`wildfly-standalone-boot-stw-jit-takeover-hang.md`) somehow got dramatically
more severe (100% vs. its documented ~91%/4-5) — that hang's own diagnostic
line (`STW cross-thread JIT takeover is still waiting for cooperative
mutators ... rounds=64 ... taken=0`) was never once observed in this
session's 46 plain attempts, including the one extended to 180s, which is
odd for that specific bug but expected if a *different* bug (this one) is
preventing the boot from ever reaching a GC pause that would trigger the
STW-takeover code path at all.

**Recommendation:** this deserves its own dedicated follow-up investigation
— it is a different, more tractable bug than the register-invisible-root
family (Family 1 bugs have a well-established, successful fix pattern:
`pin_native_root`/`read_native_pin` at the specific call site once located),
and if confirmed as the reason this session couldn't reach a live
`AttributeAccess` capture, fixing it would unblock a much more productive
line of investigation into this doc's actual target. Flagged as a follow-up
task; see the spawned background-task chip alongside this investigation.

## Conclusion

**Root cause: CONFIRMED as the already-tracked, currently-OPEN
"register-invisible JIT root" / precise-JIT-oop-map-coverage-gap family**
(the `SB-CRASH-04` residual within the default-on precise-maps machinery —
NOT the original, already-fixed A3 caller-frame gap). Reasoning, entirely
from source and cross-referenced independent investigations, not from a
fresh live capture this session (blocked by host contention and the
separate "Family 1" finding above):

1. `jit_typecheck_resolve`'s fast path (the path an already-loaded class
   like `AttributeAccess` takes) makes zero GC-triggering calls — confirmed
   by reading the function in full. Staleness cannot originate inside it.
2. The receiver argument reaching `jit_checkcast`/`jit_instanceof` was
   therefore already stale when the JIT-compiled caller loaded it into a
   register/stack slot — the corruption happened earlier, at whatever
   safepoint the JIT-compiled method's own control flow last passed through
   while still needing that value live.
3. Generational's young collector provably never relocates objects while
   any JIT frame is active (source-confirmed) — WildFly boots under this
   default backend with JIT required for the bug — so the "moving GC left a
   dangling old-address pointer" mechanism is structurally excluded here.
4. What remains, and is fully consistent with every observed symptom, is
   the non-moving sweep's *marking* phase missing a live root at a
   cooperative safepoint, due to the documented, still-open gaps in the
   default-on precise local-oop dataflow (CFG-merge AND-under-reporting;
   uncovered operand-stack temporaries) — exactly the mechanism three other
   independent 2026-07-10 investigations (DoHead, swallow-uploads,
   AccessLogValve) already root-caused via live register/core-dump analysis
   for structurally different call shapes, all citing the same open
   residual.
5. `CRATONVM_DBG_STALE_OBJREF` — scoped specifically to the *moving-GC*
   sub-case of stale-`ObjectRef` bugs — did not fire for this bug (it fired
   instead for a separate Family-1 site), which is the expected, predicted
   result given point 3, and is corroborating (not decisive) evidence
   against the moving-GC-relocation flavor of staleness for this specific
   bug.

**No fix attempted here**, per the project's established policy for this
family (`docs/internal/fixed-suite-bugs/dohead-jit-heap-corruption-register-invisibility-FIXED.md`'s
"Known accepted residuals," and the swallow-uploads/AccessLogValve docs'
identical conclusions): the real fix is completing the precise-oop-map/
shadow-stack infrastructure (tracked at
`docs/feature-designs/precise-jit-maps-default.md`), not a targeted patch at
any one call shape — every attempted narrow mitigation for this family
documented elsewhere (`CRATONVM_JIT_SAFEPOINT_REG_SPILL`,
`CRATONVM_SHADOW_STACK`, `CRATONVM_NO_SELECTIVE_PROMOTE`) has been tried and
found insufficient or actively broken.

## Related

- [[wildfly-remoting-classcastexception-parallel-extension-add]]
  (`docs/internal/fixed-suite-bugs/wildfly-remoting-classcastexception-parallel-extension-add.md`)
  — the sibling `AttributeDefinition` CCE, fixed (`70154861`); its own
  "Related" section first flagged the `AttributeAccess` residual this doc
  investigates. Updated to point here.
- `docs/known-issues/wildfly-standalone-boot-stw-jit-takeover-hang.md` — the
  separate, still-OPEN STW-takeover hang hit during the same boot step;
  confirmed still OPEN before starting this investigation. Not the same bug
  as either CCE, and likely not what this session's 46 hung attempts hit
  either (see "Separate finding" above) — its diagnostic warning line was
  never observed in any of this session's attempts, including one extended
  to 180s.
- `docs/internal/fixed-suite-bugs/dohead-jit-heap-corruption-register-invisibility-FIXED.md`,
  `docs/internal/tomcat-08-07/swallowabortedupploads-unexpected-socketexception-RESOLVED.md`,
  `docs/known-issues/README.md` (2026-07-10 AccessLogValve/RewriteValve
  entry) — the three prior independent occurrences of this same family this
  doc's conclusion is based on.
- `docs/feature-designs/precise-jit-maps-default.md` — the roadmap doc for
  the actual fix (precise oop maps / shadow stack completion).
- `docs/internal/wildfly-parallel-boot-stale-objectref-residual.md` — the
  "Family 1" bug class this session's `CRATONVM_DBG_STALE_OBJREF` run
  instead mostly caught a fresh (post-"FIXED") instance of; see "Separate
  finding" above.

## 2026-07-15 follow-up: generalized to `WFLYCTL0079` (any extension), "JIT required" DISPROVED, one real contributing site FIXED, residual re-characterized as a likely cross-thread GC-root-visibility gap

Investigated 2026-07-15, isolated worktree `/data/data/wt-io0079-20260715` (Azure host), forked from
`origin/dev @ 3a6bd4a6`, following up on a fresh sighting of `WFLYCTL0079: Failed initializing module
org.wildfly.extension.io` (the generic `parallel-extension-add` wrapper this doc's `AttributeAccess` CCE
also produces).

### This is the same bug, not `io`-specific, and not `AttributeAccess`-specific

A 12-attempt plain repro batch (default JIT-on, real JDK25, no diagnostic flags — matching this project's
standard WildFly repro recipe) against a fresh baseline binary hit `WFLYCTL0079` via
`ClassCastException: java.lang.Object cannot be cast to X` 5/12 times, with `X` and the failing extension
different on every occurrence:

| Run | Failing extension | Cast target `X` |
|---|---|---|
| 1 | `org.wildfly.extension.elytron` | `org.jboss.as.controller.registry.AttributeAccess` |
| 4 | `org.jboss.as.jaxrs` | `org.jboss.as.controller.registry.AttributeAccess` |
| 5 | `org.wildfly.extension.undertow` | `org.jboss.as.controller.registry.AttributeAccess` |
| 6 | `org.jboss.as.clustering.infinispan` | `java.lang.Comparable` |
| 11 | `org.jboss.as.jaxrs` | `org.jboss.as.controller.AttributeDefinition` |

A second such batch (see "Verification" under the fix doc below) also hit `WFLYCTL0079` directly against
**`org.wildfly.extension.io`** twice (cast targets `AttributeDefinition` both times), confirming the
original bug report's module name is circumstantial — whichever of the ~37-42 `parallel-extension-add`
worker threads happens to read a just-corrupted/reclaimed address first is the one that fails, and that's
a function of GC/scheduling timing, not anything specific to the `io`/XNIO extension itself. Other
observed cast targets across both sessions: `[Lorg.jboss.as.controller.registry.AttributeAccess$Flag;`
(an array type), `org.jboss.as.controller.capability.registry.RegistrationPoint`,
`org.jboss.as.controller.capability.registry.CapabilityRegistration`,
`java.util.function.Predicate`. This is one bug family, not several — updating this doc's status
accordingly rather than filing a separate `WFLYCTL0079` doc.

### "JIT is required" is DISPROVED — reproduces identically under `--nojit`

This doc's own "Conclusion" section (above) rests centrally on: "Generational's young collector provably
never relocates objects while any JIT frame is active... so the 'moving GC left a dangling old-address
pointer' mechanism is structurally excluded" when JIT is required for the bug. An 8-attempt repro batch
with `CRATONVM_DISABLE_JIT=1` (zero JIT frames anywhere in the process) hit the byte-for-byte identical
`ClassCastException: java.lang.Object cannot be cast to X` / `WFLYCTL0079` shape 4/8 times (cast targets:
`java.util.function.Predicate`, `org.jboss.as.controller.AttributeDefinition` ×2,
`org.jboss.as.controller.registry.AttributeAccess`,
`org.jboss.as.controller.capability.registry.RegistrationPoint`). **JIT is not required.** This doc's
"register-invisible JIT root" / `SB-CRASH-04` attribution — a gap specifically in the JIT's precise
local-oop dataflow tracking — cannot be the (sole) mechanism, since there is no JIT-compiled code involved
in a `--nojit` run at all. The underlying moving-GC-during-active-mutation mechanism this doc's own
"Conclusion" reasoned must be excluded is, per this new evidence, not excluded — it is precisely what
happens when JIT is off (Generational's own documented invariant only suppresses the moving collector
`while any JIT frame is active`; nothing suppresses it when there are none).

### Live root-cause chase: found and fixed one genuine, narrow contributing site

Live `CRATONVM_DBG_STALE_OBJREF=1 RUST_BACKTRACE=1` captures consistently isolated the panic to a
`java.util.stream` pipeline's deferred lambda dispatch:
`native_stream_to_array_gen` → `stream_process_chain` → `invoke_deferred_stream_lambda` → `invoke_virtual`
→ `coerce_lambda_args` → `checkcast_lambda_instantiated_args` → `lambda_arg_provably_not_instance` →
panic in `get_header`. Reading `invoke_virtual` (`vm/src/vm/vm_exec.rs`) found a genuine, unpinned-local
GC-safety bug at its lambda-dispatch decision point: `receiver` and `args` are read again (to build
`full_args`) *after* the `.filter()` predicate's `lambda_args_sam_compatible` call, which can itself
trigger class loading (a GC-triggering call) — and neither local was pinned across that window. **Fixed**
— see `docs/internal/fixed-suite-bugs/wildfly-invoke-virtual-lambda-sam-compat-stale-locals-FIXED.md` for
the full fix writeup and verification (`cargo test -p cratonvm-vm --lib`: 2202 passed / 9 pre-existing
unrelated failures, byte-for-byte identical before/after via `git stash`).

### The fix does not close the residual — and the residual is NOT another missed-pin site

Matched 12-attempt plain repro batches before/after the fix show `WFLYCTL0079`-via-CCE at essentially the
same rate (5/12 both). To find out why, temporary diagnostic instrumentation was added to
`checkcast_lambda_instantiated_args`'s per-argument loop (`vm/src/runtime/interpreter.rs`, reverted before
landing — not part of the committed fix) to log, for the exact argument index that panics, whether the
read went through the pinned path (`handles.get(idx) => Some(h) => thread.native_pin_roots[h]`) or the
raw unpinned fallback (`args.get(idx)`).

**Result: `via_pin=true` on every captured panic, both with JIT on and with `--nojit`.** The stale read is
not falling through to the unpinned fallback branch — it goes through the "self-healing" pinned path this
function's own GC-safety comment describes, and *still* observes a stale/reclaimed address. This rules out
"another site simply forgot to pin" as the explanation for the residual (that class of bug is
straightforwardly fixable per-site, as the `invoke_virtual` fix above demonstrates) and points instead at
something more fundamental: either the per-thread `native_pin_roots` remap performed during a moving GC
does not reliably reach every thread's pin table before that thread's own next read (a cross-thread
visibility/ordering gap, plausible given `parallel-extension-add` runs ~37-42 concurrently *executing*
mutator threads, each independently pinning/reading its own native locals while a GC initiated by any one
of them needs to freeze — or at least correctly observe — all the others), or an equivalent race in how a
newly-pushed pin becomes visible to a GC cycle that starts concurrently with the push. Both are
plausible restatements of the general "register-invisible root" concern this doc already tracks, but the
concrete mechanism is now better characterized as a **cross-thread GC-root-visibility/timing race**,
distinct from (though related to) the JIT-specific precise-oop-map dataflow gap (`SB-CRASH-04`) this doc's
original "Conclusion" attributed it to. Diagnosing the exact synchronization gap (in the GC's cross-thread
pin-scan/remap protocol, `gc/src/gen_heap.rs` and whatever cross-thread suspend/scan machinery it uses
outside the JIT-specific `vm/src/jit/xt_root_scan.rs` path) is its own dedicated investigation — deep
GC/threading infrastructure work, consistent with this doc's existing policy of documenting rather than
speculatively patching this family. Not attempted here.

### Updated conclusion

`WFLYCTL0079`/`AttributeAccess`/`AttributeDefinition`/etc. CCEs during `parallel-extension-add` remain
**OPEN**. One confirmed, narrow, real contributing site was found and fixed
(`wildfly-invoke-virtual-lambda-sam-compat-stale-locals-FIXED.md`), safe to land on its own merits
(verified via `cargo test`, zero regressions) but not sufficient to close this residual. The residual
itself is now better evidenced as a cross-thread GC-root-visibility race rather than confirmed to be the
JIT-only `SB-CRASH-04` gap the original investigation named — that attribution should be treated as
superseded by this section, not as still-authoritative. Whoever picks this up next should start from the
`via_pin=true` finding above rather than re-chasing individual unpinned-local sites.

## 2026-07-15 follow-up (second session): "excluded-while-running" DISPROVED by a purpose-built canary; two boot-wedge mechanisms in the same window root-caused and FIXED; CCE re-measured far lower

Investigated 2026-07-15, isolated worktree `/data/wt-wfgc-20260715` (Azure host), branch
`fix/wildfly-gc-pin-stream-20260715`, forked from `origin/dev @ a783d31f`. Probe artifacts (logs,
summary.txt, cores) under `/data/wt-wfgc-20260715/probes/` on that host.

### New tool: `CRATONVM_DBG_BLOCKED_ACCESS` — and what it disproved

The previous section left the residual characterized as a likely "cross-thread GC-root-visibility/timing
race" — a thread whose `in_blocked_region` flag makes the STW census exclude it while it actually keeps
running (its pins invisible to both the root scan and `fold_pointer_map_into_blocked`). This session
built a dedicated detector (`gc/src/blocked_access_debug.rs`): with
`CRATONVM_DBG_BLOCKED_ACCESS=warn|1`, any heap-header access (via the `GenerationalHeap::get_header`
funnel), any `pin_native_root`/`pin_native_object_values` push, and any interpreter-safepoint arrival
performed by a thread whose OWN `in_blocked_region` flag is raised is reported with a backtrace (or
panics). Registration is per-thread via `ThreadRegistry::set_os_tid_current`; default-off, one
cached-bool branch when off.

**Result: zero reports across 16 instrumented boots — including one boot that produced this doc's
exact `WFLYCTL0079`/`AttributeDefinition` CCE while the canary was live.** The excluded-while-running
mechanism does not occur in this workload; the previous section's leading hypothesis is disproved as
the CCE's cause. (A static audit of every VM-side raise/clear pairing done alongside — monitor paths,
park, join, class-init wait, JNI foreign attach, blocking-region begin/end — found them all correctly
paired post-GCAUDIT-0711, consistent with the canary's silence.)

### The same boot window's dominant failures were two OTHER, now-fixed mechanisms

Re-running this doc's standard repro recipe on `a783d31f` produced almost no CCEs (see below) but a
much higher rate of full boot WEDGES, in two distinguishable modes, both root-caused live this session:

1. **STW-barrier deadlock via census-counted CHM segment-monitor contenders** (the
   `STW cross-thread JIT takeover is still waiting for cooperative mutators rounds=64 pending=N taken=0`
   warning — reproduced 6/20 plain boots on `a783d31f`, *including under `--nojit`*, disproving the
   "JIT takeover" attribution in that warning's name). gdb-attach on a wedged boot +
   `CRATONVM_DBG_STW_CENSUS=1` identified the pending threads exactly: contenders inside
   `native_chm_put → ChmMonitorGuard::acquire → NativeContext::monitor_enter` — the plain,
   census-COUNTED monitor path — while the segment owner was parked at the barrier. The 2026-07-13
   session had already built the escape hatch (`monitor_enter_gc_safe`) but applied it to a single
   CountDownLatch site; the ConcurrentHashMap mutator family was the remaining live population.
   **FIXED** by converting all 14 live `ChmMonitorGuard::acquire` sites to a new `acquire_gc_safe`
   (blocking-region protocol; returned/relocated segment ref; every spanning local pinned and
   re-read). Post-fix: **0/49 runs show the warning** (vs 6/20 pre-fix).
2. **A Java-level lock-order cycle our CHM's coarse segment lock creates where real JDK bin-level
   granularity cannot** (the silent wedge that remained once mode 1 stopped masking it): compute-family
   callbacks run user code under the segment monitor; WildFly's registry callbacks take the
   management-registry write lock (`stamped_lock.rs::rw_write_lock`), while registry read-lock holders
   contend the same *segment* (different keys — different bins on HotSpot, so this graph is acyclic
   there). gdb showed 4 threads starving in `rw_write_lock` with a compute contender parked on the
   segment. **FIXED** by implementing real CHM's lock-free probes: `computeIfAbsent` on a PRESENT key
   and `computeIfPresent` on an ABSENT key now return without touching the segment monitor
   (JDK-exact). The absent-key `computeIfAbsent` still runs its mapper under the segment monitor
   (JDK runs it under the bin lock; atomicity preserved) — a residual, far narrower cross-key window
   real JDK does not have; if wedges recur, finer segment granularity is the next step.

While converting, several **pre-existing unpinned windows in the same CHM mutators** were also fixed
(`this`/`key`/`value` across `chm_key_hash`'s `hashCode()` dispatch; `values_equal`'s `equals()`
dispatch; the returned `current` across a put; `putAll`/copy-constructor entry vectors across
GC-triggering iteration). These are direct candidate mechanisms for this doc's CCE family — a stale
value stored under a moved-during-hashing window is read back later by an innocent thread as a
wrong-but-valid object (exactly the `via_pin=true` reader-side signature: the pin machinery worked;
the *stored value* was already wrong).

### CCE rate on current dev is already far below this doc's 5/12

Matched plain JIT batches: **1 CCE / 19 boots** on `a783d31f`+StreamDecoder-fix (0/12 batch A + 1/7
canary batch C), vs 5/12 measured by the previous section two dev-days earlier. The intervening dev
commits (`12bf61c6`, `8665d1ad`, `cf45bb80` — Stream/ArrayList pin fixes on the exact deferred-lambda
path the live captures blamed) plus this session's CHM pin fixes are the plausible causes. The one
captured CCE fired with the blocked-access canary live and silent (see above).

### The SIGSEGV bucket is the already-tracked JIT frame-slot/oop-map family — core preserved

3/12 baseline JIT boots SIGSEGV'd (0 under `--nojit`, all batches). One core was captured under the
canary binary and analyzed:
`/data/wt-wfgc-20260715/probes/cores/C_jitca_5-core.Thread.3097910.1784092895` (binary
`probes/cratonvm-wfgc-canary-20260715`, debug info intact). Signature: JIT-compiled code loads a frame
slot `-0x8(%rbp)` containing `0x360` (a small integer, not a pointer), passes its own null check, and
faults reading `0x15(%rax)` at `si_addr=0x375` — a frame slot the compiled code types as an oop holding
a non-oop value. This is the open register-invisible/oop-map family
(`docs/feature-designs/precise-jit-maps-default.md`, SB-CRASH-04); per standing policy no targeted
patch was attempted. The core is the first saved-artifact reproduction with symbols for that roadmap
work.

### Verification

- `cargo test --lib`: cratonvm-vm 2217/0 (baseline had 9 pre-existing failures), cratonvm-native-builtins
  2995/0 (baseline had 4), cratonvm-native-collections 72/0, cratonvm-native-io 349/0, cratonvm-gc
  875/876 (the one failure is `satb_pre_barrier_captured_during_concurrent_phase`, a pre-existing
  parallel-run flake; passes 3/3 in isolation).
- Barrier-wedge warning (`rounds=64 ... taken=0`): 6/20 plain boots pre-fix → 0 across every post-fix
  run.
- Standalone hang RATES this session are not cleanly comparable batch-to-batch: the shared host's load
  ranged 6→19 across the day (an OK boot takes ~11 s at load 6 and can exceed a 90 s timeout at load
  19), so marker-based metrics (warning lines, CCE lines, canary lines) are the reliable signals here.

### Updated status

The original `AttributeAccess`/`WFLYCTL0079` CCE remains formally OPEN (1/19 ≠ 0, and the mechanism of
the historical `via_pin=true` captures is still not positively identified — though the field of
candidates is now: stale-at-store CHM windows [fixed this session], NOT excluded-while-running
[disproved], NOT the census accounting [audited sound]). The boot-wedge failure modes that dominated
this window are fixed; the SIGSEGV family stays with the precise-maps roadmap. Whoever re-measures next
should use marker-based counts on a quiet host and treat any fresh CCE as highest-value live capture
(run with `CRATONVM_DBG_BLOCKED_ACCESS=warn CRATONVM_DBG_STALE_OBJREF=1`).
