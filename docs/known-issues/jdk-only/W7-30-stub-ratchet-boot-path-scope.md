# W7-30 — the stub ratchet censused 6 of `vm_init`'s 48 registrars

**Status:** FIXED for the scope, 2026-08-11. Both residuals taken 2026-08-12 —
see §6.1 and §7.1 — and **the fix of 2026-08-11 was found to have been INERT
the whole time**: §9. Nothing was built or run on 2026-08-12; every claim
added that day is source-verified or derived by reading, and the one number
that would need a run is not written down.

**The gate is now FIRING (1253 → 1261) and the whole +8 is derived — §11.**
Not re-frozen here, and §11.2 says which of the two constants may be re-frozen
from the derivation alone and which needs its own run. The `stub_ratchet.rs`
prediction of `1269 / 1259` is short by exactly two rows and the correction is
nominated in §11.3.

**Species:** blind instrument. Same family as W7-22 (the
`CRATONVM_ENFORCE_NATIVE_SHADOW` dial that yields at most once per triple and
then hands every later dispatch back to the native, so it is a strictly weaker
experiment than the retirement it licenses) and W4-4 (the layout-alias detector
that reported under-allocation and stayed silent on over-allocation, which is
the direction that is heap corruption rather than a wrong answer). The shared
shape: **an instrument that cannot observe the thing it is trusted to
adjudicate, while reporting green.**

**File:** `native-builtins/tests/stub_ratchet.rs`.

---

## 1. The defect

`BASELINE_SYNTHETIC_STUBS` is an **exact** assertion — `SLACK = 0`, and the
constant's own doc says "adding one synthetic stub fails". It is taken over the
registry that `register_boot_path` builds.

`register_boot_path` made **six** registrar calls. `vm_init.rs`'s
`#[cfg(not(feature = "synthetic-jdk"))]` arm — the shipping `cratonvm-cli`
boot — makes **48**.

So forty registrars' worth of registrations sat outside a zero-slack ratchet.
A registration there could be **added**, **deleted**, or **retagged
`Bridge` <-> `SyntheticStub`** and the number would not move by a single row.
That is not a margin-of-error problem; it is the specific failure a ratchet
exists to prevent, holding over most of the population it named.

The file's own history says this happened before. On 2026-08-05 the census ran
`register_essential_natives` and nothing else, "under a doc comment claiming it
built the default native registry exactly as the VM's real-JDK boot path does.
That was false, and the gap was large." The fix that day added five registrars
and wrote a section headed *"What is still not counted, and why that is
acceptable"*. That section was itself a hand-derived list, and it was short by
forty.

## 2. How it surfaced — by disagreement, twice, and never by the gate

Both instances of this defect were found because a **second** measurement
disagreed with this one. Neither was found by reading the gate.

* **2026-08-05.** Retagging four `native-collections` registrars moved 364 rows
  `Bridge` -> `SyntheticStub`. L6's `bridge-ratchet.sh`, which censuses a
  running VM, counted every one. This gate did not move. Two ratchets over one
  VM, 364 apart.
* **2026-08-11.** A lane retagging the `ProcessHandle` block predicted
  1038 -> 1041. After its work merged, the gate reported **7 passed, 0 failed,
  count unmoved**. The same lane had already written down why, in
  W7-10-processhandle-interface-stub-bodies.md: `register_boot_path` calls
  neither `register_p60_process_handle` nor `register_classvalue_natives`.

The second one is the sharper evidence, because the prediction was **also
wrong**, and in a way the gate had no way to correct. The lane expected +3 —
the three `current`/`pid`/`isAlive` restatements in `register_phase57_process`,
which it assumed were in scope. Measured: the old census held **zero**
`java/lang/ProcessHandle` rows **of any kind**. Not three mistagged ones. None.
Neither the prediction nor the observation was a measurement of anything.

## 3. Every registrar `vm_init` reached and the gate did not

