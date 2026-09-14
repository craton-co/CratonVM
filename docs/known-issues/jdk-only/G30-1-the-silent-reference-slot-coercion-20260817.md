# G30-1 — the silent reference-slot coercion, made visible without moving it

> **RECONCILED 2026-08-17 (G59-1) — this instrument has a blind spot, and it
> is not stated anywhere in this record.**
>
> The guard fires on a **descriptor MISMATCH**. A write to the WRONG SLOT whose
> value happens to fit that field's own descriptor is invisible to it, and no
> count in this document is a count of wrong writes — only of wrong-*typed*
> ones.
>
> That distinction is not theoretical. G59-1 measured one defect writing four
> wrong slots on the same line: two warned here, and two landed silently — one
> setting `URLConnection.connectTimeout` to 1 millisecond, because `Int(1)`
> into an `I` field is perfectly well typed. **A quiet log is not a clean one.**
> The warning text now says so; this record's numbers should be read the same
> way.
>
> Unaffected: the mechanism, the file and line, and the census — all of which
> are about the mismatch path and are correct for it.

**Status:** INSTRUMENTED-SOURCE / CENSUS-MEASURED / RUNTIME-POPULATION-MEASURED
/ AFTER-NOT-MEASURABLE-BY-THIS-LANE.
**Provenance:** every runtime number below is MEASURED on
`C:/craton/target-fcheck/release/cratonvm.exe`, whose `cratonvm.d` names
`C:\craton\cvm-mergecheck\...` as its source root — i.e. a **BEFORE** binary
that provably cannot contain this lane's edits. The mechanism is
SOURCE-VERIFIED: read out of the tree with file and line, not inferred from a
symptom. The census is MEASURED by script against `javap -p` on
HotSpot 25.0.3+9-LTS. Nothing here claims an "after"; this lane is forbidden
to build. Scripts: `scratchpad/g30/census2.py`.

This record continues
`G25-1-the-int-written-into-a-reference-slot-20260817.md`. It **confirms G25's
central finding**, **corrects two things G25 got wrong about where the code
runs**, roughly **doubles the census and classifies it**, and takes the one
change that can be taken safely: the coercion now reports itself.

---

## 0. The headline

| claim | G25-1 said | this record (MEASURED) |
|---|---|---|
| the coercion path | `VmHeap::set_field_as` → `gc/src/heap.rs:880` | **`Heap` (`gc/src/heap.rs`) is test-only** and is not on any live path; the live path is `gen_heap.rs:4174` → `heap::coerce_field_value_by_descriptor` (§1.1) |
| who calls the descriptor-aware accessors | (not stated) | **only the native boundary** — `NativeContextImpl` in `vm_exec.rs`, plus one JIT helper. The interpreter's own `putfield`/`getfield` never coerce (§1.2) |
| census | 271 sites, self-declared lower bound | **400 sites**, classified; the residue that is *not* this defect is named and sized (§3) |
| runtime population | not measured | **362 cross-type accesses over 6 vectors: 18 stores, 344 reads**, and 336 of the reads are one benign slot (§2) |
| W7-84 as a proxy | "different code path, no warning at all" | **confirmed and re-measured on six vectors**: every W7-84 warning in every run is `ClassId(12) index=0` (§2.2) |
| visibility | nominated | **implemented**: counted always, warned rate-limited, zero behaviour change (§4) |

| surface | before | after |
|---|---|---|
| a native writing `Int` at a reference slot | silent in release | counted + rate-limited `cratonvm::gc::guard` warning, species and direction named |
| a native writing `Object(None)` at an `I`/`Z` slot | silent in release | same, counted separately |
| a live pointer written at a primitive slot | silent in release | same, counted separately (worst species, §4.1) |
| `HashMap.table` receiving `Int(capacity)` | degrades to null | **degrades to null**, pinned by a test (§5) |
| every one of the 400 sites | its current answer | **its current answer, byte for byte** (§4.2) |

---

## 1. The mechanism, corrected twice

G25-1 §1 is right about the important part and this record does not soften it:
a primitive handed to a slot the loaded class declares `L`/`[` is **not
dropped**. The slot is written, and what is written is `null`. The rule is
deliberate, tagged `S111r29`, and load-bearing for `HashMap`.

Two things about *where that code runs* are wrong, and both matter to anyone
trying to fix it.

### 1.1 `gc/src/heap.rs`'s `Heap` is not on the live path. SOURCE-VERIFIED.

