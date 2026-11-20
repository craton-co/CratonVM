public class RejectInvoke {
    public static int outer(int[] a) {
        return inner(a);                  // invokestatic — triggers Reject(Invoke)
    }

    private static int inner(int[] a) {
        int s = 0;
        for (int v : a) s += v;
        return s;
    }
}
