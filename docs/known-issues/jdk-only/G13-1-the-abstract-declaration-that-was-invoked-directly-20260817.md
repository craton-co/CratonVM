# G13-1 — the abstract declaration that was invoked directly: three vectors, two mechanisms

**Status:** DIAGNOSED-MEASURED / PARTIALLY-FIXED. Every "before" below is
MEASURED on a real binary. Both root causes are MEASURED. The one code change
this lane was permitted to make is in `vm/src/runtime/interpreter.rs` and its
"after" is **NOT** measured — see §9, which says so in the plainest terms
available, because this directory's standing rule is that a prediction must
never be dressed as a result.

**Provenance.** Binary: checkpoint **`d2e127930`**, at
`C:/craton/target-fcheck/release/cratonvm.exe` — the same binary
`BASELINE-20260817`'s re-measurement section describes. Oracle: HotSpot
25.0.3+9-LTS. Vectors from `C:/craton/cvm-mergecheck/regression-suite/build`.
Probes written for this lane, in `scratchpad/probe/`:
`G13Probe.java` (map-view Collection surface × 5 map families),
`G13Slot.java` (view-carrier field slots by reflection),
`G13Http.java` (the `HttpRequest`/`HttpClient` accessor surface),
`G13Path.java` (`PathMatcher` and the three `newDirectoryStream` overloads).

> **Note on binaries.** This lane began on the `d87dff06a`+2 binary and the
> binary was replaced under it, mid-lane, by `d2e127930`. **Every number in
> this record was re-taken on `d2e127930`**, including the corpus-wide census
> in §6 (105 classes × 2 policy modes, re-run in full). The two binaries agreed
> on every figure that appears here — but they were re-taken rather than
> carried over, because "measured on a binary" means nothing without naming
> which.

---

## 0. The headline

`BASELINE-20260817` asks the question directly:

> Whether it is one mechanism or three is **not yet established** and must not
> be assumed from the matching text.

Measured. **It is two.** And the split is not the one the three type names
suggest: `java.net.http.HttpRequest` and `java.nio.file.PathMatcher` are the
same bug; `java.util.Map` is the odd one out.

| | `RJdkOptionalShape` | `RCrypto` | `RJdkMapViews` |
|---|---|---|---|
| message | `AbstractMethodError: java/net/http/HttpRequest.version()Ljava/util/Optional;` | `AbstractMethodError: java/nio/file/PathMatcher.matches(Ljava/nio/file/Path;)Z` | `AbstractMethodError: java/util/Map.isEmpty()Z` |
| receiver's runtime class | **`java/net/http/HttpRequest`** (cid 702) | **`java/nio/file/PathMatcher`** (cid 539) | an **`Object[]` of length 11** (cid 0, `kind=Array`) |
| i.e. the receiver IS the resolved abstract type | **yes** | **yes** | **no — not even an instance of it** |
| how it got there | `HttpRequest$Builder.build()` mints an instance of the abstract class; **4 of 7** accessors have no native | `FileSystem.getPathMatcher()` mints an instance of the interface; **0 of 1** methods have a native | a **field-slot collision** hands the view's element buffer over as if it were the backing `Map` |
| reaches `interpreter.rs` Path B? | no (`[CANONICAL_CENSUS] rows=0`) | no (`rows=0`) | **yes, once** |
| policy-dependent? | **no** — identical without `--jdk-only` | **no** | **yes** — `Compatible` takes the substitution and answers a *silently wrong* result |
| **mechanism** | **A** | **A** | **B** |
| owning file | `native-builtins/src/net_phase_e.rs` | `native-builtins/src/phases_late/nio_file.rs` | `native-collections/src/lib.rs` |

**Mechanism A** — a CratonVM native fabricates an instance of an
abstract class or interface, and a method invoked on it has no native
registration. Selection finds nothing because there *is* nothing: no bytecode,
no native, anywhere in the receiver's chain. `AbstractMethodError` is the
correct JVMS answer to the question the interpreter was asked; the wrong thing
happened one call earlier, when an abstract type was instantiated.

**Mechanism B** — the receiver is not an instance of the resolved
interface at all. A real JVM answers `IncompatibleClassChangeError` here; this
VM instead substitutes a canonical concrete class's native, which under
`Compatible` reads an array through a bucket layout and answers **empty**, and
under `--jdk-only` is refused and falls through to `AbstractMethodError`.

All three root causes are outside this lane's one file. All are nominated in §8.

