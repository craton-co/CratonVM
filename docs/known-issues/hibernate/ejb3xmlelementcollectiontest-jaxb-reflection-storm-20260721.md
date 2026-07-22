# `annotations.xml.ejb3.Ejb3XmlElementCollectionTest` HANG — another confirmed trigger of the tracked JAXB reflection-storm throughput wall, not the old (refuted) `retainAll` hypothesis

| | |
|---|---|
| **Status** | 🔴 **OPEN** — not a new/distinct bug; a new confirmed trigger of the already-documented, already-OPEN `update_root_snapshot` reflection-storm throughput wall ([`docs/internal/tomcat-suite-bugs/04-embedded-server-throughput-wall-OPEN.md`](../../internal/tomcat-suite-bugs/04-embedded-server-throughput-wall-OPEN.md)), same mechanism as this directory's [`astparserloadingtest-jaxb-reflection-storm-throughput-wall-20260721.md`](astparserloadingtest-jaxb-reflection-storm-throughput-wall-20260721.md). |
| **Class** | `org.hibernate.orm.test.annotations.xml.ejb3.Ejb3XmlElementCollectionTest` (tests `@ElementCollection` mapped via `orm.xml`/EJB3-style XML, not annotations) |
| **Symptom this run** | `HANG`, `process-died rc=124` — the suite harness's flat 300s per-invocation timeout expired with **zero** `@@RESULT`/`@@FAIL` ever printed. |

## Source run

`apps/hib-suite-runner/runs/run-20260721-175909-passed/on-real/shard-7/raw.log`
(idx 7, `@@BEGIN 7 org.hibernate.orm.test.annotations.xml.ejb3.Ejb3XmlElementCollectionTest`
at line 1593; next class's process starts at line 1779). Binary from
worktree `CratonVM-hib-local-0712` (branch `test/hib-local-0712`, merged with
`origin/dev` @ `7aed580f0`).

The raw log shows roughly 20 repetitions of the same
`jakarta.xml.bind.JAXBContextFactory` / `org.glassfish.jaxb.runtime.v2.ContextFactory`
initialization block before the kill — consistent with this class's many
`@Test` methods, each rebuilding a `SessionFactory` (and therefore
re-running JAXB `orm.xml` model-building) from scratch.

## Prior investigation and why this doc supersedes it

`docs/internal/hibernate-bugs/hibernate-jaxb-classloading-bytebuddy-bootstrap-slow.md`
lists this exact class as its "Confirmed" repro for an `AbstractCollection.retainAll`
infinite-loop hypothesis, later revised in the same doc to "interpreter-slow
class-loading/reflection storm" (live `cdb` showed the spinning frames
*moving* between samples, refuting a true infinite loop) — and finally
marked **"RESOLVED / does-not-reproduce (re-verified 2026-06-20)"** on the
theory that the dominant class-load rescan-storm layer was fixed by
`1db07c35`/`25c42e13`. Both of those commits are confirmed ancestors of the
binary used in this run (`git merge-base --is-ancestor` true against
`7aed580f0`), yet the class still hangs — so the "does-not-reproduce"
verdict does not hold for the current `dev` tip. This is not a regression
of that specific fix, though: this session's repro shows **no**
`retainAll`-stuck frame at all (see below) — it is a different, later-dated
root cause (`update_root_snapshot`, first characterized 2026-07-21, i.e.
after that older doc was written) manifesting in the same subsystem.

`docs/internal/hibernate-bugs/hibernate-hang-clusters-summary.md`'s
"Cluster H1" entry already lists this exact class as a **confirmed** member
of an *unresolved* "JAXB model-building slow / class-loading" cluster
("Deeper investigation, not a quick fix") — this doc's finding is
consistent with, and sharpens, that entry.

## Solo reproduction

```
cd C:/craton/CratonVM/apps/hib-suite-runner
echo org.hibernate.orm.test.annotations.xml.ejb3.Ejb3XmlElementCollectionTest > /tmp/single-ejb3.txt
"C:/craton/CratonVM-hib-local-0712/target/release/cratonvm.exe" \
  --java-home "C:/Program Files/Eclipse Adoptium/jdk-25.0.3.9-hotspot" --Xmx 1500m \
  @common.args -Dcraton.batch=1 --stack-dump-on-timeout 90 CratonRunner /tmp/single-ejb3.txt 0
```

