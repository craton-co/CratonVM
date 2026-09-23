# W7-66 — the LIVE over-allocations, repaired; and why "over" is not a defect predicate

Status: of the 22 LIVE `over` sites W7-59-layout-detector-coverage.md
enumerated, **16 are repaired, 6 are left with a measured reason**, and the 5
dead sites are deleted outright along with the registrar that held them. Every
width below was re-derived with `javap -p` against the JDK 25.0.3.9 image on
this Windows host (Temurin 25.0.3+9), transitively over the superclass chain
with `static` excluded — the same oracle and convention as
W4-4-slot-index-species-sweep.md, W7-49-slot-index-recensus.md and W7-59.
**Nothing here was built or run as CratonVM**; the probe transcript quoted below
is HotSpot, which is the oracle, not the subject.

Branch `fix/layout-over-allocation-live-defects-20260812`.

> **STATUS BANNER — 2026-08-12, later pass.** Two changes to what is open here,
> and one census consequence this record did not state.
>
> 1. **§6's slot-5 `keys` clobber is REPAIRED**, by
>    W7-72-ssc-socket-and-filechannel.md §1 — an identity-hash-keyed, GC-rooted,
>    remapped, close-evicted side table, with the receiver pinned across both
>    allocating calls. §11 item 4 is struck.
> 2. **§4.3's narrowing moved a site into the OTHER census, and nobody noticed on
>    the day.** `alloc_obj`'s four callers went from 12 to `SC_OBJECT_SLOTS` = 6,
>    and `SocketChannel`/`ServerSocketChannel` declare **10** — so
>    `native-io/src/socket_channel.rs:637`'s `Err(_) => alloc_object(ClassId::new(0),
>    nfields)` arm, which W7-73-short-object-blind-spot.md §3.2 filed as
>    *"12 against 10 — over"*, is now **short by 4**. It is not a defect
>    (it is an `Err(_)` arm, latent by construction — W7-74-short-object-repairs.md
>    §2.1), and the repair itself is sound: on the live `Ok` arm the request is
>    `real.max(6)` = 10. It is a **census reclassification**, carried in
>    W7-73 §3.4. The general lesson is worth more than the row: a repair that
>    narrows a request toward the declared width can push its own fallback arm
>    below it, and the `over` and `under` censuses are the same measurement read
>    from two sides.
> 3. §11 item 6's *"`phases_late/net_channels.rs` … still allocate `SocketChannel`
>    at 4 and 5"* should be read alongside W7-88-net-channels-dead-registration.md,
>    which found that file's losing `ServerSocketChannel.socket()` never registered
>    at all in any of the four configurations.
>
> §1's correction — that `over` is not on its own a defect predicate — is the
> most-cited thing in this record and is re-read and holds. So does §2's
> superclass-fields-take-the-low-slots rule, which W7-72 §1.1 re-derived
> independently.

> **SOURCE-VERIFICATION BANNER — 2026-08-12, triage pass (A28). Three rows of
> this record are stale and one worry is closed. Everything else re-read holds.**
>
> Read against the tree, not against the record. What was *verified* is marked;
> the rest of this record remains source-level argument as it always said.
>
> 1. **§4.3 and §10 are stale on `SC_OBJECT_SLOTS`.** This record says the
>    constant is *"defined as `SSC_SOCKET_CACHE + 1`"*. **`SSC_SOCKET_CACHE` no
>    longer exists anywhere in the tree** — W7-72 §1 moved the slot-5 cache to
>    `ssc_socket_cache_table`, and took the constant with it. What is in
>    `native-io/src/socket_channel.rs:796` today is a bare
>    `const SC_OBJECT_SLOTS: usize = 6;` whose own doc states that **no native in
>    that file addresses a channel object slot by index any more**, so the number
>    encodes no slot map and is only a floor. Consequence: §10's *"largest single
>    risk"* bullet — "if a later lane adds a second object slot there without
>    raising the floor" — is **obsolete**, and the constant's doc already forbids
>    the thing it warns about ("must go through the side table, not raise this").
>    Also obsolete: the justification "6 is chosen precisely to keep
>    `SSC_SOCKET_CACHE` in range". The value is now arbitrary-but-harmless, and
>    the doc says to leave it alone for that reason.
> 2. **§3's liveness table mis-files `servlet.rs`, in the direction that
>    overstates open work.** The table lists `servlet.rs` under step 1,
>    `register_essential_natives_with_shims`. Its **channel** registrars are not
>    there: `register_s2_socket_channel` / `register_s2_server_socket_channel`
>    have exactly one caller each (`register_s2_nio`,
>    `native-builtins/src/servlet.rs:4666`–`:4667`), `register_s2_nio` has
>    exactly one caller in the workspace (`native-builtins/src/lib.rs:24147`),
>    and that line is inside `register_synthetic_overrides`, which spans
>    `:21554`–`:24335` and is `#[cfg(feature = "synthetic-jdk")]`. **They are
>    synthetic-only**, exactly like `net_channels.rs`'s `register_p58_nio_channels`
>    (W7-88). This is the same trap W7-68 §3.2 fell into from the other side and
>    W7-72 §2.1 named the rule for: *liveness of a registrar is a property of the
>    call graph from `vm_init`, not of the file the registrations are in.*
> 3. **§11 item 6 is therefore CLOSED in the real-JDK direction, and measured in
>    the other.** The question was whether these classes are allocated at two
>    widths in one run. They are allocated at **three**, and now all three are
>    accounted for:
>
>    | producer | requests | its slot map | mode |
>    |---|---:|---|---|
>    | `native-io/src/socket_channel.rs` (`alloc_obj`, ×4) | `SC_OBJECT_SLOTS` = **6** | none — `chan_fields` side table | both (WINNER) |
>    | `native-builtins/src/servlet.rs` `:7030 :7045 :7354 :7381` | **5** | `S2SC_*` 0..4 / `S2SSC_*` 0..4, written by INDEX | synthetic only |
>    | `native-builtins/src/phases_late/net_channels.rs` `:85 :100 :512` | **4** | `SSC_P58_SLOT_MAP` 0..3 | synthetic only |
>
>    Against a fabricated width of **5** (`class_manager.rs:13479`–`:13480`,
>    verified) and a declared width of **10**. In real-JDK mode the two narrow
>    producers do not exist, so neither 4-slot nor 5-slot map can ever meet a real
>    ten-field channel. In synthetic mode both maps land inside the fabricated
>    five. **No aliasing is reachable from either, in any of the four
>    configurations** — which is a stronger answer than "out of scope".
>
>    Worth stating because the write set is alarming and the reader should not
>    have to re-derive that it is inert: `servlet.rs`'s `S2SC_OPEN = 1` would be
>    `AbstractInterruptibleChannel.closed` on a real channel, and
>    `set_field(ch, S2SC_OPEN, Int(1))` would make a real `isOpen()` answer
>    **false** for a freshly opened channel. That is a worse shape than anything
>    in §5 — and it is unreachable, for the call-graph reason in item 2 alone.
> 4. **§6's second finding is still in the tree, unrepaired.**
>    `classloading/src/class_manager.rs:13472`–`:13473` still reads
>    `java/nio/channels/ServerSocketChannel = 1 (provider)` /
>    `java/nio/channels/SocketChannel = 1 (provider)`, six and seven lines above
>    the arms at `:13479`–`:13480` that fabricate both at **5**. Nomination N-1
>    below.
>
> **Verified true and not to be re-derived:** `class_manager` fabricates
> `TreeSet` 3 (`:12359`), `CompletableFuture` 4 (`:13095`), `InetSocketAddress` 3
> (`:13328`), both channels 5 (`:13479`–`:13480`) — every fabricated width §4 and
> §10 rest on. `CF_NUM_FIELDS = 2` (`native-collections/src/lib.rs:56126`), and
> `CF_FIELD_SOURCE`/`CF_FIELD_HANDLER` survive only inside a doc comment, so
> §4.2's deletion landed. `try_alloc_declared_width` exists (`:2703`) with nine
> `TreeSet` callers, so §4.1 landed. **`register_selector` has no definition
> anywhere in the workspace**, so §7's deletion landed.
>
> **Nomination N-1 — `classloading/src/class_manager.rs`, comment only.**
> Exact old text (two consecutive lines, `:13472`–`:13473`):
>
> ```text
>         // java/nio/channels/ServerSocketChannel = 1 (provider)
>         // java/nio/channels/SocketChannel    = 1 (provider)
> ```
>
> Exact new text:
>
> ```text
>         // java/nio/channels/ServerSocketChannel = 5 — NOT 1. The comment read
>         // "1 (provider)" until 2026-08-12, describing the REAL JDK class's own
>         // declaration (`provider`) while the arm below fabricates five. The
>         // real transitive width is 10, not 1 (four from
>         // AbstractInterruptibleChannel, six from AbstractSelectableChannel);
>         // see W7-72 §1.1 for the slot-by-slot derivation.
>         // java/nio/channels/SocketChannel    = 5 — same, same reason.
> ```
>
> This is the shape §6 calls the campaign's signature failure — a slot-map or
> width comment describing a different layout from the code beneath it — sitting
> five lines from the arm it misdescribes, in the file every fabricated width in
> this record is read from.


