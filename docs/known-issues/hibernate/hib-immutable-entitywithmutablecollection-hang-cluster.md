# Hibernate `immutable.entitywithmutablecollection.*` — 17/17 classes HANG

| | |
|---|---|
| **Status** | 🔴 OPEN — every class in this test family hangs (`rc=124`, `TIMEOUT=300`). HotSpot confirmation pending. |
| **Discovered** | 2026-07-11 full 4548-class suite audit, `dev` post `44f16ee2`+. |
| **Area** | Immutable entities that own a mutable collection — inverse and non-inverse many-to-many/one-to-many association management. |

## Symptom

Every class under `org.hibernate.orm.test.immutable.entitywithmutablecollection.{inverse,noninverse}.*`
hangs — 17/17, no exceptions, `process-died rc=124`. Plus `ImmutableTest`
itself (the parent package's own top-level test) also hangs, and
`AbstractEntityWithManyToManyTest`/`AbstractEntityWithOneToManyTest` (the
shared base classes) correctly show `NOTESTS` (abstract, expected).

## Affected classes (17 + `ImmutableTest`)

```
immutable.ImmutableTest
immutable.entitywithmutablecollection.inverse.EntityWithInverseManyToManyTest
immutable.entitywithmutablecollection.inverse.EntityWithInverseOneToManyTest
immutable.entitywithmutablecollection.inverse.EntityWithInverseOneToManyJoinTest
immutable.entitywithmutablecollection.inverse.VersionedEntityWithInverseManyToManyTest
immutable.entitywithmutablecollection.inverse.VersionedEntityWithInverseOneToManyTest
immutable.entitywithmutablecollection.inverse.VersionedEntityWithInverseOneToManyJoinTest
immutable.entitywithmutablecollection.inverse.VersionedEntityWithInverseOneToManyFailureExpectedTest
immutable.entitywithmutablecollection.inverse.VersionedEntityWithInverseOneToManyJoinFailureExpectedTest
immutable.entitywithmutablecollection.noninverse.EntityWithNonInverseManyToManyTest
immutable.entitywithmutablecollection.noninverse.EntityWithNonInverseManyToManyUnidirTest
immutable.entitywithmutablecollection.noninverse.EntityWithNonInverseOneToManyTest
immutable.entitywithmutablecollection.noninverse.EntityWithNonInverseOneToManyJoinTest
immutable.entitywithmutablecollection.noninverse.EntityWithNonInverseOneToManyUnidirTest
immutable.entitywithmutablecollection.noninverse.VersionedEntityWithNonInverseManyToManyTest
immutable.entitywithmutablecollection.noninverse.VersionedEntityWithNonInverseOneToManyTest
immutable.entitywithmutablecollection.noninverse.VersionedEntityWithNonInverseOneToManyJoinTest
```

## Root-cause hypothesis (not yet confirmed)

Every combinatoric variant (inverse/non-inverse × many-to-many/one-to-many ×
join-table/no-join-table × versioned/non-versioned) of "immutable entity
owns a mutable collection" hangs uniformly — the 100% hit rate across every
variant strongly suggests the bug is in something the whole family shares
structurally (e.g. Hibernate's `@Immutable`-entity + mutable-collection
interceptor/dirty-checking setup, or the shared test fixture/base class's
`setUp` establishing that combination), rather than any per-variant logic.
Plausible candidates: an infinite loop in collection-dirtiness tracking for
a collection attached to an entity Hibernate treats as immutable (the
combination is somewhat contradictory — the entity can't be dirty-checked
normally, but its collection can — so Hibernate has special-case handling
here that CratonVM may not replicate correctly), or a deadlock/livelock in
whatever locking these tests' shared fixture uses.

## Repro

Azure host harness:
```bash
echo org.hibernate.orm.test.immutable.entitywithmutablecollection.inverse.EntityWithInverseManyToManyTest > /tmp/one.txt
timeout 300 <cratonvm> --java-home <jdk25> --Xmx 1500m @common.linux.args -Dcraton.batch=1 CratonRunner /tmp/one.txt 0
```

## Next steps (not yet done)

- Attach to a live hung process to identify the loop/lock, same approach as
  [hib-cascade-multipathcircle-hang-cluster.md](hib-cascade-multipathcircle-hang-cluster.md)
  (pick the simplest variant, `EntityWithInverseManyToManyTest`, for the
  first repro).
- Check whether `immutable.ImmutableTest` itself (not part of the
  `entitywithmutablecollection` sub-package but hanging too) shares the
  exact same mechanism, or is a coincidentally-separate hang — it tests
  plain `@Immutable` entities without the mutable-collection wrinkle, so if
  it hangs the same way, the bug may be broader (immutable-entity handling
  generally) rather than specific to the mutable-collection interaction.
- Confirm CratonVM-specificity with a HotSpot run (pending) — the 100%
  consistency across every combinatoric variant makes host-load-driven
  false-positive hangs unlikely (a load artifact would more plausibly hit
  some variants and not others), so this is a strong CratonVM-specific
  candidate.
