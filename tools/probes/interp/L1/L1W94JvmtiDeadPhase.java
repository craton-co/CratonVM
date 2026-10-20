// Interpreter round i4 wave 94, lane L1: the C JVMTI table's dead phase
// (stage 2 of docs/internal/fixed-bugs/interpreter-i54-L1-proposal-one-jvmti-phase-per-vm-DONE-20261224.md).
// HotSpot's `jvmtiEnter` wrapper checks each function's phase from
// `jvmti.xml` before the function runs: the `phase="start"` functions (the
// class, method and object facts, `GetCurrentThread`) answer
// `JVMTI_ERROR_WRONG_PHASE` (112) outside the start and live phases, the
// live-only ones too, the `phase="any"` ones answer in every phase. Since wave
// 94 this VM's start-or-live functions check the phase too
// (`JvmtiNativeEnv::start_or_live_vm`); they answered in the dead phase while
// the VM still existed.
//
// The agent (tools/probes/interp/L1/L1W94JvmtiDeadPhase.c) calls ten
// functions from its `VMDeath` callback (live phase, 4) and again from
// `Agent_OnUnload` (dead phase, 8):
//
//   gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o /tmp/libl1w94dead.so tools/probes/interp/L1/L1W94JvmtiDeadPhase.c
//   (Windows: cl /LD /I"%JAVA_HOME%\include" /I"%JAVA_HOME%\include\win32" L1W94JvmtiDeadPhase.c /Fe:l1w94dead.dll)
//   javac -d /tmp/l1w94dead tools/probes/interp/L1/L1W94JvmtiDeadPhase.java
//   java     -agentpath:/tmp/libl1w94dead.so -cp /tmp/l1w94dead L1W94JvmtiDeadPhase
//   cratonvm [--compatible] -agentpath:/tmp/libl1w94dead.so -cp /tmp/l1w94dead L1W94JvmtiDeadPhase
//
// A/B: CRATONVM_JVMTI_C_PHASES=0 (no dead phase at all) prints `unload:
// phase=4` and the vmdeath answers again.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3 HotSpot, Windows, MSVC-built agent; MEASURED in wave 116)
// main
// vmdeath: phase=4 signature=0 modifiers=0 method=0 locals=104 hash=0 current=0 threads=0 time=0 errname=0 allocate=0
// unload: phase=8 signature=112 modifiers=112 method=112 locals=112 hash=112 current=112 threads=112 time=0 errname=0 allocate=0
// HOTSPOT_EXPECTED_END
//
// Settled by the wave-116 HotSpot run: the env is still valid in
// `Agent_OnUnload` (112, not 116), `current` is 0 in `VMDeath`, and `locals`
// is 104 (JVMTI_ERROR_NATIVE_METHOD: the agent asks `GetMaxLocals` of the
// native `Object.hashCode`; the derived block said 0). Build the agent on
// Windows from a "x64 Native Tools" prompt with the `cl` line above.
public class L1W94JvmtiDeadPhase {
    public static void main(String[] args) {
        System.out.println("main");
    }
}
