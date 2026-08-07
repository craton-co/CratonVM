# `Files.copy` silently overwrote, and every plain `Files.walk` logged a `NoSuchMethodError`

**Status:** ROOT-CAUSED in source 2026-08-06 (lane L5, JDK-only wave 2).
The fix is **one helper landed in `native-io`** plus **two patches that lane L5
does not own** — see *Out-of-file patches* below. Nothing is verified against a
binary yet; see *How to verify*.

## The failure

`regression-suite/src/RJdkNio.java` fails in **both** `--real-jdk` and
`--jdk-only`. HotSpot 25 runs it to `PASS RJdkNio (78 checks)`. Both CratonVM
arms produce the byte-identical trace below, which is the tell that this is an
ordinary Compatible-mode defect and not a strict-mode policy drop — nothing was
*refused*:

```
WARN cratonvm_vm::vm::vm_exec: NoSuchMethodError
  method="java/nio/file/FileVisitOption.isEmpty()Z"
  caller="RJdkNio.deleteTree(Ljava/nio/file/Path;)V @pc=28"
Exception in thread "main" java/lang/AssertionError: copy onto an existing file must throw FileAlreadyExistsException
    at RJdkNio.main(RJdkNio.java:383)
    at RJdkNio.filesApi(RJdkNio.java:102)
    at RJdkNio.check(RJdkNio.java:51)
```

Two independent defects. The `AssertionError` is fatal; the
`NoSuchMethodError` is a swallowed WARN that has been firing on every
`Files.walk(path)` in the tree.

## Which registration actually runs (checked first, because there are two)

`java/nio/file/Files.copy(Path,Path,CopyOption[])` is registered **twice**:

| site | function |
| --- | --- |
| `native-io/src/lib.rs:11380` (`register_nio_file_natives`) | `native_files_copy`, `native-io/src/lib.rs:12268` |
| `native-builtins/src/phases_late/nio_file.rs:4853` (`register_phase57_nio_file`) | inline closure |

`NativeMethodRegistry` is **last-write-wins** for an identical triple
(`native-api/src/registry.rs`, `overwrite_same_triple` test at :7337; the
`first-wins` rule at :6261 governs only the class-agnostic
`by_method_desc` index, not the triple table). Both boot arms in
`vm/src/vm/vm_init.rs` call `register_io_natives` **before**
`register_phase57_nio_file` — real-JDK arm at :1840 then :1900, the other arm at
:2315 then :2385 — which is the same ordering
`native-builtins/tests/registry_contracts.rs:287` replays and asserts on for the
WatchService surface.

**So the `native-builtins` copy wins in every arm, and the `native-io` one is
shadowed dead code.** `scripts/baselines/jdk-only-kind-map-25-linux.tsv:5618-5619`
independently confirms the duplicate: two `Files.copy` rows, indices 0 and 1.
Both are patched below, but only the `native-builtins` one changes behaviour.

## Defect 1 — `Files.copy` never reads its `CopyOption[]`

`RJdkNio.filesApi` (`regression-suite/src/RJdkNio.java:93-103`):

```java
Path copy = dir.resolve("copy.txt");
Files.copy(f, copy);                       // creates it
boolean threw = false;
try {
    Files.copy(f, copy);                   // must throw: target exists, no REPLACE_EXISTING
} catch (FileAlreadyExistsException expected) {
    threw = true;
}
check(threw, "copy onto an existing file must throw FileAlreadyExistsException");
Files.copy(f, copy, StandardCopyOption.REPLACE_EXISTING);   // must succeed
```

The winning native, `native-builtins/src/phases_late/nio_file.rs:4857-4919`,
**never looks at `args[2]` at all**. It classifies the source, then:

```rust
} else {
    std::fs::copy(&src_path, &dst_path).map(|_| ())        // :4910
};
```

`std::fs::copy` is `CopyFileExW`/`open(O_TRUNC)` — it overwrites
unconditionally and reports success. So the second `Files.copy` returned
normally, `threw` stayed `false`, and the assertion fired. This is a *wrong
answer*, not a missing feature: there is no `UnsatisfiedLinkError`, no refusal,
nothing in the log.

