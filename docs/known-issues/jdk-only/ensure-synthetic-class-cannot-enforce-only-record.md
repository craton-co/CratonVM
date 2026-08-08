# `ClassManager::ensure_synthetic_class` can record a JDK-only violation but cannot refuse one

**Status:** OPEN, **and no longer dangerous on any measured workload.** Filed
2026-07-31; instrumented 2026-08-04; **migrated 2026-08-05 by JDK-only wave-2
lane L7**, which took the census, migrated every call site that fires, and
replaced this record's "52 call sites" with a measured 10. What remains is
step 3 — deleting the infallible entry point — and the reason it is still open
is not the call sites. See *What is still open*.

> **2026-08-06.** Item 4's last shape is closed: `java/util/Enumeration$Impl`
> now has somewhere for its refusal to land (a real
> `Collections.enumeration(Arrays$ArrayList)`), so every class this record ever
> listed as "refused with nowhere to go" is answered. Item 5 is **two thirds
> **CLOSED, 2026-08-07.** All three allocation funnels are fallible. The
> `native-builtins` one — 1,904 call sites, the last and by far the largest —
> landed on the fifth attempt; see *Why `alloc_concurrent_synthetic` was
> abandoned rather than finished* for the four that did not and what the fifth
> did differently. `ensure_synthetic_class` itself still exists (72 direct
> callers outside the funnels), so this record stays open on **step 3** alone.

The original defect: under `--jdk-only` this API recorded the violation and
then fabricated the class anyway, so the run reported a violation while
continuing in the exact state the contract forbids. That is now false for every
path the three measured workloads reach.

## The measurement (2026-08-05, real JDK 25, `--jdk-only`)

| workload | rows before → after | `compatibility-stub` before → after |
|---|---|---|
| `StrictBoot` (a `main` that prints one line) | 392 → 379 | **13 → 0** |
| `JdkOnlyCensusLoadProbe` | 704 → 656 | **17 → 0** |
| `JdkOnlyBreadthProbe` | 828 → 709 | **18 → 0** |

`--jdk-only-report`'s `counts.compatibility_classes` agrees. The row total falls
because a refused fabrication is a class that never enters the store, and
because the retagged factories now hand back real `java.base` classes the
census counts under `boot-image` instead.

Two further probes, as a check that these three were not a lucky sample:
`L1LoaderIdentityProbe` 13 → **0** with byte-identical stdout, and
`MapLayoutMatrixProbe` 15 → **1**. That last row is `java/util/Enumeration$Impl`
from `native-builtins/src/classloader.rs`'s `getResources` helpers — see
*What is still open*.

**Ten call sites fire, not 52** — the union over the three workloads:

| site | class(es) |
|---|---|
| `vm/src/vm/vm_init.rs:1052` | `java/util/Enumeration$Impl` |
| `vm/src/vm/vm_init.rs:1110` | `java/util/Comparator$Native` |
| `vm/src/vm/vm_init.rs:1227` | 11 × `cratonvm/internal/Unmodifiable*` |
| `native-collections/src/lib.rs:4844` | `cratonvm/internal/ArrayListSubList` |
| `native-collections/src/lib.rs:12143` | `java/util/HashMap$KeyItr` |
| `native-collections/src/lib.rs:15904` | `cratonvm/internal/StreamCollector` |
| `native-collections/src/lib.rs:39187` | `java/util/TreeSet$Itr` |
| `native-builtins/src/lib.rs:25891` | `cratonvm/internal/SystemLogger` |
| `native-builtins/src/lib.rs:37099` | `java/util/function/Function$Identity` |
| `native-builtins/src/phases_late/streams.rs:2730` | `java/util/function/Function$AndThen` |

All ten are migrated. The retired lane write-up
(`L7-ensure-synthetic-class-migration-RETIRED-20260805`) carries the full
before/after, the `Compatible`-mode diff and the HotSpot control.

### The instrument was wrong until it was fixed

Every one of the seven native-minted classes was attributed to a single line —
`vm_exec.rs:13670`, the `NativeContextImpl::ensure_synthetic_class` forwarder
that all ~2,000 native allocation sites funnel through. `#[track_caller]` is now
threaded through the two `NativeContext` trait declarations, their two impls,
and the three allocation funnels. Control: totals and stub counts
byte-identical before and after that change; only `requested_by` moved.

