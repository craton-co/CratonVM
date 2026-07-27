# Mockito silently selects its **fallback** `Location` and `MemberAccessor` implementations on CratonVM — FIXED

**Status: FIXED 2026-07-27** (found 2026-07-27, same day).
Branch `fix/mockito-fallback-selectors-20260727`, merged to `dev`.

Supersedes the OPEN known-issue doc that used to live at
`docs/known-issues/springboot/mockito-silently-selects-fallback-location-and-memberaccessor.md`.

## What was wrong

On CratonVM, Mockito took the *fallback* branch of both of its internal
selectors on every run — `Java8LocationImpl` instead of `LocationImpl`, and
`ReflectionMemberAccessor` instead of `InstrumentationMemberAccessor` — with
nothing logged, because both selectors are wrapped in swallow-everything
handlers.

## Root cause — NOT what the original doc hypothesised

The original doc's "Suggested next step" proposed instrumenting `ifeq`/`ifne`
outcomes, on the theory that "a boolean branch is being evaluated differently
in that context", with a "much wider blast radius than Mockito". **That
hypothesis was wrong.** No branch, dispatch or return-value defect is
involved, which is also why every individual probe of the selectors' *inputs*
came back correct — the composite never ran at all.

CratonVM ships two deliberate native overrides that replace the selector
methods wholesale:

| Override | Effect |
|---|---|
| `native_mockito_location_factory_create` — registered for `LocationFactory.create()`, `LocationFactory.create(Z)` and `LocationFactory$DefaultLocationFactory.create(Z)` | allocates a `Java8LocationImpl` and stuffs it with the **hardcoded** strings `"-> at <<unknown line>>"` and `"<unknown source file>"`. It does not walk the stack at all. |
| `native_mockito_module_member_accessor_delegate` — registered for `ModuleMemberAccessor.delegate()` | unconditionally returns a fresh `ReflectionMemberAccessor` |

Both live in `native-builtins/src/test_frameworks.rs`
(`register_mockito_debugging_intrinsics`). Three dispatch gates route to them:

