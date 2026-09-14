# H11-2 — 237 of `native-io`'s 334 abstract-class rows serve a receiver the VM mints, and the two sites H5-1 could not find were four lines from its grep

**Status: MEASURED**, with one ARGUED column marked as such (§3, the *twin*
heuristic). The census is a **live `--jdk-only` registry dump** of the prebuilt
binary at `fe59bf9d9`, not a static parse — so it says what strict mode
**admits and reaches**, which is a different and smaller population than
`H5-1`'s source parse. §6 says exactly how the two differ and why they must not
be subtracted.

**Depends on** `H11-1`, which establishes that dispatch keys on the receiver's
class. Without that, none of the verdicts below are decidable.

**Provenance.** `--dump-native-registry` schema 5, `--jdk-only` and
`--real-jdk`, prebuilt `C:/craton/target-jdkonly-h2/release/cratonvm.exe`
(`fe59bf9d9`). **The binary does not contain this lane's edits.** No `cargo`
command was run. Oracle HotSpot 25.0.3+9, resolved via `command -v javap`.

Lane H11, 2026-08-20. Merge/base note: see `H11-1`.

---

## 0. The table this record exists to replace

The P1 *NIO, files, networking* row of `docs/jdk-only-runtime-services.md`
prescribes moving `native-io`'s abstract-API registrations down to
`sun.nio.ch.*Impl`. `H5-1` §3.2 disproved that for 13 named classes.
**Here is the whole population, per registration, with the verdict decidable.**

| verdict | classes | rows | meaning |
|---|---:|---:|---|
| **(ii) CANNOT MOVE** | 14 | **237** | the VM mints a receiver whose class name IS this class. Moving the registration strands it. |
| **(iii) already on BOTH** | 7 | 21 | an `*Impl` row for the same method already exists; the abstract half is dead weight, and 6 of those 21 are blocked by a cross-crate duplicate (§4) |
| **(iii) partial** | 8 | 57 | some methods have an `*Impl` twin, some do not |
| **(?) OPEN** | 7 | 19 | abstract/interface, no fabrication site found, no `*Impl` twin. The only genuine move candidates, and 4 of the 19 are the rows `H11-3` already removed |
| | **36** | **334** | |

**Zero rows are unambiguously (i) "safe to move to the Impl".** The row's
prescription has no clean instance in this crate.

---

## 1. MEASURED — the fabricated receivers, from a run

`H5-1` §3.2 listed 13 allocation sites from a source grep. This is the same
claim taken from the other end: what class name does the object *actually*
carry at run time? Probe `H11Dispatch.caseF`, `--jdk-only`, prebuilt binary,
against the oracle:

| expression | CratonVM `getClass().getName()` | HotSpot 25.0.3+9 |
|---|---|---|
| `SocketChannel.open()` | **`java.nio.channels.SocketChannel`** | `sun.nio.ch.SocketChannelImpl` |
| `ServerSocketChannel.open()` | **`java.nio.channels.ServerSocketChannel`** | `sun.nio.ch.ServerSocketChannelImpl` |
| `DatagramChannel.open()` | **`java.nio.channels.DatagramChannel`** | `sun.nio.ch.DatagramChannelImpl` |
| `AsynchronousFileChannel.open(…)` | **`java.nio.channels.AsynchronousFileChannel`** | `sun.nio.ch.WindowsAsynchronousFileChannelImpl` |
| `AsynchronousServerSocketChannel.open()` | **`java.nio.channels.AsynchronousServerSocketChannel`** | `sun.nio.ch.WindowsAsynchronousServerSocketChannelImpl` |
| `AsynchronousSocketChannel.open()` | **`java.nio.channels.AsynchronousSocketChannel`** | `sun.nio.ch.WindowsAsynchronousSocketChannelImpl` |
| `pipe.source()` | `sun.nio.ch.SourceChannelImpl` | `sun.nio.ch.SourceChannelImpl` |
| `pipe.sink()` | `sun.nio.ch.SinkChannelImpl` | `sun.nio.ch.SinkChannelImpl` |
| `Paths.get(".")` | `sun.nio.fs.WindowsPath` | `sun.nio.fs.WindowsPath` |

**A caveat I am obliged to state:** `[getClass=alias]` records that `getClass()`
can lie about a synthetic receiver's class on this VM. So these nine rows are
*corroboration*, not the primary instrument. The primary instrument is §3's
registry census, which reaches the same verdicts from `owns_slot` and
`invocations`, and the two agree everywhere. Where they would have disagreed —
`Pipe` — the invocation counts decide it (`H11-1` §3.3) and `getClass()` merely
agrees.

