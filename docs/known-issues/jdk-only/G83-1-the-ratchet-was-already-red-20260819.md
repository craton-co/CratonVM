# G83-1 — the ratchet a P0 row cites has been red, and nobody ran it

**Status:** MEASURED. Nothing fixed — the failure is PRE-EXISTING, its remedy is
explicitly not "raise the baseline", and the work is CONTRACTUALLY DEFERRED to a
later wave (§3a). Three published numbers corrected.
**Provenance:** `cargo test -p cratonvm-native-builtins --test stub_ratchet`,
run 2026-08-19 on `90d27779d` and again on `8a7e2727f` in a clean worktree.
Registry composition from `--dump-native-registry` on both modes.

This follows `G82-1` N2 — *look for rows waiting on execution, not on work* —
applied to the P0 **Residual synthetic native set** row. It found the opposite
of a closure, which is why it is recorded rather than quietly dropped.

---

## 0. The finding

The P0 row cites `native-builtins/tests/stub_ratchet.rs` as its evidence.

**That test fails, and has been failing before anything in this session.**

```
stub-ratchet [no-management]: 1308 SyntheticStub registrations out of 12792
total (baseline 1277, slack 0)

STUB-RATCHET REGRESSION … 1308 SyntheticStub natives now registered,
exceeding the frozen baseline of 1277. A change added a NEW synthetic stub.
Make the new native a real Bridge/Intrinsic (correct behavior) instead of a
fake — do NOT just raise the baseline.
```

Verified not to be mine: a clean worktree at `8a7e2727f` — the last commit
before this session touched `native-awt` — reports **the same 1308**. Nine of
the file's ten other tests pass; this one is 31 over its frozen ceiling.

It went unnoticed for the reason the row itself records in its provenance
note: *"**Not executed**: no `cargo` command was run this session, so the tests
are cited as present, not as passing."* An evidence citation that nobody
executes is a claim, not evidence.

## 1. Two numbers in the row are wrong

**The baseline.** The row says `BASELINE_SYNTHETIC_STUBS = 157` exactly, with
`SLACK = 0`. The file has **two** baselines, chosen by feature:

```rust
const BASELINE_SYNTHETIC_STUBS_MANAGEMENT:    usize = 1287;
const BASELINE_SYNTHETIC_STUBS_NO_MANAGEMENT: usize = 1277;
```

`157` appears in neither. Whatever it described, the tree moved on and the row
did not.

**The population.** The row treats the 157 as "the residual synthetic native
set" to be classified down to zero. Measured from the registry dump:

| mode | total | `SyntheticStub` | `Bridge` | `Intrinsic` |
| --- | ---: | ---: | ---: | ---: |
| default (compatible) | 12079 | **1330** | 10104 | 645 |
| `--jdk-only` (strict) | 10710 | **0** | 10065 | 645 |

Not 157 in either direction.

## 2. The strict count is already zero, and that means less than it looks

The row's required resolution says **"Strict final count must be zero."** It
already is — measurably, today, 0 of 10710.

**But not because anything was classified.** `SyntheticStub` is not
`allowed_in(JdkOnly)` (`native-api/src/registry.rs`), so all 1330 are DROPPED
at registration under strict mode. Nothing was resolved into a bridge, an
intrinsic, or a deletion; the population was filtered out of the measurement.

That makes "strict final count must be zero" a criterion the over-tagging
defect satisfies trivially. Worse, the two defects interact: `G79-1` measured
that mis-tagging a shim as `Bridge` keeps it ALIVE in strict mode, and this row
measures the count of things tagged `SyntheticStub`. **Retagging a fake from
`SyntheticStub` to `Bridge` improves both numbers while making the VM less
correct** — the stub count falls and the fake now survives strict mode. Two
metrics that a single wrong move improves at once are not, together, a
safeguard.

## 3. What this does NOT license

It does not license closing the row. The strict zero is an artefact, the
classification work the row actually asks for has not been done, and the row's
own ratchet is red.

