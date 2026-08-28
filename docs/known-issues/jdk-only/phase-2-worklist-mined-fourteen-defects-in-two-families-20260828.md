# Working Phase 2 from the report's `outcome` field: 14 defects in the first two families

**Status: 14 FIXED 2026-08-28.** `java.util.Arrays` 10, `java.util.HashMap` 4.
All identical in both modes, so none is a `--jdk-only` defect — they are ordinary
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
