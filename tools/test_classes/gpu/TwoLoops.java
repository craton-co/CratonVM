public class TwoLoops {
    // Two distinct for-loops in the same method. The analyzer accepts
    // this (every opcode is fine on its own), but the lowering rejects
    // it because the canonical-pattern recognizer requires exactly
    // one backward branch.
    public static void clearTwice(int[] a, int[] b) {
        int n = a.length;
        for (int i = 0; i < n; i++) {
            a[i] = 0;
        }
        int m = b.length;
        for (int j = 0; j < m; j++) {
            b[j] = 0;
        }
    }
}
