# G77-1 — two clean sweeps, and what a flattened curve means

**Status:** MEASURED, **no defects found**, nothing fixed. That is the result.
**Provenance:** both VMs. Oracle HotSpot 25.0.3+9-LTS; CratonVM
`C:/craton/target-nolto`, `--jdk-only`. Probes
`regression-suite/probes/Sweep14StringBounds.java` (44 rows) and
`Sweep15Serialization.java` (20 rows).

---

## 0. Why record a sweep that found nothing

Because the alternative is that somebody probes these surfaces again. Six
sweeps on the exception-contract axis returned defects every time
(`G68-1` 4, `G69-1` 82 rows of message defects, `G72-1` 5, `G73-1` 5,
`G76-1` 6). These two returned none, and the surfaces are not obscure — they
are two of the highest-traffic areas in any Java application.

**64 rows, 0 divergences.**

## 1. Sweep 14 — String bounds, case mapping, comparison (44 rows, all exact)

Every bounds message and every case-mapping edge, including the ones that are
routinely got wrong:

* `charAt`/`substring`/`codePointAt`/`getChars` past the end and negative, and
  `substring(3, 1)` where end precedes begin — messages exact in all cases;
* `String.valueOf(char[], off, len)`, `copyValueOf`, `new String(char[], …)`
  and `new String(byte[], …)` with bad ranges;
* the whole `StringBuilder` bounds family — `charAt`, `setCharAt`,
  `deleteCharAt`, `insert`, `substring`, `setLength(-1)`;
* **case mappings that change LENGTH**: `ß` uppercasing to `SS`, the `ﬁ`
  ligature, final sigma in `ΣΣ`, the `ǳ` digraph;
* **locale-sensitive case**: Turkish `i`/`I` against `Locale.ROOT`, which is
  the classic divergence and is exact here;
* a surrogate PAIR through `toUpperCase`, and a LONE surrogate through both
  cases — the hazard this session has chased through nine other records —
  all exact;
* `equalsIgnoreCase`, `compareToIgnoreCase`, `regionMatches` with an
  out-of-range length, `contentEquals`, and `intern()` identity.

## 2. Sweep 15 — serialization (20 rows, all exact)

* round trips for a plain object, `String`, boxed `Integer`, `ArrayList`,
  `HashMap`, `int[]`, an `enum`, and `null`;
* `transient` genuinely skipped;
* **back-reference identity**: the same object written twice deserialises to
  ONE object, which is the contract most naive implementations miss;
* refusals — `NotSerializableException` naming the class, a non-serializable
  FIELD, garbage bytes, a truncated stream, an empty stream, constructing an
  `ObjectInputStream` on an empty stream, reading past the last object, and
  writing after close;
* the stream magic (`aced`) and version (`0005`).

## 3. What the flattened curve does and does not mean

It does NOT mean the VM is finished. It means **these two axes are done**, and
that the cheap, broad, message-shaped defect hunt has reached diminishing
returns. The evidence for that reading rather than "I got unlucky twice":

* the two axes are unrelated to each other and to the six that preceded them;
* both were probed the same way, by the same method, at the same depth;
* the six preceding sweeps found a defect in the first ten rows every time.

The work that remains on this goal is therefore not more sweeping of the same
kind. It is the two items that were measured, costed and deliberately left:
`G75-1` N1 (the URI parse and its getters, a whole pass) and `G70-1` N1 (the
`read_string` caller audit, which needs per-site judgement about whether a
value is inspected or handed back to Java — a distinction no grep answers).

## 4. NOMINATIONS

**N1 — do not re-probe §1 or §2.** The probes are checked in with their oracle
columns; re-run them after any change to the string or serialization paths,
but do not treat these surfaces as unexplored.

**N2 — if sweeping continues, change the SHAPE of the question, not the
subject.** Every sweep so far has asked "what does this call answer or throw".
Three shapes have never been asked: what does it do CONCURRENTLY (two threads
racing the same native), what does it do under MEMORY PRESSURE (a moving GC
mid-native, which several records reach for by hand with pins), and what does
it do at SCALE (inputs past the sizes the fast paths assume). Each is a
different kind of question, and this session's evidence is that a new shape of
question outperforms a new subject.
