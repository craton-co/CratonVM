# G85-1 — six retags that were no-ops, and the test I never wrote

**Status:** CORRECTION. Six commits reverted. The real figure for the P0
over-tagging row this session is **54 registrations**, not the 433 I reported.
**Provenance:** `--dump-native-registry`, default mode, 2026-08-19, after the
sixth retag. `java/util/ArrayDeque` → `{'bridge': 34}`; same for `Vector`,
`HashMap`, `Optional`, `LinkedList`. Zero `synthetic-stub` among them.

---

## 0. What happened

I retagged six `native-collections` registrars by wrapping their call sites:

```rust
r.with_category(NativeKind::SyntheticStub, register_array_deque_natives);
```

**Every one was a no-op.** Those registrars set their OWN category at their
head:

```rust
fn register_hashset_natives(r: &mut NativeMethodRegistry) {
    let __prev_cat = r.current_category();
    r.set_category(cratonvm_native_api::NativeKind::Bridge);
```

An inner explicit `set_category` beats an outer ambient window, which is the
whole point of having both. So the wrapper changed the ambient tag on the way
in, and the function immediately overrode it. **48 of this crate's 60
registrars are built this way**; only 12 inherit the ambient tag. The AWT
retag worked precisely because `native-awt`'s registrars are among the
inheriting kind.

Reported totals of 180, 209, 349 and 433 were therefore all inflated. The
correct number for this session is **54**, from `native-awt` alone.

## 1. Why five rounds of testing did not catch it

Each retag was verified two ways, and both were blind to this:

* **the arms** — 101/101, 96/101, 61/62 every time. Green is consistent with
  "the retag is safe" AND with "the retag did nothing". Nothing distinguished
  them.
* **per-vector spot-checks** — `RCollections` 53 checks, `RJdkBridge1` 483,
  `ROverlaySystemGcStress` 127,920. All behavioural. A no-op passes every
  behavioural test by construction, and the bigger the number the more
  convincing the illusion.

**The missing test is one line**: re-dump the registry and assert the tag
moved. I had that instrument, used it to CHOOSE each registrar, and never once
pointed it at the result.

## 2. The part that is genuinely uncomfortable

This session's recurring finding — stated in `G82-1`, `G83-1`, `G84-1` and
`G79-1` — is that rows go stale because numbers get cited instead of
re-measured, and that instruments must be RUN rather than referenced. I wrote
that four times, then produced five commits of numbers I never re-measured,
each one asserting a specific count in its message and in an in-code comment.

The failure is not that a subtle mechanism defeated me. It is that I verified
the thing I expected to be fragile (behaviour) and not the thing I was actually
claiming (the tag).

## 3. What was reverted, and what was kept

**Reverted** — six commits' worth of changes to `native-collections/src/lib.rs`
(`2cb61a689`, `87a32c9f4`, `d0d4933a9`, `ee0425c09`, `987977daf`, `ac1ba478f`,
`956374dae`). Their code was inert, but their COMMENTS claimed measured retags
that never happened; leaving them would tell the next reader `ArrayDeque` is
retagged when it is not. That is worse than no comment.

**Kept** — `regression-suite/probes/NotDeclaredSplit.java` and its findings,
which stand on their own and are independent of the failed retags:

* the crate's 534 not-declared-here rows split **271 inherited-concrete /
  254 ABSTRACT-INTERFACE / 9 absent**;
* `register_set_view_carrier_natives`' 88 not-declared-here rows are **all
  inherited-concrete, none abstract-interface** — so the largest registrar is
  not the dangerous one, and my published claim that it was "where not to
  start" was wrong on the evidence;
* the 254 genuinely abstract-interface rows live in the STREAM and INTERFACE
  registrars, and those remain the ones to leave alone;
* the probe's own first version mis-reported constructors as absent
  (`getMethods()` cannot see `<init>`), which is fixed and is why the absent
  count moved 14 → 9.

## 3a. A SECOND constraint, found by a retag that broke

After the corrected procedure was working, a batch of four small registrars
(`stack` 18, `linked_hashmap` 23, `chm_key_set_view` 23, `priority_queue` 15)
passed the tag assertion — all four moved to `synthetic-stub` — and then
**failed the arms**: `RCollections`, `RStrings` and `RChmKeySetView` all red.

```
java.lang.NullPointerException: Cannot read the array length because "a" is null
    at RCollections.main(RCollections.java:59)          // new ArrayList<>(lh.keySet())
    at java/util/ArrayList.<init>(ArrayList.java:183)   // c.toArray() returned null
```

