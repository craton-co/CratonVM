public class FibProbe {
    static int fib(int n) {
        if (n <= 1) return n;
        return fib(n - 1) + fib(n - 2);
    }
    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 44;
        long t0 = System.currentTimeMillis();
        int r = fib(n);
        long ms = System.currentTimeMillis() - t0;
        System.out.println("Fib n=" + n + ": " + ms + " ms  [" + r + "]");
    }
}
