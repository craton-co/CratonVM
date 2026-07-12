# Hibernate circular-cascade tests — `MultiPathCircleCascade*` family all HANG

| | |
|---|---|
| **Status** | 🔴 OPEN — 12/12 classes in the family HANG (`rc=124`, `TIMEOUT=300`). HotSpot confirmation pending. |
| **Discovered** | 2026-07-11 full 4548-class suite audit, `dev` post `44f16ee2`+. |
| **Area** | Hibernate cascade save/delete graph traversal with cycles (`org.hibernate.orm.test.cascade.circle.*` and its bytecode-enhanced twin package). |

## Symptom

Every single class in the `MultiPathCircleCascade*` family hangs — 12/12,
no exceptions:
```
process-died rc=124   (TIMEOUT=300 exceeded)
```

## Affected classes (12 — 6 tests × 2 packages: plain + bytecode-enhanced)

```
cascade.circle.MultiPathCircleCascadeTest
cascade.circle.MultiPathCircleCascadeDelayedInsertTest
cascade.circle.MultiPathCircleCascadeCheckNullFalseDelayedInsertTest
cascade.circle.MultiPathCircleCascadeCheckNullTrueDelayedInsertTest
cascade.circle.MultiPathCircleCascadeCheckNullibilityFalseTest
cascade.circle.MultiPathCircleCascadeCheckNullibilityTrueTest
bytecode.enhancement.cascade.circle.MultiPathCircleCascadeTest
bytecode.enhancement.cascade.circle.MultiPathCircleCascadeDelayedInsertTest
bytecode.enhancement.cascade.circle.MultiPathCircleCascadeCheckNullFalseDelayedInsertTest
bytecode.enhancement.cascade.circle.MultiPathCircleCascadeCheckNullTrueDelayedInsertTest
bytecode.enhancement.cascade.circle.MultiPathCircleCascadeCheckNullibilityFalseTest
bytecode.enhancement.cascade.circle.MultiPathCircleCascadeCheckNullibilityTrueTest
```

Also `bytecode.enhancement.cascade.circle.AbstractMultiPathCircleCascadeTest`
shows as `NOTESTS` (abstract base, expected, matches the pattern in
[hib-notests-abstract-baseclass-list.md](hib-notests-abstract-baseclass-list.md)).

## Root-cause hypothesis (not yet confirmed)

100% hang rate across every variant of this specific test family (both
plain and enhanced) strongly suggests an infinite loop in CratonVM's
handling of Hibernate's cascade-cycle-detection logic — these tests are
*specifically designed* to exercise entity graphs with cycles (A→B→C→A) to
verify Hibernate's cascade engine correctly detects and breaks the cycle
during save/delete cascading, using an internal "already processed" tracking
set/map. If that tracking mechanism relies on `hashCode()`/`equals()` or
identity-map semantics that CratonVM implements subtly differently (e.g. an
`IdentityHashMap`-like structure, or `Set` membership checks against
entities whose `hashCode()` involves enhanced/proxied state), the cascade
walker could fail to recognize "already visited" and loop forever re-walking
the same cycle.

## Repro

Azure host harness:
```bash
echo org.hibernate.orm.test.cascade.circle.MultiPathCircleCascadeTest > /tmp/one.txt
timeout 300 <cratonvm> --java-home <jdk25> --Xmx 1500m @common.linux.args -Dcraton.batch=1 CratonRunner /tmp/one.txt 0
```

## Next steps (not yet done)

- Attach to a live hung process (`gdb -p <pid>`, or a thread-dump-style
  facility if CratonVM has one) to see exactly which loop it's stuck in —
  the 100% consistency across 12 classes makes this an easy, cheap repro to
  debug interactively (any one of the 12 will do, pick the plain
  `MultiPathCircleCascadeTest` for simplicity).
- Once the loop is identified, check whether it's in Hibernate's own cascade
  engine (real bytecode) calling into a CratonVM collection/map native whose
  behavior differs from HotSpot for the specific `Set`/`Map` type used to
  track visited entities during cascade.
- Confirm CratonVM-specificity with a HotSpot run (pending) — given the
  100% consistency and the "infinite loop in cycle detection" hypothesis,
  this is a strong CratonVM-specific candidate, but not yet proven.
