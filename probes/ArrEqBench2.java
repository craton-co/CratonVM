import java.util.Arrays;
import java.util.Objects;

/**
 * Rungs that separate "Arrays.equals is special" from "a static call in this
 * loop is not inlined". Every rung walks the same two String[1000]s whose
 * elements are reference-identical, so each comparison short-circuits.
 *
 *   arrayseq  java.util.Arrays.equals(Object[], Object[])   — what Spring calls
 *   objcall   the same loop, calling java.util.Objects.equals per element
 *   mycall    the same loop, calling a LOCAL static with the identical body
 *   myfinal   the same, but the local static is in this class and private
 *   inline    the body written out by hand — no call at all
 *   refeq     bare `==`
 */
public class ArrEqBench2 {

    static int sink;

    private static boolean myEquals(Object a, Object b) {
        return (a == b) || (a != null && a.equals(b));
    }

    static boolean viaObjectsCall(Object[] a, Object[] b) {
        int n = a.length;
        if (b.length != n) { return false; }
        for (int i = 0; i < n; i++) {
            if (!Objects.equals(a[i], b[i])) { return false; }
        }
        return true;
    }

    static boolean viaMyCall(Object[] a, Object[] b) {
        int n = a.length;
        if (b.length != n) { return false; }
        for (int i = 0; i < n; i++) {
            if (!myEquals(a[i], b[i])) { return false; }
        }
        return true;
    }

    static boolean viaInline(Object[] a, Object[] b) {
        int n = a.length;
        if (b.length != n) { return false; }
        for (int i = 0; i < n; i++) {
            Object x = a[i];
            Object y = b[i];
            if (!(x == y || (x != null && x.equals(y)))) { return false; }
        }
        return true;
    }

    static boolean viaRefEq(Object[] a, Object[] b) {
        int n = a.length;
        if (b.length != n) { return false; }
        for (int i = 0; i < n; i++) {
            if (a[i] != b[i]) { return false; }
        }
        return true;
    }

    public static void main(String[] args) {
        String rung = args.length > 0 ? args[0] : "arrayseq";
        int outer = args.length > 1 ? Integer.parseInt(args[1]) : 20_000;
        int width = args.length > 2 ? Integer.parseInt(args[2]) : 1000;

        String[] a = new String[width];
        for (int i = 0; i < width; i++) { a[i] = "test-7-property-" + i; }
        String[] b = a.clone();

        long t0 = System.nanoTime();
        long ok = 0;
        for (int k = 0; k < outer; k++) {
            boolean r;
            switch (rung) {
                case "arrayseq" -> r = Arrays.equals(a, b);
                case "objcall"  -> r = viaObjectsCall(a, b);
                case "mycall"   -> r = viaMyCall(a, b);
                case "inline"   -> r = viaInline(a, b);
                case "refeq"    -> r = viaRefEq(a, b);
                default -> throw new IllegalArgumentException(rung);
            }
            if (r) { ok++; }
        }
        long ms = (System.nanoTime() - t0) / 1_000_000L;
        long elems = (long) outer * width;
        System.out.printf("ARREQ2 rung=%-9s elems=%d ms=%d ns/elem=%.1f ok=%d%n",
                rung, elems, ms, (ms * 1e6) / elems, ok);
    }
}