> **VERIFIED AGAINST A BINARY 2026-09-03.** This record's status read *"Nothing
> here was built or run as CratonVM; the probe transcript quoted below is
> HotSpot, which is the oracle, not the subject."* The subject has now been run:
> `probes/OverAllocationWidthProbe.java` — this record's own probe, recovered
> from a sibling worktree after `3b2901531` deleted `probes/` from the checkout
> — under `CRATONVM_DBG_LAYOUT_ALIAS=1` on a binary built from this tree, with
> the instrument confirmed present in it by string-probing the binary first.
>
> **§1's correction is now measured, not argued.** This record's central claim
> is that `over` is not on its own a defect predicate. The live census says the
> same thing from the other side:
>
> ```text
> over   19 species /  97 observations    widest +5
> under  19 species /  81 observations    widest -16
> ```
>
> Every divergence wider than 6 slots is on the `under` side. The widest `over`
> in the whole live census is `java/util/HashMap$KeyIterator` at 10 against 5,
> and `sun/nio/ch/EPollSelectorImpl` at 23 against 18. Against that,
> `java/util/Properties` runs 16 against 32 and `java/util/HashSet` 1 against
> 16, the latter on 15 separate observations. A record that had repaired 16
> `over` sites and left 6 with a reason was right to insist the predicate was
> the wrong one.
>
> **The remaining live `over` species, in full:**
>
> ```text
> java/util/HashMap$KeyIterator      10/5  +5     cratonvm/internal/UnmodifiableSet    2/1  +1   x17
> java/util/HashMap$EntryIterator    10/5  +5     cratonvm/internal/UnmodifiableList   2/1  +1   x9
> sun/nio/ch/EPollSelectorImpl      23/18  +5     cratonvm/internal/UnmodifiableItr    2/1  +1   x4
> sun/nio/ch/UnixAsynchronousSocketChannelImpl 52/48 +4  cratonvm/internal/UnmodifiableMap 2/1 +1 x4
> sun/nio/ch/UnixAsynchronousServerSocketChannelImpl 20/16 +4  …$UnmodifiableListItr 2/1 +1 x4
> java/lang/reflect/Field           18/15  +3     java/lang/module/…$Exports          4/3  +1   x4
> java/util/TreeMap$KeyIterator       7/4  +3     java/lang/module/…$Opens            4/3  +1   x4
> java/lang/module/ModuleDescriptor 16/14  +2     java/net/URI                      18/17  +1   x2
> java/lang/invoke/VarHandle          6/4  +2     java/util/HashMap$EntrySet          2/1  +1   x1
>                                                 java/util/Spliterator               4/3  +1   x1
> ```
>
> Five of these are `cratonvm/internal/Unmodifiable*` — VM-minted carriers, +1
> each, 38 observations between them. They are the numerically dominant `over`
> and they are the least interesting one, which is §1's argument restated as
> data.
>
> **What this does NOT verify.** The instrument reports a CLASS and a call-site
> chain; this record enumerates SOURCE SITES. No row above is matched to any of
> the 22 sites, so **it is not shown that the 6 sites left with a reason are the
> ones still firing**, nor that the 16 repairs are the reason the others are
> absent — a repaired site and a site the probe never dispatched look identical
> here. The census is also a lower bound by construction: W7-49's headline is
> that 511 direct allocation call sites never reach this detector at all. Widths
> are the VM's own request against the VM's own declared layout, so an `over`
> against a class the VM declares wrongly is not distinguished from an `over`
> against a class it declares right. The 5 deleted dead sites are untestable by
> a runtime instrument and are not revisited.
## 1. The correction this record exists for