* `vm/src/runtime/interpreter.rs::is_mockito_debugging_native_override` — `LocationFactory`
* `vm/src/runtime/interpreter.rs::force_native_over_real_jdk_bytecode` — `ModuleMemberAccessor`
* `vm/src/vm/vm_exec.rs`'s `check_override` — `ModuleMemberAccessor` again
  (the second half of the dual gate; cf. the general "dual dispatch-gate
  native override" pattern — both lists must agree)

The `ModuleMemberAccessor` override's own comment explains its original
motive: `InstrumentationMemberAccessor` "eagerly bootstraps Byte Buddy just to
choose its instrumentation path", which was once unsupported. That is no
longer true — see the verification below.

### The fork classloader was a red herring

The original doc framed this as a `@ForkedClassPath` /
`ModifiedClassPathClassLoader` phenomenon and built a whole fork-loader repro
around it. It reproduces just as well in a **plain** run with no fork loader
at all:

```bash
cratonvm --java-home <jdk25> -cp <spring-boot-tomcat test cp> Plain
```

(`Plain.java` in the repro dir below: mock a `Runnable`, fail a `verify`, then
ask `LocationFactory`/`ModuleMemberAccessor` what they picked.) That collapses
the repro from a ~5s fork-loader harness to a ~2s one-class run, and rules the
loader out as a factor.

## Why it mattered

`LocationFactory` is the one with real, user-visible cost. Mockito builds a
`Location` for **every recorded invocation** and prints it in every failure
message. On CratonVM every such message named nothing:

```
HotSpot                              CratonVM (before)
runnable.run();                      runnable.run();
Wanted 2 times:                      Wanted 2 times:
-> at Plain.main(Plain.java:53)      -> at <<unknown line>>
But was 1 time:                      But was 1 time:
-> at Plain.main(Plain.java:51)      -> at <<unknown line>>
```

Every Mockito verification failure across the whole Spring Boot suite lost its
call site — which also degrades any triage that reads those messages.

`ModuleMemberAccessor` is, honestly, the lesser half: **it had no functional
cost on CratonVM today.** The reflective accessor is supposed to fail on
strongly-encapsulated (JPMS) members that the instrumentation accessor
handles, but CratonVM does not enforce strong encapsulation, so both accessors
succeed on exactly the same inputs here — including a
`java.lang.ProcessEnvironment.theEnvironment` read that requires
`--add-opens` on real HotSpot (see `AccessorProbe` below). Fixing it is a
fidelity/class-identity correction, not a behaviour repair.

## Residual found by the fix — `StackWalker$StackFrame.toString()`

Removing the `Location` override exposed a **second, previously-masked bug**
with a broader blast radius than Mockito.

`LocationImpl` renders its frame through
`LocationImpl$MetadataShim.toString()`, which is just
`stackFrame.toString()`. The JDK's concrete frame class
(`java.lang.StackFrameInfo`) overrides `toString()` as
`toStackTraceElement().toString()`. CratonVM does not have that class: it
allocates a **synthetic 7-slot carrier named after the interface**,
`java/lang/StackWalker$StackFrame`
(`native-builtins/src/phases_late/reflect_invoke.rs::populate_stack_frame`),
and registered natives for `getClassName`/`getMethodName`/`getFileName`/
`getLineNumber`/`getByteCodeIndex`/`getDeclaringClass`/`getMethodType`/
`isNativeMethod`/`toStackTraceElement` — but **not** `toString`. So it
inherited `Object.toString`:

```
-> at java.lang.StackWalker$StackFrame@a166
```

The old override had been hiding this the whole time. Anything that prints a
`StackWalker.StackFrame` was affected, not just Mockito.

Fixed by registering `toString` next to the other accessors, reproducing
`StackTraceElement.toString()`'s exact formatting rules — `Cls.method(File:line)`,
degrading to `(File)` when the line is unknown, `(Native Method)` for a
`-2` line with no file, and `(Unknown Source)` otherwise.

## The fix

Both selector overrides are **off by default**, gated behind
`cratonvm_types::flags::mockito_legacy_selectors()`
(`CRATONVM_MOCKITO_LEGACY_SELECTORS=1` restores the old interception as an
escape hatch). `MockMethodAdvice.isOverridden`, which is a genuine semantic
bridge rather than a selector short-circuit, stays registered
unconditionally.

| File | Change |
|---|---|
| `types/src/flags.rs` | new `mockito_legacy_selectors()` opt-in flag |
| `native-builtins/src/test_frameworks.rs` | skip registering the four selector natives unless the flag is set |
| `vm/src/runtime/interpreter.rs` | both dispatch gates honour the flag |
| `vm/src/vm/vm_exec.rs` | the `check_override` half of the dual gate honours the flag |
| `native-builtins/src/phases_late/reflect_invoke.rs` | register `StackWalker$StackFrame.toString()` |
| `native-builtins/src/antlr_intrinsics.rs` | the registration unit test now asserts the selectors are **absent** by default, and keeps asserting `isOverridden` is present |

## Verification

Repro + probes kept at `C:\craton\mockfallback-20260727`
(`run.ps1 -Vm craton|hotspot -Main <probe>`, `FINDINGS.md`).

### 1. Both selectors now match HotSpot

`Plain.java`, plain app loader, no fork loader:

| Probe | HotSpot | CratonVM before | CratonVM after |
|---|---|---|---|
| `LocationFactory.create()` | `LocationImpl` | `Java8LocationImpl` | `LocationImpl` |
| `ModuleMemberAccessor.delegate` | `InstrumentationMemberAccessor` | `ReflectionMemberAccessor` | `InstrumentationMemberAccessor` |
| `location.getSourceFile()` | `Plain.java` | `<unknown source file>` | `Plain.java` |
| failing `verify()` message | `-> at Plain.main(Plain.java:53)` | `-> at <<unknown line>>` | `-> at Plain.main(Plain.java:53)` |

The failure message is now byte-identical to HotSpot's.

`ForkRepro`/`ForkReproInner` (the original fork-loader repro, carried over
from `C:\craton\forkrepro-20260726`) shows the same result.

### 2. The original doc's own evidence table, closed

`LoadCheck.java` reproduces the doc's class-load table via
`ClassLoader.findLoadedClass`, so the probe never loads the watched classes
itself. CratonVM-after is byte-identical to HotSpot in all five rows:

| Class | HotSpot | CratonVM before | CratonVM after |
|---|---|---|---|
| `LocationFactory$DefaultLocationFactory` | loaded | — | loaded |
| `debugging.LocationImpl` | loaded | — | loaded |
| `debugging.Java8LocationImpl` | never loaded | loaded | never loaded |
| `reflection.InstrumentationMemberAccessor` | loaded | — | loaded |
| `reflection.ReflectionMemberAccessor` | never loaded | loaded | never loaded |

(HotSpot needs `--add-opens=java.base/java.lang=ALL-UNNAMED` for the probe;
CratonVM does not — itself a demonstration of the non-enforced JPMS
encapsulation discussed above.)

### 3. `InstrumentationMemberAccessor` actually works

`AccessorProbe.java` drives the operations Mockito relies on through whichever
delegate `ModuleMemberAccessor` selects: private-constructor `newInstance`,
private-field `get`/`set`, private-method `invoke`, and a strongly-encapsulated
`java.lang.ProcessEnvironment.theEnvironment` read. Output is identical on
HotSpot, CratonVM-before and CratonVM-after — so the eager Byte Buddy
bootstrap the override was written to avoid is supported now.

### 4. The class the original doc cites

`org.springframework.boot.tomcat.servlet.TomcatServletWebServerServletContextListenerTests`
— 2/2 tests PASS on the fixed binary.

### 5. Regression sweep

See the "Regression sweep" section appended below.
