# W7-49 — the slot-index species, re-censused against the un-blinded detector

Status: the both-directions repair IS present and correct in source. The census
it enables is still a **partial** census, in one way nobody had measured: the
detector sits on ONE allocation funnel, and **511 direct allocation call sites
in the native crates never reach it** — including the live owner of the widest
LIVE over-allocation this lane found. Four sites repaired, one large one
reported as dead-plus-disagreeing rather than repaired.

Branch `fix/w44-slot-index-sweep-20260812`. Nothing here is built or run. Every
field count is `javap -p` against the JDK 25.0.3.9 image on the Windows host
(`C:\Program Files\Eclipse Adoptium\jdk-25.0.3.9-hotspot`), `javap -version` =
`25.0.3`, counted transitively over the superclass chain with `static` excluded
— the same oracle and the same convention as
`W4-4-slot-index-species-sweep.md`.

> **SOURCE-VERIFICATION BANNER — 2026-08-12, triage pass (A28). §9.3's "what
> remains is the RUN" is now measured, and the answer is that it cannot be done
> on this host without a build. Plus one wrong premise in §6.3/§6.4.**
>
> **1. The evidence for this whole family is unscheduled, and the one artifact
> that looks like it could run it is a trap.** §9.3 ends *"the census is a
> source-level upper bound until somebody executes
> `CRATONVM_DBG_LAYOUT_ALIAS=1` over a workload, which nobody has done for this
> record, W7-66, W7-68, W7-73 or W7-90."* Two things checked:
>
> * **The probes are not scheduled.** All four exist —
>   `probes/SlotIndexRecensusProbe.java`, `probes/OverAllocationWidthProbe.java`,
>   `probes/UnderAllocationProbe.java`, `probes/GuardedSlotMapProbe.java` — and
>   `regression-suite/run.sh` contains **no reference to `probes/` at any
>   `SUITE=` value**. Every "how it fails" section in this family therefore
>   describes an observation nothing will ever make on its own. That is a
>   property of the directory, not of the probes, and it is worth stating once
>   per family rather than once per record.
> * **The only shipping binary on this host predates the instrument.**
>   `C:\craton\cratonvm\target\release\cratonvm.exe` is dated **26 July** —
>   seventeen days before `native-api/src/layout_alias.rs` landed (2026-08-12
>   05:20). Probed for its own strings: `CRATONVM_DBG_LAYOUT_ALIAS` **absent**,
>   `undeclared` **absent**, `unresolved:ClassId` **absent**, `explain-jdk-only`
>   **absent**. (`CRATONVM_DBG_LAYOUT` **is** present — that flag is older, and
>   it is the one that resolves a numeric `ClassId` to a name, which is a
>   different job.) So the census cannot be run here without a build, and — the
>   reason this is written down — **a later lane that finds this binary and runs
>   the flag against it will get an empty transcript that reads exactly like
>   "the census is clean".** It would be measuring a VM older than every repair
>   in W7-66, W7-68, W7-72, W7-73, W7-74 and W7-77. Rebuild first; check
>   `--explain-jdk-only` is accepted before believing any run.
>
> The same argument disposes of the read-side half: `read_alias::observe_read`
> cannot have been exercised either, by the same binary date.
>
> **2. §6.3/§6.4's synthetic arm rests on a width that is not the fabricated
> width.** Both sections say of the `HashSet` repair: *"on a fabricated stub the
> 3-slot shape IS the layout and that arm is unchanged"*. It is not.
> `ClassManager::synthetic_stub_fields` fabricates `java/util/HashSet` — sharing
> an arm with `HashMap`, `EnumMap`, `Hashtable` and `ConcurrentHashMap` — at
> **16** (`classloading/src/class_manager.rs:12310`), under a comment two lines
> above that says *"= 3 fields (buckets, size, capacity)"*. The **conclusion**
> survives untouched: the count is written at slot 1, which is inside 16 as it is
> inside 3, and `alloc_object` clamps a 3-slot request up to 16, so the arm
> really is unchanged and really is safe. The **premise** does not, and it is the
> premise the next reader will reuse. Same finding, same file, same function as
> W7-68's triage banner (nomination N-4) and W7-66's (N-1): three width comments
> in `synthetic_stub_fields` disagree with the arms beneath them, all
> understating, and this family has been reading its fabricated widths out of
> those comments.
>
> §7's last bullet is affected the same way and the same distance:
> `util_time.rs::native_zone_id_get_available`'s third 3-slot `HashSet` is still
> genuinely indeterminate for reachability, which was the reason for leaving it,
> and that reason is untouched by the width.
>
> **Verified true and not to be re-derived:** §8's idiom moved crates as §5 of
> W7-68 describes — `cratonvm_native_api::appended_slots::base_for_class` exists
> with `appended_slot_base_for_class` kept as a forwarder. §2(c)'s closure note
> holds; the ratchet it points at is `BOUND = 28`
> (`native-api/tests/layout_alias_coverage.rs:947`).


