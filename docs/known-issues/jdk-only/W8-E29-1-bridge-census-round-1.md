# W8-E29-1 — the first `Bridge` census: the largest `NativeKind` had never been measured

**Status: OPEN — a vector plus a population study.
`regression-suite/src/RJdkBridge1.java` is **394 checks over 268 registered
`Bridge` triples**, green on Microsoft OpenJDK 25.0.3+9, mutation-checked family
by family. It is **not** registered in `regression-suite/run.sh` — see
NOMINATIONS. This lane cannot build or run CratonVM: every statement about VM
behaviour below is labelled SOURCE-READ or PREDICTED, never MEASURED.**

`NativeKind` has three values. `Intrinsic` (645 rows) has had three generations
of census and they found atomic arrays with no bounds check at all, a `UUID`
parser wrong in both directions, `Math.pow` 44 ulp out, and five `Base64`
defects behind a 27/27-green fixture. `SyntheticStub` (1,304 rows) has had none.
`Bridge` — **9,799 rows, 8,777 distinct triples, the largest of the three by an
order of magnitude** — has had none either. This is the first.

---

## 0. The instrument, and a correction that changes every denominator

Everything a `--dump-native-registry` JSON says is **mode-specific**, and the
dump this lane was given (`scratchpad/p1/reg.json`, schema 4) says so in its own
header: `"mode": "compatible"`. Lane E23 established what that costs and this
lane re-derived it independently:

| | |
|---|---|
| `native-builtins/src/lib.rs` fn `register_synthetic_overrides`, body | lines **21588–24384** |
| its gate | `#[cfg(feature = "synthetic-jdk")]` |
| `.register(` calls inside it | **273** |
| dump rows whose `registered_by` cites a line in that range | **0** |
| dump rows citing that file at all (max cited line **43579**) | 1,449 |

So the dump cannot see a single one of those 273 registrations, while seeing
1,449 others in the same file. **Any count taken from the dump alone is a lower
bound for one mode, not a count.** Two further consequences this lane measured:

* **`invocations` is unusable.** Across the whole dump it totals **1,527**, of
  which 1,001 are `bridge` — spread over exactly **35 rows**. A hello-world run
  is not a reachability instrument. Every "is this exercised" number below comes
  from a JDK 25 ClassFile-API walk of the compiled fixture, not from
  `invocations`.
* **Kind is a property of the REGISTRAR, not of the call.** The registry has a
  category stack — `registry.set_category(NativeKind::Intrinsic)` /
  `set_category(__prev_cat)` — and `register()` is last-registration-wins. That
  is why every bridge row in the dump has `kind_stated: false, kind_chosen:
  true`. It also means **a source parse cannot tell you a triple's kind**; only
  a dump can, and only for the mode it was taken in.

### The union census (source-derived + dump-derived)

A parse of all 996 `.rs` files in the tree, resolving literal and
`let`-bound-literal arguments:

| | |
|---|---|
| `.register(` call sites in the working tree | **15,082** |
| resolved to a full literal triple | 12,889 sites → **10,306 distinct triples** |
| unresolved (class is a fn parameter, 1,122; all three, 665; …) | 2,193 |
| distinct triples in the dump (all kinds) | **10,531** |
| **union** | **14,293** |
| in both | 6,544 |
| source-only, invisible to this dump | **3,762** |
| dump-only, i.e. the source parse could not resolve it | 3,987 |

Both residuals are instrument limits, not findings. The 3,987 "dump-only" rows
are overwhelmingly parameterised registrars — `register_string_builder_natives
(registry, class)` is called once per builder class, so `java/lang/
AbstractStringBuilder` resolves to **0** source triples and 64 dump triples.
Read the two columns as two partial views, never as a difference.

### The finding that comes out of taking the union seriously

`register_synthetic_overrides` resolves to **253 triples**. Of those:

| | |
|---|---|
| **also a `Bridge` in the compatible-mode dump** | **164** |
| in the dump under some other kind | 15 |
| invisible to the dump | 74 |

