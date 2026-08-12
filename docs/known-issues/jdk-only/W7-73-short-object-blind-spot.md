# W7-73 — the short-object blind spot: where the genuinely narrow objects were, and why neither direction of the census could see them

Status: the blind spot is measured, instrumented and gated. W7-68-live-under-allocations.md
was right that `direction=under` cannot describe a short object and right that
the short objects are in the `declared == 0` case it could not see. It was
**incomplete about where that case comes from**, and the missing half is the
larger one: the dominant producer of `declared == 0` is not "the class is not
loaded yet" — it is the `Err(_) => ClassId::new(0)` fallback arm, which W7-68 §2
examined at four sites and dismissed as *"not an under-allocation"*. That verdict
is correct for the width species and exactly backwards for the short-object
species. **Those arms ARE the blind spot.**

The census: **30 production sites** can reach `NativeContextImpl::alloc_object`
with no declared layout to clamp against. **16 of the 30 request fewer slots than
the class they name really declares.** Two of the 16 are not fallback arms at
all — `java/lang/Thread` allocated **5 slots wide against a class declaring 19**,
unconditionally, at every native thread-mirror creation in `vertx_eventloop.rs`
and `xnio_io_thread.rs`.

Branch `fix/short-object-blind-spot-20260812`. **Nothing here was built or
run** — this lane writes code and docs only. Every declared width is `javap -p`
against the JDK 25.0.3.9 image on this Windows host, counted transitively over
the superclass chain with `static` excluded — the same oracle and convention as
W4-4-slot-index-species-sweep.md, W7-49-slot-index-recensus.md,
W7-59-layout-detector-coverage.md and W7-68-live-under-allocations.md. The oracle
was validated against W7-68 §2 before it was trusted: `Pattern` 20, `ZipEntry` 14,
`ServiceLoader` 10, `ConcurrentHashMap` 12, `MappedByteBuffer` 13, `ByteBuffer`
11, `DatagramChannel` 10, `TreeMap` 9, `HashMap` 8, `IOException` 6, `File` 4,
`FileChannel` 4, `ArrayList` 3, `ZoneOffset` 3, `HashSet` 1,
`AsynchronousSocketChannel` 1 — **all sixteen reproduce exactly**.

---

## 1. The clamp-after-detect analysis: CONFIRMED, and one correction

W7-68 §1 is confirmed on both halves, read directly rather than inherited.

`vm/src/vm/vm_exec.rs`, `NativeContextImpl::alloc_object`. The observation block
opens with `if cratonvm_native_api::layout_alias::enabled() && …classify(num_fields,
real_fields).is_some()`, and the very next statement after that block closes is

```rust
let slots = num_fields.max(real_fields);
```

`real_fields` is the loaded class's `num_total_fields`, resolved once above for
the clamp and read by both. And `layout_alias::classify` began

```rust
if requested == 0 || declared == 0 || requested == declared { return None; }
```

So a row could only print `under` when `declared > 0`, and whenever
`declared > 0` the clamp had already widened the object to `declared`.
**`direction=under` ⟹ the object came back at its full declared width**, and
`direction=under` is therefore a report about a *caller* that asked wrongly, not
about an object that is wrong. Structurally, not usually.

The second observation point behaves the same way for the same reason:
`try_alloc_concurrent_synthetic` clamps `n = requested.max(real)` after its
report.

**The correction.** W7-68 §1 states the consequence as *"if the class is not
loaded, `real_fields` is 0"*. That is one producer of `declared == 0` and it is
not the interesting one, because the base allocator does not simply pass an
unresolved id through. §2 is what the reading missed.

---

## 2. Where `real_fields == 0` actually comes from — and no, `ClassId::new(0)` does not widen it, it *bypasses* it

The lane was asked whether a `ClassId::new(0)` sentinel flowing into
`alloc_object` produces `declared == 0` for a class that *is* loaded, which would
make the blind spot much larger than "not yet loaded" suggests. **It does not,
and the truth is worse.** `alloc_object` opens with

```rust
let class_id = if class_id == ClassId::new(0) && num_fields > 0 { … } else { class_id };
```