Two of these rows are new information, and they are the point of this section.

---

## 2. `H5-1` N7 is DISPROVED: both "movable candidates" are fabricated, in this crate

`H5-1` §3.4 marked exactly two classes *"abstract; no fabrication site found —
candidate, unproven"*, and N7 nominated them as **"the two families in §3.4 that
might genuinely be movable"**. Both are minted, both by `native-io` itself:

```text
native-io/src/async_socket.rs:3099   alloc_obj(
                                         ctx,
                                         "java/nio/channels/AsynchronousServerSocketChannel",
                                         N_FIELDS,
                                     )
native-io/src/lib.rs:21919           try_alloc_synthetic(
                                         ctx,
                                         "java/nio/channels/AsynchronousFileChannel",
                                         AFC_NUM_FIELDS,
                                     )
```

**Why the census missed them, which is the transferable part.** `H5-1` §7.1's
parser and the obvious grep both match `alloc_obj(ctx, "…"` on ONE line. These
two calls are `rustfmt`-split across four. Every other fabrication site in the
crate is single-line, so the pattern looked complete and returned 13 confident
hits. `[window≠absence]` — the lesson is normally about a 50-line window, and
this is the same failure at the width of a single line.

The fix is a multiline-aware pattern, and it is worth pasting because the next
census will want it:

```text
rg -U --multiline-dotall -o '(alloc_obj|try_alloc_synthetic)\s*\(\s*ctx\s*,\s*"([^"]+)"' native-io/src
```

That returns **15** sites where the single-line form returns 13, and the two it
adds are precisely the two families a lane was about to try to move.

`aio_assc_open`'s doc comment is 30 lines long and discusses this exact class's
layout at length. **The fabrication was documented in prose four lines above the
call and still invisible to the census**, because the census read a regex and
the prose read as background. Both files now say so at the site.

---

## 3. The per-class census

Method in §5. `rows` counts registrations whose `registered_by` starts with
`native-io/`, in a `--jdk-only` boot. `inv` is this probe run's invocations.
`dupX` is how many of those rows have a same-triple registration from ANOTHER
crate — the number that decides whether deleting a line here removes anything
(§4). `twin` is **ARGUED, heuristic**: how many of the class's methods are also
registered on some class whose simple name ends in `Impl`; it over-counts,
because it does not check that the `Impl` is a subtype of this class.

