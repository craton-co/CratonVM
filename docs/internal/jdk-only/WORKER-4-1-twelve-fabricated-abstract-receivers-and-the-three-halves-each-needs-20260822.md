# WORKER-4-1 — twelve fabricated abstract receivers, the three halves each one needs, and the census the fabrication was hiding

**Status: FIXED AND VERIFIED ON A BUILD.** Every claim marked MEASURED below was
run by this lane on this host. Lane WORKER 4, 2026-08-21/22.

**Provenance.** Linux (Azure host 2), Temurin **25.0.4+7**, worktree
`/data/cvm-w4io-20260821` on `wt-w4io-20260821`, cut from
`claude/jdk-only-mode-handoff-09b48c` at `22cb4338d` and merged up to
`ddf277ea4` before the final build. Binaries frozen per round
(`vm-w4io-{base,r1..r5}.bin`); `base` is the same worktree built at the
pre-change commit, so every "before" number is a real run of a real binary, not
a quotation.

**This is a DIFFERENT HOST AND JDK from the brief's baseline.** The brief's
`105/105 · 105/105 · 65/65` is Windows + 25.0.3+9. On Linux + 25.0.4+7 the
PRE-CHANGE baseline is `104/105 · 103/105 · 64/65`; the two extra failures are
`RJdkOptionalShape` (red at the branch point on this host, in all three arms)
and `RJdkFunctionCombinators` (closed on the branch tip by `ecd4f56e1`, which
this worktree predated). Judge this lane against the `base` column, not against
the brief.

---

## 1. The finding, in one line

`H21-1` fixed one site and nominated the assertion that finds the rest.
**The assertion finds twelve, and this closes all twelve.**

```
regression-suite/probes/W4Abstract.java  — 63 java.io / NIO receivers,
one line each: <tag> <runtime class> <CONCRETE|ABSTRACT|INTERFACE>

                                   abstract-or-interface, of 63
  HotSpot 25.0.4+7                      0
  CratonVM --jdk-only   before         12      after   0
  CratonVM --real-jdk   before         12      after   0
```

`Modifier.isAbstract(o.getClass().getModifiers())` on a VM-minted receiver is a
defect by JVMS §6.5 **with no oracle run required** (`H21-1` N3) — `new` on an
abstract class or an interface is an `InstantiationError`, so such an object is
one no bytecode in any image could have produced. The probe is in the tree.

### 1.1 The twelve, MEASURED before and after

| receiver | CratonVM, before | HotSpot 25.0.4+7 | CratonVM, after |
|---|---|---|---|
| `FileSystems.getDefault()` | `sun.nio.fs.UnixFileSystem` **abstract** | `sun.nio.fs.LinuxFileSystem` | `sun.nio.fs.LinuxFileSystem` |
| `FileSystems.getDefault().provider()` | `sun.nio.fs.UnixFileSystemProvider` **abstract** | `sun.nio.fs.LinuxFileSystemProvider` | `sun.nio.fs.LinuxFileSystemProvider` |
| `DatagramChannel.open()` | `java.nio.channels.DatagramChannel` **abstract** | `sun.nio.ch.DatagramChannelImpl` | `sun.nio.ch.DatagramChannelImpl` |
| `Selector.open()` | `sun.nio.ch.SelectorImpl` **abstract** | `sun.nio.ch.EPollSelectorImpl` | `sun.nio.ch.EPollSelectorImpl` |
| `AsynchronousFileChannel.open(…)` | `java.nio.channels.AsynchronousFileChannel` **abstract** | `sun.nio.ch.SimpleAsynchronousFileChannelImpl` | same |
| `AsynchronousSocketChannel.open()` | `java.nio.channels.AsynchronousSocketChannel` **abstract** | `sun.nio.ch.UnixAsynchronousSocketChannelImpl` | same |
| `AsynchronousServerSocketChannel.open()` | `java.nio.channels.AsynchronousServerSocketChannel` **abstract** | `sun.nio.ch.UnixAsynchronousServerSocketChannelImpl` | same |
| `AsynchronousChannelGroup.withFixedThreadPool(…)` | `java.nio.channels.AsynchronousChannelGroup` **abstract** | `sun.nio.ch.EPollPort` | `sun.nio.ch.EPollPort` |
| `FileSystems.getDefault().newWatchService()` | `java.nio.file.WatchService` **INTERFACE** | `sun.nio.fs.LinuxWatchService` | `sun.nio.fs.LinuxWatchService` |
| `path.register(ws, …)` | `java.nio.file.WatchKey` **INTERFACE** | `sun.nio.fs.LinuxWatchService$LinuxWatchKey` | same |
| `Files.newDirectoryStream(dir)` | `java.nio.file.DirectoryStream` **INTERFACE** | `sun.nio.fs.UnixSecureDirectoryStream` | same |
| `Files.getFileStore(f)` | `java.nio.file.FileStore` **abstract** | `sun.nio.fs.LinuxFileStore` | `sun.nio.fs.LinuxFileStore` |

