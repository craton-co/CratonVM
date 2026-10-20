// Interpreter round i2 wave 55, lane L1: the C JVMTI table's phases against
// HotSpot: the primordial phase before `System.initPhase1` returns, `VMStart`
// (57), the start, live and dead phases, and `can_generate_all_class_hook_events`
// (docs/internal/fixed-bugs/interpreter-L1-the-c-jvmti-start-phase-begins-before-initphase1-and-vmstart-is-not-posted-FIXED-20261019.md,
// docs/internal/fixed-bugs/interpreter-i54-L1-proposal-one-jvmti-phase-per-vm-DONE-20261224.md).
//
// The shim's `Agent_OnLoad` reads `GetPhase` (OnLoad, 1), sets callbacks for
// `VMStart`, `VMInit`, `VMDeath`, `ClassLoad`, `ClassPrepare` and
// `ThreadStart` and enables the six for every thread; then reads whether
// `can_generate_all_class_hook_events` is potential and adds it. The
// `VMStart` callback counts itself, reads `GetPhase`, and calls five
// functions in the start phase: `GetCurrentThread` (start or live),
// `GetAllThreads` and `GetLoadedClasses` (live only: WRONG_PHASE 112),
// `GetClassSignature` of `java.lang.Object` (start or live) and
// `SetEventNotificationMode` (OnLoad or live: 112). `ClassLoad`,
// `ClassPrepare` and `ThreadStart` only note whether one arrived before
// `VMStart` (yes/no, not a count, which differs between VMs). `VMInit`
// counts itself, reads the phase and whether `VMStart` came first. `main`
// prints what was seen, its own phase, whether the capability is potential
// for an env obtained in the live phase, and its phase again; `VMDeath` and
// `Agent_OnUnload` print their phase from C (`fflush`ed), after `main`'s
// lines.
//
// Needs the native shim tools/probes/interp/L1/L1W55JvmtiPhases.c, loaded
// both as an agent and with `System.load` (for the natives):
//
//   gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o /tmp/libl1w55phases.so tools/probes/interp/L1/L1W55JvmtiPhases.c
//   javac -d /tmp/l1w55phases tools/probes/interp/L1/L1W55JvmtiPhases.java
//   java     -agentpath:/tmp/libl1w55phases.so -cp /tmp/l1w55phases L1W55JvmtiPhases /tmp/libl1w55phases.so
//   cratonvm -agentpath:/tmp/libl1w55phases.so -cp /tmp/l1w55phases L1W55JvmtiPhases /tmp/libl1w55phases.so
//
// Without the argument it prints only the usage line. The Rust port of the
// shim is tools/probes/interp/L1/rust-ports/L1W55JvmtiPhases (README in
// tools/probes/interp/L1/rust-ports/).
//
// Expected stdout (HotSpot 25.0.3): MEASURED on the Windows box with the Rust
// port of the C shim (same calls, same order, same output), three runs, the
// same each time:
//
//   onload: phase=1 callbacks=0 start=0 init=0 death=0 load=0 prepare=0 thread=0
//   onload all class hook events: potential err=0 bit=1 add=0
//   vmstart: seen=1 phase=6
//   before vmstart: load=no prepare=no thread=no
//   start phase calls: current=0 threads=112 loaded=112 signature=0 mode=112
//   vminit: seen=1 phase=4 after vmstart=yes
//   main: phase=4
//   live all class hook events: potential err=0 bit=1 add=0
//   main again: phase=4
//   vmdeath: phase=4
//   unload: phase=8
//
// CratonVM before wave 55 (code read): `start=98` (VMStart not available),
// `onload all class hook events: potential err=0 bit=0 add=98`,
// `vmstart: seen=0 phase=0`, `before vmstart: load=yes prepare=yes
// thread=yes` (the start phase began at `Vm::new`, before
// `System.initPhase1`, and every class and thread event of the boot came
// before a `VMStart` that never came), the `start phase calls` row `-1` five
// times (never called), `vminit: ... after vmstart=no`, the live capability
// row `bit=0 add=98`, and `unload: phase=4`. Since wave 55, with
// `CRATONVM_JVMTI_C_PHASES` on (the default from the lane's last commit) and
// `CRATONVM_JVMTI_C_CLASS_HOOKS` on (the default): HotSpot's lines, MEASURED
// on the Windows box with a debug `cratonvm` and the Rust port, both modes;
// with `CRATONVM_JVMTI_C_PHASES=0`, the rows before wave 55 except the
// capability rows (measured too). The `vmstart: seen=1` row is the positive control
// (`jvmti::native_env::enter_start_phase`); unit test
// `jvmti::native_env::tests::a_startup_agent_gets_vm_start_after_the_primordial_phase`.
public class L1W55JvmtiPhases {
    static native String rows();

    static native int phase();

    static native String livePotential();

    public static void main(String[] args) {
        if (args.length != 1) {
            System.out.println("usage: L1W55JvmtiPhases <path of the shim library>");
            return;
        }
        System.load(args[0]);
        System.out.print(rows());
        System.out.println(new StringBuilder("main: phase=").append(phase()).toString());
        System.out.println(new StringBuilder("live all class hook events: ").append(livePotential()).toString());
        System.out.println(new StringBuilder("main again: phase=").append(phase()).toString());
    }
}
