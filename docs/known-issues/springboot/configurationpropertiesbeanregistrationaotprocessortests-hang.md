# `ConfigurationPropertiesBeanRegistrationAotProcessorTests` — original hang RESOLVED, class still blocked by 2 unrelated JIT bugs (OPEN)

Class: `org.springframework.boot.context.properties.ConfigurationPropertiesBeanRegistrationAotProcessorTests`
(`core/spring-boot`). Originally found investigating Spring Boot core39
residual Cluster A (`spring-boot-core39-residual-clusters-20260723.md`),
2026-07-24. Re-investigated 2026-07-25/26 (worktree
`wt-cpbrap-hang-20260725`, branch `fix/cpbrap-hang-20260725`, Azure host).

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

**However, the class still does not pass under CratonVM's default (JIT-on)
configuration**, due to two unrelated, newly-discovered JIT correctness
bugs described below. Since the suite runner's default mode is JIT-on, this
class remains a real failure — just a different one than originally
documented. Left in `known-issues/` per the doc convention (only moves to
`docs/internal/` once the class actually passes).

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

## Bug 1 (OPEN): AOT-generated `void`-returning methods lose their return type under JIT

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

## Bug 2 (OPEN, minimally isolated): `javax.lang.model.SourceVersion.isIdentifier` JIT miscompilation

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
full suite runner, note the runner's default is JIT-**on**, which will now
hit Bug 1/Bug 2 above instead of completing — pass `-Jit off` to see the
original hang's absence directly, matching this doc's original repro.
