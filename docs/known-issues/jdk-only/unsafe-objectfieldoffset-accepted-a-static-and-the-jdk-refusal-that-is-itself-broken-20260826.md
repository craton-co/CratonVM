# `Unsafe.objectFieldOffset` accepted a STATIC field — and the JDK's own refusal for a non-array class is itself broken

**Status: the `objectFieldOffset` defect is FIXED 2026-08-26.** The second
finding is **recorded, deliberately NOT matched**, §3.

## 1. The families

`sun/misc/Unsafe` (82 rows) and `java/nio/file/Files` (40) are the last two
large families on the bridge-kind retirement surface.
`probes/UnsafeFilesSweep.java` diffs both against HotSpot.

**Unsafe is diffed on BEHAVIOUR, not on offsets.** A field offset is an
implementation token: two VMs may legally disagree on its value and both be
correct. What they must agree on is that a put at an offset is visible to the
matching get, that a CAS with the wrong witness fails, and that the base/scale
relationship over an array is self-consistent. Printing a raw offset would have
manufactured differences out of a legal freedom — the same error as diffing an
`fd` id.

Coverage, taken in the probe's own run:

| class | rows | ran | invocations |
| --- | ---: | ---: | ---: |
| `sun/misc/Unsafe` | 89 | 27 | 43 |
| `jdk/internal/misc/Unsafe` | 179 | 16 | 43 |
| `java/nio/file/Files` | 40 | 22 | 45 |

`Files` was **clean** across 30 checks: read/write/append, copy with and without
`REPLACE_EXISTING`, move, `createDirectories`, `newDirectoryStream`, `walk`,
`readAttributes`, and six refusal shapes (`delete` absent, `size` absent,
`copy` onto existing, `createDirectory` existing, `delete` a non-empty dir).

## 2. The defect — a static field got an instance offset

```text
u.objectFieldOffset(Integer.class.getDeclaredField("MAX_VALUE"))
  HotSpot 25.0.3+9   IllegalArgumentException
  CratonVM           returned an INSTANCE slot index, both modes
```

A static field has no object offset. The JDK refuses and provides
`staticFieldOffset` as the separate accessor — which CratonVM registers three
lines above the one that was wrong.

**The flag was already there and discarded.** The body destructured
`let (_is_static, _cid, slot, _desc) = read_field_meta(...)` — it read the
static bit and threw it away. The fix is the check that destructuring always
anticipated.

**Why this direction is the dangerous one:** the caller's next move after
`objectFieldOffset` is a `getInt`/`putInt` at that offset against some receiver.
An instance offset invented for a static means reading — or **writing** — an
unrelated instance field of whatever object is passed. A refusal is loud; this
was silent.

## 3. The second finding: HotSpot's refusal is broken, so it is NOT matched

```text
u.arrayIndexScale(String.class)     HotSpot: NoClassDefFoundError: java/lang/InvalidClassException
u.arrayBaseOffset(String.class)     HotSpot: NoClassDefFoundError: java/lang/InvalidClassException
                                    CratonVM: 1  and  16
```

Read that exception name carefully. `InvalidClassException` lives in
**`java.io`**, not `java.lang`. HotSpot's deprecation shim is trying to refuse a
non-array class and naming the exception in the wrong package, so the throw
itself fails to link and the caller gets a `NoClassDefFoundError` instead. **The
intent is clearly a refusal; the mechanism is a JDK bug.**

CratonVM is also wrong — answering `1` and `16` gives a caller a
plausible-looking basis for an address computation over a class that has no
elements — but "match HotSpot" is the wrong instruction here, because matching
would mean reproducing a broken throw. That makes the correct behaviour a
judgement call rather than a measurement, and this record flags it instead of
taking it silently:

* returning **0** for a non-array is what `arrayIndexScale`'s long-standing
  javadoc specifies and what callers guard on (`if (scale == 0) throw`);
* throwing the real `java.io.InvalidClassException` is what HotSpot evidently
  meant;
* neither is what HotSpot *does*.

Not fixed here. Flagged because a wrong scale is an address-arithmetic hazard,
and because "the oracle says X" is not a licence when X is the oracle
malfunctioning.

## 4. Method note

Two of this probe's six initial "differences" were the harness, not the VM: the
HotSpot run captured stderr (`2>&1`) while the CratonVM runs discarded it
(`2>/dev/null`), so four `sun.misc.Unsafe` deprecation WARNINGs showed as
diffs. **A cross-VM stdout diff has to capture the same streams on both sides.**
That is the second harness artefact in this survey after the stdout-encoding one
in `FilePathSweep`; both produced confident-looking differences with no VM
behaviour behind them.

A third came from probe design: the first draft asserted
`arrayIndexScale(String.class) == 0` and threw on failure, so the diff showed
*my* exception on one VM and the callee's on the other, hiding what either
actually returned. Printing the VALUE is what exposed the `1`/`16` and the
`java/lang/InvalidClassException`.

## Reproduce

```bash
cratonvm --java-home "$JDK" --jdk-only -cp probes/out UnsafeFilesSweep
```
