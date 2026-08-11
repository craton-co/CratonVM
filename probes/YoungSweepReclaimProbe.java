// Does the non-moving young sweep reclaim anything?
//
// The fourth-recurrence writeup for `TestDefaultInstanceManager` concluded "no
// young object is reclaimed at all", off a counter that is printed before it is
// incremented. That is a whole-VM claim and it deserves a whole-VM measurement
// that does not need Tomcat, three JSPs and 17 seconds to make.
//
// Shape: allocate a large, KNOWN quantity of unreachable garbage into the young
// generation, keeping nothing, then ask for a collection. Run it under
// `CRATONVM_NO_MOVING_YOUNG=1 CRATONVM_DBG_GC_OVERHEAD=1` and read
// `young_free_list` off the `[GC-OVERHEAD]` line: a non-moving sweep that
// reclaims nothing leaves it at zero no matter how much garbage was made.
//
// `drop` is deliberately a method rather than a loop in `main` — a loop inline
// in `main` is refused OSR, so it would never tier up and the allocation would
// come from a different path than a real workload's.
//
// The sink is `static volatile` so the allocation cannot be optimised away, and
// the last reference is dropped before the collection so the garbage is
// genuinely unreachable rather than merely old.
public class YoungSweepReclaimProbe {
    static volatile Object sink;

    static long drop(int rounds, int perRound) {
        long n = 0;
        for (int r = 0; r < rounds; r++) {
            for (int i = 0; i < perRound; i++) {
                byte[] b = new byte[256];
                b[0] = (byte) i;
                sink = b;
                n += b.length;
            }
        }
        return n;
    }

    public static void main(String[] args) throws Exception {
        long bytes = drop(200, 2000);
        sink = null;
        System.out.println("allocated_garbage_bytes=" + bytes);
        System.gc();
        // A second round, so the line printed after the first collection is not
        // the only evidence: a sweep that works reclaims on both.
        bytes += drop(200, 2000);
        sink = null;
        System.gc();
        System.out.println("PASS YoungSweepReclaimProbe total=" + bytes);
    }
}
