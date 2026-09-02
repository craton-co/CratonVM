# W7-88 — the dead `ServerSocketChannel.socket()` in `net_channels.rs`

Status: **deleted**. Residual 7 of W7-72-ssc-socket-and-filechannel.md is
discharged. The registration turned out to be one step deader than that record
believed — it does not merely *lose*, it never *registers* in any configuration
a user can run — and the field writes it carried were worse than the two the
record named.

Branch `fix/net-channels-losing-registration-20260812`.

> **READ §10 BEFORE APPLYING ANYTHING IN THIS RECORD TO THE FILE AS A WHOLE.**
> Everything below is scoped to `register_p58_nio_channels`, which registers
> nothing in any runnable configuration. The **next function down**,
> `register_p67_async_channels`, is reached from
> `register_essential_natives_with_shims` and is **live in both shipping
> modes** — §2's own census says so and the sentence reads the other way at a
> glance. Four of its triples survive `native-io`'s later registrations, and two
> of those are broken: `AsynchronousFileChannel.force(Z)V` is a **silent no-op**
> and `AsynchronousFileChannel.lock()` / `AsynchronousServerSocketChannel.accept()`
> hand back a `FutureTask` whose `get()` **never returns**.

**This lane did not build.** Every CratonVM claim is either (a) a transcript of
the **prebuilt dev binary** `C:/craton/CratonVM/target/release/cratonvm.exe`,
built 2026-08-12 07:33 — the same commit this branch is cut from, and later than
the 05:20 commit that last touched either file involved — or (b) source-level,
and says so. Every JDK layout is `javap -p` against Eclipse Adoptium 25.0.3.9 on
this Windows host (`javap -version` = `25.0.3`), counted transitively over the
superclass chain, superclass first, declaration order within a class, `static`
excluded — the same oracle and convention as W4-4-slot-index-species-sweep.md,
W7-49-slot-index-recensus.md, W7-59-layout-detector-coverage.md, W7-72 and
W7-77-guarded-slot-maps.md.

---

## 1. What was there

`native-builtins/src/phases_late/net_channels.rs`, inside
`register_p58_nio_channels`, registered a second
`(java/nio/channels/ServerSocketChannel, socket, ()Ljava/net/ServerSocket;)`.
The body cached a fabricated 5-slot `java.net.ServerSocket` in slot 3 of the
channel and read the fd from slot 2:

```rust
let this = obj_arg(args, 0)?;
if let Value::Object(Some(cached)) = ctx.get_field(this, 3) { /* return it */ }
// 5-field ServerSocket: SS_PORT=0, SS_BACKLOG=1, SS_CLOSED=2, SS_LISTENER_ID=3, channel_ref=4
let ss = try_alloc_concurrent_synthetic(ctx, "java/net/ServerSocket", 5)?;
ctx.set_field(ss, 0, Value::Int(0));
ctx.set_field(ss, 1, Value::Int(50));
ctx.set_field(ss, 2, Value::Int(0));
ctx.set_field(ss, 3, Value::Int(-1));
ctx.set_field(ss, 4, Value::Object(Some(this)));
let fd = ctx.get_field(this, 2).as_int().unwrap_or(-1);
/* ... */ ctx.set_field(ss, 0, Value::Int(port));
ctx.set_field(this, 3, Value::Object(Some(ss)));
```

W7-72 §1.5 described this as writing *three* real JDK fields, naming the
registrar's channel map (`slot 3 = interruptedTarget`, `slot 1 = closed`,
`slot 2 = interruptor`). Both halves of that sentence need correcting, in
opposite directions, and §3 does it: `socket()` writes **one** of those three
channel slots — and **six more** on a second real JDK class the record never
mentions.

## 2. The ordering — measured, not read

W7-72 §2.1 and W7-86-static-native-arity.md §3 both insist a "which registration
wins" claim be measured. W7-86 records a static call-graph walk as a **failed
instrument** (`fn register` name collisions make everything reachable from every
root). So the primary evidence here is `--dump-native-registry`, run four times
on the prebuilt binary. The `owns_slot` column is the last-write-wins winner and
`overwrote` names whom it displaced.

| # | configuration | rows | `socket()` owner | `overwrote` |
|---|---|---:|---|---|
| 1 | default build, `--real-jdk` (the default; `mode = compatible`) | 11,666 | `native-io/src/socket_channel.rs:4776` | `null` |
| 2 | default build, `--jdk-only` (`mode = jdk-only`) | 10,386 | `native-io/src/socket_channel.rs:4776` | `null` |
| 3 | as 1, `CRATONVM_REAL=-net-sockets` | 11,748 | `native-io/src/socket_channel.rs:4776` | `null` |
| 4 | as 2, `CRATONVM_REAL=-net-sockets` | 10,386 | `native-io/src/socket_channel.rs:4776` | `null` |

