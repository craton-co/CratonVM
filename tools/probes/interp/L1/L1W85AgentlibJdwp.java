// Interpreter round i4 wave 85, lane L1: `-agentlib:jdwp=...` reaches the
// built-in JDWP server (docs/internal/fixed-bugs/interpreter-i84-L1-agentlib-jdwp-is-not-the-built-in-server-FIXED-20261118.md).
// A cratonvm built with --features experimental-debug; a default build
// answers every case below with HotSpot's missing-library init error:
//   stdout: Error occurred during initialization of VM
//           Could not find agent library jdwp on the library path, with error: JVMTI support is not compiled into this CratonVM build (it needs the experimental-debug feature)
//   rc=1
// (HotSpot without the jdk.jdwp.agent module prints the same two lines with
// the dynamic loader's text, e.g. `libjdwp.so: cannot open shared object
// file: No such file or directory`.)
//
// Build: javac -d /tmp/l1w85jdwp tools/probes/interp/L1/L1W85AgentlibJdwp.java
// Each case: <vm> <options> -cp /tmp/l1w85jdwp L1W85AgentlibJdwp >out.txt 2>err.txt; echo "rc=$?"
// with <vm> = java, cratonvm --jdk-only, cratonvm --compatible.
//
// 1. -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=18585
//    out.txt: Listening for transport dt_socket at address: 18585
//             main ran
//    err.txt: empty   rc=0
// 2. -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,quiet=y,address=18586
//    out.txt: main ran   rc=0
// 3. -agentlib:jdwp=transport=dt_socket,server=yes
//    err.txt: ERROR: JDWP option syntax error: -agentlib:jdwp=transport=dt_socket,server=yes
//    out.txt: empty   rc=1
// 4. -agentlib:jdwp=server=y
//    err.txt: ERROR: JDWP no transport specified: -agentlib:jdwp=server=y   rc=1
// 5. -agentlib:jdwp=transport=dt_socket
//    err.txt: ERROR: JDWP Non-server transport dt_socket must have a connection address specified through the 'address=' option
//             ERROR: JDWP invalid option: -agentlib:jdwp=transport=dt_socket   rc=1
// 6. -agentlib:jdwp=address=1,transport=dt_socket
//    err.txt: ERROR: JDWP address specified without transport: -agentlib:jdwp=address=1,transport=dt_socket   rc=1
// 7. -agentlib:jdwp=transport=dt_socket,server=n,address=localhost:1,suspend=n   (nothing listens on port 1)
//    err.txt: ERROR: transport error 202: connect failed: Connection refused
//             ERROR: JDWP Transport dt_socket failed to initialize, TRANSPORT_INIT(510)
//             JDWP exit error AGENT_ERROR_TRANSPORT_INIT(197): No transports initialized [s\src\jdk.jdwp.agent\share\native\libjdwp\debugInit.c:697]
//    rc=2   (MEASURED, Temurin 25.0.3 on Windows, including the POSIX text
//    "Connection refused" there. The bracket is the JDK build's own source
//    path: on Linux/macOS CratonVM prints
//    [src/jdk.jdwp.agent/share/native/libjdwp/debugInit.c:697], not
//    measured; refresh JDWP_NO_TRANSPORTS_AT in vm/src/config.rs from that
//    host's HotSpot if its build path differs.)
// 8. -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=18585 -agentlib:jdwp=transport=dt_socket,server=y,suspend=n,address=18587
//    err.txt: ERROR: Cannot load this JVM TI agent twice, check your java command line for duplicate jdwp options.
//    out.txt: Error occurred during initialization of VM
//             agent library failed Agent_OnLoad: jdwp
//    rc=1
// 9. suspend=y with a debugger: tools/probes/interp/L1/L1W85EventSetOrder.java
//    launches its debuggee through JDI's CommandLineLaunch, i.e.
//    -agentlib:jdwp=transport=dt_socket,address=<host:port>,suspend=y
//    (server=n, the VM connects out, sends VM_START and waits).
//
// Not reproduced: `-agentlib:jdwp=help` prints an abbreviated usage (exit 0);
// launch=/onthrow=/onuncaught= are refused with a CratonVM line; an accept
// `timeout=` is not applied (connect only).
public class L1W85AgentlibJdwp {
    public static void main(String[] args) {
        System.out.println("main ran");
    }
}
