import java.io.RandomAccessFile;
import java.nio.channels.FileChannel;
import java.nio.channels.FileLock;
import java.nio.file.Files;
import java.nio.file.Path;

/**
 * Pure-JDK witness for `FileChannelImpl.tryLock` answering
 * `NullPointerException: … because "flt" is null`.
 *
 * `sun.nio.ch.FileChannelImpl.fileLockTable()` (JDK 25, line 1651) is a
 * double-checked lazy init that ASSIGNS the field and then returns it:
 *
 *     private volatile FileLockTable fileLockTable;
 *     private FileLockTable fileLockTable() throws IOException {
 *         if (fileLockTable == null) {
 *             synchronized (this) {
 *                 if (fileLockTable == null) {
 *                     fileLockTable = new FileLockTable(this, fd);
 *                 } ...
 *         return fileLockTable;
 *     }
 *
 * so a null return means the write and the read did not land on the same slot,
 * or the write was dropped — a field-layout symptom, not a locking one.
 */
public final class FileLockTableProbe {

    public static void main(String[] args) throws Exception {
        Path f = Files.createTempFile("flt", ".bin");
        Files.write(f, new byte[] { 1, 2, 3, 4 });
        try (RandomAccessFile raf = new RandomAccessFile(f.toFile(), "rw");
             FileChannel ch = raf.getChannel()) {
            System.out.println("channel=" + ch.getClass().getName());
            FileLock lock = ch.tryLock();
            System.out.println("tryLock -> " + (lock == null ? "null (already held)" : "held"));
            if (lock != null) {
                System.out.println("  valid=" + lock.isValid()
                        + " shared=" + lock.isShared()
                        + " position=" + lock.position());
                lock.release();
                System.out.println("  released, valid=" + lock.isValid());
            }
            // A second acquire after release must succeed — this is the
            // close/reopen path H2's SingleFileStore depends on.
            FileLock again = ch.tryLock();
            System.out.println("tryLock again -> " + (again == null ? "null" : "held"));
            if (again != null) {
                again.release();
            }
        } catch (Throwable t) {
            System.out.println("FAILED: " + t);
            t.printStackTrace(System.out);
        } finally {
            Files.deleteIfExists(f);
        }
        System.out.println("DONE");
    }

    private FileLockTableProbe() {
    }
}