G25-1 §1 traces `NativeContextImpl::set_field` → `VmHeap::set_field_as` →
`gc/src/heap.rs:880`. The last hop does not exist. `VmHeap` has three variants
and `Heap` is not one of them (`gc/src/vm_heap.rs:222`):

```rust
macro_rules! dispatch {
    ($self:expr, $method:ident ( $($arg:expr),* ) ) => {
        match $self {
            VmHeap::Generational(h) => h.$method($($arg),*),
            VmHeap::G1(h) => h.$method($($arg),*),
            #[cfg(feature = "zgc")]
            VmHeap::Zgc(h) => h.$method($($arg),*),
        }
    };
}
```

`Heap::new` / `Heap::with_capacity` are constructed in exactly one non-test
place in the tree (`gc/src/gc.rs:1009`, itself a test fixture) and 40-odd
times inside `gc/src/heap.rs`'s own `#[cfg(test)]` module. **A fix applied to
`Heap::set_field_as` alone would change nothing a program can observe.**

What IS shared is the free function. All four implementations —
`heap.rs`, `gen_heap.rs:4174`, and `collector.rs:433`/`:439` for G1 and ZGC —
call `crate::heap::coerce_field_value_by_descriptor`. That single function is
the whole choke point, it lives in this lane's file, and it is therefore where
the instrument went (§4).

### 1.2 The descriptor coercion is a NATIVE-BOUNDARY-ONLY path. SOURCE-VERIFIED.

Every caller of `set_field_as` / `get_field_as` / their volatile and CAS
variants in `vm/` and `jit/`:

| site | what it is |
|---|---|
| `vm_exec.rs:11152`, `:11312` | `NativeContextImpl::get_field` / `set_field` |
| `vm_exec.rs:12517`, `:12550` | the volatile pair on the same trait |
| `vm_exec.rs:12753`, `:12778` | the native-facing compare-and-set |
| `vm_exec.rs:11159`, `:11175`, `:29667` | native-facing bulk/typed reads |
| `jit/helpers.rs:11097` | `set_field_as(obj, 0, Value::Int(v), b'I')` — an `Integer.value` store, never lossy |

The interpreter's own `putfield`/`getfield` and the JIT's inline field
emitters do **not** go through it. Two consequences:

* **The instrument in §4 costs the hot path nothing**, because the hot path
  never reaches the function. That is the reason it could be added at all.
* Every one of the 400 census sites is a **native**. There is no
  bytecode-authored member of this population, which is why a per-site repair
  is tractable and why the census can be complete in principle.

### 1.3 The existing detector is narrower than it looks. SOURCE-VERIFIED.

`overlay_access_is_cross_type` (`vm/src/vm/vm_exec.rs:4380`) classifies exactly
this species, in both directions, and correctly excludes `Uninitialized`. But
it is reached only when `crate::runtime::env_cache::overlay_corruption_dbg()`
is true (`CRATONVM_DBG_OVERLAY`) **and** `shadow_layout_for(class_id)` returns
a fabricated model — every report it prints carries the suffix
`(overlay layout bound to a real JDK class)`. A class this VM never modelled
produces no report even with the flag on. G25's NOMINATION 1 suggested
defaulting it ON under `--jdk-only`; §4.3 explains why that is the *second*
thing to do and not the first.

---

## 2. What the defect actually does at runtime — the first measurement

MEASURED on the BEFORE binary, `--jdk-only`,
`CRATONVM_DBG_OVERLAY=1 CRATONVM_DISABLE_DEFAULT_WATCHDOG=1`, classpath
`C:/craton/cvm-mergecheck/regression-suite/build`.

| vector | exit | `[OVERLAY] … [cross-type]` rows | W7-84 warnings |
|---|---|---|---|
| `RJdkHello` | 0 | 5 | 12 |
| `RJdkNet` | 0 | 315 | 12 |
| `RJdkAsyncChannel` | 0 | 3 | 12 |
| `RSslNullSession` | 0 | 25 | 11 |
| `RJdkCollections` | 0 | 9 | 11 |
| `RCrypto` | 1 (pre-existing) | 5 | 11 |
| **total** | | **362** | |

### 2.1 362 accesses, 18 of them stores

Aggregated over all six runs:

| direction | class | slot | real field | count |
|---|---|---|---|---|
| **store** | `javax/net/ssl/SSLSocket` | 3 | `in : InputStream` | 6 |
| **store** | `javax/net/ssl/SSLContext` | 1 | `contextSpi : SSLContextSpi` | 5 |
| **store** | `javax/net/ssl/SSLSocket` | 2 | `socketLock : Object` | 3 |
| **store** | `java/util/TreeMap$EntrySet` | 0 | `this$0 : TreeMap` | 2 |
| **store** | `java/nio/channels/AsynchronousServerSocketChannel` | 0 | `provider : AsynchronousChannelProvider` | 2 |
| read | `java/lang/ref/ReferenceQueue` | 0 | `head : Reference` | 336 |
| read | `javax/net/ssl/SSLSocket` | 4 | `out : OutputStream` | 5 |
| read | `java/util/TreeMap$EntrySet` | 0 | `this$0 : TreeMap` | 2 |
| read | `javax/net/ssl/SSLSocket` | 0 | `impl : SocketImpl` | 1 |

**Reads and stores are not the same defect and must not share a counter.**
The 336 `ReferenceQueue.head` reads are `Int(0)` raw, from a slot that was
never descriptor-initialised — the R-niche decode rule in `gc/src/heap.rs`
means an untouched slot reads back `Int(0)`, and `get_field_as(.., b'L')`
turns that into `null`, which is what `head` means when the queue is empty.
Those reads are the coercion doing its job. Their stack is always
`jdk/internal/util/ReferencedKeyMap.removeStaleReferences()`. Put them in one
bucket with the 18 stores and the stores become 5% noise; that is why the
instrument in §4 counts by `(species, direction)` and not by species alone.

Three of the five store sites are the ones G25-1 §7.1 already disclosed. **Two
are new and were in no previous record:** `TreeMap$EntrySet` slot 0 (which is
`this$0`, the outer-`TreeMap` back-reference, written under
`java/util/AbstractMap.toString()` in `RJdkCollections`) and
`AsynchronousServerSocketChannel` slot 0 (`provider`, written directly by
`RJdkAsyncChannel.acceptFutureMustNotHang`). Both vectors PASS today, so both
are latent — but `TreeMap$EntrySet#0` is written *and read back*, so it is the
one to look at first among them.

### 2.2 The W7-84 correction, re-measured and widened

G25-1 measured this on one vector. Re-measured here on six: **every
`cratonvm::gc::guard` warning in every run is `class_id=ClassId(12) index=0`**,
value `Int(-1)` / `Int(345)` / `Int(404)` / … — the VM's own class-mirror
populator writing a `ClassId` over `java.lang.Class.cachedConstructor`
(`vm/src/vm/vm_object.rs`). 11–12 log lines per run stand for at least 33
stores, because that guard is rate-limited to `n < 8 || n.is_power_of_two()`.

So a W7-84 count is a census of **one line of `vm_object.rs`** and of nothing
else. It is not a proxy for this defect, in either direction: the 400 sites in
§3 produce no W7-84 warning at all, and W7-84's one site produces no
descriptor coercion at all. That distinction is now written at the write site
itself, and pinned — see §6.

---

## 3. The census — 400 sites, classified

Method: `scratchpad/g30/census2.py`. It is built as a strict **superset** of
G25's `scratchpad/g25/census.py`, whose matcher is reproduced and whose 271
hits are never dropped (re-run here: G25's script still reports exactly 271;
the same matcher inside this script reports 275, the +4 being
`set_field_volatile`). On top of that:

* **statement joining** — multi-line `ctx.set_field(\n obj,\n IDX,\n
  Value::Int(x),\n )` calls and multi-line allocations, which G25's
  line-at-a-time matcher structurally could not see. **101 sites found only
  this way.**
* **the other direction** — `Value::Object(None)` written at a slot the real
  class declares primitive. G25 looked for primitives only. **26 sites.**
* **constant-expression slots** — `BASE + 3`, `X as usize`, and cross-module
  constants.
* **`native-collections/src`** added to the roots.
* **read sites resolved the same way**, so a write can be told whether
  anything in this tree reads that `(class, slot)` back.

### 3.1 The classification

| group | sites | meaning |
|---|---|---|
| **A** | **14** | **MEASURED-LIVE**: the store fired at runtime in §2's six-vector sweep |
| **B** | **2** | **DOCUMENTED-DAMAGE**: named in G16-1/G25-1 as having produced an observed failure |
| **C** | **10** | **DESIGNED-DEGRADE**: `HashMap.table`, the case `S111r29` exists for — null is the wanted answer |
| **E** | **112** | **LATENT, but the slot IS read back** by some native in this tree |
| **F** | **262** | **LATENT, no reader found** anywhere in the tree |
| | **400** | |

(Group D — measured live on the read side only — is empty once
`TreeMap$EntrySet#0` is counted under A, where its store puts it.)

