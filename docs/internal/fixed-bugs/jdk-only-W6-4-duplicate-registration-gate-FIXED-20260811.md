> **FIXED 2026-08-11 — moved out of `docs/known-issues/jdk-only/`.**
>
> It owns **no corpus vector of its own** — it is an instrument record. The gate ships and is seeded from its first real run (1206 shadowed registrations, 53 with a kind disagreement); the analysis API is in `native-api` and the ratchet in `native-builtins/tests/duplicate_registration_gate.rs`. The one live defect it filed, "The unjustified one", was REFUTED in the record's own margin by lane W6-12 — the losing `StampedLock` registrar had been dead at its call site since 2026-07-28 — and its "second candidate" (`Phaser`) was confirmed and fixed by that same lane. What is left is "What this gate does not cover", which states the instrument's scope, not an unfixed defect.
>
> Previous location: `docs/known-issues/jdk-only/W6-4-duplicate-registration-gate.md`.
> Audit that moved it: `docs/known-issues/jdk-only/RETIREMENT-20260811.md`.

# Duplicate registration: the species, the gate, and one live instance

**Status:** the gate ships, and is now **seeded from its first real run**
(2026-08-07): 1206 shadowed registrations, 53 of them with a kind disagreement.
The analysis API is in `native-api`, the ratchet is in
`native-builtins/tests/duplicate_registration_gate.rs`. See *Seeding* for the
numbers, and for why the first run took two months to happen. One previously-unrecorded live defect fell out of building it and is
written up under *The unjustified one*.

Filed 2026-08-07 (JDK-only wave 2, lane W6-4).

> **2026-08-07 — two qualifications on "the gate ships", both verified against
> the tree.**
>
> 1. **Nothing runs it.** `duplicate_registration_gate` appears nowhere in
>    `.github/workflows/ci.yml`, which wires `stub_ratchet` and
>    `regression-suite/bridge-ratchet.sh` but not this. So the seed described
>    under *Seeding* has not been taken by CI either, and the ratchet is not
>    guarding anything until someone adds the step and pastes the numbers.
> 2. **The number, once taken, will be scoped.** The gate's own header says it:
>    it observes only the registrars `vm_init_real_jdk_boot_path` calls, minus
>    everything `register` drops before it can push a row. **The whole
>    synthetic-JDK registration graph is invisible to it** — 154 triples
>    registered by both `register_essential_natives_with_shims` and
>    `register_synthetic_overrides` in `native-builtins/src/lib.rs` alone. Read
>    [§7 of *Natives over real JDK
>    classes*](../../architecture/natives-over-real-jdk-classes.md) before
>    quoting `BASELINE_SHADOWED` anywhere.

## The species

`NativeMethodRegistry::register` is last-write-wins and **updates the existing
slot in place**. A correct, guarded, carefully-reviewed native therefore loses
silently to an unguarded twin registered later, and every symptom points at the
wrong source file. Four instances, each found only after a full
measure-fix-rebuild cycle:

| # | Triple | What the loss looked like |
|---|---|---|
| 1 | `MethodHandles$Lookup.defineHiddenClass` | A placeholder in `lang_invoke.rs` registered LATE shadowed the real implementation in `lookup_define.rs`. It returned a `Lookup` whose slot 0 was never written, so `lookupClass()` answered null. Broke `RJdkHidden` AND `RJdkStrict`. |
| 2 | `Files.copy(Path,Path,CopyOption...)` | The winner never read its options argument, so a copy onto an existing file overwrote instead of throwing `FileAlreadyExistsException`. |
| 3 | `Module.getResourceAsStream` | Registered twice ~9k lines apart. Wave 2's `opens` gate was dead code; an unguarded classpath resolver served resources from any package. |
| 4 | `SSLContext.getInstance` | Four competing registrations disagreeing with each other; the live one threw a `java.io.IOException` whose *message* said `NoSuchAlgorithmException`, so the caller's catch did not match. |

The misdirection that made all four expensive, stated once because it is the
thing that costs the cycle: **forcing "the native" over real bytecode does not
help when the slot holds a DIFFERENT native.** Every instrument that answers
"native or bytecode?" answers *native*, correctly, and says nothing.

## Why it was findable all along

Nothing about this needs new instrumentation. `register` is `#[track_caller]`
and records `Location::caller()`; `registrations` is append-only, so the
**loser's row survives**, tagged `NativeCensusEntry::owns_slot == false`; and
`census()` already formats both provenance strings. The lever existed and
nothing consumed it.

`cratonvm_native_api::registry::shadowed_registrations_in` now does: it groups a
census by triple, takes the `owns_slot` row as the winner, and emits one
`ShadowedRegistration` per loser carrying both `file:line` sites and both
`NativeKind`s.

## The gate

`native-builtins/tests/duplicate_registration_gate.rs`, four tests:

* `no_new_shadowed_registrations` — ratchet on the number of displaced
  registrations, prints the full loser→winner census.
