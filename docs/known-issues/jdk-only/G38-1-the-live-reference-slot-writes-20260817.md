# G38-1 — the live reference-slot writes, and the 79 in this lane's files that cannot be

**Status:** RUNTIME-MEASURED (100 vectors + a purpose-built probe) /
SOURCE-FIXED (2 sites) / REACHABILITY-FALSIFIED (77 sites) /
AFTER-NOT-MEASURABLE-BY-THIS-LANE.

**Provenance.** Every runtime number below is MEASURED on
`C:/craton/target-rel3/release/cratonvm.exe`, built from
`C:\craton\cvm-mergecheck` at commit `9ae371468`, which was this repository's
`HEAD` when the sweep ran — so it contains G30-1's instrument and provably
none of this lane's edits. Real layouts are `javap -p` against Eclipse
Adoptium 25.0.3.9. The reachability results are read out of
`--dump-native-registry` and out of the tree with file and line, not inferred
from a symptom.

This record continues
`G30-1-the-silent-reference-slot-coercion-20260817.md` and
`G25-1-the-int-written-into-a-reference-slot-20260817.md`. It uses G30's
instrument for what G30 said it was for — "run a vector with that variable
set, and the VM will tell you which of your sites actually fire" — and the
answer for this lane's three files is **none of them, in any mode**, for a
reason that also retires a large fraction of G30's census. It fixes the two
sites in those files that a reader *can* reach, and it hands the measured
firing population to the lanes that own it.

---

## 0. The headline

| claim | the brief / G30-1 §3.3 | this record (MEASURED) |
|---|---|---|
| group A sites in `servlet.rs` / `net_channels.rs` / `tls.rs` | "the shared `SSLSocket`/`SSLContext` models" are in your files | **zero.** The measured `SSLContext#1`, `SSLSocket#2/#3`, `AsyncServerSocketChannel#0` stores are owned by `net_phase_e.rs`, `ssl_security.rs` and `native-io/src/async_socket.rs` (§2.2) |
| this lane's census sites | 22 + 19 + 18 (brief) / 28 + 30 + 21 (census2) | **79 sites, 0 firing** over 100 vectors and 5 431 coercion events (§2.1) |
| why they do not fire | (latent — "this corpus did not observe it") | **structural.** 63 of 79 are in registrars reachable only from `register_synthetic_overrides`, which runs only when `config.use_synthetic_jdk` — and in that mode every class is a `CompatibilityStub`, which `resolve_field_descriptor_byte_cached` short-circuits *before* any coercion (§3) |
| `pointer-into-primitive` | "No site in the census produces it and none fired in the sweep" (G30 §4.1) | **229 occurrences, 217 from one line**: `jca/provider_chain.rs` writes a live `String` reference into `java.util.Hashtable.threshold`, an `int` (§4.1) |
| the largest firing site in the tree | `ArrayList#1 elementData`, 62 static sites (G30 N5) | `native-builtins/src/reference.rs` — **2 737 of 5 431 events**, and it is the benign `ReferenceQueue.head` read G30 §2.1 already identified (§4) |

| surface | before | after |
|---|---|---|
| `s2_byte_order_object`'s fallback on a real `java.nio.ByteOrder` | `Int(ord)` into `name : String` → **null** → `LITTLE_ENDIAN.toString()` prints `BIG_ENDIAN` and the two constants compare equal | writes the genuine name String at the slot the class declares; the flag write is unchanged |
| `s2_bb_alloc_direct` on a real `java.nio.ByteBuffer` | `Object(None)` over `mark : int` and `Int(0)` over `segment : MemorySegment` → **the order flag destroyed** | by-name writes first, indexed overlay behind `s2_bb_synthetic_layout` — the guard the other three members of that family already carry |
| the bare six-slot synthetic layout | its current answer | **its current answer, slot for slot** (pinned, §5) |
| the other 77 census sites in these three files | their current answer | **their current answer** — left, with the reachability proof, and nominated |

---

## 1. What was run

