# `JooqAutoConfigurationTests` — timeout — FIXED 2026-08-07

**Status: ✅ FIXED.** `Method.getModifiers()` rebuilt the declaring class's
entire method table on every call. On jOOQ's 1003-method `DefaultDSLContext`
that made one getter cost **39 µs** against HotSpot's **1 ns**, and Spring's
annotation scanner calls it ~1.5 million times per context. Fixed by reading
the value out of the mirror field it was already stored in.

| arm | wall clock | result |
|---|---:|---|
| CratonVM before | **1013.9 s / 972.2 s** | 17/17 |
| CratonVM after | **42.5 s / 40.5 s** | 17/17 |
| HotSpot (same host, same runner) | 2.9 s | 17/17 |

A/B/B/A interleaved on the Azure host, 1200 s ceiling, `module/spring-boot-jooq`
via `SbRunner`. **24x**, and 14.7x HotSpot afterwards — an ordinary CratonVM
ratio, comfortably inside the suite's 300 s budget that this class had been
blowing since 2026-08-02.

## Root cause

`native-builtins/src/lang_class.rs::method_modifiers_value` answered
`Method.getModifiers()` with:

```rust
ctx.declared_methods(class_id).into_iter().find(|m| m.name == name && m.descriptor == desc)
```

`declared_methods` allocates a fresh `MethodMetadata` — two owned `String`s and
two `Vec`s — for **every method the class declares**. So one getter call on
`DefaultDSLContext` built ~1000 structs and ~3000 strings, linear-searched
them, and threw the lot away, to obtain a value already sitting in the mirror's
`modifiers` field.

`probes/MethodGetterCostProbe.java`, ns per call, swept by the DECLARING
class's method count:

| declared methods | 125 | 250 | 500 | 1000 | `DefaultDSLContext` (1003) |
|---|---:|---:|---:|---:|---:|
| CratonVM before | 4233 | 7354 | 13015 | 25149 | **38995** |
| CratonVM after | 1146 | 1175 | 1206 | 1085 | **1137** |
| HotSpot | 23 | 23 | 1 | 1 | 1 |

The cost was **linear in the declaring class's method count** — which is what
turns a wide class into a wall, and why this class and not another.

Spring's `AnnotationsScanner.isOverride` calls it once per candidate method
pair. A native-invocation census (`--dump-native-registry`) of ONE
`MethodIntrospector.selectMethods` pass over `DefaultDSLContext`:

```
Method.getParameterCount  3 075 043
Method.getModifiers       1 549 873
Method.getName              942 430
```

1.55M × 39 µs = 60 s, and the pass took 65–75 s (HotSpot: 37 ms). That is the
whole of it. `isOverride` itself went 40 152 ns → 2 744 ns per pair.

### The fix keeps the search for mirrors CratonVM did not build

The search was not pointless: a JDK-private `Method` copy can carry a truncated
`modifiers`, and only the class file settles it. But a mirror
`create_method_object` built has the exact `access_flags` in its `modifiers`
field — the very same `m.access_flags.bits()` the search would have found — and
carries the `METHOD_EXTRA_TRUSTED_MARKER` metadata tail that
`read_method_descriptor` already refuses to trust without. A JDK-allocated copy
is allocated at the real JDK width, has no tail, fails
`method_has_trusted_metadata`, and takes the old path unchanged.

## Correction: the 2026-08-05 page named the wrong native, and the arithmetic says so

The previous revision concluded **"(a) is `Class.getDeclaredMethods()`"**. It is
not. Measured on the same workload with `CRATONVM_DBG_GDM_PROF=1`,
`Class.getDeclaredMethods` is called **8 times, for 13 ms total**, in a
65.6-second `selectMethods` pass — **0.02%**.

The chain looked airtight and every link was individually true: a
`--stack-sample-ms` profile put 93.5% of samples under
`EventListenerMethodProcessor`; a probe of `MethodIntrospector.selectMethods`
reproduced a 332x gap; a microbenchmark found `getDeclaredMethods` to be 73x.
The unchecked step is the one joining them — nobody multiplied 3.4 ms by the
number of CALLS. 3.4 ms can only make 65 s if it is called ~19 000 times, and
it is called 8.