> **VERIFIED AGAINST A BINARY 2026-09-03. §9.3's owed run has been done.** That
> section ended: *"the census is a source-level upper bound until somebody
> executes `CRATONVM_DBG_LAYOUT_ALIAS=1` over a workload, which nobody has done
> for this record, W7-66, W7-68, W7-73 or W7-90."* Somebody has now.
>
> **The banner's trap was avoided by checking, not by assuming.** It warns that
> the shipping binary on the Windows host predates `layout_alias.rs` and that a
> later lane running the flag against it "will get an empty transcript that
> reads exactly like *the census is clean*". The binary used here was built from
> this tree on 2026-09-03 and was probed for its own strings first —
> `CRATONVM_DBG_LAYOUT_ALIAS`, `undeclared` and `explain-jdk-only` are all
> **present**. The transcript below is not empty, which is itself the proof the
> instrument is live.
>
> **The live census: 51 species, 216 observations**, over
> `SlotIndexRecensusProbe` and `OverAllocationWidthProbe` — the two probes this
> record names, recovered from a sibling worktree after `3b2901531` deleted
> `probes/` from the checkout. The widest divergences, both directions:
>
> ```text
> class                                       req   real   dir      obs   width
> java/util/Properties                         16     32   under      2    -16
> java/util/HashSet                             1     16   under     15    -15
> jdk/internal/loader/…$PlatformClassLoader     7     21   under      2    -14
> jdk/internal/loader/…$AppClassLoader          7     21   under      2    -14
> java/lang/Thread                              5     19   under      1    -14
> java/net/http/HttpClient                      9      0   undeclared 2     +9
> sun/net/httpserver/HttpServerImpl             6     12   under      1     -6
> java/util/TreeMap                             3      9   under     12     -6
> java/util/HashMap$KeyIterator                10      5   over       8     +5
> sun/nio/ch/EPollSelectorImpl                 23     18   over       1     +5
> <unresolved:ClassId(0)>                       3      0   undeclared 15    +3
>
> over  19 species /  97 observations
> under 19 species /  81 observations
> undeclared (real_fields=0) 13 species / 38 observations
> ```
>
> **Three things this measurement says that the source census could not.**
>
> * **The `under` side carries the wide divergences and the `over` side does
>   not.** Every width past 6 is `under`; the widest `over` is +5. That is
>   direct evidence for W7-66 §1's thesis that `over` is not on its own a defect
>   predicate — and its mirror, that this record's own subject is where the
>   width lives.
> * **13 species allocate against a class the VM declares NO fields for**
>   (`real_fields=0`) — `java/net/http/HttpClient`, `HttpRequest`, `Path`,
>   `FileSystem`, `Stream`. A `+9` against an undeclared layout is a different
>   species from a `+9` against a known one, and it is invisible to a `javap`
>   oracle because the disagreement is with our own table, not with the JDK's.
> * **15 observations name no class at all** — `<unresolved:ClassId(0)>`, the
>   exact string the banner tested the old binary for. The site is live, the
>   class id is 0, and the census cannot say what was allocated.
>
> **What this does NOT verify, and it is the load-bearing limit.** This record's
> own headline is that **the detector sits on ONE allocation funnel and 511
> direct allocation call sites in the native crates never reach it**. That is
> unchanged. Everything above is therefore a **LOWER bound on live aliasing**,
> not a census — including the claim that the widest LIVE over-allocation's
> owner bypasses the detector, which by construction this instrument cannot see
> and which is NOT adjudicated here. The four repaired sites are not
> individually confirmed either: the instrument reports classes and call-site
> chains, not the source sites §6 enumerates, so no row above is matched to a
> row in this record. What is settled is that the run is possible, was done on
> an instrumented binary, and is not empty.
## 1. The repair is real — verified, not taken on trust

`native-builtins/src/util_concurrent_ext.rs`, the condition at the funnel:

```rust
let real = ctx.class_num_total_fields(cid);
if num_fields > 0 && real > 0 && num_fields != real {
    report_layout_alias(class_name, num_fields, real);
}
let n = num_fields.max(real);
```