**Caveat, and it bites:** `origin_requesters` records the *first* requester of a
class name, and a refusal records one. Once a site refuses a name, a later
*successful* fabrication of the same name by a different site is still
attributed to the refusing one. The residual rows below are reported as
`vm_init.rs:484` — the refusal helper — but are minted in `native-collections`.

### The population, recounted

**39 live sites in 25 files**, not 52 in 27. The difference is entirely
mis-scoped test code: `vm/src/vm.rs`'s six sit behind
`#[cfg(all(test, feature = "synthetic-jdk"))]`, which a `#[cfg(test)]` scan
misses, and all four of `proxy_gen.rs`'s are in its test module — so this
record's *"`proxy_gen.rs` has 5 sites in the current tree, not 1; each needs its
own adjudication"* was counting tests.

## What is still open

**Step 3 — deleting `ensure_synthetic_class` — and the blocker is not the call
sites.** Three of the 39 *are* the infallible allocation funnels themselves:

* `native-collections::alloc_synthetic` — **migrated 2026-08-06**
* `native-io::alloc_synthetic` — **migrated 2026-08-06**
* `native-builtins::alloc_concurrent_synthetic` — **not migrated; see below**

Between them they have roughly **2,300 callers**, none of which returns a
`Result`. Deleting `ensure_synthetic_class` means making those three fallible,
which is that many call sites — not the 39, and certainly not "52". Any plan
that sizes this work off a grep of `.ensure_synthetic_class(` is sizing the
wrong thing.

The fallible siblings those funnels need already exist and now have real
callers: `try_alloc_synthetic` (native-collections and, since 2026-08-06,
native-io) and `try_alloc_concurrent_synthetic` (native-builtins).

### Two of the three are done, and the unit above is still wrong

**"~2,300 call sites" is not the size of this work, and neither is 39.** The
call sites are MECHANICAL — a paren-matching rewrite to the `try_` spelling
with `?`, plus the `use` lines. What costs is the cascade: every helper that
returned a bare `ObjectRef`/`Value`/`()` and therefore had nowhere to put a
refusal has to gain an error channel, and so does everything that calls it.
The compiler enumerates that set exactly, so it drives the work rather than a
grep. Measured on 2026-08-06:

| funnel | call sites | functions needing an error channel | outcome |
|---|---:|---:|---|
| `native-io::alloc_synthetic` | 23 | **6** | done, 2 rounds |
| `native-collections::alloc_synthetic` | 146 | **44** | done, 4 rounds + 11 by hand |
| `native-builtins::alloc_concurrent_synthetic` | 1,932 | **≥340** | **abandoned — see below** |

Landing the first two also forced **34 cross-crate call sites** in
native-builtins, because `make_hashset_with_elements`, the collector factories
and the view builders are `pub` and became fallible. That is part of the cost of
each funnel and is easy to forget when sizing one in isolation.

### Why `alloc_concurrent_synthetic` was abandoned rather than finished

Not because it is big. Because the cascade **stopped converging**: rounds of
"give the reported functions an error channel, then re-ask the compiler" went
277 → 160 → 134 → 106 → 105 → 155 → 137, and a second loop that also repaired
the mechanical fallout (a stray `?`, a bare `return;`, an unwrapped tail) went
315 → 317 → 322. A loop whose error count RISES is repairing less than it
breaks, and the honest reading is that the remaining sites need per-site
judgement, not another pass.

Three tooling faults found on the way, all of which produce a PARSE error rather
than a type error — which matters, because rustc stops at the first parse error
per file and hides everything behind it:

* a parameter that is itself a closure (`&dyn Fn(usize) -> bool`) makes "the
  last `->` in the signature" pick the CLOSURE's return type; `str.replace` then
  splits `Result<..>` across two parameters;
* `(?<![\w.])get\(` matches inside `$get(`, so a `macro_rules!`-defined function
  gets a `?` appended to its PARAMETER LIST;
* a unit fn whose last line is a tail expression needs a `;` before the appended
  `Ok(())`, and a tail that merely closes a multi-line call (`})`) is not an
  expression to wrap at all — wrapping it yields the literal text `Ok(}))`.

