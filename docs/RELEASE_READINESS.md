# Release readiness

CratonVM is experimental software. A release candidate is ready for review
only when the evidence below is attached to the release commit; none of these
checks makes the VM a security boundary for hostile Java code.

## Required evidence

- `cargo fmt --all -- --check`, workspace build, Clippy, and workspace tests pass.
- Default-feature and `synthetic-jdk` test configurations pass.
- The HotSpot differential regression suite and semantic-differential ledger
  report no new divergence.
- The exact synthetic-stub ratchet does not increase.
- Maintained Markdown links pass `python tools/check_markdown_links.py`.
- Coverage generation completes and its LCOV artifact is retained. The project
  does not currently assert a minimum percentage until a reproducible baseline
  has been measured on the release CI environment.
- Security-sensitive parser fuzz targets compile and the core pointer/value
  representation passes its Miri job.
- Performance results include the named framework and CPU regression workloads;
  no benchmark claim is updated without the raw comparison.
- The release binary is built from the tagged commit and smoke-tested with the
  documented JDK version on every supported host platform.

## Explicit non-claims

A green checklist does not establish sandboxing, production suitability,
complete Java compatibility, or a particular coverage percentage. Those claims
require separate evidence and must agree with [SECURITY.md](../SECURITY.md),
[coverage](COVERAGE.md), and the [compatibility policy](book/src/reference/compatibility-policy.md).
