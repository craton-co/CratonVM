# The VM-internal door, swept: 44 classes, two gates, and four verdicts

> **RECONCILED 2026-08-12 (W7-55-record-reconciliation.md).** Section 6 Hunk A
> is **APPLIED**: `ctx.ensure_vm_internal_class(HS_LOOP_CLASS, 1);` at
> `native-builtins/src/net_phase_e.rs:16397`, immediately above the surviving
> `try_alloc_concurrent_synthetic(ctx, HS_LOOP_CLASS, 1)?` at `:16398`, commit
> `a8b5342a5`. Hunk B was **withdrawn by this record itself**; W7-14's fix is in
> the tree instead. The two hunks the status line claims are confirmed:
> `native-builtins/src/lang_class.rs:13720` and
> `classloading/src/class_manager.rs:11261`.
>
> * **Headline: CLOSED in source; section 6 carries no pending work.**
> * **Residual: STILL OPEN** — section 8's "what this record does not fix", plus
>   the optional `fabricated_origin_for_name` arm for `CratonVM$...` names,
>   which was not added.
>
> **2026-08-12 — the `CratonVM$…` arm IS now added, and it was not optional.**
> `fabricated_origin_for_name` gained an
> `is_vm_reserved_namespace_name` arm (`classloading/src/class_manager.rs`),
> routing every `CratonVM$…` name to `ClassOrigin::VmInternal`. The reason it
> stopped being optional is a **second name this sweep never saw**:
> `CratonVM$StsForkRunner` (`native-builtins/src/jdk25_concurrency.rs:860`, the
> JEP 505 `StructuredTaskScope.fork()` worker body) is minted with a bare
> `try_alloc_concurrent_synthetic` and has **no** `ensure_vm_internal_class`
> pre-mint of its own — the same door defect §6A fixed for `HttpServerLoop`, in
> a file §6A never looked at. For that name the arm is not belt-and-braces; it
> is the only thing covering it. Gate 2 checked per name and not by analogy:
> neither name appears in any table in `native-api/src/no_image_receiver.rs`,
> and `receiver_declared_by_no_supported_image` returns `false` for a name on
> none of them, so nothing re-tags either class's `run()V` — §3's "reviewed VM
> service" shape, where the door fix is necessary **and** sufficient. Not
> built, not run. Section 8's other residuals are still open; §8's R1 is
> partially discharged by `W7-26`'s widened census (2026-08-12).

> ## 2026-08-12 — THE `strict?` COLUMN IS FALSIFIED. Read §5.0 before §5.
>
> This record's headline was "45 of 45 pairs adjudicated, 45 of 45 resolved."
> **That is wrong, and it was wrong on the day it was written.** Eight rows in
> §5 carry `no` in the `strict?` column. `no` there does not mean *strict mode
> does not reach this class*; it means *nothing in this sweep's 71 vectors and
> 321 probes reached it*. Those are different statements and this record used
> the first word for the second finding.
>
> Five of the eight have since been falsified by other lanes, one at a time,
> each with a fatal `--jdk-only` witness: `javax/net/ssl/SSLSocket{In,Out}putStream`
> (all HTTPS), `Atomic*FieldUpdater$RustJvmImpl`, `java/util/function/Consumer$AndThen`,
> `java/util/ArrayDeque$Itr` + `java/util/LinkedList$Itr`, and — from this
> re-audit — `cratonvm/internal/SnapshotEnumeration`. A sixth
> (`cratonvm/synthetic/Process*`) and a seventh
> (`java/util/concurrent/CompletedFuture`) are falsified below by source and by
> the frozen kind map, not yet by a run. **One of the eight survives the
> re-audit.** A column with a 7-in-8 falsification rate was never a result.
>
> The `YES` rows are not clean either: the 11 × `cratonvm/internal/Unmodifiable*`
> row says *"refusal absorbed and warned"*, and `cratonvm/internal/UnmodifiableMap`
> is on the 2026-08-12 Phase 1 blocking set. Absorbed-and-warned was measured on
> the boot path, not on `System.getenv()`.
>
> §5.0 gives the discriminator this record should have used, applies it to every
> row, and marks each verdict **measured** or **unreached**. §5's table is kept
> verbatim underneath so the two can be diffed; **do not cite §5 without §5.0.**

**Status: two hunks APPLIED IN SOURCE, sweep COMPLETE, 2026-08-11. NOT
REBUILT.** Every number in this record was taken by running the already-built
`dev` binary at `C:/craton/CratonVM/target/release/cratonvm.exe` (built
2026-08-11 19:41), the JDK 25 image at
`C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot`, and that image's
`javap`. No claim is made that the source changes here compile or work.

Branch: `fix/jdk-only-vm-internal-door-sweep-20260811`.
Files changed: `native-builtins/src/lang_class.rs`,
`classloading/src/class_manager.rs`, and this record.

**`dev` moved under this sweep, and the table is stated against `dev` as of
`21232b4ef`, not against the branch point.** Two rows were resolved by other
lanes while the measurements were being taken —
`W7-14-fjp-common-factory-bound-by-name.md` and the `__mh_*` half of
`W7-13` — and one row this sweep would have patched is a patch `W7-14`
evaluated and rejected. §6B keeps the withdrawal, because the mistake is
cheaper to read than to repeat. Every "unfixed" verdict below was re-checked
against `dev`'s tree, not this branch's.

`W7-12-strict-annotation-proxy.md` and `W7-13-strict-mh-insert-wrapper.md`
each found one instance of a species — a shape the VM invents, minted through
the door that means "a class whose real bytes should have been found" — and
each fixed its own. This record applies W7-12's two out-of-file hunks and then
asks how many more instances there are.

The answer is not the one the two records imply, and the correction is the
useful part of this file.

---

## 1. Were the two recorded hunks still needed?

**Yes, both, verified against the source rather than against the record.**
This campaign has fifteen records claiming a patch was never applied when it
was in the tree, so each was checked before being written:

| hunk | file | state before this branch |
|---|---|---|
| 1 — `ensure_vm_internal_class` pre-mint | `native-builtins/src/lang_class.rs`, `create_annotation_proxy_with_type` | **absent**; the function opened directly on `try_alloc_concurrent_synthetic` |
| 2 — name arm in `fabricated_origin_for_name` | `classloading/src/class_manager.rs` | **absent**; the function had `is_vm_proxy_supertype_name` and the three generated-name families, nothing else |

Both are now applied. Confirmed live on the shipped binary before touching
anything — a `--jdk-only` `RReflect` run still emits the row W7-12 quotes,
byte for byte:

```json
{"kind":"compatibility-class-requested",
 "class":"java/lang/annotation/AnnotationProxy",
 "initiating_loader":"bootstrap",
 "requester":"native-builtins\\src\\lang_class.rs:13685", …}
```

**One disagreement with W7-12, and it is about which hunk is load-bearing.**
W7-12 calls hunk 1 "the authoritative one" and hunk 2 "belt-and-braces".
Mechanically it is the other way round, or rather: **either one alone is
sufficient**, because both of `ClassManager::fabricate_class`'s refusals are
gated on `origin.is_compatibility_stub()` and `fabricated_origin_for_name` is
the sole supplier of that origin on the `try_ensure_synthetic_class` path.
`ensure_generated_class`'s own doc says so in terms: *"Both of
[`fabricate_class`]'s refusals are gated on `origin.is_compatibility_stub()`."*
So hunk 2 alone would clear the refusal even with hunk 1 absent. Both are kept
anyway, for the reason W7-12 gives for hunk 2 and which is right: hunk 1 names
the shape where it is minted, hunk 2 stops a second minting route re-acquiring
the wrong label. That is the pairing `Proxy$Instance` already has. The
redundancy is deliberate; the claim that one of them is optional is not.

## 2. On the name-keyed classifier — accepted, and sharpened

Hunk 2 classifies by string, which collides with this campaign's most
expensive rule. **I accept W7-12's argument.** I do not accept it in the form
W7-12 states it, because that form is weaker than the facts allow, and a
weaker argument is what gets "simplified away" later.

W7-12's version is that the six *never bind by name* defects were all dispatch
or identity decisions and this is a classification. True, and insufficient on
its own — a classification that picks the wrong class is still wrong. The two
statements that actually close it are structural:

1. **There is no second class to confuse this with, at the moment the
   function runs.** `fabricated_origin_for_name` is reached only from
   `try_ensure_synthetic_class` → `fabricate_class`, and by that point
   `get_loaded_class_id(name)` has answered `None` *and*
   `find_class_bytes_delegated(name)` has failed. Every one of the six defects
   was a decision taken while two live same-named classes existed. This one is
   taken only when none does.
2. **No other party may occupy the name.** `java/lang/annotation/` is a package
   no non-bootstrap loader is permitted to define into, and the bootstrap
   loader defines what the image declares — which, per `javap
   java.lang.annotation.AnnotationProxy` → *class not found*, is not this. The
   string is not a guess at an identity; in that namespace it *is* the
   identity.

**And the cost, which W7-12 does not state.** `fabricate_class` runs its
ambiguity gate (`classify_loaded_name` → `ambiguous_stand_in_refused`) **only**
for compatibility-stub origins. A name routed to `VmInternal` therefore skips
it. That is deliberate — the gate's own comment scopes it to stand-ins because
`ensure_generated_class` mints "under a name it just constructed" — but it is a
real semantic difference, and it is inert here only because of point 2. Both
points and this cost are written onto `is_vm_annotation_carrier_name` in
`classloading/src/class_manager.rs`, where the next reader will go looking for
the smell, with the standing instruction that **any future entry beside it must
be able to make the same statement**.

## 3. What the sweep actually found: the species has two gates, not one

Both source records frame this as one bit — the class's origin. It is two, and
a fix that moves one moves the symptom rather than removing it.

| gate | what refuses | what clears it |
|---|---|---|
| **1 — the class** | `try_ensure_synthetic_class` stamps `ClassOrigin::CompatibilityStub`; `fabricate_class` refuses it under `JdkOnly` | `ensure_vm_internal_class` at the mint, or a `fabricated_origin_for_name` arm |
| **2 — its natives** | `NativeMethodRegistry::register` re-tags the receiver's natives `SyntheticStub` (`receiver_declared_by_no_supported_image`, `native-api/src/no_image_receiver.rs`) and the `JdkOnly` arm then returns without inserting | membership in `VM_SERVICE_RECEIVERS`, or never being on the tables at all |

Gate 2 is invisible from the mint site and is not touched by the door. A
concurrent lane measured the consequence directly on
`cratonvm/internal/LinkedListSnapshotListItr`: clearing gate 1 alone moved the
failure from `NoClassDefFoundError` at the mint to **`UnsatisfiedLinkError` at
the first `hasNext()`**. `no_image_receiver.rs` already says this in its own
words — *"Drop the natives without closing that, and strict mode mints the
object and then cannot dispatch on it"* — and keeps `STRICT_STILL_FABRICATES`
as an empty table precisely so the shape stays visible.

So the door question is decided by **what the class carries**, and the
instrument for that is `--dump-native-registry` taken once **per mode**, not
once:

* **A data carrier** — zero natives under its own name in either mode; the
  native that minted it reads its slots positionally. Gate 2 is vacuous, so
  the door is the whole defect and flipping it is complete.
  `java/lang/reflect/Proxy$Instance`, the ten `__mh_*` combinator carriers,
  `java/lang/annotation/AnnotationProxy`.
* **A behaviour carrier** — natives registered on it, which strict mode drops.
  `CompatibilityStub` is the **correct** label: the class exists to carry an
  implementation strict mode is deliberately retiring, and both halves are
  refused together on purpose. What such a site needs is not a door but a
  **fallback to real JDK bytecode at the refusal** — the pattern
  `craton_alloc_system_logger` and `native_ksv_iterator` already implement.
* **A reviewed VM service** — natives kept `Bridge` and therefore *present in
  strict*, while the class mint still goes through the compatibility door.
  Gate 2 is already open and gate 1 alone refuses. This is the one shape where
  the door fix is both necessary and sufficient, and the sweep found one
  unfixed instance of it (§5, `CratonVM$HttpServerLoop`).

## 4. The instrument, and what it did and did not measure

`--jdk-only-report` emits one `compatibility-class-requested` row per refusal
carrying `requester` — the Rust call site, which is what makes this tractable
at all, since most of these fabrications happen inside a native with no Java
frame. Runs:

```sh
# 71 regression-suite vectors, strict
cratonvm --jdk-only --jdk-only-report <F> --java-home <JDK25> -cp regression-suite/build <V>
# the same 71, Compatible — the WIDER net: execution continues past a mint that
# strict kills, so it reaches sites strict never gets to
cratonvm --real-jdk --jdk-only-report <F> …
# 321 of the 409 `probes/*.java` (67 do not compile standalone; 21 hit the
# per-run timeout), Compatible
```

**465 runs, 6,284 raw `compatibility-class-requested` rows, 44 distinct
classes, 45 distinct (class, `requester`) pairs — 22 of the 44 reachable under
`--jdk-only`. All 45 pairs adjudicated; 45 of 45 resolved.**
**WITHDRAWN 2026-08-12 — "45 of 45 resolved" is false; see the header block and
§5.0.** 45 of 45 pairs were *given a verdict*; six of those verdicts were the
corpus's reach reported as the class's property, and have since been falsified
one at a time. Adjudicated ≠ resolved, and this sentence used the second word.
The count is of rows actually adjudicated, not of grep hits: a
grep for `try_alloc_concurrent_synthetic` over the workspace returns thousands
of call sites and would have answered a different question badly, which is the
error the campaign README records being made in this direction repeatedly.

**Two scope limits, stated rather than papered over:**

* **Only one of the two fabrication choke points was ever reached.** Every one
  of the 6,284 rows carries the `ENSURE_SYNTHETIC_STUB_REASON` text. The other
  choke point — `create_synthetic_stub` on the `load_class` chain, with
  `NATIVE_BACKED_STUB_REASON` / `ENTERPRISE_PREFIX_STUB_REASON` /
  `MISSING_CLASS_FILE_STUB_REASON` — produced **zero** rows in this corpus. Its
  population is unmeasured here, not empty.
* **The binary predates `W7-13`.** Its fix (`alloc_mh_carrier`) is in this
  worktree's `lang_invoke.rs` but not in the running binary, which is why
  `__mh_*` rows still appear below. That is a feature for this sweep — it keeps
  the ten carriers visible as the control group whose verdict is already
  settled.

## 5.0 The re-audit: the discriminator this sweep should have used

**Read this before §5.** Added 2026-08-12 by the lane that owns this record,
after a sibling lane falsified the `SSLSocket*Stream` row and the campaign's
33-probe reachability screen falsified three more.

### What went wrong

§4 states the corpus honestly — 71 vectors, 321 probes, one of two choke
points — and then §5's `strict?` column silently converts *"no row appeared"*
into *"strict mode is fine here"*. Nothing bridged the two, and the four-way
verdict column inherited the error: sixteen rows read **"door correct; none"**
on the strength of a run that never called the method.

This is the campaign's own recurring shape — a narrow probe reports its own
reach — and this record is the largest instance of it in the directory.

### The discriminator, which is a two-term predicate over data already frozen

A fabricated-receiver mint is a **live** `--jdk-only` blocker iff **both**
hold, and neither term needs a run:

1. **The receiver is refused.** Its name is on `NO_IMAGE_JDK_RECEIVERS` or
   `VM_MINTED_STAND_IN_RECEIVERS` (`native-api/src/no_image_receiver.rs`), so
   the class is `CompatibilityStub` at the door **and** its own natives are
   re-tagged `SyntheticStub` and dropped. Gate 1 and gate 2 both shut.
2. **The minting native survives.** The native that performs the mint is
   `bridge` — so `NativeKind::allowed_in(JdkOnly)` keeps it, it runs in strict,
   and it asks for a class §5 forbids. This is Phase 1's shape exactly: *the
   refusal is correct; the survival of its caller is the defect.*

And a third term decides how loud the failure is:

3. **The mint site has no `Err(_)` arm.** A bare `try_alloc_*(…)?` propagates
   the refusal as `NoClassDefFoundError` at the application's call site; a
   `match` with a real-JDK fallback absorbs it. §8's "refusal laundered into a
   wrong answer" is the third case and the worst.

**Term 2 is the one this sweep never evaluated, and it is free.**
`scripts/baselines/jdk-only-kind-map-25-linux.tsv` is a frozen per-registration
census whose unit is one `(class, name, descriptor, ordinal)` triple with its
adjudicated `kind`. Every verdict in §5.0 below is a lookup in that file plus a
read of the mint site's `?`-vs-`match`. **It is not a run**, and it is stated
as such: a `bridge` minting native proves the mint is *reachable* in strict, not
that any workload reaches it.

Two secondary findings from the same instrument, both worth recording:

* `sun/misc/Cleaner` and `jdk/internal/logger/AbstractLoggerFinder` are in
  `NO_IMAGE_JDK_RECEIVERS` and have **zero rows in the kind map** — no
  registration exists on either name in either mode. They are inert table
  entries, not live fabrications; the predicate answers `true` for a name
  nothing ever asks about. Confirmed here, not fixed: they cost nothing and
  their image fact is still true, so the gate script should keep checking it.
* A pair of siblings can be split by term 2 alone. `Function.andThen` /
  `.compose` are `synthetic-stub` and `Consumer.andThen` is `bridge`, which is
  the entire reason `Function$AndThen` is dead in strict and `Consumer$AndThen`
  is a blocker — three names that §5 puts on one row with one verdict.

### The re-audit, row by row

`survives?` is the minting native's frozen kind. `fallback?` is `Err(_)` arm
present at the mint. **basis** is the point of the table: whether the verdict
rests on a measurement of the class, or on nothing having reached it.

| §5 row | minting native · frozen kind | fallback? | corrected strict verdict | basis |
|---|---|---|---|---|
| `AnnotationProxy` | `lang_class.rs` · 0 natives on the class | n/a | YES — fixed here (door) | **measured** — the `compatibility-class-requested` row is quoted in §1 |
| 10 × `__mh_*` | `lang_invoke.rs` · 0 natives on the class | n/a | YES — fixed by `W7-13` | **measured** |
| `CratonVM$HttpServerLoop` | `HttpServer.start` · **bridge** | no | YES, fatal — fixed (door) | **measured** — `NoClassDefFoundError` witness, §6A |
| `ForkJoinPool$DefaultCommonPool…` | `bridge` | n/a | YES, fatal — fixed by `W7-14` | **measured** |
| `cratonvm/internal/SystemLogger` | VM service, natives kept `bridge` | yes | correct | **measured** |
| `cratonvm/stream/LazyOp` | — | — | correct, deliberate | **measured** |
| `HashMap$KeyItr` | `HashSet.iterator`, `HashMap.keySet` · **bridge** | yes (landed after this binary) | YES — was fatal, now absorbed | **measured** — `RChmKeySetView` |
| `TreeSet$Itr` | `TreeSet.iterator`/`descendingIterator` · **bridge** | yes | YES — absorbed | **measured** |
| `IteratorEnumeration` | `*KeyStore.engineAliases` · **bridge** | yes (`keystore.rs:2666` is a `match`) | YES — absorbed | **measured** |
| 11 × `Unmodifiable*` | boot path, **and** `System.getenv()Ljava/util/Map;` · **bridge** | boot arm warns; the `getenv` arm is another lane's | YES — **`UnmodifiableMap` is on the 2026-08-12 blocking set** | **PARTLY UNREACHED** — "absorbed and warned" was measured on the boot path only |
| `Comparator$Native` | `Comparator.naturalOrder`/`comparing*` · **synthetic-stub** | n/a | not reached in strict — real bytecode serves | **measured**, and now *structurally* so |
| `Enumeration$Impl` | boot path | absorbed | correct | **measured** |
| `ArrayListSubList` | `ArrayList.subList` · **synthetic-stub** | n/a | dropped before the mint | **structural** — term 2 fails, so unreachable by construction |
| `StreamCollector`, `StreamChainCollector` | collection `stream()` · **bridge** | **yes** (`lib.rs:17846` is a `match`) | absorbed | **measured** |
| **`SnapshotEnumeration`** | `Properties.propertyNames`/`keys`/`elements`, `ConcurrentHashMap.keys`/`elements`, `Hashtable.keys`/`elements` · **bridge** | **no** — `make_snapshot_enumeration` is a bare `?` | **YES, fatal — NEW, §5.1** | was **unreached** |
| **`cratonvm/synthetic/Process*`** | `ProcessBuilder.start` · synthetic-stub **but `Runtime.exec` ×6 · bridge** | **no** — `refused_class(…)?` | **YES, fatal — NEW, §5.1** | was **unreached** |
| `ArrayDeque$Itr`, `LinkedList$Itr` | `.iterator()` · **bridge** | **yes**, both, landed by `W7-16` | was fatal; now absorbed | was **unreached** — the source comments say both died before `hasNext()` |
| **`CompletedFuture`** | `AsynchronousFileChannel.read`/`write`/`lock` · **bridge** | **no** — `wrap_completed_future` is a bare `?` | **YES, fatal — NEW, §5.1** | was **unreached** |
| 3 × `Atomic*FieldUpdater$RustJvmImpl` | `*FieldUpdater.newUpdater` · **bridge** | no | **YES, fatal** — on the blocking set | was **unreached** |
| `Function$AndThen`, `Function$Compose` | `Function.andThen`/`.compose` · **synthetic-stub** | n/a | dropped before the mint | **structural** |
| `Consumer$AndThen` | `Consumer.andThen` · **bridge** | no | **YES, fatal** — on the blocking set | was **unreached**; §5 put it on the `Function$*` row and it does not belong there |
| `LogManager$StringEnumeration` | `LogManager.getLoggerNames` · **synthetic-stub** | n/a | dropped before the mint | **structural** |
| `SSLSocketInputStream`, `SSLSocketOutputStream` | `SSLSocket.getInputStream`/`getOutputStream` · **bridge** | no | **YES, fatal — all HTTPS.** Fixed 2026-08-12 by re-targeting the receiver | was **unreached** — falsified by a sibling lane |

**Score: of 22 rows, 6 were `unreached` and stated as if measured, 1 is partly
so, and 3 are structural (right answer, wrong reason).** The three structural
rows are worth separating from the measured ones: `ArrayListSubList`,
`Function$AndThen`/`$Compose` and `LogManager$StringEnumeration` really are dead
in strict, but not for the reason §5 gives ("door correct"). They are dead
because their *minting native* is dropped first. That is a stronger guarantee
than the one §5 claims, and it is the one that would survive somebody re-tagging
a class.

### The correction to §3's taxonomy

§3 gives three carrier kinds and two repairs — the door, or a real-JDK fallback.
The `SSLSocket*Stream` fix is neither, and it is the best of the three:

> **4 — a wrong receiver NAME.** The carrier is a behaviour carrier, its natives
> are the whole of its behaviour and must survive, and the fix is to mint them
> on **the class the real JDK returns** instead of on an invented name. Gate 1
> stops refusing because the class is real; gate 2 stops firing because
> `receiver_declared_by_no_supported_image` no longer matches; and
> `getClass().getName()` starts agreeing with HotSpot, which neither of the
> other two repairs buys. Landed for `SSLSocketImpl$AppInputStream` /
> `$AppOutputStream` (`native-builtins/src/phases_late/ssl_security.rs`,
> another lane's file, working tree, uncommitted, not built).

It costs a layout audit — a real class has declared fields, so the native's
private slot must be **appended**, not written at slot 0 — which is why it is
fourth and not first. `alloc_tls_stream` uses `try_alloc_with_appended_slots`
for exactly that reason.

### 5.1 Three blockers the 33-probe screen did not reach

All three satisfy terms 1–3: refused receiver, `bridge` minting native, bare
`?` at the mint. **Read from source and from the frozen kind map; no run.**
Each needs a probe, and the probe is the deliverable, not the fix.

**N1 — `cratonvm/synthetic/Process*` via `Runtime.exec`.** A prior lane
re-tagged `java/lang/ProcessBuilder.start` `SyntheticStub` **explicitly to stop
a fabricated `cratonvm/synthetic/Process` escaping into `--jdk-only`**
(`native-io/src/process.rs`, the `register_with_kind` comment says so in as many
words). It pinned one of two spawn routes. All six
`java/lang/Runtime.exec` overloads (`native-builtins/src/lib.rs`, bare
`registry.register`, ambient kind **bridge** in the frozen map) call
`native-builtins/src/lang_system.rs::runtime_spawn_process` →
`native_io::process::spawn_and_wrap` → `spawn_and_wrap_with_redirects`, whose
mint is `crate::refused_class(ctx, SYNTHETIC_PROCESS_CLASS, PROC_FIELD_COUNT)?`
— the same `?`, in the same function, that `ProcessBuilder.start` no longer
reaches. Predicted witness: `Runtime.getRuntime().exec("…")` under `--jdk-only`
→ `NoClassDefFoundError: cratonvm/synthetic/Process`. This is the
"a fix that only pins the positive half" shape, and the half it left open is the
older API.

**N2 — `java/util/concurrent/CompletedFuture` via `AsynchronousFileChannel`.**
`read(ByteBuffer,J)Ljava/util/concurrent/Future;`, its `write` twin and `lock()`
are all **bridge**; every one of their exit points goes through
`native-io/src/lib.rs::wrap_completed_future`, which is
`try_alloc_synthetic(ctx, "java/util/concurrent/CompletedFuture", 2)?` with no
`Err` arm, on a name that is in `NO_IMAGE_JDK_RECEIVERS`. Predicted witness:
any `AsynchronousFileChannel.read/write` under `--jdk-only`. **This one is not
hypothetical for the app corpus** — the comment two lines above that helper
names H2's `FileAsync.write` and `TestFileSystem.testConcurrent` against the
`async:` filesystem as the caller it was hardened for.

**N3 — `cratonvm/internal/SnapshotEnumeration` via the legacy `Enumeration`
getters.** `java/util/Properties.propertyNames()`, `.keys()`, `.elements()` and
`java/util/concurrent/ConcurrentHashMap.keys()`, `.elements()` are all
**bridge**, and all route to
`native-collections/src/lib.rs::make_snapshot_enumeration`, whose mint is a bare
`try_alloc_synthetic(ctx, "cratonvm/internal/SnapshotEnumeration", 2)?` on a
name in `VM_MINTED_STAND_IN_RECEIVERS`. Predicted witness:
`System.getProperties().propertyNames()` under `--jdk-only`. This is the widest
of the three by call-site count — it is the JDBC/logging idiom.
**One caveat, stated rather than assumed:** `java/util/Hashtable.keys` has
**two** registrations in the frozen map (both `bridge`), so which registrar owns
the slot under last-write-wins is unresolved here and the `Hashtable` half of
this claim is *unadjudicated*. `Properties` and `ConcurrentHashMap` were traced
to source and are not.

### What a future sweep must do differently

1. **Never write `no` in a reachability column.** Write `not reached by <this
   corpus>`. The two rows this record lost were both lost to that word.
2. **Take term 2 before taking the corpus.** The kind map is frozen, free, and
   splits every candidate into *structurally dead* and *live and waiting for a
   probe* before a single vector runs. Every one of §5.1's three would have
   fallen out of a `join` between `NO_IMAGE_JDK_RECEIVERS` and that file.
3. **A retag that pins one caller must enumerate the callers.** N1 exists
   because `ProcessBuilder.start` and `Runtime.exec` share a mint and only one
   was adjudicated.

---

## 5. The table

**Superseded in its `strict?` and `verdict` columns by §5.0.** Kept verbatim.

`javap` column: run against the JDK 25 image on this host. **Every one of the
44 answered "class not found" — there is not a single genuinely-missing real
JDK class in this species.** Two are near-misses worth naming, because the
name the VM chose is a *stale* JDK name rather than an invented one, and that
changes the fix completely.

`nat` column: natives registered under the class's own name,
`--dump-native-registry` on a boot in each mode: `compat → strict`.

| class | requester (owning file · fn) | strict? | nat | verdict | action |
|---|---|---|---|---|---|
| `java/lang/annotation/AnnotationProxy` | `native-builtins/src/lang_class.rs` · `create_annotation_proxy_with_type` | YES | 0 → 0 | **VM-internal, wrong door** | **FIXED here** |
| `__mh_insert_wrapper__` + 9 siblings | `native-builtins/src/lang_invoke.rs` · 11 sites | YES | 0 → 0 | VM-internal, wrong door | already fixed (`W7-13`), binary predates it |
| `CratonVM$HttpServerLoop` | `native-builtins/src/net_phase_e.rs` · `re10_spawn_dispatcher` | **YES, fatal** | 1 bridge → **1 bridge** | **VM-internal, wrong door — gate 2 already open** | **§6 hunk A** |
| `java/util/concurrent/ForkJoinPool$DefaultCommonPoolForkJoinWorkerThreadFactory` | `native-builtins/src/phases_late/concurrent.rs` · `resolve_common_factory_internal_name` / `alloc_common_factory` | **YES, fatal** | 0 → 0 | **stale JDK name — NOT a door defect** | strict half already FIXED on `dev` by `W7-14`; §6B corroborates its open `Compatible` half |
| `cratonvm/internal/SystemLogger` | `native-builtins/src/lib.rs` · `craton_alloc_system_logger` | YES | 10 bridge → **10 bridge** | already correct — refusal has a measured real-JDK fallback | none |
| `cratonvm/stream/LazyOp` | `native-collections/src/lib.rs` · `stream_make_lazy_derived` | YES | 0 → 0 | already correct — **deliberately** not laundered | none |
| `java/util/HashMap$KeyItr` (×2 sites) | `native-collections/src/lib.rs` · `native_hs_iterator`, `native_ksv_iterator` | YES | 3 → 0 | behaviour carrier; door correct | none — fallback landed on `dev` after this binary |
| `java/util/TreeSet$Itr` | `native-collections/src/lib.rs` · `native_ts_iterator`, `native_ts_descending_iterator` | YES | 3 → 0 | behaviour carrier; door correct, fallback present | none |
| `java/util/IteratorEnumeration` | `native-builtins/src/keystore.rs` · `engine_aliases` | YES | 2 → 0 | behaviour carrier; door correct, fallback present | none |
| 11 × `cratonvm/internal/Unmodifiable*` | `vm/src/vm/vm_init.rs` · `ensure_bootstrap_compat_class` — **and a second requester this sweep did not fold: `java/lang/System.getenv()Ljava/util/Map;`, kind `bridge`** | YES | 3–49 → 0 | behaviour carriers; door correct, refusal absorbed and warned **on the boot path only — `UnmodifiableMap` is on the 2026-08-12 Phase 1 blocking set; §5.0** | none — another lane's |
| `java/util/Comparator$Native`, `java/util/Enumeration$Impl` | same | YES | 2 / 4 → 0 | same | none |
| `cratonvm/internal/ArrayListSubList`, `StreamCollector`, `StreamChainCollector` | `native-collections/src/lib.rs` | no (subList dropped; the `stream()` sites have `Err` arms) | 1–30 → 0 | door correct | none |
| `cratonvm/internal/SnapshotEnumeration` — **split off this row by §5.0** | `native-collections/src/lib.rs` · `make_snapshot_enumeration` | ~~no~~ **YES, fatal (N1/N3)** | 2 → 0 | ~~door correct~~ **FALSIFIED — `Properties.propertyNames`/`CHM.keys` are `bridge`, mint is a bare `?`; §5.1** | NOMINATED |
| `cratonvm/synthetic/Process`, `ProcessPipeInputStream`, `ProcessPipeOutputStream` | `native-io/src/process.rs` · `spawn_and_wrap_with_redirects` | ~~no~~ **YES, fatal via `Runtime.exec`** | 5–13 → 0 | ~~door correct~~ **FALSIFIED — `ProcessBuilder.start` was retagged, `Runtime.exec` ×6 was not; §5.1 N1** | NOMINATED |
| `java/util/ArrayDeque$Itr`, `java/util/LinkedList$Itr` | `native-collections/src/lib.rs` · `native_ad_iterator`, `native_ll_iterator` | ~~no~~ **was YES, fatal** | 3 → 0 | ~~door correct~~ **FALSIFIED — both mint sites' own comments record a `NoClassDefFoundError` before `hasNext()`** | fallbacks landed by `W7-16` |
| `java/util/concurrent/CompletedFuture` | `native-io/src/lib.rs` · `wrap_completed_future` | ~~no~~ **YES, fatal** | 5 → 0 | ~~door correct~~ **FALSIFIED — `AsynchronousFileChannel.read`/`write` are `bridge`, mint is a bare `?`; §5.1 N2** | NOMINATED |
| 3 × `Atomic*FieldUpdater$RustJvmImpl` | `native-builtins/src/atomic_updater.rs` | ~~no~~ **YES, fatal** | 8–12 → 0 | ~~door correct~~ **FALSIFIED — `newUpdater` is `bridge`; §5.0** | none — another lane's |
| `Function$AndThen`, `Function$Compose` (`.andThen`/`.compose` are `synthetic-stub`) | `native-builtins/src/phases_late/streams.rs` | no — **structurally**, the minting native is dropped first | 1 → 0 | door correct, for a reason §5 did not give | none |
| `Consumer$AndThen` — **split off this row by §5.0** | same | ~~no~~ **YES, fatal** | 1 → 0 | ~~door correct~~ **FALSIFIED — `Consumer.andThen` is `bridge`; §5.0** | none — another lane's |
| `java/util/logging/LogManager$StringEnumeration` | `native-builtins/src/logmanager.rs` | no | 2 → 0 | behaviour carrier; door correct | none |
| `javax/net/ssl/SSLSocketInputStream`, `SSLSocketOutputStream` | `native-builtins/src/phases_late/ssl_security.rs` | ~~no~~ **YES, fatal** | 4 → 0 | ~~behaviour carriers; door correct~~ **FALSIFIED — see §5.0** | ~~none~~ receiver re-targeted, 2026-08-12 |

**Why "no" in the strict column is a result, not a gap.** `RJdkLambdas`,
`RJdkProcess` and `RJdkCollections` all **PASS** under `--jdk-only` while
minting `Function$AndThen`, `cratonvm/synthetic/Process`, `ArrayListSubList`
and `StreamCollector` under `--real-jdk`. In strict the minting natives are
themselves dropped, the real JDK bytecode serves, and the mint is never
reached. That is the behaviour-carrier verdict working end to end, and it is
the direct evidence that opening those doors would be wrong: it would put a
class back that nothing in strict mode has an implementation for.

### The two near-misses

`java/util/HashMap$KeyItr` stands where the real image declares
`HashMap$KeyIterator`, and `ForkJoinPool$DefaultCommonPoolForkJoinWorkerThreadFactory`
where JDK 25 declares `ForkJoinPool$DefaultForkJoinWorkerThreadFactory`. The
first is a deliberate rename (a snapshot iterator is genuinely not the real
class, and `native-collections` says so in place). The second is not — see §6
hunk B.

## 6. Out-of-file patch (not applied)

Not built, not run. Both hunks are in files this lane does not own.

### Hunk A — `native-builtins/src/net_phase_e.rs`, `re10_spawn_dispatcher`

**The sweep's one unfixed door defect, and it is fatal in strict mode.**
Still unfixed on `dev` at `21232b4ef`: `git show dev:native-builtins/src/net_phase_e.rs`
has the bare `try_alloc_concurrent_synthetic(ctx, HS_LOOP_CLASS, 1)?` and no
`ensure_vm_internal_class` anywhere in the file. Measured on the shipped
binary:

```
$ cratonvm --jdk-only --java-home <JDK25> -cp <probes> HttpServerWildcardAddressProbe
Exception in thread "main" java/lang/NoClassDefFoundError: CratonVM$HttpServerLoop
	at HttpServerWildcardAddressProbe.main(…:49)
```

`com.sun.net.httpserver.HttpServer.start()` cannot start under `--jdk-only`.
The class is a 1-slot `Runnable` holding a `server_id`, minted four times (one
per `HS_DISPATCHER_POOL` dispatcher). `javap CratonVM$HttpServerLoop` → class
not found; the name is not even in a JDK namespace.

**Both gates checked, per class, not by analogy.** `--dump-native-registry`
reports its single `run()V` registration as `bridge` in Compatible **and under
`--jdk-only`** — it is on none of `no_image_receiver.rs`'s tables, so nothing
re-tags it. Gate 2 is already open; gate 1 alone is refusing. The door fix is
therefore necessary *and* sufficient here, which is not true of most of the
table.

```rust
    for idx in 0..HS_DISPATCHER_POOL {
        // W7-17 — `ensure_vm_internal_class`, not the compatibility door.
        // `CratonVM$HttpServerLoop` is a 1-slot Runnable this VM invents to
        // carry `server_id` between here and `re10_serve_loop_run`; `javap`
        // against the JDK 25 image answers "class not found", the name is in
        // no JDK namespace, and no class file can ever back it — contract §1
        // item 6's shape. Through the compatibility door it looked like a §5
        // stand-in and `--jdk-only` refused it, taking
        // `HttpServer.start()` with it (`NoClassDefFoundError:
        // CratonVM$HttpServerLoop`, HttpServerWildcardAddressProbe).
        //
        // The natives are the OTHER gate and are already open here:
        // `--dump-native-registry` reports this class's one `run()V` as
        // `bridge` in Compatible AND under `--jdk-only` — the name is on none
        // of `no_image_receiver.rs`'s tables, so nothing re-tags it
        // `SyntheticStub`. That check is per class: flipping the door on a
        // receiver whose natives ARE dropped in strict buys an
        // `UnsatisfiedLinkError` at the first call instead of a
        // `NoClassDefFoundError` at the mint.
        //
        // Pre-mint, so the allocation below keeps its field-count widening and
        // GC-safe retry: `fabricate_class` returns the existing `ClassId` for
        // an already-loaded name, so `Compatible` is byte-for-byte unchanged
        // and only the recorded ORIGIN moves.
        ctx.ensure_vm_internal_class(HS_LOOP_CLASS, 1);
        let runner = try_alloc_concurrent_synthetic(ctx, HS_LOOP_CLASS, 1)?;
```

The `try_alloc_concurrent_synthetic(ctx, "java/lang/Thread", …)` two lines
below is **not** part of this and must not be touched: `java/lang/Thread` has
real bytes, `fabricate_class` loads them, and no row is ever emitted for it.

Optionally pair with a `fabricated_origin_for_name` arm, as `Proxy$Instance`
and (now) `AnnotationProxy` have — but only if the §2 argument can be made for
this name, and it can: `CratonVM$…` is this VM's reserved namespace and no
loader defines into it.

**APPLIED 2026-08-12, and "optionally" was wrong.** The arm is
`is_vm_reserved_namespace_name` in `classloading/src/class_manager.rs`, keyed
on the `CratonVM$` prefix, carrying the §2 argument in full — including the
one statement that changes shape (`java/lang/annotation/` is closed by the
JVM's package rules; `CratonVM$` is closed by convention, so the standing
instruction is that anything added under the prefix must be a carrier the VM
invents, never a stand-in for bytes some image declares). It is not
belt-and-braces because a **second** mint exists that this sweep's corpus
never reached: `CratonVM$StsForkRunner`, at
`native-builtins/src/jdk25_concurrency.rs:860`, with a bare
`try_alloc_concurrent_synthetic` and no pre-mint. §4's own scope limit
predicted this — the corpus reached one of the two fabrication choke points,
and `StructuredTaskScope.fork()` is not exercised by any of the 71 vectors or
321 probes it ran. The prefix arm covers both names and any future one from
the one place that cannot be forgotten at a new mint site.

### B — `ForkJoinPool$DefaultCommonPool…`: **no patch. `dev` got there first.**

**This lane's proposed patch is WITHDRAWN, and the withdrawal is the useful
record.** The sweep reached this row independently and drafted the obvious fix
— swap the literal for the name JDK 25 declares. `W7-14-fjp-common-factory-bound-by-name.md`
landed on `dev` while this sweep was running, diagnosed the same row, and
**explicitly rejects that patch**: swapping the string *"would be correct today
and would rot at the next release exactly as this one did, silently, because
nothing in the tree tests the answer"*. Its fix reads the specified
`public static final ForkJoinPool.defaultForkJoinWorkerThreadFactory` out of
the image instead, from `alloc_common_factory`'s `Err(_)` arm so `Compatible`
is untouched by construction — which also reproduces HotSpot's *reference*
identity, not merely its class identity. That is a better fix than the one this
lane drafted, and it is already in the tree.

Two things this sweep still contributes to that row, neither of them a patch:

**1. The door verdict, which W7-14 does not state and which its fix depends
on.** This name is the sweep's only `0 → 0` class that is *not* a door defect.
Both gates are vacuous — zero natives registered under it in either mode — so
`ensure_vm_internal_class` would "work" here in the sense of removing the
refusal, and would have been the wrong move: it would have made an answer the
image contradicts permanent. **A class passing the `javap` guardrail is
necessary and not sufficient.** The guardrail asks whether a class file must
exist for this name; it does not ask whether the VM should have been asking for
this name at all. This row is the one instance in 42 where those two questions
gave different answers.

**2. Independent corroboration of W7-14's still-open half.** That record leaves
`Compatible` mode deliberately unchanged and flags it as a human's call, noting
it is *"measurably wrong here, and nothing tests it"*. Measured again here, on
the same binary, from a different probe, agreeing exactly — and with one
observation W7-14 does not record:

```
HotSpot 25   java.util.concurrent.ForkJoinPool$DefaultForkJoinWorkerThreadFactory
             Class.forName("…$DefaultCommonPoolForkJoinWorkerThreadFactory")
                 -> ClassNotFoundException

CratonVM --real-jdk
             …$DefaultCommonPoolForkJoinWorkerThreadFactory        <- WRONG NAME
             Class.forName on it SUCCEEDS                          <- the
                                                                      fabrication
                                                                      is visible
                                                                      to reflection

CratonVM --jdk-only
             NoClassDefFoundError: …$DefaultCommonPoolForkJoinWorkerThreadFactory
             (RJdkForkJoin, parallelStreams, RJdkForkJoin.java:209)
```

The third line is the part worth adding: it is not only that `getFactory()`
answers a wrong name, it is that `Class.forName` on that name **succeeds** in
`Compatible` mode, so an application probing for the class the way HotSpot code
does gets `true` where HotSpot raises `ClassNotFoundException`. A fabrication
that is reachable by name from application reflection is a larger surface than
a wrong `getName()`, and it strengthens W7-14's argument for closing the
`Compatible` half.

The drafted patch was a one-literal swap in
`resolve_common_factory_internal_name` — `"…$DefaultCommonPoolForkJoinWorkerThreadFactory"`
→ `"…$DefaultForkJoinWorkerThreadFactory"` — so that
`alloc_common_factory`'s `ensure_class_initialized` arm hits and no
fabrication happens in either mode. It is written down here **only** so the
next sweep that reaches this row recognises it and stops: W7-14 evaluated
exactly this and rejected it on the campaign's own rule. Do not apply it.

**The general lesson, since this lane made the mistake in full before catching
it.** `dev` moves hourly, and a sweep that runs for an hour against a fixed
binary is reading a tree that no longer exists. Two of the four rows this
sweep would have "fixed" were already resolved on `dev` by other lanes —
`W7-13` for the `__mh_*` carriers and `W7-14` for this one. **Run
`git diff origin/dev --stat` and read the `docs/known-issues/jdk-only/`
directory on `dev`, not on your branch point, before writing any patch this
sweep produces.** A grep of your own worktree cannot see the fix that landed
while you measured.

## 7. Census and ratchet movement, with direction

Measured on a Compatible `RReflect` run with `--dump-class-origins`, which is
the run this branch's one applied change moves:

```
before   compatibility-stub 14   vm-internal 3
after    compatibility-stub 13   vm-internal 4
```

* `--jdk-only-report` `counts.compatibility_classes`: **−1** (14 → 13 on that
  run). `counts.generated_classes`: **+1** (9 → 10).
* Under `--jdk-only` the row leaves `violations[]` entirely: RReflect's strict
  report held exactly one `compatibility-class-requested` row for this class,
  and it should hold none. `compatibility_classes` is already 0 there and
  stays 0; `generated_classes` goes 0 → 1.
* `--dump-class-origins` reports the class `vm-internal` instead of
  `compatibility-stub`, and `vm_exec.rs`'s `stub_hint` stops appending
  *"[class not found on any classpath entry — synthetic stub, add the missing
  jar]"* to a `NoSuchMethodError` naming it. Both are corrections: it is not a
  classpath gap.

**`BASELINE_SYNTHETIC_STUBS` / `stub_ratchet` do not move, and the reason is
measured rather than assumed.** They count `SyntheticStub`-tagged
*registrations*; `--dump-native-registry` reports **zero** natives under
`java/lang/annotation/AnnotationProxy` in either mode, so there is no
registration to count and `counts.synthetic-stub` (1,262 Compatible / 0 strict)
is untouched. Nothing here needs a baseline re-seeded, in either direction.

`classloading/tests/jdk_only_class_origin.rs::dispatch_predicate_matches_the_stub_bit`
enumerates a closed fixture list that does not include this name, so it is
unaffected. Its sibling asserting §11's zero-`CompatibilityStub` criterion moves
one class **towards** green.

### `Compatible` behaviour, per site

**Unchanged, byte for byte, at the one site changed.**
`ClassManager::fabricate_class` returns the existing `ClassId` for an
already-loaded name before it reaches `admit_compatibility_class`, so the
pre-mint makes the class exist and `try_alloc_concurrent_synthetic` below then
finds it loaded and does exactly what it does today — same `ClassId`, same four
slots, same `java/lang/Object` superclass, same
`java/lang/annotation/Annotation` superinterface (`jdk_interfaces(name)` is
applied in `fabricate_class` for **every** origin; confirmed in the census row,
which lists both supertypes). Two observable things move and neither is a
vector-level behaviour:

1. the census label and the `stub_hint` text, above;
2. `Class::dispatch_lacks_class_file()` flips `true` → `false`, because the
   `VmInternal` arm is `methods.iter().any(|m| m.is_native())` and this class
   has an empty method table — there is no `synthetic_stub_ctor_methods` arm
   for the name (grepped: zero occurrences in that function).

W7-12 read all three consumers of that predicate and found them inert for this
class. Re-checked here from the other end, which is the cheaper check: **all
28 workspace references to `java/lang/annotation/AnnotationProxy` outside
`lang_class.rs` key on the class NAME, not on the origin or the predicate** —
`dispatch_virtual.rs` (3), `typecheck.rs` (4), `invoke.rs`, `interpreter.rs`,
`vm_exec.rs` (7), `reflect_annotations.rs` (5). No read site consults the bit
this change moves.

## 8. What this record does not fix

* **R1, inherited from W7-12 and still open.** The four
  `if let Ok(Some(proxy)) = …` sites in `native-builtins/src/lang_class.rs`
  turn any `Err` from the builder into "annotation absent", including a real
  `MethodCallFailed::ExceptionThrown`. This branch removes the *refusal* that
  was being swallowed; it does not remove the swallowing. Left deliberately:
  changing five error paths in a file nothing can rebuild is how a fix becomes
  two defects.
  **CLOSED by `W7-26-getannotation-swallowed-exception.md`** (all five sites,
  source-only), whose own R1 — the loader ladders one layer down — is
  partially discharged as of 2026-08-12 with a workspace-wide census.
* **A refusal laundered into a wrong answer.** A concurrent lane found
  `collection_elements_generic` returning `Vec<Value>` with no error channel,
  so a strict `NoClassDefFoundError` out of its `iterator()` call becomes
  `Vec::new()` and `arrayList.equals(linkedList)` answers `false` under
  `--jdk-only` with no exception — while the census still reports the refusal
  as "measured". That is a second, worse failure mode than either gate, and
  the behaviour-carrier verdict in §3 depends on refusals being *loud*. Every
  fallback added under that verdict should be checked for it.
* **The unmeasured choke point.** `create_synthetic_stub` produced zero rows in
  this corpus (§4). A workload that resolves a missing class through
  `load_class` rather than through a native's allocation would exercise it, and
  none in `regression-suite/` or `probes/` does.
* **`cratonvm/internal/LinkedListSnapshotListItr`** is on
  `VM_MINTED_STAND_IN_RECEIVERS` and was adjudicated by the concurrent lane
  that supplied §3's measurement; it does not appear in this sweep's rows and
  is that lane's to close.

### 8.1 Residuals opened by the 2026-08-12 re-audit

* **R2 — the three §5.1 blockers have no probe.** N1 (`Runtime.exec` →
  `cratonvm/synthetic/Process`), N2 (`AsynchronousFileChannel` →
  `CompletedFuture`), N3 (`Properties.propertyNames` →
  `SnapshotEnumeration`) are read from source and from the frozen kind map.
  **Term 2 of §5.0's predicate proves the mint is reachable in strict; it does
  not prove any workload reaches it.** Each needs a `--jdk-only` witness with a
  HotSpot 25 control, and `probes/` is not run by `regression-suite/run.sh` at
  any `SUITE=`, so a probe alone cannot discharge them — the witness has to land
  as a `regression-suite` vector or it is unscheduled evidence.
* **R3 — this record's own evidence is unscheduled.** §9's recipe is a
  hand-run over `regression-suite/build` and `probes/`. Nothing in CI re-takes
  it, which is why the `strict?` column could rot for a day without a red.
  The frozen kind map *is* gated (`scripts/jdk-only-kind-map.py`), so the
  cheapest durable guard for this whole species is a check that **no name in
  `NO_IMAGE_JDK_RECEIVERS` ∪ `VM_MINTED_STAND_IN_RECEIVERS` is minted from a
  `bridge` native without an `Err(_)` arm** — i.e. §5.0's predicate, run as a
  gate rather than as a sweep. Not attempted here: the third term needs the mint
  sites enumerated, and §4 already records why a grep over
  `try_alloc_concurrent_synthetic` answers a different question badly.
* **R4 — `sun/misc/Cleaner` and `jdk/internal/logger/AbstractLoggerFinder`
  have zero registrations.** Confirmed against the frozen kind map (§5.0). They
  are inert rows on `NO_IMAGE_JDK_RECEIVERS`, not live fabrications. Left in
  place deliberately — the image fact is true and the gate script should keep
  checking it — but a reader counting "fabricated classes" from that table
  overcounts by two.
* **R5 — `java/util/Hashtable.keys` has two registrations, both `bridge`.**
  Which registrar owns the slot decides whether the `Hashtable` half of N3 is
  real. Unresolved; `register()` is last-write-wins and this lane did not read
  the boot order. The `Properties` and `ConcurrentHashMap` halves do not depend
  on it.

## 9. How to re-take all of this

```sh
BIN=target/release/cratonvm.exe
JDK="C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot"

# the census, per vector, in BOTH modes — Compatible is the wider net
for v in regression-suite/build/R*.class; do b=$(basename "$v" .class)
  "$BIN" --jdk-only --jdk-only-report "rep/$b.json"  --java-home "$JDK" -cp regression-suite/build "$b"
  "$BIN" --real-jdk --jdk-only-report "rep2/$b.json" --java-home "$JDK" -cp regression-suite/build "$b"
done
# then fold on (violations[].kind == "compatibility-class-requested",
#               row.class, row.requester)

# gate 2, PER CLASS and PER MODE. One dump is not enough: the whole point is
# that the kinds differ between them.
"$BIN" --dump-native-registry reg_compat.json --real-jdk --java-home "$JDK" …
"$BIN" --dump-native-registry reg_strict.json --jdk-only --java-home "$JDK" …

# the guardrail question, against the image the run used
"$JDK/bin/javap.exe" -p <dotted.name>

# and the census label this branch moves
"$BIN" --real-jdk --dump-class-origins origins.json --java-home "$JDK" \
    -cp regression-suite/build RReflect
```

`regression-suite/run.sh` supplies `--java-home` on every invocation; a
hand-run that omits it measures the host's default JDK and has inverted a
per-mode verdict before (`W7-11`).
