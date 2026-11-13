# ByteBuf access reproduction

Run `run-bytebuf-probe.sh CRATONVM JAVA_HOME NETTY_CLASSPATH OUTPUT_DIR [allow|default]` on Linux
with `perf` available. The classpath needs Netty common, buffer, JCTools core and JCTools core-jdk11;
use the same classes for all VMs. Outputs include exact binary hash, Java version,
per-process instructions and all checksum/timing rows. A nonzero process status,
timeout, or missing checksum stops the script.

Each buffer kind runs in a separate process to keep its access site monomorphic:
unpooled heap/direct, pooled heap/direct. Every process checks unaligned access,
both byte orders, bounds and slice aliasing, then releases the buffer. Five rounds
of one million variable long set/get pairs share one JVM. Round zero includes
warm-up; compare later rounds separately from total process instructions. Four
repetitions alternate VM order; no access checks or JIT policies are disabled.
The optional policy defaults to `allow`, which passes Java 25's explicit Unsafe
permission to every VM. `default` passes no permission flag and requires
`unsafe=false`. Run each policy into its own output directory. The policy file,
`unsafe` row, and implementation names identify the measured path.

This is a reproducible probe, not the missing `RunNetty bytebuf` driver referenced
by the enterprise benchmark runner. Its numbers cannot replace the old 983/9 ms
row without stating that the workload and measurement boundary changed.

For the composite-suite CPU investigation, build CratonVM with
`RUSTFLAGS='-C force-frame-pointers=yes -C target-feature=+sse4.2,+pclmulqdq'`.
Capture `perf record --call-graph fp -e instructions:u -c 2000003` with
`CRATONVM_JIT=perf-map` and the same runner arguments as the cost comparison.
Export and attribute the leaf samples:

```sh
perf script -G --no-inline -F comm,period,ip,sym,dso -i RUN.data > RUN.samples
python3 tools/bench/apps/netty/attribute-perf.py RUN.samples > RUN.buckets.json
python3 tools/bench/apps/netty/attribute-perf.py --self-test
```

The JSON accounts for every sample's instruction period by thread class and
execution category. Categories describe the sampled instruction, so a compiled
call's Rust bridge is runtime work. Unresolved symbols remain visible. Thread
names follow the Netty/JUnit runner; unfamiliar names appear as `other`.

For HotSpot, add `-XX:+UnlockDiagnosticVMOptions -XX:+PreserveFramePointer
-XX:+DumpPerfMapAtExit -XX:+LogCompilation -XX:LogFile=RUN.xml` to the profiled
process. Pass `RUN.xml` as the second argument to `attribute-perf.py` to identify
C1/C2 code ranges and count nmethod publications. Reused addresses with ambiguous
tiers remain unresolved. XML logging adds work: use these captures for attribution
and unlogged interleaved runs for the performance gate.