**`overwrote = null` is the finding, and it is stronger than "it loses".** A
losing registration shows up as the winner's `overwrote`. Nothing was
overwritten, because `register_p58_nio_channels` never ran: in all four
censuses **every** `net_channels.rs` row is at line >= 1252, i.e. contributed by
`register_p67_async_channels` (line 1247) and later. Lines 70–804 —
`register_p58_nio_channels`, the whole SocketChannel / ServerSocketChannel /
Selector / SelectionKey surface — contribute **zero** rows.

Configuration 3 is worth keeping: `CRATONVM_REAL=-net-sockets` selects the
legacy synthetic socket surface (bridge count moves 9,741 -> 9,823, so the flag
really does change registrations) and is the mode
`vm/tests/server_socket_adaptor_accept.rs` describes as producing "a CratonVM
`ServerSocket` carrying a back-ref to a CratonVM channel" — which is exactly the
shape the deleted body built. It does **not** revive this registration.

### 2.1 Why it cannot run, source-level

The census says *that*; this says *why*, and covers the one arm no binary on
this host can measure. Every cited line was re-read with `sed`.

* `register_p58_nio_channels` — `native-builtins/src/phases_late/net_channels.rs:70`.
  Exactly one call: `phases_late.rs:1696`.
* `register_phase58_natives` — `phases_late.rs:1692`. Exactly one call:
  `native-builtins/src/lib.rs:23922`.
* That call sits at depth 1 inside `register_synthetic_overrides`
  (`lib.rs:21430`), which carries `#[cfg(feature = "synthetic-jdk")]` on
  `lib.rs:21429`. The next top-level `fn` is `lib.rs:24221`, so the call is
  inside that body and nowhere else.
* `register_synthetic_overrides` has one call: `lib.rs:21423`, inside
  `register_builtins` (`lib.rs:21419`), also `#[cfg(feature = "synthetic-jdk")]`.
* Without the feature, `vm/src/native/builtins.rs:29` supplies a **no-op shim**
  of the same name, so even the call site compiles to nothing.
* `vm/src/vm/vm_init.rs`: `#[cfg(feature = "synthetic-jdk")]` at 1835 wrapping
  `if config.use_synthetic_jdk {` at 1837 -> `register_builtins` 1839,
  `register_io_natives` **1840**; the `else` arm ->
  `register_essential_natives_with_shims` 1960, `register_io_natives` 2157;
  `#[cfg(not(feature = "synthetic-jdk"))]` at 2472 — the default `cratonvm-cli`
  build — 2498 and `register_io_natives` **2693**.

Both symbols are `pub(crate)`, so `native-builtins/src` is the whole search
space for callers, and the exact-identifier counts there are 2 and 2 (the `fn`
and the one call). The counting is not a brace scan — the two names are unique
identifiers, and `grep -F` over the workspace returns those two lines and no
comment hits.

| build / mode | p58 registers? | `native-io` registers? | winner |
|---|---|---|---|
| default build, `--real-jdk` (Compatible) | no | yes, `vm_init:2693` | **native-io** |
| default build, `--jdk-only` | no | yes, `vm_init:2693` | **native-io** |
| `synthetic-jdk` build, `--synthetic-jdk` | yes, via `vm_init:1839` | yes, via `vm_init:**1840**` — the next line | **native-io** |
| `synthetic-jdk` build, real-JDK arm | no | yes, `vm_init:2157` | **native-io** |

Rows 1–2 are measured (and again under `-net-sockets`). Rows 3–4 are
source-level: this host's binary reports `jdk.mode.synthetic_compiled_in = false`
(`-Xinternalversion`), so the feature build cannot be censused here. **That is
stated rather than glossed** — it is the one arm where the registration exists
at all, and it is the arm a reorder would have to touch.

The ambient `NativeKind` was checked, because W7-86 records a retirement-table
entry that did nothing for exactly this reason: `register_synthetic_overrides`
sets `Intrinsic`, but `register_phase58_natives` sets `Bridge` at its top and
`register_p58_nio_channels` sets `Bridge` again at line 72, so the registration
would have been `Bridge` — matching the winner's `kind: "bridge"`. The
Intrinsic/Bridge mix-up is not what is going on here.

## 3. What the writes land on — `javap -p`, Adoptium 25.0.3.9

### 3.1 `java.nio.channels.ServerSocketChannel` — the receiver

Chain `AbstractInterruptibleChannel` -> `SelectableChannel` (no instance fields)
-> `AbstractSelectableChannel` -> `ServerSocketChannel` (no instance fields).
Ten instance fields; `U`, `INTERRUPTED_TARGET` and `$assertionsDisabled` are
`static` and excluded.

