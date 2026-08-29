# `java.util.Properties.clone()` NPEs — FIXED 2026-08-29

## Status

**FIXED.** `clone()` and `replaceAll()` are now natively overridden; the class
the report named, `org.hibernate.reactive.HQLQueryTest`, runs `ok=9 failed=0`
against a live Postgres through Testcontainers, and the exact call its stack
trace blamed succeeds. `RPropertiesClone` (30 checks, oracle-matched against
HotSpot) guards it.

## Symptom, as reported

138 of 191 classes in one `hibernate-reactive-suite-runner` batch failed
identically:

```
Caused by: java.lang.NullPointerException
	at java.util.Properties.clone(Properties.java:1526)
	at org.testcontainers.shaded.com.github.dockerjava.core.DefaultDockerClientConfig.createDefaultConfigBuilder(DefaultDockerClientConfig.java:222)
	at org.testcontainers.dockerclient.TestcontainersHostPropertyClientProviderStrategy.<init>(TestcontainersHostPropertyClientProviderStrategy.java:23)
	at java.util.ServiceLoader$ProviderImpl.newInstance(ServiceLoader.java:707)
```

`ServiceLoader` rewraps that NPE as a `ServiceConfigurationError`, which callers
did not expect from a provider that should simply fail over to the next
strategy, so it propagated as a hard failure instead of a graceful fallback.

## Root cause

Line 1526 is the second of the method's two statements:

```java
public synchronized Object clone() {
    Properties clone = (Properties) cloneHashtable();   // 1525 — = Object.clone()
    clone.map = new ConcurrentHashMap<>(map);           // 1526 — throws
    return clone;
}
```

`map` is null, **and that is by design.** The note on
`register_properties_sidetable` says so outright: this VM keeps a Properties'
entries in an identity-keyed side-table, and the inherited
`ConcurrentHashMap map` backing "is deliberately never populated" — which is
precisely why every read and write method of `Properties` is natively
overridden there.

`clone` and `replaceAll` are two methods that never got that treatment, and
both dereference `map`. Nothing else does. A differential probe of 27
operations across three receiver shapes, diffed against HotSpot, failed on
exactly those two:

```
FAIL fresh.clone        -> NullPointerException
FAIL fresh.replaceAll   -> NullPointerException: Cannot invoke
                           "ConcurrentHashMap.replaceAll(BiFunction)"
                           because "this.map" is null
FAIL sysprops.clone     -> NullPointerException
FAIL sysprops.replaceAll-> (same)
TOTAL FAILS=4          (HotSpot: 0)
```

The second message names the cause outright, and the shape of the failure set
explains why this was not caught sooner:

| receiver | `map` on CratonVM | `map` on HotSpot |
|---|---|---|
| `new Properties()` | **null** | `ConcurrentHashMap(size=0)` |
| after `setProperty(k,v)` | `ConcurrentHashMap(size=1)` | `ConcurrentHashMap(size=1)` |
| `new Properties(defaults)` | **null** | `ConcurrentHashMap(size=0)` |
| `System.getProperties()` | **null** (`size()` = 54) | `ConcurrentHashMap(size=48)` |

Only a Properties that has never been **written through** has a null `map` —
the write paths lazily create the CHM. So a populated Properties cloned fine,
which is why the bug looked exotic; and `System.getProperties()`, which the VM
synthesises and never writes through, never gets one at all. Testcontainers
clones exactly that object.

## Fix

`native_properties_clone` does what the real body does, safely. It reaches
`Object.clone` for the shallow copy and the side-table replication (which
`native_object_clone` already knew to do for a Properties), then rebuilds the
clone's **own** CHM. The second half is not optional: skipping it would leave
the shallow copy's `map` aliasing the receiver's, so a write through the clone
would land in the original — the independence `clone` exists to provide.

`native_properties_replace_all` routes each replacement through
`native_properties_put` rather than writing the store itself, so it inherits
that path's three obligations — the side-table write, the mirror into the `map`
CHM that generic `Map` walkers read, and propagation to the VM's
system-property store when the receiver is the `System.getProperties()` view —
instead of re-deriving them. Re-deriving any of those is how the
side-table-vs-backing asymmetry that `native_properties_clear` documents gets
reintroduced.

Both are registered in `register_properties_sidetable` with companion entries
in `vm_exec.rs`'s force-native list, exactly as the nine Properties methods
already there.

### What was deliberately NOT done

**`map` was not made non-null.** It is the obvious one-line fix and it is
wrong twice over. An empty CHM would make `replaceAll` *succeed while replacing
nothing* on a receiver holding 54 system properties — trading a loud NPE for a
silent wrong answer. And it would leave every other un-overridden real body
failing quietly instead of loudly. Loudly is how this was found.

## Verification

* the probe's 4 failures → **0**; `PropsClone`'s eight cases are output-identical
  to HotSpot's but for VM identity (54 properties vs 48, `java.version`)
