public class SieveBench {
    static int sieve(boolean[] composite, int limit) {
        for (int i = 0; i <= limit; i++) composite[i] = false;
        int count = 0;
        for (int i = 2; i <= limit; i++) {
            if (!composite[i]) {
                count++;
                for (int j = i + i; j <= limit; j += i) {
                    composite[j] = true;
                }
            }
        }
        return count;
    }
    public static void main(String[] args) {
        int limit = 100000;
        boolean[] composite = new boolean[limit + 1];
        // Warmup
        for (int rep = 0; rep < 100; rep++) sieve(composite, limit);
        // Benchmark
        long t0 = System.currentTimeMillis();
        int r = 0;
        for (int rep = 0; rep < 500; rep++) r = sieve(composite, limit);
        long elapsed = System.currentTimeMillis() - t0;
        System.out.println("Sieve: " + elapsed + " ms [" + r + "]");
    }
}