And one structural flaw worth knowing before anyone tries again: **the
`?`-appender matches by NAME across every file**, so converting `alloc_foo` in
one module also stamps a `?` on an unrelated `alloc_foo` in another. rustc
reports each as "`?` operator has incompatible types" and the repair is to
delete that `?` — but a name-based rewriter over a 112-file crate will keep
generating them.

**If you pick this up:** not with a textual rewriter. See the next section —
that advice was tried and is not sufficient.

### Attempt 3, 2026-08-07 — rustc's own suggestions, and where they stop

The 2026-08-06 note above blamed the name-based `?`-appender and prescribed a
per-module loop. The appender WAS a real fault, but fixing it is not enough.
Three strategies, each run to its own stopping point on the same 1,908 sites:

| strategy | from | to | why it stopped |
|---|---:|---:|---|
| name-based `?` appender | 277 | 137, rising | matches by NAME across 112 files; stamps `?` on same-named functions it never converted |
| span-precise regex on rustc's `line:col` | 2,072 | ~1,630, flat | the span often points at a PATTERN (`if let Some(v) = f(..)`); text cannot tell which paren belongs to the failing expression |
| **rustc's own suggestions** | **2,072** | **775, flat** | best by far — rustc knows the expression tree — but its `?` suggestion is `MaybeIncorrect`, and 494 more are `HasPlaceholders`, i.e. not applicable at all |

Two things the third attempt established that the others could not:

* **Apply rustc's suggestions, not your own regex.** `cargo check
  --message-format json` carries a byte-exact `suggested_replacement` per span.
  Accept `MachineApplicable`, plus `MaybeIncorrect` entries whose replacement is
  the original text with a `?` appended — that is the "use `?` to unwrap"
  suggestion, and rustc chose the span. One round applied **556**.
* **The residue looked like SEMANTICS, and was not.** At the stopping point:
  631 `E0308 mismatched types` and — read at the time as the signal that
  mattered — **104 `E0382` "use of moved value"**, which this record concluded
  was ownership damage no textual tool could see. **Attempt 4 showed that
  reading was wrong**; see below. The E0382s were an artefact of repairing the
  right error in the wrong PLACE.

Nothing from these three attempts was committed; `dev` has never carried a
half-migrated `native-builtins`.

### Attempt 4, 2026-08-07 — two real findings, and a hard floor at 758

Same 1,904 sites, driven by rustc's suggestions again, plus two rules the
earlier attempts did not have. Both are worth keeping; neither was enough.

**Finding 1 — rustc will not suggest `?` until the enclosing function already
returns `Result`.** Until then the same expression gets an `.expect(..)`
suggestion instead, which is not the edit anyone wants and which the driver was
correctly refusing. So the order matters: *widen the function first, then ask
rustc again.* The trigger is an `E0308` whose expected/found pair is
`expected T, found Result<T, MethodCallFailed>` — and that text lives on the
span's **`label`**, not on `message`, which is only the string `"mismatched
types"`. A first version of the rule matched on `message` and widened nothing.
With the rule reading `label`, one run went **736 → 4**.

**Finding 2 — the `?` belongs on the `let` BINDING, not on the uses, and that
is where the E0382s came from.** When a `let` binds a now-fallible call, rustc
reports the type error at *every use* of that local and suggests `?` at each
one. Applying all of them unwraps the same value repeatedly:

```rust
let package = i2_alloc_synthetic_package(ctx, &name);   // Result
let handle = ctx.add_global_root(package?);             // use 1
Ok(Some(Value::Object(Some(package?))))                 // use 2 -> E0382
```

The repair is one edit above, and it fixes every use at once:

```rust
let package = i2_alloc_synthetic_package(ctx, &name)?;
```

That accounts for the "use of moved value" wall attempt 3 read as semantic
damage. **It is not ownership damage — it is the correct fix applied at the
wrong site.** A pass that moves the `?` to the binding fixed 719 of them.

One trap inside that repair: stripping `name?` at *file* scope to clean up the
uses also strips `?` from same-named locals in other functions that legitimately
need it, so a pass can fix 342 bindings and leave the error count flat. Drop
only the `?`s rustc itself flags (`` `?` operator has incompatible types ``),
span-precise.

