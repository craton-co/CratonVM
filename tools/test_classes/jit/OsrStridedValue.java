// `in[i] = i` is correct; `in[i] = i ^ round` and `in[i] = i + round` are
// not. The difference is `round` -- the OUTER induction variable, live
// across the `scale(in, out)` call -- read from inside the inner loop.
//
// Three arrays, three stored expressions, one run.
public class OsrStridedValue {
    static void scale(int[] in, int[] out) {
        for (int i = 0; i < out.length; i++) out[i] = in[i] * 3 - 7;
    }
    static long mix(long h, long v) { return h * 1000003L + v; }
    static long sum(int[] v) { long h = 0; for (int x : v) h = mix(h, x); return h; }

    static void body(int n) {
        int[] in = new int[n];
        int[] out = new int[n];
        int[] justRound = new int[n];
        int[] iPlusRound = new int[n];
        for (int i = 0; i < n; i++) in[i] = i % 1013;
        long h = 0;
        for (int round = 0; round < 12; round++) {
            scale(in, out);
            h = mix(h, sum(out));
            for (int i = round; i < n; i += 1024) {
                justRound[i] = round;
                iPlusRound[i] = i + round;
                in[i] = i ^ round;
            }
        }
        StringBuilder a = new StringBuilder();
        StringBuilder b = new StringBuilder();
        StringBuilder c = new StringBuilder();
        for (int k = 0; k < 6; k++) {
            int idx = 11 + k * 1024;
            a.append(justRound[idx]).append(' ');
            b.append(iPlusRound[idx]).append(' ');
            c.append(in[idx]).append(' ');
        }
        // Last round is 11, so `round` must read 11 at every address.
        System.out.println("round   " + a);
        System.out.println("i+round " + b);
        System.out.println("i^round " + c);
    }

    public static void main(String[] args) {
        body(args.length > 0 ? Integer.parseInt(args[0]) : 65536);
    }
}
