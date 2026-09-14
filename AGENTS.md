# Project Memory

- Every unsolved bug document goes in `docs/known-issues`.
- Once a bug is fixed, move its document under `docs/internal`.

## Native registration and JDK-only mode

Contract: `docs/feature-designs/jdk-only-mode.md`. These rules are load-bearing
because getting them wrong fails **silently** — no exception, no log line, no
failing test.

- **`NativeKind` is ambient, and the default is `SyntheticStub`.**
  `NativeMethodRegistry::register` takes no kind; it reads the registry's
  mutable `current_category`, which the constructor initialises to
  `SyntheticStub`. A registrar that does not set its own category inherits
  whatever the call chain left there — so a genuine bridge registered outside a
  `with_category` / `set_category` scope is silently classified a stub, and is
  then **dropped** under `CRATONVM_NO_STUBS` / `--jdk-only`. This has already
  broken a real-JDK boot once. Set the category explicitly in every new
  registrar and restore it on exit. Note that `with_category` given a *function
  pointer* extends the scope over that function's entire dynamic call tree, and
  that a stub created by omission has no syntactic marker — `rg
  'NativeKind::SyntheticStub'` will never find it.
- **Real class bytes are authoritative over registered natives.** Concrete Java
  bytecode beats a registered `Bridge` or `SyntheticStub`; only a reviewed
  `Intrinsic` may win. Registration is last-write-wins, so the kind is decided
  by the *last* registration of a triple. When dispatch picks the wrong side,
  fix the resolution through `resolve_dispatch` — do not add another hard-coded
  class-name allow-list. Several already exist, in disagreeing copies, and each
  new one makes the next removal harder.
- **A new compatibility stub needs an explicit classification and a removal
  issue.** State the `NativeKind` (or `ClassOrigin`) at the site rather than
  inheriting it, and file the record under `docs/known-issues/jdk-only/` saying
  what would have to exist for the stub to go away. `native-builtins/tests/stub_ratchet.rs`
  freezes the synthetic-stub baseline exactly, with zero slack, so an
  unclassified addition fails CI rather than accumulating quietly.
- **The default `compatible` mode must stay byte-for-byte unchanged.** Strict
  mode is a separate policy, never inferred from a Cargo feature or an
  environment variable. Most of the dangerous mistakes recorded in
  `docs/known-issues/jdk-only/` were `compatible`-mode behaviour changes made
  while intending to fix strict mode.
- **No process globals for compatibility state.** It is per-VM (`VmConfig` →
  native registry, `ClassManager`). Two pre-existing globals are already logged
  as violations to remove; do not add a third.
