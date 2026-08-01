import java.util.ArrayList;
import java.util.List;
import java.util.Set;
import java.util.TreeSet;

/**
 * Reproduces the shape behind
 * `docs/known-issues/springboot/springboot-basicerrorcontroller-checkcast-abort-20260731.md`
 * without Spring, Tomcat or Jetty.
 *
 * The failing production shape is Spring's
 * `JdkClientHttpRequest.DISALLOWED_HEADERS` — a `static final`
 * `TreeSet<>(String.CASE_INSENSITIVE_ORDER)` populated once at class-init and
 * then read (`contains`) on every outbound request, for the lifetime of the
 * process. Jetty's `HttpCookie.from` hits the same shape from a different call
 * site.
 *
 * Why that shape and not `ROverlaySystemGcStress`'s: CratonVM keeps the
 * TreeSet's backing array in a process-global Rust side table, so the only
 * thing that keeps it alive is the collector's overlay rooting. A set that is
 * built AND read inside one young cycle never exercises that — it has to be
 * old enough to have been promoted, and the reads have to keep coming after
 * many later collections. `contains` on a `CASE_INSENSITIVE_ORDER` set also
 * routes through `native_ts_contains` -> `ts_binary_search` -> `tree_compare`
 * -> `comparator_compare` -> the real `String$CaseInsensitiveComparator
 * .compare`, which is where a reclaimed backing array surfaces as
 * `checkcast: not an object reference (got Int(0))`.
 *
 * WHAT A GREEN RUN DOES NOT PROVE (measured 2026-08-01 — do not skip this).
 * This probe was written to reproduce the 2026-07-31 regression in
 * `docs/known-issues/springboot/springboot-basicerrorcontroller-checkcast-abort-20260731.md`.
 * It does not. Against `9fcd1b63f` — the exact commit that regression was
 * reported on — it passes 4/4, the same as against fixed dev. So it CANNOT
 * express that defect, and a PASS here is not evidence the defect is absent.
 * Neither can `regression-suite/src/ROverlaySystemGcStress.java`, checked the
 * same way and equally green on `9fcd1b63f`. The only vehicle that has ever
 * reproduced it is the real Spring Boot class.
 *
 * What it does cover: the array-mode TreeSet read path under a moving young
 * collector with same-cycle major collections — the mechanism — so a gross
 * regression in overlay rooting would fail it. A cheap smoke test, not an
 * acceptance gate.
 *
 * Run (pin `--Xmx` — the default heap derives from system RAM and decides how
 * many collections run at all, so an unpinned run is not a controlled
 * experiment). Pass `0` as the 4th argument to drive collections by allocation
 * pressure: `System.gc()` diverts to the NON-MOVING young sweep, so the
 * explicit-collection arm never reaches the moving path at all. Confirm with
 * `CRATONVM_GC_STATS=1` that `decision histogram: moving=N` is nonzero before
 * trusting any result:
 *
 *   CRATONVM_GC_STATS=1 cratonvm --java-home <jdk> --Xmx 192m -cp probes \
 *       OverlayCaseInsensitiveTreeSetProbe 30 60000 20000 0
 *
 * That configuration measured `minor=16 major=12`, all 16 moving.
 *
 * Exit code 0 and a `PASS` line means every read saw an intact backing array.
 * Any other outcome — an assertion, a `checkcast: not an object reference`
 * abort, or a SIGSEGV — is the defect.
 */
public class OverlayCaseInsensitiveTreeSetProbe {

    /**
     * Same construction as `JdkClientHttpRequest.DISALLOWED_HEADERS`: built in
     * a static initializer so it is promoted long before the reads that matter.
     */
    private static final Set<String> DISALLOWED_HEADERS = disallowedHeaders();

    private static Set<String> disallowedHeaders() {
        TreeSet<String> headers = new TreeSet<>(String.CASE_INSENSITIVE_ORDER);
        headers.add("connection");
        headers.add("content-length");
        headers.add("expect");
        headers.add("host");
        headers.add("upgrade");
        return headers;
    }

    /** Probed both ways round: a hit and a miss take different search paths. */
    private static final String[] PROBED = {
        "Connection", "CONTENT-LENGTH", "Expect", "host", "Upgrade",
        "Accept", "X-Trace-Id", "authorization", "zzz-last", "aaa-first",
    };

    private static int checks;

    private static void check(boolean condition, String message) {
        checks++;
        if (!condition) {
            throw new AssertionError(message);
        }
    }

    /**
     * Exactly the assertion the production call site relies on: five names are
     * in the set case-insensitively, everything else is not. A reclaimed
     * backing array reads back as `Int(0)`, so the comparator aborts before
     * this ever returns false — but assert anyway, since a pruned-but-present
     * overlay entry loses the contents silently instead.
     */
    private static void readEveryHeader() {
        for (String name : PROBED) {
            boolean expected = name.equalsIgnoreCase("connection")
                    || name.equalsIgnoreCase("content-length")
                    || name.equalsIgnoreCase("expect")
                    || name.equalsIgnoreCase("host")
                    || name.equalsIgnoreCase("upgrade");
            check(DISALLOWED_HEADERS.contains(name) == expected,
                    "contains(" + name + ") != " + expected
                            + " (set size " + DISALLOWED_HEADERS.size() + ")");
        }
        check(DISALLOWED_HEADERS.size() == 5,
                "set size " + DISALLOWED_HEADERS.size() + " != 5");
    }

    public static void main(String[] args) {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 400;
        // Allocation churn per round, to drive moving-young collections the way
        // a Tomcat/Jetty boot cycle does. Retained ballast ages the set toward
        // promotion instead of letting every cycle be a cheap one.
        int churnPerRound = args.length > 1 ? Integer.parseInt(args[1]) : 20000;
        int ballastBlocks = args.length > 2 ? Integer.parseInt(args[2]) : 2048;
        // `System.gc()` diverts to the NON-MOVING young sweep and sets
        // `major_gc_requested()`, which is what makes the VM SKIP the
        // unconditional overlay root scan. Both arms matter, so interleave
        // them rather than picking one.
        boolean explicitGc = args.length <= 3 || !"0".equals(args[3]);

        List<byte[]> ballast = new ArrayList<>();
        for (int i = 0; i < ballastBlocks; i++) {
            byte[] block = new byte[4096];
            block[0] = (byte) i;
            ballast.add(block);
        }

        long sink = ballast.size();
        for (int round = 0; round < rounds; round++) {
            for (int i = 0; i < churnPerRound; i++) {
                sink += new StringBuilder("churn").append(round).append('.').append(i)
                        .toString().length();
                // Read INSIDE the churn loop, not only between rounds: the
                // production call site reads on every request, i.e. constantly
                // interleaved with allocation, and a read that only ever
                // happens at a quiet point can miss a window entirely.
                if ((i & 0x3ff) == 0) {
                    readEveryHeader();
                }
            }
            if (explicitGc && (round & 1) == 0) {
                System.gc();
            }
            readEveryHeader();
        }

        System.out.println("CK rounds=" + rounds + " ballast=" + ballast.size()
                + " sink=" + (sink > 0));
        System.out.println("PASS OverlayCaseInsensitiveTreeSetProbe (" + checks + " checks)");
    }
}
