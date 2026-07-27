# `ConfigurationPropertiesBeanRegistrationAotProcessorTests` — ALL THREE issues RESOLVED (see closure note)

Class: `org.springframework.boot.context.properties.ConfigurationPropertiesBeanRegistrationAotProcessorTests`
(`core/spring-boot`). Originally found investigating Spring Boot core39
residual Cluster A (`spring-boot-core39-residual-clusters-20260723.md`),
2026-07-24. Re-investigated 2026-07-25/26 (worktree
`wt-cpbrap-hang-20260725`, branch `fix/cpbrap-hang-20260725`, Azure host).
Closed 2026-07-26 (worktree `wt-cpbrap-fix2-20260726`, branch
`fix/cpbrap-jitbugs-20260726`, Azure host) — see "Closure" section at the
bottom; this doc has moved to `docs/internal/fixed-suite-bugs/` as part of
that closure, per the known-issues-vs-internal convention (known-issues
holds only unfixed bugs; a doc moves out once fully resolved).

## Status: original Hibernate-Validator hang no longer reproduces — CONFIRMED FIXED

The `--nojit` hang described below (BeanMetaDataImpl stuck for hours) **does
not reproduce on current `dev`**. A faithful, from-scratch reproduction of
the real test method — see "How this was verified" below — completes
cleanly in ~13s under `--nojit`, matching the *exact* repro recipe this doc
originally specified. This was very likely fixed as a side effect of
unrelated work that landed on `dev` between 2026-07-24 and 2026-07-25 (the
2026-07-24 investigation predates a large wave of fixes; see project
history around that window). No specific commit was identified as *the*
fix — this is inferred from the hang no longer occurring, not from reading
a diff.

**However, at the time this section was written, the class still did not
pass under CratonVM's default (JIT-on) configuration**, due to two
unrelated, newly-discovered JIT correctness bugs described below. See the
"Closure" section at the bottom — both are now also fixed.

## How this was verified (no Gradle/Spring Boot checkout needed)

