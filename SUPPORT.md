# Getting Support for CratonVM

CratonVM is an experimental JVM written in Rust. The project is maintained by
[Craton Software Company](https://github.com/craton-co) under the Apache-2.0 license.
This document describes the available support channels.

## Before You Ask

1. Read the relevant docs:
   - [README.md](README.md) — project overview and quick start
   - [BUILD_GUIDE.md](BUILD_GUIDE.md) — building from source
   - [docs/INSTALL.md](docs/INSTALL.md) — installation
   - [docs/TROUBLESHOOTING.md](docs/TROUBLESHOOTING.md) — common problems
   - [docs/CONFIG.md](docs/CONFIG.md) — runtime flags and tuning
2. Search the [existing issues](https://github.com/craton-co/cratonvm/issues?q=is%3Aissue)
   — odds are someone else has hit the same problem.
3. Check the [CHANGELOG.md](CHANGELOG.md) — your issue may already be fixed on
   `main` or scheduled for the next release.

## Community Channels

### GitHub Discussions

For open-ended questions, design discussions, usage help, and "how do I…?"
queries, use
[GitHub Discussions](https://github.com/craton-co/cratonvm/discussions).
This is the preferred channel — answers are searchable and benefit the wider
community.

### GitHub Issues

For confirmed bugs, missing JVM features, and concrete feature requests, file
a [GitHub Issue](https://github.com/craton-co/cratonvm/issues/new/choose) using
the bug-report or feature-request template. Before filing a bug, review the
filing checklist in [CONTRIBUTING.md](CONTRIBUTING.md#reporting-issues) and
include a minimal reproducer (Java source plus the exact command line).

### Email (general inquiries)

For questions that don't fit a public forum (partnerships, press, license
clarifications), email **hello@craton.com.ar**. Please use GitHub Discussions or
Issues for technical support — email is not staffed for engineering help and
will be slower.

## Reporting Security Vulnerabilities

**Do not** file a public GitHub Issue for security problems. Follow the
disclosure process in [SECURITY.md](SECURITY.md), which routes sensitive
reports through GitHub Security Advisories (private). Non-sensitive
security-adjacent issues may be filed with the `security` label.

## Commercial Support

Paid support, prioritized bug fixes, custom feature work, and consulting
engagements are available from Craton Software Company. Contact
**support@craton.com.ar** to discuss a commercial arrangement. Note: as CratonVM
is still pre-1.0 and experimental, commercial offerings are limited and
scoped case-by-case.

## What to Include in a Help Request

For the fastest answer, include:

- The CratonVM version or commit hash (`cargo run -- --version` if available,
  otherwise `git rev-parse HEAD`).
- Your OS and architecture (`uname -a` / `systeminfo`).
- The Rust toolchain version (`rustc --version`).
- The JDK used to compile your `.class` files (`javac -version`).
- The exact command line you ran.
- The full output, ideally with `RUST_LOG=info` set. (`debug` and
  `trace` are compiled out of release builds -- the workspace pins
  `tracing` with `release_max_level_info` -- so setting them on a
  released binary changes nothing. They work only in a debug build.)
- A minimal Java source file that reproduces the problem.

## Response Expectations

CratonVM is maintained primarily by Craton Software Company and volunteer
contributors. There is **no SLA** on community channels. Maintainers aim to
acknowledge new issues within a week, but complex investigations can take
significantly longer. If you need a guaranteed response time, see the
*Commercial Support* section above.

## Contributing Back

If you solve your own problem, please consider:

- Posting the resolution in the original Discussion or Issue.
- Filing a doc PR against [docs/TROUBLESHOOTING.md](docs/TROUBLESHOOTING.md)
  so the next person finds the answer.

See [CONTRIBUTING.md](CONTRIBUTING.md) for the contribution workflow.
