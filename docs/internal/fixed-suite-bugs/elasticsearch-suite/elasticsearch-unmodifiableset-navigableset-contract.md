# Elasticsearch UnmodifiableSet sorted/navigable contract break

Status: FIXED

Date fixed: 2026-07-04

Date observed: 2026-07-02

## Summary

CratonVM's `cratonvm.internal.UnmodifiableSet` escapes in places where the JDK
collection returned by HotSpot behaves as a `SortedSet` or `NavigableSet`.
Elasticsearch then fails casts or method dispatch.

Observed signatures:

```text
java.lang.ClassCastException:
cratonvm.internal.UnmodifiableSet cannot be cast to java.util.SortedSet
```

```text
java.lang.NoSuchMethodError:
cratonvm/internal/UnmodifiableSet.tailSet(Ljava/lang/Object;Z)Ljava/util/NavigableSet;
```

```text
java.lang.NoSuchMethodError:
cratonvm/internal/UnmodifiableSet.lower(Ljava/lang/Object;)Ljava/lang/Object;
```

## Current full-suite result

Run `es-current-full-jiton-20260702`, `all[1..2701]`, CratonVM JIT-on,
`-TimeoutSec 300`:

- 107 CratonVM failures with this collection-contract signature.
- 106 are CratonVM-only: HotSpot passed the same classes.
- 1 overlaps a HotSpot baseline failure.

Representative rows:

```text
index=221
module=server
class=org.elasticsearch.action.admin.cluster.allocation.TransportDeleteDesiredBalanceActionTests
CratonVM=FAIL, 9.607s
HotSpot=PASS, 65.734s
```

```text
index=247
module=server
class=org.elasticsearch.action.admin.cluster.node.reload.NodesReloadSecureSettingsResponseTests
CratonVM=FAIL, 17.206s
HotSpot=PASS, 23.776s
```

Other CratonVM-only examples:

```text
org.elasticsearch.action.admin.cluster.reroute.ClusterRerouteTests
org.elasticsearch.action.admin.indices.create.TransportCreateIndexActionTests
org.elasticsearch.action.fieldcaps.RequestDispatcherTests
org.elasticsearch.cluster.routing.GlobalRoutingTableTests
org.elasticsearch.index.codec.PerFieldMapperCodecTests
```

## Source touchpoints

The `SortedSet` cast path initializes:

```text
server\src\main\java\org\elasticsearch\cluster\node\DiscoveryNodeRole.java:312
final SortedSet<DiscoveryNodeRole> roles = roleMap.values().stream().collect(Sets.toUnmodifiableSortedSet());
```

The `lower(Object)` path reaches:

```text
test\framework\src\main\java\org\elasticsearch\test\TransportVersionUtils.java:74
TransportVersion lower = (isPatchVersion(version) ? RELEASED_VERSIONS : NON_PATCH_VERSIONS).lower(version);
```

## Repro

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass `
  -File apps\elasticsearch-suite-runner\run-elasticsearch-suite.ps1 `
  -Vm craton -Category all -Jit on -Start 221 -Count 1 -Parallel 1 -TimeoutSec 300 `
  -RunName es-unmodifiableset-sortedset-repro-20260702 `
  -ElasticsearchRoot C:\craton\CratonVM\apps\elasticsearch `
  -WorkDir C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite `
  -Exe C:\craton\CratonVM-elasticsearch-current-suite-20260702\target\release\cratonvm-elasticsearch-current-suite-20260702.exe
```

## Evidence

```text
C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-current-full-jiton-20260702\all-jit\results.tsv
C:\craton\CratonVM-elasticsearch-full-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-full-hotspot-20260702\hotspot-jit\results.tsv
C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-current-full-jiton-20260702\all-jit\logs\server.org.elasticsearch.action.admin.cluster.allocation.Transpo.d64f1184279c.out.log
C:\craton\CratonVM-elasticsearch-current-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-current-full-jiton-20260702\all-jit\logs\server.org.elasticsearch.index.codec.PerFieldMapperCodecTests.out.log
```

## No-JIT partial evidence

Run `es-nojit-full-20260702` was stopped by request after 1366 recorded
classes. In that partial CratonVM no-JIT run:

- 100 failures matched this collection-contract signature.
- 99 were CratonVM-only versus the HotSpot baseline.
- Representative rows shifted to index 222
  `TransportDeleteDesiredBalanceActionTests` and index 248
  `NodesReloadSecureSettingsResponseTests`.

```text
C:\craton\CratonVM-elasticsearch-nojit-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-nojit-full-20260702\all-nojit\results.tsv
```
## Fix (2026-07-04)

This issue is fixed by the current `dev` implementation of `cratonvm.internal.UnmodifiableSet` in:

- `../../../../native-collections/src/lib.rs` (full `NavigableSet`/`SortedSet` interface surface and full method remap)
- `../../../../vm/src/vm/vm_init.rs` (JDK `SortedSet` / `NavigableSet` interface registration)

Residual instances were removed from the active suite-open set and archived here in
`..`.
