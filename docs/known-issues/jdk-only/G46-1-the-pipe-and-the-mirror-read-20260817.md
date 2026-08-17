# G46-1 — the pipe that could not carry a byte, and the 398 mirror reads that were never a defect

**Status:** RUNTIME-MEASURED (BEFORE) / SOURCE-FIXED (`native-io/src/pipe.rs`)
/ SOURCE-EXPLAINED-AND-INSTRUMENTED (`native-builtins/src/lang_class.rs`)
/ AFTER-NOT-MEASURABLE-BY-THIS-LANE.

**Provenance.** Every runtime number below is MEASURED on
`C:/craton/target-rel3/release/cratonvm.exe`, built from
`C:\craton\cvm-mergecheck` at commit `9ae371468` — the first binary carrying
G30-1's coercion instrument, and one that provably contains none of this
lane's edits. Real layouts are `javap -p` against Eclipse Adoptium
25.0.3.9-hotspot (`java -version`: `Temurin-25.0.3+9-LTS`). Registration facts
are read out of `--dump-native-registry`, not inferred. This lane is forbidden
to build, so **there is no "after" runtime column anywhere in this record**;
§7 says exactly what that costs.

Predecessors: `G30-1-the-silent-reference-slot-coercion-20260817.md` (the
instrument, and the W7-84 write), `G38-1-the-live-reference-slot-writes-20260817.md`
(NOMINATION 3 and NOMINATION 4, which are this record's two subjects),
`W7-53-blocking-close-family.md` (the pipe's close-awareness family) and
`G39-1-the-close-family-closed-20260817.md` §2.7 (the channel oracle).

---

## 0. The headline

| claim | G38-1 said | this record (MEASURED) |
|---|---|---|
| `pipe.rs:761/763/768` | "5 events … `closeLock` and `interruptor` become null and the 'open' flag lands on `closeLock`" (§4.3) | **confirmed and worse.** The nulled `closeLock` is where the PIPE ID lives, so `Pipe` is not merely mis-flagged — it **cannot carry a byte**. `Pipe.open()` succeeds, `isOpen()` answers `true` by accident, and the first `sink.write(…)` raises `java.io.IOException: SinkChannel.write: missing pipe id` (§2) |
| the open flag | "lands on `closeLock` rather than on `closed`" | it lands on `closed`, and the polarity is **inverted**: a fresh channel is `closed = true` and `close()` sets `closed = false`. Slot 1 really is `closed : Z`, and an `Int` into a `Z` slot is not a coercion — so this half is **invisible to the instrument** and had to be read off `javap` (§2.2) |
| `lang_class.rs::mirror_class_id`, 398 events | "398 events say the fallback is not rare. G30 §6 pinned the write; nobody has looked at the read" (N4) | **378 events over 36 vectors, and every single one is `descriptor=L value=Int(-1)`** — `get_or_create_primitive_mirror`'s sentinel, on a PRIMITIVE class mirror. `mirror_class_id`'s own `v >= 0` guard rejects `-1`, so the coercion changes no answer in any measured event. **Benign, settled, A/B-confirmed** (§4) |
| — | (not stated anywhere) | but the read is **descriptor-AWARE**, so `Value::Int` can never match on a real `java.lang.Class` at all. G30 §6's claim that `Heap::get_field` "un-boxes it back to `Int`, which is what `mirror_class_id`'s fallback needs" is true of `Heap::get_field` and **false of the accessor `mirror_class_id` actually calls**. The fallback is dead in real-JDK mode, for positive ClassIds as much as for `-1` (§4.3) |
| corpus cover for `java.nio.channels.Pipe` | — | **none.** MEASURED: `grep` over all 100 `regression-suite/src/*.java` finds `Pipe.open` nowhere; the only hits are `PipedInputStream` reflection in `RJdkFieldModule` and an unrelated nested class it happens to call `Pipe`. That is why a totally broken `Pipe` sits behind a 99-green suite (§6.2) |

| surface | before (MEASURED) | after (SOURCE) |
|---|---|---|
| `Pipe.open(); sink.write(buf)` | `java.io.IOException: SinkChannel.write: missing pipe id` | the id is stored above the declared layout, where nothing coerces it |
| a fresh `Pipe.SourceChannel` seen by real JDK bytecode | `closed == true` | `closed == false`, written by name |
| `sourceChannel.close()` seen by real JDK bytecode | `closed == false` — the channel **becomes open** | `closed == true` |
| `configureBlocking(false)` | argument written into `interruptedTarget : Object`, nulled; `nonBlocking` never touched | private flag above the layout, and `nonBlocking = true` by name |
| `mirror_class_id` | `None` for every measured receiver | **`None` for every measured receiver — unchanged on purpose** (§4.4) |
| `CRATONVM_DBG_OVERLAY`'s mirror line | could not fire, in either of two independent ways | fires on the MISS too, and prints what slot 0 actually read back (§4.5) |

---

## 1. What was run

`--jdk-only`, `--java-home` Adoptium 25.0.3.9, `CRATONVM_DISABLE_DEFAULT_WATCHDOG=1`,
classpath `C:/craton/CratonVM1/regression-suite/build`, binary `target-rel3`.

* **`G46PipeProbe`** — written for this record, 15 assertions over
  `java.nio.channels.Pipe`, each of the four sections independently guarded so
  a VM that cannot finish one is still measured on the others. Its oracle is
  HotSpot 25.0.3+9 on this host, and it reproduces `G39-1` §2.7's channel rows
  for the pipe (`AsynchronousCloseException` / `ClosedByInterruptException` /
  `ClosedChannelException`), which is the oracle `W7-53`'s open Windows arm
  lacked.
* **`G46MirrorProbe`** — a controlled A/B: the identical program over
  PRIMITIVE class mirrors and over ORDINARY ones.
* **a 36-vector sweep** with `CRATONVM_DBG_COERCION=1`, attributing every
  backtrace to its first non-`gc`, non-`vm_exec`, non-`interpreter` frame.
* **`--dump-native-registry`**, for `owns_slot` / `registered_by` on all 23
  `Pipe`-family rows.
* the eight named verification vectors plus five class-mirror-adjacent ones,
  each diffed line-for-line against HotSpot (§6).

---

## 2. Site A — `native-io/src/pipe.rs::alloc_channel`

### 2.1 The layout, from `javap -p`

`Pipe.open()` allocates `sun/nio/ch/SourceChannelImpl` and
`sun/nio/ch/SinkChannelImpl`. Both are real classes in `--jdk-only`, and their
transitive instance layout (superclass first, statics excluded) is:

```text
  0 closeLock          : Ljava/lang/Object;                       AbstractInterruptibleChannel
  1 closed             : Z
  2 interruptor        : Lsun/nio/ch/Interruptible;
  3 interruptedTarget  : Ljava/lang/Object;
  4 provider           : Ljava/nio/channels/spi/SelectorProvider;  AbstractSelectableChannel
  5 keys               : [Ljava/nio/channels/SelectionKey;
  6 keyCount           : I
  7 keyLock            : Ljava/lang/Object;
  8 regLock            : Ljava/lang/Object;
  9 nonBlocking        : Z
 10 sc                 : L…SocketChannel;                          S{ource,ink}ChannelImpl
```

`alloc_channel` asked for four slots and wrote its private map at absolute
0..3. `NativeContextImpl::alloc_object` clamps the count UP to the declared
width, so the object is eleven slots wide and every one of those four writes
lands on a field the class declares.

### 2.2 The site table

`descriptor` is what `resolve_field_descriptor_byte_cached` answered, i.e. what
the loaded class says. "instrument" is the `CRATONVM_DBG_COERCION=1` reading
from **one** `Pipe.open()`.

| # | line (rel3) | class | slot | real field : descriptor | written | what happens now | instrument, before | after |
|---|---|---|---|---|---|---|---|---|
| A1 | `pipe.rs:761` | `sun/nio/ch/S{ource,ink}ChannelImpl` | 0 | `closeLock : Ljava/lang/Object;` | `Int(pipe_id)` | **nulled.** Every subsequent `read`/`write`/`close` reads `Object(None)` where it expects the id | 2 events, `primitive-into-reference`, `descriptor=L`, `value=Int(1)` / `Int(2)` | id moves to `base+0`; slot 0 is left at its class default | 
| A2 | `pipe.rs:762` | same | 1 | `closed : Z` | `Int(1)` meaning *open* | **stored, and inverted.** `Int`→`Z` is a legal normalisation, so the instrument is silent; but the JDK's meaning of a `1` here is CLOSED. `close()` then wrote `Int(0)` — the channel *becomes open* | **0 events — silent by construction** | private flag moves to `base+1`; `closed` written BY NAME, `false` fresh / `true` on close |
| A3 | `pipe.rs:763` | same | 2 | `interruptor : Lsun/nio/ch/Interruptible;` | `Int(is_sink)` | **nulled.** The sink/source discriminator is lost; `read` on a sink and `write` on a source stop being distinguishable by the field | 2 events, `descriptor=L`, `value=Int(0)` / `Int(1)` | moves to `base+2` |
| A4 | `pipe.rs:768` | same | 3 | `interruptedTarget : Ljava/lang/Object;` | `Int(1)` (blocking) | **nulled** | 2 events, `descriptor=L`, `value=Int(1)` | moves to `base+3`; `nonBlocking` written BY NAME as the negation |
| A5 | `pipe.rs:896/899` | `java/nio/channels/Pipe` | 0,1 | *(the class declares NO instance fields)* | `Object(source)` / `Object(sink)` | **correct.** No descriptor resolves, and both values are references anyway | **0 events** | unchanged, deliberately — §5.2 |

Scaling it: `G46PipeProbe` opens four pipes and produces **24 events**, exactly
3 × 2 channels × 4 opens, with no other site in the run.

### 2.3 What a program sees — the oracle table

`G46PipeProbe`, same flags. HotSpot 25.0.3+9 is the oracle column.

| row | HotSpot | CratonVM, BEFORE |
|---|---|---|
| `source.isOpen.fresh` | `true` | `true` — **agrees by accident**: the native reads its own flag, which happens to be 1 |
| `sink.isOpen.fresh` | `true` | `true` (same accident) |
| `sink.write.n` | `10` | **not reached** — `java.io.IOException: SinkChannel.write: missing pipe id` |
| `source.read.n` / `.data` | `10` / `hello-pipe` | not reached |
| `source.isOpen.afterIo` / `sink.isOpen.afterIo` | `true` / `true` | not reached |
| `source.isOpen.afterClose` / `sink.isOpen.afterClose` | `false` / `false` | not reached |
| `source.read.afterClose` | `ClosedChannelException msg=null` | `IOException msg=SourceChannel.read: missing pipe id` |
| `sink.write.afterClose` | `ClosedChannelException msg=null` | `IOException msg=SinkChannel.write: missing pipe id` |
| `source.doubleClose` | quiet | quiet — agrees |
| `blockedRead.wokenByClose` | **`AsynchronousCloseException msg=null`** | `IOException … missing pipe id` |
| `blockedRead.wokenByInterrupt` | **`ClosedByInterruptException msg=null`** | `IOException … missing pipe id` |
| `source.isOpen.afterInterrupt` | `false` | `true` |

**3 of 15 oracle rows match**, and two of the three match for the wrong
reason.

### 2.4 What this settles about `W7-53`

`W7-53` lists "the Windows arm of the pipe sink write" as one of seven open
sites and `G39-1` §2.7 supplies the oracle it lacked. This record supplies the
missing precondition: **that row is not measurable on this VM today, and the
reason is not the close-awareness machinery.** `W7-53` rows 16–19 landed the
close-aware `poll`/`ReadFile` loop in this same file, and it is correct — the
Rust-level tests `a_close_wakes_a_reader_parked_on_a_pipe` and
`a_close_aware_pipe_read_still_delivers_bytes` exercise it directly through
`pipe_read_close_aware` and pass. What no Java program can reach is the *Java*
side of it, because `SourceChannel.read` refuses before it ever parks. The
slot map is strictly upstream of the wakeup, and until it is repaired a red
`blockedRead.wokenByClose` says nothing about the wakeup at all.

After this fix those three oracle rows become *askable* for the first time.
This record does not claim they become green — the exception TYPES
(`AsynchronousCloseException` / `ClosedByInterruptException` /
`ClosedChannelException` rather than a bare `IOException`) are a separate,
un-taken repair. **NOMINATION 2.**

---

## 3. The fix — a private slot map above the declared layout

The remedy is the one this crate already uses and that
`cratonvm_native_api::appended_slots` exists for: start the private map ABOVE
every field the real class declares, and collapse the base to 0 exactly when
the class is a fabricated stub, where the private map IS the layout.

`native-io/src/lib.rs`'s `MBB_PRIVATE_*` map is the precedent, landed by
`W7-68` for the identical species on `java/nio/MappedByteBuffer`, and its
comment already states the discipline this follows: *"one base function, called
the same way by the allocator and by every accessor, is what keeps the two from
ever disagreeing."*

* `PIPE_FIELD_ID/OPEN/KIND/BLOCKING` are now documented and used as
  **relative** offsets; `PIPE_CHANNEL_PRIVATE_SLOTS` is their width.
* `alloc_channel` resolves `base` BEFORE allocating (so no GC point separates
  the decision from the writes), asks for `base + 4` slots, and writes the four
  private values at `base + i`.
* every accessor — `sink_write_buffer`, `source_read_buffer`,
  `sink_write_bytes`, `source_read_bytes`, `channel_is_open`, `channel_close`,
  `channel_configure_blocking` — resolves the same base from its receiver
  through `channel_private_base`.

### 3.1 The width guard, which is load-bearing in both directions

`channel_private_base` falls back to 0 when
`object_num_fields(this) < base + PIPE_CHANNEL_PRIVATE_SLOTS`:

* a receiver this crate did NOT allocate — a real `SourceChannelImpl` built by
  JDK bytecode, or a stub-mode object — is too narrow, so the accessor reads
  exactly the slots it read before this fix. That is the pre-existing answer,
  **never a new refusal and never an out-of-range access.** `appended_slots`'s
  own doc is explicit that a per-class base cannot be made safe against a
  foreign receiver (`W7-49` §8); the guard is how that is honoured here.
* `alloc_channel`'s class-resolution-FAILED arm allocates against
  `ClassId::new(0)`, which `NativeContextImpl::alloc_object` substitutes with
  `cratonvm/synthetic/AnonymousObject$4`. That substitute *declares* four
  fields, so a later `base_for_class` over the receiver would answer 4 and
  disagree with the 0 the allocator used — two layouts on one object, the exact
  condition `appended_slots` exists to prevent. `4 < 4 + 4` sends it back to 0.

### 3.2 Why a per-class base is sound here — MEASURED, not assumed

`--dump-native-registry` on the same binary resolves every `Pipe`-family row:

| row | owner | `owns_slot` |
|---|---|---|
| `java/nio/channels/Pipe.open/source/sink` | `native-io/src/pipe.rs:1238/1239/1245` | **true** |
| the same three | `net_channels.rs:2242/2260/2269` | false |
| `Pipe$SourceChannel.read/isOpen/close` | `pipe.rs:1254/1260/1261` | **true** |
| `Pipe$SinkChannel.write/isOpen/close` | `pipe.rs:1283/1284/1285` | **true** |
| the same six | `net_channels.rs:2281/2324/2315/2337/2377/2368` | false |
| `sun/nio/ch/S{ource,ink}ChannelImpl.*` (8 rows) | `pipe.rs:1254…1286` | **true** |
| `Pipe$SourceChannel.configureBlocking` | `net_channels.rs:2328` | **true** — and it is `\|_ctx, args\| Ok(args.first()…)`, an identity that **writes no field** |
| `Pipe$SinkChannel.configureBlocking` | `net_channels.rs:2381` | **true** — same identity |

So every receiver that reaches the private slots is one `alloc_channel`
produced, and the only rows this lane does not own touch no field. The twin's
belief about the channel layout is therefore **unobservable**, which is why
moving this file alone cannot desynchronise the two models — and why the twin
must still be repaired before it ever wins a registration (**NOMINATION 1**).

### 3.3 The two real fields that are now written, and the four that are not

`closed : Z` and `nonBlocking : Z` are written **by name**, with the JDK's
polarity: `AbstractInterruptibleChannel.isOpen()` is `return !closed` and
`AbstractSelectableChannel.isBlocking()` is `return !nonBlocking`. These are
correct values in the slots the class declares, and they are no-ops on any
layout that does not declare the names.

`closeLock`, `interruptor`, `interruptedTarget`, `provider`, `keyLock` and
`regLock` are deliberately **not** written. This is the brief's "if the right
value is genuinely unavailable, leave it and nominate" applied honestly: this
crate has no `SelectorProvider` and no `Interruptible`, and the one value that
IS constructible — a fresh `java.lang.Object` for `closeLock`, which the real
constructor makes — needs an allocation *between* the writes above, i.e. a GC
point over a receiver held in a bare local, on a path this lane cannot run.
They read back null today (the coercion already nulled `closeLock`) and they
read back null after. **NOMINATION 3.**

### 3.4 The tests

In `pipe.rs`'s existing `mod tests`, beside the `MockNativeContext` fixtures it
already uses:

* `the_private_slot_map_clears_every_declared_reference_slot` — writes the
  `javap` table down as `REAL_CHANNEL_LAYOUT` and asserts that `base + i` for
  every private `i` is past its end, that the three slots the coercion
  destroyed really are `L`/`[`, and that the fourth really is `closed : Z`.
  RED against the pre-G46 map, which was the absolute 0..3 the first assertion
  now forbids.
* `a_fresh_channel_is_not_closed_on_the_field_the_class_declares` — RED before:
  nothing wrote `closed` by name, and the indexed open flag put `Int(1)` on it.
* `closing_a_channel_sets_the_real_closed_field_true` — the
  `ServerSocket.bound` shape, from the other end. RED before: `close()` wrote
  `Int(0)` at that slot, i.e. told the JDK the channel had just become open.
* `configure_blocking_records_the_negation_on_the_real_field` — RED before, in
  two ways: the argument went to `interruptedTarget` and `nonBlocking` was
  never touched.
* `a_receiver_too_narrow_for_the_private_map_uses_the_legacy_base` — §3.1's
  guard, and that a foreign receiver still raises the pre-existing
  `IOException` rather than a new refusal or an out-of-range read.
* `every_private_pipe_slot_access_is_base_relative` — a source tripwire over
  everything above the test module, because the change this guards against
  compiles, runs, and is silent.

---

## 4. Site B — `native-builtins/src/lang_class.rs::mirror_class_id`

### 4.1 What the 398 events are: **all of them are primitive class mirrors**

MEASURED over 36 `--jdk-only` vectors, `CRATONVM_DBG_COERCION=1`:
**378 events at `lang_class.rs:1737`, and the value is `Int(-1)` in 378 of
378.** Not "mostly"; every one.

| where they come from | events |
|---|---|
| `native_class_get_name` (two `mirror_class_id` calls per `getName`) | 181 + 181 |
| `array_new_instance_component` (`lib.rs:42816`, i.e. `Array.newInstance`) | 10 |
| `lang_class.rs:19297` / `:3942` / `:3810` | 4 + 2 |

| vector | events |
|---|---|
| `RJdkJmx` | 372 |
| `RJdkRecords` | 4 |
| `RJdkReflect` | 2 |

`Int(-1)` is not a ClassId. It is the sentinel
`vm/src/vm/vm_object.rs::get_or_create_primitive_mirror` writes at slot 0, and
that function's own comment (2026-08-12) already states the consequence:

> *Primitive mirrors are NOT in `class_mirrors_reverse` (this function never
> inserts; it registers by name in `primitive_mirrors`), so `mirror_class_id`
> returns `None` for them TODAY — `Int(-1)` fails its `v >= 0` guard.*

That paragraph was never joined to the coercion census, so a number that is
entirely `int.class` / `void.class` was carried in G38-1 N4 as an unexamined
population.

### 4.2 The A/B that closes it

`G46MirrorProbe`, same binary and flags, two arms of the same program:

| arm | what it does | events at `lang_class.rs:1737` |
|---|---|---|
| `prim` | ten `getName()` over `int/long/double/float/boolean/byte/char/short/void` + `int[].class.getComponentType()`, then `Array.newInstance(int.class, 3)` | **21** = 10×2 + 1 |
| `ref` | the identical program over `String/Object/Integer/…` and `Array.newInstance(String.class, 3)` | **0** |

Both arms print output identical to HotSpot (`[I`, `[Ljava.lang.String;`, all
names correct). The events are the fallback being *asked*, and answering
correctly.

**Verdict: a benign read of a sentinel that is designed to be rejected. Not a
divergence. No value is lost, because `None` is the right answer for a
primitive mirror and `None` is what both the coerced and the uncoerced read
produce.**

### 4.3 The second finding, which nobody had recorded: the fallback is DEAD

`G30-1` §6's table reads:

| store | path | slot 0 afterwards |
|---|---|---|
| `set_field` (descriptor-less) — what the code does | `autobox::box_for_reference_slot` | an `AUTOBOX_CLASS_ID` wrapper that `get_field` un-boxes back to `Int` |

Both halves of that are true of `Heap::get_field`, and MEASURED here: a
`RReflect` run emits the W7-84 boxing warning at `class_id=ClassId(12) index=0`
with `value=Int(-1)`, `Int(404)`, `Int(443)`.

But `mirror_class_id` does not call `Heap::get_field`. It calls
`NativeContextImpl::get_field` (`vm/src/vm/vm_exec.rs:11163`), which is
**descriptor-aware**: it resolves the receiver's declared descriptor and routes
through `VmHeap::get_field_as`. `javap -p java.lang.Class` gives instance field
0 as `private volatile transient Constructor<T> cachedConstructor` — `L`. So
the collector un-boxes the wrapper back to `Int(v)` and
`heap::coerce_field_value_for_slot`'s `b'L'` arm maps that `Int` to
`Value::Object(None)` two frames later. The whole chain is in the measured
backtrace: `mirror_class_id` → `vm_exec.rs:11175` → `vm_heap.rs:973` →
`heap.rs:2150` → `note_field_coercion_loss`.

**`if let Value::Int(v) = …` therefore cannot match for any receiver whose
class is the real `java.lang.Class`, whatever the overlay holds.** The
fallback is live only where the class carries no usable descriptor — a
synthetic-stub `java/lang/Class`, and `MockNativeContext`. It is a
mode-dependent path and nothing said so.

That has a consequence for `vm_object.rs`, which is not this lane's file: the
comment there restored the slot-0 write because gating it made `RJdkHello` fail
at `System.out instanceof PrintStream`, and it attributes the need to
`mirror_class_id`'s fallback ("Do not re-gate this without first finding the
reader that needs the fallback"). **`mirror_class_id` is not that reader in
real-JDK mode**, because it cannot be. The reader that needs it is still
unidentified. **NOMINATION 4.**

### 4.4 Why the answer is left exactly as it is

The tempting repair — read the slot with an explicit descriptor so the `Int`
survives — is worse than the bug, in two independent ways:

1. **No beneficiary.** Making the fallback live would change what 157 call
   sites of `mirror_class_id` see, in a mode where it has answered `None` for
   the whole life of `--jdk-only`, for a measured beneficiary population of
   **zero**: every event carries `-1`, which the `v >= 0` guard rejects on its
   own merits.
2. **`get_field_typed(mirror, 0, b'I')` is actively dangerous.** On a mirror
   whose `cachedConstructor` holds a genuine `Constructor` — which is what that
   field is for — the `b'I'` arm takes
   `coerce_field_value_for_slot`'s `pointer-into-primitive` branch and publishes
   the object's own ADDRESS as an `Int`. That is a large positive number, it
   sails past `v >= 0`, and `mirror_class_id` hands out a fabricated ClassId.
   `G38-1` §4.1 ranks that species the worst of the four.

So: **this one is correct, and §4.1–4.3 is why.** What changed is the account
and the instrument, not the answer.

### 4.5 What did change — a guard that could not fire

The `CRATONVM_DBG_OVERLAY` report at this site sat INSIDE the `v >= 0` arm and
printed *"class-mirror slot-0 fallback HIT … the overlay in `vm_object.rs` is
load-bearing on this workload — it cannot simply be deleted."* In real-JDK mode
it could not fire, for either of two independent reasons (§4.1 and §4.3), and
its silence had already been read once as "the fallback is never used" — a
zero-hit count over 87 Spring tests is quoted in `vm_object.rs` as evidence
for exactly that. The two readings are not the same claim: "never reached" and
"can never SUCCEED" want opposite repairs.

The report now covers the MISS as well and prints what slot 0 actually read
back, naming both expected readings (`Int(-1)` = the primitive sentinel;
`Object(None)` = the descriptor-aware read nulled it, or nothing ever wrote
it). Diagnostic only, behind the same flag, and the returned value is
byte-identical.

### 4.6 The tests

In `lang_class.rs`'s existing `mod tests`, using its own `mock_ctx` import
block:

* `the_primitive_mirror_sentinel_is_rejected_by_the_slot_zero_fallback` — the
  whole measured population, in one assertion.
* `a_reference_at_slot_zero_is_never_decoded_as_a_class_id` — pins §4.4 item 2:
  a genuine `cachedConstructor` must never become a ClassId. This is the test
  that fails the moment someone closes this record with a descriptor hint.
* `a_nulled_slot_zero_overlay_answers_none_not_class_id_zero` — the real-JDK
  reading after the coercion. `None`, specifically not `ClassId(0)` =
  `java/lang/Object`, which is the aliasing behind ByteBuddy's *"Failed to
  resolve super class class java.lang.Object"* that `native_class_get_name`'s
  strict-name-first ordering already exists to avoid.
* `a_non_negative_slot_zero_overlay_is_still_that_class_id` — so the three
  refusals cannot be satisfied by a function that always answers `None`, and so
  the still-live stub/mock arm stays pinned.
* `the_mirror_slot_zero_overlay_is_read_through_the_descriptor_aware_accessor`
  — source tripwire, scanning only above the test module.

---

## 5. NOMINATIONS

**N1 — `native-builtins/src/phases_late/net_channels.rs:2242-2253`, the dead
`Pipe.open` twin. MUST MOVE WITH THIS RECORD.** It allocates
`Pipe$SourceChannel` / `Pipe$SinkChannel` with three slots and writes
`open=0, fd_id=1, blocking=2` — the identical belief this record just retired,
against the identical real layout, plus `Pipe$SourceChannel.read`/`isOpen` and
`Pipe$SinkChannel.write`/`isOpen` reading slots 0/1 the same way.
MEASURED `owns_slot=false` on all of them today, so repairing the copy changes
nothing a program can observe — which is exactly why it must be done now:
it is one registration-order change away from winning, and then it re-creates
the defect with `pipe.rs` fixed and no instrument pointing at it. Its two
`configureBlocking` rows DO own their slot and are inert identities; leaving
those is fine. **Fix it with `appended_slots::base_for_class`, the same way,
or delete the registrations.**

**N2 — the pipe's exception TYPES, `native-io/src/pipe.rs` (this lane's file,
deliberately not taken).** §2.3's oracle wants
`ClosedChannelException` after close, `AsynchronousCloseException` for a read
woken by another thread's `close()`, and `ClosedByInterruptException` for one
woken by `Thread.interrupt()` (with `isOpen()` false afterwards). CratonVM
raises a bare `java.io.IOException` with a CratonVM-authored message on all
three. This lane did not take it because it is a different defect from the slot
map, it needs the `async_close_error` helper's classifier extended rather than
a layout change, and taking two unrunnable changes in one file at once is how a
lane that cannot build produces a regression it cannot see. **It is now
askable for the first time — that is what §2.4 buys.**

**N3 — `closeLock` / `provider` on a pipe channel, and the allocation problem
behind them.** The real constructor sets `closeLock = new Object()` and
`provider = SelectorProvider.provider()`. Both read back null on every channel
this VM makes, so real `AbstractInterruptibleChannel.close()` bytecode
(`synchronized (closeLock)`) would NPE if it ever ran — it does not today only
because `close()` is registered as a native. Fixing it needs an allocation
between the field writes, i.e. GC-safe pinning of the receiver, which is a
shape this file does not currently carry. Same argument for
`interruptor`/`interruptedTarget`, except that null is the JDK's own initial
value for those two, so they are already correct.

**N4 — `vm/src/vm/vm_object.rs`, the reader that actually needs the slot-0
overlay is still unidentified.** §4.3 shows `mirror_class_id`'s fallback cannot
be it in real-JDK mode. The write was restored on 2026-08-12 because gating it
made `RJdkHello` fail at `System.out instanceof PrintStream`; that measurement
stands, but its attribution does not. Candidates the evidence points at, none
checked by this lane: the JIT's compact-reference `getfield` (which reads the
slot RAW and sees a NON-NULL `AUTOBOX` wrapper — `vm_object.rs` already
documents this as an open hazard), and `ctx_annotation_values_equal`, which
`vm_object.rs` names as a reader that never asks the reverse map. Until one of
them is named, "load-bearing" is a fact about a symptom and not about a reader.