| slot | field | type | declared by |
|---:|---|---|---|
| 0 | `closeLock` | `Object`, final | `AbstractInterruptibleChannel` |
| 1 | `closed` | `boolean`, volatile | `AbstractInterruptibleChannel` |
| 2 | `interruptor` | `sun.nio.ch.Interruptible`, final | `AbstractInterruptibleChannel` |
| 3 | `interruptedTarget` | `Object`, volatile | `AbstractInterruptibleChannel` |
| 4 | `provider` | `SelectorProvider`, final | `AbstractSelectableChannel` |
| 5 | `keys` | `SelectionKey[]` | `AbstractSelectableChannel` |
| 6 | `keyCount` | `int` | `AbstractSelectableChannel` |
| 7 | `keyLock` | `Object`, final | `AbstractSelectableChannel` |
| 8 | `regLock` | `Object`, final | `AbstractSelectableChannel` |
| 9 | `nonBlocking` | `boolean`, volatile | `AbstractSelectableChannel` |

This reproduces W7-72 §1.1's table, including `keys` at 5 — the slot that
record repaired on the winning side.

`socket()` touched **two** of these: it read slot 2 as an `int` fd
(`interruptor`, a reference — `.as_int()` answers `None`, so the fd was always
`-1` and the port mirror never ran), and it **wrote slot 3**, `interruptedTarget`.

That write is the worse of the two defects here, and worse than the `keys` write
W7-72 §1 repaired, because a real JDK reader consumes it on a hot path.
`AbstractInterruptibleChannel.end(boolean)` (`javap -c`) is:

```
 5: getfield  interruptedTarget       // if non-null:
14: getfield  interruptor
17: invokeinterface sun/nio/ch/Interruptible.postInterrupt:()V
22: ... if_acmpne currentThread -> ClosedByInterruptException
```

`end()` runs after **every** interruptible operation. A non-null
`interruptedTarget` therefore makes the channel call `postInterrupt()` on
whatever is in `interruptor` — and `interruptor` is the very slot the same
registrar's `bind`/`close` write an `int` fd into (lines 270/329/481 of the
pre-change file), which the W7-84 guard boxes into an `AUTOBOX_CLASS_ID`
wrapper. `invokeinterface` on that is not a subtle wrong answer.

### 3.2 `java.net.ServerSocket` — the object it fabricated

Extends `Object`. Six instance fields; `factory` and `$assertionsDisabled` are
`static`.

| slot | field | type | what the body wrote | its comment claimed |
|---:|---|---|---|---|
| 0 | `impl` | `SocketImpl`, final | `Int(0)`, later `Int(port)` | `SS_PORT` |
| 1 | `created` | `boolean`, volatile | `Int(50)` | `SS_BACKLOG` |
| 2 | `bound` | `boolean`, volatile | `Int(0)` | `SS_CLOSED` |
| 3 | `closed` | `boolean`, volatile | `Int(-1)` | `SS_LISTENER_ID` |
| 4 | `socketLock` | `Object`, final | the channel | `channel_ref` |
| 5 | `options` | `Set`, volatile | — | — |

The object is not short: `try_alloc_concurrent_synthetic` clamps
`n = num_fields.max(real)`, so a request for 5 against 6 declared allocates 6.
It is **mis-mapped**, the same species as W7-72 §2.2 and the same species
`native-api/src/read_alias.rs` was built for — an in-bounds write of the wrong
field, invisible to the allocation-width instrument because slot 0 exists.

Read as the real class:

* `created := 50` — nonzero, so **true**.
* `closed := -1` — nonzero, so the `ServerSocket` reports itself **CLOSED** to
  its own bytecode. `isClosed()` is `return closed`.
* `socketLock := <the channel>` — a `final` field the JDK synchronises on;
  every `synchronized (socketLock)` in `ServerSocket` would have locked the
  channel.
* `impl := 0` and later `:= port` — an `int` into a `SocketImpl` reference slot,
  the W7-84 primitive-in-reference-store species.

**So the count.** Seven real JDK field writes, not three: one on the channel
(`interruptedTarget`) and six on the `ServerSocket` (slot 0 twice). W7-72's
residual line named three because it was describing the registrar's *channel*
map, of which `socket()` writes one; the `java.net.ServerSocket` half is not in
that record at all. Recorded here so the census carries the real number.

## 4. Delete, not correct

Deleted. `native-builtins/src/phases_late/net_channels.rs`, commit
`fix(net-channels): delete the dead ServerSocketChannel.socket() registration`.

**Correct-in-place fails both ways, and that is the whole argument.** The map is
not simply wrong — it is right for a layout and wrong for another:

* Corrected for the **real** class, it would break the fabricated one it was
  written against. That is precisely the W7-66-live-over-allocations.md shape:
  a repair to dead code that would have broken the live path, correctly reverted.
* Corrected for the **fabricated** class — i.e. left as-is, which is what it
  already is — it is an inert fix, the trap eight lanes shipped on 2026-08-12,
  because the registration loses in that arm too (§2, row 3).

Deletion is a no-op **by construction**, and the construction is checkable:
removing an entry from a last-write-wins map changes nothing provided the later
writer always runs. It does. `register_io_natives` calls
`socket_channel::register_socket_channel_real` unconditionally
(`native-io/src/lib.rs:5679`; no `if`/`match` between the `fn` at 5609 and that
line), and `vm_init` calls `register_io_natives` in **all three** arms (1840,
2157, 2693). In two of the four configurations p58 does not even register.

### 4.1 The other eight triples — measured, and left

`register_p58_nio_channels` registers nine `ssc` triples. All nine are
re-registered later by `native-io`. Diffed against the configuration-1 census:

| p58 line (pre-change) | triple | later owner |
|---:|---|---|
| 240 | `open()Ljava/nio/channels/ServerSocketChannel;` | `socket_channel.rs:4759` |
| 253 | `bind(Ljava/net/SocketAddress;)L…/ServerSocketChannel;` | `socket_channel.rs:4829` |
| 310 | `bind(Ljava/net/SocketAddress;I)L…/ServerSocketChannel;` | `socket_channel.rs:4803` |
| **366** | **`socket()Ljava/net/ServerSocket;`** | **`socket_channel.rs:4776` — DELETED here** |
| 399 | `getLocalAddress()Ljava/net/SocketAddress;` | `socket_channel.rs:4878` |
| 430 | `accept()Ljava/nio/channels/SocketChannel;` | `socket_channel.rs:4835` |
| 470 | `isOpen()Z` | `socket_channel.rs:4777` |
| 474 | `close()V` | `socket_channel.rs:4798` |
| 484 | `configureBlocking(Z)L…/SelectableChannel;` | `socket_channel.rs:4786` |

The eight are dead the same way and by the same argument, and they write
`closeLock` / `closed` / `interruptor` on the same real class. **They are not
deleted**: this lane owns `socket()`, that is the row with a second class and
seven writes, and eight more deletions is a diff whose review cost is not paid
for by a landmine that is already published (§5). The table is here so a
follow-up lane does not re-derive it.

### 4.2 Compatible mode

**Compatible mode is not touched.** The registration is absent from the
Compatible-mode registry (§2, configurations 1 and 3, `overwrote = null`), so
there is nothing here to weigh against the freeze — no parity exception is being
claimed and none is needed. `--jdk-only` is likewise untouched (configurations
2 and 4). The `synthetic-jdk` arm is the only one where a row disappears, and it
disappears from a losing position.

No allocation width moves: `open`'s
`try_alloc_concurrent_synthetic("java/nio/channels/ServerSocketChannel", 4)` is
unchanged, so no `CRATONVM_DBG_LAYOUT_ALIAS` row for either class shifts —
deliberately, the same care W7-72 §1.3 took with `SC_OBJECT_SLOTS`.
`HEADER_SIZE` is not referenced. Nothing is bound by name.

## 5. The census records what was there — `SSC_P58_SLOT_MAP`

`native-builtins/src/phases_late/net_channels.rs` now declares and publishes

```rust
pub static SSC_P58_SLOT_MAP: cratonvm_native_api::read_alias::SlotMap = …
    class: "java/nio/channels/ServerSocketChannel",
    slots: &[(0, "open"), (1, "bound"), (2, "fd"), (3, "socket")],
```

stating the registrar's **belief**, not `javap` — the W7-77 rule, because a map
that publishes the correct answer sweeps clean and measures nothing. Every entry
disagrees with §3.1, and the disagreement is the row. Slot 3 kept its entry: it
lost its only producer with `socket()`, but `open` still nulls it and two `bind`
arms still read it, so the belief is still held.

Two things this deliberately does **not** claim. It is published from a
registrar that does not run in any measurable configuration, so
`verify_declared_slot_maps` will sweep nothing for it today — that is honest
(the code holding the belief does not run either) and it is the same position
`MONTH_SLOT_MAP` is in, per W7-77 §2. And it is not a guard: unlike the other
four rows in `native-api/tests/guarded_slot_maps.rs`, there is no runtime
witness, because there is no runtime.

Also corrected, in passing: the file header comment said
`ServerSocketChannel = 3-field synthetic (open=0, bound=1, fd_id=2)`. It had
been wrong since the slot-3 cache was added — `open` allocates 4 and every
`ssc` body indexes 0..=3. A comment outlives its defect.

