# H2 `TestValueMemory`: every value measures zero

*2026-09-04. Found while re-surveying the H2 corpus after the defect this page
replaces turned out to be fixed.*

## Status of the page this replaces

`org.h2.test.store.TestCacheLIRS` was recorded on 2026-09-03 as failing on
CratonVM with `AssertionError: Expected: 0 actual: 5` while passing on HotSpot.
**It now passes** — 5 runs of 5, on the default configuration and under
`CRATONVM_COMPACT_TLAB_ALLOC=0`, at 12.9 s. Something on dev between 09-03 and
09-04 fixed it; attributing which change would cost a bisect of ~15-minute
builds and the bug is gone either way. Recorded here so the next person does not
go looking for it.

## The one that is still open

`org.h2.test.unit.TestValueMemory` passes on HotSpot (`rc=0`, 41 value types)
and fails on CratonVM (`rc=1`) after 4 types.

The diagnostic lines it prints name the shape of the failure exactly. HotSpot:

```
Type: 39 Used memory: 796 calculated: 976 length: 25000 size: 25000
Type: 40 Used memory: 713 calculated: 976 length: 510  size: 510
```

CratonVM:

```
type: 38 calculated: 111  real: 0   org.h2.value.ValueJson
type: 39 calculated: 32   real: 0   org.h2.value.ValueUuid
type: 40 calculated: 1972 real: 0   org.h2.value.ValueArray
type: 41 calculated: 1706 real: 0   org.h2.value.ValueRow
```

**Every value measures `real: 0`.** The test compares a value's computed
`getMemory()` against a measured retained size, so a measurement that is
identically zero fails every type it reaches.

## What has been ruled out

The obvious suspect is `Utils.getMemoryUsed()`, which H2 defines as
`collectGarbage(); (Runtime.totalMemory() - Runtime.freeMemory()) >> 10`. That
is **not** the fault:

`scratchpad/HeapUsedDelta.java` retains 400,000 small arrays and prints that
exact expression before and after:

| VM | before | after | delta |
|---|---|---|---|
| HotSpot | 1,784 KB | 34,612 KB | **+32,828 KB** |
| CratonVM | 1,412 KB | 36,025 KB | **+34,613 KB** |

Both grow, by comparable amounts. So `totalMemory()`/`freeMemory()` track
retention on CratonVM in the general case, and the `Used memory:` line (capital
`T` `Type:`) is a different measurement from the `real:` line (lowercase
`type:`) that actually fails.

## What has NOT been established

Which measurement produces `real:`. The string is not in
`TestValueMemory`'s own bytecode — it comes from elsewhere in H2, so the next
step is to find the producer rather than assume it is the `Runtime` pair. It
would be a mistake to "fix" `totalMemory`/`freeMemory` on the strength of this
page: the probe above says they already work.

## Reproducer

```
CP=$(cat /c/craton/h2corpus/cp.txt)
cratonvm --java-home "$JDK25" -XX:+UseGenerationalGC -Xmx2g -cp "$CP" \
    org.h2.test.unit.TestValueMemory
```

`rc=1` on CratonVM, `rc=0` on HotSpot, in under a second either way.

## The rest of the corpus, as of this date

| class | CratonVM | note |
|---|---|---|
| `store.TestCacheLIRS` | `rc=0` 12.9 s | **fixed since 09-03** |
| `unit.TestBitStream` | `rc=0` 5.7 s | |
| `store.TestObjectDataType` | `rc=0` 0.8 s | |
| `store.TestDataUtils` | `rc=0` 6.4 s | |
| `store.TestSpinLock` | `rc=0` 0.7 s | |
| `unit.TestIntPerfectHash` | `rc=0` 10.5 s | |
| `unit.TestStringUtils` | `rc=0` 0.6 s | |
| `unit.TestValueMemory` | `rc=1` | **this page** |
| `store.TestMVStore` | `rc=124` (timeout, 200 s) | also fails on HotSpot |
| `store.TestMVRTree` | `rc=1` | also fails on HotSpot |

`TestMVStore` is worth a second look on its own terms: it fails an assertion on
HotSpot but **hangs** on CratonVM, and a hang and a failed assertion are not the
same defect. It is not an oracle either way.
