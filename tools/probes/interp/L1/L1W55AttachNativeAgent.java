// Interpreter round i2 wave 55, lane L1: a self-attach loads a native agent
// (`VirtualMachine.loadAgentPath` / `loadAgentLibrary`), against HotSpot
// (docs/internal/fixed-bugs/interpreter-L1-self-attach-cannot-load-a-native-agent-FIXED-20261019.md).
//
// The probe loads the shim with `System.load` (for `rows`), attaches to its
// own process and loads the shim as an agent five ways: with options, with
// none (NULL), with options "fail" (its `Agent_OnAttach` answers 3), a path
// that does not exist, a library name found nowhere, and the shim again.
// `Agent_OnAttach` records its options, `GetEnv` for JVMTI 1.2 and the phase
// that env sees. A failure prints the exception's class, its message up to
// the first ':' (the rest is the platform loader's text) and, for an
// `AgentInitializationException`, `returnValue()`.
//
// Needs the native shim tools/probes/interp/L1/L1W55AttachNativeAgent.c and
// `-Djdk.attach.allowAttachSelf=true`; the argument must be an absolute path:
//
//   gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o /tmp/libl1w55attach.so tools/probes/interp/L1/L1W55AttachNativeAgent.c
//   javac -d /tmp/l1w55attach tools/probes/interp/L1/L1W55AttachNativeAgent.java
//   java     -Djdk.attach.allowAttachSelf=true -cp /tmp/l1w55attach L1W55AttachNativeAgent /tmp/libl1w55attach.so
//   cratonvm -Djdk.attach.allowAttachSelf=true -cp /tmp/l1w55attach L1W55AttachNativeAgent /tmp/libl1w55attach.so
//
// Without the argument it prints only the usage line. The Rust port of the
// shim is tools/probes/interp/L1/rust-ports/L1W55AttachNativeAgent (README in
// tools/probes/interp/L1/rust-ports/). HotSpot also prints its three-line
// "A JVM TI agent has been loaded dynamically" warning on stderr, once.
//
// Expected stdout (HotSpot 25.0.3): MEASURED on the Windows box with the Rust
// port of the C shim (same calls, same order, same output), three runs, the
// same each time:
//
//   path with options: loaded
//   onattach 1: options=opts getenv=0 phase=4
//   path without options: loaded
//   onattach 2: options= getenv=0 phase=4
//   failing agent: com.sun.tools.attach.AgentInitializationException [Agent_OnAttach failed] returnValue=3
//   onattach 3: options=fail getenv=0 phase=4
//   missing path: com.sun.tools.attach.AgentLoadException [Failed to load agent library: ...]
//   missing library: com.sun.tools.attach.AgentLoadException [Failed to load agent library: ...]
//   path again: loaded
//   onattach 4: options=again getenv=0 phase=4
//   detached
//
// CratonVM before wave 55 (code read): each `load` ran the JDK's
// `HotSpotVirtualMachine` code on the bare `VirtualMachine` the in-process
// `attach` returns, with no attach listener (an `AbstractMethodError` or an
// `IOException` row), and no `onattach` row. Since wave 55
// (`instrument::native_vm_load_native_agent`): HotSpot's lines (MEASURED on
// the Windows box with a debug `cratonvm` and the Rust port, both modes; the
// `onattach` rows are the positive control). Not behind
// a switch: the natives replace a path that could not work.
import com.sun.tools.attach.AgentInitializationException;
import com.sun.tools.attach.AgentLoadException;
import com.sun.tools.attach.VirtualMachine;

public class L1W55AttachNativeAgent {
    static native String rows();

    static String shape(Throwable t) {
        String message = String.valueOf(t.getMessage());
        int colon = message.indexOf(':');
        String head = colon < 0 ? message : message.substring(0, colon) + ": ...";
        StringBuilder sb = new StringBuilder(t.getClass().getName()).append(" [").append(head).append(']');
        if (t instanceof AgentInitializationException) {
            sb.append(" returnValue=").append(((AgentInitializationException) t).returnValue());
        }
        return sb.toString();
    }

    static void load(VirtualMachine vm, String label, String path, String options, boolean library) {
        try {
            if (library) {
                vm.loadAgentLibrary(path, options);
            } else if (options == null) {
                vm.loadAgentPath(path);
            } else {
                vm.loadAgentPath(path, options);
            }
            System.out.println(new StringBuilder(label).append(": loaded").toString());
        } catch (Exception e) {
            System.out.println(new StringBuilder(label).append(": ").append(shape(e)).toString());
        }
        System.out.print(rows());
    }

    public static void main(String[] args) throws Exception {
        if (args.length != 1) {
            System.out.println("usage: L1W55AttachNativeAgent <absolute path of the shim library>");
            return;
        }
        System.load(args[0]);
        VirtualMachine vm = VirtualMachine.attach(String.valueOf(ProcessHandle.current().pid()));
        load(vm, "path with options", args[0], "opts", false);
        load(vm, "path without options", args[0], null, false);
        load(vm, "failing agent", args[0], "fail", false);
        load(vm, "missing path", args[0] + ".absent", "x", false);
        load(vm, "missing library", "l1w55-no-such-agent", "x", true);
        load(vm, "path again", args[0], "again", false);
        vm.detach();
        System.out.println("detached");
    }
}
