# Name-keyed attributes on a jar/jrt entry were `stat`ed on the host — FIXED 2026-08-07

**Status: ✅ FIXED** for the read side; the write side now refuses honestly
instead of failing confusingly. The underlying single-provider shape is
unchanged and is still the durable item — see *What is left*.

## What it was

CratonVM has ONE synthetic `java/nio/file/spi/FileSystemProvider` object, and
the name-keyed attribute natives are registered on that abstract class, so they
answer for **every** provider — `jdk.nio.zipfs.ZipFileSystemProvider` included.
A mounted archive's `Path`s are the sentinel-delimited
`\x01JARFS\x01<jar>\x01<entry>` form (see `vfs_encode`), which names no host
file at all. `read_named_attributes` handed that string to `stat_facts`, so
every `Files.readAttributes` / `Files.getAttribute` on a zip or jrt entry came
back `NoSuchFileException` naming a path the user never wrote.

`apps/h2database-suite-runner/probes/ZipAttrProbe.java`, against `dev`
@ `cf4274fda`:

| | HotSpot | before | after |
|---|---|---|---|
| `fs.provider().getClass()` | `jdk.nio.zipfs.ZipFileSystemProvider` | `sun.nio.fs.UnixFileSystemProvider` | unchanged |
| `readAttributes(entry, "basic:size")` | `{size=5}` | `NoSuchFileException` | **`{size=5}`** |
| `Files.size(entry)` | 5 | 5 | 5 |
| `getAttribute(entry, "lastModifiedTime")` | entry's stamp | `NoSuchFileException` | **archive's stamp** |
| `setAttribute(entry, "lastModifiedTime", …)` | OK | `IOException: No such file or directory` | `UnsupportedOperationException` |

`Files.size` worked throughout, which is what made this look narrower than it
was: the size path already went through the archive helpers, and only the
name-keyed reader/writer leaked to the host.

## The fix

`vfs_stat_facts` builds `StatFacts` for a jar/jrt entry out of the archive
index — the same `jarfs_classify` / `jarfs_entry_size` / `jrtfs_*` helpers
everything else already uses — and `read_named_attributes` consults it before
falling back to `stat`. `attribute_view_names_for_path` reports `basic` and only
`basic` for an archive path, because that is what HotSpot's zipfs offers for an
entry and because answering `unix:mode` here would mean answering it out of a
host `stat` of a path that does not exist.

`write_named_attribute` refuses an archive path by name with
`UnsupportedOperationException`. Writing into a mounted archive means rewriting
the archive, which this entry point does not do; the alternative was to fall
through to the host writes, which would chmod or re-time whatever the sentinel
string happened to resolve to.

## Two divergences that remain, both measured

**Timestamps are the ARCHIVE's, not the entry's.**
`probes/ZipMtimeProbe.java` writes an entry stamped 2001-01-01 into an archive
created now, then reads it back without writing anything first:

| | HotSpot | CratonVM |
|---|---|---|
| archive mtime | 2026-08-07T14:43:59Z | 2026-08-07T14:44:01Z |
| **entry** `lastModifiedTime` | **2001-01-01T00:00:00Z** | 2026-08-07T14:44:01Z |
| entry `size` | 5 | 5 |
| entry `isDirectory` | false | false |

The zip central directory does carry a per-entry MS-DOS timestamp, but the `zip`
crate only exposes it through `DateTime`, whose fields are private and whose
only accessor (`to_time`) is behind the crate's `time` feature. The workspace
pins `zip = { default-features = false, features = ["deflate"] }` deliberately
(see the comment on that pin), and `time` is not otherwise in `Cargo.lock`, so
enabling it would add a dependency tree for one field. The container's own mtime
is at least a *real* timestamp, is already computed for the index cache key, and
is what every entry of a rebuilt archive would carry anyway. Whoever needs
per-entry precision should reopen the pin decision, or read the central
directory directly.

**`setAttribute` on an entry refuses where HotSpot succeeds.** A JDK-legal
`UnsupportedOperationException` — which is what zipfs itself throws for a
read-only archive — rather than the previous `IOException` about a file that was
never named.

## What is left

`fs.provider()` still reports `sun.nio.fs.UnixFileSystemProvider` for a `jar:`
filesystem, because there is still one provider object for every filesystem.
This page fixes the two natives that leaked; it does not fix the shape. The
durable item is for `FileSystems.newFileSystem(jar:…)` to hand back a provider
stamped with a zipfs-specific class carrying its own attribute natives — which
would also retire the standing hazard recorded in the retired
`bug-h2-files-setattribute-abstract-provider` write-up: every abstract method on
`FileSystemProvider` is one unregistered native away from `AbstractMethodError`.

## Verification

`a_jar_entry_is_stat_ed_out_of_the_archive_not_the_host` builds a real two-entry
zip and asserts the file/dir/absent classification and the size come from the
archive, and that a plain host path still falls through to `stat`.
`an_archive_entry_offers_only_the_basic_view` pins the view screening.
`cratonvm-native-builtins` lib is 3337 passed.

Same-host A/B, `dev` @ `cf4274fda` with and without the change: the first 40 H2
suite classes are byte-identical between arms, and `TfsProbe` fails identically
in both — because `dev` currently has an unrelated `FileLockTable` NPE on
`FileChannel.tryLock` that takes `TestFileSystem` and every database-opening H2
class down (filed separately). That makes the A/B a valid no-delta check but a
low-power one; the unit tests and the probe table above are what the read-side
verdict rests on. Running a class from a jar classpath is unchanged in both arms.
