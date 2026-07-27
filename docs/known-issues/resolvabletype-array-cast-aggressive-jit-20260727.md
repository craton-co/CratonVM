# `ResolvableType[]` array-vs-element `ClassCastException` under aggressive JIT (2026-07-27)

## Summary

While re-verifying SPB.4/.4b/.4c (`org/springframework/boot/*` JIT bans,
removed 2026-07-27 -- see that removal comment in `vm/src/jit/skip_list.rs`
and `docs/known-issues/jit-bans/...`), a `CRATONVM_JIT_THRESHOLD=1`
aggressive-compilation pass over the real Spring Boot functional suite
(`spring-boot-tomcat-crossmodule-20260717/cratonvm-suite`, real
`spring-boot-4.0.6.jar`) surfaced a genuine `ClassCastException`:

```
<clinit> failed — wrapping in ExceptionInInitializerError
class=org/springframework/boot/context/config/Profiles
cause=java/lang/ClassCastException:
  class [Lorg.springframework.core.ResolvableType;
  cannot be cast to class org.springframework.core.ResolvableType
```

i.e. an array of `ResolvableType` (`ResolvableType[]`) is being cast to a
single, scalar `ResolvableType` -- an array-vs-element-type confusion.

## Scope: NOT related to the SPB.4/.4b/.4c bans

This reproduces **identically** whether `org/springframework/boot/` is
banned or allow-listed via `CRATONVM_JIT_ALLOW_PACKAGES` -- i.e. the bans
being removed do not gate this bug at all. The failing class
(`Profiles`) calls into `org/springframework/core/ResolvableType`, which
was already unbanned earlier this session (SPB.2's own removal,
2026-07-26). This is therefore an independent, standing correctness bug
in the JIT, not something either SPB.2 or SPB.4/.4b/.4c's removal
reintroduces or is responsible for.

## Reachability caveat

This did **not** reproduce under the suite's normal run (default JIT
tiering) -- only under `CRATONVM_JIT_THRESHOLD=1` (forces eager/aggressive
compilation from the first invocation). The offending code runs inside a
`<clinit>`, which executes exactly once per JVM process; under normal
tiered compilation a method that runs once essentially never accumulates
enough invocations to be promoted to a JIT-compiled tier, so this is very
unlikely to be hit by any real (non-aggressive-threshold) workload. It is
still a genuine, reproducible miscompile and worth fixing, just lower
real-world priority than a bug reachable at default thresholds.

## Repro

```
CP=/data/data/spring-boot-tomcat-crossmodule-20260717/cratonvm-suite/classes:$(ls /data/data/spring-boot-tomcat-crossmodule-20260717/cratonvm-suite/lib/*.jar | tr '\n' ':')
CRATONVM_JIT_THRESHOLD=1 /data/data/cratonvm/target/release/cratonvm \
  --java-home /data/jdk25-real-20260717/jdk-25.0.3+9 -cp "$CP" S01_Context
# -> SUITE S01_Context passed=10 failed=1 total=11 (baseline/default JIT: 15/15)
```

Reproduces with `CRATONVM_JIT_ALLOW_PACKAGES=org/springframework/boot/` set
or unset -- identical either way.

## Next steps for a follow-up session

- Root-cause in the JIT codegen: find what compiles
  `org/springframework/boot/context/config/Profiles.<clinit>` (or its
  callees resolving `ResolvableType` arrays, likely something like
  `ResolvableType.forClass`/`resolveGeneric`/a varargs `ResolvableType...`
  call) under `CRATONVM_JIT_THRESHOLD=1` and produces an array where a
  scalar reference was expected (or vice versa) -- classic array-covariance
  /boxing-vs-array-element miscompile, similar in shape to prior sessions'
  "boxed annot arrays" and varargs-related findings (see memory:
  `bug05-escape-analysis-varargs-ctor-receiver-null.md` for a related
  varargs-array pattern, though this is a distinct symptom).
- Given the low real-world reachability (only via `CRATONVM_JIT_THRESHOLD=1`,
  a debug/testing knob), this can be deprioritized relative to
  default-threshold-reachable bugs, but should not be silently dropped.
