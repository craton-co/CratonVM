public class RejectSynchronized {
    public static synchronized int addAll(int[] a) {
        int s = 0;
        for (int v : a) s += v;
        return s;
    }
}
