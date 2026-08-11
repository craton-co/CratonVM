# W7-9 — minted interface/abstract receivers: the non-stream half of W7-5's set

**Status:** adjudicated, 2026-08-11. Nine of eleven classes are **already
covered** in the default build; the census's list of *minted* classes is right
and its implied list of *exposed* classes is much shorter than it looks. Five
residual triples are named below, four of which cannot be fixed from this lane's
files.

**Nothing here has been built or run.** Every claim is either `javap` output from
the JDK 25 image on this host (`Eclipse Adoptium jdk-25.0.3.9-hotspot`, `javap
-version` = `25.0.3`) or a read of the tree at `7d4d545e0`. No measurement is
claimed.

---

## 1. What was being tested

W7-5 established that the VM mints instances **of the real class** — an
`ensure_class_initialized("java/util/Spliterator")` gives back the real
interface's `ClassId`, so a receiver's runtime class *is* the interface. On such
a receiver an **abstract** declaration has no `Code` to fall back on, and
`interpreter.rs`'s `if !has_code` arm raises `AbstractMethodError` unless a
native is registered for the exact triple.

W7-5 also split the risk correctly: **only the abstract declarations are
load-bearing.** A `default` method has real `Code` in the JDK 25 classfile and
runs; a `static` keeps the native check in real-JDK mode and is a *shadowing*
risk rather than an `AbstractMethodError` risk.

This record takes the non-stream half of the minted set:
`java/util/Spliterator`, `java/util/Map$Entry`,
`java/nio/channels/{Selector,SelectionKey,DatagramChannel}`,
`java/nio/file/{Path,WatchService,WatchKey,WatchEvent}`,
`java/util/concurrent/{CompletableFuture,ScheduledFuture}`.

## 2. Why registering on these class names is safe — the mechanism, not a hope

`docs/architecture/natives-over-real-jdk-classes.md` §1 says registration itself
is the gate on the cold paths, which reads as "a native on a real JDK class
shadows its bytecode for every instance". For *these* class names it does not,
and the reason is mechanical rather than lucky:

**Every native-shadow hierarchy walk in the tree is a `superclass` walk. None of
them walks interfaces.** Source-verified at all three sites:

* `vm/src/runtime/interpreter/invoke.rs`, `try_stackless_invoke`'s step-1
  `or_else` — `let parent_id = cm.get_class(cid)?.superclass?;`
* `vm/src/runtime/interpreter/dispatch_virtual.rs`, the vtable fast path —
  `while let Some(parent_id) = cm.get_class(cid).and_then(|c| c.superclass)`
* `dispatch_virtual.rs`, `populate_virtual_invoke_cache` — the same loop again.

So a native registered on an **interface** name is reachable only when the
receiver's runtime class *is* that interface — i.e. only for a synthetic mint.
`native-builtins/src/lib.rs` already states the same rule in place, for the JDBC
surface: *"the rest of the surface is registered on `java/sql/*` INTERFACES,
which do not intercept an implementation class"*.

For the two **abstract classes** in this set (`Selector`, `SelectionKey`,
`DatagramChannel`) the walk *can* reach them, and the protection is different
but still structural: each walk checks the parent's native **and then** stops on
`has_bytecode`. An `abstract` declaration is by definition implemented by some
concrete class below it, so the walk finds that implementation's bytecode before
it ever reaches the abstract declaration's native. **The safety property is
exactly "abstract only".** Registering a *concrete* method of `Selector` (e.g.
`select(Consumer,long)`, which no `SelectorImpl` overrides) would be found by
the walk on a real `WEPollSelectorImpl` and would shadow it. Same for the two
`NetworkChannel` bridge methods on `DatagramChannel`.

**Corollary for the slot-index species (W4-4 / W6-3).** A Java interface
declares no instance fields, so for the eight interfaces here
`class_num_total_fields` is 0, `try_alloc_concurrent_synthetic`'s
`num_fields.max(real)` keeps the caller's count, and slots `0..n` are the
native's own. The species cannot bite. It bites on the three **classes** — see
§5.

