# `sun.misc.Unsafe.MEMORY_ACCESS_OPTION` repair still fails for real Keycloak/Infinispan integration — field-index hypothesis REFUTED; real cause is `MemoryAccessOption` never being loaded

Status: partially fixed (2026-07-14) — `already_initialized`'s false-positive risk is
hardened, but the actual failure mode observed via a real repro this session is a
**different** bug than originally hypothesized, and remains open. Full real-Keycloak
verification is separately blocked by
[docs/known-issues/vm/locale-real-jdk-bootstrap-noclassdeffounderror.md](../vm/locale-real-jdk-bootstrap-noclassdeffounderror.md).

Date observed: 2026-07-13 (original), reinvestigated 2026-07-14 with a real, isolated,
falsifiable repro (see below) on the Azure build host.

## Original summary (2026-07-13, still accurate)

36 `testsuite/model` classes failed with:
```
=> java.lang.ExceptionInInitializerError
 Caused by: org.infinispan.commons.CacheConfigurationException: Unable to construct a GlobalComponentRegistry!
 Caused by: java.lang.RuntimeException: Failed to construct component io.netty.channel.EventLoopGroup, path io.netty.channel.EventLoopGroup
 Caused by: java.lang.IllegalStateException: failed to create a child event loop
 Caused by: java.lang.NullPointerException: Cannot invoke "sun.misc.Unsafe$MemoryAccessOption.ordinal()"
```
with the repair's own log line confirming it ran and chose not to repair:
```
Post-clinit fixup: sun.misc.Unsafe MEMORY_ACCESS_OPTION policy=ALLOW repaired=false
```

## 2026-07-14 reinvestigation: the field-index hypothesis is REFUTED

The original doc hypothesized that `already_initialized`'s manual static-field-index
recomputation (`vm/src/vm/vm_util.rs` ~line 2280) diverged from the indexing convention
`get_static_shared`/`resolve_field_ref` actually use, causing a false-positive
"already initialized" read that forces `repaired=false`.

**This is not the bug.** Careful comparison (this session) of `already_initialized`'s
static-field counting loop against BOTH `resolve_field_ref` (`vm/src/runtime/
interpreter.rs`) and `set_static_by_name`'s own documented convention (same file, ~line
2155, which has an explicit comment warning about exactly this class of bug from an
earlier, unrelated BigDecimal incident) shows all three use the **identical**
convention: count only static fields, in `class.fields` declaration order, break on the
first name match without incrementing for the match itself. No divergence exists.

## Real root cause (evidence-based, 2026-07-14)

An isolated, minimal, falsifiable repro (a standalone Java program doing
reflection-based `Unsafe.putOrderedLong`, matching the ORIGINAL fix's own validation
probe from `docs/internal/fixed-suite-bugs/
testsuite-model-unsafe-putorderedlong-memoryaccessoption-npe-FIXED.md`) **reproduces
`repaired=false` and the exact NPE, on the current dev tip**, with debug tracing added
to pin down exactly why:
```
Post-clinit fixup: sun.misc.Unsafe MEMORY_ACCESS_OPTION repair could not resolve
enum_slot (unsafe_option_slot=Some(25) enum_class_id=None policy_field=ALLOW) —
repair skipped
```

