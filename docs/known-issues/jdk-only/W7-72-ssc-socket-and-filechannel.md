# W7-72 — the in-bounds write of the wrong field, and the FileChannel map whose "backwards repair" was based on an inverted ordering

Status: both defects W7-66-live-over-allocations.md §6 and
W7-68-live-under-allocations.md §3.2 declined are **repaired**. Neither lane was
wrong to decline; each named its blocker precisely, and both blockers were real.
This record is what happened when the two blockers were actually paid off — a
GC-rooted side table for the first, and a cross-crate owner plus a settled
registrar ordering for the second.

> **VERIFIED AGAINST A BINARY 2026-09-02.** The banner above says "Nothing here
> was built or run as CratonVM". The ratchet this record's residual table names
> has now been run, on a build from this tree, **by name**:
>
> ```text
> cargo test -p cratonvm-native-api --test guarded_slot_maps \
>     ssc_p58_socket_stays_deleted_and_the_registrar_stays_gated
> 1 passed, 0 failed, 10 filtered out
> ```
>
> It was run by NAME rather than by file on purpose. A file that passes proves
> its tests pass; it does not prove the test a record cites still exists, because
> a renamed or deleted test leaves the file green and the citation dangling. The
> `1 passed / 10 filtered out` line is what says the name still resolves.
>
> The whole file is green too (11 passed). What this does NOT do is verify the
> record's field-layout claims: those come from `javap` against a JDK image, and
> a Rust ratchet cannot check them.

Branch `fix/ssc-socket-wrong-field-and-filechannel-isopen-20260812`.
**Nothing here was built or run as CratonVM.** Every JDK field layout is
`javap -p` against the Temurin 25.0.3+9 image on this Windows host
(`javap -version` = `25.0.3`), counted transitively over the superclass chain
with `static` excluded — the same oracle and convention as
W4-4-slot-index-species-sweep.md, W7-49-slot-index-recensus.md, W7-59-layout-detector-coverage.md,
W7-66-live-over-allocations.md and W7-68-live-under-allocations.md. Every Java
value quoted as "HotSpot says" is a transcript of a probe in this commit, run on
that image. §7 states exactly what the orchestrator's build must show.

---

## 1. Item 1 — a `ServerSocket` was living in `AbstractSelectableChannel.keys`

### 1.1 What was wrong

`native-io/src/socket_channel.rs::ssc_socket` cached the `java.net.ServerSocket`
adaptor in **slot 5 of the channel object**, under a constant whose original
comment read *"unused F_REMOTE slot"*. That comment is the whole defect in one
line: it read the `chan_fields` **side-table index map** as if it were the
object layout. `F_REMOTE` is a key into that side table.

Slot 5 of the OBJECT, on the real class:

| slot | field | declared by |
|---:|---|---|
| 0 | `closeLock` (`Ljava/lang/Object;`, final) | `AbstractInterruptibleChannel` |
| 1 | `closed` (`Z`, volatile) | `AbstractInterruptibleChannel` |
| 2 | `interruptor` (`Lsun/nio/ch/Interruptible;`) | `AbstractInterruptibleChannel` |
| 3 | `interruptedTarget` (`Ljava/lang/Object;`, volatile) | `AbstractInterruptibleChannel` |
| 4 | `provider` (`Ljava/nio/channels/spi/SelectorProvider;`, final) | `AbstractSelectableChannel` |
| **5** | **`keys` (`[Ljava/nio/channels/SelectionKey;`)** | `AbstractSelectableChannel` |
| 6 | `keyCount` (`I`) | `AbstractSelectableChannel` |
| 7 | `keyLock` (`Ljava/lang/Object;`, final) | `AbstractSelectableChannel` |
| 8 | `regLock` (`Ljava/lang/Object;`, final) | `AbstractSelectableChannel` |
| 9 | `nonBlocking` (`Z`, volatile) | `AbstractSelectableChannel` |

`SelectableChannel` and `ServerSocketChannel` declare no instance fields of their
own; `$assertionsDisabled`, `U` and `INTERRUPTED_TARGET` are static and excluded.
CratonVM gives a superclass's instance fields the low slots
(`class_manager::compute_field_layout`), so the order above is the slot order.
This reproduces W7-66's derivation exactly, re-run rather than inherited.

So `socket()` put a `java.net.ServerSocket` where the JDK expects a
`SelectionKey[]`. Real bytecode that reads `keys`:
`AbstractSelectableChannel.register`, `isRegistered`, `keyFor`, `removeKey`,
`implCloseChannel` — and `NioEndpoint`-shaped code (Tomcat) calls both `socket()`
and those on the same channel.

### 1.2 Why no instrument could find it, and why that generalises

This is an **in-bounds write of the wrong field**. The count is correct;
only the meaning is wrong. Therefore:

* `report_layout_alias` compares slot **counts**. Both the `over` and the `under`
  direction are silent here — narrowing the request from twelve to ten (W7-66
  §4.3) changed nothing about this, and could not have.
* the `cratonvm::gc::guard` out-of-bounds discriminator misses for the same
  reason: slot 5 exists.
* the descriptor coercion cannot flag it either: a `ServerSocket` reference into
  a `SelectionKey[]` slot is reference-into-reference, so
  `coerce_field_value_by_descriptor`'s `L`/`[` arm passes it through unchanged.
  (Contrast §2, where an `Int` into an `L` slot was degraded to null and the
  value was silently lost — the same class of defect with the opposite failure
  mode.)

W7-59 §6 named this species and specified a receiver-keyed read-side instrument
for it; a separate lane is building that. **This record is the one confirmed
instance, not the instrument.**

### 1.3 The repair, and the trap it had to avoid

The cache moved to an **identity-keyed side table**, `ssc_socket_cache_table`,
built on the pattern this same file already runs in the other direction
(`SsBackRef`, wrapper → channel). Four properties, each load-bearing:

1. **Keyed on the GC-stable identity hash, not on the address.** An
   address-keyed table recycles: a fresh object allocated where a collected
   channel used to live inherits the dead row. That is precisely the C27 defect
   this file's own header comment records having already been fixed once for
   `SsBackRef`, and repeating it would have been the
   `reference_corpus_fixture_must_be_static_and_side_tables_keyed_by_address_recycle`
   shape. The collector carries the hash word across a move
   (`gc/src/compact_header.rs::HashCodeTable::update_after_gc`).
2. **Collision-disambiguated by the `ObjectRef`.** Java identity hashes are not
   unique, so each bucket is a `Vec` and every lookup also matches
   `row.channel == ssc`. Probe §3 asserts the observable consequence — two
   channels get two distinct `ServerSocket`s.
3. **GC-rooted, both ends.** `gc_scan_ssc_socket_cache_roots` pushes the channel
   and the socket; `ssc_socket_cache_update_after_gc` rewrites both after a
   compaction. Wired into `vm/src/memory/native_roots.rs`'s `scan_nio` /
   `remap_nio`, next to the two hooks already there. Without the remap the
   `ObjectRef` discriminator goes stale and the next lookup silently misses —
   and worse, a later object landing at the old address matches instead.
4. **Evicted on close.** `sc_close` (which `ssc_close` delegates to) drops the
   row next to `cf_clear`. Two GC roots per row would otherwise keep a closed
   listener and its `ServerSocket` view alive for the life of the process, and
   the table would grow with a server's channel churn.

The two allocating calls in `ssc_socket` are now **pinned**:
`ServerSocketAdaptor.create` runs bytecode and `new_object` allocates, either of
which can relocate the receiver, and the row is a `(channel, socket)` pair — a
stale receiver would key it on a dead address. This was a live latent bug in the
old code too (`ss_record_back_ref(ctx, ss_value, this)` used `this` after
`new_object`); it becomes load-bearing here because the receiver is now the
table key rather than just a field target.

`SC_OBJECT_SLOTS` no longer encodes any slot map — **no native in
`socket_channel.rs` addresses a channel object slot by index any more.** Its
numeric value is unchanged (6), deliberately, so no allocation width moves in
either mode and no `CRATONVM_DBG_LAYOUT_ALIAS` row changes for these classes.

### 1.4 Does this touch Compatible mode, and is that justified

**Yes, and it is a genuine HotSpot-parity bug fix** — the exception the freeze
names. Writing a `java.net.ServerSocket` into a JDK-private `SelectionKey[]`
field is heap corruption in the §5 sense of
natives-over-real-jdk-classes.md; it is not a compatibility choice.

**The synthetic-JDK arm is behaviour-preserving, and this is worth stating
because it is the arm where the old code was fine.** `class_manager` fabricates
both channels with five instance fields, and `alloc_obj` clamps
`real.max(nfields)` with `nfields = SC_OBJECT_SLOTS = 6` — so a fabricated
channel carries six slots and slot 5 is a synthetic `_f5` that no class declares
and nothing else reads. The cache worked there and aliased nothing. The defect
was **real-JDK-mode only**, because that is the only mode in which slot 5 means
`keys`. Moving the cache to the side table keeps the synthetic arm's semantics
(one `ServerSocket` per channel, same instance every call) by a different
mechanism.

### 1.5 Left in place, on purpose

`native-builtins/src/phases_late/net_channels.rs:366` registers a second
`ServerSocketChannel.socket()` with its own object-slot map (cache in slot 3 =
`interruptedTarget`, open flag in slot 1 = `closed`, fd in slot 2 =
`interruptor`). It is the **losing** registration: `register_io_natives` runs
after `register_essential_natives_with_shims` in both real-JDK arms, so
`native-io`'s `ssc_socket` overwrites it. Shipping a fix to a losing registrar is
the inert-fix trap six lanes hit today, so it is recorded here and not touched.
It is an `under` row for those classes (allocating them at 4 and 5), which
W7-66 §11.6 already flags.

