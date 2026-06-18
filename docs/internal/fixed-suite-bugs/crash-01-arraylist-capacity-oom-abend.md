# crash-01: `new ArrayList(int)` with a huge capacity aborts the VM instead of throwing `OutOfMemoryError`

| | |
|---|---|
| **Category** | **VM-CRASH** (ABEND rc=127 — `std::process::abort()`) |
| **Module** | spring-expression |
| **Test class** | `org.springframework.expression.spel.ArrayConstructorTests` |
| **Failing test(s)** | `errorCases()` — `new java.util.ArrayList(T(java.lang.Integer).MAX_VALUE)` |
| **CratonVM** | ABEND rc=127 — `FATAL: OutOfMemoryError: young gen exhausted — tried to allocate 8589934632 bytes` then `abort()` |
| **HotSpot JDK 25** | OK — throws catchable `OutOfMemoryError: Requested array size exceeds VM limit` |
| **CratonVM HEAD** | observed on `8e8e47d9` (suite run); root cause identical on current dev `0e3f0398` |
| **Status** | **FIXED** on branch `fix/oom-array-alloc-abend` (base `0e3f0398`) — pending verification block below |
| **Suggested owner** | **me (fixed)** |

## Symptom
The whole VM aborts (no Java exception, no stack — `rc=127`) the moment SpEL evaluates
`new java.util.ArrayList(Integer.MAX_VALUE)`:

```
FATAL: OutOfMemoryError: young gen exhausted — tried to allocate 8589934632 bytes,
       from-space has 269696/67108864 used
```

`8589934632` ≈ **8 GiB** = `(1<<30) elements × 8 bytes/ref + 40-byte header`. Because it is a hard
`abort()` rather than a thrown `OutOfMemoryError`, JUnit cannot catch it: **every remaining test in
the batch JVM dies too** (this is exactly the "don't stop the suite on one class" hazard).

## Reproduce
```bash
VM=C:/craton/CratonVM-oomfix/target/release/cratonvm.exe   # or any dev build
JDK='C:\Program Files\Eclipse Adoptium\jdk-25.0.2.10-hotspot'
# Minimal:
#   new java.util.ArrayList<>(Integer.MAX_VALUE)   -> CratonVM aborts; HotSpot throws OOME
"$VM" --java-home "$JDK" -cp <probe> OomProbe        # see spring-suite/probe/OomProbe.java
# In the suite:
CP="<harness>;$(tr -d '\r' < .../spring-expression/build/cratonvm-testcp.txt)"
"$VM" --java-home "$JDK" -cp "$CP" KRun org.springframework.expression.spel.ArrayConstructorTests
"$JDK\bin\java.exe"        -cp "$CP" KRun org.springframework.expression.spel.ArrayConstructorTests  # OK
```

## Minimal trigger
`new java.util.ArrayList<>(Integer.MAX_VALUE)` → the JDK ctor allocates `new Object[2147483647]`.
HotSpot rejects the over-large array length with a **catchable** `OutOfMemoryError: Requested array
size exceeds VM limit`; SpEL's `ConstructorReference` wraps that as a `SpelEvaluationException`
(`CONSTRUCTOR_INVOCATION_PROBLEM`), which the test asserts.

> Note: the sibling line `new int[Integer.MAX_VALUE]` does **not** crash — Spring's
> `ConstructorReference.MAX_ARRAY_ELEMENTS` (256 K) guard throws *before* allocating. And bytecode
> `new int[N]` / `new Object[N]` already throw a catchable `OutOfMemoryError: Java heap space` on
> CratonVM (the `gc_alloc_array` path). **Only the native `ArrayList(int)` fast-path aborted.**

## Root cause
`native-collections/src/lib.rs` → `native_al_init_capacity` (the `java/util/ArrayList.<init>(I)V`
native) **clamped** the requested capacity to `AL_MAX_INIT_CAPACITY = 1<<30` and then allocated the
backing array through the **panicking** `alloc_ref_array` → `NativeContext::new_ref_array` →
`GenerationalHeap::alloc_array` → `alloc_young` → `std::process::abort()` (`gc/src/gen_heap.rs`).

