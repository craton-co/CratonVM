import java.lang.foreign.*;
import java.lang.invoke.VarHandle;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.nio.DoubleBuffer;
import java.nio.FloatBuffer;
import java.nio.channels.FileChannel;
import java.nio.file.*;
import java.util.*;
import java.util.concurrent.atomic.AtomicLong;

/**
 * nio20 wave 5, lane L5: the windowed {@code ScopedMemoryAccess} twins
 * ({@code native-builtins/src/scoped_twins.rs}, {@code CRATONVM_FFM_WINDOWED_SCOPED_TWINS}, default
 * ON since this wave; page
 * {@code r20-shared-close-java-cleanup-actions-race-unwindowed-scoped-bodies-20261007}).
 *
 * <p><b>Race rows</b> (deterministic text, diff against HotSpot 25): per row and round a worker
 * loops one access family on a 1 MiB file mapped into a SHARED arena, then the main thread closes
 * the arena, whose close runs the JDK's unmap cleanup action. HotSpot: the worker always ends with
 * {@code java.lang.IllegalStateException: Already closed} (never a crash, a hang or a different
 * line), and so does the closer's touch afterwards.
 * <ul>
 *   <li>{@code vh}: {@code VarHandle} volatile get/set, {@code getAndAdd}, {@code compareAndSet},
 *       {@code getAndBitwiseOr} over {@code JAVA_LONG} / {@code JAVA_INT} (the twins' CAS /
 *       volatile / getAndOp rows);</li>
 *   <li>{@code fbuf}: a native-order {@code FloatBuffer} and {@code DoubleBuffer} view of the
 *       mapped block ({@code DirectFloatBufferU.get/put}: the raw-address float / double rows);</li>
 *   <li>{@code vhfp}: {@code VarHandle} plain + volatile access over {@code JAVA_FLOAT} /
 *       {@code JAVA_DOUBLE};</li>
 *   <li>{@code mapped}: {@code MemorySegment.isLoaded()/load()/force()} (the mapped-memory rows).</li>
 * </ul>
 * Without the twins a close lands between a body's session check and its access and the unmap
 * faults it (a crash, a {@code TIMEOUT} line or a garbage worker line); with them every round
 * prints the {@code Already closed} line. {@code -Dnio20.rows=a,b} selects race rows (default
 * {@code vh,mapped}; {@code fbuf} and {@code vhfp} are opt-in since the w5 fix-back: the four plain
 * float / double {@code *Internal} rows are deliberately NOT twinned -- a native there cost ~1.8x
 * -- so their race with a cleanup action is still open and those rows may crash on CratonVM); {@code -Dnio20.rounds=N} (default 5); {@code -Dnio20.warm=N}
 * worker iterations before the close (default 100000).
 *
 * <p><b>Bench rows</b> ({@code -Dnio20.bench=vhcas,vhvol,vhadd,fbuf,dbuf,vhfloat,copy,fill}; off
 * by default): {@code <row> <N> ns/op} over a 64 KiB {@code Arena.ofShared()} segment (no close).
 * {@code copy} / {@code fill} (4 KiB each) are the windowed BULK intrinsics and do not move with
 * the twins switch: they are the control rows. {@code -Dnio20.iters=N} (default 2000000; bulk rows
 * use a tenth), {@code -Dnio20.reps=N} (default 5; the median is printed).
 *
 * <p>A/B: {@code CRATONVM_FFM_WINDOWED_SCOPED_TWINS=0} (the old bytecode bodies, racy) against
 * unset (default ON). Expected: race rows match HotSpot every round only with the twins ON;
 * {@code vhcas vhvol vhadd} cost one native round trip plus an interpreted body more with them ON;
 * {@code fbuf dbuf} must not move (those rows are not twinned); {@code copy fill} unchanged.
 */
public class Nio20ScopedTwins {
    static final String ROWS = System.getProperty("nio20.rows", "vh,mapped");
    static final String BENCH = System.getProperty("nio20.bench", "");
    static final int ROUNDS = Integer.getInteger("nio20.rounds", 5);
    static final long WARM = Long.getLong("nio20.warm", 100_000L);
    static final long ITERS = Long.getLong("nio20.iters", 2_000_000L);
    static final int REPS = Integer.getInteger("nio20.reps", 5);
    static final long SIZE = 1L << 20;

