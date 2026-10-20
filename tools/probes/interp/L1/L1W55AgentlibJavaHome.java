// Interpreter round i2 wave 55, lane L1: `-agentlib:<name>` loads an agent
// installed in the JDK's own library directory, as HotSpot's
// `JvmtiAgent::load` finds it (`os::dll_locate_lib` over
// `Arguments::get_dll_dir()`: `<java.home>/lib` on Linux, `<java.home>\bin`
// on Windows) before it asks the library path
// (docs/internal/fixed-bugs/interpreter-L1-agentlib-does-not-search-the-jdks-library-directory-FIXED-20261019.md).
//
// Reuses the wave-53 shim tools/probes/interp/L1/L1W53JvmtiClassHooks.c
// (its `rows` native, bound to `L1W53JvmtiClassHooks`, so compile both
// probes together), installed in the JDK under an agent name of its own,
// with an EMPTY library path, so only the JDK directory can supply it:
//
//   gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o "$JAVA_HOME/lib/libl1w55home.so" tools/probes/interp/L1/L1W53JvmtiClassHooks.c
//   javac -d /tmp/l1w55home tools/probes/interp/L1/L1W53JvmtiClassHooks.java \
//       tools/probes/interp/L1/L1W55AgentlibJavaHome.java
//   env -u LD_LIBRARY_PATH java     -agentlib:l1w55home=w55 -cp /tmp/l1w55home L1W55AgentlibJavaHome "$JAVA_HOME/lib/libl1w55home.so"
//   env -u LD_LIBRARY_PATH cratonvm -agentlib:l1w55home=w55 -cp /tmp/l1w55home L1W55AgentlibJavaHome "$JAVA_HOME/lib/libl1w55home.so"
//   rm "$JAVA_HOME/lib/libl1w55home.so"
//
// (`$JAVA_HOME` is the JDK both VMs boot from: CratonVM's `java.home`,
// `VmConfig::java_home`. On Windows the Rust port
// tools/probes/interp/L1/rust-ports/L1W53JvmtiClassHooks goes to
// `%JAVA_HOME%\bin\l1w55home.dll`.) `System.load` of the same file hands the
// agent's already-loaded library back, so `rows` answers what its
// `Agent_OnLoad` recorded.
//
// Expected stdout (HotSpot 25): PREDICTED from `JvmtiAgent::load` and
// `os::dll_locate_lib` (the JDK copy is found; the options are the agent's).
// Not measured on the Windows box, whose JDK directory is not writable; the
// orchestrator's host run is the measurement:
//
//   onload: callbacks err=0 options=w55
//   agentlib: loaded
//
// CratonVM before wave 55 (code read): the boot stops with
// `Error occurred during initialization of VM` and `Could not find agent
// library l1w55home on the library path, with error: ...` (the bare
// `libl1w55home.so` was the only name tried). Since wave 55
// (`AgentRegistry::load_startup_agent`): HotSpot's lines. Unit test
// `jvmti::agent::tests::agentlib_looks_in_the_jdks_library_directory_first`.
public class L1W55AgentlibJavaHome {
    public static void main(String[] args) {
        if (args.length != 1) {
            System.out.println("usage: L1W55AgentlibJavaHome <path of the shim in the JDK>");
            return;
        }
        System.load(args[0]);
        System.out.print(L1W53JvmtiClassHooks.rows());
        System.out.println("agentlib: loaded");
    }
}
