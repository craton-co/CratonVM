---
name: jarfile-accessors-stat-the-file-on-every-call-FIXED-20260811
description: FIXED 2026-08-11. Every java.util.jar.JarFile accessor rebuilt its jar-cache key from a live std::fs::metadata call, which on Windows opens a file handle - 20-54 us per call, measured, against a 0.3 us cache lookup. Jasper's TLD scan reaches it up to three times per jar entry on a multi-release jar, once per embedded-container start. Memoising the mtime probe and re-taking it only when an archive is OPENED (HotSpot's own ZipFile.Source model) took a 130-jar/32491-entry classpath walk from 789-2807 ms to 140-155 ms, flat, against HotSpot's 40-58 ms.
metadata:
  type: fixed-bug
  area: native-io, jar, zip, throughput, tomcat, jetty
---

# A stat per `JarFile` accessor call

**FIXED 2026-08-11.** Found while re-measuring the two Spring Boot pages this
change retires
(`fixed-suite-bugs/springboot/tomcat-jetty-servletwebserverfactorytests-300s-budget-overrun-FIXED-20260811.md`
and its Pulsar sibling).

## The defect

`native-builtins/src/phases_late/jar_manifest.rs` owns the
`java.util.jar.JarFile` natives — `getEntry`, `getJarEntry`, `entries`,
`size`, `getInputStream`, `getManifest`. Each of them resolves its data
through `jar_contents_cached(path)` or `jar_entry_bytes_cached(path, entry)`,
and both of those built their cache key like this:

```rust
let mtime = std::fs::metadata(path).and_then(|m| m.modified()) … ;
let key = format!("{path}\u{0}{mtime}");
```

The parse is cached. **The freshness check was not.** On Windows
`std::fs::metadata` opens a file handle (`CreateFileW` +
`GetFileInformationByHandle` + `CloseHandle`); on the development host that
measured **20-54 us**, against roughly 0.3 us for the hash lookup it guarded.

Nothing about the shape of the code says so. The comment above the cache says
"O(1) per call", and it is — the *cache* is. The stat sits one line above it.

## What it cost, on the workload that found it

Jasper's TLD scan drives Tomcat's `org.apache.tomcat.util.scan.JarFileUrlJar`,
whose `nextEntry()` is, for a **multi-release** jar:

```java
entries = jarFile.entries();
…
entry = jarFile.getJarEntry(entry.getName());   // once PER ENTRY
```

and `getJarEntry` reaches `jar_contents_cached` up to three times —
`p59_jar_is_multi_release`, the `../../../apps/META-INF/versions/N/…` search, and the entry
lookup itself. Tomcat runs that scan once per embedded-container start;
`TomcatServletWebServerFactoryTests` starts 121 of them over a 130-jar test
classpath, 38 of which really are multi-release.

`probes/TldJarScanProbe.java` reproduces exactly that walk outside Tomcat.
130 jars, 32 491 entries, 11 751 re-lookups, five rounds:

| round | before | after | HotSpot |
|---|---:|---:|---:|
| 0 | 789 ms | 155 ms | 58 ms |
| 1 | 1000 ms | 144 ms | 41 ms |
| 2 | 1720 ms | 140 ms | 40 ms |
| 3 | 2133 ms | 150 ms | — |
| 4 | 2807 ms | 151 ms | — |

Two things are worth reading off that table beyond the 5-18x. The before
column **grows every round** — the stat is not just expensive, it gets more
expensive as the process ages — and the after column does not. And the gap to
HotSpot goes from 20-50x to ~3x, which moves this workload out of the
"pathological" bracket and into the ordinary interpreter-throughput one.