---

## 1. The one line that separates them

```bash
export JAVA_HOME="C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot"
cd C:/craton/cvm-mergecheck
CRATONVM_DBG_NOCODE=1 CRATONVM_DISABLE_DEFAULT_WATCHDOG=1 \
  C:/craton/target-fcheck/release/cratonvm.exe --java-home "$JAVA_HOME" \
  --jdk-only -cp regression-suite/build <VECTOR> 2>&1 | grep -a DBG_NOCODE
```

```text
RJdkOptionalShape  … java/net/http/HttpRequest.version()…  | recv_cid=702 recv_class=java/net/http/HttpRequest
RCrypto            … java/nio/file/PathMatcher.matches()…  | recv_cid=539 recv_class=java/nio/file/PathMatcher
RJdkMapViews       … java/util/Map.isEmpty()Z             | recv_cid=0   recv_class=java/lang/Object
```

One line each, and it is where this lane's first hypothesis died. But note what
the third line does **not** say. **`recv_cid=0 recv_class=java/lang/Object`
does not mean "an object of class Object".** `class_id_of` returns
`ClassId(0)` for anything never stamped — including every array
`alloc_ref_array` produces (`ctx.new_ref_array(ClassId::new(0), n)`) — and
`get_class(0)` happens to resolve to `java/lang/Object`. An `Object[]` receiver
and a class-less allocation printed identically, and the rescue chain treats
them completely differently. §5(c) fixes that line.

---

## 2. Mechanism A, instance 1 — `RJdkOptionalShape`

`RJdkOptionalShape` never reaches Path B and fails **identically** without
`--jdk-only`. `HttpRequest$Builder.build()` (`net_phase_e.rs:12660`,
`invocations=2`, `owns_slot=true`) mints an object stamped with the abstract
class `java/net/http/HttpRequest`; HotSpot returns a
`jdk.internal.net.http.ImmutableHttpRequest`. Registry dump for that class:

```
method      ()Ljava/lang/String;      net_phase_e.rs:12484   owns_slot=true
uri         ()Ljava/net/URI;          net_phase_e.rs:12491   owns_slot=true
timeout     ()Ljava/util/Optional;    net_phase_e.rs:12455   owns_slot=true  invocations=2
newBuilder  … (2 statics)
```

Three instance accessors registered. The class declares seven. `G13Http.java`,
MEASURED against HotSpot:

| call | HotSpot | CratonVM |
|---|---|---|
| `plain.method()` / `uri()` / `timeout()` | GET / the URI / `Optional.empty` | identical |
| `plain.version()` | `Optional.empty` | **AbstractMethodError** |
| `plain.bodyPublisher()` | `Optional.empty` | **AbstractMethodError** |
| `plain.expectContinue()` | `false` | **AbstractMethodError** |
| `plain.headers()` | `HttpHeaders …{}` | **AbstractMethodError** |
| `full.*` (the same four) | HTTP_2 / a publisher / false / 1 | **AbstractMethodError** ×4 |
| `plain.getClass()` | `jdk.internal.net.http.ImmutableHttpRequest` | `java.net.http.HttpRequest` |

The sibling family is the control that makes this a registration gap rather
than a design gap: **`HttpClient`'s accessor surface is 7 of 7 correct**
(`connectTimeout`, `version`, `followRedirects`, `authenticator`, `proxy`,
`cookieHandler`, `executor`) — built the same way, in the same file, and also
minted as an instance of its own abstract class. `HttpClient`'s registrar
finished the job; `HttpRequest`'s stopped after three.

## 3. Mechanism A, instance 2 — `RCrypto`, and it is the cheapest repro

`RCrypto.java` names neither `PathMatcher` nor `newDirectoryStream`; the call
comes from inside JDK code. `[CANONICAL_CENSUS] rows=0`. `G13Path.java`,
MEASURED on both VMs:

| call | HotSpot | CratonVM `--jdk-only` |
|---|---|---|
| `FileSystems.getDefault().getClass()` | `sun.nio.fs.WindowsFileSystem` | **identical** |
| `fs.getPathMatcher("glob:*.txt").getClass()` | `sun.nio.fs.WindowsFileSystem$1` | **`java.nio.file.PathMatcher`** |
| `glob.matches(a.txt)` | true | **AbstractMethodError** |
| `glob.matches(a.bin)` | false | **AbstractMethodError** |
| `fs.getPathMatcher("regex:…").getClass()` | `sun.nio.fs.WindowsFileSystem$1` | **`java.nio.file.PathMatcher`** |
| `regex.matches(b.txt)` | true | **AbstractMethodError** |
| `Files.newDirectoryStream(dir, "*.txt")` | 1 | **AbstractMethodError** |
| `Files.newDirectoryStream(dir)` | 2 | 2 |
| `Files.newDirectoryStream(dir, aUserFilter)` | 1 | 1 |

