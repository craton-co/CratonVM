// Minimal repro for WildFly bug #2's core: an object reference held live in a
// frame across an allocation (→ GC) is reclaimed and its slot reused. Run under
// CRATONVM_DBG_GC_STRESS=65536 (force young GC every 64KB) — JIT not required.
//   ClassCastException / wrong .length ⇒ the held reference was lost across GC.
import java.lang.reflect.Field;

public class MinRepro {
    int a, b, c2, d; long e; String s; Object[] arr;

    // Allocate enough to trigger a young GC while the caller holds a reference.
    static void churn() {
        Object sink = null;
        for (int i = 0; i < 64; i++) sink = new byte[512];
        if (sink == null) System.out.println("x");
    }

    // Case A: native-returned array held across GC.
    static long caseNativeArray(Class<?> k, int iters) {
        long acc = 0;
        for (int i = 0; i < iters; i++) {
            Field[] f = k.getDeclaredFields(); // native return
            churn();                           // GC here, f must stay live
            acc += f.length;                   // use after GC
            for (Field x : f) acc += x.getName().length(); // deref elements too
        }
        return acc;
    }

    // Case B: plain Java array held across GC (control — should never fail).
    static long casePlainArray(int iters) {
        long acc = 0;
        for (int i = 0; i < iters; i++) {
            String[] arr = new String[]{"aa", "bb", "cc", "dd"};
            churn();
            acc += arr.length;
            for (String x : arr) acc += x.length();
        }
        return acc;
    }

    public static void main(String[] args) {
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 50000;
        long b = casePlainArray(iters);
        System.out.println("B(plain) ok acc=" + b);
        long a = caseNativeArray(MinRepro.class, iters);
        System.out.println("A(native) ok acc=" + a);
        System.out.println("done");
    }
}