## 6. Proving it

**There is no runtime observable, and inventing one would be the defect this
campaign is about.** The row does not leave `--dump-native-registry`, because it
was never in it. The census is byte-identical before and after in all four
runnable configurations. Anything that changes is in the `synthetic-jdk` feature
build, where the row was already losing.

So the proof is in two parts, and neither pretends to be a behaviour change.

**`probes/SscSocketOwnerProbe.java` — every line marked NO-CHANGE.** It exists
to make the census name the row that served the call. Six values, all read back
through real JDK accessors (`getClass`, `isClosed`, `isBound`, `getLocalPort`,
`isOpen`) and never through the native that wrote the field — the W7-72 rule.
HotSpot 25.0.3.9 and CratonVM (default, pre-change) agree on all six;
`socket.class` is `sun.nio.ch.ServerSocketAdaptor` on both, which is the JDK's
own adaptor and not a fabricated `java.net.ServerSocket`. The census from the
same run:

```
java/nio/channels/ServerSocketChannel.socket ()Ljava/net/ServerSocket;
   registered_by = native-io/src/socket_channel.rs:4776
   owns_slot = true   overwrote = null   invocations = 1
```

`invocations = 1` closes the last door: the call reached *that* row.

**A ratchet — `ssc_p58_socket_stays_deleted_and_the_registrar_stays_gated`** in
`native-api/tests/guarded_slot_maps.rs`, joining that file as row 5. Three
assertions, one per fact the deletion rests on: the registration stays deleted;
`native-io` still registers the winner; and `register_p58_nio_channels` /
`register_phase58_natives` each keep exactly one call site, behind
`#[cfg(feature = "synthetic-jdk")] register_synthetic_overrides`. A measured
population closes by becoming a ratchet.

### 6.1 The scanner, validated before it was trusted

The task named brace-scanning as the thing that burned four lanes on
2026-08-12. Two defences were used.

First, the ordering claims in §2 do not rest on a scan at all: they rest on the
census, and on `sed` re-reads of every cited line. `register_p58_nio_channels`
and `register_phase58_natives` are unique identifiers, so their call sites came
from `grep -F` over the workspace (two hits each, no comment hits), not from a
depth counter.

Second, the ratchet reuses `guarded_slot_maps.rs`'s existing `fn_body`, and it
was run against ground truth on all four target functions before any assertion
was written on it:

| function | file | body returned | ends at |
|---|---|---|---|
| `register_p58_nio_channels` | net_channels.rs | 70..804 | `r.set_category(__prev_cat);` then `}`, followed by `p98_extract_socket_addr`'s doc comment |
| `register_socket_channel_real` | socket_channel.rs | 4524..4972 | `r.set_category(__prev_cat);` then `}` |
| `register_synthetic_overrides` | lib.rs | 21430..24211 | `tracing::info!(… "Registered synthetic overrides");` then `}` |
| `register_phase58_natives` | phases_late.rs | 1692..1703 | `}` |

**That validation caught a real bug in the first spelling of the gate.**
Assertion (a) originally matched the bare descriptor `()Ljava/net/ServerSocket;`
and went **RED on the unmutated tree** — because the comment left in place of
the deleted body names the triple it replaced. A gate that fires on the tree it
ships with gets muted, not investigated. It now matches the descriptor as a Rust
string literal, quotes included, which has exactly one meaning inside a
registrar. All five assertions were then re-run against the tree and pass.

Both edited files were parse-checked with `rustfmt --check` (which writes
nothing, and is not `cargo fmt` — the tree is not fmt-clean and no formatting
was applied): no parse errors.

## 7. What the orchestrator's build must show

1. **`cargo test -p cratonvm-native-api --test guarded_slot_maps`** — green,
   including the new `ssc_p58_socket_stays_deleted_and_the_registrar_stays_gated`
   and the extended `every_guarded_row_publishes_its_slot_map` (five rows) and
   `the_published_maps_state_the_belief_not_the_truth` (three rows).
2. **`cargo test -p cratonvm-native-api --test read_alias_coverage`** — green.
   `every_declared_slot_map_is_published` must find
   `declare_slot_map(&SSC_P58_SLOT_MAP)`; the printed `census` gains no row and
   loses nine constant-slot accessor calls in `native-builtins` (the seven field
   writes of §3 plus the two slot reads), which is printed, not a ratchet.
3. **`--dump-native-registry` is UNCHANGED** in all four configurations of §2.
   Specifically `java/nio/channels/ServerSocketChannel.socket ()Ljava/net/ServerSocket;`
   still reads `registered_by = native-io/src/socket_channel.rs:4776`,
   `owns_slot = true`, `overwrote = null`. **A changed row here is a RED**, and
   means the ordering derived in §2 was wrong.
