# Lane 4 handoff, 2026-09-19

Read [`lane-4-io-nio-foreign.md`](lane-4-io-nio-foreign.md) first (waves 1-17
are recorded there, newest last). This page is the state of the lane, what to
do next, and the traps a new session would otherwise re-find.

## Where the lane stands

**The lane is NOT done, and its page must not be retired yet.** §8 of the
lane page requires every bucket-A/B row in the prefix set to be retired,
classified C/D/E/F, a reviewed `Intrinsic`, or blocked with the blocker
named. Waves 14-17 retired the Windows filesystem stack and the buffer /
`Files` families; the ledger of the OTHER rows of the lane's 1,110 was not
re-walked in waves 14-17, and the funnel that finds candidates cannot see
rows no probe invokes (see "Traps"). What can be said is which rows are
retired, which are blocked with a reason, and which are unexamined.

### Landed on `dev`

| Wave | Content |
|---|---|
| 14 | `WindowsPath.toRealPath` (needed four natives, not one); three sibling regressions fixed |
| 15 | `WindowsPath.path` stored in the JDK's own `\` form (`path_layout::stored_form` / `canonical_form`); `getFileName`/`getParent`; `RJdkNio` and `RNioNoFollow` green; link/copy/move/delete natives |
| 16 | 13 `WindowsNativeDispatcher` natives (directory streams, volume/drive queries, `SetFileTime0`, `SetEndOfFile`, sparse/reparse `DeviceIoControl`, `GetFinalPathNameByHandle`); 23 `sun/nio/fs` rows; eager `path_layout` slot resolve in `p57_alloc_default_filesystem` |

### Wave 17 (this branch)

Landed with this handoff. Four tables (134 rows) and one native fix, all
recorded in the lane page's "Wave 17" section:

* `RETIRED_SHADOW_L4_WINNTFS_TRIPLES` (11), `RETIRED_SHADOW_L4_FILETMP_TRIPLES`
  (1), `RETIRED_SHADOW_L4_FILES2_TRIPLES` (57: all of `Files`, the Windows
  provider / store rows), `RETIRED_SHADOW_L4_BUFFERS2_TRIPLES` (65).
* `write0(..., append)` honours `append` (`../../../native-io/src/nio_native.rs`).
* New probes `L4FileTempExit` (env `L4TMPDIR`, run by hand) and
  `L4WinStoreAttrs`; the funnel script `../../../apps/probes/l4funnel.py`.
* Measured on the final release tree: regression suite `--jdk-only` 135/136
  (only `RBigIntMontgomery`), `SUITE=all` 136/136, `SUITE=core` 95/95; probe
  battery strict 0 everywhere except `L4AbsPath` 8 (random temp name) and
  `L4FfmLayoutSweep` 4; refusal survivors 0 rows (4789 refusals);
  `native-api --lib` 473, `cratonvm-types`, `native-io` green;
  `native-builtins --tests` in default, `management` and `synthetic-jdk`
  fail only the two gates listed under "Known-failing gates".

## What is blocked, and why (each is measured, not assumed)

| Row(s) | Blocker |
|---|---|
| `File.deleteOnExit` | The JDK body registers `DeleteOnExitHook` through `JavaLangAccess.registerShutdownHook`; this VM does not run `java.lang.Shutdown`'s hook slots at exit (`shutdown hooks: ran=0`). Measured with `L4FileTempExit`: HotSpot leaves the temp dir empty, this VM leaves the files. Fix belongs to the VM's exit path, not to `sun/nio`. |
| `Bits.reserveMemory` | Its native (`bits_reserve_memory`, `../../../native-io/src/direct_buffer.rs`) is the VM's own direct-memory accounting. Retiring it made `RBufferPoolCount` report `countMoved=false` in both routes; the mechanism is inferred, not traced. Its twin `unreserveMemory` has no probe. |
| `FileSystems.getDefault`, `FileSystems.newFileSystem` x2, `WindowsFileSystemProvider.getFileSystem(URI)` | `getDefault` is the identity anchor for the native `Path`/`FileSystem` carrier. Retiring `getFileSystem(URI)` alone makes `getFileSystem(file:///) != getDefault()`. Retiring the `newFileSystem` pair made a non-archive `newFileSystem` stop throwing `ProviderNotFoundException` (they lean on `installedProviders`). |
| `FileSystemProvider.installedProviders` | Real body does `ServiceLoader` discovery; the VM ships no `META-INF/services` for `jar:`/`jrt:`, so it would answer one provider where the native answers three. |
| `WindowsFileSystemProvider.checkAccess` | Reaches `WindowsSecurity` (token, `AccessCheck`): the security family is unimplemented. |
| `WindowsFileSystem.newWatchService`, `WindowsPath.register` x2, `newAsynchronousFileChannel` | IOCP / `ReadDirectoryChangesW` async runtime does not exist. |
| `WindowsFileAttributes.fileKey` | Held on purpose since 2026-08-19 (`the_held_windows_attribute_triple_is_not_retired`): the native lets `FileTreeWalker` see symlink cycles; the JDK body is `return null`. |
| `WindowsFileSystemProvider.newFileSystem(Path,Map)` | Class inherits the abstract default; the native answers an archive filesystem. |
| `ByteArrayInputStream` (3 rows), `sun/nio/ch/` package | Wave-5 and the 2026-08-19 package verdict: the rows carry an observation / the package is not retirable as a whole. |
| `Buffer.session`, `Buffer.checkSession` (and the same pair on the ten buffer classes) | Refused by the JOTP wave on `dev` (`the_jotp_wave_refuses_the_buffer_session_pair`): a segment-backed buffer's real `session()` runs against the VM's arena carrier. Measured on netty tests, which this repo's suite does not contain. |
| `FileDispatcherImpl.writev0` | `unsupported` in `../../../native-io/src/nio_native.rs`; gathering writes are not implemented. |
| `UnixFileSystem` rows | Linux image only. Not measured on this (Windows) host; the wave-17 `WinNTFileSystem` table names the Windows class only. |