The directory branch at `:4901-4905` has the same hole and additionally
**swallows** the one signal the OS does give:
`Err(AlreadyExists) => Ok(())`.

### Why this survived so long

The sibling `Files.move` native, thirty lines below at `:4926-4988`, has the
*exact* fix already — a `replace_existing` scan of `args[2]`, a
`std::fs::symlink_metadata(&dst_path).is_ok()` pre-check, a `src_path != dst_path`
self-move exemption, and a REAL `FileAlreadyExistsException` built through its
single-`String` constructor. It was added for H2's `FilePathDisk.moveTo`
(internal record `bug-h2-files-setposixfilepermissions-FIXED.md`). Both helpers it
needs are `pub(crate)` in the same file and already have a second caller:

* `copy_options_replace_existing` — `nio_file.rs:10422`
* `p57_file_already_exists` — `nio_file.rs:10460`, used by the
  `Files.copy(InputStream,Path,CopyOption[])` overload at `:17832-17833`,
  which **does** get this right.

So of the three `Files.copy`/`move` entry points, the two that were fixed for a
specific application bug are correct and the third — the one the JDK's own
`Files.copy(Path,Path)` convenience overload routes through — was never
revisited. Nothing about `Files.copy` is special; it was simply not the method
that broke H2.

## Defect 2 — the `isEmpty()` probe is aimed at an array

`RJdkNio.deleteTree` (`:61`) does `Files.walk(root)`. javac compiles the
zero-arg varargs call to `Files.walk(root, new FileVisitOption[0])`:

```
20: aload_0
21: iconst_0
22: anewarray  java/nio/file/FileVisitOption
25: invokestatic java/nio/file/Files.walk(Ljava/nio/file/Path;[Ljava/nio/file/FileVisitOption;)Ljava/util/stream/Stream;
28: astore_2                                  <-- the "@pc=28" in the WARN
```

That native (`nio_file.rs:4528-4540`) calls
`p57_visit_options_follow_links(ctx, args.get(1))`, at `nio_file.rs:9787-9814`:

```rust
if ctx.array_length(o) > 0 {          // :9798 — empty varargs array: 0, falls through
    return true;
}
...
match ctx.invoke_virtual(o, "isEmpty", "()Z", &[]) {   // :9810 — receiver is the ARRAY
```

`array_length` answers `0` both for "not an array" and for "empty array", so an
empty varargs array falls through to an `isEmpty()` call **on the array
object**. The existing comment at `:9803` is aware of this — *"`isEmpty` on an
array simply fails to resolve — landing on the same `false` an empty array
deserves"* — and it is right about the answer. It just does not account for the
resolution failure being **logged**, on every `Files.walk`/`Files.find` call in
every program.

**This is not an interpreter or JIT dispatch bug.** The class name in the
message is the tell, and it is correct behaviour, not a mislabelling:

> `object_is_array` ... does NOT go through `class_id_of_object`/`class_name_of_id`,
> which for a heap-allocated reference array report the *component* class
> (arrays store their element class id + an array kind flag rather than a
> distinct `[L…;` class id)
> — `native-api/src/registry.rs:2369-2376`