4. **`probes/SscSocketOwnerProbe.java` on the new binary** — the six lines of
   `probes/SscSocketOwnerProbe.expected.txt`, identical to HotSpot and to the
   pre-change binary, with `invocations = 1` on that row. Any change is a RED.
5. `vm/tests/server_socket_adaptor_accept.rs` — green in both socket modes, and
   it is the integration coverage for the surface that actually serves this
   triple.

Nothing else should move. If a suite that was 70/0 goes red, the ordering claim
in §2 is the thing to re-measure first.

## 8. Left open

1. **The other eight `ssc` triples** in `register_p58_nio_channels` (§4.1).
   Dead by the same argument, writing `closeLock`/`closed`/`interruptor` on the
   same real class. `SSC_P58_SLOT_MAP` now publishes their belief, so they are
   censused rather than silent. Deleting them is a mechanical follow-up with the
   twin table in §4.1 as its evidence.
2. **The `sc` (SocketChannel), `sel` (Selector) and `sk` (SelectionKey) blocks**
   in the same registrar were not examined. They are in the same dead function
   and the same census rows are absent, so the *liveness* half is already
   answered for them; the *layout* half is not. `java.nio.channels.SocketChannel`
   shares the `AbstractInterruptibleChannel` prefix, so a 4-slot map over it has
   the same shape of collision.
3. **The `synthetic-jdk` arm is source-level only.** No binary on this host has
   the feature (`jdk.mode.synthetic_compiled_in = false`), so rows 3–4 of §2.1's
   table are read, not measured. That arm is the only one where the deleted
   registration ever existed; a build with the feature should re-census it once,
   and the expected result is one fewer losing row and no change to `owns_slot`.
4. **W7-72 §1.5's description of this row is superseded** by §3 here — three
   channel-map slots named, one of which `socket()` wrote, and a second real JDK
   class not mentioned. Worth carrying forward as a reading lesson: a
   registrar's slot map and a single body's writes are different sets, and the
   residual named the first while describing the second.

---

## 9. Landed-state check, 2026-08-12 — on the live path, and what a run must still add

Source re-read on this tree by a later lane (still **no build, no binary**). All
four facts the deletion rests on hold, and every line number in §2.1/§4 has
drifted under concurrent edits, so **anchor on the identifiers, not the lines**:

| fact | where it is now | was |
|---|---|---|
| the registration stays deleted, with the tombstone comment naming the triple | `net_channels.rs`, in `register_p58_nio_channels` | line 366 |
| `SSC_P58_SLOT_MAP` declared and published | `net_channels.rs` (`declare_slot_map(&SSC_P58_SLOT_MAP)`) | — |
| the winner still registers unconditionally | `socket_channel.rs`, `r.register(c, "socket", "()Ljava/net/ServerSocket;", ssc_socket)` in `register_socket_channel_real`, called from `native-io/src/lib.rs` with no `if` above it | `socket_channel.rs:4776` → **4778** |
| the loser's registrar is still gated and still has exactly one call site each | `register_p58_nio_channels` ← `phases_late.rs` (one call) ← `register_phase58_natives` ← `lib.rs` (one call), inside `pub fn register_synthetic_overrides` under `#[cfg(feature = "synthetic-jdk")]`; the next top-level `fn` in `lib.rs` is far below that call | `lib.rs:23922` → **24018**; the `fn` at `21430` → **21526** |
| the ratchet exists | `ssc_p58_socket_stays_deleted_and_the_registrar_stays_gated` in `native-api/tests/guarded_slot_maps.rs` | — |

**This is a source verification and it does not upgrade §7.** Everything in §7
still needs the run, and §7 item 3 is the one that matters: the census must be
**byte-identical** in all four configurations. Two riders a build lane should not
skip:

* **the `--dump-native-registry` row must still read
  `registered_by = native-io/src/socket_channel.rs:<line>` with `owns_slot = true`
  and `overwrote = null`** — the line number in §6 is now stale by two, which is
  exactly the drift §2.1 warns about, and a lane diffing the census text rather
  than the fields will read that as a change;
* **no fixture assertion is possible for this record and none was added.** §6
  says why in full — the row was never in the registry, so there is nothing a
  Java-visible predicate can move. `probes/SscSocketOwnerProbe.java` is
  NO-CHANGE by construction and is still not scheduled (`run.sh` reads a word
  list, not `probes/`). This is a record that closes on a census diff, not on a
  vector.

---

## 10. The OTHER registrar in this file is LIVE, 2026-08-12 — and its two live AFC triples are both broken

Source read on this tree; **no build, no binary, no `cargo`**. Ordering
re-measured from `vm/src/vm/vm_init.rs`; JDK layouts are `javap -p` against
Adoptium 25.0.3.9 on this host, the same oracle and convention as §3.

