import java.lang.foreign.*;
import java.nio.ByteBuffer;
import java.nio.channels.FileChannel;
import java.nio.file.*;
import java.util.*;
import java.util.concurrent.atomic.AtomicLong;

/**
 * nio20 wave 2, lane L1: a shared {@code Arena.close()} racing a compiled scoped access
 * ({@code r20-shared-arena-close-does-not-wait-for-compiled-scoped-access-bodies-20261007}).
 *
 * <p>Per row and round: a worker thread loops one segment operation on a shared-arena segment
 * (long enough for the JIT to compile the loop and the scoped bodies under it), then the main
 * thread closes the arena. HotSpot: the worker always ends with
 * {@code java.lang.IllegalStateException: Already closed}, never a crash, garbage or a hang; a
 * touch from the closer after {@code close()} is the same exception. Every line is deterministic
 * (no counts, no timings), so the output diffs against HotSpot directly.
 *
 * <p>Rows: getput (VarHandle element accessors), copy (native to native), heapcopy (heap to
 * native), fill, mismatch, buffer (an {@code asByteBuffer()} view). Opt-in rows (nio20 wave 3,
 * lane L5, {@code r20-shared-close-java-cleanup-actions-race-unwindowed-scoped-bodies}):
 * {@code mapped} (a 1 MiB file mapped into the shared arena, whose close runs the JDK's unmap
 * cleanup action) and {@code cleanup} (a 1 MiB {@code malloc} block reinterpreted into the shared
 * arena with a {@code free} cleanup action; needs native access, the restricted-method warning
 * goes to stderr). Both drive the BULK scoped bodies over the whole block, round-robin --
 * {@code copyMemory}, {@code setMemory} (fill), {@code vectorizedMismatch}, {@code copySwapMemory}
 * (a copy between opposite byte orders) -- each 256 KiB long, so a close lands inside one far
 * more often than between two. Without the bulk bodies' close window the cleanup action unmaps
 * or frees the block under a running copy (a crash, a {@code TIMEOUT} line or a different worker
 * line); with it every round prints HotSpot's {@code Already closed} line.
 * {@code -Dnio20.rows=a,b} selects rows; {@code -Dnio20.rounds=N} (default 3);
 * {@code -Dnio20.warm=N} worker iterations before the close (default 200000; the two bulk rows
 * use {@code -Dnio20.bulkwarm=N}, default 2048).
 */
public class Nio20ArenaClose {
    static final String ROWS = System.getProperty("nio20.rows", "getput,copy,heapcopy,fill,mismatch,buffer");
    static final int ROUNDS = Integer.getInteger("nio20.rounds", 3);
    static final long WARM = Long.getLong("nio20.warm", 200_000L);
    static final long BULK_WARM = Long.getLong("nio20.bulkwarm", 2048L);
    static final long SIZE = 4096;
    static final long BULK_SIZE = 1L << 20;
    static final long BULK_SPAN = 256L * 1024;

    static boolean isBulk(String row) {
        return row.equals("mapped") || row.equals("cleanup");
    }