**The `over` direction is not, on its own, a defect predicate — and W7-49's own
landed remedy produces `over` rows.** `try_alloc_with_appended_slots`
(W7-49 §8) allocates `base + width` on a class declaring `base`. Requested
exceeds declared, by construction, forever. Any census that reads an `over` row
as "a native writes past the end of a real object" will therefore mis-file the
correct code as a defect and, worse, will keep doing so after each repair.

W7-59 §5.1's eleven-triple table is accurate as a **width census**. Its §8
narrative attached a defect story to those widths — "this is exactly the
`HashSet` shape W7-49 §6.3 repaired: the backing map is null and the count sits
past the end where nothing can read it" — and that story is **wrong for the two
largest clusters**, for a reason no width can express: those subsystems moved
their state into address-keyed side tables years ago, so there is no count
written past the end, because there is no count written to the object at all.

Three distinct things wear the same `over` row, and only one of them is a bug:

| shape | what it is | example here |
|---|---|---|
| **wide-but-unused** | request > declared, extra slots never written or read; state lives in a side table | TreeSet 3/1, SocketChannel 12/10, CompletableFuture slots 2–3, InetSocketAddress 2/1 |
| **appended private slots** | request > declared *on purpose*; the private slots start above every declared field | FileLock 6/4 — already the W7-49 §8 idiom, hand-written |
| **genuine aliasing** | request > declared AND a write lands on a field the class declares | AsynchronousServerSocketChannel 4/1 (`Int` into `provider`) |