    static final VarHandle VH_LONG = ValueLayout.JAVA_LONG.varHandle();
    static final VarHandle VH_INT = ValueLayout.JAVA_INT.varHandle();
    static final VarHandle VH_FLOAT = ValueLayout.JAVA_FLOAT.varHandle();
    static final VarHandle VH_DOUBLE = ValueLayout.JAVA_DOUBLE.varHandle();

    static volatile long sink;

    interface Step { void run(long i) throws Throwable; }

    static final class Worker extends Thread {
        final Step step;
        final AtomicLong iters = new AtomicLong();
        volatile String outcome;

        Worker(String name, Step step) {
            super(name);
            this.step = step;
            setDaemon(true);
        }

        @Override
        public void run() {
            long i = 0;
            try {
                while (true) {
                    step.run(i);
                    i++;
                    if ((i & 1023) == 0) {
                        iters.set(i);
                    }
                }
            } catch (Throwable t) {
                outcome = ex(t);
            }
        }
    }

    static String ex(Throwable t) {
        return t.getClass().getName() + ": " + t.getMessage();
    }

    /** Spread the accesses over the whole block, so the unmap is hit wherever the close lands. */
    static long slot(long i) {
        return ((i * 4099) & ((SIZE / 8) - 1)) * 8;
    }

    static Step stepFor(String row, MemorySegment seg) {
        switch (row) {
            case "vh":
                return i -> {
                    long off = slot(i);
                    VH_LONG.setVolatile(seg, off, i);
                    sink += (long) VH_LONG.getVolatile(seg, off);
                    sink += (long) VH_LONG.getAndAdd(seg, off, 3L);
                    sink += ((boolean) VH_LONG.compareAndSet(seg, off, i + 3, i)) ? 1 : 0;
                    sink += (int) VH_INT.getAndBitwiseOr(seg, off, 1);
                };
            case "fbuf": {
                ByteBuffer bb = seg.asByteBuffer().order(ByteOrder.nativeOrder());
                FloatBuffer fb = bb.asFloatBuffer();
                DoubleBuffer db = bb.asDoubleBuffer();
                return i -> {
                    int ix = (int) (slot(i) / 8);
                    fb.put(ix * 2, (float) i);
                    db.put(ix, (double) i);
                    sink += (long) (fb.get(ix * 2) + db.get(ix));
                };
            }
            case "vhfp":
                return i -> {
                    long off = slot(i);
                    VH_FLOAT.set(seg, off, (float) i);
                    sink += (long) (float) VH_FLOAT.get(seg, off);
                    VH_DOUBLE.setVolatile(seg, off, (double) i);
                    sink += (long) (double) VH_DOUBLE.getVolatile(seg, off);
                };
            case "mapped":
                return i -> {
                    sink += seg.isLoaded() ? 1 : 0;
                    if ((i & 15) == 0) {
                        seg.load();
                    }
                    if ((i & 63) == 0) {
                        seg.force();
                    }
                };
            default:
                throw new IllegalArgumentException(row);
        }
    }

    static void round(String row, int round) throws Throwable {
        Arena arena = Arena.ofShared();
        Path file = Files.createTempFile("nio20twins", ".bin");
        FileChannel fc = FileChannel.open(file, StandardOpenOption.READ, StandardOpenOption.WRITE);
        MemorySegment seg = fc.map(FileChannel.MapMode.READ_WRITE, 0, SIZE, arena);
        Worker w = new Worker("nio20-twins-" + row + "-" + round, stepFor(row, seg));
        w.start();
        long deadline = System.nanoTime() + 60_000_000_000L;
        long warm = row.equals("mapped") ? WARM / 50 : WARM;
        while (w.iters.get() < warm && w.isAlive() && System.nanoTime() < deadline) {
            Thread.onSpinWait();
        }
        String closeOutcome = "ok";
        try {
            arena.close();
        } catch (Throwable t) {
            closeOutcome = ex(t);
        }
        w.join(60_000);
        String outcome = w.isAlive() ? "TIMEOUT (worker never saw the close)" : w.outcome;
        String after;
        try {
            sink += (long) VH_LONG.getVolatile(seg, 0L);
            after = "read succeeded";
        } catch (Throwable t) {
            after = ex(t);
        }
        System.out.println(row + " round " + round + ": close " + closeOutcome
                + "; worker " + outcome + "; closer touch " + after
                + "; arena alive " + arena.scope().isAlive());
        fc.close();
        Files.deleteIfExists(file);
    }