**N5 — `regression-suite/src`, `java.nio.channels.Pipe` has NO vector.**
MEASURED: `Pipe.open` appears in none of the 100 compiled vectors.
`G46PipeProbe` (§9) is written, has a HotSpot oracle on this host, and is 15
rows; it belongs in `JDKONLY_CLASSES` once the fix is built, and it is RED on
the pre-G46 binary at 12 of 15, which is a gate that can fail. Registering it
before the build would redden the suite for every lane, so it is handed over as
source rather than scheduled.

**N6 — G30's NOMINATION 1 is still the difference between a 4-hour and a
20-minute attribution, and this record paid it again.** Every event in §2 and
§4 prints `class_id=-1 index=-1`, so the class had to be recovered from `javap`
plus the backtrace, twice. Note also that the provenance is NOT uniformly
missing: `gen_heap.rs:4178` and `collector.rs:450` already pass
`FieldCoercionSite::read(Some(class_id), index)`; the live default path
(`zgc.rs`) does not. It is one line.

---

## 6. Verification

### 6.1 The named vectors

`--jdk-only`, `target-rel3`, each run on CratonVM and on HotSpot 25.0.3+9 and
the `^(PASS|CK) ` projections diffed line-for-line. **This is the BEFORE
state** — §7.

| vector | result | note |
|---|---|---|
| `RJdkNio` | PASS | 101 checks. Does **not** touch `java.nio.channels.Pipe` (§6.2) |
| `RChannelInterrupt` | PASS | |
| `RSocketChannelInterrupt` | PASS | |
| `RJdkAsyncChannel` | PASS | |
| `RJdkHello` | PASS | the `System.out instanceof PrintStream` vector of §4.3 |
| `RJdkReflect` | PASS | 2 of the 378 mirror events |
| `RReflect` | PASS | 1 mirror event |
| `RCrypto` | PASS | |
| `RJdkJmx` | PASS | **372 of the 378 mirror events**, and green |
| `RJdkRecords` | PASS | 4 mirror events |
| `RJdkHidden` | PASS | `RJdkClass`-adjacent |
| `RJdkProxy` | PASS | `RJdkClass`-adjacent |
| `RJdkFieldModule` | PASS | `RJdkClass`-adjacent; also the only vector whose source contains the token `Pipe` |

