public class BoundsDeopt2 {
    public static void main(String[] args) {
        int n = 1 << 20;
        int[] a = new int[n];
        int[] b = new int[n];
        int[] out = new int[n / 2];
        for (int i = 0; i < n; i++) { a[i] = i; b[i] = 2 * i; }
        try {
            EligibleVectorAdd.vectorAdd(a, b, out);
            System.out.println("NO-EXCEPTION out[1000]=" + out[1000]
                + " out[last]=" + out[n / 2 - 1]);
        } catch (Throwable e) {
            System.out.println("THROWN " + e.getClass().getName()
                + " out[1000]=" + out[1000] + " out[last]=" + out[n / 2 - 1]);
        }
    }
}
