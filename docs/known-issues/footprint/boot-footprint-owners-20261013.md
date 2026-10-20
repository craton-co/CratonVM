> **MOVED (20261019, zt round orchestrator):** out of the ZGC set: its open part belongs to another owner (zt t5/pg3 audit, `docs/internal/zgc-round-zt-20261018/t5-pg3-report.md`). Former name `docs/known-issues/footprint/boot-footprint-owners-20261013.md`.

# Boot footprint owners outside the GC: the JMOD open and the jrt image copy

> **STATUS (2026-10-19, zt t5/pg3): OTHER-OWNER (VM footprint: class path / jimage / JMOD).** Open items 1, 2, 4 of 'Not done' are class-path code, not GC. Move to `docs/known-issues/footprint/boot-footprint-owners-20261013.md`.

> **STATUS (2026-10-19, zt t3/fp): `resource_to_modules`' directory-record share FIXED IN CODE** (`CRATONVM_JIMAGE_SKIP_DIRECTORY_RECORDS`, default ON, `=0` kill). "Not done" items 1, 2 and 4 unchanged. Rows: `docs/internal/zgc-round-zt-20261018/t3-fp-report.md`.

> **STATUS (2026-10-19, zt t2/fp): `resource_to_modules` split out.** Its directory-record share (~2 300 of ~3 200 entries) is now its own page with a Java-visible probe, `zgc-zt2fp-jimage-directory-records-are-indexed-as-resources-CLOSED-20261019.md` (`Zt2FpJimageDirRecords`). "Not done" items 1, 2 and 4 unchanged.

> **STATUS (2026-10-16, zs s10/pg2): OPEN -- "Not done" item 3 (`CRATONVM_JIMAGE_PACKAGE_INDEX`, default ON) has run on the host. zs orch-notes s6a: the rs hello/cls rows are SAME in both modes, CPU is within noise, and the key stays ON. orch-notes records no RSS figure for it. F1 (`CRATONVM_JMOD_LAZY_INDEX`) is still not host-measured: the s5a home has no `jmods/`. Still open: "Not done" items 1, 2 and 4, and `resource_to_modules`.**

> **STATUS (2026-10-15, zs s6/rs): "Not done" item 3 FIXED IN CODE, PENDING HOST PROBE.** The jimage `class_to_module` map (27 086 `String` pairs on JDK 25, ~5 MB) is now a package -> module-number index (876 packages) confirmed by the image's perfect-hash lookup (`JImageReader::has_class`), as libjimage does (`CRATONVM_JIMAGE_PACKAGE_INDEX`, ON, `=0` kill; a split package keeps the per-class map). Verified locally against the real JDK 25.0.3 image: every class answers alike in both arms (`zs6rs_real_jdk_image_package_index_matches_the_class_map`). Still open here: items 1, 2 and 4, and `resource_to_modules` (~0.65 MB, including the `/packages` and `/modules` directory records). Rows: `docs/internal/zgc-round-zs-20261014/s6-rs-report.md`.

> **STATUS 2026-10-15 (zs s5/pg): PARTLY -- Both fixes are default ON: `CRATONVM_JMOD_LAZY_INDEX` (`JmodCentralIndex::locate`, classloading/src/jmod_index.rs) and `CRATONVM_JIMAGE_DIRECT_MAP` (`jimage_direct_mapping`, native-io/src/lib.rs). F2 is host-measured: zr orch-notes z8c, -Xmx512m RSS 297-303 vs 430-441 MB, CPU 1.62 vs 1.69 s. F1 is not host-measured (the host image had no `jmods/`), and that same line refutes the "Open residual: CPU" section on Linux. "Not done" items 1-4 are unchanged (JAR central-directory index, JMOD triple name store, jimage `class_to_module` map, lib/modules preference).**

> **STATUS (2026-10-14, zs s1/fp): unchanged.** The class-metadata share moved again (type-map packing, -1.0 KB a class). The allocator share is addressed by the opt-in `CRATONVM_MIMALLOC_FIRST_TOUCH` (`zgc-zs1fp-proposal-mimalloc-first-touch-arena-20261014.md`).

