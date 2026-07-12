# FIXED: Hibernate `MultiPathCircleCascade*` family hang and follow-on failures

| | |
|---|---|
| **Status** | FIXED on 2026-07-12. All 12 concrete classes pass on the optimized CratonVM runtime. |
| **Affected area** | Hibernate circular cascade tests in the plain and bytecode-enhanced `cascade.circle` packages. |

## Root cause and repair

The original apparent cascade-cycle hang occurred before Hibernate's cascade
walker. `Class$Atomic.casAnnotationData` updated only CratonVM's GC-safe
atomic side table. `Class.annotationData()` reads the mirror's
`annotationData` field directly, so it continued to see `null`, rebuilt the
same annotation graph, and retried forever. A successful annotation-data CAS
now also updates that direct field, matching the reflection-data CAS behavior.

Once the hang was removed, two independent correctness residuals surfaced:

- Hibernate's JAXB mapping initialization produced a self-cast `QName cannot
  be cast to QName` only with JIT enabled. Package bisection showed that
  keeping `org.glassfish.jaxb` interpreted matches the no-JIT result. The VM
  skip list and final JIT gate now apply the same narrow, explicitly liftable
  package guard.
- Hibernate statistics use the real JDK `LongAdder`, which inherits
  `Striped64`; its first instance field is not a numeric base. The native
  bridge had treated heap field zero as a `long`, causing statistics counters
  to remain zero. The bridge now keeps all LongAdder contributions in its
  GC-stable identity-keyed stripe table and reads/resets that table consistently.

## Validation

Built the task-specific release binary and ran each concrete class through
`apps/hib-suite-runner/CratonRunner` with JDK 25, `--Xmx 1500m`, and the
normal optimized runtime. Every run completed with `failed=0`:

| Family | Classes | Result per class |
|---|---:|---|
| `cascade.circle.MultiPathCircleCascade*` | 6 | `found=9 started=9 ok=9` |
| `bytecode.enhancement.cascade.circle.MultiPathCircleCascade*` | 6 | `found=18 started=18 ok=18` |

The abstract enhanced base remains intentionally untested; it contains no
concrete JUnit tests and was not part of the 12-class failure set.

The JIT package guards have dedicated slash- and dot-name regression tests in
both the VM enqueue-side skip list and the final compiler gate.
