# Documentation Guide

Documentation is part of the implementation contract. A code change is not
complete when the user-facing behavior, architecture invariant, configuration,
or known-issue status has changed but the manual still describes the old tree.

## Documentation layers

| Location | Role |
|----------|------|
| `README.md` | Project landing page, quick start, headline status. |
| `docs/book/src/` | Canonical navigable user, operator, architecture, and contributor manual. |
| Root guides such as `ARCHITECTURE.md` and `BENCHMARK.md` | Deep standalone references and evidence. |
| `docs/*.md` | Focused reference tables and subsystem guides linked by the manual. |
| `docs/architecture/` | Current architectural design/implementation notes useful outside a single incident. |
| `docs/feature-designs/` | Forward-looking proposals; not proof that a feature has landed. |
| `docs/known-issues/` | Only unresolved defects and active investigations. |
| `` | Historical evidence, fixed-issue reports, audit trails, and non-normative internal notes. |

Do not create a second canonical guide for a topic already owned by the book.
Extend the book and link to a deep standalone reference when the detail would
overwhelm the chapter.

## Bug-document lifecycle

The repository policy is:

1. create an unresolved bug document under `docs/known-issues`;
2. include reproduction, expected/actual behavior, current evidence, and owner
   or next experiment;
3. land an executable regression test or probe with the fix;
4. update the document with the cause, fix, and validation; and
5. move it under `docs/internal`.

Fixed documents must not remain under `docs/known-issues`. Internal retention
preserves useful history without presenting completed work as open.

## Sources of truth

Prefer generated or executable truth:

- CLI options: `vm-cli/src/main.rs` and `cratonvm --help`;
- declared VM flags/defaults: `types/src/flags.rs` and the configuration tests;
- Cargo topology: workspace manifests and `cargo metadata`;
- lock order: `vm/src/runtime/lock_order.rs`;
- generated-code offsets: layout constants plus contract tests;
- native coverage: registry inventory tools;
- benchmark results: raw checked-in output plus binary/host provenance.

If prose duplicates a constant, link to its owner and add a test when feasible.
Avoid line numbers in durable docs because large generated/native files move
frequently.

## Writing rules

- State whether a feature is default, opt-in, experimental, partial, or a stub.
- Separate current behavior from a proposal or historical measurement.
- Give complete commands with prerequisites and working directory.
- Use repository-relative links.
- Do not promise certification, audit status, or production support that the
  project has not earned.
- Do not describe synthetic stubs as equivalent to real JDK bytecode.
- For unsafe/concurrent behavior, document the invariant and fail-closed path,
  not only the happy path.
- Preserve exact exception types, checksums, commits, and artifact hashes in
  evidence documents.

## Performance claims

Every published comparison should include:

- workload and input size;
- correctness checksum or equivalent semantic assertion;
- CratonVM and reference-JDK versions;
- Git commit and binary identity;
- build profile, including LTO;
- heap and VM flags;
- host/CPU and load;
- sample order and count;
- all samples or a linked raw result file; and
- the aggregation rule, normally median.

Loaded-host timings can be useful diagnostics but must be labeled. Never
silently drop an outlier or re-anchor a baseline to make a gate green.

## Adding a chapter

1. Choose the existing section whose readers need it.
2. Add the Markdown file under `docs/book/src/<section>/`.
3. Add it to `docs/book/src/SUMMARY.md`.
4. Link it from `docs/README.md` and nearby chapters.
5. Ensure every relative link and local image resolves.
6. Build the book when `mdbook` is available.
7. Run the repository Markdown link check.

Keep chapter titles task-oriented. "Operating CratonVM in a container" is
easier to find than an internal subsystem nickname.

## Updating architecture documentation

An architecture change should update:

- crate/component ownership;
- dependency direction;
- runtime lifecycle or state machine;
- cross-boundary safety contracts;
- failure/fallback behavior;
- configuration ownership;
- executable validation; and
- any old design document whose status changed.

When a proposal lands, mark or move the proposal so readers do not implement
the completed design a second time.

## Review checklist

- [ ] Claims match the current `dev` tree.
- [ ] New pages are present in `SUMMARY.md`.
- [ ] Links resolve with exact filename case.
- [ ] Commands use current package/binary names.
- [ ] Defaults and experimental status are explicit.
- [ ] Security caveats are preserved.
- [ ] Benchmarks include correctness and provenance.
- [ ] Open/fixed issue documents are in the correct directory.
- [ ] No generated build output or secrets are checked in.
- [ ] The manual and root entry points cross-link the change.