and the taken branch substitutes `cratonvm/synthetic/AnonymousObject$N`, which
`fabricate_class` registers with `num_total_fields: num_fields` — **exactly the
requested count**. So the sentinel never reaches `classify` as `declared == 0`.
It reaches it as `classify(n, n)`: perfect agreement, no row, in either
direction.

That is not a smaller blind spot than the one W7-68 predicted. It is a
*different and larger* one, and it is invisible from two sides at once:

* the `under` direction cannot fire, because the clamp runs first;
* this path cannot fire, because the substitution makes the two widths agree by
  construction.

And there is a third bypass underneath: the `anon_class_cache` fast path

```rust
if cached != 0 { return self.heap_alloc_object(ClassId::new(cached), num_fields); }
```

**returns from `alloc_object` before the census block is reached at all.** The
gate `the_layout_asserting_allocation_surface_is_exactly_two_methods` asserts
there are exactly two allocation doors on the `NativeContext` trait; it cannot
see a door opened *inside* one of them. That early return is the busiest untyped
allocation path in the VM (every `HashMap`/`LinkedHashMap` node).

So the honest taxonomy of "the base allocator has no declared layout to clamp
against", in descending order of how much it matters:

| producer | what the caller did | what the census said before | short? |
|---|---|---|---|
| **`ClassId::new(0)` sentinel** | resolved a class, FAILED, allocated anyway | nothing — `classify(n, n)` | **yes, 16 of 30 sites** |
| a fabricated stub narrower than the real class | got a stub back from `ensure_class_initialized` | `over` if wider, nothing if equal | possible; needs a build |
| `get_class(class_id)` → `None` | held a stale id, or one observed mid-registration | nothing — `declared == 0` | possible, non-deterministic |
| genuinely field-less class | allocated an interface or `java/lang/Object` at N slots | nothing — `declared == 0` | **no**, benign and common |

The last row is why the exclusion existed and why the argument for it was sound.
It is also why the fix is a **third direction** rather than folding the case into
`over`: the instrument genuinely cannot tell these four apart from where it
stands, and a row that says "I cannot adjudicate this" is actionable in a way
that silence is not.

### 2.1 The four rows W7-68 §2 cleared are the four rows this lane files

Worth stating plainly, because two lanes reading the same four sites reach
opposite verdicts and both are right about different things.

W7-68 §2 found that `ZipEntry` 6/14, `ServiceLoader` 2/10, `ConcurrentHashMap`
2/12 and `DatagramChannel` 5/10 were the source-level census reading an `Err(_)`
arm beside a live `real.max(N)` arm, and filed them **"not an under-allocation"**.
For the width species that is correct: on the live arm the request *is* `real`,
and no row prints.

But the `Err(_)` arm is not decoration. `zip_real_jar.rs`:

```rust
let (entry, real_layout) = match ctx.ensure_class_initialized("java/util/zip/ZipEntry") {
    Ok(cid)  => { let real = ctx.class_num_total_fields(cid);
                  (ctx.alloc_object(cid, real.max(6)),
                   ctx.resolve_field_index_by_class_id(cid, "xdostime").is_some()) }
    Err(_)   => (ctx.alloc_object(ClassId::new(0), 6), false),
};
```

When that arm is taken the object is `cratonvm/synthetic/AnonymousObject$6`, six
slots wide, `real_layout` is false so the ten writes go by **index** into slots
0–5, and it is handed back to Java as a `java.util.zip.ZipEntry` — a class
declaring fourteen fields, which it is also not an instance of. Nothing clamps
it, nothing reports it, and `direction=under` is structurally incapable of
mentioning it.

This is the same reading error in both directions, which is what makes it worth
a paragraph: **a source reader who follows only the arm that normally runs
mis-reads the width census, and a source reader who follows only the arm that
normally runs also mis-reads the risk.** `docs/architecture/natives-over-real-jdk-classes.md`
§5 already contains the rule, in a blockquote, about the *other* half of exactly
this shape:

> A fix that pins only the positive half hides what it unmasked. … read every
> `else`, every error branch, and every "re-assert if that did not land"
> fallback in the same function before calling it done. The fallback is
> precisely the code a green transcript never executes.

The last sentence is also the reason this census cannot be closed from source.
Nothing in the tree can say whether any of these 30 arms is ever taken, which is
precisely why the runtime row has to exist.

---

## 3. The census