> **STATUS (2026-10-13, zr z11/md): unchanged by z11/md; the per-class share (item 3) moved.**
> z11/md cut the per-class metadata on its own page,
> `zgc-zr8fm-class-metadata-per-class-cost-20261013.md`. One interaction with this page: under
> `CRATONVM_BOOT_CLASS_BYTES_REFETCH` a reader of a boot class's class file (JVMTI, CDS dump,
> `Class.getAnnotatedSuperclass`) re-reads it through the boot class path, i.e. through the
> lazy JMOD index or the jimage reader; those reads are rare and not on a boot path.

> **STATUS (2026-10-13, zr z8/fm): FIXED IN CODE, PENDING HOST PROBE.** Two default-ON fixes
> with `=0` kill switches: `CRATONVM_JMOD_LAZY_INDEX` (a JMOD is indexed from its central
> directory alone) and `CRATONVM_JIMAGE_DIRECT_MAP` (`NativeImageBuffer.getNativeMap` answers
> a direct buffer over a mapping of `lib/modules`). The rows below decide whether either stays
> ON. The per-class metadata share (item 3 of the lane) is on its own page,
> `zgc-zr8fm-class-metadata-per-class-cost-20261013.md`.

Area: footprint, non-GC owners. Parent page: `boot-rss-by-owner-20261013.md`
(z7/rs). That page split a Hello world's ~151 MB working set (HotSpot 25: ~40 MB) by owner.
Two of the owners it named are fixed here.

## 1. The 70 JMODs: ~47 MB of file pages at boot

### What it was

On a JDK that ships `jmods/`, `discover_boot_classpath` (`vm/src/config.rs`) puts all 70 JMODs
on the boot class path. `load_jmod` (`classloading/src/class_path.rs`) maps each one and builds
a `zip::ZipArchive` over it.

z7/rs blamed the `by_index_raw` loop. The loop is not the cause: zip 2.4.2 reads every local
header earlier, in `ZipArchive::new` itself. `get_metadata` calls `central_header_to_zip_file`
for every central-directory record (`zip-2.4.2/src/read.rs:708-710`), and that function calls
`find_data_start` (`read.rs:1259`). `find_data_start` seeks to the entry's local header and
parses it (`read.rs:362-371`).

So the fix direction z7/rs filed (`file_names()` / `name_for_index` instead of
`by_index_raw`) would have measured as no change. Any `ZipArchive` over a JMOD costs about one
page per entry. The same holds for every JAR the class path opens through `ZipArchive`; see
"Not done" below.

### Fix (`CRATONVM_JMOD_LAZY_INDEX`, default ON, `=0` kill)

`classloading/src/jmod_index.rs` reads the end-of-central-directory record and the central
directory, which sit together at the end of the file. It reads nothing else at open. An entry's
local header is read only when that entry is served (`JmodCentralIndex::locate`).

`ClassPathEntry::JmodFile.archive` is now a `JmodArchive`:

- `Central`: the new index;
- `Zip`: the old `ZipArchive`. It is used under the kill switch, and for a JMOD the index does
  not model.

The index refuses these, and that JMOD then keeps `Zip`: ZIP64, encryption, a method other than
stored or deflated, a non-ASCII name, a ZIP64 / AES / Unicode-path extra field, a malformed
extra field, a prefixed archive, and a multi-disk or trailing-garbage end record.

Answers are the same as the `Zip` arm's, by construction and by test:

- names are deduplicated the way `IndexMap::insert` deduplicates them (first position, last
  record);
- a stored entry is served as a range view of `size()` bytes, with no CRC check, as before;
- an owned read is the `compressed_size` bytes at the data start, inflated if deflated, with the
  CRC checked over the whole output and `read_entry_capped`'s cap on it. That is zip's
  `Decompressor` plus `Crc32Reader`.

One designed residual: a JMOD with a corrupt local header used to fail `ZipArchive::new` and was
dropped from the class path whole. Now only the damaged entry fails, when it is served. JDK
JMODs never have one.

### Local evidence (Windows, debug test build, JDK 25.0.3 Microsoft build, 70 JMODs)

