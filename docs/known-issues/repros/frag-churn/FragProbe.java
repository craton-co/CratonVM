/**
 * JIT-heavy fragmentation workload — the cost arm the corpse-read fix left
 * open: ZGC declines to relocate while a compiled frame is live, and on this
 * collector compaction is also the only defragmentation, so a permanently
 * JIT-busy process may never defragment.
 *
 * The churn loop lives in its own method so it tiers up and the process spends
 * its life inside compiled frames (a loop in main() measures the interpreter).
 * It keeps a rolling live window of mixed-size arrays, which is what leaves
 * holes; the final phase then asks for large contiguous blocks, which is what a
 * fragmented heap cannot satisfy however much total free space it reports.
 *
 * Prints one RESULT line; the VM's own [GC] zgc-features / zgc-frag lines carry
 * relocation_skipped_jit, compaction_cycles and the fragmentation gauge.
 */
public final class FragProbe {
    private static final int WINDOW = 512;
    private static final Object[] LIVE = new Object[WINDOW];
    private static long sink;

    /** Hot, allocating, called in a loop — this is what gets compiled. */
    private static int churn(int seed, int rounds) {
        int x = seed;
        for (int i = 0; i < rounds; i++) {
            x = x * 1103515245 + 12345;
            int size = 64 + ((x >>> 16) & 0x1FFF); // 64 B .. ~8 KiB
            byte[] b = new byte[size];
            b[0] = (byte) x;
            b[size - 1] = (byte) i;
            sink += b[0] + b[size - 1];
            LIVE[(x >>> 4) & (WINDOW - 1)] = b;
        }
        return x;
    }

    public static void main(String[] args) {
        int outer = args.length > 0 ? Integer.parseInt(args[0]) : 400;
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 20000;
        long t0 = System.currentTimeMillis();
        int x = 1;
        for (int i = 0; i < outer; i++) {
            x = churn(x, rounds);
        }
        long churnMs = System.currentTimeMillis() - t0;

        // Fragmentation assay: how many 4 MiB contiguous blocks can this heap
        // still hand out? Total free space is not the question; contiguity is.
        int big = 0;
        java.util.List<byte[]> blocks = new java.util.ArrayList<>();
        try {
            while (big < 256) {
                blocks.add(new byte[4 * 1024 * 1024]);
                big++;
            }
        } catch (OutOfMemoryError e) {
            // expected terminator
        }
        blocks.clear();
        System.out.println("RESULT churn_ms=" + churnMs + " big_4mib_blocks=" + big
                + " sink=" + (sink & 0xFF) + " x=" + (x & 0xFF));
    }
}
