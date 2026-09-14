# W8-C14-3 — `nio_file.rs` split-singleton audit, and one classification corrected

> **STATUS: AUDIT + NOMINATIONS.** No fix in this record; the two applied fixes
> are W8-C14-1 and W8-C14-2. Everything here is either measured on HotSpot,
> read out of the source, or explicitly marked as inference.

Lane C14, 2026-08-12. Oracle: Microsoft OpenJDK `25.0.3+9`, windows/x64.

---

## 1. The wrong-typed field read is NOT VM-FATAL — correcting the brief

This lane was asked to check whether `p57_read_path` reading `P57_PATH_FIELD`
(= 0) on a real `WindowsPath` — whose field 0 is a `FileSystem` object, not a
`String` — is a memory-safety-adjacent bug deserving a **VM-FATAL**
classification. **It is not, and the reason is worth recording**, because the
prior on this codebase ("an out-of-bounds field read on a real JDK class is a
real bug") points the other way and would have been the easy answer.

Three independent guards make the read safe, all of them deliberate and each
carrying a comment naming the incident that added it:

1. **In bounds.** A real `WindowsPath` has 7 instance fields, so index 0 is a
   legitimate slot. There is no OOB read here at all.
2. **`read_string` guards by class identity *before* the structural reader**
   (`vm/src/vm/vm_exec.rs`): if the receiver's class resolves and is not
   `java/lang/String`, it returns `None` immediately. A `WindowsFileSystem`
   resolves, so this is where the refusal happens.
3. **Even the fallback structural reader is guarded.**
   `java_string_value_and_coder` (`vm/src/vm/vm_object.rs`) — reached only when
   the class is genuinely unresolvable, i.e. early bootstrap — requires
   `num_fields >= 2`, field 0 to be an `Object`, that object to be
   `ObjectKind::Array`, `coder ∈ {0,1}`, and `num_fields >= 4`. A
   `WindowsFileSystem`'s field 0 is a `WindowsFileSystemProvider` **object**,
   which fails the array check. Regression tests
   `read_string_non_string_object` and
   `read_string_returns_none_for_reference_array_field` pin exactly this.

So the sequence is: correct refusal → documented `toString()` fallback →
re-entrancy → the `IN_TOSTRING_FALLBACK` guard returning `""`. Every step is a
guard behaving as designed. The failure is a **wrong answer** (an empty path,
surfacing as an empty-message `NoSuchFileException`), reachable as a normal
Java throwable, catchable, and not a Rust panic.

**Classification: wrong answer, high blast radius. Not VM-FATAL.** Recording
the negative explicitly so the next lane does not re-open it.

The genuine OOB hazard in this area was one the *fix* would have introduced,
not one that existed: caching a provider in slot 3 of a receiver that might be
a 3-field real `WindowsFileSystem`. See W8-C14-2 §4.1 for the width guard that
closes it.

## 2. The audit — split singleton pairs in `nio_file.rs`

The shape: CratonVM natively intercepts one method that hands out an object
Java guarantees is a singleton, while a sibling reaching the same conceptual
object is not intercepted (or is intercepted by a helper that allocates
separately). Under `--jdk-only` real bytecode runs for the un-intercepted
sibling and a second object appears where the API contract guarantees one.

### Fixed by this lane

| # | Site | Was | Now |
|---|---|---|---|
| 1 | `sun/nio/fs/DefaultFileSystemProvider.theFileSystem` | **unregistered** | singleton (W8-C14-1) |
| 2 | `FileSystemProvider.getFileSystem(URI)`, `file` scheme | `p57_alloc_default_filesystem` (fresh) | singleton (W8-C14-1) |
| 3 | `FileSystem.provider()` | fresh per call | cached per FS (W8-C14-2) |
| 4 | `FileSystemProvider.installedProviders()` element 0 | fresh per call | singleton (W8-C14-2) |
| 5 | `sun/nio/fs/DefaultFileSystemProvider.instance` | **unregistered** | singleton (W8-C14-2) |

### Open — nominated, not taken

| # | Site | Contract broken |
|---|---|---|
| 6 | jar/zip mount has **no registry**; `FileSystemAlreadyExistsException` appears **nowhere in the repo** | second `newFileSystem` on the same jar must throw; `getFileSystem(jarURI)` must return the mounted one |
| 7 | `newFileSystem(URI,Map)` non-jar/jrt fallback returns a fresh default FS | a `file:` URI must throw `FileSystemAlreadyExistsException` |
| 8 | jar/jrt `Path` → `FileSystem` identity not stable; `P57_PATH_FS_FIELD` is dropped by `resolve`/`getParent`/`getRoot`/`getFileName`/`normalize`/`subpath` (kept only by `relativize`) | every `ZipPath` derived from a zipfs path returns the same `ZipFileSystem` |
| 9 | `FileVisitResult.values()` / `valueOf()` build fresh enum objects with no static-constant lookup | `values()[0] == FileVisitResult.CONTINUE`; `switch`, `EnumSet`, `EnumMap` |
| 10 | `StandardWatchEventKinds.ENTRY_CREATE` &co. registered as **field natives returning a fresh `java.lang.String`** | they are `WatchEvent.Kind` singletons; `native-io` deliberately stopped registering these so `event.kind() == ENTRY_CREATE` holds, and `nio_file.rs` still does |

Item 10 is the one to check first — it is cheap and it *contradicts an existing
fix*. `native-io/src/lib.rs` stopped registering these constants on purpose,
with a comment saying identity comparison against the real static must hold;
`nio_file.rs` still registers them, un-gated by `is_class_synthetic_stub`, and
returns a `String` rather than a `Kind`. **Confirm registration precedence
before acting**: if field-descriptor registrations are suppressed for
non-stub classes the way the `PosixFilePermission` `<clinit>` is gated, item 10
downgrades to CLEAR. The absence of any such gate at that site is what makes it
a suspected hit rather than a confirmed one.

### Checked and CLEARED — with the reason, not just the verdict

| Family | Why clear |
|---|---|
| `File.toPath()` / `Path.toFile()` | Java guarantees **no** identity in either direction; both are specified to return new objects, and both allocate fresh here. Matches HotSpot. |
| `Path.getFileSystem()` for default-FS paths | Correctly reaches `p57_default_filesystem_singleton`. `p57_alloc_path` leaving `P57_PATH_FS_FIELD` null is *right*: it makes plain `Paths.get` paths fall through to the singleton. |
| `FileStore` objects | HotSpot's `getFileStore` also constructs a new `WindowsFileStore` per call; `FileStore` has no identity contract. Both sides intercepted consistently. |
| `FileSystem.getRootDirectories()` | Returns fresh `Path`s, and `Path` identity is contracted nowhere. Confirmed by the `NEG.getPath==getPath|false` oracle row. |
| `FileSystem.newWatchService()` | Returns a new service per call on HotSpot. Deliberately unregistered here; owned by `native-io`. |
| `PosixFilePermission` constants | Gated on `is_class_synthetic_stub`, so under `--jdk-only` the real enum's `<clinit>` wins and consumers read the real static. **This is the reference implementation of the correct pattern** for items 9 and 10. |
| `jdk_concrete_getclass_alias` | Rewrites only the `getClass()` mirror. No registration, no allocation, no identity impact. It *concealed* this defect family; it does not cause it. |
| Rust-side `OnceLock`s in the file (`jar_bytes_cached`, `jar_index`, `jrt_image`, `file_canonicalize_path`, `DELETE_ON_EXIT`) | Data caches holding no Java object identity. No rooting or VM-scoping exposure. |

### UNCERTAIN

`FileSystem.getUserPrincipalLookupService()` — zero occurrences of
`getUserPrincipalLookupService` or `UserPrincipalLookupService` in any `.rs`.
HotSpot returns the same lookup service per FileSystem, so a split is possible,
but with *neither* half intercepted the likelier first failure is
`AbstractMethodError` on the synthetic receiver. Different defect family;
flagged for completeness, not nominated.

## 3. Mode analysis — this is a both-modes change

`native-api/src/registry.rs`: `Bridge` is permitted under both
`CompatibilityMode::Compatible` and `CompatibilityMode::JdkOnly`; only
`SyntheticStub` is rejected by strict mode. Since `FileSystems.getDefault()`
and every native touched here is a `Bridge` registered inside
`register_phase57_nio_file`, all of it is live in `--real-jdk` too — and the
**defect** was live there too, because door 1 was intercepted in that mode
while door 2 ran real bytecode.

Do not file either fix as strict-only. Concrete `--real-jdk` check worth
running first: whether `java.util.zip.ZipFile$Source` is reached at all, since
its `builtInFS` static is built from door 2 (W8-C14-1 §7).

## 4. Nominations (files this lane does not own)

**N1 — register `RFsSingleton` in the suite.** `regression-suite/run.sh`,
line 106. The fixture needs **no** JVM flags (that is why its rows are limited
to exported API), so this is a pure name addition.

* file: `regression-suite/run.sh`
* old (exact, end of the `CORE_CLASSES` value): `RJdkStringCodePoints"`
* new: `RJdkStringCodePoints RFsSingleton"`

Measured on HotSpot 25.0.3+9: `@@RESULT checks=12 fails=0`, exit 0. Mutation
control run (flip `prov.stable` and the `neg.pathsDistinct` expectation):
`@@RESULT checks=12 fails=2`, exit 1 — the fixture can go red.

**N2 — jar/zip mount registry + `FileSystemAlreadyExistsException`** (audit
items 6, 7, 8). `native-builtins/src/phases_late/nio_file.rs` is this lane's
file but this is a design change with real failure modes (mount lifetime,
`close()` semantics, keying by canonical path), not a mechanical repair, and it
wants its own measurement against JUnit5 `CloseablePath.create` and Jetty
`PathResourceFactory`. Not attempted here rather than half-attempted.

**N3 — confirm the `instance()` descriptor on non-Windows images.** Run
`javap -p --module java.base sun.nio.fs.DefaultFileSystemProvider` on the Linux
JDK and confirm `()Lsun/nio/fs/LinuxFileSystemProvider;`. Wrong ⇒ dead
registration (today's behaviour), not a crash — but it silently un-fixes the
Linux leg.

**N4 — resolve the `StandardWatchEventKinds` contradiction** (audit item 10)
against `native-io/src/lib.rs`'s deliberate non-registration. Needs a
registration-precedence check first; see §2.

## 5. Probes and artefacts

| Path | What it is |
|---|---|
| `scratchpad/c14/FsIdentity.java` | 28-row identity probe, 3 negative controls. Needs `--add-opens java.base/sun.nio.fs=ALL-UNNAMED` **on both arms**. |
| `scratchpad/c14/FsIdentityMutant.java` | Mutation control: builds a second provider reflectively, proves the load-bearing rows flip and that a `Path.equals` assertion does **not**. |
| `regression-suite/src/RFsSingleton.java` | Flag-free fixture, 12 checks, pending N1. |

Every CratonVM "after" value in this cluster is **PREDICTED**. This lane did
not run the VM binary.
