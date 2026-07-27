# Mockito silently selects its **fallback** `Location` and `MemberAccessor` implementations on CratonVM — OPEN

**Status: OPEN — found 2026-07-27.** Not a correctness failure (every affected
test passes), but a confirmed, reproducible behavioural + performance
divergence from real HotSpot that Mockito swallows without any diagnostic.

Found while auditing the "residual Application-loader copies" noted in
[`../../internal/fixed-suite-bugs/springboot/servletcontextlistener-forkedclasspath-mockito-notamock-FIXED.md`](../../internal/fixed-suite-bugs/springboot/servletcontextlistener-forkedclasspath-mockito-notamock-FIXED.md).
That audit showed most of those copies were **not** defects (see that doc's
corrected "Residual observations"); these two are the real divergence, and they
have nothing to do with classloader identity.

## Evidence

Same class, same harness, both VMs run to completion:
`org.springframework.boot.tomcat.servlet.TomcatServletWebServerServletContextListenerTests`.

HotSpot side captured with
`-Xlog:class+load=info:file=…` (`C:\craton\forkrepro-20260726\hs-classload.log`),
CratonVM side from a `CRATONVM_DBG=dupclass` run.

| Class | HotSpot | CratonVM |
|---|---:|---:|
| `LocationFactory$DefaultLocationFactory` | loaded | — |
| `debugging.LocationImpl` | loaded | — |
| `debugging.Java8LocationImpl` | **never loaded** | **loaded** |
| `reflection.InstrumentationMemberAccessor` | loaded | — |
| `reflection.ReflectionMemberAccessor` | **never loaded** | **loaded** |

So on CratonVM Mockito takes the *fallback* branch in both selectors.

## Why this matters

* **`Java8LocationImpl` vs `LocationImpl`** — `LocationImpl` uses
  `StackWalker`; `Java8LocationImpl` constructs a `new Throwable()` and walks
  its stack trace instead. Mockito builds a `Location` for **every** recorded
  invocation, so this is a hot path — this is a plausible contributor to the
  Mockito-heavy slowness seen elsewhere in the suite.
* **`ReflectionMemberAccessor` vs `InstrumentationMemberAccessor`** — the
  reflective accessor relies on `setAccessible`; the instrumentation one uses a
  ByteBuddy-generated dispatcher backed by real `Instrumentation`. The
  reflective fallback fails on strongly-encapsulated (JPMS) members that the
  instrumentation path handles, so this can surface later as
  `InaccessibleObjectException` where HotSpot succeeds.

Both selections are wrapped in swallow-everything handlers
(`catch (ClassNotFoundException)` / `catch (Throwable)`), so nothing is
logged — the divergence is completely silent.

## Selector logic (mockito-core 5.23.0, from `javap`)

```
LocationFactory.createLocationFactory():
   0: Platform.isAndroid()                 // false on both VMs (verified)
   3: ifeq 20
   6: AndroidPlatform.isStackWalkerUsable()
   9: ifne 20
  12: new LocationFactory$Java8LocationFactory   // -> Java8LocationImpl
  20: ldc "java.lang.StackWalker"; Class.forName; pop
  26: new LocationFactory$DefaultLocationFactory // -> LocationImpl
  Exception table: 20..33 -> 34  type java/lang/ClassNotFoundException
  34: new LocationFactory$Java8LocationFactory

ModuleMemberAccessor.<init>():
   4: delegate()                           // try
      delegate(): ClassFileVersion.ofThisVm().isAtLeast(JAVA_V9)
                    ? new InstrumentationMemberAccessor()
                    : new ReflectionMemberAccessor()
  Exception table: 4..8 -> 11  type java/lang/Throwable
  12: new ReflectionMemberAccessor
```

## What has been ruled out

All measured on the standalone fork-loader repro
(`C:\craton\forkrepro-20260726`, `ForkRepro`/`ForkReproInner`, which mirrors
`ModifiedClassPathClassLoader` exactly — platform parent, full URL list, TCCL
set), CratonVM vs HotSpot:

* **Not `isAndroid()`** — `Platform.isAndroid()` returns `false` on both
  (`java.vendor` = `CratonVM`, no "android" substring).
* **Not a `Class.forName` CNFE** — `Class.forName("java.lang.StackWalker")`
  succeeds on CratonVM from the fork loader, *and* from inside a `<clinit>`
  (tested with a dedicated nested-class probe). A temporary VM-side wrapper
  around `native_class_for_name` logging **every** CNFE it returns recorded
  **zero** CNFEs for the whole run.
* **Not an `athrow`** — `CRATONVM_DBG=athrow` shows only two throws in the
  entire run, both the expected `ByteBuddyAgent`/`Installer.getInstrumentation`
  probe that also happens on HotSpot.
* **Not a linkage error** — `CRATONVM_DBG=ncdfe` / `CRATONVM_NSEE_TRACE=1`
  report nothing.
* **Not the JIT** — reproduces identically under `--nojit`.
* **Not the capability itself** — `ClassFileVersion.ofThisVm()` returns
  `Java 25 (69)` and `isAtLeast(JAVA_V9)` returns `true` on CratonVM, and
  `new InstrumentationMemberAccessor()` constructed **directly** succeeds.
  Yet `new ModuleMemberAccessor()` still ends up with the reflective delegate.
* **Not the loader-identity bug fixed in the FIXED doc above** — this
  reproduces on the post-fix binary.

That last point is the puzzle: every individual step of both selectors behaves
correctly when probed in isolation, no exception is observed, and yet both
composites take the fallback branch. Either the probes perturb the state they
measure (they necessarily run after Mockito has initialised), or a boolean
branch is being evaluated differently in that context.

## Suggested next step

Instrument the branch itself rather than its inputs: trace `ifeq`/`ifne`
outcomes (or the `Z` return coercion) for
`LocationFactory.createLocationFactory` and `ModuleMemberAccessor.delegate`
specifically, and log the value actually on the operand stack. If the observed
value disagrees with what the callee returns, this is a dispatch/return-value
defect, and the same shape could silently affect any `boolean`-returning
call consumed by a conditional — a much wider blast radius than Mockito.

Reproduce cheaply (no suite needed, ~5s):

```bash
powershell -NoProfile -ExecutionPolicy Bypass -File C:\craton\forkrepro-20260726\run.ps1 -Vm craton -Exe <exe> -Tag t
```

and compare the `LocationFactory.create() ->` and `ModuleMemberAccessor
delegate =` lines against `-Vm hotspot`.
