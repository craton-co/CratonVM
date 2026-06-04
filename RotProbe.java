// Verifies the JIT Integer/Long rotateLeft/rotateRight intrinsics are
// byte-identical to the JDK across edge distances (0, >=width, negative) and
// negative values (high bit set). Folds everything into a checksum so the JIT
// can't dead-code it; loops enough to cross the compile threshold.
public class RotProbe {
    static int  rliL(int v, int d)  { return Integer.rotateLeft(v, d); }
    static int  rliR(int v, int d)  { return Integer.rotateRight(v, d); }
    static long rllL(long v, int d) { return Long.rotateLeft(v, d); }
    static long rllR(long v, int d) { return Long.rotateRight(v, d); }

    public static void main(String[] args) {
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 3_000_000;
        int[]  iv = { 0, 1, -1, 0x80000000, 0x12345678, -2023406815, Integer.MAX_VALUE, Integer.MIN_VALUE };
        long[] lv = { 0L, 1L, -1L, 0x8000000000000000L, 0x0123456789ABCDEFL, Long.MAX_VALUE, Long.MIN_VALUE };
        int[]  dist = { 0, 1, 7, 8, 16, 31, 32, 33, 63, 64, 65, -1, -7, -33 };

        long checksum = 0;
        for (int n = 0; n < iters; n++) {
            int  i = iv[n & 7];
            long l = lv[n % 7];
            int  d = dist[n % dist.length];
            checksum += rliL(i, d);
            checksum += rliR(i, d);
            checksum += rllL(l, d);
            checksum += rllR(l, d);
        }
        System.out.println("RotProbe: iters=" + iters + " checksum=" + checksum);
        System.out.flush();
    }
}