Because `register()` is last-wins and that function runs *after* the general
registrars, **164 triples are `Bridge` in compatible mode and a different
implementation under the `Intrinsic` category in synthetic-jdk mode.** Heaviest:
`java/lang/Class` 32, `java/lang/Thread` 26, `java/io/PrintStream` 25,
`java/lang/reflect/Field` 24, `java/lang/Throwable` 12. A census of `Bridge`
taken in one mode is not the census of the other, and a fix to a bridge in one
mode may not be reached in the other. This is `[flag≠mode drops it]` at the
scale of a whole category.

---

## 1. The population (dump-derived, compatible mode)

| | |
|---|---|
| rows with `kind == "bridge"` | **9,799** |
| distinct (class, name, descriptor) triples | **8,777** |
| triples registered more than once | 902 triples / 1,022 surplus rows |
| rows that OVERWROTE a previous registration | 1,043 (1,022 over a bridge, **17 over an intrinsic**, 4 over a synthetic-stub) |
| rows with `invocations > 0` in the hello-world dump | 35 — see §0, do not use |

By namespace: **jdk-public 6,288 · jdk-internal (`jdk/`, `sun/`, `com/sun/`)
1,706 · application/library 783.** Registrar files, top five: `native-builtins/
src/lib.rs` 1,205 · `lang_misc.rs` 990 · `native-collections/src/lib.rs` 973 ·
`phases_late/nio_file.rs` 412 · `phases_late/foreign_ffm.rs` 393.

---

## 2. Triage — what a Bridge IS, and which ones are shadows

The dump carries `real_declaring_method: {loaded, declared, acc_native,
has_code}` per row. That is exactly the "why is this native here at all"
question, and it partitions the category cleanly:

| bucket | rows | triples | what it means |
|---|---|---|---|
| **A — SHADOW** `has_code && !acc_native` | 2,472 | 2,103 | the real JDK class declares this method **with a `Code` attribute** and the VM runs Rust instead. A retirement candidate *and* a divergence risk. |
| **B — legitimate** `acc_native` | 242 | 206 | the real JDK method is itself `native`. The VM MUST provide it. Not retirable. `jdk/internal/misc/Unsafe` 94, `java/lang/Class` 28, `java/lang/Thread` 21. |
| **C — abstract** declared, no `Code` | 400 | 365 | the owner is an interface or abstract class. The bridge is standing in for a *declaration*. `java/nio/file/Path` 45, `java/nio/ByteBuffer` 38, the four `Stream` interfaces 138. |
| **D — not declared here** loaded, `!declared` | 983 | 848 | the class was resolved and does **not** declare this method. Mostly INHERITED — 18 rows each on twenty exception classes (`fillInStackTrace`, `getMessage`, … declared on `Throwable`), 24 on `LinkedHashSet`. A subclass-scoped shadow of an inherited body. |
| **E — unresolved** not loaded | 5,702 | 5,255 | the class was not loaded in a hello-world run. 4,913 rows JDK, 789 rows application. Status unknown; **this bucket is the reason the triage below is partial.** |

**Answer to "which bridges could be retired": at least 2,103 distinct triples
(bucket A) shadow a working JDK implementation, and a further 848 (bucket D)
shadow an inherited one — against 206 that genuinely cannot be retired (B).**
Bucket E, 60% of the population, has never been adjudicated at all; adjudicating
it needs a dump from a run that actually loads those classes (`image_adjudication`
was `false` here), which is the single highest-value follow-up in this record.

### The shape-level triage (SOURCE-READ, not measured)

A source read of the seven families this vector drives, against the six hazards
this project keeps paying for. Everything here is read off the Rust, not
observed on a VM.