`--jdk-only`, `CRATONVM_DBG_COERCION=1`,
`CRATONVM_DISABLE_DEFAULT_WATCHDOG=1`, classpath
`regression-suite/build`, module path a locally built
`cratonvm.jdkonly.svc`.

* the eight vectors this lane was asked to verify;
* **all 100 compiled `regression-suite/src` vectors** (the sweep), because the
  eight could not falsify a claim about files none of them reach;
* `G38Probe`, written for this record, which drives `ByteBuffer.allocate` /
  `allocateDirect` / `order(…)` / `wrap`, `ByteOrder.nativeOrder` /
  `toString`, `asCharBuffer`, `SocketChannel` / `ServerSocketChannel` /
  `DatagramChannel` / `Pipe` open+`isOpen`, and
  `Class.getResourceAsStream` — i.e. one call into every live registrar in
  `servlet.rs` and `net_channels.rs` that the census names. **23 of 23 CK
  lines match HotSpot 25.0.3+9-LTS**, and the run produces 6 coercion events,
  5 of them in `native-io/src/pipe.rs`.

The same eight vectors were also re-run in the DEFAULT (compatibility) mode,
because `--jdk-only` is not the only mode in which real class bytes are
authoritative. Same answer: 476 events, none in these three files.

`CRATONVM_DBG_OVERLAY=1` was run alongside on `target-rel2` (which predates
the counter) and reproduces G30 §2.1's store table plus five store sites G30's
six-vector sweep did not reach — see §4.2.

---

## 2. The measurement

### 2.1 Coercion events, `--jdk-only`

| vector | events | vector | events |
|---|---|---|---|
| `RJdkNet` | 249 | `RSocketChannelInterrupt` | 19 |
| `RSslLiveSession` | 126 | `RJdkAsyncChannel` | 17 |
| `RChannelInterrupt` | 86 | `RJdkX509Intercept` | 29 |
| `RSslNullSession` | 55 | `RCrypto` | 33 |

Over the full sweep: **101 runs, 63 with at least one event, 5 431 events**
— 5 196 `primitive-into-reference`, 229 `pointer-into-primitive`, 6
`null-into-primitive`.

Every event carries a backtrace (`CRATONVM_DBG_COERCION=1`). Attributing each
to its first non-`gc`, non-`vm_exec`-boundary frame:

| events | file |
|---|---|
| 2 737 | `native-builtins/src/reference.rs` |
| 1 013 | `native-builtins/src/properties_sidetable.rs` |
| 398 | `native-builtins/src/lang_class.rs` |
| 290 | `native-builtins/src/lib.rs` |
| 217 | `native-builtins/src/jca/provider_chain.rs` |
| 192 | `native-builtins/src/net_phase_e.rs` |
| 163 | `vm/src/threading/monitor.rs` |
| 102 | `native-collections/src/lib.rs` |
| 57 | `vm/src/runtime/interpreter/invoke.rs` |
| 22 | `native-builtins/src/unsafe_natives_ext.rs` |
| 14 each | `phases_late/jar_manifest.rs`, `phases_late/foreign_ffm.rs` |
| 12 | `phases_late/ssl_security.rs` |
| 7 each | `classloader.rs`, `x509_manager.rs` |
| ≤ 4 | `native-io/src/zip_real_jar.rs`, `jmx.rs`, `phases_late/nio_file.rs`, `keystore.rs`, `native-io/src/async_socket.rs`, `t27_tls.rs` |
| 169 | unattributable (the whole backtrace inlined away) |
| **0** | **`servlet.rs`, `phases_late/net_channels.rs`, `tls.rs`** |

The last row is the finding. It is not "no vector happened to reach them";
§3 shows they cannot be reached.

### 2.2 The five group-A store sites, and who owns them

`--dump-native-registry` resolves each measured store to the registrar that
`owns_slot`. Not one is in this lane's files:

