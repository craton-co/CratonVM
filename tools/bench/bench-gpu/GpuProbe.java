// GPU offload probe — exercises the three analyzer outcomes plus a correctness
// check. Each kernel is a static method invoked from main(), so the
// interpreter's invokestatic offload hook analyzes (and, under --gpu, attempts
// to offload) it. Results are deterministic so the runner can diff against a
// HotSpot reference.  Usage: java GpuProbe [n]   (default n = 1<<20)
public class GpuProbe {
    // Eligible MAP: writes out[i]; NOT a reduction -> is_reduction:false.
    static void vaddMap(int[] a, int[] b, int[] out) {
        int n = a.length;
        for (int i = 0; i < n; i++) {
            out[i] = a[i] + b[i];
        }
    }
    // Eligible REDUCTION: accumulates into a scalar return, no array store
    // -> is_reduction:true.
    static long dotReduce(int[] a, int[] b) {
        long sum = 0;
        int n = a.length;
        for (int i = 0; i < n; i++) {
            sum += (long) a[i] * b[i];
        }
        return sum;
    }
    // INELIGIBLE: contains an invokestatic (Math.max) -> Rejected(Invoke).
    static int withCall(int[] a) {
        int m = 0;
        for (int i = 0; i < a.length; i++) {
            m = Math.max(m, a[i]);
        }
        return m;
    }
    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : (1 << 20);
        int[] a = new int[n];
        int[] b = new int[n];
        int[] out = new int[n];
        for (int i = 0; i < n; i++) {
            a[i] = i;
            b[i] = (i * 7) % 1000;
        }
        vaddMap(a, b, out);
        long dot = dotReduce(a, b);
        int mx = withCall(a);
        long mapChecksum = 0;
        for (int i = 0; i < n; i++) {
            mapChecksum += out[i];
        }
        System.out.println("n=" + n);
        System.out.println("MAP_CHECKSUM=" + mapChecksum);
        System.out.println("DOT_CHECKSUM=" + dot);
        System.out.println("MAX=" + mx);
        System.out.println("OUT0=" + out[0] + " OUTN=" + out[n - 1]);
    }
}
