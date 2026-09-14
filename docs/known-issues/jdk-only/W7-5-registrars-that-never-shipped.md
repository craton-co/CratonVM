# W7-5 — 301 native registrars that are not in the shipping binary, and the four reasons that is sometimes right

Status: **census complete; one registrar wired; the §6.3 ratchet written and
the `ConcurrentSkipListMap` verdict settled, 2026-08-12** (§6.3.1, §6.4.1).
**Source-re-verified 2026-08-12 — see §8: every verdict survives, §4.3/§6.2's
`forEachOrdered` gap is CLOSED, §5.1's mechanism needs re-scoping to the
registrar's accessors, and §5.3's `register_tls_impl_natives` row is wrong.**
Wave 7, lane W7-5. Nothing was built or run on 2026-08-12.

This started from one bug. `IntStream.summaryStatistics()` was killing whole probe
runs with `AbstractMethodError: … has no Code attribute`, and the lane fixing it
found that the registration which *does* exist has never been in the shipping
binary: `register_phase56_stream_extras`' only non-test caller is
`lib.rs::register_synthetic_overrides`, which is `#[cfg(feature = "synthetic-jdk")]`
— not a default feature. This record is the sweep that asked how many more there
are.

The answer is 301, carrying 4,264 `register(...)` call sites. **Most of them are
correctly gated.** A list that treats all 301 as bugs is worth nothing, so the
whole point of this record is §3: the four reasons a registrar can be
feature-gated, three of which are good ones.

Predecessors: `W6-5-vacuous-tests.md` (the species — code that looks like
coverage and is not), `W6-4-duplicate-registration-gate.md` (the duplicate
census, and why it cannot see any of this),
`W7-2-primitive-stream-terminal-surface.md` (the bug this lane came from).
Background you need before reading any of it:
`docs/architecture/natives-over-real-jdk-classes.md` §1 (registration is the
gate), §2 (a Cargo feature is not a runtime mode), §3 (last-registration-wins),
§5 (a slot index against a real layout is heap corruption).

---

## 0. What was counted, and the one place my own count was wrong

**Method.** Every `fn` in `native-builtins/`, `native-collections/`,
`native-io/`, `native-awt/` and `vm/src/` whose signature mentions
`NativeMethodRegistry` and which is not inside `#[cfg(test)]` or carrying
`#[test]` — 825 functions. Call sites were resolved into a graph, each edge
labelled with the `#[cfg]` attributes on the calling function plus any
`#[cfg(..)] { … }` block enclosing the call. A node is **live** iff it is
reachable from an ungated call site through ungated edges only. `awt`,
`management`, `zgc` and `mimalloc` are treated as present, because they are in
the default feature sets of `cratonvm-vm` / `cratonvm-cli`; `synthetic-jdk`,
`app-stubs`, `legacy-synthetic-crypto`, `synthetic-quarkus-arc`,
`experimental-*` and `gpu-offload` are not.

| | count |
|---|---:|
| registrar functions | 825 |
| live in the default `cratonvm-cli` build | 524 |
| **absent from the default build** | **301** |
| …of those, reachable only via `register_synthetic_overrides` | 278 |
| `register(...)` call sites inside the dead set | 4,264 |
| `register(...)` call sites inside the live set | 9,959 |

**The caveat that has to travel with those numbers.** A `register(` call site is
not a triple. It undercounts loops — `register_synthetic_overrides` registers
four constructors across a 30-class array from four call sites, i.e. 120 triples
from 4 — and it cannot resolve a descriptor built with `format!`.

I hit the second one head-on and it is worth writing down, because it is the
campaign's documented failure mode arriving from the inside. Comparing
`register_phase56_stream_extras`' registrations against the whole live set gave
**32 triples registered nowhere else**. Twelve of those 32 are in fact already
live: `streams.rs::register_basestream_mode_overrides` registers
`sequential` / `parallel` / `unordered` / `onClose` over a nine-entry
`(class, return-descriptor)` table with `let sig_return_self = format!("(){}", ret);`,
and `isParallel()Z` over a five-class list. A literal-scanning comparison cannot
see a `format!`. The true gap is **20**, not 32 — a 60% over-statement, in the
same direction the campaign README warns about. Every gap count below is
post-correction.

---

## 1. The mechanism, in one paragraph

`register_builtins` → `register_synthetic_overrides` is
`#[cfg(feature = "synthetic-jdk")]`, `synthetic-jdk` is in no crate's default
feature set, and `vm/src/native/builtins.rs` supplies
`#[cfg(not(feature = "synthetic-jdk"))]` no-op shims for both names so inline
call sites still resolve. So in a plain `cargo build -p cratonvm-cli` those
registrars **do not compile in at all** — which is a different thing from being
present and declined by policy. `--jdk-only` / `--real-jdk` are runtime policy;
`--features synthetic-jdk` is a build-time decision. Conflating the two is how
this survived: a reader who sees `register(...)` calls for
`java/util/stream/IntStream` reasonably concludes the surface is covered and
that strict mode merely chooses whether to use it. It is not there to choose.

**Yes, there is a shim.** For the entry points only. Every registrar *below*
`register_synthetic_overrides` — all 278 — has no shim and no substitute; it
simply is not in the binary.

---

## 2. The census

Rows are the 69 direct callees of `register_synthetic_overrides` that have a
non-empty dead subtree, i.e. the roots of the dead forest. `fns` counts the
registrar functions in that subtree, `triples` the `register(` call sites in it.
The three classification columns are what decide the verdict, and they are
explained in §3.

* **on VM-minted** — registrations whose class is one CratonVM allocates
  instances of itself, via `try_alloc_synthetic` / `try_alloc_concurrent_synthetic`
  on the *real* JDK type. These are the load-bearing ones.
* **3rd-party** — `org/…`, `com/…`: application classes, not JDK surface.
* **absent-in-JDK25** — the class name does not exist in JDK 25 (`javap` says
  so). Dead in every mode, gate or no gate.