Note the shape of that table. The `FileSystem` itself is **real and correct**;
the two overloads that do not route through `getPathMatcher` are **correct**; a
user-supplied `DirectoryStream.Filter` lambda **works**. Exactly one thing is
fabricated, and it is fabricated with no method at all:

```
java/nio/file/FileSystem  getPathMatcher (Ljava/lang/String;)Ljava/nio/file/PathMatcher;
                          nio_file.rs:7113  owns_slot=true  invocations=1
```

and **no `java/nio/file/PathMatcher.matches` row exists in the registry.** The
object handed back to JDK bytecode has a single declared method and zero
implementations of it. That is the same sentence as §2, with the ratio at its
limit: `HttpRequest` was 3 of 7, `PathMatcher` is 0 of 1.

`RCrypto` is indeed the cheapest repro of Mechanism A — `G13Path.java` above
reproduces it in three lines with no crypto in sight — but the orchestrator's
framing that it involves "no synthetic carrier" is what the measurement
corrects: **the matcher IS a synthetic carrier.** What is real is everything
around it, which is precisely why it took a sibling lane's correct
`newDirectoryStream` fix to expose it. Before that fix the glob overload
returned every entry and never called the matcher, so `RCrypto` was green over
a filter that was never invoked.

---

## 4. Mechanism B — `RJdkMapViews`, a slot collision, proven by reflection

### 4.1 The family, measured

`G13Probe.java`: ten Collection operations × the `values()` and `keySet()`
views × five map families, both VMs. Of 150 comparable rows, **7 diverge, and
all 7 are `LinkedHashMap.values()`**:

| row | HotSpot | CratonVM `--jdk-only` | CratonVM `Compatible` |
|---|---|---|---|
| `LinkedHashMap.values.size` | 3 | **0** | **0** |
| `LinkedHashMap.values.isEmpty` | false | **true** | **true** |
| `LinkedHashMap.values.toArray.len` | 3 | **AbstractMethodError** | **0** |
| `LinkedHashMap.values.newArrayList.size` | 3 | **AbstractMethodError** | 3 |
| `LinkedHashMap.values.toArrayTyped.len` | 3 | **AbstractMethodError** | **0** |
| `LinkedHashMap.values.stream.count` | 3 | **AbstractMethodError** | **0** |
| `LinkedHashMap.values.forloop` | 3 | **AbstractMethodError** | **0** |

`HashMap`, `TreeMap`, `Hashtable` and `ConcurrentHashMap` values views are
clean, and so is every `keySet()`. The `--jdk-only` column is *louder*; the
`Compatible` column is *worse*: a three-entry map's `values()` iterates zero
times and nothing says a word.

### 4.2 The cause: `elementData` lands on `this$0`

`MAP_VIEW_CARRIERS` mints a values view under its real JDK class and keeps the
view's state in `java/util/ArrayList`'s own resolved slots. The carrier doc
argues this is safe because those slots sit "past the single `this$0` these
classes declare". In the real-JDK layout `AbstractList.modCount` is slot 0,
`ArrayList.elementData` slot **1**, `ArrayList.size` slot **2**.

`javap -p`, JDK 25.0.3+9 — five of the six carriers declare exactly one field,
and one does not:

```text
java.util.HashMap$Values                    final HashMap this$0;        -> this$0 @0
java.util.TreeMap$Values                    final TreeMap this$0;        -> this$0 @0
java.util.TreeMap$EntrySet                  final TreeMap this$0;        -> this$0 @0
java.util.Hashtable$ValueCollection         final Hashtable this$0;      -> this$0 @0
ConcurrentHashMap$ValuesView (super CollectionView)  final CHM map;      -> map    @0
java.util.LinkedHashMap$LinkedValues        final boolean reversed;      -> reversed @0
                                            final LinkedHashMap this$0;  -> this$0 @1
```