| class#slot | real field | registrar that owns the method | this lane's file? |
|---|---|---|---|
| `javax/net/ssl/SSLContext#1` | `contextSpi : SSLContextSpi` | `net_phase_e.rs:13813` (`getInstance`), `:13917` (`init`) | no |
| `javax/net/ssl/SSLSocket#2` | `socketLock : Object` | `ssl_security.rs:3448` (`SSLSocketFactory.createSocket`) | no |
| `javax/net/ssl/SSLSocket#3` | `in : InputStream` | same | no |
| `java/nio/channels/AsynchronousServerSocketChannel#0` | `provider : AsynchronousChannelProvider` | `native-io/src/async_socket.rs:3104` (`aio_assc_open`) | no |
| `java/util/TreeMap$EntrySet#0` | `this$0 : TreeMap` | `native-collections/src/lib.rs` | no |

`tls.rs` registers `SSLContext.getInstance`, `TrustManagerFactory.getInstance`,
`KeyManagerFactory.getInstance` and `KeyStore.getInstance` — **and none of
those registrations appear in the registry at all.** The whole file
contributes exactly three live rows, all `org/conscrypt/NativeCrypto`
(`tls.rs:3619/3625/3631`), reached from `lib.rs:7263`. `net_channels.rs`
contributes 37 rows of which 4 own their slot, and all four
(`AsynchronousFileChannel.force`, `AsynchronousSocketChannel.connect`,
`Pipe$SourceChannel.configureBlocking`, `Pipe$SinkChannel.configureBlocking`)
write no fields. `servlet.rs` contributes 215 rows, 200 owning — the
`ByteBuffer`/typed-buffer/`ByteOrder` family, `URLClassLoader`,
`ScriptEngineManager`, jython and two Spring mocks.

---

## 3. Why 63 of the 79 cannot fire — the reachability proof

The defect needs **two** things at once: a native writing an indexed
primitive, *and* a **real, non-stub** loaded class at that slot. Those two
conditions are mutually exclusive for most of this lane's census.

**3.1 The registrars are synthetic-mode-only. SOURCE-VERIFIED.**
`vm/src/vm/vm_init.rs:1930` gates the call:

```rust
#[cfg(feature = "synthetic-jdk")]
{
    if config.use_synthetic_jdk {
        register_builtins(&mut native_methods);   // essentials, then
                                                  // register_synthetic_overrides
```

`register_synthetic_overrides` (`native-builtins/src/lib.rs:22259`) is the
only caller of `register_tls_natives` (`lib.rs:24775`), of
`register_phase72_natives` (which is the only caller of
`net_channels::register_datagram_channel` and
`register_p72_server_socket`), and of `servlet::register_s2_nio`
(`lib.rs:24841`, the only caller of `register_s2_socket_channel` /
`register_s2_server_socket_channel`). `net_channels::register_p58_nio_channels`
says so about itself, in the file, and has said so since 2026-08-12. This is
`vm_init.rs`'s own F30 gate (`docs/…/F30-1-…md`) applied to three more
registrars.

MEASURED: the shipping binary is not even built with the feature —
`--dump-native-registry` reports `"synthetic-stub": 0` of 10 691 natives, and
`--synthetic-jdk` is refused with *"Needs a binary built with the
`synthetic-jdk` [feature]"*.

**3.2 And in the one mode that does run them, there is no descriptor.
SOURCE-VERIFIED.** `--synthetic-jdk` conflicts with `--real-jdk` and
`--jdk-only`; every class is fabricated, i.e.
`ClassOrigin::CompatibilityStub` (`classloading/src/class_origin.rs:123`).
`resolve_field_descriptor_byte_cached` (`vm/src/vm/vm_exec.rs:4208`) opens
its slow path with

```rust
if concrete_cls.origin.is_compatibility_stub() {
    None            // and the same bail-out for any stub ANCESTOR
}
```

so `NativeContextImpl::set_field` takes the descriptor-**less**
`heap.set_field` and `coerce_field_value_by_descriptor` is never called. The
comment there says exactly why, and names this defect as the thing it is
avoiding. **A registrar that runs only in synthetic mode therefore cannot
produce this species at all**, whatever `javap` says its slots collide with.

