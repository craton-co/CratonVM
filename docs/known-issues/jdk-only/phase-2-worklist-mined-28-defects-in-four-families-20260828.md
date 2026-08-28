# Working Phase 2 from the report's `outcome` field: 28 defects in the first four families

**Status: 27 FIXED, 1 recorded OPEN, 2026-08-28.** `java.util.Arrays` 10,
`java.util.HashMap` 4, `java.lang.Class`/`Module` 9 (8 fixed),
`java.io.ByteArrayOutputStream`/`java.util.Collections` 5. All but one identical
in both modes, so almost none is a `--jdk-only` defect — they are ordinary
correctness bugs that the strict-mode instrument found.

## 1. The families were not chosen by hand

`the-definition-of-done-screen-run-for-the-first-time-20260828.md` established
that the report's `native-shadows-bytecode` rows carry an `outcome`, and that
filtering to `native-won` gives **334 distinct triples** — the shadows that
actually ran, as opposed to the 104 that lost their dispatch and have nothing to
retire.

These two families are the top of that list by tractability, and the probes ask
**exactly the triples the report named**, nothing more:

```text
Arrays   asList, copyOf x3, copyOfRange([BII), equals([B[B), fill x2, hashCode([B)
HashMap  <init> x2, put, get, containsKey, getOrDefault, computeIfAbsent,
         isEmpty, keySet, values, entrySet
```

Both probes ask the CONTRACT EDGES rather than the happy path, on the theory
that a shim's middle is where it is most likely to be right and its refusals,
nulls and bounds are where it was written from memory. **Every one of the 14
defects is on an edge.** Not one is a wrong answer to an ordinary call.

## 2. `java.util.Arrays` — 10

### Two are type-safety holes

```text
Arrays.copyOf(new Object[]{"s", Integer.valueOf(1)}, 2, String[].class)
  HotSpot  ArrayStoreException      CratonVM  no-throw
String[] t = new String[2]; Arrays.fill((Object[]) t, Integer.valueOf(1));
  HotSpot  ArrayStoreException      CratonVM  no-throw, and toString(t) is [1, 1]
```

A `String[]` whose contents are `Integer`s is an object that contradicts its own
type. Every later reader — a cast, a `checkcast` the JIT may elide on the
strength of the array's declared type, any serializer — is entitled to assume it
cannot exist. This is the same hole the roadmap tracked as **P3-A for the
`aastore` opcode** (now closed), reached through a library method instead.

### Four are null handling

`copyOf(null, 1)`, `copyOfRange(null, 0, 1)` and every `fill` variant returned a
**null result or silently did nothing** where the JDK throws NPE. Each body
opened with

```rust
_ => return Ok(Some(Value::Object(None))),   // a null array answers null
```

which is the worst shape a refusal can take: the caller does not learn it passed
null until the null it got back is dereferenced somewhere else, with a stack
trace pointing away from the mistake.

### Three are bounds

`copyOfRange` read both ends with `(*n).max(0)`, so a negative `from` became `0`
and an out-of-range call became indistinguishable from a legal one.
**Clamping an argument is not validating it.** `from > to`,
`from < 0` and `from > length` all silently returned an array; note that `to`
past the end IS legal and pads, which is the row a from-memory implementation
most often gets wrong in the other direction.

### One is a type error that became a big one

```text
Arrays.copyOf(src, -1)
  HotSpot  NegativeArraySizeException     CratonVM  OutOfMemoryError
```

The length was read as `*n as usize`, and `-1 as usize` on a 64-bit host is
18 446 744 073 709 551 615 — a sincere request for eighteen exabytes, which the
allocator answered the only way it could. The wrong TYPE matters more than the
wrong message: code catching `NegativeArraySizeException` around a computed
length recovers, and `OutOfMemoryError` is an `Error` most catch blocks
deliberately decline.

## 3. `java.util.HashMap` — 4

```text
new HashMap<>(-1)        HotSpot IllegalArgumentException  CratonVM no-throw
new HashMap<>(16, -1f)   HotSpot IllegalArgumentException  CratonVM no-throw
new HashMap<>(16, NaN)   HotSpot IllegalArgumentException  CratonVM no-throw
```

