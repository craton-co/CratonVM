// Interpreter round i2 wave 58, lane L1: which of the Java stack walkers list
// a lambda proxy's frame (`Host$$Lambda.run`) between the lambda's body and
// its caller, against HotSpot
// (docs/internal/fixed-bugs/interpreter-L1-the-c-jvmti-table-does-not-know-the-vms-lambda-proxy-classes-FIXED-20261007.md,
// "Next step (from wave 57)" item 1).
//
// The body of a `Runnable` lambda, a `Supplier` lambda and a method reference
// each print, for the frames from the body down to `main`'s caller of the
// functional method, the class (a proxy printed as `<proxy>`) and method of
// each frame, as seen by:
//
// * `throwable`: `new Throwable().getStackTrace()`;
// * `walker`: `StackWalker.getInstance()` (default options);
// * `walker+hidden`: `StackWalker.getInstance(Option.SHOW_HIDDEN_FRAMES)`.
//
// Each row lists the frames above the probe's own `call*` method, innermost
// first. The first case again at the end (cold-VM first call).
//
// Expected stdout (HotSpot 25.0.3, default flags; MEASURED on the Windows box,
// also with -Xint, the same; `P` is `L1W58LambdaProxyFrames`, abbreviated
// here only -- the probe prints the full name):
//
//   Runnable throwable: P.report < P.lambda$callRunnable$0 < P.callRunnable
//   Runnable walker: P.report < P.lambda$callRunnable$0 < P.callRunnable
//   Runnable walker+hidden: P.report < P.lambda$callRunnable$0 < <proxy>.run < P.callRunnable
//   Supplier throwable: P.report < P.lambda$callSupplier$0 < P.callSupplier
//   Supplier walker: P.report < P.lambda$callSupplier$0 < P.callSupplier
//   Supplier walker+hidden: P.report < P.lambda$callSupplier$0 < <proxy>.get < P.callSupplier
//   Ref throwable: P.report < P.target < P.callRef
//   Ref walker: P.report < P.target < P.callRef
//   Ref walker+hidden: P.report < P.target < <proxy>.run < P.callRef
//   (the three Runnable rows again)
//
// So a `Throwable` trace and a default `StackWalker` hide the proxy's frame
// (a hidden class, `-XX:-ShowHiddenFrames`), and only `SHOW_HIDDEN_FRAMES`
// lists it -- with the JVMTI stack functions and JDWP, which list every
// frame (`L1W56JvmtiLambdaMethods`' `stack:` rows).
//
// CratonVM (code read, wave 58): the interpreter's lambda dispatch calls the
// body with no frame for the proxy, so the `throwable` and `walker` rows are
// HotSpot's and the `walker+hidden` rows lack `<proxy>.run` / `<proxy>.get`.
import java.util.ArrayList;
import java.util.List;
import java.util.function.Supplier;

public class L1W58LambdaProxyFrames {
    static String name(String n, String method) {
        if (n.contains("$$Lambda")) {
            n = "<proxy>";
        }
        return n + "." + method;
    }

    static String walk(String label, List<String> frames) {
        List<String> out = new ArrayList<>();
        for (String f : frames) {
            out.add(f);
            if (f.endsWith(".call" + label)) {
                break;
            }
        }
        return String.join(" < ", out);
    }

    static void report(String label) {
        List<String> t = new ArrayList<>();
        for (StackTraceElement e : new Throwable().getStackTrace()) {
            String cn = e.getClassName();
            t.add((cn.contains("$$Lambda") ? "<proxy>" : cn) + "." + e.getMethodName());
        }
        System.out.println(label + " throwable: " + walk(label, t));
        List<String> w = StackWalker.getInstance()
                .walk(s -> s.map(f -> name(f.getClassName(), f.getMethodName())).toList());
        System.out.println(label + " walker: " + walk(label, w));
        List<String> h = StackWalker.getInstance(StackWalker.Option.SHOW_HIDDEN_FRAMES)
                .walk(s -> s.map(f -> name(f.getClassName(), f.getMethodName())).toList());
        System.out.println(label + " walker+hidden: " + walk(label, h));
    }

    static void target() {
        report("Ref");
    }

    static void callRunnable() {
        Runnable r = () -> report("Runnable");
        r.run();
    }

    static String callSupplier() {
        Supplier<String> s = () -> {
            report("Supplier");
            return "x";
        };
        return s.get();
    }

    static void callRef() {
        Runnable r = L1W58LambdaProxyFrames::target;
        r.run();
    }

    public static void main(String[] args) {
        callRunnable();
        callSupplier();
        callRef();
        callRunnable();
    }
}