and `report_layout_alias` itself branches `if num_fields < real { … direction =
"under" … } else { … direction = "over" … }`, one flag
(`CRATONVM_DBG_LAYOUT_ALIAS`), one dedup key, one `#[track_caller]` chain. Both
halves are present, on the same channel, exactly as W4-4's 2026-08-11 section
describes them. This lane read the code rather than the record, because this
campaign has produced instruments declared fixed that were not
(`an-instrument-on-one-arm`, `three JIT levers that are inert`) — this one is
fixed.

There is one qualification the repair's own doc comment already makes and it is
worth restating because it bounds everything below: **the detector compares the
requested count against whatever CratonVM has loaded.** In synthetic-JDK mode a
fabricated class declares exactly `num_fields` and nothing fires. Every number
in this record is a source-level upper bound on what the flag would print in
real-JDK mode with the class loaded, not a transcript.

## 2. Three blind spots remain, and the third is new

**(a) `real == 0` — known, and bigger than the record's figure.** 197 distinct
(class, requested) pairs across 447 call sites here, against W4-4's 152/339.
Unmeasured, not cleared; the reason (0 means both "not loaded" and "declares
nothing") is on `report_layout_alias` and needs a `class_is_loaded` predicate
that `NativeContext` does not have.

**(b) 340 non-literal call sites — known.** They pass the class or the count
through a constant, a variable or an expression and cannot be read from source.
W4-4 put this at ~350; the two agree.

> **(c) IS CLOSED, 2026-08-12 — do not quote it as an open hole.** This
> record's sharpest finding was acted on: the detector was moved out of
> `native-builtins/src/util_concurrent_ext.rs` into
> `native-api/src/layout_alias.rs` and given a second observation point on
> `NativeContextImpl::alloc_object` (`vm/src/vm/vm_exec.rs:12058`), the terminal
> every native object allocation in every native crate reaches. The funnel's own
> call is **kept** and is load-bearing for the `under` direction — the funnel
> clamps `n = requested.max(real)` before it allocates, so by the time an
> under-request reaches the base allocator it has become `n == real` and there is
> nothing left to see. One implementation, two callers; the funnel's copy was
> deleted rather than left beside it. `native-api/tests/layout_alias_coverage.rs`
> is the gate that every production direct-allocation site now reaches it.
>
> Two consequences for this record. **§2(c)'s "511 direct call sites" is a
> historical figure**, not a current one — the population is now printed by that
> file's `census` test rather than counted from source here. And **§9.3 is
> discharged**: it asked for exactly this change ("a census over them needs the
> same requested-vs-declared comparison moved down to
> `NativeContext::alloc_object`, in `vm/`"), including its warning about the
> dedup key, which the landed form answers by keying on the **Java frame** rather
> than a `#[track_caller]` Rust location — a site says a path exists, a frame says
> it ran.
>
> One thing the closure did **not** do, stated because it is the natural
> misreading: the base allocator clamps `slots = num_fields.max(real_fields)` two
> lines after it observes, so `direction=under` still cannot describe a short
> object. The genuinely short population arrives with `real_fields == 0` and is
> reported as `undeclared` — a third direction added by
> `W7-73-short-object-blind-spot.md`, whose source-level bound is the ratchet
> `the_unresolved_class_fallback_population_only_shrinks` (`BOUND = 28`, 14 of
> them short; MAY ONLY GO DOWN).

**(c) NEW — the census covers ONE funnel, and it is not the only allocator.**
`native-builtins`, `native-io` and `native-collections` contain **511**
`alloc_object(` / `try_alloc_object_gc_safe(` call sites that never pass through
`try_alloc_concurrent_synthetic`, so no request/declared comparison is made for
any of them and `CRATONVM_DBG_LAYOUT_ALIAS=1` cannot name one. This is not
hypothetical: `native-io/src/async_socket.rs` allocates
`java/nio/channels/AsynchronousSocketChannel` as
`alloc_obj(ctx, "java/nio/channels/AsynchronousSocketChannel", N_FIELDS)` with
`N_FIELDS = 4` against a class declaring **1**, and that module is the live
owner of the class (§5). **The widest LIVE over-allocation in this workspace is
invisible to the instrument built to find over-allocations.**

That matters for the follow-up procedure W4-4 prescribes — run the flag,
intersect with `CRATONVM_DBG_VALIDATE_NEW=1` and the `cratonvm::gc::guard`
out-of-bounds reads. List 1 is drawn from one funnel; lists 2 and 3 are drawn
from the whole heap. **An intersection is only as complete as its smallest
list**, and a class allocated wide by a direct `alloc_object` will appear in
lists 2 and 3 with no list-1 row to intersect it with, which reads as "not this
species" when it is exactly this species.

## 3. The re-derived census, and what it corrects

Extracted by walking `native-builtins/src/**.rs`, matching the funnel name and
parsing the argument list with balanced-paren splitting (so multi-line calls are
included, which is where this differs from W4-4's count), then `javap -p`
transitively for every JDK class named.

| direction | distinct (class, requested) pairs | call sites | W4-4's figure |
|---|---|---|---|
| **over** (requested > declared) | 46 | 159 | 48 / 162 |
| under | 189 | 528 | 179 / 442 |
| exact | 79 | 251 | 78 / 320 |
| `real == 0` (interface / no instance fields) | 197 | 447 | 152 / 339 |
| class not on the JDK 25 image | 98 | 174 | 158 / 280 |
| **total resolved** | — | **1,559 of 1,899** | 1,543 of 1,895 |

The two censuses agree on shape and on the `over` column to within ~2%. They
disagree most on `real == 0` versus "not on the image", which is a
class-resolution difference, not a direction difference: a class W4-4 could not
resolve at all lands here as an interface with zero instance fields.

**One row of W4-4's own table is off by two.**
`java/util/concurrent/ConcurrentHashMap` is listed as 16 vs **10**. It declares
10 instance fields itself and inherits **2** more from `java.util.AbstractMap`
(`keySet`, `values`), so the transitive count — the one CratonVM's
`num_total_fields` computes, and the one the detector compares against — is
**12**. Still `over`, by 4 rather than 6. `java/nio/channels/DatagramChannel`
5 vs 10 UNDER, W4-4's other headline row, reproduces exactly.

## 4. The census W4-4 did not take: the WRITES

Allocation width is the instrument. The species is the **write**. So this lane
also resolved, for every object allocated through the funnel with a literal
class and count, every literal `set_field(<that object>, <k>, …)` in the
allocating function, and asked whether `k` is past the class's declared width.

* **3,282** literal slot writes on funnel-allocated objects resolve this way.
* **1,340** are at an index at or beyond the declared width.
* **174** of those are on classes that actually **declare instance fields** —
  the rest are interfaces and other zero-field classes, where a non-zero
  fabrication is the intent and there is nothing to alias.

Those 174 writes cover **23 classes**. Split by whether the enclosing registrar
is reachable from `register_essential_natives(_with_shims)` (live in Compatible
mode) or only from `register_synthetic_overrides` (synthetic-JDK-only, which
`vm_init.rs:1552` gates on `config.use_synthetic_jdk` at runtime):

| class | declared | max index written | reachability |
|---|---|---|---|
| `java/util/concurrent/CompletableFuture` | 2 | 3 | 39 writes synthetic-only (`register_phase55_executors`), 2 **LIVE** (`hci_send_async`) |
| `java/util/HashSet` | 1 | 2 | 19 synthetic-only, **3 LIVE** (`wildfly_security`, `util_time`) |
| `java/time/LocalDateTime` | 2 | 6 | synthetic-only |
| `java/util/Optional` | 1 | 1 | synthetic-only |
| `java/time/Year` | 1 | 1 | synthetic-only |
| `javax/script/SimpleBindings` | 1 | 2 | synthetic-only |
| `java/nio/channels/AsynchronousSocketChannel` | 1 | 3 | **LIVE** — and see §5 |
| `java/net/InetSocketAddress` | 1 | 2 | synthetic-only |
| `java/nio/channels/SelectionKey` | 1 | 3 | synthetic-only + `servlet.rs` (already diagnosed by W7-9) |
| `javax/net/ssl/SSLEngine` | 2 | 6 | **LIVE** (`ssleng_alloc`, `register_p68_ssl`) |
| `java/lang/invoke/VarHandle` | 4 | 5 | synthetic-only |
| `java/security/KeyStore` | 4 | 4 | synthetic-only |
| `java/nio/ByteOrder` | 1 | 3 | synthetic-only |
| `jdk/internal/reflect/ConstantPool` | 1 | 1 | **LIVE** — repaired, §6 |
| `java/net/http/HttpHeaders` | 1 | 1 | **LIVE** — not repaired, §7 |
| `java/text/DateFormat`, `java/text/CollationKey`, `java/time/YearMonth`, `java/time/MonthDay`, `java/net/DatagramSocket`, `java/lang/ScopedValue`, `javax/xml/parsers/SAXParserFactory` | 1–2 | 1–2 | synthetic-only |

And the allocation-width census split the same way: of the **159** `over` call
sites, **20** sit in a registrar this lane's call-graph walk reaches from the
essential path, **61** are synthetic-only, and **78** are reached only through
a function pointer or a closure the walk does not follow — indeterminate, not
cleared. The 20 LIVE ones, widest first: `java/util/Locale` 32 vs 4,
`java/util/Properties` 16 vs 10, `javax/net/ssl/SSLEngine` 7 vs 2,
`java/util/concurrent/ConcurrentHashMap` 16 vs 12,
`java/nio/channels/AsynchronousSocketChannel` 4 vs 1,
`java/lang/module/ModuleDescriptor` 16 vs 14, the two
`jdk/internal/module/SystemModuleFinders$…` 4 vs 2,
`java/net/InetSocketAddress` 3 vs 1 and 2 vs 1, `java/net/http/HttpHeaders`
2 vs 1, `java/net/URI` 18 vs 17, both `java/net/Socket$Socket*Stream` 3 vs 2.

### What this does to W4-4's status line

W4-4 says *"two MISMATCHes fixed, the rest of the lane's surface verified safe
with a reason per site"*. That holds for **the sites W4-4 enumerated**. It is
not a statement about this species' population, and three of its own paragraphs
are narrower than the truth:

* **"Only slots 0 and 1 are ever written" (the CompletableFuture paragraph)** is
  false as a statement about the workspace. `phases_late/concurrent.rs` writes
  slot **2** at 39 sites and `http_client.rs` wrote slots **2 and 3**. The first
  set is synthetic-only, so the paragraph's *conclusion* (don't narrow the
  allocation yet) survives; its *premise* does not, and the LIVE pair it did not
  know about is repaired here.
* **The "Latent, not currently live" HashSet paragraph** names four sites. The
  real population is **20 allocation sites across 14 files**, of which three
  writes are LIVE on the essential path, in `wildfly_security.rs` (twice) and
  `util_time.rs`.
* **The `register_p67_async_channels` row — "safe — abstract classes; the static
  `open()` factories have `Code`, so the receiver is always
  CratonVM-fabricated"** — is a right verdict for a wrong reason, and the reason
  is the load-bearing half. "CratonVM fabricated it" does not make a slot write
  safe: the object still carries the **real class's `ClassId`**, so slot 0 is
  `provider`, a reference the collector scans as an oop, whoever allocated it.
  What actually makes that row harmless is something else entirely, and W4-4
  could not have known it because it is in another crate (§5).

**`HEADER_SIZE` was checked and is not a factor here.** Every site in this
species addresses fields by slot INDEX (`set_field(obj, i, …)`), which the
object model resolves relative to the header for the caller; no arithmetic in
`native-builtins`, `native-io` or `native-collections` adds a header size by
hand. Grepped for hard-coded header constants in those crates: none.

## 5. The one that is dead, and disagrees with its owner anyway

`native-builtins/src/phases_late/net_channels.rs::register_p67_async_channels`
registers eight triples on `java/nio/channels/AsynchronousSocketChannel` under a
4-slot map. `native-io/src/async_socket.rs::register_async_socket_real`
registers **the same class** under a **different** 4-slot map, and
`register_io_natives` runs AFTER `register_essential_natives_with_shims`
(`vm/src/vm/vm_init.rs:1701` then `1898`; `2208` then `2403`). Registration is
last-write-wins, so seven of the eight — `open()` ×2, `isOpen`, `close`,
`getRemoteAddress`, `read(ByteBuffer)`, `write(ByteBuffer)` — are **dead code**.

The eighth survives, because native-io registers only the
`(SocketAddress, Object, CompletionHandler)V` form of `connect` and this one is
`(Ljava/net/SocketAddress;)Ljava/util/concurrent/Future;`. It therefore runs
against objects `aio_asc_open` allocated, under the wrong map:

| slot | native-builtins (the survivor) | native-io (the owner AND the allocator) |
|---|---|---|
| 0 | connected | `F_OPEN` |
| 1 | open | `F_CONNECTED` |
| 2 | fd_id (an `fd_table` fd) | `F_REG_ID` (an AIO registry id) |
| 3 | remote | `F_REMOTE` |

Slots 0 and 1 have **opposite** meanings and slot 2 holds a different *kind* of
integer. That is the two-layouts-on-one-class condition — the shape that made
`java.lang.Process` a bug — and neither map is layout-correct in the first
place: the class declares exactly one field, `provider`, so slot 0 of both maps
writes an `Int` into a reference the collector scans, and slots 1–3 sit past the
end of the real layout.

**Not repaired here, deliberately.** The owner is also the allocator, so the
repair has to move both crates in one step, and a one-sided renumber only moves
the disagreement — the same reason W7-9 gave for not repairing `SelectionKey`
from the wrong side. This lane's first attempt DID convert the
native-builtins side to the appended-slot idiom; that was reverted, because it
would have made the one surviving triple throw against an object the other crate
allocates. **A repair to dead code that breaks the one live path is worse than
no repair.** What is committed instead is the measurement, stated in place at
the registration site so the next reader cannot repeat the analysis from
scratch.

## 6. Repaired

Four sites. All four are cases where the write lands on, or past, a field the
real class declares — the over direction. Each says whether it touches
Compatible mode and **how the check would fail**, because a layout repair with
no failing observation attached is how this campaign accumulated an index of
vacuous greens.

### 6.1 `jdk/internal/reflect/ConstantPool` — `lang_class.rs`

Real class declares **one** field, `private final Object constantPoolOop`. The
old form allocated 2 slots and wrote `Int(class_id)` into slot 0 — a small int
in a slot the collector scans as an oop, the `MethodHandles$Lookup` /
`allowedModes` shape of §5 of `natives-over-real-jdk-classes.md` — with the
mirror one slot past the end.

Moved onto the new appended-slot idiom (§8): the two private slots now start
above every field the class declares. **Touches Compatible mode** — genuine bug
fix. Safe to move because this file holds the only mention of that class name in
the workspace: nothing reads either slot back, and the real JDK's own
`ConstantPool` accessors are `native` with no registration here, so there is no
reader of the old layout to break. In synthetic mode the class is a fabricated
stub, the base is 0, and the allocation and both writes are byte-identical.

**How it fails:** `debug_assert!` inside `try_alloc_with_appended_slots` fires if
the object comes back narrower than `base + width` — i.e. if the funnel's clamp
ever stops honouring an over-request. Beyond that, the direct evidence is the
detector itself: before the change this site is one of the `over` rows
`CRATONVM_DBG_LAYOUT_ALIAS=1` prints (`requested_fields=2, real_fields=1,
direction="over"`); after it, the request equals the declared width and the row
disappears. That is a report changing state on a real run, not an assertion
about itself.

### 6.2 `java/util/concurrent/CompletableFuture` — `http_client.rs::hci_send_async`

Real class declares two fields: `volatile Object result` and
`volatile Completion stack`. The old form allocated **four** slots and wrote
`result` (right), `Int(1)` into `stack` (a reference slot), and two values past
the end — then handed the object to Java, where the **real** `CompletableFuture`
bytecode owns it and `complete`, `postComplete` and `getNumberOfDependents` all
walk `stack` as a `Completion` chain. This is the same `done`-int-over-a-
reference shape `util_concurrent_ext::native_cf_complete` documents on itself,
in a live real-JDK path rather than a synthetic one.

Replaced by `aio_completed_future`, which invokes the JDK's own
`CompletableFuture.completedFuture(Object)`: the layout is whatever the loaded
class actually is, and no index is written at all. That is the same helper the
async-channel natives on the same essential path already use, in both modes.
Semantics unchanged — the old form always marked the future done, with a null
result on failure, which is exactly `completedFuture(null)`.
**Touches Compatible mode** — genuine bug fix.

**How it fails:** `probes/SlotIndexRecensusProbe.java` §1 calls
`future.getNumberOfDependents()` and `future.thenApply(…)` on the future
`sendAsync` returns. Both are JDK bytecode that dereferences `stack` **by the
JDK's own index**, and CratonVM registers no native on either, so the read
cannot be satisfied by the same wrong slot map that produced the write. Before
the fix that field holds `Int(1)`; after it, a real completed future with
`stack == null` answers 0. The probe reaches the code over a
`com.sun.net.httpserver.HttpServer` bound to `127.0.0.1:0`, so it needs no
external network.

### 6.3 / 6.4 `java/util/HashSet` — `wildfly_security.rs`, two LIVE sites

`Subject.getPrincipals()` and `SecurityIdentity.getRoles()` returned a
`java.util.HashSet` allocated with **three** slots, element count in slot 1.
Real `HashSet` declares exactly one field, `private transient HashMap map`. So
on the real layout the returned object had `map == null` — every real `Set`
method (`size`, `isEmpty`, `iterator`, `contains`) dereferences it and throws —
and the count sat one slot past the end of everything the class declares, where
**nothing can read it back**: there is no `size` field on a real `HashSet` for
reflection to find. The count was write-only garbage in Compatible mode.

Both natives are registered on the essential path
(`register_jdk_security_natives` from `register_essential_natives_with_shims`,
and `register_wildfly_security_natives`), and `javax.security.auth.Subject` is a
real JDK class whose `getPrincipals` has `Code` — which, under the corrected
reachability rule (registration is the gate on the cold and reflective paths),
means our native answers. This is a live Compatible-mode path.
**Touches Compatible mode** — genuine bug fix.

Now routed through a `count_carrying_hash_set` helper: on a fabricated stub the
3-slot shape IS the layout and that arm is unchanged; on the real layout it
builds a well-formed set through the pre-existing remedy helper
`build_real_layout_string_hashset`, so `size()` answers 0 through the JDK's own
bytecode instead of throwing. The Rust-side count is not representable on the
real layout without materialising Java `Principal` objects, which is a larger
change than this lane; losing it costs nothing, because on the real layout it
was never readable.

**How it fails, and the honest limit.** `probes/SlotIndexRecensusProbe.java` §2
calls `size()`, `isEmpty()` and `iterator()` on the returned set — real
`HashSet` bytecode dereferencing `map`, not our natives. But a fresh
`new Subject()` takes the *other* branch of `native_subject_get_principals` (the
real backing set, because `principal_count() == 0`), so the probe exercises the
**shape** and not the repaired branch: reaching the repaired branch needs
principals recorded Rust-side, which only a WildFly login module does. The probe
says so in its own header rather than claiming a green it did not earn. The
repaired branch's real check is the source argument above — `map` is null there,
which is not a judgement call.

## 7. Live, over-allocating, and NOT repaired — with the reason

> **CORRECTED 2026-08-12 — the `SSLEngine` bullet below is a FALSE POSITIVE in
> both shipping modes, and the correction is not this lane's to re-derive.**
> `W7-61-sslengine-layout-and-tls-blocking.md` measured the registration order:
> `register_p68_ssl` registers both `SSLContext.createSSLEngine` descriptors onto
> `ssleng_alloc` (which requests 7), and **23 lines later**
> `net_phase_e::register_re6_ssl_context` re-registers both of them onto bodies
> that allocate `sun/security/ssl/SSLEngineImpl` instead. Registration is
> last-write-wins, so `ssleng_alloc`'s only two callers are overwritten and it
> allocates nothing on either shipping boot path. The 7-vs-2 over-allocation is
> **live only under `--synthetic-jdk`**, where `register_phase68_natives` calls
> `register_p68_ssl` again and it wins back.
>
> This is not taken on trust: `native-builtins/src/tls.rs:4948`–`:5021` is a
> source gate asserting `register_re6_ssl_context` is the LAST writer of both
> descriptors on both the solo and boot orders, whose own failure message says
> *"If p68 wins here, `ssleng_alloc` is live again"*. Read W7-61 before touching
> the SSL slot map; do not re-derive the order.
>
> Two other places in this record carry the same error and are corrected by this
> note rather than edited in place, so what they contribute stays traceable:
> §4's write-side table (`javax/net/ssl/SSLEngine` | 2 | 6 | **LIVE**) and the
> `javax/net/ssl/SSLEngine 7 vs 2` entry in the "20 LIVE" allocation-site list
> under it. **Both should read `synthetic-jdk only`.** The 23-class figure for
> that table is unchanged — the class stays in the census, its reachability label
> is what was wrong — and the "20 LIVE `over` call sites" figure drops by the one
> site inside `ssleng_alloc`. It is **not** restated as 19 here: that list is
> enumerated by class while the 20 counts call sites, this lane did not re-run
> the call-graph walk that produced either, and re-deriving one number from the
> other by hand is how the `ConcurrentHashMap` 16-vs-10 row in W4-4 got written.
> The correction is the label; the recount belongs to whoever next runs the flag.
>
> The error leaned in the direction that overstates open work — the direction
> HANDOFF-20260812.md found every stale row in this directory leaning.

* ~~**`javax/net/ssl/SSLEngine` 7 vs 2, 27 receiver-slot accesses at indices 2–6**~~
  — **DEAD on both shipping boot paths; see the correction above.**
  (`phases_late/ssl_security.rs`, `ssleng_alloc` + `register_p68_ssl`). Real
  `SSLEngine` declares `peerHost` (a reference) and `peerPort`, and the model
  writes `Int` into both. Not repaired because the fix is a base offset threaded
  through 27 access sites plus two sibling registrars in `tls.rs` and
  `t27_tls.rs` that also register `SSLEngine`/`SSLSession` triples — a
  last-write-wins question this lane could not settle without a build. The
  mitigating fact, unverified: a real engine is a
  `sun.security.ssl.SSLEngineImpl`, which declares each of these methods with
  `Code`, so the registry lookup is keyed on a name with no registration and the
  superclass walk is skipped — the same gate as W4-4's `X509Certificate` row.
  That makes the receivers ours, which bounds the damage to the aliasing of
  `peerHost`/`peerPort`, not to a foreign object.
* **`java/net/http/HttpHeaders` 2 vs 1** (`http_client.rs:1504`). Slot 0 is
  `headers`, a `Map`, and the native writes a `String[]` array reference into
  it — so this site is *already* wrong in a way narrowing the allocation would
  not fix, and the repair is a `HttpHeaders.of(...)` call, not a renumber.
  `java/net/http/HttpHeaders` is registered from three files; the owner has to
  be settled first.
* **`java/util/Locale` 32 vs 4, `java/util/Properties` 16 vs 10,
  `java/util/concurrent/ConcurrentHashMap` 16 vs 12,
  `java/lang/module/ModuleDescriptor` 16 vs 14** — LIVE and wide, but this lane
  found no literal write past the declared width at any of them, so they are
  wide-but-unused: the cheap, local narrowing W4-4's follow-up section describes,
  not a defect. They should be narrowed at the call site by whoever next runs the
  flag on a real workload and confirms no guard hit.
* **`java/util/HashSet` in `util_time.rs::native_zone_id_get_available`** — the
  third LIVE 3-slot HashSet. Left alone because its registrar
  (`register_time_extras_natives`) is synthetic-only by the call-graph walk while
  the function is reached through a function pointer the walk does not follow;
  the reachability is genuinely indeterminate and a wrong guess here changes
  `ZoneId.getAvailableZoneIds()`.

## 8. The idiom this lane adds

Two functions next to the funnel in `util_concurrent_ext.rs`, because the
"append your private slots above the real layout" remedy existed only as four
private copies of `synthetic_base_offset` in `jca/`:

* `appended_slot_base_for_class` — 0 for a fabricated stub, the real transitive
  field count otherwise.
* `try_alloc_with_appended_slots` — allocate `base + width` and hand back the
  base, with a debug assertion that the object came back at least that wide.

**The stub arm is the part `jca/`'s copies do not have, and it is load-bearing.**
`synthetic_base_offset` asks `class_num_total_fields` unconditionally. In
synthetic-JDK mode the first allocation fabricates a class declaring
`base + width` fields, so the *next* call reads that number back as the new base
— the base RATCHETS, and two objects of one class end up with two different slot
maps in one run, which is the very condition this species is about.
`is_class_synthetic_stub` is stable under that: a stub stays a stub.

**What the idiom cannot do**, stated so nobody reaches for it there: it cannot
make a native safe against a receiver it did not allocate. A third function was
written for that — the base for a receiver, `None` when the object is too narrow
to carry the map — and is NOT committed, because its only caller was the
reverted §5 conversion and a helper with no caller is how a knob outlives its
reader. The reasoning is kept as a comment where it would have gone, because it
is the first thing the next reader will reach for and it does not work: given a
foreign object, "how many private slots does it carry" is unanswerable from its
width alone. A real-layout instance of the exact class is narrow and can be
refused; a real SUBCLASS instance is wide for its own reasons, and `width -
real` lands squarely inside its own fields. Such a helper can only refuse the
easy half. The sound remedy for foreign receivers is a side table keyed on
object identity, which `jca/key_factory.rs` already runs — with the GC-stable
key that lane had to invent when the raw `ObjectRef` address aliased across a
young collection — and which is out of scope here.

## 9. What this lane could not resolve

1. **The 78 `over` call sites whose registrar reachability is indeterminate.**
   The call-graph walk follows named calls; a native installed as a function
   pointer or a closure inside a registrar is only reachable through the
   registrar, and the walk resolves that correctly, but a registrar reached
   *only* through a pointer is not resolved. 78 of 159 `over` sites are in that
   state. They are unclassified, not cleared.
2. **Whether any class is allocated at BOTH widths in one run.** Still the
   question no instrument answers, and §5 now supplies a concrete candidate:
   `AsynchronousSocketChannel` at 4 from two crates with incompatible meanings,
   plus whatever real bytecode allocates at 1.
3. ~~**The 511 direct `alloc_object` sites.**~~ **DONE 2026-08-12 — see the
   §2(c) closure note.** The comparison was moved to
   `NativeContextImpl::alloc_object` exactly as prescribed, the funnel's call was
   kept for the `under` direction, and the dedup-key worry was answered by keying
   on the Java frame. What remains is not the wiring but the **run**: the census
   is a source-level upper bound until somebody executes
   `CRATONVM_DBG_LAYOUT_ALIAS=1` over a workload, which nobody has done for this
   record, W7-66, W7-68, W7-73 or W7-90.
4. **Runtime confirmation of anything here.** This lane cannot build or run the
   VM. Every claim is source-level and argued from `javap` against JDK 25.0.3.9,
   exactly as W4-4's own closing sentence says of itself.
