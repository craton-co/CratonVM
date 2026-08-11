# Platform `MBeanServer.getAttribute` returns the raw MXBean value, not `CompositeData`

| | |
|---|---|
| **Status** | OPEN |
| **HotSpot** | returns `javax.management.openmbean.CompositeDataSupport` |
| **CratonVM** | returns the raw MXBean type (`java.lang.management.MemoryUsage`) |
| **Discovered** | 2026-08-11, while fixing the openmbean carrier natives |

## Symptom

```java
MBeanServer s = ManagementFactory.getPlatformMBeanServer();
Object hu = s.getAttribute(new ObjectName("java.lang:type=Memory"), "HeapMemoryUsage");
System.out.println(hu.getClass().getName());
```

```
HotSpot  : javax.management.openmbean.CompositeDataSupport
CratonVM : java.lang.management.MemoryUsage
```

The MXBean specification requires the platform MBean server to map an MXBean
attribute's Java type to its *open type* before returning it — a
`MemoryUsage` becomes a `CompositeData` with items `init`/`used`/`committed`/
`max`. CratonVM skips that conversion and hands back the underlying object, so
any client written to the documented contract (`(CompositeData) value`,
`CompositeDataSupport`-shaped remote JMX traffic, `jconsole`-style generic
browsers) sees a `ClassCastException` or the wrong shape.

## Scope

Not a regression: reproduced identically on binaries from either side of the
2026-08-11 openmbean carrier fix, so it predates that work.

Independent of the carrier natives: the value never becomes a
`CompositeDataSupport` at all, so no `CompositeDataSupport` native is involved
in the divergence.

## Note on the carrier builders

`jmx_openmbean.rs` has `build_composite_data` / `build_tabular_data`, which
mint exactly the `CompositeData` shape this conversion needs, but they
currently have no caller outside that module's tests. Whatever closes this
issue is likely to be their first production caller — and the per-instance
discriminator that the carrier natives grew on 2026-08-11 is what will keep
those natives serving the minted carriers while application-built instances go
to real bytecode.

## Reproduction

```bash
source /data/toolchain/env.sh
<cratonvm> --java-home /data/toolchain/jdk-25 -cp <probe-dir> MxProbe
java -cp <probe-dir> MxProbe          # HotSpot control
```
