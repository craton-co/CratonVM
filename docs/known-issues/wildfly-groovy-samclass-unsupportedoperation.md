# Groovy CachedSAMClass.getSAMMethodImpl() throws UnsupportedOperationException under CratonVM reflection

Status: OPEN — new, found 2026-07-08 during round-4 rerun of the full WildFly suite (post SIGSEGV-fix)
Severity: Low-Medium (breaks any Groovy script that instantiates a class Groovy needs to SAM-check via
reflection; narrow but affects the `vdx` module's XML-transformation test harness broadly)
First confirmed: 2026-07-08, Azure worktree `test/wildfly-full-suite-20260707`, dev@6881f7e9 (round-4 binary)

## Symptom

Any WildFly `testsuite/integration/vdx` test that applies a Groovy-scripted XML transformation
(`org.wildfly.extras.creaper.commands.foundation.offline.xml.GroovyXmlTransform.apply()`, used by
`ServerBase.applyXmlTransformation()` to mutate a `standalone.xml`/`host.xml` before boot) fails
instantiating the Groovy script object:

```text
java.lang.ExceptionInInitializerError
	at org.codehaus.groovy.runtime.InvokerHelper.<clinit>(InvokerHelper.java:66)
	at groovy.lang.GroovyObjectSupport.<init>(GroovyObjectSupport.java:34)
	at groovy.lang.Binding.<init>(Binding.java:35)
	at groovy.lang.Script.<init>(Script.java:39)
	at <SomeGeneratedGroovyScriptClass>.<init>(<SomeGeneratedGroovyScriptClass>.groovy)
	at org.wildfly.extras.creaper.commands.foundation.offline.xml.GroovyXmlTransform.apply(GroovyXmlTransform.java:107)
	...
Caused by: java.lang.UnsupportedOperationException
	at org.codehaus.groovy.reflection.stdclasses.CachedSAMClass.getSAMMethodImpl(CachedSAMClass.java:199)
	at org.codehaus.groovy.reflection.stdclasses.CachedSAMClass.getSAMMethod(CachedSAMClass.java:161)
	at org.codehaus.groovy.reflection.ClassInfo.isSAM(ClassInfo.java:376)
	at org.codehaus.groovy.reflection.ClassInfo.createCachedClass(ClassInfo.java:366)
	at org.codehaus.groovy.reflection.ClassInfo$LazyCachedClassRef.initValue(ClassInfo.java:423)
	...
Caused by: java.lang.UnsupportedOperationException: InstantiationException: no no-arg constructor in <ScriptClassName>
	at org.wildfly.extras.creaper.commands.foundation.offline.xml.GroovyXmlTransform.apply(GroovyXmlTransform.java:107)
```

Groovy's `ClassInfo` machinery lazily determines whether a class is a "SAM type" (single-abstract-method,
i.e. lambda-compatible functional interface) the first time any `ClassInfo` is created for it — this runs
for essentially every class Groovy touches, including its own generated `Script` subclasses, so it fires
during ordinary Groovy script instantiation, not anything exotic. `CachedSAMClass.getSAMMethodImpl()`
throws `UnsupportedOperationException` while inspecting the class via reflection, which Groovy's own
`GroovyXmlTransform.apply()` re-wraps into a misleading "no no-arg constructor" message (a red herring —
the constructor exists; the SAM-detection reflection call itself is what's failing).

## Confirmed CratonVM-specific via HotSpot A/B

`org.wildfly.test.integration.vdx.standalone.NoSchemaTestCase` (module `testsuite/integration/vdx`):

- **CratonVM** (jit-real mode): `ExceptionInInitializerError` as above — the Groovy XML transformation
  never completes, so the server is never even started with the transformed config.
- **Real HotSpot** (same class, same harness invocation): the Groovy transformation succeeds — the test
  gets past this step entirely and fails later for an unrelated, pre-existing environmental reason
  (`AssertionError: log doesn't contain 'WFLYCTL0097'`, likely a WildFly-version-specific expected message
  mismatch, not a crash).

Same Maven/Surefire invocation, same class — only the JVM differs, and only CratonVM ever reaches the
Groovy reflection code path that throws.

## Root cause — not yet pinpointed

Not root-caused to a specific CratonVM native override in this session (time-boxed to confirming the A/B
and getting the exact failure signature). The failing call is Groovy's own `CachedSAMClass.getSAMMethodImpl`
inspecting a class's methods via standard `java.lang.reflect` (likely `Class.getMethods()` /
`Method.getModifiers()` / `Method.isDefault()` / similar) to decide if it has exactly one abstract method.
`UnsupportedOperationException` as the *type* thrown (not `NullPointerException` or a CratonVM-specific
error) suggests either:
- A CratonVM reflection API deliberately throwing `UnsupportedOperationException` for a method/query Groovy
  relies on (worth grepping `native-builtins/src` for `UnsupportedOperationException` near reflection code —
  `classfile_api.rs:28` is one existing throw site, unclear yet if related), or
- A collection CratonVM's reflection layer returns as immutable (e.g. `Class.getMethods()`'s array/list)
  where Groovy's code path expects to mutate it.

Whoever picks this up should build a minimal repro isolating exactly which reflective call inside
`CachedSAMClass.getSAMMethodImpl` throws (Groovy's own source is on Maven Central,
`org.codehaus.groovy:groovy:4.x` — `getSAMMethodImpl` is short and worth reading directly rather than
guessing), then trace that specific CratonVM native override.

## Scale

Only 3 classes observed with this exact signature in this run (`NoSchemaTestCase`, `ElytronTestCase`, and
one other `vdx.standalone`/`vdx.domain` class) — low count, but every `vdx` test that uses Groovy-scripted
XML transformation (a substantial fraction of that module, which exists specifically to test WildFly's
"did you get a helpful validation error" behavior for broken configs) is blocked by this, separate from
[[wildfly-logging-subsystem-requires-real-logmanager]] (most `vdx` tests without a Groovy transformation
step are blocked by that instead — this is a second, independent `vdx`-module blocker).

## Repro

```bash
cd apps/wildfly-suite-runner   # own copy pointed at WILDFLY=<built wildfly checkout>
export WILDFLY=/data/data/cratonvm/apps/wildfly
export CRATONVM_BIN=<any cratonvm release binary>
export JDK25_WIN=<real JDK 25 home>
./run-suite-linux.sh run --category all --jit on --jdk real --class-to 300 \
  --only 'NoSchemaTestCase' --tag repro
# -> FAIL, ExceptionInInitializerError / UnsupportedOperationException in CachedSAMClass.getSAMMethodImpl

./run-suite-linux.sh hotspot --category all --only 'NoSchemaTestCase' --tag repro-hotspot
# -> FAIL for an unrelated reason (WFLYCTL0097 message mismatch), but the Groovy step itself succeeds
```

## Evidence

```text
/data/data/wt-wildfly-bugbash-20260707-runner/out/rerun4-s1of2-jit-real-all-20260708-143042/surefire-reports/00695-org.wildfly.test.integration.vdx.standalone.NoSchemaTestCase/
/data/data/wt-wildfly-bugbash-20260707-runner/out/hscheck-vdx-hotspot-all-20260708-201951/  (HotSpot A/B baseline)
```

## Related

Not the same as the already-fixed Groovy/invokedynocard uncommon-trap regression (commits `752796a0` /
`4ee4eb9a` / `4224c705` and siblings, already in this session's binary) — that was about JIT deopt-path
side-effect double-execution during Groovy dynamic dispatch; this is a plain reflection-API gap hit during
Groovy's `ClassInfo`/SAM-detection bookkeeping, a different code path entirely.
