import java.lang.ref.ReferenceQueue;
import java.lang.ref.SoftReference;
import java.lang.ref.WeakReference;
import java.util.ArrayList;

/**
 * RefCheckOld — INT-8 acceptance probe.
 *
 * Two behaviors that ONLY remark-time reference processing can produce:
 *
 * 1. Weak referents are interleaved with permanently-retained "keeper"
 *    arrays, so after promotion every region holding a referent is ~95%
 *    live. A mixed-GC collection set never selects such low-efficiency
 *    regions, so evacuation-pause reference processing can never observe
 *    these referents dead — they clear ONLY if the collector processes
 *    references against the completed mark bitmap at remark.
 *
 * 2. Finalizable objects are allocated in a segregated burst surrounded by
 *    garbage, so after the strong drop their regions are wholly dead. A
 *    collector that frees such regions in place at cleanup without
 *    resurrection silently never runs finalize().
 *
 * Run with -XX:+UseG1GC -XX:InitiatingHeapOccupancyPercent=1 -Xmx256m on
 * both HotSpot (golden) and CratonVM.
 */
public class RefCheckOld {
    static final int N = 64;
    static final int NFIN = 32;
    static final int NSOFT = 8;

    static ArrayList<byte[]> keepers = new ArrayList<>();
    static Object[] strong = new Object[N];
    static WeakReference<Object>[] weaks;
    static Object[] softStrong = new Object[NSOFT];
    static SoftReference<Object>[] softs;
    static ReferenceQueue<Object> queue = new ReferenceQueue<>();

    static final Object LOCK = new Object();
    static int finalized = 0;

    static class Fin {
        byte[] pad = new byte[1024];

        @Override
        protected void finalize() {
            synchronized (LOCK) {
                finalized++;
            }
        }
    }

    static Fin[] fins = new Fin[NFIN];
    static Object churn;

    @SuppressWarnings("unchecked")
    public static void main(String[] args) throws Exception {
        weaks = new WeakReference[N];
        softs = new SoftReference[NSOFT];

        // Weak referents pinned inside mostly-live regions: keeper, referent,
        // keeper, referent, ... — after promotion each region holding a
        // referent is dominated by retained keeper bytes.
        for (int i = 0; i < N; i++) {
            keepers.add(new byte[32 * 1024]);
            strong[i] = new byte[1024];
            weaks[i] = new WeakReference<>(strong[i], queue);
            keepers.add(new byte[32 * 1024]);
        }
        for (int i = 0; i < NSOFT; i++) {
            keepers.add(new byte[32 * 1024]);
            softStrong[i] = new byte[1024];
            softs[i] = new SoftReference<>(softStrong[i]);
        }

        // Finalizables in a segregated burst surrounded by garbage — their
        // regions become WHOLLY dead once dropped.
        churnMB(2);
        for (int i = 0; i < NFIN; i++) {
            fins[i] = new Fin();
        }
        churnMB(2);

        // Promote everything to Old: force >= 20 young collections while
        // strongly held (CratonVM's G1 tenuring threshold is 15; HotSpot's
        // adaptive threshold promotes sooner).
        for (int r = 0; r < 20; r++) {
            churnMB(4);
            System.gc();
        }

        // Drop the strong paths.
        java.util.Arrays.fill(strong, null);
        java.util.Arrays.fill(fins, null);
        java.util.Arrays.fill(softStrong, null);

        // Bounded cycle driving: with IHOP=1 a few young pauses start and
        // finish a marking cycle. 25 rounds is far more than enough for a
        // remark-time processor; a collector that relies on mixed
        // evacuation will never select the ~95%-live keeper regions.
        int cleared = 0;
        int fin = 0;
        for (int round = 0; round < 25; round++) {
            churnMB(48);
            System.gc();
            Thread.sleep(25);
            cleared = 0;
            for (int i = 0; i < N; i++) {
                if (weaks[i].get() == null) {
                    cleared++;
                }
            }
            synchronized (LOCK) {
                fin = finalized;
            }
            if (cleared == N && fin >= NFIN) {
                break;
            }
        }
        for (int i = 0; i < 12; i++) {
            synchronized (LOCK) {
                fin = finalized;
            }
            if (fin >= NFIN) {
                break;
            }
            System.gc();
            Thread.sleep(25);
        }

        int enq = 0;
        while (queue.poll() != null) {
            enq++;
        }
        int softKept = 0;
        for (int i = 0; i < NSOFT; i++) {
            if (softs[i].get() != null) {
                softKept++;
            }
        }
        synchronized (LOCK) {
            fin = finalized;
        }
        // Touch the keepers so no optimizer can pretend they died.
        long keepSum = 0;
        for (byte[] k : keepers) {
            keepSum += k.length;
        }
        System.out.println(
                "RESULT oldCleared=" + cleared + "/" + N + " enqueued=" + enq + " finalized=" + fin
                        + "/" + NFIN + " softKept=" + softKept + "/" + NSOFT + " keepKB="
                        + (keepSum / 1024));
    }

    static void churnMB(int mb) {
        for (int i = 0; i < mb * 16; i++) {
            churn = new byte[64 * 1024];
        }
        churn = null;
    }
}
