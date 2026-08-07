import java.io.File;
import java.io.RandomAccessFile;
import java.nio.channels.FileChannel;
import java.nio.channels.FileLock;

/**
 * `FileChannel.tryLock()` returns a corrupt `fileLockTable` cell under the
 * compact reference-field layout.
 *
 * <p>`FileChannelImpl.fileLockTable()` is textbook double-checked locking over a
 * `volatile FileLockTable` field: assign it inside `synchronized (this)`, then
 * `return fileLockTable;`. On a build with `CRATONVM_COMPACT_REF_FIELDS` on
 * (the default since `HEADER_SIZE 24 -> 16`), that read comes back through the
 * heap's corrupt-cell guard:
 *
 * <pre>
 * gen_heap::read_slot: corrupt Value cell (out-of-range discriminant)
 *     — returning null instead of a UB-on-match Value
 * </pre>
 *
 * so the guard hands back `null` and the very next line NPEs on
 * `flt.add(fli)`. Nothing here is H2-specific: every file-backed database, and
 * anything else that takes a file lock, dies on its first open.
 *
 * <p>The workaround is a single environment variable — and the fact that it is
 * a complete one is the point, because it says the defect is in the compact
 * layout and nowhere else:
 *
 * <pre>
 * CRATONVM_COMPACT_REF_FIELDS=0 &lt;cratonvm&gt; ... CompactLayoutFileLockProbe
 * </pre>
 *
 * <p>Run with `CRATONVM_DBG=cellcorrupt` to get the holder census
 * (`[CELLCORRUPT] holder=... class=sun/nio/ch/FileChannelImpl ... index=4`),
 * which is what identifies the object and slot rather than just the symptom.
 */
public final class CompactLayoutFileLockProbe {

    public static void main(String[] args) throws Exception {
        File f = new File(System.getProperty("probe.file", "compactlayout-filelock.tmp"));
        f.deleteOnExit();
        int fails = 0;

        try (RandomAccessFile raf = new RandomAccessFile(f, "rw");
             FileChannel ch = raf.getChannel()) {

            FileLock lock = ch.tryLock();
            if (lock == null) {
                System.out.println("  FAIL tryLock returned null (file already locked?)");
                fails++;
            } else {
                System.out.println("  ok   tryLock -> " + lock);
                lock.release();
            }

            // Second round: the lock table now exists, so this exercises the
            // already-initialised read rather than the lazy-init one.
            FileLock again = ch.tryLock();
            if (again == null) {
                System.out.println("  FAIL second tryLock returned null");
                fails++;
            } else {
                System.out.println("  ok   second tryLock -> " + again);
                again.release();
            }
        } catch (NullPointerException e) {
            // This is the failure the probe exists for. The guard has already
            // logged the corrupt cell to stderr by the time we get here.
            System.out.println("  FAIL NPE out of tryLock: " + e.getMessage());
            System.out.println("       -> the fileLockTable field read hit the corrupt-cell guard.");
            System.out.println("       -> re-run with CRATONVM_COMPACT_REF_FIELDS=0 to confirm the layout.");
            fails++;
        }

        System.out.println(fails == 0 ? "PROBE-OK" : "PROBE-FAILURES=" + fails);
        if (fails != 0) {
            System.exit(1);
        }
    }

    private CompactLayoutFileLockProbe() {
    }
}
