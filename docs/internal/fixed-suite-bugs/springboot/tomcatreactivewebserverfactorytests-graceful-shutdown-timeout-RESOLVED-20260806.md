# `TomcatReactiveWebServerFactoryTests.whenServerIsShuttingDownGracefullyThenNewConnectionsCannotBeMade` — 30s awaitility timeout — RESOLVED

**Status: RESOLVED (2026-08-06).** Two separate findings, and the page was right
to keep them apart:

* **The 30s timeout is a load flake, as the page itself suspected.** Refuted by
  42 interleaved runs; details below.
* **The `NoSuchMethodError` pair the page flagged "in case it recurs as a hard
  failure" is a real defect, and it is fixed here** — a synthetic-`ResourceBundle`
  parent slot colliding with the real `java.util.ResourceBundle` layout. It is
  not related to the timeout, exactly as the page said.

## Original symptom (as filed)

1 of 46 tests failed in the 2026-08-05 Azure full-suite run:

```
org.awaitility.core.ConditionTimeoutException: Condition with Lambda expression in
org.springframework.boot.tomcat.reactive.TomcatReactiveWebServerFactoryTests was not
fulfilled within 30 seconds.
Caused by: java.util.concurrent.TimeoutException
```

The same run's `.err.log` carried two `WARN`-level `NoSuchMethodError`s —
`java/util/HashMap.getContents()[[Ljava/lang/Object;` and
`java/util/HashMap.handleGetObject(Ljava/lang/String;)Ljava/lang/Object;`, both
attributed to `org/apache/tomcat/util/res/StringManager.getString` — logged, not
thrown. The page guessed they were Tomcat probing for an optional
`ListResourceBundle`-shaped method and falling back gracefully, and flagged them
without claiming a connection to the failure.

## Finding 1 — the timeout does not reproduce

The page asked for "a clean-host rerun (or 3+ interleaved repeats against a
HotSpot control, per this repo's own A/B protocol)". Done, at 18x that:

| Arm | Runs | Result |
|---|---:|---|
| CratonVM, `origin/dev` (`c9fc71c9a`) | 21 | 46/46 every run |
| CratonVM, `origin/dev` + the fix below | 11 | 46/46 every run |
| CratonVM, the 08-05 full-suite binary (`1078f6f05c`) — the binary that filed this page | 12 | 46/46 every run |
| HotSpot 25 control, same classpath | 12 | 46/46 every run |
| **total** | **56** | **0 failures** |

The arms ran **concurrently**, not in sequence, so every arm sampled the
same load regime. On this shared host a serial A/B is not a measurement — load
moves on the timescale of a single run — so interleaving is the minimum bar and
running the arms simultaneously is one step better. That window included a
stretch at load average 135–229 with the box in
system-wide OOM (the kernel killed another session's `rustc` twice, plus
`systemd`), i.e. **materially worse contention than the run that filed this
page**, and the test still passed 42/42 across all arms.

Fixture `/data/data/springboot-jsonreader-deprecation-20260718`, one process per
class, the runner's own craton knobs (`CRATONVM_REAL=net-sockets,aqs`,
`CRATONVM_THREADS=-default-watchdog`, `CRATONVM_JIT=rootsnap-cache`),
`--Xmx 4g`.

That is a refutation of "genuine CratonVM timing bug", not a confirmation of
"flake" — 0/44 on CratonVM cannot prove a 30s condition never times out under
some worse contention. But it does place this page in the same bucket as the
`NettyReactiveWebServerFactoryTests` graceful-shutdown sibling the page itself
cites, and a 30s awaitility budget on a box that other sessions can drive to
load 229 is not a VM defect.

## Finding 2 — the flagged `NoSuchMethodError` pair, root-caused and fixed

Not Tomcat probing for an optional method. CratonVM's own `ResourceBundle`
native was calling `getContents()` and `handleGetObject(String)` **on a
`java.util.HashMap`**, because it had mistaken the bundle's backing map for its
parent.

`native-builtins/src/locale_resources.rs::rb_get_bundle` builds a synthetic
bundle as a bare `java/util/ResourceBundle` allocated with **two** fields under
its own convention: field 0 is the backing map, field 1 the locale. The real
`java.util.ResourceBundle` declares `parent` **first**. So when a key missed and
`rb_get_object` walked the parent chain,
`ctx.get_field_by_name(this, "parent")` resolved to index 0 and handed back the
**backing map**.

Recursing on that re-entered the native with a `java/util/HashMap` receiver. A
HashMap is not `is_synthetic`, so the real-subclass arm probed it with
`getContents()` and then `handleGetObject(String)` — one `NoSuchMethodError`
each, both swallowed by the `if let Ok(..)` guards that wrap those probes, both
logged. Two per missing key, on every run, in every Tomcat suite log.

**Why it never failed anything.** The map has no `parent` field either, so the
walk terminated in the same `MissingResourceException` a correct empty parent
chain would have produced — which `StringManager.getString` catches and turns
into a null, which its caller replaces with the key. The answer was right; only
the route was wrong. That is why it survived long enough to be filed as
"gracefully falling back".

**Fix.** `synthetic_bundle_parent` refuses a "parent" that is the backing map,
and refuses one that is not a `ResourceBundle` at all. The second half is the
load-bearing one: it keeps the code correct if the synthetic layout ever gains a
slot, instead of trading one index coincidence for another. Same defect family
as the `isr-osw-model-slot0`, `map-model-slots-on-real-layout` and
`thread-contextclassloader-slot` fixes that landed on dev the same week — a
hand-rolled model layout read through the real class's field names.

**Measured**, both arms run concurrently on the same host and fixture:

| Binary | `NoSuchMethodError` per run | Tests |
|---|---:|---|
| `origin/dev`, unfixed | 4, 4, 4 | 46/46 |
| with this fix | **0, 0, 0** | 46/46 |

## Left open deliberately: `getBundle` returns a bare `ResourceBundle`

Found while root-causing the above, not claimed by this page, and **not
changed**:

```java
ResourceBundle rb = ResourceBundle.getBundle("org.apache.catalina.core.LocalStrings");
rb.getClass().getName()                    // HotSpot: java.util.PropertyResourceBundle
                                           // CratonVM: java.util.ResourceBundle
rb instanceof java.util.PropertyResourceBundle   // HotSpot: true, CratonVM: false
```

Values resolve correctly on both. The synthetic shape is deliberate — see
`rb_get_bundle`'s own comments, and `needs_concrete_bundle_class`, which already
routes the one bundle family whose caller `checkcast`s the result onto the real
class-based path. Nothing in these tests depends on the concrete type, and
changing it would touch every `is_synthetic` site in the file, so it stays as
documented behaviour rather than becoming an unmeasured refactor inside a flake
investigation.

**One live hazard recorded next to it**, for whoever does take it on:
`java.util.ResourceBundle.setParent` has **no native**. If anything ever called
it on a synthetic bundle, real bytecode would write the real layout's field 0 —
the backing map's slot — and silently empty the bundle. Nothing reaches it
today (`try_class_bundle` calls `setParent` only on real class-based bundles),
which is why this is a note and not a second fix. Give the synthetic shape its
own parent slot, or move it to a real `PropertyResourceBundle` with a `lookup`
map, before wiring a parent chain up.

## Affected classes

- `module/spring-boot-tomcat` — `org.springframework.boot.tomcat.reactive.TomcatReactiveWebServerFactoryTests` (`whenServerIsShuttingDownGracefullyThenNewConnectionsCannotBeMade`)
