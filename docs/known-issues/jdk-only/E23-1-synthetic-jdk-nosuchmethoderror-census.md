# The synthetic-JDK `NoSuchMethodError` census: 7,700 methods over 845 classes, and the one that costs 88 fixtures

> **Status: MEASUREMENT, plus one behaviour-neutral deletion.** The census is
> the deliverable. The only code change is the removal of three *dead duplicate*
> registrations in `native-builtins/src/deprecated_io_util.rs` (§6), which were
> proven byte-identical to the copies that win, so nothing observable moves.
>
> Every HotSpot number is **MEASURED** on Microsoft OpenJDK 25.0.3+9-LTS.
> Every CratonVM number is **DERIVED FROM SOURCE** — this lane may not build and
> may not run the VM. Every CratonVM "after" is **PREDICTED**.

Lane E23, 2026-08-13. Answers E17-1 §9 N3 (the census) and §9 N2 (the duplicate
registrars). Files touched: `native-builtins/src/deprecated_io_util.rs`,
`native-builtins/src/deprecated_util.rs` (comment only), and this record.

---

## 1. The numbers, up front

| | |
|---|---|
| synthetic classes with a real JDK 25 counterpart | **845** |
| their public declared methods + public constructors | **15,123** |
| covered by a registered native, a declared stub method, or an ancestor's | 7,423 |
| **`NoSuchMethodError` set (the gap)** | **7,700 — 50.9 %** |
| classes with **zero** gap (synthetic mode can answer everything) | 232 |
| classes where the gap is **the whole class** (the stub can do nothing at all) | 38 |
| registered names with **no JDK 25 class at all** behind them | 59 |

E17-1 §9 N3 predicted "`Character`'s 57 is unlikely to be the largest". It is
**rank 17**. The largest is `jdk/internal/misc/ScopedMemoryAccess` at **238**,
four times as big.

The `Character` row reproduces E17-1 §5.1 exactly, which is how this census is
calibrated: **97** public declared entries (96 methods + the deprecated
`Character(char)` ctor), **39** registered, **0** declared, **58** gap = E17-1's
**57** methods plus that ctor. Independently, the corpus scan (§4) finds exactly
the two `charcls` call sites E17-1 §6 named by hand — `digit(II)I` and
`isISOControl(C)Z` — and nothing else on that class.

---

## 2. The arithmetic, and where each term comes from

E17-1 N3's formula, per class:

```
  (public methods on the real JDK 25 class)
- (triples registered as natives)
- (methods the synthetic stub declares)
- (the same, on any ancestor in the synthetic superclass chain)
= the NoSuchMethodError set
```

| term | source | checked against |
|---|---|---|
| real public methods | `Class.getDeclaredMethods()` + `getDeclaredConstructors()`, filtered to `Modifier.isPublic`, on JDK 25 | **MEASURED** — the JDK 25 image itself |
| registered triples | union of two sources, see §3 | one is the **stale dump**, one is the **working tree** |
| declared stub methods | `classloading/src/class_manager.rs` `synthetic_stub_ctor_methods`, parsed | **WORKING TREE** |
| superclass chain | `classloading/src/class_manager.rs` `jdk_superclass`, 192 arms parsed, default `java/lang/Object` | **WORKING TREE** |

Metric choice: `getDeclaredMethods()` filtered to public — **not** `getMethods()`
— because that is the unit E17-1's 96 used, and because inherited methods are
handled explicitly by the superclass-chain term instead of being double-counted.
Bridge and synthetic methods are counted (E17-1's list includes
`compareTo(Ljava/lang/Object;)I`, a bridge). Public constructors are counted
because a stub with no `<init>` cannot be `new`'d, which is a `NoSuchMethodError`
of exactly the same kind — §5 shows that this is where the worst row lives.

**The gap is a LOWER BOUND.** Coverage is a union of three over-approximating
sources and 1,882 `.register(` call sites in the tree compute their class or
descriptor strings at run time and could not be resolved statically. Every
unresolved site can only *shrink* the gap. Nothing in this arithmetic can invent
a gap that is not there — the resolution path is exact on the descriptor at
every step (E17-1 §5.2, re-verified in §7 below).

---

## 3. `scratchpad/p1/reg.json` is not stale on four rows. It is missing a whole mode.

E17-1 §5.4 named four `Character` rows in the dump as stale. That is true and it
is the smaller half of the problem.

**The dump was captured from a build without the `synthetic-jdk` cargo feature.**
`native-builtins/src/lib.rs:21558` declares