"Latent" is an honest word and not a safe one. It means *this* corpus did not
observe it. Group F is where `ServerSocket.impl` sat until `RSslLiveSession`
was written, and `RSslLiveSession` is the vector that found it.

### 3.2 By file

| file | sites |
|---|---|
| `native-builtins/src/phases_early.rs` | 77 |
| `native-builtins/src/http2.rs` | 33 |
| `native-builtins/src/phases_late/net_channels.rs` | 30 |
| `native-builtins/src/servlet.rs` | 28 |
| `native-builtins/src/phases_late/collections.rs` | 22 |
| `native-builtins/src/phases_late/ssl_security.rs` | 22 |
| `native-builtins/src/tls.rs` | 21 |
| `native-builtins/src/phases_late/reflect_invoke.rs` | 20 |
| `native-builtins/src/t3_impl.rs` | 19 |
| `native-builtins/src/lib.rs` | 15 |
| `native-builtins/src/phases_late/concurrent.rs` | 13 |
| `native-collections/src/lib.rs` | 10 |
| 30 further files | 90 |

### 3.3 By `(class, slot)` — the ranked table, which is also the fix order

| grp | n | class | slot | real field | species |
|---|---|---|---|---|---|
| A | 6 | `javax/net/ssl/SSLContext` | 1 | `contextSpi : SSLContextSpi` | prim→ref |
| A | 5 | `javax/net/ssl/SSLSocket` | 2 | `socketLock : Object` | prim→ref |
| A | 3 | `javax/net/ssl/SSLSocket` | 3 | `in : InputStream` | prim→ref |
| B | 2 | `javax/net/ssl/SSLSocket` | 0 | `impl : SocketImpl` | prim→ref |
| C | 10 | `java/util/HashMap` | 2 | `table : HashMap$Node[]` | prim→ref |
| E | **62** | `java/util/ArrayList` | 1 | `elementData : Object[]` | prim→ref |
| E | 9 | `java/util/HashMap` | 0 | `keySet : Set` | prim→ref |
| E | 8 | `java/util/HashMap` | 1 | `values : Collection` | prim→ref |
| E | 8 | `java/lang/Thread` | 4 | `contextClassLoader : ClassLoader` | prim→ref |
| E | 7 | `java/math/RoundingMode` | 0 | `name : String` | prim→ref |
| E | 4 | `java/util/ArrayList` | 0 | `modCount : int` | **null→prim** |
| E | 2 | `java/util/GregorianCalendar` | 0,1,2 | `fields : int[]`, `isSet : boolean[]`, `stamp : int[]` | prim→ref |
| E | 2 | `java/util/HashMap$Node` | 2 | `value : V` | prim→ref |
| E | 1 each | `java/net/URL#0 protocol`, `java/net/URI#5 port`, `java/nio/ByteBuffer#0 mark`, `javax/script/SimpleBindings#0 map`, `java/nio/channels/SelectionKey#0 attachment`, `java/nio/file/attribute/FileTime#0 unit` | | | |
| F | 15 | `java/nio/channels/SocketChannel` | 0 | `closeLock : Object` | prim→ref |
| F | 12 | `java/util/concurrent/CompletableFuture` | 1 | `stack : Completion` | prim→ref |
| F | 12 | `java/lang/invoke/VarHandle` | 0 | `vform : VarForm` | prim→ref |
| F | 9 | `java/nio/channels/SocketChannel` | 3 | `interruptedTarget : Object` | prim→ref |
| F | 8 | `java/util/EnumSet` | 1 | `ordinal : int` | **null→prim** |
| F | 7 | `javax/net/ssl/SSLContext` | 0 | `provider : Provider` | prim→ref |
| F | 7 | `java/util/Optional` | 0 | `value : T` | prim→ref |
| F | 6 | `java/time/DayOfWeek` | 0 | `name : String` | prim→ref |
| F | 6 | `java/util/HashSet` | 0 | `map : HashMap` | prim→ref |
| F | 5 | `java/nio/ByteOrder` | 0 | `name : String` | prim→ref |
| F | 4 | `java/lang/RuntimePermission` | 1 | `wildcard : boolean` | **null→prim** |
| F | 4 each | `CompletableFuture#0 result`, `HexFormat#0 delimiter`, `JarEntry#2 mtime`, `JarEntry#3 atime`, `ByteArrayInputStream#0 buf`, `Socket#4 out`, `SocketChannel#4 provider` | | | |