Only the third is heap corruption in the §5 sense of
natives-over-real-jdk-classes.md. Distinguishing them needs the **writes**, not
the width, and the writes are what this lane read.

## 2. Widths: no disagreement with the census

All seven re-derived, and all seven reproduce W7-59 exactly.

| triple | declared | derivation (`javap -p`, statics excluded) |
|---|---:|---|
| `java/util/TreeSet` | 1 | `m` only; `AbstractSet`/`AbstractCollection` declare none |
| `java/util/concurrent/CompletableFuture` | 2 | `result`, `stack`; superclass `Object` |
| `java/nio/channels/SocketChannel` | 10 | own 0 + `AbstractSelectableChannel` 6 + `SelectableChannel` 0 + `AbstractInterruptibleChannel` 4 |
| `java/nio/channels/ServerSocketChannel` | 10 | same chain, own 0 |
| `java/nio/channels/AsynchronousServerSocketChannel` | 1 | `provider`; superclass `Object` |
| `java/nio/channels/FileLock` | 4 | `channel`, `position`, `size`, `shared`; superclass `Object` |
| `java/net/InetSocketAddress` | 1 | `holder`; `SocketAddress` declares only a static |

Two things W7-59 did not state that the repairs turned on, both measured here:

* **CratonVM assigns superclass fields the low slots.**
  `class_manager::compute_field_layout` — *"Instance fields (non-static) from the
  superclass occupy slots `0..N`. This class's own instance fields start at
  `N`."* So on a real `ServerSocketChannel` the slots are
  `closeLock(0) closed(1) interruptor(2) interruptedTarget(3) provider(4)
  keys(5) keyCount(6) keyLock(7) regLock(8) nonBlocking(9)`. §6 below is
  entirely a consequence of that line.
* **`NativeContext::alloc_object` clamps requests UP** to the declared count and
  never down (`let slots = num_fields.max(real_fields);`). Every narrowing below
  is therefore a no-op wherever the class declares at least as much, which is
  what makes the synthetic-JDK arm safe without a second code path.

## 3. Liveness: which registrar wins, on which boot path

`register()` is last-write-wins, so a LIVE verdict has to name the winner, not
just a caller. The real-JDK arm of `vm/src/vm/vm_init.rs` calls, in order:

1. `register_essential_natives_with_shims` (`:2239`) — everything under
   `native-builtins`, including `phases_late/net_channels.rs`, `servlet.rs` and
   `phases_late/collections.rs`
2. `register_io_natives` (`:2434`) — `native-io`
3. `register_collections_natives` (`:2465`) — `native-collections`, **last**

| triple | winner | why, and what it beat |
|---|---|---|
| `java/util/TreeSet` | **`native-collections`** `register_tree_set_natives` (from `register_collections_natives`, `:2409`) | runs last, so it overwrites `native-builtins::register_p62_navigable_expansion`, which registers `floor`/`ceiling`/`lower`/`higher` on the same class. All nine repaired sites are inside natives this registrar installs. |
| `java/util/concurrent/CompletableFuture` | **`native-collections`** (the `let cf = …` block) | same last-write-wins position; `native-builtins::util_concurrent_ext` registers the same triples earlier and loses. |
| `SocketChannel`, `ServerSocketChannel` | **`native-io`** `socket_channel::register_socket_channel_real` (from `register_io_natives`) | beats `phases_late/net_channels.rs` and `servlet.rs`, which register the same classes at step 1. Note both losers also *allocate* these classes at 4 and 5 slots — an UNDER row, not this lane. |
| `AsynchronousServerSocketChannel` | **`native-io`** `async_socket::register_async_socket_real` | beats `phases_late/net_channels.rs:1730`. |
| `java/nio/channels/FileLock` | **nobody, in Compatible mode** | the `FileLock` registrations sit inside `#[cfg(feature = "synthetic-jdk")]`. The *allocation* (`alloc_file_lock`, from `native_fc_lock`/`native_fc_try_lock`) is unconditional and therefore live in both modes — which is exactly why slots 0–3 aliasing the real fields is load-bearing: in Compatible mode the readers are the JDK's own `final` accessors. |
| `java/net/InetSocketAddress` | **`native-io`** `dc_inet_socket_address`, reached from `register_datagram_channel` (from `register_io_natives`) | the site is a helper, not a registration; its caller is live. |
| `java/nio/channels/Selector`, `SelectionKey` (the 5 dead) | **`nio_selector.rs::register_nio_selector`** | `register_selector` had no caller at all — see §7. |

