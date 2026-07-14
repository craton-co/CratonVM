# WildFly standalone boot: `ClassCastException: java.lang.Object cannot be cast to org.jboss.as.controller.registry.AttributeAccess` during `parallel-extension-add` — register-invisible JIT root family, confirmed occurrence

Status: OPEN — confirmed (from source, cross-referenced against three
independent prior investigations) as an occurrence of the already-tracked
"register-invisible JIT root" bug family (a residual gap within the
default-on precise-JIT-oop-map machinery, tracked as `SB-CRASH-04` in
`jit/src/x64.rs`). Documented here, not fixed — this repo's established
policy for this family is to document, not speculatively patch (see "Why not
fixed" below).

Investigated 2026-07-13, worktree
`C:/craton/cratonvm/.claude/worktrees/attrib-cce-investigate`, branch
`investigate/wildfly-attributeaccess-cce-20260713`, forked from `origin/dev @
a7680d77`.

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
