// Targeted repro for the BOX_UNBOX intrinsic's SIGSEGV under a relocating
// collector (fixed-bugs/zgc-relocation-slides-wrote-into-decommitted-granules-FIXED-20260904.md).
//
// The first version of this probe ran clean 3/3 and meant nothing:
// `objects_relocated=0`, `compaction_cycles=0`. It allocated the boxed
// receivers in one dense block, so they were never worth moving, and a
// collector that never relocates cannot exercise a relocation defect. Read
// those two counters (CRATONVM_GC_STATS=1) before believing any result here.
//
// So: allocate the receivers INTERLEAVED with garbage, so they land scattered
// across pages; then drop the garbage, which leaves those pages sparse and
// makes them exactly the compaction candidates the collector is looking for.
// Re-box a slice of the table every round to keep fragmenting it.
//
// The checksum is the other half: silent corruption of an unboxed payload is
// the same defect as the crash, and only a value check sees it.
public class BoxUnboxReloc {

    static final int TABLE = 8192;
    static Long[] longs = new Long[TABLE];
    static Integer[] ints = new Integer[TABLE];
    static Object[] sparse = new Object[4096];

    static long unboxLong(Long v) {
        return v.longValue();
    }

    static int unboxInt(Integer v) {
        return v.intValue();
    }

    /** Box slot `i`, with garbage on either side so it lands scattered. */
    static void refill(int i) {
        sparse[(i * 3) % sparse.length] = new byte[96];
        longs[i] = Long.valueOf(1000000L + i);
        sparse[(i * 5 + 1) % sparse.length] = new byte[96];
        ints[i] = Integer.valueOf(100000 + i);
        sparse[(i * 7 + 2) % sparse.length] = new byte[96];
    }

    public static void main(String[] args) {
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 2000;

        long expected = 0;
        for (int i = 0; i < TABLE; i++) {
            refill(i);
            expected += 1000000L + i;
            expected += 100000 + i;
        }

        long acc = 0;
        int bad = 0;
        long nulls = 0;
        for (int round = 0; round < iters; round++) {
            long sum = 0;
            for (int i = 0; i < TABLE; i++) {
                sum += unboxLong(longs[i]);
                sum += unboxInt(ints[i]);
            }
            // Force the intrinsic's BAIL edge. The inline sequence has no call
            // and therefore no safepoint, so nothing can relocate inside it --
            // but the null check deopts to the interpreter, and THAT is a call,
            // taken with the receiver already popped from the simulated operand
            // stack. If the window is anywhere, it is here.
            for (int n = 0; n < 64; n++) {
                try {
                    sum += unboxLong(null);
                } catch (NullPointerException e1) {
                    nulls++;
                }
                try {
                    sum += unboxInt(null);
                } catch (NullPointerException e2) {
                    nulls++;
                }
            }
            if (sum != expected) {
                bad++;
                if (bad <= 3) {
                    System.out.println("MISMATCH round=" + round
                            + " got=" + sum + " want=" + expected);
                }
            }
            acc += sum;

            // Drop most of the interleaved garbage: the pages holding the
            // boxed receivers go sparse, which is what makes them worth
            // relocating rather than sweeping.
            for (int j = 0; j < sparse.length; j += 3) {
                sparse[j] = null;
            }
            // Churn hard enough to force cycles.
            for (int j = 0; j < 2048; j++) {
                sparse[(round * 11 + j) % sparse.length] = new byte[256];
            }
            // Re-box a moving slice, so the table keeps fragmenting instead of
            // settling into one compacted block.
            int base = (round * 512) % TABLE;
            for (int k = 0; k < 512; k++) {
                refill((base + k) % TABLE);
            }
        }

        System.out.println("BoxUnboxReloc iters=" + iters
                + " acc=" + acc + " mismatches=" + bad + " nullBails=" + nulls);
    }
}