| registrar | file:line | fns | triples | on VM-minted | 3rd-party | absent-in-JDK25 | top classes |
|---|---|---:|---:|---:|---:|---:|---|
| `register_enterprise_final_natives` | `lib.rs:40114` | 4 | 204 | 0 | 0 | 0 | `java/lang/Class`, `java/util/Arrays` |
| `register_phase54_natives` | `phases_early.rs:18767` | 4 | 182 | 0 | 0 | 0 | `java/net/HttpURLConnection`, `java/util/concurrent/atomic/AtomicInteger` |
| `register_phase58_natives` | `phases_late.rs:1669` | 7 | 148 | 45 | 0 | 0 | `java/util/concurrent/CompletableFuture`, `java/nio/channels/Selector` |
| `register_phase51_natives` | `phases_early.rs:6606` | 10 | 137 | 0 | 0 | 0 | `java/util/Calendar`, `java/util/GregorianCalendar` |
| `register_vector_api_natives` | `vector_api.rs:1871` | 11 | 136 | 0 | 0 | 0 | `jdk/incubator/vector/*` |
| `register_phase72_natives` | `phases_late.rs:7694` | 7 | 129 | 0 | 19 | 0 | `java/net/Socket`, `java/net/MulticastSocket` |
| `register_phase52_natives` | `phases_early.rs:11289` | 13 | 127 | 0 | 0 | 0 | `java/time/OffsetDateTime`, `java/time/Month` |
| `register_phase59_natives` | `phases_late.rs:1873` | 6 | 126 | 14 | 0 | 0 | `java/lang/invoke/VarHandle`, `java/lang/Module` |
| `register_time_extras_natives` | `util_time.rs:2070` | 1 | 117 | 0 | 0 | 0 | `java/time/LocalDateTime`, `java/time/ZonedDateTime` |
| `register_tls_natives` | `tls.rs:3128` | 13 | 110 | 0 | 0 | 0 | `javax/net/ssl/SSLEngine`, `java/security/KeyStore` |
| `register_classloader_natives` | `classloader.rs:9649` | 2 | 109 | 0 | 0 | 0 | `java/io/DataInputStream`, `java/lang/ClassLoader` |
| `register_serialization_natives` | `serialization.rs:4965` | 15 | 106 | 0 | 0 | 0 | `java/io/ObjectInputStream`, `java/io/ObjectOutputStream` |
| `register_phase67_natives` | `phases_late.rs:4275` | 8 | 105 | 1 | 0 | **33** | `jdk/incubator/concurrent/StructuredTaskScope` |
| `register_phase71_natives` | `phases_late.rs:5997` | 5 | 104 | 0 | 0 | 0 | `java/util/zip/Deflater`, `java/util/zip/Inflater` |
| **`register_phase56_natives`** | **`phases_late/streams.rs:24`** | **4** | **94** | **53** | 0 | 0 | `java/util/stream/Stream`, `java/util/stream/Collectors` |
| `register_phase53_natives` | `phases_early.rs:14106` | 7 | 93 | 0 | 0 | 2 | `javax/crypto/Cipher`, `java/security/Signature` |
| `register_phase55_natives` | `phases_late.rs:262` | 5 | 90 | 16 | 0 | 0 | `java/util/concurrent/CompletableFuture`, `java/lang/reflect/Modifier` |
| `register_http2_natives` | `http2.rs:2687` | 10 | 90 | 0 | 0 | 0 | `java/net/http/HttpClient`, `java/net/http/HttpRequest$Builder` |
| `register_phase50_natives` | `phases_early.rs:3321` | 7 | 86 | 0 | 0 | 0 | `java/util/BitSet`, `java/util/IdentityHashMap` |
| `register_phase61_natives` | `phases_late.rs:2518` | 6 | 86 | 0 | 0 | 0 | `java/text/DecimalFormatSymbols`, `java/lang/ClassLoader` |
| `register_phase68_natives` | `phases_late.rs:5103` | 3 | 85 | 0 | 25 | 0 | `org/w3c/dom/Element`, `javax/xml/parsers/DocumentBuilderFactory` |
| `register_classfile_api_natives` | `classfile_api.rs:1083` | 12 | 83 | 0 | 0 | 0 | `java/lang/classfile/CodeBuilder`, `java/lang/classfile/ClassModel` |
| `register_phase62_natives` | `phases_late.rs:3198` | 7 | 83 | 8 | 0 | 0 | `java/time/Year`, `java/util/zip/ZipEntry` |
| `register_slf4j_natives` | `logging_shims.rs:2318` | 1 | 78 | 0 | 52 | 0 | `org/slf4j/Logger`, `org/apache/logging/log4j/Logger` |
| `register_phase64_natives` | `phases_late.rs:3475` | 8 | 78 | 5 | 0 | 0 | `java/util/HexFormat`, `java/util/SequencedMap` |
| `register_phase69_natives` | `phases_late.rs:5183` | 8 | 68 | 14 | 0 | 0 | `java/util/Spliterator`, `java/net/http/WebSocket` |
| `register_phase65_natives` | `phases_late.rs:4129` | 8 | 66 | 4 | 0 | 0 | `java/time/format/DateTimeFormatterBuilder`, `java/util/concurrent/PriorityBlockingQueue` |
| `register_jdk25_concurrency_natives` | `jdk25_concurrency.rs:2002` | 1 | 65 | 0 | 0 | 0 | `java/util/concurrent/*` |
| `register_pe_panama` | `panama.rs:281` | 7 | 63 | 0 | 0 | 0 | `java/lang/foreign/ValueLayout`, `java/lang/foreign/Arena` |
| `register_phase70_natives` | `phases_late.rs:5945` | 5 | 62 | 0 | 0 | 0 | `java/io/ObjectOutputStream`, `java/io/ObjectInputStream` |
| `register_phase60_natives` | `phases_late.rs:2021` | 6 | 54 | 0 | 0 | 0 | `java/net/http/HttpRequest$Builder`, `java/util/concurrent/SubmissionPublisher` |
| `register_phase63_natives` | `phases_late.rs:3248` | 7 | 52 | 3 | 0 | 0 | `java/util/concurrent/ScheduledThreadPoolExecutor`, `java/util/ResourceBundle` |
| `register_phase66_natives` | `phases_late.rs:4226` | 5 | 46 | 0 | 0 | 0 | `java/text/Collator`, `java/lang/constant/ClassDesc` |
| `register_phase_d_natives` | `lib.rs:40865` | 4 | 46 | 1 | 0 | 0 | `java/util/concurrent/StructuredTaskScope`, `java/util/stream/Gatherer` |
| `register_biginteger_natives` | `math_bignum.rs:1303` | 1 | 43 | 0 | 0 | 0 | `java/math/BigInteger` |
| `register_m18_concurrent_fixes` | `util_concurrent_ext.rs:1624` | 1 | 41 | 0 | 0 | 0 | `java/util/concurrent/LinkedBlockingQueue` |
| `register_jackson_gson_natives` | `phases_late/xml_json.rs:3251` | 1 | 38 | 0 | 20 | 0 | `com/fasterxml/jackson/databind/ObjectMapper` |
| `register_s2_nio` | `servlet.rs:4404` | 4 | 37 | 17 | 0 | 0 | `java/nio/channels/SocketChannel`, `java/nio/channels/Selector` |
| `register_bigdecimal_natives` | `math_bignum.rs:2569` | 1 | 32 | 0 | 0 | 0 | `java/math/BigDecimal` |
| `register_logging_natives` | `logging_shims.rs:1443` | 1 | 30 | 0 | 0 | 0 | `java/util/logging/Logger`, `java/util/logging/Level` |
| `register_t25_natives` | `util_time.rs:5686` | 1 | 29 | 0 | 0 | 0 | `java/time/Clock`, `java/time/Year` |
| `register_t31_concurrent_extras` | `util_concurrent_ext.rs:2689` | 1 | 28 | 0 | 0 | 0 | `java/util/concurrent/LinkedTransferQueue` |
| `register_t39_stax` | `t3_impl.rs:517` | 1 | 26 | 0 | 0 | 0 | `javax/xml/stream/XMLStreamReader` |
| `register_quarkus_arc_natives` | `quarkus_arc.rs:389` | 7 | 23 | 0 | 23 | 0 | `io/quarkus/arc/*` |
| `register_completable_future_natives` | `util_concurrent_ext.rs:4911` | 1 | 23 | 13 | 0 | 0 | `java/util/concurrent/CompletableFuture` |
| `register_cds_natives` | `cds.rs:793` | 1 | 22 | 0 | 0 | 0 | `sun/management/ManagementFactoryHelper` |
| `register_s1_classloading` | `servlet.rs:1652` | 1 | 22 | 0 | 0 | 0 | `java/net/URLClassLoader`, `java/util/ServiceLoader` |
| `register_java_lang_extras_natives` | `lib.rs:37473` | 1 | 18 | 0 | 0 | 0 | `java/lang/Number`, `java/lang/ClassLoader` |
| `register_phase57_natives` | `phases_late/nio_file.rs:18` | 2 | 18 | 0 | 0 | 0 | `java/text/DecimalFormat`, `java/text/NumberFormat` |
| `register_t38_jndi` | `t3_impl.rs:30` | 1 | 16 | 0 | 0 | 0 | `javax/naming/InitialContext`, `java/rmi/registry/Registry` |
| `register_concurrent_extras` | `concurrent_extras.rs:805` | 3 | 15 | 0 | 0 | 0 | `java/util/concurrent/SynchronousQueue` |
| `register_graalvm_compat_natives` | `graalvm_compat.rs:1381` | 1 | 15 | 0 | 15 | 0 | `org/graalvm/nativeimage/ImageInfo` |
| `register_jdk25_patterns_natives` | `jdk25_patterns.rs:541` | 1 | 15 | 0 | 0 | **5** | `java/lang/StableValue`, `jdk/internal/misc/PatternSupport` |
| `register_t312_tooling` | `t3_impl.rs:1555` | 1 | 15 | 0 | 0 | 0 | `javax/tools/JavaCompiler`, `jdk/jshell/JShell` |
| `register_aot_natives` | `aot.rs:1451` | 1 | 14 | 0 | 0 | 0 | `jdk/internal/misc/CDS` |
| `register_security_natives` | `lib.rs:35389` | 1 | 13 | 0 | 0 | 0 | `java/security/MessageDigest` |
| `register_byte_array_output_stream` | `serialization.rs:5083` | 1 | 12 | 0 | 0 | 0 | `java/io/ByteArrayOutputStream` |
| `register_enterprise_natives` | `lib.rs:37111` | 1 | 12 | 0 | 0 | 0 | `java/lang/StackTraceElement`, `java/lang/ProcessBuilder` |
| `register_jdk25_language_natives` | `jdk25_language.rs:368` | 1 | 9 | 0 | 0 | **9** | `jdk/internal/misc/ImplicitClasses`, `jdk/internal/module/ModuleImports` |
| `register_t310_scripting` | `t3_impl.rs:1057` | 1 | 9 | 0 | 0 | 0 | `javax/script/ScriptEngine` |
| `register_atomic_boolean_natives` | `util_concurrent_ext.rs:7790` | 1 | 8 | 0 | 0 | 0 | `java/util/concurrent/atomic/AtomicBoolean` |
| `register_enum_natives` | `lang_misc.rs:1628` | 1 | 8 | 0 | 0 | 0 | `java/lang/Enum` |
| `register_functional_completion_natives` | `lib.rs:37776` | 1 | 8 | 0 | 0 | 0 | `java/util/IntSummaryStatistics` |
| `register_crypto_impl_natives` | `crypto_impl.rs:1336` | 1 | 5 | 0 | 0 | 0 | `java/security/SecureRandom` |
| `register_letsgo_compat_natives` | `letsgo_compat.rs:29` | 2 | 4 | 0 | 0 | 0 | `java/util/concurrent/BlockingQueue` |
| `register_s3_http_client` | `servlet.rs:7354` | 1 | 4 | 0 | 0 | 0 | `java/net/http/HttpClient` |
| `register_unsafe_define_class` | `unsafe_natives.rs:1374` | 1 | 3 | 0 | 0 | 0 | `jdk/internal/misc/Unsafe` |
| `register_t311_i18n` | `t3_impl.rs:1456` | 1 | 2 | 0 | 0 | 0 | `java/util/Locale`, `java/nio/charset/Charset` |
| `register_mbean_server_factory_synthetic` | `jmx.rs:6821` | 1 | 2 | 0 | 0 | 0 | `javax/management/MBeanServerFactory` |
| `register_management_factory_platform_server_stub` | `jmx.rs:1418` | 1 | 1 | 0 | 0 | 0 | `java/lang/management/ManagementFactory` |

