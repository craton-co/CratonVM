/**
 * gci i8/gs: compiled array churn for the occupancy door label
 * ({@code gci-i7orch-g1-young-sizing-and-door-label-review-findings} item 1).
 *
 * <p>A hot method allocates {@code byte[]} and {@code Object[]} arrays of
 * {@code kib} KiB each, keeping the last 16 of each alive, so that once it is
 * compiled its allocations reach the compiled TLAB refill trigger
 * ({@code tlab-alloc-shaped}) and, on G1, the two JIT array headroom guards
 * ({@code jit-newarray-headroom}, {@code jit-anewarray-headroom}). Every
 * collection those take is an occupancy verdict, not a failed allocation.
 *
 * <p>Single-threaded, so every {@code [GC_OVERHEAD] refill-trigger:} line on
 * stderr follows the {@code [GC] pause:} line of its own pause. Stdout is a
 * deterministic checksum and {@code PROBE-OK}.
 *
 * <p>Usage: {@code GciGs8JitArrayChurnProbe [rounds=40] [kib=128]}
 */
public final class GciGs8JitArrayChurnProbe {

    private static final int PER_ROUND = 256;

    public static void main(String[] args) {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 40;
        int kib = args.length > 1 ? Integer.parseInt(args[1]) : 128;
        byte[][] ring = new byte[16][];
        Object[][] refRing = new Object[16][];
        long sum = 0;
        for (int r = 0; r < rounds; r++) {
            sum += round(ring, refRing, kib, r);
        }
        long live = 0;
        for (int i = 0; i < 16; i++) {
            live += ring[i].length + refRing[i].length;
        }
        System.out.println("GciGs8JitArrayChurnProbe rounds=" + rounds + " kib=" + kib
                + " checksum=" + sum + " live=" + live);
        System.out.println("PROBE-OK");
    }

    private static long round(byte[][] ring, Object[][] refRing, int kib, int r) {
        long s = 0;
        int bytes = kib * 1024;
        int refs = kib * 128;
        for (int i = 0; i < PER_ROUND; i++) {
            byte[] b = new byte[bytes];
            b[i] = (byte) (i + r);
            b[bytes - 1] = (byte) r;
            ring[i & 15] = b;
            Object[] o = new Object[refs];
            o[0] = b;
            o[refs - 1] = ring[(i + 7) & 15];
            refRing[i & 15] = o;
            s += b[i] + b[bytes - 1] + o.length + ((byte[]) o[0]).length;
        }
        return s;
    }
}