**`java/util/ArrayList#1 elementData` is 62 sites and it is NOT the
`HashMap.table` case.** `HashMap.resize()` has an explicit
`(oldTab == null) ? 0 : oldTab.length`; `ArrayList` has no equivalent null
branch on `elementData`, so the degrade there buys nothing — it converts a
type error into a null that the JDK's own `elementData.length` would NPE on
the moment real `ArrayList` bytecode ran against one of these objects. It is
group E rather than A only because no vector in this corpus hands one to real
`ArrayList` bytecode. This is the single largest concentration in the tree and
it is the right first target of the layout-aware writer.

**`java/lang/Thread#4 contextClassLoader`** is the sleeper: eight sites null
the field `Thread.getContextClassLoader()` returns, and a null context class
loader is the classic cause of a `ServiceLoader` finding nothing. Ranked below
the SSL trio only because nothing measured it.

### 3.4 The residue, named and sized — and why most of it is NOT this defect

877 further `set_field` sites were resolved to a class but not to a
descriptor-checkable slot. G25's script discarded these silently; they are the
reason "271" felt like a lower bound. Broken down:

| residue | n | is it this defect? |
|---|---|---|
| slot ≥ the real class's field count, **on a 0-field class** (interfaces and markers: `MemorySegment` 131, `Gatherer` 37, `Spliterator` 34, `Arena` 20, `ExecutorService` 18, …) | 706 | **No.** SOURCE-VERIFIED: `resolve_field_descriptor_byte_cached` (`vm_exec.rs:4117`) walks the hierarchy and returns `None` when `field_at_index` misses, and `NativeContextImpl::set_field` then takes the descriptor-**less** `heap.set_field`. No descriptor, no coercion. |
| slot ≥ the real class's field count, on a class that **has** fields (`HashSet` 21, `SSLContext` 12, `LocalDateTime` 10, `SSLEngine` 9, `InetSocketAddress` 8, …) | 85 | **No**, by the same argument — but it is an adjacent defect worth its own record: a synthetic model *wider* than the class it is stamped with. |
| slot index not a compile-time constant | 86 | **Unknown.** These are the honest remainder. |

So the true statement is: **400 sites are in this species, 791 look like it and
are not, and 86 cannot be decided by static analysis.** The 791 are the reason
a bare grep over-counts by a factor of three.

---

## 4. What changed: the coercion now reports itself

Both edits are in `gc/src/heap.rs`, the file that owns the one shared function
(§1.1).

### 4.1 Three species, three directions, one cold counter

`coerce_field_value_by_descriptor` has arms that **normalise** (a `Double` bit
pattern meant as a long, an `Int` widened to a `Long` — no information lost)
and arms that **destroy**. The destroying arms are now named:

| species | what happens |
|---|---|
| `primitive-into-reference` | `Int`/`Long`/`Double` at an `L`/`[` slot → `null`. The G25 headline. |
| `primitive-into-reference-uncoerced` | `Float`/`ReturnAddress` at an `L`/`[` slot → **stored as-is**. See §4.4. |
| `null-into-primitive` | `Object(None)` at a primitive slot → the typed zero. This is how `create_ssl_server_socket`'s fourth write set `ServerSocket.closed = false` (G25-1 §1). |
| `pointer-into-primitive` | a live `Object(Some(o))` at a primitive slot → **its own address, as a number**. The worst of the four: not merely wrong, non-deterministic, and it publishes a heap address into a Java `int`. No site in the census produces it and none fired in the sweep; it is counted because if it ever fires, nothing else in the VM will say so. |

Each is counted against the access direction (`read` / `store` /
`unattributed`) for the reason §2.1 gives. The counter matrix is readable at
runtime:

```rust
pub fn field_coercion_loss_counts() -> [[u64; 3]; 4];
pub fn field_coercion_loss_total() -> u64;
pub fn field_coercion_loss_report() -> Option<String>;
```

`gc::heap` is `pub mod`, so those are reachable from the VM crate with no edit
to any file this lane does not own. Wiring them into `--dump-native-registry`
or a shutdown summary is NOMINATION 2.

Logging: `tracing::warn!` on target `cratonvm::gc::guard`, rate-limited
**per species** to `n < 4 || n.is_power_of_two()`. Per species rather than
globally so the 336-per-run benign read population cannot bury a rare store.
The target is the one W7-84 already uses, deliberately — it is already on in
this VM's default stderr configuration, so nobody has to learn a new filter,
and the message says in as many words that it is *not* W7-84. Volume on the
measured population is about a dozen lines per run.
`CRATONVM_DBG_COERCION=1` removes the rate limit and adds a backtrace per
occurrence; that is the flag for a lane repairing individual sites.

