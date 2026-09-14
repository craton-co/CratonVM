# W7-68 — the UNDER half of the layout census: what is actually short, and what is only mis-asked

Status: the `under` direction is measured, and the headline it was handed does
not survive the measurement. **No object in this population is short.** The base
allocator clamps, so every `under` row is a *mis-request*, and the risk it names
is a different one: the narrow number is the width of a native's PRIVATE SLOT
MAP, and the damage is that the map points INSIDE the real class's declared
fields. Of the census's 24 triples, **one** had a real-JDK-bytecode reader of a
field it aliased. That one is repaired. Three more were the census misreading a
fallback arm, four are synthetic-only, and the largest block is the collections
overlay, which is an architecture, not a defect.

Branch `fix/layout-under-allocations-20260812`. **Nothing here was built or
run** — this lane writes code and docs only. Every field count is `javap -p`
against the JDK 25.0.3.9 image on this Windows host (`javap -version` = `25.0.3`),
counted transitively over the superclass chain with `static` excluded — the same
oracle and convention as W4-4-slot-index-species-sweep.md,
W7-49-slot-index-recensus.md and W7-59-layout-detector-coverage.md. Every Java
value quoted as "HotSpot says" is a transcript of
`probes/UnderAllocationProbe.java` run on that image, not a guess.

> **STALE-ROW BANNER — 2026-08-12, later pass.** Exactly one row of this record
> is dead: **§3.2, `FileChannel`**. It is repaired (W7-72-ssc-socket-and-filechannel.md
> §2) *and* the reason it gave for refusing the repair is factually inverted. Read
> the banner on §3.2 before acting on anything in it; §4's tally row and §6 item 3
> are struck to match. §6 item 2 is closed as an instrument gap by
> W7-73-short-object-blind-spot.md §4 and measured by its §3.4 — the short-object
> census this record predicted exists and stands at **12 short of 28**. Every
> other verdict in this record was re-read on the later pass and holds, including
> §1's structural finding, which three later records depend on.

