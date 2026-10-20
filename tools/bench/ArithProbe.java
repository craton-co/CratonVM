public class ArithProbe {
    static long benchArithmetic(long iterations) {
        long sum = 0;
        for (long i = 0; i < iterations; i++) {
            sum += i * 3 - i / 2 + i % 7;
        }
        return sum;
    }
    public static void main(String[] args) {
        long n = args.length > 0 ? Long.parseLong(args[0]) : 2_000_000_000L;
        long t0 = System.currentTimeMillis();
        long r = benchArithmetic(n);
        long ms = System.currentTimeMillis() - t0;
        System.out.println("Arith n=" + n + ": " + ms + " ms  [" + r + "]");
    }
}