**No `debug_assert!` and no refusal.** `gc/src/autobox.rs`'s module note
settles this and the reasoning transfers verbatim: the population is live on
shipped paths, so a hard error converts a wrong answer into a crash, and an
assert reds the synthetic-JDK tests where a fabricated class's slot genuinely
IS the primitive it is handed. Take the diagnostic half without the behaviour
half.

### 4.2 Why this cannot change release behaviour at any of the 400 sites

1. **The returned `Value` is byte-identical in every arm.** The only
   structural change to the match is splitting `Value::Object(None) |
   Value::Uninitialized` into two arms that return the same thing, so that the
   allocator's "no value yet" tag is not reported as a violation.
   `the_g30_instrument_changes_no_answer` asserts the full table of answers —
   including the three that G25 relied on and the `Float` fall-through — and
   asserts that the provenance-carrying entry point agrees with the
   descriptor-only one.
2. **Nothing new runs on a non-lossy access.** The observation lives *inside*
   the arms that were already lossy; the fast paths (`Value::Object(_)` at an
   `L` slot, `Value::Int(_)` at an `I` slot, `Value::Long(_)` at a `J` slot)
   gained not one instruction.
3. **It is not on a hot path at all.** §1.2: the interpreter and the JIT's
   inline emitters never call this function.
4. **The counter increment is `#[cold]` and behind the loss.** A process that
   never destroys a value never touches the atomics.
5. The only observable difference is stderr, on a target that already emits.

### 4.3 Why `overlay_check_access` was *not* defaulted ON

G25's NOMINATION 1 pairs the layout-aware writer with making
`overlay_check_access`'s cross-type arm default-ON under `--jdk-only`. That is
a good idea and it is still nominated (NOMINATION 3) — but it is the second
instrument, not the first, for three measured reasons. It requires a
fabricated shadow layout, so it is blind to any class this VM never modelled
(§1.3). It prints an unbounded, un-rate-limited multi-line stack per event —
362 events over six small vectors, with 4–8 stack lines each, is over a
thousand lines of stderr for `RJdkNet` alone. And it lives in `vm_exec.rs`,
which this lane does not own. The counter in §4.1 is complete, bounded, and in
a file this lane can edit; turning the stack-dumping hunter on is the right
*next* step once the counts say which class to point it at.

### 4.4 One thing found while instrumenting, reported and not fixed

The `b'L' | b'[' ` arm nulls `Int`, `Long` and `Double` — and then falls
through `_ => value` for **`Float`**. A `Value::Float` written at a
reference-typed slot is neither refused nor nulled: it is **stored**, and a
later reader gets a `Float` where the class declares an object. That is almost
certainly an oversight (the `Double` case immediately above it exists
precisely because someone noticed the same gap), but closing it is a behaviour
change at an unknown number of sites and this lane cannot run one. It is
counted under its own species, passed through byte-identically, and nominated
(NOMINATION 6).

---

## 5. `HashMap.table` — the pin

The one thing that must not move. `java.util.HashMap.table` is declared
`[Ljava/util/HashMap$Node;` and this VM's synthetic init paths write
`Value::Int(capacity)` there (10 sites, §3.3). The `b'L' | b'['` arm turning
that into `null` is what lets `HashMap.resize()`'s
`(oldTab == null) ? 0 : oldTab.length` take its null branch; without it the
JDK's own `arraylength` aborts with `expected object reference, got int(N)`.

`the_hashmap_table_degrade_to_null_is_pinned` (`gc/src/heap.rs`) writes
`Int(16)`, `Int(1)` and `Long(64)` through the descriptor-aware setter with
descriptor `[` and asserts, for each, that the slot reads back
`Value::Object(None)` through **both** the raw `get_field` and
`get_field_as` — i.e. that `oldTab == null` is true and `arraylength` is never
attempted. It then writes a genuine array reference and asserts it survives
untouched, because a "fix" that nulled everything would pass the first half.

The comment at the arm now says the same thing in the imperative, with the
test named, because deleting that arm is the obvious wrong move and G25 had to
say so too.

The other three new tests: `each_destroyed_value_is_counted_under_its_own_species_and_direction`,
`an_uninitialized_slot_is_not_reported_as_a_loss` (the R1 zero-init contract
must stay silent or the instrument is worthless),
`the_loss_report_names_every_species_that_fired`, and
`the_descriptor_aware_setter_reports_a_store_with_provenance`. They share a
mutex because the counters are process-global and `cargo test` runs the module
in parallel; the pre-G30 `t10_9_e_*` tests need no lock because they land in
the `unattributed` column and every new assertion is on `read`/`store`.