**Totals: 69 roots, 4,014 registrations, of which 182 are on a VM-minted real
JDK type, 157 are third-party, 49 are on names that do not exist in JDK 25.**

Twelve further dead registrars sit outside `register_synthetic_overrides`
entirely and are listed in §5.3. The read-only crates contribute
`native-collections/src/lib.rs::register_concurrent_skip_list_map_natives`
(11 registrations, **no caller at all**) and
`native-io/src/lib.rs::register_selector` (22, §5.2). `native-awt/` is clean:
its whole registrar tree hangs off `register_awt_natives`, and `awt` **is** a
default feature of `cratonvm-vm`, so nothing there is dead. The first pass of
this sweep called all 12 AWT registrars dead by treating every `feature = "…"`
as non-default — a reminder that the polarity of the gate is half the answer.

---

## 3. The four reasons, and which registrars each one covers

### 3.1 Reason A — the class is real, its bytecode works, and the native would shadow it. **Gate is correct.**

This is the large majority: roughly 3,600 of the 4,014. `java.math.BigInteger`,
`java.util.Calendar`, `java.time.*`, `java.util.zip.Deflater`,
`java.io.ObjectInputStream` all arrive from `$JAVA_HOME/lib/modules` with
complete, correct bytecode. Registration is the gate on the cold interpreter
paths (natives-over-real-jdk-classes.md §1: *"a registered native wins over real
bytecode unconditionally"*), so wiring one of these registrars into the default
build does not *add* coverage — it **replaces working JDK code with a partial
Rust reimplementation**.

The tree already carries the worked instance of that going wrong:
`Integer.toString(II)` has two registrations, the winner performs no radix
validation at all, and `toString(5, 40)` panics inside `char::from_digit` while
`toString(5, 1)` never terminates (natives-over-real-jdk-classes.md §3). That is
what a "coverage improvement" in this bucket buys.

`register_biginteger_natives` says so about itself, at `math_bignum.rs:1127`:
the file already documents that the registrar is synthetic-only and that the
real-JDK path is the bytecode. **Leave every row in this bucket gated.**

### 3.2 Reason B — the class name does not exist in JDK 25. **Gate is irrelevant; the code is dead in every mode.**

49 registrations, verified with `javap` against the JDK 25.0.3 on PATH:

| name | status |
|---|---|
| `jdk/incubator/concurrent/StructuredTaskScope` (+ 3 nested) | ABSENT — moved to `java.util.concurrent`, which `register_phase_d_natives` targets separately |
| `jdk/internal/misc/PatternSupport` | ABSENT |
| `jdk/internal/misc/ImplicitClasses` | ABSENT |
| `jdk/internal/module/ModuleImports` | ABSENT |
| `jdk/internal/vm/FlexibleConstructors` | ABSENT |
| `java/lang/StringTemplate` (+ `$Processor`) | ABSENT — withdrawn after JDK 23 |
| `java/util/IteratorEnumeration` | ABSENT — never a JDK class |

`register_jdk25_language_natives` is **9 for 9** in this bucket: every triple it
registers is on a class JDK 25 does not have. `register_jdk25_patterns_natives`
is 5 of 15. `register_phase67_natives` is 33 of 105. Ungating these would change
nothing at all, which is worth knowing before someone spends a day on it.

### 3.3 Reason C — the class is an application class. **Gate is a policy choice, not a coverage gap.**

157 registrations on `org/slf4j/*`, `org/apache/logging/log4j/*`,
`com/fasterxml/jackson/*`, `com/google/gson/*`, `io/quarkus/arc/*`,
`org/graalvm/nativeimage/*`, `org/w3c/dom/*`. These only ever bind if the
application ships the class, and then they shadow *the application's own code*.
`native-builtins/Cargo.toml` already has `app-stubs` and
`synthetic-quarkus-arc` as separate default-off features for exactly this, so
the intent is on record. Not a `--jdk-only` question.

### 3.4 Reason D — CratonVM mints instances of the real class itself, so the native **is** the implementation. **Gate is wrong.**

182 registrations. This is the bucket the original bug came from, and the
mechanism is precise:

```rust
// native-collections/src/lib.rs, make_int_stream
let stream = try_alloc_synthetic(ctx, "java/util/stream/IntStream", STREAM_NUM_FIELDS)?;
```

The receiver's runtime class **is** `java.util.stream.IntStream` — the real JDK
interface. There is no `AbstractPipeline` underneath it. So every method call on
it resolves to the interface declaration, and for an *abstract* declaration
there is no `Code` attribute to run. If the triple is not registered, the call
dies with `AbstractMethodError: … has no Code attribute`. There is no bytecode
fallback, because the object was never a real pipeline.

The complete minted set, from `try_alloc_synthetic` / `try_alloc_concurrent_synthetic`
call sites across `native-collections/`, `native-builtins/` and `native-io/`:
`java/util/stream/{Stream,IntStream,LongStream,DoubleStream,Collector}`,
`java/util/Spliterator`, `java/util/Map$Entry`,
`java/nio/channels/{Selector,SelectionKey,DatagramChannel}`,
`java/nio/file/{Path,WatchService,WatchKey,WatchEvent}`,
`java/util/concurrent/{CompletableFuture,ScheduledFuture}`.

**The critical refinement, and it is what keeps this bucket from becoming another
blanket list: only the ABSTRACT declarations are load-bearing.** `javap` on
JDK 25 splits the phase-56 stream surface three ways, and the three halves need
opposite treatment:

| kind on the real interface | what happens on a minted receiver | verdict |
|---|---|---|
| **abstract** (`Stream.forEachOrdered`, `IntStream.summaryStatistics`, `IntStream.forEachOrdered`) | `AbstractMethodError: has no Code attribute` — hard failure, no fallback | **must be registered in the default build** |
| **default** (`Stream.takeWhile`, `dropWhile`) | the real default body runs, walks a Spliterator our object does not have | a real defect, but a *different* one; a native here is a judgement call, not a forced move |
| **static** (`Stream.iterate` / `generate` / `ofNullable`, `{Int,Long,Double}Stream.concat`) | static interface methods **keep** the native check in real-JDK mode | **actively dangerous** — see §4.2 |

---

## 4. The stream family, resolved

### 4.1 The evidence that somebody already needed these

Five sites in live code carry a comment naming a synthetic-only registrar as the
reason they exist. Each is one method hand-copied out of the dead file for the
one workload that reached it:

| live site | method rescued | names |
|---|---|---|
| `native-collections/src/lib.rs:25943` | `LongStream.mapToObj` | *"the synthetic-jdk-only `register_phase56_stream_extras` registration is compiled out of the real-JDK CLI"* |
| `vm/src/runtime/interpreter.rs:996` | `Stream.forEachOrdered` | *"its only native registration lives in `register_phase56_stream_extras`, reachable solely from `register_synthetic_overrides`"* |
| `vm/src/vm/vm_init.rs:2235` | `java.util.logging.FileHandler` ctor | *"registered … only under `register_synthetic_overrides`, which is `#[cfg(feature = "synthetic-jdk")]`-gated and never runs in real-JDK mode"* |
| `native-builtins/src/reflect_annotations.rs:515` | `BaseStream.sequential/parallel/…` | *"The full synthetic-jdk-only `register_stream_overrides` registration was unreachable in real-JDK builds; pull it into essentials"* |
| `native-builtins/src/service_loader.rs:3311` | `ServiceLoader` iterator | *"synthetic-jdk-only and absent from real-JDK"* |

The `interpreter.rs` one deserves separate billing, because it is the most
expensive shape this record found. `Stream.forEachOrdered` was not fixed by
registering the native. It was fixed by adding a **hardcoded method-name special
case to the interpreter's `!has_code` fallback** —
`if method_name == "forEachOrdered" && method_descriptor == "(Ljava/util/function/Consumer;)V"`
→ re-dispatch as `forEach`. A dead registrar pushed a workaround into the
dispatch core. And it is `Stream`-only: `IntStream.forEachOrdered(IntConsumer)V`
is equally abstract, equally unregistered, and has no special case.

The tree also already contains the right *shape* of guard for this species, in
`reflect_annotations.rs`'s `module_can_read_essential_tests`: two `#[test]`s that
build a fresh registry, call `register_essential_natives`, and assert
`registry.find(...)` is `Some` — *"must be registered in the essential (real-JDK)
native path, not just the synthetic-jdk-only `register_p59_module`"*. That is a
ratchet. It exists for `java.lang.Module` and for nothing else.

### 4.2 Why `register_phase56_stream_extras` must **not** be wired wholesale

Three independent reasons, any one of which is sufficient:

1. **22 of its 54 registrations are already served by live registrars** —
   `Stream.peek`, `iterator`, `concat`, `mapToInt/Long/Double`,
   `flatMapToInt/Long/Double`, `IntStream.peek/sorted/boxed/asLongStream` and the
   rest, all live in `native-collections`' `register_stream_natives` /
   `register_int_stream_natives`. Those are the hand-copies of §4.1, and they are
   the *maintained* copies.
2. **Its stream constructor uses a different object layout.** `p56_build_stream`
   allocates with `try_alloc_concurrent_synthetic(ctx, class, 1)` — **one field**
   — while `native-collections`' `make_stream` allocates `STREAM_NUM_FIELDS`
   (elements, close-handlers, lazy-spliterator, op-chain). `native-collections`
   already carries the scar: *"W1 fix: several ad-hoc synthetic-stream
   allocations use a 1-field (elements-only) layout with no close-handler slot …
   otherwise this intermediate-op propagation OOB-reads slot 1 on every chained
   op against such a source, flooding the GC out-of-bounds-field guard."* Wiring
   phase 56 in makes every `takeWhile` / `dropWhile` / `peek` return one of those.
3. **The file says not to, in writing.** The `Gatherer.defaultInitializer` note
   at `phases_late/streams.rs:3943`: *"these are STATIC interface methods, and
   static interface methods DO keep the native check … So unlike the instance
   methods around them they would intercept in real-JDK mode and hand real
   `Gatherers` a null where it requires `Gatherers.Value.DEFAULT`. They are safe
   only because this whole registrar is synthetic-only; do not move
   `register_phase56_stream_extras` onto the real-JDK path with these in it."*

So the fix is not a wiring line, it is a **narrowed registrar** — the abstract
declarations only, with a stream constructor that agrees with
`native-collections`. §6.1 specifies it.

### 4.3 The corrected gap: 20 triples, 5 of them the hard failure

After removing the twelve already covered by `register_basestream_mode_overrides`
(§0), `register_phase56_stream_extras` leaves 20 triples registered nowhere:

* **abstract — hard `AbstractMethodError` (5)**: `Stream.forEachOrdered(Consumer)V`
  (masked by the interpreter special case), `IntStream.forEachOrdered(IntConsumer)V`
  (**unmasked, live defect**), `IntStream.summaryStatistics()`,
  `LongStream.summaryStatistics()`, `DoubleStream.summaryStatistics()`.
* **default — wrong behaviour, not a crash (8)**: `takeWhile` / `dropWhile` on
  `Stream`, `IntStream`, `LongStream`, `DoubleStream`.
* **static — must stay out (7)**: `Stream.ofNullable`, `Stream.iterate` ×2,
  `Stream.generate`, `IntStream.concat`, `LongStream.concat`,
  `DoubleStream.concat`.

`register_phase56_collectors_extras` is a clean 16-for-16 overlap with the live
`register_collectors_natives` — nothing to do. `register_phase56_function_extras`
is already wired here and is 41-for-41 live. `register_phase56_summary_stats` is
24 triples with zero overlap, and it is the subject of §5.1.

---

## 5. Individual verdicts that do not follow from the buckets

### 5.1 `register_phase56_summary_stats` — do **not** wire. It carries a §5 slot-index defect for `DoubleSummaryStatistics`.

24 registrations, none of them live anywhere, all on real concrete JDK classes.
Tempting, and wrong. The natives address fields by index —
`STATS_FIELD_COUNT/SUM/MIN/MAX` = 0/1/2/3 — and `javap` on JDK 25 says:

```text
java.util.IntSummaryStatistics     count(long)  sum(long)  min(int)     max(int)              — 4 fields, order MATCHES
java.util.LongSummaryStatistics    count(long)  sum(long)  min(long)    max(long)             — 4 fields, order MATCHES
java.util.DoubleSummaryStatistics  count(long)  sum(double)  sumCompensation(double)
                                   simpleSum(double)  min(double)  max(double)                — 6 fields, order DIFFERS
```

On the real `DoubleSummaryStatistics` layout, slot 2 is `sumCompensation` and
slot 3 is `simpleSum`. The registrar's `<init>` and `accept(D)V` would write min
and max into the compensation accumulators, and `getMin()` would return
`sumCompensation`. Its allocator (`try_alloc_concurrent_synthetic(ctx, …, 4)`)
under-allocates by two slots against the real class. This is
natives-over-real-jdk-classes.md §5 exactly, in a fourth file. The Int and Long
halves happen to be safe; the Double half is not, and they are one registrar.

### 5.2 `java/nio/channels/Selector` — three dead registrars, and the gate is nevertheless **correct**

`register_selector` (`native-io/src/lib.rs:19647`, 22 registrations),
`register_p58_nio_channels` (`phases_late/net_channels.rs`, 43) and
`register_s2_selector` (`servlet.rs`, 17) all register the `Selector` /
`SelectionKey` instance surface, all three are dead, and only three triples
(`Selector.open()`, `isOpen()`, `select(J)I`) are live. `Selector.select()`,
`selectNow()`, `keys()`, `selectedKeys()`, `wakeup()` and `close()` are abstract
on the real `java.nio.channels.Selector`, so on paper this is a second §3.4
instance at 17 triples.

It is not, and the reason is worth recording because it is the shape that would
catch the next reader. The **live** path does not mint a bare `Selector`: the
live `register_nio_selector_real` (`native-io/src/nio_selector.rs:3626`)
registers `Selector.open()` → `selector_open_native` and then registers the whole
instance surface on **`sun/nio/ch/SelectorImpl`**, which is what that factory
returns. The dead registrars are a *second, complete implementation* keyed on the
interface, for a synthetic world where `SelectorImpl` does not exist. Two
implementations of one primitive, one per mode — not a gap. Wiring either dead
one in would put a second `Selector.open()` into a last-write-wins registry and
decide which selector the VM uses by call ordering.

### 5.3 Dead registrars that `register_synthetic_overrides` does not explain

Twelve registrars are absent from the default build for some other reason. They
are a mixed bag and none is a stream-shaped defect, but two are worth eyes:

| registrar | file | registrations | why dead |
|---|---|---:|---|
| `register_concurrent_skip_list_map_natives` | `native-collections/src/lib.rs:51054` | 11 | **no caller at all**, in any mode |
| `register_p62_stamped_lock` | `phases_late/concurrent.rs:2636` | 19 | call site disabled — see `W6-12-stampedlock-split-brain.md` |
| `register_tls_impl_natives` | `tls_impl.rs:1327` | 5 | no caller |
| `register_apps_h2_overrides` | `apps_h2.rs:51` | 2 | no caller |
| `register_jboss_logmanager_natives` | `jboss_logmanager.rs:328` | 14 | test-only caller |
| `register_missing_deprecated_shims` | `deprecated_verify.rs:227` | 1 | no caller |
| `register_object_stream_class_for_phases_late` | `serialization.rs:4985` | 0 | `feature = "experimental-serialization"` |
| `register_de4_demo_stubs`, `register_pbe_workaround`, `objectstreamclass_natives`, `unsafe_wp1_2_natives`, `register_t2_3_completion_natives` | `phases_late.rs`, `phases_early.rs` | 0 direct | dispatchers whose whole subtree is dead |

`register_concurrent_skip_list_map_natives` is the sharpest: 11 registrations
that no build of any configuration has ever executed. It is in a read-only crate
for this lane.

---

## 6. What was changed, and what was not

### 6.1 Applied — one line, in this lane's file

`native-builtins/src/reflect_annotations.rs`, in `register_annotation_overrides`,
immediately after the existing `crate::streams::register_stream_overrides(registry);`:

```rust
crate::phases_late::register_phase56_primitive_stream_terminals(registry);
```

This is the patch specified by `W7-2-primitive-stream-terminal-surface.md` §7.1.
`register_phase56_primitive_stream_terminals` is **new on the
`fix/jdk-only-stream-summarystatistics-20260811` branch and does not exist in
this worktree** — the two branches merge before anything is built. The call is
placed beside `register_stream_overrides` because that is the precedent for a
real-JDK stream registration reached from essentials.

**Ordering / overlap check.** Safe in both directions:

* *No overlap.* The three `{Int,Long,Double}Stream.summaryStatistics()` triples
  appear in no live registrar — checked against the complete live registration
  set, not against a grep. There is no key here for a later `register()` to take
  over, and none for this call to take over from anything else.
* *And the direction is favourable anyway.* `register_annotation_overrides` is
  called from `register_essential_natives` (`lib.rs:18817`), which the default
  build reaches at `vm/src/vm/vm_init.rs:2208` —
  **before** `register_collections_natives` at `vm_init.rs:2434`. Under
  last-registration-wins, any overlap that appears later is resolved in
  `native-collections`' favour, which is the maintained copy. A wiring line added
  *here* cannot steal a triple from `native-collections`; one added after line
  2434 could.

Nothing else was wired. §3.1–§3.3 say why the other 66 rows should not be, and
§4.2/§5.1 say why the two most tempting remaining ones must not be.

### 6.2 Out-of-file patch (not applied) — the narrowed stream registrar

Owner: whoever owns `native-builtins/src/phases_late/streams.rs`. This closes the
five §4.3 abstract gaps without any of the three §4.2 hazards. It deliberately
omits every `default` and every `static` declaration, and it builds its result
streams through `native-collections`' constructor rather than `p56_build_stream`,
so the layout matches.

```rust
/// The `register_phase56_stream_extras` triples that are ABSTRACT on the real
/// JDK 25 interfaces, and therefore cannot fall back to bytecode when the
/// receiver is one of `native-collections`' minted streams
/// (`try_alloc_synthetic(ctx, "java/util/stream/IntStream", ..)`). Without
/// these the call resolves to the bodiless interface declaration and raises
/// `AbstractMethodError: … has no Code attribute`.
///
/// Deliberately NOT a wrapper around `register_phase56_stream_extras`. That
/// registrar also carries (a) 22 triples `native-collections` already serves
/// with maintained implementations, (b) STATIC interface methods
/// (`Stream.iterate/generate/ofNullable`, `*Stream.concat`) which keep the
/// native check in real-JDK mode and would intercept real pipelines, and
/// (c) `p56_build_stream`'s 1-field stream layout, which disagrees with
/// `make_stream`'s `STREAM_NUM_FIELDS`. See
/// docs/known-issues/jdk-only/W7-5-registrars-that-never-shipped.md §4.2.
pub(crate) fn register_phase56_abstract_stream_terminals(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
    // Currently masked by a hardcoded method-name special case in
    // `vm/src/runtime/interpreter.rs` (`!has_code` fallback). Registering the
    // triple is what lets that special case be deleted.
    r.register(
        "java/util/stream/Stream",
        "forEachOrdered",
        "(Ljava/util/function/Consumer;)V",
        p56_stream_for_each_ordered,
    );
    // The same declaration on IntStream is equally abstract and has NO
    // interpreter special case — this one is an unmasked live defect.
    r.register(
        "java/util/stream/IntStream",
        "forEachOrdered",
        "(Ljava/util/function/IntConsumer;)V",
        p56_int_stream_for_each_ordered,
    );
    r.set_category(__prev_cat);
}
```

The `summaryStatistics()` half of the five is
`register_phase56_primitive_stream_terminals` on the sibling branch; whichever of
the two lands second should fold the other's triples in rather than adding a
second call, so there is one registrar per surface.

### 6.3 Out-of-file patch (not applied) — the ratchet

Owner: the same. Without this the wiring rots the way the first one did. The
shape is already in the tree — `reflect_annotations.rs`'s
`module_can_read_essential_tests` — and this is the same test for the stream
surface:

```rust
#[cfg(test)]
mod stream_terminals_essential_tests {
    use super::*;

    /// These three are ABSTRACT on the real JDK 25 interfaces and the receiver
    /// is a VM-minted instance of the interface itself, so a missing
    /// registration is an `AbstractMethodError`, not a fallback to bytecode.
    /// They must be reachable from `register_essential_natives` — the real-JDK
    /// boot path — and not only from the `synthetic-jdk`-gated
    /// `register_phase56_stream_extras`.
    #[test]
    fn essentials_cover_the_abstract_primitive_stream_terminals() {
        let mut registry = NativeMethodRegistry::new();
        crate::register_essential_natives(&mut registry);
        for (cls, m, d) in [
            ("java/util/stream/IntStream", "summaryStatistics", "()Ljava/util/IntSummaryStatistics;"),
            ("java/util/stream/LongStream", "summaryStatistics", "()Ljava/util/LongSummaryStatistics;"),
            ("java/util/stream/DoubleStream", "summaryStatistics", "()Ljava/util/DoubleSummaryStatistics;"),
            ("java/util/stream/Stream", "forEachOrdered", "(Ljava/util/function/Consumer;)V"),
            ("java/util/stream/IntStream", "forEachOrdered", "(Ljava/util/function/IntConsumer;)V"),
        ] {
            assert!(
                registry.find(cls, m, d).is_some(),
                "{cls}.{m}{d} must be registered on the essential (real-JDK) path"
            );
        }
    }
}
```

Not added in this lane: it names functions that do not exist in this worktree, so
landing it here would produce a test that cannot compile until the sibling branch
merges, and this lane cannot build to check that. It belongs in the same commit
as §6.2.

> **VERIFIED AGAINST A BINARY 2026-09-02.** The banner says "Nothing was built or
> run on 2026-08-12". §6.3.1's ratchet has now been built and run — which was the
> open question, because §6.3.1 records it going from a test that "cannot
> compile" to "a real test" only when a sibling branch merged. It compiles, and
> all three of §6.3.1's tests pass:
>
> ```text
> cargo test -p cratonvm-native-builtins --test essential_wiring_ratchet
>   essentials_cover_the_abstract_primitive_stream_terminals ... ok
>   none_of_the_terminals_is_a_synthetic_stub ................ ok
>   the_terminals_survive_the_whole_real_jdk_boot ............ ok
>   boot_path::the_replayed_sequence_matches_vm_init ......... ok
>   boot_path::the_inline_registrations_in_vm_init_are_enumerated ... ok
>   5 passed, 0 failed
> ```
>
> **Five tests, and §6.3.1 says three — but this is NOT the count drift every
> other record in this campaign showed.** The two extra are not growth and not
> this record's: `boot_path::the_replayed_sequence_matches_vm_init` and
> `boot_path::the_inline_registrations_in_vm_init_are_enumerated` belong to
> [`W7-30`](W7-30-stub-ratchet-boot-path-scope.md), which names both. Two records
> share one file. Reported as drift it would have been noise; read, it is two
> lanes' work sitting side by side, and both are green.
>
> Unchanged: §6.3.1's note that this is **not wired into CI** — that file was not
> that lane's to edit, and running it by hand here does not wire it.
> `forEachOrdered` is still deliberately unasserted for the reason §6.3.1 gives.

### 6.3.1 WRITTEN, 2026-08-12 — `native-builtins/tests/essential_wiring_ratchet.rs`

The sibling branch merged: `register_phase56_primitive_stream_terminals` is in
`native-builtins/src/phases_late/streams.rs` and
`reflect_annotations::register_annotation_overrides` calls it, reached from
`register_essential_natives_with_shims`. So the blocker on §6.3 is gone and the
ratchet is a real test rather than one that cannot compile.

It went into `native-builtins/tests/` rather than a `#[cfg(test)] mod` inside
`streams.rs` for two reasons: it asserts about the shipping BOOT PATH, which is
an integration-level property, and `streams.rs` is another lane's file.

**The triple list is six, not the five §6.3 predicted, and both differences are
deliberate.** §6.3 was written before the narrowed registrar existed and
predicted its contents:

* **`java/util/stream/Stream.forEachOrdered(Consumer)V` is NOT asserted.** It is
  equally abstract and it is still served by the hardcoded method-name special
  case in `vm/src/runtime/interpreter.rs`'s `!has_code` fallback (§4.1).
  Asserting it would freeze a state the tree is not in. It is the next row to
  add, and adding it is what would let that special case be deleted.
* **`{Long,Double}Stream.forEachOrdered` ARE asserted.** §4.3 named only the
  `IntStream` one as the unmasked live defect; the registrar that landed covers
  all three widths.

Three tests, because they fail for three different reasons:

| test | what only it can catch |
|---|---|
| `essentials_cover_the_abstract_primitive_stream_terminals` | the regression this record is about — the triple is registered only from a `synthetic-jdk`-gated registrar, so a default `cratonvm-cli` build raises `AbstractMethodError` |
| `none_of_the_terminals_is_a_synthetic_stub` | the half `find(..).is_some()` cannot see: `register()` refuses a `SyntheticStub` under `JdkOnly`, and these triples have no bytecode underneath, so a refusal hands the method back to `AbstractMethodError` rather than to the JDK |
| `the_terminals_survive_the_whole_real_jdk_boot` | the hazard the registrar's own doc comment asks a HUMAN to check for: "adding a triple that native-collections also registers makes this registrar silently inert". `register_collections_natives` runs after essentials and `register()` is last-write-wins, so the check has to be taken at the END of the boot, not at the end of essentials |

The third replays `vm_init`'s real-JDK arm through the shared
`native-builtins/tests/common/vm_init_boot_path.rs` model
(W7-30-stub-ratchet-boot-path-scope.md §7.1) rather than mirroring it a fourth
time.

**Not wired into CI** — `.github/workflows/ci.yml` is not this lane's file. It
belongs beside the existing
`cargo test -p cratonvm-native-builtins --test stub_ratchet` step; see the
out-of-file list in the lane report.

### 6.4 Out-of-file — reported, not patched

* **`native-collections/src/lib.rs::register_concurrent_skip_list_map_natives`**
  (11 registrations, no caller in any configuration). Either wire it or delete
  it; leaving it is the same trap one level down. Read-only for this lane.
  **Both halves of that instruction are withdrawn — see §6.4.1.**
* **`register_jdk25_language_natives`** registers 9 triples on 3 class names that
  JDK 25 does not have. The registrar is inert whatever the feature flag says.
  Same for 5 of `register_jdk25_patterns_natives`' 15 and 33 of
  `register_phase67_natives`' 105 (`jdk.incubator.concurrent.StructuredTaskScope`
  became `java.util.concurrent.StructuredTaskScope`; `register_phase_d_natives`
  already targets the new name).