So for `LinkedValues` — and **only** `LinkedValues` — `al_set_data(list, buf)`
writes the element buffer into absolute slot 1, which is `this$0`.
`values_view_class_source` then resolves `this$0` **by name** (it resolves by
name precisely *because* `LinkedValues` puts it at slot 1 — its own doc says
so) and reads the buffer back.

**Proof, MEASURED on both VMs** (`G13Slot.java`, each run with
`--add-opens java.base/java.util=ALL-UNNAMED`):

```text
                                        HotSpot                   CratonVM --jdk-only
HashMap$Values.this$0                   java.util.HashMap         null
LinkedHashMap$LinkedValues.reversed     java.lang.Boolean         java.lang.Boolean
LinkedHashMap$LinkedValues.this$0       java.util.LinkedHashMap   [Ljava.lang.Object;[len=11]
TreeMap$Values.this$0                   java.util.TreeMap         null
```

`len=11` is `make_view_list_of`'s own
`cap = max(vals.len(), AL_DEFAULT_CAPACITY = 10) + 1` for a three-element view.
It is not a coincidence-shaped number; it is that buffer.

(The `null` rows are a second, milder finding: `this$0` is never populated on
*any* carrier — the source map lives only in the buffer's trailing capacity
slot. Nominated as N2.)

### 4.3 …and how an `Object[]` comes to be asked whether it is empty

`vc_route` (`native-collections/src/lib.rs:14738`) rebuilds a view by asking
the source map for its entries:

```
native_al_to_array(LinkedValues)
  -> vc_route
     -> values_view_class_source(view)   == the Object[11] buffer, not the map
     -> collect_entries_any(ctx, buffer)
        -> is_native_bucket_map(buffer) == false
        -> ctx.invoke("java/util/Map", "isEmpty", "()Z", [buffer])   <-- HERE
```

That is the only site in `native-collections` that invokes `isEmpty` on the
interface name `java/util/Map` (one hit, line 13570). In `execute`:

* `java/util/Map.isEmpty()Z` resolves to the abstract declaration — no `Code`;
* no native is registered for it. MEASURED from `--dump-native-registry`:
  `java/util/Map` carries **8** interface-door rows — `size`, `forEach`, `get`,
  `put`, `containsKey`, `keySet`, `values`, `entrySet` — and `isEmpty` is not
  one of them, although `java/util/Collection.isEmpty()Z` **is** registered;
* the receiver-own-class rescue and Path A are skipped (`recv_cid ==
  ClassId::new(0)`);
* **Path B** fires, maps `java/util/Map -> java/util/HashMap`, and under
  `--jdk-only` refuses the substitution, falling through to
  `AbstractMethodError`.

MEASURED, exactly once:

```
[CANONICAL_CENSUS] rows=1
[CANONICAL] java/util/Map	java/util/HashMap	isEmpty	1
```

**So the message names `java/util/Map` and the defect is in
`LinkedHashMap$LinkedValues`.** No interface door is missing that would fix
this vector; registering `java/util/Map.isEmpty` would only make the wrong
answer quiet again.

---

## 5. What this lane changed — and why it fixes none of the three vectors

`vm/src/runtime/interpreter.rs`, three changes, all confined to Mechanism B and
to diagnosis. **Mechanism A cannot be addressed from this file at all**: for
`HttpRequest.version()` and `PathMatcher.matches()` there is no bytecode and no
native anywhere in the receiver's chain, the receiver's runtime class IS the
resolved class (so the receiver-walk rescue is correctly skipped by
`target_cid != class_id`), and the only remaining move would be to fabricate a
return value — which the site's own comment already forbids, in terms this
record endorses: *"Fabricating a benign result here is forbidden: it would make
a non-empty collection silently appear empty."*

**(a) An array receiver can never take the Path B substitution.** JLS 4.10.3:
an array type's only superinterfaces are `java.lang.Cloneable` and
`java.io.Serializable`. Neither is in the substitution table, so for an array
receiver the substitution is not a shim that is probably right — it is provably
wrong, and it is the shape that hides its wrongness best, because every
canonical native reads its receiver through a layout an array does not have and
answers **empty** instead of refusing. That is exactly how `Compatible` mode
reports a three-entry `values()` view as empty. The site now raises
`IncompatibleClassChangeError` naming the requested interface — which is also
what a real JVM raises when `objectref` is not an instance of the resolved
interface (JVMS §6.5, *invokeinterface*).

