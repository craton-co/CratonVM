# `MemorySegment.set` had no implementation for five of its nine carriers

**Status: FIXED / RETIRED 2026-08-10**, hours after it was filed. The page named
one carrier because that is the one a probe happened to reach; measuring the
other eight found **five** broken, plus two wrong conversions on the `get` side
that no failure had ever surfaced.

## What it was

`java.lang.foreign.MemorySegment` declares nine `get`/`set` pairs and all
eighteen are `public abstract`. A descriptor nobody registers is therefore not a
slow path — it is `AbstractMethodError: … has no Code attribute`, thrown at the
interface method itself.

CratonVM registers them in two files that disagreed about which:

* `native-builtins/src/panama.rs` enumerated all nine covariant `get`
  descriptors and, for `set`, only the ERASED
  `(Ljava/lang/foreign/ValueLayout;JLjava/lang/Object;)V` — which real bytecode
  never emits;
* `native-builtins/src/phases_late/foreign_ffm.rs` covered Byte, Short, Int and
  Long for both.

Four of the nine `set` carriers therefore worked and five did not. Measured with
`probes/FfmSegmentAccessProbe` on the pre-fix binary, identical in `--real-jdk`
and `--jdk-only`:

| carrier | `get` | `set` (pre-fix) |
|---|---|---|
| byte, short, int, long | ok | ok |
| **boolean, char, float, double, address** | ok | **AbstractMethodError** |

**The implementation was never the problem.** `setAtIndex(JAVA_DOUBLE, 2, 2.25)`
worked and round-tripped on the pre-fix binary, because *it* is registered on a
descriptor real bytecode does emit and lands in the same `pe_segment_set_impl`.
That one line is what turned a plausible "FFM cannot write doubles" into "one
registration list is short", and it cost nothing to check.

## What the probe found that nothing had reported

Two `get` conversions, both wrong against Temurin 25.0.3 and neither reachable
until the `set` half worked — so they had to be measured through a carrier that
already worked, writing with `JAVA_SHORT`/`JAVA_LONG` and reading back with
`JAVA_CHAR`/`ADDRESS`:

| line | HotSpot | pre-fix |
|---|---|---|
| `get(JAVA_CHAR)` of `0xFFFE` | `65534` | **`-2`** — sign-extended, and a `char` is unsigned |
| `get(ADDRESS)` | a segment at `0x1234` | **`null`** — `Value::Long` returned where the descriptor says `Ljava/lang/foreign/MemorySegment;`, and the `L` coercion turns a primitive into null |

The address one is the sharper of the two: the accessor answered `null` and the
caller died on `.address()`, which reads as a caller bug rather than a VM one.
`set(ADDRESS, off, seg)` was the same defect in the other direction — the
`MemorySegment` argument arrived as `Value::Object`, missed every arm of the
write match and did nothing at all, silently.

A third suspicion did **not** survive measurement: `get(JAVA_BOOLEAN)` returned
the raw byte rather than 0/1, and that already answered `true` for a stray 2
through the return coercion. It is written explicitly now, but that is hardening
and the record says so rather than counting it as a fix.

## The fix

One registration loop for `set`, written directly under the `get` one and in the
same order, so the asymmetry that *is* this defect is visible to anyone reading
either list. Plus the two conversions, and the zero-length segment the JDK
specifies for an address read (size 0, so a caller must `reinterpret` before
dereferencing — the property that makes the read safe at all).

**Measured:** thirteen probe sections, `--real-jdk` and `--jdk-only`, byte-identical
to HotSpot on every behavioural line; five failing sections → **zero**.

One deliberate narrowing, recorded because it is a side effect rather than the
goal: every `MemorySegment.get` was already gated behind
`--enable-native-access`, and the four `set` carriers that had an implementation
were the only accessors **not** gated — a raw-address write guarded less than a
read. Routing all nine through `pe_segment_set` closes that, so a write-only
program that never passed the flag now sees `IllegalCallerException`.

## What this did not fix

`Arena.allocate` hands out an instance of `java.lang.foreign.MemorySegment`
itself where the JDK builds a `jdk.internal.foreign.NativeMemorySegmentImpl` —
which is why dispatch could reach an abstract interface method in the first
place. That is the `CompatibilityClassRequested` family, not an accessor defect,
and the probe prints it on its `shape` line precisely so a reader does not
mistake the one intentionally-differing line for a regression.

## How to check it stays closed

```sh
cratonvm --real-jdk --java-home "$JAVA_HOME" --enable-native-access=ALL-UNNAMED \
  -cp . FfmSegmentAccessProbe
```

Must print `FFMSEG sections=13 failed=0` and differ from
`java -cp . FfmSegmentAccessProbe` on the `shape` line only.