    /** One bulk scoped body over a 256 KiB span of {@code seg}, chosen by {@code i}. */
    static void bulkStep(MemorySegment seg, long i) {
        switch ((int) (i & 3)) {
            case 0:
                MemorySegment.copy(seg, 0, seg, BULK_SPAN * 2, BULK_SPAN);
                break;
            case 1:
                seg.asSlice(BULK_SPAN, BULK_SPAN).fill((byte) 5);
                break;
            case 2:
                sink += seg.asSlice(0, BULK_SPAN).mismatch(seg.asSlice(BULK_SPAN * 2, BULK_SPAN));
                break;
            default:
                MemorySegment.copy(seg, ValueLayout.JAVA_INT_UNALIGNED.withOrder(java.nio.ByteOrder.BIG_ENDIAN), 0,
                        seg, ValueLayout.JAVA_INT_UNALIGNED.withOrder(java.nio.ByteOrder.LITTLE_ENDIAN),
                        BULK_SPAN * 3, BULK_SPAN / 4);
                break;
        }
    }

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
                outcome = t.getClass().getName() + ": " + t.getMessage();
            }
        }
    }

    static String ex(Throwable t) {
        return t.getClass().getName() + ": " + t.getMessage();
    }

    static Step stepFor(String row, MemorySegment seg, ByteBuffer bb, MemorySegment heap) {
        switch (row) {
            case "getput":
                return i -> {
                    long ix = i & 255;
                    seg.setAtIndex(ValueLayout.JAVA_INT, ix, (int) i);
                    sink += seg.getAtIndex(ValueLayout.JAVA_INT, ix);
                    sink += seg.get(ValueLayout.JAVA_LONG, (i & 63) * 8);
                };
            case "copy":
                return i -> MemorySegment.copy(seg, 0, seg, 2048, 1024);
            case "heapcopy":
                return i -> {
                    MemorySegment.copy(heap, 0, seg, 0, 512);
                    MemorySegment.copy(seg, 1024, heap, 0, 512);
                };
            case "fill":
                return i -> seg.asSlice(0, 1024).fill((byte) i);
            case "mismatch":
                return i -> sink += seg.asSlice(0, 1024).mismatch(seg.asSlice(2048, 1024));
            case "buffer":
                return i -> {
                    int ix = (int) (i & 255) * 4;
                    bb.putInt(ix, (int) i);
                    sink += bb.getInt(ix);
                };
            case "mapped":
            case "cleanup":
                return i -> bulkStep(seg, i);
            default:
                throw new IllegalArgumentException(row);
        }
    }

    static void round(String row, int round) throws Throwable {
        Arena arena = Arena.ofShared();
        MemorySegment seg;
        FileChannel fc = null;
        Path file = null;
        if (row.equals("mapped")) {
            file = Files.createTempFile("nio20arena", ".bin");
            fc = FileChannel.open(file, StandardOpenOption.READ, StandardOpenOption.WRITE);
            seg = fc.map(FileChannel.MapMode.READ_WRITE, 0, BULK_SIZE, arena);
        } else if (row.equals("cleanup")) {
            seg = mallocInto(arena);
        } else {
            seg = arena.allocate(SIZE, 16);
        }
        ByteBuffer bb = row.equals("buffer") ? seg.asByteBuffer() : null;
        MemorySegment heap = MemorySegment.ofArray(new byte[1024]);
        Worker w = new Worker("nio20-" + row + "-" + round, stepFor(row, seg, bb, heap));
        w.start();
        long deadline = System.nanoTime() + 60_000_000_000L;
        long warm = isBulk(row) ? BULK_WARM : WARM;
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
            sink += seg.get(ValueLayout.JAVA_BYTE, 0);
            after = "read succeeded";
        } catch (Throwable t) {
            after = ex(t);
        }
        System.out.println(row + " round " + round + ": close " + closeOutcome
                + "; worker " + outcome + "; closer touch " + after
                + "; arena alive " + arena.scope().isAlive());
        if (fc != null) {
            fc.close();
            Files.deleteIfExists(file);
        }
    }

    /**
     * A 1 MiB {@code malloc} block (glibc serves that size with its own mapping, so the
     * {@code free} cleanup unmaps it) owned by {@code arena}, freed by the arena's close.
     */
    static MemorySegment mallocInto(Arena arena) throws Throwable {
        Linker linker = Linker.nativeLinker();
        java.lang.invoke.MethodHandle malloc = linker.downcallHandle(
                linker.defaultLookup().find("malloc").orElseThrow(),
                FunctionDescriptor.of(ValueLayout.ADDRESS, ValueLayout.JAVA_LONG));
        java.lang.invoke.MethodHandle free = linker.downcallHandle(
                linker.defaultLookup().find("free").orElseThrow(),
                FunctionDescriptor.ofVoid(ValueLayout.ADDRESS));
        MemorySegment raw = (MemorySegment) malloc.invokeExact(BULK_SIZE);
        return raw.reinterpret(BULK_SIZE, arena, s -> {
            try {
                free.invokeExact(s);
            } catch (Throwable t) {
                throw new RuntimeException(t);
            }
        });
    }

    public static void main(String[] args) throws Throwable {
        List<String> rows = Arrays.asList(ROWS.split(","));
        for (String row : rows) {
            for (int r = 0; r < ROUNDS; r++) {
                try {
                    round(row, r);
                } catch (Throwable t) {
                    System.out.println(row + " round " + r + " THREW " + ex(t));
                }
            }
        }
        // The deferred free: closed sessions die, a collection frees their blocks, and fresh
        // shared arenas still read back what they wrote.
        for (int i = 0; i < 3; i++) {
            System.gc();
        }
        boolean fresh = true;
        for (int i = 0; i < 64; i++) {
            try (Arena a = Arena.ofShared()) {
                MemorySegment s = a.allocate(SIZE, 16);
                s.fill((byte) 7);
                for (long k = 0; k < SIZE; k += 512) {
                    fresh &= s.get(ValueLayout.JAVA_BYTE, k) == 7;
                }
            }
        }
        System.out.println("fresh arenas after collection read back: " + fresh);
        System.out.println("done");
    }
}
