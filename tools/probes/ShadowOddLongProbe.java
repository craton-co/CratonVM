/**
 * A compiled allocation loop whose `long` counter takes odd values must not
 * crash the collector.
 *
 * Found by the gc-common round (2026-09-24): the JIT publishes the counter in
 * a reference home of the shadow stack, and the shadow-stack readers treat any
 * entry with bit 0 set as an indirect frame-slot entry and dereference it. The
 * process then faults at `n - 1` inside `update_root_snapshot` or the
 * moving-young coverage verifier, on all three collectors. Stepping the counter
 * by two (always even) never crashes. See
 * `docs/internal/gc-common-round-20260923/common-w4o-shadow-stack-dereferences-a-mistyped-odd-primitive-FIXED-20260926.md`.
 *
 * Prints `PROBE-OK` when both rounds finish. HotSpot: PROBE-OK.
 */
public class ShadowOddLongProbe {
    static volatile Object sink;

    public static void main(String[] a) {
        long total = 0;
        for (int round = 0; round < 2; round++) {
            long n = 0;
            while (n < 20_000_000L) {
                sink = new byte[256];
                n++;
            }
            total += n;
        }
        System.out.println("total=" + total + " PROBE-OK");
    }
}