* **`register_phase56_summary_stats`'s `DoubleSummaryStatistics` half** (§5.1) is
  a latent §5 slot-index defect. It is harmless only while the registrar stays
  dead, which makes it a trap for the next person who wires it "for coverage".
  If the Int/Long halves are ever wanted, split the registrar first.

### 6.4.1 `ConcurrentSkipListMap` — VERDICT: neither wire nor delete. Leave it, 2026-08-12

§5.3 calls this "the sharpest: 11 registrations that no build of any
configuration has ever executed", and §6.4 gives a binary instruction: *"Either
wire it or delete it; leaving it is the same trap one level down."* **The
binary is the error.** There is a third state, this registrar is in it, and it
is the state a defect record should want:

> **Disabled deliberately, with the measurement that caused it, a test that
> enforces the disable, and its bodies kept reachable by test hooks.**

Source-verified in `native-collections/src/lib.rs`, all four:

1. **The disable is a written, measured decision, not an omission.** The
   `let _ = register_concurrent_skip_list_map_natives;` no-op carries the
   reason: the native "sorted-array" overlay hardcoded natural ordering and
   ignored the `(Comparator)` constructor, so puts over keys ordered only by
   that comparator were silently dropped or misordered — *"broke the Gradle test
   worker's serializer registry"*. Real
   `java.util.concurrent.ConcurrentSkipListMap` bytecode runs instead. That is
   §3.1's verdict, correctly applied.
