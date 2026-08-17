# G56-1 — the zero-memory premise that expired, and the raw accessor that was never there

**Status:** FIXED-SOURCE / BEFORE-MEASURED / AFTER-PREDICTED (this lane may not
build) / ACCESSORS ADDED / ONE BETTER FIX FOUND AND NOMINATED.

**Provenance.** Every runtime number below is MEASURED on
`C:/craton/target-rel4/release/cratonvm.exe` (built from `cb2ade4fd`, a
**BEFORE** binary that provably cannot contain this lane's edits), `--jdk-only`,
classpath `C:/craton/CratonVM1/regression-suite/build`. Unlike every prior
record in this series, `target-rel4` **does** populate `class_id` and `index`,
so `CRATONVM_DBG_LAYOUT=1` + `CRATONVM_DBG_COERCION=1` in the same run names
every event's class and slot directly — no backtrace inference. Layout claims
are cross-checked against `javap -p -s` on HotSpot 25.0.3+9-LTS
(`Temurin-25.0.3+9`). Source claims are SOURCE-VERIFIED with file and symbol.
`C:/craton/target-fcheck/` was ignored as instructed.

**Files owned and changed:** `vm/src/runtime/interpreter/gc_and_alloc.rs`,
`native-api/src/registry.rs`. Nothing else.

**Logs / scripts:** scratchpad `g56/` — 16 attributed vector logs
(`*.full.txt`), the `--nojit` differential, the `javap -p -s` field census.

Continues `G30-1` (the instrument), `G49-1` (NOMINATION N1, which is this
record's Assignment A), and `G52-1` (NOMINATION 1, which is Assignment B).

---

## 0. The headline

| the brief asked | answer |
|---|---|
| Does the stale comment explain ~95% of the instrument? | **It explains 99.0%.** MEASURED, 16 vectors, 1,122 events: **1,111** are `primitive-into-reference` / `read` / `L`\|`[` / `Int(0)` — the first descriptor-aware read of a reference field the allocator left as raw zero. The residue is **11 events**, every one named in §2.2. |
| Is writing an explicit `Object(None)` what the readers expect? | **Yes, and four of them are already doing it by hand.** The interpreter's `getfield` carries a local `Int(0)|Long(0) => Object(None)` fixup for exactly this slot shape; the JIT's inline `getfield` never reads the tag; the coercion answers `Object(None)`; `values_equal_for_cas` equates the two in both directions. Nothing reads the raw slot and depends on `Int(0)`. §3. |
| Write, or tag? | **Write — because the tag option is not available at this layer, and I proved it rather than assumed it.** `Value` is `#[repr(u32)]` with `Int = 0` and `Object = 4`, so `null` has a *nonzero* tag word and cannot come out of a zero fill. §1. |
| The cost? | One `VmHeap::set_field` per reference instance field per allocation; MEASURED static field mix 361 primitive : 613 reference, so ≈1.7× the stores this function makes. The added store is the cheapest one it makes — `write_barrier` returns on its first tag test for a null, and there is no SATB pre-barrier on this path. **The after-cost itself could not be measured: this lane may not build.** §4. |
| Is there a cheaper correct option? | **Yes, and it is not mine to take.** Swapping `Value`'s `Int` and `Object` discriminants makes the zero fill *itself* mean null, deletes 613 writes and adds 318, and unblocks the JIT's `skip_helper` fast path instead of breaking it. It is a `types/` + `jit/` + `gc/` change. NOMINATION 4, with the measurement attached. |
| The raw accessors? | Added. `get_field_raw` is **genuinely raw in production today with no override**; `set_field_raw` cannot be, and its doc says so in as many words. §5. |

---

## 1. The premise, and why it is a write and not a tag

`vm/src/runtime/interpreter/gc_and_alloc.rs`, `init_primitive_fields`, before:

```rust
    _ => None, // Reference types: already Object(None) from zero memory
```

`types/src/value.rs` settles it. `Value` is `#[repr(u32)]` and the header there
calls that layout "load-bearing, not decoration" because the JIT emits machine
code against it. Declaration order fixes the discriminants:

| variant | tag word at byte 0 | 16-byte cell |
|---|---:|---|
| `Value::Int(0)` | **0** | all zero |
| `Value::Long(0)` | 1 | |
| `Value::Float(0.0)` | 2 | |
| `Value::Double(0.0)` | 3 | |
| `Value::Object(None)` | **4** | tag word 4, payload64 zero |

So the all-zero cell decodes as `Int(0)`, and `Object(None)` is a *different*
bit pattern. `gen_heap::read_slot`'s own doc says the same thing in the same
words — *"there is no 'zeroed slot reads as null' shortcut"* — and
`alloc_object_with_descriptors` has been compensating with an explicit
`.unwrap_or(Value::Object(None))` since the niche landed. The comment predates
the niche. Everything downstream of it has been repairing the slot on the way
out ever since.

**The tag option, considered and rejected at this layer.** A zero fill cannot
produce a nonzero tag word, so the only way to make the slot null without a
per-field store is to stop zero-filling and start pattern-filling with
`{4,0,0,0, 0,0,0,0, 0,0,0,0, 0,0,0,0}`. That is `gc/src/gen_heap.rs`'s
`try_alloc_young_initialized` (`std::ptr::write_bytes(ptr, 0, size)`) — not this
lane's file — and it is not free: `write_bytes` with a repeating 16-byte pattern
is a loop, not a `memset`, and it would then owe an explicit write to every
*primitive* slot, which the zero fill currently supplies for free. It trades one
population of writes for another. NOMINATION 4 is the version of this idea that
actually wins, and it is a different change.

**A note the next lane should not have to rediscover.** Because the body *is*
zero-filled, `jvm_default_for_descriptor`'s `Int(0)` arm is writing bits the
allocator already wrote — 318 of the 361 primitive fields in the measured mix
(`I B C S Z`; `J F D` have nonzero tags and genuinely need theirs). Skipping
them would nearly halve this fix's added cost. **It was deliberately not taken**,
because "the allocator zeroes" is a premise held in a different crate across
three collector backends and two allocation paths (the old-gen spill reuses
freed blocks), and *believing an allocator-behaviour premise stated in a distant
file* is the exact mistake this record exists to close. NOMINATION 5.

### 1.1 The fix

```rust
#[inline]
pub fn jvm_default_for_descriptor(desc_first: u8) -> Value {
    match desc_first {
        b'I' | b'B' | b'C' | b'S' | b'Z' => Value::Int(0),
        b'J' => Value::Long(0),
        b'F' => Value::Float(0.0),
        b'D' => Value::Double(0.0),
        _ => Value::Object(None),
    }
}
```

and the loop stores it unconditionally instead of `if let Some(val)`. The
function keeps the name `init_primitive_fields`: it is spelled at **eleven** call
sites in `vm_exec.rs` and `vm_init.rs`, neither of which is this lane's file, so
renaming it is a wider edit than the fix deserves. Its doc comment says to read
it as `init_default_fields`.

Three properties, each deliberate:

1. **Total by construction.** A malformed or unrecognised descriptor byte
   answers `null` — the same fall-open
   `heap::default_value_for_descriptor(b).unwrap_or(Value::Object(None))` takes
   in `alloc_object_with_descriptors`. `f.descriptor.as_bytes().first()` can
   genuinely produce a malformed byte, and the two allocation entry points must
   not disagree about it. Pinned over **all 256 bytes** by
   `the_two_allocation_entry_points_agree_on_all_256_descriptor_bytes`.
2. **The mapping is split out of the hot loop** so it is testable without a
   `SharedVm` and a populated class store, and so the JIT's byte-for-byte
   duplicate can be collapsed onto one table (NOMINATION 1). `#[inline]`, so
   the split costs the allocation path nothing.
3. **The "fresh object only" precondition is now load-bearing** and is stated in
   the doc comment. All eleven call sites were read: every one invokes this
   immediately after `alloc_object` / `try_alloc_object_full`, on an object
   nothing has written. Called on a *populated* object this would now null every
   reference field. It was harmless before only because the reference arm did
   nothing.

---

## 2. The instrument, before

MEASURED, `target-rel4`, `--jdk-only`, `CRATONVM_DBG_LAYOUT=1` +
`CRATONVM_DBG_COERCION=1` in the same run so class ids resolve within their own
run's id space. 16 vectors, **1,122 events**.

| vector | events | | vector | events |
|---|---:|---|---|---:|
| RJdkNet | 243 | | RJdkHello | 22 |
| RJdkReflect | 231 | | RPriorityQueueGc | 14 |
| RSerial | 217 | | RCollections | **0** |
| RJdkLogging | 142 | | RForNameGcStress | **0** |
| RJdkProcess | 106 | | RJitGc | **0** |
| RStrings | 69 | | RMapGcStress | **0** |
| RJdkSecurity | 47 | | RMapResizeGc | **0** |
| RCrypto | 31 | | RTreeRangeGc | **0** |

The six zeros are the GC-stress family, confirming G45-1 §3 and G49-1 §2 for a
third time: this defect does not live where the collector is exercised, it lives
where JDK library objects are constructed and read.

### 2.1 By site — the classes, named, not inferred

| n | class | slot | real field | desc | value | dir |
|---:|---|---:|---|---|---|---|
| **742** | `java/lang/ref/ReferenceQueue` | 0 | `head` | `L` | `Int(0)` | read |
| **243** | `java/util/Properties` | 16 | `defaults` | `L` | `Int(0)` | read |
| 27 | `java/lang/invoke/MemberName` | 4 | `method` | `L` | `Int(0)` | read |
| 21 | `java/lang/invoke/MemberName` | 5 | `resolution` | `L` | `Int(0)` | read |
| 6 | `java/util/concurrent/CountDownLatch$Sync` | 1 | | `L` | `Int(0)` | read |
| 6 | `java/util/HashMap` | 2 | `table` | `[` | `Int(0)` | read |
| 5+5+5 | `java/lang/StackTraceElement` | 1,2,3 | | `L` | `Int(0)` | read |
| 4+3 | `java/net/Socket` | 3,4 | | `L` | `Int(0)` | read |
| 3+2 | `java/util/concurrent/CompletableFuture` | 0,1 | | `L` | `Int(0)` | read |
| … | 20 further sites, each ≤ 3 | | | | | |

`ReferenceQueue` cid resolves to `java/lang/ref/ReferenceQueue cid=46 body=24
refs=2 fields=3` under `CRATONVM_DBG_LAYOUT=1`, and `javap -p
java.lang.ref.ReferenceQueue` gives `head`/`queueLength`/`lock`. Both classes
report `LEGACY, no compact layout`, which is why the 16-byte cell's decode rule
is the one that governs them. G49-1's two clusters reproduce exactly, at 742 and
243 against its 793 and 337 (its sweep was 19 vectors, this one is 16).

**None of these is a defect.** Every one is a field the JDK's own constructor
never assigns because the JVM is supposed to supply the default. `ReferenceQueue`
has no explicit `head = null`; `Properties` has no parent unless one is passed;
an unresolved `MemberName` has no `method` and no `resolution`. G49-1 settled the
first two against HotSpot with 35 differential checks and G52-1 settled the third
with `javap`. The instrument was reporting the allocator, not the readers.

### 2.2 The residue — 11 events, and this is the column that matters

Everything the fix does **not** remove:

| n | class | slot | desc | value | dir |
|---:|---|---:|---|---|---|
| 4 | `java/lang/Class` | 0 | `L` | `Int(-1)` | read |
| 2 | `javax/net/ssl/SSLContext` | 1 | `L` | `Int(1)` | **store** |
| 1 | `javax/net/ssl/SSLContext` | 1 | `L` | `Int(0)` | **store** |
| 3 | `sun/security/ssl/X509TrustManagerImpl` | 0 | `L` | `Int(1)`,`Int(2)`,`Int(3)` | **store** |
| 1 | `sun/security/pkcs12/PKCS12KeyStore` | 4 | `L` | `Int(3)` | **store** |

The four `Class` slot-0 reads are `cachedConstructor` holding the VM's own
class-mirror `Int(-1)` — the W7-84 population, a different mechanism (it BOXES;
this one NULLS), and G30-1's header says not to count them together. The seven
**stores** are the real signal: a native writing a small integer handle into a
slot a real JDK class declares as a reference, which the coercion silently turns
into `null`. That is what `gc/src/collector.rs`'s own doc reserves the `store`
column for. **After this fix the instrument's whole output is that column plus
four mirror reads**, which is the point: the census stops being a measurement of
the allocator and becomes a measurement of natives.

### 2.3 The JIT is not a second source here — MEASURED, but only on these vectors

`init_primitive_fields` is the *interpreter's* loop.
`vm/src/jit/helpers.rs::jit_init_primitive_fields` is a byte-for-byte duplicate
that this lane may not touch, so a JIT-allocated object keeps its raw-zero
reference slots. How much does that leave behind?

| vector | with JIT | `--nojit` |
|---|---:|---:|
| RSerial | 217 | 217 |
| RJdkSecurity | 47 | 47 |
| RStrings | 69 | 69 |
| RJdkNet | 239 | 218 |

Three of four are identical to the event; RJdkNet's 21-event gap is within that
vector's ordinary run-to-run variance (it moved between 218 and 243 across
uninstrumented reruns) and is not attributable to JIT allocation on this
evidence. **On these vectors the JIT contributes approximately nothing to this
population** — the classes involved are boot-path JDK library objects. That is
an observation about these vectors, not a guarantee, and NOMINATION 1 exists
because the divergence is real even where it is currently unmeasurable.

---

## 3. Does anything read the raw slot and depend on `Int(0)`?

The brief's precondition, and the only question that could have made this fix
unsafe. All four readers of a never-written reference slot, SOURCE-VERIFIED:

| reader | today (`Int(0)`) | after (`Object(None)`) | same? |
|---|---|---|---|
| interpreter `getfield` — `vm/src/runtime/interpreter/opcodes.rs`, the `field.is_reference` arm: `Value::Int(0) \| Value::Long(0) => value = Value::Object(None)` | rewrites it to null | falls through `_ => {}` | **yes** |
| JIT inline `getfield` — `jit/src/x64/bytecode_walk.rs`, `c_is_ref` arm | loads payload64 at `FIELD_CELL_PAYLOAD64_OFFSET`; never reads the tag → 0 | payload64 is still 0 | **yes** |
| descriptor-aware pair — `heap::coerce_field_value_for_slot`, `b'L' \| b'['` | `Int(_) \| Long(_)` → `Object(None)`, **and reports the loss** | `Object(None)` takes a different arm, silent | **yes**, minus the event |
| `values_equal_for_cas` — `vm/src/vm/vm_exec.rs` | `(Int(0), Object(None))` and `(Object(None), Int(0))` are **both** listed as equal | unchanged | **yes** |

The collector agrees too: `gen_heap::for_each_ref_slot`'s legacy arm matches
`Value::Object(Some(r))`, which neither shape satisfies, so neither is ever
handed to a mark or a forward.

So the fix **changes no answer anywhere**. It removes a mis-tagged intermediate
state that four separate readers were each repairing locally. G49-1 §5 recorded
that `props_defaults` "cannot stop firing the guard from inside
`properties_sidetable.rs`"; this is the reason — the repair was never the
reader's to make.

### 3.1 The two behaviours that must not move

**`HashMap.table` must still degrade to null.** Untouched: that rule lives in
`coerce_field_value_for_slot`'s `b'L' | b'['` arm, and this change writes a
`Value` rather than altering the coercion. Pinned again from this lane at the
`VmHeap` dispatch level by
`the_hashmap_table_degrade_to_null_still_holds_through_vmheap`, with the same
three values (`Int(16)`, `Int(1)`, `Long(64)`) through an array descriptor that
`gc/src/heap.rs`'s `the_hashmap_table_degrade_to_null_is_pinned` uses against the
test-only `Heap`. `HashMap.resize()` reads `(oldTab == null) ? 0 :
oldTab.length`; refusing or boxing the store breaks resize outright.

**`Reference.isEnqueued` must stay fixed.** G49-1 §4 taught it to accept both the
synthetic `Int(1)` sentinel and a live `ReferenceQueue.ENQUEUED` object. This
change writes `Object(None)` into `Reference.queue` **at allocation**; the GC's
auto-enqueue in this same file (`gc_and_alloc.rs`, two sites) writes the `Int(1)`
sentinel **later**, through the raw setter, and wins. `isEnqueued`'s `_ => false`
arm already covers `Object(None)`, which is the correct answer for a reference
that was never enqueued. Ordering pinned by
`the_enqueued_int_sentinel_outlives_the_default_null_written_at_alloc`.

One incidental improvement, worth recording because it is a behaviour change even
though it is an unobservable one: the auto-enqueue reads
`old_head = get_field(q_obj, 0)` **raw** and writes it into `Reference.next`.
Before, on a never-initialised queue that was `Int(0)` — a mis-tagged null in a
slot the class declares `L`. Now it is a properly tagged `Object(None)`. Both
mean "no next" to every reader; the new one is the one the class declares.

---

## 4. Cost

**Stated, not measured — this lane may not build, so there is no after-binary to
time.** What can be established without one:

* **The marginal work is one `VmHeap::set_field` per reference instance field.**
  Not a lock (the class-manager read guard is taken once for the whole
  hierarchy walk), not an allocation, not a hierarchy lookup — the loop already
  visits every field, static-tests it, and dereferences its descriptor `String`.
  Only the store is new.
* **It is the cheapest store this function makes.** SOURCE-VERIFIED in
  `gc/src/gen_heap.rs`: `set_field` ends with `self.write_barrier(obj_ref,
  value)`, whose first statement is `let target_ref = match stored_value {
  Value::Object(Some(r)) => r, _ => return };`. A null store returns on one tag
  test. There is no SATB pre-barrier on the generational `set_field` path at
  all. And the object body was `write_bytes`-zeroed by the allocator
  microseconds earlier, so every slot is L1-resident.
* **How many more stores?** MEASURED, static: `javap -p -s` over the **494
  classes `RJdkHello --jdk-only` loads**, non-static fields only —
  **361 primitive / 613 reference** (`L` 526, `[` 87, `I` 196, `Z` 88, `J` 33,
  `C` 21, `B` 12, `F` 9, `D` 1, `S` 1). So on the boot mix the store count rises
  by ≈1.7×. This is a per-class-declaration ratio, **not** allocation-weighted,
  and the allocation-weighted mix is more primitive-heavy than it (`String`
  1 ref / 3 prim, `Integer` 0 / 1, `AbstractStringBuilder` 1 / 2,
  `ArrayList` 1 / 2; `HashMap$Node` 3 / 1 is the counterexample). Treat 1.7× as
  an upper bound.
* **The baseline to re-run against.** MEASURED wall time, uninstrumented,
  `target-rel4`: `RMapGcStress` **29,748 ms** (378,053 checks) and
  `RMapResizeGc` **15,029 ms** (120,006 checks) — by an order of magnitude the
  two allocation-heaviest vectors in the required set, and therefore the cheap
  A/B for whoever builds next. Everything else in the set is under 5.1 s.

If that A/B shows a regression, §1's two nominations are the levers, in order:
NOMINATION 5 (stop rewriting the 318 int-family zeroes the allocator already
wrote) halves this fix's cost inside this file; NOMINATION 4 (swap the
discriminants) removes the reference writes *and* the JIT's problem together.

---

## 5. Assignment B — the raw accessors

`native-api/src/registry.rs`, `NativeHeapAccess`, two new provided methods and
one new public const. Both are **defaulted**, so the four out-of-crate
`impl NativeHeapAccess` blocks (`native-api/src/test_mock.rs`,
`native-builtins/src/atomic_updater.rs`, two in `native-api/tests/`) need no
change — the conservatism the brief asked for in the crate everything depends
on.

```rust
pub const RAW_SLOT_DESCRIPTOR: u8 = 0;

fn get_field_raw(&self, obj: ObjectRef, index: usize) -> Value {
    self.get_field_typed(obj, index, RAW_SLOT_DESCRIPTOR)
}

fn set_field_raw(&self, obj: ObjectRef, index: usize, value: Value) {
    self.set_field(obj, index, value);
}
```

### 5.1 The read half is genuinely raw today, with no override

Three facts compose, each read out of the tree:

1. `NativeContextImpl::get_field_typed` (`vm/src/vm/vm_exec.rs`) is
   `heap.get_field_as(obj, index, descriptor)` after `load_and_forward` — the
   descriptor lookup is skipped precisely because the caller supplied one.
2. `heap::get_field_as` reads the slot raw and hands it to
   `coerce_field_value_for_slot(raw, desc_byte, site)`.
3. That function is a `match desc_byte` with arms for `J`, `D`, `F`,
   `I|B|C|S|Z`, `L|[` — and a final **`_ => value`**. Every coercion-loss report
   lives *inside* one of the recognised arms.

So an unrecognised descriptor byte returns the slot verbatim **and fires no G30
event**. `0` is not a JVM field-descriptor first byte (JVMS §4.3.2 admits only
`B C D F I J S Z L [`), so it cannot collide with a real one; pinned by
`the_raw_slot_descriptor_is_not_a_jvm_field_descriptor`. Mocks that do not
override `get_field_typed` inherit this trait's default, which ignores the byte
and calls `get_field` — already raw there. It is also *cheaper* than
`get_field`: `resolve_field_descriptor_byte_cached`, which a production comment
puts at 15.3% of the profile it was measured in, is skipped entirely.

### 5.2 The store half cannot be, and its doc says so

There is no typed setter on the trait to lean on the way the read half leans on
`get_field_typed`, and the only non-coercing setter reachable from
`native-api` — `set_field_by_name` — is name-keyed, cannot be driven from a slot
index, and resolves a shadowed name to the wrong slot (G52-1 §1.4). The default
therefore delegates to `set_field`, which coerces in production. Overriding it
in `NativeContextImpl` is a one-line body; that is NOMINATION 2.

The doc comment on `set_field_raw` carries the warning in as many words,
because the half-way state is worse than either endpoint:

> **Do not pair a raw read with this default in a copy loop.** A `read` of
> `Int(0)` at an `L` slot currently answers `Object(None)` and is counted in the
> benign `read` column; a raw read followed by a coercing store hands the
> `Int(0)` to the setter and re-reports it as a **`store`** — the column
> reserved for real defects. G52-1 §1.5 measured that migration at ~24 events
> for `native_object_clone` alone.

`Object.clone()` was **not** changed — `native-builtins/src/lib.rs` is not this
lane's file, and it must not switch until both halves are raw. NOMINATION 3.

---

## 6. After-line (PREDICTED)

```
$ CRATONVM_DBG_LAYOUT=1 CRATONVM_DBG_COERCION=1 \
    cratonvm --jdk-only -cp regression-suite/build <16 vectors>
  before: 1,122 events   (1,111 never-written-reference reads + 11 residue)
  after:      11 events   (7 stores, 4 java.lang.Class mirror reads)
```

The prediction is falsifiable in one command and cheap to check. Two ways it
could come out higher, both worth knowing:

* **JIT-allocated objects are not covered** (§2.3, NOMINATION 1). If the after
  number lands between 11 and ~1,122, the remainder is the JIT's copy of the
  loop and the `skip_helper` gate, not a wrong analysis of the interpreter's.
* **A `read` of `Int(0)` at an `L` slot is not *proof* of a never-written
  slot** — a native could have stored `Int(0)` raw. No such writer was found
  (the raw `Int` writers in `gc_and_alloc.rs` write `Int(1)`, and a *coercing*
  `Int(0)` store lands as `Object(None)` and cannot be read back as `Int(0)`),
  so 1,111 is treated as the removable population. If the after number is a few
  events above 11, that is where they are.

Whoever runs it first should mark §0 MEASURED and, in the same session, time
`RMapGcStress` against the 29,748 ms in §4.

---

## 7. Vector results

MEASURED, `target-rel4`, `--jdk-only`, uninstrumented. This binary predates the
source change, so these are BEFORE-state confirmations that the required set is
green and a timing baseline — **not** an after-measurement.

| vector | flags | result | ms |
|---|---|---|---:|
| RJdkHello | | PASS (41 checks) | 429 |
| RCollections | | PASS (53) | 364 |
| RStrings | | PASS (46) | 517 |
| RMapGcStress | | PASS (378,053) | 29,748 |
| RMapResizeGc | | PASS (120,006) | 15,029 |
| RPriorityQueueGc | `--nojit --Xmx 64m` | PASS (3,609) | 690 |
| RTreeRangeGc | `--Xmx 64m` | PASS (14,014) | 664 |
| RJitGc | | PASS (1) | 1,637 |
| RForNameGcStress | | PASS (28,800) | 820 |
| RJdkSecurity | | PASS (153) | 4,396 |
| RSerial | | PASS (21) | 503 |
| RCrypto | | PASS (57) | 5,022 |

The two GC-sensitive vectors' flags are read from
`regression-suite/harness-guard.sh`'s `class_cv_args` (read-only). Every one of
the twelve also PASSed under `CRATONVM_DBG_COERCION=1`, and again under
`CRATONVM_DBG_LAYOUT=1 + CRATONVM_DBG_COERCION=1`.

## 8. Tests

`vm/src/runtime/interpreter/gc_and_alloc.rs`, new `mod
default_field_init_tests` (6). The heap fixtures pin
`GcAlgorithm::Generational` deliberately and say why: the claim under test is
about the legacy 16-byte cell's decode rule, which is a property of
`gen_heap::read_slot`; whether ZGC's and G1's own encodings answer the same way
is a real and separate question this fixture cannot ask. That is the same
reasoning `root_snapshot_screen_tests` in this file already carries.

- `every_jvm_field_descriptor_gets_its_spec_default` — the JVMS §2.3 table,
  byte for byte, including the two arms this record adds.
- `the_two_allocation_entry_points_agree_on_all_256_descriptor_bytes` — against
  `cratonvm_gc::heap::default_value_for_descriptor(b).unwrap_or(Object(None))`.
  Red the moment field defaults start depending on which allocator ran.
- `zero_memory_does_not_decode_as_null_which_is_why_the_write_exists` — **the
  pin.** Asserts the raw slot reads `Int(0)`, asserts it is *not* `Object(None)`,
  then asserts an explicit null reads back as null. If the first ever flips, the
  niche was reverted and this write became redundant — worth being told.
- `a_raw_zero_and_an_explicit_null_read_identically_through_the_descriptor` —
  the safety half: both shapes answer `Object(None)` at `L` and at `[`, so the
  delta is signal and not behaviour.
- `the_hashmap_table_degrade_to_null_still_holds_through_vmheap` — §3.1.
- `the_enqueued_int_sentinel_outlives_the_default_null_written_at_alloc` — §3.1.

`native-api/src/registry.rs`, existing `mod tests` (5), all exercising the trait
**defaults** through `MockNativeContext`, which overrides neither
`get_field_typed` nor the new pair:

- `the_raw_slot_descriptor_is_not_a_jvm_field_descriptor` — §5.1's collision
  check against all ten JVMS bytes.
- `get_field_raw_hands_back_the_stored_tag_verbatim` — seven variants, plus the
  falsifier: `Int(0)` and `Object(None)` must remain **distinguishable**, which
  is the entire capability. A reader that always answered null passes the first
  half and fails this.
- `the_raw_pair_round_trips_a_live_reference` — the `Object(Some(_))` case,
  which is where the coercing pair does real damage (G52-1 §1.6: truncation to
  `Int(ptr as i32)`, an address no collector remaps).
- `set_field_raw_writes_the_slot_the_raw_reader_reads` — the delegation lands in
  the slot the raw reader reads. Deliberately does **not** claim production
  stores raw.
- `the_raw_pair_expresses_a_verbatim_field_copy` — the two-line loop
  NOMINATION 3 is expected to become.

**Not run:** this lane may not invoke `cargo build`/`check`/`test`. Both files
parse: `rustfmt --edition 2021 --check` completed with an empty stderr on each,
which is a real parse of the whole file including the new modules.

## 9. Formatting and hygiene

`rustfmt --edition 2021 --check` in place, and against `git show HEAD:<file>` to
establish the pre-existing drift. `gc_and_alloc.rs` declares a submodule
(`mod root_snapshot_cache_tests;`), so the HEAD copy was extracted **next to a
copy of that directory** — checking the bare extract makes rustfmt bail and
report zero, which is a false baseline and would have hidden a new hunk.

| file | hunks at HEAD | hunks now |
|---|---:|---:|
| `vm/src/runtime/interpreter/gc_and_alloc.rs` (+ its submodule) | 7 | **7** |
| `native-api/src/registry.rs` | 6 | **6** |

**No new formatting hunk.** Every hunk in both files is at a line this lane did
not touch, and each of the six in `registry.rs` is the same hunk shifted by
exactly +105 lines (the size of the trait addition). Both files are **0 CR
bytes** — byte-counted — and `git ls-files --eol` reports `i/lf w/lf` for both.

---

## NOMINATIONS

**N1 — the JIT has a byte-for-byte copy of this loop, and an inline fast path
that skips field init entirely.** Three sites, one family:
`vm/src/jit/helpers.rs::jit_init_primitive_fields` (identical to the loop this
record fixed, including the `_ => None` arm);
`vm/src/jit/alloc_class_cache.rs::ClassAllocInfo::prim_inits`, a per-class recipe
that by construction lists only **primitive** field slots; and
`jit/src/x64/bytecode_walk.rs`'s `0xbb` arm, whose
`let skip_helper = !has_prim_init && !has_finalizer;` makes the inline TLAB
`new` emit **no field initialisation at all** for a class with no primitive
fields — which, on the measured mix, is a very common shape. Consequence: an
object of the same class now has null-tagged reference slots when the
interpreter allocates it and raw-zero ones when compiled code does. This lane
MEASURED that the JIT contributes ≈0 to the current population on four vectors
(§2.3), so it is not urgent, but it is a real divergence and the naive repair
(`has_prim_init` → `num_fields > 0`) costs the JIT the `skip_helper` fast path
that a 1.9s→6.0s regression note in that file says was hard-won. `helpers.rs`
now has a shared table to delegate to —
`crate::runtime::interpreter::jvm_default_for_descriptor`, `pub` and `#[inline]`
for exactly this. **Take N4 first if it is takeable; it dissolves this one.**

**N2 — `NativeContextImpl` should override `set_field_raw`.**
`vm/src/vm/vm_exec.rs`, alongside the existing `set_field`. One line:
`self.shared.mem.heap.set_field(self.shared.mem.heap.load_and_forward(obj),
index, value)` — the same body `set_field_by_name` already uses after resolving
its name, minus the resolution. Until this lands, `set_field_raw` is raw only on
the test mocks (§5.2), and **N3 must not land before it**. Overriding
`get_field_raw` there too is optional: the default is already raw and already
cheaper than `get_field`, but a direct `heap.get_field` would skip the coercion
`match` as well.

**N3 — `Object.clone()`, the two-line follow-up. Land it AFTER N2, not before.**
`native-builtins/src/lib.rs`, the `native_object_clone` copy loop: replace
`ctx.get_field(this, i)` / `ctx.set_field(clone_ref, i, val)` with
`ctx.get_field_raw` / `ctx.set_field_raw`. The JVM specifies `clone()` as a
verbatim field copy and the loop never opted out of coercion — the API moved
underneath it (G52-1 §1.4). Two measured cautions, both from G52-1: the win is
real but small (99.98% of `RJdkSecurity`'s 4,534 clones take the
`ObjectKind::Array` arm and never touch a field descriptor), and landing only the
read half migrates ~24 benign events from the instrument's `read` column into its
`store` column, which is the column reserved for real defects. Both halves, or
neither.

**N4 — the fix that is actually cheaper: swap `Value`'s `Int` and `Object`
discriminants.** `types/src/value.rs`. If `Object` were `0` and `Int` were `4`,
the all-zero cell would decode as `Value::Object(None)` and the *reference*
default would come free out of the allocator's existing `write_bytes(ptr, 0,
size)`, while the int-family default would have to be written. On the MEASURED
boot mix (361 primitive / 613 reference, of which 318 are int-family) that is
**−613 writes and +318** per one-of-each-class — a net reduction — and it
dissolves N1 outright, because the JIT's `skip_helper` gate would then be
correct for exactly the classes it currently gets wrong. This is the honest
answer to "the tag rather than the write", and it is not a one-file change: the
discriminants are baked into JIT codegen (`ir_lower.rs` emits `MOV dword [rax +
FIELD_CELL_TAG_OFFSET], 0` for `Int`; `x64/objects.rs` recognises an `Object`
cell by the literal word `4`), `ValueLayout`'s `const` assertions pin them, and
`gen_heap::read_slot`'s decode doc and `values_equal_for_cas`'s
`Object(None)`/typed-zero arms both encode the current direction. A `types/` +
`jit/` + `gc/` change, needing a build and the full suite. Recorded with the
measurement attached so the next lane does not have to derive the payoff.

**N5 — `init_primitive_fields` rewrites 318 zeroes the allocator already
wrote.** Same file, this lane's, deliberately not taken. `Value::Int(0)` is the
all-zero cell and `try_alloc_young_initialized` does `write_bytes(ptr, 0, size)`,
so the `I B C S Z` arm is a no-op on the generational young path — 318 of the
361 primitive fields in the measured mix. Skipping it would nearly halve this
fix's added store count. It requires proving the body is zero-filled on *every*
path that reaches this function: TLAB bump, the old-gen spill (which reuses
freed blocks), `try_alloc_object_full`, and the G1 and ZGC backends. Believing an
allocator-behaviour premise asserted in a distant crate is the exact mistake this
record closes, so it wants a measurement, not a reading.

**N6 — `native-builtins/src/field_read.rs`'s module doc is now stale in one
row.** Its table says an unwritten `L…;`/`[…` field answers `Int(0)` through
`get_field_by_name` and `Object(None)` through the descriptor pair, citing this
record's `_ => None` arm by name. After this fix a *default-initialised* slot
answers `Object(None)` through both. The module's advice does not change —
`ref_field` / `ref_field_is_null` are still the right readers, and
`get_field_by_name` still answers `Object(None)` for an **absent** field, so
consequence (1) ("a null test is not a null test") survives for a different
reason — but the table and the `init_primitive_fields` citation should be
updated, and its
`a_resolvable_unwritten_reference_slot_reads_as_int_zero` fixture re-read: it
plants the `Int(0)` through `MockNativeContext`, which never runs
`init_primitive_fields`, so it stays green and stays *correct about the mock*
while no longer describing production. Not this lane's file.

**N7 — the two GC auto-enqueue paths in `gc_and_alloc.rs` disagree about the
width of `ReferenceQueue.queueLength`.** This lane's file, deliberately not
changed. The post-GC path preserves the stored width
(`Long(v) => Long(v+1) | Int(v) => Int(v+1)`); the pre-GC path 3,000 lines later
does `let size = match get_field(q_obj, 1) { Int(v) => v, _ => 0 };
set_field(q_obj, 1, Value::Int(size + 1))`, which on a real JDK layout —
`queueLength : J`, correctly seeded `Long(0)` by this very function — reads 0,
discards the long, and stores an `Int`. G49-1 §5 left three unexplained
`descriptor=J` reads at this queue's slot 1 open; this is a candidate mechanism
for one direction of that. Not fixed here because it is a behaviour change to
the GC's reference protocol, the vectors that would exercise it produce **zero**
coercion events (§2), and this lane cannot build to falsify a repair. The fix is
to copy the width-preserving arm from the sibling path in the same file.

**N8 — `RAW_SLOT_DESCRIPTOR` is not in `native-api/src/lib.rs`'s re-export
list.** Reachable today as
`cratonvm_native_api::registry::RAW_SLOT_DESCRIPTOR` (`pub mod registry`), but
every neighbouring name is re-exported at the crate root and a consumer will
reasonably expect this one to be. One line in a file that is not this lane's.
