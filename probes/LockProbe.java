import java.io.RandomAccessFile;
import java.nio.channels.FileChannel;
import java.nio.channels.FileLock;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.StandardOpenOption;

/**
 * Pure-JDK witness for `FileChannelImpl.tryLock` NPE-ing on a null
 * `FileLockTable`. No H2, no test framework.
 *
 * `tryLock` builds a `FileLockImpl` and then calls `fileLockTable().add(fli)`;
 * `fileLockTable()` is a double-checked lazy init of a `volatile` instance
 * field. If that field reads back null after the init, `flt` is null at the
 * `add` and the NPE names the field. Each step is separate so the report says
 * which one broke.
 */
public class LockProbe {

    public static void main(String... args) throws Exception {
        Path f = Files.createTempFile("lockprobe", ".tmp");
        f.toFile().deleteOnExit();
        Files.write(f, new byte[64]);
        System.out.println("file=" + f);

        // 1. Through FileChannel.open, which is what most code uses.
        step("FileChannel.open + tryLock", () -> {
            try (FileChannel ch = FileChannel.open(f, StandardOpenOption.READ,
                    StandardOpenOption.WRITE)) {
                System.out.println("    channel=" + ch.getClass().getName());
                FileLock lock = ch.tryLock();
                System.out.println("    lock=" + lock);
                if (lock != null) {
                    System.out.println("    valid=" + lock.isValid()
                            + " shared=" + lock.isShared());
                    lock.release();
                }
            }
        });

        // 2. Through RandomAccessFile.getChannel, the route H2 takes.
        step("RandomAccessFile.getChannel + tryLock", () -> {
            try (RandomAccessFile raf = new RandomAccessFile(f.toFile(), "rw")) {
                FileChannel ch = raf.getChannel();
                FileLock lock = ch.tryLock();
                System.out.println("    lock=" + lock);
                if (lock != null) {
                    lock.release();
                }
            }
        });

        // 3. The blocking form, which takes the same fileLockTable() path.
        step("FileChannel.lock()", () -> {
            try (FileChannel ch = FileChannel.open(f, StandardOpenOption.READ,
                    StandardOpenOption.WRITE)) {
                FileLock lock = ch.lock();
                System.out.println("    lock=" + lock);
                if (lock != null) {
                    lock.release();
                }
            }
        });

        // 4. A shared lock on a read-only channel — a different argument path
        //    into the same table.
        step("shared tryLock on a READ channel", () -> {
            try (FileChannel ch = FileChannel.open(f, StandardOpenOption.READ)) {
                FileLock lock = ch.tryLock(0L, Long.MAX_VALUE, true);
                System.out.println("    lock=" + lock);
                if (lock != null) {
                    lock.release();
                }
            }
        });

        Files.deleteIfExists(f);
        System.out.println("=== DONE");
    }

    private interface Step {
        void run() throws Exception;
    }

    private static void step(String label, Step s) {
        try {
            s.run();
            System.out.println("OK   " + label);
        } catch (Throwable t) {
            System.out.println("FAIL " + label + " -> " + t);
            for (StackTraceElement e : t.getStackTrace()) {
                System.out.println("       at " + e);
                if (e.getClassName().startsWith("LockProbe")) {
                    break;
                }
            }
        }
        System.out.flush();
    }
}