| family | verdict |
|---|---|
| `AtomicIntegerArray` / `AtomicLongArray` | **clean.** The long twins share `atomic_array_slot` → `atomic_array_index`, so the bounds check is the same one; `set(int,long)` reads `args[2]`, which is right because native `args` are **value-packed, one entry per descriptor token** (`vm/src/runtime/interpreter/invoke.rs:498-537`), not slot-packed. |
| `ArrayDeque` | **clean** on both contracts: empty-deque `removeFirst`/`getFirst`/`element`/`pop` raise `NoSuchElementException`, `poll`/`peek` return `Ok(Some(Object(None)))`, and every inserter funnels through `ad_refuse_null`. |
| `Vector` | bounds present and negatives rejected before the `usize` cast — but `get`/`elementAt`/`set`/`remove(int)` raise the **plain** `IndexOutOfBoundsException` (they share `native_al_*` with `ArrayList`), where a `Vector` receiver on HotSpot raises the `ArrayIndexOutOfBoundsException` **subclass**. `native_vec_set_element_at` uses `aioobe_index_only` — the file disagrees with itself. |
| `BigInteger` | divide/mod/modPow/modInverse-by-zero **clean**. `new BigInteger(byte[0])` returns **0** where HotSpot throws `NumberFormatException`; `new BigInteger(int,byte[])` accepts an out-of-range signum and signum-0-with-magnitude; `shiftLeft/shiftRight(Integer.MIN_VALUE)` uses `n.unsigned_abs()` → a `vec![0u32; 67_108_864]` (~256 MB) allocation instead of the specified `ArithmeticException`. |
| `Properties` | **null key answered with null instead of NPE in BOTH registrars.** In synthetic-jdk mode the winning `native_props_get_property` has **no `instanceof String` filter**, so `p.put("k", Integer)` then `getProperty("k")` hands an `Integer` back out of a `()Ljava/lang/String;` method. Winner is mode-dependent: collections wins in synthetic-jdk, the sidetable wins in mixed/real-JDK. |
| `AbstractStringBuilder` | index checks **clean and deliberate**. But `toString`/`substring`/`indexOf` go through `String::from_utf16_lossy`, so a lone surrogate becomes U+FFFD; `reverse()` is a plain code-unit swap that does **not** re-order surrogate pairs; and `repeat(II)Ljava/lang/StringBuilder;` is registered **twice**, the later inline closure clamping a negative count with `.max(0)` and truncating a code point with `code_point as u16` — and because that second registration spells `StringBuilder` literally, **`StringBuffer` keeps the correct throwing version. Same call, two answers, chosen by the static type.** |
| `URI` | absent-component contract (`-1`, `null`) **clean**; syntax errors raise a real `URISyntaxException`. The port is parsed with `p.parse::<i32>()`, i.e. Rust's grammar, which accepts a leading `+`/`-`; `new URI((String) null)` builds a URI over `""` instead of NPE and `URI.create(null)` returns a **null URI from a `()Ljava/net/URI;` method**. |

Two of these are `[1 of 10 callsites]` again: a correct implementation exists
and a later, worse registration shadows it (`repeat`), and a helper that does
the right thing is used by one of two sibling registrars (`Properties`).

---

## 3. The vector

`regression-suite/src/RJdkBridge1.java` — 394 checks, eleven families, no
lambdas, all operands out of `OPAQUE_*` arrays, all expected values measured on
HotSpot 25.0.3+9 before they were written.

```
CK RJdkBridge1 props=40      CK RJdkBridge1 treenav=42   CK RJdkBridge1 collect=19
CK RJdkBridge1 deque=33      CK RJdkBridge1 vector=20    CK RJdkBridge1 uri=50
CK RJdkBridge1 bytebuf=32    CK RJdkBridge1 sbidx=55     CK RJdkBridge1 surrog=22
CK RJdkBridge1 atomarr=26    CK RJdkBridge1 bigint=55
CK RJdkBridge1 checks=394
PASS RJdkBridge1 (394 checks)
```