**(b) The substitution table is now one function,
`canonical_concrete_for_interface`,** rather than an inline `match`, so the
refusal in (a) and the substitution itself cannot come to list different names.
Pinned by four unit tests in a new inline `#[cfg(test)] mod
g13_array_receiver_tests` — inline in `interpreter.rs` rather than in
`interpreter/tests.rs`, because this lane owns exactly one file.

**(c) `CRATONVM_DBG_NOCODE` now prints `recv_kind` and `recv_is_declaring`.**
§1 is the argument. `recv_kind` separates an `Object[]` receiver from a
class-less allocation, which the line could not do. `recv_is_declaring`
(`recv_cid == class_id`) is the Mechanism A/B discriminator directly:
`true` means a native minted an instance of an abstract type and the method has
no native either — go add a registration; `false` means dispatch was handed
something that is not an instance of the resolved type — go fix the caller. One
line now tells the next lane which of the two it is looking at, which is the
question this record existed to answer.

---

## 6. Blast radius of (a), MEASURED — and the part of it that is *not* zero

Every one of the 105 main classes in `regression-suite/build` was run twice —
`--jdk-only` and `Compatible` — under `CRATONVM_DBG_CHECK_OVERRIDE=1`,
collecting every `[CANONICAL]` row. Run in full on `d87dff06a`+2 and **again
in full on `d2e127930`**, with identical results:

```
--jdk-only :  RJdkMapViews [CANONICAL] java/util/Map  java/util/HashMap  isEmpty  1
Compatible :  (no rows, in any of the 105)
```

**Across the corpus: one row, one vector, one call, and that vector is already
red.** No currently-green vector reaches the code (a) changes, in either mode.

That is a claim about *the corpus*, not about the VM, and stating it as "Path B
is unreachable in Compatible mode" would be false — this lane nearly wrote it.
`G13Probe`, which calls `LinkedHashMap.values().toArray()` directly, produces
**10** substitutions in `Compatible` and 5 in `--jdk-only`. `RJdkMapViews`
misses the Compatible path only because `new ArrayList<>(collection)` takes a
registered constructor native there instead of calling `c.toArray()`, so its
Compatible run reaches a wrong answer by a different route and fails later, at
`valuesLiveness` line 120, "view size at capture".

So (a) is a no-op **on the corpus** and deliberately not a no-op in general: in
`Compatible`, `values().toArray()` on a `LinkedHashMap` currently runs
`native_map_is_empty` against an `Object[]`, gets "empty", and returns a
zero-length array for a three-element view. After (a) that call raises
`IncompatibleClassChangeError`. **That is a behaviour change to a path no
corpus vector exercises and some application code will**, and it is the change
this record argues for: the answer it replaces is wrong, silent, and
undetectable from inside Java.

This re-takes, on both post-merge binaries, the measurement the interpreter's
own JDK-ONLY-WAVE2 §8 comment reports from August (`[CANONICAL_CENSUS] rows=0`
in both). It is now non-zero — for this bug and nothing else.

---

## 7. The six vectors, MEASURED before, on `d2e127930`

`--jdk-only`, both VMs, stdout diffed after `tr -d '\r'` and stripping
`^\[cratonvm\]`:

| vector | HotSpot | CratonVM before | verdict |
|---|---|---|---|
| `RJdkMapViews` | exit 0, 7 `CK` lines | **exit 1, 0 lines** | RED — dies before its first family line |
| `RJdkOptionalShape` | exit 0, 30 lines | **exit 1, 16 lines** | RED — after `misc=130`, `process=172` |
| `RCrypto` | exit 0, 20 lines | **exit 1, 16 lines** | RED — first missing line `keygen2arg=32,IllegalArgumentException` |
| `RJdkCollections` | exit 0, 7 lines | exit 0, 7 lines, **diff empty** | GREEN |
| `RJdkViews` | exit 0, 19 lines | exit 0, 19 lines, **diff empty** | GREEN |
| `RCollections` | exit 0, 1 line | exit 0, 1 line, **diff empty** | GREEN |

---

## 8. Nominations

