# Spring four-class session, 2026-08-05 — 2 VM bugs, 3 harness bugs

| | |
|---|---|
| **Goal** | Make `beans.factory.aot.BeanRegistrationsAotContributionTests`, `test.context.aot.AotIntegrationTests`, `web.reactive.result.method.annotation.RequestMappingMessageConversionIntegrationTests` and `web.client.RestClientIntegrationTests` behave identically to HotSpot. |
| **Branch** | `fix/spring-4tests-20260802` |
| **Measured** | Azure host `20.83.144.174`, real JDK 25, `apps/spring-suite-runner`, and independently on the Windows box against its own spring-framework checkout. |

## Result

| class | HotSpot | CratonVM | |
|---|---|---|---|
| `web.client.RestClientIntegrationTests` | 230 found / 229 succ / 1 abort | same | already matched |
| `web.reactive…RequestMappingMessageConversionIntegrationTests` | 160/160 | **160/160** | was 146/160 |
| `test.context.aot.AotIntegrationTests` | 4 found / 2 succ / 2 skip | **same** | was FAIL |
| `beans.factory.aot.BeanRegistrationsAotContributionTests` | 14/14 | **13 of 14** | see residual |

The first three rows are whole-class runs of the final binary against the
repaired classpath. The BeanRegistrations row is from **per-method** runs
(`KRunM`): the twelve cheap methods and `applyToWithLargeBeanDefinitions
CreatesSlices` all pass, and `applyToWithVeryLargeBeanDefinitions
CreatesSeparateSourceFiles` fails. Three attempts at a single whole-class run
were killed by the host rather than by the VM — the box was at load average
85-146 with other sessions' work and the OOM killer active — so the per-method
figure is what is actually evidenced, and the whole-class number is not
claimed.

The single residual is
`applyToWithVeryLargeBeanDefinitionsCreatesSeparateSourceFiles`, filed as
[`../../../known-issues/spring/beanregistrations-verylarge-heap-footprint-20260805.md`](../../../known-issues/spring/beanregistrations-verylarge-heap-footprint-20260805.md).
It is a memory-footprint gap, not a correctness one: HotSpot compiles the
10001-definition case in 13 s inside `-Xmx512m`; CratonVM's live set for the
same test is ~1.2 GB and it exhausts a 4 GiB heap inside javac.

## VM bug 1 — `Net.poll` turned a signal into a dropped connection

`net_poll_raw` (`native-io/src/net.rs`, the `#[cfg(unix)]` arm) reported
`EINTR` as an I/O error. OpenJDK's `Net.poll`
(`unix/native/libnio/ch/Net.c`) returns **0 revents** on `EINTR` instead of
throwing, and `poll(2)` is never auto-restarted by `SA_RESTART` — so any
signal delivered to a thread parked there comes straight back as `EINTR`.

CratonVM sends such a signal *on purpose*: `jit::xt_root_scan` `SIGUSR2`s every
thread to take it over for a cross-thread JIT root scan. `net_err` has no
`Interrupted` arm, so a GC landing on a parked socket produced
`SocketException: poll: Interrupted system call` — a random mid-request
connection abort. That was **14 of the 160** failures in
`RequestMappingMessageConversionIntegrationTests`, and zero after the fix.

This is the same audit that fixed `read0`/`write0` on 2026-07-26 (the comments
there describe exactly this failure mode); it missed the poll primitive those
two park on.

Reporting "not ready" rather than retrying in place is what keeps the caller's
deadline honest — `NioSocketImpl.timedRead`/`timedAccept` recompute the
remaining timeout from `System.nanoTime()` each pass, and `net_poll_listener`
loops on `Ok(false)`.

## VM bug 2 — the dirty-card scan was quadratic

`OldGen::scan_region_filtered` (`gc/src/old_gen.rs`) tested each object with
`dirty_ranges.iter().any(..)`, i.e. O(old-gen objects x dirty ranges). The
range list is built sorted, non-overlapping and coalesced by
`gen_heap::scan_dirty_cards`, and `offset` only increases, so a monotone cursor
replaces the rescan.

`perf record -F 199` during the 10001-definition test put
`scan_region_filtered` at **73.7% of all CPU samples**; after the change it
does not appear in the profile at all.

