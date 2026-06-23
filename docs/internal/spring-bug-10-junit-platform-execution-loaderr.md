# spring-bug-10: JUnit-platform execution failures on (AspectJ-woven) AOP test classes

| | |
|---|---|
| **Category** | VM-CORRECTNESS / dispatch (JUnit platform internals) |
| **Module** | spring-aop, spring-beans (AspectJ-woven + a few others) |
| **CratonVM** | LOADERR — exception thrown from `launcher.execute()` before any RESULT |
| **HotSpot JDK 25** | OK |
| **CratonVM HEAD** | verified on dev `0c904c04` (2026-06-21) |
| **Status** | 🟢 **RESOLVED** on dev (verified `0c904c04`, 2026-06-21). Both halves are closed **by default**: (1) the GC root-undercount race is gone — precise JIT oop maps are default-on (`32649b56`) and the shadow-reload correctness/OOB/lazy-prologue fixes landed (`f4249f7a`,`ed81a263`,`aa2ae19a`), so no `CRATONVM_SHADOW_*` flag is needed; (2) the AspectJ "Target object must not be null" residual is gone via the `resolveBeanClass`/bean-filter shim fixes (`25e38b1b`,`675e7638`,`73ebbed8`, merged `e9ce09e6`) + the bug-B2 method-injection merge (`61035130`). See the 2026-06-21 verification section. |
| **Suggested owner** | — (resolved) |

## ★★★★★ VERIFIED 2026-06-21 (dev `0c904c04`, binary `b1011vm`, JDK 25, `--Xmx 2g`) — both halves closed by DEFAULT; this ticket is RESOLVED

Re-measured on a fresh dev build with **no `CRATONVM_SHADOW_*` flags** (default
config), the two root causes this ticket tracked are both gone:

