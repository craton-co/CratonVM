# The VM-internal door, swept: 44 classes, two gates, and four verdicts

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
`--jdk-only`. All 45 pairs adjudicated; 45 of 45 resolved.** The count is of rows actually adjudicated, not of grep hits: a
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

## 5. The table

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
| 11 × `cratonvm/internal/Unmodifiable*` | `vm/src/vm/vm_init.rs` · `ensure_bootstrap_compat_class` | YES | 3–49 → 0 | behaviour carriers; door correct, refusal absorbed and warned | none |
| `java/util/Comparator$Native`, `java/util/Enumeration$Impl` | same | YES | 2 / 4 → 0 | same | none |
| `cratonvm/internal/ArrayListSubList`, `StreamCollector`, `StreamChainCollector`, `SnapshotEnumeration` | `native-collections/src/lib.rs` | no | 1–30 → 0 | behaviour carriers; door correct | none |
| `cratonvm/synthetic/Process`, `ProcessPipeInputStream`, `ProcessPipeOutputStream` | `native-io/src/process.rs` · `spawn_and_wrap_with_redirects` | no | 5–13 → 0 | behaviour carriers; door correct | none |
| `java/util/ArrayDeque$Itr`, `java/util/LinkedList$Itr` | `native-collections/src/lib.rs` · `native_ad_iterator`, `native_ll_iterator` | no | 3 → 0 | behaviour carriers; door correct | none |
| `java/util/concurrent/CompletedFuture` | `native-io/src/lib.rs` · `wrap_completed_future` | no | 5 → 0 | behaviour carrier; door correct | none |
| 3 × `Atomic*FieldUpdater$RustJvmImpl` | `native-builtins/src/atomic_updater.rs` | no | 8–12 → 0 | behaviour carriers; door correct | none |
| `Function$AndThen`, `Function$Compose`, `Consumer$AndThen` | `native-builtins/src/phases_late/streams.rs` | no | 1 → 0 | behaviour carriers; door correct | none |
| `java/util/logging/LogManager$StringEnumeration` | `native-builtins/src/logmanager.rs` | no | 2 → 0 | behaviour carrier; door correct | none |
| `javax/net/ssl/SSLSocketInputStream`, `SSLSocketOutputStream` | `native-builtins/src/phases_late/ssl_security.rs` | no | 4 → 0 | behaviour carriers; door correct | none |

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
