# W7-59 — the layout-alias detector, moved onto the allocation every native reaches

Status: the detector's blind spot is closed and the closure is gated. The
counting, the flag, the dedup key and the output channel moved out of
`native-builtins` into `cratonvm_native_api::layout_alias`; the observation now
sits on `NativeContextImpl::alloc_object`, which is the terminal every native
object allocation in every native crate passes through. Five gates in
`native-api/tests/layout_alias_coverage.rs` keep it there. **Nothing here was
built or run** — this lane writes code and docs only; every runtime claim below
says which of its premises is source-level and which is `javap`.

Branch `fix/layout-alias-detector-coverage-20260812`. Field counts are
`javap -p` against the JDK 25.0.3.9 image on this Windows host, counted
transitively over the superclass chain with `static` excluded — the same oracle
and convention as W4-4-slot-index-species-sweep.md and
W7-49-slot-index-recensus.md. The oracle was checked against W7-49's own table
before it was trusted: `AsynchronousSocketChannel` 1, `HashSet` 1,
`ConcurrentHashMap` 12 (W7-49's correction of W4-4's 10 — it reproduces),
`SSLEngine` 2, `DatagramChannel` 10, `Locale` 4, `Properties` 10. All seven
agree.

## 1. What the detector could not see, and why that shape matters

W7-49 §2(c) found the hole: `report_layout_alias` fired from exactly one caller,
`try_alloc_concurrent_synthetic`. That funnel has ~1,800 call sites, which is
enough to make it *look* like the allocation. It is not. The native crates also
call `NativeContext::alloc_object` / `try_alloc_object_gc_safe` directly, and
none of those reached the detector — so `CRATONVM_DBG_LAYOUT_ALIAS=1` printed a
census that was silently partial.

The concrete cost, and it is the campaign's signature failure in miniature:
`native-io/src/async_socket.rs` allocates
`java/nio/channels/AsynchronousSocketChannel` four slots wide against a class
declaring one, and `native-io` is the live owner of that class. **The widest live
over-allocation in the workspace was invisible to the instrument built to find
over-allocations**, and W4-4's prescribed follow-up — intersect the flag's output
with `CRATONVM_DBG_VALIDATE_NEW=1` and the `cratonvm::gc::guard` out-of-bounds
reads — would have read that absence as "not this species". An intersection is
only as complete as its smallest list.

## 2. The site count: 513, and only 208 of them matter

W7-49 counted **511** bypassing sites across `native-builtins`, `native-io` and
`native-collections`. This lane counts **513** raw call sites — agreement to
0.4%, which is what two independent readers of the same regex should produce.

But the raw figure is not the actionable one, and this is the correction this
section exists for:

| | native-builtins | native-io | native-collections | total |
|---|---:|---:|---:|---:|
| production | 136 | 56 | 16 | **208** |
| `#[cfg(test)]` | 245 | 52 | 8 | **305** |
| raw | 381 | 108 | 24 | **513** |

**305 of the 513 sit inside `#[cfg(test)]` modules**, where the allocator is
`MockNativeContext` and no VM, no heap and no loaded class is involved. They are
outside this census by construction, not by exemption. `native-builtins-crypto`,
`native-builtins-security` and `native-awt` hold zero direct sites.

Splitting that correctly is not free. The first draft of the gate's scanner
matched braces blind; `graalvm_compat.rs`'s `#[cfg(test)] mod tests` contains a
string literal with an unbalanced `{`, the span never closed, and seven test-only
sites were counted as production — the number was wrong in the direction that
flatters the finding. The shipped scanner skips string literals and treats an
unclosed span as running to end of file.

### 2.1 The bypassing sites are not 208 hand-written allocations — they are 15 private funnels

Scanning for helpers that take a class **name** and a slot **count** and end in
`ctx.alloc_object` turns up **sixteen** of them, of which `try_alloc_concurrent_synthetic`
is one:

