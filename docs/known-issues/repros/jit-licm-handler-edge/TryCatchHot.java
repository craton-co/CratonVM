public class TryCatchHot {
    static int f(int n, int d) {
        int s = 0;
        for (int i = 0; i < n; i++) {
            try {
                s += 100 / d;
            } catch (ArithmeticException e) {
                s += 1;
            }
        }
        return s;
    }

    public static void main(String[] a) {
        long t = 0;
        int iters = a.length > 0 ? Integer.parseInt(a[0]) : 200000;
        for (int i = 0; i < iters; i++) {
            t += f(8, (i % 7) == 0 ? 0 : 2);
        }
        System.out.println("DONE t=" + t);
    }
}