**Retagging drops the shim, but the OBJECT LAYOUT is still ours.** A
`LinkedHashMap` built by our natives has our field layout; drop the `keySet()`
shim and real JDK bytecode runs against an instance whose real internals were
never populated, so the view it returns is empty and `toArray()` answers null.

That is a different constraint from anything in §0–§3, and it is the one that
actually bounds this work:

> A registrar is safe to retag only if the OBJECTS its methods receive are real
> too. Rule 4's "concrete bytecode + incomplete replacement → delete from the
> strict path" assumes the real bytecode can READ the receiver. Where the VM
> mints the object itself, it cannot.

**This retroactively weakens my earlier confidence, and the record should say
so.** `Vector` (28 registrations) was retagged and the arms stayed green — but
it had **0 invocations**, meaning nothing in the corpus exercised it. Green
arms over an unexercised surface prove nothing about it. The
`unmodifiable` retag (300) is on firmer ground because it was measured
DIRECTLY: `Collections.unmodifiableList(...)` returns the real
`java.util.Collections$UnmodifiableRandomAccessList` under `--jdk-only`, so
real objects are already in play there. `Comparator` (14, 0 invocations) and
the collection factories (36, 61 invocations, real `List.of` objects) sit
between those two poles.

The batch of four was reverted; it was never committed.

## 3b. So the verification a retag needs is THREE things, not one

1. **the tag moved** — from a registry dump (§1, the test I first missed);
2. **the behaviour holds** — the arms (which caught this one);
3. **the surface was actually exercised** — non-zero invocations. Without this,
   (2) is vacuous: green arms over an unexercised surface prove nothing.

**And a shortcut that does NOT work, tested rather than assumed.** The obvious
way to satisfy (3) cheaply is to ask whether `--jdk-only` hands back real JDK
objects — if the object is real, surely real bytecode can read it.
`regression-suite/probes/RealObjectCheck.java` asks exactly that across 31
containers and derived views, and answers **0 of 31 diverging**: every one,
`LinkedHashMap.keySet()` included, already carries its real JDK class name.

Yet retagging `LinkedHashMap` broke precisely that call. **Class identity is
not state ownership.** The object really is a `java.util.LinkedHashMap`; what
our `put()` shim maintains is a side structure rather than the real
`table`/`head`/`tail` fields, so real `keySet()` bytecode reads a real object
whose real fields were never populated.

The probe is kept because a RED line is conclusive the other way — a stand-in
class name disqualifies a registrar outright. It is a cheap disqualifier, never
a certificate.

Only the `unmodifiable` retag in this session satisfies all three on its own
evidence.

## 4. NOMINATIONS

**N1 — retagging these 48 registrars means editing their explicit
`set_category` line, which is a different act.** Wrapping a call site changes
an ambient default; changing `set_category(Bridge)` overrides a judgement
somebody wrote deliberately. The ambient-category audit's guidance ("never move
an existing `set_category` line") is about not relocating them; this is about
not overriding one without knowing why it is there. Each of the 48 needs its
tag's rationale read before it is changed — and several have none written
down, which is itself worth recording.

**N2 — every retag PR must assert the tag moved.** Dump the registry after the
change and check the affected class's `kind`. It is one command, it is the only
test that distinguishes a safe retag from an inert one, and its absence made
five commits of false progress look like five commits of verified progress.

**N3 — DONE for one, and the corrected procedure is demonstrated end to end.**
The 12 ambient-inheriting registrars are the ones the call-site technique works
on, and they are identified mechanically: no `set_category(NativeKind::Bridge)`
in the body. `register_comparator_natives` (14 registrations, 0 invocations, 0
`overwrote`, 12 declared-with-code) was retagged and **the tag move was asserted
from a registry dump BEFORE any behavioural test** — `{'synthetic-stub': 14}`.
Then the arms: 101/101, 96/101, 61/62.

That ordering is the whole correction. Dump first, arms second. A no-op passes
the arms; it cannot pass the dump.

The remaining 11: `register_unmodifiable_natives` (300 registrations, 137
invocations, and 300 not-declared-here rows that the reflection pass has not
resolved — NOT a next candidate without that work), `register_factory_natives`
(36, all declared-with-code, but 60 invocations and adjacent to the already-red
`RImmutableFactoryTypes`), `register_linked_blocking_deque_stub_natives` (15),
`register_string_joiner_natives_with_category` (7),
`register_map_conditional_mutators` (3),
`register_hibernate_persistent_map_natives` (2), and five with no
registrations of their own.
