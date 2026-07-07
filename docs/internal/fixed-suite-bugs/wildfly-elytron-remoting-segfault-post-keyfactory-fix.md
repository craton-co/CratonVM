# ElytronRemoteOutboundConnectionTestCase: native SIGSEGV during elytron subsystem / remoting-client test execution

Status: RESOLVED — fixed 2026-07-07
Severity: was High (hard native crash, not a catchable Java exception; blocked the whole test class)
First confirmed: 2026-07-07, Azure worktree `wt-keyfactory-translatekey` (branch `fix/keyfactory-translatekey-20260707`)
Root-caused (initial, later corrected): 2026-07-07, Azure worktree `wt-elytron-segv-20260707` (branch `fix/elytron-remoting-segv-20260707`)
Actually fixed: 2026-07-07, Azure worktree `wt-a4-register-oop-20260707` (branch `fix/a4-register-oop-bitmap-20260707`)

## Symptom

Running `org.jboss.as.test.manualmode.ejb.client.outbound.connection.security.ElytronRemoteOutboundConnectionTestCase`
(module `testsuite/integration/manualmode`) under CratonVM (`jit-real` mode) via Maven/Surefire crashed
the forked JVM outright:

```text
[ERROR] Process Exit Code: 139
[ERROR] Crashed tests:
[ERROR] org.jboss.as.test.manualmode.ejb.client.outbound.connection.security.ElytronRemoteOutboundConnectionTestCase
[ERROR] org.apache.maven.surefire.booter.SurefireBooterForkException: ExecutionException The forked VM
terminated without properly saying goodbye. VM crash or System.exit called?
```

## Root cause (CONFIRMED and FIXED 2026-07-07): missing autoboxing in `OptionMap.get(Option)Object`

The initial investigation (same day, see history below) got a real gdb backtrace and found the crash
site — `NativeContextImpl::read_string` dereferencing a garbage `ObjectRef` inside
`xnio_async::native_builder_set` — but mis-attributed the mechanism to the known-but-unrelated "A4
register-only oop" JIT/GC gap. That attribution was wrong. The **actual** root cause, found by enabling
`CRATONVM_DBG_JIT_MIC=1` and inspecting the exact call sequence immediately before the crash:

```text
[JIT_MIC] org/xnio/OptionMap.get(Lorg/xnio/Option;)Ljava/lang/Object; cached_cid=0 recv_cid=2426 entry=0
[JIT_MIC] org/xnio/OptionMap$Builder.set(Lorg/xnio/Option;Ljava/lang/Object;)Lorg/xnio/OptionMap$Builder; cached_cid=0 recv_cid=2404 entry=0
Segmentation fault (core dumped)
```

`OptionMap.get(Option)Object`'s result flows directly into `Builder.set(Option, Object)`'s second
argument (copying an option from one map into another, a common XNIO idiom). `native_option_map_get`
(`native-builtins/src/xnio_async.rs`) stores numeric option values unboxed internally
(`OptionValue::Int`/`Long`/`Bool`, populated by the primitive `set(Option<Integer>, int)`-family
overloads in `native_builder_set`), which is a fine internal representation — but when read back through
the **generic, `Object`-returning** `get(Option)Object` / `get(Option, Object)Object` overloads, the old
code returned the raw value directly:

```rust
Some(OptionValue::Int(n)) => Ok(Some(Value::Int(*n))),   // WRONG: declared return type is Object
Some(OptionValue::Long(n)) => Ok(Some(Value::Long(*n))), // WRONG
Some(OptionValue::Bool(b)) => Ok(Some(Value::Int(if *b { 1 } else { 0 }))), // WRONG
```

This violates the Java method's contract (`Ljava/lang/Object;`) — a `60000` int option (e.g.
`Options.READ_TIMEOUT`'s common default) came back as raw `Value::Int(60000)` instead of a boxed
`Integer`. The **interpreter's** generic native-return handling tolerated this (manifesting more mildly,
as a spurious `null` — confirmed separately, see Verification below), but the **JIT's** fast MIC cache-
miss return path (`vm/src/jit/helpers.rs`, the `invoke_or_native` result conversion) takes the raw `i64`
and treats it as an already-valid pointer-shaped `ObjectRef` when the call site's static return type says
`Object`:

```rust
Some(Value::Int(v)) => v as i64,   // returned as the call's raw ABI result — the JIT
                                    // caller reads it back as a pointer for an Object-typed call
```