## Known residuals (not regressions)

* **Compat mode** still differs on `L4PathShapes` (58 lines), `L4FilesSweep`
  (18), `L4W5Sweep` (14), `L4FfmLayoutSweep` (56): compat natives were not
  rewritten. `--jdk-only` is where the retirement tables apply.
* `L4FfmLayoutSweep` strict 4 and `L4AbsPath` strict 8 (random temp-file name)
  predate wave 15 and are unchanged.
* `write0(fd,addr,len,append=true)` now seeks to end-of-file before writing;
  it is not atomic across two appenders.
* `FileChannelImpl.open`'s old "inert" note (RETIRED_SHADOW_PHASE2) is
  superseded: it is reached once `Files` is bytecode.
* `GetDiskFreeSpace0`, `DeviceIoControlSetSparse` and
  `DeviceIoControlGetReparsePoint` (waves 15-16) are not called by any probe in
  the battery; the wave-15 `SetFileAttributes0` and `CopyFileEx0` were only
  reached in wave 17 through `Files.copy`/`Files.setAttribute`, and only as far
  as `L4FilesSweep` and `L4WinStoreAttrs` go. No per-native invocation counter
  was taken, so "reached" is inferred from behaviour. Treat the first real caller
  of the rest as their measurement.

## Known-failing gates (not this lane's)