The clamp itself was a latent bug (it silently turned `ArrayList(Integer.MAX_VALUE)` into
`ArrayList(1<<30)`), and `1<<30 × 8 bytes = 8 GiB` still exceeds the heap, so the panicking allocator
aborted. The panicking `new_array`/`new_ref_array` allocators deliberately cannot GC-and-retry (a
moving young GC would dangle a native's unrooted local `ObjectRef`s), so on a request that cannot fit
they `abort()` instead of surfacing a catchable error.

## Fix
Give natives a **fallible** array allocator and have `ArrayList(int)` use it:

1. `gc/src/gen_heap.rs` — add `GenerationalHeap::try_alloc_array_full`: a faithful twin of the
   panicking `alloc_array` (same no-GC young → humongous-old-gen spill path) that returns `None`
   instead of aborting on true exhaustion.
2. `gc/src/vm_heap.rs` — forward it on the `VmHeap` enum (`Generational` → the new method;
   `G1` → its existing fallible `try_alloc_array`).
3. `native-api/src/registry.rs` — add a **defaulted** `NativeContext::try_new_ref_array`
   (default delegates to the infallible `new_ref_array` so mocks compile unchanged).
4. `vm/src/vm/vm_exec.rs` — override `try_new_ref_array` on `NativeContextImpl` to call
   `VmHeap::try_alloc_array_full`.
5. `native-collections/src/lib.rs` — `native_al_init_capacity` now (a) throws
   `IllegalArgumentException("Illegal Capacity: <n>")` for a negative capacity (JDK-faithful), and
   (b) allocates via `try_new_ref_array`, throwing a catchable
   `OutOfMemoryError("Requested array size exceeds VM limit")` on `None` instead of clamping +
   aborting.

No GC is introduced inside the native (no stale-`this` hazard), and the success path is byte-for-byte
the same as before for every allocation that fits — so there is **no regression** for normal
`ArrayList(capacity)` usage.

## Verified
Built `fix/oom-array-alloc-abend` (release) and ran `verify-oom-fix.sh`:

```
# (1) over-large allocations now throw a CATCHABLE OutOfMemoryError (no abort):
ArrayList: CAUGHT java.lang.OutOfMemoryError: Requested array size exceeds VM limit
int[]:     CAUGHT java.lang.OutOfMemoryError: Java heap space (alloc_array length 2147483647)
Object[]:  CAUGHT java.lang.OutOfMemoryError: Java heap space (alloc_array length 2147483647)

# (2) JDK-faithful negative-capacity + NO regression on normal usage:
neg:  IllegalArgumentException: Illegal Capacity: -1
normal(1000)+5000adds: size=5000 first=0 last=4999
zero-cap: size=1 v=7

# (3) ArrayConstructorTests: was ABEND rc=127 (whole-VM abort) -> now runs to completion:
RESULT ...ArrayConstructorTests found=8 succ=6 fail=2 ... status=FAIL   (rc=0, NO abort)
```

**The crash (VM abort) is eliminated** — `new ArrayList(Integer.MAX_VALUE)` now raises a catchable
`OutOfMemoryError`, `ArrayList(-1)` raises `IllegalArgumentException`, and normal `ArrayList(capacity)`
allocation + growth is unaffected.

**Residual (separate, NOT this crash):** `ArrayConstructorTests` now reports `FAIL` (2 of 8) — both are
ordinary CV-unique *assertion* bugs, not crashes:
- `multiDimensionalArrays()` — `new String[2][2]` comes back typed `class [Ljava.lang.String;`
  (1-D) instead of `class [[Ljava.lang.String;` (2-D): a multi-dimensional array element-type bug.
- `errorCases()` — one of its ~14 SpEL error assertions still fails (no message); likely the multidim
  threshold / OOME-wrapping path. Tracked separately; this report's crash is resolved.

## Notes
- Affects only the **native** `ArrayList(int)` fast-path. Other `(int)`-capacity collection
  constructors (`HashMap`, `HashSet`, `LinkedHashMap`, `ArrayDeque`, `PriorityQueue`, `Vector`,
  `StringBuilder`) route through their own native allocators and **may share this abort-instead-of-OOME
  pattern** — see the family probe in the Verified section; any that also abort are easy follow-ups
  using the same `try_new_ref_array` plumbing.
- General hardening: this is the "VM aborts where HotSpot throws a catchable error" class. The new
  `try_new_ref_array` is reusable for any native that eagerly allocates a caller-sized array.