`enum_class_id=None` — `sun/misc/Unsafe$MemoryAccessOption` (the nested enum whose
`ALLOW`/`WARN`/`DEBUG`/`DENY` constants the repair copies from) **was never loaded** at
the point the repair runs. `already_initialized` correctly read `Value::Int(0)`
(genuinely uninitialized, not a false positive) — the repair falls through to the
`enum_slot` branch, which needs `cm.find_class_by_name("sun/misc/Unsafe$MemoryAccessOption")`
to succeed, and it can't: **`find_class_by_name` is lookup-only and never triggers
class loading.** In this minimal repro nothing else in the program had touched
`MemoryAccessOption` yet — even though real `Unsafe.<clinit>` bytecode is documented
(this repair's own comments) to call `MemoryAccessOption.value()` as part of its own
execution, which should load it as a side effect. Why that reference doesn't
consistently load the enum class in every execution path is unresolved — plausibly
`Unsafe.<clinit>`'s real bytecode doesn't always reach that statement (e.g. an early
branch on the configured policy), or loads it via a path this investigation didn't
trace far enough to find.

### Fix attempted and reverted: eager force-load deadlocks

The natural fix — force-load and force-initialize `sun/misc/Unsafe$MemoryAccessOption`
via `shared.load_class_concurrent(...)` + `ensure_class_initialized_shared(...)` when
`find_class_by_name` returns `None` — was implemented, and **deadlocked** (confirmed:
process hung indefinitely, `ps aux` showed 0% CPU, genuinely blocked not spinning).
Root cause: `post_clinit_fixup` (where this repair lives) runs from *inside*
`initialize_class_shared` for `sun/misc/Unsafe` itself, **before** that call's
`InitCleanupGuard`/`finalize_init` releases its claim on the class (see the
"success-path" call site comments in `vm_util.rs` ~line 1169). Recursively driving
another class's *full* initialization from within this window is unsafe — this needs a
non-recursive fix (e.g. deferring the eager load until after `finalize_init`, via a
follow-up task/queue, rather than calling it synchronously from inside the fixup).
**Reverted** rather than shipped with a deadlock risk.

## What was actually shipped (2026-07-14, safe, verified)

`vm/src/vm/vm_util.rs`'s `already_initialized` check now verifies the slot actually
holds an instance of `MemoryAccessOption` (via `shared.heap.class_id_of(obj) ==
enum_class_id`) rather than trusting "slot is non-null" alone — per the original doc's
own suggested next-step #3. This is a genuine, low-risk correctness hardening
(confirmed via `cargo build --release`, no deadlock, no behavior change on the isolated
repro since it wasn't the false-positive case) but does **not** resolve the
`enum_class_id=None` failure mode found this session. Verified: on both dev-tip
(baseline) and this fix, the isolated Unsafe probe produces the identical
`repaired=false` + NPE — the fix is safe but not (yet) sufficient.

## Next steps

1. Implement the eager-load fix for `enum_class_id=None` **without** the recursion
   hazard — e.g. have `initialize_class_shared` (or its caller) perform a
   *post*-`finalize_init` pass for `sun/misc/Unsafe` specifically that ensures
   `MemoryAccessOption` is loaded+initialized, or restructure `post_clinit_fixup`'s
   `sun/misc/Unsafe` arm to only *read* (never trigger loading of) `enum_class_id`
   and instead schedule the repair to run again (idempotently — the fixup already
   only overwrites when `!already_initialized`) the next time ANYTHING touches
   `sun/misc/Unsafe` post-boot, by which point `MemoryAccessOption` is far more likely
   to have been loaded via some other path.
2. Alternatively (more invasive, likely more correct): find out why real
   `Unsafe.<clinit>` bytecode doesn't reliably load `MemoryAccessOption` as a side
   effect in the first place, and fix that instead of working around it in the repair.
3. Re-verify against the real `testsuite/model` classes once
   [the java.util.Locale bootstrap bug](../vm/locale-real-jdk-bootstrap-noclassdeffounderror.md)
   is fixed — that bug independently blocks every `testsuite/model` class from
   completing far enough to reach this code path (confirmed this session: the Locale
   bug fires earlier in boot, during `Liquibase`'s provider-factory init, before
   Netty/Infinispan's event-loop construction is ever reached).

## Repro

Isolated (no Keycloak needed) — this is the fastest way to iterate on this specific bug:
```java
import java.lang.reflect.Field;
import sun.misc.Unsafe;

public class UnsafeProbe {
    static long value;
    public static void main(String[] args) throws Exception {
        Field f = Unsafe.class.getDeclaredField("theUnsafe");
        f.setAccessible(true);
        Unsafe unsafe = (Unsafe) f.get(null);
        Field optField = Unsafe.class.getDeclaredField("MEMORY_ACCESS_OPTION");
        optField.setAccessible(true);
        System.out.println("MEMORY_ACCESS_CONFIGURED_OPTION=" + optField.get(null));
        Field valueField = UnsafeProbe.class.getDeclaredField("value");
        long offset = unsafe.objectFieldOffset(valueField);
        unsafe.putOrderedLong(new UnsafeProbe(), offset, 42L);  // NPEs here pre-fix
        System.out.println("UNSAFE_PUT_ORDERED_LONG_OK memoryAccess=allow");
    }
}
```
Run: `cratonvm --java-home <realjdk25> -cp . UnsafeProbe`. Expect (pre-fix, and still on
dev tip after the 2026-07-14 partial fix):
```
Post-clinit fixup: sun.misc.Unsafe MEMORY_ACCESS_OPTION policy=ALLOW repaired=false
MEMORY_ACCESS_CONFIGURED_OPTION=null
...NullPointerException: Cannot invoke "sun.misc.Unsafe$MemoryAccessOption.ordinal()"
```

Full real-Keycloak repro (currently blocked by the separate Locale bug before reaching
this code path — see that doc for the harness setup, host, and exact commands):
```
cd /data/data/wt-keycloak-memaccess-fieldindex-20260714  (Azure host victor@20.83.144.174)
apps/keycloak/kc-runner/KcRunner org.keycloak.testsuite.model.authz.ConcurrentAuthzTest
  (via cratonvm --java-home /home/victor/jdk25
   -Dkeycloak.model.parameters=Infinispan,Jpa -Djava.util.logging.manager=org.jboss.logmanager.LogManager
   -Dkeycloak.connectionsJpa.default.driver=org.h2.Driver -Dkeycloak.connectionsJpa.default.database=keycloak
   -Dkeycloak.connectionsJpa.default.user=sa -Dkeycloak.connectionsJpa.default.password=
   -Dkeycloak.connectionsJpa.default.url=jdbc:h2:mem:test;DB_CLOSE_DELAY=-1
   -cp <testsuite/model target/classes:target/test-classes:resolved-deps:kc-runner> KcRunner <class>)
```

## Evidence

- Isolated probe run on both baseline (dev-tip `6addc1e0`) and the 2026-07-14 fix,
  Azure host `/data/data/wt-keycloak-memaccess-fieldindex-20260714`, binaries
  `target/release/cratonvm-memaccess-baseline-20260714` /
  `cratonvm-memaccess-fieldindex-fix3-20260714` — identical `repaired=false` + NPE on
  both, confirming the shipped fix doesn't regress anything and doesn't (yet) resolve
  this failure mode.
- Debug-instrumented build (`cratonvm-memaccess-fieldindex-fix-debug-20260714`,
  reverted before final commit) directly confirmed `enum_class_id=None` at the moment
  of failure.
- Original (incomplete) fix doc:
  `docs/internal/fixed-suite-bugs/testsuite-model-unsafe-putorderedlong-memoryaccessoption-npe-FIXED.md`.
  Fix source: `vm/src/vm/vm_util.rs` `post_clinit_fixup`'s `"sun/misc/Unsafe"` arm
  (~line 2254).