| Class | JDK 25 | rows | inv | dupX | twin | verdict |
|---|---|---:|---:|---:|---:|---|
| `java/nio/channels/DatagramChannel` | ABSTRACT | 66 | 2 | 0 | 17 | **(ii)** minted `lib.rs:24380` |
| `java/nio/channels/SocketChannel` | ABSTRACT | 41 | 2 | 0 | 41 | **(ii)** minted `socket_channel.rs:1398`, `:4766`, `:4905` |
| `java/nio/channels/ServerSocketChannel` | ABSTRACT | 25 | 2 | 0 | 25 | **(ii)** minted `socket_channel.rs:4334` |
| `java/nio/file/Path` | INTERFACE | 20 | 0 | 18 | 3 | **(ii)** minted `lib.rs:22869`, `:23484`, `:14465` — but see §3.1 |
| `java/nio/channels/AsynchronousSocketChannel` | ABSTRACT | 19 | 2 | 11 | 5 | **(ii)** minted `async_socket.rs:611`, `:2099` |
| `java/nio/channels/AsynchronousFileChannel` | ABSTRACT | 16 | 2 | 11 | 2 | **(ii)** minted `lib.rs:21919` — **§2, was "candidate"** |
| `java/nio/channels/AsynchronousChannelGroup` | ABSTRACT | 15 | 0 | 14 | 0 | **(ii)** minted `async_socket.rs:2023` |
| `java/lang/Process` | ABSTRACT | 13 | 0 | 0 | 4 | (iii) partial 4/13 — `process.rs` already splits the fabricated half onto `cratonvm/synthetic/*` |
| `java/nio/channels/AsynchronousServerSocketChannel` | ABSTRACT | 12 | 2 | 3 | 6 | **(ii)** minted `async_socket.rs:3099` — **§2, was "candidate"** |
| `java/nio/channels/Selector` | ABSTRACT | 11 | 0 | 2 | 10 | (iii) partial 10/11 |
| `sun/nio/ch/SelectorImpl` | ABSTRACT | 10 | 0 | 0 | 2 | (iii) partial — already an `Impl`, and still abstract |
| `java/io/InputStream` | ABSTRACT | 9 | 0 | 0 | 1 | (iii) partial 1/9 — **the live superclass-walk hazard, `H11-1` N1** |
| `java/nio/channels/SelectionKey` | ABSTRACT | 9 | 0 | 0 | 9 | (iii) **already on BOTH** |
| `sun/nio/ch/SelectorProviderImpl` | ABSTRACT | 9 | 0 | 0 | 0 | **(?)** OPEN |
| `java/nio/file/WatchKey` | INTERFACE | 5 | 0 | 0 | 2 | **(ii)** minted `lib.rs:23096` |
| `java/io/OutputStream` | ABSTRACT | 4 | 0 | 0 | 1 | (iii) partial 1/4 — same hazard as `InputStream` |
| `java/nio/channels/FileChannel` | ABSTRACT | 4 | 0 | 0 | 1 | **(ii)** minted `lib.rs:11665` |
| `java/nio/channels/MembershipKey` | ABSTRACT | 4 | 0 | 0 | 1 | **(ii)** minted `datagram.rs:325` |
| `java/nio/channels/SelectableChannel` | ABSTRACT | 4 | 0 | 0 | 3 | (iii) partial 3/4 |
| `java/nio/channels/spi/SelectorProvider` | ABSTRACT | 4 | 0 | 0 | 2 | (iii) partial 2/4 — **abstract SPI: intercepts every application provider** |
| `java/nio/file/WatchService` | INTERFACE | 4 | 0 | 0 | 1 | **(ii)** minted `lib.rs:22918` |
| `java/nio/MappedByteBuffer` | ABSTRACT | 3 | 0 | 0 | 0 | **(?)** OPEN |
| `java/nio/channels/Pipe` | ABSTRACT | 3 | 3 | 3 | 0 | **(ii)** minted `pipe.rs:1024` — the factory object itself |
| `java/nio/channels/Pipe$SinkChannel` | ABSTRACT | 3 | 0 | 3 | 3 | (iii) **BOTH**, §4 blocks the deletion |
| `java/nio/channels/Pipe$SourceChannel` | ABSTRACT | 3 | 0 | 3 | 3 | (iii) **BOTH**, §4 blocks the deletion |
| `java/nio/channels/spi/AbstractSelectableChannel` | ABSTRACT | 3 | 0 | 0 | 3 | (iii) **already on BOTH** |
| `java/nio/file/WatchEvent` | INTERFACE | 3 | 0 | 0 | 0 | **(ii)** minted `lib.rs:23203` |
| `java/io/DataInput` | INTERFACE | 2 | 0 | 0 | 0 | **(?)** OPEN → **REMOVED**, `H11-3` |
| `java/io/DataOutput` | INTERFACE | 2 | 0 | 0 | 0 | **(?)** OPEN → **REMOVED**, `H11-3` |
| `java/nio/channels/NetworkChannel` | INTERFACE | 2 | 0 | 0 | 1 | (iii) partial |
| `java/io/Closeable` | INTERFACE | 1 | 0 | 0 | 1 | (iii) **BOTH** — dead, blocked by a unit test, `H11-3` N1 |
| `java/lang/AutoCloseable` | INTERFACE | 1 | 0 | 0 | 1 | (iii) **BOTH** — same |
| `java/nio/file/FileSystem` | ABSTRACT | 1 | 0 | 0 | 0 | **(?)** OPEN |
| `java/nio/file/spi/FileSystemProvider` | ABSTRACT | 1 | 0 | 0 | 0 | **(?)** OPEN |
| `sun/nio/ch/DirectBuffer` | INTERFACE | 1 | 0 | 0 | 1 | (iii) **BOTH** |
| `sun/nio/ch/NativeDispatcher` | ABSTRACT | 1 | 0 | 0 | 0 | **(?)** OPEN |

**36 classes, 334 rows.** 237 of those rows (14 classes) are on a class the VM
mints.

### 3.1 `java/nio/file/Path` is TWO populations under one name