That argument is not specific to this lane. Applied to G30's census it
retires a large block of groups E and F wholesale, and it is the reason a
static census over `javap` over-counts a *second* time, after the 791 §3.4
already named.

**3.3 The 16 that are NOT excluded by §3.1–3.2**, all in `servlet.rs`, all in
live registrars:

| site | verdict |
|---|---|
| `servlet.rs:1256` `Class.getResourceAsStream` → `ByteArrayInputStream` slots 0–3 | **census false positive.** Real layout is `buf:[B / pos:I / mark:I / count:I` and the model writes exactly that. The joined-statement matcher attributed the alloc line to the wrong write. |
| `servlet.rs:3109` `bb_write_hb` | **already guarded** by `s2_bb_synthetic_layout` (2026-07-12). |
| `servlet.rs:5173` `s2_bb_as_char_buffer` | **already guarded** by the same screen. |
| `servlet.rs:3860` `s2_byte_order_object` fallback | **real, live, FIXED (§5.1).** |
| `servlet.rs:3016`/`3021` `s2_bb_alloc_direct` | **real, gated on a different class, FIXED (§5.2).** |

---

## 4. The population that IS firing — handed over

### 4.1 `pointer-into-primitive` is not empty. It is 229, and 217 are one line.

G30 §4.1 introduced this species with *"No site in the census produces it and
none fired in the sweep; it is counted because if it ever fires, nothing else
in the VM will say so."* It fires. MEASURED, `descriptor=I`,
`value=Object(Some(ObjectRef { … }))`, attributed to
`native-builtins/src/jca/provider_chain.rs:317`, in 63-vector-wide use.

The write is `make_provider`'s "synthetic fallback" block:

```rust
ctx.set_field(p, 0, Value::Object(Some(n)));      // name String
ctx.set_field(p, 1, Value::Double(version));
ctx.set_field(p, 2, Value::Object(Some(info)));   // info String
```

`java.security.Provider extends java.util.Properties extends
java.util.Hashtable`, and the real transitive layout (superclass first,
statics excluded) opens

```text
0 table:[Ljava/util/Hashtable$Entry;   1 count:I   2 threshold:I
3 loadFactor:F   4 modCount:I   5 keySet:L   6 entrySet:L   7 values:L
8 defaults:L (Properties)   9 map:L   10 name:L (Provider)   11 info:L
12 version:D
```

so slot 2 is `Hashtable.threshold`, an `int`, and the `info` String's **heap
address is written into it as a number**. That is the worst of the four
species by G30's own ranking: not merely wrong, non-deterministic, and it
publishes a pointer into a Java `int` that `Hashtable.addEntry` compares
against `count` to decide whether to rehash. Slot 1 takes a `Double` where
`count : int` is declared, and slot 0 takes the name String where `table` is
declared — a reference into a `[` slot, which is not coerced and not
reported.

The block's own comment says it exists so that
`phases_early::register_phase53_security` callers "still see consistent
state", three lines after the *correct* write
(`set_field_by_name(p, "initialized", …)`) that shows the by-name form was
already available at this site. **NOMINATION 1.**

### 4.2 Five store sites in no earlier record