`NativeKind` was checked at every block this lane touched: the surrounding
category is `Bridge` in `register_tree_set_natives`,
`register_nio_channel_extras` and `register_async_socket_real`
(`set_category(NativeKind::Bridge)` … `set_category(__prev_cat)`). No
registration was added, moved or removed by any repair, so no ambient category
changed. The one registrar deleted (§7) took its own `set_category`/restore pair
with it.

## 4. Repaired — 16 of the 22 LIVE sites

Each says whether it touches Compatible mode and why that is justified. Writing
past a real class's declared width is a genuine defect, which is the exception
the Compatible-mode freeze names; but **three of these four repairs are not even
that** — they remove slots nothing wrote, which is a strictly smaller change
than the freeze contemplates.

### 4.1 `java/util/TreeSet` 3 → declared, nine sites — `native-collections/src/lib.rs`

`native_tm_key_set`, `native_ts_head_set`, `native_ts_tail_set`,
`native_ts_sub_set`, `native_ts_tail_set_inclusive`,
`native_ts_head_set_inclusive`, `native_ts_sub_set_inclusive`,
`native_ts_descending_set`, `native_cslm_key_set`.

**The census's story does not hold, and the disagreement is worth the ink.**
W7-59 §8 says the count sits "in slot 1 … where nothing can read it back". It is
not in slot 1. `ts_set_slot`'s own doc says *"The object's own fields are never
touched"* — `TS_FIELD_DATA`/`SIZE`/`COMPARATOR` are keys into `ts_array_table`,
an address-keyed side table. So the three-slot request wrote **nothing**
anywhere on the object; it simply asked for two slots past the declared width
that no site in the workspace reads or writes.

Repaired with a new `try_alloc_declared_width`, which asks the loaded class and
keeps `TS_NUM_FIELDS` only as the "no class of this name anywhere" fallback that
`try_alloc_synthetic` already had. **Touches Compatible mode**: the object is
one slot wide instead of three. Nothing observable changes, and that is the
claim, not a hope — the only readers of a TreeSet slot in the workspace are
`ts_get_slot`/`ts_set_slot`, which do not read slots.

Provably a no-op in synthetic-JDK mode: `class_manager` fabricates
`java/util/TreeSet` with three instance fields
(`"java/util/TreeMap" | "java/util/TreeSet" => instance_fields(3)`), and
`alloc_object` clamps up to them.

**The half this does NOT fix, and it is real.** `m` is null on every set these
natives return. Registered methods (`size`, `iterator`, `contains`, `first`,
`toArray`, `stream`, …) answer from the side table and are fine; the *un*
registered ones run real bytecode into a null `m` — `spliterator()`, `clone()`,
`equals`/`hashCode` where they do not route through `size`/`iterator`, and the
JDK 21+ `SequencedCollection` additions. That is the null-backing-map species of
W7-49 §6.3, its remedy is a real backing `TreeMap` (much larger than a width),
and `probes/OverAllocationWidthProbe.java` prints the reds so the next lane has
them measured rather than argued.

### 4.2 `java/util/concurrent/CompletableFuture` 4 → 2, two sites — `native-collections/src/lib.rs`

`cf_make_synthetic` and `cf_make_completed`'s last-resort arm.

`CF_FIELD_SOURCE` (2) and `CF_FIELD_HANDLER` (3) have **zero read sites and zero
write sites anywhere in the workspace** — a workspace-wide grep returns their
two `const` declarations and nothing else. Both slots sat past the declared
width with no reader. Width is now `CF_NUM_FIELDS = 2`, the declared width, and
the two constants are deleted rather than left as an invitation.

`cf_make_completed`'s fallback now tries the JDK's own
`CompletableFuture.completedFuture(Object)` before any slot-written stand-in —
the remedy W7-49 §6.2 landed as `aio_completed_future`. It is a *different door*
from the `new_object_initialized` above it (a static method, no `<init>`
interception), so it can succeed where that failed, and it writes no index at
all. The two copies of the stand-in are collapsed onto one
`cf_make_synthetic_slots`, because two implementations of one primitive is how
this codebase gets primitives that disagree.

**Touches Compatible mode** — genuine defect (slots past the declared width) plus
one strictly-better fallback door.

**Slot 1 is NOT repaired, and the reason is not timidity.** Slot 1 aliases the
real `volatile Completion stack`, a reference, and the synthetic `done` marker
written there is an `Int` — the §5 shape exactly. Three findings say a blind fix
is worse than the defect:

1. **It is the discriminator.** `cf_is_real_jdk` is
   `!matches!(get_field(this, 1), Value::Int(_))`, and eleven call sites gate
   BUG-17's real-JDK delegation on it.
2. **It is the only discriminator that survives a subclass.** Moving the marker
   to an appended slot `base` breaks on a real subclass such as
   `KafkaCompletableFuture`, whose slot `base` is one of *its own* fields and may
   well be an int — precisely the foreign-receiver trap W7-49 §8 refuses to build
   a helper for. Slot 1 of any subclass is still `stack`, still a reference.