## 3. Per-class adjudication

`javap -p` on JDK 25 for every row. "Registered" means *by a registrar that
actually runs in the default (`synthetic-jdk`-off) build*, resolved through the
call graph rather than grepped — see §4 for the two places a grep would have
lied.

| class | kind | minted? | abstract | default | static | abstract registered live | missing |
|---|---|---|---|---|---|---|---|
| `java/util/Spliterator` | interface | yes | 4 | 4 | 0 | 4 | **0** |
| `java/util/Map$Entry` | interface | yes | 5 | 0 | 5 | 5 | **0** |
| `java/nio/channels/Selector` | abstract class | yes | 9 | 0 (3 concrete) | 1 | 8 | **1** — `provider()` |
| `java/nio/channels/SelectionKey` | abstract class | yes | 7 | 0 (6 concrete `final`) | 0 | 7 | **0** |
| `java/nio/channels/DatagramChannel` | abstract class | yes | 14 | 0 (3 concrete + 2 bridges) | 2 | 10 | **4** |
| `java/nio/file/Path` | interface | yes | 21 | 11 | 2 | 21 | **0** |
| `java/nio/file/WatchService` | interface | yes | 4 | 0 | 0 | 4 | **0** |
| `java/nio/file/WatchKey` | interface | yes | 5 | 0 | 0 | 5 | **0** |
| `java/nio/file/WatchEvent` | interface | yes | 3 | 0 | 0 | 3 | **0** |
| `java/util/concurrent/CompletableFuture` | **concrete class** | yes | **0** | — | many | n/a | **n/a — no abstract surface exists** |
| `java/util/concurrent/ScheduledFuture` | interface | yes | **0 declared** | 0 | 0 | n/a | **n/a — declares nothing** |

### The splits, in full

**`java/util/Spliterator`** — abstract: `tryAdvance(Consumer)Z`, `trySplit()`,
`estimateSize()J`, `characteristics()I`. default: `forEachRemaining(Consumer)V`,
`getExactSizeIfKnown()J`, `hasCharacteristics(I)Z`, `getComparator()`. static:
none. All four abstracts are registered by
`native-collections`'s `register_iterator_protocol_natives`, reached from
`register_collections_natives` (`vm_init.rs:2434`). The `default` three are also
registered, by `phases_late/streams.rs::register_p59_spliterator` — which is
`pub(crate)` and reached only through `register_synthetic_overrides`, so it is
**dead in the default build**. That costs nothing: those three have real `Code`,
and `forEachRemaining`'s real body is a loop over `tryAdvance`, which *is* ours.

**`java/util/Map$Entry`** — abstract: `getKey`, `getValue`, `setValue`,
`equals(Object)Z`, `hashCode()I`. default: none. static: `comparingByKey` ×2,
`comparingByValue` ×2, `copyOf` (plus four synthetic `lambda$…` and
`$deserializeLambda$`, which are `private static` and unreachable by name). All
five abstracts registered by `native-collections`'s `register_interface_natives`
(`register_collections_natives`, `vm_init.rs:2434`). Note `equals`/`hashCode`
never actually reach the abstract declaration — an interface's `super_class` is
`java/lang/Object`, whose concrete bodies resolve first — so those two
registrations are belt-and-braces rather than load-bearing.

**`java/nio/channels/Selector`** — abstract: `isOpen()Z`, `provider()`,
`keys()`, `selectedKeys()`, `selectNow()I`, `select(J)I`, `select()I`,
`wakeup()`, `close()V`. **Not default methods but concrete instance bodies**
(it is a class): `select(Consumer,long)I`, `select(Consumer)I`,
`selectNow(Consumer)I`, plus `private doSelect`. static: `open()`.
Eight of nine abstracts are registered by
`native-io/src/nio_selector.rs::register_nio_selector_real`, live via
`register_nio_selector` ← `register_io_natives` (`vm_init.rs:2403`).
**`provider()Ljava/nio/channels/spi/SelectorProvider;` is registered nowhere in
the tree** — see §6.