2. **A test enforces it.** `concurrent_skip_list_map_not_intercepted` asserts
   `find(...)` is `None` for the CSLM triples *"so the real class is not
   shadowed again"*, and its own comment records that it *"previously asserted
   the opposite and had failed since the natives were disabled"*. Wiring the
   registrar in reddens that test. This is not dead code nobody is watching; it
   is code with a guard pointed at it.
3. **Deleting it would drop the only record of two triples.** The no-op's
   comment names them: `<init>(Ljava/util/Comparator;)V` and
   `keySet()Ljava/util/Set;` exist nowhere else in the tree. The sibling
   `native-builtins/src/util_concurrent_ext.rs::register_t31_concurrent_extras`
   covers twelve triples and neither of those two, so the symmetric difference
   is not one-sided and "delete the redundant copy" is not what a deletion would
   do.
4. **The bodies are exercised.** `__test_cslm_*` hooks drive these natives from
   `native-collections/tests/gc_side_table_root_audit.rs`, which exists because
   the 2026-08-01 GC-safety fixes to this family *"are otherwise untestable — the
   natives cannot be reached through the registry — and an untested fix in code
   someone may re-enable is"* the trap worth avoiding.

So the species this record is named after — a registrar that looks like coverage
and is not — does not apply here. The species it *would* be is different and
milder: an implementation kept for reference behind a guard. The action is to
correct this record, which is done, and the one real residual is that §5.3's
"no caller at all, in any mode" reads as an accident when it is a decision;
the `let _ =` line is exactly the idiom that makes it survive `dead_code`
review, and it works.