Reproduced on the first attempt — did not complete even the first `@Test`
method's `SessionFactory` bootstrap within 90s. The `--stack-dump-on-timeout 90`
watchdog fired and took 371 rapid successive stack samples of the single
`main` thread before aborting. Frame count fluctuates sample-to-sample
(148, 150, 150, 149, 150, … then a run of eleven samples flat at 139, then
back to 149-150, …) — genuinely executing, not parked (same diagnostic used
throughout this directory to rule out deadlock: a truly stuck/parked thread
would show one fixed PC across every sample).

**No sample anywhere in the 371-sample burst shows `AbstractCollection.retainAll`**
(the old, refuted hypothesis) — the deepest frames are consistently inside
GlassFish JAXB's runtime model builder:

```
RuntimeModelBuilder.getClassInfo → ModelBuilder.getClassInfo
  → RuntimeModelBuilder.getClassInfo (overload) → ModelBuilder.getClassInfo
  → RuntimeClassInfoImpl.getProperties → ClassInfoImpl.getProperties
  → ClassInfoImpl.findFieldProperties → ClassInfoImpl.addProperty
  → RuntimeClassInfoImpl.createElementProperty
  → RuntimeElementPropertyInfoImpl.<init> → ElementPropertyInfoImpl.<init>
  → ERPropertyInfoImpl.<init> → PropertyInfoImpl.<init>
  → Adapter.<init> → RuntimeInlineAnnotationReader.getClassValue (×2)
  → XmlJavaTypeAdapterQuick.value
```

i.e. JAXB recursively walking the object graph of annotated/`orm.xml`-mapped
classes, building `ClassInfoImpl`/`PropertyInfoImpl` metadata and reading
`@XmlJavaTypeAdapter`-style annotation values one field/property at a time
— exactly the reflection-heavy model-construction work the H1 cluster doc
already characterizes as "very slow on the interpreter," now confirmed to
still exceed the harness's 300s budget on the current `dev` tip.

## Root-cause hypothesis

Same as `ASTParserLoadingTest` and (via a different subsystem)
`InsertOrderingRCATest` in this same directory: most likely the already-
tracked, OPEN, deferred `update_root_snapshot` per-native-call GC-root-
snapshot cost being `O(current interpreter stack depth)` — JAXB's
reflection-based model walk for this class's `orm.xml`-mapped
`@ElementCollection` entities is a 140-150-frame-deep, native-call-heavy
(annotation/field/method reflection) recursive chain whose top frames churn
every call, the documented worst case for that mechanism. Not independently
fixed or diagnosed further this session.

## Host-load caveat

Concurrent, unrelated `cratonvm.exe`/`cratonvm-spring-boot-suite` processes
from other sessions were present on this shared host during this
investigation. This does not change the qualitative finding (continuous,
varying-depth, JAXB-only forward progress, never stuck at one PC) but the
absolute "still running past 90s" figure could be somewhat inflated versus
a fully quiet host.

## Recommendation

Not independently fixable here. Re-run this class (and its documented
"almost certainly same root" siblings — `Ejb3XmlManyToOneTest`,
`Ejb3XmlOneToOneTest`, `bootstrap.binding.annotations.access.xml.XmlAccessTest`,
`boot.models.xml.XmlProcessingSmokeTests`, `boot.jaxb.mapping.HbmTransformationJaxbTests`
— per the older JAXB doc) once any `update_root_snapshot` fix lands on
`dev`, using `--stack-dump-on-timeout 60` and a `--timeout` well above 300s.
Until then, treat this class's `HANG` under the suite's flat 300s default as
expected/explained, not a fresh regression — update the older
`hibernate-jaxb-classloading-bytebuddy-bootstrap-slow.md`'s "RESOLVED /
does-not-reproduce" verdict to note it does not hold for this class on the
current `dev` tip (superseded by the `update_root_snapshot` mechanism,
unrelated to the `retainAll`/class-load-storm layers that doc addressed).