**RUN THE FAMILIES IN SEPARATE PROCESSES FIRST.** With no arguments the eleven
run in ascending order of how likely each is to abort the VM rather than fail an
assertion, so a VM that dies in `bigint` has already reported the other ten —
but a VM that dies in `props` has reported nothing. `--only=<family>` runs one
alone; `--list` prints the names. Every call that passes an out-of-range index,
a negative count, a null where the JDK specifies a throw, or an unpaired
surrogate prints `CK RJdkBridge1 <family>-step=<call>` **before** the call, so on
an abort the last line of stdout names the killing call.

Suggested first pass, one process each:

```
--only=props --only=treenav --only=collect --only=deque --only=vector \
--only=uri --only=bytebuf --only=sbidx --only=surrog --only=atomarr --only=bigint
```

### Mutation check — every family, transcripts

Each mutation changes exactly one expected value (verified unique in the source)
and must turn its own family red. All eleven did:

```
MUT props   | AssertionError: stringPropertyNames must drop entries whose key OR value is not a String, got [s]
MUT treenav | AssertionError: descendingMap order
MUT collect | AssertionError: binarySearch miss must return -(insertion point) - 1 == -2
MUT deque   | AssertionError: push must insert at the FRONT and toArray must be in head-to-tail order, got [z, a, b, c]
MUT vector  | AssertionError: new Vector().capacity() is 10
MUT uri     | AssertionError: URI.getAuthority
MUT bytebuf | AssertionError: getShort of FF FE must be the SIGNED -2, got -2
MUT sbidx   | AssertionError: insert AT the length is legal and appends
MUT surrog  | AssertionError: codePointAt on an unpaired high surrogate must return the SURROGATE VALUE, not a combined code point and not U+FFFD
MUT atomarr | AssertionError: AtomicLongArray.set(int, long) must store all SIXTY-FOUR bits — the long value argument spans two frame slots; got 1122334455667788
MUT bigint  | AssertionError: mod of a negative value is the NON-NEGATIVE residue 5
```

The `--list` and `--only=` paths were smoke-tested separately
(`--only=deque` → `CK RJdkBridge1 deque=33 / PASS RJdkBridge1 (33 checks)`).

Two expected values in the first draft were wrong because they were written from
memory instead of from the transcript — `bitCount(-9)` (1, not 2) and a
30-digit quotient. Both were caught by the fixture itself on the first HotSpot
run. `[verbatim from the transcript, never from memory]` earns its place again.

### What each family is aimed at

| family | hazard | the discriminating rows |
|---|---|---|
| `props` | slot-index read with no type check; null contract | `put("k", Integer)` then `getProperty("k")` must be **null**; `getProperty(null)` must NPE; the defaults chain must not make a key a member (`size()==1`, `containsKey==false`, `get==null`) |
| `treenav` | `Ok(None)` vs throw over one state | `firstKey()` throws / `firstEntry()` is null; `subMap(b,a)` is `IllegalArgumentException`; null key NPEs even on an **empty** map |
| `collect` | negative counts, immutable views | `nCopies(-1)` is `IllegalArgumentException` not `NegativeArraySize`; `emptyList().get(0)` is exactly `IndexOutOfBoundsException`; `rotate(list,-1)` |
| `deque` | the asymmetric null rule | seven inserters NPE, four queries answer `false`; seven accessors throw, six return null; `new ArrayDeque(-1)` is **legal** |
| `vector` | legacy exception classes | `elementAt`/`get`/`set`/`remove(int)` must be `ArrayIndexOutOfBoundsException`, `firstElement` must be `NoSuchElementException`, `new Vector(-1)` must be `IllegalArgumentException` — three different classes for three shapes |
| `uri` | a Rust parser on a Java grammar | `http://h:-5/p` → port **-1**, host **null**, authority `h:-5`; `+80` likewise; `new URI(null)` NPEs; `create` and the ctor throw **different** classes |
| `bytebuf` | signed/unsigned, two bounds regimes | `getShort(FF FE)` is `-2` **and** `getChar` of the same bytes is `0xFFFE`; absolute out-of-range is `IndexOutOfBoundsException`, relative is `BufferUnderflow`/`Overflow`, position/limit is `IllegalArgumentException`; a limit below the mark discards the mark |
| `sbidx` | every index form, out of range | `charAt` is `StringIndexOutOfBounds` but `getChars` is plain `IndexOutOfBounds`; `setLength(-1)` is `StringIndexOutOfBounds` but `new StringBuilder(-1)` is `NegativeArraySize`; `repeat(cs,-1)` is `IllegalArgumentException`; `reverse()` keeps a pair in order |
| `surrog` | Rust `str` cannot hold a lone surrogate | a lone `\uD800` through `StringBuilder`, `Properties`, `TreeMap`, `ArrayDeque`, `Vector`, `URI`, `BigInteger`, asserted on **char values**, never on a printed form; `codePointAt` must be `0xD800`, not U+FFFD |
| `atomarr` | bounds + long slot packing | six out-of-range forms on each of the int and long arrays; `set(1, 0x1122334455667788L)` then `get` must return all 64 bits |
| `bigint` | division, `MIN_VALUE` negation, a parser | divide/remainder/mod/modInverse/modPow by zero; `shiftLeft(Integer.MIN_VALUE)` must answer **0** and not throw; `testBit(-1)` must be `ArithmeticException`; `new BigInteger(byte[0])` must be `NumberFormatException`; `"1_0"` must be `NumberFormatException` |