Derived from `vm_init.rs` mechanically, not by `rg` — the README (§"Do not size
anything here from an `rg` count") records grep-derived sizes in this area as
wrong by up to an order of magnitude, always the same direction. The extractor
isolates the `cfg(not(feature = "synthetic-jdk"))` arm by brace depth (so the
sibling synthetic arm's own real-JDK block, which also calls
`register_p60_process_handle` at `:1901`, cannot be spliced in) and takes bare
`register_*` calls at statement start.

**48 registrar calls in the arm. 6 were in the gate. 40 were not.** The two the
prior lane found incidentally are marked ★.

| # | registrar | `vm_init.rs` |
|---|---|---|
| 1 | `register_p61_file_handler` | 2235 |
| 2 | `register_url_classloader_close_bridge` | 2237 |
| 3 | ★ `register_p60_process_handle` | 2406 |
| 4 | ★ `register_classvalue_natives` | 2413 |
| 5 | `register_random_and_securerandom_natives` | 2447 |
| 6 | `register_properties_sidetable` | 2460 |
| 7 | `register_t12_unsafe_natives` | 2463 |
| 8 | `register_t14_system_bootstrap` | 2466 |
| 9 | `register_boot_loader_natives` | 2470 |
| 10 | `register_phase57_nio_file` | 2473 |
| 11 | `register_phase57_file` | 2483 |
| 12 | `register_p59_jar` | 2492 |
| 13 | `register_p59_bulk_stream_transfer` | 2497 |
| 14 | `register_p59_zip_output_primitives` | 2500 |
| 15 | `register_spring_boot_logback_apply` | 2508 |
| 16 | `register_url_codec` | 2705 |
| 17 | `register_charset_natives_pub` | 2706 |
| 18 | `register_p58_charset_coder` | 2707 |
| 19 | `register_real_charset_natives` | 2708 |
| 20 | `register_deprecated_internal_natives` | 2709 |
| 21 | `register_arrays_support_natives` | 2712 |
| 22 | `register_string_latin1_natives` | 2715 |
| 23 | `register_classloader_real_natives` | 2733 |
| 24 | `register_phase54_method_handle` | 2736 |
| 25 | `register_p63_method_handles_lookup` | 2739 |
| 26 | `register_t4_method_handle_invoke` | 2742 |
| 27 | `register_t28_method_handle_completeness` | 2746 |
| 28 | `register_p68_invoke_extras` | 2758 |
| 29 | `register_reflect_proxy_natives` | 2765 |
| 30 | `register_instrumentation_natives` † | 2773 |
| 31 | `register_self_attach_natives` † | 2778 |
| 32 | `register_vm_management_impl` ‡ | 2783 |
| 33 | `register_jmx_natives` ‡ | 2817 |
| 34 | `register_thread_impl` ‡ | 2822 |
| 35 | `register_class_loading_impl` ‡ | 2824 |
| 36 | `register_garbage_collector_impl` ‡ | 2826 |
| 37 | `register_memory_pool_impl` ‡ | 2830 |
| 38 | `register_memory_manager_impl` ‡ | 2832 |
| 39 | `register_operating_system_impl` ‡ | 2834 |
| 40 | `register_hotspot_diagnostic` ‡ | 2836 |
| 41 | `register_flag_impl` ‡ | 2838 |
| 42 | `register_slf4j_binder_stubs_pub` | 2842 |

† Lives in the `vm` crate (`crate::runtime::instrument::*`). `native-builtins`
cannot dev-depend on `vm` — that is a dependency cycle — so these two are the
only permitted residue, ratcheted as `UNMODELLED_VM_CRATE_REGISTRARS = 2`.

‡ `#[cfg(feature = "management")]` at the `vm_init` call site, and `nb::jmx` is
itself feature-gated. Replayed under the same `cfg`. See §5.

**`register_random_and_securerandom_natives` and `register_properties_sidetable`
are the two that make this a WRONG census and not merely a narrow one.**
`vm_init` runs them immediately after `register_collections_natives`, under a
banner it carries verbatim — `LAST-WRITE-WINS BOUNDARY — do not reorder` —
*because* `register_collections_natives` overwrites them with layout-wrong
versions. Stopping at `register_collections_natives`, as the gate did, counts
the whole `java/util/Random` family and ~23 `java/util/Properties` triples at
the kind of the row the shipping VM **discards**.

## 4. The fix, and why the number is the smaller half of it

`register_boot_path` now replays all 46 reachable registrars in `vm_init`'s
order, including the last-write-wins boundary with its comment intact.

**The load-bearing change is not the wider number.** A count baseline cannot
detect a registration outside its own scope — that is what "outside the scope"
means — so widening it once buys nothing against the next drift, which is
exactly how the 2026-08-05 fix decayed. The scope itself has to be checkable.

Added: `the_censused_scope_is_vm_inits_boot_path`, a source witness that reads
`vm_init.rs` from the working tree and asserts two things that need different
fixes:

* an **unmodelled registrar** — present in `vm_init`, absent from
  `VM_INIT_SEQUENCE` — ratchets against `UNMODELLED_VM_CRATE_REGISTRARS` (2);
* an **order inversion** among modelled registrars is a hard failure with no
  baseline, because registration is last-write-wins and an inversion makes the
  census count the discarded row's kind (§3's boundary is one such pair).

**What the witness honestly does not assert**, stated in its doc comment rather
than left for a reader to assume: that `register_boot_path` *calls* every name
in `VM_INIT_SEQUENCE`. Rust has no reflection over a function body. The list is
checked against `vm_init`, and the replay is checked against the list by review.
Closing that last gap needs either a proc-macro that generates both from one
list or a registry-side "which registrars ran" census; neither is this lane's
work, and pretending the witness covers it would be a third blind instrument.

## 5. New baselines, and the delta split by cause

Taken from real runs of the test in both configurations, per the constant's own
instruction ("do not hand-derive this number"). `cargo` exit code checked, not a
trailing command's.

| | old scope | new scope | delta |
|---|---|---|---|
| stubs, no-management | 1032 | **1253** | +221 |
| rows, no-management | 11,471 | **12,445** | +974 |
| stubs, management | 1032 | **1263** | +231 |
| rows, management | 11,471 | **12,758** | +1,287 |
| strict rows, no-management | 10,439 | **11,192** | +753 |
| strict rows, management | 10,439 | **11,495** | +1,056 |

Result in both configurations: **8 passed, 0 failed, 1 ignored.**

**The +221 splits by cause, measured per row from `registered_by` provenance:**

* **21 rows are the `ProcessHandle` retag** (`0ab1067ec`) — the movement that
  change was entitled to and could not produce. 18 in
  `register_p60_process_handle`, plus the 3 `current`/`pid`/`isAlive`
  restatements in `register_phase57_process`. All 21 were `Bridge` before that
  commit: it flips `register_p60_process_handle`'s scope
  `Bridge` -> `SyntheticStub`, and the restated three inherited
  `register_phase57_process`'s `Bridge` ambient until it wrapped them in an
  explicit `SyntheticStub` scope.