---

## 6. `vm_object.rs` — the W7-84 site, documented and pinned

The class-mirror populator's slot-0 write is the sole source of every W7-84
warning (§2.2). It is also one `set_field_as` away from being a *second*
instance of this record's defect, and that would be silent and catastrophic:

| store | path | slot 0 afterwards |
|---|---|---|
| `set_field` (descriptor-less) — **what the code does** | `autobox::box_for_reference_slot` | an `AUTOBOX_CLASS_ID` wrapper that `get_field` un-boxes back to `Int` |
| `set_field_as(.., b'L')` | `heap::coerce_field_value_for_slot` | **`Value::Object(None)`** — the tag is gone |

`mirror_class_id` falls back to slot 0 when `class_mirrors_reverse` misses,
and that fallback is load-bearing: gating this write off was MEASURED to fail
`RJdkHello` at `System.out instanceof PrintStream`. Nulling it does the same
damage by a different route. The existing comment at the site actually points
the wrong way — it says `NativeContextImpl::set_field` "runs the value through
`set_field_as` … so it is not obviously a stable Int", which is true of a
native writing that slot and false of this write.

Corrected in place, and pinned:
`the_class_mirror_id_is_written_without_a_field_descriptor` scans the source
**above** the test module (the needles appear verbatim in the assertions, so
scanning the whole file would find them in themselves) and fails if either
mirror populator stops using the descriptor-less `set_field`, or if a
`set_field_as(mirror, 0` ever appears. It has to scan source: the change it
guards against compiles, runs, and produces a mirror that is wrong only where
the reverse map happens to miss.

---

## 7. What this lane did NOT do

* **It did not build or run its own change.** The binary at
  `C:/craton/target-fcheck/release/cratonvm.exe` is built from
  `C:\craton\cvm-mergecheck` (its `cratonvm.d` says so) and is a BEFORE for
  everything here. **The next lane's first act should be to rebuild and
  re-run §2's six vectors with and without `CRATONVM_DBG_OVERLAY`, and
  compare the new counter matrix against §2.1's table** — those 18 stores and
  344 reads are the prediction this change makes and the only way to falsify
  it.
* **It fixed no site.** Not one of the 400 changed. That is the point; a VM
  that started refusing them all at once would fail catastrophically and prove
  nothing.
* It did not touch `gen_heap.rs`, `zgc.rs`, `g1.rs`, `collector.rs`,
  `vm_heap.rs` or `vm_exec.rs` — all other lanes' files — so the three
  collectors on the live dispatch path still report `unattributed` (class and
  slot as `-1`). That is NOMINATION 1 and it is one line per call site.
* It did not decide the 86 dynamic-slot residue (§3.4).
* It did not close the `Float`-at-a-reference-slot hole (§4.4).

---

## 8. NOMINATIONS, ranked by whether the slot is actually read

**N1 — `gc/src/gen_heap.rs:4175`, `:4187` and `gc/src/collector.rs:433`,
`:439`. Give the live collectors provenance. ONE LINE EACH.** Replace
`coerce_field_value_by_descriptor(value, desc_byte)` with
`coerce_field_value_for_slot(value, desc_byte, FieldCoercionSite::store(Some(self.class_id_of(obj)), index))`
in the setters and `::read(..)` in the getters. Everything in §4 already works
today; without this the warnings say `class_id=-1 index=-1` and a reader has
to guess which of the 400 sites fired. `Heap::set_field_as` in this lane's
file is the worked example and
`the_descriptor_aware_setter_reports_a_store_with_provenance` pins the shape.
**Highest value per line of change in this record.**

**N2 — wherever `--dump-native-registry` is assembled, plus a shutdown
summary. Print `gc::heap::field_coercion_loss_report()`.** It returns `None`
when nothing fired, so it costs a clean run one comparison and prints nothing.
A count that nobody reads is not visibility.

**N3 — `vm/src/vm/vm_exec.rs`. `overlay_check_access` under `--jdk-only`.**
G25's nomination, kept, with the three caveats in §4.3: it needs a rate limit
and a shadow-layout-independent arm before it can be defaulted on, and the
counter from N1/N2 should be what tells you when to reach for it.

### The per-site repairs, in order