3. **The obvious alternative regresses synthetic mode.** Returning a real
   `new CompletableFuture()` for the pending case (every live caller of
   `cf_make_synthetic` passes `CfState::Pending`; the `Normal`/`Exceptional` arms
   are dead at every call site) flips those futures onto the
   `invoke_special(… "uniRunStage" …)` path — which does not exist in
   synthetic-JDK mode. It is also a semantic change on the Spring/reactor path,
   unbuildable here.

The sound remedy is the side table TreeSet and `socket_channel.rs` already use.
That is a lane, not a line.

### 4.3 `SocketChannel` / `ServerSocketChannel` 12 → declared, four sites — `native-io/src/socket_channel.rs`

`F_OPEN`..`F_REUSEADDR` are indices into the identity-keyed `chan_fields` side
table — `cf_set`'s doc says so and `cf_get` enforces it with an
`idx >= N_FIELDS` early return. `alloc_obj` was already clamping up to the real
layout deliberately (so `init_channel_locks` can seed the real named monitor
fields), so the twelve was pure over-request: slots 10 and 11 had no reader.

The four sites now pass `SC_OBJECT_SLOTS`, a floor rather than a width, defined
as `SSC_SOCKET_CACHE + 1` — because the *only* slot on a channel object any
native in the file touches is `SSC_SOCKET_CACHE`. Verified by grep: the sole
`ctx.get_field`/`ctx.set_field`/`object_num_fields` calls on a channel receiver
in that file are the three at `SSC_SOCKET_CACHE`.

**Touches Compatible mode**: ten slots instead of twelve. In synthetic-JDK mode
the width goes twelve → six, which is why the constant is a floor and not just
the declared count: `class_manager` fabricates both channels with **five**, and
a bare five would push `SSC_SOCKET_CACHE` out of range and silently disable the
`ServerSocket` adaptor cache.

### 4.4 `java/net/InetSocketAddress` 2 → 1, one site — `native-io/src/lib.rs`

`dc_inet_socket_address` writes every field with `set_field_by_name`, so the
request only ever had to cover the declared width. The second slot had no writer
and no reader. **Touches Compatible mode**; no-op in synthetic-JDK mode, where
`class_manager` fabricates three.

## 5. Left, with the reason — 6 of the 22

| triple | sites | why |
|---|---:|---|
| `java/nio/channels/AsynchronousSocketChannel` 4/1 | 3 | Out of bounds for this lane by instruction, and the instruction is right: `native-io` owns and allocates the class, but `native-builtins`' surviving `connect` triple reads slots 0–3 of *these* objects under a map whose slots 0 and 1 mean the opposite (W7-49 §5). A prior lane converted and correctly reverted. |
| `java/nio/channels/AsynchronousServerSocketChannel` 4/1 | 1 | **The one genuine aliasing defect in this census, and it cannot be fixed alone.** `aio_assc_open` writes `Int(1)` into slot 0, which is the real `provider` reference, and slots 1–3 past the end. But `F_OPEN`..`N_FIELDS` are module-level constants shared with `AsynchronousSocketChannel`, and three registrations bind the *same* native to both classes — `isOpen` is `aio_asc_is_open`, reading `F_OPEN` off whichever receiver arrives. Renumbering here alone breaks `isOpen` on every server channel; renumbering both drags in the excluded triple above. The repair is: split the maps per class, give `aio_asc_is_open` a per-class sibling, and settle the `native-builtins` survivor in the same step. One change, two crates, needs a build. Recorded in place on `aio_assc_open`. |
| `java/nio/channels/FileLock` 6/4 | 2 | **Not a defect.** Slots 0–3 alias `channel`, `position`, `size`, `shared` exactly by index and by type, which is what makes the real `final` accessors return our values in Compatible mode; slots 4–5 are private state starting above every declared field. That is the W7-49 §8 appended-slot idiom, hand-written. Left as literal indices on purpose — these natives also run on receivers they did not allocate (in real-JDK mode `<init>` lands on a real five-field `sun.nio.ch.FileLockImpl`), and W7-49 §8 records why a per-receiver base cannot be recovered from a foreign object's width. Documented in place with the `javap` derivation so the next census does not re-open it. |

That is 3 + 1 + 2 = 6 left, 22 − 6 = **16 repaired**.

## 6. Found while reading the writes, and worse than anything in the census

**`native-io/src/socket_channel.rs::ssc_socket` caches a `java.net.ServerSocket`
in slot 5 of a real `ServerSocketChannel`, where the JDK declares
`AbstractSelectableChannel.keys` — the `SelectionKey[]`.**