* **200 rows predate that retag entirely** and were never counted by anything
  here. They are not one registrar's backlog — the largest contributors are
  `messaging_shims.rs` (103 stub rows in the new census),
  `logging_shims.rs` (53), `logmanager.rs` (44), `plain_socket.rs` (34),
  `atomic_updater.rs` (32), `spring_startup_bootstrap.rs` (29),
  `native-io/src/process.rs` (26) and `shared_secrets_bridge.rs` (23).

**No row changed meaning.** Of the 221, **204 sit on triples the old census did
not hold at all** and the other 17 are second registrations of triples it did
hold. **Zero** sit on a triple the old census counted as a non-stub. So this is
a population that grew, not a set of kinds re-decided — the only reading a scope
fix admits, and the check that separates it from a retag wearing a scope fix's
clothes.

`register_classvalue_natives` contributes **2 rows and 0 stubs**: it is in the
gap for the same structural reason, but it holds no fakes.

### 5.1 The frozen constant was six above what the code measured

Independent of the scope: the live count under the OLD scope was **1032**, and
`BASELINE_SYNTHETIC_STUBS` was frozen at **1038** — in both configurations, so
this was not feature drift. A constant whose doc says "this is the exact current
observed count" and "the ratchet has zero slack" had been silently admitting six
new stubs.

The mechanism is that the printed line and the constant were never compared,
because nothing in the output named the configuration or the constant. The
census now prints `stub-ratchet [<config>]: ...` and a paste-ready
`const <NAME>: usize = <N>;` line, and the failure message names which constant
to re-freeze.

### 5.2 The baseline is now keyed per configuration

`vm_init` gates ten `jmx::*` registrars on `#[cfg(feature = "management")]`;
`cratonvm-vm` declares `management` in its defaults so **every shipping
`cratonvm-cli` build has it**, while a `-p cratonvm-native-builtins` resolve
does not. The registry therefore genuinely differs by 313 rows and 10 stubs.

Collapsing this to one number requires dropping those ten registrars from the
model in *both* configurations — i.e. deliberately re-opening the blind spot to
get a tidier constant. So there are two constants, both compiled in both
configurations (neither can be edited while invisible to the compiler), with the
`cfg` selecting which one adjudicates. This mirrors
W6-4-duplicate-registration-gate.md, which reached the same conclusion
independently.

## 6. Residual: one `SyntheticStub` is outside this gate by construction

`register_boot_path`'s previous comment said of `vm_init`'s inline
registrations: *"They are `Bridge`, and a `SyntheticStub` added there would slip
past."*

**The conditional was already false when it was written.**
`vm/src/vm/vm_init.rs:2726` registers
`io/quarkus/bootstrap/runner/RunnerClassLoader.close()V` with an explicit
`NativeKind::SyntheticStub`, inside the real-JDK arm. The comment beside it is
deliberate and correct about its own intent; it is simply invisible to this
census, because `native-builtins` cannot depend on `vm`.

So **the baseline is a floor by one**, and the comment now says so instead of
predicting the case in the future tense. The only real fix is to move the gate
to `vm/tests/`, where the whole arm including its inline registrations is
reachable. That is a cross-crate move touching CI wiring and is not this lane's
scope.

### 6.1 What was taken, 2026-08-12 — the half that needs no census

**(a) is HALF DONE, and the half that is done is the half that needed no
measurement.** `vm/tests/stub_ratchet.rs` now exists. It is deliberately **not**
a second stub census — two count-ratchets over "the same" VM is the exact
configuration `bridge-ratchet.sh`'s header records as having cost weeks, and
seeding a second baseline was impossible in a session that could not run
`cargo`. It closes the part of this section that is settled by reading:

* **`the_vm_crate_registrars_add_no_synthetic_stub`** replays the two
  `crate::runtime::instrument::*` registrars and asserts they contribute zero
  `SyntheticStub` rows and zero *unscoped* registrations. That turns
  `UNMODELLED_VM_CRATE_REGISTRARS = 2` from an unbounded admission into a
  bounded one: **the two registrars outside the census cost the frozen number
  nothing.**
* It is not self-evident. `register_instrumentation_natives` makes fourteen
  plain `r.register(...)` calls and `register_self_attach_natives` four, none of
  which states a kind. `effective_category` is
  `current_category.unwrap_or(NativeKind::SyntheticStub)`, and every registrar
  in the sequence restores its own category, so at `vm_init`'s top level the
  ambient is the constructor's `None`. Called bare, those eighteen would land as
  unscoped `SyntheticStub` and `--jdk-only` would refuse the whole
  `java.lang.instrument` and self-attach surface **by accident**. They do not,
  because `vm_init` wraps both calls in `set_category(NativeKind::Bridge)` and
  restores afterwards.
* That scope lives in a file neither gate replays and nothing asserted it.
  **`the_instrument_registrars_run_under_a_bridge_scope`** is the source witness
  for it — without it, the replay above would keep passing (it applies the scope
  itself) while the shipping boot silently lost eighteen bridges.