`Paths.get(".")` returns a real `sun.nio.fs.WindowsPath` (§1), so the ordinary
file-system path is not fabricated — yet `lib.rs:22869` / `:23484` / `:14465`
DO mint objects named `java/nio/file/Path`, on the watch-service and async
routes. So the same 20 registrations serve a real JDK receiver on one code path
and a fabricated one on another. **Its `dupX` is 18**: `native-builtins`
registers 18 of the same 20 triples. Anyone retiring `Path` rows has to settle
which population each row is for AND which crate owns the slot. Not attempted
here; `[2 producers, 1 slot]`.

---

## 4. The trap under "just delete the dead abstract row"

`java/nio/channels/Pipe$SourceChannel` and `Pipe$SinkChannel` look like the
cleanest deletion in the crate: `H11-1` §3.3 measures all six rows at
`invocations: 0` while the `sun/nio/ch/*Impl` rows take every call, and the
comment defending them is false. **Deleting them from `native-io` removes
nothing.**

`native-builtins/src/phases_late/net_channels.rs` (~2281–2400) registers the
**same six triples**, and the dump shows both:

```text
bridge inv=0  java/nio/channels/Pipe$SourceChannel.read(…)I
              [native-builtins/src/phases_late/net_channels.rs:2281] owns_slot=false
bridge inv=0  java/nio/channels/Pipe$SourceChannel.read(…)I
              [native-io/src/pipe.rs:1428]                          owns_slot=true
```

`native-io` wins today. Delete its line and the slot does not disappear — it
falls to `net_channels.rs`, whose callback is **a different implementation with
a different field layout** (fd at slot 1, open flag at slot 0; `pipe.rs`'s
bodies use neither). So the "obviously safe" deletion is a silent
implementation swap on a slot nobody is watching. `[dup nati]`,
`[2 producers, 1 slot]`. Retiring these six is a single commit spanning both
crates, with a build. **Not done. `H11-3` N2.**

`dupX` in §3's table is exactly this hazard, counted. It is non-zero for
`Path` (18/20), `AsynchronousChannelGroup` (14/15), `AsynchronousSocketChannel`
(11/19), `AsynchronousFileChannel` (11/16), `Pipe` (3/3),
`Pipe$SourceChannel`/`Pipe$SinkChannel` (3/3 each) and
`AsynchronousServerSocketChannel` (3/12). **For 55 of the 334 rows, editing
`native-io` alone cannot remove the registration.**

---

## 5. Method

1. `cratonvm --jdk-only --dump-native-registry reg.json -cp <scratch> H11Dispatch`
   and the same with `--real-jdk` into `reg-compat.json`.
2. Filter `natives[]` to `registered_by.startswith("native-io/")`; group by
   `class`; sum `invocations`; count `owns_slot`; count rows whose triple also
   appears with a non-`native-io` `registered_by` (that is `dupX`).
3. `javap -p <dotted class>` once per class, cached; read the first line
   containing ` class ` or ` interface ` for `interface` / `abstract` / `final`
   / concrete. Only `ABSTRACT` and `INTERFACE` classes are in §3.