**Where it stopped: 758, flat over six rounds** (528 `E0308`, 132 `E0382`,
16 incompatible `match` arms, 14 `?`-on-`Option`, 12 `return;` in a non-unit
fn, 8 `?`-in-a-closure). Concentrated in
`phases_late/nio_file.rs` (73), `util_concurrent_ext.rs` (54),
`lang_class.rs` (49), `lib.rs` (43), `lang_invoke.rs` (35),
`phases_late/xml_json.rs` (34).

The floor is where it is because the last errors are each a *shape* rather than
an instance: a `match` whose arms must all be widened together, a closure that
must become fallible along with the iterator adapter that takes it, an `Option`
chain that has to choose between `ok_or` and a different return type. Each needs
a decision, and 758 decisions is hand work.

**This attempt WAS preserved,** unlike the first three, because starting from
758 is worth more than starting from 2,072:

    wip/jdk-only-concurrent-funnel-758-DO-NOT-MERGE   (532925266)

It does **not compile** and must never be merged. It is a starting point for
whoever does the hand pass, and nothing else. `dev` is unchanged.

**Recommendation for attempt 5:** stop trying to converge the whole crate.
Take the branch above, pick one file, finish it by hand until
`cargo check` reports nothing in that file, commit, and repeat. The two findings
above make the mechanical majority of each file free; the per-file residue is
small enough to read. Six files carry 40% of what is left.

### Attempt 5, 2026-08-07 — landed

**2,072 errors to zero, and the acceptance run is green.** What made the
difference was not persistence; it was finding three bugs in how the previous
attempts read rustc.

**rustc ELIDES long types.** The single most expensive mistake in attempts
1-4. Both classifier regexes required the error type to appear in the
diagnostic:

    expected `Result<Option<ObjectRef>, ...>`, found `Option<_>`
                                    ^^^ no `MethodCallFailed` anywhere

118 mechanical errors were filed as "needs a human" because of that `...`, and
attempt 4's 758-error "floor" was mostly them. Matching `expected \`Result<`
and `found \`Result<` without naming the error type — and allowing `&?Result<`
for the borrowed form — was worth more than every other rule combined.

**The `Ok(..)` wrap was never automated.** Attempts 1-4 only ever wrapped tails
*textually at widen time*, which is where all the old damage came from (a
`return` matched inside a comment, a `;` matched inside the JVM descriptor
`"()Ljava/io/InputStream;"`, a tail closing a multi-line call turning into
`Ok(}))`). Wrapping from rustc's exact byte span instead is the mirror image of
the `?` rule and just as reliable: **203 in the first round.** The general form
is *widen the signature only, then let the compiler find every return* — it
knows the expression tree and text does not.

**The messages are in the label, not the message.** `E0005`'s message is
`refutable pattern in local binding`; `Err(_) not covered` is on the span
label. A rule keyed on the message matched nothing at all, silently. Two rules
were dead for a whole round each for this reason.

Beyond that, the shapes worth naming for anyone doing this again:

| shape | edit | count |
|---|---|---:|
| `Ok(X)?` | -> `X` — always safe, and pure damage where it does not compile | 40 |
| `f(..);` in statement position | -> `f(..)?;` — the value was already discarded, so the refusal is now the ONLY thing it carries | 181 |
| `match f(..) { .. }` | -> `match f(..)? {` (E0004 "Err(_) not covered") | 16 |
| `let Ok((a,b)) = f(..);` | -> `let (a,b) = f(..)?;` | 6 |
| `opt.unwrap_or_else(\|\| f(..)?)` | `.map(Ok)` before it, one `?` after | 8 |
| `Option<Result<T,E>>` | `.transpose()?` | several |
| double-widen `Result<MethodCallResult, E>` | `MethodCallResult` IS `Result<Option<Value>, E>`; this broke 12 registrations with "expected fn pointer, found fn item" | 14 |

**Two traps that compile.** `for x in f(..)` keeps compiling after `f` becomes
fallible, because `Result` is `IntoIterator` over its Ok value — the loop then
silently iterates zero-or-one `Vec<T>` instead of the elements, and only
surfaces much later as a type error on `x`. And rustc's own suggestion for a
discarded `Result` is `let _ = ..`, which here would reintroduce precisely the
defect this record exists to remove.