**No code change. Nothing in `native-collections/` was edited.**

---

## 7. What would falsify this

Three claims carry the rest, and each has a cheap refutation:

1. **That the 301 are genuinely absent from a default `cratonvm-cli` binary.**
   Refuted by `--dump-native-registry` on a default build showing any triple that
   only `register_synthetic_overrides` registers. The census is static; nothing
   here was built or run.
2. **That §3.4's mechanism is the operative one** — that a minted receiver whose
   class is the real interface takes `AbstractMethodError` on an abstract
   declaration. Refuted by a real-JDK run in which `IntStream.forEachOrdered`
   succeeds without any registration, which would mean some dispatch path
   retargets it the way `forEach` is retargeted, and the §4.3 abstract list
   shrinks accordingly.
3. **That the three `summaryStatistics()` triples overlap nothing live.** Refuted
   by any live registrar registering them, which would make §6.1's call a
   last-write-wins takeover rather than a gap fill. The check was a literal
   comparison over resolved registrations, and §0 shows that exact method
   over-stating a gap by 12 once already — a `format!`-built descriptor could hide
   a fourth registration the same way.

---

## 8. Re-verification against the working tree, 2026-08-12 (lane A2)

Read, not rebuilt — nothing below was built or run. §7's falsifiers still need a
binary; this section only re-checks the *source* claims. **The record's verdicts
all survive. Two of its factual sub-claims are now stale, both in the direction
of "already fixed", and several line numbers have moved.**