---

## 4. Coverage arithmetic

Driven-ness is measured by parsing `RJdkBridge1.class` with the JDK 25
`java.lang.classfile` API and intersecting its method references with the
registry — **not** with `invocations`.

| | |
|---|---|
| method references in the compiled fixture | **324** |
| of those, to the harness itself | 19 |
| to a JDK/library owner | **305** |
| → a **dump-confirmed `Bridge` triple** | **268** |
| → a dump triple of another kind | 9 (6 intrinsic, 3 synthetic-stub) |
| → source-only, invisible to the dump | 15 (`String.length/charAt/equals`, `BigInteger.<init>(String)`, `AtomicIntegerArray.toString`, …) |
| → no registration resolvable either way | 13 (interface owners, `Vector(Collection)`, `AssertionError.<init>`) |

**So: 268 driven / 8,777 distinct `Bridge` triples = 3.05%. Undriven: 8,509.**

That denominator is a compatible-mode lower bound (§0), and the 268 is a
lower bound too: a static owner of `java/util/List` or `java/util/SortedMap`
resolves at run time by walking the receiver's class chain, so those 13
"unresolved" references very likely land on a registered bridge of the
implementing class. Scored the way lane E23 scored interfaces, the 13 would
mostly disappear; this record does **not** claim them.

Per class, driven against that class's own bridge population:

| class | driven / bridge triples |
|---|---|
| `java/util/ArrayDeque` | **30 / 34** |
| `java/math/BigInteger` | **22 / 24** |
| `java/net/URI` | **24 / 29** |
| `java/util/Vector` | **20 / 29** |
| `java/util/TreeMap` | 27 / 45 |
| `java/nio/ByteBuffer` | 30 / 76 |
| `java/lang/StringBuilder` | 32 / 61 |
| `java/util/Properties` | 18 / 37 |
| `java/util/TreeSet` | 12 / 34 |
| `AtomicIntegerArray` / `AtomicLongArray` | 11 / 26, 7 / 26 |
| `java/util/Collections` | 9 / 25 |
| `java/lang/StringBuffer` | 8 / 64 |

### Undriven, with reasons

Of the 8,509 undriven distinct triples:

* **1,706 are `jdk/`, `sun/`, `com/sun/`** — not reachable from ordinary Java
  without `--add-exports`/`--add-opens`. Reaching them needs a fixture built
  with module flags, which `run.sh` supports but this vector does not use.
  (`jdk/internal/misc/Unsafe` alone is 27 + 94 + 120 rows across buckets.)
* **783 are application/library classes** (`org/…`, `io/netty/…`,
  `com/carrotsearch/…`, `brave/…`) — reachable only with that jar on the
  classpath. This is the Spring/netty/H2 suites' territory, not the regression
  suite's.
