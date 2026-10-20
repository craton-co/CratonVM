// Interpreter round i3 wave 81, lane L1: a `SingleStep` enabled by a JNI
// native is posted from the caller's next bytecode, against HotSpot
// (docs/internal/fixed-bugs/interpreter-i62-L1-single-step-enabled-in-a-frame-is-seen-only-at-its-next-call-or-back-edge-FIXED-20261115.md).
//
// `row` calls the native `startStep()` (which enables `SingleStep` for the
// calling thread), then `target(a, b)`, then `stopStep()`, which disables it
// and answers the steps posted in between, `<method>@<location>`, up to and
// including the first step in `target`. `row`'s bytecode (javac 25):
// 0 invokestatic startStep, 3 istore_2, 4 iload_0, 5 iload_1,
// 6 invokestatic target, 9 istore_3, ...
//
// Needs the native shim tools/probes/interp/L1/L1W81JvmtiStepLatency.c,
// loaded both as an agent and with `System.load` (for the natives):
//
//   gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o /tmp/libl1w81step.so tools/probes/interp/L1/L1W81JvmtiStepLatency.c
//   javac -d /tmp/l1w81step tools/probes/interp/L1/L1W81JvmtiStepLatency.java
//   java     -agentpath:/tmp/libl1w81step.so -cp /tmp/l1w81step L1W81JvmtiStepLatency /tmp/libl1w81step.so
//   cratonvm -agentpath:/tmp/libl1w81step.so -cp /tmp/l1w81step L1W81JvmtiStepLatency /tmp/libl1w81step.so
//   CRATONVM_JVMTI_STEP_POLL=1 cratonvm -agentpath:/tmp/libl1w81step.so -cp /tmp/l1w81step L1W81JvmtiStepLatency /tmp/libl1w81step.so
//
// (a `cratonvm` built with `--features experimental-debug`, as every C
// JVMTI probe needs). Without the argument it prints only the usage line.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// cold: enable=0 r=28 steps: row@3 row@4 row@5 row@6 target@0
// again: enable=0 r=-7 steps: row@3 row@4 row@5 row@6 target@0
// HOTSPOT_EXPECTED_END
//
// The block is derived, not measured: HotSpot 25.0.3 MEASURED (wave 62, the
// location-printing port of L1W61JvmtiStepIntoIntrinsic) posts a step at
// every bytecode of the caller from the one after the native's invoke
// (`row@3 row@4 row@5 row@6 row@8 target@0` for that probe's `row`), and
// this `row`'s locations follow from its bytecode above.
//
// CratonVM before wave 81 (page, wave 62): nothing in `row` until the frame
// switch, i.e. `steps: target@0`. Wave 81: HotSpot's rows with
// CRATONVM_JVMTI_STEP_POLL=1 (or CRATONVM_INTERP_HOISTED_OBSERVERS=1), both
// default off.
public class L1W81JvmtiStepLatency {
    /** Enables `SingleStep` for this thread; the error. */
    static native int startStep();

    /** Disables it; the steps posted, `<method>@<location>` each. */
    static native String stopStep();

    static int target(int a, int b) {
        return a * b + 1;
    }

    static String row(int a, int b) {
        int err = startStep();
        int r = target(a, b);
        String steps = stopStep();
        return "enable=" + err + " r=" + r + " steps: " + steps;
    }

    public static void main(String[] args) {
        if (args.length != 1) {
            System.out.println("usage: L1W81JvmtiStepLatency <path of the shim library>");
            return;
        }
        System.load(args[0]);
        System.out.println("cold: " + row(3, 9));
        System.out.println("again: " + row(-4, 2));
    }
}