`CRATONVM_DBG_OVERLAY=1` on the eight vectors reproduces G30 §2.1 and adds,
from `RSslLiveSession` (which G30's six-vector sweep did not include):

| class | slot | real descriptor | written | count |
|---|---|---|---|---|
| `sun/net/www/protocol/https/HttpsURLConnectionImpl` | 1 | `Z` | `Object(Some(…))` | 3 |
| `sun/net/www/protocol/https/HttpsURLConnectionImpl` | 9 | `L` | `Int(0)` | 3 |
| `sun/security/pkcs12/PKCS12KeyStore` | 4 | `L` | `Int(1)`, `Int(2)` | 2 |
| `sun/security/ssl/SunX509KeyManagerImpl` | 0 | `L` | `Int(1)` | 1 |
| `sun/security/ssl/X509TrustManagerImpl` | 0 | `L` | `Int(1)` | 1 |

`HttpsURLConnectionImpl#1` is a **second** `pointer-into-primitive`: a live
object reference into a `boolean`. All five are under
`RSslLiveSession.buildContexts()` / `.open(…)`, i.e. the vector that is
already failing, and none is in this lane's files.

### 4.3 `native-io/src/pipe.rs::alloc_channel`, and its dead twin here

`G38Probe` measures 5 events in `native-io/src/pipe.rs:761/763/768`, writing
`Int` into `java/nio/channels/Pipe$SourceChannel` and `Pipe$SinkChannel`
slots 0/1/2 — which the real hierarchy declares
`closeLock : Object`, `closed : boolean`, `interruptor : sun.nio.ch.Interruptible`.
So `closeLock` and `interruptor` become null and the "open" flag lands on
`closeLock` rather than on `closed`.

This lane's `net_channels.rs:2242` registers a byte-for-byte equivalent
`Pipe.open`, with the *same* three writes and the *same* slot beliefs — and
loses the slot to `native-io/src/pipe.rs:1238` (`owns_slot=false`, MEASURED).
Repairing the copy in this lane's file would change nothing a program can
observe and would leave the two models disagreeing. **NOMINATION 3.**

---

## 5. What changed — the site table

Both fixes are in `native-builtins/src/servlet.rs`. Both are a **correct
value in the right slot**, not a refusal.

| # | class | slot | real descriptor | was written | is written now | instrument: firing before? | silent after? |
|---|---|---|---|---|---|---|---|
| 1 | `java/nio/ByteOrder` | 0 | `Ljava/lang/String;` (`name`) | `Value::Int(ord)` → coerced to **null** | `Value::Int(ord)`, then the genuine `"BIG_ENDIAN"` / `"LITTLE_ENDIAN"` String at the slot the class declares for `name`, when that slot is in range | **not observed** — 0 events over 100 vectors + `G38Probe`; the arm is a fallback that the measured runs never entered (§5.1) | not measurable by this lane (§7) |
| 2a | `java/nio/ByteBuffer` | 0 | `I` (`mark`) | `Value::Object(None)` → coerced to **`Int(0)`** | `mark` written **by name** as `Int(-1)`; slot 0 written only behind `s2_bb_synthetic_layout` | **not observed** — the caller gates on `java/nio/DirectByteBuffer` being a stub, which no measured run reached (§5.2) | not measurable |
| 2b | `java/nio/ByteBuffer` | 5 | `Ljava/lang/foreign/MemorySegment;` (`segment`) | `Value::Int(0)` (the ORDER FLAG) → coerced to **null** | byte order written through `native_io::seed_buffer_byte_order` into the `bigEndian`/`nativeByteOrder` pair `s2_bb_order` reads; slot 5 written only behind the screen | **not observed**, same gate | not measurable |
| 2c | `java/nio/ByteBuffer` | 1,2,3,4 | `I`,`I`,`I`,`J` | `position`/`limit`/`capacity`/`address` by index | the same values, **by name**, then by index behind the screen | n/a — these four aliased correctly by luck | unchanged |

### 5.1 `s2_byte_order_object` — the unrepaired twin

`phases_late/foreign_ffm.rs::p67_byte_order_object` already solves exactly
this problem for exactly this class, with the GC-pinning shape and the
three-shape argument, and its doc comment is a W6-3 slot-index audit of
`java.nio.ByteOrder`. `servlet.rs`'s copy was never given the same repair.

The important correction is to the premise. Reaching the fallback does **not**
prove the real class is absent: `ensure_class_initialized` can succeed while
`static_field_index_by_name` / `get_static_field` miss the constant, and on
that path `alloc_concurrent_synthetic` hands back an object of the **real**
`java.nio.ByteOrder`, whose only instance field is `private final String name`
at slot 0. `Int(ord)` there is nulled, and both readers in the same registrar
— `toString` and `s2_byte_order_ord`, which back `equals` — decode a null as
`0`, i.e. `BIG_ENDIAN`. So `LITTLE_ENDIAN.toString()` printed `BIG_ENDIAN` and
`BIG_ENDIAN.equals(LITTLE_ENDIAN)` was true.

The flag write is kept, because it is the synthetic-stub layout and every
reader still falls back to it; the String is written *on top*, only when the
class declares `name` at a slot the object has. Both readers already accept a
String there (`toString` returns it directly; `s2_byte_order_ord` maps
`"LITTLE_ENDIAN"` → 1), and so does the third reader in another file,
`foreign_ffm::p67_layout_is_little`, which asks for `name` by name first.

### 5.2 `s2_bb_alloc_direct` — the last unguarded member of a family of four

`bb_write_hb`, `s2_bb_as_char_buffer` and `s2_bb_new_heap_view` all write the
real named fields first and gate the indexed `BB_*` overlay on
`s2_bb_synthetic_layout`; the doc comment on `bb_write_hb` is a written-down
account of the 2026-07-12 bug that forced the guard
(`ByteBuffer.allocate(n)` throwing `ArrayIndexOutOfBoundsException` on the
first bulk put, because slot 4 clobbered the real `address`).
`s2_bb_alloc_direct` was left out.

It is reached only when `java/nio/DirectByteBuffer` is a synthetic stub — the
real-JDK arm above it runs the genuine `DirectByteBuffer(int)` constructor —
but that gate is on `DirectByteBuffer`, not on `ByteBuffer`, so it does not
imply the six-slot layout the indexed block assumes. `hb`, `offset` and
`isReadOnly` are deliberately *not* written: a fresh allocation already reads
back null/0 for all three, and every extra name is one more slot to get wrong
on a layout this function cannot see.

### 5.3 The tests

In `servlet.rs`'s existing `mod tests`, beside the sibling
`a_real_layout_buffer_without_big_endian_refuses_rather_than_writing_slot_zero`
whose fixture idiom they reuse (`set_declared_fields` + `alloc_object`):

* `the_byte_order_fallback_names_the_constant_when_the_class_declares_name` —
  the falsifying half. Declares `name : Ljava/lang/String;` at slot 0 and
  asserts the fallback leaves a String reading its own name there, for both
  constants. Before the fix the slot holds an `Int` and the test panics with
  the value the coercion turns into null.
* `a_byte_order_shape_with_no_in_range_name_slot_keeps_only_the_flag` — the
  other half, so the fix cannot be "write a String everywhere". A shape with
  no in-range `name` slot is byte-identical to the pre-G38 object.
* `the_byte_order_readers_decode_a_name_string_as_well_as_the_flag` — drives
  the registered `toString` and `equals` natives over both constants, and
  asserts `BIG_ENDIAN.equals(LITTLE_ENDIAN)` is false and
  `BIG_ENDIAN.equals(BIG_ENDIAN)` is true. This is the assertion the null
  broke, and it pins that the repair did not move the defect into the
  readers.
* `the_direct_buffer_overlay_is_unchanged_on_the_bare_synthetic_layout` —
  the no-behaviour-change half of §5.2: on the six-slot layout every indexed
  slot still ends exactly where it did, and `s2_bb_arr` still answers `None`.
* `the_real_byte_buffer_layout_closes_the_direct_overlay_screen` — the other
  side: on the eleven-field JDK 25 layout the screen is closed, and
  `BB_ORDER` resolves to `segment` and `BB_ARRAY` to `mark`, which is the
  whole reason it must be.

---

## 6. Verification

`--jdk-only`, `regression-suite/run.sh`, `CV=target-rel3`, HotSpot
25.0.3+9-LTS as the oracle. This is the BEFORE state for §5 — see §7.

| vector | result | note |
|---|---|---|
| `RJdkNet` | PASS | 10 CK |
| `RSslNullSession` | PASS | 91 CK |
| `RSslLiveSession` | **FAIL** `rc=1` | pre-existing; 67 CK rows, the same count and the same failure G25-1 §0 records. Its five §4.2 store sites are all outside this lane |
| `RJdkAsyncChannel` | PASS | 6 CK |
| `RSocketChannelInterrupt` | PASS | 19 CK |
| `RChannelInterrupt` | PASS | 3 CK |
| `RJdkX509Intercept` | PASS | 11 CK |
| `RCrypto` | PASS | 19 CK |

`7 passed, 1 failed`. `G38Probe`: 23 of 23 CK lines identical to HotSpot.

---

## 7. What this lane did NOT do

* **It did not build or run its own change.** No binary contains §5. The two
  repaired sites were measured at **0 events** before, so "silent after" is
  not a claim this record can make; what it can and does pin is that the
  synthetic-layout answer is unchanged, by unit test.
* **It fixed no site in `net_channels.rs` or `tls.rs`.** §3 is why: for every
  one of their 51 census sites the slot's correct value is genuinely
  unavailable (a file descriptor, an open flag and an algorithm index have no
  home in `closeLock`/`interruptor`/`contextSpi`), and the models are shared
  with `net_phase_e.rs`, `ssl_security.rs`, `http2.rs` and
  `native-io/src/pipe.rs`, which this lane does not own. That is G25-1's
  NOMINATION 3 and it stands unchanged. Moving one file's indices alone would
  desynchronise the readers in the others — a widened wrong answer, which is
  worse than the bug.
* It did not apply G30's NOMINATION 1, so every event in §2.1 still reports
  `class_id=-1 index=-1` and the class had to be recovered from `javap` plus
  the backtrace. **That nomination is now the difference between a 4-hour and
  a 20-minute attribution, and it is still one line per call site.**
* It did not touch `gc/src/heap.rs`, `phases_early.rs`, `t27_tls.rs`,
  `native-collections/src/lib.rs`, `INDEX.md` or `README.md`.

---

## 8. NOMINATIONS, ranked by whether the slot is actually read

**N1 — `native-builtins/src/jca/provider_chain.rs:315-317`, `make_provider`.
217 measured `pointer-into-primitive` events across 63 vectors. THE WORST
SPECIES, AND IT IS LIVE.** A live `String` reference is written into
`java.util.Hashtable.threshold` (an `int`) and a `Double` into
`Hashtable.count`; `Hashtable.addEntry` reads `threshold` on every insertion
to decide whether to rehash. The correct values are in scope at the site and
the *correct form of the write is already on the line above it*
(`set_field_by_name(p, "initialized", …)`): `name`, `info` and `version` are
real `java.security.Provider` fields at slots 10, 11 and 12, resolvable by
name. G30 §4.1 predicted this species would be empty; it is the single
loudest thing in the sweep after the two benign readers. **Highest value in
this record.**

**N2 — `native-builtins/src/reference.rs` (2 737 events) and
`properties_sidetable.rs` (1 013).** Both are READS of an untouched slot
through `get_field_as(.., b'L')` — `ReferenceQueue.head` and
`Properties.defaults` — and both are the coercion doing its job (G30 §2.1).
They are nominated not for repair but because together they are **69 % of
every event in the sweep**, and until G30's NOMINATION 1 lands they are
indistinguishable in the log from a store that destroys a value. Give the
read path its own default-quiet threshold, or land N1-of-G30 so the
`access="read"` column is populated and these can be filtered.

**N3 — `native-io/src/pipe.rs:761/763/768`, `alloc_channel`. MEASURED
(§4.3).** `Int` into `Pipe$SourceChannel`/`SinkChannel` slots 0/1/2, which
are `closeLock : Object`, `closed : boolean` and
`interruptor : sun.nio.ch.Interruptible`. The "open" flag lands on
`closeLock` and never on `closed`, which is the field real
`AbstractInterruptibleChannel.isOpen()` reads — the exact shape of G16-1's
`ServerSocket.bound`. Fix it together with its dead twin at
`native-builtins/src/phases_late/net_channels.rs:2242-2253` (this lane's
file), which holds the identical belief and loses the slot; leaving the twin
is how the two drift.

**N4 — `native-builtins/src/lang_class.rs:1737`, `mirror_class_id`
(398 events).** A READ of `java.lang.Class` slot 0 through the
descriptor-aware path. This is the *read* counterpart of the W7-84 write
G30 §6 pinned, and it is the reason that write must stay descriptor-less: the
populator stores an `Int` there via `autobox`, and this reader takes it back
out — but the `get_field_as(b'L')` on the same slot nulls it. 398 events say
the fallback is not rare. G30 §6 pinned the write; nobody has looked at the
read.

**N5 — `native-builtins/src/net_phase_e.rs` (192 events) and
`phases_late/ssl_security.rs` (12).** The group-A sites: `SSLContext#1
contextSpi`, `SSLSocket#2 socketLock`, `#3 in`, `#0 impl`, plus
`net_phase_e.rs:8017/8636/8667/8744`'s `java/net/URL` slot family
(163 events, mostly reads of `URL` slots 1/3/4/5/6). These are the two files
that must move together (G25-1 NOMINATION 3). This lane confirms the census
attribution and adds the owner: `SSLContext.getInstance`/`init` are
`net_phase_e.rs:13813`/`:13917`, `SSLSocketFactory.createSocket()` is
`ssl_security.rs:3448`, both `owns_slot=true`.

**N6 — `native-collections/src/lib.rs` (102 events).** Not this lane's file.
The `ArrayList#1 elementData` concentration G30 N5 ranks first statically
(62 sites) is measured at 102 events here, so it is live as well as large.

**N7 — `vm/src/threading/monitor.rs:2074/2092` (163 events) and
`vm/src/runtime/interpreter/invoke.rs:2847/3614` (57, including all 6
`null-into-primitive` and one `pointer-into-primitive`).** These are VM-side,
not native-builtins, and appear in no census — `census2.py`'s roots do not
include a `set_field_as` reached from the monitor table or from
`invoke_cached_native_callback_impl`. Whatever the census's denominator is,
it is not the population.

**N8 — the census's reachability model, `scratchpad/g30/census2.py`.**
§3 shows the script's 400 sites include a large block that is structurally
incapable of firing, because a registrar reachable only from
`register_synthetic_overrides` never meets a non-stub class. The script
already resolves the enclosing file; resolving the enclosing `register_*`
function and joining it against `--dump-native-registry`'s `owns_slot` and
`registered_by` would separate "latent" from "impossible" for the whole
census in one pass. On this lane's three files that reclassification is
**79 sites → 2**.

**N9 — `phases_late/foreign_ffm.rs:1800/1806` (14 events),
`phases_late/jar_manifest.rs:1989/1990` (14),
`native-builtins/src/lib.rs:26586` `native_object_clone` (270),
`unsafe_natives_ext.rs:2677` (22), `classloader.rs:4087` (7),
`x509_manager.rs:4330/4401` (7), `keystore.rs:4019` (3),
`jmx.rs:3906/3907` (4, one of them `pointer-into-primitive`),
`native-io/src/zip_real_jar.rs` (4), `phases_late/nio_file.rs` (3).**
The measured tail, in one place, for whichever lane owns each file.
`native_object_clone` at 270 events is the largest unexamined one:
`Object.clone()` copying slot by slot through the descriptor-aware accessor
re-runs the coercion on every field of every cloned object, so it *amplifies*
whatever the original object already got wrong.

---

## 9. Files this lane touched

* `native-builtins/src/servlet.rs` — §5.1, §5.2 and the five tests in §5.3.
* `docs/known-issues/jdk-only/G38-1-…md` — this record.

Nothing else. No `INDEX.md` / `README.md` edit; no other Rust file;
`native-builtins/src/phases_late/net_channels.rs` and
`native-builtins/src/tls.rs` are unchanged, for the reason in §7.
