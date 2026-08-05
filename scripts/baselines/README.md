# `scripts/baselines/`

Committed baselines for the gates in `scripts/`. One file per gate.

| File | Gate | Taken by |
|---|---|---|
| `jdk-only-bridge-ratchet.json` | [`scripts/jdk-only-bridge-ratchet.py`](../jdk-only-bridge-ratchet.py) — the unadjudicated-`Bridge` ratchet (wave-2 lane L6) | [`regression-suite/bridge-ratchet.sh`](../../regression-suite/bridge-ratchet.sh) |

`tools/jdk-only-blockers/baselines/` is a *different* directory for a different
gate (the design-§6 blocker pair) and is not related to these.

## Rules

**Never hand-edit a number.** Every entry is written by the gate itself from a
census it took, with `--update-baseline`. A hand-written figure is a claim, and
this feature has produced five wrong ones — "about 8,000" registrations (11,916),
"1,195 mis-tagged" (10,069), "52 call sites" (3 fire).

**Entries are keyed by `<jdk-feature>/<os>`.** The measurements adjudicate
registrations against one runtime image, and the registrars are
platform-conditional, so an entry answers for exactly one (image, platform)
pair. A run with no matching entry is refused (exit 2), never scored against a
neighbouring key.

**`SLACK` is zero and stays zero.** A count that improves is not absorbed
automatically: the gate prints a re-freeze instruction and passes, so locking
the improvement in is a deliberate act in the same change. A slack-free ratchet
left at the old number silently re-admits exactly that many new violations.

**Record why it moved.** `--note` is written into the entry. A baseline that
moved for an unstated reason is indistinguishable from one that moved by
accident; the gate says so when the note is empty.
