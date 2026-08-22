import java.util.Arrays;

/**
 * The hot loop under ConfigurationPropertySourcesTests: Spring's property-source
 * cache decides whether to rebuild with
 * `Arrays.equals(String[1000] lastUpdated, String[1000] current)`, where every
 * element pair is REFERENCE-IDENTICAL, so each comparison short-circuits in
 * `Objects.equals`. Measured 100,008,000 such element comparisons per run of
 * `environmentPropertyAccessWhenImmutableShouldBePerformant`, identical on both
 * VMs.
 *
 * Rungs, so the cost can be attributed rather than just observed:
 *   arrayseq   java.util.Arrays.equals(Object[], Object[])   — what Spring calls
 *   objeq      the same loop hand-written with Objects.equals
 *   refeq      the same loop with a bare `==`                — no Objects.equals
 *   load       aaload both sides, no comparison at all       — the read cost
 */
public class ArrEqBench {

    static int sink;

    static boolean handObjectsEquals(Object[] a, Object[] b) {
        if (a == b) { return true; }
        if (a == null || b == null) { return false; }
        int n = a.length;
        if (b.length != n) { return false; }
        for (int i = 0; i < n; i++) {
            Object x = a[i];
            Object y = b[i];
            if (!(x == y || (x != null && x.equals(y)))) { return false; }
        }
        return true;
    }

    static boolean handRefEquals(Object[] a, Object[] b) {
        int n = a.length;
        if (b.length != n) { return false; }
        for (int i = 0; i < n; i++) {
            if (a[i] != b[i]) { return false; }
        }
        return true;
    }

    static int loadOnly(Object[] a, Object[] b) {
        int n = a.length;
        int acc = 0;
        for (int i = 0; i < n; i++) {
            Object x = a[i];
            Object y = b[i];
            if (x != null) { acc++; }
            if (y != null) { acc++; }
        }
        return acc;
    }

    public static void main(String[] args) {
        String rung = args.length > 0 ? args[0] : "arrayseq";
        int outer = args.length > 1 ? Integer.parseInt(args[1]) : 100_000;
        int width = args.length > 2 ? Integer.parseInt(args[2]) : 1000;

        String[] a = new String[width];
        for (int i = 0; i < width; i++) { a[i] = "test-7-property-" + i; }
        String[] b = a.clone();   // distinct array, reference-identical elements

        long t0 = System.nanoTime();
        long ok = 0;
        for (int k = 0; k < outer; k++) {
            switch (rung) {
                case "arrayseq" -> { if (Arrays.equals(a, b)) { ok++; } }
                case "objeq"    -> { if (handObjectsEquals(a, b)) { ok++; } }
                case "refeq"    -> { if (handRefEquals(a, b)) { ok++; } }
                case "load"     -> { sink += loadOnly(a, b); ok++; }
                default -> throw new IllegalArgumentException(rung);
            }
        }
        long ms = (System.nanoTime() - t0) / 1_000_000L;
        long elems = (long) outer * width;
        double nsPer = (ms * 1e6) / elems;
        System.out.printf("ARREQ rung=%s outer=%d width=%d elems=%d ms=%d ns/elem=%.1f ok=%d sink=%d%n",
                rung, outer, width, elems, ms, nsPer, ok, sink);
    }
}
