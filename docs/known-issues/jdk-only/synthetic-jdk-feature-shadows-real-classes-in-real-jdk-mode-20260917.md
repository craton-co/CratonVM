# A `--features synthetic-jdk` binary shadows real JDK classes in real-JDK mode

**Follow-up, 2026-09-17 (same day): 17 of 20 classes fixed, 3 confirmed
legitimate shims.** Filed after the first of them took out the entire
hibernate-reactive suite — 188 of 188 classes crashed before a single test
method ran. Every remaining row from "The whole family: 20 classes" below was
individually investigated (not blanket-applied): `java/io/BufferedReader`,
`BufferedWriter`, `CharArrayReader`, `CharArrayWriter`, `LineNumberReader`,
`Reader`, `Writer`, `java/nio/channels/FileChannel` (the `lock`/`tryLock`
rows — `open` was already fixed), `FileLock`, `java/nio/CharBuffer`,
`com/sun/jmx/mbeanserver/ConvertingMethod`/`MXBeanMapping`/
`MXBeanMappingFactory`/`DefaultMXBeanMappingFactory`, `java/util/Spliterators`,
and `java/lang/System` (`initPhase1`/`2`/`3`, found during this pass — not in
the original 20, see "One more found" below) all had **concrete evidence** of
a genuine shadow (mirroring `InputStreamReader`'s own evidence bar, not just
"the doc lists it") and got both halves of the fix: the site-level
`!r.drops_real_layout_synthetic()` guard, and — except where noted — a
matching entry in `native-api/src/registry.rs`'s central drop filter.

