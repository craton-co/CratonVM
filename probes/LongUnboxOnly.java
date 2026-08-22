public final class LongUnboxOnly {
    static long sink;
    static final Long BOXED = Long.valueOf(123456789012L);

    // Whole-method compile door: a small method called many times.
    static long unboxLoop(int n) {
        long a = 0;
        for (int i = 0; i < n; i++) { a += BOXED.longValue(); }
        return a;
    }

    public static void main(String[] args) {
        for (int r = 0; r < 200; r++) { sink += unboxLoop(20000); }
        // OSR door: one long-running loop inside main.
        long a = 0;
        for (int i = 0; i < 4000000; i++) { a += BOXED.longValue(); }
        sink += a;
        System.out.println("sink=" + (sink != 0));
    }
}