Population: production (non-`#[cfg(test)]`) call sites of `alloc_object` /
`try_alloc_object_gc_safe` in the six native crates whose **class argument is
literally `ClassId::new(0)`**. 208 production sites scanned, 35 match, 5 of those
request `0` slots (no substitution happens, no layout is asserted, nothing to
say). **30 sites in the population.** Two independent scanners — one written for
this lane, one written to mirror the Rust gate's matching exactly — agree on 35
and on 30.

`javap -p` transitive widths, JDK 25.0.3.9. "short" = requested < declared.

### 3.1 The 16 short sites

| site | class it names | requested | declared | short by |
|---|---|---:|---:|---:|
| `native-builtins/src/vertx_eventloop.rs:713` | `java/lang/Thread` | 5 | **19** | **14** |
| `native-builtins/src/xnio_io_thread.rs:939` | `java/lang/Thread` | 5 | **19** | **14** |
| `native-io/src/lib.rs:5050` | `java/util/regex/Pattern` | 2 | 20 | 18 |
| `native-io/src/lib.rs:16603` | `java/nio/MappedByteBuffer` | 2 | 13 | 11 |
| `native-io/src/async_socket.rs:1980` (`alloc_obj`) | `sun/nio/ch/Iocp` | 1 | 14 | 13 |
| `native-io/src/zip_real_jar.rs:650` | `java/util/zip/ZipEntry` | 6 | 14 | 8 |
| `native-builtins/src/service_loader.rs:90` | `java/util/ServiceLoader` | 2 | 10 | 8 |
| `native-io/src/nio_native.rs:1410` (`alloc_t16`) | `java/nio/channels/DatagramChannel` | 5 | 10 | 5 |
| `native-io/src/lib.rs:7474` | `java/nio/ByteBuffer` | 5 | 11 | 6 |
| `native-io/src/lib.rs:14956` (`alloc_typed_buffer`) | `java/nio/CharBuffer` &c. | 5 | 9 | 4 |
| `native-io/src/lib.rs:12722` | `java/io/File` | 1 | 4 | 3 |
| `native-io/src/lib.rs:9439` | `java/nio/channels/FileChannel` | 2 | 4 | 2 |
| `native-builtins/src/apps_h2.rs:62` | `java/lang/Thread$State` | 2 | 3 | 1 |
| `native-io/src/lib.rs:12982` | `java/util/ArrayList` | 2 | 3 | 1 |
| `native-builtins/src/lib.rs:30175` (`alloc_time_synthetic`) | `java/time/ZoneOffset` | 2 | 3 | 1 |
| `native-builtins/src/util_time.rs:140` (`alloc_time_synthetic`) | `java/time/ZoneOffset` | 2 | 3 | 1 |

Four of these are shared helpers reached with several `(class, count)` pairs; the
table shows the widest gap each helper can produce. `alloc_obj` and `alloc_t16`
also reach `AsynchronousSocketChannel` at 4/1 and `AsynchronousChannelGroup` at
1/1, which are `over` and exact respectively — a helper qualifies on its worst
caller, and a helper's `ClassId::new(0)` arm is one site however many classes
reach it.

The last two rows are `util_time.rs`'s registrars, which W7-68 §3.8 established
are reachable only through `register_synthetic_overrides` and are therefore
**dead in Compatible mode**. Kept in the count because this is a shape census and
liveness is the reader's first item of work, per W7-59 §5.3's stated convention;
flagged here so nobody re-derives it.

### 3.2 The 14 that are not short

| site | why not |
|---|---|
| `native-io/src/lib.rs:12128` | `java/nio/file/Path` is an **interface**, declares 0 — a 1-slot fabrication is intended |
| `native-io/src/lib.rs:17290` | `java/util/stream/Stream`, same |
| `native-io/src/lib.rs:16591` | `FileLock` 6 against 4 — `over`, the appended-slot idiom |
| `native-io/src/socket_channel.rs:637` | `SocketChannel`/`ServerSocketChannel` 12 against 10 — `over` |
| `native-builtins/src/lang_math.rs:2724`, `:2729` | the boxed wrappers all declare exactly 1 — exact |
| `native-io/src/nio_selector.rs:3141` | `HashSet` 1 against 1 — exact |
| `native-collections/src/lib.rs:9632`, `:44808` | `HashMap$Node` 4 against 4 — exact, and deliberately untyped (the in-file comment records nine probe failures from a previous attempt to type it) |
| `native-collections/src/lib.rs:44379` | a `ConcurrentHashMap` segment — an internal shape with no Java class at all |
| `native-builtins/src/quarkus_staticinit.rs:451`, `:455` | Quarkus classes; not in the JDK image, no oracle |
| `native-builtins/src/shared_secrets_bridge.rs:180` | dynamic `owner_class`, resolved per call; not settleable from source |
| `native-builtins/src/test_utils.rs:2432` | a mock `NativeContext`; `self.alloc_object` is the mock's own and never reaches the VM |

