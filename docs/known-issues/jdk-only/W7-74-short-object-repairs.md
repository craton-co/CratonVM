# W7-74 — repairing the short objects: two were live, twelve are latent or dead behind an `Err(_)` arm, and two were never short

Status: the two `java/lang/Thread` carrier mirrors are repaired; the ratchet is
down from 30 to 28. The other rows are classified rather than repaired, and the
classification has a single structural spine that W7-73-short-object-blind-spot.md
did not draw: **every remaining short site sits on an
`Err(_) => alloc_object(ClassId::new(0), N)` arm of a
`match ctx.ensure_class_initialized(…)`.** The two `Thread` sites were the only
two in the population that were not, which is why they were the only two that
could be short on an image where the class they name really declares more.

Branch `fix/short-thread-objects-20260812`. **Nothing here was built or run
under CratonVM** — this lane writes code and docs only. Every declared width is
`javap -p` against the JDK 25.0.3.9 image on this Windows host, counted
transitively over the superclass chain with `static` excluded, the same oracle
and convention as W4-4-slot-index-species-sweep.md, W7-49-slot-index-recensus.md,
W7-59-layout-detector-coverage.md, W7-68-live-under-allocations.md and W7-73.
Every Java value quoted as "HotSpot says" is a transcript of
`probes/ShortThreadMirrorProbe.java` run on that image.

> **CORRECTIONS BANNER — 2026-08-12, later pass.**
>
> 1. **§5's "14 of 28" is an arithmetic slip. The answer is 12 of 28.** §1's
>    corrections took W7-73's 16 to **14 out of a population of 30**. The two
>    `java/lang/Thread` mirrors this record then repaired were **members of that
>    14**, so subtracting them from the population must subtract them from the
>    short count too: 30 → 28 and 14 → **12**, in the same edit. The ratchet's doc
>    comment and failure message inherited the 14 and are corrected with it.
> 2. **§2 row 12 (`FileChannel`) is DEAD, and its stated blocker was never the
>    real one.** The row reads *"latent — and the live map is shared with
>    `nio_file.rs`, so a one-sided renumber would break `isOpen()` (W7-68 §3.2)"*.
>    W7-72-ssc-socket-and-filechannel.md §2.1 shows that premise **inverted**: the
>    `isOpen` copy that reads the private slot lives in
>    `register_phase57_file_channel`, whose whole transitive caller chain is
>    `#[cfg(feature = "synthetic-jdk")]`, and which is overwritten by the
>    constant-returning copy even inside that build. It never runs in any shipping
>    configuration. W7-72 then paid the *actual* blocker — a cross-crate owner —
>    and moved the map in one step to `native-api/src/synthetic_file_channel.rs`.
>    The site's width is now `alloc_slots` = `base_for_class(…) + 2`, which makes
>    the row **structurally unable to be short**, by §1.3's own argument. It
>    leaves the short column.
> 3. **One row JOINS it, so the twelve is not eleven.**
>    `native-io/src/socket_channel.rs:637`'s `alloc_obj` is filed in W7-73 §3.2 as
>    `over` at 12 against 10. W7-66-live-over-allocations.md §4.3 narrowed all
>    four of its callers to `SC_OBJECT_SLOTS` = **6**, and
>    `SocketChannel`/`ServerSocketChannel` declare 10 — short by 4. It is on an
>    `Err(_)` arm, so §2's spine and §2.1's refusal both apply to it unchanged.
> 4. **Every line number in §1.4 and §2 has moved again.** The re-derived table is
>    W7-73-short-object-blind-spot.md §3.4. §1.4's *method* is the durable part:
>    these tables are a work queue and the next reader will `sed -n` them.
>
> §1.1, §1.2, §1.3, §2, §2.1, §3, §4 and §6 are otherwise re-read and hold.

