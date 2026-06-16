// Pinpoint: is a NATIVE-allocated array (java.lang.reflect.Array.newInstance)
// GC-safe when held live across an allocation, vs a bytecode-allocated array?
// Run under CRATONVM_DBG_GC_STRESS=65536 (JIT not required).
import java.lang.reflect.Array;

public class ArrRepro {
    static void churn() {
        Object sink = null;
        for (int i = 0; i < 64; i++) sink = new byte[512];
        if (sink == null) System.out.println("x");
    }

    // Native-allocated array (Array.newInstance is a native), held across GC.
    static long caseNativeAlloc(int iters) {
        long acc = 0;
        for (int i = 0; i < iters; i++) {
            String[] a = (String[]) Array.newInstance(String.class, 4);
            a[0] = "p"; a[1] = "q"; a[2] = "r"; a[3] = "s";
            churn();                 // GC while `a` is live
            acc += a.length;         // use after GC
            for (String x : a) acc += (x == null ? 0 : x.length());
        }
        return acc;
    }

    // Bytecode-allocated array (control).
    static long caseBytecodeAlloc(int iters) {
        long acc = 0;
        for (int i = 0; i < iters; i++) {
            String[] a = new String[4];
            a[0] = "p"; a[1] = "q"; a[2] = "r"; a[3] = "s";
            churn();
            acc += a.length;
            for (String x : a) acc += (x == null ? 0 : x.length());
        }
        return acc;
    }

    public static void main(String[] args) {
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 50000;
        System.out.println("bytecode acc=" + caseBytecodeAlloc(iters));
        System.out.println("native   acc=" + caseNativeAlloc(iters));
        System.out.println("done");
    }
}
