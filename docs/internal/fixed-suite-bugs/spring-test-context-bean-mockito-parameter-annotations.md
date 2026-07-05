# Spring `test.context.bean` Mockito / parameter-annotation cluster

Status: fixed 2026-07-04.

## Symptom

The Spring Framework `test.context.bean.` slice matched HotSpot under discovery,
but CratonVM `jit-real` had seven CV-unique failures in bean override tests:
constructor parameter annotation lookups returned arrays with the wrong shape,
and Mockito inline mocks of `StringBuilder` corrupted ByteBuddy descriptor
generation or crashed in `AbstractStringBuilder` bytecode.

## Root causes

- `Class.getParameterAnnotations()` exposed raw class-file parameter annotation
  arrays without aligning them to the reflective parameter count. Constructors
  with hidden enum or non-static inner-class parameters could therefore report
  annotation rows shifted away from the Java-visible parameters.
- Mockito inline redefinition of `java.lang.StringBuilder` made CratonVM stop
  using native StringBuilder layout shims. Real JDK bytecode then mixed compact
  `byte[]` assumptions with CratonVM's synthetic builder backing layout, which
  broke ByteBuddy descriptor strings and Mockito failure formatting.
- Real JDK compact-string paths around `String(AbstractStringBuilder, Void)` and
  `System.arraycopy` needed narrow bridges for CratonVM's synthetic
  `char[]`-backed builder contents.

## Fix

- Normalize parameter annotation rows to the descriptor-visible parameter count,
  including constructor-only hidden enum and non-static nested-class receiver
  parameter shifts.
- Add a native `String(AbstractStringBuilder, Void)` constructor and keep
  StringBuilder count/coder slots coherent enough for real JDK code that still
  reads `coder` and `count`.
- Add scoped `System.arraycopy` bridges for real JDK
  `AbstractStringBuilder`/`String` compact-string transitions.
- Keep `StringBuilder`/`StringBuffer`/`AbstractStringBuilder` constructor,
  `append(...)`, and `toString()` layout shims authoritative after JVMTI
  redefinition, while still letting Mockito advice handle mock interactions
  such as `length()` and `substring(...)`.

## Validation

- Fixed CratonVM on Azure:
  `/data/cratonvm/apps/spring-suite-runner/out/jit-real-all-20260704-231807`
  reported `classes: EMPTY=3 OK=117` and `failed=0` for
  `--only "test\.context\.bean\."`.
- HotSpot baseline on the same slice:
  `/data/cratonvm/apps/spring-suite-runner/out/hotspot-all-20260704-232412`
  reported the same `classes: EMPTY=3 OK=117` and `failed=0`.