* **442 are on an inner or anonymous JDK class** (`java/lang/System$1` 39,
  `MethodHandles$Lookup` 25, `ConcurrentHashMap$KeySetView` 23, the
  `ValueLayout$Of*` family 15 each) — reachable only through the public API that
  hands the instance out, which is a per-class research problem.
* **353 are owned by an interface** (`Stream`/`IntStream`/`LongStream`/
  `DoubleStream` 138, `Path` 45, `List`/`Map`/`Set`/`Deque`/… the rest) — these
  should be scored against implementors before anyone reports them as a gap;
  E23 measured `java/util/List`'s 27-method gap collapsing to **1** under that
  scoring.
* the residue — **≈5,200 jdk-public, concrete, non-inner triples** — is the real
  target list for generation 2. It is reachable from ordinary Java and nothing
  drives it.

---

## NOMINATIONS

This lane does not own `regression-suite/run.sh`. Nominated change, one line:

```
CORE_CLASSES="… RSimpleDateFormatZone RJdkIntrinsics3 RJdkBridge1"
```

`CORE_CLASSES`, not `JDKONLY_CLASSES`: these are JDK library semantics, the
bridges are registered in both arms, and the file has no mode dependence — the
same reasoning that put `RJdkIntrinsics` and `RJdkIntrinsics2` there. It takes
no JVM flags and no classpath beyond the suite's own.

**Before it goes in `run.sh`, run the eleven families in eleven separate
processes** (§3). `run.sh` drives a class with no arguments, which is the
all-eleven-in-order path; if any family aborts the VM, the registration will
truncate the class's own output and the `-step=` breadcrumb is what will name
the call.

---

## What a second generation needs

1. **A dump per mode.** One `--dump-native-registry` from a `synthetic-jdk`
   build and one from `real-jdk`, not just `compatible`. Without it the 164
   mode-swapped triples in §0 and the 273 gated registrations are guesses.
2. **A dump with `image_adjudication: true`, from a run that loads the
   classes.** Bucket E is 5,255 triples — 60% of the category — whose
   shadow-or-not status is simply unknown. That single flag turns the largest
   bucket in §2 into a triage.
3. **A module-flagged vector** for the 1,706 `jdk/`/`sun/` triples. Most of the
   `Unsafe`, `VM`, `SharedSecrets` and JFR surface is bucket B or A and is
   entirely unmeasured.
4. **Interface scoring**, per the coordinator's instruction: resolve the 353
   interface-owned triples against the receiver-class walk before reporting them
   as a gap.
5. **A retirement experiment.** Bucket A is 2,103 triples that shadow working
   JDK bytecode. The cheap version of the experiment is a build flag that skips
   one registrar's `register()` calls and re-runs the suite; if nothing reddens,
   the shadow was dead weight and its divergence risk goes to zero by deletion
   rather than by repair.
6. The specific shapes §2 lists — `Properties`' missing `instanceof String`
   filter, `BigInteger`'s `unsigned_abs` shift, the double-registered
   `StringBuilder.repeat`, `Vector`'s exception class, `from_utf16_lossy` in
   `toString`/`substring`, `reverse()`'s surrogate pairs, `URI`'s
   `parse::<i32>` port — each already has a check in the vector. Running it is
   what turns them from SOURCE-READ into MEASURED.

## Honest summary

Is `Bridge` healthy? **Unknown, and this record is the first evidence either
way.** What is now measured: the category is 8,777 triples of which **2,103
demonstrably shadow working JDK bytecode**, only **206 genuinely cannot be
retired**, and **5,255 have never been adjudicated at all**. The seven families
read in source were mixed — `AtomicIntegerArray`/`AtomicLongArray` and
`ArrayDeque` looked genuinely clean, which is a real result and the first
positive evidence anyone has for any part of this category; the other five each
had at least one shape that a Java program can reach. The vector that decides it
is 394 checks and 268 triples — **3.05% of the category**.
