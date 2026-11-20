# Project Memory

- In `docs/known-issues/` are actionable defects and non-bug divergencies.
- Use Real-JDK terminology in public prose (real JDK execution is the VM's foundational architecture; `--compatible` is the optimization layer).
- Feature proposals live in the public feature contracts (or the public feature contracts).
- Once an issue or bug is fixed, move its document under `docs/internal/`.
- Remote builds and test compiles take 25-40 minutes; polling period must NEVER be less than 5 minutes (300 seconds) to conserve tokens and avoid intermediate status messages.

## Native registration, Foundational Execution, and Compatible Mode

Contract: `docs/feature-designs/real-jdk-mode.md`. By foundational architecture,
CratonVM runs authentic OpenJDK class libraries directly, treating real JDK bytecode
as authoritative. The compatible mode (`--compatible`) is engineered as an
optimization layer for fast cold-start loading and specialized throughput execution paths.
These rules are load-bearing because getting them wrong fails **silently** — no exception, no log line, no
failing test.

- **`NativeKind` is ambient, and the default is `SyntheticStub`.**
  `NativeMethodRegistry::register` takes no kind; it reads the registry's
  mutable `current_category`, which the constructor initialises to
  `SyntheticStub`. A registrar that does not set its own category inherits
  whatever the call chain left there — so a genuine bridge registered outside a
  `with_category` / `set_category` scope is silently classified a stub, and is
  then **dropped** under `CRATONVM_NO_STUBS` / `--real-jdk`. This has already
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
- **Explicit `NativeKind` classification is required.** State the `NativeKind`
  (or `ClassOrigin`) at the site rather than inheriting it so `--real-jdk` and
  `--compatible` dispatch remain deterministic.
- **Both modes target HotSpot, byte for byte.** `--real-jdk` and
  `--compatible` must both produce HotSpot 25's Java-visible behaviour (output,
  exception types and messages, reflection and debugger answers). A change that
  moves `--compatible` closer to HotSpot is a fix, not a drift; there is no
  "keep `--compatible` unchanged" rule and no census is needed to approve one.
  What still holds: strict mode is a separate runtime policy, never inferred
  from a Cargo feature or an environment variable; when you change either mode,
  check the other against HotSpot too (most of the dangerous mistakes recorded
  under `docs/internal/` were a fix for one mode that broke the other). The
  `cratonvm` launcher defaults to foundational `--real-jdk` (`--compatible`, alias
  `--fast-boot`, selects the optimization layer); every embedding entry point
  (`VmConfig::default`, `VmConfig::with_host_jdk_default`, `libcratonvm`,
  `cratonvm-embed`) still defaults to `Compatible`, deliberately; do not
  "align" them with the launcher.
- **No process globals for compatibility state.** It is per-VM (`VmConfig` →
  native registry, `ClassManager`). Two pre-existing globals are already logged
  as violations to remove; do not add a third.

## Public documentation

Public docs describe current behavior and performance. Keep benchmark result
tables, supported capabilities, limitations, and operating instructions public.
Remove measurement dates and investigation narratives without deleting the
results or reference material. Dates are allowed only in actionable known-issue
records; accepted limitations are undated. Keep proposals, delivery notes, raw
run artifacts, and fixed-issue write-ups in ignored `docs/internal/`.
Relink public docs and source comments to maintained feature contracts.
