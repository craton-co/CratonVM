# crash-02: native `(int)`-capacity collection/builder constructors abort the VM on a huge capacity

| | |
|---|---|
| **Category** | **VM-CRASH** (ABEND — `std::process::abort()`), family |
| **Module** | (VM-wide; surfaced while fixing [crash-01](crash-01-arraylist-capacity-oom-abend.md)) |
| **Affected APIs** | `HashMap(int)`, `HashSet(int)`, `LinkedHashMap(int)`, `ArrayDeque(int)`, `PriorityQueue(int)`, `StringBuilder(int)` |
| **CratonVM** | ABEND — `FATAL: OutOfMemoryError: young gen exhausted — tried to allocate … bytes`, then `abort()` |
| **HotSpot JDK 25** | maps: **no throw** (working empty map, lazy table); deque/PQ/StringBuilder: catchable `OutOfMemoryError` |
| **CratonVM HEAD** | `0e3f0398` (current dev) |
| **Status** | **FIXED** on `fix/oom-array-alloc-abend` (`da58ff4e`) — verified vs HotSpot |
| **Suggested owner** | **me (fixed)** |

## Symptom — measured (CratonVM `fix/oom-array-alloc-abend` build, each ctor with `Integer.MAX_VALUE`)
| Constructor | CratonVM | bytes it tried to allocate | HotSpot JDK 25 |
|---|---|---|---|
| `new ArrayList(M)` | **OOME (catchable)** ✓ fixed in crash-01 | — | OOME |
| `new Vector(M)` | **OOME (catchable)** ✓ (runs real JDK bytecode `new Object[M]`) | — | OOME |
| `new HashMap(M)` | **ABORT** | 8 589 934 632 (`Object[1<<30]`) | **no throw** (lazy table) |
| `new HashSet(M)` | **ABORT** | 8 589 934 632 | **no throw** |
| `new LinkedHashMap(M)` | **ABORT** | 8 589 934 632 | **no throw** |
| `new ArrayDeque(M)` | **ABORT** | 17 179 869 216 (`Object[2^31]`) | OOME |
| `new PriorityQueue(M)` | **ABORT** | 17 179 869 216 | OOME |
| `new StringBuilder(M)` | **ABORT** | 4 294 967 336 (`char[2^31]`) | OOME |

`M = Integer.MAX_VALUE`. The abort kills the whole VM (and, in a batch run, every other test in that JVM).

## Reproduce
```bash
VM=C:/craton/CratonVM-oomfix/target/release/cratonvm.exe
JDK='C:\Program Files\Eclipse Adoptium\jdk-25.0.2.10-hotspot'
# spring-suite/probe/One.java takes one of: hashmap hashset lhm deque pq vector sb
"$VM" --java-home "$JDK" -cp C:/craton/CratonVM/spring-suite/probe One hashmap   # ABORT
"$JDK\bin\java.exe"      -cp C:/craton/CratonVM/spring-suite/probe One hashmap   # NO THROW
```

## Root cause
Same archetype as crash-01: each of these native `<init>(I)V` handlers in
`native-collections/src/lib.rs` eagerly allocates the backing array sized to the requested capacity
through the **panicking** allocator (`ctx.new_ref_array` / `ctx.new_array` →
`GenerationalHeap::alloc_array` → `alloc_young` → `std::process::abort()`):
- `native_map_init_capacity` / `native_hs_init_capacity` / `native_lhm_init_capacity` allocate the
  bucket table at `tableSizeFor(cap)` (capped at `MAXIMUM_CAPACITY = 1<<30`) → `Object[1<<30]` = 8 GiB.
- `native_ad_init_capacity` (ArrayDeque) / `native_pq_init_capacity` (PriorityQueue) allocate
  `Object[~2^31]` = 16 GiB.
- the `StringBuilder(int)` native allocates `char[cap]` = 4 GiB.

## Fix (two shapes)
The fallible `NativeContext::try_new_ref_array` added for crash-01 already supplies the plumbing.

