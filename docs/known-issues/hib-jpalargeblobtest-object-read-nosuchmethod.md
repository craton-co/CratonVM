# Hibernate `JpaLargeBlobTest` — `read()` dispatch resolves to `java.lang.Object`, not the Blob stream's real class

| | |
|---|---|
| **Status** | 🔴 OPEN — not yet root-caused. Confirmed CratonVM-specific (HotSpot passes 1/1). |
| **Area** | VM — virtual/interface method dispatch for `InputStream.read()` on a JDBC `Blob`'s binary stream |
| **Symptom** | `java.lang.NoSuchMethodError: java/lang/Object.read()I` |
| **Severity** | medium — single class, but the failure mode (dispatch landing on `Object`'s non-existent method) suggests a general vtable/interface-dispatch defect that could recur elsewhere. |
| **Discovered** | 2026-07-05, Hibernate 121-class pruned non-passed triage (Azure host, dev `49aaf713`, real-JDK, JIT on, `TIMEOUT=1200`). |

## Symptom

`org.hibernate.orm.test.lob.JpaLargeBlobTest` builds a `LobEntity` with a
`byte[]`-backed `blob` field, persists it (insert succeeds — see the SQL log
below), then reads it back and calls into the JDBC `Blob`'s binary stream.
That call fails hard:

```
Hibernate:
    insert
    into
        LobEntity
        (blob, id)
    values
        (?, ?)
@@FAIL org.hibernate.orm.test.lob.JpaLargeBlobTest :: java.lang.NoSuchMethodError: java/lang/Object.read()I
```

`java.lang.Object` has no `read()` method at all — this is not "the wrong
overload was picked," it's the method resolver falling through the entire
class hierarchy and landing on `Object`'s (non-existent) vtable slot instead
of raising `AbstractMethodError`/finding the real implementation. HotSpot
passes this test cleanly (`found=1 ok=1 failed=0`, Azure HotSpot baseline),
confirming this is CratonVM-specific.

## What's ruled out

This is a different call site/object hierarchy from the `bytecode.enhance*`
package's `NoSuchMethodError: java/lang/Object.X` failures documented in
[hib-bytecode-enhancement-loader-faithful-linking.md](hib-bytecode-enhancement-loader-faithful-linking.md)
— that doc's root causes are all specific to ByteBuddy's per-package
`EnhancingClassLoader` and the loader-faithful supertype-linking gate.
`org.hibernate.orm.test.lob` uses no bytecode enhancement and no custom
classloader; the receiver here is whatever `InputStream` implementation H2's
JDBC driver returns from `Blob.getBinaryStream()` (or the object Hibernate's
BLOB-reading utility wraps it in). The shared symptom (`Object.<method>`
NoSuchMethodError as a generic "dispatch gave up" fallback) recurs across
several unrelated docs in this repo
([fork6-fjp-multithread-jit-root-reclamation.md](fork6-fjp-multithread-jit-root-reclamation.md),
[hib-temporal-gc-lambda-native-stale-local.md](hib-temporal-gc-lambda-native-stale-local.md)) —
worth keeping in mind if a common dispatch-fallback bug is ever found, but
each occurrence so far has had a distinct, unrelated root cause on inspection.

## Repro

Azure host, harness at `/home/victor/hibpkg/runner`:
```
CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 CRATONVM_JIT_OSR=1 \
  <cv-binary> --java-home /home/victor/jdk25 --Xmx 1500m @common.linux.args \
  -Dcraton.batch=1 CratonRunner <(echo org.hibernate.orm.test.lob.JpaLargeBlobTest) 0
```

## Next steps (not yet done)

- Identify the concrete runtime class of the `Blob`'s binary stream (H2's
  `org.h2.jdbc.JdbcBlob` wraps a memory- or file-backed stream — likely
  `org.h2.value.ValueLob`'s internal stream class, or an H2-internal
  `RangeInputStream`/`BufferedInputStream` chain).
- Instrument the invoke-site to see what CP-resolved owner/descriptor the
  interpreter/JIT used for the `.read()` call, and what vtable index it
  computed vs. what `Object`'s (empty) method table actually contains at
  that index — this smells like an interface-dispatch (`invokeinterface`)
  vtable-index bug rather than a name/loader-resolution bug, given the
  target is a completely unrelated class (`Object`) rather than a stale or
  wrong-loader copy of the right class.
- Check whether `--nojit` avoids it (would confirm/rule out a JIT-specific
  interface-dispatch codegen bug, matching the session's established
  pattern of JIT-only dispatch defects).