13 of 13 PASS. There is no `RJdkClass` vector in either class list; the four
`RJdkClass`-adjacent rows above are the class-mirror/reflection surface that
`mirror_class_id` serves.

### 6.2 Why a broken `Pipe` is behind a green suite

MEASURED by `grep` over all 100 `regression-suite/src/*.java`: `Pipe.open`
occurs **nowhere**, and `java.nio.channels.Pipe` occurs nowhere. The only
`Pipe` tokens in the corpus are `PipedInputStream.PIPE_SIZE` reflection in
`RJdkFieldModule` and a nested class that file happens to name `Pipe`. The
registry agrees: `Pipe.open` has `invocations=1` in the `G46PipeProbe` run and
the eight channel methods have `invocations` 0 or 1 there — but across the
scheduled corpus nothing calls them at all.

### 6.3 `git status --short`

Restricted to this lane's paths, which is the only stable projection — the
worktree is shared with other lanes and its full status changed twice while
this record was being written:

```
$ git status --short native-io/src/pipe.rs \
                     native-builtins/src/lang_class.rs \
                     docs/known-issues/jdk-only/G46-1-the-pipe-and-the-mirror-read-20260817.md
 M native-builtins/src/lang_class.rs
 M native-io/src/pipe.rs
?? docs/known-issues/jdk-only/G46-1-the-pipe-and-the-mirror-read-20260817.md
```