The last row is an over-count of exactly one in the ratchet. Left in
deliberately: excluding it needs a special case in the scanner, and a scanner
special case rots silently while a documented off-by-one does not.

### 3.3 The two rows worth acting on first

Not the widest, but the only two in the table that are **not on a fallback arm**.
`vertx_eventloop.rs:713` and `xnio_io_thread.rs:939` both read

```rust
let mirror = ctx.alloc_object(cratonvm_types::ClassId::new(0), 5);
```

with a comment explaining that *"synthetic `java/lang/Thread` is 5 slots
(see `classloading::class_manager::synthetic_field_count`): `name=0, priority=1,
tid=2, target=3, virtualFlag=4`"*. That is a correct description of the synthetic
image. On a real image `java.lang.Thread` declares **19** instance fields, the
object is `AnonymousObject$5`, and it is then published to the thread registry via
`set_native_thread_java_obj` — so it is what `Thread.currentThread()` hands back
on those carrier threads.

Both are unconditional: no resolution is attempted, so there is no arm to be
"the fallback". The comment is not wrong, it is *scoped to the wrong mode*, and
it is the tidiest example in the whole campaign of an in-place measurement that
stops being true when the image changes underneath it.

Whether it is live depends on which `java/lang/Thread` methods are registered as
natives and win last-write-wins, which needs a build. It is not repaired here —
this lane's product is the instrument and the census, and a lane that widens an
instrument and then gets lost repairing what it finds delivers neither.

---

## 4. What changed in the instrument

Three edits, all observation-only, and none of them alters what is allocated in
any mode.

**1. `classify` reports `declared == 0` instead of swallowing it.**
`native-api/src/layout_alias.rs` gains `Direction::Undeclared` and a
`direction = "undeclared"` arm. `requested == 0` and `requested == declared`
remain `None`, which really are nothing.

*Why a third direction and not `over`.* An `over` row asserts the object carries
more slots than its class has fields — a claim about a known layout. Here there
is no known layout to make a claim about, and the four producers in §2 need four
different fixes. Folding them into `over` would put a benign interface
fabrication and a 14-slot-short `Thread` mirror in the same bucket as the
appended-slot idiom, which is the correct remedy for a different species.

*Why `undeclared` and not `unknown`.* It names the observable — the class
declares nothing — rather than the instrument's state of mind. `unknown` invites
"unknown to whom", and it is the kind of name that later becomes a bucket for
every unrelated non-adjudication somebody wants somewhere to put.

*What a consumer sees.* The channel, the flag, the dedup key and the five field
names (`class`, `requested_fields`, `real_fields`, `direction`, `site`) are
unchanged. A consumer that wants the pre-2026-08-12 census filters
`direction in (under, over)` and sees precisely what it saw before.

**2. The base allocator observes the `ClassId::new(0)` sentinel *before* it is
substituted**, under the literal `layout_alias::UNRESOLVED_CLASS`
(`<unresolved:ClassId(0)>`) with `declared = 0`. It sits at the top of the
existing `if class_id == ClassId::new(0) && num_fields > 0` branch — above the
`ensure_generated_class` substitution and above the `anon_class_cache` early
`return`, both of which had to be cleared. Without it the entire §3 population is
invisible in both directions at once.

The block has no `else`. With the flag off it costs one `OnceLock` load and one
predictable branch, paid only by allocations already inside the untyped branch —
strictly less than the base observation below it, which pays the same on every
native allocation. This matches the cost profile W7-59 §3.1 committed to.

