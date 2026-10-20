// Lane L5 probe: reading a throwable's stack trace must not cost O(live throwables).
//
// Until round i1 wave 2, every Throwable trace read on CratonVM
// (getStackTrace, printStackTrace, StackTraceElement.initStackTraceElements,
// getStackTraceDepth) looked the retained trace up by IDENTITY HASH, which
// scans every retained trace in every shard (minting a hash on each) --
// see NativeExceptionAccess::get_throwable_stack_trace. It is now one
// address-keyed probe.
//
// stdout (deterministic; HotSpot 25 prints exactly this):
//   len 7
//   top make
//   sum 2000
//   retained 20000
// stderr: "ns-per-lookup N" -- the thing to MEASURE. Compare the old and new
// CratonVM binaries (interleaved, medians); it should no longer scale with the
// 20000 retained throwables (try 2000 vs 20000 by editing RETAINED).
public class ThrowableTraceLookup {
    static final int RETAINED = 20000;
    static final int LOOKUPS = 2000;

    static Throwable make(int depth) {
        return depth == 0 ? new RuntimeException("x") : make(depth - 1);
    }

    public static void main(String[] args) {
        java.util.ArrayList<Throwable> keep = new java.util.ArrayList<>();
        for (int i = 0; i < RETAINED; i++) {
            keep.add(new IllegalStateException("k" + i));
        }
        Throwable t = make(5);
        StackTraceElement[] st = t.getStackTrace();
        System.out.println("len " + st.length);
        System.out.println("top " + st[0].getMethodName());

        long t0 = System.nanoTime();
        long sum = 0;
        for (int i = 0; i < LOOKUPS; i++) {
            // Distinct throwables, so each read materialises its trace once.
            Throwable u = keep.get((i * 7) % RETAINED);
            sum += u.getStackTrace().length;
        }
        long dt = System.nanoTime() - t0;
        System.out.println("sum " + sum);
        System.out.println("retained " + keep.size());
        System.err.println("ns-per-lookup " + (dt / LOOKUPS));
    }
}