**What (a) still does not do, and what it needs.** The inline registrations —
and therefore the floor-by-one — are still outside every census. Counting them
means *running* the arm, and the arm is 666 lines in the middle of
`SharedVm::new`; there is no registration-only entry point to call. Completing
the move is blocked on extracting one from `vm/src/vm/vm_init.rs`:

```rust
pub(crate) fn register_real_jdk_boot_natives(
    native_methods: &mut NativeMethodRegistry,
    shim_selection: cratonvm_native_builtins::app_shims::ShimSelection,
)
```

with the `#[cfg(not(feature = "synthetic-jdk"))]` arm reduced to a call to it.
That is a `vm/src` change with a real review surface — the arm interleaves
registration with VM state the extraction must not capture — and it has to land
together with a re-freeze of both stub baselines from **one** real run, because
the count grows by the inline registrations. Owner: whoever owns
`vm/src/vm/vm_init.rs`.

**Meanwhile the floor is ratcheted rather than merely documented.**
`the_inline_registrations_in_vm_init_are_enumerated`
(`native-builtins/tests/common/vm_init_boot_path.rs`) scans the arm and fails if
more than `INLINE_SYNTHETIC_STUBS_IN_VM_INIT = 1` inline registrations state or
scope `NativeKind::SyntheticStub`. Source-verified on 2026-08-12: eight inline
`native_methods.register*` calls in the arm, exactly one of them
`register_with_kind(..., NativeKind::SyntheticStub)` for
`io/quarkus/bootstrap/runner/RunnerClassLoader.close()V`. A floor by one that
nothing checks becomes a floor by two.

**(b) is DONE.** See §7.1.

> **VERIFIED AGAINST A BINARY 2026-09-02.** Both ratchets this section names have
> been run, on a build from this tree:
>
> ```text
> the_inline_registrations_in_vm_init_are_enumerated ... ok   (the floor of 1)
> the_replayed_sequence_matches_vm_init ............... ok   (§7's replay)
> stub_ratchet, management     1645 stubs / 13897 total, baseline 1645, slack 0
> stub_ratchet, no-management  1634 stubs / 13529 total, baseline 1634, slack 0
> duplicate_registration_gate  6 passed
> ```
>
> Both arms of the stub ratchet were run separately and pasted separately, never
> derived from one another.
>
> **The numbers in this record's banner are STALE and cannot be re-checked.** It
> says "the gate is now FIRING (1253 -> 1261)" and that `stub_ratchet.rs`'s
> prediction of `1269 / 1259` is "short by exactly two rows". Today's baselines
> are **1645 / 1634**, and the gate is GREEN with slack 0 — someone re-froze it
> in the three weeks since. The +8 derivation in §11 no longer has a subtrahend
> in the tree, exactly as `H3-1` §5's `−7` no longer does. Stale, not wrong:
> nobody can now tell whether those eight rows arrived as derived.
>
> **§7's residual is still true and is now visible from the other side.** The
> boot-path model exists twice, and the second copy is in
> `essential_wiring_ratchet.rs` alongside `W7-5`'s three tests — so that file
> reads as five tests where `W7-5` §6.3.1 describes three. Two records, one
> file, both green.

## 7. Residual: the boot-path model now exists twice

`native-builtins/tests/duplicate_registration_gate.rs` already carried a
complete, source-witnessed replay of `vm_init`'s real-JDK arm
(`VM_INIT_SEQUENCE` + `vm_init_real_jdk_boot_path` +
`the_replayed_sequence_matches_vm_init`). This lane owns only
`stub_ratchet.rs`, and two integration-test binaries cannot share a module
without a new file, so the model is now duplicated.

That is not free, and it is not catastrophic either: both copies are checked
against the same `vm_init.rs` by two independent witnesses, so a drift in one
fails that one. The failure mode of duplication here is *redundant maintenance*,
not *silent disagreement* — which is strictly better than the single
unwitnessed model this record is about.

**The collapse, for a lane owning both files:** add
`native-builtins/tests/common/vm_init_boot_path.rs`, move `VM_INIT_SEQUENCE`,
the replay and the witness into it, and `#[path = "common/vm_init_boot_path.rs"]
mod boot_path;` from both test targets. The witness must stay ungated on
`management` while the replay stays gated, for the reason
`duplicate_registration_gate.rs` documents: the ten `jmx::*` calls are textually
present in `vm_init.rs` in every build, so a `cfg`-gated list would report ten
unmodelled registrars in the default resolve.

If (a) above lands first, this residual disappears with it.

### 7.1 DONE, 2026-08-12 — and the paragraph above was wrong about the cost

`native-builtins/tests/common/vm_init_boot_path.rs` exists and holds the one
model: `VM_INIT_SEQUENCE`, `UNMODELLED_VM_CRATE_REGISTRARS`, the replay
(`vm_init_real_jdk_boot_path`), the arm locator, and both source witnesses.
`stub_ratchet.rs` and `duplicate_registration_gate.rs` each carry
`#[path = "common/vm_init_boot_path.rs"] mod boot_path;` and a `use` of the
replay, so every call site is unchanged; `essential_wiring_ratchet.rs` (new, see
W7-5-registrars-that-never-shipped.md §6.3) includes it too, for its
"survives the whole boot" assertion. The `cfg` advice above was followed
exactly: the list is ungated, the ten `jmx::*` calls in the replay are not.