The phase split (`openMs` / `enumMs` / `lookupMs` in the probe's own output)
is what named the site: opening the 130 jars was **faster** than HotSpot
(15-23 ms vs 31-38 ms), the enumeration was ~20x, and the `getJarEntry`
re-lookups were 560-2474 ms of the 648-2807 ms total.

`probes/HandleCostProbe.java` isolates the per-call figure, and its control is
the part that makes it a diagnosis rather than a guess: `ZipFile.getName()` is
the one accessor on this class that does **not** consult the jar cache.

| call | before | after | HotSpot |
|---|---:|---:|---:|
| `getName()` (no cache lookup — the control) | 349-564 ns | 227-364 ns | 12-71 ns |
| `size()` | 20 392-53 965 ns | 712-993 ns | 24-54 ns |
| `getJarEntry()` hit | 20 447-60 605 ns | 2 281-2 842 ns | 70-735 ns |
| `getJarEntry()` miss | 33 416-102 506 ns | 1 568-1 850 ns | 50-134 ns |

`getName` at 350 ns beside `size` at 20 000 ns, in the same native file with
the same handle lookup and the same table lock, is the whole argument. The
only thing `size` does that `getName` does not is ask the cache.

Two alternatives were killed before the stat was believed:

* **`ctx.read_string` on a long path.** Copying the jar to `C:\j.jar` (15
  chars) and re-running against the 171-char Gradle-cache path moved `size`
  from 47-50 us to 43-51 us. Not the string.
* **The monitor on a `synchronized` accessor.** `ZipFile.size()` is
  `synchronized(this)` in the JDK source, so an uncontended-monitor cost was a
  live candidate — and a topical one, a leaked JMX owned-monitor set having
  been 54% of a JIT run the day before. `probes/SyncCostProbe.java` prices an
  uncontended acquire/release pair on this VM at ~650 ns (synchronized method
  1574-1765 ns against a plain call at 878-1066 ns; a `synchronized` block
  732-832 ns), not 20 us — and the JDK method is not `ACC_SYNCHRONIZED`
  anyway, so a native replacing it takes no monitor at all.

## The fix

`jar_path_mtime(path)` memoises the probe per path.
`jar_cache_revalidate(path)` re-takes it, and is called from the two `JarFile`
constructors — the moment an archive is *opened*.

Both caches keep their `(path, mtime)` keys untouched, so a changed mtime
still produces a different key and a fresh parse. Only the *frequency of
asking the filesystem* changed.

A failed stat is deliberately not memoised: a path that does not exist yet (a
jar about to be written) must not be pinned to 0 for the life of the process.

## What was given up, and why it is not a weakening

Before: a jar rewritten on disk was picked up by the very next accessor call.
After: by the next *open*.

That is HotSpot's behaviour, not a relaxation of it. The JDK's
`ZipFile.Source` cache is keyed on `(file, lastModified, size)` sampled in
`Source.get()` at open time and never re-sampled, and the open file is held
for the life of the `ZipFile`. Re-stat-per-accessor was stricter than the
thing it was emulating, and nobody was paying for the strictness on purpose.

`jar_mtime_memo_tests` in the same file pins both halves — that the accessor
path does **not** re-stat, and that `jar_cache_revalidate` does — using
explicitly stamped mtimes so the test does not ride on the host filesystem's
timestamp granularity. A second test pins the non-memoisation of a failed
stat, which is the one way this optimisation could have made a path that
appears later permanently invisible.

## A second, smaller change in the same area

`native-io/src/zip_real_jar.rs`'s `getEntry` reached the archive through
`ZipArchive::by_index`, which builds the entire decompressor chain
(`BufReader` + `Decompressor` + `Crc32Reader` — an inflate window and trees)
purely so the call could then ask for `name()` and `size()`. Nothing about
that reader was used. Per-index metadata is now cached in `JarState::meta`
and read once with `by_index_raw`, which skips `make_reader`; `entries()`
shares the same cache, so a second enumeration of an open archive does no I/O
at all.

Honesty about its measured value: this did **not** move either probe, because
`java.util.jar.JarFile` is owned by `jar_manifest.rs`'s registrations (they
run later and `overwrote` native-io's — visible in
`--dump-native-registry` as `"overwrote": "bridge"`), so the probes never
reach this code. It is kept because it is strictly less work on the plain
`java.util.zip.ZipFile` path, not because a number moved.

**That registrar overlap is the trap worth remembering.** The first version of
this fix went entirely into `zip_real_jar.rs`, compiled, ran, and changed
nothing measurable — because for `JarFile` that file is dead code.
`--dump-native-registry` names the owner in one run; guessing from the file
that *looks* like the JarFile implementation does not.

## What this does not fix

The classes that found it are still slower than HotSpot; see the two retired
pages for their post-fix wall-clock and for the per-class budgets the suite
now carries. The remaining TLD-scan cost is Xerces/Digester XML parsing of the
`.tld` files themselves, which is ordinary interpreter throughput and belongs
to `known-issues/tomcat/!webapp-deploy-annotation-scan-interpreted-226x.md`.