Neither the Windows nor the Azure-host checkout had `apps/spring-boot`
present this session (it's an unversioned, ad-hoc local checkout elsewhere
— see `apps/spring-boot-suite-runner/run-spring-boot-suite.md`'s "Restoring
the checkout" section). Rather than paying the cost of reconstructing the
full ~150-module Gradle build, this investigation used a much cheaper path
that still exercises 100% real production bytecode:

1. Sparse-`git clone --filter=blob:none --sparse` just `core/spring-boot`
   from `github.com/spring-projects/spring-boot` (`v7.0.7`-era, matching
   this module's own Spring Framework dependency).
2. Assembled a classpath from jars already resolved in this shared host's
   `~/.gradle/caches/modules-2` (Spring Framework 7.0.7, Hibernate
   Validator 9.1.2.Final + deps, JUnit Jupiter 6.1.1, AssertJ, qdox,
   commons-logging, snakeyaml, Kotlin stdlib/reflect, log4j2, jakarta.el +
   `org.glassfish.expressly` — the actual EL impl needed for
   `Validation.buildDefaultValidatorFactory()` to succeed instead of
   throwing `HV000183`).
3. `javac`-compiled `core/spring-boot`'s **entire** `src/main/java` tree
   directly (skip Gradle) plus the specific test file
   `ConfigurationPropertiesBeanRegistrationAotProcessorTests.java` +
   its `BScanConfiguration` dependency, with `-parameters` (required for
   `ValueObjectBinder`'s constructor-parameter-name discovery). Two tiny
   stub classes (`SpringBootVersion`, `SpringBootProviderVersion`) were
   needed to replace Gradle-generated version-constant sources; one
   unrelated file (`SpringBootTriggeringPolicy.java`, log4j2 rolling
   policy, irrelevant to this test) was excluded due to an unrelated
   log4j-core API mismatch in the resolved jar version.
4. **Faithfully replicated the `@CompileWithForkedClassLoader` JUnit5
   extension** (`ForkedDriver`/`ExtDriver`, no JUnit launcher needed): the
   real mechanism (`CompileWithForkedClassLoaderClassLoader`, package-
   private in `spring-core-test`) re-defines the test class's own bytecode
   through a fresh `ClassLoader` (reading via `getResourceAsStream` on the
   original loader, `defineClass`-ing into itself) so that `TestCompiler`'s
   `DynamicClassLoader` — whose parent becomes this special loader — can
   reflectively delegate `defineClass` to it (`defineDynamicClass`,
   package-private), giving the freshly-AOT-compiled generated class the
   *same defining-loader identity* as the test's own package-private
   nested classes. Without this, package-private cross-loader access fails
   immediately (`IllegalAccessError`) — a naive "wrap it in some
   `URLClassLoader`" attempt does NOT reproduce this; it must be the real
   `CompileWithForkedClassLoaderClassLoader`, obtained via reflection
   (`Class.forName` + `setAccessible`).
5. Ran `aotContributedInitializerBindsValueObject` (representative of the
   4 `@CompileWithForkedClassLoader` methods — the ones that actually
   `refresh()` a context and are the only ones that could plausibly hang)
   directly via reflection, under real JDK25/HotSpot as a control (passes,
   ~1s) and under a freshly-built `cratonvm` from this worktree.

Reproduction (once the checkout above is staged — paths are host-specific,
kept for reference, not meant to be re-run verbatim without redoing step 1-4):

```bash
CP=$(cat sb-cp.txt):classes-main2:classes-test2:classes-driver
# nojit — the ORIGINAL doc's repro mode: now passes.
TMPDIR=/data/tmp ./cratonvm --java-home <jdk25> --nojit -cp "$CP" \
  ExtDriver aotContributedInitializerBindsValueObject
# => "=== DONE OK in 12701ms ==="

# default (JIT on) — fails, but NOT with the original hang:
TMPDIR=/data/tmp ./cratonvm --java-home <jdk25> -cp "$CP" \
  ExtDriver aotContributedInitializerBindsValueObject
# => CompilationException: "invalid method declaration; return type required"
#    (see Bug 1 below) — fails in ~2s, not a multi-hour hang.
```

## Bug 1 (FIXED 2026-07-26 — see Closure section): AOT-generated `void`-returning methods lose their return type under JIT

The generated `..._TestTarget__BeanFactoryRegistrations.java` source (built
by `BeanRegistrationsAotContribution` via `org.springframework.javapoet`'s
`MethodSpec`, relying on the **default** `TypeName.VOID` — no explicit
`.returns(...)` call) comes out of `TestCompiler`'s in-process `javac` with
the return type token missing entirely:

```java
public  registerBeanDefinitions(DefaultListableBeanFactory beanFactory) {
```
(should be `public void registerBeanDefinitions(...)`) — a genuine javac
parse error (`invalid method declaration; return type required`), not a
warning-as-error.

**Not yet minimally isolated.** A hand-rolled loop of 200,000 iterations
building an equivalent `MethodSpec` via the real
`org.springframework.javapoet` classes from `spring-core-7.0.7.jar`
(no-explicit-return-type, `Object` parameter, `System.out.println`
statement body) does **not** reproduce this under CratonVM JIT — so it
needs Spring's actual `GeneratedMethod`/`BeanRegistrationsAotContribution`
codegen path (not just raw javapoet) to trigger. Confirmed **JIT-only**
(passes under `--nojit`). `CRATONVM_JIT_DENY=isIdentifier` (see Bug 2) does
**not** fix this one — the two bugs are independent, both currently gate
this same test method from passing under JIT.

## Bug 2 (FIXED 2026-07-26 — see Closure section): `javax.lang.model.SourceVersion.isIdentifier` JIT miscompilation

**Fully isolated, dependency-free, 20-line repro** (no javapoet/Spring
needed at all):

```java
import javax.lang.model.SourceVersion;

public class SourceVersionProbe {
    public static void main(String[] args) throws Exception {
        for (int i = 0; i < 20000; i++) {
            boolean r = SourceVersion.isName("Object");   // ALWAYS true
            if (!r) {
                System.out.println("BAD at i=" + i + ": isName(\"Object\")=" + r);
            }
        }
    }
}
```

Under CratonVM with JIT on (default), this starts printing `BAD at i=504`
(varies slightly run-to-run, always in the 500-510 range — matches the
default `CRATONVM_TIER_C1_THRESHOLD=500`) and prints `BAD` on **every**
subsequent iteration. `SourceVersion.isName("Object")` must always return
`true` (`Object` has no dots, is not a keyword) — under real
JDK25/HotSpot it does, for all 20,000 iterations. Under `--nojit` on
CratonVM it also always returns `true`. **JIT-only, 100% deterministic.**

Bisection so far:
- `CRATONVM_JIT_DENY=isIdentifier` (env-gated JIT compile-deny filter,
  substring-matched against `Class.method`) **fixes it completely** — 0
  bad results across thousands of iterations. Denying `isName` or
  `isKeyword` instead does **not** fix it — the miscompiled code is
  specifically `SourceVersion.isIdentifier(CharSequence)`'s own compiled
  body (JIT-dumped: 4145 bytes machine code via
  `CRATONVM_DBG_JIT_DISASM=isIdentifier`, saved to this investigation's
  scratch — not checked in).
- `CRATONVM_JIT_LICM=0`, `CRATONVM_JIT_UNROLL=0`,
  `CRATONVM_JIT_ENABLE_CALLEE_SAVED_GPR_LOCALS=0`,
  `CRATONVM_JIT_KERNEL_REG_LOCALS=0`, `CRATONVM_JIT_KERNEL_REG_OSR=0`,
  `CRATONVM_JIT_INCLUSIVE_BCE=0`, `CRATONVM_JIT_IR_DIRECT_CALL=0`,
  `CRATONVM_JIT_DIRECT_CALLEE_CALLS=0`, `CRATONVM_JIT_STACK_BANG=0`,
  `CRATONVM_JIT_INLINE_SELF_GUARD=0` — none of these individually fix it.
- A **hand-copied, verbatim reimplementation** of `isIdentifier`'s exact
  Java source (same tricky `for (int i = Character.charCount(cp); ...; i
  += Character.charCount(cp)) { cp = id.codePointAt(i); ... }` loop shape,
  where the for-loop's own increment expression reads a variable
  reassigned in the loop body) does **not** reproduce the bug when it's
  MY OWN method, called from my own class — 20,000 iterations clean.
- Calling the **real** `SourceVersion.isIdentifier` **directly** (bypassing
  `isName`/`isKeyword` entirely) from a plain user class, at up to 200,000
  iterations and with `CRATONVM_TIER_C1_THRESHOLD` lowered to `10`, **never
  even triggers JIT compilation of `isIdentifier`** (confirmed via
  `CRATONVM_DBG_JIT_DISASM` showing zero compiles of it) — it stays
  interpreted indefinitely and is therefore always correct. It only
  compiles (and only then misbehaves) when reached via
  `SourceVersion.isName`'s own call — whether `isName` itself is JIT-
  compiled or denied/interpreted makes no difference to whether
  `isIdentifier` ends up compiled+buggy. This points at something in
  CratonVM's tiering/hot-callee heuristics treating calls originating
  from within `javax.lang.model.SourceVersion` (a platform/boot-loaded
  class calling its own sibling method) differently from calls originating
  in user/app-loaded code — plausibly related to the "statically-bound
  call site" / "direct callee calls" optimizations landed recently in
  `jit/src/lib.rs` (commits `dbdc5367f`, `d7bad9194`, `e0e08e4f2` on
  `arch/tiers-1-3`) — but this is a hypothesis, not confirmed; the
  `CRATONVM_JIT_IR_DIRECT_CALL=0`/`CRATONVM_JIT_DIRECT_CALLEE_CALLS=0`
  flags gating those specific optimizations did NOT fix the bug when
  tested, so if they're related it's indirect (e.g. a different call-site
  eligibility heuristic upstream of those flags, not the lowering itself).

  **2026-07-26 closure update: this specific "only compiles via isName,
  never via a direct call" tiering asymmetry no longer holds either** —
  see Closure section. `CRATONVM_DBG_JITC` on current `dev` shows
  `isIdentifier` tiering up through the normal background C1→C2 path like
  any other hot method, regardless of caller.

## What's needed to close this

**Bug 1** needs a minimal repro first (the standalone javapoet loop test
didn't trigger it — needs Spring's actual `GeneratedMethod` codegen wired
up, or a closer hand-reduction of it).

**Bug 2** already has an excellent minimal repro (`SourceVersionProbe`
above, zero dependencies, 100% deterministic at ~iteration 500). The
`CRATONVM_DBG_JIT_DISASM`/`CRATONVM_JIT_DENY` env-gated diagnostics (see
`vm/src/jit/disasm.rs`, `jit/src/lib.rs::jit_deny_filter`) make bisection
fast. Next step is reading the captured disassembly's loop body (back-edge
at compiled-offset `0xd47` → `0x5ab` in the captured dump) against the
bytecode for `codePointAt`/`isJavaIdentifierPart`/the loop increment, or
attaching a debugger to a process paused right after the buggy compile
(the bug is deterministic and fast — no heisenbug risk, unlike the
JIT-perturbation issues documented for SIGSEGV races elsewhere).

Both bugs are JIT-only (never reproduce under `--nojit`), so a `--nojit`
suite run of this class (and likely others exercising Spring AOT codegen
under JIT) is unaffected — only JIT-mode runs of AOT-codegen-heavy tests
are at risk. `CRATONVM_JIT_DENY=isIdentifier` is a viable *workaround* for
Bug 2 specifically (not a fix) if this class needs to pass under JIT before
a real fix lands; no equivalent workaround was found for Bug 1.

**2026-07-26 closure update: this whole section is moot** — see Closure.

## Reproduction of the (RESOLVED) original doc content, for the record

```powershell
$env:JAVA_HOME = 'C:\Program Files\Eclipse Adoptium\jdk-25.0.3.9-hotspot'
powershell.exe -NoProfile -ExecutionPolicy Bypass -File apps\spring-boot-suite-runner\run-spring-boot-suite.ps1 `
  -SpringBootRoot 'C:\craton\CratonVM\apps\spring-boot' `
  -Vm craton -Exe <built cratonvm.exe> -JdkHome $env:JAVA_HOME `
  -ClassList <tsv with just this class> `
  -RunName <name> -Jit off -Parallel 1 -TimeoutSec 300 -CratonArgs @('--stack-dump-on-timeout=60')
```

This no longer hangs (confirmed via the from-scratch reproduction above,
not by re-running this exact PowerShell command — `apps/spring-boot` was
not available on either machine this session). If re-verifying against the
full suite runner, note the runner's default is JIT-**on** — as of this
doc's closure (below), that no longer matters: the class passes JIT-on too.

## Closure (2026-07-26): both remaining bugs turned out to already be fixed on `dev`

Re-investigated in worktree `wt-cpbrap-fix2-20260726`
(branch `fix/cpbrap-jitbugs-20260726`, from `origin/dev` at `3f080ad48`,
Azure host), with the goal of actually fixing Bug 1 and Bug 2 above. The
prior session's full repro environment had survived on the host at
`/data/data/cpbrap-repro/` (sparse Spring Boot checkout, assembled
classpath, `ExtDriver`/`ForkedDriver`, and — critically — the exact
`cratonvm` binary built when this doc was written, preserved at
`/data/data/cpbrap-repro/cratonvm-cpbrap-20260725`), so no re-assembly was
needed.

**Both bugs no longer reproduce on current `dev`.** Building a fresh
`cratonvm` from `origin/dev` tip and re-running:

- `SourceVersionProbe` (Bug 2's exact 20-line repro, 20,000 iterations of
  `SourceVersion.isName("Object")`): **0 bad results**, both at the
  default `CRATONVM_TIER_C1_THRESHOLD=500` and at `=10` with 200,000
  iterations. `CRATONVM_DBG_JITC` confirms `isIdentifier` still tiers up
  through the normal background C1→C2 path (contradicting this doc's
  earlier claim that it only ever compiled via the eager direct-call-as-
  callee path — that claim no longer holds on current `dev` either,
  suggesting the underlying tiering behavior shifted along with the fix).
- All 5 `@CompileWithForkedClassLoader` test methods
  (`aotContributedInitializerBindsValueObject`,
  `...WithSpecificConstructor`, `...BindsJavaBean`,
  `...BindsScannedValueObject`, `...BindsScannedJavaBean` — one more than
  this doc originally counted) plus the 4 plain `@Test` methods on the
  class: **all 9 pass**, default JIT-on, real Spring Boot 7.0.7 bytecode,
  multiple repeated runs for determinism. No `CompilationException`, no
  hang.

Re-running the **same** repros against the preserved 2026-07-25 binary
(`cratonvm-cpbrap-20260725`) confirms both bugs are genuinely present
there (Bug 2: `BAD` starting at iteration ~505-512, ~97% of iterations bad
thereafter; Bug 1: the exact `CompilationException` /
"invalid method declaration; return type required" from the doc) — so
this is a real fix on `dev`, not an environment or repro difference.

**Root cause, identified via bisection:** both bugs were fixed by the
*same* commit, `13055f75c` ("fix(jit): String compact-layout field offsets
and the branch-join reload mirror"), landed on `dev` 2026-07-26 13:46 UTC
— found independently, investigating an unrelated H2/`org/hibernate`
JIT-ban lift (see that commit's own log for the H2 angle). Built the
worktree at `13055f75c~1` (its immediate parent) in a separate throwaway
worktree (`wt-cpbrap-bisect-20260726`) and reran both repros: **both bugs
reproduce at `13055f75c~1`** (Bug 2: `BAD` from iteration ~505; Bug 1: same
`CompilationException`), confirming `13055f75c` is the exact fixing
commit for both, not just "some fix somewhere in the range."

The commit's second fix, **BUG-JOIN-MIRROR**, is the relevant half: the
x64 backend's reload-elision mirror (tracking which register already
holds a given stack/local value, to skip redundant reloads) was cleared
at the top of a branch-TARGET pc, but the merge-point
`canonicalize_stack()` runs *after* that clear and emits fall-through-only
code before `pc_to_native[pc]` — so a mirror entry recorded during that
fall-through window leaked across the join, and code reached via the
branch edge read a stale register. The commit's own example was a ternary
(`s == null ? defaultValue : s`) returning the wrong arm's stale value.
This is a generic branch-merge-point bug, not specific to H2, Spring, or
either bug documented here — it explains both:

- **Bug 2**: `isIdentifier`'s loop (`for (int i = ...; i < id.length(); i
  += ...) { cp = id.codePointAt(i); if (!Character.isJavaIdentifierPart(cp))
  return false; }`) has exactly the shape BUG-JOIN-MIRROR corrupts — a
  loop back-edge merging with the loop-entry path, with an early-return
  branch inside. A stale register value read at the merge point plausibly
  explains the always-wrong-after-first-hit behavior once the method was
  JIT-compiled.
- **Bug 1**: the generated source's missing `void ` token is exactly the
  kind of corruption a stale/wrong register produces when it feeds into
  the `StringBuilder`/string-concat machinery inside javapoet's
  `MethodSpec.emit()` (a conditional — "if there's an explicit return
  type, emit it" — around a ternary-shaped branch join), which is why the
  doc's own earlier attempt to isolate Bug 1 with a hand-rolled 200k-loop
  javapoet-only probe never reproduced it: that probe likely never hit the
  same branch-join shape the real `BeanRegistrationsAotContribution`
  codegen path does.

Neither of these attributions was re-verified by reading `13055f75c`'s
diff line-by-line against a hand-reduced minimal repro of either bug
specifically (the bisection + repro re-run above is what's actually
verified) — flagged here as inference, matching this doc's own established
convention from the original hang's closure.

**Disposition:** doc moved to `docs/internal/fixed-suite-bugs/` (top status
is now fully closed, no remaining OPEN sub-part). No code changes were
needed in this session — the fix already existed on `dev`; this session's
contribution is the verification, bisection, and root-cause attribution
above.
