# Bug 02 — `ZipFile.entries()` returns `null` (and getName/size/getEntry fail)

**Severity:** High — **CratonVM-only**. Every method on a plain
`java.util.zip.ZipFile` fails: `entries()` → `null`, `getName()` → `null`,
`size()` → `0`, `getEntry()` → `null`. HotSpot works. `java.util.jar.JarFile`
(which extends `ZipFile`) is unaffected. Boot JDK 25.

**Status: FIXED** (worktree `test/wildfly-suite`, `native-io/src/zip_real_jar.rs`).
Verified by standalone repro + the WildFly live-container client path.

## Why it matters (how it was found)
Running the WildFly Arquillian **client under CratonVM** (Gap B in
[live-container-smoke.md](live-container-smoke.md)) failed with
`NullPointerException: Cannot invoke "java.util.Enumeration.hasMoreElements()"
because "<local>" is null` deep in ShrinkWrap's `URLPackageScanner.handleArchiveByFile`
— it opens a package's jar and iterates `ZipFile.entries()`, which CratonVM
returned as `null`.

## Minimal standalone repro
[`JarEntriesRepro.java`](../../wildfly-suite/repro/JarEntriesRepro.java) /
`ZipProbe.java` on any jar:
```java
ZipFile zf = new ZipFile(jarPath);
zf.getName();   // null (HotSpot: the path)
zf.size();      // 0    (HotSpot: 19)
zf.entries();   // null (HotSpot: Enumeration)  <- ShrinkWrap NPEs here
```

| VM | `ZipFile.entries()` | `JarFile.entries()` |
|----|---------------------|---------------------|
| HotSpot 25 | ok | ok |
| CratonVM (before) | **null** | ok |
| CratonVM (after fix) | ok | ok |

## Root cause (confirmed by instrumentation)
All `ZipFile`/`JarFile` natives find their Rust-side archive state via
`get_jar_handle(this)`, which reads an `i64` handle stored on the Java object in
`<init>` (`set_jar_handle` writes the `jzfile` field by name and slot 1).

`JarFile` is a CratonVM-synthetic class whose object carries a usable handle
slot, so this round-trips. A plain `java.util.zip.ZipFile` object, however, is
allocated with **no writable handle slot** — instrumentation showed that
immediately after `set_jar_handle` in `<init>`, *every* field reads back
`Object(None)` (the `set_field`/`set_field_by_name` calls silently no-op).
So `get_jar_handle` returns `0`, no `JarState` is found, and every native bails:
`entries()` returns `null` (its "handle not in table" branch), `getName()`/`size()`
return empty, etc. Modern JDK (9+) `ZipFile` no longer even has the `jzfile`
field the old code targeted, compounding the miss.

(The object identity was confirmed identical between `<init>` and `entries()` —
ptr `0x…710` in both — so this is a field-storage failure, not a wrong-object bug.)

## Fix
`native-io/src/zip_real_jar.rs`: since the handle can't live on the object, key a
side table on the object's **identity hash** (`System.identityHashCode`, stable
across GC):
- `open_and_register` records `identity_hash(this) → handle`.
- `get_jar_handle` keeps the fast on-object field path (JarFile), then falls back
  to the identity table (plain ZipFile).
- `close` removes the identity entry.

This is self-contained in the jar/zip natives and leaves the JarFile fast path
untouched (no per-call `identityHashCode` for JarFile).

## Verification
- `ZipProbe`: `getName`→path, `size`→19, `getEntry`→`../../../../apps/META-INF/MANIFEST.MF`,
  `entries`→ok. `JarEntriesRepro`: both `JarFile.entries()` and
  `ZipFile.entries()` ok — **no JarFile regression**.
- WildFly cratonvm-client (remote container): the ShrinkWrap
  `URLPackageScanner` NPE is gone; the client now proceeds past package scanning
  into deployment (further CratonVM/Arquillian-client behaviour is a separate
  matter — this bug is the `ZipFile.entries()` null).

## Follow-up
The deeper issue — a real-JDK `ZipFile` object having no writable native-handle
slot under CratonVM — likely affects any native that stashes a handle on a
real-JDK (non-synthetic) object. The identity-table pattern here is a targeted
work-around; a general fix would ensure such objects get a usable hidden slot.
