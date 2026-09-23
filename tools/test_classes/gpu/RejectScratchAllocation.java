public class RejectScratchAllocation {
    // Allocates a scratch array sized from a parameter — the exact
    // `iload_0; newarray int` shape `AdmissionHint::ALLOW_ALLOCATION`
    // used to loosen — but returns a SCALAR.
    //
    // That matters: `RejectAllocation.build` returns `[I`, so since
    // 2026-09-21 it is rejected on its return type before the body is
    // ever scanned, and can no longer pin the allocation band. This
    // fixture isolates allocation and nothing else.
    public static int sumSquares(int n) {
        int[] t = new int[n];
        for (int i = 0; i < n; i++) {
            t[i] = i * i;
        }
        int s = 0;
        for (int i = 0; i < n; i++) {
            s += t[i];
        }
        return s;
    }
}