### 8.1 Mechanism and gating — CONFIRMED, unchanged

`#[cfg(feature = "synthetic-jdk")]` at `native-builtins/src/lib.rs:21525`, over
`register_synthetic_overrides` at `:21526`. The `#[cfg(not(feature =
"synthetic-jdk"))]` no-op shims are at `vm/src/native/builtins.rs:23-26`
(`register_builtins`) and `:28-29` (`register_synthetic_overrides`).
`synthetic-jdk` is in no default set: `native-builtins/Cargo.toml:19`
`default = []`, `vm/Cargo.toml:69` `default = ["awt", "management", "zgc"]`,
`vm-cli/Cargo.toml:94` `default = ["mimalloc", "zgc"]`. §1 stands verbatim.

### 8.2 §6.1's wiring — CONFIRMED live; three line numbers stale

`crate::phases_late::register_phase56_primitive_stream_terminals(registry);` is
at `native-builtins/src/reflect_annotations.rs:589`, the statement after
`crate::streams::register_stream_overrides(registry);` (`:560`). The registrar
exists at `native-builtins/src/phases_late/streams.rs:469`, is **ungated** (no
`#[cfg]` on it or on any enclosing module), and registers as
`NativeKind::Bridge` (`streams.rs:471`, restored at `:512`) — so it is neither a
`SyntheticStub` that strict mode would drop nor a `synthetic-jdk` registrar that
a shipping build would compile out. Both traps the record is named after are
avoided.

Chain to the boot path: `lib.rs:7097` `register_essential_natives` → `:7098`
delegates to `register_essential_natives_with_shims` (`lib.rs:7103`) → `:19159`
`register_annotation_overrides(registry)` → `reflect_annotations.rs:589`.