**`java/nio/channels/SelectionKey`** — abstract: `channel()`, `selector()`,
`isValid()Z`, `cancel()V`, `interestOps()I`, `interestOps(I)`, `readyOps()I`.
Concrete `final`: `isReadable`, `isWritable`, `isConnectable`, `isAcceptable`,
`attach`, `attachment`; concrete non-final: `interestOpsOr(I)I`,
`interestOpsAnd(I)I`. static: none. All seven abstracts are registered by
`register_nio_selector_real`, which also registers `attach`/`attachment` —
those two are `final` concrete methods on a real abstract class, so they *are* a
shadow of real bytecode, but only for a receiver whose class is literally
`SelectionKey`, i.e. a mint. Left alone.

**`java/nio/channels/DatagramChannel`** — abstract (14): `bind(SocketAddress)`,
`setOption(SocketOption,Object)DatagramChannel`, `socket()`, `isConnected()Z`,
`connect(SocketAddress)`, `disconnect()`, `getRemoteAddress()`,
`receive(ByteBuffer)`, `send(ByteBuffer,SocketAddress)I`, `read(ByteBuffer)I`,
`read([ByteBuffer,I,I)J`, `write(ByteBuffer)I`, `write([ByteBuffer,I,I)J`,
`getLocalAddress()`. Concrete: `validOps()I` (`final`), `read([ByteBuffer)J`
(`final`), `write([ByteBuffer)J` (`final`), plus the two `NetworkChannel`
covariant bridges. static: `open()`, `open(ProtocolFamily)`.
Ten abstracts registered live by `native-io/src/lib.rs::register_datagram_channel`
(reached twice from `register_io_natives`, once directly and once via
`register_phase92_io_completeness`). **Missing: `setOption(SocketOption,Object)`,
`getRemoteAddress()`, `read([Ljava/nio/ByteBuffer;II)J`,
`write([Ljava/nio/ByteBuffer;II)J`.** See §6 for why they are not fixed here.

**`java/nio/file/Path`** — abstract (21): `getFileSystem`, `isAbsolute`,
`getRoot`, `getFileName`, `getParent`, `getNameCount`, `getName(I)`,
`subpath(II)`, `startsWith(Path)`, `endsWith(Path)`, `normalize`,
`resolve(Path)`, `relativize(Path)`, `toUri`, `toAbsolutePath`,
`toRealPath(LinkOption...)`, `register(WatchService,Kind[],Modifier...)`,
`compareTo(Path)`, `equals(Object)`, `hashCode`, `toString`. default (11):
`startsWith(String)`, `endsWith(String)`, `resolve(String)`,
`resolve(Path,Path...)`, `resolve(String,String...)`, `resolveSibling(Path)`,
`resolveSibling(String)`, `toFile`, `register(WatchService,Kind...)`,
`iterator`, `compareTo(Object)`. static (2): `of(String,String...)`, `of(URI)`.
**All 21 abstracts are covered**, across three live registrars:
`phases_late/nio_file.rs::register_phase57_nio_file` (`vm_init.rs:2473`) carries
19 of them, `native-io`'s `register_nio_file_natives` (`vm_init.rs:2403`)
overlaps on 18, and the one neither has —
`register(WatchService,Kind[],Modifier...)` — is registered by
`native-io/src/lib.rs::register_watch_service`, live via
`register_phase92_io_completeness` ← `register_io_natives`.

**`java/nio/file/WatchService`** — abstract: `close()V`, `poll()`,
`poll(J,TimeUnit)`, `take()`. No defaults, no statics. All four registered by
`register_watch_service` (live, `vm_init.rs:2403`).

**`java/nio/file/WatchKey`** — abstract: `isValid()Z`, `pollEvents()`,
`reset()Z`, `cancel()V`, `watchable()`. No defaults, no statics. All five
registered by `register_watch_service`.

**`java/nio/file/WatchEvent`** — abstract: `kind()`, `count()I`, `context()`.
No defaults, no statics. All three registered by `register_watch_service`.

