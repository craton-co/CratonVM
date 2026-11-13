# `scripts/baselines/`

Committed baselines for the gates in `scripts/`. One file per gate.

| File | Gate | Taken by |
|---|---|---|
| `jdk-only-bridge-ratchet.json` | `scripts/internal/jdk-only-bridge-ratchet.py` (gitignored; local research tooling) — the unadjudicated-`Bridge` ratchet (wave-2 lane L6) | [`regression-suite/bridge-ratchet.sh`](../../regression-suite/bridge-ratchet.sh) |
| `jdk25-<binary.name>.tsv`, `jdk25-module-<name>.tsv` | the public/protected surface of one JDK 25 class or module, for any guard whose expected set would otherwise be transcribed from the registrar it audits. Read from Rust by [`native-builtins/src/jdk_baseline.rs`](../../native-builtins/src/jdk_baseline.rs) | [`scripts/jdk-baseline/generate.py`](../jdk-baseline/generate.py) — `--update` writes, `--check` diffs, `--verify` runs its known-answer test |

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

**A `jdk25-*.tsv` is not a number but a population, and the rule is the same.**
It is written by `scripts/jdk-baseline/generate.py` from the running JDK's own
image. A row typed by a person is the defect these files exist to remove.
`native-builtins/src/jdk_baseline.rs::parse` enforces what it can: each file is
re-counted against three headers the generator wrote (`# rows`,
`# public-methods` / `# unqualified-exports`), its `# columns` grammar is
checked verbatim, and its `# java.version` is pinned to `25.` so a regeneration
on a newer JDK is a decision rather than a silent re-baselining.

**A baseline these files' consumer does not read is a file that cannot go red.**
`jdk_baseline.rs::ALL` is compared against `read_dir` of this directory in both
directions, so a `jdk25-*.tsv` added here without an `include_str!` fails the
build's tests rather than sitting unread.
