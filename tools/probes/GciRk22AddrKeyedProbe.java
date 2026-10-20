import java.util.concurrent.locks.StampedLock;

/**
 * gci i22/g1rk -- address-keyed side-table rows (the Throwable back-trace
 * table, the lock-key registry) whose key dies in a G1 region that the
 * remark's cleanup KEEPS (it also holds live objects).
 *
 * {@code gch-h4g1-proposal-g1-remark-sweeps-every-address-keyed-table-mark-exact-20261009}:
 * G1 swept those tables on a region-granular verdict, so such a row outlived
 * its key until a mixed pause evacuated the region. Behind
 * {@code CRATONVM_G1_REMARK_ADDR_KEYED_MARK_EXACT=1} the remark doors sweep
 * them on the completed mark instead. This probe is the soundness side, which
 * is Java-visible: every round tenures interleaved keepers, Throwables and
 * write-locked StampedLocks together, drops three of every four Throwables and
 * locks (keeping every keeper, so their regions stay), collects, and then
 * checks that
 * <ul>
 *   <li>every surviving Throwable still reports its own stack trace (a live
 *       row dropped by a wrong "dead" verdict reads empty or foreign),</li>
 *   <li>every surviving lock still reads write-locked and excludes a reader,</li>
 *   <li>fresh Throwables and locks (possibly on reused addresses) start clean
 *       and report their own traces (a dead row inherited reads foreign).</li>
 * </ul>
 * The engagement (rows judged dead) is on stderr under
 * {@code CRATONVM_DBG_G1_REMARK_ADDR_KEYED=1}, from the VM.
 *
 * stdout is deterministic: one line per round, then PROBE-OK / PROBE-FAIL.
 * stderr: {@code elapsed-ms=}. Args: [rounds] [n] [gcs-to-tenure]
 * (default 4 2000 16).
 */
public final class GciRk22AddrKeyedProbe {
    static volatile Object sink;

    static Throwable makeThrowable(int i) {
        return new Throwable("t" + i);
    }

    static Throwable freshThrowable(int i) {
        return new Throwable("f" + i);
    }

    static void churn() {
        for (int k = 0; k < 2000; k++) {
            sink = new byte[1024];
        }
    }

    static String top(Throwable t) {
        StackTraceElement[] st = t.getStackTrace();
        return st.length == 0 ? "<empty>" : st[0].getMethodName() + "/" + st.length;
    }

    public static void main(String[] a) throws Exception {
        int rounds = a.length > 0 ? Integer.parseInt(a[0]) : 4;
        int n = a.length > 1 ? Integer.parseInt(a[1]) : 2000;
        int tenure = a.length > 2 ? Integer.parseInt(a[2]) : 16;
        // The VM's own answer for each creation site, taken before any
        // collection: the checks below compare against it, so they test the
        // rows' survival, not the frame naming.
        String madeTop = top(makeThrowable(-1));
        String freshTop = top(freshThrowable(-1));
        int bad = 0;
        long t0 = System.nanoTime();
        for (int r = 1; r <= rounds; r++) {
            Object[] keep = new Object[n];
            Throwable[] thr = new Throwable[n];
            StampedLock[] locks = new StampedLock[n];
            for (int i = 0; i < n; i++) {
                keep[i] = new long[] {i, r};
                thr[i] = makeThrowable(i);
                locks[i] = new StampedLock();
                locks[i].writeLock();
            }
            for (int g = 0; g < tenure; g++) {
                churn();
                System.gc();
            }
            int kept = (n + 3) / 4;
            Throwable[] keptThr = new Throwable[kept];
            StampedLock[] keptLocks = new StampedLock[kept];
            for (int i = 0, j = 0; i < n; i += 4, j++) {
                keptThr[j] = thr[i];
                keptLocks[j] = locks[i];
            }
            thr = null;
            locks = null;
            for (int g = 0; g < 3; g++) {
                churn();
                System.gc();
            }
            int traceBad = 0;
            for (Throwable t : keptThr) {
                if (!madeTop.equals(top(t))) {
                    traceBad++;
                }
            }
            int lockBad = 0;
            for (StampedLock l : keptLocks) {
                if (!l.isWriteLocked() || l.tryReadLock() != 0L) {
                    lockBad++;
                }
            }
            int freshBad = 0;
            for (int i = 0; i < n; i++) {
                StampedLock l = new StampedLock();
                if (l.isWriteLocked() || l.isReadLocked()) {
                    freshBad++;
                }
                if (!freshTop.equals(top(freshThrowable(i)))) {
                    freshBad++;
                }
            }
            long sum = 0;
            for (Object o : keep) {
                long[] v = (long[]) o;
                sum += v[0] + v[1];
            }
            bad += traceBad + lockBad + freshBad;
            System.out.println("round " + r + " kept=" + kept + " traceBad=" + traceBad
                    + " lockBad=" + lockBad + " freshBad=" + freshBad + " sum=" + sum);
            sink = keptThr;
            sink = keptLocks;
        }
        System.err.println("elapsed-ms=" + (System.nanoTime() - t0) / 1_000_000);
        System.out.println(bad == 0 ? "PROBE-OK addr-keyed" : "PROBE-FAIL addr-keyed bad=" + bad);
    }
}