> **RE-VERIFIED AGAINST THE TREE 2026-08-12 (later pass, A9 record triage).**
> Read only — nothing was built or run in that pass. Two things changed under
> this record, and every line number moved again.
>
> **1. The repair itself was AMENDED after this record was written, by
> `395e5790c fix(thread): a refused Thread.<init> must not publish an
> unconstructed mirror`.** §3.2's step 3 said "drive `Thread.<init>` … through
> `ctx.invoke`" and the landed helper discarded the result with `let _ =`. That
> was a hole this record did not see: allocation succeeds, so `mirror` is a
> full-width real `java.lang.Thread`, but if the constructor fails nothing ran —
> `holder` null, `name` null, `tid` 0 — and the callers **publish it to the
> thread registry anyway. That is byte-for-byte §4.1's `Unsafe.allocateInstance`
> object, the very shape this record measured as RED on HotSpot.** Today
> `alloc_carrier_thread_mirror` (`native-builtins/src/lib.rs:6772`) checks
> `.is_err()`, unpins and returns `None`, which is §3.2's step 5 outcome. Worth
> naming as a lesson, not just as a diff: **a repair that widens an object and
> then runs a constructor has TWO failure modes, and this record only reasoned
> about the first.** §4.1's own red-proof was the discriminator for the second
> and it was already in the file.
>
> **2. §3.3's hoist is real but is reached through a wrapper.**
> `is_synthetic_thread_layout` is at module scope
> (`native-builtins/src/lib.rs:6460`) and is still crate-root-private; the
> cross-module callers go through `pub(crate) fn
> thread_mirror_is_synthetic_layout` (`:6703`), which is what
> `alloc_carrier_thread_mirror` (`:6795`) and `jdk25_concurrency::sts_fork`
> (`jdk25_concurrency.rs:898`) call. Same value, one declaration, so the claim
> holds; the name in §3.3 is not the one you will grep for.
>
> **3. Everything else re-verified, item by item.** The ratchet is
> `const BOUND: usize = 28` with "**12** of today's 28" in its doc comment and
> "12 of the 28" in its failure message
> (`native-api/tests/layout_alias_coverage.rs:947`, `:877`, `:1006`) — the
> banner's arithmetic correction is in the code. Both carrier sites call the
> helper (`vertx_eventloop.rs:731`, `xnio_io_thread.rs:951`). Row 13's registrar
> is still dead: `register_apps_h2_overrides` still has exactly one reference,
> `let _ = apps_h2::register_apps_h2_overrides;` at `native-builtins/src/lib.rs:9118`.
> §1.1's two same-named helpers both still exist
> (`native-builtins/src/lib.rs:30779`, `native-builtins/src/util_time.rs:133`),
> so the mis-attribution that produced the row is still there to be made again.
> §1.3's `MappedByteBuffer` argument still holds verbatim: both arms of the
> `match` request `mbb_base + MBB_PRIVATE_WIDTH` (`native-io/src/lib.rs:17138`).
>
> **4. Line numbers, re-derived (§1.4's method, applied again).** `native-io/src/lib.rs`:
> `Pattern` 5058 -> **5062**, `ByteBuffer` 7548 -> **7663**, `FileChannel`
> **9811** (as the banner already had it), `java/io/File` 12853 -> **13124**,
> `ArrayList` 13113 -> **13384**, `CharBuffer`/`alloc_typed_buffer` 15087 ->
> **15494**, `MappedByteBuffer` 16734 -> **17139**.
> `native-builtins/src/lib.rs`'s `alloc_time_synthetic` 30418 -> **30779**.
> Unmoved: `async_socket.rs:1980`, `nio_native.rs:1410`, `socket_channel.rs:637`,
> `zip_real_jar.rs:650`, `apps_h2.rs:62`, `util_time.rs:140`,
> `vertx_eventloop.rs`/`xnio_io_thread.rs` (now the helper call sites above).
>
> These are against the **committed** tree at `768ac2de0`, and the distinction
> matters this time: a concurrent lane holds uncommitted edits to
> `native-io/src/lib.rs` around lines 1858-2324 (`native_fd_close0` /
> `native_fos_close`) which add ~72 net lines, so every `native-io/src/lib.rs`
> row above moves down by about that much the moment they land. A first read of
> this file during that lane's window returned exactly those +72 numbers. This is
> the same trap §1.4 records, arriving from a new direction: **on a tree nine
> lanes are editing, a line number is only meaningful with the tree state it was
> taken against.**
>
> **5. Per-item disposition, since §2's table mixes three verdicts.** *Fixed:*
> rows 1 and 2 (the two `java/lang/Thread` carrier mirrors), and fixed a second
> time by `395e5790c`. *Never real:* row 4 (`MappedByteBuffer`, §1.3), row 12
> (`FileChannel`, banner item 2), and the `native-builtins/src/lib.rs`
> `alloc_time_synthetic` row (§1.1) — three rows the census produced by reading
> the wrong arm or the wrong helper. *Dead code, so not live but not fixed
> either:* rows 5 (`Iocp`), 13 (`Thread$State` via `apps_h2`) and 15
> (`util_time`, synthetic-only registrar). *Still live, latent behind an
> `Err(_)` arm:* rows 3, 6, 7, 8, 9, 10, 11, 14 and `socket_channel.rs:637` —
> nine, unchanged, and §2.1's argument for leaving them is unchanged with them.
>
> **6. One thing to know before anyone deletes row 13.** `apps_h2`'s dead
> `thread_state_runnable` fabricates a `Thread$State` by writing the string
> `"RUNNABLE"` into slot 0 and `Int(1)` into slot 1 of a fresh object. That is
> the *fabricated enum constant* shape — non-null, correctly named, and not
> identical to `Thread.State.RUNNABLE`, so `==` against the real constant and any
> `switch` over it would fail while every metadata query answered right. It is
> **not a live defect** (nothing calls the registrar), but it is one more reason
> the deletion §7.4 asks for is the right disposition rather than a re-enable.

---

## 1. The width re-verification, and three disagreements with the 16

**Every *declared* width in W7-73 §3.1 reproduces exactly.** `java/lang/Thread`
19, `Pattern` 20, `MappedByteBuffer` 13, `Iocp` 14, `ZipEntry` 14,
`ServiceLoader` 10, `DatagramChannel` 10, `ByteBuffer` 11, `CharBuffer` 9,
`File` 4, `FileChannel` 4, `Thread$State` 3, `ArrayList` 3, `ZoneOffset` 3. No
arithmetic disagreement anywhere. The population itself also reproduces: an
independent replication of the ratchet's own scanner — `strip_comments`,
`match_brace`, `cfg_test_spans`, `paren_args`, `split_top_level` reimplemented
line-for-line rather than approximated — returns **30 sites**, the same 30, and
returns **28** after this lane's two repairs.

The disagreements are about **which caller a row belongs to**, and there are
three. All three are the same species W7-73 §2.1 names and W7-68 §2 was caught
by: reading one line of a `match` as if it were the arm that runs, or reading
one helper as if it were the other helper of the same name.

### 1.1 `native-builtins/src/lib.rs`'s `alloc_time_synthetic` is not short at all

W7-73 §3.1 lists it as `java/time/ZoneOffset` **2 against 3**. That helper never
reaches `java/time/ZoneOffset`. It has exactly four callers, all in the same
file:

| caller | class | requested | declared |
|---|---|---:|---:|
| `alloc_local_date` | `java/time/LocalDate` | `LD_NUM_FIELDS` = 3 | 3 |
| `alloc_local_time` | `java/time/LocalTime` | `LT_NUM_FIELDS` = 4 | 4 |
| `alloc_instant` | `java/time/Instant` | `INST_NUM_FIELDS` = 2 | 2 |
| `alloc_duration` | `java/time/Duration` | `DUR_NUM_FIELDS` = 2 | 2 |

**Four for four exact.** The row belongs in §3.2's "not short" table, not §3.1's.

What happened is worth naming because it is the third instance in this area:
`alloc_time_synthetic` is declared **twice in one crate**, once in
`native-builtins/src/lib.rs` and once in `native-builtins/src/util_time.rs`,
with the same signature and the same three-line body. The census attributed the
`util_time.rs` helper's worst caller to both. A helper qualifying "on its worst
caller" is the right convention — it is *which helper* that has to be
established by call graph and not by name.

**The short population is 15, not 16.**

### 1.2 `util_time.rs:140`'s worst caller is `ZoneRules` 1 against 7, not `ZoneOffset` 2 against 3

The same helper in the other file, enumerated properly. Fifteen `(class, count)`
pairs reach it:

| class | requested | declared | |
|---|---:|---:|---|
| `java/time/zone/ZoneRules` | `ZR_NUM_FIELDS` = 1 | 7 | **short by 6** |
| `java/time/format/DateTimeFormatter` | `DTF_NUM_FIELDS` = 2 | 7 | **short by 5** |
| `java/time/DayOfWeek` | 1 | 3 | short by 2 |
| `java/time/Month` | 1 | 3 | short by 2 |
| `java/time/ZoneOffset` | `ZO_NUM_FIELDS` = **1** | 3 | short by 2 |
| `java/time/LocalDateTime` | 7 | 2 | over |
| `java/time/Clock` | 2 | 0 | over (abstract) |
| `java/time/ZoneId` | 1 | 0 | over (abstract) |
| `LocalDate` 3/3, `LocalTime` 4/4, `Instant` 2/2, `Duration` 2/2, `Period` 3/3, `Year` 1/1, `ZonedDateTime` 3/3 | | | exact |

So the census understates its own row by five slots and quotes a request
(`2`) that no caller of this helper makes — `ZO_NUM_FIELDS` is 1. The **verdict**
is unchanged and W7-73 already flagged it: `register_time_natives` /
`register_time_extras_natives` are reachable only through
`register_synthetic_overrides`, which `vm_init.rs` gates on
`config.use_synthetic_jdk`, so the whole helper is **dead in Compatible mode**
(W7-68 §3.8, confirmed independently there by rooting at the registrar rather
than at the native).

### 1.3 `alloc_mapped_byte_buffer`'s fallback cannot request 2 against a class declaring 13

W7-73 §3.1 lists `java/nio/MappedByteBuffer` **2 against 13, short by 11**. The
site reads

```rust
let mbb_base = cratonvm_native_api::appended_slots::base_for_class(ctx, MBB_CLASS);
let obj = match ctx.ensure_class_initialized(MBB_CLASS) {
    Ok(cid) => ctx.alloc_object(cid, mbb_base + MBB_PRIVATE_WIDTH),
    Err(_)  => ctx.alloc_object(ClassId::new(0), mbb_base + MBB_PRIVATE_WIDTH),
};
```

The `2` is `MBB_PRIVATE_WIDTH`; the request is `mbb_base + 2`. And `base_for_class`
resolves the class through **the same** `ensure_class_initialized` this `match`
scrutinises, returning 0 when it fails and 0 when the class is a stub. So the two
halves of the census row cannot both hold in one execution:

* on any image where `MappedByteBuffer` really declares 13, `mbb_base` is 13 and
  the request is **15** — an `over` row, which is exactly the appended-slot
  signature W7-68 §5 warned the next census would misfile;
* on any image where the request is **2**, `mbb_base` is 0, which means the class
  did not resolve — and then there is no "declared 13" to be short against.

The row as stated is structurally unreachable. This is not a defect in the site;
it is W7-68's repair working, observed from the census's blind side.

With §1.1, that takes the short population from **16 to 14**. The table in §2
still carries all sixteen rows, because a row corrected out of the population is
more useful than a row silently dropped from it.

### 1.4 Line numbers

Every line number in W7-73 §3.1 outside `native-io/src/lib.rs` reproduces
exactly (`vertx_eventloop.rs:713`, `xnio_io_thread.rs:939`, `apps_h2.rs:62`,
`service_loader.rs:90`, `zip_real_jar.rs:650`, `async_socket.rs:1980`,
`nio_native.rs:1410`, `util_time.rs:140`). All seven `native-io/src/lib.rs` rows
are stale by +8 to +131 (`Pattern` 5050→5058, `ByteBuffer` 7474→7548,
`FileChannel` 9439→9570, `File` 12722→12853, `ArrayList` 12982→13113,
`CharBuffer` 14956→15087, `MappedByteBuffer` 16603→16734), as is
`native-builtins/src/lib.rs` 30175→30418. No verdict changes; recorded because
these tables are a work queue and the next reader will `sed -n` them.

---

## 2. The classification: what each of the sixteen rows is actually used for

The question W7-73 §8 left open — *"whether any of the 30 fallback arms is ever
taken"* — cannot be answered from source. But a **weaker and sufficient**
question can be, and answering it collapses the table:

> When the `Err(_)` arm runs, is there a class declaring more fields for the
> object to be short *against*?

`ensure_class_initialized` fails in two ways. If the class **did not resolve**,
there is no declared width, the object is exactly as wide as the caller asked,
and "short by 8" is a comparison against a class this image does not have. If
the class resolved and its **`<clinit>` threw**, the class *is* registered at its
real width and the fallback object really is short — but that is an image in
which `java.util.zip.ZipEntry`'s static initialiser failed, which is not a state
any of these natives is the first casualty of.

So: **an `Err(_) => ClassId::new(0)` arm is latent by construction.** The live
arm beside it is the one that runs on a real image, and W7-68 §2 already
established that the live arms of `ZipEntry`, `ServiceLoader` and
`ConcurrentHashMap` are written correctly (`real.max(N)`, writes by name).

The two `Thread` sites had no such arm. That is the whole difference, and it is
the reason they are the two this lane repaired.

| # | site | class | req | decl | arm | reaches real JDK bytecode? |
|---|---|---|---:|---:|---|---|
| 1 | `native-builtins/src/vertx_eventloop.rs:713` | `java/lang/Thread` | 5 | **19** | **unconditional** | **YES — REPAIRED** (§3) |
| 2 | `native-builtins/src/xnio_io_thread.rs:939` | `java/lang/Thread` | 5 | **19** | **unconditional** | **YES — REPAIRED** (§3) |
| 3 | `native-io/src/lib.rs:5058` | `Pattern` | 2 | 20 | `Err(_)`, and below a real `Pattern.compile` invoke | latent — a real image returns a genuinely compiled `Pattern` (W7-68 §3.6) |
| 4 | `native-io/src/lib.rs:16734` | `MappedByteBuffer` | `base+2` | 13 | `Err(_)` | **not short** — §1.3 |
| 5 | `native-io/src/async_socket.rs:1980` (`alloc_obj`) | `sun/nio/ch/Iocp` | 1 | 14 | `Err(_)` | **dead** — all four `Iocp` registrations name methods (`open`/`drain`/`poll`) the real class does not declare, so no verifying call site can reach them (W7-68 §3.7, the `method-nowhere` shape) |
| 6 | `native-io/src/zip_real_jar.rs:650` | `ZipEntry` | 6 | 14 | `Err(_)`; live arm is `real.max(6)` + ten `set_field_by_name` | latent |
| 7 | `native-builtins/src/service_loader.rs:90` | `ServiceLoader` | 2 | 10 | `Err(_)`; live arm is `real.max(2)` | latent |
| 8 | `native-io/src/nio_native.rs:1410` (`alloc_t16`) | `DatagramChannel` | 5 | 10 | `Err(_)` | latent **and vacuous** — the slot map is empty; every field moved to identity-keyed side tables (W7-68 §3.4) |
| 9 | `native-io/src/lib.rs:7548` | `ByteBuffer` | 5 | 11 | `Err(_)` | latent — the live path double-writes by index *and* by name in an order that ends correct on both layouts (W7-68 §3.3) |
| 10 | `native-io/src/lib.rs:15087` (`alloc_typed_buffer`) | `CharBuffer` &c. | 5 | 9 | `Err(_)`; live arm is `BB_NUM_FIELDS.max(real)` | latent |
| 11 | `native-io/src/lib.rs:12853` | `java/io/File` | 1 | 4 | `Err(_)` | latent — slot 0 *is* `path`, and `java/io/File` is fully overlaid, so nothing reads `prefixLength` (W7-68 §3.6, where a repair was written and reverted) |
| 12 | ~~`native-io/src/lib.rs:9570`~~ (now `:9811`) | `FileChannel` | `base+2` | 4 | `Err(_)` | **DEAD ROW — not short.** W7-72 §2 moved the map onto the appended-slot idiom across both crates; the width is `synthetic_file_channel::alloc_slots`, so §1.3's argument applies verbatim. The stated blocker was also inverted — see the banner |
| 13 | `native-builtins/src/apps_h2.rs:62` | `Thread$State` | 2 | 3 | `Err(_)` | **dead** — `register_apps_h2_overrides` has no call site; the only reference in the tree is `let _ = apps_h2::register_apps_h2_overrides;` in `lib.rs`, and the module's own header records why it was switched off |
| 14 | `native-io/src/lib.rs:13113` | `ArrayList` | 2 | 3 | `Err(_)` | latent — and the collections overlay is an architecture, not a defect (W7-68 §3.5) |
| 15 | `native-builtins/src/util_time.rs:140` (`alloc_time_synthetic`) | `ZoneRules` (worst) | 1 | 7 | `Err(_)` | **dead in Compatible** — synthetic-only registrar (§1.2) |
| — | `native-builtins/src/lib.rs:30418` (`alloc_time_synthetic`) | `LocalDate` &c. | = | = | `Err(_)` | **not short** — §1.1 |

**Tally: 2 reach real JDK bytecode and are repaired. 3 are dead (`Iocp`,
`Thread$State`, `util_time`). 9 are latent behind an `Err(_)` arm whose live
sibling is correct. 1 is not short (`MappedByteBuffer`), and 1 was never in the
short population at all (`lib.rs`'s `alloc_time_synthetic`).**

### 2.1 Why the nine latent ones were left alone, in one paragraph

The remedies the ratchet's own failure message names are *"propagate the
resolution failure (`MethodCallFailed`)"* or *"allocate against a class you
actually resolved"*. Neither is available on these arms: the arm exists
*because* the class did not resolve, so there is nothing to allocate against,
and propagating turns a degraded-but-working synthetic path into a thrown
exception on an image where the whole point of the arm is that the class is
absent. Widening the request would not help either, because the object's problem
on that arm is **identity**, not width: `AnonymousObject$6` is not a `ZipEntry`
at six slots and is not a `ZipEntry` at fourteen. A lane that "fixed" these into
`real.max(N)` would have bought a bigger wrong object and a smaller census.

---

## 3. The repair: `java/lang/Thread` 5 against 19, twice

### 3.1 What was actually wrong — two defects, and width is the smaller one

Both sites read, with no attempt to resolve the class:

```rust
let mirror = ctx.alloc_object(cratonvm_types::ClassId::new(0), 5);
let name_obj = ctx.create_string(&name);
ctx.set_field(mirror, 0, Value::Object(Some(name_obj)));
ctx.set_field(mirror, 2, Value::Long(vm_tid as i64));
let attached = ctx.set_native_thread_java_obj(vm_tid, mirror);
```

`alloc_object` substitutes `cratonvm/synthetic/AnonymousObject$5` for the
sentinel. So on a real image the object published to the thread registry — the
object `Thread.currentThread()` returns on every Vert.x/Netty event-loop carrier
and every XNIO I/O thread — is:

**Short.** Real `java.lang.Thread` declares 19 instance fields on JDK 25.0.3.9:

```text
 0 eetop        1 tid          2 name          3 interrupted
 4 contextClassLoader          5 holder        6 threadLocals
 7 inheritableThreadLocals     8 scopedValueBindings          9 interruptLock
10 parkBlocker  11 nioBlocker  12 cont        13 uncaughtExceptionHandler
14 threadLocalRandomSeed      15 threadLocalRandomProbe
16 threadLocalRandomSecondarySeed            17 container
18 headStackableScopes
```

Slots 5..18 are off the end of a five-slot object, and the private map is wrong
*inside* the five as well — slot 0 is `eetop` (a `long`), not `name`, and slot 2
is `name` (a `String` reference), not `tid`. The native wrote a `String` into
`eetop` and a `Long` into `name`.

**And not a `Thread` at all.** `AnonymousObject$5` is assignable to nothing, so
`getName`/`threadId`/`getThreadGroup`/`getState` — **none of which is a
registered native in Compatible mode** (`register_synthetic_overrides` is where
`getId` lives, and `vm_init.rs` gates it on `use_synthetic_jdk`) — cannot even
dispatch. `read_java_thread_tid` (`vm/src/vm/vm_exec.rs`) resolves `"tid"` **by
name** against the receiver's own class, finds nothing, and returns `None`, so
`set_java_thread_obj_with_tid` recorded java-tid `0` for these carriers and the
registry's tid index was never populated for them.

**Worst of all, it pre-empted the correct path.**
`NativeContextImpl::current_thread_object` opens with

```rust
if let Some(obj) = self.thread.java_thread_obj { … return obj; }
```

and everything below that line is a fully developed builder for a **real,
full-width** mirror: it resolves `java/lang/Thread`, allocates
`num_total_fields`, runs `init_primitive_fields`, writes `name`/`tid`/`priority`
by *resolved* slot, builds a `Thread$FieldHolder` through
`build_thread_field_holder`, links `group`, and seeds `contextClassLoader`. Every
carrier that pre-registered a five-slot mirror was taking the early return past
all of it. The defect is not only that the object is short — it is that a
correct object was already available and was being displaced.

### 3.2 The fix, and why it is not `n.max(real)`

`real.max(n)` — the obvious repair, and the one W7-73 §3.3 anticipated — is
**not** the right one here, because it fixes the smaller of the two defects.
Nineteen slots of `AnonymousObject$19` is still not a `java.lang.Thread`.

Both sites now call one helper,
`native-builtins/src/lib.rs::alloc_carrier_thread_mirror`, which follows the two
landed in-tree precedents for building a `Thread` from a native —
`net_phase_e::re10_spawn_dispatcher` (the HTTP server dispatcher) and
`jdk25_concurrency::sts_fork` (the `StructuredTaskScope` worker):

1. **`try_alloc_concurrent_synthetic(ctx, "java/lang/Thread", 5)`.** The
   fabrication funnel resolves the class, reports to the layout-alias census,
   widens the request to `class_num_total_fields`, and allocates against the
   **resolved** class id. On a real image that is 19 slots of a genuine
   `java.lang.Thread`; on a synthetic image it is the 5-slot synthetic class; and
   under `--jdk-only` its `refused_class` arm refuses rather than fabricating.
2. **Branch on `ctx.object_num_fields(mirror)`**, through the shared
   `is_synthetic_thread_layout` cutoff (`<= 8`) that the twelve `Thread.<init>`
   natives already use. This is the *width* test rather than
   `has_real_jdk_thread_layout`'s field-name test, and deliberately: the
   field-name test reads `tid` and answers `Long(_)`, which a freshly allocated
   mirror does not yet hold. The width test is exact here for the one reason it
   is not exact in general — **we allocated this object, and the funnel set its
   width from the class's own answer**.
3. **Real layout: drive `Thread.<init>(ThreadGroup, Runnable, String)` through
   `ctx.invoke`**, with a null `Runnable` (the carrier's body is Rust; a non-null
   target would make `Thread.run()` execute it on whoever called `start()`), then
   `setDaemon(Z)V` when the carrier is a daemon. That native runs
   `populate_real_thread_holder`, which allocates and links `Thread$FieldHolder`
   — without which `getPriority`/`isDaemon`/`getThreadGroup`/`getState` all NPE
   on `this.holder`. It also assigns `tid` from `next_java_thread_tid()`, the
   same authority Java-constructed threads use, so `read_java_thread_tid` now
   succeeds and `set_native_thread_java_obj` populates the registry's java-tid
   index for these carriers for the first time.
4. **Synthetic layout: byte-for-byte what both sites did before** — `name` into
   slot 0, the VM `ThreadId` as a `Long` into slot 2, which is the convention
   `resolve_thread_id_from_thread_obj`'s legacy fallback reads.
5. **`None` when the class will not resolve at all.** The caller then registers
   no mirror, and `current_thread_object` builds a correct one lazily. That is a
   better outcome than the old code's, per §3.1.

**Why `ctx.invoke` and not a direct call to `populate_real_thread_holder`.**
`register()` is last-write-wins and `java/lang/Thread`'s `<init>()V` is
registered in **two** registrars — `register_essential_natives_with_shims`
(Compatible) and `register_synthetic_overrides` (synthetic-only). Calling the
Rust function directly would hard-wire one of them; invoking by name gets
whichever actually won, in whichever mode. This is the trap the task brief flags
and the one six lanes hit today, met head-on rather than reasoned around.

**Why no accessor was added.** W7-68 §5 records a `&dyn`-taking
`appended_slot_base_for_class` sibling that was written and deleted because an
accessor resolving a class differently from its allocator recreates
two-layouts-on-one-class from the other side. The same rule applies here and is
the reason this lane added no `thread_mirror_name(ctx, obj)` helper: the
consumers of these mirrors are `Thread.currentThread()` and real JDK bytecode,
and they must keep resolving fields the way the JDK does.

### 3.3 What moved besides the two call sites

* `is_synthetic_thread_layout` **hoisted to module scope** in
  `native-builtins/src/lib.rs`. It was nested inside
  `register_essential_natives_with_shims` — the same mistake the block comment
  immediately above `has_real_jdk_thread_layout` already records for its two
  neighbours — and that is why `jdk25_concurrency::sts_fork` had open-coded
  `worker_fields <= 8` beside a comment naming the function it could not reach.
  `sts_fork` now calls it. One declaration, same value, no behaviour change.
* The synthetic map (`SLOTS`/`NAME_SLOT`/`TID_SLOT`) moved from
  `vertx_eventloop.rs` to `crate::SYNTHETIC_THREAD_MIRROR_*`, beside the one
  allocator that uses it. `xnio_io_thread.rs` had been carrying an open-coded
  copy of the same three numbers. This is the drift that let the width be handed
  to `alloc_object(ClassId::new(0), …)` on every image in the first place.

### 3.4 Compatible mode

**Touched, and it is a genuine bug fix.** In Compatible mode the object returned
by `Thread.currentThread()` on a Vert.x or XNIO carrier goes from a five-slot
`cratonvm/synthetic/AnonymousObject$5` to a real, constructed
`java.lang.Thread`. Compatible mode is contractually frozen except for genuine
bug fixes; a `Thread.currentThread()` that is not a `Thread` is not a contract
worth freezing, and the `--jdk-only` arm is *stricter* than before (the funnel's
`refused_class` refuses to fabricate, where the old code fabricated
unconditionally).

The synthetic-JDK arm is unchanged: the funnel's widen is `max`, a synthetic
`java/lang/Thread` declares 5 (or 6 with `contextClassLoader`), the layout test
routes to the same two `set_field` calls at the same two indices.
`MockNativeContext` reports `class_num_total_fields == 0`, so it also takes the
synthetic arm and the four T19_K4 tests in `vertx_eventloop.rs` and the four in
`xnio_io_thread.rs` exercise exactly the code they exercised before. **No test
was weakened and no detector was made quieter.**

---

## 4. Proving the RED

`probes/ShortThreadMirrorProbe.java`, following
`probes/SlotIndexRecensusProbe.java` and `probes/UnderAllocationProbe.java`:
every read goes through a **real JDK accessor** — a method whose body is JDK
bytecode reading a JDK field by the JDK's own index — never through the native
that wrote it. **HotSpot 25.0.3.9: 108 checks, 0 failures.**

The battery is six accessors, and the choice of six is the whole point:

| accessor | reads | why it is in the battery |
|---|---|---|
| `Thread.class.isInstance` / `isAssignableFrom` | the header | identity, which width cannot express |
| `getName()` | `Thread.name`, slot 2 | the field the native thought was slot 0 |
| `threadId()` | `Thread.tid`, slot 1 | the field the native thought was slot 2 |
| `getPriority()` | `holder.priority` | **slot 5 — off the end of the object** |
| `isDaemon()` | `holder.daemon` | slot 5 |
| `getThreadGroup()` | `holder.group` | slot 5 |
| `getState()` | `holder.threadStatus` | slot 5 |

The last four are deliberately **fields the old native never wrote**. This
campaign's recurring vacuous shape is a probe that reads back only what the
native stored, which passes against an object short in exactly the fields nobody
reads yet; four of these seven reads cannot be satisfied by any object narrower
than six slots, whatever it stored.

### 4.1 The red is measured, not asserted

Discriminating power was checked **on HotSpot itself**, against the closest
thing HotSpot can produce to the pre-repair mirror: a `java.lang.Thread` from
`Unsafe.allocateInstance` — right class, full 19-slot width, no constructor run,
so `holder` is null and `name`/`tid` are at their defaults.

```text
  isInstanceOfThread = true
  getName    = null
  threadId   = 0
  getPriority     THREW NullPointerException: Cannot read field "priority" because "this.holder" is null
  isDaemon        THREW NullPointerException: Cannot read field "daemon" because "this.holder" is null
  getThreadGroup  THREW NullPointerException: Cannot read field "threadStatus" because "this.holder" is null
  getState        THREW NullPointerException: Cannot read field "threadStatus" because "this.holder" is null
```

Four throw; the other two return values the probe asserts against exactly
(`getName != null`, `threadId > 0`). **Six of six discriminate** — and against an
object *better* than the one removed, since that one was a real `Thread` of full
width and merely unpopulated. Whatever the five-slot `AnonymousObject$5` does, it
cannot do better than this, and this is already red.

`batteryCatching` converts a throw into a counted FAIL rather than letting it
abort the walk, so a run over N carriers reports how many are bad instead of
stopping at the first.

### 4.2 What the probe cannot do, stated rather than papered over

Section 2 walks **every live thread** via `Thread.getAllStackTraces()` and
`ThreadGroup.enumerate(Thread[])` — the two real JDK enumerations CratonVM
answers out of `ThreadRegistry::alive_thread_objects()`, which is precisely where
`set_native_thread_java_obj` publishes the carrier mirrors. `enumerate` is the
sharper of the two because it has to **store** each thread into a `Thread[]`,
which a non-`Thread` cannot survive.

But a plain `java Probe` run has no carriers in that set. Section 3 drives
`VertxImpl.init(4)` reflectively to create them, and **prints `SKIP` rather than
passing when `io.vertx.core.impl.VertxImpl` is not on the classpath** — a run
that never ran is not a green, and this probe's carrier coverage is real only on
a workload that has the Vert.x/Undertow jars (Quarkus, Keycloak, WildFly). Said
plainly so the next reader does not quote "108 checks, 0 failures" as evidence
about the carriers.

### 4.3 The instrument's own price, paid and not hidden

W7-68 §5 records that moving a site onto the appended-slot idiom converts an
`under`/`undeclared` row into an `over` row and makes
`CRATONVM_DBG_VALIDATE_NEW` call the object `BAD`. **This repair does not use the
appended-slot idiom** and does not incur that: it asks for the declared width and
carries no private slots above it, so the two `Thread` rows leave the census
entirely rather than moving to `over`. If a later lane needs private state on a
carrier mirror it will pay that price, and it should say so rather than silence
the instrument.

What the repair *does* change in the census: with `CRATONVM_DBG_LAYOUT_ALIAS=1`,
`class=<unresolved:ClassId(0)>, requested_fields=5, real_fields=0,
direction=undeclared` disappears from these two sites, and on a synthetic image
`class=java/lang/Thread, requested_fields=5, real_fields=0,
direction=undeclared` appears instead — the funnel now reports under the class's
real name because it now knows the name. That is a strictly better row, not a
new finding.

---

> **VERIFIED AGAINST A BINARY 2026-09-02.** The banner says "Nothing here was
> built or run". §5's ratchet has now been run, on a build from this tree, by
> name:
>
> ```text
> cargo test -p cratonvm-native-api --test layout_alias_coverage \
>     the_unresolved_class_fallback_population_only_shrinks
> 1 passed, 0 failed, 12 filtered out
> ```
>
> Run by NAME, not by file: a file that passes says nothing about whether the
> test a record cites still exists under that name. The whole file is green as
> well (13 passed).
>
> **The 28 still holds.** `const BOUND: usize = 28` at
> `native-api/tests/layout_alias_coverage.rs:953`, and both the doc comment and
> the failure message still read "12 of today's 28" / "12 of the 28". So the
> ratchet this record left at 28 has neither shrunk nor been relaxed in the three
> weeks since — which is the one thing a population-only-shrinks ratchet cannot
> tell you by passing, and the reason to read the constant rather than the result
> line.
>
> Unchanged: the twelve latent rows are still classified rather than repaired,
> and this note does not touch that.

## 5. The ratchet

`native-api/tests/layout_alias_coverage.rs::the_unresolved_class_fallback_population_only_shrinks`,
**30 → 28**. Both removed sites are the two `Thread` mirrors; the scanner's own
output confirms it (replicated line-for-line rather than eyeballed, and
returning 30 before the edit and 28 after). The doc comment and the failure
message were updated to match — ~~14~~ **12** of 28 short (the banner has the
arithmetic; 14 was this record's own slip, corrected in the same file on the
later pass of 2026-08-12), with §1.1's correction folded in — and the failure
message now cites the repair as an instance of the remedy it prescribes.
**Nothing else in that file was touched, no bound was raised, and no gate was
relaxed.**

A twelfth link was added to that file on the later pass:
`the_appended_slot_allocators_do_not_regress_to_a_literal_width`. It holds the
property that takes the `FileChannel` and `MappedByteBuffer` rows out of the
short column — that their requested widths are DERIVED from
`appended_slots::base_for_class`, never literal. Neither the ratchet nor the
census can see that regression: the site count does not move, and the width the
detector reports is correct for the object it actually allocated.

---

## 6. Blast radius

The two sites are on the Vert.x/Netty event-loop and XNIO I/O-thread carrier
paths — Quarkus and Keycloak boot through the first, WildFly and Undertow
through the second. What a wrong repair would break, in the order it would
surface:

* **A non-null `Runnable` on the mirror.** `Thread.run()` reads
  `holder.task`; if the carrier's mirror carried one, anything that called
  `start()` or `run()` on the mirror — a thread dump walker, a `ThreadFactory`
  wrapper, `Thread.enumerate` consumers — would execute the event loop's body on
  the wrong thread. The helper passes `Value::Object(None)` and says why.
* **Widening without fixing identity.** `alloc_object(ClassId::new(0), 19)`
  would clear the census row and the ratchet and change nothing that matters:
  `AnonymousObject$19` still cannot dispatch `getName`. A repair that makes the
  instrument quiet without making the object right is the failure mode this
  campaign is named after.
* **Writing `name`/`tid` by index on the real layout.** Slot 0 is `eetop`
  (`long`) and slot 2 is `name` (a reference). Writing a `String` into `eetop`
  and a `Long` into `name` on a *real* `java.lang.Thread` is worse than doing it
  to an anonymous object: `getName()` would then return a `Long`-shaped slot on
  a genuine `Thread`, and `eetop` is read by `Thread.isAlive`'s JDK path. The
  helper writes by index **only** on the synthetic arm, where those indices are
  the class's real layout.
* **Calling `populate_real_thread_holder` directly instead of invoking.** It
  would work today and silently become the loser's behaviour the next time a
  registrar order changes. Last-write-wins is the trap, and `ctx.invoke` is
  immune to it.
* **Not pinning across the string and the invoke.** `create_string` and
  `Thread.<init>` both allocate and both can move the mirror; a bare Rust local
  is not a GC root. This is BUG-03 exactly — the stale-thread-mirror report that
  `populate_real_thread_holder` and `build_thread_field_holder` both cite in
  place — where a stale mirror address left `holder == null` on the live object
  and `Thread.<init>`'s `currentThread().getThreadGroup()` NPE'd. The helper pins and
  re-reads through the pin after every allocating step, mirroring
  `populate_real_thread_holder`'s own pattern.
* **Failing the spawn when the class will not resolve.** Returning an error from
  `spawn_vertx_event_loop_with_ctx` would take out `VertxImpl.init` under
  `--jdk-only` the way `HS_LOOP_CLASS`'s refusal took out `HttpServer.start()`
  (`net_phase_e.rs`'s comment records that transcript). The helper returns
  `None`, the carrier still spawns and still registers, and the mirror is built
  lazily.

The one behaviour change a reader should expect to *see*: carrier threads now
report a real `Thread.threadId()` from the shared `next_java_thread_tid()`
authority instead of the VM `ThreadId`, and the registry resolves them through
`find_thread_id_by_java_tid` rather than the legacy slot-2 pointer walk. Both
indices are populated by `set_java_thread_obj_with_tid` on the same call, so no
lookup loses a door — but a consumer that assumed `mirror.slot(2) == vm_tid` on a
real image was reading `name`, and there is no such consumer.

---

## 7. What this lane could not resolve

1. **Runtime confirmation of anything.** Nothing was built or run under
   CratonVM. Every claim is source-level plus `javap -p` plus a HotSpot
   transcript of `probes/ShortThreadMirrorProbe.java`; the CratonVM column of
   that probe has not been produced, and section 3 needs the Vert.x jars.
2. **Whether any of the 28 remaining `Err(_)` arms is ever taken.** Unanswerable
   from source — §2 argues they are latent by construction, which is a weaker and
   sufficient claim, not the same claim. Run any real suite with
   `CRATONVM_DBG_LAYOUT_ALIAS=1` and grep `direction=undeclared` under
   `class=<unresolved:ClassId(0)>`.
3. **The nine latent rows.** §2.1. Their fix is to stop fabricating an object of
   the wrong class on a resolution failure, which is a policy question about
   `ClassId::new(0)` itself and not a per-site edit.
4. **`sun/nio/ch/Iocp`'s four registrations and
   `register_apps_h2_overrides`.** Both are dead code whose deletion would take
   two more sites off the ratchet. That is a dead-code question, not a layout
   one, and a `method-nowhere` deletion is exactly the change that needs a build
   to be safe (65 live registrations were once deleted on that verdict).
5. **`docs/architecture/natives-over-real-jdk-classes.md` §5.** W7-73 §7 proposed
   a scoped correction and did not apply it. Still not applied, for the same
   reason: a lane that discovered a section from one direction should not become
   its second voice.