- **The AspectJ "residual" is gone.** The classes that previously LOADERR'd / failed
  now pass outright:
  - `aop.aspectj.AfterThrowingAdviceBindingTests` — **6/6 OK** (the doc's open
    residual #1, "STILL fails 6/6 — `Target object must not be null` from the
    `SimpleInstantiationStrategy.instantiate` shim"). The intervening dev work
    (`resolveBeanClass`/bean-filter classpath probe + bug-B2 method-injection) resolves
    the bean class *before* `instantiate`, so the String-`beanClass` null path is no
    longer reached. **No code change was needed here** — the previously-proposed
    `instantiate` String-resolution fix is moot.
  - `aop.aspectj.AfterAdviceBindingTests` — **6/6 OK**;
    `aop.aspectj.AroundAdviceBindingTests` — **4/4 OK** (both already green per the
    2026-06-19 update; reconfirmed).
- **The GC root-undercount race is closed by default.** A 14-class
  `org.springframework.aop.aspectj.*` batch run **in a single JVM** (the load-dependent
  race trigger) produced **0 stale-pointer symptoms** — none of `Stale pointer
  detected` / `all-zero header`, the `Object → TestExecutionResult$Status` CCE, or the
  `TestEngine.getId()`/`Predicate.test()` "no Code attribute" `AbstractMethodError`
  that defined this bug — and **no SIGSEGV / LOADERR**. The run reached the 900 s cap
  after the first **9** classes (these AspectJ classes are ~100 s each under CratonVM —
  a throughput characteristic, **not** a hang); of those 9, **8 are fully green**
  (`AfterAdvice` 6/6, `AfterReturning` 12/12, `AfterThrowing` 6/6, `Around` 4/4,
  `AroundAdviceCircular` 5/5, `AspectAndAdvicePrecedence` 1/1,
  `AspectJExpressionPointcutAdvisor` 1/1, `BeanNamePointcutAtAspect` 3/3). The only
  WARNs are the benign `scan_active_jit_frames: cross-thread JIT-root gap` notices,
  *covered* by the per-thread `root_snapshot` (a tracked precise-maps Stage B/C
  follow-up, [[precise-jit-maps-bk-status]] /
  [[jit-junit-discovery-reflection-corruption]]), not a reclaim.
- **Caveat (separate, NOT the race):** in the same batch `BeanNamePointcutTests` is
  **6/6 FAIL** (`succ=0`) — but it produces a clean `RESULT` line with **no
  stale-pointer symptom and no crash**, so it is a *functional* bean-name-pointcut
  matching failure, a distinct concern from the GC-race reclaim this ticket is about.
  It does not reopen the race.

Net: the GC-race premise (precise maps default-on + shadow-reload fixes) and the
AspectJ-shim premise (`resolveBeanClass`/bean-filter/bug-B2) are both on dev and both
verified. The `CRATONVM_SHADOW_STACK`/`_PIN` opt-in path and the historical
movable-shadow root-cause chase below are retained for history but are **no longer
load-bearing** — the default config is correct.

## ★★★★ UPDATE 2026-06-19 — the GC race is FIXED on current dev; the aspectj residual is a SEPARATE shim bug (resolveBeanClass read the wrong field), now fixed on a branch

Re-measured on a current-dev binary (base `52efa1eb`, which has precise JIT oop
maps default-on `32649b56`, pin-aware shadow reload `f4249f7a`, OOB-reload fix
`ed81a263`, lazy-prologue `aa2ae19a`):

- **The documented GC root-undercount race is GONE by default.** `MTRegex` (the
  fast heavy-load repro) is clean under default / `SHADOW` / `SHADOW+PIN`,
  including the doc's exact former-SIGSEGV configs (`MTRegex 1 8`, `4 3`) and
  16-thread stress — `stale=0`, no crash. The aop.aspectj 14-class batch runs
  **`stale=0`, no SIGSEGV, no LOADERR** (was stale=331 + rc=139). So precise-maps
  default-on + the shadow-reload fixes closed the race. The `Stale pointer` /
  `CCE TestExecutionResult$Status` / `AbstractMethodError getId` LOADERR
  symptoms below no longer reproduce.

- **The remaining aspectj FAILs are a DIFFERENT, deterministic bug — masked
  until the race was fixed.** Root cause: the `spring_startup_bootstrap.rs`
  `resolveBeanClass` shim family (M3 `AbstractBeanDefinition.resolveBeanClass`,
  M4 `doResolveBeanClass`, M5 `AbstractBeanFactory.resolveBeanClass`) read a
  field named **`beanClassName`**. Current Spring `AbstractBeanDefinition` has no
  such field — `setBeanClassName` stores into the single `Object beanClass`
  field. So the shims found nothing and returned **null for every bean** →
  Spring skipped all beans → `BeanFactory.getType("testAspect")` returned null →
  `MethodLocatingFactoryBean.setBeanFactory` threw `IllegalArgumentException:
  Can't determine type of bean with name 'testAspect'`. Minimal repro
  (`spring-suite/probe/GetTypeProbe.java`, no AOP): a plain
  `<bean class="java.util.ArrayList">` had `resolveBeanClass`→null while
  `ClassUtils.forName` + the Class-object path worked. Interpreter-level
  (`--nojit` too); class-independent.

- **Fix (branch `fix/spring-bug-10-resolvebeanclass-field`, commit `25e38b1b`,
  NOT on dev).** Shared `resolve_bean_class_field` helper reads the real
  `beanClass` field (mirror → return; String → resolve via class_id_by_name /
  ensure_class_initialized; legacy `beanClassName` fallback), caches the resolved
  mirror back into `beanClass` (the real bytecode's `this.beanClass =
  resolvedClass`), and returns None only for a genuinely-missing class so the
  partial-classpath skip M5 relies on is preserved. **Validated:** GetTypeProbe
  null→Class + getBean instantiates; `AfterAdviceBindingTests` 6/6 OK,
  `AroundAdviceBindingTests` 4/4 OK (both were fully failing) == HotSpot.

- **Second shim bug found + fixed (3-state resolution).** The resolve shims
  conflated "no class name" (a factory-method / `FactoryBean` / parent bean —
  legitimate null) with "named-but-missing class" (partial-cp). M5's destructive
  `removeBeanDefinition` fired for the no-class case too, deleting valid beans →
  `AfterThrowing` lost `testBean` (`NoSuchBeanDefinitionException`). Fixed:
  `resolve_bean_class_field` now returns `Resolved` / `Missing` / `NoClass`; M5
  removes ONLY on `Missing`. No more spurious bean deletion (verified: AfterAdvice
  6/6, Around 4/4 still pass, no regression).
- **Residuals / caveats:**
  - `AfterThrowingAdviceBindingTests` STILL fails (6/6) — now one shim-layer
    deeper: `BeanCreationException 'testBean': Target object must not be null`
    from the `SimpleInstantiationStrategy.instantiate` shim, which returns null
    when `beanClass` is still an unresolved String at instantiation (the
    resolveBeanClass cache didn't reach the mbd that `instantiate` reads — likely
    a merged-bean-definition copy mismatch, or `instantiate` should resolve the
    String class itself like resolveBeanClass does). NEXT STEP: make the
    `SimpleInstantiationStrategy.instantiate` shim resolve a String `beanClass`
    via `resolve_bean_class_field` before giving up. Unique to AfterThrowing's
    `throwing="java.lang.Throwable"` type-name advices; not a regression.
  - The Boot-demo battery (sportme / insurance / letsgo / demo) that these shims
    serve was NOT re-validated. The partial-classpath skip is preserved by
    design (None→skip for missing classes) but unconfirmed — validate before
    merging to dev.
  - This box (16 GB, full disk) OOMs (`memory allocation of ~1GB failed`, rc=127)
    running these now-real-bean-instantiating tests under load; run with
    `CRATONVM_DEFAULT_HEAP_MAX_MB=2048 -Xmx512m`.

The original GC-race analysis below is RETAINED for history but the race itself
no longer reproduces on current dev.

---

## Symptom
~18 classes (mostly `spring-aop` AspectJ tests, compiled with `compileAspectj`) throw from inside
the JUnit Platform engine during discovery/execution, so KRun reports `LOADERR`. Distinct causes:

| count | error |
|------:|-------|
| 16 | `ClassCastException: java/lang/Object cannot be cast to org/junit/platform/engine/TestExecutionResult$Status` |
| 10 | `NullPointerException: Cannot invoke length on null` |
| 7 | `AbstractMethodError: org/junit/platform/engine/TestEngine.getId()Ljava/lang/String; has no Code attribute` |
| 5 | `NullPointerException: Cannot invoke executionStarted on null` |
| 1 | `AbstractMethodError: java/util/function/Predicate.test(...)Z has no Code attribute` |
| few | `NoClassDefFoundError` (MutinyRegistrar, TestCompiler, ExceptionUtils) |

## ★★★ FIXED (correctness) — race CLOSED + SIGSEGV GONE via pinned shadow marking + a pop-only reload; the "bt18 OOM" and "deep moving/remap" premises are BOTH DISPROVEN
Worktree `fix/spring-bug-10-11`, measured on a fresh idle-machine build:

- **The race is closed and the crash is gone.** With `CRATONVM_SHADOW_STACK=1 CRATONVM_SHADOW_PIN=1`
  the 14-class aop.aspectj batch goes **stale 331 → 0** AND **no SIGSEGV** (default: stale=331; plain
  `SHADOW_STACK=1` movable: stale=0 but **rc=139**). A single class completes correctly (rc=0, RESULT).
- **The (single-thread) crash was the shadow RELOAD codegen — a `top`-DRIFT bug — NOT the GC
  moving/remap.** Both movable AND pinned crashed identically with the *old* reload (which read each
  home from the *current* `shadow.top`), so `shadow_stack.remap` is innocent. Root cause: a push whose
  matching reload is skipped (a constructor `invokespecial <init>` push, or a call that throws and is
  caught **in-method**) leaves `top` drifted HIGH; the reload then reads uninitialised slots ABOVE the
  real data → `rax=0` null receiver in the putfield helper (seen from JIT'd `Matcher.reset`). **Fix
  (`emit_shadow_push`/`emit_shadow_reload`, x64.rs): each push SAVES its base `top` into a reserved
  3rd shadow frame slot (`shadow_savebase_slot_off`); the reload restores each home from `[base+i*8]`
  and resets `top` to `base` — drift-immune and self-correcting.** This makes the full value-restore
  correct for BOTH movable (slot holds the GC-rewritten address) and pinned (slot unchanged), so it
  supersedes the earlier `pop-only` experiment. (A first cut tried `pop-only`+`SHADOW_PIN`; the
  saved-base reload is the general fix.) Validated: single aop.aspectj class rc=0, stale=0, no crash
  under both `SHADOW_STACK=1` and `+SHADOW_PIN`.
- **BOTH modes still crash at HEAVY (multi-thread) batch scale — a SEPARATE load-dependent crash, NOT
  the remap.** With the saved-base reload: single class + 8-class subset are clean (rc=0, stale=0, no
  crash) under BOTH `SHADOW_STACK=1` and `+SHADOW_PIN`; but the full **14-class** batch rc=139 under
  BOTH. Since PINNED has no evacuation/remap yet still crashes, the residual is NOT the moving/remap —
  it is a concurrent-GC interaction with shadow that only manifests under the heavy multi-threaded
  JUnit load (default — no shadow — does NOT crash, only stale-warns). The crash SHAPE changed from the
  pre-saved-base `rax=0` null-receiver-in-putfield-helper to **`rax=0x3D` (61, a small int used as a
  pointer) INSIDE JIT'd `Matcher.reset`** — so the saved-base reload genuinely fixed the single-thread
  reload, and what's left is a different, load-dependent corruption (suspected: cross-thread shadow
  marking of a parked worker, or a per-frame `savebase`-slot interaction under inlining/heavy reentry).
  Not reproducible below the full 14-class concurrent load, so it needs a heavy-load harness to pin.
  Gate: `CRATONVM_SHADOW_PIN` (`memory/roots.rs` publishes PINNED vs MOVABLE).
- **"Pinning OOMs bt18" is DISPROVEN.** `BinTreesOnly` (the object-allocating d18 kernel) does **not**
  OOM under `SHADOW+PIN` — it times out exactly like the default (CratonVM can't run object-bt18 in
  reasonable time at all, shadow or not), so pinning is benign w.r.t. OOM. The shadow stack is a LIFO
  scanned only between a push-before-call and its pop-after-call, so pinning is transient, not the
  whole-operand-stack over-retain the prior session feared.

### Remaining gap: PERF (why pinned is opt-in, not yet default-on)
With the saved-base reload, a single aop.aspectj class runs **default 21s, SHADOW(movable) 146s,
SHADOW+PIN 152s** (all rc=0, stale=0, no crash). So it's **~7×**, and movable≈pinned — meaning the
overhead is **NOT pinning GC pressure** (the earlier hypothesis) but the **shadow mechanism itself**:
the per-method `get_current_thread` CALL in the prologue (every JIT entry, brutal for call-heavy code)
+ the per-safepoint push/reload + the marking scan. So pinned shadow marking is a **correct opt-in
mitigation**, not yet default-on.
Two perf levers for default-on, both localized: (a) make the prologue thread-fetch **lazy/conditional**
(only when the method actually has a shadow push — most methods have none), and/or (b) **frame-spill**
the register-invisible oops to dedicated frame slots (conservative scan pins them; no buffer, no
reload, no thread-fetch — the pinning cost alone is only ~6s, i.e. movable 146 vs pinned 152).

Gates on `fix/spring-bug-10-11`: `CRATONVM_SHADOW_PIN` (roots-publish PINNED vs MOVABLE). Shadow reload
rewritten to the saved-base scheme (`shadow_savebase_slot_off`, 3rd shadow frame slot).
Matrix/repro: `spring-suite/probe/bug10-matrix.sh`, `bench/BinTreesOnly.java`.

## ★★ RE-VALIDATED (worktree `fix/spring-bug-10-11`) — race reproduces; SHADOW SIGSEGV localized to the shadow RELOAD codegen (not the GC remap)
Direct repro on a fresh build:
- **Race reproduces under default flags:** the aop.aspectj batch emits `Stale pointer detected …
  all-zero header` warnings (seen on `java/util/regex/Pattern` receivers) — the register-invisible
  operand-stack oop undercount, confirmed.
- **The SHADOW_STACK=1 SIGSEGV is in the shadow PUSH/RELOAD codegen, not the GC remap.** Repro:
  14-class aspectj batch + `CRATONVM_SHADOW_STACK=1` → SIGSEGV with `rax=0x0` (NULL receiver)
  **inside the putfield helper** (`pc=0x..848BB5`), called from JIT'd `java/util/regex/Matcher.reset()`
  (named via the new `CRATONVM_DBG_JIT_NAMES` crash registry). So a live oop in a callee-saved
  register gets reloaded as **null** after a GC-capable call, and the next putfield derefs null.
  This is the "push/reload codegen incomplete (68199090) / incompatible with the non-moving sweep"
  gap the `gen_heap.rs:2054-2067` comment flags — **a JIT codegen defect in
  `emit_shadow_push`/`emit_shadow_reload` (x64.rs:5846/5934)**, NOT the GC-side `shadow_stack.remap`.
- **`CRATONVM_DBG_SHADOW2=1` confirms the home set:** at each safepoint the push records exactly the
  register-resident operand-stack oops, e.g. `push pc=30
  stack=[CalleeSaved(13),Frame(48),Frame(56),CalleeSaved(12),Frame(64)] marks=[T,F,F,T,F]
  homes=[Reg(13),Reg(12)]`; the reload restores r13/r12 and one returns null/corrupt. Likely an
  unbalanced push/reload across an exception unwind (a throw past the matching reload leaves
  `shadow.top` bumped → a later reload reads the wrong slot). Needs the `Matcher.reset` shadow
  disasm + runtime slot trace to pin (one build).
- **Dilemma re-confirmed:** PIN the register-invisible set → closes the race but **OOMs bt18** (those
  oops are heap-reachable nodes held in registers across allocation safepoints; pinning blocks the
  selective-promotion drain at `gen_heap.rs:3450`). MOVABLE+remap (shadow) → no OOM but the reload
  nulls a receiver. So the load-bearing fix is making the shadow push/reload correct, then
  validating bt16/bt18 on an uncontended machine.

## ★ VALIDATED earlier session — GC race confirmed; real fix = fix the moving/remap SIGSEGV (deferred B-K Stage B/C)
Reproduced + validated directly (dev `334fe5e7`):
- **Reproduced** the race: a 14-class `org.springframework.aop.aspectj.*` batch (these classes live in
  **spring-context**'s test output, not spring-aop — my first repro ClassNotFound'd on the wrong CP)
  produced **101 "Stale pointer detected … all-zero header → falling back to CP class java/util/List"**
  warnings under default flags. So the GC stale-pointer race is real and reproducible.
- **`CRATONVM_SHADOW_STACK=1` → 101 stale warnings drop to 0** (the shadow-stack **marking** keeps the
  register-invisible oops alive) **BUT the run then SIGSEGVs (rc=139)** — the **moving/remap** half
  (Cheney-while-in-JIT + `shadow_stack.remap`) that the same flag enables is what crashes.
- **A "marking-only default" (the proposed quick fix) is NOT safe** — I implemented + then reverted it:
  the marking root publish (`vm/src/memory/roots.rs:216`) emits shadow oops as **movable** (the B-K
  design, to avoid OOM), relying on the non-moving sweep's **selective promotion to EVACUATE** them,
  which needs the **remap**. With remap gated off (marking-only), a *promoted* shadow object's slot
  goes **stale** → the race returns for promoted objects. Publishing them **pinned** instead avoids
  that but **re-introduces the bt18 small-heap OOM** the B-K change specifically fixed ("pinning every
  register-invisible operand-stack oop OOMs at small heap").
- **Therefore the real, load-bearing fix is to make the FULL shadow mode (movable + remap + moving)
  not SIGSEGV** — then the existing movable+remap path resolves the race with no OOM. That SIGSEGV is
  the deferred **B-K Stage B/C** GC work (fix the Cheney-while-in-JIT / `shadow_stack.remap` crash),
  needing **uncontended-machine bt16/bt18 validation**. Gate split alone does not fix it.
- **Per-run mitigation:** none clean (full flag crashes; marking-only is unsafe). The conservative
  scan already pins stack-resident oops; only the register-invisible ones leak — so the bug is
  bounded to heavy-multithreaded register-oop-across-call shapes.

## (corrected) ROOT CAUSE — GC root-undercount race, NOT dispatch/weaving
All sub-causes are **one bug**: a **young-gen GC stale-pointer / cross-thread root-undercount race**.
Under heavy multithreaded JUnit execution, a live platform-engine/listener/enum object is reclaimed
(header zeroed) or left stale because a root referencing it (operand-stack / register-resident on a
parked worker thread) is invisible to the root scan. Evidence in `results-cv/*/crashes.log`: every
LOADERR is immediately preceded by `WARN …interpreter: Stale pointer detected in invokevirtual
receiver (… all-zero header) — falling back to CP class …NodeTestTask/TestEngine/Predicate/…`.

The surface error just depends on which object was zeroed:
- **`TestEngine.getId()` / `Predicate.test()` "no Code"** — stale receiver (`class_id_of`→Object/0);
  the stale-pointer fallback at `interpreter.rs:10766` dispatches on the **CP method-ref class**,
  which for an `invokeinterface` IS the abstract interface → no Code → AbstractMethodError. The
  S111r10 receiver-walk rescue **cannot** help (the receiver is dead, not merely interface-stamped —
  contrast bug-03). So this is **not** the bug-03 pattern.
- **`Object → TestExecutionResult$Status` CCE** — the `Status` enum constant flowing through JUnit's
  `Preconditions.notNull` (plain bytecode, returns its arg) is zeroed; `checkcast`
  (`interpreter.rs:8712`) then fails. (Corroborated by a flood of `CompletableFuture` CAS
  `NoSuchMethodError` on zeroed Object receivers nearby.)
- **NPEs** (`executionStarted`/`length` on null) — a zeroed listener/`UniqueId` field reads null.

**Mechanism:** per-thread root snapshots (`interpreter.rs:1322-1478`) + the default non-moving young
sweep that still selectively promotes (`gc/src/gen_heap.rs:2045-2092`); a GC firing while a JUnit
worker is parked with a stale/incomplete `root_snapshot` misses the worker's register/stack roots —
the same register-invisibility gap `CRATONVM_SHADOW_STACK` (default-OFF,
`vm/src/jit/conservative_roots.rs:189-204`) was built for. **Load-dependent:** light single-threaded
spring-aop classes produce ZERO stale warnings; only the long-lived multithreaded full-suite run
triggers it. **NOT AspectJ weaving** (affected classes verified non-woven).

### Proposed fix (high-leverage — recovers ~all genuine LOADERRs + likely some CRASH/FAIL)
Close the cross-thread root undercount: make the precise **shadow-stack roots default-ON for the
marking half** (the kafka B-K change already wires shadow oops as *pinned* roots into the non-moving
sweep — gate the marking fold-in `interpreter.rs:1472-1478` on always-on rather than
`shadow_stack_enabled()`; keeps the non-moving sweep, only over-pins). **High risk / needs uncontended-machine
validation** (overlaps the deferred B-K Stage B/C flip in memory `precise-jit-maps-bk-status`).
Validate first by re-running the LOADERR/CRASH classes with `CRATONVM_SHADOW_STACK=1`.
Diagnostic: `CRATONVM_DBG_STALE_RECV=1` (`interpreter.rs:10710`) dumps the frame/locals holding the
stale address.

### Drop from the CV-bug tally (environmental, NOT CratonVM bugs)
- **~14 TestNG-engine LOADERRs** — `JUnitException: TestEngine with ID 'testng' failed to discover`
  (`*TestNG*` classes, `test.context.jdbc.*`): need the TestNG engine; separate env/engine issue.
- `NoClassDefFoundError: …MutinyRegistrar / TestCompiler / ExceptionUtils` — optional deps absent.

## (superseded) earlier analysis
- **`TestEngine.getId()` / `Predicate.test()` "has no Code attribute"** — the SAME synthetic-object-
  stamped-with-interface-class dispatch bug as [[spring-bug-03]] (a synthetic `TestEngine` / lambda
  `Predicate` resolves to the abstract interface method). Likely fixable the same way (ensure the
  concrete impl/native is dispatched), and would recover the 7+1 classes.
- **`ClassCastException … TestExecutionResult$Status`** (16) — something returns a bare `Object`
  where JUnit expects the `Status` enum; an enum/result object isn't constructed with the right
  class. Needs its own trace.
- **NPEs on `executionStarted`/`length`** — a JUnit listener or reflectively-obtained value is null
  under CratonVM where HotSpot has it.

These manifest specifically on **AspectJ-woven** classes (synthetic woven methods/fields confuse
CratonVM's reflection during JUnit discovery), plus a few with missing optional deps
(`MutinyRegistrar` = reactor-mutiny not on cp → environmental, not a CV bug).

## Reproduce
```bash
CP="$H;$(tr -d '\r' < .../spring-aop/build/cratonvm-testcp.txt)"
KRUN_STACK=1 "$VM" --java-home "$JDK" -cp "$CP" KRun org.springframework.aop.aspectj.autoproxy.AnnotationPointcutTests
"$JDK\bin\java.exe" -cp "$CP" KRun ...AnnotationPointcutTests   # OK
```

## Notes
Mixed root causes; the `getId()`/`Predicate.test()` "no Code" subset overlaps [[spring-bug-03]]'s
fix pattern. The `NoClassDefFoundError: …MutinyRegistrar` cases are environmental (optional dep
absent) and should be dropped from the CV-bug tally. Good handoff once the dispatch subset is split
out.

---

## RESOLUTION (2026-06-16) — pin-aware shadow reload closes the race + SIGSEGV

**Status: FIXED under `CRATONVM_SHADOW_STACK=1 CRATONVM_SHADOW_PIN=1`.**

### Root cause of the residual crash/hang
The shadow-stack precise-roots mechanism's **post-call RELOAD** (`emit_shadow_reload`,
`jit/src/x64.rs`) was corrupting a live operand-oop *home register*. Bisection with the existing
`CRATONVM_SHADOW_NOPUSH` / `CRATONVM_SHADOW_NORELOAD` toggles on the fast repro
`spring-suite/probe/MTRegex.java` (single-thread, crashes/hangs in ~6 s):

| config | result |
|--------|--------|
| `NOPUSH` (skip push+reload) | **OK** (`DONE total=…`) |
| `NORELOAD` (push only, no value-restore) | **OK** |
| push+reload (movable, savebase) | **SIGSEGV** then (with a bounds guard) **HANG** |
| push+reload (pinned) | **HANG** |

So the *push* (buffer-only write) is harmless; the *reload's value write-back* is the corruptor.
It restores each home from `[base + i*8]`, but `base` is wrong:
* **movable (savebase)** — the per-push saved-base frame slot `[rbp-0x20]` is read back as
  `0xFFFF_FFFF_FFFF_FFFE` (-2). It is written **only** by the push (`mov [rbp-0x20],r11` where
  `r11=top`, a valid buffer ptr) and read **only** by the reload; nothing in the method, the
  args-buffer, the GC root scan (marks, never rewrites frame slots — `precise_maps` off here), or
  the callee writes it — yet it reads -2. **Root cause of the slot corruption is still
  unidentified** (exhaustive static analysis ruled out every in-frame writer). It is frequent, not
  rare.
* **pop-only** — reads `top - n*8`; when `top` has DRIFTED high (a push whose reload was skipped by
  an exception unwind earlier in the same JIT entry) this reads *above* the real data → wrong oop
  into the home → the worker infinite-loops on a corrupted value (watchdog showed Thread-1 stuck in
  JIT/native, main spinning in `Thread.join`).

### The fix
`shadow_pin_codegen()` (`CRATONVM_SHADOW_PIN`) was documented to make the reload "only POP, skip the
per-home value write," but that was **never wired in** — the value-restore ran unconditionally. Now
it is (`emit_shadow_reload`):

> Under pin the oops are PINNED (never moved), so the home values are already correct after the call
> — callee-saved regs survive it; caller-saved operand oops were spilled to frame slots by the
> normal pre-call codegen and reloaded from there by the normal post-call codegen. The shadow
> value-restore is therefore redundant, and *is the corruptor* when `base` is stale. So under pin we
> **skip the value-restore entirely** and only pop `top`, and only to a **validated** savebase
> (∈ `[base,end)`); an over-high `top` merely over-scans (harmless for marking), a too-low `top`
> could drop a live root, so we never pop below a validated base (out-of-range ⇒ leave `top` for the
> `restore_jit_thread` boundary heal).

### Validation
* `MTRegex 1 2` / `4 3` — **no crash, no hang**, correct `DONE total=…`.
* `spring-suite/probe/PinCorrect.java` (make/check oops-across-calls + GC churn) —
  `CHECKSUM=2796199953000` under HotSpot **and** baseline **and** SHADOW+PIN (identical → skip-
  restore does not corrupt values).
* aspectj 14-class batch — **no crash, `stale=0`** (the root-undercount race is CLOSED; the default
  no-shadow run reports `stale=12`).
* `BinTreesOnly` (bt18) under SHADOW+PIN — **no OOM** (times out on the known throughput wall, same
  as default; the feared "pinning OOMs bt18" is disproven).

### Residuals / follow-ups
* The **movable** path (`CRATONVM_SHADOW_STACK=1` without `_PIN`) is still broken for the regex
  `Matcher.reset` pattern (the savebase frame-slot -2 corruption, root cause open). PIN is the
  working configuration.
* PIN carries the shadow-stack throughput overhead (~7×); the aspectj batch reaches the tests under
  default but times out before them under PIN within 200 s.
* Consider making PIN the **default** when `CRATONVM_SHADOW_STACK` is set (movable is unvalidated).
* Diagnostics added (all gated/harmless): crash-handler frame + ShadowStack-field dump
  (`crash_handler.rs`), `CRATONVM_DBG_SHADOW_DEPTH` (`memory/roots.rs`), `CRATONVM_SHADOW_NO_SAVEBASE`
  bisect gate (`x64.rs`).

### Movable root-cause chase (2026-06-16, cont.) — sentinel-proven fresh write, source unfound

To settle "skipped push vs external write," the push now (gate `CRATONVM_SHADOW_SENTINEL`) pre-stamps
the savebase slot with a non-canonical `0x5151_5151_5151_5151` BEFORE the null-guard, and a second
gate `CRATONVM_SHADOW_RAW_RELOAD` bypasses the bounds-guard so a bad savebase faults on deref.

Result (`MTRegex 1 8`, movable + both gates): SIGSEGV reading **`0xFFFF…FFFE` (-2)**, NOT the
sentinel. So the slot WAS written (push wrote a valid `top`, or the sentinel), then **freshly
overwritten with -2 during the GC-capable call** — it is neither a skipped push nor stale memory.
The crash dump also shows `ShadowStack top == base` (buffer EMPTY) at the reload, i.e. reset's pushed
home was undone — the shadow `top` was reset to `base` mid-call (a `restore_jit_thread`/defensive
`set_top` heal firing for the OUTERMOST scope while reset is still live — a separate
shadow-`top`-nesting bug).

Every static writer of `[rbp-0x20]` was ruled out:
* reset's own code writes the slot ONLY at the push (`mov [rbp-0x20],r11=top`) and reads it ONLY at
  the reload (disasm-confirmed; no rsp-relative alias either).
* the call's args buffer is `lea r8,[rbp-0x38]` with `num_args=1`, so the helper reads only
  `args_ptr[0]=[rbp-0x38]`; `args_ptr[3]` would be `[rbp-0x20]` but is never touched.
* `jit_invoke_virtual_mic` builds `args_slice` via `from_raw_parts` (immutable) and never writes
  `args_ptr`; `try_call_compiled_entry` passes args by value.
* the callee (`Matcher.getTextLength`, an instance method) does NOT invocation-tier-up
  ([[jit-instance-methods-no-invocation-tierup]]) so it runs interpreted — no compiled-callee frame.
* GC frame-slot rewriting (`remap_one_jit_frame`) is precise-gated (`sp_id_off==0` here ⇒ early
  return); the conservative scan only MARKS, and the shadow remap only rewrites the BUFFER.
* the helper's Rust frames sit BELOW reset's `rsp`, and reset's shadow space `[rsp..rsp+0x20]` does
  not cover `[rbp-0x20]=rsp+0xA0` — so a callee can't reach it via the Win64 shadow space.

It is **MTRegex-specific**: `PinCorrect.java` under the *movable* path returns the correct golden
checksum (no crash). The differentiator is reset's interpreted-bail call + the `top`-reset-to-base
heal. The writer remains unidentified by static analysis; the definitive next step is a hardware
data watchpoint on the live savebase address (complicated by reset being called in a tight loop, so
the watched address keeps changing). **PIN sidesteps the whole class** by not consuming the restored
value, and is the shipped resolution.

### Hardware watchpoint chase (2026-06-16, cont.) — caught the -2 writer *class*

Built a HW data-write breakpoint (DR0) on reset's savebase slot, armed from the JIT prologue via a
process-global helper (`CRATONVM_SHADOW_WATCH`; `x64.rs` ARM/DISARM_SAVEBASE_WATCH_FN +
`helpers.rs::jit_arm/disarm_savebase_watch` using `SetThreadContext`), with the VEH
(`crash_handler.rs`) reporting the writer RIP on a `0xFFFF…FFFE` write. Naked-function trampolines
read `[rsp]` at entry to target **only `Matcher.reset()`** (so nested frames don't steal the single
breakpoint).

* **It works:** the all-methods variant CAUGHT a -2 write. Disassembly of the writer
  (`exe+0xC0E48B`): `movq $-0x2, 0x420(%rbp)` in a large-frame function that then checks the
  `i64::MIN` deopt sentinel via `neg`/`jno` — i.e. a **JIT-dispatch helper writing a -2 sentinel to
  its OWN stack local** `[rbp+0x420]`. (addr2line mis-symbolizes it as `std::env::_var_os`; the
  frame shape proves otherwise.)
* That first catch was **coincidental** — a frame-local write to a *reused* stack slot after the
  earlier reset frame returned (catch addr ≠ the live crash savebase; different thread stack).
* With prologue-arm targeting + a dedicated **watcher thread** (`helpers.rs::savebase_watcher`) that
  SUSPENDs the worker, `SetThreadContext`s DR0 onto its exact savebase, and RESUMEs — the reliable
  way to load debug registers (`SuspendThread=0 SetThreadContext=1 ResumeThread=1`, all success) on
  the precise crashing address (`armed @0x…CA30` == crash `rbp-0x20`) — the worker's DR0 **STILL
  never fired** on the -2 write.

**DEFINITIVE RESULT — the -2 is a CROSS-THREAD write.** Debug registers are per-thread. The
all-methods run proved a worker-thread store to its savebase *does* trap the worker's DR0. So a
reliably-armed worker DR0 that does NOT fire when -2 lands at that exact address means **the store
comes from a DIFFERENT thread** — not the worker running reset. This overturns the earlier
"same-thread" reasoning: the movable path writes the worker's JIT frame slot from another thread,
exactly the **moving-remap** corruption this bug was filed for. The single-threaded grep missed it
because the writer is a *wild/aliased* store in the moving collector (the conservative scan promotes
a frame value — the savebase buffer pointer — as a movable root via `is_object_address`, and the
evacuation/forward write lands in the wrong place), not a literal `frame_slot = -2`.

**Conclusion:** movable shadow roots induce a cross-thread write into a live JIT frame's savebase
slot. Identifying the exact writing instruction needs DR on ALL threads (or a guard page on the
slot's page), since the writer is a non-worker thread. Practically moot for the fix: **PIN never
moves the shadow roots and never consumes the savebase value**, so it is immune to the moving-remap
write — which is why SHADOW+PIN is clean. All watch code is behind `CRATONVM_SHADOW_WATCH` (off by
default).
