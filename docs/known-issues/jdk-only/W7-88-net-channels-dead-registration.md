# W7-88 — the dead `ServerSocketChannel.socket()` in `net_channels.rs`

Status: **deleted**. Residual 7 of W7-72-ssc-socket-and-filechannel.md is
discharged. The registration turned out to be one step deader than that record
believed — it does not merely *lose*, it never *registers* in any configuration
a user can run — and the field writes it carried were worse than the two the
record named.

Branch `fix/net-channels-losing-registration-20260812`.

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