4. Fabrication sites: the **multiline** ripgrep in §2, plus a second pass for
   the other minting helpers this crate uses — `ensure_class_initialized`,
   `new_object_initialized`, `alloc_typed_buffer`, `ctx.alloc_object` (which
   takes NO class name at all and cannot be attributed by grep; `lib.rs:26552`
   is one such site and is not in §3's minted column).
5. `real_declaring_method{loaded, declared, acc_native, has_code}` is in the
   dump and replaces a per-triple `javap` for the shadow/bridge/absent-method
   question. Prefer it: it reads the image the VM actually loaded.

Scripts are ~60 lines each, in the session scratchpad.

---

## 6. What this census is NOT, and how it differs from `H5-1`'s

**This is a per-boot REACHABILITY census. `H5-1`'s is a static registration
parse. Neither is a superset of the other and they must not be subtracted.**

| | this record | `H5-1` §2/§3.4 |
|---|---|---|
| source | live `--dump-native-registry` | regex parse of `register*(` call sites |
| counts | what a `--jdk-only` boot **admitted and reached** | what the source **writes** |
| `native-io` total | **925** rows strict / **1056** compatible | **1493** registrations |
| sees `SyntheticStub` rows | **no** — strict mode refuses them | yes |
| sees unreached registrars | **no** | yes |
| sees loop-expanded and macro-generated rows | **yes** | partly (§2.C lists 15 unresolvable sites) |

MEASURED, both dumps, same boot shape: `native-io` contributes **1056**
registrations in compatible mode (922 `Bridge`, 3 `Intrinsic`, 131
`SyntheticStub`) over 992 distinct triples. `--jdk-only` **refuses all 131
`SyntheticStub` rows**, leaving **925** over 861 triples. The refused 131 are
concentrated in `sun/nio/ch/WindowsFileDispatcherImpl` (30),
`cratonvm/synthetic/*` (25), `sun/nio/cs/Stream{Encoder,Decoder}` (22), the four
watch-service classes (36) and `java/io/FileInputStream` (7 — the block `H5-A`
retagged). Whole-registry:
strict 10422 rows vs compatible 12142, i.e. 1693 synthetic-stub rows refused and
**27 `Bridge` rows also absent in strict** (9804 → 9777) — that second number is
not explained by this record and may be `no_image_receiver` re-tagging; it is
worth someone's grep.

The largest single discrepancy with `H5-1`'s map: **`java/nio/ByteBuffer` and
the six typed buffers appear in NEITHER mode's dump under a `native-io`
`registered_by`.** `H5-1` §3.4 credits `native-io` with 24 `ByteBuffer` and 18
`CharBuffer` shadows. In a live boot those rows come from
`native-builtins/src/servlet.rs`, `.../lib.rs` and
`.../phases_late/charset_buffers.rs`. Either the `native-io` typed-buffer
registrar is not reached on this boot path or it is reached only under a feature
this build lacks. **I did not chase it, and neither number should be quoted
without saying which instrument produced it.** `[MODE≠feat]`.

---

## 7. OUT-OF-FILE EDITS REQUIRED

**None from this record.** Two are *requested* of other owners, as information:

* `native-builtins/src/phases_late/net_channels.rs` — its six
  `Pipe$SourceChannel` / `Pipe$SinkChannel` rows are the blocking half of §4.
  They lose the slot to `native-io` today, so they are also dead, and retiring
  both halves together is one commit.
* whoever owns `docs/jdk-only-runtime-services.md`'s P1 row — §0's table is the
  replacement for its prescription. I did not edit it; `INDEX.md` and the
  roadmap are out of bounds for this lane.

---

## 8. NOMINATIONS

**N1 — the OPEN column is the entire remaining move-candidate list, and after
this lane it is 5 classes and 15 registrations.**
`sun/nio/ch/SelectorProviderImpl` (9), `java/nio/MappedByteBuffer` (3),
`java/nio/file/FileSystem` (1), `java/nio/file/spi/FileSystemProvider` (1),
`sun/nio/ch/NativeDispatcher` (1); `java/io/DataInput` (2) and
`java/io/DataOutput` (2) were the other four and `H11-3` removed them rather
than moved them. **The P1 row's "~200 registrations in the wrong place"
resolves to at most 15, and possibly zero** — two of the five are already
`Impl`-named classes that happen to be abstract, and have nowhere lower to go.
Whoever rewrites that row should say so.

**N2 — `java/nio/channels/spi/SelectorProvider` (4 rows) is an abstract SPI and
is the sharpest instance of `H11-1` N1's hazard.** An application that ships its
own `SelectorProvider` subclass and does not override, say, `openSelector()`
gets this crate's body through the superclass walk. Probe: a user
`SelectorProvider` subclass, diffed against HotSpot. `H5-1` §3.4 flagged this
class and nobody has probed it.

**N3 — `ctx.alloc_object(N)` mints with no class name and is invisible to every
fabrication census in this directory.** `native-io/src/lib.rs:26552` is one
call. A grep-based site list — mine, `H5-1`'s, and any future one — cannot
attribute it. Either those sites need a named helper or the census needs a
run-time instrument (`H11-1` N4's `resolved_via` field would also serve here).

**N4 — 27 `Bridge` rows are present in a compatible dump and absent in a strict
one, and nothing in this directory explains them.** §6. Strict mode is supposed
to drop `SyntheticStub` and admit `Bridge`; 27 bridges going missing is either
`no_image_receiver` demotion working as designed or a silent refusal nobody has
named. One diff of the two `natives[]` arrays answers it.

**N5 — the `twin` column in §3 is a heuristic and should be replaced.** It
matches any registered class whose simple name ends in `Impl` and shares the
method's name+descriptor; it does not check subtyping. Every "(iii) partial"
verdict is only as good as that. `javap`-derived supertype sets would make the
column exact, and the whole table is generated from one script.