**N1 — `native-collections/src/lib.rs` (the `RJdkMapViews` root cause,
Mechanism B).** `make_view_list_of` / `al_set_data` / `al_set_size` write
`java/util/ArrayList`'s absolute `elementData`/`size` slots (1 and 2) into every
`MAP_VIEW_CARRIERS` receiver. `java/util/LinkedHashMap$LinkedValues` is the one
carrier that declares **two** fields (`reversed` @0, `this$0` @1), so
`elementData` lands on `this$0` and `values_view_class_source` reads the element
buffer back as the source map. Measured consequence: 7 of 7
`LinkedHashMap.values()` operations diverge; in `Compatible` the view is
silently empty, in `--jdk-only` it throws. `al_slots_for_uncached` already has
the right shape for the fix — it special-cases `java/util/Vector` because Vector
has its own layout — and `LinkedValues` needs the same treatment: place the
carrier's list slots **above the carrier's own declared field count**, or move
the view's state off ArrayList's absolute slots for the view carriers entirely.
Do **not** fix it by registering `java/util/Map.isEmpty()Z` as an interface
door: that restores the silently-empty answer and removes the only signal.
Verify with `G13Slot.java` — landed when CratonVM reports
`LinkedHashMap$LinkedValues.this$0 -> java.util.LinkedHashMap`.

**N2 — `native-collections/src/lib.rs` (same family, milder).** No values or
keySet carrier ever populates `this$0` / `map`: HotSpot reports the backing map,
CratonVM reports `null` for `HashMap$Values`, `TreeMap$Values` and the rest. The
source lives only in the element buffer's trailing capacity slot. Any reflective
reader, and any JDK bytecode that escapes `force_native_over_real_jdk_bytecode`,
sees a null enclosing map. MEASURED by `G13Slot.java`.

**N3 — `native-builtins/src/net_phase_e.rs` (the `RJdkOptionalShape` root
cause, Mechanism A).** `java/net/http/HttpRequest` is missing four instance
accessors, all of which the builder already records state for:

- `version()Ljava/util/Optional;` — set by `HttpRequest$Builder.version(…)` at `:12653`
- `bodyPublisher()Ljava/util/Optional;` — set by `POST`/`PUT`/`method` at `:12541`–`:12601`
- `expectContinue()Z` — set by `HttpRequest$Builder.expectContinue(Z)` at `:12647`
- `headers()Ljava/net/http/HttpHeaders;` — set by `HttpRequest$Builder.header(…)` at `:12571`

Register them beside `timeout()`, which is the exact pattern that works.
`HttpClient`'s registrar in the same file is the model: it registers all seven
of its accessors and all seven are measured correct. Verify with
`G13Http.java`; landed when all 22 rows match HotSpot except `getClass()`.

Line numbers here are from the `d2e127930` build and **that file is being
edited by another lane right now** (525 insertions in the working tree as this
was written), so navigate by the `let req = "java/net/http/HttpRequest";`
binding rather than by line. Re-checked against the live working tree:
`req` still carries exactly five rows — `newBuilder` ×2, `timeout`, `method`,
`uri` — so this nomination is not already done.

**N4 — `native-builtins/src/phases_late/nio_file.rs` (the `RCrypto` root cause,
Mechanism A).** `FileSystem.getPathMatcher(String)` at `:7113` returns an object
stamped `java/nio/file/PathMatcher` — the interface — and **no
`java/nio/file/PathMatcher.matches(Ljava/nio/file/Path;)Z` native is registered
anywhere**, so the returned object has zero usable methods. Register `matches`
on that class, honouring both syntaxes `getPathMatcher` accepts (`glob:` and
`regex:`); the glob→regex translation the JDK does in `sun.nio.fs.Globs` is the
reference. Verify with `G13Path.java`; landed when `glob.matches(a.txt)=true`,
`glob.matches(a.bin)=false`, `regex.matches(b.txt)=true` and
`newDirectoryStream(dir, "*.txt")=1`. **Do not close this by making
`newDirectoryStream`'s glob overload skip the matcher** — that is precisely the
fabricated success `d378eee51` removed, and re-adding it would make `RCrypto`
green over a lie for the second time.

**N5 — `docs/known-issues/jdk-only/BASELINE-20260817.md`** (not mine; `INDEX.md`
is shared and this lane may not touch it either). Two edits.

The `## Triage` table's `owner` column for two rows says `interface doors`. It
is wrong for both, and it is the label that sent this lane at `C7-2`/`C13-1`
first:

- exact old text: `` | `RJdkMapViews` | `AbstractMethodError: method java/util/Map.isEmpty()Z has no Code attribute` | interface doors | ``
- exact new text: `` | `RJdkMapViews` | `AbstractMethodError: method java/util/Map.isEmpty()Z has no Code attribute` | `native-collections` view carriers (G13-1 N1) | ``

and

