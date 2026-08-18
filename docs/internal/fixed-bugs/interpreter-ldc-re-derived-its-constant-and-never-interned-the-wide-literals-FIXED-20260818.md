# `ldc` re-derived its constant every execution, and never interned the surrogate literals — FIXED 2026-08-18

**Status:** FIXED. The recorded-constant probe is default-ON;
`CRATONVM_JIT_NO_LDC_CONST_CACHE=1` opts out.

**Reproducers:** `probes/LdcStringIdentityProbe.java` (identity),
`probes/LdcConstCostProbe.java` (cost),
`difftest/seeds/LdcConstCache.java` (parity, `strict`).

Two findings in one function. They are filed together because the identity bug
had to be fixed *first* — caching a value that was a fresh object on every
execution would have frozen one arbitrary instance per call site and made the
divergence harder to see, not easier.

## 1. A surrogate-bearing string literal was never interned

JVMS §5.1 interns string literals. Two `ldc`s of one `CONSTANT_String` must
push the identical reference, and `LITERAL == LITERAL.intern()` must hold.

The ordinary literal path got that from `create_java_string`, which pools on
the Rust `String`. A literal carrying a **lone surrogate** cannot be keyed that
way — a Rust `String` cannot hold one — so the wide path called
`create_java_string_from_units`:

```rust
pub fn create_java_string_from_units(shared: &SharedVm, units: &[u16]) -> ObjectRef {
    alloc_java_string_object_from_units(shared, units)   // pools NOTHING
}
```

A fresh object, every execution. So `"\uD800" == "\uD800"` answered `false`
where HotSpot answers `true`, `LITERAL == LITERAL.intern()` answered `false`,
and every execution of such an `ldc` put another `String` on the heap that
nothing could dedupe.

The reachable case is named in the wide path's own comment: ANTLR's
`_serializedATN`, which is exactly a long surrogate-bearing literal `ldc`'d on
a hot path.

### The fix, and why it is the existing pool

`String.intern()` already had a surrogate pool, keyed on UTF-16 units, added
when `intern()` hit the same representability wall. Its comment states the
contract it exists to keep: `s.equals(t)` must imply `s.intern() == t.intern()`.

`ldc` now interns through **that** table rather than a second one. It has to be
that table: two tables would keep `"\uD800" == "\uD800"` true and still answer
`LITERAL == LITERAL.intern()` false.

The pool grew two published halves — `surrogate_intern_probe` (read) and
`surrogate_intern_claim` (write) — because the two callers own different root
APIs: `String.intern()` has a `NativeContext`, the interpreter's `ldc` has a
`SharedVm`. Values are global-root **handles**, never `ObjectRef`s, for the
reason the pool's original comment gives: the collector owns the reference and
a raw `ObjectRef` parked in a `static` is a stale address after the next
relocating cycle.

`intern_unrepresentable` now probes before rooting, so an already-interned
literal costs one lock and no root traffic.

## 2. `ldc` re-derived a constant whose answer is fixed

Every executed `ldc` did this before it could push:

```rust
let cm = shared.classes.class_manager.read();   // RwLock read
let class = cm.get_class(frame_class_id)…;
let entry = class.constant_pool.get(index)…;
```

and then, per tag:

* **String** — `get_utf8`, `.to_string()` (an owned allocation of the literal's
  content), then the intern pool's read lock and a hash of the whole content.
* **Class** — `.to_string()`, a full `resolve_class_loader_aware` **by name**,
  then a mirror lookup.

JVMS §5.4.3: a resolved constant-pool entry returns the same result on every
later resolution of it. So this is re-derivation, not work.

This is the fifth instance of the shape the 2026-08-18 interpreter audit kept
finding — the interpreter recomputing per execution an answer that is fixed per
call site — after the back-edge counters, the field site cache, the eleven
armless opcodes and the cast sites.

### The store already existed and was already correct

The recorded-constant map, keyed `(class, cp index)`:

* the collector **scans and remaps** it — `for_each_condy_root` /
  `update_condy_refs`, reached from `collect_roots` and `update_all_roots`,
  which are the collector-agnostic VM-side hooks fed whatever `pointer_map` the
  active collector produced. Verified by call-site enumeration, because
  compacting ZGC is the default and a single-collector remap would have been a
  use-after-move;
* **loader unloading** visits each root with its owning class, so the root is
  conditional on defining-loader liveness;
* **redefinition** invalidates by referencing class (`invalidate_class`, fanned
  out to every live VM);
* it is **bounded** at `RESOLUTION_CACHE_CAP` (64K) with FIFO eviction.

`CONSTANT_MethodType` / `CONSTANT_MethodHandle` had recorded through it for
some time — but in the *second* phase, after the lock was taken and an
`LdcValue` built. They skipped the JDK factory and paid the lock anyway.

### What changed

The probe moved to the **top** of `execute_ldc`, ahead of the lock. `String`,
the wide literal, `Class`, `Integer` and `Float` now record as well.

**Every tag records, and `Integer`/`Float` are the reason to say so.** Left
unrecorded they would MISS the probe on every execution and then resolve
anyway — the probe would be pure added cost for them. This was caught while
writing the change, not measured afterwards; the first draft recorded only the
expensive tags on the reasoning that "a hashmap probe does not beat a slice
index", which ignored that reaching the slice index still costs the
`class_manager` lock and a `get_class`.

**`ldc2_w` is deliberately untouched.** It must push through `push_long` /
`push_double` to keep the CompactValue tag — a `Value` round-trip collapses
`Long` into the untagged `Double` bucket — and its only costly tag (condy)
already caches inside `resolve_condy_constant`. A constant-pool index has
exactly one tag, so a `Long`/`Double` entry is unreachable from the `ldc`
probe.

**`Class` records without a loader-namespace guard**, unlike the cast and `new`
site caches. Those key on the referencing class to make a *resolution* reusable;
here the key **is** the constant-pool entry, and §5.4.3 makes recording the
specified behaviour rather than an optimization that has to prove itself
loader-safe. The `MethodType`/`MethodHandle` arms have recorded on the same key
through the same store all along. A **failed** resolution is not recorded — the
error must be raised again on each attempt, and the seed checks that.

## Measurements

TO BE FILLED

## Verification

TO BE FILLED