## 4. Two census claims refuted, and how a grep would have gotten them wrong

**Refuted — `CompletableFuture` contributes no `AbstractMethodError` surface.**
It is listed in W7-5's minted set, and that part is true (182 registrations'
worth of mints; `try_alloc_concurrent_synthetic(ctx,
"java/util/concurrent/CompletableFuture", …)` appears in `http2.rs`,
`http_client.rs`, `phases_late/concurrent.rs`, `util_concurrent_ext.rs`,
`native-collections`). But `javap -p java.util.concurrent.CompletableFuture` on
JDK 25 contains **zero** occurrences of `abstract`: it is a concrete class, every
method has `Code`, and the `!has_code` arm can never fire on it. Whatever else is
wrong with the CompletableFuture mints, it is not this defect class. (What *is*
wrong with them is in §5.)

**Refuted — `ScheduledFuture` declares nothing at all.** `javap -p
java.util.concurrent.ScheduledFuture` is three lines: the header, and the closing
brace. Every method a caller reaches (`cancel`, `isCancelled`, `isDone`, `get`,
`get(J,TimeUnit)`, `getDelay`) is declared on `Future` or `Delayed`. So there is
no `java/util/concurrent/ScheduledFuture.*` triple to register: a fix for a
minted `ScheduledFuture` has to be registered on `java/util/concurrent/Future`
or `java/util/concurrent/Delayed` — which is **not** the safe shape described in
§2, because those two interfaces are implemented by every real `FutureTask` and
`CompletableFuture` in the process and a registration on them is reachable by
interface resolution from a genuine implementation that inherits rather than
overrides. Do not "fix" ScheduledFuture by registering on `Future`.

**Two places a literal grep gives the wrong answer** — W7-5's own methodology
warning, in a new form:

1. `native-io/src/nio_selector.rs::register_nio_selector_real` has **no direct
   caller by that name** anywhere outside its own file, which reads as a dead
   registrar. It is live: the four-line `register_nio_selector` wrapper (same
   file) calls it, and `register_io_natives` calls the wrapper. Twenty-one
   Selector/SelectionKey triples turn on that one hop.
2. `native-io/src/lib.rs::register_selector` (a *different* function, same crate,
   registering an overlapping 20 triples on the same two classes) genuinely **is
   dead** — `register_phase92_io_completeness` used to call it and the call was
   deliberately removed, with a comment saying `nio_selector.rs` is now the
   source of truth. Counting either function's `register(` lines as coverage
   without resolving the call graph gets the answer wrong in **both**
   directions.

## 5. A new §5 (slot-index) residual, not covered by W4-4 / W6-3 / W6-12

Not this record's defect, but found by the field-count check the wiring
instructions demanded, and it is the heap-corruption species rather than a wrong
answer, so it is written down here rather than dropped.

`try_alloc_concurrent_synthetic` computes `n = num_fields.max(real)` and only
reports (`report_layout_alias`) when `num_fields < real`. When
`num_fields > real` — the case for every row below — it is silent, and the
native's slot indices land on top of the real class's declared instance fields:

| mint | requested | real instance fields (`javap -p`) | slot 0 in the real layout |
|---|---|---|---|
| `SelectionKey` (`phases_late/net_channels.rs`, `servlet.rs`, `native-io/src/lib.rs`) | 4 | 1 — `private volatile Object attachment` | a **reference** the GC scans as an oop |
| `CompletableFuture` (many sites, 2/3/4 fields) | 2–4 | 2 — `volatile Object result`, `volatile Completion stack` | both **references** |
| `DatagramChannel` (`native-io`, `phases_late/net_channels.rs`) | 5 | inherited from `AbstractSelectableChannel` / `AbstractInterruptibleChannel` | mixed |