**N4 — group A, 14 sites, MEASURED to fire.** `javax/net/ssl/SSLContext#1
contextSpi` (6, `net_phase_e.rs` ×2 + `ssl_security.rs` + `t27_tls.rs`),
`javax/net/ssl/SSLSocket#2 socketLock` (5) and `#3 in` (3). These are G25's
§7.1 shared 6-field `SSLSocket` model and its width-2 `SSLContext` model;
neither file can move alone, which is G25's NOMINATION 3 and it stands
unchanged. Plus the two this record found: `java/util/TreeMap$EntrySet#0`
(`this$0` — written AND read back, under `AbstractMap.toString()`) and
`java/nio/channels/AsynchronousServerSocketChannel#0` (`provider`).

**N5 — `java/util/ArrayList#1 elementData`, 62 sites, the largest single
concentration in the tree.** Group E, not C: unlike `HashMap.resize()`,
`ArrayList` has no null branch on `elementData`, so the degrade converts a
type error into a null that real `ArrayList` bytecode would NPE on. This is
the shape the layout-aware writer (`net_phase_e::dp_layout` / `BbLayout`) was
invented for and the one where it pays for itself 62 times.

**N6 — `gc/src/heap.rs` (this lane's file), the `Float` fall-through, §4.4.**
Not taken here because it is a behaviour change this lane cannot run. The
instrument already counts it under
`primitive-into-reference-uncoerced`, so the next lane can find out whether
the population is empty before deciding.

**N7 — `java/lang/Thread#4 contextClassLoader`, 8 sites.** Group E. Nulling
the field `Thread.getContextClassLoader()` returns is the classic reason a
`ServiceLoader` finds nothing; ranked here only because nothing in this corpus
measured it.

**N8 — the `null-into-primitive` direction, 26 sites, entirely unexamined
before this record.** `java/util/EnumSet#1 ordinal` (8),
`java/lang/RuntimePermission#1 wildcard` (4),
`java/util/ArrayList#0 modCount` (4, group E),
`java/text/CompactNumberFormat#0 groupingUsed` (2), and singletons on
`java/net/URI#5 port`, `java/nio/ByteBuffer#0 mark`,
`java/util/GregorianCalendar#7 lenient`, `java/util/LinkedHashMap#4 size`,
`java/lang/ScopedValue#0 hash`. Every one of these silently means "0" or
"false" where the caller wrote "null". `RuntimePermission.wildcard` and
`GregorianCalendar.lenient` are the two whose false-by-default is a
semantically loaded answer.

**N9 — a record for the 85 over-wide models (§3.4).** Not this defect, but a
synthetic model that writes slots the real class does not have is a distinct
species with its own blast radius (`HashSet` 21, `SSLContext` 12,
`LocalDateTime` 10, `SSLEngine` 9, `InetSocketAddress` 8). Nobody has counted
it before.

**Carried forward unchanged from G25-1:** NOM-2 (`t27_tls.rs`
`getAcceptedIssuers`'s hand-rolled `X509Certificate` mirror), NOM-4b
(`net_phase_e.rs`'s comment still says "DROPPED by the field-layout guard"),
NOM-7 (`tls.rs:1370`'s 16384), NOM-9 (`http_url_connection.rs`, the
per-connection `HostnameVerifier`, still the front of `RSslLiveSession`).

---

## 9. Files this lane touched

* `gc/src/heap.rs` — the loss taxonomy (`FieldCoercionLoss`,
  `FieldAccessKind`, `FieldCoercionSite`), the counter matrix and its three
  public accessors, `note_field_coercion_loss`, `coerce_field_value_for_slot`
  (`coerce_field_value_by_descriptor` is now a thin wrapper over it, with
  identical behaviour), provenance on `Heap`'s four descriptor-aware
  accessors, and six unit tests:
  `the_hashmap_table_degrade_to_null_is_pinned`,
  `the_g30_instrument_changes_no_answer`,
  `each_destroyed_value_is_counted_under_its_own_species_and_direction`,
  `an_uninitialized_slot_is_not_reported_as_a_loss`,
  `the_loss_report_names_every_species_that_fired`,
  `the_descriptor_aware_setter_reports_a_store_with_provenance`.
* `vm/src/vm/vm_object.rs` — the two corrections at the class-mirror slot-0
  write (§6) and
  `the_class_mirror_id_is_written_without_a_field_descriptor`.
* `docs/known-issues/jdk-only/G30-1-…md` — this record.
* `scratchpad/g30/census2.py` — the census.

Nothing else. No `INDEX.md` / `README.md` edit; no other Rust file.
