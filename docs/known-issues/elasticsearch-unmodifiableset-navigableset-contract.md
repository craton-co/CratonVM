# Elasticsearch UnmodifiableSet sorted/navigable contract break

Status: open

Date observed: 2026-07-02

## Summary

CratonVM's `cratonvm.internal.UnmodifiableSet` is escaping in places where the
JDK collection returned by HotSpot implements `SortedSet` or `NavigableSet`.
Elasticsearch then either fails a cast to `SortedSet` or calls a missing
`lower(Object)` method.

Observed failures:

```text
java/lang/ClassCastException:
cratonvm.internal.UnmodifiableSet cannot be cast to java.util.SortedSet
```

```text
java.lang.NoSuchMethodError:
cratonvm/internal/UnmodifiableSet.lower(Ljava/lang/Object;)Ljava/lang/Object;
```

## Full-suite result

Full suite `all[1..2701]` on 2026-07-02 with `-TimeoutSec 300` found 100
CratonVM-only failures with this collection-contract signature. HotSpot passed
the same 100 classes.

Breakdown:

- 98 classes fail on `UnmodifiableSet cannot be cast to java.util.SortedSet`.
- 2 classes fail on missing `UnmodifiableSet.lower(Object)`.

Representative rows:

```text
index=222
module=server
class=org.elasticsearch.action.admin.cluster.allocation.TransportDeleteDesiredBalanceActionTests
CratonVM=FAIL, 7.987s
HotSpot=PASS, 65.734s
```

```text
index=246
module=server
class=org.elasticsearch.action.admin.cluster.node.reload.NodesReloadSecureSettingsResponseTests
CratonVM=FAIL, 24.250s
HotSpot=PASS, 23.776s
```

## Repro

SortedSet cast:

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass `
  -File apps\elasticsearch-suite-runner\run-elasticsearch-suite.ps1 `
  -Vm craton -Category all -Jit on -Start 222 -Count 1 -Parallel 1 -TimeoutSec 300 `
  -RunName es-unmodifiableset-sortedset-repro-20260702 `
  -ElasticsearchRoot C:\craton\CratonVM\apps\elasticsearch `
  -WorkDir C:\craton\CratonVM-elasticsearch-full-suite-20260702\apps\elasticsearch-suite-runner\.suite `
  -Exe C:\craton\CratonVM-elasticsearch-full-suite-20260702\target\release\cratonvm-elasticsearch-full-suite-20260702.exe
```

NavigableSet method:

```powershell
powershell.exe -NoProfile -ExecutionPolicy Bypass `
  -File apps\elasticsearch-suite-runner\run-elasticsearch-suite.ps1 `
  -Vm craton -Category all -Jit on -Start 246 -Count 1 -Parallel 1 -TimeoutSec 300 `
  -RunName es-unmodifiableset-lower-repro-20260702 `
  -ElasticsearchRoot C:\craton\CratonVM\apps\elasticsearch `
  -WorkDir C:\craton\CratonVM-elasticsearch-full-suite-20260702\apps\elasticsearch-suite-runner\.suite `
  -Exe C:\craton\CratonVM-elasticsearch-full-suite-20260702\target\release\cratonvm-elasticsearch-full-suite-20260702.exe
```

## Source touchpoints

The cast failure initializes:

```text
server\src\main\java\org\elasticsearch\cluster\node\DiscoveryNodeRole.java:312
final SortedSet<DiscoveryNodeRole> roles = roleMap.values().stream().collect(Sets.toUnmodifiableSortedSet());
```

The missing method failure reaches:

```text
test\framework\src\main\java\org\elasticsearch\test\TransportVersionUtils.java:74
TransportVersion lower = (isPatchVersion(version) ? RELEASED_VERSIONS : NON_PATCH_VERSIONS).lower(version);
```

## Evidence

```text
C:\craton\CratonVM-elasticsearch-full-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-full-jiton-20260702\all-jit\results.tsv
C:\craton\CratonVM-elasticsearch-full-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-full-hotspot-20260702\hotspot-jit\results.tsv
C:\craton\CratonVM-elasticsearch-full-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-full-jiton-20260702\all-jit\logs\server.org.elasticsearch.action.admin.cluster.allocation.TransportDeleteDesiredBalanceActionTests.err.log
C:\craton\CratonVM-elasticsearch-full-suite-20260702\apps\elasticsearch-suite-runner\.suite\results\es-full-jiton-20260702\all-jit\logs\server.org.elasticsearch.action.admin.cluster.node.reload.NodesReloadSecureSettingsResponseTests.out.log
```