`net_channels.rs`'s own comment says the layout out loud — *"SelectionKey =
4-field synthetic (channel=0, selector=1, interestOps=2, readyOps=3)"* — and
slots 2 and 3 are written with `Value::Int`. On a real-JDK `SelectionKey` class
id, slot 0 is `attachment`; the ints go into slots the real layout does not
declare at all, which is the benign half. The sharp half is the mirror case:
`sk_attach`/`sk_attachment` in `nio_selector.rs` and the `interestOps` writers
disagree about what slot 0 means. This is exactly the shape of
`W6-3-slot-index-species-residuals.md`, in three files it does not name. It needs
a lane that owns `native-io` and `servlet.rs`.

The three interfaces-only classes are immune for the reason in §2's corollary.

## 6. The five residual triples, and why four are not fixed here

**`java/nio/channels/Selector.provider()Ljava/nio/channels/spi/SelectorProvider;`**
— confirmed missing everywhere (`"provider"` appears as a registered method name
on no class in `native-io`, `native-builtins` or `native-collections`). It is
**not fixed here, deliberately**: the only correct body calls the *static*
`SelectorProvider.provider()`, and `NativeContext` has `invoke_virtual`,
`invoke_virtual_declared` and `invoke_virtual_bytecode_only` but **no
`invoke_static`** (`native-api/src/registry.rs`). The two implementable
alternatives are both worse than the current `AbstractMethodError`: returning
`null` is the indiscriminate-implementation shape
(`W3-7-sslcontext-bogus-protocol.md`) and would make `sel.provider().openXxx()`
an NPE at a site that no longer names the cause; fabricating a bare
`SelectorProvider` carrier would collide with the `sun/nio/ch/SelectorProviderImpl`
and `WEPollSelectorProvider` natives `native-io` already registers against the
*real* provider object. The patch is in §8 for a lane that can add
`invoke_static` or work inside `native-io`.