Stale numbers, corrected: §6.1 cites `lib.rs:18817` for the essentials call — it
is `lib.rs:19159`, and it is inside `register_essential_natives_with_shims`, not
the thin `register_essential_natives` wrapper. It cites `vm_init.rs:2208` /
`:2434` for the real-JDK arm's ordering — those are `vm/src/vm/vm_init.rs:2498`
(essentials) and `:2724` (`register_collections_natives`) today; the synthetic
arm is `:1960` / `:2191`. **The ordering claim itself — essentials before
collections, so a wiring line added in essentials cannot steal a triple from
`native-collections` — is CONFIRMED.**

### 8.3 §6.3.1's ratchet — CONFIRMED exactly as described

`native-builtins/tests/essential_wiring_ratchet.rs` exists (265 lines), with the
three tests `essentials_cover_the_abstract_primitive_stream_terminals` (`:136`),
`none_of_the_terminals_is_a_synthetic_stub` (`:183`) and
`the_terminals_survive_the_whole_real_jdk_boot` (`:231`), the last driving the
shared `#[path = "common/vm_init_boot_path.rs"] mod boot_path` model (`:69-70`).
`ABSTRACT_PRIMITIVE_STREAM_TERMINALS` (`:94-125`) holds exactly six triples —
`{Int,Long,Double}Stream.forEachOrdered` and `{Int,Long,Double}Stream.
summaryStatistics` — and `java/util/stream/Stream.forEachOrdered(Ljava/util/
function/Consumer;)V` is excluded with the reason written out at `:81-86`. Every
detail §6.3.1 claims about its own contents is true.

### 8.4 STALE — §4.3 and §6.2 describe a gap that has since been closed

Two claims that were live when written are not any more:

* **`IntStream.forEachOrdered(IntConsumer)V` is no longer "unmasked, live
  defect".** It is registered at `native-builtins/src/phases_late/streams.rs:475`,
  inside the essentials-reachable narrowed registrar of §8.2.
* **`Stream.forEachOrdered(Consumer)V` is no longer unregistered on the default
  path.** `native-collections/src/lib.rs:19764-19769`, inside
  `register_stream_natives` (`:19629`), reached from `register_collections_natives`
  (`:2387`) — live in a plain build. §6.3.1's "it is the next row to add" and
  §4.1's framing of it as rescued only by an interpreter hack are both overtaken.

`register_phase56_abstract_stream_terminals` (§6.2's proposed patch) exists
nowhere in the tree except inside this record, which is consistent with "not
applied" — and it no longer needs to be: both of its two triples are now covered
by the two registrars above. **§6.2 should be read as superseded, not pending.**

**The interpreter special case survives.** `vm/src/runtime/interpreter.rs:1007-1011`
still carries
`if method_name == "forEachOrdered" && method_descriptor == "(Ljava/util/function/Consumer;)V"`
→ re-dispatch as `forEach`, in the `!has_code` arm, with its comment at `:995-1006`
still naming `register_phase56_stream_extras` as the reason. It runs *above*
`resolve_native_for_dispatch`, so the new `native-collections` registration is
behaviour-neutral today (`native-collections/src/lib.rs:19750-19755` says so at
the registration site). **The deletion §4.1 asked for is now unblocked and is the
one open item this section leaves behind** — see residuals.

### 8.5 §5.1 — verdict survives; its stated MECHANISM should be re-scoped

`register_phase56_summary_stats` still exists (`phases_late/streams.rs:1942`),
its only caller is still `register_phase56_natives` (`streams.rs:29`), which is
still `register_synthetic_overrides`-only — **still dead, still "do not wire".**

But the reason has narrowed. The record says the natives "would write min and
max into the compensation accumulators". Since it was written,
`p56_double_stats_store` (`streams.rs:1908-1943`) grew a real-layout branch: it
reads `ctx.class_num_total_fields(class_id)` and, above `REAL_DSS_FIELD_MAX`,
writes `REAL_DSS_FIELD_*` = 0..5 (`streams.rs:1639-1644`), and the header at
`:1615-1628` records that `try_alloc_concurrent_synthetic` clamps the requested
slot count **up** to the real class's field count — so the literal `4` at
`streams.rs:1889` is a floor, not an under-allocation, and the
`summaryStatistics()` terminal writes the right slots on a real receiver.

The defect is still there, one layer over: the registrar's **accessors** do not
branch. `<init>()V`, `accept(D)V`, `getMin`, `getMax` and `toString` still use
the flat `STATS_FIELD_MIN`/`MAX` = 2/3 (`streams.rs:1629-1635`, uses around
`:2321-2420`), which on a real six-field `DoubleSummaryStatistics` are
`sumCompensation` and `simpleSum`. **§5.1's conclusion is unchanged; read its
mechanism as "the registrar's accessors", not "the registrar".**

### 8.6 §6.4.1 (`ConcurrentSkipListMap`) — CONFIRMED, all four legs; one line stale

`register_concurrent_skip_list_map_natives` is at
`native-collections/src/lib.rs:54440` (§5.3 says `51054` — stale), private, with
no real caller. The `let _ = register_concurrent_skip_list_map_natives;` no-op is
at `:2465` inside `register_collections_natives` (`:2360`), and the comment above
it still names the two triples that exist nowhere else. The guard test
`concurrent_skip_list_map_not_intercepted` is at `:59744` and asserts
`find(..).is_none()` for `<init>()V` and `put`. The `__test_cslm_*` hooks are at
`:54682`, `:54690`, `:54695`, `:54700`, consumed by
`native-collections/tests/gc_side_table_root_audit.rs`. **The "third state"
verdict — neither wire nor delete — holds on all four legs.**

### 8.7 §5.3 spot-checks — two rows need a word changed

| row | verdict |
|---|---|
| `register_p62_stamped_lock` | dead CONFIRMED, at `phases_late/concurrent.rs:2643` (record says `2636`). "Call site disabled" is imprecise: it has **no call site at all**. The *disabled* thing was `native-collections`' rival StampedLock block (`native-collections/src/lib.rs:2466ff`); `util_concurrent_ext.rs:6449-6452` states this in prose. |
| `register_tls_impl_natives` | "no caller" is **WRONG**. It is at `tls_impl.rs:1327` and has four callers, all test-only: `tls.rs:4778` and `:4828` inside `#[cfg(test)] mod tls_tests` (`:3160`–`:4893`), plus `tls_impl.rs:2727` and `:2752`. The correct row value is **test-only caller**, the same as `register_jboss_logmanager_natives`. |
| `register_apps_h2_overrides` | CONFIRMED. `apps_h2.rs:51`; its only reference is the dead-code silencer `let _ = apps_h2::register_apps_h2_overrides;` at `lib.rs:9118`, explained at `lib.rs:9111-9117` and again at `apps_h2.rs:86-87`. |

### 8.8 What this section does NOT establish

* The 301/825 census in §0/§2 was **not** re-run. It is a static graph analysis
  and rechecking it is its own lane; every number in §2 should still be read as
  of the date it was taken.
* Nothing was built. §7's three falsifiers all require a binary and all remain
  open.
* `native-collections/src/lib.rs:25943` (§4.1's `LongStream.mapToObj` row) was
  not re-checked. §4.1's `reflect_annotations.rs:515` citation is stale — the
  comment it names is at `reflect_annotations.rs:550-560` today.

### 8.9 Residual left by this re-verification

**Delete the `forEachOrdered` special case in `vm/src/runtime/interpreter.rs:1007-1011`.**
It was a workaround for a registration that did not ship; the registration now
ships (§8.4), the special case sits above native dispatch so it still wins, and
while it does, `Stream.forEachOrdered`'s *registered* implementation is
unreachable and untested. Deleting it needs a build and a run of the stream
vectors, which this pass could not do. Not this lane's file.
