# WORKER-4-2 — the `java.io` rows adjudicated: 194 shadows, one file that is not in the build, and a file named `deprecated_io_util` that owns no `java.io` row

**Status: MEASURED, with three retirements landed and two refusals recorded.**
Lane WORKER 4, 2026-08-22. Companion to `WORKER-4-1`, which closed the
fabricated-receiver half of this brief.

**Provenance.** Linux (Azure host 2), Temurin **25.0.4+7**, worktree
`/data/cvm-w4io-20260821`. Every count below is unioned over **105 per-vector
`--dump-native-registry` runs** — one strict-mode boot per scheduled vector,
using the classes `run.sh` had already compiled — not over one boot and not from
a source grep. Probes: `regression-suite/probes/W4Deprecated.java` (116 cases),
`W4BaseStream.java` (26 cases), both diffed against the oracle in both modes.

---

## 1. Three instruments, three different numbers, and they must not be mixed

The brief asks for *"`java.io` streams: 99 unclaimed rows (`H14-2`)"*. There are
at least three defensible ways to count that, and they do not agree:

| instrument | what it counts | `java/io/*` |
|---|---|---:|
| `--jdk-only-report`, unioned (`H14-1`/`H14-2`, and `run.sh`'s own summary line) | DISPATCH observations: a native was reached where the real method has `Code` | 99 (`H14-2`) |
| `--dump-native-registry`, unioned — **this record** | REGISTRATIONS the native owns, where the real method is declared, has `Code`, and is not `ACC_NATIVE` | **194** |
| of those, with **zero invocations** across all 105 vectors | the registrations no vector reaches | **99** |

**The two 99s are not known to be the same 99.** They are produced by different
instruments from different sides — one is "reached at least once", the other is
"reached never" — and nothing here intersects them. Quoting either without
naming the instrument is how `[MODE≠feat]` happens.

The registration-level table is the one an adjudication needs, because a
retirement acts on a REGISTRATION. It carries, per row: total invocations, how
many OTHER files register the same triple (the `H11-2` §4 promote-the-loser
hazard, counted), and whether the real method has bytecode.

### 1.1 The 194, by class

| class | rows | | class | rows |
|---|---:|---|---|---:|
| `java/io/File` | 54 | | `java/io/FilterOutputStream` | 5 |
| `java/io/PrintStream` | 30 | | `java/io/FileOutputStream` | 4 |
| `java/io/DataInputStream` | 15 | | `java/io/IOException` | 4 |
| `java/io/DataOutputStream` | 14 | | `java/io/OutputStream` | 4 |
| `java/io/UnixFileSystem` | 12 | | `java/io/BufferedOutputStream` | 3 |
| `java/io/ByteArrayOutputStream` | 11 | | `java/io/EOFException` | 2 |
| `java/io/FileDescriptor$1` | 10 | | `java/io/FileNotFoundException` | 2 |
| `java/io/ByteArrayInputStream` | 8 | | `java/io/FilterInputStream` | 2 |
| `java/io/InputStream` | 8 | | `java/io/UnsupportedEncodingException` | 2 |
| `java/io/PrintWriter` | 7 | | `java/io/BufferedInputStream` | 1 |
| `java/io/BufferedWriter` | 6 | | `java/io/FileDescriptor` | 1 |
| | | | `java/io/ObjectInputStream` | 1 |

By registrar file: `phases_late/nio_file.rs` 70, `native-io/src/lib.rs` 67,
`logging_shims.rs` 36, `native-builtins/src/lib.rs` 12, `lang_misc.rs` 10,
`shared_secrets_bridge.rs` 10, `reflect_annotations.rs` 1.

**Note what is not in that list: neither of the two files the brief names.**

---

## 2. `phases_late/io_streams.rs` is NOT IN THE SHIPPING BUILD

The brief lists it as a third work item. It registers `PushbackInputStream`,
`PushbackReader` and the `Object{Input,Output}Stream` stubs — and in the default
build it registers **nothing**, because nothing calls its registrars.

The chain, read from the source and confirmed by measurement:

```text
io_streams::register_p58_pushback
  <- phases_late.rs::register_phase58_natives
      <- native-builtins/src/lib.rs::register_synthetic_overrides
          <- vm/src/native/builtins.rs, #[cfg(feature = "synthetic-jdk")]
```

MEASURED: across all 105 per-vector dumps there is **not one
`java/io/PushbackInputStream` or `java/io/PushbackReader` row**, in either
mode. The only `java/io/ObjectInputStream` rows present come from
`reflect_annotations.rs` and `shared_secrets_bridge.rs`.

**Verdict: NOT A `java.io` DEFECT, and it cannot become one.** Under
`--features synthetic-jdk` there is no class library, so there is no real
bytecode for these rows to shadow and contract §1.4 does not apply to them at
all. Anyone auditing this file against a default-build census will find zero
rows and conclude it is clean; anyone auditing it against a synthetic-jdk build
will find rows that §1.4 cannot judge. `[2cfgs]` — and this is the case where
checking both configurations changes the verdict from "clean" to "out of
scope", which is a different statement.

---

## 3. `deprecated_io_util.rs` registers 38 triples, WINS 6, and none of the six is `java.io`

The brief calls it *"25 sentinel sites"*. MEASURED, unioned over 105 vectors:

| | |
|---|---:|
| triples the file registers | 38 |
| triples it OWNS the slot for | **6** |
| of those, §1.4 shadows (real method declared, has `Code`) | **5** |
| of those five, in `java.io` | **0** |

```text
SHADOW  inv=7  java/lang/Class.newInstance()Ljava/lang/Object;
SHADOW  inv=0  java/lang/Number.byteValue()B
SHADOW  inv=0  java/lang/Number.shortValue()S
SHADOW  inv=0  java/util/Date.toGMTString()Ljava/lang/String;
SHADOW  inv=0  java/util/Date.toLocaleString()Ljava/lang/String;
absent  inv=0  java/io/LineNumberInputStream.mark(I)V     (no such method in the image)
```

The other 32 registrations are `owns_slot=False` — some other file registered
the same triple later and took the slot. **A file's registration count is not
its surface**, and the ratio here is 6/38.

`dupX = 0` for all five: one registration each, this file's. So a deletion
removes a row and promotes nobody — unlike `pipe.rs`'s six abstract rows, where
`net_channels.rs` waits underneath with an incompatible body (`H11-2` §4).

### 3.1 RETIRED — `java.util.Date.toLocaleString()`

**The native answered the wrong string and the JDK's own body answers the right
one on this VM.** MEASURED, `W4Deprecated`, `new Date(946684800000L)`:

```text
HotSpot 25.0.4+7                                  Jan 1, 2000, 12:00:00 AM
CratonVM, this native                             01/01/2000 00:00:00       <- WRONG
CratonVM, DateFormat.getDateTimeInstance(…)       Jan 1, 2000, 12:00:00 AM  <- the JDK body
```

`Date.toLocaleString()`'s body **is** that third expression. So the third row is
not an approximation of the JDK path; it is the JDK path, run on this VM,
agreeing with the oracle exactly. Retiring is a fix, not a trade.

**And a unit test was holding it in place.** `test_date_to_locale_string`
asserted `"01/01/1970 00:00:00"` — a string no JDK returns. That is
`[a test that freezes VM output locks in the divergence]`, and it is why this
row survived a green suite for as long as it did. Removed with the row.

### 3.2 RETIRED — `java.lang.Number.byteValue()` / `shortValue()`

Two hand-written copies of two one-line `java.base` bodies:

```java
public byte  byteValue()  { return (byte)  intValue(); }
public short shortValue() { return (short) intValue(); }
```

The natives were a faithful transcription — `invoke_virtual("intValue")`, then
the narrowing cast. Nothing crosses a VM boundary, so §1.5 cannot call them
bridges.

MEASURED before retiring, `W4Deprecated`, **zero diffs against the oracle in
both modes** over: 15 truncating and sign-flipping inputs (`127/128/255/256`,
`-1/-128/-129`, `32767/32768/65535/65536`, `-32768/-32769`) x a USER `Number`
subclass declaring only the four abstract primitives — the shape the superclass
walk serves and the only shape a `java.lang.Number` row exists for — x
`Integer`/`Long`, plus `Double`/`Float` rounding, plus `BigInteger`/`BigDecimal`
(the two the retired comment was specifically about), plus a call through a
`Number`-typed reference.

**The `BigDecimal` history is the argument for retiring, not against it.** The
old comment records a real regression: the native used to READ FIELD 0, which
is 0 for `BigDecimal`, and *"broke JSON-B/Yasson's untyped numeric binding"*.
The fix was to make the native call `intValue()` virtually. The JDK's own body
has always called `intValue()` virtually. Standing in front of a body that
never had the defect is what created the opportunity for it.

Their two unit tests went with them: both scripted the mock's `invoke_virtual`
to return the value they then asserted the narrowing of, so they tested a Rust
`as` cast and could not fail while the registration existed.

### 3.3 REFUSED — `java.util.Date.toGMTString()`

**Correct, and kept.** MEASURED over six instants including two before the epoch
(`0, 1, 946684800000, 1234567890123, -1, -86400000`): identical to the oracle in
both modes.

Retiring it would hand the call to `Date.toGMTString()`'s real body, which
builds its string from `sun.util.calendar.BaseCalendar` internals rather than
from a public formatter — a much larger surface than `toLocaleString()`'s
one-line delegation, and one this lane has not measured. The probe's
`date.toGMTString.jdkPath` row shows an EQUIVALENT `SimpleDateFormat` expression
works on this VM; that is not the same as showing `Date.toGMTString()`'s own
bytecode does. `[a refusal with evidence beats a retirement without it]`.

### 3.4 REFUSED — `java.lang.Class.newInstance()`

**The only one of the six with traffic: `invocations = 7` across the corpus.**
MEASURED correct on nine cases — a plain class, an abstract class, an interface,
a private constructor, a class with no no-arg constructor, a constructor that
throws unchecked, one that throws CHECKED (the unwrapped propagation that is the
whole reason the method is deprecated), a primitive `Class`, and an array
`Class` — all identical to the oracle in both modes.

Kept because the JDK's own `Class.newInstance()` body runs a caller-sensitive
access check (`Reflection.getCallerClass`) and a constructor cache, and this
lane has not measured either on this VM. A row with live traffic, measured
correct, whose replacement is unmeasured, is not a free retirement.

---

## 4. `native-io`'s own 25 zero-traffic `java.io` shadows, and why they are NOT free

Within this lane's own crate the census finds 25 §1.4 shadows with zero
invocations across all 105 vectors:

```text
java/io/InputStream            7   (read([B), read([BII), available, skip,
                                    readAllBytes, readNBytes x2)
java/io/DataInputStream        5
java/io/DataOutputStream       4
java/io/BufferedOutputStream   3
java/io/OutputStream           2
java/io/ByteArrayInputStream   2
java/io/ByteArrayOutputStream  1
java/io/FilterOutputStream     1
```

**They are not retireable on the zero alone, and `WORKER-4-1` §8.2 is why.**
`W4BaseStream.java` drives exactly the population these base-class rows exist
for — a user subclass declaring only the abstract primitive — and gets 26/26
agreement with the oracle, with `java/io/InputStream.read([BII)I` taking ONE
invocation as the positive control. So the rows are correct AND almost entirely
unreached for that shape.

What keeps them is a different population, named in `native_bais_read_bytes`'s
own comment: *"synthetic streams (`URL.openStream`, `getResourceAsStream`) that
materialise as bare `InputStream`-typed receivers but actually have the
`ByteArrayInputStream` layout in slots 0..3."*

**That is a fabricated receiver, and it is the thirteenth instance of the
species `WORKER-4-1` closes twelve of.** Fix the mint — make those two factories
produce a real `java.io.ByteArrayInputStream` — and 25 §1.4 shadows become
retireable with no behaviour change at all. That is the shape of the next lane
in this file, and it is N1.

---

## 5. What this record does NOT claim

* **It does not adjudicate the other 169 rows.** `java/io/File` (54),
  `PrintStream` (30) and `PrintWriter` (7) are the three biggest groups and
  none is in this lane's ownership (`phases_late/nio_file.rs`,
  `logging_shims.rs`). The table is per-row and reproducible; the verdicts are
  not written.