Plus `WatchEvent` (`java.nio.file.WatchEvent`, INTERFACE →
`sun.nio.fs.AbstractWatchKey$Event`) and `MembershipKey`
(`java.nio.channels.MembershipKey`, abstract →
`sun.nio.ch.MembershipKeyImpl$Type4`/`$Type6`), which the probe does not drive
directly — both fixed, both ARGUED from the source rather than MEASURED at the
receiver.

**Every one of the twelve now answers the ORACLE'S string**, not merely a
concrete one. That was not the acceptance bar (JVMS §6.5 is), and it is worth
saying because it means the candidate lists are right and not just non-abstract.

---

## 2. Two of the twelve were an ALIAS, not a mint, and that is the sharper half

`FileSystems.getDefault()` and `.provider()` do not fabricate an object of an
abstract class. They fabricate an object stamped with the PUBLIC class and then
**report a different class from `getClass()`** — `native-builtins/src/lib.rs`'s
`jdk_concrete_getclass_alias` table, the deliberate mechanism for synthetic
carriers. Two of its rows named a class that is itself ABSTRACT:

```rust
"java/nio/file/FileSystem"                => "sun/nio/fs/UnixFileSystem",          // abstract
"java/nio/file/spi/FileSystemProvider"    => "sun/nio/fs/UnixFileSystemProvider",  // abstract
```

`UnixFileSystem` is the abstract PARENT of `LinuxFileSystem`. So the VM was
**telling** the application it held an instance of a class `new` cannot produce.
That is the same defect as minting one, arrived at from the other direction, and
no amount of care at the allocation sites would have found it.

**The remedy is not a better name, it is a check.** Each row is now an ORDERED
CANDIDATE LIST and the resolver takes the first entry that is present in the
image **and instantiable**:
`cratonvm_native_api::instantiable::first_instantiable`. The abstract parents
stay as last-resort entries, which is safe precisely because the filter is
there — on a platform this list does not name, the answer degrades to today's
rather than to nothing.

`class_is_instantiable` is one function in `native-api` and not two, because two
crates need it: the one that MINTS these receivers (`native-io`) and the one
that REPORTS their class (`native-builtins`). `[fifteen copies of one
primitive]` is the standing lesson and `appended_slots` is the standing example.

---

## 3. THREE halves, not two — and the third one cost a red vector

`H21-1` states the fix as two halves: mint the concrete class, and register the
natives on it because dispatch keys on the receiver (`H11-1`). Both are
necessary. **They are not sufficient**, and this lane learned the third from a
regression rather than from reading:

### 3.1 Half one — mint the concrete class

`native-io/src/concrete_receiver.rs::alloc_concrete`. Ordered candidates,
`instantiable` filter, abstract public name as the final fallback (which is what
synthetic-JDK mode has always used and still gets).

### 3.2 Half two — mirror the registrations

`mirror_class_registrations(r, since, from, to)` copies every row the CALLING
REGISTRAR wrote for `from` onto `to`, preserving each row's `NativeKind`.

