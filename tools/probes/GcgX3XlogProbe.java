/**
 * gcg g3/x (2026-10-01): the {@code -Xlog:gc*} lines HotSpot 25 Serial prints,
 * one mode per shape. stdout is deterministic (one {@code done <mode>} line);
 * the log lines are what the rows compare, by shape (uptime and sizes vary).
 *
 * <ul>
 *   <li>{@code young}: young collections only (8 KiB arrays, none kept).</li>
 *   <li>{@code sysgc}: one {@code System.gc()} (a {@code Pause Full}).</li>
 *   <li>{@code escalate}: keeps about 3/4 of {@code -Xmx} reachable through
 *       a list of 64 KiB arrays, then allocates garbage: a young collection
 *       whose promotion cannot fit escalates to a full collection. HotSpot
 *       25 Serial prints a {@code Pause Young} (with its own {@code GC(n)})
 *       and then a {@code Pause Full} with the next id; run with
 *       {@code -Xmx64m}.</li>
 *   <li>{@code refs}: one dead-referent {@code WeakReference} on a queue, one
 *       recent {@code SoftReference}, one dead-referent
 *       {@code PhantomReference}, then {@code System.gc()} (the
 *       {@code gc+ref} / {@code gc+phases+ref} shapes).</li>
 *   <li>{@code freeratio}: tenures about 48 MiB, drops it, calls
 *       {@code System.gc()} three times and prints {@code totalMemory()} in
 *       MiB after the tenuring collection and after each of the three on
 *       stderr ({@code x3 freeratio total_mb a b c d}); the
 *       {@code -XX:MaxHeapFreeRatio} / {@code MinHeapFreeRatio} rows, run with
 *       {@code -Xms8m -Xmx128m} (HotSpot 25.0.3 Serial: {@code 117 117 106 67}
 *       by default, {@code 74 74 67 43} at 5/10, {@code 70 70 70 70} at
 *       0/100).</li>
 *   <li>{@code promofail}: a young collection whose promotion fails (HotSpot:
 *       {@code Promotion failed}, {@code Pause Young}, then {@code Pause Full}
 *       with the next id); run with {@code -Xmx64m}.</li>
 * </ul>
 */
import java.lang.ref.PhantomReference;
import java.lang.ref.ReferenceQueue;
import java.lang.ref.SoftReference;
import java.lang.ref.WeakReference;
import java.util.ArrayList;
import java.util.List;

public class GcgX3XlogProbe {
    static volatile Object sink;
    static List<byte[]> keep = new ArrayList<>();

    public static void main(String[] args) {
        String mode = args.length > 0 ? args[0] : "young";
        switch (mode) {
            case "young":
                for (int i = 0; i < 20_000; i++) {
                    sink = new byte[8192];
                }
                break;
            case "sysgc":
                for (int i = 0; i < 1000; i++) {
                    sink = new byte[8192];
                }
                System.gc();
                break;
            case "escalate": {
                long max = Runtime.getRuntime().maxMemory();
                long target = max / 4 * 3;
                long held = 0;
                while (held < target) {
                    keep.add(new byte[64 * 1024]);
                    held += 64 * 1024;
                }
                for (int i = 0; i < 20_000; i++) {
                    sink = new byte[8192];
                }
                break;
            }
            case "promofail": {
                // Tenure ~32 MiB, churn garbage (the promotion average falls to
                // about 0), then keep a whole eden's worth: the next young
                // collection is attempted and its promotion fails.
                for (int i = 0; i < 512; i++) {
                    keep.add(new byte[64 * 1024]);
                }
                System.gc();
                for (int i = 0; i < 100_000; i++) {
                    sink = new byte[8192];
                }
                List<byte[]> burst = new ArrayList<>();
                for (int i = 0; i < 2_000; i++) {
                    burst.add(new byte[8192]);
                }
                sink = burst;
                break;
            }
            case "refs": {
                ReferenceQueue<Object> q = new ReferenceQueue<>();
                WeakReference<Object> w = new WeakReference<>(new Object(), q);
                Object softReferent = new Object();
                SoftReference<Object> s = new SoftReference<>(softReferent);
                PhantomReference<Object> p = new PhantomReference<>(new Object(), q);
                softReferent = null;
                System.gc();
                sink = w;
                sink = s;
                sink = p;
                break;
            }
            case "freeratio": {
                Runtime rt = Runtime.getRuntime();
                for (int i = 0; i < 768; i++) {
                    keep.add(new byte[64 * 1024]);
                }
                System.gc();
                StringBuilder sb = new StringBuilder();
                sb.append(' ').append(rt.totalMemory() >> 20);
                keep.clear();
                for (int i = 0; i < 3; i++) {
                    System.gc();
                    sb.append(' ').append(rt.totalMemory() >> 20);
                }
                // Sizes differ per VM: stderr, compared between arms of one VM.
                System.err.println("x3 freeratio total_mb" + sb);
                break;
            }
            case "isverbose":
                // gcg g5/yg: HotSpot's MemoryMXBean.isVerbose() is the `gc`
                // log level at info: true under -verbose:gc AND -Xlog:gc.
                System.out.println("isVerbose="
                        + java.lang.management.ManagementFactory.getMemoryMXBean().isVerbose());
                break;
            case "freeratiolive": {
                // gcg g5/yg: as `freeratio`, but half the tenured set stays
                // live (24 MiB), so the shrink target `used * 100 / (100 -
                // MaxHeapFreeRatio)` is above any floor and the arms can
                // differ (vg3: with nothing live, `def` and `tight` both sat
                // at the same floor).
                Runtime rt = Runtime.getRuntime();
                for (int i = 0; i < 768; i++) {
                    keep.add(new byte[64 * 1024]);
                }
                System.gc();
                StringBuilder sb = new StringBuilder();
                sb.append(' ').append(rt.totalMemory() >> 20);
                keep.subList(384, keep.size()).clear();
                for (int i = 0; i < 3; i++) {
                    System.gc();
                    sb.append(' ').append(rt.totalMemory() >> 20);
                }
                System.err.println("x3 freeratiolive total_mb" + sb + " kept=" + keep.size());
                break;
            }
            default:
                throw new IllegalArgumentException(mode);
        }
        System.out.println("done " + mode);
    }
}
