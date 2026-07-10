# ES FAIL family - Build current holder receives null Manifest

Status: OPEN

Observed in:
- Run: `esfull-20260710-083851`
- Host: local Windows box
- CratonVM binary: `C:\craton\cratonvm-targets\es-full-local-20260710-083851\release\cratonvm-es-full-local-20260710-083851.exe`
- Base branch: current `dev` as of 2026-07-10
- Suite mode: `craton` / JIT on
- Stopped partial run totals: 367 recorded classes, 69 PASS, 283 FAIL, 15 HANG, 0 CRASH

Family count:
- 265 rows in the stopped partial run.
- The runner note usually records the first recoverable warning:
  `java.lang.UnsatisfiedLinkError: Native library [C:\craton\CratonVM\apps\elasticsearch\lib\platform\windows-x64\zstd.dll] does not exist`.
- The process-killing exception is later in stderr: `ExceptionInInitializerError` from `org/elasticsearch/Build$CurrentHolder`, caused by `NullPointerException: Cannot invoke "java.util.jar.Manifest.getMainAttributes()" because "manifest" is null`.

Representative class:
- `libs/cli-terminal org.elasticsearch.cli.terminal.internal.EcsJsonUtilsTests`

Control:
- HotSpot with the same Elasticsearch checkout and missing `zstd.dll` path passes the representative class:
  `esprobe-hotspot-zstd-20260710`, `hotspot-zstd`, `PASS`, 2.3s, `OK (14 tests)`.
- HotSpot also logs the same recoverable native-access warning before continuing.

Evidence:
- Craton stdout: `C:\craton\esfull-20260710-083851\results\esfull-20260710-083851\jit-shard1\logs\libs_cli-terminal.org.elasticsearch.cli.terminal.internal.EcsJsonUtilsTests.out.log`
- Craton stderr: `C:\craton\esfull-20260710-083851\results\esfull-20260710-083851\jit-shard1\logs\libs_cli-terminal.org.elasticsearch.cli.terminal.internal.EcsJsonUtilsTests.err.log`
- HotSpot stdout: `C:\craton\esfull-20260710-083851\results\esprobe-hotspot-zstd-20260710\hotspot-zstd\logs\libs_cli-terminal.org.elasticsearch.cli.terminal.internal.EcsJsonUtilsTests.out.log`

Key stderr signal:
```text
<clinit> failed - wrapping in ExceptionInInitializerError class=org/elasticsearch/Build$CurrentHolder
cause=java/lang/NullPointerException Cannot invoke "java.util.jar.Manifest.getMainAttributes()" because "manifest" is null
...
at org/elasticsearch/Build$CurrentHolder.findCurrent (Build.java:53)
at org/elasticsearch/Build.findLocalBuild (Build.java:80)
```

Interpretation:
- This is not a missing-library setup failure by itself. HotSpot sees the same absent native library, logs it, disables native methods, and continues.
- The CratonVM-specific failure is that the later Elasticsearch build metadata path receives a null `Manifest` where HotSpot finds usable build metadata.
- Likely areas: jar URL / code-source / manifest loading, or a class-resource path used by `Build.findLocalBuild`.

Not duplicates:
- Do not split this into one document per affected class. The 265 affected rows share this same bootstrap failure shape.
- This is distinct from the older `NativeAccessHolder catch LinkageError` note because the warning is logged and execution continues past native-access initialization before the `Build$CurrentHolder` failure.