    // ------------------------------------------------------------------ bench

    static long benchOnce(String row, MemorySegment seg, long iters) {
        long t0 = System.nanoTime();
        switch (row) {
            case "vhcas":
                for (long i = 0; i < iters; i++) {
                    long off = (i & 1023) * 8;
                    long v = (long) VH_LONG.getVolatile(seg, off);
                    sink += ((boolean) VH_LONG.compareAndSet(seg, off, v, v + 1)) ? 1 : 0;
                }
                break;
            case "vhvol":
                for (long i = 0; i < iters; i++) {
                    long off = (i & 1023) * 8;
                    VH_LONG.setVolatile(seg, off, i);
                    sink += (long) VH_LONG.getVolatile(seg, off);
                }
                break;
            case "vhadd":
                for (long i = 0; i < iters; i++) {
                    sink += (int) VH_INT.getAndAdd(seg, (i & 1023) * 4, 1);
                }
                break;
            case "fbuf": {
                FloatBuffer fb = seg.asByteBuffer().order(ByteOrder.nativeOrder()).asFloatBuffer();
                float acc = 0;
                for (long i = 0; i < iters; i++) {
                    int ix = (int) (i & 4095);
                    fb.put(ix, acc);
                    acc += fb.get(ix) + 1.0f;
                }
                sink += (long) acc;
                break;
            }
            case "dbuf": {
                DoubleBuffer db = seg.asByteBuffer().order(ByteOrder.nativeOrder()).asDoubleBuffer();
                double acc = 0;
                for (long i = 0; i < iters; i++) {
                    int ix = (int) (i & 2047);
                    db.put(ix, acc);
                    acc += db.get(ix) + 1.0;
                }
                sink += (long) acc;
                break;
            }
            case "vhfloat": {
                float acc = 0;
                for (long i = 0; i < iters; i++) {
                    long off = (i & 1023) * 4;
                    VH_FLOAT.set(seg, off, acc);
                    acc += (float) VH_FLOAT.get(seg, off) + 1.0f;
                }
                sink += (long) acc;
                break;
            }
            case "copy":
                for (long i = 0; i < iters; i++) {
                    MemorySegment.copy(seg, 0, seg, 32768, 4096);
                }
                break;
            case "fill":
                for (long i = 0; i < iters; i++) {
                    seg.asSlice(8192, 4096).fill((byte) i);
                }
                break;
            default:
                throw new IllegalArgumentException(row);
        }
        return System.nanoTime() - t0;
    }

    static void bench(String row) {
        long iters = (row.equals("copy") || row.equals("fill")) ? Math.max(1, ITERS / 10) : ITERS;
        try (Arena arena = Arena.ofShared()) {
            MemorySegment seg = arena.allocate(65536, 64);
            benchOnce(row, seg, iters); // warm-up
            long[] ns = new long[REPS];
            for (int r = 0; r < REPS; r++) {
                ns[r] = benchOnce(row, seg, iters);
            }
            Arrays.sort(ns);
            System.out.println(row + " " + (ns[REPS / 2] / iters) + " ns/op");
        }
    }

    public static void main(String[] args) throws Throwable {
        if (!BENCH.isEmpty()) {
            for (String row : BENCH.split(",")) {
                bench(row);
            }
            return;
        }
        for (String row : ROWS.split(",")) {
            for (int r = 0; r < ROUNDS; r++) {
                try {
                    round(row, r);
                } catch (Throwable t) {
                    System.out.println(row + " round " + r + " THREW " + ex(t));
                }
            }
        }
        System.out.println("done");
    }
}