**The cost estimate in §7 was wrong, and the error is the interesting part.**
It said the duplication's failure mode was *redundant maintenance, not silent
disagreement*, "strictly better than one shared model with one witness". That is
true of the MODEL and false of the INSTRUMENT, and both copies shared one
instrument bug — §9. Two witnesses reading the same source file with the same
broken locator do not disagree. They agree, and they are both wrong.

One consequence worth stating: the witnesses are compiled into every including
binary, so they run once per test target rather than once per crate. That is the
intended cost of sharing a module between integration-test targets; each reads
one file and scans it once.

## 8. What the widened gate did NOT reveal

Stated because a widened gate that goes green everywhere invites the suspicion
that it was widened until it passed. It was not; each of these was a live
assertion over 974 newly-visible rows and each held:

* `no_registration_runs_on_the_ambient_default` — **0 of 12,445** registrations
  made with no category scope in effect. Forty registrars entered the census and
  not one relies on the registry's conservative default.
* `no_fake_survives_strict_mode_as_someone_elses_bridge` — **0 triples** are a
  `SyntheticStub` in compatible mode and a `Bridge` in strict. This was 58 when
  the test was written; the wider scope adds none.
* `strict_registry_has_zero_synthetic_stubs` — 0 stubs in the strict registry;
  1,305 refusals recorded (was 1,084).
* `strict_registry_drops_only_the_stubs` — both one-sided bounds hold at the new
  scope.

The two vacuity floors were re-derived with the scope, both from the **smaller**
configuration (a floor that only holds in the build with more registrars in it
is not a floor): `MIN_TOTAL_REGISTRATIONS` 11,000 -> 11,800 and
`STRICT_MIN_TOTAL_REGISTRATIONS` 10,200 -> 10,900, the latter keeping the ~300
rows of headroom its own comment justifies.

## 9. The witness §4 added was BLIND from the day it landed — found 2026-08-12

This is the finding of the 2026-08-12 pass, and it is the same species one level
up. §4 says the load-bearing change is not the wider number but the source
witness, "the difference between a scope that is documented and a scope that is
checked". **The witness was not checking it.** It located `vm_init`'s real-JDK
arm with

```rust
lines.iter().position(|l| l.contains("cfg(not(feature = \"synthetic-jdk\"))"))
```

and the first line of `vm/src/vm/vm_init.rs` matching that substring is a
**comment** — the W7-50 tombstone inside the `#[cfg(feature = "synthetic-jdk")]`
arm's own real-JDK `else` branch, which quotes the attribute in prose: *"'The
branch below' was read as the `#[cfg(not(feature = "synthetic-jdk"))]` block, but
the relevant fork is `if config.use_synthetic_jdk`"*. The real attribute is 44
lines below it.

So the brace scan isolated a 39-line window in the **wrong arm**. Measured by
replaying the witness's own algorithm against the tree, both before and after:

| | broken locator | fixed locator |
|---|---:|---:|
| registrars observed | 8 | 48 |
| of those, modelled by `VM_INIT_SEQUENCE` | 8 | 46 |
| unmodelled (ratcheted `<= 2`) | 0 | 2 |
| names in `VM_INIT_SEQUENCE` never observed | 38 | 0 |
| verdict | **PASS** | **PASS** |

The 8 it saw are the `jmx::*` calls, which both arms make in the same relative
order — so every one was in the list, `unmodelled` was zero, and the order check
ran over a sequence it could not fail on. **A green line, a plausible number, and
no assertion about the arm the test is named after.** The same code, and
therefore the same blindness, sat in `duplicate_registration_gate.rs`'s copy:
one locator defect, two files, neither able to notice it by disagreeing with the
other.

**The fix, and the assertion that would have caught it.** The locator now
requires the *trimmed* line to `starts_with` the attribute — a comment can
contain an attribute, but a comment cannot start with one. And the witness now
asserts the direction nobody had: **every name in `VM_INIT_SEQUENCE` must
actually be OBSERVED**. Under the broken locator that assertion fails with 38
names, immediately and unambiguously; under the fixed one it holds at 46/46.
An unmodelled-registrar ratchet is a bound on what the scan found and says
nothing at all when the scan found the wrong thing.

**The model was never stale; only the instrument was.** With the locator fixed,
all 46 modelled registrars are observed in `VM_INIT_SEQUENCE` order and the two
unmodelled ones are exactly the `crate::runtime::instrument::*` pair §3 names.
Nothing in §3, §4 or §5 needs revising — which is why this went undetected: the
gate agreed with the truth for a reason unrelated to the gate.

Two further population holes in the same scan, both derived by reading and
neither fixed here, because fixing either changes a slack-free count nobody has
re-taken:

* **The scan is NAME-SHAPED.** It matches bare callees beginning with
  `register_`. `vm_init` also calls `init_service_loader_bootstrap` — a `pub fn`
  in `vm_init.rs` that wraps
  `cratonvm_native_builtins::service_loader::register_service_loader_natives` —
  and it is invisible to both the observation and the unmodelled ratchet. Benign
  for the KIND (that registrar states `SyntheticStub` explicitly, and its other
  caller `jdbc::register_jdbc_driver_natives` is on the replayed path), but a
  real hole in the POSITION: whether the replay holds those 11 triples where the
  shipping VM holds them is a question for a registry dump, not a grep.
* **The inline registrations** — eight, one of them a stated `SyntheticStub` —
  are now ratcheted (§6.1) but still not counted.

## 10. The generalisation