`native-builtins`: `agroal_pool.rs`, `infinispan_local.rs`, `ironjacamar_pool.rs`
and `wildfly_datasources_tx.rs` each hold a private `alloc_object_for`;
`lang_invoke.rs::alloc_mh_carrier`; `util_time.rs::alloc_time_synthetic` plus a
second copy in `lib.rs`. `native-io`: `async_socket.rs::alloc_obj`,
`socket_channel.rs::alloc_obj`, `nio_native.rs::alloc_t16`,
`lib.rs::try_alloc_synthetic`, `lib.rs::alloc_typed_buffer`.
`native-collections`: `lib.rs::try_alloc_synthetic`.

That is the same primitive re-implemented fifteen times, which is exactly why
instrumenting *call sites* was never going to work and why the observation
belongs at the bottom. It is also the reason a per-crate detector would have been
the wrong answer: fifteen private funnels plus two detectors is a drift machine.

## 3. What changed

**One implementation, in `native-api/src/layout_alias.rs`.** `classify`,
`enabled`, `observe`, the dedup set and both `tracing::warn!` arms live there,
once. `native-builtins`' `report_layout_alias` is now a four-line forwarder; its
body was deleted rather than left beside the shared one. The channel, the flag
and the field names (`class`, `requested_fields`, `real_fields`, `direction`,
`site`) are unchanged, so an existing consumer's filter still works.

**Two observation points, and the second is not redundant.**

1. `NativeContextImpl::alloc_object` in `vm/src/vm/vm_exec.rs` — the terminal.
   It *already* resolves the loaded class's `num_total_fields` (it has clamped
   the requested count up to it since the Kafka `HashSet` failure) and keeps it
   in a per-thread cache, so the comparison the census needs is two integers
   already in hand. Nothing is looked up for the detector's sake.
2. `try_alloc_concurrent_synthetic` — kept, because that funnel **clamps before
   it allocates**: `n = requested.max(real)`. An under-request arrives at (1) as
   `n == real` and there is nothing left to see. Removing this call would make
   the detector quieter in the direction it has reported since it was written.
   Louder is always allowed; quieter never is.

Two callers into one implementation is not two detectors. The rule this project
paid for — *one sick collector ⇒ diff the two impls of the same primitive* — is
about two implementations drifting; there is one here.

**The dedup key gained the site.** It was `(class, requested, caller)`; it is now
`(class, requested, declared, site)`. Two natives making the same mistake on the
same class are two findings, and the old key could collapse them.

### 3.1 Cost, and why Compatible mode is untouched

`enabled()` is a `OnceLock<bool>` that every caller checks **first**. With the
flag off — the default, and every Compatible-mode run that has not opted in —
the added cost on the allocation path is one relaxed load and one predictable
branch. No class name is resolved, no frame is formatted, no lock is taken. The
allocation itself is unchanged in both modes with the flag on or off: `slots` is
computed from the same two integers as before and the inserted block has no
`else`. The detector can only print.

`#[track_caller]` on `NativeContext::alloc_object` was considered and rejected.
It would add a hidden argument to every native allocation plus a reify shim on
the `dyn` vtable, paid in every run, to serve a flag that is off in almost all of
them — on a method a previous lane rebuilt with a thread-local field-count cache
and a TLAB fast path specifically to shave a `RwLock`. The base observation
instead reports the **Java frames**, which are already a `Vec` on the thread and
are formatted only when a row is about to print. They are also the better answer
to the question the census asks: a Rust source location says a site *exists*, a
Java frame says the path *ran*, and "did it run" is the LIVE-versus-dead
distinction that last-write-wins registration makes unanswerable from source
alone. The funnel keeps its exact `#[track_caller]` location, which costs nothing
new because that funnel was already `#[track_caller]`.