**What must NOT be widened.** `extern` declarations (no Rust body to return
`Ok` from), trait-impl methods (`Drop::drop` cannot return a `Result`), and
**registration functions** — 83 `register_*` had been widened by an earlier
pass, and left that way the chain propagates into `vm_init`, whose `SharedVm`
constructor has no error channel. Registration wires closures into a
`NativeMethodRegistry`; the closures are the fallible part and they run later.

**Where a refusal cannot propagate, it is absorbed deliberately and in one
place**: rustls' own `resolve` trait method returns `Option`, so a refusal
becomes "no key" — what rustls already does when a resolver has nothing; the
bytecode-transformer entry point returns `Vec<u8>`, so a refused app loader
falls back to the bootstrap loader. Both are commented at the site. The
violation is recorded upstream either way.

### What the ratchet caught

The strict corpus earned its keep. With the funnel refusing
`java/util/IteratorEnumeration`, `KeyStore.aliases()` raised
`NoClassDefFoundError` and `JdkOnlyPlatformProbe`'s whole `security` section
went from six passing assertions to one failure. That is a class needing a
*landing*, exactly as item 4 described: it now falls back to an enumeration the
JDK builds itself (`Arrays$ArrayList` + `Collections.enumeration`), reusing
`real_snapshot_enumeration`. `Compatible` mode is untouched — the fallback is
only reachable from the refusal arm.

Note the twin in `phases_early.rs` sits behind
`#[cfg(feature = "legacy-synthetic-crypto")]` and is **not** the path the CLI
build takes; the first fix went there and the gate failed again, identically.
Both are wired now.

### Acceptance, 2026-08-07

| check | result |
|---|---|
| `cargo build --release -p cratonvm-cli` | clean, **0** `unused Result` warnings |
| `--jdk-only-report` `counts.compatibility_classes` | **0** (`JdkOnlyCensusLoadProbe`, `JdkOnlyBreadthProbe`; 1,211 violations still recorded — the backlog, not the result) |
| `scripts/jdk-only-strict-probes.sh` | **PASS**, 0 divergent sections observed against 2 baselined; transcript byte-identical to HotSpot in both modes |
| `native-builtins/tests/stub_ratchet.rs` | 7 passed, 0 failed |
| `regression-suite` | 30/31, and the one failure **moves** — see below |

`cargo check -p cratonvm-native-builtins` is **not** sufficient: the CLI's
feature set compiles paths that check does not, and it turned up 46 more errors
plus every one of the 181 discarded refusals after the lib alone was clean.

**And neither is one platform — this one got through.** A span-driven sweep
edits text but is corrected only by diagnostics, and rustc emits none for a
`#[cfg(windows)]` block on Linux. Those arms are therefore **rewritten and
never type-checked**, which is the worst of both. This migration was green on
the Azure Linux host and broke the Windows build: 8 stray `?`, including
`let addr = [0u8; 16]?;` on an array literal and `read_native_pin(..)?` on an
infallible function that fourteen other call sites in the same file spell
without one. Fixed in `604bab335` by another lane, which is not where that
should have been found.

Step 3 sweeps the remaining 72 direct callers the same way, so: `grep -c
'#[cfg('` the files you touch, and run `cargo check` on every target and
feature combination whose arms the sweep edited — not only the one you happen
to be driving from.

**On the regression suite's one failure.** It is not the same test twice:
`RSocketChannelInterrupt` on one run, `RMapGcStress` on the next, and each
passes when run on its own. The Azure host was simultaneously running two other
sessions' Hibernate and H2 suites. `RSocketChannelInterrupt` additionally fails
**identically on a pre-change `dev` binary** (2026-08-05), naming the
`blocked-reader-never-wakes` defect in `native-io/src/socket_channel.rs` that
its own assertion text points at — so that one is pre-existing, not a
regression. Both are load-sensitive (a blocking-read interrupt and a GC stress
loop); neither is evidence about this change, and neither should be read as a
clean 31/31 either.

### What making a funnel fallible actually FINDS — the reason to do it at all

Two mint sites the earlier waves had missed, both surfaced the moment the
funnel stopped fabricating silently, and neither would have been found by
reading:

* **`java/util/ServiceLoader$Itr`** (`native_stream_iterator`). Refusing it
  broke `ServiceLoader`, and `ServiceLoader` is how the CLDR locale provider is
  discovered — so ONE unlanded refusal produced
  `ServiceConfigurationError: Locale provider adapter "CLDR" cannot be
  instantiated` in the probe's `textformat` section *and*
  `attach=throw-NoClassDefFoundError` in `JdkOnlyPlatformProbe`'s `agent`
  section. Two gate sections, one cause, neither naming the class. It now lands
  on a real `Arrays$ArrayItr` like its siblings.
* **`java/util/HashMap$KeyItr` on the `ConcurrentHashMap` key-set path**
  (`native_ksv_iterator`). The 2026-08-05 wave routed the four `HashSet`-side
  mint sites through the refusal and left this one on the infallible funnel, so
  `for (String x : ConcurrentHashMap.newKeySet())` died outright. `RChmKeySetView`
  caught it.

**And the second one is the exception that proves the rule about landings.** It
is deliberately left on the infallible funnel. Every other snapshot iterator
lands on a real `Arrays$ArrayItr`, and that trade was argued as free — "on the
strict path the alternative was never a working `remove()`, it was an iteration
that did not reach `next()`". That argument does not hold here: HotSpot's
`ConcurrentHashMap$KeySetView.iterator()` returns a `KeyIterator` whose
`remove()` writes through, `RChmKeySetView` exercises exactly that, and a
fixed-size list's iterator answers `UnsupportedOperationException: remove`.
Landing it would trade a WORKING capability for a fidelity gain.

So `counts.compatibility_classes` is **1**, not 0, on any workload that iterates
a `ConcurrentHashMap` key set — and that number is the honest reading, not a
regression to paper over. It goes to zero when CratonVM's `ConcurrentHashMap`
carries a real `table[]` its own `KeyIterator` can walk, which is the
collections reclassification wave. **Do not "fix" it by landing that site**
without checking `RChmKeySetView` first.

**The unmodifiable/factory/comparator family is closed — and the order was the
whole lesson.** After the bootstrap migration, `JdkOnlyCensusLoadProbe` still
fabricated `cratonvm/internal/UnmodifiableSet` and `JdkOnlyBreadthProbe` four
more, from `native-collections`' `alloc_unmod_wrapper` / `alloc_unmod_list_itr`
/ `make_comparator` and `native-builtins::lang_system::wrap_system_env_map`.

Making those allocators fallible **on their own** reaches 0 / 0 / 0 and breaks
real JDK `<clinit>`s, because `java.util.Collections.unmodifiable*` and
`List.of` are real methods the JDK's own bootstrap calls. Measured on that
build: `SECTION-FAILED zip: NullPointerException: zone` and `SECTION-FAILED
reflection: NoSuchMethodError: cratonvm.synthetic.AnonymousObject$16.newInstance`
— neither naming a refused class, which is the opposite of a diagnosable
refusal — with the breadth probe going 4 → 8 failures.

**Retag first, then refuse.** `register_factory_natives`,
`register_unmodifiable_natives`, `register_comparator_natives` and the six
`Collections.unmodifiable*` factories are now `SyntheticStub`, so `--jdk-only`
drops them and `java.base`'s bytecode runs; the allocators are fallible behind
that. 0 / 0 / 0, with the strict failure count *unchanged* at 5 and 4.

The test that decides whether a family can be retagged is **not** "does a real
class with this name exist" — it is **"does the real product delegate, or does
it read the backing object's own fields?"** A real `Collections$UnmodifiableMap`
delegates every call to the map it was handed, and that map's CratonVM natives
still answer, so it works. A real `HashMap$KeyIterator` reads the real
`table[]`, which CratonVM's `HashMap.put` native never fills, so retagging
`HashSet.iterator()` would return a silently EMPTY iteration instead of a loud
error.