The unrestricted status at the end of this lane's work was:

```
 M native-api/src/registry.rs
 M native-builtins/src/lang_class.rs
 M native-io/src/lib.rs
 M native-io/src/pipe.rs
 M vm/src/vm/vm_exec.rs
 M vm/src/vm/vm_init.rs
?? docs/known-issues/jdk-only/G46-1-the-pipe-and-the-mirror-read-20260817.md
?? docs/known-issues/jdk-only/G48-1-the-input-side-hook-and-a-gate-that-could-go-quiet-20260817.md
?? scratchpad/
```

Everything in that list other than the two Rust files above and this record is
**another lane's concurrent edit to the shared worktree**: `registry.rs`,
`native-io/src/lib.rs`, `vm_exec.rs` and `vm_init.rs` were unmodified by this
lane (`native-api/src/registry.rs` and `vm/src/vm/vm_init.rs` were already
dirty before it started; `vm/src/vm/vm_exec.rs` and `G48-1` appeared during
it). `scratchpad/` is untracked and pre-existing.

`rustfmt --edition 2021 --check` on both owned files reports **exactly the
hunks it reports on their `HEAD` revisions** — 36 lines for `pipe.rs`, 2 308
for `lang_class.rs`, byte-identical modulo the file path — so this lane
introduced no new formatting divergence and did not reformat pre-existing code.
Both files are LF-only (0 CR bytes).