* the exact call the trace names — `DefaultDockerClientConfig
  .createDefaultConfigBuilder()`, and the `ServiceLoader` construction of
  `TestcontainersHostPropertyClientProviderStrategy` — succeeds, matching HotSpot
* `org.hibernate.reactive.HQLQueryTest`: **`ok=9 failed=0`** through a live
  Postgres container, zero NPEs in the log
* `regression-suite/run.sh` **75/75**

### Is there a third?

No. Brace-matched over `Properties.java`, **29** methods have a body that
touches the `map` field. **27** are natively overridden in
`register_properties_sidetable` / `phases_early`. The two that are not are
`toString` and `hashCode` — both `return map.something()` — and both are
nonetheless correct on every receiver shape, including a fresh
`new Properties()` whose `map` is null. That is unexplained rather than
designed, so it is written down here: if either ever starts throwing the
null-`map` NPE, it is the same defect and the same one-line cure, and the
enumeration above is where to start.

`RPropertiesClone` asserts CONTENTS and independence in BOTH directions across
all three receiver shapes. That is deliberate: a clone that aliased the
original's backing, or that came back empty, passes any "did it throw" check,
and both were live failure modes here — the shallow copy aliases `map` unless
the override replaces it, and the side-table is keyed by object identity, so a
clone starts with an empty one unless the override replicates it.

## Was it a regression? Yes — UNCOVERED, not caused, by a correctness fix

The null `map` is long-standing. What changed is what `new ConcurrentHashMap<>
(null)` does, and it changed 12 hours before the report.

`Properties.clone()`'s second statement is `clone.map = new ConcurrentHashMap<>
(map)`. `ConcurrentHashMap(Map m)` is `this.sizeCtl = DEFAULT_CAPACITY;
putAll(m);`, and CratonVM serves that constructor with
`native_chm_init_from_map`. Until 2026-08-28 that body **answered an empty map
for a null source** instead of throwing. Its own comment now records what it
used to do:

> `ConcurrentHashMap(Map m)` … `putAll` opens by calling `m.size()`, so a null
> source is an NPE BEFORE the map is usable. **This body answered an empty map
> instead** — the shape `phase-2-worklist` records as the worst a refusal can
> take, because the caller does not learn it passed null until much later.

So `clone.map = new ConcurrentHashMap<>(null)` quietly produced an empty CHM,
`clone()` returned normally, and the clone was even CORRECT — every Properties
reader in this VM is native and reads the side-table, which `Object.clone`
replicates. The defect was fully masked.

`c8f47f9a5` (**L6 concurrency lane, 2026-08-28 23:36 UTC** — "508 differential
rows, 30 defects, all four families 0-diff") added the null rejection, correctly
and to match HotSpot. The next morning `Properties.clone()` began throwing, and
the report was filed.

Three checks pin it:

* a binary that predates `c8f47f9a5` answers `new ConcurrentHashMap<>(null)`
  with an empty map, `new HashMap<>(null)` likewise, and `chm.putAll(null)`
  without throwing — where HotSpot raises NPE for all three — and on that same
  binary all eight `clone` shapes pass, including the two that fail on `dev`.
* `replaceAll` fails on BOTH sides, before and after. It is
  `map.replaceAll(function)` — a direct null-receiver dereference with no
  constructor argument for anything to be lenient about. That asymmetry is the
  tell, and it is what rules out any theory based on `clone` being dispatched
  differently.
* the traced statement is the one the report's trace names.
  `Properties.java:1526` IS `clone.map = new ConcurrentHashMap<>(map)`; the NPE
  has no `ConcurrentHashMap` frame above it because the constructor is served by
  a native.

**`c8f47f9a5` was right to land.** It replaced a silent wrong answer with the
JDK's own behaviour. It is named here as the uncovering change, not as a
mistake — the defect it exposed is the one this page is about, and it had been
latent for as long as the synthetic Properties has existed.

A caution for whoever reads this next: do NOT date a binary by its worktree's
HEAD. The one used above had moved since it was built, and an earlier attempt to
date it by two unrelated L3-lane markers pointed at the wrong lane entirely. The
only marker that settles it is the presence of the behaviour under test —
here, whether `new ConcurrentHashMap<>(null)` throws.

None of this changes the fix. An explicit registration does not read `map` at
all, so it is immune both to the null and to whatever the CHM constructor does
with it. `RPropertiesClone` is what makes the question moot going forward.

## Repro (pre-fix)

```bash
cd apps/hibernate-reactive-suite-runner
cratonvm --java-home <jdk25> --Xmx 2g @common.args \
  CratonRunner org.hibernate.reactive.HQLQueryTest
```

The one-line standalone form, which needs nothing but a JDK:

```java
System.getProperties().clone();   // or: new Properties().clone();
```