Also, on the same path: `get_class` returning `None` used to reach the row as an
empty `class` field via `unwrap_or_default()`. It now reads
`<unregistered:ClassId(N)>`, which is one of the three shapes `undeclared`
covers and needs to be distinguishable from an interface allocated at N slots.
That string is built only when a row is about to print.

**3. The fabrication funnel calls the shared rule.**
`try_alloc_concurrent_synthetic` carried

```rust
if num_fields > 0 && real > 0 && num_fields != real { report_layout_alias(…); }
```

which is `classify` open-coded. W7-59 §3 asserted *"there is one implementation
here"* and moved the counting, the flag, the dedup key and the channel into one
module — but it moved the **machinery** and left a second copy of the
**decision**, and the two had already drifted on the case that matters:
`real > 0` is exactly the exclusion this lane removed. The funnel would have
stayed blind to the short-object species after `classify` learned to report it.
It now reads `if layout_alias::classify(num_fields, real).is_some()`.

`there_is_exactly_one_detector` structurally cannot catch that: the funnel does
not read the flag and does not emit a direction, so it is not an owner by that
gate's definition. It just decides, privately, what the owner is allowed to hear.
Gate 10 below closes it.

**Compatible mode.** Untouched. No allocation, clamp, substitution, cache or
returned object changes in either mode with the flag on or off; the only thing
that changes is which rows a debug flag prints. No `CRATONVM_*` flag was added —
`CRATONVM_DBG_LAYOUT_ALIAS` is reused, so `docs/config/flag-inventory.md` needs
no regeneration. Nothing was made quieter.

---

## 5. Should the clamp stay? Yes — and the loudness belongs where the clamp is absent

The recommendation, then the reasoning.

**Keep the clamp, exactly where it is. Do not ratchet the `under` population. Do
not make a mis-request a debug-build error. Ratchet the population the clamp
cannot save — which is what this lane's gate 11 does.**

*Keep it.* Removing it converts a reported non-defect into live corruption. It is
also load-bearing history: it exists because Kafka 3.7's boot failed on
`java/util/HashSet` allocated at 1 slot against a class declaring 3, and because
`ensure_class_initialized` succeeding says nothing about whether the caller's
hard-coded slot map was written for the same image. That is settled.

*The evidence-destruction objection is real but misaimed.* It is true that the
clamp silently corrects a mis-request and that every `under` row is a caller bug
nothing forces anyone to fix. But the clamp is also **the reason** `under` rows
are safe: the correction is what makes them non-defects. Asking the clamp to be
louder is asking the wrong component to carry the risk, because the risk is
entirely in the case where **there is no clamp**. `n.max(0) == n`. That asymmetry
is the whole finding, and it is what decides the next two.

*Against ratcheting `under`.* The population is ~180 distinct triples through the
funnel plus 24 direct, and W7-68 §3.5 established that its largest single block —
eleven of the 28 live direct sites — is `native-collections`' overlay model,
where the narrow width **is** the model's layout, across five classes and two
crates. A ratchet whose population is dominated by working-as-designed rows gets
re-baselined on sight, and W7-59's `census` refused a count ratchet for exactly
this reason. Forcing that number down buys hygiene at the price of an
architectural project, and it would train people to re-baseline a file that also
contains gates worth keeping.

*Against a debug-build hard error.* Three reasons, each sufficient. It would be a
behaviour change in one build configuration, and this project's own rule is that
an instrument present on one arm becomes a variable of the comparison — a
`debug_assert` here makes debug and release disagree about whether a workload
completes. The `under` population is, on today's evidence, harmless: W7-59
measured 49 distinct JDK classes firing across a 30-class Tomcat sample with
**zero** out-of-bounds field reads in the same runs. And the failure would land
at a fabrication funnel with ~1,800 call sites, so the first debug run to trip it
becomes a bisect exercise rather than a diagnosis. Aborting on a population
nobody has yet shown to be a bug list is how a diagnostic gets switched off
permanently.

*For ratcheting the blind spot.* The 30 `ClassId::new(0)` sites are different in
kind, and that difference is what earns the ratchet `under` does not get: **every
member is defect-shaped.** Each one resolved a class, failed, and allocated
anyway, handing back an object of a class nobody asked for. Growth is never
routine, 16 of the 30 produce a genuinely short object, and shrinking the
population is a well-defined repair — propagate the failure (`MethodCallFailed`),
or allocate against a class you actually resolved. Gate 11 bounds it at 30 with
"may only go down" and prints the list on every run.

