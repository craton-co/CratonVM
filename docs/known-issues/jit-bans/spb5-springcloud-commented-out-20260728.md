# SPB.5 (Spring Cloud) — commented out 2026-07-28, UNVERIFIED

**Status**: commented out (not deleted), no re-verification. Per explicit
user decision — the 5-app scope is plain `spring`/`spring-boot`, not
Spring **Cloud** (a separate, broader microservices ecosystem the user
did not list).

## What it banned

`org/springframework/cloud/` (blanket package prefix).

## Original symptom (Session 113 r1)

`ms-course-youtube`'s `admin-service` fixture (a Spring Cloud Eureka
client, never found on this host) hit allocate-then-putfield corruption
once the property binder (SPB.4) was unblocked, in Spring Cloud's
`BootstrapApplicationListener`, `ConfigDataLocationResolver`, and
`EnvironmentChangeEvent` plumbing — all heavy `ConcurrentHashMap`/
`LinkedHashMap` allocators. This ban was pre-emptive (added defensively,
never independently triggered on its own).

## Why it was never re-verified before being commented out

The original fixture app (`ms-course-youtube`) was never found on this
host across multiple sessions' exhaustive searches. Spring Cloud is
outside the 5-app scope (plain Spring/Spring Boot only) this session
narrowed to.

## How to restore

In `vm/src/jit/skip_list.rs`, uncomment the `org/springframework/cloud/`
guard block (search for `SPB.5`) inside `should_skip_jit_internal`.

## Repro (for whoever re-verifies)

Any real Spring Cloud application boot (Eureka client, Config Server
client, or Bootstrap context) under default JIT tiering, watching for
corruption in `ConcurrentHashMap`/`LinkedHashMap` allocation during
`BootstrapApplicationListener`/`ConfigDataLocationResolver` execution.

See also: `docs/known-issues/jit-bans/jit-ban-sweep-consolidated-status-20260726.md`
("Commented out 2026-07-28" section).
