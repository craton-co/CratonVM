// Interpreter round i2 wave 54, lane L1: a startup agent that fails stops
// the VM with HotSpot's initialization error
// (docs/internal/fixed-bugs/interpreter-L1-a-failed-startup-agent-panics-instead-of-hotspots-init-error-FIXED-20261018.md).
//
// The probe's own `main` must never run: the agent fails first. It uses the
// shim of wave 53's probe, whose `Agent_OnLoad` returns 7 when its option
// string is `fail` (tools/probes/interp/L1/L1W53JvmtiClassHooks.c; build it
// as that probe's header says):
//
//   javac -d /tmp/l1w54init tools/probes/interp/L1/L1W54JvmtiAgentInitError.java
//   java     -agentpath:/tmp/libl1w53hooks.so=fail -cp /tmp/l1w54init L1W54JvmtiAgentInitError
//   cratonvm -agentpath:/tmp/libl1w53hooks.so=fail -cp /tmp/l1w54init L1W54JvmtiAgentInitError
//
// and, for a library that does not load (compare the first line and the
// second up to `with error: `; the rest is the platform loader's text):
//
//   java     -agentpath:/nonexistent/libnope.so -cp /tmp/l1w54init L1W54JvmtiAgentInitError
//   cratonvm -agentpath:/nonexistent/libnope.so -cp /tmp/l1w54init L1W54JvmtiAgentInitError
//   java     -agentlib:nope -cp /tmp/l1w54init L1W54JvmtiAgentInitError
//   cratonvm -agentlib:nope -cp /tmp/l1w54init L1W54JvmtiAgentInitError
//
// Expected (HotSpot 25.0.3, MEASURED on the Windows box with the Rust port of
// the wave-53 shim, three runs, the same each time): exit status 1, nothing
// on stderr, on stdout
//
//   Error occurred during initialization of VM
//   agent library failed Agent_OnLoad: /tmp/libl1w53hooks.so
//
// (the path exactly as the command line gave it), and for the two libraries
// that do not load
//
//   Error occurred during initialization of VM
//   Could not find agent library /nonexistent/libnope.so in absolute path, with error: <os text>
//
//   Error occurred during initialization of VM
//   Could not find agent library nope on the library path, with error: <os text>
//
// (`-agentpath:` says `in absolute path` for a relative path too). CratonVM
// before wave 54: a Rust panic, `failed to load JVMTI startup agent ...`, on
// stderr with Rust's panic status. The positive control is the unit test
// `vm::vm_init::tests::startup_jvmti_missing_native_agent_is_hotspots_init_error`.
// Note: `cratonvm -version` with a failing agent still prints the version
// (the launcher answers `-version` without a VM; HotSpot runs `Agent_OnLoad`
// first): run the probe with its class, as above.
public class L1W54JvmtiAgentInitError {
    public static void main(String[] args) {
        System.out.println("main ran: the agent did not stop the VM");
    }
}