---

## 7. What this lane did NOT do

* **It did not build or run its own change.** No binary contains §3 or §4.5.
  Every "after" column in this record is a claim about source, pinned by unit
  test, and the six `pipe.rs` coercion events per `Pipe.open()` are a BEFORE
  measurement whose disappearance is predicted, not observed. **The next lane's
  first act should be to rebuild and re-run `G46PipeProbe` (§9) with
  `CRATONVM_DBG_COERCION=1`: the prediction is 0 events at `pipe.rs`, and
  15 of 15 rows minus whatever N2 still owes.**
* **It changed no answer in `lang_class.rs`.** §4.4 is why, and the change
  there is a comment, a diagnostic that can now fire, and five tests.
* It did not touch `net_channels.rs` (N1), `vm_object.rs` (N4), `gc/src/heap.rs`,
  `zgc.rs`, `INDEX.md` or `README.md`.
* It did not schedule `G46PipeProbe` in `regression-suite/run.sh` (N5); the
  probe source is §9 and lives outside the tree.
* It did not identify the reader that makes the W7-84 write load-bearing. It
  established which reader it is NOT, which is the half that was falsifiable
  without a build.

---

## 8. Files this lane touched

* `native-io/src/pipe.rs` — §3 and its six tests.
* `native-builtins/src/lang_class.rs` — §4.5 and its five tests.
* `docs/known-issues/jdk-only/G46-1-…md` — this record.

