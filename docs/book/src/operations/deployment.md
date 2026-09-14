# Deployment and Operations

This chapter describes how to package and operate CratonVM consistently. It is
an operational guide, not a certification statement: CratonVM is not a
certified Java implementation and has not completed a formal security audit.
Review the [compatibility policy](../reference/compatibility-policy.md), [known
limitations](../java-support/limitations.md), and [security
guidance](../security/overview.md) before choosing a workload.

## Deployment unit

A normal real-JDK deployment consists of:

1. a release build of the `cratonvm` executable;
2. a supported JDK installation containing the runtime modules;
3. application classes and dependency JARs; and
4. an explicit, reviewed VM configuration.

Prefer real-JDK mode for applications and frameworks. Synthetic-JDK mode is a
standalone compatibility and differential-testing path; it is not a drop-in
replacement for every JDK library.

Build the launcher with the default release profile:

```bash
cargo build --release -p cratonvm-cli --bin cratonvm
```

The workspace release profile uses fat LTO and one codegen unit. Do not compare
the performance of an LTO-disabled build with the published baselines. Copy the
finished executable under a versioned name, record its Git commit and checksum,
and deploy that immutable artifact:

```bash
install -m 755 target/release/cratonvm /opt/cratonvm/bin/cratonvm-0.3.0
sha256sum /opt/cratonvm/bin/cratonvm-0.3.0
```

Keep the JDK version equally explicit. Relying on whichever `java` happens to be
first on `PATH` makes rollbacks and incident reproduction unnecessarily hard.

## Stable launch shape

Use one of the two supported entry forms:

```bash
cratonvm --java-home /opt/jdk-25 -Xmx2g --jar app.jar
cratonvm --java-home /opt/jdk-25 -Xmx2g -cp "app:lib/*" com.example.Main
```

For a service, pin at least:

- the CratonVM artifact and commit;
- the JDK vendor and version;
- `--java-home`;
- `-Xmx`;
- the collector selection;
- the application classpath or JAR;
- Java system properties; and
- any `CRATONVM_*` setting that changes execution.

Put long argument lists in a Java-style `@file` and keep that file under the
same configuration management as the service:

```text
# cratonvm.args
--java-home /opt/jdk-25
-Xmx2g
--classpath app:lib/*
-Dapp.environment=production
```

```bash
cratonvm @cratonvm.args com.example.Main
```

See [Running Programs](../user-guide/running-programs.md), the [command-line
reference](../user-guide/cli-reference.md), and
[Configuration](../user-guide/configuration.md) for the complete syntax.

## Memory and collectors

Set `-Xmx` explicitly. CratonVM's generational heap commits substantial backing
memory at startup, so an unconstrained or incorrectly detected host limit can
turn into an avoidable container eviction.

The default collector is the generational collector with a non-moving young
sweep and selective promotion. Other modes have different maturity and
performance characteristics:

| Mode | Operational position |
|------|----------------------|
| Default generational | First choice for general workloads. |
| `-XX:+UseG1GC` | Experimental; validate on the exact workload before rollout. |
| Moving young generation | Opt-in and fail-closed when root coverage is not proven; use for controlled evaluation. |
| `--nojit` | Diagnostic fallback, not a normal performance configuration. |

Leave headroom outside the Java heap for compiled code, class metadata, Rust
allocations, native libraries, thread stacks, JFR buffers, and mapped JARs. In a
container, make the cgroup limit larger than `-Xmx`; do not set both to the same
number. See [Containers and cgroups](../user-guide/containers.md) and [Memory
and Garbage Collection](../user-guide/memory-and-gc.md).

## Security boundary

CratonVM does not turn arbitrary Java bytecode into a trusted sandbox. For
untrusted or multi-tenant code, use an operating-system or container boundary
with:

- a non-root service account;
- a read-only application filesystem where possible;
- a private writable data directory;
- explicit network egress policy;
- CPU, memory, process, and file-descriptor limits;
- no host device access unless required; and
- a minimal JDK and native-library set.

Do not disable bytecode verification in production. `--noverify` is a
diagnostic switch that removes a safety boundary. Review [Sandboxing and
Hardening](../security/sandboxing.md) and the cryptography limitations before
exposing a service to hostile inputs.

## Startup and health

Treat successful process creation as necessary but insufficient. A readiness
check should prove application behavior after:

1. JDK discovery and VM bootstrap;
2. class loading and static initialization;
3. native registration;
4. application dependency initialization; and
5. opening required listeners or completing a small application transaction.

Use an application-level health endpoint or command. CratonVM does not define a
universal service-health protocol because command-line tools and servers need
different readiness semantics.

Keep startup logs. When investigating bootstrap failures, add
`--verbose:class`; for collector behavior add `--verbose:gc` or a focused
`--Xlog` specification. Remove high-volume diagnostics after the incident.

## Rollout and rollback

A safe rollout preserves both the old executable and the old configuration:

1. build and checksum a uniquely named artifact;
2. run the application's correctness smoke against HotSpot and CratonVM;
3. run the CratonVM smoke once with JIT and once with `--nojit`;
4. canary the exact artifact/JDK/configuration tuple;
5. compare errors, startup time, memory, GC, and tail latency;
6. expand gradually; and
7. roll back the whole tuple, not just the executable.

Do not change the VM, JDK, collector, heap size, and application version in one
unattributable rollout.

## Shutdown and data integrity

Applications should use their normal Java shutdown protocol: stop accepting
work, finish or reject in-flight operations, flush durable state, and then let
the process exit. External supervisors should allow an application-specific
grace period before force termination.

For crash-only applications, ensure durability belongs to the database or file
format rather than to process-exit hooks. A VM crash can prevent shutdown hooks
from running, just as it can on other JVMs.

## Deployment checklist

- [ ] Release build, Git commit, artifact size, and SHA-256 recorded.
- [ ] Real JDK vendor/version and `--java-home` pinned.
- [ ] Classpath or executable JAR pinned.
- [ ] Explicit `-Xmx` with native-memory headroom.
- [ ] Collector and JIT settings tested on the target workload.
- [ ] Verification enabled.
- [ ] OS/container isolation and egress policy reviewed.
- [ ] Application readiness and liveness checks defined.
- [ ] Logs and incident artifacts have retention limits.
- [ ] Canary, rollback artifact, and rollback configuration tested.
