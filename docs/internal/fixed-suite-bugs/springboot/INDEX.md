# Spring Boot runner-probe sweep — CratonVM divergence index (2026-06-22)

**Binary:** `cvsbfull.exe` — release build of **dev `df11ac00`** (worktree `C:\craton\CratonVM-sbfull`).
**Reference:** HotSpot `jdk-25` (`C:\Program Files\Java\jdk-25`).
**Suite:** all **103 standalone probe classes** in `apps/spring-boot/buildSrc/runner/` (each a `main()` that exercises a specific VM/Spring behavior), run under both VMs, output normalized and diffed. Per-probe timeout 60 s. No early stop.
**Driver:** `C:\craton\CratonVM-sbfull\run-probes.sh` → `C:\craton\CratonVM-sbfull\results-probes\` (`SUMMARY.txt`, `results.tsv`, `cv/*.log`, `hs/*.log`, `DIFFS.txt`).

## Aggregate (exact)

| Outcome | Count |
|---|---|
| Probes run | **103** |
| Match HotSpot (ok) | **54** |
| `both-fail` (shared classpath/infra gap — **not** a CV bug) | **7** |
| **CV-CRASH** (CV fails, HS passes) | **0** |
| **CV-HANG** (CV ≥60 s, HS completes) | **11** |
| **CV-DIFF** (both exit 0, output differs) | **31** raw → **19** real (12 were identity-hashcode/temp-path/timing artifacts) |

The 11 hangs + 19 real diffs collapse to **14 distinct root-cause bugs** below.
Wall time: probe sweep START 20:31 → END 20:45 (~14 min; dominated by the 11 × 60 s hang timeouts).

`both-fail` (excluded, both VMs fail identically — missing libs on the runner classpath): `DApi`, `DOMWalkProbe`, `InjectProbe`, `KFinderProbe`, `SepProbe`, `XPathProbe`, `YamlProbe`.

## Bugs — recommendation column is my suggestion; you decide

| ID | Title | Class | Probes | Rec. |
|----|-------|-------|--------|------|
| [SBR-01](SBR-01-groovy-parseclass-hang.md) | `GroovyClassLoader.parseClass` hangs (no output, >60 s) | HANG | 9 Groovy probes | **HANDOFF** |
| [SBR-02](SBR-02-string-regex-throughput.md) | `String.replaceAll` throughput wall (~30–60× slower) | HANG/perf | MinRegexProbe, RegexLoopProbe | ✅ **FIXED** dev `0d7dfc28` |
| [SBR-03](SBR-03-array-interface-instanceof.md) | `Object[] instanceof I[]` returns `true` (HS `false`) | DIFF/correctness | ArrInstProbe | ✅ **FIXED** dev `a245002c` |
| [SBR-04](SBR-04-annotation-getclass-proxy-tostring.md) | Annotation `getClass()` = annotation type not `$ProxyN`; `toString` format | DIFF | DAnnCore, DAnnWalk | FIX (=SB-09) |
| [SBR-05](SBR-05-getdeclaredmethods-order.md) | `getDeclaredMethods()` order differs from HotSpot | DIFF (spec-unspec) | DTags, DAnnWalk | LOW |
| [SBR-06](SBR-06-field-getgenerictype-raw.md) | Constructor `Parameter.getParameterizedType()` erases generics → raw `Class` | DIFF | GenProbe | ✅ **FIXED** dev `bce39db7` |
| [SBR-07](SBR-07-getsimplename-nested.md) | `getSimpleName()` on nested class returns `Outer$Inner` | DIFF | KProtoProbe | ✅ FIXED (dev 2026-06-23) |
| [SBR-08](SBR-08-jarurl-openconnection-abstract.md) | jar-URL `openConnection()` → abstract `java.net.JarURLConnection` | DIFF | KExactProbe, KUrlProbe | FIX |
| [SBR-09](SBR-09-nio-filesystem-attrview-abstract.md) | NIO `FileSystem`/`FileAttributeView` report abstract types | DIFF | FSEq, DirProbe | FIX |
| [SBR-10](SBR-10-intstream-pipeline-abstract-class.md) | `IntStream` pipeline objects report abstract `IntStream` class | DIFF | IntSortProbe2 | FIX |
| [SBR-11](SBR-11-methodhandle-class-tostring.md) | `MethodHandle` runtime class + `toString` descriptor | DIFF | MHKind, MHCtor | FIX |
| [SBR-12](SBR-12-immutable-list-internal-class-leak.md) | `List.of/copyOf`/`unmodifiableList` leak `cratonvm.internal.UnmodifiableList` | DIFF | LOfHash2, LOfHashProbe, KListProbe | **FIX** |
| [SBR-13](SBR-13-protectiondomain-classloader-permissions.md) | `getProtectionDomain()`: classloader=`Object`, permissions `null` | DIFF | InstProbe | ✅ FIXED (dev 2026-06-23) |
| [SBR-14](SBR-14-urlclassloader-parent-null-bypassed.md) | Custom `URLClassLoader(parent=null)` bypassed → `AppClassLoader` | DIFF | UCLProbe | **HANDOFF** |

## Fix session (2026-06-22, worktree `CratonVM-sbfull`)

Two correctness bugs fixed, committed, and re-verified across the full 103-probe
sweep (deterministic, **no regressions** — ok 54→56, no new divergence):

| ID | Fix | Commit |
|----|-----|--------|
| SBR-03 | strict array `instanceof` (lenient flag; checkcast/aastore unchanged) | `a245002c` |
| SBR-06 | populate Constructor mirror `signature` field (was Method-only) | `bce39db7` |

Both byte-identical to HotSpot after the fix (ArrInstProbe, GenProbe + reflection
set). The remaining IDs were each investigated and **root-caused** (see the
individual reports) but deferred as not-safe-one-liners:
- **SBR-07** real defect = `getDeclaringClass0()`/InnerClasses resolution for the
  Kotlin protobuf classes (getSimpleName itself is correct). Needs kotlin-reflect repro.
- **SBR-12** one synthetic class backs both `List.of` and `unmodifiableList`;
  renaming risks colliding with the real rt.jar class. Needs real `ImmutableCollections`.
- **SBR-08/09/10/11/13** one theme: CV synthesizes JDK objects as abstract/base-typed
  without concrete class identity/fields. Subsystem work, gauntlet-regression risk.
- **SBR-05** won't-fix (order is spec-unspecified; JUnit re-sorts).

### Suggested split
- **Handoff (deep / framework-impacting):** SBR-01 (Groovy compiler), SBR-14 (classloader isolation).
- **Fix here (well-scoped):** SBR-03, SBR-06, SBR-07, SBR-12 are small, self-contained correctness gaps with crisp repros.
- **Low priority:** SBR-05 (`getDeclaredMethods` order is spec-unspecified; documenting only).

Note: SBR-04 overlaps prior `SB-09`; SBR-02 overlapped prior `SB-12`/`SB-16` perf family and is now fixed. The rest (SBR-03, 06–14) are **newly surfaced** by this probe sweep.
