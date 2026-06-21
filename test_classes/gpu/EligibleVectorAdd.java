public class EligibleVectorAdd {
    // Out-parameter style: the GPU host pre-allocates `out`. Methods
    // that allocate internally (see RejectAllocation) are deliberately
    // rejected by the analyzer in the first cut.
    public static void vectorAdd(int[] a, int[] b, int[] out) {
        int n = a.length;
        for (int i = 0; i < n; i++) {
            out[i] = a[i] + b[i];
        }
    }
}