**`HEADER_SIZE` is not a factor at any newly-routed site.** Grepped for
hard-coded header constants across `native-builtins`, `native-io` and
`native-collections`: the only occurrence of `heap_alloc_object` in any of them
is a doc comment in `lang_invoke.rs`. Every site addresses fields by slot index,
which the object model resolves relative to the header for the caller. W7-49's
finding still holds for the sites this lane added.

## 4. Does it fire on `AsynchronousSocketChannel`?

Yes, and the claim needs no build, because the widening put the observation where
the two numbers already were.

* `javap -p java.nio.channels.AsynchronousSocketChannel` on JDK 25.0.3.9: one
  instance field, `private final AsynchronousChannelProvider provider`;
  superclass `java.lang.Object`. Transitive count **1**.
* `native-io/src/async_socket.rs` allocates it at `N_FIELDS = 4` from
  `aio_asc_open` (:1857) and `drain_completions` (:385), and
  `native-io/src/nio_native.rs::t16_asc_open` (:1513) does the same through
  `alloc_t16`. `alloc_obj`/`alloc_t16` both end in `ctx.alloc_object(cid, n)`.
* `NativeContextImpl::alloc_object` therefore runs with `num_fields = 4` and
  `real_fields = 1` — the same `real_fields` it uses for the clamp — and
  `classify(4, 1) == Some(Over)`.

So "does it fire" reduces to "is 4 different from 1". The routing half — that
this site reaches that terminal at all — is what
`native-api/tests/layout_alias_coverage.rs` proves mechanically, and the
classification half is asserted in `layout_alias`'s own unit test against exactly
this shape. Neither alone would be worth much: the unit test on its own is a
probe that cannot fail.

`AsynchronousServerSocketChannel` (also 1 declared, also allocated at 4) fires
the same way, and W7-49 did not list it.

## 5. The newly-visible census

"Newly visible" = production sites reached through the fifteen sibling wrappers
or a traceable direct `alloc_object`, i.e. everything except the one funnel the
old detector already watched. Only sites where both the class name and the
requested count are resolvable from source are classified; the rest are
unresolved, not clean.

| | sites | distinct (class, requested, declared) |
|---|---:|---:|
| **newly visible OVER** | **28** | **11** |
| **newly visible UNDER** | **35** | **24** |
| already visible OVER (the old funnel) | 122 | 48 |
| already visible UNDER (the old funnel) | 505 | 180 |

The funnel columns are production-only and so run below W7-49's 159/528, which
counted `#[cfg(test)]` sites too; the distinct-pair columns (48 over) sit right
on W7-49's 46 and W4-4's 48.

### 5.1 Newly visible OVER, split by reachability

**22 LIVE, 1 synthetic-only, 5 dead, 0 indeterminate.**

| class | requested / declared | sites | where |
|---|---|---:|---|
| `java/nio/channels/AsynchronousSocketChannel` | 4 / 1 | 3 | `async_socket.rs:385`, `:1857`; `nio_native.rs:1513` |
| `java/nio/channels/AsynchronousServerSocketChannel` | 4 / 1 | 1 | `async_socket.rs:2824` |
| `java/nio/channels/SocketChannel` | 12 / 10 | 3 | `socket_channel.rs:1298`, `:4325`, `:4464` |
| `java/nio/channels/ServerSocketChannel` | 12 / 10 | 1 | `socket_channel.rs:3893` |
| `java/nio/channels/FileLock` | 6 / 4 | 2 | `native-io/src/lib.rs:16028`, `:16029` |
| `java/util/TreeSet` | 3 / 1 | 9 | `native-collections/src/lib.rs`, nine set-view natives |
| `java/util/concurrent/CompletableFuture` | 4 / 2 | 2 | `native-collections/src/lib.rs:56244`, `:56330` |
| `java/net/InetSocketAddress` | 2 / 1 | 1 | `native-io/src/lib.rs:19752` |
| `java/time/LocalDateTime` | 7 / 2 | 1 | `util_time.rs:2015` — **synthetic-only** |
| `java/nio/channels/SelectionKey` | 4 / 1 | 1 | `native-io/src/lib.rs:20049` — **dead** |
| `java/util/HashSet` | 2 / 1 | 4 | `native-io/src/lib.rs:20172`–`:20235` — **dead** |