---

## 2. Item 2 — the FileChannel map, and the ordering that decides it

### 2.1 The registrar ordering, established by reading, and by what method

W7-68 §3.2 declined this repair on a specific factual claim:

> `isOpen()` is registered twice in `native-builtins/src/phases_late/nio_file.rs`,
> and the surviving copy reads slot 0 as the fd, *agreeing* with native-io's map.
> Repairing one side would make `isOpen()` return FALSE for every open channel.

**That claim is inverted.** The copy that reads slot 0 is the one that never
runs. Here is the derivation.

There are exactly two registrations of the triple
(`java/nio/channels/FileChannel`, `isOpen`, `()Z`) in the workspace, both in
`native-builtins/src/phases_late/nio_file.rs`:

| # | registrar | old body for a literal `java/nio/channels/FileChannel` receiver |
|---|---|---|
| A | `register_phase57_nio_file` | `Ok(Some(Value::Int(1)))` — a constant, reads no slot |
| B | `register_phase57_file_channel` | `fd >= 0`, reading object slot 0 |

Both are ambient `NativeKind::Bridge` (`set_category(Bridge)` at the top of each
registrar, restore at the bottom, no intervening `set_category` in either
range — checked, because the ambient kind is what re-tags a registration into
`SyntheticStub` and a `Bridge`/`Intrinsic` mix-up is how a retirement-table entry
did nothing today). Neither is dropped by any filter: the
`drop_real_layout_synthetic` arm for this class is scoped to `method_name == "open"`.

The call graph:

* `register_phase57_file_channel` has **one** caller: `register_phase57_natives`
  (nio_file.rs:36).
* `register_phase57_natives` has **one** caller: `register_synthetic_overrides`
  (native-builtins/src/lib.rs:23692), which is `#[cfg(feature = "synthetic-jdk")]`.
* `register_synthetic_overrides` has **one** caller: `register_builtins`
  (lib.rs:21220), also `#[cfg(feature = "synthetic-jdk")]`.
* `register_builtins` is called from `vm/src/vm/vm_init.rs:1839`, inside
  `if config.use_synthetic_jdk {` (line 1837), inside
  `#[cfg(feature = "synthetic-jdk")]` (line 1835).
* `register_phase57_nio_file` is called **directly** twice: vm_init.rs:2217
  (same synthetic arm, later than 1839) and vm_init.rs:2763 (inside
  `#[cfg(not(feature = "synthetic-jdk"))]`, line 2472 — the default
  `cratonvm-cli` build, which is the only build that serves `--real-jdk`).

Registration is last-write-wins, so:

| build / mode | B runs? | A runs? | winner |
|---|---|---|---|
| default build, `--real-jdk` (Compatible) and `--jdk-only` | no | yes (2763) | **A** |
| feature build, `--synthetic-jdk` | yes (via 1839) | yes, twice — inside `register_phase57_natives` **and again** at 2217, which is later | **A** |
| feature build, real-JDK arm (the `else` of 1837) | no | no | neither; real bytecode runs |

**A wins everywhere.** B is inert in every configuration.

