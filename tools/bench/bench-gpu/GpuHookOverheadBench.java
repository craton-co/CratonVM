// Per-invokestatic overhead of CratonVM's GPU offload hook.
//
// Background: the interpreter's invokestatic slow path carries a GPU-offload
// hook. As of the fix landing today, any call site whose target is
// offload-eligible (or already GPU-handled) is deliberately kept OUT of the
// per-call-site invoke cache, so it re-enters the hook -- and gets
// re-analyzed / re-dispatched -- on EVERY call, forever. Ineligible targets
// are the opposite: once the analyzer blacklists them, their call site IS
// cached like a normal invokestatic, and the hook is never consulted again.
//
// This bench isolates the resulting per-call cost with three scenarios, each
// just a static method invoked many times (fewer for the offload case) in a
// tight timed loop, best-of-`reps`:
//
//   1) addSmall(int,int)  ["tinyIneligible"]
//      Scalar-only, no arrays, no loop -> analyzer-INELIGIBLE for offload
//      (nothing GPU-shaped to find). After the analyzer blacklists the call
//      site (during warmup), every subsequent call is a plain CACHED
//      invokestatic. This is the baseline. Prints base_ns_per_call.
//
//   2) tinyEligibleSmall(int[],int[]) called with length-64 arrays
//      Canonical single-counted-loop map kernel (vectorAdd-like:
//      out[i] = a[i] + a[i]) -> analyzer-ELIGIBLE. 64 elements is far below
//      the default --gpu-min-work=4096, so under --gpu every call takes the
//      hook path, gets re-analyzed / re-looked-up (FallThroughKeepHooked,
//      uncached), decides the work is too small, and falls through to the
//      same trivial CPU loop. Prints small_ns_per_call.
//
//      small_ns_per_call - base_ns_per_call approximates the hook's own
//      per-call overhead (hook entry + re-analysis + threshold check +
//      fall-through dispatch). It is not *exactly* the hook cost in
//      isolation: it also includes the 64-element CPU loop body, which is
//      why that body is kept deliberately trivial (a single add, no second
//      input array) -- mentally subtract "a few ns * 64" for the loop
//      itself when reading the delta, the residual after that is the hook.
//      Under a non---gpu build (or with --gpu-min-work above 64) the hook
//      either isn't active or never disagrees with caching for this site,
//      so run WITHOUT --gpu as a sanity check: small_ns_per_call should
//      then track base_ns_per_call closely (no hook detour either way).
//
//   3) tinyEligibleSmall(int[],int[]) called with length-8192 arrays
//      Same kernel, same eligibility, but 8192 elements is ABOVE
//      --gpu-min-work=4096, so under --gpu every single call offloads:
//      H2D copy + kernel launch + D2H copy, EVERY TIME (never cached,
//      never batched across calls). Uses a much smaller call count
//      (n / 1000, floor 1000) since each call costs microseconds-to-
//      milliseconds instead of tens of nanoseconds. Prints big_ns_per_call.
//
//      big_ns_per_call is the number that shows transparent per-call
//      offload of small-but-above-threshold arrays is a NET LOSS vs. just
//      running the 8192-element loop on CPU -- useful for tuning
//      --gpu-min-work upward, or for justifying a future "stay offloaded /
//      batch across calls" optimization for hot call sites.
//
// Interpretation summary (all printed as key=value lines):
//   base_ns_per_call              steady-state cached invokestatic, no hook
//   small_ns_per_call             hook path, offload rejected every call
//   big_ns_per_call                hook path, offload accepted every call
//   hook_overhead_ns_per_call     small_ns_per_call - base_ns_per_call
//   offload_overhead_ns_per_call  big_ns_per_call   - small_ns_per_call
//
// If a future change adds a lock, an allocation, or any other per-call cost
// to the hot hook path, hook_overhead_ns_per_call is the number that should
// move -- treat a regression there as the canary. base_ns_per_call should
// stay essentially flat across --gpu / non---gpu builds, since a blacklisted
// site never touches the hook either way.
//
// Usage: java GpuHookOverheadBench [n] [reps]
//   n     total calls for scenarios 1 and 2 (default 5_000_000)
//   reps  best-of-reps repetitions per scenario (default 5)
//   scenario 3's call count is derived: max(1000, n / 1000)
public class GpuHookOverheadBench {

    // Scenario 1: scalar-only, no arrays/loop -> analyzer-INELIGIBLE.
    // Blacklisted after warmup -> plain cached invokestatic thereafter.
    static int addSmall(int a, int b) {
        return a + b;
    }