`60000 as i64 = 0x00_00_00_00_00_00_EA_60` — the exact faulting address (`segfault at ea60`) confirmed by
kernel logs and gdb across every reproduction. The very next native call to touch that bogus "reference"
(`Builder.set`'s `ctx.read_string(s)` on the "value" argument) dereferences it and crashes.

## Fix

`native_option_map_get`, in the three primitive branches, now boxes the value through the crate's
existing `lang_class::box_value(ctx, value, type_desc)` helper (the same helper used elsewhere for
reflection/`Method.invoke` boxing) before returning it as `Value::Object(Some(boxed))`, matching what a
real `Map<Option<?>, Object>`-backed `OptionMap` would already hold. The specialized primitive overloads
(`get(Option<Integer>, int)I`, `get(Option<Long>, long)J`, `get(Option<Boolean>, boolean)Z`) are
untouched — they correctly return raw primitives because *their* declared return type is the primitive
itself, not `Object`.

## Verification

1. **The real WildFly repro** — `ElytronRemoteOutboundConnectionTestCase` via the actual Maven/Surefire
   harness — no longer crashes. Confirmed twice in a row (`Tests run: 22, Failures: 0, Errors: 22`, zero
   `Process Exit Code: 139` / `Segmentation fault` anywhere in either run's log — the 22 errors are a
   separate, pre-existing `java.io.IOException` issue unrelated to this fix, matching exactly what
   `CRATONVM_DISABLE_JIT=1` already showed against the pre-fix binary).
2. **Targeted correctness repro** (`BoxingRepro.java`, driving the real `xnio-api` jar directly, no
   WildFly needed): sets an int/string/boolean option, reads them back via the generic `get(Option)Object`
   accessor, and checks `getClass()`, `instanceof`, `equals()`, the typed-primitive-overload path, and the
   default-`null` fallback for a missing key. Pre-fix CratonVM (interpreter-only, too short-lived to JIT)
   threw `NullPointerException: Cannot invoke "Object.getClass()" because "<local3>" is null` — the
   SAME missing-box bug, manifesting as null instead of a crash outside JIT. Fixed CratonVM matches real
   HotSpot output exactly on every check.
3. **Stress repros** (16-thread × 15s, 500K-iteration single-thread, both using the real `OptionMap.Builder`)
   — clean on the fixed binary, no regressions.
4. **`cargo test -p cratonvm-native-builtins xnio_async`** — 26/26 pass. Three pre-existing tests asserted
   the *buggy* raw-`Value::Int` return and needed updating to assert the correctly-boxed value instead
   (they were literally encoding the bug as expected behavior).

## Investigation history (for context — the initial mis-attribution)

The same-day investigation that found the crash site initially concluded this was the long-standing,
deliberately-deferred "A4 register-only oop" JIT/GC gap (`OopMapEntry` has no register-oop bitmap —
tracked in `fork6-fjp-multithread-jit-root-reclamation.md` / `gcstress-residual-corruption-faces.md`),
based on: (a) `CRATONVM_DISABLE_JIT=1` made the crash disappear, and (b) the crash shape (small,
non-heap-looking faulting address) superficially matched that family's known signature. Both were true
but pointed at the wrong mechanism — every existing GC-root-visibility mitigation was bisection-tested
(`CRATONVM_NO_PRECISE_JIT_MAPS=1`, `CRATONVM_JIT_SAFEPOINT_REG_SPILL=all`, `CRATONVM_MOVING_YOUNG=1` +
`CRATONVM_SHADOW_STACK=1`) and **none** changed the crash, which in hindsight was the tell that this
wasn't a root-visibility problem at all. Three independent minimal-repro attempts using real XNIO
`OptionMap.Builder` (plain loop, 16-thread concurrent, reflective-recursion-mimicking) also failed to
reproduce it — because none of them replicated the actual data flow (`get()`'s return value fed directly
into `set()`'s argument). Enabling the existing `CRATONVM_DBG_JIT_MIC=1` diagnostic against the real
WildFly repro was what actually cracked it: the exact call sequence immediately preceding the crash,
including the *cache-miss* status (`entry=0`) showing this was a cold first-dispatch, not a warmed hot
path — which reframed the investigation away from "stale reference across a GC safepoint" and toward
"wrong value produced at this exact call," leading directly to the boxing bug above.

**Lesson for next time**: when several independent GC-tracking mitigations *all* fail to change a
crash, stop assuming it's a GC-root-visibility bug — it likely isn't. A faulting address that looks like
a plausible *domain value* (a round number, a common config default) is a stronger signal of a
type/boxing confusion than of pointer staleness.

## Related

Found via [[wildfly-keyfactory-translatekey-null-spi]]'s own fix-verification run — not caused by that
fix, just newly reachable because of it (this WildFly test class never got far enough to hit this code
path before). NOT the A4 register-only-oop family after all, despite the initial same-day
misattribution — see investigation history above. `docs/known-issues/fork6-fjp-multithread-jit-root-reclamation.md`
and `gcstress-residual-corruption-faces.md` remain open, unrelated issues.
