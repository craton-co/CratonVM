public class Sbr2W9Small2StCost {
    static volatile Object sink;
    static int depth(int n) { if (n == 0) return new Throwable().getStackTrace().length; return depth(n - 1); }
    public static void main(String[] a) {
        int n = 20000;
        for (int rep = 0; rep < 6; rep++) {
            long t = System.nanoTime();
            long x = 0;
            for (int i = 0; i < n; i++) x += depth(8);
            sink = x;
            System.out.println("getStackTrace depth~11 ns/op " + (System.nanoTime() - t) / n);
        }
        long t = System.nanoTime();
        for (int i = 0; i < n; i++) sink = new Throwable().getStackTrace()[0].toString();
        System.out.println("getStackTrace()[0].toString ns/op " + (System.nanoTime() - t) / n);
    }
}
