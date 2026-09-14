# CratonVM Governance

CratonVM is an open-source project licensed under Apache-2.0 and stewarded by
[Craton Software Company](https://github.com/craton-co). This document
describes how decisions are made and how individuals take on responsibility
in the project. It is intentionally light — the project is small and the
process should not outweigh the work.

## Principles

1. **Open by default.** Discussions, decisions, and design rationale happen
   in public on GitHub. Private channels are used only for security
   coordination, conduct issues, or legal matters.
2. **Code over committees.** Direction is set by working code, benchmarks,
   and reproducible evidence rather than by debate alone.
3. **Apache Way, lightly.** We borrow lazy consensus, RFC-style design
   documents, and a small set of escalation rules from the Apache Software
   Foundation. We do not run formal foundation votes.

## Roles

### Steward

Craton Software Company is the project Steward. The Steward:

- Owns the trademark and the `craton-co/cratonvm` GitHub organisation.
- Holds tie-breaking authority on Maintainer disputes (used sparingly).
- Coordinates security disclosure (see [SECURITY.md](SECURITY.md)).
- Manages releases, signs published artefacts, and operates infrastructure.

The Steward delegates day-to-day technical authority to Maintainers.

### Maintainer

Maintainers have write access to the repository and the
`@craton-co/cratonvm-maintainers` team. A Maintainer may:

- Approve and merge pull requests.
- Cut release branches and tags.
- Triage issues and apply labels.
- Vote on RFCs and contested decisions.

Maintainers are listed in [MAINTAINERS.md](MAINTAINERS.md).

### Reviewer

Reviewers do not have merge rights but are recognised as trusted reviewers in
one or more subsystems. A Maintainer +1 on a Reviewer-approved PR is sufficient
to merge. Reviewer status is a typical step on the path to Maintainer.

### Contributor

Anyone who opens an issue, files a PR, or participates in Discussions. No
formal status; see [CONTRIBUTING.md](CONTRIBUTING.md).

## Decision-Making

### Lazy Consensus (default)

Most decisions — bug fixes, refactors, small features, doc changes — proceed
by **lazy consensus**: a Maintainer approves the PR, CI passes, no other
Maintainer objects within 72 hours. The change merges.

### RFC Process (major changes)

A change is "major" if it:

- Adds, removes, or breaks a public API or CLI flag.
- Adds a new top-level crate or subsystem.
- Changes the on-disk format of a persisted artefact (JFR, AOT cache, etc.).
- Materially shifts the licence, governance, or security posture.

Major changes require an **RFC**: a markdown document in `docs/rfcs/`
(or as a discussion if no rfcs directory exists yet) describing the
motivation, design, alternatives considered, and migration plan. The RFC is
open for at least seven days of public comment. Merging requires +2 from
Maintainers and no -1 from a Maintainer with a substantive objection.

### Voting (escalation only)

Lazy consensus is the rule; voting is the exception. When a decision is
genuinely contested and discussion has stalled, a Maintainer may call a
vote on the PR or RFC thread. Votes run for seven days. A motion passes if
it has +1 from a simple majority of active Maintainers and no -1 from the
Steward. Abstentions do not count.

In the rare event of a tied vote among Maintainers, the Steward casts the
deciding vote.

## Becoming a Maintainer

Maintainers are added by invitation from the existing Maintainer team
(approved by the Steward). The typical path:

1. **Track record.** Several substantive merged PRs across multiple
   subsystems, sustained over several months. Quality matters more than
   volume.
2. **Review work.** Demonstrated ability to review others' PRs — finding
   real issues, asking good questions, mentoring new contributors.
3. **Sponsor.** An existing Maintainer proposes the candidate on the
   maintainers' channel with a brief case.
4. **Lazy consensus.** If no Maintainer objects within seven days, the
   candidate is invited.

A new Maintainer agrees to uphold the [Code of Conduct](CODE_OF_CONDUCT.md)
and to act in the project's interest rather than any single employer's.

## Stepping Down and Emeritus Status

Maintainers may step down at any time by opening a PR against
[MAINTAINERS.md](MAINTAINERS.md). Maintainers who have been inactive for
twelve months may be moved to emeritus status by lazy consensus of the
active Maintainer team. Emeritus Maintainers keep historical credit but
lose merge rights; they may rejoin without the usual track-record process.

## Conduct and Disputes

All participation is governed by the
[Code of Conduct](CODE_OF_CONDUCT.md). Conduct reports go to the Steward
via the address listed in that document. Technical disagreements are
resolved through the decision-making process above; conduct issues are not.

## Changes to This Document

Changes to `GOVERNANCE.md` are themselves major changes and follow the RFC
process. The Steward must +1 any governance change.