- `zr8_fm_every_real_jdk_jmod_serves_the_same_bytes_both_ways` (ignored by default, needs
  `JAVA_HOME`). Every entry of every JMOD gives the same `jmod_find_shared`, `jmod_find_vec`
  and `jmod_has_entry` answers in both arms. It also asserts that the `Central` arm took every
  one of the 70 files, so the measured win is not a fallback.
- `zr8_fm_jmod_open_resident_cost_per_arm` opens all 70 JMODs and serves
  `java/lang/Object.class`. The working-set delta is **14.7 MB with the index vs 65.8 MB with
  `ZipArchive`**:
  - the `ZipArchive` arm's 65.8 MB is about 47 MB of file pages plus its per-entry
    `ZipFileData`;
  - the index arm's 14.7 MB is the three name tables (see "Not done") plus the central
    directories' pages.

End to end, on this lane's debug `cratonvm` build. Run: `Zr7RsBoot hello 6000`, jdk-only,
`-Xmx256m`, 2 interleaved pairs. Value: the working set sampled during the hold.

| arm | working set | private bytes |
|---|---|---|
| default | **163.5 / 163.7 MB** | 403 MB |
| `CRATONVM_JMOD_LAZY_INDEX=0` | **211.6 / 212.0 MB** | 413 MB |

So the index saves about 48 MB of working set and 10 MB of private bytes. The private-bytes
saving is the `ZipArchive`s' `ZipFileData`. Stdout was SAME in both arms (`Hello, World!` /
`zr7rs mode=hello done`, 938 classes).

Peak-working-set samples on this shared box were noisy: the `=0` arm read 102-214 MB. Read the
hold samples, not the peaks.

## 2. A `jrt:/` mount copied the whole runtime image into the Java heap

### What it was

`NativeImageBuffer.getNativeMap` (`native-io/src/lib.rs`, `native_jimage_get_native_map`)
read the ~145 MiB `lib/modules` into a Java `byte[]`. It kept that array behind a global root
for the life of the VM and returned `ByteBuffer.wrap` over it.

HotSpot's native (`NativeImageBuffer.c`) returns libjimage's mapping through
`NewDirectByteBuffer`.

What triggers it is `BasicImageReader.<init>`, which calls `getNativeMap` when
`jdk.image.use.jvm.map` is true (the default) and the reader was loaded by the boot loader. The
callers that reach it are:

- `FileSystems.getFileSystem(URI.create("jrt:/"))`;
- `ImageReaderFactory`, through `SystemModuleFinders` / `ModuleFinder.ofSystem()` when it
  reads module contents;
- javac in-process;
- JDK class-path scanners.

A Hello world does not reach it: z7/rs measured 1.6 MB of Java heap at boot.

The native's old comment said CratonVM's `FileChannel.map` returned a heap snapshot that
`BasicImageReader`'s typed reads did not survive. That is no longer the case:

- the real `FileChannelImpl.map0` (`native-io/src/file_channel.rs` `native_fc_map0`) is a
  real `mmap`;
- the `Unsafe` natives read untagged raw addresses (`unsafe_natives.rs`, "FIX(bug-A)").

### Local evidence (Windows, the 2026-10-03 release binary, unmodified)

Probe `tools/probes/Zr8FmJrt.java`:

- `map` reads `lib/modules` through `FileChannel.map`: absolute `getInt`, a sliced
  `asIntBuffer`, and a bulk `get`. It printed `same=true` on CVM and on HotSpot.
- `jrt` walks `/modules/java.base` and CRC-checks all 7 379 class files. The output was SAME on
  CVM and HotSpot. With the property flipped, the base binary takes the mapped path that the
  fix now takes by default:

| arm | `-Xmx512m` peak WS | `-Xmx64m` |
|---|---|---|
| CVM default (heap copy) | 435 MB, CPU 12.2 s | **rc=1, `OutOfMemoryError: Java heap space`** (native primitive array of length 144908395, in `BasicImageReader.<init>`) |
| CVM `-Djdk.image.use.jvm.map=false` (mapped direct buffer) | 335 MB, CPU 18.2 s | rc=0, 146 MB, SAME |
| HotSpot 25 ZGC | 161 MB, CPU 2.8 s | rc=0, 133 MB |