### 5.2 Newly visible UNDER, LIVE, widest first

`java/util/regex/Pattern` 2 vs 20 (×2, `native-io/src/lib.rs:5023`);
`ScheduledThreadPoolExecutor` 3 vs 17; `sun/nio/ch/Iocp` 1 vs 14;
`ConcurrentHashMap` 2 vs 12; `java/util/zip/ZipEntry` 6 vs 14;
`DatagramChannel` 3 vs 10 and 5 vs 10; `java/nio/ByteBuffer` 5 vs 11 (×2);
`TreeMap` 3 vs 9; `HashMap` 3 vs 8; `IOException` 2 vs 6; `java/io/File` 1 vs 4
(×2); `LinkedBlockingQueue$Itr` 2 vs 5; `FileChannel` 2 vs 4 (×2);
`ArrayList` 2 vs 3 (×7); `InetAddress$InetAddressHolder` 3 vs 4;
`MappedByteBuffer` 12 vs 13 (×2).

The `java/nio/ByteBuffer` 5-vs-11 row at `native-io/src/lib.rs:7375`
(`alloc_byte_buffer`) is an independent confirmation from the ByteBuffer lane,
which predicted from the other direction that `native-io`'s `alloc_byte_buffer` /
`alloc_typed_buffer` bypass the funnel entirely. It is newly visible here. That
lane's other signal — `servlet.rs::s2_bb_alloc` asking 6 on a class declaring 11
— sits in the funnel column, and reproduces: `s2_bb_alloc_direct` at
`servlet.rs:2880`, `direction=under`, already reported before this change.

### 5.3 How many of W7-49's 78 indeterminate this resolves, and how

W7-49 left **78 of 159** `over` sites unclassified: its call-graph walk followed
named calls, so a registrar reached only through a function pointer was
unresolved. Two changes close most of that.

**Bare-name edges.** A registration is `r.register(cls, name, desc, native_fn)`
— the native's identifier is a bare token in the registrar's body, and a
registrar held in a table is a bare token in that table's body. Taking *every*
identifier that names a known function as an edge follows both. It
over-approximates, which errs toward LIVE — the louder verdict, and the right way
round: a site called LIVE that is dead costs a follow-up lane a look, while a
site called dead that is live is the failure this campaign is about. (Restricting
edges to identifiers of eight-plus characters containing `_`, to stop `new`/
`get`/`read` linking everything to everything, moved no verdict in the
newly-visible population — 22 LIVE / 6 unreached either way.)

**Rooting at what `vm_init` actually calls.** W7-49 rooted LIVE at
`register_essential_natives(_with_shims)`. The real root set is the 44 registrars
the real-JDK arm of `vm_init.rs`'s `if config.use_synthetic_jdk { … } else { … }`
fork calls — including `register_io_natives` and `register_collections_natives`,
which are called in **both** arms and are the entry points for almost everything
in §5.1.

Result on the newly-visible `over` population: **0 of 28 indeterminate.** Every
site is LIVE (22), synthetic-only (1) or unreached (5). Two of those verdicts
were then hand-confirmed rather than taken from the walk, because a walk that
silently drops a root does not fail — it answers a different question,
confidently, and this one did exactly that twice before it was right:

* **The five "unreached" sites are genuinely dead.** They live in
  `native-io/src/lib.rs::register_selector`, whose only reference in the
  workspace is its own definition — the call was removed in Wave 3 / Task C, with
  a comment at `native-io/src/lib.rs:17470` saying the modern implementation in
  `nio_selector.rs` is the source of truth. Dead registrar, dead sites.