1. **deque / PQ → catchable OOME** (HotSpot-faithful): swap their eager `new_ref_array` for
   `try_new_ref_array`, throwing `OutOfMemoryError("Requested array size exceeds VM limit")` on `None`.
   Trivial copy of the crash-01 ArrayList change.
2. **StringBuilder → catchable OOME**: add a symmetric fallible **primitive** allocator
   (`try_new_array`, default-delegating like `try_new_ref_array`) and use it for the `char[]`.
3. **maps (HashMap/HashSet/LinkedHashMap) → do NOT throw** — HotSpot returns a *working* empty map
   (the table is sized lazily / capped, not eagerly allocated at `1<<30`). The correct fix is to
   **stop eagerly allocating the full `tableSizeFor(cap)` table**: cap the eager bucket-array to a
   sane bound (or defer allocation to first insert) and grow on demand. This is a behavioral change to
   the map-init path (slightly larger than 1–2; do NOT simply throw OOME here — that would diverge
   from HotSpot, which succeeds).

## Verified (FamFix.java — CratonVM vs HotSpot JDK 25)
```
== huge capacity (Integer.MAX_VALUE) — CratonVM now == HotSpot ==
HashMap(M)/HashSet(M)/LinkedHashMap(M): NO THROW (working empty map)   [HotSpot: NO THROW]
ArrayDeque(M)/PriorityQueue(M)/StringBuilder(M): OutOfMemoryError      [HotSpot: OutOfMemoryError]
== normal usage — no regression ==
HashMap(16)+100puts: size=100 get(50)=2500 get(99)=9801
HashMap(M)+1000puts: size=1000 ...           (capped table grew on demand; HotSpot OOMs on 1st put)
LinkedHashMap insertion order: [c, a, b]
HashSet(32): size=50 contains(25)=true
ArrayDeque(4)+10: size=10 ...   PriorityQueue/StringBuilder: ok
```
All six aborts eliminated. Maps match HotSpot's "ctor succeeds, no throw" (CratonVM is in fact more
lenient on a later `put` to a huge-capacity map, where HotSpot OOMs on the deferred table alloc —
acceptable; it never aborts).

> Side-finding (separate, **pre-existing**, NOT a crash): CratonVM's native `PriorityQueue.poll()`
> does **not** return min-heap order (`add 5,1,3,9,2` → polls `5 2 9 3 1`; HotSpot `1 2 3 5 9`).
> Identical on baseline `8e8e47d9`, so unrelated to this fix. Tracked separately (correctness, not
> in this crash/hang run's scope).

## Family closed generically (2026-07-31)

A fourth member turned up in H2 (`ByteBuffer.allocate(int)`, which aborted
`org.h2.test.db.TestOutOfMemory` with `SIGABRT`), which made it clear that
converting call sites one at a time does not converge — `native-builtins` has
~1000 `ctx.new_array(..)` sites. The abort is now closed at the *boundary*
instead: `vm/src/runtime/native_oom.rs` gives the native allocation helpers a
scoped unwind channel (permitted only directly under `safe_native_call`'s
`catch_unwind`, suspended for the duration of any JIT-compiled frame), and
`safe_native_call_impl` converts it into a catchable
`java.lang.OutOfMemoryError`. Every native reachable through `safe_native_call`
is covered without touching its call site. See
`docs/internal/fixed-suite-bugs/h2-suite-bugs/bug-h2-testoutofmemory-sigabrt-young-old-gen-both-exhausted-FIXED.md`.

## Notes
- **Severity in practice:** low likelihood of organic occurrence in the Spring suite — real framework
  code passes *realistic* sizes to these ctors, not `Integer.MAX_VALUE`. crash-01 surfaced only
  because SpEL's `ArrayConstructorTests` deliberately exercises the limit. Still a genuine
  VM-robustness gap (HotSpot never aborts here).
- Same general class as crash-01: "VM aborts where HotSpot throws/handles." All fixable with the
  `try_new_*` fallible-allocator pattern (plus the lazy-table change for maps).
- Related: [[crash-01-arraylist-capacity-oom-abend]].
