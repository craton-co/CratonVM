# Lane 4 continuation — `WindowsPath.type`/`.root`: the fourth defect in one chain

**Status: OPEN. Opened 2026-09-17** as a continuation of lane 4
(`java/io/`, `java/nio/`, `sun/nio/`, `jdk/internal/foreign` —
[`lane-4-io-nio-foreign.md`](lane-4-io-nio-foreign.md)),
which is the one prefix lane from the original campaign **still active, not
retired**. This page is a new file rather than an edit to that 1,641-line page
because the item it covers is explicitly flagged there as a follow-up, not
folded into any of that page's own numbered waves.

**A dedicated worktree, `jdk-lanes-windowspath-typeroot-20260917`, already
exists at `dev`'s tip with no commits of its own yet.** If you are the session
in that worktree, this page is written for you — start here.

## The chain, in order, so the next step isn't re-derived

Lane 4's own page landed this in sequence, each fix exposing the next:

1. **`FileSystem` carrier fix** (landed, Windows-verified, unconditional —
   not a retirement, a correctness fix every caller benefits from): the
   default `FileSystem`'s own class-identity stamp was wrong, so real
   `WindowsFileSystem`/`WindowsFileSystemProvider` methods that reached
   `getFileSystem()` ran against the wrong object for every caller.
2. **Third `FileSystemProvider` retirement attempt, same result**: with `Path.fs`
   and the `FileSystem` carrier both fixed, real
   `WindowsFileSystemProvider.newFileChannel` now reaches real
   `WindowsPath.getPathForWin32Calls` → `getAbsolutePath` → `isSameDrive`,
   which reads `this.root` directly and throws
   `NullPointerException: Cannot invoke "String.charAt(int)" because "root1" is null`.
   Confirmed isolated to this one table
   (`CRATONVM_UNRETIRE_NATIVE_SHADOW=java/nio/file/spi/FileSystemProvider`
   makes the crash disappear).
3. **The fourth defect, and it is `Path`'s own contents again**: `sun.nio.fs.WindowsPath`
   (`javap -p`) declares seven real instance fields — `fs`, `type`
   (`WindowsPathType`), `root` (`String`), `path` (`String`),
   `pathForWin32Calls`, `offsets`, plus two static constants.
   `native-api/src/path_layout.rs`'s `PathSlots` resolves all seven (so
   nothing is written *out of bounds*) but `p57_write_path_fields` only
   **writes** three: `string` (→ `path`), `bytes` (Unix only), and `fs` (added
   in step 1). **`type` and `root` are permanently null/zero on every `Path`
   this VM's natives build**, invisible until a retired `newFileChannel` let
   real internals run far enough to read `root` unconditionally with no null
   guard (HotSpot never expects null there, because on HotSpot both fields are
   written in `WindowsPath`'s own constructor, in the same assignment as
   `path`).

## What this lane is: write `type` and `root` at every allocation site

**Why it is not a same-session, single-site fix, unlike `fs`.** `fs` is one
object reference, set once at `FileSystem`-construction time and shared. `root`
and `type` are **per-path**: every `p57_alloc_path`/`p57_alloc_path_raw` call
site (lane 4's own page counts dozens, §9.28) needs to classify its own input
the same way real `WindowsPathParser.parse` does —
`ABSOLUTE`/`UNC`/`RELATIVE`/`DIRECTORY_RELATIVE`/`DRIVE_RELATIVE` — and store
the resulting `root` string alongside it.

**The lowest-risk route, following this wave's own precedent** (the
`FileSystem` fix used the same real `WindowsPathParser.parse` to derive
`defaultDirectory`/`defaultRoot`): invoke that same real, already-functional
parser from `p57_write_path_fields` itself — the one writer every allocator
already goes through — rather than hand-porting Windows path classification
into Rust a second time.

**What that route needs measuring before it can be trusted**, per lane 4's own
flag: does every call site's input text survive round-tripping through the
real parser unchanged? `jar:`/`jrt:` sentinel-encoded strings (used elsewhere
in this VM's own path encoding) almost certainly do **not**, and would need to
keep today's behaviour rather than being run through the real parser blind.
Enumerate every `p57_alloc_path`/`p57_alloc_path_raw` call site first, and
classify which ones carry a sentinel-encoded string before wiring the parser
in universally.

## Acceptance

`FileSystemProvider` retirement (the four-row table:
`createLink`/`createSymbolicLink`/`newFileChannel`/`readSymbolicLink`) is the
existing, already-written acceptance test — it has failed differently on each
of its three prior attempts, and closing this defect is what should make the
fourth attempt hold. Re-run `L4FilesSweep` (395 rows) for zero new
divergences and the same corpus gate set
([`../../contributing/jdk-only-lane-operations.md`](../../contributing/jdk-only-lane-operations.md)
§5) lane 4 already used for its own waves. This is Windows-only work by
construction — `WindowsPath`/`WindowsPathType`/`WindowsPathParser` have no
Unix analogue in this defect.

## Landing

This is `sun/nio/fs/` prefix territory, already lane 4's own — coordinate with
whoever else is active on `jdk-lanes-io-nio-foreign-*` before landing, since
both touch `path_layout.rs` and the `p57_*` allocation sites. Do not duplicate
lane 4's own retirement-table entries; this page's deliverable is the field
writes, and the `FileSystemProvider` retirement itself is lane 4's table to
amend once this lands.