**Method, and why it is stated.** A sibling lane doing this same work today got
it right only on the second try, because two independent scans disagreed twice.
So: attribution was done with a comment/string-aware brace tokenizer that keeps a
**stack of `fn` bodies** (not a column-0 scan, which puts a nested `fn`'s
registrations outside its enclosing function; and not indentation). It was
validated three ways before its answers were used — final brace depth exactly 0
at EOF, its line count equal to the file's, and every `fn NAME` it recorded
verified to be on the line it claims. **That validation caught a real bug in the
first version of the tool**: a backslash-newline continuation inside a Rust string
literal (`"... \` + newline) advanced the cursor past the newline without
incrementing the line counter, losing six lines in `native-io/src/lib.rs` and
attributing a statement to the *next* function. Before the fix the tool put
`native_fc_open`'s field writes inside `native_fc_read`. **Every ordering claim
in the table above was additionally confirmed by reading the cited line numbers
directly with `sed`**, which is what the claims actually rest on; the tool
narrowed where to look.

### 2.2 What was actually wrong

`java.nio.channels.FileChannel` declares no instance fields of its own; all four
come from `AbstractInterruptibleChannel` — `closeLock`(0) `closed`(1)
`interruptor`(2) `interruptedTarget`(3). The private map was
`{0: fd (Int), 1: position (Long)}`. So:

* **the fd landed in `closeLock`**, an `L` slot. `gc::coerce_field_value_by_descriptor`
  degrades an `Int` written to an `L` slot to null, so in real-JDK mode the fd
  was **never stored at all** — every accessor read back `Object(None)`, `as_int()`
  gave `None`, and the channel behaved as if it had no fd. That is the quieter
  half and it had no reader to complain.
* **the position landed in `closed`**, a `Z` slot that real JDK bytecode reads.
  `AbstractInterruptibleChannel.isOpen()` is `return !closed`; `close()` and
  `begin()` read it too. A channel whose position moved off zero reported itself
  CLOSED to any of them.

The object was never SHORT — `NativeContextImpl::alloc_object` clamps
`slots = requested.max(declared)`, which is W7-68 §1's finding and it holds. It
was mis-mapped.

**Which producers are live.** `native_fc_open` is dropped in real-JDK mode by the
`drop_real_layout_synthetic` filter (scoped to `open` by name), so it only runs
under `--synthetic-jdk`, where the class is a fabricated stub and the map is the
layout — no aliasing. The producer that is live in **Compatible** mode is the
legacy synthetic fallback inside the `FileSystemProvider.newFileChannel` shim
(`register_phase57_nio_file`), reached when the RECONCILE-WITH-REAL construction
of a genuine `sun.nio.ch.FileChannelImpl` fails. It is a fallback, so the damage
is host-dependent rather than universal — which is why W7-68 could observe the
map as "mis-mapped, live" and no suite had turned it red.

### 2.3 The repair

The map moved to the **appended-slot idiom** — above every field the class
declares — and, crucially, it moved **in one step across both crates**, which is
what W7-68 refused to do one-sidedly and was right to refuse.

New owner: `native-api/src/synthetic_file_channel.rs`. It lives in `native-api`
for the same reason `appended_slots` does: `native-io` and `native-builtins` both
own accessors, and `native-builtins` is not reachable from `native-io`. The base
comes from `appended_slots::base_for_class`, so the stub arm collapses to 0 and
the synthetic-JDK layout is byte-identical to before. The allocator width
(`alloc_slots`) and every accessor's base are derived from **one function**, which
is the property that makes them unable to disagree.

**The receiver screen moved inside the accessors, and that is load-bearing.**
Before the change the accessors were safe against a foreign receiver *by
accident*: `get_field(real_channel, 0)` returned the real `closeLock` reference
and every call site's `Value::Int(v)` match or `.as_int()` then failed into the
"not open" arm. With a base applied, slot `base + 0` on a real
`sun.nio.ch.FileChannelImpl` is one of **its** declared fields, and a write there
would be exactly the corruption being repaired. So `private_base` returns `None`
unless the receiver's class is exactly `java/nio/channels/FileChannel`, and the
accessors answer `Value::Object(None)` (writes dropped) in that case — the same
value the call sites used to get, so every foreign-receiver site is
behaviour-preserving by construction. Nineteen call sites in `native-io` and
twenty-two in `native-builtins` inherit the screen and none can forget it: convert
the idiom, not the sites.

`private_base` additionally requires the slot to exist
(`object_num_fields(this) > base + 1`) and otherwise leaves the object alone,
so a literal `FileChannel` minted elsewhere at the bare declared width is not
written past.

### 2.4 `isOpen()`, in both directions

The winning body (A) is now **one body for both receiver kinds**:

```rust
Ok(Some(Value::Int(
    if matches!(ctx.get_field_by_name(this, "closed"), Value::Int(1)) { 0 } else { 1 },
)))
```

and `close()` sets `closed` on the synthetic arm as well as the real one, in all
three `close` natives (A, B, and `native-io`'s `native_fc_close`).

Why this is safe in **both** directions, which is the property to preserve if it
is ever touched again:

* **Open → true.** Nothing writes `closed` any more except `close()`. The
  position no longer aliases it. So an ordinary open channel — including one
  whose position has moved, been read from, written to and forced — answers
  `true`. It **does not consult the fd**, so a channel minted without one cannot
  be reported closed by mistake. That is the over-correction the reverting lane
  correctly feared, and it is structurally excluded rather than merely tested.
* **Closed → false.** The old literal-class body returned a constant `1`, so
  `close(); isOpen()` reported the channel open forever. It now flips. This is a
  HotSpot-parity gain the census never asked for, found by settling the ordering.

The losing registration B was given the **same** body. That is not an inert fix
being shipped for its own sake: the two bodies used to **disagree**, and that
disagreement is what let a careful reader conclude that moving the map would
break `isOpen()`. With both answering `!closed`, the last-write-wins outcome for
this triple stops being load-bearing at all. An inert registration that
contradicts the live one is a trap for the next reader, not a saving.

### 2.5 Does this touch Compatible mode, and is that justified

**Yes — genuine bug fix, on both counts.** Writing a `Long` into a `boolean` the
JDK's own `isOpen()` reads is a parity defect, and `isOpen()` answering a
constant `true` after `close()` is another. Neither is a compatibility choice.

Synthetic-JDK mode: `base_for_class` returns 0 for the fabricated stub, so the
private slots are 0 and 1 exactly as before and `alloc_slots` is `2` exactly as
before — **byte-identical**, which is why the arm needs no second code path. The
one intentional change there is `close()` now also writing `closed`, which is a
no-op on a stub that declares no such field.

---

## 3. What changed, file by file

| file | change |
|---|---|
| `native-io/src/socket_channel.rs` | slot-5 cache → `ssc_socket_cache_table` (identity-keyed, GC-rooted, remapped, evicted on close); `ssc_socket` pins the receiver across both allocating calls; `SC_OBJECT_SLOTS` documented as a bare floor, value unchanged |
| `vm/src/memory/native_roots.rs` | `scan_nio` / `remap_nio` gain the new table's two hooks |
| `native-api/src/synthetic_file_channel.rs` | **new** — the FileChannel private map, its one owner |
| `native-api/src/lib.rs` | `pub mod synthetic_file_channel;` |
| `native-io/src/lib.rs` | `FC_FIELD_FD` / `FC_FIELD_POS` deleted; 19 sites forward to the new owner; `native_fc_open` asks for `alloc_slots`; `native_fc_close` resets the fd to `-1` and sets `closed`; the five fd matches that did not guard `v >= 0` now do, matching the sibling crate and making that sentinel meaningful; `fd_from_file_channel` takes `&mut dyn` (all three callers already had one) |
| `native-builtins/src/phases_late/nio_file.rs` | 20 literal slot-0 sites forward (22 call sites with the allocator and the close-path fd reset) to the new owner; the `newFileChannel` legacy fallback uses `alloc_slots`; both `isOpen` bodies unified to `!closed`; both `close` bodies set `closed` |
| `probes/SscSocketKeysProbe.java` | **new** — item 1's read side, through real JDK bytecode |
| `probes/FileChannelIsOpenProbe.java` | **new** — item 2's both-directions probe |

No registration was added, removed or reordered. No ambient `NativeKind` changed.
No new `CRATONVM_*` flag, so `types/src/flag_groups.rs`,
`types/tests/flag-surface.txt`, `docs/flag-tokens.md` and
`docs/config/flag-inventory.md` are untouched. `HEADER_SIZE` is 16 and no site
here does header arithmetic — every field is addressed by slot index and the
object model resolves it relative to the header.

## 4. Proving it

Both probes follow `probes/SlotIndexRecensusProbe.java`'s rule: **every read goes
through a real JDK accessor, never through the native that wrote the slot.**
Reading a slot back through its own writer is the pairing that agrees no matter
how wrong the index is.

### 4.1 `probes/SscSocketKeysProbe.java`

`register`, `isRegistered`, `keyFor` and `implCloseChannel` are
`AbstractSelectableChannel` bytecode indexing `keys` (and `keyCount`, one slot
past the clobbered one) by the JDK's own field index; CratonVM registers no
native on any of them.

**Both orders are exercised, and that is not decoration.** With the clobber in
place, `socket()`-then-`register()` hands `register` a `ServerSocket` where it
expects a `SelectionKey[]`; `register()`-then-`socket()` lets the registration
succeed and then overwrites a live key array, so it is the *later* `keyFor` and
`close` that fail. A one-order probe reports green on half a corrupt VM.

§3 is the over-correction guard for item 1: `socket()` must keep answering the
**same** `ServerSocket` (its specified contract), and two channels must get
**distinct** ones — which is what the receiver disambiguation inside the
identity-hash bucket buys, and what an identity-hash-only table would fail.

HotSpot 25.0.3+9 (measured, appended to the probe): every line `true` except
`closed.isOpen = false` and `key.validAfterClose = false`. `socket.class` is
**informational, not a red** — CratonVM answers `sun.nio.ch.ServerSocketAdaptor`
only under `CRATONVM_REAL_NET_SOCKETS`, and a bare `java.net.ServerSocket`
otherwise; that difference predates this repair.

### 4.2 `probes/FileChannelIsOpenProbe.java`

Exactly three `false` lines in the HotSpot transcript, all "after close"; every
other `isOpen` is `true`. That asymmetry **is** the assertion. A VM that answers
`true` uniformly fails §2 and §4; a VM that answers `false` uniformly fails §1
and §3. The old CratonVM body failed the first way; a naive renumber that left
`isOpen()` reading slot 0 as an fd would have failed the second way. Only a VM
that tracks the transition passes.

§3 moves the position to **1** specifically — the value a `boolean true` reads
as, and therefore the sharpest single case — then to 7 to show it is not a
one-value accident, then reads, writes and forces, asking `isOpen()` after each.

### 4.3 The one Rust test added

`native-api/src/synthetic_file_channel.rs`'s
`the_requested_width_covers_every_private_slot` asserts that the width handed to
the allocator always covers both private slots **and** that it is derived from
the same `base_for_class` call the accessors use. It guards the shape that cost a
build in `socket_channel.rs` (`F_REUSEADDR = 11` with `N_FIELDS = 11`): an index
outside the requested width is dropped on write and reads back as the zero value,
which is indistinguishable from "never set". No existing test was weakened, and
no detector was made quieter.

## 5. The honest limits

1. **Nothing was built or run as CratonVM.** The two HotSpot transcripts are the
   oracle, not evidence about this VM. §7 says what the build must show.
2. **The `socket()` cache now outlives a width condition it used to depend on.**
   The old code guarded every access with `object_num_fields(this) > 5` and
   silently did nothing when it failed. That guard passed in both modes as the
   code stood (§1.4), so removing it changes nothing today — but it means a
   future change that narrows a channel allocation can no longer disable the
   cache by accident, which is a property gained, not a behaviour changed. The
   claim that the guard passed is reasoned from `alloc_obj`'s clamp and
   `class_manager`'s fabricated width; it is not measured, and a run is what
   would settle it.
3. **The `Err(_)` arm of `native_fc_open`** allocates with `ClassId::new(0)`, whose
   class is not `java/nio/channels/FileChannel`, so the new accessors screen it
   out and its fd write is now dropped where it previously landed in slot 0. That
   arm only fires when the JDK's `FileChannel` class cannot be initialised at all,
   in which case every registration on that triple is unreachable anyway — but it
   is a behaviour change and it is not hypothetical-free.
4. **`CRATONVM_DBG_LAYOUT_ALIAS` will gain an `over` row for
   `java/nio/channels/FileChannel`** (6 requested vs 4 declared) in real-JDK mode.
   That is the appended-slot idiom's signature, exactly as W7-68 §3.1 recorded for
   `java/nio/MappedByteBuffer`, and it is expected — not a regression. The
   `SocketChannel` / `ServerSocketChannel` rows do **not** move: `SC_OBJECT_SLOTS`
   keeps its value.
5. **The accessor cost.** Each FileChannel accessor now pays one
   `is_class_synthetic_stub` plus, in real-JDK mode, one already-warm
   `ensure_class_initialized`. `appended_slots`' module doc accepts this
   deliberately ("Every accessor pays one already-warm
   `ensure_class_initialized` instead, and no call site can pick the wrong one").
   These natives are off the hot path in Compatible mode — a real
   `sun.nio.ch.FileChannelImpl` never resolves to them — but the claim is
   reasoned, not measured, and H2/MVStore is the workload that would notice.
6. **Item 1 has no Rust unit test**, only the probe. A side-table test needs a
   `NativeContext` mock with a working `identity_hash_code`, and `native-io` does
   not enable `native-api`'s `test-mock` feature. Adding that plumbing blind, with
   no build available, would be a larger risk than the coverage is worth.
7. **The losing `socket()` registration in `net_channels.rs` still writes three
   real JDK fields** (§1.5). It is inert today. If a future reorder makes it win,
   it reintroduces a worse version of the defect this record closes.

## 6. What this changes about how the census is read

W7-66 and W7-68 each declined one of these, and each named the blocker exactly
right. But **W7-68's factual premise for its decline was wrong in a way its own
reasoning could not have caught**: it read the registrar ordering from the
registration sites, and the ordering is decided somewhere else entirely — in
`vm_init.rs`'s two `#[cfg]` arms and one `if`, several thousand lines away, plus
a `#[cfg(feature)]` on the transitive caller. A "which registration wins" claim
is not a statement about the file the registrations are in.

The general rule, restated because it cost a lane a repair: **liveness of a
registrar is a property of the call graph from `vm_init`, gated by Cargo features
and by a runtime mode flag, and it must be traced to the top in every one of the
three configurations.** Two of the three arms here register neither copy.

## 7. What the orchestrator's build must show

**Item 1 — `probes/SscSocketKeysProbe.java`, run in real-JDK (Compatible) mode.**

* §1 `isRegistered.after = true`, `keyFor.sameKey = true`, `keyFor.nonNull = true`
  — `register` after `socket()`.
* §2 `keyFor.beforeSocket = true` **and** `keyFor.afterSocket = true`,
  `isRegistered.afterSocket = true`, `key.isValid = true`,
  `key.channel.isSame = true` — the live key array survives `socket()`. **This is
  the line that was impossible before the fix.**
* §1/§2 `socket.stable = true` and §3 `a==b`, `b==c` all `true` — the cache
  survived the move to the side table.
* §3 `distinctChannels = true` — the identity-hash bucket disambiguates by
  receiver.
* §4 `closed.isOpen = false`, `key.validAfterClose = false`.
* `socket.class` may read `java.net.ServerSocket` rather than
  `sun.nio.ch.ServerSocketAdaptor` unless `CRATONVM_REAL_NET_SOCKETS` is set; that
  is expected and is not a failure of this repair.
* No `SECTION THREW` line anywhere.

Also run it once more with `CRATONVM_DBG_GC_STRESS=<bytes>` set, if the harness
allows: the new table is the only GC-rooted structure this change adds, and a
stress run is what exercises the root-scan and the remap. The same eight `true`
lines must hold.

**Item 2 — `probes/FileChannelIsOpenProbe.java`, run in real-JDK (Compatible) mode.**

* §1 `isOpen.fresh`, `isOpen.afterRead` = `true`.
* §3 `isOpen.at0`, `isOpen.at1`, `isOpen.at7`, `isOpen.afterWrite`,
  `isOpen.afterForce`, `isOpen.backAt0` **all `true`** — this is the
  over-correction guard, and `isOpen.at1` is the sharpest one.
* §2 `isOpen.beforeClose = true`, `isOpen.afterClose = false`,
  `isOpen.afterCloseTwice = false`.
* §4 `isOpen.fresh = true`, `isOpen.afterPosition = true`,
  `isOpen.afterClose = false`.
* A transcript that is `true` everywhere, or `false` everywhere, is a **failure**
  even though half of each would look correct.

### 7.1 ADJUDICATED 2026-08-12 — the landed fix has NO VECTOR on the defect path

The source claims above were re-verified from the tree, and all of them hold:
`ssc_socket_cache_table` / `gc_scan_ssc_socket_cache_roots` /
`ssc_socket_cache_update_after_gc` are in `native-io/src/socket_channel.rs`
(`:5063`, `:5111`, `:5125`) and wired at `vm/src/memory/native_roots.rs:417`
and `:423`; `native-api/src/synthetic_file_channel.rs` exists; **both**
`isOpen` registrations (`nio_file.rs:5999` and `:15552`) now name the single
body `p57_fc_is_open` (`:15037`), which reads `closed` by name — so the
disagreement §2.4 says was load-bearing is gone from the source, not merely
resolved on paper. §2.1's ordering reproduces exactly:
`register_phase57_file_channel` has one caller (`nio_file.rs:36`), that has one
(`native-builtins/src/lib.rs:24015`, inside `register_synthetic_overrides`,
which is `#[cfg(feature = "synthetic-jdk")]` at `:21525`–`:21526`), and
`register_phase57_nio_file` is called directly at `vm_init.rs:2217` and
`:2763`. B is not merely inert — **it is not compiled into the default binary
at all**, which is a stronger statement than §2.1 makes.

**What does not hold is the assumption that a suite run could show any of it.**

* **Item 1 has no vector, and no near miss.** No fixture calls
  `ServerSocketChannel.socket()`. The six `.socket()` hits in
  `regression-suite/src/*.java` are all `DatagramChannel.socket()` in
  `RJdkNet.java` (`:511` and its messages), a different class and a different
  native. `RJdkNio` and `RSocketChannelInterrupt` open `ServerSocketChannel`s
  and register them with a `Selector`, so they touch `keys` — but neither ever
  calls `socket()`, which is the only thing that used to clobber it. The
  probe `probes/SscSocketKeysProbe.java` is the whole instrument, and
  `regression-suite/run.sh` names no path under `probes/` at any `SUITE=`
  value, so **no suite run, however green, discharges item 1.**
* **Item 2 has a vector on the class, and none on the defect path.**
  `RChannelInterrupt.java` DOES call `FileChannel.isOpen()`, in **both**
  polarities — `:125` asserts `true` on a freshly opened channel and `:147`/
  `:157` assert `false` after an interrupted operation closed it — which is
  precisely the asymmetry §4.2 says is the assertion. But
  `FileChannel.open(...)` yields a real `sun.nio.ch.FileChannelImpl`, and the
  private map lives only on a **literal** `java/nio/channels/FileChannel`, the
  one `private_base` screens for. The producer that is live in Compatible mode
  is the legacy synthetic fallback inside the `FileSystemProvider.newFileChannel`
  shim, reached only when the RECONCILE-WITH-REAL construction fails — which is
  exactly why §2.2 could say "no suite had turned it red". A green
  `RChannelInterrupt` is evidence that the unified `!closed` body did not break
  the real path. It is **not** evidence about the map that was moved.

**What a real vector would have to do.** Stated so the next lane does not write
another probe by accident:

1. **Item 1** — a fixture (an `R*.java` in `regression-suite/src`, registered in
   `run.sh`; a probe cannot be scheduled) that, on ONE
   `ServerSocketChannel`, exercises **both orders**: `socket()` then
   `register(sel, OP_ACCEPT)`, and `register(...)` then `socket()`. Every read
   must go through `AbstractSelectableChannel` bytecode — `isRegistered()`,
   `keyFor(sel)`, `key.isValid()`, `close()` — never through the native that
   wrote the slot. One order alone reports green on half a corrupt VM. It must
   also pin `socket()`'s stability (same instance twice) and that two channels
   get **distinct** sockets, which is the identity-hash bucket's receiver
   disambiguation and the one thing an over-correction would break.
2. **Item 2** — a fixture that gets hold of a literal
   `java/nio/channels/FileChannel`, i.e. one built by the `newFileChannel`
   legacy fallback rather than by the real provider. `FileChannel.open` on a
   default-filesystem path will not do it. The reachable shape is a
   non-default `FileSystemProvider` (the `TestFileSystem` per-prefix fixture
   family is the existing precedent) whose `newFileChannel` drives the shim.
   Then: `position(1)` — the value a `boolean true` reads as, and the sharpest
   single case — then `isOpen()`; then `position(7)`, read, write, force, each
   followed by `isOpen()`; then `close()` and `isOpen()` twice. A transcript
   that is uniformly `true` or uniformly `false` is a **failure** even though
   half of each looks correct.

Until one of those exists, this record's status is *source landed, verified by
reading, unverifiable by the suite* — not *verified*.

**Both items — the build itself.**

* `cargo test -p cratonvm-types` must stay green (no flag surface moved, so this
  is only a guard).
* `cargo test -p cratonvm-native-api` must run
  `synthetic_file_channel::tests::the_requested_width_covers_every_private_slot`.
* The vm test gate must be run in a way that actually compiles
  `vm/src/vm/tests.rs` — `cargo check --all-targets` runs no tests and the default
  `--lib` never compiles that module, which is how two green branches merged into
  a non-compiling module once already.
* `CRATONVM_DBG_LAYOUT_ALIAS=1` on any run that opens a file: a new `over` row for
  `java/nio/channels/FileChannel` at 6 vs 4 is **expected** (§5.4). No new row for
  `java/nio/channels/SocketChannel` or `ServerSocketChannel`.

### 7.2 Re-verified 2026-08-12 (second pass) — every symbol still present, and residual 7 has a sequel

Source read only; **no build, no binary, no `cargo`**. §7.1's landed-state check
reproduces symbol for symbol, with the line numbers drifted again (anchor on the
identifiers, as §7.1 already says):

| §7.1 claim | status |
|---|---|
| `ssc_socket_cache_table` / `gc_scan_ssc_socket_cache_roots` / `ssc_socket_cache_update_after_gc` in `native-io/src/socket_channel.rs` | present (`:5063`, `:5111`, `:5125`); the table is `RwLock<FxHashMap<i32, Vec<SscSocketRow>>>`, i.e. still identity-hash-keyed with the per-bucket `Vec` §1.3 item 2 requires |
| eviction on close | present — `ssc_socket_cache_clear(ctx, this)` at `:1622` |
| `native-api/src/synthetic_file_channel.rs` exists and owns the private map | present |
| both `isOpen` registrations name one body | present — `nio_file.rs:5999` and `:15552` both name `p57_fc_is_open` (`:15037`) |
| the winner still registers unconditionally | present — `socket_channel.rs:4778` |
| the ratchet | present — `ssc_p58_socket_stays_deleted_and_the_registrar_stays_gated`, `native-api/tests/guarded_slot_maps.rs:539` |

**Residual 7 — "the losing `socket()` registration in `net_channels.rs` still
writes three real JDK fields" — is discharged twice over and has a sequel.** It
was discharged as written by W7-88, which deleted the registration and measured
that it never registered at all (and corrected the count from three writes to
seven, across two classes rather than one). The sequel is that the residual's
framing — *"a losing registration in `net_channels.rs`, inert today, dangerous
if a future reorder makes it win"* — turned out to be the wrong worry for that
file. **The dangerous registrations in `net_channels.rs` are not the losing ones
waiting for a reorder; they are the ones that already win because nobody else
registers the triple.** `register_p67_async_channels`, the next function down
from the one W7-88 audited, is reached from
`register_essential_natives_with_shims` and is live in both shipping modes;
four of its triples survive `native-io`'s later pass, and two of those are
fabricated success — `AsynchronousFileChannel.force(Z)V` returns without doing
anything, and `lock()` hands back a real `java/util/concurrent/FutureTask` at a
two-slot width so `get()` parks in `awaitDone` forever. W7-88 §10 and
W7-8-fabricated-success-io-sweep.md §9 carry it.

The reading lesson, since this record's §6 is where the campaign states them:
§6 says a liveness claim must be traced to `vm_init` in every configuration, and
it is right. What §6 does not say, and residual 7 is the reason to add it, is
that **the trace has to be done per registrar, not per file** — two functions
fifty lines apart in `net_channels.rs` have entirely different call chains, one
gated behind `#[cfg(feature = "synthetic-jdk")]` and one not. A file-level
verdict is the same error as a registration-site-level one, one altitude up.

Nothing in §1–§7.1 changes. The `FileChannel` half of this record is
unaffected — `p57_fc_*` and `synthetic_file_channel` are a different class, a
different crate and a different registrar — and its "source landed, verified by
reading, unverifiable by the suite" status stands.
