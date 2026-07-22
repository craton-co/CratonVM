# Internal development notes (NON-NORMATIVE)

> **2026-06-22:** still-**OPEN** bug docs that lived here (app-jvm-bugs/, gaps/,
> h2-suite-bugs/, keycloak-crash-reports/, tomcat-suite-bugs/, wildfly-suite-bugs/)
> were **relocated to `docs/known-issues/`** so every unfixed bug lives under
> known-issues. FIXED/resolved bugs (e.g. `fixed-suite-bugs/`, the ✅-RESOLVED
> docs) stay here — a doc moves out of known-issues only when its bug is fixed.
> See `docs/known-issues/README.md` "Open bug docs relocated…".

This folder holds **non-normative internal development notes**: working
artifacts, point-in-time audit findings, and large-app bring-up logs. They are
written for contributors mid-flight and may be **outdated, incomplete, or
superseded** by later work. Treat them as historical scratch, not as a
description of how CratonVM behaves today.

## Do not cite these as current truth

The canonical, authoritative project status lives in the repository root and
the docs index:

- [`../../ROADMAP.md`](../../ROADMAP.md) — current feature status and direction.
- [`../../SECURITY.md`](../../SECURITY.md) — security posture and policy.
- [`../../CHANGELOG.md`](../../CHANGELOG.md) — what actually changed, per release.
- [`../README.md`](../README.md) — index of the maintained documentation set.

If anything in this folder conflicts with those, the documents above win.

## What's in here

At a high level, the files fall into a few kinds:

- **Audit rounds** — per-subsystem findings captured during successive review
  passes (classloading, GC, JIT, JFR, VM, native, concurrency, and so on).
- **Blocker maps and census** — running lists of gaps blocking bring-up of
  large applications such as Keycloak, WildFly, and EJBCA.
- **Divergence log** — recorded points where CratonVM intentionally or
  incidentally differs from HotSpot behaviour.
- **Performance notes** — perf-gap analyses, campaign tracking, and benchmark
  scratch READMEs.
- **Plans and roadmaps** — exploratory plans and bring-up roadmaps that predate
  or feed into the canonical `ROADMAP.md`.
- **Bring-up diagnostics and baselines** — boot/deploy diagnostics and
  regression baselines from specific debugging sessions.

These notes are snapshots. Always confirm against the source tree and the
canonical documents before acting on anything here.