**The instrument that does answer it is `--dump-native-registry`**, whose
`invocations` field gives exact per-native call counts in a single run.
`--stack-sample-ms` cannot: a native creates no interpreted frame, so its time
is charged to the calling *Java* frame. That is why 92% of a `--nojit` sample
set reported `AnnotationsScanner.isOverride` as the leaf — the three natives it
calls were invisible, and `isOverride`'s own four-line body was innocent.
(`perf` is not an alternative on this host: `perf_event_paranoid` is 4, so even
`-e cpu-clock` user-only recording is refused.)

## `Class.getDeclaredMethods()` was ALSO quadratic — fixed in the same branch

Kept because it is real and now measured, **not** because it was the timeout.

`create_method_object` called `ctx.method_exceptions(class_id, &name, &desc)`
and `ctx.method_signature(class_id, &name, &desc)` once per mirror. Both are
O(methods) linear searches of the declaring class's method table, inside a loop
that already runs once per method. With `CRATONVM_DBG_GDM_PROF=1` (added here;
splits `create_method_object` into phases) on a 1000-method class, of a
2440 µs call:

```
method_exceptions = 766 us   method_signature = 659 us
```

58% of the call, and quadratic — `method_exceptions` is 38 µs at n=250 and
766 µs at n=1000: 4x the methods, 20x the cost.

`MethodMetadata::exceptions` already carried the first answer, computed by the
VM's `declared_methods` from the very same `Attribute::Exceptions`.
`MethodMetadata::signature` now carries the second, from the same pass. A
fabricated method (synthetic stub, lambda-proxy SAM, shim) is not generic and
correctly reports `None`. Result:

| declared methods | 125 | 250 | 500 | 1000 |
|---|---:|---:|---:|---:|
| before (µs/call) | 219.2 | 445.4 | 1219.2 | 4125.0 |
| after (µs/call) | 96.3 | 191.8 | 380.8 | 761.1 |
| after, µs **per method** | 0.77 | 0.77 | 0.76 | 0.76 |
| HotSpot | 30.1 | 20.3 | 73.7 | 88.4 |

Per-method cost is now flat, so the super-linear term is gone. The remaining
8.6x at n=1000 is ordinary per-mirror allocation and is not pursued.

The old page's *"Dead end (measured, do not repeat)"* — hoisting
`ensure_class_initialized` / `class_num_total_fields` /
`method_class_has_named_layout` and the 13 by-name field writes to once per
call — **stands, and the phase profiler now explains why it measured as a
wash**: those phases are 300 µs + 85 µs of a 2440 µs call, while the two table
searches were 1425 µs of it. It was optimising the wrong 16%.

Its proposed redesign — caching root `Method` mirrors per class and handing
back cheap copies, HotSpot-style — is **not needed** and was not attempted.

## What is left, and what was not the problem

* **`Method.getName` (~900 ns) and `getParameterCount` (~600 ns)** are now the
  next items, both ~300x HotSpot but **constant**, not size-dependent. They are
  near this VM's ordinary native-call floor, so they belong to the general
  native-dispatch cost, not to this class. Not pursued.
* **(b) the `Settings` NPE — withdrawn, HotSpot does the same thing.** The old
  page called `Failed to instantiate [org.jooq.conf.Settings]: Factory method
  'settings' threw exception with message: null` a real, cheap, still-open
  correctness bug. It is not ours. Grepping the two `.out.log`s for that
  message gives **byte-identical output on both VMs**:

  ```
  HS: Factory method 'settings' threw exception with message: null
  HS: Factory method 'settings' threw exception with message: Resource class path resource [does-not-exist.xml] set in spring.jooq.config does not exist
  CV: Factory method 'settings' threw exception with message: null
  CV: Factory method 'settings' threw exception with message: Resource class path resource [does-not-exist.xml] set in spring.jooq.config does not exist
  ```

  Both lines are Spring Boot's own negative-path tests logging a context they
  deliberately fail to start; the class scores 17/17 with 0 failures on HotSpot
  and on both CratonVM arms, before the fix as well as after. The lesson is the
  cheap one: a `WARN` line in a passing class is not evidence of anything until
  it has been diffed against the oracle.
