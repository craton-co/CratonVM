# bug-03: native `PriorityQueue` doesn't order boxed elements (not a min-heap)

| | |
|---|---|
| **Category** | **VM-CORRECTNESS** (not a crash/hang — surfaced during crash-02) |
| **Affected API** | `java.util.PriorityQueue` (natural ordering, no explicit comparator) |
| **CratonVM** | `poll()`/`peek()` return wrong element — heap never orders boxed values |
| **HotSpot JDK 25** | correct min-heap order |
| **CratonVM HEAD** | pre-existing on `8e8e47d9`; fixed on `0e3f0398` |
| **Status** | **FIXED** on `fix/oom-array-alloc-abend` (`936b8e19`) — verified vs HotSpot |
| **Suggested owner** | **me (fixed)** |

## Symptom
```java
PriorityQueue<Integer> pq = new PriorityQueue<>();
pq.add(5); pq.add(1); pq.add(3); pq.add(9); pq.add(2);
while(!pq.isEmpty()) System.out.print(pq.poll()+" ");
// CratonVM (before): 5 2 9 3 1     HotSpot: 1 2 3 5 9
```
`peek()` likewise returned a non-minimal element. Affects any natural-ordering `PriorityQueue`
(comparator-based queues were unaffected).

## Root cause
`native-collections/src/lib.rs` `pq_compare`: the heap machinery (`pq_sift_up`/`pq_sift_down`/
`add`/`poll`) was correct, but the comparator returned **0 (equal)** for the actual element type.
`PriorityQueue` elements are virtually always **boxed** (`pq.add(5)` stores an `Integer` object, not a
primitive `Value::Int`), and the natural-ordering branch only handled raw `Value::Int/Long/..` plus
`String`-via-`read_string`. Two boxed `Integer`s fell through to `0`, so `sift_up`/`sift_down` never
swapped → elements came out in (heap-shuffled) insertion order.

## Fix
For the no-comparator path, dispatch the element's real `Comparable.compareTo` —
`ctx.invoke_virtual(a, "compareTo", "(Ljava/lang/Object;)I", &[b])` — exactly what HotSpot's
`PriorityQueue.siftUpComparable` does. Works uniformly for `Integer`/`Long`/`Double`/`String` and any
user `Comparable`. The explicit-`Comparator` path is unchanged.

## Verified (PqFix.java — CratonVM == HotSpot)
```
int natural:      0 1 2 3 4 5 6 7 8 9   sorted=true
peek min:         3
string natural:   apple banana fig mango pear
int reverse-cmp:  9 8 7 6 5 4 3 2 1 0          (Comparator.reverseOrder())
closest-to-5:     5 4                          (custom comparator)
```

## Notes
- Pre-existing (identical on baseline `8e8e47d9`); unrelated to the crash-01/02 capacity-OOM fix on
  the same branch, just discovered alongside it.
- Blast radius: any Spring/JDK test relying on `PriorityQueue` natural ordering would FAIL CV-uniquely.
- Related: [[crash-02-native-capacity-ctor-abort-family]].