* **`java/time/LocalDateTime` 7 vs 2 is synthetic-only.** Its registrars
  (`register_time_natives`, `register_time_extras_natives`, called at
  `native-builtins/src/lib.rs:23024` and `:23621`) sit inside
  `register_synthetic_overrides`. W7-49 called `util_time.rs` reachability
  "genuinely indeterminate"; this settles it in the direction W7-49 guessed.
* And the 22 LIVE were spot-checked at the registrar level rather than trusted:
  `register_async_socket_real` and `register_t16_channel_overrides` are called
  from `native-io/src/lib.rs:5637` and `:6790`, both inside
  `pub fn register_io_natives` (`:5571`); `register_tree_set_natives` from
  `native-collections/src/lib.rs:2409`, inside
  `pub fn register_collections_natives` (`:2360`); and `vm_init.rs` calls both
  top-level registrars in the real-JDK arm at `:2434` and `:2465`.

Two walk bugs are worth recording because both would have produced a confident
wrong census. Brace-matching fn bodies dropped
`register_essential_natives_with_shims` — the root of the LIVE walk — on a brace
imbalance somewhere in 36,000 lines, and everything downstream then reported
dead. And taking `register_*` mentions on `use` lines as roots made
`register_builtins`, the `#[cfg(feature = "synthetic-jdk")]`-gated synthetic
entry point, a LIVE root, after which essentially everything reported LIVE. The
committed reasoning uses text-between-definitions for bodies and call sites only
(`register_x(`) for roots.

## 6. What this detector covers, and what it structurally cannot

Stated explicitly, because a clean report from this instrument will otherwise be
read as coverage of a species it cannot express. This is folded in from the
ByteBuffer lane, which reached it independently.

**Covered: the allocation-width species.** A native asking for a slot count that
disagrees with the loaded class's declared width. That is one integer against
another, in both directions, at every allocation door in the workspace.

**Not covered, and not fixable by widening this instrument: the read-side
wrong-field species.** A native reading or writing slot *k* of an object it did
**not** allocate, where slot *k* on the real layout means something else. Three
independent reasons, each sufficient:

1. It is an **allocation** instrument. A read on a foreign receiver passes no
   allocation at all, so there is no observation point to widen to.
2. Its vocabulary is a **count**, not a slot map. It cannot express "slot 0 is
   `mark`, not `hb`". Both directions are counts, so neither the 2026-08-11
   over-direction repair nor this lane's widening helps.
3. Its documented discriminator — intersect with the `cratonvm::gc::guard`
   out-of-bounds reads — misses too, because on a real `DirectByteBuffer` **slot
   0 exists**. The read is perfectly in bounds. An in-bounds read of the wrong
   field is invisible to both halves of the intersection.

The live example is `bb_state` reading slot 0 of a real `DirectByteBuffer`,
getting `Buffer.mark` = -1, and treating it as the backing array. Real JDK 25
layout, from `lib/src.zip`: `mark(0) position(1) limit(2) capacity(3) address(4)
segment(5) hb(6) offset(7)`.

**A second instrument is genuinely needed and is not built here.** Its shape, so
the next lane does not have to derive it: it must key on the **receiver**, not
the allocation, and compare a native's slot *index* against the *named field* at
that index in the loaded class. The two pieces that already exist are
`NativeContext::declared_fields` (index-ordered field metadata for a `ClassId`)
and the per-native slot maps, which today are `const F_OPEN: usize = 0`-style
constants with no machine-readable link to a field name. The minimum viable form
is a per-triple declaration — "this native reads slot 2 of its receiver expecting
`fd`" — checked once at registration against `declared_fields` for the real
class, which makes it a startup cost rather than a hot-path one and lets it fail
loudly on a real image. Do not stretch the allocation detector to reach it.

## 7. Proving the RED

Five gates in `native-api/tests/layout_alias_coverage.rs`, each its own test so a
break names which link broke. All five were simulated green against this tree
before landing, and two were simulated red against mutated copies.