* `no_new_kind_disagreements_between_a_winner_and_the_native_it_shadows` — the
  high-signal subset: the winner and loser disagree about `NativeKind`, which is
  instance 1 exactly. Never cosmetic — `JdkOnly` refuses a `SyntheticStub`,
  `CRATONVM_NO_STUBS` drops one, and
  `synthetic_stub_kind_should_yield_to_real_bytecode` arbitrates for one.
* `the_replayed_sequence_matches_vm_init` — source witness, green today, no
  measurement. See *Why not the six-call helper*.
* `the_gate_measures_a_populated_registry` — negative control against the gate
  becoming vacuous.

The detector itself is pinned separately in
`native-api/tests/shadowed_registration_detection.rs`, on registries that test
builds itself, so a bug in the analysis cannot quietly turn the ratchets into
tests of nothing.

### Why not the six-call helper

`stub_ratchet.rs::register_boot_path` replays six registrars. `vm_init.rs`'s
real-JDK arm (`#[cfg(not(feature = "synthetic-jdk"))]`, the default
`cratonvm-cli` build) calls **47**, and the helper stops at
`register_collections_natives` — which is one line before a block `vm_init`
labels:

```
╔══ LAST-WRITE-WINS BOUNDARY — do not reorder ═══════════════════════╗
```

That block re-registers `securerandom` and `properties_sidetable` **because**
`register_collections_natives` overwrites them with layout-wrong versions. So a
duplicate census taken over the six-call helper names `native-collections` as
the winner for ~23 `java/util/Properties` triples and the whole `java/util
/Random` family — the exact opposite of what the shipping VM does. A gate is not
allowed to be confidently wrong about the winner column; that is the defect, not
the measurement of it.

Hence `vm_init_real_jdk_boot_path` mirrors `vm_init` call-for-call (45 of 47;
the two `crate::runtime::instrument::*` ones live in the `vm` crate, which
`native-builtins` must not dev-depend on), and the source-witness test fails on
an order inversion with no baseline and ratchets unmodelled registrars at 2.

### Seeding

**Seeded 2026-08-07 at dev `1082eb446`: `BASELINE_SHADOWED = 1206`,
`BASELINE_KIND_DISAGREEMENTS = 53`.** Both are ratchet ceilings, not
approvals — every one of the 1206 is a callback that can never be dispatched,
and each of the 53 is a winner that disagrees with the loser about what the
native IS. They are the number to drive DOWN; the assert only ever forbids
going up.

Why it took until now, since the procedure below is one command: the gate is a
`cargo test` target, and `cargo test --workspace` is the LAST step of ci.yml's
`build-and-test` job, behind `cargo fmt --all --check` — which fails on every
push (2442 diffs at that tip), and GitHub Actions skips every later step once
one fails. So this ratchet had never run in CI at all. The same blindness is
what let a shadowed `java/nio/CharBuffer.toString()` ship an empty string
through every reflective route (`fixed-suite-bugs/stringcharbuffer-tostring-empty-via-native-invoke-FIXED.md`);
deleting that duplicate is why the count reads 1206 and not 1207. Formatting
now runs as its own CI job so it can no longer stand in front of the
correctness gates.

The original seeding note follows, because the reasoning still governs any
future change to these numbers.

`BASELINE_SHADOWED` and `BASELINE_KIND_DISAGREEMENTS` were `0`, so the two
ratchets were **RED until one run pasted the real numbers in**. This is the
procedure `stub_ratchet.rs` documents for its own baseline, and the alternative
is worse: a baseline seeded above the true count is a gate that silently
tolerates every duplicate below it — which is the failure this species already
has. The test prints the exact `const ... = N;` line to paste.

```text
cargo test -p cratonvm-native-builtins --test duplicate_registration_gate -- --nocapture
```

## What the static scan found

A source scan of the boot-reachable registrars (resolving class-name variables
per function, skipping `#[cfg(test)]`, following the call graph from the six
crate-level entry points) resolved 7,135 registration sites and found **483
triples registered from more than one call site** — 445 of them cross-file. That
is a lower bound on the runtime number, since it cannot resolve class names held
in unresolvable expressions (~1,600 sites) and counts triples rather than
losers. It is a sanity check on the seed, not the seed.

Two independent confirmations that the winner column derived from boot order is
right, both from comments written by the people who fixed the bugs:

* `Module.getResourceAsStream` — the scan names `lib.rs` the winner; the source
  there says *"this registration is the LAST one for this triple, so it decides
  the callback."* Now fixed: both sites point at the encapsulating callback.
* `SSLContext.getInstance` — the scan names `net_phase_e.rs::register_re6_ssl_context`
  the winner, which is the arm carrying the W3-7 fix for the wrong-exception-class
  defect. The loser, `phases_late/ssl_security.rs::register_p68_ssl`, is the old
  version and is now dead.

Instances 2, 3 and 4 are therefore closed but **still duplicated**: the losing
registration is inert code. They belong in the baseline as *intended*, with the
justification being the comment already at each site.

## The unjustified one