Three were investigated and found to be **legitimate shims, not defects** —
the doc's own carve-out ("a feature-gated registration over a class whose
real bytecode needs a native this VM does not have"), confirmed rather than
assumed: `java/lang/management/MemoryUsage` (real-field-name reads, output
already byte-identical to HotSpot's), `java/util/concurrent/ScheduledFuture`
(`get`/`get(timeout)` have no real-bytecode fallback anywhere in the tree —
dropping it trades a working call for a guaranteed `AbstractMethodError`),
and `javax/naming/InitialContext` (WildFly's JNDI backend has no real
provider chain to fall back to; this is the same "permanent bridge, no
real-bytecode fallback" bucket as the pre-existing JMX/`Function$Identity`
precedent). None of these three were touched.

**One more found, not in the original 20:** `java/lang/System`'s
`initPhase1`/`initPhase2`/`initPhase3` (`native-builtins/src/lib.rs`,
`register_essential_natives_with_shims`) had the exact same shape — a comment
saying "only override in synthetic-jdk mode... real-JDK mode the actual JDK
bytecode runs", enforced by `#[cfg(feature = "synthetic-jdk")]` alone.
`native_system_init_phase1` unconditionally writes `System.out`/`err` static
fields and stamps a `Charset` onto them — real bootstrap side effects the
real bytecode's own `initPhase1`, backed by the T14 natives the comment
names, already performs. Fixed the same way.

All fixes verified: `cargo check --workspace` and `--features synthetic-jdk`
both clean; `cargo test -p cratonvm-native-builtins --test registrar_drift
--test registrar_reachability --test duplicate_registration_gate --test
stub_ratchet --test registry_contracts` all green; `cargo test -p cratonvm-vm
--lib --features synthetic-jdk` (the full ~4,300-test suite) showed zero
regressions.

---

**Original filing, below — now historical.** "Open, 20 classes / 72
registrations. One of them is fixed; the rest are enumerated below and are
not." Filed 2026-09-17 after the first of them took
out the entire hibernate-reactive suite — 188 of 188 classes crashed before a
single test method ran.

## The configuration

Three things have to be true, and the third is the one nobody checks:

1. the binary was built `--features synthetic-jdk` (**not** the default — no
   workspace member enables it, and no build script in this repo passes it);
2. it is run against a real JDK (the default: `--java-home`, or autodetected);
3. a registrar guards its synthetic surface with `#[cfg(feature =
   "synthetic-jdk")]` **alone**.

`NativeMethodRegistry::drops_real_layout_synthetic`'s own doc has said since
2026-08-12 why (3) is not a guard:

> A `#[cfg(feature = "synthetic-jdk")]` guard is NOT equivalent and must not be
> used for this: the Cargo feature decides what is COMPILED, the launcher flag
> decides which CLASS LIBRARY loads, and a feature-enabled binary run
> `--real-jdk` satisfies the cfg while facing real JDK classes.

Most registrars pair the cfg with the runtime predicate. The ones below do not.

## What it cost, concretely

`native-builtins/src/servlet.rs`'s synthetic Reader-stack block registered
`java/io/InputStreamReader.<init>` over the real class. The shim parks the
wrapped `InputStream` in slot 0; on a real JDK 25 `InputStreamReader`, slot 0 is
`Reader.lock`. So it clobbered the monitor object and — decisively — never
created the real `sd` (`sun.nio.cs.StreamDecoder`) that the genuine constructor
builds via `StreamDecoder.forInputStreamReader`.

The shim registers `read()I` but **not** `read([CII)I`. The bulk read is the one
`BufferedReader.fill` calls, so every `readLine()` in the image kept running real
bytecode straight into the null:

```
java/lang/NullPointerException: Cannot invoke
  "sun.nio.cs.StreamDecoder.read(char[], int, int)" because "this.sd" is null
  at java/io/InputStreamReader.read(InputStreamReader.java:183)
  ... at java/util/ServiceLoader$LazyClassPathLookupIterator.parseLine
```

`ServiceLoader`'s own provider-file parse goes through `BufferedReader.readLine`,
so nothing that loads a service provider could start. That is why the count was
188/188 and the wall time was two minutes rather than nine.

Reproducer, four lines, no warm-up, `--nojit` included — this is not a JIT path:

```java
InputStreamReader r = new InputStreamReader(new ByteArrayInputStream(bytes), UTF_8);
r.read(new char[32], 0, 32);   // NPE: this.sd is null
```

The 2x2 that isolates it, all at the same commit:

| | default build | `--features synthetic-jdk` |
|---|---|---|
| debug | passes | **fails** |
| release | passes | **fails** |

The release profile is innocent; the feature is the whole difference.

## Fixed: one of twenty

`java/io/InputStreamReader` is closed, in both halves:

* `native-builtins/src/servlet.rs` now carries the site-level
  `!r.drops_real_layout_synthetic()` guard that its `native-io` twin grew on
  2026-08-12 (W7-50);
* `java/io/InputStreamReader` joins `java/io/StringReader`,
  `java/io/PipedInputStream`/`PipedOutputStream`, `java/util/EnumSet` and
  `java/security/Permissions` in `NativeMethodRegistry::register`'s real-layout
  drop filter — the mechanism-level half, so the next registrar to repeat this
  is refused instead of shipping the same outage.

`StringReader` is in that filter for *literally this defect, one class over*:
"the fake constructor leaves that delegate null and real `mark()`/`read()`
immediately NPE". Two occurrences of one defect is the signal `AGENTS.md` names.

## The whole family: 20 classes

The `java/io/InputStreamReader` row is the one fixed above and is kept here for
shape — it is what a row that has been read and confirmed looks like. The other
nineteen are open.

Measured by diffing `--dump-native-registry` between a `--features
synthetic-jdk` build and a default build of the same commit, both run
real-JDK. Every row is a registration the feature build puts over a real JDK
class and the default build does not.

| class | rows | registered by | real bytecode seen in the probe run | 2026-09-17 follow-up |
|---|---|---|---|---|
| `com/sun/jmx/mbeanserver/ConvertingMethod` | 1 | `native-builtins/src/jmx_openmbean.rs` | not loaded in that run | **FIXED** — cfg-only gate, no runtime component |
| `com/sun/jmx/mbeanserver/DefaultMXBeanMappingFactory` | 2 | `native-builtins/src/jmx_openmbean.rs` | not loaded in that run | **FIXED** |
| `com/sun/jmx/mbeanserver/MXBeanMapping` | 2 | `native-builtins/src/jmx_openmbean.rs` | not loaded in that run | **FIXED** |
| `com/sun/jmx/mbeanserver/MXBeanMappingFactory` | 1 | `native-builtins/src/jmx_openmbean.rs` | not loaded in that run | **FIXED** |
| `java/io/BufferedReader` | 2 | `native-builtins/src/phases_late/nio_file.rs` | yes | **FIXED** — measured (H2 mark/reset BOM-skip) |
| `java/io/BufferedWriter` | 6 | `native-builtins/src/phases_late/nio_file.rs` | yes | **FIXED** — measured (probe: unbuffered writes) |
| `java/io/CharArrayReader` | 5 | `native-io/src/lib.rs` | not loaded in that run | **FIXED** — slot-0 collides with `Reader.lock` |
| `java/io/CharArrayWriter` | 9 | `native-io/src/lib.rs` | not loaded in that run | **FIXED** — same collision (Jasper JSP → empty output) |
| `java/io/InputStreamReader` | 5 | `native-builtins/src/servlet.rs` | yes — **FIXED**, see above | (fixed same day as filing) |
| `java/io/LineNumberReader` | 5 | `native-io/src/lib.rs` | not loaded in that run | **FIXED** — same collision |
| `java/io/Reader` | 2 | `native-io/src/lib.rs` | yes | **FIXED** — base-class side-table leak onto subclasses |
| `java/io/Writer` | 1 | `native-io/src/lib.rs` | yes | **FIXED** — same shape, wrong-slot writes |
| `java/lang/System` | 2 | `native-builtins/src/lib.rs` | yes | **FIXED** — `initPhase1/2/3`, found this pass (not the original 2 rows — see "One more found" above) |
| `java/lang/management/MemoryUsage` | 1 | `native-builtins/src/jmx.rs` | not loaded in that run | **Legitimate shim** — real-field reads, output already HotSpot-identical |
| `java/nio/CharBuffer` | 2 | `native-builtins/src/phases_late/charset_buffers.rs` | yes | **FIXED** — wrong concrete class (mutable copy vs. real `StringCharBuffer` view) |
| `java/nio/channels/FileChannel` | 2 | `native-io/src/lib.rs` | not loaded in that run | **FIXED** — `lock`/`tryLock`, scoped by method name |
| `java/nio/channels/FileLock` | 7 | `native-io/src/lib.rs` | not loaded in that run | **FIXED** — out-of-bounds slot write, measured (H2 `OverlappingFileLockException`) |
| `java/util/Spliterators` | 1 | `native-collections/src/lib.rs` | not loaded in that run | **FIXED** (site-level guard only — see note below) |
| `java/util/concurrent/ScheduledFuture` | 5 | `native-collections/src/lib.rs` | not loaded in that run | **Legitimate shim** — `get`/`get(timeout)` have no real fallback |
| `javax/naming/InitialContext` | 11 | `native-builtins/src/wildfly_naming.rs` | not loaded in that run | **Legitimate shim** — WildFly JNDI backend has no real provider chain |

`Spliterators` deliberately got the site-level guard only, not the central
`registry.rs` filter: `native-builtins/src/phases_late/streams.rs`'s
`register_p69_spliterator` registers the identical triple unconditionally
(part of the always-on essential layer, and what default builds already
ship), and a class-keyed central filter would also suppress *that*
registration in a plain default build — trading a long-shipping shim for
untested real bytecode VM-wide, a materially bigger and unverified change
than this pass's scope.

**Read the last column carefully.** "not loaded in that run" means the probe
program never touched the class — it is *unproven*, not *safe*. The second
failure found after fixing `InputStreamReader` was
`java/util/Spliterators.emptySpliterator`, which that column reports as "not
loaded": the same hibernate-reactive class then died with
`NoSuchMethodError: java.util.Spliterators$EmptySpliterator.tryAdvance`.
Classifying a row needs a workload that loads the class, not this table.

## What the table does NOT say

It does not say all 72 are defects. A feature-gated registration over a class
whose real bytecode needs a native this VM does not have is a legitimate shim.
The defect is specifically: shadowing real bytecode that would have worked,
with a surface that assumes a synthetic field layout. Each row needs reading.

## How to tell whether this is biting you

`--dump-native-registry <file>` and look for the class. `invocations > 0` on a
row for a real `java.*` class in a real-JDK run is the tell. Or, faster: build
without the feature and see whether the symptom goes.

## The remedy that is not a code change

**Do not build `--features synthetic-jdk` unless you mean to run
`--synthetic-jdk`.** It is not in any default feature set, no build script here
passes it, and the configuration "feature compiled in, real JDK loaded" is the
one this page is about. A default build of the same commit ran
`org.hibernate.reactive.BeforeExecutionIdGeneratorTypeTest` 2/2 green against
the live database.

## Where to start

* `native-api/src/registry.rs` — `drops_real_layout_synthetic`, its doc, and
  the real-layout drop filter in `register()` — now ~30 entries after this
  pass, up from the original ~15.
* `native-io/src/lib.rs` — four correctly-paired call sites to copy from
  (now several more, in this file and `native-builtins/src/phases_late/
  nio_file.rs`, `charset_buffers.rs`, `jmx_openmbean.rs`, `lib.rs`, and
  `native-collections/src/lib.rs`).
* `native-builtins/tests/stub_ratchet.rs` — the existing instrument for
  counting registrations per configuration; it still does not have a
  `--features synthetic-jdk` + real-JDK arm as of this follow-up. That gap
  itself was NOT closed in this pass (out of scope — it needs a new gate,
  not a registration fix) and remains the highest-leverage next step to stop
  this class of defect from recurring undetected.
