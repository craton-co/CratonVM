public class LoopTest18 {
    static int dummy = 0;
    public static int sum2(int[] a) {
        int n = a.length;
        dummy = 1;
        return n;
    }
    public static void main(String[] args) throws Exception {
        int[] a = new int[100];
        System.out.println("step1");
        // Just two calls to trigger JIT then use it
        // We need many calls to hit threshold of 2000
        int r0 = 0;
        for (int k = 0; k < 2005; k++) {
            r0 = sum2(a);
            if (k == 1999 || k == 2000 || k == 2001) System.out.println("k=" + k + " r0=" + r0);
        }
        System.out.println("FINAL r0=" + r0);
        System.out.println("DONE");
    }
}