1. `the_layout_asserting_allocation_surface_is_exactly_two_methods` — the set of
   `NativeContext` methods taking a caller-supplied `num_fields` and returning an
   object is exactly `{alloc_object, try_alloc_object_gc_safe}`. **Fails** when a
   third allocation door is added, which is the exact shape of the hole this lane
   closed. (Verified red by inserting an `alloc_object_raw` declaration.)
2. `try_alloc_object_gc_safe_has_no_override_that_could_route_around_alloc_object`
   — that method is observed only because its trait default is
   `Some(self.alloc_object(..))` and nothing overrides it. Its own doc comment
   says "the real VM implementation overrides this", which is not true today;
   the day it becomes true the override allocates without passing the
   observation. **Fails** naming the new file, and tells the author to carry the
   observation into it.
3. `the_base_allocator_observes` — `NativeContextImpl::alloc_object` still
   contains `layout_alias::enabled()` and `layout_alias::observe(`, **and** runs
   them before `let slots = num_fields.max(real_fields);`. The ordering half
   matters as much as the presence half: after the clamp the two counts always
   agree and the `under` direction silently reports nothing. **Fails** when a
   hot-path cleanup lifts the block out or moves it down. (Verified red by
   deleting the `observe` call.)
4. `no_native_crate_touches_the_heap_directly` — no native crate names
   `try_alloc_object_full`, `alloc_object_shared`, `heap_alloc_object`, `GenHeap`
   or `mem.heap`. This is the link that makes the argument closed rather than
   suggestive: `native-collections` does depend on `cratonvm-gc` (for
   `external_roots::register_external_root_provider`), so the dependency graph
   alone does not settle it. **Fails** naming the file and the spelling.
5. `there_is_exactly_one_detector` — exactly one file both reads
   `CRATONVM_DBG_LAYOUT_ALIAS` and emits a `direction`. **Fails** when someone
   re-inlines a second copy of the machinery.

Plus `census`, which **prints** the population and asserts only that it is
non-zero. Deliberately not a ratchet: a count over a population that changes with
every native added gets re-baselined on sight, and a gate people re-baseline
teaches them to re-baseline the whole file.

No `CRATONVM_*` flag was added; the existing `CRATONVM_DBG_LAYOUT_ALIAS` is
reused. Its home crate moved from `native-builtins` to `native-api`, which is a
column in `docs/config/flag-inventory.md` — regenerated.

## 8. Defects for follow-up lanes, not fixed here

None of these were repaired. This lane's product is the instrument and the
census; a lane that widens an instrument and then gets lost fixing what it finds
delivers neither.

**Known and deliberately left (recorded elsewhere, one of them a trap):**

* `java/nio/channels/AsynchronousSocketChannel` 4 vs 1 — W7-49 §5. `native-io`
  owns the class and wins registration, so seven of `net_channels.rs`'s eight
  triples are dead, but the survivor runs under a slot map whose slots 0 and 1
  mean the *opposite* of the owner's. W7-49 converted it and reverted; a repair
  to dead code that breaks the one live path is worse than none. The detector now
  SEES it, which was the whole ask.
* `javax/net/ssl/SSLEngine` 7 vs 2, 27 receiver accesses at indices 2–6 — W7-49
  §7. Threads a base through three registrars across two files and is a
  last-write-wins question that needs a build.

**Newly visible, LIVE, and new to the record:**

1. `java/util/TreeSet` 3 vs 1 — **nine** sites in `native-collections/src/lib.rs`
   (`native_tm_key_set`, `native_ts_head_set`, `native_ts_tail_set`,
   `native_ts_sub_set` and their `_inclusive` twins, `native_ts_descending_set`,
   `native_cslm_key_set`). Real `TreeSet` declares one field. This is exactly the
   `HashSet` shape W7-49 §6.3 repaired in `wildfly_security.rs`: on the real
   layout the returned set's backing map is null, so every real `Set` method
   throws, and the count in slot 1 sits where nothing can read it back. Biggest
   single cluster in the newly-visible population and the most likely to be a
   live defect rather than a wide-but-unused allocation.
