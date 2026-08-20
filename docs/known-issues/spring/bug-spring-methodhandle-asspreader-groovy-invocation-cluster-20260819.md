# `MethodHandle.asSpreader(Object[].class, n)` broken under CratonVM — explains most of the Spring Groovy-integration FAILs

## Status
**OPEN, confirmed CratonVM-specific, root-caused with a minimal repro
independent of Groovy** — found 2026-08-19 running the Spring Framework
suite under CratonVM on Azure. Explains at least 11 of the 88 common FAILs,
with several more plausibly related but not confirmed (see below).
Differential-verified against real HotSpot JDK 25: passes cleanly.

## Minimal repro (no Groovy involved at all)
```java
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;

public class MHSpreaderProbe {
    public static int add(int a, int b) { return a + b; }
    public static void main(String[] args) throws Throwable {
        MethodHandle mh = MethodHandles.lookup().findStatic(MHSpreaderProbe.class, "add",
                MethodType.methodType(int.class, int.class, int.class));
        MethodHandle spread = mh
                .asSpreader(Object[].class, 2)
                .asType(MethodType.methodType(Object.class, Object[].class));
        Object result = spread.invoke(new Object[]{3, 4});
        System.out.println("result: " + result);
    }
}
```
**HotSpot**: `spreader type: (Object[])Object`, `result: 7`.

**CratonVM**: fails at the `.asSpreader(Object[].class, 2)` call itself —
before the handle is ever invoked:
```
original type: (int,int)int
Exception in thread "main" java/lang/invoke/WrongMethodTypeException: cannot convert MethodHandle(int,int)int to (Object[])Object
	at MHSpreaderProbe.main(MHSpreaderProbe.java:19)
	at java/lang/invoke/WrongMethodTypeException.<init>(WrongMethodTypeException.java:61)
```
`asSpreader` is supposed to *produce* a new handle of type `(Object[])int`
(later adapted to `(Object[])Object` via `asType`) that spreads an incoming
array across the original handle's fixed parameters when invoked — it should
not itself throw. CratonVM's `MethodHandle.asSpreader` (or the adapter chain
it builds) rejects the conversion immediately.

## Why this hits Groovy specifically
Groovy's `InvokerHelper`/`CallSite` machinery uses exactly this
spreader-adapter pattern to invoke arbitrary target methods generically
through a single `Object[] args` calling convention — this is how Groovy
implements dynamic method dispatch and closure invocation efficiently via
`invokedynamic` bootstrap methods (rather than reflection). Any Groovy script
execution that goes through this path fails immediately with the same
`WrongMethodTypeException` shape (or a wrapping exception from Spring's own
Groovy-integration code):

* `GroovyScriptEvaluatorTests` — direct `WrongMethodTypeException`s matching
  the probe exactly: `cannot convert MethodHandle(Object,Object)int to
  (Object[])Object`, `cannot convert MethodHandle(int,int)int to
  (Object[])Object`.
* 4 classes fail with `org.springframework.beans.factory.parsing.BeanDefinitionParsingException:
  Configuration problem: Error evaluating Groovy script: cannot convert
  MethodHandle(beans,Closure)Object to (Object[])Object` /
  `not primitive: org.springframework.context.groovy.beans` —
  `GroovyApplicationContextDynamicBeanPropertyTests`,
  `GroovyApplicationContextTests`, `GroovyBeanDefinitionReaderTests`,
  `GroovyControlGroupTests` — all fail evaluating the Groovy Spring-beans DSL,
  which is itself a closure invoked through the same MethodHandle mechanism.
* 6 classes fail with `IllegalStateException: Failed to load ApplicationContext`
  wrapping the same underlying Groovy-script evaluation failure (confirmed by
  inspecting the exception chain) — `AbsolutePathGroovySpringContextTests`,
  `DefaultScriptDetectionGroovySpringContextTests`, `GroovySpringContextTests`,
  `MixedXmlAndGroovySpringContextTests`, `RelativePathGroovySpringContextTests`,
  `BasicGroovyWacTests` — every one of these loads its `ApplicationContext`
  from a `.groovy` config script, which fails to evaluate for the same reason.

## Plausibly related, NOT confirmed
These Groovy-named classes fail in this same sweep but were not traced back
to a `MethodHandle`/`asSpreader` stack frame in this pass — flagged here
rather than silently left out, since the class names make them likely
candidates for the same root cause once someone has time to check:
* `GroovyAspectTests` / `GroovyAspectIntegrationTests` (bare `AssertionError`
  / `AssertionFailedError`, no message captured in the pooled failcause line
  — needs the full stack trace, not just the one-line summary used for this
  triage pass).
* `GroovyScriptFactoryTests` (`BeanCreationException: ... BeanPostProcessor
  before instantiation of bean failed` — plausible if the bean is itself a
  Groovy-scripted object being instantiated via the same mechanism, not
  confirmed).
* `JRubyScriptTemplateTests` (both the `web.servlet.view.script` and
  `web.reactive.result.view.script` variants) — `IllegalStateException:
  Failed to evaluate script [.../render.rb]`. JRuby is a **different**
  scripting engine from Groovy with its own invocation machinery; it may
  independently rely on `MethodHandle.asSpreader` (plausible, JRuby also uses
  invokedynamic-based dispatch heavily) or may be failing for an unrelated
  reason. Not distinguished in this pass.

**Not related** (checked and ruled out of this cluster): `GroovyMarkupViewTests`
and `ViewResolutionIntegrationTests`'s `groovyMarkup()` case both fail with
`org.codehaus.groovy.control.MultipleCompilationErrorsException: startup
failed` — a Groovy **template source failing to compile**, a different
failure stage entirely (compile-time, not the runtime MethodHandle-adapter
failure this doc is about). Filed separately — see
`bug-spring-remaining-fail-clusters-20260819.md`.

## Next steps
* Find `MethodHandle.asSpreader` in CratonVM's `java.lang.invoke`
  implementation (native or bytecode) and compare its type-compatibility
  check against the JDK spec: `asSpreader(Object[].class, n)` should succeed
  for any handle whose *last n* parameter types are reference-assignable
  from `Object` (after boxing for primitives) — the probe's `(int,int)int`
  handle is exactly the documented common case (spreading into two `int`
  parameters via auto-unboxing) and must succeed.
* Once fixed, re-run `GroovyScriptEvaluatorTests` and the two Groovy
  ApplicationContext-loading clusters above to confirm, then check whether
  the "plausibly related" classes are fixed too — that will settle whether
  they share this root cause without needing separate investigation.

## Repro
```bash
source /data/toolchain/env.sh
javac -d probeclasses MHSpreaderProbe.java
<cratonvm-bin> --java-home $JAVA_HOME -c probeclasses MHSpreaderProbe
# compare against: $JAVA_HOME/bin/java -cp probeclasses MHSpreaderProbe
```
