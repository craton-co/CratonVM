import java.lang.ref.WeakReference;

/**
 * gci i23/rf: stale heap words in reference-free compiled frames.
 *
 * <p>Each round first runs {@code refDeep}, a recursion that keeps a fresh
 * {@code Box} in every frame and hands each one to a {@code WeakReference}, and
 * then, at the same stack depths, {@code refFree}, an int-only recursion the
 * optimizing tier publishes {@code frame_holds_no_references}. Its frames
 * reuse the stack words {@code refDeep} left behind; at the bottom it calls
 * {@code Bottom.at}, which churns young objects and, every {@code gcEvery}-th
 * round, runs {@code System.gc()} with every {@code refFree} frame still on
 * the stack and counts how many of the round's {@code Box}es (all garbage
 * once {@code refDeep} returned) the collection kept.
 *
 * <p>stdout is one deterministic line (HotSpot-comparable): the arithmetic
 * checksum and {@code liveBad} (a live array's invariant broken after a
 * collection: must be 0). stderr carries the retention measurement:
 * {@code [rf23] gcs=G retained=R of=T maxRetained=M}.
 *
 * <p>Usage: {@code GciRf23RefFreeFrameProbe [rounds=4000] [depth=200] [gcEvery=50]}
 */
public final class GciRf23RefFreeFrameProbe {

    static final class Box {
        final int v;
        final long[] pad;

        Box(int v) {
            this.v = v;
            this.pad = new long[8];
        }
    }

    static final int LIVE = 1024;
    static final long LIVE_SUM = (long) (LIVE - 1) * LIVE / 2;

    static WeakReference<?>[] refs;
    static Box[] live;
    static final byte[][] ring = new byte[64][];
    static int ringPos;
    static int round;
    static int gcEvery;
    static long gcs;
    static long retained;
    static long checked;
    static long maxRetained;
    static long liveBad;

    /** Reference-heavy: a Box per frame, each frame's words name it. */
    static int refDeep(int d, Box up) {
        Box x = new Box(d * 7 + (up.v & 15));
        refs[d] = new WeakReference<>(x);
        if (d == 0) {
            return x.v;
        }
        int r = refDeep(d - 1, x);
        return r + x.v - up.v;
    }

    /** Reference-free: ints only, so its frames hold no reference by contract. */
    static int refFree(int d, int s) {
        if (d == 0) {
            return Bottom.at(s);
        }
        int a = s * 31 + d;
        int r = refFree(d - 1, a);
        return r ^ (a >>> 3) ^ d;
    }

    static void collect() {
        System.gc();
        gcs++;
        int r = 0;
        for (int i = 0; i < refs.length; i++) {
            WeakReference<?> w = refs[i];
            if (w != null && w.get() != null) {
                r++;
            }
        }
        retained += r;
        checked += refs.length;
        if (r > maxRetained) {
            maxRetained = r;
        }
        long sum = 0;
        for (int i = 0; i < live.length; i++) {
            sum += live[i].v;
        }
        if (sum != LIVE_SUM) {
            liveBad++;
        }
    }

    /** Kept above the optimizing tier's inline size (325 bytecode bytes). */
    static final class Bottom {
        static int at(int s) {
            int p = ringPos;
            ring[p & 63] = new byte[1024 + (s & 255)];
            ringPos = p + 1;
            int k = s & (LIVE - 1);
            live[k] = new Box(live[k].v);
            if (gcEvery > 0 && round % gcEvery == 0) {
                collect();
            }
            int h = s;
            switch (s & 31) {
                case 0: h = h * 3 + 0x11; break;
                case 1: h = h * 5 + 0x1234; break;
                case 2: h = h * 7 - 0x2345; break;
                case 3: h = h * 11 + 0x3456; break;
                case 4: h = h * 13 - 0x4567; break;
                case 5: h = h * 17 + 0x5678; break;
                case 6: h = h * 19 - 0x6789; break;
                case 7: h = h * 23 + 0x789a; break;
                case 8: h = h * 29 - 0x89ab; break;
                case 9: h = h * 31 + 0x9abc; break;
                case 10: h = h * 37 - 0xabcd; break;
                case 11: h = h * 41 + 0xbcde; break;
                case 12: h = h * 43 - 0xcdef; break;
                case 13: h = h * 47 + 0xdef0; break;
                case 14: h = h * 53 - 0xef01; break;
                case 15: h = h * 59 + 0xf012; break;
                case 16: h = (h ^ 0x10203) * 61; break;
                case 17: h = (h ^ 0x20304) * 67; break;
                case 18: h = (h ^ 0x30405) * 71; break;
                case 19: h = (h ^ 0x40506) * 73; break;
                case 20: h = (h ^ 0x50607) * 79; break;
                case 21: h = (h ^ 0x60708) * 83; break;
                case 22: h = (h ^ 0x70809) * 89; break;
                case 23: h = (h ^ 0x8090a) * 97; break;
                case 24: h = (h >>> 1) + 0x90a0b; break;
                case 25: h = (h >>> 2) - 0xa0b0c; break;
                case 26: h = (h >>> 3) + 0xb0c0d; break;
                case 27: h = (h >>> 4) - 0xc0d0e; break;
                case 28: h = (h >>> 5) + 0xd0e0f; break;
                case 29: h = (h >>> 6) - 0xe0f10; break;
                case 30: h = (h >>> 7) + 0xf1011; break;
                default: h = (h >>> 8) - 0x101112; break;
            }
            return h;
        }
    }

    static long step(int depth, Box root, int r) {
        int a = refDeep(depth, root);
        int b = refFree(depth, r);
        return (long) a * 3 + b;
    }

    public static void main(String[] args) {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 4000;
        int depth = args.length > 1 ? Integer.parseInt(args[1]) : 200;
        gcEvery = args.length > 2 ? Integer.parseInt(args[2]) : 50;
        refs = new WeakReference<?>[depth + 1];
        live = new Box[LIVE];
        for (int i = 0; i < LIVE; i++) {
            live[i] = new Box(i);
        }
        Box root = new Box(1);
        long sum = 0;
        for (round = 1; round <= rounds; round++) {
            sum += step(depth, root, round);
        }
        System.out.println("RF23 ok rounds=" + rounds + " depth=" + depth + " sum=" + sum
                + " liveBad=" + liveBad);
        System.err.println("[rf23] gcs=" + gcs + " retained=" + retained + " of=" + checked
                + " maxRetained=" + maxRetained);
    }
}
