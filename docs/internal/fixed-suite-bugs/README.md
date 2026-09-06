# `..` — non-public engineering archive

**This directory is slated for removal from public git history.** Treat
everything under it as internal working material with a finite lifetime.

## What lives here

Anything that answers *"how did we get here"* rather than *"what does the VM do
today"*:

- reviews and audits, including the ones that produced the current design;
- fixed-bug write-ups, once the fix and its regression coverage have landed;
- retired, superseded, and closed plans and investigations;
- session logs, dated sweeps, per-round work records;
- raw suite output and comparison handoffs.

Public `../..` holds **current state only**. If a fact in here is still true of
the VM today, it belongs in a public document *as a plain present-tense
statement*, not as a pointer into this tree.

## Two hard rules

1. **No public document may link to a path under `..`.** Those
   links die when this tree is dropped. Inline the durable fact instead, or
   name the retired write-up without a path.
   `types/tests/doc_citation_paths.rs::no_source_file_links_into_docs_internal`
   used to enforce the source-side half of this; it was **switched off on
   2026-09-06** (`#[ignore]`, by request — an in-source link into
   `docs/internal/` is accepted at this stage), so rule 1 is now a convention
   with nothing checking it. Re-arming it is deleting one attribute.
2. **Cite within this tree by a path relative to this directory**, not by a
   `docs/internal/...` path.

`../../../tools/check_markdown_links.py` deliberately excludes this tree from its
default scope; use `--all` to audit it.

## Layout

| Directory | Contents |
|---|---|
| `../audits` | Point-in-time audits of a crate, subsystem, or surface. |
| `../reviews` | Crate and workspace code reviews, and their remediation records. |
| `../fixed-bugs` | Bug write-ups whose fix and regression coverage have landed. |
| `` | The same, for application/test-suite bugs. |
| `../retired` | Retired, superseded, closed, resolved, and done work items. |
| `../feature-designs` | Designs whose work has landed or been abandoned. |
| `../history` | Per-round review records and superseded roadmaps. |
| `../performance` | Closed performance investigations and half-gap analyses. |
| `../gaps` | Open-failure inventories and long-running investigations. |
| `../plans` | Completed or superseded implementation plans. |
| `../repros`, `../gcprobes` | Reproducers and probe kits kept for re-use. |
| `arch-*/` | Dated architecture snapshots. |
| `../gpu`, `../keycloak`, `../springboot`, `../tomcat`, `../comparison-handoff` | Per-target working material. |
| `../jit-bans` | JIT-ban sweep records. |

Loose files at the top level are investigations that have not been given a
disposition yet. Give one — or move the file into the right bucket above —
rather than adding to the pile.