2. `java/util/concurrent/CompletableFuture` 4 vs 2 —
   `native-collections/src/lib.rs:56244` (`cf_make_synthetic`) and `:56330`
   (`cf_make_completed`). The same shape W7-49 §6.2 repaired in
   `http_client.rs::hci_send_async`, in a different crate: `Int` written into
   `stack`, which real `CompletableFuture` bytecode walks as a `Completion`
   chain. The remedy is already written — `aio_completed_future`, which invokes
   the JDK's own `completedFuture(Object)` and writes no index.
3. `java/nio/channels/SocketChannel` 12 vs 10 (×3) and `ServerSocketChannel`
   12 vs 10 — `native-io/src/socket_channel.rs`. Two slots past the declared
   width; check for reads at 10–11 before narrowing.
4. `java/nio/channels/AsynchronousServerSocketChannel` 4 vs 1 —
   `async_socket.rs:2824`. Same shape and same owner as its sibling; W7-49 did
   not have it.
5. `java/nio/channels/FileLock` 6 vs 4 — `native-io/src/lib.rs:16028`–`:16029`.
6. `java/net/InetSocketAddress` 2 vs 1 — `native-io/src/lib.rs:19752`. Another
   instance of a class W7-49 already lists at 3 vs 1 and 2 vs 1 elsewhere; whoever
   takes it should take all of them together.
7. The widest newly-visible UNDER rows, all LIVE: `java/util/regex/Pattern`
   2 vs 20 (`native-io/src/lib.rs:5023`), `ScheduledThreadPoolExecutor` 3 vs 17,
   `sun/nio/ch/Iocp` 1 vs 14, `ConcurrentHashMap` 2 vs 12,
   `java/util/zip/ZipEntry` 6 vs 14. The clamp keeps these in bounds, so they are
   a risk register rather than a bug list — until one of them also appears in the
   guard's out-of-bounds reads.

**Dead code worth deleting, separately from any layout question:**

8. `native-io/src/lib.rs::register_selector` and everything only it registers —
   `native_channel_register`, `native_sel_keys`, `native_sel_selected_keys` and
   their `SelectionKey` 4-vs-1 and `HashSet` 2-vs-1 allocations. The registrar
   has had no caller since Wave 3 / Task C. Five of this census's `over` sites
   are in it, and they are noise in every future run of the flag.

## 9. What this lane could not resolve

1. **Runtime confirmation of anything.** Nothing here was built or run. Every
   claim is source-level plus `javap` against JDK 25.0.3.9, and the census is a
   source-level upper bound on what the flag would print with the classes loaded,
   not a transcript.
2. **`declared == 0` is still unmeasured, not cleared.** Zero means both "class
   not loaded yet" and "genuinely no instance fields", so both directions exclude
   it. Closing that needs a `class_is_loaded` predicate `NativeContext` does not
   have. Inherited from the original detector; the widening does not change it.
3. **The unresolved sites.** 167 of the newly-visible requests pass the class or
   the count through an expression this lane could not read from source. They are
   observed at runtime by the widened detector — that is the point of putting it
   at the bottom — but they are absent from §5's tables.
4. **Last-write-wins is settled only at the registrar level.** A LIVE verdict here
   means "a registrar the real-JDK arm calls installs this native". Whether a
   *later* registrar overwrites that specific triple is per-triple and needs a
   build. `AsynchronousSocketChannel` is the worked example of why that matters.
5. **Whether any class is allocated at BOTH widths in one run.** Still the
   question no instrument answers. The widened detector now at least *lists* both
   widths when both are asked for, which it could not before — a class allocated
   wide by a direct site and narrow by the funnel produces two rows with the same
   class and different `requested_fields`. That is a discriminator this census
   did not have; nobody has run it.
6. **The read-side species — see §6.** A second instrument, specified there, not
   built here.