This record's title, its §2 census and its §8 residual list all say
`net_channels.rs` in the voice of a file that does not run. That is true of
`register_p58_nio_channels` and **false of the file.** §2's own census says so
and the sentence was read the wrong way round: *"in all four censuses every
`net_channels.rs` row is at line >= 1252, i.e. contributed by
`register_p67_async_channels` (line 1247) and later."* Those rows are **present
in all four configurations**, including `--jdk-only`. `register_p58_nio_channels`
registers nothing; `register_p67_async_channels` registers into every shipping
binary, and it is the next function down.

The call chain is different, and that is the whole reason:

| | `register_p58_nio_channels` (§2) | `register_p67_async_channels` (this section) |
|---|---|---|
| declared at | `net_channels.rs:70` | `net_channels.rs:1307` |
| reached from | `phases_late.rs` → `register_phase58_natives` → `native-builtins/src/lib.rs`, inside `#[cfg(feature = "synthetic-jdk")] register_synthetic_overrides` — **one** call site | `native-builtins/src/lib.rs:7943`, inside **`register_essential_natives_with_shims`** (`lib.rs:7103`) — *and* a second time in the synthetic arm via `register_phase67_natives` (`lib.rs:24073`) |
| `vm_init` reaches it | only the `synthetic-jdk` feature build in `--synthetic-jdk` mode | `:2028` (feature build, real-JDK arm) and **`:2566`** (`#[cfg(not(feature = "synthetic-jdk"))]` at `:2540` — the shipping `cratonvm-cli`) |
| in the census | zero rows | all rows at `>= 1252` |

So a reader who takes "this file is dead" from §1–§9 and applies it below line
1247 reaches the wrong conclusion — which is this campaign's own
`chk dev`/losing-registration failure mode, committed by a record *about* that
failure mode. The correction is filed here rather than in the title because the
title's claim, scoped to the function it names, is still exactly right.

### 10.1 What survives, per triple

`register_io_natives` runs **after** `register_essential_natives_with_shims` in
all three `vm_init` arms (`:1908`, `:2225`, `:2761`), so everything `native-io`
re-registers overwrites this registrar. Diffed triple by triple against
`native-io/src/lib.rs::register_async_file_channel`,
`native-io/src/nio_native.rs::register_t16_channel_overrides` and
`native-io/src/async_socket.rs::register_async_socket_real`:

| this file | triple | later owner | live here? |
|---:|---|---|:-:|
| `:1312` | `AFC.open(Path,[OpenOption])` | `native_afc_open` → `t16_afc_open` | no |
| `:1325` | `AFC.read(ByteBuffer,J)Future` | `native_afc_read` | no |
| `:1389` | `AFC.write(ByteBuffer,J)Future` | `native_afc_write` | no |
| `:1470` | `AFC.size()J` | `native_afc_size` → `t16_afc_size` | no |
| `:1481` | `AFC.truncate(J)` | `native_afc_truncate` | no |
| **`:1487`** | **`AFC.force(Z)V`** | **nobody** | **YES** |
| **`:1506`** | **`AFC.lock()Future`** | **nobody** | **YES** |
| `:1517` | `AFC.close()V` | `native_afc_close` → `t16_afc_close` | no |
| `:1522` | `AFC.isOpen()Z` | `native_afc_is_open` → `t16_afc_is_open` | no |
| `:1565`/`:1579` | `ASC.open()` ×2 | `aio_asc_open` / `aio_asc_open_group` | no |
| **`:1593`** | **`ASC.connect(SocketAddress)Future`** | **nobody** (`native-io` registers only the `(SocketAddress,Object,CompletionHandler)V` form) | **YES** |
| `:1624`/`:1693` | `ASC.read/write(ByteBuffer)Future` | `aio_asc_read_future` / `aio_asc_write_future` | no |
| `:1765`/`:1775`/`:1779` | `ASC.close`/`isOpen`/`getRemoteAddress` | `aio_asc_close` / `aio_asc_is_open` / `aio_asc_remote_address` | no |
| `:1791`/`:1805` | `ASSC.open()` / `bind(SocketAddress)` | `aio_assc_open` / `aio_assc_bind` | no |
| **`:1811`** | **`ASSC.accept()Future`** | **nobody** (`async_socket.rs:3516` registers only `accept(Object,CompletionHandler)V`) | **YES** |
| `:1822` | `ASSC.close()V` | `aio_assc_close` | no |

**One row this corrects in another record.** The comment at `:1539-1541` says
*"The one survivor is `connect(SocketAddress)Future` — native-io registers only
the `(SocketAddress, Object, CompletionHandler)V` form"*, and W7-49 records the
same. That was true of `connect` and has since become **false as a statement
about the block**: `native-io` now registers `asc.read`/`asc.write` in both
forms, so those two joined the dead, while `ASSC.accept()Future` — which the
comment does not mention — was a survivor all along. Four survivors, not one.

