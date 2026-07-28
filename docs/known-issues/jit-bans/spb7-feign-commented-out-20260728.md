# SPB.7 (Feign / OpenFeign) — commented out 2026-07-28, UNVERIFIED

**Status**: commented out (not deleted), no re-verification. Per explicit
user decision — Feign (a Spring Cloud OpenFeign HTTP client, not plain
Spring/Spring Boot) is outside the 5-app scope.

## What it banned

`feign/` (blanket package prefix).

## Original symptom (Session 113 r1)

Spring Cloud OpenFeign builds a `feign.Feign$Builder` that allocates
per-method `MethodMetadata` and `RequestTemplate` objects, each storing
`template`/`headers`/`body` slots immediately after `new` — the classic
allocate-then-putfield corruption pattern. Provisional/pre-emptive ban,
never independently triggered on its own (added alongside SPB.5/SPB.8
family as a defensive measure).

## Why it was never re-verified before being commented out

Feign is a Spring Cloud component (not plain Spring/Spring Boot); no
fixture app using it was ever found on this host. Outside the 5-app
scope this session narrowed to.

## How to restore

In `vm/src/jit/skip_list.rs`, uncomment the `feign/` guard block (search
for `SPB.7`) inside `should_skip_jit_internal`.

## Repro (for whoever re-verifies)

Any real Spring Cloud OpenFeign client under default JIT tiering,
watching for corruption in `MethodMetadata`/`RequestTemplate`
construction during `Feign$Builder` usage.

See also: `docs/known-issues/jit-bans/jit-ban-sweep-consolidated-status-20260726.md`
("Commented out 2026-07-28" section).
