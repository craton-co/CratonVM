/**
 * Concurrent readers against a live `putstatic` — does the inline (helper-free)
 * compiled `getstatic` ever observe a value that was never stored?
 *
 * A compiled `getstatic` loads the field's payload word directly out of the
 * class's statics block with no lock, which is what the `jit_getstatic` helper
 * effectively did too (`StaticsIndex::get` copies the whole 16-byte `Value`
 * unsynchronized). This probe makes that property observable: while a writer
 * thread flips each static between two known values, readers assert that every
 * value they see is one of the two — never a mix, never a torn `long`, never a
 * reference that is neither instance.
 *
 * Usage: StaticRaceProbe [seconds]
 */
public final class StaticRaceProbe {

    static final class Box {
        final int tag;
        Box(int tag) { this.tag = tag; }
    }

    static final Box A = new Box(1);
    static final Box B = new Box(2);

    static final long LONG_A = 0x0123456789ABCDEFL;
    static final long LONG_B = 0x7EDCBA9876543210L;

    static Box sRef = A;
    static long sLong = LONG_A;
    static volatile long sVolLong = LONG_A;
    static int sInt = Integer.MIN_VALUE;
    static volatile int sVol = -1;

    static volatile boolean stop = false;
    static volatile long sink;

    static long readRef(int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) {
            Box b = sRef;
            if (b == null || (b.tag != 1 && b.tag != 2)) {
                throw new IllegalStateException("torn reference static: " + b);
            }
            acc += b.tag;
        }
        return acc;
    }

    static long readLong(int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) {
            long v = sLong;
            if (v != LONG_A && v != LONG_B) {
                throw new IllegalStateException("torn long static: " + Long.toHexString(v));
            }
            long w = sVolLong;
            if (w != LONG_A && w != LONG_B) {
                throw new IllegalStateException("torn VOLATILE long static: " + Long.toHexString(w));
            }
            acc += (v >>> 60) + (w >>> 60);
        }
        return acc;
    }

    static long readInt(int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) {
            int v = sInt;
            if (v != Integer.MIN_VALUE && v != Integer.MAX_VALUE) {
                throw new IllegalStateException("unexpected int static: " + v);
            }
            int w = sVol;
            if (w != -1 && w != 1) {
                throw new IllegalStateException("unexpected volatile static: " + w);
            }
            acc += v + w;
        }
        return acc;
    }

    public static void main(String[] args) throws Exception {
        int seconds = args.length > 0 ? Integer.parseInt(args[0]) : 5;

        // Tier the readers up by INVOCATION count, not by loop iterations, so
        // they run compiled from entry rather than depending on an OSR entry at
        // the loop header. (OSR entry was refused outright until 2026-08-03 --
        // osr-entry-unresumable-exit-FIXED-20260803.md -- which is
        // why the probes here all warm this way.)
        for (int w = 0; w < 1200; w++) {
            sink += readRef(200);
            sink += readLong(200);
            sink += readInt(200);
        }

        Thread writer = new Thread(() -> {
            boolean flip = false;
            while (!stop) {
                flip = !flip;
                sRef = flip ? B : A;
                sLong = flip ? LONG_B : LONG_A;
                sVolLong = flip ? LONG_B : LONG_A;
                sInt = flip ? Integer.MAX_VALUE : Integer.MIN_VALUE;
                sVol = flip ? 1 : -1;
            }
        });
        writer.setDaemon(true);
        writer.start();

        Thread[] readers = new Thread[4];
        final Throwable[] failure = new Throwable[1];
        for (int t = 0; t < readers.length; t++) {
            readers[t] = new Thread(() -> {
                try {
                    long acc = 0;
                    while (!stop) {
                        acc += readRef(10_000);
                        acc += readLong(10_000);
                        acc += readInt(10_000);
                    }
                    sink += acc;
                } catch (Throwable e) {
                    failure[0] = e;
                }
            });
            readers[t].start();
        }

        Thread.sleep(seconds * 1000L);
        stop = true;
        for (Thread t : readers) {
            t.join();
        }

        if (failure[0] != null) {
            System.out.println("FAIL " + failure[0]);
            throw new IllegalStateException("StaticRaceProbe observed a value that was never stored");
        }
        System.out.println("PASS StaticRaceProbe (" + seconds + "s, 4 readers, 1 writer)");
    }
}