    // Scenarios 2 & 3: canonical single-counted-loop map kernel, primitive
    // int[] params, array store indexed by the loop var, no calls/allocation
    // /fields -> analyzer-ELIGIBLE (same shape as GpuWarm.heavy / GpuProbe.
    // vaddMap, just a one-line body so the loop's own cost stays small).
    static void tinyEligibleSmall(int[] a, int[] out) {
        int n = a.length;
        for (int i = 0; i < n; i++) {
            out[i] = a[i] + a[i];
        }
    }

    public static void main(String[] args) {
        int n    = args.length > 0 ? Integer.parseInt(args[0]) : 5_000_000;
        int reps = args.length > 1 ? Integer.parseInt(args[1]) : 5;
        int bigN = Math.max(1000, n / 1000);

        System.out.println("n=" + n);
        System.out.println("reps=" + reps);
        System.out.println("big_n=" + bigN);

        // ---- Scenario 1: baseline cached invokestatic (ineligible) ----
        int warmup1 = Math.min(n, 50_000);
        int acc = 0;
        for (int i = 0; i < warmup1; i++) {
            acc = addSmall(acc, i);            // warmup: let the analyzer
        }                                       // blacklist + cache this site
        long best1 = Long.MAX_VALUE;
        int checksum1 = 0;
        for (int r = 0; r < reps; r++) {
            int s = 0;
            long t0 = System.nanoTime();
            for (int i = 0; i < n; i++) {
                s = addSmall(s, i);
            }
            long elapsed = System.nanoTime() - t0;
            if (elapsed < best1) best1 = elapsed;
            checksum1 = s;                      // deterministic: same n each rep
        }
        double baseNsPerCall = (double) best1 / n;
        System.out.println("base_best_ns=" + best1);
        System.out.println("base_ns_per_call=" + fmt(baseNsPerCall));
        System.out.println("base_checksum=" + checksum1);
        System.out.println("acc_unused=" + acc); // anti-DCE for the warmup loop

        // ---- Scenario 2: eligible kernel, below --gpu-min-work=4096 ----
        int smallLen = 64;
        int[] smallA = new int[smallLen];
        int[] smallOut = new int[smallLen];
        for (int i = 0; i < smallLen; i++) {
            smallA[i] = i * 1103515245 + 12345;   // LCG-style, deterministic
        }
        int warmup2 = Math.min(n, 50_000);
        for (int i = 0; i < warmup2; i++) {
            tinyEligibleSmall(smallA, smallOut);   // warmup: prime any
        }                                            // one-time analysis/PTX cost
        long best2 = Long.MAX_VALUE;
        for (int r = 0; r < reps; r++) {
            long t0 = System.nanoTime();
            for (int i = 0; i < n; i++) {
                tinyEligibleSmall(smallA, smallOut);
            }
            long elapsed = System.nanoTime() - t0;
            if (elapsed < best2) best2 = elapsed;
        }
        double smallNsPerCall = (double) best2 / n;
        long smallSample = (long) smallOut[0] + smallOut[smallLen - 1];
        System.out.println("small_best_ns=" + best2);
        System.out.println("small_ns_per_call=" + fmt(smallNsPerCall));
        System.out.println("SMALL_SAMPLE=" + smallSample);

        // ---- Scenario 3: eligible kernel, above --gpu-min-work=4096 ----
        int bigLen = 8192;
        int[] bigA = new int[bigLen];
        int[] bigOut = new int[bigLen];
        for (int i = 0; i < bigLen; i++) {
            bigA[i] = i * 1103515245 + 12345;
        }
        int warmup3 = Math.min(bigN, 5);
        for (int i = 0; i < warmup3; i++) {
            tinyEligibleSmall(bigA, bigOut);      // warmup: first-call H2D/PTX
        }                                           // context & module setup
        long best3 = Long.MAX_VALUE;
        for (int r = 0; r < reps; r++) {
            long t0 = System.nanoTime();
            for (int i = 0; i < bigN; i++) {
                tinyEligibleSmall(bigA, bigOut);
            }
            long elapsed = System.nanoTime() - t0;
            if (elapsed < best3) best3 = elapsed;
        }
        double bigNsPerCall = (double) best3 / bigN;
        long bigSample = (long) bigOut[0] + bigOut[bigLen - 1];
        System.out.println("big_best_ns=" + best3);
        System.out.println("big_ns_per_call=" + fmt(bigNsPerCall));
        System.out.println("BIG_SAMPLE=" + bigSample);

        // ---- Derived deltas ----
        System.out.println("hook_overhead_ns_per_call=" + fmt(smallNsPerCall - baseNsPerCall));
        System.out.println("offload_overhead_ns_per_call=" + fmt(bigNsPerCall - smallNsPerCall));
    }

    // Fixed-point formatting (never scientific notation) so every value is
    // trivially parseable as a plain decimal by a shell/regex consumer.
    private static String fmt(double v) {
        return String.format("%.3f", v);
    }
}