*What this lane deliberately does not do.* The clamp could preserve the evidence
rather than only correcting it — record the pre-clamp requested width against the
object, so that a later out-of-bounds read or a `CRATONVM_DBG_VALIDATE_NEW` `BAD`
line could say *"this object was requested at 2 and clamped to 14 by site X"*.
That is the shape that would make W4-4's prescribed intersection work in the
`under` direction, which today it structurally cannot. It needs a side table with
a GC-stable key and a build to settle its lifetime, and it is a follow-up, not a
line.

---

## 6. Proving the RED

Five new gates in `native-api/tests/layout_alias_coverage.rs`, following the
existing file's precedent: one test per link, so a break names which link broke.

Links 1–5 prove that every layout-asserting allocation **reaches** the census.
They say nothing about whether the census has anything to say when it gets there,
and this lane found that for the species it is named after it did not. **A gate
proving routing while the rule at the end of the route is silent is a vacuous
green with extra steps**, which is why all five of these gate the rule.

6. **`a_class_declaring_nothing_is_a_reported_direction`** — a **live call** into
   `classify`, not a source scan, because the arm can be deleted without any text
   a scanner greps for changing. Also asserts the direction is its own variant
   rather than `Over`.
   *Red when*: the `requested == 0 || declared == 0 || …` guard is restored.
   Simulated red: `classify(6, 0)` returns `None` and the first assertion fires,
   quoting `ZipEntry` 6-against-14, `Pattern` 2-against-20 and `Thread`
   5-against-19.
7. **`the_undeclared_direction_reaches_the_wire`** — the detector emits
   `direction = "undeclared"`, **and still emits `under` and `over`**, so
   widening the census can never be paid for by narrowing it.
   *Red when*: the `Undeclared` arm is dropped from `observe`'s `match` or falls
   through to another message. Simulated red by renaming the literal.
8. **`no_allocation_door_opens_before_the_census`** — no `return` appears in
   `alloc_object` above the first `layout_alias::enabled()`, with word-boundary
   checks so `returned` is not a hit. This is the link a hot-path change breaks
   silently, and it is not hypothetical: the `anon_class_cache` fast path was
   exactly this shape.
   *Red when*: any early exit is added above the observation. Simulated red with
   `if num_fields == 0 { return self.heap_alloc_object(class_id, 0); }` at the
   top of the method — the assertion fires and quotes the line.
9. **`the_unresolved_class_sentinel_is_observed_before_it_is_substituted`** —
   `layout_alias::UNRESOLVED_CLASS` must appear before `ensure_generated_class(`
   in the same body. After the substitution the widths agree by construction and
   the row cannot exist.
   *Red when*: the substitution is hoisted. Simulated red by moving an
   `ensure_generated_class` call above the observation.
10. **`the_fabrication_funnel_uses_the_shared_classify`** — the funnel's body must
    contain `layout_alias::classify(` and must not contain `real > 0`.
    *Red when*: the open-coded predicate returns. Simulated red by restoring
    `num_fields > 0 && real > 0 && num_fields != real`.
11. **`the_unresolved_class_fallback_population_only_shrinks`** — the ratchet, at
    **30**, printing the full site list on every run.
    *Red when*: a new `alloc_object(ClassId::new(0), N)` site is added. Simulated
    red with one extra site: 31 against the bound of 30.

All five simulated **green against this tree** and **red against mutated copies**
before landing — five of five, where the file's own precedent was two of five.
The scanners were replicated line-for-line in a scratch harness rather than
eyeballed, because two independent census walks in this area were confidently
wrong on the day this lane started and a third mis-read four rows. The five
pre-existing gates were re-simulated green against the changed tree, including
`the_base_allocator_observes`'s observe-before-clamp ordering (the sentinel
observation is now the first `layout_alias::observe(` in the body and still
precedes the clamp) and `there_is_exactly_one_detector`.

**What none of them prove.** That any of the 30 arms is ever taken. Nothing in
the tree can prove that, which is the reason the runtime row exists.

---

## 7. The GC framing in W7-49 §4 and architecture §5 — W7-68 is right, and §5 needs a scoped correction

