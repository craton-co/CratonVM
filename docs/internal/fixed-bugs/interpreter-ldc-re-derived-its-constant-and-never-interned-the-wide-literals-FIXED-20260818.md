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

## Verification

### Engagement, before any number was believed

```text
default        ldc: hit=1327337 miss=2469 fill=2469
kill switch    ldc: hit=0       miss=0    fill=0
```

The first build of this change reported `hit=0 miss=0 fill=1329806` under the
kill switch — the cache not being read and still being written, so the OFF arm
took a resolution-cache WRITE lock per `ldc` that the pre-change interpreter
never took. That does not merely waste work: the OFF arm is supposed to BE the
baseline, so making it slower than the real pre-change code inflates any
speedup computed against it. The five new recording sites now go through
`record_cp_constant_if_enabled`; `MethodType`/`MethodHandle` still record
unconditionally, because they recorded before this change and the switched-off
arm has to reproduce that.

### Identity, against HotSpot

`probes/LdcStringIdentityProbe.java`, 14 rows, every one an exact value.

| binary | rows differing from HotSpot |
|---|---|
| pre-change dev `9d79b364f` | **5** |
| this change, cache ON | 0 |
| this change, cache OFF | 0 |
| this change, JIT on | 0 |

The five: `same-method lone-high`, `same-method lone-low`, `cross-method
lone-high`, `repeat-stable lone-high`, `intern lone-high`. `pair` and `plain`
pass on both binaries, so the probe discriminates rather than failing
everything.

### The probe was vacuous first, and the count is the reason it was caught

Four of those fourteen rows originally tested **nothing**. Written the obvious
way —

```java
System.out.println("same-method lone-high: " + ("\uD800" == "\uD800"));
```

— javac constant-folds the comparison and emits a single
`ldc // String same-method lone-high: true`. The row prints `true` on a VM that
never interned anything and never executed the opcode under test. Confirmed
with `javap -c`.

The same defect covered **6 of 10** identity rows in
`difftest/seeds/LdcConstCache.java`, because `STATIC_FINAL == "literal"` folds
exactly as `"a" == "a"` does.

Two things worth carrying forward. The vacuous rows were the ones that read as
the *most direct* test in the file; the rows that actually caught the defect
were the indirect-looking ones going through `loneHigh()` and `.intern()`. And
the cheap check that settles it is to count the opcode in the compiled probe —
a probe for `ldc` that does not contain an `ldc` is the purest vacuous green
there is. After the rewrite through non-final locals, `main` carries 23.

De-vacuuming changed the verdict: the probe went from catching 3 divergences
against the pre-change binary to catching 5.

## Measurements

One binary, one env var, `--nojit`, six rounds — **three with the OFF arm first
and three with the ON arm first**. Compared **paired within each round**, since
both arms of a round see the same host load; the host was building throughout,
so unpaired medians would mostly measure drift. Marginal ns per `ldc`:

| round | | `ldc` String | `ldc` Class | `iadd` control |
|---|---|---|---|---|
| 1 | OFF → ON | 220.8 → 69.2 | 554.2 → 70.3 | 35.0 / 22.6 |
| 2 | OFF → ON | 133.3 → 84.4 | 305.0 → 80.7 | 19.9 / 25.8 |
| 3 | OFF → ON | 134.8 → 65.5 | 305.6 → 169.9 | 20.0 / 33.3 |
| 4 | ON → OFF | 148.5 ← 227.0 | 144.0 ← 543.1 | 39.6 / 30.4 |
| 5 | ON → OFF | 151.2 ← 211.8 | 148.1 ← 497.7 | 34.3 / 37.1 |
| 6 | ON → OFF | 147.0 ← 205.0 | 138.3 ← 516.9 | 34.6 / 30.7 |

**`ldc` of a String ~1.6x** (per-round ratios 3.2 / 1.6 / 2.1 / 1.5 / 1.4 / 1.4)
and **`ldc` of a Class ~3.8x** (7.9 / 3.8 / 1.8 / 3.8 / 3.4 / 3.7). All six
rounds favour the cache in both metrics, and the direction survives reversing
the arm order at round 4.

The Class figure is the larger one because that arm was skipping the most: a
`String` allocation plus a full `resolve_class_loader_aware` BY NAME plus a
mirror lookup, against the String arm's allocation plus content hash.

The control spans 19.9-39.6 ns across the whole matrix without tracking the arm
(the ON arm holds both near-minimum and maximum control readings), which is what
a loaded host looks like and why the analysis is paired rather than pooled.

**The `ldc-string` sets do overlap** between arms — ON reaches 151.2 while OFF
reaches 133.3, across different rounds — so 1.6x is the honest reading of the
paired ratios, not a separation claim. The `ldc-class` sets do not overlap at
all: ON peaks at 169.9 against an OFF floor of 305.0.

## A withdrawn claim, recorded because the number was stated

While measuring invocation cost on this binary, a first version of
`probes/MethodCountDispatchProbe.java` reported `final` instance calls at 3.8x
`virtual`. That claim is **withdrawn**.

There is no finality branch anywhere in `runtime/interpreter/` or
`runtime/resolve/` — nothing in the code can produce the effect. The probe ran
its six arms sequentially with `final` measured after `virtual`, on a host whose
load was ramping from a concurrent build: the run's `iadd` control read 61.6ns
against 21.9ns minutes earlier. Rising load lands entirely on whichever arm is
measured last, which is sufficient to manufacture the whole ratio.

The probe now interleaves every arm within each of five rounds and prints a
per-round control so a drifting run can be discarded rather than believed. What
survives from the control-stable run is only this: against HotSpot's
interpreter, CratonVM's invocations cost 12-29x while its `iadd` costs 5.9x, so
invocation is disproportionately expensive rather than uniformly slow. The
per-opcode attribution has not been earned.

## Suite results

* **difftest** — 0/6 diverged across `jit-on`, `nojit` and `interp-decoded`,
  including the new `LdcConstCache` seed.
* **regression-suite** — 63/64. `RImmutableFactoryTypes` fails, and fails
  **identically with `CRATONVM_JIT_NO_LDC_CONST_CACHE=1`**, so it is pre-existing
  rather than caused here. Checked on this binary rather than carried over from
  an earlier note.
