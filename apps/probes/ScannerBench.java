import java.util.*;

/** L3 — what retiring `java.util.Scanner`'s 43 shadows COSTS.
 *
 *  `apps/probes/ScannerShadowSweep` says the retirement is free in correctness:
 *  strict drops all 43 and is 0-diff on all 94 rows, where the shadowed path is
 *  wrong on 25 of them. The 25 are still open in COMPATIBLE mode, and the only
 *  thing standing between them and the same fix is whether the Rust tokenizer is
 *  earning its keep on throughput -- which is the axis a correctness probe
 *  cannot see, and which is exactly why `java.util.Random`'s equivalent gate
 *  defaults off (8x-11x there).
 *
 *  Three shapes, because a Scanner is used three ways: token-at-a-time, typed
 *  reads, and line-at-a-time. `nextLine` matters most -- it is the one built on
 *  `findPatternInBuffer`, and the one a log reader runs in a loop.
 *
 *  ns/op, not a verdict. Run it with the gate off and on, interleaved.
 */
public class ScannerBench {
    static final int WARMUP = 200;
    static final int ITERS = 2000;

    static String tokens(int n) {
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < n; i++) sb.append("tok").append(i).append(' ');
        return sb.toString();
    }

    static String ints(int n) {
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < n; i++) sb.append(i).append(' ');
        return sb.toString();
    }

    static String lines(int n) {
        StringBuilder sb = new StringBuilder();
        for (int i = 0; i < n; i++) sb.append("line ").append(i).append('\n');
        return sb.toString();
    }

    static long bench(String src, int mode, int iters) {
        long acc = 0;
        for (int i = 0; i < iters; i++) {
            Scanner s = new Scanner(src).useLocale(Locale.US);
            switch (mode) {
                case 0 -> { while (s.hasNext()) acc += s.next().length(); }
                case 1 -> { while (s.hasNextInt()) acc += s.nextInt(); }
                default -> { while (s.hasNextLine()) acc += s.nextLine().length(); }
            }
            s.close();
        }
        return acc;
    }

    static long timed(String src, int mode) {
        bench(src, mode, WARMUP);
        long t0 = System.nanoTime();
        long acc = bench(src, mode, ITERS);
        long dt = System.nanoTime() - t0;
        if (acc == Long.MIN_VALUE) System.out.print("");
        return dt / ITERS;
    }

    public static void main(String[] args) {
        String t = tokens(40);
        String n = ints(40);
        String l = lines(40);
        System.out.println("next    (40 tokens) ns/scan " + timed(t, 0));
        System.out.println("nextInt (40 ints)   ns/scan " + timed(n, 1));
        System.out.println("nextLine(40 lines)  ns/scan " + timed(l, 2));
        System.out.println("DONE ScannerBench");
    }
}
