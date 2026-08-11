# Platform `MBeanServer.getAttribute` returned the raw MXBean value — the type-mapping overlay was obsolete

| | |
|---|---|
| **Status** | FIXED 2026-08-11 |
| **HotSpot** | returns `javax.management.openmbean.CompositeDataSupport` |
| **CratonVM** | now identical — every attribute type and every `MBeanInfo` attribute type matches |
| **Discovered** | 2026-08-11, while fixing the openmbean carrier natives |
| **Fixed in** | `fix/mxbean-getattribute-opentype-20260811` |

## Symptom

```java
ManagementFactory.getPlatformMBeanServer()
    .getAttribute(new ObjectName("java.lang:type=Memory"), "HeapMemoryUsage")
```

| | HotSpot | CratonVM (before) |
|---|---|---|
| `HeapMemoryUsage` | `CompositeDataSupport` | `java.lang.management.MemoryUsage` |
| every `MemoryPool` `Usage`/`PeakUsage`/`CollectionUsage` | `CompositeDataSupport` | `java.lang.management.MemoryUsage` |
| `Runtime.SystemProperties` | `TabularDataSupport` | `java.util.HashMap` |
| `Runtime.InputArguments` | `String[]` | `Collections$UnmodifiableRandomAccessList` |
| `getMBeanInfo` → `HeapMemoryUsage` | `CompositeData`, real `CompositeType` | `java.lang.String`, `SimpleType.STRING` |
| `getMBeanInfo` → `ObjectName` | `javax.management.ObjectName` | `java.lang.String` |

## What was actually running

Not what the surrounding code suggested. CratonVM does **not** use its synthetic
in-process `MBeanServer` here: `getPlatformMBeanServer()` returns a real
`com.sun.jmx.mbeanserver.JmxMBeanServer`, `getMBeanInfo` returns a real
`MBeanInfo`, and `queryNames` works. The whole real MXBean introspection path
was already live.

The gap was one layer down. `jmx_openmbean.rs` overrode the JDK's MXBean
*type-mapping* entry points —

* `MXBeanMappingFactory.mappingForType`
* `DefaultMXBeanMappingFactory.mappingForType`
* `DefaultMXBeanMappingFactory.makeMapping`
* `MXBeanMapping.toOpenValue` / `fromOpenValue`
* `ConvertingMethod.from`

— with a synthetic mapping that types every Java type it does not recognise as
`SimpleType.STRING` and whose `toOpenValue` is the **identity**. That identity
is the bug: the value is handed to the caller unconverted. The module's own
header had already written down why it was thought safe — "our mappings are
only ever consumed by a `toOpenValue` call on the MBeanServer side immediately
followed by a `fromOpenValue` call on the client/proxy side of the SAME
in-process round trip". True for `newPlatformMXBeanProxy`, which is why proxies
always looked right. False for a direct `getAttribute`, which is the documented
MXBean contract and what every generic JMX client uses.

## Why the overlay was there, and why it is no longer needed

It was defence in depth on top of a *different*, primary fix. The original
failure (KC16 / WildFly boot) was `MXBeanIntrospector` walking the
`Object.getClass` method into `Class` → `AnnotatedType[]`, hitting a
self-reference and throwing `OpenDataException`. The primary fix filters Object
methods out at `MBeanIntrospector.getMethods(Class)`, which is where
`MBeanAnalyzer.initMaps` consumes them. **That filter stays registered
unconditionally** — and it is precisely why the real
`DefaultMXBeanMappingFactory` terminates today: the self-reference that started
all of this is never offered to the mapping factory at all.

The overlay was solving a problem the filter had already solved, and was
charging the entire open-type contract for it.

## Evidence

A/B on one binary via a temporary env gate, JDK 25 on Linux:

* **overlay on** (old default) — `MemoryUsage` raw, `SystemProperties` a
  `HashMap`, `MBeanInfo` types all `java.lang.String`.
* **overlay off** — every attribute type and every `MBeanInfo` attribute type
  byte-identical to HotSpot (verified by diffing the type columns, not by eye).

The termination question was tested directly rather than assumed. A custom
MXBean with a self-referential type (`Node getChild()`), a `Map<String,
List<String>>` and a `List<Integer>`:

* HotSpot: `NotCompliantMBeanException: … getRoot has parameter or return type
  that cannot be translated into an open type`
* CratonVM, overlay off: **the same exception, same message**
* CratonVM, overlay on: silently *accepted* the non-compliant bean and answered
  `Root` with a raw `MxDeep$Node`

So the overlay was not merely unnecessary; on the one case it existed to
protect, it was hiding a compliance error that HotSpot raises.

A plain (non-MX) `StandardMBean` is unaffected in both arms — it goes through
`StandardMBeanIntrospector`, which never touches `MXBeanMapping`.

## Fix

`real_mxbean_mapping_enabled()` in `native-builtins/src/jmx_openmbean.rs`, and
`register_jmx_openmbean_natives_with(registry, synthetic_mapping)` so the
decision is a parameter rather than an environment read.

Default: the real machinery. **Gated, not deleted** —

* `synthetic-jdk` builds keep the overlay (no real `com.sun.jmx.mbeanserver`
  bytecode to fall back to), via `cfg!(feature = "synthetic-jdk")` rather than
  a `#[cfg]` arm, so the code compiles in both configurations;
* `CRATONVM_SYNTHETIC_MXBEAN_MAPPING=1` restores it on a real-JDK run — the
  one-run answer if an application MBean ever does drive the real factory into
  a recursion this VM cannot finish.

This mirrors `native-io`'s `real_raf_enabled()`, which flipped the same way for
the same reason.

Recorded while here: the `OpenConverter.toConverter` and
`MappedMXBeanType.getMappedMXBeanType` registrations are **inert on JDK 25** —
`javap --module java.management` finds neither class; both are pre-JDK-7
spellings. Left registered (they still name real classes on older images, and a
registration that targets nothing costs nothing), but their presence is not
evidence that path is live.

## Verification

* `MxSurvey` — attribute types and `MBeanInfo` attribute types diff **clean**
  against HotSpot.
* `MxDeep` — every `MemoryPool` usage attribute becomes `CompositeData`;
  self-referential MXBean rejected as HotSpot rejects it; plain `StandardMBean`
  unchanged.
* In-tree `apps/jmx_probe/JmxProbe.java` — identical output on the pre-fix
  binary, the fixed binary, and the fixed binary with the escape hatch set.
* Tomcat: `TestJMXAccessorTask`, `mbeans.TestRegistration`,
  `startup.TestTomcat`, `core.TestStandardContext` — 56 tests, all green.
* `cratonvm-native-builtins` and `cratonvm-cli` test suites green.

## Residual divergences, all pre-existing and unrelated

Confirmed identical on the pre-fix binary:

* CratonVM registers fewer platform beans (2 memory pools / 1 manager / 1
  collector vs HotSpot's 8 / 5 / 3), so `mbeanCount` and the `java.lang:*` set
  are smaller.
* `ThreadMXBean.dumpAllThreads` → `UnsupportedOperationException: Monitoring of
  Object Monitor Usage is not supported`.
* `GarbageCollector.LastGcInfo` → `AttributeNotFoundException`.
* `AllThreadIds` reports a single thread.
* `MemoryUsage.toString()` renders `init=…, used=…` where HotSpot renders
  `init = …(…K) used = …` — a native override of `toString` on a class whose
  four field slots already match the real layout. Filed separately as
  `docs/known-issues/memoryusage-tostring-format-diverges.md`.