### Fix (`CRATONVM_JIMAGE_DIRECT_MAP`, default ON, `=0` kill)

`getNativeMap` maps the image read-only, once per path, for the life of the process
(`jimage_direct_mapping`; the mapping is leaked on purpose because Java holds raw addresses into
it). It returns `new DirectByteBuffer(long addr, long cap)`, the JDK's own JNI constructor. That
is HotSpot's answer, so `BasicImageReader` sets `channel = null` as on HotSpot.

The mapping is not per-VM compatibility state: it is the file's bytes, shared by every VM in the
process. A mapping that fails falls back to the copy. The registration is unchanged
(`NativeKind::Bridge` on the ACC_NATIVE `getNativeMap`), so there is no new native and no new
stub.

End to end, on this lane's debug `cratonvm` build, `Zr8FmJrt jrt` at `-Xmx64m`:

| arm | result |
|---|---|
| default, jdk-only | rc=0, `classes=7379 crc=55ab613d object_len=2412 done` (SAME as HotSpot), Java heap used 39 MB |
| default, `--compatible` | rc=0, SAME, heap used 39 MB |
| `CRATONVM_JIMAGE_DIRECT_MAP=0` (positive control) | rc=1, `OutOfMemoryError`, as on the base binary |

### Open residual: CPU

The mapped arm used about 50 % more CPU than the heap copy (18 s vs 12 s on the walk).
`ImageReader` reads its index byte by byte and int by int (`getAttributes`, `ImageStringsReader`,
the redirect table):

- on a heap buffer each read is an array load;
- on a direct buffer each read is a `ScopedMemoryAccess` / `Unsafe` raw-address read.

HotSpot intrinsifies both. Raw-address `Unsafe` gets are evidently not inlined as cheaply here.
That is a JIT / Unsafe-intrinsic item, filed as
`zgc-zr8fm-proposal-raw-address-unsafe-reads-in-the-jit-20261013.md`. The memory and the
`-Xmx` floor are worth more than that CPU, which is why the fix is default ON.

## Rows for the host (Linux)

`P` = compiled `tools/probes` (`Zr7RsBoot`, `Zr8FmJrt`). `CVM` = the round binary. `JH` = the
JDK the VM boots from. Run each row 3 times and take the median.

```
# F0 does this JDK ship jmods? (if 0, F1 is vacuous by construction: the boot path is lib/modules)
ls $JH/jmods 2>/dev/null | wc -l

# F1 Hello, jmods share: kill arm vs default, both modes, vs HotSpot
CRATONVM_JMOD_LAZY_INDEX=0 tools/rss-by-mapping.sh -l cvm-jmodzip -- $CVM -Xmx256m -cp $P Zr7RsBoot hello 1500
tools/rss-by-mapping.sh -l cvm-jmodidx -- $CVM -Xmx256m -cp $P Zr7RsBoot hello 1500
CRATONVM_JMOD_LAZY_INDEX=0 tools/rss-by-mapping.sh -l cvm-c-jmodzip -- $CVM --compatible -Xmx256m -cp $P Zr7RsBoot hello 1500
tools/rss-by-mapping.sh -l cvm-c-jmodidx -- $CVM --compatible -Xmx256m -cp $P Zr7RsBoot hello 1500
tools/rss-by-mapping.sh -l hs-zgc -- java -XX:+UseZGC -Xmx256m -cp $P Zr7RsBoot hello 1500
for a in "" "CRATONVM_JMOD_LAZY_INDEX=0"; do for m in "" "--compatible"; do
  env $a /usr/bin/time -f '%M KB %U %S' $CVM $m -Xmx256m -cp $P Zr7RsBoot hello 0; done; done
/usr/bin/time -f '%M KB %U %S' java -XX:+UseZGC -Xmx256m -cp $P Zr7RsBoot hello 0

# F2 jrt mount: kill arm vs default, both modes, vs HotSpot, at two heap sizes
for x in 512m 64m; do for a in "" "CRATONVM_JIMAGE_DIRECT_MAP=0"; do for m in "" "--compatible"; do
  env $a /usr/bin/time -f "x=$x a=$a m=$m %M KB %U %S" $CVM $m -Xmx$x -cp $P Zr8FmJrt jrt; done; done
  /usr/bin/time -f "x=$x hs %M KB %U %S" java -XX:+UseZGC -Xmx$x -cp $P Zr8FmJrt jrt; done
tools/rss-by-mapping.sh -m zr8fm-hold -l cvm-jrt -- $CVM -Xmx512m -cp $P Zr8FmJrt jrt 1500
CRATONVM_JIMAGE_DIRECT_MAP=0 tools/rss-by-mapping.sh -m zr8fm-hold -l cvm-jrt-copy -- $CVM -Xmx512m -cp $P Zr8FmJrt jrt 1500
$CVM -cp $P Zr8FmJrt map ; java -cp $P Zr8FmJrt map
```