Checked as asked, from the code rather than from the record. **W7-68 §1.1 holds.**

* `NativeContextImpl::alloc_object` computes
  `HEADER_SIZE + slots * SLOT_SIZE` and reaches `tlab_alloc_object` →
  `tlab_alloc_shaped_inner` → `init_object_header`, which writes
  `ObjectHeader::new(class_id, ObjectKind::Object, …)` with **no
  `GC_FLAG_COMPACT`**. Its own breadcrumb comment calls it *"the legacy-layout
  object header it writes here"*. The old-gen batch and `heap.alloc_object`
  fallbacks below it are the same shape. Native-allocated objects are legacy.
* `gen_heap::for_each_ref_slot` has three arms. The compact arm is gated on
  `is_compact_object(header)` — the per-object `GC_FLAG_COMPACT` bit — and only
  it derives reference offsets from the class, through
  `compact_oop_scan` → `layout.ref_offsets`. The legacy arm is

  ```rust
  if let Value::Object(Some(r)) = std::ptr::read(s as *const Value) { f(r.as_ptr(), slot_idx); }
  ```

  i.e. it dispatches on the **stored tag**, not the class's declared field type.

So an `Int` in a declared-reference slot of a native-allocated object is a
**semantic** defect — a wrong answer to whoever reads that field — and not heap
corruption. Confirmed.

**§5 needs a scoped correction, not a retraction, and this lane did not make it.**
The wording to change is the section title and the generalisation, not the worked
example. §5's own example is `MethodHandles$Lookup` written through *by a native
onto an object the native did not allocate* — which can be a real-bytecode `new`,
which can be compact, where the "bogus pointer for the collector to mark and
move" reading is correct. The over-claim is the blanket title, *"A slot index
against a real layout is not a wrong answer — it is heap corruption"*, applied to
the native-allocation half. Proposed qualifier, for whoever owns that document:

> This holds for objects with the **compact** layout — real bytecode `new`, and
> the JIT. It does **not** hold for objects a native allocated:
> `NativeContext::alloc_object` produces legacy 16-byte tagged `Value` cells with
> no `GC_FLAG_COMPACT`, and `gen_heap::for_each_ref_slot`'s legacy arm dispatches
> on the stored tag rather than the class's declared type. On those objects a
> wrong slot index is a wrong **answer**, which is bad enough and is a different
> severity. The corruption framing belongs to compact objects and to the `over`
> direction, where the slot count disagrees with the header.

Worth having because the severity is load-bearing: this lane's whole `undeclared`
population is native-allocated and therefore in the *wrong-answer* class, not the
*corrupt-the-heap* class, and a reader who does not know that will over-price
sixteen rows.

---

## 8. What this lane could not resolve

1. **Runtime confirmation of anything.** Nothing was built or run. Every claim is
   source-level plus `javap -p` against JDK 25.0.3.9, and the gates were
   simulated rather than executed.
2. **Whether any of the 30 fallback arms is ever taken.** Unanswerable from
   source — that is the point of the row. Run any real suite with
   `CRATONVM_DBG_LAYOUT_ALIAS=1` and grep `direction=undeclared`; a row under
   `class=<unresolved:ClassId(0)>` names a taken arm, with the Java frames that
   reached it.
3. **The two `java/lang/Thread` mirrors (§3.3).** 5 against 19, unconditional,
   published to the thread registry. Repair needs a build to settle which
   `java/lang/Thread` natives win last-write-wins.
4. **Whether the stub-narrower-than-real-class producer (§2, row 2) is live.**
   `fabricate_class` registers a stub at exactly the requested width, so a stub's
   `declared` equals whatever the *first* caller asked for. A second caller
   asking for more gets an `over` row; a second caller asking for the same width
   gets silence even though both may be narrower than the real class. That is a
   fifth species and it needs the loaded-image comparison
   `shadow_layout_diff` already computes — a different instrument, not a widening
   of this one.
5. **Preserving the pre-clamp width for the `under` direction (§5).** The change
   that would make W4-4's prescribed intersection work in that direction. Needs a
   side table with a GC-stable key.
6. **`docs/architecture/natives-over-real-jdk-classes.md` §5.** Correction
   proposed in §7, **not applied** — a lane rewriting an architecture section it
   discovered from one direction is how a document acquires a second voice.
