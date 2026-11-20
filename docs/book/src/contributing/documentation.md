# Documentation guide

Public documentation describes current behavior, configuration, contracts, and
limits. Update it in the same change as the owning implementation.

| Location | Purpose |
|---|---|
| `README.md` | Project status and quick start. |
| `docs/book/src/` | Navigable user, operator, and contributor manual. |
| `docs/feature-designs/` | Implemented feature contracts and explicit current limits. |
| `docs/architecture/`, `docs/jit/`, `docs/security/` | Maintained subsystem references. |
| `BENCHMARK.md`, `performance.md` | Published performance tables, workload definitions, and measurement conditions. |
| `docs/known-issues/` | Unresolved defects and accepted divergences. |
| `docs/internal/` | Ignored local proposals, investigations, delivery logs, and fixed issues. |

Public docs contain no dated progress notes, delivery diaries, or historical
result dumps. Dates belong only to actionable bug records in `docs/known-issues/`;
accepted backend/platform limitations are undated. Preserve third-party legal
notices and verbatim license text.

Benchmark results describe the VM's performance and belong in public reference
tables. Retain workload sizes, timings, HotSpot ratios, correctness checks,
hardware, runtime configuration, and aggregation method. Remove measurement dates
and the narrative of problems encountered. Raw logs, per-run reports, and the
investigation history belong in the internal archive; their presence does not
make the public result table historical material to delete.

Apply the same rule to technical reference pages: retain supported operations,
configuration, limitations, storage formats, diagnostics, and runnable commands.
A dated progress section should be rewritten into current facts. Archive an
entire page only when its useful public content is obsolete or retained in a
linked public reference.

When fixing an issue, move its write-up to the local internal archive. Put its
durable behavior and invariants in the relevant public feature contract, and
relink every public document and source comment to that contract. Public files
must not depend on internal archive contents.

Use the CLI parser, configuration read sites, source constants, and executable
tests as the source of truth. Describe authentic OpenJDK execution as the
foundational architecture and `--compatible` as its optimization layer. Both
policies target HotSpot behavior; embedding defaults are an explicit API contract.

Run `python tools/check_markdown_links.py` before submitting documentation changes.