* **It does not intersect its 99 with `H14-2`'s 99.** §1.
* **The synthetic-jdk configuration was not built.** §2's verdict for
  `io_streams.rs` rests on a source read of the `#[cfg]` chain plus the measured
  absence of every one of its triples from 105 default-build dumps. Both halves
  are stated because the second alone would be the `[MODE≠feat]` mistake.
* **`toGMTString` and `Class.newInstance` are refusals, not verdicts of
  correctness-forever.** §3.3, §3.4 say what would settle each.

---

## 6. NOMINATIONS

**N1 — `URL.openStream()` / `getResourceAsStream()` mint a bare
`InputStream`-typed receiver, and 25 §1.4 shadows exist to serve it.** §4. This
is the same defect `WORKER-4-1` closed twelve instances of, and it is the one
with the largest retirement behind it.

**N2 — the adjudication table should be a script.** The `invcensus` +
`ioadjudicate` pair used here is ~80 lines: run every scheduled vector with its
own `--dump-native-registry`, then union `invocations` per triple and join
against `real_declaring_method`. It answers "which registrations does this file
actually own, and which of them does anything reach" for ANY prefix, which is
the first question every adjudication lane has had to re-derive by hand.
`scripts/nio-concrete-receiver-audit.py` landed from this lane; this one did
not, only because its output is a table rather than a pass/fail.

**N3 — `java/io/UnixFileSystem` has 12 shadow rows and is a class no Windows
image declares.** `phases_late/nio_file.rs`. A platform-specific class in a
cross-platform registrar is the shape `[false everywhere=absence]` warns about:
on Windows those 12 rows are invisible to every census, and on Linux they are
12 natives in front of real bytecode.

**N4 — `java/io/FileDescriptor$1` has 10 shadow rows** from
`shared_secrets_bridge.rs`. An anonymous inner class is an odd thing to
register natives on; whether the JDK's `JavaIOFileDescriptorAccess`
implementation could run instead is unmeasured.

---

## 7. Index rows (for H0 to move into `INDEX.md`)

* `WORKER-4-2` — the `java.io` rows counted three ways (194 registrations / 99
  unreached / `H14-2`'s 99 by a different instrument, not known to be the same
  set); `phases_late/io_streams.rs` is `synthetic-jdk`-only and registers
  nothing in the shipping build; `deprecated_io_util.rs` owns 6 of the 38
  triples it registers and none of them is `java.io`; three retirements with a
  116-case probe behind them and two refusals with the same.