Nothing else.

---

## 9. Probe sources

Both probes are self-contained, compile against a stock JDK 25, and were run on
HotSpot 25.0.3+9 for the oracle columns. They live in this lane's scratchpad
rather than in the tree — see N5 for why.

### 9.1 `G46PipeProbe.java`

15 rows over `java.nio.channels.Pipe`: fresh `isOpen`, a `write`/`read`
round-trip through a `ByteBuffer`, `isOpen` after I/O and after `close`,
`read`/`write` after close, double close, a blocking `source.read()` woken by
another thread's `close()`, a blocking `source.read()` woken by
`Thread.interrupt()`, and `isOpen` afterwards. Each of the four sections is
independently try/caught so a VM that cannot finish one is still measured on
the others — without that, the `missing pipe id` failure in section one hides
the other three, which is how this defect reads as "one broken call".

The blocked-read sections prove the read is genuinely parked before the wakeup:
the reader thread starts, the main thread sleeps 400 ms, and only then closes
or interrupts.

### 9.2 `G46MirrorProbe.java`

One program, two arms selected by `args[0]`, structurally identical: ten
`Class.getName()` calls plus one `java.lang.reflect.Array.newInstance`, over
PRIMITIVE mirrors in the `prim` arm and over ORDINARY class mirrors in the
`ref` arm. The A/B is the measurement — 21 events against 0 — and the identical
shape is what makes it one.