The JDK's constructor is three guards before it does anything else. **NaN has to
be spelled out rather than folded into the `<= 0` test**: every comparison
against NaN is false, so `NaN <= 0` does not catch it, and a NaN load factor
produces a NaN threshold and a table that never resizes.

The fourth is the callback boundary:

```text
cme.computeIfAbsent("y", x -> { cme.put("z", 9); return 2; })
  HotSpot  ConcurrentModificationException     CratonVM  no-throw
```

Not merely a missing exception. The native decides the key is absent **before**
calling the mapper and writes its result **after**; if the mapper resized or
rewrote the table in between, that write lands against a decision taken on a
table that no longer exists. The CME is what stops a caller silently corrupting
its own map.

Implemented with the real `modCount` (`read_map_mod_count`, which already
existed) rather than a size comparison: an add paired with a remove leaves the
size unchanged and is still a structural modification. A receiver with no
real-JDK layout slot answers `None` and the check is skipped rather than guessed.

## 4. What PASSED, because it says where the work is not

`HashMap`'s hard parts are right. Every `computeIfAbsent` contract held — a null
result stores nothing, a null function throws even when the key is present, a
throwing mapper leaves the map untouched, a null key computes, and a key mapped
to null is treated as absent so the mapper runs. So did the
`containsKey`-true-while-`get`-returns-null distinction, which a map that
represents "absent" as null cannot make at all.

The gaps are at the **constructor** and the **callback boundary** — the two
places where a native's own code, rather than its data structure, has to
reproduce a contract.

## 4.5 Third family: `java.lang.Class` and `java.lang.Module` — 9

261 probed rows against the 20 `Class` and 5 `Module` triples the report named.

**Six are one bug.** `getPackageName()` answered `""` for every primitive and
for `void`, where the JDK answers `"java.lang"`:

```text
int.class.getPackageName()   HotSpot "java.lang"   CratonVM ""
```

The JDK's body opens `if (isPrimitive()) return "java.lang";`, and the reason is
not arbitrary — a primitive's wrapper and its `Class` mirror both live in
`java.lang`, so anything grouping reflected types by package (a scanner, a doc
generator, an access check keyed on package) puts primitives with the classes
they belong to rather than into an anonymous default-package bucket. The check
goes BEFORE the memo: `PACKAGE_NAME_CACHE` is keyed on `ClassId` and a primitive
mirror has one, so a wrong answer would otherwise be cached for the life of the
VM.

**Two are null handling.** `Class.getResourceAsStream(null)` answered null and
`Module.canRead(null)` answered false, where both throw NPE. `canRead` is the
worse shape: answering `false` makes a caller testing `if (!m.canRead(other))`
treat an accidentally-null argument as a legitimate "no" and take the failure
branch for the wrong reason. And for `getResourceAsStream`, null and "absent"
were otherwise the SAME answer, so a caller that builds a resource path could
not tell a missing file from a bug in its own path construction.

**One is recorded OPEN, deliberately.**

```text
java.base.canUse(Runnable.class)   HotSpot false   CratonVM true
```

`canUse` is true only when the module DECLARES `uses` for that service. A
faithful implementation has to read the descriptor's `uses` set — and this
native exists *precisely because* a named `Module` mirror can carry a **null
`descriptor` field**, which is the one thing it would have to touch. Consulting
it means calling back into Java (`getDescriptor().uses().contains(..)`) from the
native written to avoid that field, i.e. re-entrancy on the path
`ServiceLoader.checkCaller` takes during `Console.<clinit>`, for one row. The
registrar now carries the measurement and the cost: application code on a NAMED
module may load a service its descriptor never declared; code on the unnamed
module — most code on this VM — is unaffected, because an unnamed module
genuinely can use anything.

**What passed is the bulk of the reflection surface**: every `getName` /
`getSimpleName` / `descriptorString` special case across primitives, arrays,
nested, enum and anonymous classes; all of `forName` (primitives correctly NOT
findable by name, `[I` and `[[I` findable, the slash form rejected, a null
loader scoping to bootstrap); `cast` including its null and primitive rules; and
the whole public-vs-declared split for fields, methods and constructors.

## 4.55 Fourth family: `ByteArrayOutputStream` and `Collections` — 5