> **REFUTED 2026-08-07 by lane W6-12 — leave this section as the record of how
> the census misled, not as a defect.** See
> [`W6-12-stampedlock-split-brain.md`](W6-12-stampedlock-split-brain.md). The
> losing registrar `native-collections/src/lib.rs::register_stamped_lock_natives`
> was disabled at its **call site** on 2026-07-28 (`let _ = register_stamped_lock_natives;`,
> a dead-code silencer) and has since been deleted from the file outright — the
> tombstone comment is still there. It never registered anything, so nothing
> below about "the collections version wins those eleven" happened.
>
> **The methodological finding is the durable part**, and it generalises past
> this one case: *a census that counts `r.register(...)` sites inside a registrar
> function does not ask whether the function is reachable.* That is the same
> scoping trap as the runtime one in
> [§7 of *Natives over real JDK
> classes*](../../architecture/natives-over-real-jdk-classes.md) — a registrar
> reachable only from `register_synthetic_overrides` cannot register in real-JDK
> mode at all. W6-12 proposes the missing column: *is the registrar reachable
> from `vm_init` in the configuration under test*. The live split-brain the
> brief was really describing turned out to be `Phaser`.

~~**`java.util.concurrent.locks.StampedLock` is served by two different
implementations with two different state stores, and neither one owns the whole
surface.**~~

Eleven triples — `<init>`, `readLock`, `writeLock`, `unlockRead`, `unlockWrite`,
`tryOptimisticRead`, `validate`, `tryReadLock`, `tryWriteLock`, `isReadLocked`,
`isWriteLocked` — are registered by both:

* `native-builtins/src/util_concurrent_ext.rs::register_stamped_lock_natives`
  (~25 triples, the complete surface), whose handlers key lock state by **object
  address** in `crate::stamped_lock` and are therefore layout-independent;
* `native-collections/src/lib.rs::register_stamped_lock_natives` (11 triples, a
  same-named function in a different crate), whose handlers read
  `ctx.get_field(this, 0)` as an `Int` — a **synthetic 2-field layout**.

`vm_init` calls the native-builtins one at position 4 and
`register_collections_natives` at position 10, and — unlike `Random` and
`Properties` — adds **no repair call afterwards**. So the collections version
wins those eleven, and the other ~14 (`unlock(J)V`, `tryUnlockRead`,
`tryUnlockWrite`, `unstampedUnlock*`, `tryConvertToWriteLock`, …) are still
served by native-builtins. `readLock()` and `tryUnlockRead()` therefore operate
on **different state**.

Two reasons this is worth calling out beyond the bug itself:

1. It is the same defect shape `vm_init` already repairs twice, ten lines apart,
   for `Random` (synthetic 2-field layout, seeded `Random` returned all zeroes)
   and `Properties` (legacy HashMap layout). StampedLock has the same shape and
   no repair.
2. It was **partially diagnosed and then filed under the wrong question.** The
   doc comment on the native-builtins registrar records a real
   `--dump-native-registry` measurement — *"every one of the 25 StampedLock
   triples appears three times, twice bridge and once synthetic-stub, and
   registration is last-write-wins, so what actually shipped was decided by call
   ORDER"* — and then adjudicates only the **kind**. The census answers the
   *callback* question with the same two columns and nobody asked it. That is the
   species in miniature: the evidence was on screen and the question was not.

Cross-reference: `stampedlock-surface-must-be-complete-not-partial` argues a
partial surface is the failure mode here. This is a partial surface, split
across two crates.

### Second candidate, less verified

`java.util.concurrent.Phaser` has the identical shape — nine triples registered
by `native-builtins/src/phases_early.rs::register_phaser_natives` and again by
`native-collections/src/lib.rs::register_phaser_natives` (same-named function,
different crate), collections winning, no repair call. Not traced to the field
layouts; flagged for whoever owns those files.

## What this gate does not cover

* **Only the real-JDK arm.** `--jdk-only` (`CompatibilityMode::JdkOnly`) *drops*
  refused registrations before they reach `registrations`, so its duplicate set
  is a subset and a separate baseline. `synthetic-jdk` runs a different arm
  entirely, and a prior lane established that the phase registrars there fire on
  `config.use_synthetic_jdk` **at runtime** (`vm_init.rs:1552`), not merely when
  the Cargo feature is on — so no source-level replay can claim that arm.
* **45 of 47 registrars**; the two `crate::runtime::instrument::*` calls are in
  the `vm` crate. Ratcheted, not assumed.
* **`ShimSelection::ALL`**, where `vm_init` derives the selection from config.
  ALL is the superset, so this over-covers — the safe direction for a ratchet.
* **Registrations made directly by `vm_init`** (inline `native_methods.register(...)`
  closures, e.g. `real_jdk_to_array_typed`) are not replayed.
* **Environment-gated drops.** `CRATONVM_REAL_NET_SOCKETS` and
  `CRATONVM_REAL_FORKJOINPOOL` make `register` refuse whole families, so the
  baseline is only valid with them unset.