* `stub_ratchet::synthetic_stub_count_does_not_regress`
* `unconstructed_carrier_gate::no_new_class_is_both_minted_and_retired` for
  `java/nio/HeapCharBuffer` (a sibling's, present on `dev`)
* Suite noise: `RBigIntMontgomery` (harness TIMEOUT even alone), `RMapGcStress`
  (times out in full arms, passes alone at `TIMEOUT=900`), `RMethodSiteCache`
  (intermittent SIGSEGV, passes alone).
* The kind-map and bridge-ratchet shell gates refuse on Windows.

## Traps a new session will otherwise re-find

1. **The funnel had a merge bug** (fixed in `../../../apps/probes/l4funnel.py`): a triple
   with two records (one `owns_slot: false`) kept the first, hiding real
   candidates. Wave 17's first funnel said `Files` had 13 rows; it has 32. Any
   earlier "N candidates" figure in the lane page may be low for this reason.
2. **`invocations > 0` hides rows a retirement makes reachable.** Retiring
   `Files` made `WindowsFileSystemProvider.{copy,createDirectory,delete,move}`
   reachable, and they were DELEGATING natives that call `Files.*` back:
   infinite native recursion (`EXCEPTION_STACK_OVERFLOW` at startup). They must
   be retired in the same table. Use `--zero-inv` and the bisect variable below.
3. **Bisect in runs, not builds:** `CRATONVM_UNRETIRE_NATIVE_SHADOW=` takes
   `class`, `class.method` or `all`, comma-separated. Retire everything, then
   un-retire one method at a time (see how wave 17 found the four recursing
   `Files` methods in one pass).
4. **A row can be correct and unretirable** when its native carries an
   observation (a counter, a list, an identity): `Bits.reserveMemory`,
   `deleteOnExit`, `getFileSystem(URI)` all did. A one-probe green is not
   enough; the suite arms found `RBufferPoolCount`.
5. **`unconstructed_carrier_gate`** flags a native-minted class newly carrying
   retired triples. The procedure is in its failure message: `javap -p -c`, check
   whether any retired method reads a field with a non-zero declared initialiser.
   `java/io/WinNTFileSystem` is baselined with that verdict; **retiring
   `normalize`/`resolve`/`getSeparator`/`getDefaultParent` on it voids the
   verdict** (they read `slash`/`altSlash`/`userDir`, which the mint site does
   not write).
6. **Path storage:** `Path.path` holds the `\` form on Windows
   (`path_layout::stored_form`); every native READER must call `canonical_form`.
   A native reading the slot directly sees the wrong separator.
7. **Path slot resolution:** `PathSlots` must be resolved (`p57_path_slots_init`)
   before any `&dyn NativeContext` reader runs; `p57_alloc_default_filesystem`
   does it now. A new entry path that builds `WindowsPath` in bytecode without
   going through it reads `""`.
8. **Tooling, on this host:**
   * `JAVA_HOME="C:/Program Files/Microsoft/jdk-25.0.3.9-hotspot"` (Windows
     style) for `../../../apps/probes/l4run.sh`; an MSYS path silently fails every probe.
   * `CV=<binary>` for `l4run.sh`; `CV=` and `ONLY="R..."` for
     `../../../regression-suite/run.sh`. Suite arms share `BUILD` dirs: run them strictly
     sequentially.
   * `../../../scripts/jdk-only-refusal-survivors.sh` needs a `python3` on PATH: put the
     shim directory containing `python3 -> C:/Python314/python` first. From a
     worktree-isolated Bash session use PowerShell with the full Git bash path;
     `PATH=... cmd` prefixes are refused.
   * Rewriting a repo file from Python: read/write BYTES, and strip `\r` from any
     text produced by `print` redirection, or a 170-line CRLF diff appears.
   * `--dump-native-registry` needs a Windows-style output path.
9. **Probes that print random temp names** diff against themselves; `l4run.sh`
   collapses `/tmp/<word><digits>` only. `L4AbsPath` prints one.
   `L4TailSweep2` and `L4BridgeSweep` exited 1 with no output on all three
   arms including HotSpot in the wave-16 run: they compared nothing there and
   were not diagnosed.

## Owed verification (not done)

* **Wave 17 was merged with `origin/dev` after its three arms ran.** On the
  merged tree, rebuilt in release, only `--jdk-only` (135/136, `RBigIntMontgomery`)
  and the 17-probe battery (identical to pre-merge) were re-run, plus
  `cargo test` (`native-api` 476, `cratonvm-types`, `native-io`). `SUITE=all` and
  `SUITE=core` on the merged tree are owed.
* **Segment-backed buffers.** Run `PcapWriteHandlerTest` and
  `AdaptiveBigEndianDirectByteBufTest` (the netty workload; on the host named in
  the DoD-workload note) against a binary carrying the buffer table. The suite
  and the lane's probes never build a segment-backed buffer.

## Suggested next steps, in order

1. Land wave 17 (or the parts of it its arms clear), then re-run
   `apps/probes/l4funnel.py --refresh --zero-inv java/` and
   `--zero-inv sun/nio/` and `--zero-inv jdk/internal/foreign` to get a ledger
   that includes never-invoked rows.
2. Walk the whole prefix set once and write a per-row disposition table
   (retired / C-F class / Intrinsic + probe / blocked + named blocker) into the
   lane page. This is the §8 gate; nothing else closes the lane.
3. `jdk/internal/foreign` (239 rows, `L4FfmLayoutSweep` strict 4) was not
   re-walked in waves 14-17, and §4 of the lane page still requires every
   buffer/FFM retirement to be backed by a probe that reads data back and checks
   exception types. Wave 17 retired 65 buffer-family rows on the strength of
   the existing sweeps; confirm each sweep really reads data back before
   relying on it.
4. VM-side items that unblock retirements: run `Shutdown` hook slots at exit
   (`deleteOnExit`), an IOCP/completion-port runtime (watch service, async
   channels), a `WindowsSecurity` token/`AccessCheck` family (`checkAccess`).
5. Only when 2 is complete: move the lane page to `../../internal/retired`
   (naming: `<name>-<YYYYMMDD>.md`, e.g. `lane-6-carrier-hazard-census-20260912.md`;
   recently fixed suite bugs use a `-FIXED-<date>` suffix), fix every citation
   (`cargo test -p cratonvm-types`, `doc_citation_paths`), and record the move.
   A doc retirement is a source edit.