Every gate in this campaign is a predicate over a population. Reviewers argue
about the predicate, which is visible in the assertion, and the campaign has
three records now where the predicate was fine and the **population** was the
defect — W4-4 (a detector whose predicate covered one direction of the
population), W7-22 (a dial whose population was one dispatch per triple), and
this one.

Two operational consequences:

1. **A gate's scope needs its own assertion, against a source of truth, or it
   decays.** The 2026-08-05 fix here widened a scope by hand and wrote a section
   explaining what remained uncovered. Six days later that section was short by
   forty registrars and nothing had failed. The witness in §4 is the difference
   between a scope that is documented and a scope that is checked.
2. **A gate that does not move when a change predicts it will is a finding about
   the gate.** In both instances here the disagreement was visible and got read
   as "the prediction was wrong" rather than "the instrument is blind". W7-22
   records what that costs when the blind instrument is the one licensing the
   change: the `java.util.logging` shadow retirement it green-lit is a live
   regression, shipped because no vector covered it.

## 11. The gate is FIRING, and the whole delta is now derived — 2026-08-12

**Nothing was built or run for this section.** The one measurement it rests on
is W7-62's run of
`cargo test -p cratonvm-native-builtins --test stub_ratchet -- --nocapture`,
which printed

```text
stub-ratchet: const BASELINE_SYNTHETIC_STUBS_NO_MANAGEMENT: usize = 1261;
... exceeding the frozen baseline of 1253 ... 8 passed; 1 failed
```

Everything below is source-verified against today's tree. **`+8 = +6 +2`, and
neither half is a new fake.**

### 11.1 The derivation, in full

| term | value | where it is verified |
|---|---:|---|
| frozen baseline, no-management | 1253 | `native-builtins/tests/stub_ratchet.rs`, `BASELINE_SYNTHETIC_STUBS_NO_MANAGEMENT` |
| `java.util.logging` shadow retirements | +6 | `native-api/src/retired_shadow.rs` |
| the `Cipher` policy-door pair | +2 | `native-builtins/src/jca/cipher.rs` |
| **derived** | **1261** | equals the measured line above, exactly |

**The +6** is the one the ratchet's own doc comment already predicts (cause
(a), `stub_ratchet.rs`), and both edits are still live in the table today:
`LogManager.getLogManager` / `LogManager.getLogger`, and the four `LogRecord`
source-pair triples (`getSourceClassName`, `getSourceMethodName`,
`setSourceClassName`, `setSourceMethodName`) under the comment recording that
they were "retired 2026-08-12 as a SET". A triple in `RETIRED_SHADOW_TRIPLES`
lands on `SyntheticStub` through `register()`'s retired-shadow arm whatever
kind the site states, so these six moved `Bridge`/`Intrinsic` → `SyntheticStub`
**without one new fabricated method**. That is the direction the gate is *not*
built to catch, which is precisely why it must be attributed rather than
absorbed.

**The +2 is the term the ratchet's prediction is missing.**
`native-builtins/src/jca/cipher.rs` registers

* `javax/crypto/Cipher.getMaxAllowedKeyLength(Ljava/lang/String;)I`
* `javax/crypto/Cipher.getMaxAllowedParameterSpec(Ljava/lang/String;)Ljava/security/spec/AlgorithmParameterSpec;`

(W7-93's wild victim: they exist to keep `JceSecurityManager.<clinit>` off the
path). Four facts put them, and only them, in this census's population — each
read out of the tree, none inferred:

1. They are inside `register_cipher_clinit_shim`'s **`SyntheticStub` window**:
   the registrar takes `current_category()`, calls
   `set_category(NativeKind::SyntheticStub)` at its top and restores at its
   very last line, and both registrations sit between the two. They are plain
   `r.register` calls, so the ambient decides, and the ambient is a stub. That
   was deliberate — W7-62 records that they were left that kind so strict
   no-stubs mode drops the family together with the sibling `isRestricted`.
2. The registrar **is on the censused path**: `register_cipher_clinit_shim` is
   called from `native-builtins/src/lib.rs`, inside
   `register_essential_natives_with_shims`, which is `VM_INIT_SEQUENCE`'s first
   entry and therefore replayed by `register_boot_path`.
3. They are **two rows, not more**: two `register` calls, and no `alias_class`
   in the workspace names `javax/crypto/Cipher`, so the replay-synthesised rows
   §6.2 warns about cannot multiply them.
4. They are **new since the freeze** — they are this campaign's repair of
   `RCrypto`, which is why the frozen 1253 does not hold them.

`1253 + 6 + 2 = 1261`, and the measured line says 1261. The earlier "+6
unattributed" was drift measured from the **stale frozen 1253** instead of from
the test's own **prediction of 1259** — the same reading error §2 records twice
in the other direction.

### 11.2 What may be re-frozen, and what may not

The governing rule is this record's and W7-62's shared one: **a firing gate is
loud, a wrongly frozen one is silent forever**, and *never freeze a number you
cannot derive*. Applied here the two constants come out differently, and they
must not be moved in one gesture:

* **`BASELINE_SYNTHETIC_STUBS_NO_MANAGEMENT` 1253 → 1261 is safe.** It is both
  *measured* (the printed recount line, from the configuration named by
  `BASELINE_CONST` in that same run) and *derived* (§11.1), and the two agree
  to the row. Land it with §11.1's attribution written next to it, per the
  gate's own failure text.