```rust
#[cfg(feature = "synthetic-jdk")]
pub fn register_synthetic_overrides(registry: &mut NativeMethodRegistry) {
```

— 2,796 lines and **273** `.register(` calls, spanning `native-builtins/src/lib.rs`
lines 21558–24354. **Not one row in `reg.json` cites a `registered_by` line in
that range.** The dump's own header says `"mode": "compatible"`.

The consequence is not marginal:

| class | triples in `reg.json` | triples in the working tree | missing from the dump |
|---|---|---|---|
| `java/lang/String` | 24 | 89 | **65** — including `length()I`, `charAt(I)C`, `equals`, `hashCode`, `substring`, `indexOf`, `split`, every `valueOf`, and nine `<init>` overloads |
| `java/util/Arrays` | 19 | 47 | 28 |
| `java/lang/Character` | 39 | 40 | 1 (`<clinit>()V` — not a public method, so E17-1's 39/57 **survives intact**) |

Trusting the dump alone would have reported a gap of **8,969**, i.e. **1,269
phantom `NoSuchMethodError`s**, and would have put `java/lang/String` — a class
whose synthetic-mode natives are all present — on the roadmap with 74 missing
methods instead of its real 15.

This is the `[feat≠MODE]` / `[2cfgs]` shape: a `#[cfg(feature = ...)]` arm is
invisible to an instrument built without that feature, and the instrument
reports its own build, not the VM.

**Therefore:** every registration number in this record is the **union** of the
dump and a working-tree source parse (`scratchpad/e23/srcreg.py`, 14,423 register
sites scanned, 10,108 triples resolved across 1,408 classes). Where the two
disagree the working tree wins. Numbers taken from the dump alone are marked as
such and appear only in §3 and §6.

---

## 4. The ranked census

Top 30 by gap. `registered` and `declared` count only entries that correspond to
a method the real class actually has; `inherited` is the count rescued by an
ancestor. `fixture callers` is §4.1's reachability measure.

| # | class | kind | public | registered | declared | inherited | **gap** | fixture callers |
|---|---|---|---:|---:|---:|---:|---:|---:|
| 1 | `jdk/internal/misc/ScopedMemoryAccess` | class | 253 | 15 | 0 | 0 | **238** | 0 |
| 2 | `java/awt/Component` | abstract | 205 | 11 | 0 | 1 | **193** | 0 |
| 3 | `jdk/internal/misc/Unsafe` | class | 321 | 135 | 0 | 0 | **186** | 0 |
| 4 | `java/util/Arrays` | class | 214 | 36 | 1 | 0 | **178** | 19 |
| 5 | `java/sql/ResultSet` | interface | 193 | 21 | 0 | 0 | **172** | 0 |
| 6 | `java/sql/DatabaseMetaData` | interface | 177 | 13 | 0 | 0 | **164** | 2 |
| 7 | `java/sql/CallableStatement` | interface | 121 | 6 | 0 | 0 | **115** | 0 |
| 8 | `sun/awt/SunToolkit` | abstract | 104 | 0 | 0 | 1 | **103** | 0 |
| 9 | `java/util/concurrent/CompletableFuture` | class | 123 | 38 | 0 | 1 | **84** | 0 |
| 10 | `jdk/internal/foreign/AbstractMemorySegmentImpl` | abstract | 95 | 10 | 0 | 3 | **82** | 0 |
| 11 | `jdk/internal/access/JavaLangAccess` | interface | 88 | 13 | 0 | 0 | **75** | 0 |
| 12 | `javax/swing/JFileChooser` | class | 73 | 2 | 0 | 1 | **70** | 0 |
| 13 | `sun/java2d/SunGraphics2D` | class | 95 | 27 | 0 | 1 | **67** | 0 |
| 14 | `javax/swing/JOptionPane` | class | 69 | 2 | 0 | 1 | **66** | 0 |
| 15 | `java/lang/StringUTF16` | class | 63 | 1 | 0 | 0 | **62** | 0 |
| 16 | `java/lang/System$1` | class | 88 | 27 | 0 | 0 | **61** | 0 |
| 17 | **`java/lang/Character`** | class | 97 | 39 | 0 | 0 | **58** | 4 |
| 18 | `com/…/xerces/…/XSSimpleTypeDecl` | class | 58 | 1 | 0 | 2 | **55** | 0 |
| 19 | `java/awt/Font` | class | 61 | 5 | 0 | 3 | **53** | 0 |
| 20 | `jdk/internal/access/SharedSecrets` | class | 65 | 13 | 0 | 1 | **51** | 0 |
| 21 | `java/math/BigDecimal` | class | 78 | 30 | 29 | 0 | **48** | 1 |
| 22 | `java/lang/invoke/MemberName` | class | 57 | 7 | 0 | 3 | **47** | 0 |
| 23 | `java/awt/image/Raster` | class | 46 | 0 | 0 | 0 | **46** | 0 |
| 24 | `java/nio/CharBuffer` | abstract | 69 | 19 | 0 | 5 | **45** | 1 |
| 25 | `java/awt/Toolkit` | abstract | 49 | 5 | 0 | 0 | **44** | 0 |
| 26 | `java/net/URLConnection` | abstract | 53 | 8 | 1 | 1 | **44** | 0 |
| 27 | `java/nio/DirectByteBuffer` | class | 51 | 7 | 0 | 0 | **44** | 0 |
| 28 | `java/sql/PreparedStatement` | interface | 58 | 14 | 0 | 0 | **44** | 0 |
| 29 | `javax/swing/SwingUtilities` | class | 46 | 3 | 0 | 0 | **43** | 0 |
| 30 | `com/sun/tools/javac/file/JavacFileManager` | class | 44 | 2 | 0 | 0 | **42** | 0 |

By package family:

| family | classes | public | gap | % | fixture callers |
|---|---:|---:|---:|---:|---:|
| `java/util` | 194 | 3,548 | 1,413 | 40 % | 163 |
| `java/lang` | 186 | 2,814 | 1,070 | 38 % | 125 |
| `jdk/internal` | 63 | 1,469 | 990 | 67 % | 0 |
| `sun/*` | 97 | 1,332 | 814 | 61 % | 0 |
| `java/awt` | 19 | 781 | 678 | 87 % | 0 |
| `java/sql` | 13 | 757 | 619 | 82 % | 2 |
| `java/nio` | 65 | 1,195 | 562 | 47 % | 11 |
| `com/sun` | 28 | 383 | 301 | 79 % | 0 |
| `javax/swing` | 4 | 229 | 211 | 92 % | 0 |
| `java/io` | 42 | 557 | 174 | 31 % | 8 |
| everything else | 134 | 2,058 | 868 | 42 % | 15 |

(324 distinct-caller references in total; the corpus is 277 fixture classes, so a
class can and does appear in several rows.)

### 4.1 The reachability measure, and why a raw count is the wrong ranking

A method nothing calls is not a defect. The ranking below is by **measured
reachability**: every `.class` under `regression-suite/build` (277 classes) was
walked with the JDK 25 ClassFile API and every `invoke*` constant-pool reference
recorded (`scratchpad/e23/Sites.java`, 3,543 distinct triples). Two columns come
out of it — how many **distinct fixture classes** name a triple, and how many
**call sites** exist. Distinct callers is the better signal: it says how much of
the corpus stops working, not how loop-heavy one fixture is.

This corpus is the right one *because it is application bytecode*. In
synthetic-JDK mode there is no JDK bytecode, so no JDK-internal call ever
happens; the only calls that can reach a synthetic stub are the ones an
application's constant pool names. The `invocations` counters in `reg.json` are
not a substitute — they total **1,527** across the whole dump, from a trivial
workload, with only 59 of 11,748 rows non-zero.

**The interface correction.** A naive triple-level ranking puts `java/util/List`
(33 fixture callers), `java/util/Set` (30) and `java/util/Map` (17) near the top.
They are **not** gaps. `invokeinterface` on `List.contains` arrives with an
`ArrayList` receiver, and `vm_exec.rs`'s recovery walk probes the **receiver's**
runtime class chain for a native before it gives up. Re-scoring those rows
against their implementors:

| interface | naive gap | after receiver-side coverage | reachable remainder |
|---|---:|---:|---:|
| `java/util/List` | 27 | **1** (`reversed()`) | 0 callers |
| `java/util/Set` | 11 | **0** | 0 |
| `java/util/Map` | 15 | **0** | 0 |
| `java/util/Collection` | 11 | **1** (`parallelStream()`) | 0 |

Of the 180 gap triples with at least one fixture call site, **75 are
interface/abstract instance methods that the receiver walk can rescue** and 105
are not. Only the 105 belong on a roadmap.

### 4.2 The corrected ranking — what actually breaks

| callers | sites | triple | why it cannot be rescued |
|---:|---:|---|---|
| **88** | **157** | `java/lang/AssertionError.<init>(Ljava/lang/Object;)V` | `invokespecial`, exact class. §5. |
| 3 | 3 | `java/io/PrintStream.<init>(Ljava/io/OutputStream;ZLjava/lang/String;)V` | `invokespecial` |
| 2 | 15 | `java/lang/invoke/MethodHandles$Lookup.dropLookupMode(I)…` | final class |
| 2 | 12 | `java/util/Arrays.toString([B)Ljava/lang/String;` | `invokestatic` |
| 2 | 6 | `java/lang/String.<init>(Ljava/lang/String;)V` | `invokespecial`, final class |
| 2 | 5 | `java/nio/file/Files.getFileAttributeView(…)` | `invokestatic` |
| 2 | 3 | `java/util/Arrays.equals([J[J)Z` | `invokestatic` |
| 2 | 3 | `java/lang/Character.toChars(I)[C` | `invokestatic` |
| 2 | 2 | `java/util/Collections.reverseOrder()Ljava/util/Comparator;` | `invokestatic` |
| 2 | 2 | `java/io/PrintStream.checkError()Z` | concrete class |
| 1 | 11 | `java/util/Arrays.toString([D)Ljava/lang/String;` | `invokestatic` |
| 1 | 6 | `java/util/concurrent/locks/StampedLock.isWriteLockStamp(J)Z` | final class |
| 1 | 5–3 | `java/util/Arrays.mismatch` × 8 overloads | `invokestatic` |
| 1 | 4 | `java/time/Instant.parse(Ljava/lang/CharSequence;)Ljava/time/Instant;` | `invokestatic` |
| 1 | 1 | `java/lang/Character.digit(II)I`, `isISOControl(C)Z` | E17-1 §6, independently rediscovered |

Note what is *absent* from this list: AWT, Swing, `java/sql`, `jdk/internal`,
`sun/*` — **3,188 of the 7,700 gap methods are in package families with zero
fixture reachability at all.** They are the majority of the count and the
minority of the problem. `jdk/internal/misc/ScopedMemoryAccess` tops the raw
table with 238 and is called by nothing in the corpus; it matters only to a lane
driving `java.lang.foreign`, and then it matters a great deal.

---

## 5. The finding: `assert` and `throw new AssertionError(msg)` cannot run in synthetic-JDK mode

`java/lang/AssertionError.<init>(Ljava/lang/Object;)V` is named by **88 of the
277 fixture classes (32 %)** at **157 call sites** — twenty-nine times the next
row. It is not registered and not declared.

**MEASURED on HotSpot 25** (`javap -p java.lang.AssertionError`):

```
public java.lang.AssertionError();
private java.lang.AssertionError(java.lang.String);        <-- PRIVATE
public java.lang.AssertionError(java.lang.Object);
public java.lang.AssertionError(boolean|char|int|long|float|double);
public java.lang.AssertionError(java.lang.String, java.lang.Throwable);
```

`AssertionError(String)` **is private in the JDK**. So `javac` compiles both

```java
throw new AssertionError("boom");     // -> <init>:(Ljava/lang/Object;)V
assert cond : "assert-msg";           // -> <init>:(Ljava/lang/Object;)V
```

to the `(Object)` overload — **MEASURED**, `javap -c` on a fixture compiled by
JDK 25 `javac`.

CratonVM registers exactly four ctor descriptors on each of the 69 entries of
`throwable_classes` (`native-builtins/src/lang_misc.rs:2459`, loop at 2550):
`()V`, `(String)V`, `(String,Throwable)V`, `(Throwable)V`. For `AssertionError`
that means:

* `(String)V` → **shadows a private JDK method**. No bytecode can call it. Dead.
* `(Throwable)V` → **does not exist on `AssertionError` at all**. Dead.
* `(Object)V` → the one `javac` emits → **not registered, not declared, gap**.

`synthetic_stub_ctor_methods`' `is_throwable_like` arm declares the *same four*
descriptors, so the stub does not rescue it either, and `<init>` is
`invokespecial` on the exact class — the receiver-class recovery walk resolves to
`AssertionError` itself, and the descriptor-quirks fallback never rewrites
argument types (re-verified in the working tree, §7). The result is
`NoSuchMethodError: java/lang/AssertionError.<init>(Ljava/lang/Object;)V`, on the
error path, where it replaces the diagnostic the program was trying to produce.

This lands directly on top of the lane that made `-ea` honoured: `assert`
statements now execute, and their failure path cannot construct its own error.

### 5.1 The blanket 4-descriptor loop, quantified over all 69 classes

62 of the 69 `throwable_classes` entries resolve on JDK 25 (7 have no JDK
counterpart). Diffing the four registered descriptors against each class's real
public constructor set:

| | count |
|---|---:|
| registered descriptors that are **not a public ctor** on the real class (dead, or shadowing a private one) | **103** |
| **public ctors `javac` can emit that are unregistered** | **16** |

The 16 misses, by class:

| class | unregistered public ctors |
|---|---|
| `java/lang/AssertionError` | `(Ljava/lang/Object;)V` `(Z)V` `(C)V` `(I)V` `(J)V` `(F)V` `(D)V` |
| `java/io/UncheckedIOException` | `(Ljava/io/IOException;)V` `(Ljava/lang/String;Ljava/io/IOException;)V` |
| `java/lang/IndexOutOfBoundsException` | `(I)V` `(J)V` |
| `java/util/MissingResourceException` | `(Ljava/lang/String;Ljava/lang/String;Ljava/lang/String;)V` |
| `java/text/ParseException` | `(Ljava/lang/String;I)V` |
| `java/lang/reflect/InvocationTargetException` | `(Ljava/lang/Throwable;Ljava/lang/String;)V` |
| `java/lang/ArrayIndexOutOfBoundsException` | `(I)V` |
| `java/lang/StringIndexOutOfBoundsException` | `(I)V` |

`UncheckedIOException` is the sharpest case after `AssertionError`: **all four**
registered descriptors are dead — the class has only the two `IOException`-typed
ctors — so it is a stub that cannot be constructed by any means while carrying
four natives that look like it can.

---

## 6. What the right closure is, per gap family

The trap this task was written against: **do not register natives for the gaps.**
Real-JDK mode already answers them from real bytecode, and a native on such a
class shadows working code. E17-1 §3 priced one such registration at 12,246
wrong answers. §6.4 below prices two more at 949 and 1,010.

| gap family | size | right closure | why not a native |
|---|---:|---|---|
| **Ctors of throwable-family stubs** (§5) | 16 public ctors, 103 dead descriptors | **Fix the registrar's data, not its shape.** Replace `lang_misc.rs`'s blanket 4-descriptor loop with a per-class descriptor list derived from the real class, and add the same descriptors to `synthetic_stub_ctor_methods`' `is_throwable_like` arm. `AssertionError(Object)` first. | This one *is* a native gap, not a class-library gap: `Throwable`'s state (message/cause) is native-backed in **both** modes (`native_exc_init_message`), so nothing is being shadowed. The 103 dead descriptors should go at the same time — each is a registration that can only ever win a race it should lose. |
| **`Character`'s 57** (E17-1 §5) | 58 | **Declare on the stub, or leave to bytecode.** Unchanged from E17-1: `digit(II)I` and `getNumericValue(I)I` must stay unregistered. | Rust's Unicode DB is a newer version than the image's (E7-1 §3). Any reimplementation answers a different Unicode. |
| **`java/util/Arrays`** | 178, 19 callers | **Register** the reachable arithmetic (`mismatch` ×8, `toString([BDJ…)`, `equals([J[J)`, `compare`) — these are pure array arithmetic with no Unicode, no locale and no image state. | Nothing to shadow that is subtler than the code you would write; `Arrays` is `final` with no observable state. This is the one large row where "register it" is the correct answer. |
| **Interface method tables** (all `kind == interface` rows) | 1,147 raw, ~3 real | **Leave alone.** The receiver-class recovery walk already answers them; the census over-attributes (§4.1). | Registering on the interface would shadow the implementor's native, which is the one that knows the layout. |
| **`java/awt`, `javax/swing`, `sun/awt`, `sun/java2d`** | 1,153 | **Leave alone, and stop registering.** 87–92 % uncovered with zero reachability; several rows (`Raster`, `SampleModel`, `ColorModel`, `SunToolkit`) have `registered = 0` *and* a stub, i.e. a class that exists only to fail later. | A partial AWT is worse than none: it converts `NoClassDefFoundError` at the top of a feature into `NoSuchMethodError` deep inside it. |
| **`jdk/internal/*`, `sun/nio/ch/*`** | 1,141 | **Demand-drive.** Register only what a named workload reaches; `Unsafe` (135 of 321) and `ScopedMemoryAccess` (15 of 253) show the shape already works. | These are private JDK plumbing whose contracts change per release; a speculative implementation is a maintenance liability with no oracle. |
| **`java/sql/*`** | 619 | **Leave alone** unless a JDBC lane exists. All seven rows are interfaces; a driver supplies the implementation and the receiver walk finds it. | Registering on `ResultSet` would shadow H2's own driver classes. |
| **The 38 all-gap classes** | — | **Decide: implement or refuse.** A stub with zero coverage cannot serve any call; `create_synthetic_stub` already has the `ClassNotFoundException` refusal path, which produces a diagnosable failure at load instead of a mystery at first call. | — |
| **The 59 fabricated names** | — | **Audit separately.** These are classes CratonVM registers natives on or stubs for that **do not exist in JDK 25 at all** (`java/lang/Compiler`, `java/net/PlainSocketImpl`, `java/util/HashMap$KeyItr`, `java/util/Comparator$Native`, `…$RustJvmImpl`). Some are deliberate internals; some are removed JDK classes. | — |

---

## 7. What was re-verified in the working tree rather than taken from E17-1

E17-1 §5.2 traced the resolution path and concluded `NoSuchMethodError`. Two of
its load-bearing claims were re-read against the working tree because the whole
census rests on them:

* **`find_with_descriptor_quirks` never rewrites argument types.** Confirmed:
  `native-api/src/registry.rs:7399` `resolve_id_with_descriptor_quirks` bails
  unless the descriptor has whitespace/NUL or an unterminated `L…` **return**
  type; the at-most-three variants it builds are `trimmed`, `no_newlines` and a
  return-type fixup. So a registered `digit(CI)I` can never answer a `digit(II)I`
  call site, and a registered `AssertionError.<init>(String)V` can never answer
  `(Object)V`.
* **Nothing mints a body for an unregistered triple.** The terminal in
  `vm/src/vm/vm_exec.rs` (the `NoSuchMethodError` at ~24989) is reached after:
  the registry probe up the **declaring** class's superclass chain, the same walk
  up the **receiver's** runtime class chain, the annotation-proxy rescue, and the
  serialization-hook neutral result. The first two are why §4.1's interface
  correction is necessary and are modelled in the census; the last two are
  narrow, named escapes that cannot apply to a `Character` or `AssertionError`
  call.

Not re-verified, and stated as such: `synthetic_stub_ctor_methods`' three
`Collections$Synchronized*` arms contain `else` branches that the stub parser
attributes to all three names, over-counting their `declared` column by up to 7.
Those classes are package-private in the JDK and score 0 reachability, so the
error does not touch any conclusion.

---

## 8. Task 2 — the three duplicate registrations, and the 949 they hide

E17-1 §9 N2 nominated `isJavaLetter(C)Z`, `isJavaLetterOrDigit(C)Z` and
`isSpace(C)Z` as a duplicate-registrar shape.

### 8.1 Which copy wins — confirmed

`reg.json` records all three of `native-builtins/src/deprecated_io_util.rs`'s
copies (lines 933/944/955) as `owns_slot: false`, `overwrote: bridge`. The
winners are `native-builtins/src/deprecated_util.rs:2076/2078/2083`
(`native_char_is_java_letter` / `_or_digit` / `native_char_is_space`, bodies at
865/877/891). *This is a dump-derived claim* — but it is one the dump can be
trusted on, because both registrars are unconditional (neither is inside
`register_synthetic_overrides`) and both appear in it.

### 8.2 Do the copies disagree? No — 0 disagreements over 65,536 chars

Both bodies were transliterated into `scratchpad/e23/Witness23.java` and diffed
over every `char`:

```
winner (deprecated_util.rs) vs loser (deprecated_io_util.rs), 65536 chars, all 3 methods:
  disagreements = 0
```

The bodies differ only in inessentials: the loser narrows to `u16` before
widening to `u32` while the winner casts the `i32` straight to `u32` (identical
over the `(C)Z` domain), and the two `isSpace` `matches!` arms list the same five
code points in a different order.

**So the deletion is behaviour-neutral, and that is the finding: three dead
registrations, no divergence.** Done in
`native-builtins/src/deprecated_io_util.rs` — the whole
`register_character_deprecated` function, its call site, and the three unit tests
that covered it, replaced by tombstones. The tests are worth naming: they called
`setup()`, which registers only that file, so three green tests were exercising a
body that never runs in the VM.

### 8.3 The bigger finding: the copy that *wins* is wrong 949 / 1,010 times

**MEASURED, HotSpot 25, all 65,536 `char` values:**

| triple | mismatches vs HotSpot 25 | first |
|---|---:|---|
| `isSpace(C)Z` | **0** | — |
| `isJavaLetter(C)Z` | **949** | `U+00A2` |
| `isJavaLetterOrDigit(C)Z` | **1,010** | `U+0000` |

Both wrong bodies model `isJavaIdentifierStart`/`Part` as
`is_alphabetic()` / `is_alphanumeric()` `|| '_' || '$'`. The JDK's predicate is a
different set in two directions:

* it **admits** `Sc` (currency) and `Pc` (connector punctuation) — the first
  misses are `¢ £ ¤ ¥` at `U+00A2..U+00A5`, `getType == 26` (`CURRENCY_SYMBOL`);
* it **excludes** the `Other_Alphabetic` combining marks Rust's `Alphabetic`
  includes — the first over-accept is `U+0345` (`getType == 6`,
  `NON_SPACING_MARK`), then the whole `U+0363..` block.

`java/lang/Character` is `has_code: true` in real-JDK mode (E17-1 §1), so **both
natives shadow correct image bytecode with a wrong answer** — the W7-98 "the
class does not need these natives; it needs them gone" shape, now with two more
triples of evidence.

**The right closure is deletion, not repair.** An exact body needs a Unicode
general-category table, and Rust's is a newer Unicode version than the image's —
precisely the trap E17-1 §5 documents for `getType`. Deleting the two
registrations lets real-JDK mode fall through to bytecode and answer correctly,
at the price of turning them into two more synthetic-mode census rows, where they
belong. That is a mode-visible behaviour change, so it is **nominated (§9 N2),
not taken here**; a pointer comment carrying these numbers was left at the
winning registration site.

Witness limitation, stated plainly: `Character.isAlphabetic(int)` is used to
stand in for Rust's `char::is_alphabetic()`. The two compute the same derived
Unicode property, so the witness measures the **algorithm** — it cannot see the
Unicode-*version* skew of E7-1 §3, and 949/1,010 are therefore lower bounds.
Mutation-checked (a witness that cannot fail measures nothing): dropping the `$`
arm moves `isJavaLetter` from 949 to 950.

---

## 9. Nominations

### N1 — `native-builtins/src/lang_misc.rs`: the throwable ctor loop registers 103 descriptors that do not exist and misses the 16 that do

Owned by whoever owns `lang_misc.rs`. This is the highest-reachability defect in
the census (§5). The loop at `native-builtins/src/lang_misc.rs:2550` registers
four fixed `<init>` descriptors on each of the 69 `throwable_classes` entries.

*old, verbatim* (`native-builtins/src/lang_misc.rs`, at the head of the ctor
block inside `for cls in throwable_classes.iter()`):

```
        // Constructor overloads for synthetic Throwable-family stubs.
        // Some bootstrap paths instantiate subclasses directly (for example
        // InternalError and NoSuchMethodError wrappers). Register all common
        // ctor descriptors here so both synthetic and real-JDK flows can
        // initialize message/cause consistently.
        // audit-2026-05-16: previously this registered the generic
        // `native_noop_with_this`, which left `cause` un-initialised; the
        // (String) ctor uses the JDK sentinel `cause = this`, so a later
        // `initCause()` succeeded for (String) ctors but failed for noargs.
```

*new:*

```
        // Constructor overloads for synthetic Throwable-family stubs.
        // Some bootstrap paths instantiate subclasses directly (for example
        // InternalError and NoSuchMethodError wrappers). Register all common
        // ctor descriptors here so both synthetic and real-JDK flows can
        // initialize message/cause consistently.
        // audit-2026-05-16: previously this registered the generic
        // `native_noop_with_this`, which left `cause` un-initialised; the
        // (String) ctor uses the JDK sentinel `cause = this`, so a later
        // `initCause()` succeeded for (String) ctors but failed for noargs.
        //
        // MEASURED against JDK 25 over all 69 entries of `throwable_classes`
        // (E23-1 5.1): these four descriptors are NOT a public constructor on
        // the real class 103 times, and 16 public constructors that `javac`
        // actually emits are missing. The worst row is
        // `AssertionError.<init>(Ljava/lang/Object;)V` — `AssertionError(String)`
        // is PRIVATE in the JDK, so both `throw new AssertionError(msg)` and
        // `assert c : msg` compile to the `(Object)` overload, which is
        // unregistered: 88 of 277 regression-suite classes, 157 call sites.
        // `java/io/UncheckedIOException` is worse in kind — all four of these
        // descriptors are dead on it and neither of its two real ctors is here.
        // Fix the DATA (a per-class descriptor list), not the shape.
```

Plus the code change: give `AssertionError` its seven public overloads
(`(Object)`, `(Z)`, `(C)`, `(I)`, `(J)`, `(F)`, `(D)`) and drop `(String)V` /
`(Throwable)V` from it; give `UncheckedIOException` its two.

### N2 — `native-builtins/src/deprecated_util.rs:2076/2078` — I own the file and did **not** delete them

I own this file and could have deleted the two wrong registrations. I did not,
because deletion is mode-visible: real-JDK mode gains 1,959 correct answers,
synthetic-JDK mode gains two `NoSuchMethodError`s, and this lane may not run the
VM to see which fixture that moves. Nominated to whoever can run both modes. The
numbers, the mechanism and the reason not to "fix" the Rust predicate instead are
in §8.3 and in a comment at the registration site.

### N3 — `classloading/src/class_manager.rs`: `synthetic_stub_ctor_methods`' `is_throwable_like` arm mirrors N1's bug

Same four descriptors, same 103/16 split, same fix. It must move with N1 or the
stub and the registry will disagree about which ctors exist.

### N4 — whoever maintains `--dump-native-registry`: the dump cannot see its own build

`scratchpad/p1/reg.json` omits all 273 registrations made by
`register_synthetic_overrides` because it was captured without the
`synthetic-jdk` feature, and nothing in the file says so beyond
`"mode": "compatible"` (§3). Two suggestions, in order of value:

1. Have the dump emit the **cargo feature set** it was built with, not just the
   run-time mode. `"mode": "compatible"` reads as "this is the compatible-mode
   registry", when what it means is "this build has no synthetic-jdk arm at all".
2. Have the synthetic-jdk gate refuse to accept a dump whose feature set does not
   match the mode being analysed. E17-1 §5.4's four-row staleness note was
   correct and *far* too narrow; the next reader of that file needs to be stopped,
   not warned.

### N5 — `regression-suite`: the census is a ratchet, and nothing ratchets it

7,700 is a number with no test behind it. `scratchpad/e23/` recomputes it in
about twenty seconds with no VM run and no build (§10). A gate that recomputes
the per-class gap for a short allow-list — `java/lang/Character`,
`java/lang/AssertionError`, `java/util/Arrays`, `java/lang/String` — and fails on
an increase would have caught the `AssertionError` hole the day the `-ea` lane
landed. Nominated to whoever owns the synthetic-JDK gate.

---

## 10. Reproducing

Lane scratchpad `.../scratchpad/e23/`. All HotSpot-only; **no CratonVM binary and
no cargo build required.**

| file | what it does |
|---|---|
| `Enum25.java` | JDK 25 reflection over a class list → public declared methods + ctors with JVM descriptors (`real25.txt`, 845 resolved / 59 missing) |
| `parse_stub.py` | parses `classloading/src/class_manager.rs` `synthetic_stub_ctor_methods` out of the **working tree** → 512 declarations over 98 classes (`stub_declared.json`) |
| `srcreg.py` | parses every `.register(` / `.register_with_kind(` in the workspace with function-scoped `let` resolution → 10,108 triples over 1,408 classes (`src_registered.json`); 14,423 sites scanned, 1,882 unresolved |
| `mkuniv.py` | builds the class universe from `scratchpad/p1/reg.json` ∪ the stub arms (904 JDK-namespace names) |
| `Sites.java` | ClassFile-API walk of `regression-suite/build` → 3,543 `(owner,name,desc)` triples with call-site and distinct-caller counts (`sites.txt`) |
| `final_census.py` | the arithmetic, including the `jdk_superclass` chain → `census_final.json`, the ranked tables |
| `Witness23.java` | the §8 source witness: both duplicate bodies vs HotSpot 25 over 65,536 chars, mutation-checked |
| `AE.java` | the §5 oracle: what `javac` emits for `throw new AssertionError(msg)` and `assert c : msg` |

### What this proves and what it does not

It proves the **arithmetic**: given the working tree's registrations and stub
declarations and JDK 25's real method tables, at least 7,700 public methods over
845 classes have no implementation reachable in synthetic-JDK mode. It proves the
**reachability** ranking against a real corpus of application bytecode. It proves
the two duplicate `Character` bodies are identical to each other and wrong
against HotSpot.

It does **not** prove anything about a CratonVM binary — none carrying these
observations exists — and every "synthetic mode throws here" is read off the
resolution path in the source, not off a run. The one measurement to demand of
whoever runs it: in synthetic-JDK mode, any fixture whose failure path is
`throw new AssertionError(msg)` should abort with
`NoSuchMethodError: java/lang/AssertionError.<init>(Ljava/lang/Object;)V`
instead of its own assertion message. If it prints its message, §5's mechanism is
wrong and something is answering that descriptor.