62 rows against the 6 BAOS and 4 `Collections` triples the report named.

**One is not a missing exception at all.**

```text
Collections.sort(List.of("b", "a"))
  HotSpot     UnsupportedOperationException
  --jdk-only  same
  compatible  no-throw — and the list came back [a, b]
```

`al_state` reads the backing array of an immutable list just as happily as an
`ArrayList`'s, so the native **sorted an immutable list in place**. `List.of` is
shared and passed around precisely because it cannot change; every holder of
that list would have observed its contents reorder underneath them. Strict mode
was already correct, because it runs the real bytecode and refuses — the fourth
place in this survey where the DEFAULT is wrong and `--jdk-only` right.

**Four are nulls**: `BAOS.write(null, 0, 1)`, `BAOS.write(null, 0, 0)`,
`Collections.sort(null)`, and sorting a list containing a null element.

The zero-length write is the one worth stating. `Objects.checkFromIndexSize`
runs AFTER `b.length` has been read, so the JDK never reaches a "nothing to
copy" short-circuit; a caller passing a null buffer with a computed length of 0
— the ordinary shape of an empty write — learned nothing and carried the null
on.

**Every BOUNDS row already passed**, including `off + len` overflowing to a
negative int, which is the case a check written as `off + len > b.length` gets
wrong while looking right. `check_array_bounds` is correct; it simply never ran,
because the null buffer returned before it.

### The guard I wrote first could never have fired

Worth recording, because it is the second inert fix of the day and a different
cause from the first.

I screened on `java/util/ImmutableCollections`, because that is what the probe
prints for `List.of(..).getClass().getName()`. **That name is faked.** This VM
funnels every unmodifiable view through seven `cratonvm/internal/Unmodifiable*`
synthetic classes and has `getclass_immutable_marker` report the JDK name to
callers. A guard written against the name the probe shows can never match the
receiver's real class.

Both inert fixes today had the same signature — source reads correctly, build
clean, behaviour unchanged — and different causes:

| | cause | what finds it |
| --- | --- | --- |
| `Arrays.fill` | edited a registrar that does not own the slot | `--dump-native-registry`, `owns_slot`, one line |
| `Collections.sort` | matched an identity the VM deliberately misreports | only re-running the probe |

The second cannot be caught by reading, because the VM is lying to the reader on
purpose. **Re-measure; never re-read.**

The corrected guard is also strictly better than the one I intended: screening
the real receiver class catches `Collections.unmodifiableList(..)` too, which
must refuse `sort` for the same reason and which the `ImmutableCollections`
check would have missed.

## 4.6 The pattern, now strong enough to state

Four families, 510 probed rows, 28 defects — **and every one of them is on a
contract edge.** Not one is a wrong answer to an ordinary call, in any of the
four.

That is worth stating as a finding rather than an impression, because it has two
consequences. It predicts where the remaining ~295 triples will yield. And it
means **a probe that exercises only the happy path will report a family clean
when it is not** — which is how a shim's middle stays correct while its
perimeter rots unnoticed.

## 5. The process error, because it cost a build

`Arrays.fill` is registered in BOTH `native-builtins/src/phases_early.rs` and
`native-collections/src/lib.rs`. I found the first by grep, fixed it, rebuilt for
33 minutes, and the probe answered exactly as before. The registry dump said why
in one line:

```text
fill ([II)V  kind=bridge inv=1 owns=True by=native-collections/src/lib.rs:20914
```

`owns_slot` is the field that decides, and nothing in the losing file marks it as
dead. **Dump the registry and read `owns_slot` and `invocations` for the triple
BEFORE editing** — one run, and it saves a full build.

The same dump showed something worse: the WINNING `copyOf` already carried
null-source and negative-length guards, with comments arguing the same case I
was about to write, while its losing twin still had the broken body. A duplicate
pair can sit half-fixed indefinitely — the live half correct, the dead half
rotting — and nothing fails until registration order changes.

## Reproduce

```bash
cratonvm --java-home "$JDK" --jdk-only -cp probes/out ArraysHashSetShadowSweep
```

Both probes are 0 differing lines in both modes after the fixes: 105/105 and
82/82.