- exact old text: `` | `RJdkOptionalShape` | `AbstractMethodError: method java/net/http/HttpRequest.version()Ljava/util/Optional; has no Code attribute` | interface doors | ``
- exact new text: `` | `RJdkOptionalShape` | `AbstractMethodError: method java/net/http/HttpRequest.version()Ljava/util/Optional; has no Code attribute` | `net_phase_e.rs` missing accessors (G13-1 N3) | ``

And the open question at the end of the `d2e127930` section is now answered:

- exact old text: `Whether it is one mechanism or three is **not yet established** and must not be assumed from the matching text.`
- exact new text: `**ANSWERED by `G13-1` (MEASURED): it is TWO.** `HttpRequest.version()` and `PathMatcher.matches()` are one mechanism — a native mints an instance of an abstract type and the method invoked on it has no native either (`recv_cid == class_id`). `Map.isEmpty()` is a different one — the receiver is an `Object[]` that is not an instance of the resolved interface at all, handed over by a field-slot collision in `LinkedHashMap$LinkedValues`. Neither is the interface-door family. Three separate fixes, in three separate files, none of them `interpreter.rs`.`

---

## 9. What this lane did NOT do

* **It did not build the binary, and therefore did not measure its own
  change.** The lane brief forbids `cargo build` / `cargo check` / `cargo test`.
  Everything in §§0–4, 6 and 7 is measured on `d2e127930`; §5's three changes
  are **PREDICTED to compile and PREDICTED to behave as described**.
  `rustfmt --edition 2021 --check` was run on the file **in place, in its tree**
  (not on a copy — see the warning about vacuous copy checks) and reports no
  diff in any region this lane touched; the 16 diffs it does report in
  `interpreter.rs` are pre-existing and unchanged in count and content across
  every edit. That is a parse check, not a type check.
  **The next lane to build should confirm change (a) actually fires:** the DBG
  line will read `recv_kind=Array recv_is_declaring=false`, and the
  `RJdkMapViews` failure should change from `AbstractMethodError …
  java/util/Map.isEmpty()Z` to `IncompatibleClassChangeError: array receiver
  does not implement …`. If it still reads `AbstractMethodError`, the receiver
  is not an array and §4.3's chain has a link this lane got wrong.
* **It did not fix any of the three vectors.** All three root causes are in
  files owned by other lanes: N1, N3, N4. This lane's change makes one of them
  fail more honestly and cheaper to diagnose; it closes nothing.
* **It did not touch `native-collections/src/lib.rs`, `native-builtins/`,
  `classloading/`, `interpreter/typecheck.rs`, `interpreter/tests.rs`,
  `jit/helpers.rs`, `regression-suite/`, `INDEX.md` or `README.md`.**
* **It did not widen the refusal beyond arrays.** The class-less
  (`cid == 0, kind == Object`) and interface-stamped receivers Path B was built
  for are untouched. An array is the only receiver for which "not an instance of
  the resolved interface" is a *proof* rather than a strong suspicion, and this
  file is on the hot path for every bytecode executed.
* **It did not change the `AbstractMethodError` message text.** Differential
  vectors compare that string against HotSpot's.
* **It did not chase down every route by which `LinkedHashMap.values()` gives a
  wrong answer.** `size()` and `isEmpty()` on the view answer **0** and **true**
  in *both* modes without going near Path B: `vc_route_source_size` calls
  `native_map_size` on the `Object[]`, `try_delegate_real_collection` correctly
  declines (an array is not a `Map`), and the native returns 0. N1's blast
  radius is therefore wider than the `AbstractMethodError`, and change (a) does
  not narrow it — a silently-empty `size()` stays silently empty until N1 lands.
* **It did not verify N4's glob semantics against the JDK's `sun.nio.fs.Globs`
  translation.** `G13Path.java` measures four glob/regex rows; a real
  registration needs the whole syntax (`**`, `?`, `[…]`, `{a,b}`, escaping) and
  that census has not been taken.
* **It did not run `regression-suite/run.sh`,** nor any state-changing git
  command.

---

## 10. The one-sentence version

Three vectors printed the same sentence because `AbstractMethodError: … has no
Code attribute` is what this interpreter says whenever *anything* upstream hands
it a receiver it cannot dispatch on — and upstream was two different mistakes:
five natives nobody registered on two objects that should never have been
instances of an abstract type, and a field-slot collision that passed an
`Object[]` off as a `Map`.