* **`BASELINE_SYNTHETIC_STUBS_MANAGEMENT` 1263 → 1271 is DERIVED ONLY, and the
  derivation should be stated as such if it is landed without a run.** All
  eight rows land in registrars that both configurations run
  (`register_phase54_logging_extras` and `register_cipher_clinit_shim`, both
  reached from `register_essential_natives_with_shims`), so both baselines move
  by the same +8. The two configurations differ *only* by the ten `jmx::*`
  registrars — verified: `register_vm_management_impl`, `register_jmx_natives`,
  `register_thread_impl`, `register_class_loading_impl`,
  `register_garbage_collector_impl`, `register_memory_pool_impl`,
  `register_memory_manager_impl`, `register_operating_system_impl`,
  `register_hotspot_diagnostic`, `register_flag_impl` all live in
  `native-builtins/src/jmx.rs` — and
  `git diff 167bf048c..HEAD -- native-builtins/src/jmx.rs` (the freeze commit,
  confirmed an ancestor of HEAD with `git merge-base --is-ancestor`, never by
  timestamp) touches **no** `.register`, `set_category` or `NativeKind` line.
  So the 10-stub gap between the two constants is unchanged since the freeze
  and `1263 + 8 = 1271`.

  Its one weakness is the one §5's own instruction names: 182 commits separate
  the freeze from HEAD, and this derivation's only cross-check is that the
  *same* derivation predicted the no-management number exactly. **Prefer one
  `--features management` run.** What must not happen is the number being
  pasted from a no-management run — that is the 1038-for-1032 error this file
  already carries once.
* **Do not freeze the observed number bare.** The attribution is the licence,
  not the arithmetic. Anything a future run reports beyond §11.1's eight terms
  is a new finding, and freezing over it converts the signal into a permanent
  lie.

### 11.3 The prediction in `stub_ratchet.rs` is short by exactly the Cipher pair

The doc comment says *"expect **1269 / 1259**, and expect this gate to fail
until re-frozen"* and lists three causes (a)/(b)/(c). Cause (a) is the +6;
(b) correctly moves nothing; (c) has not landed. The `Cipher` pair is a fourth
cause and is not in the list, so the prediction is 1271 / 1261 and the file does
not say so. A prediction that is two low is how "+6 unattributed" got written
down — the number a reader diffs against has to be the whole model.

`stub_ratchet.rs` is not this lane's file; the correction is nominated, not
applied. The text to add is a cause **(d)**: *the two `javax/crypto/Cipher`
policy-door natives registered inside `register_cipher_clinit_shim`'s
`SyntheticStub` window — **+2**, a new registration on a real JDK class rather
than a retirement, and the only one of the four causes the gate is actually
built to catch*, together with the corrected `1271 / 1261`.

That last clause is the part worth arguing about before the re-freeze: unlike
the six retirements, these two rows *are* the species this ratchet exists to
report — a `SyntheticStub` newly standing in front of real JDK bytecode. They
were admitted knowingly (W7-93 §2 measures the real path dying in
`JceSecurityManager.<clinit>`, and the two natives are what keep it off the
path), and the correct end state is deleting them once the `StackWalker$Option`
`<clinit>` registration is made conditional on the runtime mode (W7-93 §7.1).
Re-freezing them in is right; re-freezing them in **silently** would spend the
one signal that says so.

---

## 12. Third instance, 2026-09-11 — the DUMP could not attribute what the ratchet could

Lane 2's wave 2 retired 50 triples under `java/lang` and `java/math`. **The
ratchet saw it.** Measured against a binary built from `origin/dev` (`6d16472f5`,
which sits exactly on its own baseline in all three arms), so the whole delta is
the branch's:

```text
  arm              dev    branch   delta   total registrations
  no-management   2547     2577     +30    13610 -> 13610
  management      2558     2604     +46    13978 -> 13978
  synthetic-jdk   2547     2577     +30    13645 -> 13645
```

30 added registrations in the boot arm, **0 removed**, every one a lane-2 triple,
taken with `CRATONVM_RATCHET_ROWS=1` on both binaries and `comm`-diffed. Totals
unmoved: case (b), exactly as the second-column rule says.

**What could NOT see it is `dump_synthetic_stubs`**, which is byte-identical
between the two binaries — 2167 rows, `comm` reporting zero either side. That is
not a contradiction, and the reason is the finding:

> The ratchet counts **registrations**; the dump prints **distinct triples**.
> These triples are registered more than once —
> `ExceptionInInitializerError.<init>()V` from both `lang_misc.rs` and `lib.rs`,
> `Throwable.initCause` from both `lang_misc.rs` and `reflect_annotations.rs` —
> one registration was already a `SyntheticStub`, and the table re-tags the
> other. **+1 registration, +0 distinct rows.**

### 12.1 So the failure message's own advice can come back empty

The panic text says: *"Run `dump_synthetic_stubs` here and at the commit that
last set `BASELINE_…`, and diff the sorted `@@STUB` lines."* For a delta of this
shape that diff is **empty**, while the number it is meant to explain has moved
by 30. A reader who follows the instruction and finds nothing has two readings
available and the wrong one is more natural: *"the number moved for no reason I
can find"*, or worse, going the other way — *"my retirement changed nothing."*

Lane 2 nearly abandoned its own 50-row table on the strength of that silence, and
only the VM disagreed: `--jdk-only-report` refusals under the two prefixes go
**95 → 144** with zero survivors.

