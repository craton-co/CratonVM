// Interpreter round i2 wave 61, lane L1: the third conformance probe of
// docs/internal/fixed-bugs/interpreter-L1-proposal-c-jvmti-suspension-and-frame-control-FIXED-20261025.md
// (item 5 of its wave-51 triage): a C agent's `SingleStep` through calls the
// VM may answer with an intrinsic (`Math.max`, `Math.abs`,
// `Integer.numberOfLeadingZeros`, `String.length`), against HotSpot, whose
// interpreter steps through each one's bytecode once single stepping is on.
//
// `step(a, b)` enables `SingleStep` for the calling thread, runs `target`,
// and disables it; the rows print how many steps `target` itself took and,
// for each callee, whether a step stopped in it. The rows run twice (the
// cold-VM first-call rows), then once more after `target` ran hot.
//
// Needs the native shim tools/probes/interp/L1/L1W61JvmtiStepIntoIntrinsic.c,
// loaded both as an agent and with `System.load` (for the natives):
//
//   gcc -shared -fPIC -I"$JAVA_HOME/include" -I"$JAVA_HOME/include/linux" \
//       -o /tmp/libl1w61step.so tools/probes/interp/L1/L1W61JvmtiStepIntoIntrinsic.c
//   javac -d /tmp/l1w61step tools/probes/interp/L1/L1W61JvmtiStepIntoIntrinsic.java
//   java     -agentpath:/tmp/libl1w61step.so -cp /tmp/l1w61step L1W61JvmtiStepIntoIntrinsic /tmp/libl1w61step.so
//   cratonvm -agentpath:/tmp/libl1w61step.so -cp /tmp/l1w61step L1W61JvmtiStepIntoIntrinsic /tmp/libl1w61step.so
//
// Without the argument it prints only the usage line. The Rust port of the
// shim is tools/probes/interp/L1/rust-ports/L1W61JvmtiStepIntoIntrinsic
// (README in tools/probes/interp/L1/rust-ports/).
//
// Expected stdout (HotSpot 25.0.3): MEASURED on the Windows box with the Rust
// port of the C shim (same calls, same order, same output), three runs, the
// same each time:
//
//   cold: enable=0 r=46 target=21 max=yes abs=yes numberOfLeadingZeros=yes length=yes
//   again: enable=0 r=41 target=21 max=yes abs=yes numberOfLeadingZeros=yes length=yes
//   warm sum=20004362143
//   hot: enable=0 r=47 target=21 max=yes abs=yes numberOfLeadingZeros=yes length=yes
//
// CratonVM (wave 61, a debug `cratonvm` with `experimental-debug`, both
// modes): `numberOfLeadingZeros=no` on every row and `target=20` on the
// first two:
// docs/internal/fixed-bugs/interpreter-L1-single-step-skips-a-method-a-registered-native-answers-FIXED-20261026.md.
// Since wave 62 (lane L1): HotSpot's lines, `--jdk-only`, `--compatible` and
// `--nojit`. Positive control: `CRATONVM_FRAME_TRACE=1` prints
// `[STOOD_IN] java/lang/Integer.numberOfLeadingZeros(I)I owner=java/lang/Integer jvmti_step=true`
// per row.
public class L1W61JvmtiStepIntoIntrinsic {
    /** Enables `SingleStep` for this thread; the error. */
    static native int startStep();

    /** Disables it; the methods stepped through, `name=count` each. */
    static native String stopStep();

    static int target(int a, int b, String s) {
        int m = Math.max(a, b);
        int n = Math.abs(a - b);
        int z = Integer.numberOfLeadingZeros(b);
        return m + n + z + s.length();
    }

    static String row(String label, int a, int b) {
        int err = startStep();
        int r = target(a, b, "abc");
        String steps = stopStep();
        StringBuilder out = new StringBuilder(label + ": enable=" + err + " r=" + r);
        String[] counts = steps.split(" ");
        for (String want : new String[] {"target", "max", "abs", "numberOfLeadingZeros", "length"}) {
            String found = "0";
            for (String c : counts) {
                if (c.startsWith(want + "=")) {
                    found = c.substring(want.length() + 1);
                }
            }
            out.append(' ').append(want).append('=');
            out.append(want.equals("target") ? found : (found.equals("0") ? "no" : "yes"));
        }
        return out.toString();
    }

    public static void main(String[] args) {
        if (args.length != 1) {
            System.out.println("usage: L1W61JvmtiStepIntoIntrinsic <path of the shim library>");
            return;
        }
        System.load(args[0]);
        System.out.println(row("cold", 3, 9));
        System.out.println(row("again", -4, 2));
        long sum = 0;
        for (int i = 0; i < 200_000; i++) {
            sum += target(i, i ^ 5, "x");
        }
        System.out.println("warm sum=" + sum);
        System.out.println(row("hot", 7, 1));
    }
}
