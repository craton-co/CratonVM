/**
 * Peak, then drop: does the collector give the peak back to the OS?
 *
 * `gc::reservation` reserves the heap's address space and commits it in 2 MiB
 * granules, and `Arena::decommit_unbumped_middle` hands the un-bumped middle
 * back at the start of each collection. Before that, a JVM that peaked and
 * then idled held its peak for the life of the process -- the sweep retracted
 * the bump cursor, the compactor emptied whole megabytes, and the pages stayed
 * resident.
 *
 * Run with `--verbose:gc` and read `committed=` on the `zgc-reclaim` line: it
 * must RISE through the peak and FALL afterwards. With
 * `CRATONVM_GC_RESERVE=0` it is `capacity` throughout, which is the A/B.
 */
public class HeapGiveBack {
    static Object[] hold;

    public static void main(String[] args) throws Exception {
        int peakMb = args.length > 0 ? Integer.parseInt(args[0]) : 48;
        // PEAK: hold a large live set so the cursor has to climb.
        int chunks = peakMb;
        hold = new Object[chunks];
        for (int i = 0; i < chunks; i++) {
            hold[i] = new byte[1024 * 1024];
        }
        long sum = 0;
        for (int i = 0; i < chunks; i++) {
            sum += ((byte[]) hold[i]).length;
        }
        System.out.println("peak held: " + (sum / (1024 * 1024)) + " MB");

        // DROP: release everything and collect twice. The first cycle sweeps
        // and retracts the cursor; the second finds the middle un-bumped and
        // hands it back, because the give-back runs at the START of a
        // collection (it must not destroy the last slide's forwarding records).
        hold = null;
        for (int i = 0; i < 4; i++) {
            System.gc();
        }
        System.out.println("dropped");
    }
}