`since` is `dump_registrations().len()` taken at the top of the registrar, and
it is load-bearing: filtering on the class name alone would also copy rows some
OTHER crate registered on the same class, attributing them to this one and
handing `to`'s slot to a body with an unrelated field layout. `[2 producers, 1
slot]` / `[dup nati]`.

A hand-written second table would have been the alternative and it is the wrong
one: forty rows maintained twice drift on the first row somebody adds to only
one of them.

### 3.3 Half three — every OTHER table keyed on the old class name

**`socket_channel.rs::CRATONVM_NIO_CLASSES` is "is this receiver one of ours".**
`foreign_nio_receiver` answers from it, and `g_selector_provider` and eight
sibling guards act on that answer by DELEGATING to the receiver's own bytecode
when it says "foreign". A selector minted as `sun.nio.ch.EPollSelectorImpl` was
not in the list, so every selector this VM opens was classified foreign, and
`AbstractSelector.provider()`'s real `final` bytecode returned the `provider`
field no constructor ever set.

MEASURED: `RJdkNio` went red with
`AssertionError: Selector.provider() must not be null`.

The list now READS `nio_selector::SELECTOR_IMPLS` rather than copying it, so the
two cannot drift again.

**`native-io/src/lib.rs`'s `FileLock.release()` discriminator** is the same
species, caught by reading rather than by a red vector: it asked
`channel_class != "java/nio/channels/AsynchronousFileChannel"` to decide whether
a lock belonged to an async file channel. Every such channel now carries a
concrete name, so the test would have sent all of them down the
`FileChannelImpl.release` arm.

**The transferable rule: a class-name comparison is a registration, too.**
`git grep` the OLD class name after moving a mint — not just the registrar.

---

## 4. TWO cross-crate gaps, and the audit that finds them

`H11-2` §4 documents "deleting this row hands the slot to another crate". This
lane hit the mirror image: **a triple registered ONLY by another crate, on the
abstract class, which the mirror cannot see** — because the mirror is correctly
scoped to its own registrar's rows (§3.2).

MEASURED, `RJdkAsyncChannel` red:

```
NullPointerException: Cannot invoke "sun.nio.ch.NativeThreadSet.add()"
                      because "this.threads" is null
  at sun/nio/ch/SimpleAsynchronousFileChannelImpl.implForce
```

`AsynchronousFileChannel.force(Z)V` is registered by
`native-builtins/src/phases_late/net_channels.rs` and by nothing else. With the
receiver concrete, the JDK's own `force` ran against a channel whose `<init>`
this VM never executed.

**The instrument, and it should outlive this lane.** For each class this lane
re-minted, take the triples registered on the OLD name by ANY crate and subtract
the triples registered on the NEW ones; static factories are excluded because
their key is the constant-pool class, not the receiver (`H11-1`). Run against
the `--dump-native-registry` dump it takes ten seconds and it found both:

```
== java/nio/channels/AsynchronousFileChannel  (13 triples)
   GAP  force(Z)V     registered_by=[native-builtins/…/net_channels.rs:1756]
== java/nio/channels/AsynchronousSocketChannel  (16 triples)
   GAP  connect(Ljava/net/SocketAddress;)Ljava/util/concurrent/Future;
                      registered_by=[native-builtins/…/net_channels.rs:1879]
```

Both are now registered on the concrete classes as well, **and both bodies
stopped indexing the other crate's slots by hand**: `force` reads through
`cratonvm_native_io::afc_channel_is_open` / `afc_channel_handle_id`, and
`connect` writes through `async_socket::async_socket_note_connected`. That file's
own comment already stated the rule it was breaking — *"the registration which
decides the layout is the one that ALLOCATES"* — and it is now enforced by there
being no index to get wrong.

---

## 5. The private slot maps had to move, and one of them had a 40-line comment asking for it

An abstract public class declares few or no instance fields, so a native keeping
private state at slots `0..N` was — accidentally — not colliding with anything.
The concrete `Impl` declares many. The remedy is `native-api`'s existing
`appended_slots`: start the private map ABOVE every declared field, allocate
`base + N`, and resolve the base from ONE function that the allocator and every
accessor call.

`appended_slots::base_for_object` is new here and is the accessor-side twin of
`base_for_class`. It was written out three times before it moved
(`pipe.rs::channel_private_base`, `concrete_receiver::concrete_base`, and the
`nio_file.rs` families); the two survivors now forward to it.

`async_socket.rs::aio_assc_open` carried a 40-line doc comment naming this exact
repair and stating why it could not be made:

> *"The repair is the appended-slot idiom, and it cannot be applied to this
> class alone. The four constants are module-level and shared with
> `AsynchronousSocketChannel`, and three registrations bind the SAME native to
> both classes … renumbering there is out of bounds, because
> `AsynchronousSocketChannel` is the two-crates-one-class case W7-49 §5
> measured … What this needs, and what this lane could not do: split the two
> slot maps, give `aio_asc_is_open` a per-class sibling, and settle the
> `native-builtins` survivor in the same step — a build, and one change spanning
> both crates."*

All of those sides are in this lane's scope, so they moved together. The
per-class sibling turned out to be unnecessary: `concrete_base`'s width guard
already answers per RECEIVER, which is strictly more precise than per class.

---

## 6. Two duplicate-registration defects fell out, and one of them was a THIRD slot map

### 6.1 `register_t16_channel_overrides` was winning against the real implementation

MEASURED, `--dump-native-registry`, `--jdk-only`:

```
AsynchronousSocketChannel.open()      owns_slot=True  inv=1  [native-io/src/nio_native.rs:1782]
                                      owns_slot=False        [native-io/src/async_socket.rs:3548]