> **Updated 2026-08-05.** That reasoning about *retagging* still holds, and
> `HashSet.iterator()` still must not be retagged. The conclusion drawn from it
> did not hold: the family did not have to stay a refusal. A third option
> existed — keep the native, and when the policy refuses the fabricated
> iterator class, hand back the snapshot through a real `Arrays$ArrayList`'s
> own iterator, which reads only the `Object[]` it was given. All six sections
> are fixed; see
> `jdk-only-strict-boot-refused-five-classes-FIXED-20260806.md`
> (retired from this directory 2026-08-06, once its fifth class — the
> `System.Logger` one, which had taken out every `ObjectInputStream`
> construction — landed on a real `jdk.internal.logger.SimpleConsoleLogger`).
> The rule is
> narrower than "delegates vs reads its own fields": what matters is whether
> SOME real class exists whose fields we can legitimately fill, not whether the
> obvious one can.

## What is wrong (unchanged in shape)

`classloading/src/class_manager.rs`:

```rust
pub fn ensure_synthetic_class(&mut self, name: &str, num_fields: usize) -> ClassId
```

The return type is a bare `ClassId`. There is no error channel, so the function
passes `enforce: false` and fabricates regardless of mode. `enforce` is
documented on `fabricate_class` exactly as the defect describes it: *"`true`
returns the `ClassNotFoundException` the contract asks for, `false` records the
violation and fabricates anyway. Either way the violation is recorded, and
either way `Compatible` mode fabricates."*

Contract §5 requires the opposite: *"Under `JdkOnly`, every path that today
fabricates a class … must instead return the specification-appropriate
`ClassNotFoundException` / `NoClassDefFoundError` and record a
`CompatibilityClassRequested` violation."*

**The `load_class` chain does enforce.** An absent enterprise or JDK class
arriving through ordinary class loading is correctly refused
(`create_synthetic_stub`, routing through `admit_compatibility_class`). It is
this *direct* API — used by VM bootstrap and by natives that want an allocation
shape — that cannot.

`admit_compatibility_class` is called from exactly two places
(`create_synthetic_stub` and `fabricate_class`), *"so there is no third place a
stub can be minted without the policy seeing it."* The problem was never "the
policy can be bypassed"; it was "the policy is seen and then overridden by a
signature".

## The fallible siblings, and what a refusal has to look like

* `try_ensure_synthetic_class(name, n) -> Result<ClassId, VmError>` —
  `ClassNotFoundException` under `JdkOnly`. Byte-for-byte
  `ensure_synthetic_class` under `Compatible`.
* `ensure_generated_class(name, n, origin)` — for arrays, hidden classes,
  lambdas, proxies, reflection accessors and VM-internal shapes (contract §1
  item 6). Never refused, in either mode; `debug_assert`s that the caller did
  not pass a `CompatibilityStub` origin.

**A refusal must be catchable, which needed a third piece.**
`impl From<ClassIdentityError> for MethodCallFailed` yields
`MethodCallFailed::InternalError`, documented as *"not catchable by Java code —
aborts execution entirely"*. That is the right shape for a VM invariant and the
wrong one for a policy refusal. `native_api::refusal_to_java_failure` (added
2026-08-05) builds the throwable instead: `NoClassDefFoundError` for a policy
refusal, message = the internal name, matching the VM-side
`raise_no_class_def_found` so a refusal reaching Java from a native and one from
constant-pool resolution are indistinguishable to a `catch` block;
`IncompatibleClassChangeError` for an ambiguous name. It falls back to the
uncatchable form only when the throwable itself cannot be constructed.

For a caller with no error channel at all — the VM bootstrap block — "refuse
diagnosably" means the recorded violation *plus* a `tracing::warn!` that the
CLI's default WARN/stderr filter prints with no extra flag and that states the
consequence. `vm_init::ensure_bootstrap_compat_class` is the shape to copy.

## The `Unmodifiable*` family is adjudicated as staying `CompatibilityStub`

They are the largest group and the most tempting to reclassify — no class file
exists under `cratonvm/internal/UnmodifiableList`, which is the `VmInternal`
shape. But they stand in for `java.util.Collections$UnmodifiableList` and
friends: the real `Collections.unmodifiableList()` bytecode is not running, and
that is a compatibility substitution whatever the stand-in is named.
Reclassifying them is the dangerous direction in *Blast radius* — it silences
the violation, keeps fabricating, and makes the zero-stub census green while
the substitution continues. L7 acted on that verdict: the bootstrap site
**refuses** them rather than relabelling them. See
VM-internal classes are mislabelled `CompatibilityStub` (`jdk-only-wave2-vm-internal-classes-mislabelled-RETIRED-20260806.md`)
(RETIRED 2026-08-06).

