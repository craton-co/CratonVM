# G68-1 — the contracts that are not values, and two formats I guessed wrong

**Status:** MEASURED throughout; four fixes included, one residue named.

> **RESIDUE CLOSED 2026-08-18 at `5c7a8ecb8`.** Section 5's
> `int.class.newInstance()` row is fixed, so section 2's table is **seven of
> seven** and the "six of seven" headline below is superseded. `mirror_class_id`
> answers `None` for a PRIMITIVE mirror as well as for a non-mirror, and the two
> had been collapsed onto one message: `int.class` IS a `Class`, it just has no
> class id. N2 stands only as the wider audit.
>
> **N1 is also done**, in
> `G69-1-the-message-was-the-only-thing-wrong-20260818.md`. Its four rows were
> right to be called "a start": the table is 82 rows, the exception types and
> the precedence were already exact on every one, and the messages were wrong on
> every one.
**Provenance:** every row on both VMs. Oracle HotSpot 25.0.3+9-LTS; CratonVM
`C:/craton/target-rel11` (before) and `target-rel12` (after), `--jdk-only`.
Probes: `scratchpad/g70/{Sweep7,P8,P9,PA,PB,PC}.java`, ASCII and deterministic.

---

## 0. Why this axis

Five sweeps had compared **values**. The defects that turned out to matter
most were not values: `Scanner` returned nothing (`G64-1`), fail-fast iterators
threw nothing (`G67-1`). So this sweep asked only about contracts that are an
**exception or a side effect** — what throws, with which type, carrying which
message, and what got written to disk.

29 rows, four defects, and two of my own wrong guesses caught by the oracle.

## 1. `File.createTempFile` never validated its prefix

```text
                            HotSpot                                   CratonVM
createTempFile("m", ".t")   IllegalArgumentException                  CREATED THE FILE
                            Prefix string "m" too short: length must be at least 3
createTempFile("mm", ".t")  the same, naming "mm"                     CREATED THE FILE
createTempFile("mmm", ".t") OK                                        OK
createTempFile(null, ".t")  NullPointerException                      CREATED THE FILE
```

**A validation gap, not a message gap.** The refusal is documented on the
method, and a program that relies on it — anything treating "prefix too short"
as a programming error — silently got a file instead. Both `File` overloads
defaulted a null prefix to `"tmp"` and never looked at the length.

A null SUFFIX is legal and means `.tmp`; that is untouched.

## 2. `Class.newInstance` threw the wrong TYPE

```text
                          HotSpot                              CratonVM (before)
abstract class            InstantiationException | null        UnsupportedOperationException
interface                 InstantiationException | java.lang.Runnable   UnsupportedOperationException
no no-arg constructor     InstantiationException | PC$NoNoArg  UnsupportedOperationException
int[]                     InstantiationException | [I          UnsupportedOperationException
private constructor       SUCCEEDS                             SUCCEEDS
```

The code threw `UnsupportedOperationException` **with the words
`"InstantiationException: …"` inside the message** — a stringly-typed
exception. `catch (InstantiationException)` never matched it, which is the
entire purpose of the type.

The comment three lines above said *"matches `Class.newInstance` JDK semantics
(InstantiationException for these)"*, and the file's other note says Spring's
`beanDefinitionWithAbstractClass` depends on the catch matching. **The contract
was written down correctly and the code did not implement it** — the same
species as `G66-1`'s guard in a replaced body and `G64-1`'s duck test.

## 3. Two formats I composed and got wrong

This is the part worth carrying, because both were caught in minutes by
probing instead of trusting.

### 3a. `NoSuchMethodException`

Before: `nope`. HotSpot: `P8$H.nope()`. That message is what a framework
prints when reflection misses, and a bare name says neither which class was
searched nor with what signature.

I wrote the format from memory and got **two things wrong at once**:

```text
guessed  P9$H.zz(int, java.lang.String)     separator ", "
measured P9$H.zz(int,java.lang.String)      separator ","
guessed  P9$H.zz(int[])                     getTypeName()
measured P9$H.zz([I)                        getName()
```

So an array parameter appears as `[I` and `[[Ljava.lang.String;` — reaching
for `array_descriptor_to_type_name`, which exists and renders `int[]`, is
exactly wrong here. The JDK's `Class.methodToString` joins `getName()` with a
bare comma. A five-row probe settled it.

### 3b. `InstantiationException`'s message is not derivable

```text
abstract class          null
interface               java.lang.Runnable
no no-arg constructor   PC$NoNoArg
int[]                   [I
int                     int
```

`getName()` everywhere **except an abstract non-interface class, where it is
null** — and `Constructor.newInstance` and `Class.newInstance` disagree with
each other, which is why passing the class name was right for one form and
wrong for the other. Nobody derives that; it is transcribed or it is wrong.

## 4. What is guarded

Ten rows in `RJdkIntrinsics3`'s `misc` family (21 → 31), all verified PASS on
HotSpot **before** the fixes were built — including the array-parameter
rendering and the null-versus-named message split, which are the two rows that
would have passed a plausible wrong fix.

## 5. Residue, measured and not fixed

`int.class.newInstance()` answers `UnsupportedOperationException:
Class.newInstance: receiver is not a Class mirror` where HotSpot gives
`InstantiationException | int`. A primitive `Class` mirror carries no class id
in this VM, so the guard cannot reach the branch that now throws correctly.
Narrow — but it is the one row of seven in §2 that is still wrong, and saying
"six of seven" is the honest headline.

The `IllegalAccessException` family for final-field writes is also unfixed and
deliberately uncomposed. Measured:

```text
setInt on static final int    Can not set static final int field PA$H.I to (int)9
set    on static final int    Can not set static final int field PA$H.I to java.lang.Integer
setLong on static final long  Can not set static final long field PA$H.L to (long)9
set    on static final String Can not set static final java.lang.String field PA$H.S to java.lang.String
INSTANCE final                does not throw at all
```

The primitive setter renders `(type)value`; the object setter renders the
argument's CLASS; and instance finals are writable after `setAccessible(true)`.
Given §3, composing this from four rows would be a fifth guess. It needs a
wider probe first.

## 6. NOMINATIONS

**N1 — the `IllegalAccessException` family, §5.** Probe every setter kind
(`set`, `setInt`, `setLong`, `setBoolean`, …) against every field type, then
transcribe. The rows above are a start, not the table.

**N2 — primitive `Class` mirrors have no class id.** §5's residue is one
symptom; anything else that resolves a mirror to a class will have the same
hole for `int.class` and friends. Nobody has looked for the others.

**N3 — the exception-contract axis is not exhausted.** This sweep covered
arrays, reflection, IO and exception plumbing. Untouched with the same method:
`Thread` interrupt/join semantics, lock and condition contracts, class-loading
and resource-resolution failures, and charset decode/encode error actions.
Five sweeps of values found six defects; the first sweep of *contracts* found
four.
