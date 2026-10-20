// gci wave i9, lane hj: the -XX:+HeapDumpOnOutOfMemoryError dump while another
// thread asks for full collections back to back
// (docs/known-issues/gc/gci-i8hd-jvmti-tag-functions-and-oome-dump-outside-a-pause-20261013.md,
// item 3).
//
// CratonVM's OOME dump asks for its own non-moving pause; when another
// stop-the-world held the world the request was refused and the dump walked
// the heap UNPAUSED at once. The fix waits each refusal out GC-safe and asks
// again (1 s bound) before that fallback.
//
// 1000 `HeldI` objects (fresh, so young at first, then moved by the
// collections) are kept in a static array; the main thread fills the heap with
// 512 KiB arrays until OutOfMemoryError while `gcloop` calls System.gc(). The
// dump is then read back with GciHd4FrozenPeerDumpProbe's HPROF reader (same
// class path): it must parse, and name exactly 1000 HeldI instances.
//
//   java     -XX:+UseSerialGC -Xmx128m -XX:+HeapDumpOnOutOfMemoryError -XX:HeapDumpPath=<dir> \
//            -cp <classes> GciHj9OomeDumpUnderGcProbe <dir>
//   cratonvm -Xmx128m -XX:+HeapDumpOnOutOfMemoryError -XX:HeapDumpPath=<dir> -cp <classes> GciHj9OomeDumpUnderGcProbe <dir>
//
// Expected stdout (HotSpot 25), after the VM's own lines
// (`java.lang.OutOfMemoryError: ...`, `Dumping heap to ...`, `Heap dump file created [...]`):
//
//   dump oome caught
//   dump parsed true
//   heldI instances 1000
//   heldC instances 0
//   values 1000 sum 499500
//
// stderr (diagnostics only): `[hj9o] gcs=<System.gc calls> path <dump>`.
import java.io.IOException;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicLong;

public class GciHj9OomeDumpUnderGcProbe {
    /** The class the HPROF reader counts (its name ends in `$HeldI`). */
    static final class HeldI {
        final long v;

        HeldI(long v) {
            this.v = v;
        }
    }

    static final HeldI[] KEEP = new HeldI[1000];
    static List<long[]> hog;

    public static void main(String[] args) throws Exception {
        for (int i = 0; i < KEEP.length; i++) {
            KEEP[i] = new HeldI(i);
        }
        long pid = ProcessHandle.current().pid();
        Path d = Path.of(args[0]).toAbsolutePath();
        Path dump = Files.isDirectory(d) ? d.resolve("java_pid" + pid + ".hprof") : d;

        AtomicBoolean stop = new AtomicBoolean();
        AtomicLong gcs = new AtomicLong();
        Thread gc = new Thread(() -> {
            while (!stop.get()) {
                System.gc();
                gcs.incrementAndGet();
            }
        }, "gcloop");
        gc.setDaemon(true);
        gc.start();
        while (gcs.get() < 3) {
            Thread.sleep(1);
        }

        hog = new ArrayList<>();
        try {
            while (true) {
                hog.add(new long[1 << 16]);
            }
        } catch (OutOfMemoryError e) {
            hog = null;
        }
        stop.set(true);
        gc.join();
        System.out.println("dump oome caught");

        GciHd4FrozenPeerDumpProbe.Parsed p = new GciHd4FrozenPeerDumpProbe.Parsed();
        try {
            GciHd4FrozenPeerDumpProbe.parse(dump, p);
            System.out.println("dump parsed true");
        } catch (IOException e) {
            System.out.println("dump parsed false");
            System.err.println("[hj9o] parse failed: " + e);
        }
        System.out.println("heldI instances " + p.heldICount);
        System.out.println("heldC instances " + p.heldCCount);
        long sum = 0;
        for (HeldI h : KEEP) {
            sum += h.v;
        }
        System.out.println("values " + KEEP.length + " sum " + sum);
        System.err.println("[hj9o] gcs=" + gcs.get() + " path " + dump);
        Files.deleteIfExists(dump);
    }
}
