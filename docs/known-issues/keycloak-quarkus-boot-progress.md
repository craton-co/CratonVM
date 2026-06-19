# Keycloak 26.6.3 (Quarkus) boot under CratonVM — progress & gap chain

Goal: reach and validate the **real Quarkus ArC** CDI path (`CRATONVM_REAL_ARC`,
the `quarkus_arc.rs` shim's replacement). ArC's `Arc.initialize()` runs **late**
in the Quarkus boot, so it is gated behind a chain of earlier boot gaps. This
doc tracks that chain. Each gap fixed advances the real boot one step closer to
ArC.

## Repro (this session)

Real, augmented Keycloak server (generated ArC beans present —
`lib/quarkus/generated-bytecode.jar` has 177 `*_Bean`/`*_ClientProxy` classes):

- Dist: `C:\craton\keycloak-26.6.3` (downloaded from the GitHub release; the
  `apps/keycloak` source tree is `999.0.0-SNAPSHOT` and is **not** augmented, so
  it cannot boot — the released dist is the only bootable artifact here).
- Boot (mirrors `apps/probe/test-infra/run-keycloak.sh`, paths fixed for this
  machine):
  ```
  cd C:/craton/keycloak-26.6.3
  CRATONVM_DISABLE_JIT=1 cratonvm.exe --java-home "C:/Program Files/Java/jdk-25" --Xmx 2g \
    -Dkc.config.built=true -Dkc.home.dir=C:/craton/keycloak-26.6.3 \
    -Djboss.server.config.dir=C:/craton/keycloak-26.6.3/conf \
    -Djava.util.concurrent.ForkJoinPool.common.threadFactory=io.quarkus.bootstrap.forkjoin.QuarkusForkJoinWorkerThreadFactory \
    --jar lib/quarkus-run.jar start-dev
  ```
- HotSpot baseline: the same dist boots fully — *"Keycloak 26.6.3 on JVM
  (powered by Quarkus 3.33.2) started in ~22s. Listening on http://localhost:8080.
  Profile dev activated."* (the comparison oracle).

## Gap chain (boot order)

### Gap 1 — Quarkus `RunnerClassLoader` reads 0-byte class data ✅ FIXED
**Symptom:** boot dies at the very first app class —
`ClassFormatError: class file too short (0 bytes)` for
`org/keycloak/quarkus/runtime/KeycloakMain`, then `QuarkusEntryPoint` NPEs on a
null `Class`. Reached *before* ArC.

**Root cause (general bug, not Keycloak-specific):** `JarEntry.getSize()` /
`getMethod()` returned **0** for entries whose bytes are actually readable
(probe: `getSize()=0, getMethod()=0`, but `readAllBytes()=9582` correct). Quarkus
`RunnerClassLoader` sizes its class-byte read buffer from `entry.getSize()` → 0 →
defines the class from an empty array. Cause: the `phases_late.rs` "p59"
synthetic-`JarFile` `getEntry`/`getJarEntry` path wrote size/csize/method into
**synthetic slot indices**, but real `java.util.zip.ZipEntry.getSize()/getMethod()`
bytecode reads the **real** fields by their actual offset (never set → 0). The
correct `native-io/zip_real_jar.rs` path sets fields *by name*; p59 didn't.

**Fix:** `native-builtins/src/phases_late.rs` — `p59_jar_lookup_entry` and the
inline `getEntry` now also write `size`/`csize`/`method`/`crc`/`name` **by name**
(`set_field_by_name`), mirroring `zip_real_jar::alloc_zip_entry`. Verified: probe
now reports `getSize()=9582, getMethod()=8`; boot advances from "first class" all
the way through the Quarkus bootstrap classloader + runtime init into SmallRye
Config.

### Gap 2 — NIO `Files.newInputStream` throws wrong exception for a missing file ✅ FIXED
**Symptom:** `ERROR SRCFG00035: Failed to load resource .../conf/keycloak-dev.conf
(os error 2)` aborts `start-dev`. HotSpot boots fine — `keycloak-dev.conf` does
**not** exist and is an *optional* profile config source.

**Root cause (general bug):** for a missing file, CratonVM's NIO
`Files.newInputStream` / `FileSystemProvider.newByteChannel` threw a generic
`java.io.IOException` ("os error 2"), but HotSpot throws
`java.nio.file.NoSuchFileException`. SmallRye Config treats a config source as
optional by catching `NoSuchFileException`; the generic `IOException` escapes the
catch → fatal. (Probe confirmed: HotSpot NIO → `NoSuchFileException`; CratonVM NIO
→ `IOException`. `FileInputStream` already threw `FileNotFoundException` correctly
on both.)

**Fix:** `native-builtins/src/phases_late.rs` — the three NIO handlers
(`FileSystemProvider.newByteChannel`, `FileSystemProvider.newInputStream`,
`Files.newInputStream`) now map `std::io::ErrorKind::NotFound` →
`p57_no_such_file(ctx, &p)` (the existing typed `java.nio.file.NoSuchFileException`
builder) instead of the generic `p57_io_error`. (Boot-verification pending an
isolated rebuild — the shared `target/` is under concurrent-session contention.)

### Gap 3 — `Profile.features` is null (synthetic `Stream` layout) ⏳ OPEN (next)
**Symptom:** with gaps 1+2 fixed, the boot reaches Keycloak's CLI config
validation and NPEs:
```
java.lang.NullPointerException: Cannot read field 'features' because the object is null
  at org.keycloak.common.Profile.isFeatureEnabled(Profile.java:495)
  at org.keycloak.infinispan.util.InfinispanUtils.isRemoteInfinispan(InfinispanUtils.java:51)
  at org.keycloak.quarkus.runtime.configuration.mappers.PropertyMapper.isRequired(PropertyMapper.java:215)
  at org.keycloak.quarkus.runtime.cli.Picocli.validateProperty/validateConfig
  at ...AbstractAutoBuildCommand.runCommand → QuarkusEntryPoint
```
So the real boot now gets through the entire Quarkus bootstrap + runtime init +
SmallRye config and into **Keycloak's own CLI config validation** — much closer
to ArC, but still pre-`Arc.initialize()`.

**Lead (strong):** immediately before the NPE the GC guard logs an out-of-bounds
field read on a **synthetic `java/util/stream/Stream`**:
`index=1 num_slots=1 class_id=ClassId(275) class_name=java/util/stream/Stream
real_field_count=Some(0)` — "speculative collection-layout probe dispatched on a
non-matching receiver type". `org.keycloak.common.Profile` builds its `features`
map via a stream `collect`; CratonVM's synthetic `Stream` (1 slot, 0 real fields)
returns wrong/empty data, so `Profile`'s current-instance `features` ends up null.
**Next step:** find which native allocates the 1-slot synthetic `Stream` that
`Profile.<init>`/`configure` consumes, and give it the real layout (or run the
real `java.util.stream` bytecode) so the feature map is populated. Likely a
general synthetic-Stream-layout bug (cf. the synthetic-collection-layout family).

### Gap 4+ — beyond
Not yet reached. After Profile/feature setup, expect Infinispan
(`InfinispanUtils`), datasource/Agroal, Hibernate, and finally
`Arc.initialize()` (the actual `CRATONVM_REAL_ARC` target).

## Key takeaway

**ArC is not directly reachable** — it is gated behind the full Quarkus boot.
Validating `CRATONVM_REAL_ARC` requires first walking the real server boot past
each gap above (and the ones after). Two general VM bugs (jar-entry metadata, NIO
missing-file exception type) are fixed; both are real bugs that affect more than
Keycloak. The boot is a productive, HotSpot-comparable driver for surfacing them
one at a time.