The origin is a comment, and it is the campaign's signature failure in one line.
Slot 5 was annotated `// unused F_REMOTE slot` — reading the `chan_fields`
*side-table index map* as if it were the object layout. `F_REMOTE` is a side
table key. Slot 5 of the object, by §2's ordering rule, is `keys`.

It is an **in-bounds write of the wrong field**, so:

* the allocation-width detector cannot see it (no width is wrong — this is why
  narrowing 12 → 10 changes nothing here);
* the `cratonvm::gc::guard` out-of-bounds reads cannot see it (slot 5 exists);
* W7-59 §6 names this species precisely and says a second, receiver-keyed
  instrument is needed. This is a live instance of it, found by hand.

Real bytecode that reads `keys`: `AbstractSelectableChannel.register`,
`isRegistered`, `keyFor`, `removeKey`, `implCloseChannel`. Any of them on a
channel whose `socket()` has been called gets a `ServerSocket` where a
`SelectionKey[]` is expected — and `NioEndpoint`-shaped code calls both.

~~**Not repaired here**~~ **— REPAIRED 2026-08-12 by W7-72-ssc-socket-and-filechannel.md
§1**, and along exactly the line this paragraph predicted: the sound remedy is
the identity-keyed side table this same file already runs in the *other*
direction (`SsBackRef`, with `gc_scan_ss_back_ref_roots` and
`ss_back_ref_update_after_gc`), so it means a new GC-rooted table plus its remap
hook, on a Tomcat-critical path, unbuildable here. The derivation is written out
at the constant so the next lane starts from the answer. The probe already has
the reads that expose it (`ssc.keyFor.afterSocket`, `ssc.socket.stable`), with
the HotSpot values measured.

What the repair added beyond the prediction: the table is keyed on the
**GC-stable identity hash**, not the address (an address-keyed table recycles a
dead row onto a fresh object at the same address), each bucket disambiguates by
`ObjectRef` because identity hashes are not unique, both ends are GC-rooted and
remapped through `vm/src/memory/native_roots.rs`, and the row is evicted on
`close` so two roots per row do not pin a dead listener for the process
lifetime. `SC_OBJECT_SLOTS` keeps its numeric value deliberately, so **no
allocation width and no `CRATONVM_DBG_LAYOUT_ALIAS` row moves for these
classes** — which is also why the reclassification in the status banner is a
consequence of §4.3 and not of the `keys` repair.

A second, smaller stale-comment finding, recorded and not acted on:
`class_manager.rs` comments `java/nio/channels/{Server,}SocketChannel = 1
(provider)` immediately above arms that say `instance_fields(5)`.

## 7. `register_selector` really was dead, and is deleted

Confirmed independently of the census, before deleting:

* `register_selector`'s only reference in the workspace was its own definition,
  plus a comment in `register_nio_channel_extras` explaining that Wave 3 / Task C
  removed the call and that `nio_selector.rs` is the source of truth.
* Each of the ten natives it installed — `native_sel_open`,
  `native_channel_register`, `native_sel_select{,_timeout,_now}`,
  `native_sel_selected_keys`, `native_sel_keys`, `native_sel_wakeup`,
  `native_sel_close`, `native_sel_is_open` — had exactly **one** other
  reference: its own `fn`. So did `do_select`.
* `SEL_FIELD_*` / `SK_FIELD_*` had no reference outside that block.

Deleted: the registrar, `do_select`, the ten natives, and the two slot maps.
Kept: the `OP_*` NIO-spec bits, which a test in the same file asserts and which
are constants, not a layout. The `SelectionKey.OP_READ`/`OP_WRITE`/`OP_CONNECT`/
`OP_ACCEPT` accessor registrations carried under a "KEEP" comment went with the
registrar, and that changes nothing — an unreachable registrar never installed
them, and `nio_selector.rs` declares the same four constants.

This clears **all five** of W7-59's dead `over` rows (`SelectionKey` 4/1,
`HashSet` 2/1 ×4), which were noise in every future run of
`CRATONVM_DBG_LAYOUT_ALIAS`.

## 8. `HEADER_SIZE`

`HEADER_SIZE` is 16 (`types/src/heap_types.rs:19`) and no site this lane touched
does header arithmetic. Re-verified rather than inherited: a grep for
`HEADER_SIZE`, `heap_alloc_object`, `try_alloc_object_full`,
`alloc_object_shared`, `GenHeap` and `mem.heap` across `native-io/src` and
`native-collections/src` returns **nothing at all**. Every site addresses fields
by slot index and the object model resolves them relative to the header. W7-49's
and W7-59's finding still holds.

## 9. Proving it — `probes/OverAllocationWidthProbe.java`

Written to `SlotIndexRecensusProbe.java`'s rule: every read goes through a real
JDK accessor, never through the native that wrote the slot. Sets assert exact
contents and exact sizes, in order, because "the set is non-empty" passes
against a corrupt object several ways.

