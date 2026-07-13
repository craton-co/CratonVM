# Hibernate suite — miscellaneous singleton failures (fixed)

| | |
|---|---|
| **Status** | ✅ FIXED — all named issues and the directly related stored-procedure residual pass on CratonVM and HotSpot. |
| **Discovered** | 2026-07-11 full 4548-class suite audit, `dev` post `44f16ee2`+. |
| **Resolved** | 2026-07-13. |

This document originally grouped 16 low-frequency Hibernate failures. Closure
also included the directly related
`jpa.procedure.StoredProcedureResultSetMappingTest`, for a 17-class validation
set.

## Corrected baseline

The earlier attempted HotSpot confirmation was invalid: `HOTSPOT=1` changed
the argument shape but did not change the executable, so it still launched
CratonVM. The corrected run explicitly set
`CV=/home/victor/jdk25/bin/java` and passed all 17 classes:

```
/data/data/hib-misc-runner-20260712-2215/
  out-realhotspot-corrected-20260712-2215-20260712-225137/results.tsv
status: PASS=17
```

The live pre-fix CratonVM run split the catch-all accurately:

- 10 classes already passed and had no current residual: all XML, HQL,
  named-query, entity-graph, and expected-failure entries from the original
  note.
- 4 serialization classes failed with `EOFException`.
- 3 H2 stored-procedure/dynamic-compilation classes failed because their
  generated Java source could not compile or the resulting function was not
  registered.

The pre-fix evidence is:

```
/data/data/hib-misc-runner-20260712-2215/
  out-currentdev-corrected-20260712-2215-20260712-225137/results.tsv
status: PASS=10 FAIL=7
```

## Root causes and fixes

### Shared-stream `DataInputStream` prefetch

The native `DataInputStream` bridge prefetched up to 8192 bytes from its
wrapped stream and kept the unused bytes in a Rust-only side buffer.
`ObjectInputStream` shares its `BlockDataInputStream` with an internal
`DataInputStream`: a primitive read such as `readInt()` therefore consumed
the following block bytes, while subsequent direct `ObjectInputStream` reads
saw EOF.

`native-io/src/lib.rs` now reads exactly one byte for the one-byte helper and
removes the side-buffer ownership model. If a custom stream returns zero from
that non-empty bulk request, the bridge falls back to scalar `read()` instead
of reporting false EOF. This preserves the observable position of any Java
stream shared between multiple consumers and fixes:

- `EntityManagerFactorySerializationTest`
- `EntityManagerSerializationTest`
- `EntityManagerDeserializationTest`
- `EntityEntryTest`

### Wildcard publication in `java.class.path`

The class loader expanded `jars/*` internally, but the
`java.class.path` system property still contained the literal wildcard.
H2's in-process compiler consumes that property, so generated sources could
not import `org.h2.tools.*`.

`ClassPath::expand_classpath_entries` now provides the same deterministic,
non-recursive JAR expansion used by class loading, and `vm_init` publishes
those concrete entries in `java.class.path`.

### Truncated javac platform-package listing

After the application classpath became visible, javac advanced to a second
failure: direct lookup could open `java.lang.Byte.class`, but
`JavacFileManager.list` returned a hard-coded list of only 15
`java.lang` classes and omitted `Byte`. Javac never requested the class and
reported a `Symbol$CompletionFailure`.

The platform listing now comes from the selected JDK's actual jimage index for
the requested module and package, with recursive package traversal when javac
requests it. This removes the incomplete package allowlist and fixes:

- `SessionDelegatorBaseImplTest`
- `sql.storedproc.StoredProcedureResultSetMappingTest`
- `jpa.procedure.StoredProcedureResultSetMappingTest`

The SQL “function not found” symptom was downstream: H2 did not register the
function because its generated Java class had failed to compile.

## Regression coverage

- `class_path::tests::wildcard_publication_expands_to_concrete_sorted_jars`
  verifies that property-facing wildcard expansion yields concrete,
  deterministic JAR paths.
- `phases_late::jrtfs_javac_listing_tests::javac_platform_listing_uses_complete_jrt_package_inventory`
  verifies a non-truncated `java.lang` inventory containing `Object`,
  `Byte`, and `Integer`, plus recursive nested-package discovery.
- `vm/tests/native_io_dis_shared_stream.rs` writes an int and trailing
  booleans through `ObjectOutputStream`, then reads them through
  `ObjectInputStream`; it reproduces the former EOF boundary directly.
- The existing `native_io_dis_read_fully_pin` regression now also passes its
  zero-progress bulk-read case (it failed on the pre-fix `dev` binary).

All three focused regressions pass.

## Final validation

Unique validation binary:

```
/data/hibpkg/runner/cvhibmiscsingletons-fix8-20260713-0032
sha256 b424526f528e3fbb0e7c8f84125f4e727305bd75ef8f5714e50bf730de2628bd
```

Full CratonVM result:

```
/data/data/hib-misc-runner-20260712-2215/
  out-fix8-all17-final-20260713-0040-20260713-002246/results.tsv
status: PASS=17
```

Every class named by the original note passes. The additional JPA
stored-procedure variant also passes, and no residual remains in this issue
family.