## What specifically must change

1. ~~Migrate the call sites that fire~~ — done 2026-08-05, all ten.
2. ~~Make `vm_init.rs`'s bootstrap block fail loudly under `JdkOnly`~~ — done;
   `StrictBoot` reaches `main` with **zero** fabricated compatibility classes.
3. ~~Retag the unmodifiable / factory / comparator natives, then migrate their
   allocators~~ — done 2026-08-05; all three measured workloads are at zero.
4. ~~The four shapes that are still refused rather than removed —
   `java/util/HashMap$KeyItr`, `cratonvm/internal/ArrayListSubList`,
   `StreamCollector`, `SystemLogger`~~ — all four now have somewhere for the
   refusal to LAND, which the note above was wrong to think impossible: the
   first three on 2026-08-05 (real `Arrays$ArrayList` iterator,
   `Spliterators.iterator`), `SystemLogger` on 2026-08-06 (a real
   `jdk.internal.logger.SimpleConsoleLogger`, built through its own `<init>`).
   That last one was reached from `ObjectInputFilter$Config.<clinit>`, so
   refusing it had been costing every `ObjectInputStream` construction in the
   VM. See
   `jdk-only-strict-boot-refused-five-classes-FIXED-20260806.md`.
   **Still open here:** `java/util/Enumeration$Impl` in `classloader.rs`'s
   `getResources` helpers, which has no such landing yet. And the refusals are
   landings, not removals — the natives themselves are still registered, which
   is item 5's business.
5. Make the three allocation funnels fallible, then delete
   `ensure_synthetic_class`. **Two of three done 2026-08-06** —
   `native-io::alloc_synthetic` and `native-collections::alloc_synthetic`, plus
   the 34 cross-crate callers that forced. `native-builtins::alloc_concurrent_synthetic`
   is not done and the reason is measured, not estimated; see *Why
   `alloc_concurrent_synthetic` was abandoned rather than finished* above. Until
   it is, `ensure_synthetic_class` cannot be deleted and this record stays open.

## How to verify a fix

* A `--jdk-only` boot on a complete real JDK image must reach `main` with
  **zero** compatibility-class *fabrications* in the `--jdk-only-report` JSON
  (`counts.compatibility_classes`). **Not** zero violations:
  `admit_compatibility_class` records the request before it refuses it,
  deliberately, so the violation list is the backlog and the count is the
  result. `StrictBoot` is at 0 with 13 recorded requests.
* Grep gate: `.ensure_synthetic_class(` must match zero non-test sites. 39
  today.
* `--dump-class-origins` must show no `compatibility-stub` rows for non-array
  JDK/application/dependency classes (contract §11).
* `Compatible` mode must be byte-for-byte unchanged — the existing regression
  suite plus `native-builtins/tests/stub_ratchet.rs`. That baseline moved
  157 → 165 on 2026-08-05 and the constant's doc comment explains why (a
  relabelling of eight already-existing fakes, not eight new ones); it must not
  move again without the same kind of explanation.
* Count **call sites**, not violations, when checking migration progress:
  `admit_compatibility_class` dedupes by class name (`origin_violations_seen`),
  so a migrated caller that stops fabricating a name some *other* caller also
  requests will not change the violation count. And see the `requested_by`
  first-writer caveat above before trusting an attribution.

## Blast radius if done wrong

* Migrating a **legitimately-generated** class to `try_ensure_synthetic_class`
  makes `--jdk-only` reject proxies, lambdas or array shapes — an immediate,
  loud, but wrong failure that will be misread as "strict mode doesn't work".
* Migrating a **compatibility stub** to `ensure_generated_class` is the
  dangerous direction: it silences the violation, keeps fabricating, and makes
  the zero-stub census report green while the substitution is still happening.
  Contract §11's acceptance criterion becomes unfalsifiable. Because
  `ensure_generated_class` only `debug_assert!`s on a `CompatibilityStub`
  origin, a release build will not catch this at all.
* Migrating an allocator whose class stands in for a **real JDK method the JDK
  itself calls during `<clinit>`** breaks the boot in a way that does not name
  the refused class — R1's measured outcome. Retag the native first.
