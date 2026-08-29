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

`RPropertiesClone` asserts CONTENTS and independence in BOTH directions across
all three receiver shapes. That is deliberate: a clone that aliased the
original's backing, or that came back empty, passes any "did it throw" check,
and both were live failure modes here — the shallow copy aliases `map` unless
the override replaces it, and the side-table is keyed by object identity, so a
clone starts with an empty one unless the override replicates it.

## The one question left open, and why it does not matter

The report asked whether this was a regression. A binary from another worktree
built earlier the same day did **not** reproduce it — same null `map`, yet all
eight clone cases passed — which points at a change in how `Properties.clone()`
resolves rather than at the null `map` itself. That binary's source commit
could not be confirmed (its worktree HEAD had moved since the build), and
neither its HEAD nor the dev tip has ever carried a `clone` override, so the
window was not pinned and no claim is made here.

It does not matter to the fix: an explicit registration is immune to whichever
routing decision let the real body run. It would matter to anyone who sees this
signature reappear on an OLDER commit — which is what `RPropertiesClone` is for.

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