* The 2026-07-18 closure (`jooq-destroy-method-ambiguity-and-hang-FIXED.md`,
  Panama downcall-adapter fallback) is unrelated and still holds; this was
  never a regression of it. The 2026-08-05 page's own Correction §1 was right
  that this class hung identically in the 08-02 full suite.

## The whole family, verified (2026-08-07)

While this page was being written a concurrent session expanded the
known-issues page from one class to **six**, attributing all of them to the
same reflective cost — correctly as to the *mechanism being shared*, and
incorrectly as to which native it is (that page still named
`Class.getDeclaredMethods()`; see the Correction above). Its structural
argument is worth keeping, because it is what makes the shared term
verifiable rather than inferred: the three `@JooqTest` slices resolve through
`AutoConfigureJooq.imports` straight to `JooqAutoConfiguration`, and
`JooqFlywayDatabaseInitializationTests` constructs
`new DefaultDSLContext(SQLDialect.H2)` directly — so every one of them builds
the same 1003-method bean, and pays the same per-`getModifiers()` cost against
it.

All six are green on the fixed binary, every one inside the 300 s budget:

| class | before | after |
|---|---:|---|
| `JooqAutoConfigurationTests` | 300 s HANG (1013.9 s uncapped) | **80.0 s**, 17/17 |
| `JooqFlywayDatabaseInitializationTests` | 300.002 s HANG | **11.9 s**, 3/3 |
| `JooqTestIntegrationTests` | 300.088 s HANG | **13.2 s**, 7/7 |
| `JooqTestPropertiesIntegrationTests` | 300.010 s HANG | **15.1 s**, 2/2 |
| `JooqTestWithAutoConfigureTestDatabaseIntegrationTests` | 300.182 s HANG | **14.1 s**, 1/1 |
| `SpringApplicationTests` (`core/spring-boot`, no jOOQ at all) | 300.065 s HANG | **197.0 s**, 102/102 |

The "before" column is that page's own full-suite figures
(`craton-fullsuite-windows-20260806`, `-Xmx 2g`, 300 s/class), measured on a
binary built from `dev` as of 08-06 — before this fix.

`SpringApplicationTests` is the one worth noting: it has no jOOQ dependency,
no `DefaultDSLContext` and no HikariPool, and that page filed it "by
elimination" from its timing shape alone. It is the same bug. `getModifiers()`
is charged per *candidate method pair* by `AnnotationsScanner.isOverride`, so
any class whose context startup drives an annotation scan pays it — the jOOQ
types are simply the extreme of the distribution, not a special case. That is
the same point the original page made about `DefaultDSLContext` not being a
jOOQ problem, now confirmed on a class with no jOOQ in it.

## Reproducing

```bash
# the class itself
apps/spring-boot-suite-runner  # (PowerShell) or the Linux sbone.sh equivalent
#   module/spring-boot-jooq org.springframework.boot.jooq.autoconfigure.JooqAutoConfigurationTests

# the isolated getter cost, both VMs, no Spring needed
javac -d /tmp probes/MethodGetterCostProbe.java
java -cp /tmp:jooq.jar MethodGetterCostProbe
cratonvm --java-home $JDK -c /tmp:jooq.jar MethodGetterCostProbe

# where a getDeclaredMethods call's time goes
CRATONVM_DBG_GDM_PROF=1 cratonvm ... ReflectionCacheProbe org.jooq.impl.DefaultDSLContext 20

# exact per-native call counts for any workload
cratonvm --dump-native-registry /tmp/nat.json ...
```

## Affected classes

- `module/spring-boot-jooq` — `org.springframework.boot.jooq.autoconfigure.JooqAutoConfigurationTests`

The same native is the likely load-bearing term in the Tomcat annotation-scan
wall (224–259x) and the webapp-deploy wall (234x) already on file — both are
annotation scans over wide classes, which is exactly the shape that was paying
39 µs per `getModifiers`. Worth re-measuring those against this fix.