It does not license raising the baseline to 1308, which the failure message
explicitly forbids and which would erase the only signal that 31 stubs appeared.

## 3a. THE END-STATE GATE — and the contract clause that defers it

The same file carries an `#[ignore]`d test, `strict_mode_refuses_nothing`,
labelled **"THE END-STATE GATE"**. Run on demand (`-- --ignored`), it reports:

```
1328 SyntheticStub registrations still have to be refused at VM init.
Zero refusals is the real end state: it means the stubs were reclassified or
deleted at the source, not merely filtered out of the table on the way in.
```

**1328** — a THIRD figure for this population, alongside the row's `157` and
this test's own doc comment saying `549`. The measured registry says 1330. Only
one of those four numbers is re-derived from the tree.

**And the `157` has a source.** It is not a transcription slip in the P0 row: it
is the CONTRACT's own figure. `docs/feature-designs/jdk-only-mode.md` §8, read
directly rather than through a citation, says:

> Do not edit `native-builtins/src/lib.rs`; the **157**-stub reclassification is
> a separate wave with its own subsystem-per-PR discipline.

So the number originates in the normative document, and the P0 row, the
ratchet's doc comment and everything downstream inherited it faithfully while
the tree moved to ~1330. That is a better explanation than "the row went
stale": **the root citation went stale, and three documents copied it
correctly.** Fixing the row without fixing §8 would put them back out of step at
the next re-read.

Its doc comment states plainly why `strict_registry_has_zero_synthetic_stubs`
passing means so little, in words this record reached independently in §2:

> passes today for a weak reason: `register()` refuses the stubs at the door.
> The 549 registrations still exist … and are still what an ordinary
> `--real-jdk` run dispatches into. **Refused is not retired.**

And it lists the three things that must land before the gate can be un-ignored:
reclassify or delete every `SyntheticStub` registration subsystem by subsystem;
drive the baseline to 0 in the same change; un-ignore the test and promote the
CI `jdk-only` job from advisory to blocking.

**The decisive line, for anyone asking why this row is not closed here:**

> This is explicitly *not* wave 1 work (contract §8: "do not edit
> `native-builtins/src/lib.rs`; the 549-stub reclassification is a separate
> wave with its own subsystem-per-PR discipline").

So the remaining work on this row is not merely large — it is **contractually
deferred to a later wave, with a stated discipline (one subsystem per PR), and
the contract forbids editing the file it lives in during wave 1.** A session
that "finished" this row would be violating §8 to do it.

That is worth stating precisely because three separate measurements in this
session pointed the other way — rows that were stale, or already satisfied, or
waiting on a command. This one is not. It is deferred on purpose, and the
deferral is written down.

## 4. NOMINATIONS

**N1 — find the 31, do not re-freeze.** The ratchet is red by 31 registrations
against a `SLACK = 0` ceiling. `git log` on the registrar files plus the
per-registration `registered_by` in a dump will name them; the failure message
is emphatic that the remedy is making each a real `Bridge`/`Intrinsic`, not
moving the line. Until then every future stub regression is masked by this one.

**N2 — put the ratchet in CI, or delete the citation.** A frozen baseline with
`SLACK = 0` that nobody runs is worse than no ratchet: the P0 row cites it as
live evidence, and it has been failing. Either it gates merges or the row
should stop resting on it.

**N3 — the row's criterion needs restating.** "Strict final count must be
zero" is satisfied today by a mechanism that resolves nothing (§2). The
criterion that matches the row's INTENT is about the *default* registry — that
no registration is a compatibility shim wearing another tag — and it needs the
`kind_stated` / `kind_chosen` census fields the row's own retired note says now
answer it, not a count of what strict mode happens to drop.

**N4 — `G82-1` N2 generalises further than one row, in both directions.**
Running what a row cites closed the boot-image row in an afternoon and revealed
this one is worse off than recorded. Both outcomes are worth having, and
neither was reachable by reading.
