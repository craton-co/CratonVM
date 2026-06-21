public class RejectAllocation {
    public static int[] build(int n) {
        int[] out = new int[n];           // newarray — triggers Reject(Allocation)
        for (int i = 0; i < n; i++) {
            out[i] = i * i;
        }
        return out;
    }
}