**`DatagramChannel.setOption(SocketOption,Object)DatagramChannel` and
`getRemoteAddress()SocketAddress`** — both *are* implemented, well, in
`native-builtins/src/phases_late/net_channels.rs::register_datagram_channel`
(this lane's file). That registrar is `pub(crate)`, reached only from
`phases_late.rs::register_phase72_natives` → `register_synthetic_overrides`, so
it is dead in the default build — the same "a dead registrar is not a missing
feature" shape as
`an-inert-registration-looks-exactly-like-a-missing-feature`. **They cannot
simply be wired**: that registrar reads `field 4` as a socket-registry id and
`field 2` as the connected flag, while the object a default build actually mints
comes from `native-io/src/lib.rs`'s *own* `register_datagram_channel` with a
different `DC_NUM_FIELDS` layout. Two synthetic layouts for one class name;
wiring the second registrar in makes the bodies read the first one's slots. That
is the §5 species again, so this lane declines to wire it and names the
precondition instead: **unify the two DatagramChannel layouts first.**

**`DatagramChannel.read([Ljava/nio/ByteBuffer;II)J` and
`write([Ljava/nio/ByteBuffer;II)J`** — genuinely absent, and genuinely reachable
(`ScatteringByteChannel`/`GatheringByteChannel` are what Netty's datagram path
uses). Not fixed here because they are **not composable** from the single-buffer
natives that do exist: for a datagram channel a scattering read consumes exactly
one datagram spread across the buffers, and a gathering write emits exactly one
datagram assembled from them, so a loop over `read(ByteBuffer)` would consume
*len* datagrams and a loop over `write(ByteBuffer)` would emit *len* of them.
A correct body has to reach the socket, which means the layout question above.

## 7. What was changed in the tree by this record

Nothing in Rust. Nine of the eleven classes were already covered; the
`Spliterator` and `Map$Entry` halves in particular are covered by
`native-collections` registrars that run at `vm_init.rs:2434`, and the
`nio/file` half by `native-io` at `vm_init.rs:2403` plus
`register_phase57_nio_file` at `vm_init.rs:2473`. The five residuals all
require files this lane does not own (`native-io/src/lib.rs`,
`native-api/src/registry.rs`, `vm/src/vm/vm_init.rs`).

**Ordering, stated for the record** even though no wiring was added, because the
next pass will need it. In the default (`#[cfg(not(feature = "synthetic-jdk"))]`)
arm of `vm_init.rs`, the registrars that touch these class names run in this
order, and `register()` is last-write-wins:

| `vm_init.rs` line | registrar | touches |
|---|---|---|
| 2208 | `register_essential_natives_with_shims` → `net_phase_e::register_phase_e_networking` | `Selector.isOpen()Z`, `Selector.select(J)I` |
| 2403 | `register_io_natives` → `register_nio_selector_real`, `register_watch_service_real`, `register_nio_file_natives`, `register_phase92_io_completeness` → `register_watch_service` + `register_datagram_channel` | `Selector`, `SelectionKey`, `DatagramChannel`, `Path`, `Watch*` |
| 2434 | `register_collections_natives` → `register_interface_natives`, `register_iterator_protocol_natives` | `Spliterator`, `Map$Entry` |
| 2473 | `register_phase57_nio_file` | `Path` |
| 2492 | `register_p59_jar` | — (the earliest **this lane's** files reach the default build) |
| 2707 | `register_p58_charset_coder` | — (the latest) |

Any new registration for `Spliterator` or `Map$Entry` must be wired **after
2434** or `native-collections` overwrites it; anything for `Selector`,
`DatagramChannel`, `Path` or `Watch*` must be wired **after 2403**. Both of this
lane's doors (2492, 2707) satisfy both, so a trampoline from `register_p59_jar`
would have worked had there been anything safe to register.

## 8. Out-of-file patch (not applied)

### 8.1 Delete the `forEachOrdered` special case in the dispatch core

`vm/src/runtime/interpreter.rs`, inside `execute`'s `if !has_code {` arm. Delete
lines 995–1011 as they stand at `7d4d545e0` — the comment block beginning
`// Stream.forEachOrdered(Consumer) gap:` through the closing brace of the `if`:

```rust
            // Stream.forEachOrdered(Consumer) gap: its only native registration
            // lives in `register_phase56_stream_extras`, reachable solely from
            // `register_synthetic_overrides` (synthetic-jdk feature, compiled out
            // of the real-JDK CLI). So an `invokeinterface Stream.forEachOrdered`
            // resolves to the abstract interface declaration (no Code) and the
            // interface->concrete retarget that already makes `forEach` work does
            // not fire for `forEachOrdered`, surfacing as
            //   AbstractMethodError: Stream.forEachOrdered(...)V has no Code attribute
            // (24+ WildFly `ejb.security` tests, plus any real-JDK code using it).
            // For our sequential streams `forEachOrdered` is semantically identical
            // to `forEach`; re-dispatch as `forEach`, whose receiver-walk rescue
            // (Path A below) resolves the concrete override on the receiver.
            if method_name == "forEachOrdered"
                && method_descriptor == "(Ljava/util/function/Consumer;)V"
            {
                return execute(shared, thread, class_id, "forEach", method_descriptor, args);
            }
```

Nothing replaces it: the `resolve_native_for_dispatch` call twenty lines below,
already in the same arm, is what serves a registered native for a no-`Code`
resolved method. The hack exists only because the registration was dead.

**Why it should go even on its own terms.** The guard tests `method_name` and
`method_descriptor` and **not `class_id`**, so it fires for *any* class whose
resolved `forEachOrdered(Consumer)V` lacks `Code` — an application's own
interface with that signature is silently re-dispatched to its `forEach`, which
is the name-keyed-dispatch species this campaign has closed five times
(`a-class-name-shape-test-is-a-dispatch-bug`). It is also silently wrong for a
**parallel** stream, where `forEachOrdered` must preserve encounter order and
`forEach` explicitly need not; the comment's *"for our sequential streams"*
carve-out is not checked anywhere.

**Safety precondition — all three must hold before deleting:**

1. `java/util/stream/Stream.forEachOrdered(Ljava/util/function/Consumer;)V` is
   registered by a registrar that runs in the **default** build (i.e. reachable
   from `vm_init.rs`'s `#[cfg(not(feature = "synthetic-jdk"))]` arm, not only
   from `register_synthetic_overrides`). As of `7d4d545e0` the only registration
   is in `register_phase56_stream_extras`, which is synthetic-only — that is the
   whole defect. **Verify by call graph, not by grep** (§4).
2. The primitive-stream siblings are registered the same way:
   `IntStream`/`LongStream`/`DoubleStream`'s
   `forEachOrdered(Ljava/util/function/Int|Long|DoubleConsumer;)V`. These are a
   *different* descriptor, so the deleted guard never covered them; they are
   listed here only so the deletion is not mistaken for having covered them.
   `register_phase56_primitive_stream_terminals` (landing on a sibling branch)
   is the intended home.
3. A vector actually exercises it. The guard's comment cites *"24+ WildFly
   `ejb.security` tests"*; per
   `natives-over-real-jdk-classes.md` §8 a vector that exists and is not in
   `run.sh`'s `CORE_CLASSES`/`JDKONLY_CLASSES` word list never runs, so confirm
   the vector is *scheduled* before reading a green run as evidence.

If (1) is not yet true, **do not delete** — the deletion converts a wrong answer
into an `AbstractMethodError`, which is worse for the WildFly path than the
status quo. The correct order is: land the registration, prove it live, then
delete.

### 8.2 `Selector.provider()` — needs `invoke_static` first

Requires a `NativeContext::invoke_static` (`native-api/src/registry.rs`, VM impl
in `vm/src/vm/vm_exec.rs`); with it, the body registered on
`java/nio/channels/Selector` is:

```rust
    // `provider()` is the ninth abstract on `Selector` and the only one with no
    // native. It must answer the SAME provider the real JDK would, because
    // `native-io` registers `openDatagramChannel` etc. against the concrete
    // `sun/nio/ch/SelectorProviderImpl` / `WEPollSelectorProvider` names — a
    // fabricated carrier would miss all of them. Returning `null` is worse than
    // the AbstractMethodError it replaces: `sel.provider().openSocketChannel()`
    // becomes an NPE at a site that no longer names the cause (W3-7's shape).
    r.register(
        "java/nio/channels/Selector",
        "provider",
        "()Ljava/nio/channels/spi/SelectorProvider;",
        |ctx, _args| {
            ctx.invoke_static(
                "java/nio/channels/spi/SelectorProvider",
                "provider",
                "()Ljava/nio/channels/spi/SelectorProvider;",
                &[],
            )
        },
    );
```

Home: `native-io/src/nio_selector.rs::register_nio_selector_real`, beside the
eight sibling abstracts, so no new `vm_init.rs` wiring is needed (it is already
live at `vm_init.rs:2403`). Do **not** put it in a
`register_synthetic_overrides`-only registrar.

### 8.3 The four `DatagramChannel` residuals

Blocked on unifying the two synthetic `DatagramChannel` layouts
(`native-io/src/lib.rs`'s `DC_NUM_FIELDS` vs.
`native-builtins/src/phases_late/net_channels.rs`'s `field 2 = connected,
field 4 = socket id`). Once one layout owns the class name, the two already-written
bodies for `setOption(SocketOption,Object)` and `getRemoteAddress()` in
`net_channels.rs` can move to `native-io`'s live `register_datagram_channel`
verbatim, and the two vectored overloads can be written against the socket
registry the same way `send`/`receive` already are. Do not compose them from the
single-buffer natives: one call must move exactly one datagram.

---

## References

`docs/architecture/natives-over-real-jdk-classes.md` (§1 registration is the
gate, §3 last-write-wins, §5 slot indices, §7 scoped censuses),
`docs/known-issues/jdk-only/W4-4-slot-index-species-sweep.md`,
`docs/known-issues/jdk-only/W6-3-slot-index-species-residuals.md`,
`docs/known-issues/jdk-only/W3-7-sslcontext-bogus-protocol.md`,
`docs/feature-designs/synthetic-class-fallibility.md`.