**Pass criteria.**

- **Output SAME.** Every row prints the same stdout as HotSpot, in both modes:
  - `Zr7RsBoot hello`: `Hello, World!` and `zr7rs mode=hello done`;
  - `Zr8FmJrt jrt`: `zr8fm jrt classes=<n> crc=<hex> object_len=<n> done`, with the JDK's own
    `n`/`crc`;
  - `Zr8FmJrt map`: `same=true`.
- **F1.** On a JDK with `jmods/`, the default's `jdk:jmod` share (`rss-class`) falls by about
  40 MB or more against the `=0` arm, and `/usr/bin/time %M` falls by a similar amount.
- **F2.** At `-Xmx512m` the default's `%M` is at least 100 MB below the `=0` arm. At `-Xmx64m`
  the default runs to `done` (HotSpot does too) and the `=0` arm exits with
  `OutOfMemoryError`.

**Positive controls.**

- The `=0` arms reproduce the old shares: `jdk:jmod` ~45 MB, and the 145 MiB copy in
  `anon:other` / the Java heap.
- The `-Xmx64m` `=0` row must OOME. If it does not, the copy was never made on that row, and
  the F2 comparison is vacuous.

**Kill-switch control.** The `=0` arms of the round binary must match the base binary (dev
before z8) within noise.

**Flip rule.**

- `CRATONVM_JMOD_LAZY_INDEX` stays ON if F1 is SAME and the `jdk:jmod` share drops. Turn it OFF
  only on a DIFF.
- `CRATONVM_JIMAGE_DIRECT_MAP` stays ON if F2 is SAME. If it DIFFs in either mode, flip it to
  OFF and file the read path that disagreed.

## Not done (filed here)

1. **Every `ZipArchive` pays the same local-header walk at open.** That covers application
   JARs, the class path's `open_jar_archive`, and `OuterArchiveIndex`. A fat jar with 10 000
   entries faults in about 10 000 pages at open. The fix would be the same central-directory
   index applied to JARs. Those carry more of what the index refuses (ZIP64, data descriptors
   are fine, signed JARs are fine), and the JAR path has its own index
   (`build_archive_entry_index`), so it is a separate change. The deciding row: a 10 000-entry
   jar's open, with `rss-by-mapping.sh` `files:jar` against HotSpot.
2. **The JMOD index holds each name three times** (`all_entry_names`, `class_entry_index` with
   the `classes/` prefix stripped, and the index's own `by_name`). That is most of the arm's
   14.7 MB. One name store with the other two as views (an index into `all_entry_names` keyed by
   a hash) would save about 6-8 MB.
3. **A JDK without `jmods/`** boots from `lib/modules`. There `load_jimage` builds a ~30 000-entry
   `class_to_module` `String` map (z7/rs item 3). That is unchanged here.
4. **Preferring `lib/modules` over `jmods/` when both exist** (HotSpot never reads a JMOD) was
   considered and not done:
   - it changes the boot source for every JDK that ships both, including resource URLs and
     module descriptors, which is a larger behavioural surface than this lane can probe;
   - the index already removes the JMODs' page cost.
