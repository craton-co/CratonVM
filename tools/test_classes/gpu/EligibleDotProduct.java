public class EligibleDotProduct {
    // Scalar return — no allocation; reduces two int[] to a long sum.
    public static long dot(int[] a, int[] b) {
        long sum = 0L;
        int n = a.length;
        for (int i = 0; i < n; i++) {
            sum += (long) a[i] * (long) b[i];
        }
        return sum;
    }
}