> **SOURCE-VERIFICATION BANNER — 2026-08-12, triage pass (A28). §3.5's premise
> about the fabricated widths is wrong, in the direction that makes the overlay
> look narrower than it is.**
>
> §3.5 files the collections overlay as *"an architecture, not a defect"* on the
> ground that *"the narrow map IS the model's layout"*. The map is narrow; the
> model is not. `ClassManager::synthetic_stub_fields`:
>
> | class | this record's model | the comment in `class_manager.rs` | what the arm FABRICATES | real |
> |---|---:|---:|---:|---:|
> | `java/util/ArrayList` | 2 (`AL_FIELD_DATA`=0, `AL_FIELD_SIZE`=1) | `:12300` says **2** | `:12304` **4** | 3 |
> | `java/util/HashMap` | 3 | `:12305` says **3** | `:12310` **16** | 8 |
> | `java/util/HashSet` | 3 | `:12305` says **3** | `:12310` **16** | 1 |
>
> In each row the comment sitting directly above the arm states the map's width
> and the arm fabricates something wider. This does **not** turn §3.5 into a
> defect — the overlay's writes are at 0 and 1, which are inside every one of
> these widths, and `alloc_object` clamps up, so the extra fabricated slots are
> simply never touched. What it costs is the argument's load-bearing sentence:
> the model's layout and the fabricated layout are not the same object, so
> *"the narrow map IS the model's layout"* cannot be the reason the row is safe.
> The reason it is safe is narrower and checkable — **every index the overlay
> writes is < 2, and 2 is at or below both the fabricated and the real width for
> all five classes.** Whoever takes §6 item 4 should carry that sentence instead,
> because it is the one that survives someone editing `class_manager.rs`.
>
> **This is the third instance of one shape, all in one function.** With
> `java/nio/channels/{Server,}SocketChannel` — commented `= 1 (provider)` at
> `:13472`–`:13473`, fabricated at **5** (`:13479`–`:13480`; W7-66 §6 found it,
> W7-66's own triage banner nominates the fix) — `synthetic_stub_fields` now
> carries three width comments that disagree with the arm beneath them, every one
> understating. Four separate records in this family read their fabricated widths
> out of these comments. Nomination N-4.
>
> **Verified true and not to be re-derived:** §3.2's dead-row banner holds —
> `native-api/src/synthetic_file_channel.rs::alloc_slots` exists (`:80`) and is
> the single width owner it describes. §5's crate move holds:
> `cratonvm_native_api::appended_slots::base_for_class` exists (`:83`) with
> `appended_slot_base_for_class` kept as a forwarder
> (`util_concurrent_ext.rs:1037`). §6 item 6's hazard is **still un-gated and
> still un-drifted**: `AL_FIELD_DATA = 0` / `AL_FIELD_SIZE = 1` are declared
> twice and agree (`native-io/src/lib.rs:762`–`:763`,
> `native-collections/src/lib.rs:3724`–`:3725`).
>
> **Nomination N-4 — `classloading/src/class_manager.rs`, comments only, two
> sites.** Exact old text at `:12300`:
>
> ```text
>         // Collections: ArrayList/Vector/Stack/CopyOnWriteArrayList = 2 fields (data, size)
> ```
>
> exact new text:
>
> ```text
>         // Collections: ArrayList/Vector/Stack/CopyOnWriteArrayList = 4 slots.
>         // The comment read "= 2 fields (data, size)" until 2026-08-12: 2 is the
>         // OVERLAY's map (`AL_FIELD_DATA`=0, `AL_FIELD_SIZE`=1), not this arm's
>         // width. Real `java.util.ArrayList` declares 3 transitively
>         // (`modCount` from `AbstractList`, `elementData`, `size`).
> ```
>
> Exact old text at `:12305`:
>
> ```text
>         // HashMap/HashSet/ConcurrentHashMap = 3 fields (buckets, size, capacity)
> ```
>
> exact new text:
>
> ```text
>         // HashMap/HashSet/ConcurrentHashMap = 16 slots. The comment read
>         // "= 3 fields (buckets, size, capacity)" until 2026-08-12: 3 is the
>         // overlay's map, not this arm's width. Real transitive widths are
>         // HashMap 8, HashSet 1, ConcurrentHashMap 12 — so no single number
>         // here can be "the layout", and the arm is a floor for all of them.
> ```

---

## 1. An `under` row cannot describe a short object, and the proof is two lines apart

This is the finding that reorganises the rest, so it goes first.

`NativeContextImpl::alloc_object` in `vm/src/vm/vm_exec.rs` resolves the loaded
class's `num_total_fields` into `real_fields`, calls the detector, and then:

```rust
let slots = num_fields.max(real_fields);
```

The detector and the clamp read **the same integer**. And
`layout_alias::classify` returns `None` when `declared == 0`. So a row can only
be reported `under` when `declared > 0`, and whenever `declared > 0` the clamp
has already widened the object to `declared`.

**Therefore: `direction=under` ⟹ the object came back at its full declared
width.** Not approximately, not usually — structurally. The same holds at the
second observation point: `try_alloc_concurrent_synthetic` clamps
`n = requested.max(real)` before it allocates, which is exactly why
W7-59 §3 kept the detector's call there.

Two consequences, and the second is the one to carry forward:

* **The "reads out of bounds" reading of `under` is wrong.** Every slot
  `0..requested` is inside the object; so is every slot up to `declared`. There
  is no short read to find, and intersecting this list with the
  `cratonvm::gc::guard` out-of-bounds reads — the procedure W4-4 prescribes for
  the `over` direction — cannot produce a hit from an `under` row.
* **The genuinely short objects are the ones this census CANNOT see.** If the
  class is not loaded, `real_fields` is 0, `slots = num_fields`, the object *is*
  narrow — and `classify` returns `None` for exactly that case. The unmeasured
  `declared == 0` blind spot W7-59 §9.2 inherits is not adjacent to the short-object
  question; it **is** the short-object question. Nothing in this lane closes it.

So the useful question per triple is not "is it short" but: **does the narrow
slot map point at a field the real class declares, and does any real JDK
bytecode read that field?** Both halves are needed. The first alone is the
condition; the second is what makes it a defect rather than a landmine.

### 1.1 And the GC half of the risk does not apply either

W7-49 §4 and `docs/architecture/natives-over-real-jdk-classes.md` §5 both say
that an `Int` written into a slot the class declares as a reference is *"a bogus
pointer for the collector to mark and move"*. That is **not true of any object a
native allocates**, and it is worth stating because it is load-bearing for the
severity of every row below.

`NativeContextImpl::alloc_object` computes `HEADER_SIZE + slots * SLOT_SIZE` and
allocates a **legacy** object — 16-byte tagged `Value` cells, never the compact
packed layout. `gen_heap::for_each_ref_slot`'s legacy arm is

```rust
if let Value::Object(Some(r)) = std::ptr::read(s as *const Value) { f(r.as_ptr(), slot_idx); }
```

i.e. it dispatches on the **stored tag**, not on the class's declared field
type. Only the compact arm derives its reference offsets from the class
(`compact_oop_scan` → `layout.ref_offsets`, raw 8-byte reads), and that arm is
gated on the per-object `GC_FLAG_COMPACT` header bit, which this path never
sets.

An `Int` in a declared-reference slot on a native-allocated object is therefore
a **semantic** defect — a wrong answer to whoever reads that field — and not
heap corruption. The corruption framing belongs to the `over` direction, where
the object's slot count disagrees with its header, and to compact objects, which
these are not.

**`HEADER_SIZE` is not a factor at any site this lane touched.** The base
allocator uses `cratonvm_gc::heap::HEADER_SIZE` symbolically; grepped
`native-io`, `native-builtins` and `native-collections` for hand-rolled header
arithmetic at every site in §3 and found none. Every one addresses fields by
slot index, which the object model resolves relative to the header for the
caller. W7-49 §4's finding still holds.

---

## 2. Width re-verification, and four disagreements with the census

Every class re-counted with `javap -p`, transitively. **Every declared width in
W7-59 §5.2 reproduces exactly** — `Pattern` 20, `ScheduledThreadPoolExecutor`
17, `Iocp` 14, `ZipEntry` 14, `ConcurrentHashMap` 12, `MappedByteBuffer` 13,
`ByteBuffer` 11, `DatagramChannel` 10, `ServiceLoader` 10, `TreeMap` 9,
`HashMap` 8, `IOException` 6, `LinkedBlockingQueue$Itr` 5, `File` 4,
`FileChannel` 4, `InetAddress$InetAddressHolder` 4, `ArrayList` 3,
`ZoneOffset` 3. No arithmetic disagreement anywhere.

The disagreements are about **which line the census read**, and there are four —
all the same shape, and three of them are in its top five by width.

The shape: these sites are already written correctly. They allocate
`class_num_total_fields(cid).max(N)` on the real-class arm and fall back to
`ClassId::new(0)` with a literal `N` when the class will not load. **The literal
`N` the census picked up is the fallback's, not the live arm's.**

| census row | the line it read | the live arm | verdict |
|---|---|---|---|
| `java/util/zip/ZipEntry` 6 / 14 | `zip_real_jar.rs:650`, the `Err(_)` arm | `:645` — `alloc_object(cid, real.max(6))`, then **`set_field_by_name`** for every field | **not an under-allocation** |
| `java/util/ServiceLoader` 2 / 10 | `service_loader.rs:90`, the `Err(_)` arm | `:87` — `alloc_object(cid, real.max(2))` | **not an under-allocation** |
| `java/util/concurrent/ConcurrentHashMap` 2 / 12 | `native-collections/src/lib.rs:46915`, the "no real class" arm | `:46912` — `class_num_total_fields(cid).max(_CHM_NUM_FIELDS)` | **not an under-allocation** |
| `java/nio/channels/DatagramChannel` 5 / 10 | `nio_native.rs:1594` | inside `#[cfg(feature = "synthetic-jdk")]` — **compiled out** of the default build | dead, not live |

`ZipEntry` is worth dwelling on because it is the census's fourth-widest row and
it is not merely correct, it is *exemplary*: it allocates the real width, probes
`resolve_field_index_by_class_id(cid, "xdostime")` to decide which layout it is
on, and then writes ten fields **by name**, with a comment recording the
specific defect that motivated it (*"in the real JDK layout slot 1 is
`xdostime`, not `method`. The old write of `method` to slot 1 made every native
ZipFile entry report a DOS date in 1979"*). HotSpot's answers for those getters
are in probe §5 and all five pass by construction on the real arm.

**What this says about the census as an instrument**: the flag itself would not
have made these four mistakes — it reports the count actually passed at runtime,
and on the real-class arm that count *is* `real`, so no row prints. The four are
artefacts of reading the source, which is what W7-59 §9.1 warns its own tables
are (*"a source-level upper bound on what the flag would print"*). An
`Err(_) => ClassId::new(0)` fallback beside a `real.max(N)` live arm is the
specific shape that fools a source reader, and there are more of them in the
`over` column.

This is not an isolated correction. W7-61-sslengine-layout-and-tls-blocking.md,
landed on dev the same day from the `over` side, reaches the same verdict on the
row W7-49 §7 called the biggest unrepaired live one: `javax/net/ssl/SSLEngine`
7 vs 2 is **not live** — on the Compatible path the 7-wide allocation cannot
happen and the 7-slot map has no receiver. Two lanes, opposite directions of the
same census, both finding that the LIVE column over-reports. That is the census
behaving as designed — W7-59 §5.3 chose an over-approximating walk on the
explicit ground that *"a site called LIVE that is dead costs a follow-up lane a
look, while a site called dead that is live is the failure this campaign is
about"* — but a third lane reading these tables as a bug list would be wrong
three times out of four. The tables are a **work queue**, and liveness is the
first item of work on each row, not a property already established.

### 2.1 Reproducing the population

An independent scan — all `alloc_object` / `try_alloc_object_gc_safe` / the
fifteen sibling wrappers, `#[cfg(test)]` spans excluded, class resolved by
literal argument or by the nearest preceding `ensure_class_initialized("…")` —
finds **32 `under` sites / 21 distinct triples** against the census's 35 / 24.
Agreement to ~9%; the gap is sites where the class arrives through a `const &str`
(`CHM_CLASS`) or the count through an expression this scan does not evaluate.
Both readings find the same triples. After this lane's repair the same scan
reports **30 / 20** — the two `MappedByteBuffer` sites and their triple are
gone, which is the change showing up in the instrument rather than only in the
commit message.

---

## 3. Every triple, with its verdict

Ordered by what the verdict is, not by width — because width turned out to
predict almost nothing about severity.

### 3.1 The one that reached real JDK bytecode — REPAIRED

**`java/nio/MappedByteBuffer` 12 vs 13**, `native-io/src/lib.rs`,
`alloc_mapped_byte_buffer`.

The private map was at the **absolute** indices 10 and 11. The real layout:

```text
  0 mark   1 position   2 limit   3 capacity   4 address   5 segment
  6 hb     7 offset     8 isReadOnly   9 bigEndian   10 nativeByteOrder
 11 fd     12 isSync
```

So the `MMAP_REGISTRY` id sat on `nativeByteOrder` and the writable flag sat on
`fd`, a `java.io.FileDescriptor` reference.

**Why this one is different from every other row below**: `fd` has a real reader
that CratonVM does not intercept. `MappedByteBuffer.force(int,int)` is
`public final`, and only the **no-arg** `force()` is registered
(`native-io/src/lib.rs`, `native_mbb_force`) — grepped `"force"` across
`native-builtins` and `native-io`; the two-arg form is registered nowhere. Its
JDK body short-circuits on `fd == null` before it reaches `MappedMemoryUtils`,
and an `Int` in that slot is not null, so the guard does not fire. HotSpot
25.0.3.9 answers `force(0,8)` by returning the buffer (probe §3).

Repaired onto the appended-slot idiom from W7-49 §8: the private map now starts
at `base + 0`, where `base` is the class's transitive declared width, and
collapses to 0 when the class is a fabricated stub. **Touches Compatible mode —
genuine bug fix**; the synthetic-JDK arm is byte-identical, because a stub's
base is 0 and the two offsets are 0 and 1 exactly as before.

Safe to move in one step because the map has a **single owner**: grepped
`"java/nio/MappedByteBuffer"` across `native-builtins`, `native-collections` and
`vm`; the only other mentions register `session()`/`checkSession()` over a
buffer-class list and name the class in a `toString` test. Neither touches slots
10 or 11. That is the check §3.2 fails.

**How it fails.** Two ways, and neither is an assertion about itself. First, the
detector: before the change this site is one of the `under` rows
`CRATONVM_DBG_LAYOUT_ALIAS=1` prints (`class=java/nio/MappedByteBuffer,
requested_fields=12, real_fields=13, direction="under"`); after it the row
changes to `over` at `13+2` — see §5, this is expected and is the idiom's
signature, not a regression. Second, `probes/UnderAllocationProbe.java` §3
drives a mapped buffer through `force()`, `force(0,8)`, `isLoaded()`,
`isReadOnly()` and `get(0)`, printing values rather than asserting them, so the
CratonVM-versus-HotSpot transcripts diff directly.

### 3.2 Mis-mapped, live, and NOT repaired — with the reason — **DEAD ROW, 2026-08-12: REPAIRED, and the reason was inverted**

> **This is the one row of this record that is dead on the shipping path, and it
> is dead twice over. Do not re-derive it and do not re-fix it.**
>
> **The repair landed.** W7-72-ssc-socket-and-filechannel.md §2 moved the private
> map to the appended-slot idiom, **in one step across both crates** — which is
> precisely what this section refused to do one-sidedly, and was right to refuse.
> The new owner is `native-api/src/synthetic_file_channel.rs`; the allocator
> width and every accessor base come from one function, 19 forwarding call sites
> in `native-io` and 22 in `native-builtins`, with the foreign-receiver screen
> moved **inside** the accessors so no call site can forget it.
>
> **The factual premise below is inverted.** This section says `isOpen()` is
> registered twice in `phases_late/nio_file.rs` and *"the one that reads a field
> reads slot 0 as the fd, agreeing with `native-io`'s map"* — and concludes that
> renumbering one side would make `isOpen()` return FALSE for every open channel.
> The copy that reads slot 0 (`register_phase57_file_channel`) is the one that
> **never runs**: its whole transitive caller chain up to `vm_init.rs` is
> `#[cfg(feature = "synthetic-jdk")]`, and even inside that build the
> constant-returning copy in `register_phase57_nio_file` is registered again,
> later, and wins. It is inert in all four configurations. W7-72 §2.1 traces the
> chain line by line and states the general rule this section broke: **liveness
> of a registrar is a property of the call graph from `vm_init`, gated by Cargo
> features and by a runtime mode flag — not a property of the file the
> registrations are in.**
>
> **What the repair found that this section did not.** The fd landing in
> `closeLock`, an `L` slot, meant `gc::coerce_field_value_by_descriptor` degraded
> the `Int` to null and the fd was **never stored at all** in real-JDK mode. And
> the winning `isOpen()` body was a constant `1`, so `close(); isOpen()` reported
> the channel open forever. Both are HotSpot-parity gains this census never asked
> for.
>
> **Census consequence.** The site's requested width is now
> `synthetic_file_channel::alloc_slots` = `base_for_class(…) + 2`, so its
> `ClassId::new(0)` row can no longer be short in any execution — see
> W7-74-short-object-repairs.md §1.3 for the argument and
> W7-73-short-object-blind-spot.md §3.4 for the re-derived table. Expect a new
> `over` row for `java/nio/channels/FileChannel` at 6 vs 4 under
> `CRATONVM_DBG_LAYOUT_ALIAS=1`; that is the idiom's signature, not a regression.

**`java/nio/channels/FileChannel` 2 vs 4**, `native-io/src/lib.rs`,
`native_fc_open` plus 14 accessor sites.

Real layout, all four inherited from
`java.nio.channels.spi.AbstractInterruptibleChannel`:
`closeLock(0) closed(1) interruptor(2) interruptedTarget(3)`. The map is
`{0: fd (Int), 1: position (Long)}`. So the fd is an `Int` in the object real
`close()` synchronizes on, and the file position is a `Long` in `closed`.

The obvious red — *`AbstractInterruptibleChannel.isOpen()` returns `!closed`, so
a channel reports itself closed as soon as its position moves off zero* — was
this lane's first candidate for the flagship repair, and it is **wrong**.
`isOpen()` is registered on this class, twice, in
`native-builtins/src/phases_late/nio_file.rs`; both copies screen on the exact
class name, and the one that reads a field reads **slot 0 as the fd**, agreeing
with `native-io`'s map. `close()` is registered here. `begin()`/`end()` are
`protected final` and are only invoked by an implementation subclass, which this
object is not. `sun.nio.ch.FileChannelImpl` declares every other registered
method itself with `Code` (`javap -p`), so a real channel never resolves to
these natives.

**And that agreement is precisely why the repair is refused.** Moving the two
private slots above the declared width — the fix §3.1 got — would leave
`nio_file.rs`'s `isOpen` reading `get_field(this, 0)`, which after the move is
`closeLock`: `as_int()` gives `None`, the native answers `fd_id >= 0` on `-1`,
and **`isOpen()` would start returning FALSE for every open channel**. A
one-sided renumber of a map two crates share only moves the disagreement. That
is W7-49 §5's `AsynchronousSocketChannel` rule, met here in the live direction
instead of the dead one, and it needs both crates to move together plus a
build to settle which of the three registrations wins.

The measurement is recorded **in place**, at the constants, so the next reader
does not rediscover it and does not ship the half-fix.

### 3.3 Narrow request, but every write goes by NAME — no aliasing possible

Three triples. The request is a leftover synthetic width; the object is clamped
to full width and the writes resolve through the loaded class's own field table,
so no slot can land on the wrong field.

* **`java/io/IOException` 2 vs 6** (`native-io/src/lib.rs`, `afc_io_exception`).
  The primary path is `new_object_initialized("java/io/IOException",
  "(Ljava/lang/String;)V")` — the real constructor. The 2-slot allocation is the
  fallback for when that fails, and it writes `set_field_by_name(exc,
  "detailMessage", …)`. Real `Throwable` layout is
  `backtrace(0) detailMessage(1) cause(2) stackTrace(3) depth(4)
  suppressedExceptions(5)`; an index-0 write would have put the message in
  `backtrace`. It does not.
* **`java/net/InetAddress$InetAddressHolder` 3 vs 4** (`native-io/src/lib.rs`,
  `dc_inet_socket_address`). All three writes are `set_field_by_name` —
  `hostName`, `address`, `family` — and `family` is written with the correct `1`
  for IPv4. Only `originalHostName` is left null, which is the JDK's own value
  for an address that was never resolved from a name.
* **`java/nio/ByteBuffer` 5 vs 11** (`native-io/src/lib.rs`,
  `alloc_byte_buffer`). This one deserves its own paragraph, below.

`alloc_byte_buffer` writes each field **twice** — once by CratonVM index, once
by JDK name — and the *order* is what makes it correct. Traced against the real
layout: the array goes into slot 0, which on the real class is `mark`; then
`buf_set_mark(-1)`'s by-name write puts `-1` back into slot 0, overwriting it;
`hb` was already written by name into slot 6; `buf_set_mark`'s index write puts
`-1` into slot 4, which is `address`, and the final
`set_field_by_name(obj, "address", Value::Long(16))` corrects it. The object
ends correct on both layouts. That is a real repair, already landed by the
ByteBuffer lane (W7-58-bytebuffer-direct-arm.md), and the surviving `under` row
is the **request count** it did not also update.

**One residual, reported not repaired, and routed:** nothing writes `bigEndian`
(slot 9). A real `ByteBuffer.allocate(n)` answers `order() == BIG_ENDIAN`
(measured on HotSpot 25.0.3.9); a field left at its default is `false`, which
real `ByteBuffer.order()` bytecode reads as `LITTLE_ENDIAN`. Whether that is
live depends on which of the several `order()` registrations wins for
`java/nio/ByteBuffer`, which this lane did not settle. It belongs to the
ByteBuffer lane with the rest of the family, and it is a **missing write**, not
an alias — the allocation detector cannot express it either way.

`java/nio/HeapByteBuffer` 6 vs 11 (`native-builtins/src/servlet.rs`,
`s2_bb_alloc`) is that lane's too, and is reported here only so the count adds
up.

### 3.4 Narrow request with a slot map that is EMPTY

**`java/nio/channels/DatagramChannel` 3 vs 10** (`native-io/src/lib.rs`,
`native_dc_open`). The three-slot map the comment described —
`[0] fd, [1] bound_addr, [2] open` — **does not exist**. Every field moved into
the identity-keyed side tables (`dc_fds`, `dc_nonblocking_channels`,
`dc_connected`); `native_dc_open` allocates and then calls `dc_set_blocking` and
`set_dc_fd`, neither of which touches a slot.

A narrow request with no writes cannot alias anything. The row is real and
vacuous, and the comment that made it look otherwise is corrected in place.

Worth naming as a pattern rather than a one-off: this is the **side table**
remedy W7-49 §8 calls the sound one for foreign receivers, already deployed in
`native-io`, and its cost in this census is one permanently misleading row. A
future lane that converts a slot map to a side table should zero the count in
the same commit.

### 3.5 The collections overlay — an architecture, not a defect

**`java/util/ArrayList` 2 vs 3 (×7), `java/util/HashMap` 3 vs 8,
`java/util/TreeMap` 3 vs 9,
`java/util/concurrent/ScheduledThreadPoolExecutor` 3 vs 17,
`java/util/concurrent/LinkedBlockingQueue$Itr` 2 vs 5.**

Eleven of the census's 28 LIVE `under` sites are `native-collections`' overlay
model, plus the two `native-io` sites that build an `ArrayList` for
`Files.readAllLines` / `Files.walk`. These classes are not stood in for
selectively — the overlay registers the whole method surface and the narrow map
IS the model's layout.

Two things were checked before filing them this way, because both would have
made it a defect:

* **The two crates agree.** `AL_FIELD_DATA = 0` / `AL_FIELD_SIZE = 1` in
  `native-io/src/lib.rs` and in `native-collections/src/lib.rs` — the same
  numbers, independently declared. This is a genuine two-maps-on-one-class
  *hazard* (two declarations that can drift) but not, today, a
  two-maps-on-one-class *bug*.
* **The map does not line up with the real layout, and that is the model's
  problem, not a slot bug.** Real `ArrayList` is
  `modCount(0) elementData(1) size(2)` — `modCount` inherited from
  `AbstractList` — so the overlay's `elementData` sits in `modCount` and its
  `size` sits in `elementData`. Any real `List` bytecode reaching one of these
  objects sees nonsense. `probes/UnderAllocationProbe.java` §6 is built to find
  exactly that: `Files.readAllLines` then `size()`, `get(1)`, an enhanced-for
  (which constructs `AbstractList$Itr` over `modCount`), `indexOf`, and
  `new ArrayList<>(that)`. On HotSpot all six pass.

Repairing this means retiring or renumbering the overlay for five classes across
two crates, which is a project, not a lane. W7-49 §7 declined the same call for
the `HashSet` sites for the same reason. Filed as reported, with the probe left
in the tree as the discriminator whoever takes it will need.

### 3.6 The narrow arm is a fallback the real image never takes

**`java/util/regex/Pattern` 2 vs 20 (×2)** — the census's widest row, and
`native-io/src/lib.rs`'s `scan_make_pattern` **already asks the JDK first**:

```rust
let compiled = ctx.invoke("java/util/regex/Pattern", "compile",
                          "(Ljava/lang/String;)Ljava/util/regex/Pattern;", …);
if let Ok(Some(Value::Object(Some(pat)))) = compiled { return pat; }
```

The two 2-slot allocations are the arms below that `return`, reached only where
`Pattern.compile` cannot be invoked — a synthetic image whose `Pattern` is
itself a stub. On a real image the object handed back is a genuine compiled
`Pattern`.

The function's own doc comment already contains the measurement that motivated
it, and it is the best statement of this whole species in the tree: the two
writes *do* land on the right fields (`pattern` and `flags` are the real class's
first two), *"so nothing in the overlay census ever objected, and our own
readers only want slot 0. **It is still not a usable `Pattern`**"* — because
`matcher()` compiles lazily off `compiled`, `root`, `capturingGroupCount` and
`localCount`, and threw `ArrayIndexOutOfBoundsException` inside `Matcher.search`.

That is why probe §4 prints `pattern()` and `flags()` **and** drives
`matcher("x,y").find()`. The first two are the vacuous read — they pass against a
truncated object — and they are printed precisely so the next reader can see
them passing next to the one that does not. HotSpot: `pattern()` = `,`,
`flags()` = `0`, `matcher("x,y").find()` = `true`.

Also here, on the same reasoning: **`java/io/File` 1 vs 4** (×2),
`native_path_to_file`. Slot 0 *is* `path`, so the single write is
field-correct, and `status`, `prefixLength` and `filePath` are left at their
defaults. This lane started a repair that routed the allocation through
`File.<init>(String)` — on the argument that `prefixLength = 0` makes
`WinNTFileSystem.isAbsolute` (`(pl == 2 && charAt(0) == slash) || pl == 3`)
answer false for an absolute path — and **reverted it**, because
`java/io/File` is fully overlaid: `isAbsolute`, `getAbsolutePath`, `getParent`,
`getName`, `toPath`, `equals`, `hashCode` and `compareTo` are all registered
natives in `native-builtins/src/phases_late/nio_file.rs`, and so is
`<init>(String)` itself — which writes slot 0 and nothing else, for **every**
`new File(…)` in Compatible mode, not just this one. Nothing reads
`prefixLength`. The repair would have been a real invoke on a warm path bought
with a defect that does not exist.

(Incidental, not a layout finding and not changed: `Path.toFile()` skips the
`file_normalise_path` that `File.<init>(String)` applies, so the two disagree on
a URI-style `/C:/x`.)

### 3.7 Registered, but on triples the real class does not declare

**`sun/nio/ch/Iocp` 1 vs 14**, `native-io/src/async_socket.rs`, `iocp_open`.

`register_async_socket_real` *is* on the live path — `native-io/src/lib.rs`
calls it inside `register_io_natives`, which W7-59 §5.3 confirms the real-JDK arm
of `vm_init.rs` reaches. So the registrar is live and a source-level walk calls
the site LIVE, correctly.

It still cannot run. `javap -p sun.nio.ch.Iocp` on JDK 25.0.3.9 declares no
`open()`, no `drain()`, no `poll()` — the class is constructed by
`DefaultAsynchronousChannelProvider` through `new Iocp(...)`/`start()`, and its
only close-adjacent members are `closeAllChannels()` and the static native
`close0(long)`. All four registrations name methods that do not exist on the
real class, so no real call site can reach them (it would not verify), and
grepping the workspace finds no `invoke` naming them either. This is the
`method-nowhere` shape: registered, live registrar, structurally unreachable.

Its slot-0 write (`Value::Int(1)` into `provider`, an
`AsynchronousChannelProvider`) is therefore latent. Left alone; the honest fix is
deleting the four registrations, which is a dead-code question and not a layout
one.

### 3.8 Synthetic-only

**`java/time/ZoneOffset` 1 vs 3 and 2 vs 3, `java/time/format/DateTimeFormatter`
2 vs 7, `java/time/zone/ZoneRules` 1 vs 7** — all `native-builtins/src/util_time.rs`.

Confirmed rather than inherited: `register_time_natives` and
`register_time_extras_natives` have exactly one call site each
(`native-builtins/src/lib.rs`), and both sit inside
`pub fn register_synthetic_overrides`, which `vm_init.rs` gates on
`config.use_synthetic_jdk`. Dead in Compatible mode. This is the same verdict
W7-59 §5.3 reached for `util_time.rs`'s `over` rows, arrived at independently.

Note the contrast with W7-49 §7, which left `util_time.rs::native_zone_id_get_available`
alone because its reachability was *"genuinely indeterminate"* — a function
pointer the call-graph walk could not follow. Rooting at the **registrar** rather
than at the native settles it, because a native installed by a synthetic-only
registrar is synthetic-only however it is reached.

---

## 4. The tally

| verdict | triples | sites |
|---|---:|---:|
| repaired (`MappedByteBuffer`) | 1 | 2 |
| ~~mis-mapped, live, refused with a reason (`FileChannel`)~~ **repaired by W7-72; the refusal's premise was inverted — §3.2** | 1 | 2 |
| census mis-read a fallback arm — already correct | 3 | 3 |
| writes go by NAME — no aliasing possible | 3 | 4 |
| slot map is empty — vacuous row | 1 | 1 |
| collections overlay — architecture | 5 | 13 |
| narrow arm is a fallback the real image never takes | 2 | 4 |
| registered on triples the real class does not declare | 1 | 1 |
| synthetic-only registrar | 3 | 4 |
| `#[cfg(feature = "synthetic-jdk")]` — compiled out | 1 | 1 |
| owned by the ByteBuffer lane, reported only | 1 | 1 |

**Objects genuinely short: 0**, and §1 shows why that is structural rather than
lucky. **Slot maps that point inside the real class's declared fields: 8**
(`MappedByteBuffer`, `FileChannel`, `Iocp`, and the five overlay classes).
**Of those, reaching real JDK bytecode that reads an aliased field: 1**, repaired.

**Cannot run in Compatible mode: 6 sites** — four `util_time.rs`, one
`#[cfg(feature = "synthetic-jdk")]` `t16_dc_open`, one `Iocp` family. The census
reports 7 unreached without naming them; this lane can prove 6 and does not
claim the seventh. Three further sites (`Pattern` ×2, `IOException`) are on live
registrars but on an arm a real image never takes, which a source-level walk
cannot distinguish from LIVE and which is a distinction worth having.

---

## 5. Two things a reader of the next census must not misfile

**The repair turns an `under` row into an `over` row.** `MappedByteBuffer` will
now print `requested_fields=15, real_fields=13, direction="over"`. That is the
appended-slot idiom's signature, not a regression: the request is deliberately
`base + width`, and the whole point is that those two slots are above every
declared field. It also means `CRATONVM_DBG_VALIDATE_NEW=1` will call the object
`BAD`, because its slot count exceeds `num_total_fields`. Every user of
`try_alloc_with_appended_slots` has this property — it is the price the idiom
charges, and W7-49 §6.1's claim that after such a conversion *"the request
equals the declared width and the row disappears"* is not right for any site
with private state to carry.

The alternative that has neither cost is the identity-keyed side table (§3.4),
which `native-io` already runs for `DatagramChannel` and `jca/key_factory.rs`
runs with a GC-stable key. It was not used here because it trades a visible
census row for an invisible lifetime question, and this lane could not run
anything to check the second.

**The helper moved crates.** `appended_slot_base_for_class` was `pub(crate)` in
`native-builtins/src/util_concurrent_ext.rs` and is now
`cratonvm_native_api::appended_slots::base_for_class`, with the old name kept as
a forwarder. `native-io` could not otherwise reach it, and copying it would have
made it the sixteenth private re-implementation of a primitive
W7-59 §2.1 already counted fifteen copies of. The `&dyn`-taking sibling that
existed for the first hour of this branch was **deleted**: an accessor that
resolves the class through `class_id_by_name` while its allocator resolves it
through `ensure_class_initialized` can disagree with it on any class whose
by-name lookup is ambiguous across loaders, which is the two-layouts-on-one-class
condition reached from the other direction. One base function, called the same
way by both.

No `CRATONVM_*` flag was added; the detector's existing
`CRATONVM_DBG_LAYOUT_ALIAS` is reused unchanged, and nothing here makes it
quieter. Its five coverage gates in `native-api/tests/layout_alias_coverage.rs`
are untouched.

---

## 6. What this lane could not resolve

1. **Runtime confirmation of anything.** Nothing was built or run. Every claim
   is source-level plus `javap` and a HotSpot transcript of
   `probes/UnderAllocationProbe.java`; the CratonVM column of that probe has not
   been produced.
2. **The genuinely short objects — `declared == 0`.** §1 shows this is where
   they are, and this lane does not close it. It needs the `class_is_loaded`
   predicate `NativeContext` does not have, and it is a *different* census from
   this one: the interesting rows are allocations that happen before their class
   is loaded, which no current instrument distinguishes from an interface.
   **CLOSED as an instrument gap 2026-08-12** — `classify` now reports
   `declared == 0` as a third direction, `undeclared`, and the base allocator
   observes the `ClassId::new(0)` sentinel *before* it is substituted
   (W7-73 §4). The dominant producer turned out to be neither of the two this
   record considered: it is the `Err(_) => ClassId::new(0)` arm, which §2 below
   examined at four sites and cleared — correctly for the width species, and
   exactly backwards for the short-object species. The population is bounded by a
   downward-only ratchet at **28**, of which **12** are short
   (W7-73 §3.4). What is still open is whether any of those arms is ever taken,
   which no source read can answer.
3. ~~**`java/nio/channels/FileChannel`** — §3.2. Two crates, three registrations,
   a last-write-wins question that needs a build.~~ **CLOSED** by
   W7-72-ssc-socket-and-filechannel.md §2 — and the last-write-wins question did
   **not** need a build. It was answered by tracing the call graph from
   `vm_init.rs` through both `#[cfg]` arms and the `use_synthetic_jdk` flag,
   which is where the ordering is decided; this record read it from the
   registration sites and got it backwards. See the banner on §3.2.
4. **The collections overlay** — §3.5. Five classes, two crates, an
   architectural decision.
5. **`java/nio/ByteBuffer`'s `bigEndian`** — §3.3. A missing write, not an
   alias; routed to the ByteBuffer lane.
6. **Whether the two `AL_FIELD_DATA`/`AL_FIELD_SIZE` declarations ever drift.**
   They agree today. Nothing enforces that they keep agreeing, and a gate for it
   is cheap — a test asserting the two crates' constants are equal — but it
   belongs with whoever owns the overlay, because the right fix is one
   declaration, not two that are checked.
