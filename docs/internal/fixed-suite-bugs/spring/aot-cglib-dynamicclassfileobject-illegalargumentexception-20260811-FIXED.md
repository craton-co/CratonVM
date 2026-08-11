# Spring AOT + CGLIB proxy generation — `IllegalArgumentException: DynamicClassFileObject` — **FIXED**

**Status:** FIXED (2026-08-11). Root cause is `441615852`
(`fix(jit): a MIC entry compiled BY NAME can be another loader's copy of the
method`), which was already on `dev` when this page was written — the page's
binary, `cratonvm-spring-default-postmerge2` (commit `4c4fb3902`), predates it.
Re-measured this session on both sides; see **Measurement** below.

**It is not a type-check bug, despite the exception naming a class.** The
`IllegalArgumentException` whose message is a class name is `javac`'s, thrown
verbatim from `JavacFileManager.inferBinaryName`, and it is the *last* frame in
a chain that starts in the JIT.

## What the exception actually was

The page's own recommended next step — get a real stack trace — needed no
harness change: `apps/spring-suite-runner/KRun.java` already prints the full
chain when `KRUN_STACK` is set in the environment. With
`KRUN_STACK=1` against the page's own binary, all five failures produce the
identical `Caused by`:

```
Caused by: java.lang.IllegalArgumentException: org.springframework.core.test.tools.DynamicClassFileObject
    at com.sun.tools.javac.file.JavacFileManager.inferBinaryName(JavacFileManager.java:809)
    at javax.tools.ForwardingJavaFileManager.inferBinaryName(ForwardingJavaFileManager.java)
    at com.sun.tools.javac.api.ClientCodeWrapper$WrappedJavaFileManager.inferBinaryName(ClientCodeWrapper.java)
    at com.sun.tools.javac.code.ClassFinder.includeClassFile(ClassFinder.java)
    at com.sun.tools.javac.code.ClassFinder.fillIn(ClassFinder.java:734)
    ...
```

Read the middle two frames together with the Spring source. `javac`'s
`ClientCodeWrapper$WrappedJavaFileManager.inferBinaryName` does exactly one
thing:

```java
return clientJavaFileManager.inferBinaryName(location, unwrap(file));   // invokeinterface
```

and the client file manager here is Spring's `DynamicJavaFileManager`, whose
whole reason to exist at this call is its override:

```java
@Override
public String inferBinaryName(Location location, JavaFileObject file) {
    if (file instanceof DynamicClassFileObject dynamicClassFileObject) {
        return dynamicClassFileObject.getClassName();
    }
    return super.inferBinaryName(location, file);   // <- javac's, which throws
}
```

`JavacFileManager.inferBinaryName` ends in
`throw new IllegalArgumentException(file.getClass().getName())` for any file
object it did not create — so the message *is* the class name, and reads like a
failed type check. The real defect is upstream: the `instanceof` was evaluated
against a **different loader's copy** of `DynamicClassFileObject` than the one
the receiver's file object belonged to, so it correctly answered `false` and
`super` ran.

## Root cause: the JIT compiled the callee by class NAME

`441615852`'s own message states it, measured on this very test class:

> `try_jit_compile_callee` resolves the callee by class NAME. When the
> receiver's class is not the class that name globally resolves to, that hands
> back another loader's copy, and the entry is then cached against THIS
> receiver's class id — so every monomorphic hit machine-CALLs a body compiled
> for a different copy. […] one run resolved that name to eight-plus distinct
> class ids (2690, 8510, 10377, 14030, 15877, 17724, 19569, 21414, …). The copy
> that got compiled carries its OWN `DynamicClassFileObject` id at its
> `instanceof` site […]

`globally_named` already gated `publish_mic_rust_cached_entry`; the by-name
compile feeding the machine-code MIC/PIC was left ungated. The fix: not
globally named → do not compile by name, leave the site on the dispatch helper,
which resolves on the real receiver.

### Why exactly the five Cglib methods, and no others

Not CGLIB — the **forked class loader**. The five failures are all in the
`@CompileWithForkedClassLoader`-annotated nested class
`ApplicationContextAotGeneratorTests$ConfigurationClassCglibProxy`, whose
`CompileWithForkedClassLoaderClassLoader` redefines every application class,
including all of `org.springframework.core.test.tools`, in a fresh loader whose
parent is the platform loader. Each such test therefore creates one more copy
of `DynamicJavaFileManager` / `DynamicClassFileObject`, while the other 35
methods keep using the app loader's. Confirmed by running the two arms
separately on the pre-fix binary's successor:

| arm | result |
| --- | --- |
| `…$ConfigurationClassCglibProxy` **alone** | `found=9 succ=9 fail=0` — no app-world copy exists to be compiled by name |
| whole `ApplicationContextAotGeneratorTests` | reproduces (pre-fix) |

A standalone Java probe (a `ForwardingJavaFileManager` subclass driven through
`javac` in an app world and then in a fork loader, `/tmp/goalprobe`) did **not**
reproduce it even at 19 060 warm calls, so the reproduction still needs the real
test's loader churn — that is why this page's evidence is the suite run and the
commit's own measurement rather than a minimal probe.

## Measurement

Same harness, same classpath, same `KRun` driver, JIT on, `--jdk real`.

| binary | commit | collector | result |
| --- | --- | --- | --- |
| `cratonvm-spring-default-postmerge2` (this page's) | `4c4fb3902` | default (ZGC) | `found=40 succ=33 fail=7` — 5 × the `IllegalArgumentException` |
| `cratonvm-goal-base` | `59e3b0039` | default (ZGC) | `found=40 succ=40 fail=0` |
| `cratonvm-goal-base` (repeat) | `59e3b0039` | default (ZGC) | `found=40 succ=40 fail=0` |
| `cratonvm-goal-base` | `59e3b0039` | `-XX:+UseG1GC` | `found=40 succ=40 fail=0` |
| `cratonvm-goal-base` | `59e3b0039` | `-XX:+UseGenerationalGC` | `found=40 succ=40 fail=0` |
| `cratonvm-goal-merged` | `177d9a355` (branch + `origin/dev` merge) | default (ZGC) | `found=40 succ=40 fail=0` |
| `cratonvm-goal-merged` | `177d9a355` | `-XX:+UseG1GC` | `found=40 succ=40 fail=0` |
| HotSpot (real JDK 25) | — | — | `found=40 succ=40 fail=0` |

The red arm was re-taken *this session*, on the same host, before the green
arms — so "green" here is a state change, not an environment that never showed
the failure. The page's open question about the other two GC variants is
answered by the last two rows: green on all three.

The page's note that "the other ~5-7 residual failures per variant … [were] not
checked directly" is unchanged by this: those are other classes and are not
covered here.

## Related

- `441615852` — the fix.
- `gc-variant-fullsuite-classpath-gap-and-fails-20260810-FIXED.md` (this
  folder) — the classpath-completeness bug this failure was hiding behind; a
  different, already-closed issue.
- `log4j-config-dropped-app-world-aliased-to-a-forked-one-20260810-FIXED.md`
  (this folder) — the same "one library, two loader worlds" family, different
  mechanism.
