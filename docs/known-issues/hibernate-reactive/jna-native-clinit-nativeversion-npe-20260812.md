# JNA `Native.<clinit>` throws NPE on `nativeVersion.split(...)` — breaks Testcontainers' rootless-Docker probe

**Status:** OPEN (2026-08-12), workaround available. Found on Azure host
`azureuser@20.80.105.49` while standing up a real-Postgres run of the
hibernate-reactive suite (Testcontainers-provisioned `postgres:18.4`).

## Symptom

```
java.lang.NullPointerException: Cannot invoke "String.split(String)" because "nativeVersion" is null
	at com.sun.jna.Native.isCompatibleVersion(Native.java:223)
	at com.sun.jna.Native.<clinit>(...)
```

Triggered when Testcontainers probes Docker client strategies and reaches
`RootlessDockerClientProviderStrategy$LibC`, a JNA `Library` interface —
loading it forces `com.sun.jna.Native`'s static initializer, which reads a
native-library-reported version string (`nativeVersion`) and calls
`.split(...)` on it without a null check. On CratonVM this value comes back
`null`; the class genuinely exists in `testcontainers-1.21.4.jar` on the
classpath and **loads and initializes cleanly under stock HotSpot** with
the identical classpath — CratonVM-specific.

Not yet root-caused: `Native.isCompatibleVersion` calls into JNA's own
native-version-detection logic, which on a real JVM resolves a
`Native.VERSION_NATIVE` (or equivalent) string, likely by reading a
resource embedded in `jna.jar` or a JNI call into `libjnidispatch`. Where
exactly the CratonVM path returns `null` instead of a version string (JNI
bootstrap, resource loading, or something else) has not yet been isolated.

## Workaround

Testcontainers tries Docker client strategies in order and only reaches
`RootlessDockerClientProviderStrategy` if earlier strategies don't apply.
Setting `DOCKER_HOST=unix:///var/run/docker.sock` makes
`EnvironmentAndSystemPropertyClientProviderStrategy` match first, so the
JNA-backed rootless probe is never reached at all:

```bash
export DOCKER_HOST=unix:///var/run/docker.sock
```

Confirmed this lets Testcontainers genuinely provision and reach a real
Postgres container on CratonVM. This is a routing-around, not a fix — any
code path that unconditionally goes through JNA's rootless-Docker
detection (or any other JNA `Library` init) with no env override available
would still hit this NPE.

## Impact

Silently breaks anything depending on `Native.isCompatibleVersion` /
JNA `Library` initialization when the JVM-reported native version is
absent — likely broader than just Testcontainers' Docker-strategy probe,
since this is JNA's own class-init path, not something Testcontainers-
specific. Any other JNA-based library loaded on CratonVM should be assumed
at risk until this is root-caused.

## Repro

```bash
# On azureuser@20.80.105.49, /data/cratonvm, with a real JDK + testcontainers-1.21.4.jar on cp:
# Force RootlessDockerClientProviderStrategy to be probed (e.g. unset DOCKER_HOST,
# no ~/.docker/config.json context override) and let Testcontainers auto-detect.
```
Deterministic once the rootless-strategy probe is reached; confirmed absent
under stock HotSpot with the identical classpath.

## Related

- `docs/known-issues/hibernate-reactive/vertx-pg-sasl-scram-handshake-fails-20260812.md`
  — found in the same investigation; this workaround was required just to
  get far enough to reach that (separate, dominant) defect.