**Scope this claim carefully.** It does *not* make that test pass — the test
fails on heap, not CPU, so removing the 73.7% only gets it to the same OOM
sooner. And on the 1001-definition sibling, whose old gen never grows enough
for the quadratic to bite, an alternating two-binary A/B is within noise (base
221/221/183 s, fixed 229/204 s). The justification is the algorithm and the
profile. An earlier "215 s -> 145 s" reading of this change was **load noise on
a contended host** and is withdrawn.

`walk_objects_in_card_ranges` had **no test coverage at all**; five tests were
added, including one that sweeps every prefix/suffix range set and requires the
cursor to agree with the original `any()` predicate exactly. Mutation-checked:
changing `offset >= range_start` to `>` fails 3 of the 5.

## Harness bugs — all three made a VM look wrong when it was not

1. **`one.sh` halved CratonVM's heap.** It exported
   `CRATONVM_DEFAULT_HEAP_MAX_MB=2048`, against CratonVM's own ergonomic
   default of `min(RAM/4, 4 GiB)` and HotSpot's uncapped `RAM/4` = 8.4 GiB on
   the 31 GiB host — a 4x handicap. `run-suite.sh`, which produced every
   published non-passed list, sets no such cap, so a class triaged through
   `one.sh` ran with half the heap of the sweep that flagged it.
   `RequestMappingMessageConversionIntegrationTests` scored 141/160 at 2 GiB
   (five reported causes being `Java heap space`) and 160/160 at the stock
   default, same binary, same run.

2. **`hs.sh`/`one.sh` dropped spring-test's own module property.** The runner
   mirrors `buildSrc` `TestConventions` (applied to every module) but not the
   per-module test block. `spring-test/spring-test.gradle` sets
   `junit.vintage.discovery.issue.reporting.enabled=false`, commented "we
   disable reporting of the 'deprecated' discovery issue, because that would
   otherwise fail the build" — spring-test deliberately keeps the JUnit Vintage
   engine, Vintage reports its own deprecation as an INFO discovery issue, and
   TestConventions' `discovery.issue.severity.critical=INFO` promotes it to
   critical. Without it **HotSpot** failed
   `AotIntegrationTests#endToEndTests` with `DiscoveryIssueException: TestEngine
   with ID 'junit-vintage' encountered a critical issue during test discovery`.

3. **The pinned test classpath rots.** `build/cratonvm-testcp.txt` is a
   snapshot of Gradle's `sourceSets.test.runtimeClasspath` with exact jar
   paths. On 2026-08-05 a concurrent session re-resolved dependencies and
   evicted the pinned versions (`junit-vintage-engine` 6.1.0 ->
   6.1.1/6.1.2): **54 of 254** spring-test entries vanished. This does not
   present as a classpath error — it presents as `found=0 status=EMPTY`, or, far
   worse, as a *plausible* assertion failure: with the Vintage engine silently
   absent, `BasicSpringVintageTests` is never AOT-processed, every later
   `TestContextNNN` shifts by one, and `endToEndTests` fails on the diff. That
   cost a full round of measurements on both VMs.

   Check before believing any suite result:

   ```bash
   for p in $(tr -d '\r' < build/cratonvm-testcp.txt | tr ':' '\n'); do [ -e "$p" ] || echo "MISSING: $p"; done
   ```

   Repair with
   `./gradlew --no-daemon -I ../spring-suite-runner/dump-testcp.init.gradle :spring-test:dumpTestCp`.

## Two harness notes worth keeping

* **Every one of these classes leaks non-daemon threads** (RestClient leaves
  368), so *both* VMs correctly keep the JVM alive after the tests finish —
  CratonVM even says so: `main() returned; VM held alive by N non-daemon
  thread(s) (JVM-spec behaviour)`. A plain `timeout N` therefore charges every
  class its full `N`, which reads as a hang. `KRun` flushes its `RESULT` line as
  soon as the class is done, so poll the log for `^RESULT ` and stop there —
  it turned a 3600 s-per-class ceiling into 65 s for RestClient.
* **This host is shared.** Other sessions took the 16-core box to load average
  146 with 164 OOM kills, which killed runs outright and moved absolute times
  by more than 2x. A/B two binaries *alternately inside one script*; never
  compare against a number from an earlier round.