`CRATONVM_RATCHET_ROWS=1` is the instrument that answers it — per-registration,
keyed by registering file, so a duplicate shows up as the two rows it is. It
existed and the failure text did not name it. §13.2 fixes that.

### 12.2 Two measurement errors, and they are the reusable part

Both produced confident wrong numbers, and neither was caught by the gate.

**Cross-tree.** The census on `dev`'s sources was first compared against a
`--dump-native-registry` from a binary built weeks earlier: **452** rows where
the census said `SyntheticStub` and the VM dispatched a `Bridge`. Same-tree the
number is **2**. A census on one tree against a registry dump from another is a
measurement of neither.

**A shared `CARGO_TARGET_DIR`.** With `/data` at 95% both arms were built into
one target dir. The second build printed `Finished in 0.18s`, compiled nothing,
and scored the branch with **dev's binary** — so the two trees read as identical
and the first write-up of this section said the ratchet was blind. Cargo decides
freshness by **mtime**, and a `git merge` writes sources OLDER than artefacts
built after it. Every number in the table above comes from a build asserted by a
non-zero `Compiling` count, with each binary copied out and its sha256 printed
and differing.

### 12.3 Boot census vs the registry the VM dispatches, same tree

A separate and still-valid finding, both sides from one commit:

| | rows |
|---|---|
| boot census `SyntheticStub` | 2167 |
| registry the VM dispatches, `SyntheticStub` | 2296 |
| agree | 2164 |
| boot says stub, the VM dispatches a bridge | 2 |
| **stub the VM dispatches, outside the boot census** | **132** |
| boot stub the VM never registers | 1 |

The 132 by registering file: 54 `native-awt/src/natives.rs`, 25
`native-builtins/src/jmx.rs`, 21 `native-collections/src/lib.rs`, 15
`phases_late/jar_manifest.rs`, 12 `native-builtins/src/lib.rs`, 2
`classloader_real.rs`, 2 `locale_resources.rs`, 1 `vm/src/vm/vm_init.rs`.

That last row is a check on the instrument, not a finding: it is
`INLINE_SYNTHETIC_STUBS_IN_VM_INIT`, which the boot-path module declares as
exactly **1** for `io/quarkus/bootstrap/runner/RunnerClassLoader.close()V`. An
independent census reproducing a constant derived by reading is the cheapest
evidence both are right.

### 12.4 Fourteen of lane 2's rows are a CONFIGURATION boundary, not a scope one

The management arm's delta is +46 against +30. The extra 16 are
`java/lang/management/*` registrations — 14 distinct triples, two registered
twice — from `jmx.rs`, the "ten jmx registrars short of shipping" that
`MEASURED_CONFIG` names. In the no-management arm they are invisible **by
construction**, and no widening of the boot-path replay would change it. Their
configuration has its own constant and that is where they show.

## 13. What was added, 2026-09-11

Two changes, both about the silence rather than the number.

### 13.1 The replay is no longer checked "by review"

The boot-path module header lists, first under *"What this witness still does
not assert"*: **"That the replay CALLS every name in `VM_INIT_SEQUENCE`. Rust
has no reflection over a function body. The list is checked against `vm_init`;
the replay is checked against the list by review."**

Review is not a gate, and the cost of it being wrong is silence of exactly the
kind §1 describes: a registrar named in the sequence and absent from the replay
leaves whatever registered the triple earlier in place, so the census reports
the kind of a row the shipping VM overwrites.

`the_replay_calls_every_name_in_the_sequence` closes it. It reads this file off
disk — the trick `vm_init_source` already plays on `vm_init.rs` — strips comment
lines (this file names registrars in prose on purpose, and the §9 locator defect
was a comment read as source), and asserts every sequence name is called, in the
sequence's order. Order is asserted because registration is last-write-wins: the
right calls in the wrong order replay a different VM.

**It passes as written — 46 of 46, in order.** That is the point worth stating:
the review had in fact held, and the assertion is here so the next merge does
not need it to hold again. Both `RETIRED_SHADOW_TABLES` and this list were
broken by clean `git merge`s the same week, in each case because a name is
declared in one file and consumed in another with nothing textual joining them.

### 13.2 The panic text now names an instrument that can answer it

`synthetic_stub_count_does_not_regress` told a reader to attribute a delta by
diffing `dump_synthetic_stubs`. §12.1 is a delta for which that diff is empty, so
the instruction could send a reader away with nothing and no hint that a stronger
tool existed. It now names `CRATONVM_RATCHET_ROWS=1` first — per-registration,
keyed by registering file — and says in the message itself why the dump can be
silent: it prints DISTINCT triples while the count is REGISTRATIONS.

A second line, `stub-ratchet(scope):`, prints on **every** run, pass or fail. It
carries the boot-path scope and the 132. The failure message was the wrong place
for that: a motionless count reaches no failure message at all, which is exactly
the case that misled lane 2.

### 13.3 Not done

Widening the census to the dispatched registry. §7 already nominates the shape
— the gate moving to `vm/tests/` on a registration-only helper extracted from
`SharedVm::new` — and §12.3 shows it would not recover lane 2's fourteen anyway,
which are a configuration boundary rather than a scope one. The 54 `native-awt`
and 15 `jar_manifest` rows are the ones such a move would actually buy, and
nobody has priced them.