So a reference array's reported class *is* its element class. `[Ljava/nio/file/
FileVisitOption;` naturally renders as `java/nio/file/FileVisitOption`, and the
VM's `NoSuchMethodError` message is the honest consequence of a native asking an
array for `isEmpty()`. This is the same family as
`memory/array-receivers-alias-component-class-id-in-inline-caches.md`, but here
the receiver-type confusion is **in the native, in Rust**, not in the dispatch
machinery. The interpreter's answer is correct; the question was wrong.

The prohibition in the comment at `:9806` — *"Do NOT discriminate on the class
name first"* — stands, and the fix respects it: `object_is_array` is a heap
object-KIND check, not a class-name test, which is exactly what that comment
was asking for and could not find.

## What landed in lane L5's own files

`native-io/src/nio_native.rs` — new `pub fn file_already_exists(ctx, path)`
immediately after `io_error`. It builds a REAL
`java/nio/file/FileAlreadyExistsException` through the class's single-`String`
constructor, pinning the fresh object across `create_string` (which allocates,
and would otherwise leave a stale local under a moving young GC). It mirrors
`p57_file_already_exists` in `native-builtins`.

Three reasons it is built rather than raised through `RuntimeError`:

* `cratonvm-types`' `RuntimeError` has **no** `FileAlreadyExistsException`
  variant (`types/src/error.rs:1514-1540` maps `IOException`,
  `FileNotFoundException` and `NoSuchFileException` and stops there), so
  `io_err`-style raising cannot produce this class at all.
* Callers discriminate by **type** — `RJdkNio.java:99` is a bare
  `catch (FileAlreadyExistsException expected)` — so an `IOException` carrying
  the right message is not a substitute.
* The object is handed to real JDK bytecode (`getFile()`, `Throwable`
  formatting), which reads real field offsets; a fabricated layout would also be
  refused under `--jdk-only`.

It is `pub`, not `pub(crate)`: `native-io` declares `pub mod nio_native`, so
`pub` keeps `dead_code` quiet until the out-of-file caller below is wired up,
and lets `native-builtins` collapse onto one builder later if anyone wants to.

## Out-of-file patches (lane L5 does not own these files)

### Patch A — `native-builtins/src/phases_late/nio_file.rs:4857` (REQUIRED; this is the one that fires)

Note the ordering: the option scan re-enters Java (`CopyOption.toString()`) and
can allocate, so it must run **before** any `ObjectRef` is copied out of `args`
— the same rule the `Files.walk` registration states at `:4533-4535`.

*old*

```rust
        |ctx, args| {
            let src = obj_arg(args, 0)?;
            let dst = obj_arg(args, 1)?;
            let src_path = p57_read_path(ctx, src);
            let dst_path = p57_read_path(ctx, dst);
            // Java `Files.copy(Path,Path,CopyOption...)`: copying a DIRECTORY
```

*new*

```rust
        |ctx, args| {
            // Read the `CopyOption[]` FIRST: `copy_options_replace_existing`
            // re-enters Java (`CopyOption.toString()`), which can allocate, so
            // any `ObjectRef` copied out of `args` before it would be a stale
            // local under a moving young GC — the same ordering rule the
            // `Files.walk` registration above states.
            let replace_existing = copy_options_replace_existing(ctx, args.get(2));
            let src = obj_arg(args, 0)?;
            let dst = obj_arg(args, 1)?;
            let src_path = p57_read_path(ctx, src);
            let dst_path = p57_read_path(ctx, dst);
            // NIO contract: without REPLACE_EXISTING an EXISTING target is a
            // `java.nio.file.FileAlreadyExistsException`, not a silent
            // overwrite. `std::fs::copy` below overwrites unconditionally and
            // the directory branch swallows the OS's `AlreadyExists`, so
            // `Files.copy(f, copy)` onto an existing file returned normally
            // where HotSpot 25 throws — `RJdkNio.filesApi:102`, "copy onto an
            // existing file must throw FileAlreadyExistsException". Same
            // pre-check and same REAL exception object as the `Files.move`
            // native below and the `Files.copy(InputStream,Path,CopyOption...)`
            // overload; `Files.copy` was simply never revisited when those two
            // were fixed for H2.
            //
            // `symlink_metadata` (not `exists`) so a DANGLING symlink at the
            // target counts as existing, and `src_path != dst_path` so a
            // self-copy stays the JDK's no-op rather than becoming an error —
            // both mirroring `Files.move`. A jarfs-encoded destination is a
            // mounted-zip entry, not an OS path, so it is left to the jarfs
            // writers below and keeps Quarkus's `ZipUtils.unzip` behaviour.
            if !replace_existing
                && src_path != dst_path
                && jarfs_decode(&dst_path).is_none()
                && std::fs::symlink_metadata(&dst_path).is_ok()
            {
                return Err(p57_file_already_exists(ctx, &dst_path));
            }
            // Java `Files.copy(Path,Path,CopyOption...)`: copying a DIRECTORY
```

Both helpers are `pub(crate)` in this same file (`:10422`, `:10460`); no import
changes. The `Err(AlreadyExists) => Ok(())` arm at `:4903` is deliberately left
in place — with the pre-check above it, it is now only reachable *with*
REPLACE_EXISTING, where tolerating an existing target directory is correct and
is what Tomcat's `recursiveCopy` relies on.

### Patch B — `native-builtins/src/phases_late/nio_file.rs:9795` (REQUIRED; silences the WARN)

*old*

```rust
    // Array form (`Files.walk(path, opts...)` varargs). `array_length` answers
    // 0 for a non-array, so a positive length is proof of a non-empty array and
    // nothing else needs asking.
    if ctx.array_length(o) > 0 {
        return true;
    }
    // Either an EMPTY array or the Set form
    // (`Files.walkFileTree(path, Set<FileVisitOption>, ...)`). Asking a Set is
    // the only way to tell them apart, and `isEmpty` on an array simply fails
    // to resolve — landing on the same `false` an empty array deserves.
    //
    // Do NOT discriminate on the class name first: an array's class name is not
    // reliably resolvable here, and a miss silently sent every varargs
    // `FOLLOW_LINKS` down the Set branch, which is how `Files.walk(p,
    // FOLLOW_LINKS)` kept behaving as if the option had not been passed at all.
    match ctx.invoke_virtual(o, "isEmpty", "()Z", &[]) {
```

*new*

```rust
    // Array form (`Files.walk(path, opts...)` varargs). `array_length` answers
    // 0 for a non-array, so a positive length is proof of a non-empty array and
    // nothing else needs asking.
    if ctx.array_length(o) > 0 {
        return true;
    }
    // An EMPTY array is the zero-arg varargs case — `Files.walk(root)` compiles
    // to `Files.walk(root, new FileVisitOption[0])` — and means "do not follow".
    // Ask it nothing. Falling through to `isEmpty()` on the array reached the
    // right answer by ACCIDENT, through a failed resolution swallowed by the
    // `_ => false` arm below, but it logged a `NoSuchMethodError` on every
    // plain `Files.walk` in every program:
    //
    //   NoSuchMethodError method="java/nio/file/FileVisitOption.isEmpty()Z"
    //     caller="RJdkNio.deleteTree(Ljava/nio/file/Path;)V @pc=28"
    //
    // The class named there is the COMPONENT class, not
    // `[Ljava/nio/file/FileVisitOption;`: a reference array stores its element
    // class id plus an array-kind flag rather than a distinct array class id,
    // so anything name-based reports the element type. Hence `object_is_array`
    // — a heap object-KIND check, explicitly NOT a class-name test — which is
    // exactly the discriminator the note below asks for.
    if ctx.object_is_array(o) {
        return false;
    }
    // Set form (`Files.walkFileTree(path, Set<FileVisitOption>, ...)`): asking
    // is the only way, and a non-empty set unambiguously means FOLLOW_LINKS
    // because `FileVisitOption` is a single-constant enum.
    //
    // Do NOT discriminate on the class NAME here: an array's class name is not
    // reliably resolvable, and a miss silently sent every varargs
    // `FOLLOW_LINKS` down the Set branch, which is how `Files.walk(p,
    // FOLLOW_LINKS)` kept behaving as if the option had not been passed at all.
    match ctx.invoke_virtual(o, "isEmpty", "()Z", &[]) {
```

`object_is_array` is `NativeHeapAccess::object_is_array`
(`native-api/src/registry.rs:2377`), inherited by `NativeContext`, overridden by
the VM at `vm/src/vm/vm_exec.rs:10370`. Its default is `false`, so a mock
context degrades to today's behaviour rather than to a wrong answer.

### Patch C — `native-io/src/lib.rs:12268` (OPTIONAL, hygiene: this registration is shadowed)

Keeps the losing duplicate honest so a future ordering change cannot silently
reintroduce Defect 1. It is the only consumer of the `file_already_exists`
helper landed above, so applying the helper without this patch leaves the
helper uncalled (harmless — it is `pub`).

*old*

```rust
fn native_files_copy(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    let src = validated_path(&files_path_str(ctx, args))?;
    let dst = match args.get(1) {
        Some(Value::Object(Some(o))) => read_path_str(ctx, *o),
        _ => String::new(),
    };
    let dst = validated_path(&dst)?;
```

*new*

```rust
fn native_files_copy(ctx: &mut dyn NativeContext, args: &[Value]) -> MethodCallResult {
    // Read the `CopyOption[]` FIRST: the `toString()` probe re-enters Java and
    // can allocate, so any `ObjectRef` copied out of `args` before it would be
    // a stale local under a moving young GC.
    let mut replace_existing = false;
    if let Some(Value::Object(Some(opts))) = args.get(2) {
        let opts = *opts;
        let pin = ctx.pin_native_root(opts);
        let len = ctx.array_length(opts);
        for i in 0..len {
            let opts_cur = ctx.read_native_pin(pin, opts);
            if let Value::Object(Some(opt)) = ctx.get_array_element(opts_cur, i) {
                if let Ok(Some(Value::Object(Some(s)))) =
                    ctx.invoke_virtual(opt, "toString", "()Ljava/lang/String;", &[])
                {
                    if ctx
                        .read_string(s)
                        .unwrap_or_default()
                        .contains("REPLACE_EXISTING")
                    {
                        replace_existing = true;
                        break;
                    }
                }
            }
        }
        ctx.unpin_native_roots(pin);
    }
    let src = validated_path(&files_path_str(ctx, args))?;
    let dst = match args.get(1) {
        Some(Value::Object(Some(o))) => read_path_str(ctx, *o),
        _ => String::new(),
    };
    let dst = validated_path(&dst)?;
    // NIO contract: without REPLACE_EXISTING an existing target is a
    // `FileAlreadyExistsException`, not a silent overwrite. `std::fs::copy`
    // below overwrites unconditionally. This registration currently LOSES to
    // `native-builtins`' `register_phase57_nio_file` (registered later; the
    // triple table is last-write-wins), so this is defence against an ordering
    // change, not the live fix — see Patch A.
    if !replace_existing && src != dst && std::fs::symlink_metadata(&dst).is_ok() {
        return Err(crate::nio_native::file_already_exists(ctx, &dst));
    }
```

## What this does NOT fix

`RJdkNio` dies at check ~14 of 78, so everything after `filesApi:102` is
**unmeasured** — `raf+map`, `buffers`, `asyncClose` and the rest of `filesApi`
(`Files.move`, `createDirectories`, `newDirectoryStream`, the
`NoSuchFileException` case) have never executed under CratonVM in this vector.
Expect further findings on the next run; do not read "these two are fixed" as
"RJdkNio passes".

## How to verify, once a binary exists

```
cargo build --release -p cratonvm-cli
javac -d regression-suite/build regression-suite/src/RJdkNio.java

java -cp regression-suite/build RJdkNio                            # HotSpot 25 oracle
target/release/cratonvm --real-jdk -cp regression-suite/build RJdkNio
target/release/cratonvm --jdk-only -cp regression-suite/build RJdkNio
```

Three things to look for, in this order:

1. **No** `NoSuchMethodError ... FileVisitOption.isEmpty()Z` line anywhere in
   either arm. That is Defect 2, and it is visible on the very first
   `deleteTree` call — before any assertion runs.
2. Neither arm asserts `copy onto an existing file must throw
   FileAlreadyExistsException`. That is Defect 1.
3. `CK RJdkNio files=[a, moved.txt, plain.txt] size=12 normalize=a/c` — the
   directory listing that follows the copy/move block. If this line appears but
   differs, the copy/move sequence completed and something *else* is wrong;
   if it never appears, Defect 1 is not fixed.

HotSpot's full expected output is four `CK` lines and
`PASS RJdkNio (78 checks)`.

A targeted negative control for Defect 1, in case the arms disagree: run the
same class with only the `REPLACE_EXISTING` call. It must still succeed —
a fix that makes *every* copy onto an existing target throw has broken the
option scan, not implemented it.

## Baselines

`scripts/baselines/jdk-only-kind-map-25-linux.tsv:5618-5619` already carries
both `Files.copy` rows as `bridge`. No registration is added, removed, or
re-kinded by any patch here — only bodies change — so the kind map, the bridge
ratchet and `native-builtins/tests/stub_ratchet.rs` are all untouched.
