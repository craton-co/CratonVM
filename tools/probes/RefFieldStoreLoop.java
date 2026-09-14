/**
 * A counted loop STORING four distinct reference instance fields per iteration.
 *
 * `MultiFieldLoop` is the read-side probe: four distinct int fields, four
 * inline `getfield` sites, four layout-epoch guards — in the OPTIMIZING tier,
 * which is the only tier whose field-access emitters bake a compact body
 * offset for a primitive read.
 *
 * The single-pass tier bakes one in exactly three places, and all three are
 * reference `putfield`s (`emit_layout_epoch_guard`'s call sites in
 * `jit/src/x64/objects.rs`). So a read loop cannot exercise that tier's guard
 * at all, and this is the shape that can: four DISTINCT reference fields, so
 * nothing collapses them, in a loop the single-pass tier unrolls — which
 * multiplies the site count by the unroll factor and is the reason the guard's
 * encoding is priced differently there than in the tier that does not unroll.
 *
 * The two values alternate so the stored reference actually changes; a store
 * of the value already in the field would still emit the same code, but a
 * loop whose stores are all no-ops invites a reader to wonder whether
 * something folded them.
 *
 * `churn` is the measured method. `reps` calls it so it reaches the JIT
 * through the invocation-count door and not only through OSR.
 */
public class RefFieldStoreLoop {
    Object ra;
    Object rb;
    Object rc;
    Object rd;

    static final Object X = new Object();
    static final Object Y = new Object();

    int churn(int n) {
        int hits = 0;
        for (int i = 0; i < n; i++) {
            Object v = ((i & 1) == 0) ? X : Y;
            this.ra = v;
            this.rb = v;
            this.rc = v;
            this.rd = v;
            if (this.ra == X) {
                hits++;
            }
        }
        return hits;
    }

    public static void main(String[] args) {
        RefFieldStoreLoop p = new RefFieldStoreLoop();
        int reps = Integer.getInteger("probe.reps", 2000);
        int n = Integer.getInteger("probe.n", 20000);
        long acc = 0;
        // Warm both doors before the clock starts.
        for (int r = 0; r < 50; r++) {
            acc += p.churn(1000);
        }
        long t0 = System.nanoTime();
        for (int r = 0; r < reps; r++) {
            acc += p.churn(n);
        }
        long t1 = System.nanoTime();
        System.out.println("acc=" + acc + " ms=" + (t1 - t0) / 1000000L);
    }
}