AsynchronousFileChannel.open(Path,…)  owns_slot=True  inv=1  [native-io/src/nio_native.rs:1770]
                                      owns_slot=False        [native-io/src/lib.rs:21667]
```

Fifteen triples across `AsynchronousFileChannel`, `AsynchronousSocketChannel`
and `AsynchronousChannelGroup`. `lib.rs`'s own integration comment has said the
opposite since the modules landed — *"WP3.2 … Supersedes the synthetic
`t16_asc_*` / `t16_acg_*` stubs registered in `register_t16_channel_overrides`"*
— and it is exactly backwards: `register_t16_channel_overrides` runs LAST and
re-registration updates the slot in place.

The two registrars also disagreed about the layout. `t16_asc_open` wrote
`{0: connected, 1: open, 2: fd, 3: remote}`; `aio_asc_open` — which owns
`connect`, `read`, `write`, `getRemoteAddress` and every option accessor on the
SAME object — reads `{0: open, 1: connected, 2: reg_id, 3: remote}`. Slots 0 and
1 mean the opposite thing.

All fifteen retired. **Trap 4 checked per triple, not assumed:** the promoted row
is `native-io`'s own real implementation every time, never `net_channels.rs`'s
(which is `owns_slot=False` behind BOTH).

### 6.2 …and `net_channels.rs` was the third copy of that map

W7-49 measured the two-way version of this and recorded that it *"belongs to a
lane that owns `native-io`"*. That lane owns this line too, so both sides moved
together — see §4.

### 6.3 `Closeable` / `AutoCloseable`, the two rows `H11-3` N1 could not remove

`H11-3` wrote the deletion out verbatim and was blocked by
`vm/src/vm/tests.rs::auto_closeable_close_p70`, which calls the triple directly
through a helper that `panic!`s on an unregistered one. It is a two-file commit
and this lane made it.

Trap 4, MEASURED again rather than quoted:

```
java/io/Closeable.close()V        owns_slot=True inv=0  [native-io/src/lib.rs:8050]
java/lang/AutoCloseable.close()V  owns_slot=True inv=0  [native-io/src/lib.rs:8051]
```

ONE row each, `dupX = 0`. The deletion removes two registry rows and hands
nothing to anybody.

The deleted test asserted that a row EXISTED, with a null receiver, and nothing
else — which is what made it a blocker rather than a protection.

---

## 7. The census went UP, and the fabrication is why it was down

**MEASURED. Do not quote a `native-shadows-bytecode` figure across this commit
without saying which side of it you are on.**

| instrument | before | after |
|---|---:|---:|
| suite UNION, `native-won` (dispatch level) | 1403 | 1426 |
| registration level, `owns_slot ∧ declared ∧ has_code ∧ ¬acc_native` | 2822 | 2941 |

The +119 splits, MEASURED, into two causes and **neither is new shadowing**:

* **84 are the mirrors.** A native standing in front of an ABSTRACT declaration
  is not a `native-shadows-bytecode` observation, because there is no bytecode
  there to shadow. The same native, on the class the JDK actually builds, stands
  in front of a real `Code` attribute — and now says so.
* **35 changed nothing but `real_declaring_method.loaded`.**
  `java.net.DatagramSocket` and 34 siblings are now LOADED, because resolving
  `sun.nio.ch.DatagramChannelImpl` pulls them in, and an unloaded class has no
  declaring-method record for the census to classify against. Same row, same
  callback, same `owns_slot` — only `loaded: false → true`.

So the fabricated receiver was suppressing this count **in two different ways at
once**: it kept the natives in front of abstract declarations, and it kept the
concrete classes out of the image. The population did not grow. It became
visible.

This is the `[a consumer count cannot explain its own zero]` shape at the level
of a whole census, and it is the most transferable thing in this record: **a
fabrication that keeps a real class unloaded is invisible to every instrument
that keys on the real class.**

---

## 8. Acceptance — 105/105 / 105/105 / 65/65, on a host where the baseline was not

Three arms, same host, same JDK, `TIMEOUT=600`. `base` is this worktree built at
its pre-change commit and run on the same host; `after` is the final build,
which also carries the branch tip merged in.

| arm | base | after |
|---|---|---|
| `CRATONVM_ARGS=--jdk-only` | 104/105 | **105/105** |
| `SUITE=all` | 103/105 | **105/105** |
| `SUITE=core` | 64/65 | **65/65** |

Three vectors moved, and only one of the three is somebody else's:

* **`RJdkFunctionCombinators`** (`SUITE=all`) closed on the branch tip by
  `ecd4f56e1`, which this worktree predated. Not this lane's.
* **`RJdkOptionalShape`** (all three arms) was red at the branch point ON THIS
  HOST and green on the brief's Windows baseline. **This lane closed it** — see
  §8.1; it is a `java.io`/process defect in this lane's own file.
* No vector went red.

Two vectors went red DURING the work and both are in this record because they
are the evidence for §3.3 and §4: `RJdkNio` (`Selector.provider()` null) and
`RJdkAsyncChannel` (`force` into `implForce`, then `accept()` into
`NotYetBoundException`). Both green in the final build.

### 8.1 `RJdkOptionalShape` — an `Object[]` wearing a `String[]`'s job

MEASURED at the branch point on this host, both modes:

```text
AssertionError: process.info.arguments: get() on a PRESENT Optional must
return a [Ljava.lang.String;, got [Ljava.lang.Object;
```

`ProcessHandleImpl$Info.arguments` is declared `String[]`, and `process.rs`'s
`/proc/<pid>/cmdline` reader filled it with
`ctx.new_array(ArrayElementType::Reference, n)` — which is `Object[]`. Every
element was a `String`, so every assertion about the CONTENTS passed; the ARRAY
was the wrong type, and `Info.arguments()` hands it straight to
`Optional.ofNullable`, so the caller sees the component type.

**Trap 5, exactly as the brief states it:** *"The corpus asks nothing about
array component types."* It asked nothing about this one on Windows either,
because Windows reports no arguments at all — `arguments()` is measured EMPTY
there, and this crate's own comment says so — so the array is never built and
the component type is never observed. The defect was reachable only on a host
the brief's baseline was not run on.

`new_string_array` is now one helper. There were two spellings of "allocate a
`String[]`" in this crate — a correct four-line one at `File.list()` and a wrong
one-liner — and a third site (`watch.rs::pollEventNames0`, whose registered
descriptor is `(I)[Ljava/lang/String;`) had the wrong one too.

---

## 8.2 `H11-3` N3 — "the biggest thing this lane opened and did not close" — is MEASURED NOT LIVE

`H11-1` N1 and `H11-3` N3 name the abstract-class superclass walk as the real
hazard and nominate `java/io/InputStream` (9 rows), `java/io/OutputStream` (5)
and `java/nio/channels/spi/SelectorProvider` (4) as the three to probe first:
*"One user subclass per class, diffed against HotSpot."*

`regression-suite/probes/W4BaseStream.java` (added here) is that probe for the
first two: `Counting extends InputStream` declaring only `read()`,
`Sink extends OutputStream` declaring only `write(int)`, a `BareSink` with no
`flush`/`close` override at all, a `FilterOutputStream` subclass, and
`Buffered`/`Data` streams layered over them. 26 cases.

**MEASURED, 26 cases, ZERO diffs against HotSpot 25.0.4+7, in BOTH modes.**

And it is a WITNESSED negative, not an unwitnessed one (`[neg != ruled out]`).
`--dump-native-registry` on the same run:

```text
--jdk-only     java/io/InputStream.read([BII)I   invocations: 1   <- the positive control
               every other java/io/{Input,Output}Stream row   invocations: 0
--real-jdk     every row, including read([BII)I               invocations: 0
```

So one of the fourteen rows DOES fire on this shape and answers identically to
HotSpot, which is what makes the other thirteen zeros informative rather than
merely absent (`[zero@consumer]`). Reading `native_bais_read_bytes` says why: it
already carries a receiver test (`input_stream_has_bais_layout`) and, for a
foreign receiver, reproduces `InputStream.read(byte[],int,int)`'s JDK default by
looping `invoke_virtual(this, "read", "()I")`.

**That is the good news and the finding at the same time.** The row is correct
AND it is a hand-written copy of `java.base` bytecode, which is exactly what
contract §1.4 forbids. It cannot simply be retired: its own comment names a live
population — *"synthetic streams (URL.openStream, getResourceAsStream) that
materialise as bare InputStream-typed receivers but actually have the
ByteArrayInputStream layout in slots 0..3."* **A fabricated `InputStream`-typed
receiver is the reason this shadow exists**, which is the same species this
record closes twelve instances of. See N6.

---

## 9. What this did NOT do

* **`native_fc_open`'s `java/nio/channels/FileChannel` mint is untouched**, on
  purpose. MEASURED: `FileChannel.open(path, READ).getClass()` already answers
  `sun.nio.ch.FileChannelImpl` in both modes, because the force-native gate
  declines the static factory and the JDK's own bytecode reaches
  `native_fcimpl_open`. The abstract mint is a synthetic-JDK-mode fallback that
  no live path on this host reaches, and `native-api`'s
  `synthetic_file_channel` module SCREENS on that exact class name to keep our
  private slots off a real `FileChannelImpl` — moving the mint would defeat the
  screen. Stated because a reader grepping `ensure_class_initialized(
  "java/nio/channels/FileChannel")` will find it.
* **`AsynchronousServerSocketChannel.accept()` (the `Future` form) is a
  REFUSAL, not an implementation.** Before this change the fabricated abstract
  receiver made it an `AbstractMethodError` by accident; the concrete receiver
  made it a `NotYetBoundException` — a WRONG answer about a channel that WAS
  bound. It is now an explicit `UnsupportedOperationException`. The `Future`
  form has to complete from the accept worker, and this module's completion path
  applies on the CALLING thread, so a real `CompletableFuture` would be
  completed only by a drain that a caller blocked in `get(timeout)` never
  reaches — the hang `RJdkAsyncChannel.acceptFutureMustNotHang` exists to
  forbid, and the worst of the three available answers.
* **No `--jdk-only-report` per-vector diff.** §7's dispatch-level numbers come
  from the suite's own union line; the attribution in §7 is registration-level.
  They are different instruments and are labelled as such.
* **Windows was not run.** Every candidate list carries the Windows entries and
  the `instantiable` filter is what makes an untested platform degrade rather
  than break, but that is ARGUED, not MEASURED.

---

## 10. NOMINATIONS

**N1 — `W4Abstract` should be a scheduled vector, not just a probe.** It is a
universal assertion with no oracle: any `ABSTRACT`/`INTERFACE` line is a defect.
It lives in `regression-suite/probes/` because adding a vector changes the
suite's denominators, which is `regression-suite/`'s owner's call. Promoting it
would make the next fabricated receiver a red build instead of a lane.

**N2 — the cross-crate coverage audit of §4 should be a script in
`scripts/`.** It is ~60 lines over `--dump-native-registry` and it found two
defects that a build and a corpus run did not. Its `MOVES` table is the list of
every abstract→concrete relocation in the tree and wants to live beside the
relocations.

**N3 — six class-identity divergences remain, none of them JVMS violations.**
MEASURED by the same probe, CratonVM vs HotSpot 25.0.4+7:

| expression | CratonVM | HotSpot |
|---|---|---|
| `Files.newInputStream(f)` | `java.io.FileInputStream` | `sun.nio.ch.ChannelInputStream` |
| `Files.newOutputStream(f)` | `java.io.FileOutputStream` | `sun.nio.ch.ChannelOutputStream` |
| `Files.walk(dir)` | `ReferencePipeline$Head` | `ReferencePipeline$3` |
| `Files.readAttributes(f, BasicFileAttributes.class)` | `sun.nio.fs.UnixFileAttributes` | `…$UnixAsBasicFileAttributes` |
| `System.in` | `java.io.FileInputStream` | `java.io.BufferedInputStream` |
| `new ProcessBuilder("true").start()` **(`--real-jdk` only)** | `cratonvm.synthetic.Process` | `java.lang.ProcessImpl` |

The last one is the sharpest: `--jdk-only` answers `java.lang.ProcessImpl`
correctly and COMPATIBLE mode answers a synthetic class. `[HS=oracle]` — that is
a compatible-mode defect of exactly the shape `H15-1` describes, in
`native-io/src/process.rs`, and it is this lane's own file.

**N4 — `System.in` is a `FileInputStream`, not a `BufferedInputStream`.** Same
table. It is a one-object difference with a real behavioural edge (mark/reset
support, `available()` semantics) and it is unclaimed by any P-row.

**N5 — `watch.rs::register_watch_service_real` lists `sun/nio/fs/UnixWatchService`,
a class no Linux image declares.** The Linux class is `LinuxWatchService`. The
family is `SyntheticStub`-tagged and measured at `invocations: 0`, so this is a
dead row rather than a live defect — but it is a dead row whose NAME is wrong,
which is how a census over-counts what it thinks it covers.

---

**N6 — CLOSED, by `WORKER-4-2` §4.** It read: *"the
`java/io/{Input,Output}Stream` base-class rows are a hand-written copy of
`java.base`, kept alive by a fabricated receiver … make `URL.openStream()` /
`getResourceAsStream()` mint a real `java.io.ByteArrayInputStream` and fourteen
§1.4 shadows become retireable."*

**The premise was already false and nothing had measured it.**
`regression-suite/probes/W4StreamCarrier.java` asks all fifteen carriers, in
both modes: `URL.openStream()`, `URLConnection.getInputStream()`,
`Class.getResourceAsStream()` and `ClassLoader.getResourceAsStream()` **already
return a concrete `java.io.ByteArrayInputStream`**, and `abstractOrInterface`
is 0 of 15. The bare-`InputStream` receiver the rows were written for had
stopped being produced; nothing connected the two, so the natives stayed.

Eleven of the rows are retired in `WORKER-4-2` §4.3, verified on a build
(`107/107 · 107/107 · 67/67`, four probes green). Three were deliberately left
and each has a stated reason there.

**The transferable half is the inverse of this record's own finding.** A
fabricated receiver justifies natives; when somebody fixes the fabrication, the
natives it justified do not go with it, because no instrument links a
registration to the mint that made it necessary. `[a consumer without a producer
reads as a feature]`.

**N7 — the `java.io` shadow surface, counted.** MEASURED, registration level,
unioned over all 105 vectors' registry dumps:

| | rows |
|---|---:|
| `java/io/*` triples a native OWNS | 402 |
| of those, §1.4 shadows (real method declared, has `Code`, not `ACC_NATIVE`) | **194** |
| of those, ZERO invocations across all 105 vectors | **99** |
| the rest — shadows with measured traffic | 95 |

The 99 is the same number `H14-2` reports for *"`java/io` streams"* in its
unclaimed-rows table, reached by a DIFFERENT instrument (this one is
registration-level over `--dump-native-registry`; `H14-2`'s is dispatch-level
over `--jdk-only-report`). **They must not be assumed to be the same 99 rows**
until somebody intersects them. The per-row table — with `invocations`, the
number of OTHER files registering the same triple, and whether the real method
has bytecode — is what an adjudication pass needs, and it is reproducible in one
command from the `invcensus` / `ioadjudicate` pair this lane used.

## 11. Index rows (for H0 to move into `INDEX.md`)

* `WORKER-4-1` — twelve fabricated abstract receivers closed and the corpus
  reaches 105/105 / 105/105 / 65/65 on Linux; the `getClass()` alias table is a
  SECOND mechanism for the same defect; a mint move needs THREE halves, not two;
  the shadow census was being suppressed by the fabrication in two ways at once;
  `H11-3` N3's superclass-walk hazard is measured NOT LIVE, with a witness, and
  eleven of the rows it was about are retired in the companion record.