The full HotSpot 25.0.3+9 transcript is appended to the probe as its oracle,
measured on this host. The load-bearing reads:

* `getNumberOfDependents()` — JDK bytecode walking `CompletableFuture.stack` by
  the JDK's own index; no native of ours is registered on it. HotSpot: `0` for a
  fresh pending future, `1` after one dependent.
* `keyFor(sel)` / `isRegistered()` immediately after `socket()` — JDK bytecode
  over `keys` and `keyCount`, i.e. the §6 finding. HotSpot:
  `registered=true sameKey=true`.
* `provider()` on `AsynchronousServerSocketChannel` — a real `final` accessor
  over the one field it declares, i.e. §5's aliasing. HotSpot:
  `sun.nio.ch.WindowsAsynchronousChannelProvider`.
* `FileLock.position()/size()/isShared()/channel()` — the four real `final`
  accessors, which is the check that §5's alias is *right*, not that the width
  is.

**And the honest limit, stated because a probe that hid it would be the vacuous
shape this project keeps an index of.** Three of the four repairs are
wide-but-unused: they remove slots nothing read. **No Java-visible value can
change, and none should.** For those the probe's job is the reverse of a red —
it proves the repair changed nothing else — and the actual observable is the row
disappearing from `CRATONVM_DBG_LAYOUT_ALIAS=1`, which is a report changing state
on a real run. Every line in the probe is marked NO-CHANGE or RED so the two
kinds are not confused.

The one test touched was **tightened, not weakened**:
`cf_field_layout_constants_consistent` now asserts `CF_NUM_FIELDS == 2` against
the `javap` oracle, so the width cannot drift back up unremarked.

## 10. Blast radius

* **Compatible mode is touched by all four repairs**, and each is justified as
  the freeze's genuine-bug-fix exception. Three of the four are strictly smaller
  than that: they delete slots that had no writer and no reader, which cannot
  change behaviour without changing what "no reader" means.
* **Synthetic-JDK mode is unchanged at every site by construction, not by
  argument**: `alloc_object` clamps up, and `class_manager` fabricates TreeSet at
  3, CompletableFuture at 4, InetSocketAddress at 3 — all at or above the new
  requests. The one exception is deliberate: `SocketChannel`/`ServerSocketChannel`
  go 12 → 6 there, and 6 is chosen precisely to keep `SSC_SOCKET_CACHE` in range.
* **No registration was added, removed or reordered**, so no last-write-wins
  outcome moved and no ambient `NativeKind` changed. The single exception is the
  deletion of a registrar with no caller.
* **No new `CRATONVM_*` flag.** The existing `CRATONVM_DBG_LAYOUT_ALIAS` is the
  instrument; nothing in `types/src/flag_groups.rs`,
  `types/tests/flag-surface.txt`, `docs/flag-tokens.md` or
  `docs/config/flag-inventory.md` needed to move.
* **Largest single risk** is §4.3: `SC_OBJECT_SLOTS` is derived from a grep that
  the only object-slot user in `socket_channel.rs` is `SSC_SOCKET_CACHE`. If a
  later lane adds a second object slot there without raising the floor, its
  writes land out of bounds in synthetic-JDK mode and are dropped in silence.
  The constant's doc says so at the definition.

## 11. What this lane could not resolve

1. **Runtime confirmation of anything.** Nothing was built or run as CratonVM.
   The HotSpot transcript is the oracle, not evidence about this VM.
2. **The `stack` aliasing on `CompletableFuture`** — §4.2, three independent
   reasons a blind fix is worse, and a named remedy that is its own lane.
3. **`AsynchronousServerSocketChannel`** — §5. The only true aliasing defect in
   the census, blocked behind a shared slot map and an excluded sibling.
4. ~~**The slot-5 `keys` clobber** — §6. Newly found, worse than anything the
   census listed, and of a species no existing instrument can see.~~ **CLOSED**
   by W7-72-ssc-socket-and-filechannel.md §1. The species claim stands: no
   allocation-width detector and no out-of-bounds discriminator could have found
   it, and the descriptor coercion passes a reference into a reference slot
   unchanged. It was found by hand and it is still the one confirmed instance.
5. **The TreeSet null backing map** — §4.1. Repairing the width does not touch
   it; the probe measures the reds.
6. **Whether any of these classes is allocated at BOTH widths in one run.**
   Still unanswered, and this lane narrowed several requests toward the declared
   width, which makes a *second* allocator at the old width harder to spot, not
   easier — `phases_late/net_channels.rs` and `servlet.rs` both still allocate
   `SocketChannel` at 4 and 5. Those are UNDER rows and out of scope, but they
   are the same objects the winning natives then receive.
