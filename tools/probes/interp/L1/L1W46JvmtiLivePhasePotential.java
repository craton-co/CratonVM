// Interpreter round i1 wave 46, lane L1: which of the wave-44 capabilities
// (`can_access_local_variables`, `can_get_current_contended_monitor`,
// `can_get_owned_monitor_stack_depth_info`) an env obtained in the live
// phase may acquire, against HotSpot (item 4 of
// docs/internal/fixed-bugs/interpreter-L1-the-c-jvmti-table-reads-no-other-threads-stack-FIXED-20261010.md).
//
// The shim's `Agent_OnLoad` acquires the capabilities its option names
// (`none`, `locals`, or `all` three) in the OnLoad phase. The native `rows`
// then obtains a second env in the live phase and prints, for it, the
// potential bit of each of the three and `AddCapabilities` of each alone;
// then the startup env relinquishes what it holds and a third env is asked
// the same.
//
// Needs the native shim tools/probes/interp/L1/L1W46JvmtiLivePhasePotential.c,
// loaded both as an agent and with `System.load` (for `rows`):
//
//   gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o /tmp/libl1w46pot.so tools/probes/interp/L1/L1W46JvmtiLivePhasePotential.c
//   javac -d /tmp/l1w46pot tools/probes/interp/L1/L1W46JvmtiLivePhasePotential.java
//   for o in none locals all; do
//     java     -agentpath:/tmp/libl1w46pot.so=$o -cp /tmp/l1w46pot L1W46JvmtiLivePhasePotential /tmp/libl1w46pot.so
//     cratonvm -agentpath:/tmp/libl1w46pot.so=$o -cp /tmp/l1w46pot L1W46JvmtiLivePhasePotential /tmp/libl1w46pot.so
//   done
//
// Without the argument it prints only the usage line.
//
// Expected stdout (HotSpot 25.0.3): measured on the Windows box with a Rust
// port of the C shim (the box has no C compiler; same calls, same order,
// same output format), three runs of each option, the same each time. One
// block per option, in the loop's order:
//
//   option none: Agent_OnLoad AddCapabilities: err=0
//   live-phase env: potential 0 0 0 add 98 98 98
//   startup env relinquishes: err=0
//   live-phase env after the relinquish: potential 0 0 0 add 98 98 98
//
//   option locals: Agent_OnLoad AddCapabilities: err=0
//   live-phase env: potential 1 0 0 add 0 98 98
//   startup env relinquishes: err=0
//   live-phase env after the relinquish: potential 1 0 0 add 0 98 98
//
//   option all: Agent_OnLoad AddCapabilities: err=0
//   live-phase env: potential 1 1 1 add 0 0 0
//   startup env relinquishes: err=0
//   live-phase env after the relinquish: potential 1 1 1 add 0 0 0
//
// So each of the three is potential in the live phase exactly when a startup
// agent acquired it in `Agent_OnLoad`, and stays so after it is
// relinquished.
//
// CratonVM before wave 46 (from reading `jvmti::native_env::POTENTIAL`):
// every live-phase env lists all three as potential and adds them, whatever
// the startup agent acquired (`potential 1 1 1 add 0 0 0` in every block):
// more permissive than HotSpot, never a wrong value.
//
// CratonVM since wave 46, predicted from the code: HotSpot's blocks. `Vm::new`
// records what the startup envs hold of the three
// (`native_env::bind_startup_envs`, `JvmtiEnv::onload_acquired`), and a bound
// env's potential leaves out the others (`JvmtiNativeEnv::potential`). No
// debug line: the `potential` / `add` rows are the positive control (the
// `none` and `locals` blocks change). Unit test
// `the_onload_only_capabilities_follow_what_a_startup_env_acquired`.
public class L1W46JvmtiLivePhasePotential {
    static native String rows();

    public static void main(String[] args) {
        if (args.length != 1) {
            System.out.println("usage: L1W46JvmtiLivePhasePotential <path of the shim library>");
            return;
        }
        System.load(args[0]);
        System.out.print(rows());
    }
}
