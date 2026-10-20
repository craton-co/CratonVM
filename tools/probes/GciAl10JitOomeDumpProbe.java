// gci wave i10, lane al: the -XX:+HeapDumpOnOutOfMemoryError dump raised from
// COMPILED code while another thread asks for full collections back to back
// (docs/known-issues/gc/gci-i10al-oome-dump-gc-safe-slice-re-derives-mut-jvmthread-20261013.md).
//
// GciHj9OomeDumpUnderGcProbe's shape, but the allocation that fails is in a
// small static method (`alloc`) warmed up first, so on CratonVM's JIT build the
// OutOfMemoryError is raised by the JIT allocation helpers' door
// (`jit_alloc_oom`), whose dump retries a refused pause with GC-safe slices on
// the JIT thread's own `&mut JvmThread` (the code lane i10/al re-plumbed).
// With --nojit the interpreter ladder's door dumps instead.
//
// The dump is read back with GciHd4FrozenPeerDumpProbe's HPROF reader (same
// class path): it must parse and name exactly 1000 HeldI instances.
//
//   java     -XX:+UseSerialGC -Xmx128m -XX:+HeapDumpOnOutOfMemoryError -XX:HeapDumpPath=<dir> \
//            -cp <classes> GciAl10JitOomeDumpProbe <dir>
//   cratonvm -Xmx128m -XX:+HeapDumpOnOutOfMemoryError -XX:HeapDumpPath=<dir> -cp <classes> GciAl10JitOomeDumpProbe <dir>
//
// Expected stdout (HotSpot 25), after the VM's own lines
// (`java.lang.OutOfMemoryError: ...`, `Dumping heap to ...`, `Heap dump file created [...]`):
//
//   warm done
//   dump oome caught
//   dump parsed true
//   heldI instances 1000
//   heldC instances 0
//   values 1000 sum 499500
//
// stderr (diagnostics only): `[al10] warm=<n> gcs=<System.gc calls> path <dump>`.
import java.io.IOException;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicLong;

public class GciAl10JitOomeDumpProbe {
    /** The class the HPROF reader counts (its name ends in `$HeldI`). */
    static final class HeldI {
        final long v;

        HeldI(long v) {
            this.v = v;
        }
    }

    static final HeldI[] KEEP = new HeldI[1000];
    static List<long[]> hog;

    /** The allocation site that fails; warmed so the JIT compiles it. */
    static long[] alloc(int n) {
        return new long[n];
    }

    public static void main(String[] args) throws Exception {
        for (int i = 0; i < KEEP.length; i++) {
            KEEP[i] = new HeldI(i);
        }
        long warm = 0;
        for (int i = 0; i < 50_000; i++) {
            warm += alloc(16).length;
        }
        System.out.println("warm done");
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
                hog.add(alloc(1 << 16));
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
            System.err.println("[al10] parse failed: " + e);
        }
        System.out.println("heldI instances " + p.heldICount);
        System.out.println("heldC instances " + p.heldCCount);
        long sum = 0;
        for (HeldI h : KEEP) {
            sum += h.v;
        }
        System.out.println("values " + KEEP.length + " sum " + sum);
        System.err.println("[al10] warm=" + warm + " gcs=" + gcs.get() + " path " + dump);
        Files.deleteIfExists(dump);
    }
}
