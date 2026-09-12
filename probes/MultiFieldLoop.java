/**
 * A counted loop reading FOUR DISTINCT instance fields per iteration.
 *
 * `FieldLoop` reads one field, so its loop carries exactly one inline-getfield
 * site and therefore exactly one `emit_layout_epoch_guard`. That makes it the
 * right probe for register residency and the wrong one for anything priced per
 * SITE: a change worth two instructions per guard is worth two instructions per
 * iteration there, which is under this host's noise floor.
 *
 * Four distinct fields cannot be collapsed by GVN the way four reads of one
 * field can, so this body carries four sites and four guards. It is the same
 * loop shape otherwise, deliberately — the point is to vary the site count and
 * nothing else.
 *
 * `sumGuarded` is the measured method. `reps` calls it so it reaches the JIT
 * through the invocation-count door rather than only through OSR.
 */
public class MultiFieldLoop {
    int fa = 3;
    int fb = 5;
    int fc = 7;
    int fd = 11;

    int sumGuarded(int n) {
        int a = 0;
        for (int i = 0; i < n; i++) {
            a += this.fa;
            a += this.fb;
            a += this.fc;
            a += this.fd;
        }
        return a;
    }

    public static void main(String[] args) {
        MultiFieldLoop p = new MultiFieldLoop();
        int reps = Integer.getInteger("probe.reps", 2000);
        int n = Integer.getInteger("probe.n", 20000);
        long acc = 0;
        // Warm both doors before the clock starts.
        for (int r = 0; r < 50; r++) {
            acc += p.sumGuarded(1000);
        }
        long t0 = System.nanoTime();
        for (int r = 0; r < reps; r++) {
            acc += p.sumGuarded(n);
        }
        long t1 = System.nanoTime();
        System.out.println("acc=" + acc + " ms=" + (t1 - t0) / 1000000L);
    }
}
