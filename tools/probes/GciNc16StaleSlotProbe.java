/**
 * gci i16/nc: the G1 shape behind the vi7-g1y net-of-pins wrong result and
 * crash (gci-i15ra-g1-evacuation-additions-review-findings-20261014 finding 13).
 *
 * <p>Each round makes dead Old holders D whose field names a fresh young
 * bridge Y, whose field names a dead Old target B (D -> Y -> B, nothing else
 * reaches any of them), so a young pause copies Y as floating garbage through
 * D's slot and, during a concurrent cycle, promotes it above TAMS where the
 * bitmap scrub cannot see it. Cleanup then frees B's regions in place, the
 * live int[] turnover below (ascending values, like javac's line maps) reuses
 * them, and a pause that follows Y's stale slot into the middle of a live
 * int[] corrupts it. A 48 MB ballast table keeps the old generation above the
 * default IHOP at -Xmx128m and a quarter of it turns over per round, so
 * concurrent cycles and mixed pauses run.
 *
 * <p>Every live int[] and its owning node are verified at the end of each
 * round; stdout is one line per round plus a digest, all independent of GC
 * timing (byte-identical on HotSpot). Arguments: rounds (default 40), live
 * arrays (default 4096), doomed chains per round (default 8192).
 */
public final class GciNc16StaleSlotProbe {
    static final class Node {
        Node next;
        Object link;
        int[] payload;
        int id;
    }

    static volatile Object sink;

    static void churn(int kib) {
        Object[] window = new Object[256];
        for (int i = 0; i < kib; i++) {
            window[i & 255] = new byte[1024];
        }
        sink = window;
    }

    public static void main(String[] args) {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 40;
        int liveCount = args.length > 1 ? Integer.parseInt(args[1]) : 4096;
        int doomed = args.length > 2 ? Integer.parseInt(args[2]) : 8192;

        int ballastSlots = 12 * 1024;
        Object[] ballast = new Object[ballastSlots];
        for (int i = 0; i < ballastSlots; i++) {
            ballast[i] = new byte[4096];
        }

        int[][] live = new int[liveCount][];
        Node[] owners = new Node[liveCount];
        long digest = 17;
        int badRounds = 0;
        long seed = 12345;

        for (int r = 0; r < rounds; r++) {
            // 1. Dead-to-be Old targets B and holders D, allocated in batches so
            //    whole regions die together; churn ages them into Old.
            Node[] targets = new Node[doomed];
            Node[] holders = new Node[doomed];
            for (int i = 0; i < doomed; i++) {
                Node b = new Node();
                b.id = i;
                b.payload = new int[8];
                targets[i] = b;
            }
            for (int i = 0; i < doomed; i++) {
                holders[i] = new Node();
            }
            churn(32 * 1024);

            // 2. Young bridges: D -> Y -> B. Then nothing reaches D, Y or B.
            for (int i = 0; i < doomed; i++) {
                Node y = new Node();
                y.link = targets[i];
                holders[i].next = y;
            }
            targets = null;
            holders = null;

            // 3. Ballast turnover (old regions die, mixed pauses have work).
            for (int k = 0; k < ballastSlots / 4; k++) {
                seed = seed * 6364136223846793005L + 1442695040888963407L;
                int at = (int) ((seed >>> 33) % ballastSlots);
                ballast[at] = new byte[4096];
            }

            // 4. Live int[] turnover, with churn so young pauses run meanwhile.
            for (int i = 0; i < liveCount; i++) {
                int idx = (int) (((long) i * 7 + r) % liveCount);
                int[] a = new int[64 + (idx % 64)];
                int base = r * 100000 + idx * 37;
                for (int k = 0; k < a.length; k++) {
                    a[k] = base + k * 5;
                }
                Node n = new Node();
                n.id = idx;
                n.payload = a;
                live[idx] = a;
                owners[idx] = n;
                if ((i & 31) == 0) {
                    churn(64);
                }
            }

            // 5. Verify every live array and its owner.
            long sum = 0;
            int bad = 0;
            for (int idx = 0; idx < liveCount; idx++) {
                int[] a = live[idx];
                Node n = owners[idx];
                if (a == null || n == null || n.payload != a || n.id != idx
                        || a.length != 64 + (idx % 64)) {
                    bad++;
                    continue;
                }
                int expect = r * 100000 + idx * 37;
                for (int k = 0; k < a.length; k++) {
                    if (a[k] != expect + k * 5) {
                        bad++;
                        if (bad <= 4) {
                            System.err.println("[gci-nc16] round " + r + " idx " + idx
                                    + " k " + k + " got " + a[k] + " want " + (expect + k * 5));
                        }
                        break;
                    }
                }
                sum = sum * 31 + a[0] + a[a.length - 1] + a.length;
            }
            if (bad != 0) {
                badRounds++;
                System.err.println("[gci-nc16] round " + r + " bad=" + bad);
            }
            digest = digest * 1000003 + sum;
            System.out.println("round " + r + " " + (bad == 0 ? "ok" : "CORRUPT"));
        }
        sink = ballast;
        System.out.println((badRounds == 0 ? "PASS" : "FAIL") + " rounds=" + rounds
                + " live=" + liveCount + " doomed=" + doomed
                + " digest=" + Long.toHexString(digest));
    }
}