### 10.2 The two AFC survivors are both fabricated success

Both are adjudicated in full in
[W7-8-fabricated-success-io-sweep.md](W7-8-fabricated-success-io-sweep.md) §9,
which is the fabricated-success anchor and carries the patches as out-of-file
items 7–9. In one line each, because this is the file they live in:

* **`force(Z)V` (`:1487`) reads slot 0 as a path `String`.** Every
  `AsynchronousFileChannel` in this VM is allocated by `native-io`'s
  `alloc_afc_channel` with `Int(fd)` in slot 0, so the `match` takes `_ =>` and
  **returns `Ok(None)` before touching its first argument.** `force(true)` — the
  durability barrier H2's async file store calls — does nothing, reports nothing,
  and its scheduled fixture (`RJdkAsyncChannel.java:143-144`, in
  `JDKONLY_CLASSES`) asserts only `isOpen()` afterwards, which the no-op
  satisfies. Three further defects are stacked behind the slot error: the
  `sync_all`/`sync_data` polarity is inverted, the fsync is issued on a
  *second* descriptor opened by path (so none of the channel's buffered writes
  are flushed, and the open fails outright on a read-only channel), and there is
  no closed-channel refusal.
* **`lock()Future` (`:1506`) mints `java/util/concurrent/FutureTask` with two
  field-index writes.** That is a real `java.base` class: slot 0 is `state` and
  slot 1 is `callable`. `Object(None)` into the `int` slot coerces to `Int(0)` =
  `NEW` (`gc/src/heap.rs:1657`) and the `Int(1)` meant as "done" lands on
  `callable` and degrades to null (`:1674`). Real `FutureTask.get()` sees
  `state <= COMPLETING`, enters an untimed `awaitDone`, and **parks forever**;
  `isDone()` is `false` for the life of the process. `ASSC.accept()` at `:1811`
  is the identical mint and the identical hang.

This is the same species §3 records — *"an in-bounds write of the wrong field …
invisible to the allocation-width instrument because slot 0 exists"* — with two
aggravations §3's row does not have. The receiver class is one the JDK's own
hot-path bytecode reads (`AbstractInterruptibleChannel.end()` was §3.1's
argument; `FutureTask.awaitDone` is a stronger one, because it does not merely
misbehave, it does not return). And unlike §3's row, **these are live**:
`--dump-native-registry` will show `owns_slot = true`, `overwrote = null` for
`java/nio/channels/AsynchronousFileChannel.force (Z)V` and
`.lock ()Ljava/util/concurrent/Future;` in all four configurations of §2.

### 10.3 The fix is already in this file, applied to one caller

`aio_completed_future` (`native-builtins/src/phases_late/concurrent.rs:1369`) is
four lines over `CompletableFuture.completedFuture`, and its own doc comment at
`:1366-1368` states this exact diagnosis:

> *"A synthetic `FutureTask` does NOT work here: in real-JDK mode
> `FutureTask.get()` runs the real bytecode (reads the real `state` field, stuck
> NEW) → the websocket client's `fConnect.get(timeout)` TimeoutException."*

It was applied to `asc.connect` (`:1621`) and to `asc.read`/`asc.write`
(`:1690`, `:1762`) — and to none of the four `FutureTask` mints in the same
function. The diagnosis was written down and then applied at the call site whose
test was failing, not to the shape. **Grep the shape, not the failing test:**
`try_alloc_concurrent_synthetic(ctx, "java/util/concurrent/FutureTask", 2)`
returns four hits, all in this file (`:1383`, `:1464`, `:1511`, `:1816`), and a
ratchet on that string is worth more than any of the four individual repairs.

### 10.4 What this does and does not change about §8

§8 residual 2 says the `sc` / `sel` / `sk` blocks in
`register_p58_nio_channels` were not examined, and that *"the liveness half is
already answered for them"*. That still holds — they are inside the dead
registrar. What did not hold is the unstated extension of it to the rest of the
file. Restated as a residual: **`register_p67_async_channels` (`:1307`–`:1830`)
is live in both shipping modes and has now been audited for liveness per triple
(§10.1) but not for layout.** `AsynchronousSocketChannel`'s two-maps-on-one-class
condition, quoted at `:1544-1548` and measured by W7-49, is still unrepaired and
is now known to sit under a **surviving** `connect` — so slots 0 and 2 of a
`native-io`-allocated channel are being written under a map whose slot 0 means
the opposite. That is `:1550`'s own sentence, and it needs the cross-crate lane
it names.
