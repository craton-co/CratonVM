# ES-FAIL-03 — RETRACTED (misdiagnosis; native-access `catch (LinkageError)` works fine)

**Status:** ❌ RETRACTED — not a CratonVM bug.
**Date:** 2026-06-18

## What I originally claimed (wrong)

That `NativeAccessHolder.<clinit>`'s `catch (LinkageError)` was not honored on CratonVM — i.e. the `ExceptionInInitializerError`/`UnsatisfiedLinkError` from the missing native libs escaped on CratonVM but was caught on HotSpot, failing every `ESTestCase`.

## Why it's wrong

Direct evidence from the real CratonVM `BuildTests` run shows the catch **does** fire and native access falls back correctly — identical to HotSpot:

```
[WARN][o.e.n.NativeAccess] Unable to load native provider. Native methods will be disabled.   <- the catch block logging
[WARN][o.e.n.NativeAccess] Cannot check if running as root because native access is not available   <- NoopNativeAccess behavior
```

Both VMs: `LoaderHelper.<clinit>` NPE → `ExceptionInInitializerError` → **caught by `NativeAccessHolder`'s `catch (LinkageError)`** → logged → `INSTANCE = NoopNativeAccess` → `initializeNatives` continues. The EIIE stack that appears in the logs is the **benign, caught** one (printed by `logger.warn(..., e)`), which I mistook for the failure.

(`NaProbe`, which calls `NativeAccess.instance()` in isolation, fails differently only because the ES logging provider isn't initialized there — `LogManager.getLogger` NPEs — which is an artifact of the isolated harness, not the real test flow.)

## What the real blocker turned out to be

[**ES-FAIL-04**](ES-FAIL-04-arraylist-sublist-toarray-missing.md) — the synthetic `ArrayList.subList().toArray(T[])` `NoSuchMethodError`. That is the actual dominant `:server` failure, and it is fixed.

## Lesson

CratonVM prints a `<clinit> failed — wrapping in ExceptionInInitializerError` diagnostic for **every** failed `<clinit>`, including ones that are immediately caught downstream. Don't treat that diagnostic (or a logged-and-caught exception stack) as the test's failure cause — read the *terminal* error (`OK (...)` / `Tests run: …` / the uncaught `terminated with error` line).
