import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.lang.foreign.ValueLayout;

/**
 * The per-element cost of FFM element access, against two controls.
 *
 * This is the fast loop for
 * `bug-kfusion-tornadovm-cpu-path-superlinear-slowdown-oom-20260824.md`:
 * kfusion's whole 126x is `VolumeShort2.get` -> `ShortArray.get` ->
 * `MemorySegment.getAtIndex(JAVA_SHORT, i)`, one call per voxel, and the
 * app needs ~16.7M of them per frame. Reproducing that at the element
 * level takes seconds instead of ten minutes a frame.
 *
 * Three arms, all reached the same way so the numbers are comparable:
 *
 *   short[]                  the floor  (~0.8 ns when the JIT is working)
 *   Unsafe.getShort(long)    the native dispatch funnel on its own
 *   MemorySegment.getAtIndex what kfusion actually runs
 *
 * The `Unsafe` arm is the control that separates "a native call is
 * expensive" from "the segment accessor is expensive on top of that".
 *
 * Usage: SegElemBench [elements] [reps]
 */
public class SegElemBench {
    static final int DEFAULT_N = 1 << 20;
    static final int DEFAULT_REPS = 5;

    public static void main(String[] args) throws Throwable {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : DEFAULT_N;
        int reps = args.length > 1 ? Integer.parseInt(args[1]) : DEFAULT_REPS;

        short[] heap = new short[n];
        for (int i = 0; i < n; i++) heap[i] = (short) (i & 0x7fff);

        Arena arena = Arena.ofAuto();
        MemorySegment seg = arena.allocate((long) n * 2, 2);
        for (int i = 0; i < n; i++) seg.setAtIndex(ValueLayout.JAVA_SHORT, i, (short) (i & 0x7fff));

        long want = 0;
        for (int i = 0; i < n; i++) want += heap[i];

        double bestHeap = Double.MAX_VALUE, bestSeg = Double.MAX_VALUE, bestGetSet = Double.MAX_VALUE;
        long sink = 0;
        for (int r = 0; r < reps; r++) {
            long t0 = System.nanoTime();
            long s1 = sumHeap(heap, n);
            double nsHeap = (System.nanoTime() - t0) / (double) n;

            t0 = System.nanoTime();
            long s2 = sumSegment(seg, n);
            double nsSeg = (System.nanoTime() - t0) / (double) n;

            t0 = System.nanoTime();
            long s3 = copySegment(seg, n);
            double nsGetSet = (System.nanoTime() - t0) / (double) n;

            if (s1 != want || s2 != want) {
                throw new IllegalStateException("checksum mismatch: heap=" + s1 + " seg=" + s2 + " want=" + want);
            }
            sink += s3;
            bestHeap = Math.min(bestHeap, nsHeap);
            bestSeg = Math.min(bestSeg, nsSeg);
            bestGetSet = Math.min(bestGetSet, nsGetSet);
        }

        System.out.println("SEGELEM n=" + n + " reps=" + reps
                + " heap_ns=" + fmt(bestHeap)
                + " segment_get_ns=" + fmt(bestSeg)
                + " segment_getset_ns=" + fmt(bestGetSet)
                + " ratio_get_over_heap=" + fmt(bestSeg / Math.max(bestHeap, 1e-9))
                + " checksum=" + want
                + " sink=" + (sink == 0 ? 0 : 1));
    }

    /** The floor: a plain array element. */
    static long sumHeap(short[] a, int n) {
        long s = 0;
        for (int i = 0; i < n; i++) s += a[i];
        return s;
    }

    /** What kfusion runs, one `getAtIndex` per element. */
    static long sumSegment(MemorySegment seg, int n) {
        long s = 0;
        for (int i = 0; i < n; i++) s += seg.getAtIndex(ValueLayout.JAVA_SHORT, i);
        return s;
    }

    /** The integration-stage shape: a read AND a write per element. */
    static long copySegment(MemorySegment seg, int n) {
        long s = 0;
        for (int i = 0; i < n; i++) {
            short v = seg.getAtIndex(ValueLayout.JAVA_SHORT, i);
            seg.setAtIndex(ValueLayout.JAVA_SHORT, i, v);
            s += v;
        }
        return s;
    }

    static String fmt(double v) {
        return String.format(java.util.Locale.ROOT, "%.3f", v);
    }
}
