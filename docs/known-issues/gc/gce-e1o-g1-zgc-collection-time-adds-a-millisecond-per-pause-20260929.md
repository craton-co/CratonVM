# On G1 and ZGC, `getCollectionTime()` adds one millisecond per pause; Serial's bean stopped doing that

- **Status:** OPEN (observability; a wrong number, no crash). Filed 2026-09-29 by gce e1/o, by reading `adb9178bc`. Not run.
- **Backend:** G1 and ZGC beans (common code); Generational is already right.
- **Code:** `gc/src/gc_metrics.rs::BackendGcBeans::collectors` -> `gc/src/vm_heap.rs::pause_sum_as_collection_time_ms` (`total_pause_us / 1000 + collections`), also `g1_collection_time_ms`; against `gc/src/gc_metrics.rs::serial_collectors` (`total_us / 1000`).

## What is wrong

HotSpot's `GarbageCollectorMXBean.getCollectionTime()` is the bean's accumulated elapsed pause time converted once to milliseconds (`GCMemoryManager::_accumulated_timer`). gen r5w3/obs7 made the Generational (Serial-named) beans do exactly that (`serial_collectors`: "1 000 young pauses of 200 µs read 1 000 ms where HotSpot reads 200"), and its doc establishes that H2's `Utils.collectGarbage()` needs the summed time to move EVENTUALLY, not on every collection -- HotSpot carries no per-collection guarantee either.

The G1 and ZGC beans still use the pre-obs7 rule: `pause_sum_as_collection_time_ms(us, count)` = whole milliseconds of the sum PLUS ONE PER COLLECTION, justified by the older reading of H2 ("a caller polling for change would wait on forever"). So on ZGC, whose `ZGC Pauses` bean counts every stop-the-world pause (typically well under a millisecond), `getCollectionTime()` is roughly the pause COUNT in milliseconds; 1 000 pauses of 100 µs read 1 100 ms where HotSpot reads 100. G1's young bean is inflated the same way. Two docstrings in the tree now give opposite answers to the same H2 question.

## What would fix it

`BackendGcBeans::collectors`: `time_ms = us / 1_000` (Serial's rule), and retire `pause_sum_as_collection_time_ms` / `g1_collection_time_ms`'s `+ collections` with their doc corrected to `serial_collectors`' reading. Behind a switch for one wave (it changes G1/ZGC-visible numbers): `CRATONVM_GC_BEAN_TIME_PER_PAUSE_CEIL=1` would restore the `+1`.

## How to verify

- Unit: a `BackendGcBeans` with 1 000 notes of 100 µs reports `time_ms == 100` for each counting bean (today 1 100).
- Probe: `tools/bench/Gcd1MxNamesProbe.java` and an H2-shaped loop (`while (count == sum(getCollectionTime())) System.gc();`) under `-XX:+UseZGC` and `-XX:+UseG1GC`: the loop ends (it did on Serial after obs7), and `getCollectionTime()` stays within a few ms of `-Xlog:gc`'s summed pauses; HotSpot 25 with the same collector for the oracle's order of magnitude.
