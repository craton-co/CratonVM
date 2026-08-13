# SPB.6 (Netflix Eureka DiscoveryClient) — removed 2026-07-27, UNVERIFIED

**Status**: removed (not just commented out — the code was deleted, since
this was addressed a day before the broader "comment out non-target-app
bans" pass), no re-verification. Per explicit user decision.

## What it banned

`com/netflix/discovery/` (blanket package prefix).

## Original symptom (Session 113 r1)

`DiscoveryClient.<init>` allocates Eureka `InstanceInfo`/
`ApplicationInfoManager` objects whose constructors store
`metadata`/`leaseInfo`/`port` slots immediately after allocation — the
same allocate-then-putfield archetype as `Integer.valueOf`/
`String.toLowerCase`. Eureka also installs a `ScheduledExecutorService`
whose task-submit path (`LinkedBlockingQueue.offer`/`enqueue`) was
already separately covered by `EXEC.1`; this per-package ban covered the
Eureka-specific allocations.

## Why it was never re-verified before being removed

No fixture ever existed on this host: exhaustively searched twice
(this session and an independent concurrent session) for the original
`eureka-server` app or any `eureka-client`/`eureka-core` jar
(Maven/Gradle cache, vendored, source checkout) — zero matches in
either search. Netflix Eureka (Spring Cloud's service-discovery client)
is also outside the plain Spring/Spring Boot 5-app scope this session
later narrowed to, reinforcing the decision not to re-verify.

## How to restore

In `vm/src/jit/skip_list.rs`, find the `SPB.6 -- REMOVED 2026-07-27,
UNVERIFIED` comment inside `should_skip_jit_internal` and re-add:

```rust
if class_name.starts_with("com/netflix/discovery/")
    && !package_allowed("com/netflix/discovery/", allow_packages)
{
    return Some(SkipReason::RustJvmTestFixture);
}
```

## Repro (for whoever re-verifies)

Any real Netflix Eureka `DiscoveryClient` boot/registration under
default JIT tiering, watching for corruption in `InstanceInfo`/
`ApplicationInfoManager` field writes immediately after construction.

See also: `docs/known-issues/jit-bans/jit-ban-sweep-consolidated-status-20260726.md`
("Removed 2026-07-27, UNVERIFIED" section) and the corresponding unit
test `netflix_discovery_is_jit_eligible_after_spb6_removal` in
`vm/src/jit/skip_list.rs`.
