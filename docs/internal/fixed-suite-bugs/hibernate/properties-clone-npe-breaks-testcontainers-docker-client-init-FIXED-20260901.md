# `java.util.Properties.clone()` NPE — FIXED

Retires `known-issues/hibernate/properties-clone-npe-breaks-testcontainers-docker-client-init-20260829.md`.

## Status

**FIXED by `5a6348d28` (2026-08-29), verified on dev 2026-09-01.** The page was
never retired when the fix landed, so it stayed open for three days describing a
defect that no longer existed. Its three "not yet done" items are all answered
below.

## What it was

`Properties.clone()` NPE'd at `Properties.java:1526`, which is the second of the
real method's two statements:

```java
Properties clone = (Properties) cloneHashtable();   // Object.clone()
clone.map = new ConcurrentHashMap<>(map);           // <- here
```

`map` is null, **and that is by design**. This VM keeps a `Properties`' entries
in an identity-keyed side table, and `register_properties_sidetable`'s own note
says the inherited CHM backing "is deliberately never populated" — which is why
every read and write method of `Properties` is natively overridden there.
`clone` and `replaceAll` were two that never were, and both dereference `map`.

The fix overrides both: `native_properties_clone` reaches `Object.clone` for the
shallow copy and the side-table replication, then rebuilds the clone's **own**
CHM — not optional, because skipping it leaves the shallow copy's `map`
ALIASING the receiver's, so a write through the clone would land in the
original. `native_properties_replace_all` routes each replacement through
`native_properties_put` so it inherits that path's three obligations rather than
re-deriving them.

`6ecaaa54e` then fixed the sibling the same area produced — a clone enumerating
in a different order than its source.

## The page's three open items, answered

**1. "Which field is null that HotSpot's isn't."** `map`, and it is null on
purpose. The page's own hypothesis — the primitive/reference slot-coercion
family (`gc::guard` W7-84/G30), offered as "a plausible starting hypothesis, not
a confirmed cause" — was **wrong**, and correctly hedged. Nothing was coerced;
a field the design leaves null was dereferenced by two methods that forgot to be
overridden.

**2. "A minimal standalone repro … whether this needs a populated/large
properties table, specific key/value types, or reproduces on an empty one."**
None of those. **The discriminator is whether the receiver was ever WRITTEN
through.** The write paths lazily create the CHM, so a populated `Properties`
cloned fine and hid the bug; a fresh `new Properties()` and
`System.getProperties()` both failed. That is why the obvious repro passes and
why the differential had to be built: 27 operations across three receiver
shapes, diffed against HotSpot, failed on exactly two.

`probes/PropertiesCloneNpe.java` is that minimal repro, added here. It prints
values rather than asserting, so it diffs against real HotSpot.

**3. "Whether this is a regression."** **It is not.** The side table and its
deliberately-null `map` date to the initial open-source commit, `a6dc911ed`
(2026-04-26), so `clone`/`replaceAll` were broken for four months. The page
guessed as much — "that batch may simply not have exercised any class needing
Testcontainers' Docker-client init path yet" — and the guess was right.

## Verified

`probes/PropertiesCloneNpe.java`, CratonVM on dev `aa0c1fadf` against real
HotSpot 25.0.3+9, same host:

| row | HotSpot | CratonVM |
|---|---|---|
| `System.getProperties().clone()` — the Testcontainers shape | ok | ok |
| clone carries the entries | 53 vs 53 | 54 vs 54 |
| clone has `java.version` | true | true |
| write to clone leaks to source | false | false |
| `new Properties().clone()` — never written | ok | ok |
| `new Properties()` + put, then `clone()` | ok | ok |
| `.replaceAll` on a system-properties copy | ok | ok |
| `.replaceAll` on a written `Properties` | ok | ok |
| clone order equals source order | true | true |

Zero throws. The 53-vs-54 is the two VMs carrying a different number of system
properties; each is self-consistent (`sys.size == clone.size` on both), which is
the property the row is actually checking.

The aliasing hazard the fix's second half exists for is checked directly: a
write through the clone does **not** appear in the source.

## Why it mattered

138 of 191 hibernate-reactive classes in one batch, all with the same root
cause: Testcontainers' `DefaultDockerClientConfig.createDefaultConfigBuilder`
clones `System.getProperties()` on every Docker-client provider-strategy
attempt, `ServiceLoader` rewrapped the NPE as a `ServiceConfigurationError`, and
what should have been a graceful failover to the next strategy became a hard
failure.

Nothing about it was Testcontainers-specific — snapshotting config by cloning a
`Properties` is an ordinary pattern, and the two receivers that failed
(`new Properties()` and `System.getProperties()`) are the two most common ones.

## Related

* `5a6348d28` — the fix.
* `6ecaaa54e` — the clone-enumeration-order sibling, and its record
  `system-properties-clone-enumerates-in-a-different-order-than-its-source-20260829.md`.
* `native-builtins/src/properties_sidetable.rs` — the design note that names the
  rule ("every method must be overridden") which `clone`/`replaceAll` fell
  outside of. **A design note stating a rule is not a list of the methods that
  satisfy it**; the way to find the gaps is to enumerate the surface and diff it
  against HotSpot, which is what found these two.
