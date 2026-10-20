// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// nio21 wave 1, lane L3: the JIT-readable `Unsafe` arena. A direct buffer's
// `ScopedMemoryAccess` access (base null, address = an arena handle) now folds
// in the optimizing tier and resolves the handle through a lock-free table
// instead of calling the compiled public accessor, its `*Internal` intrinsic
// and the locked `ArenaStore::copy_out`. Switches:
//   CRATONVM_JIT_IR_SCOPED_ARENA_FOLD=0   the nio20 refusal (control arm)
//   CRATONVM_UNSAFE_ARENA_JIT_TABLE=0     legacy handles only, no table
//
//   conf   deterministic text, to diff against HotSpot 25 (both modes):
//          every width / order on direct buffers from compiled kernels, the
//          bounds exceptions from those kernels, slices and duplicates, a
//          confined / shared / global segment's buffer view (closed arenas
//          must still throw), and an allocate-many loop that recycles table
//          entries (more live-and-dead blocks than the table has entries).
//   rows   `<row> <N> ns/op` lines (one op = 16 accesses):
//          directget directput directlong directle segconfined
//          (nio21 wave 2 L3) segglobal segauto: an `Arena.global()` /
//          `Arena.ofAuto()` segment's view, whose session (owner null, never
//          closes) the fold now admits (CRATONVM_JIT_IR_SCOPED_NONCLOSEABLE_MODEL_FOLD,
//          CRATONVM_FFM_SESSION_NONCLOSEABLE_KIND).
//
//   java Nio21ArenaFold conf
//   java -Dnio21.rows=directget,directput,directlong,directle,segconfined,segglobal,segauto Nio21ArenaFold rows

import java.lang.foreign.Arena;
import java.lang.foreign.MemorySegment;
import java.nio.ByteBuffer;
import java.nio.ByteOrder;

public class Nio21ArenaFold {
    static final int REPS = 20_000;

    public static void main(String[] args) {
        String mode = args.length > 0 ? args[0] : "conf";
        if (mode.equals("rows")) {
            rows(System.getProperty("nio21.rows",
                    "directget,directput,directlong,directle,segconfined,segglobal,segauto"));
        } else {
            conf();
        }
    }

    // ------------------------------------------------------------- kernels

    static int getInts(ByteBuffer b) {
        int h = 0;
        for (int k = 0; k < 16; k++) h = 31 * h + b.getInt(4 * k);
        return h;
    }

    static int putInts(ByteBuffer b, int s) {
        for (int k = 0; k < 16; k++) b.putInt(4 * k, s + k);
        return b.getInt(60);
    }

    static long longs(ByteBuffer b, long s) {
        long h = 0;
        for (int k = 0; k < 8; k++) {
            b.putLong(8 * k, s + k);
            h = 31 * h + b.getLong(8 * k);
        }
        return h;
    }

    static int narrow(ByteBuffer b, int s) {
        int h = 0;
        for (int k = 0; k < 8; k++) {
            b.putShort(2 * k, (short) (s * 7 + k));
            b.putChar(16 + 2 * k, (char) (s * 13 + k));
            b.put(32 + k, (byte) (s + k));
            h = 31 * h + b.getShort(2 * k) + b.getChar(16 + 2 * k) + b.get(32 + k);
        }
        return h;
    }

    static int at(ByteBuffer b, int i) {
        return b.getInt(i);
    }

    // Separate copies for the bench rows, so each row's sites carry their own
    // receiver profile.
    static int getIntsRow(ByteBuffer b) { int h = 0; for (int k = 0; k < 16; k++) h = 31 * h + b.getInt(4 * k); return h; }
    static int putIntsRow(ByteBuffer b, int s) { for (int k = 0; k < 16; k++) b.putInt(4 * k, s + k); return s; }
    static long longsRow(ByteBuffer b, long s) { long h = 0; for (int k = 0; k < 8; k++) { b.putLong(8 * k, s + k); h += b.getLong(8 * k); } return h; }
    static int getIntsLe(ByteBuffer b) { int h = 0; for (int k = 0; k < 16; k++) h = 31 * h + b.getInt(4 * k); return h; }
    static int getIntsSeg(ByteBuffer b) { int h = 0; for (int k = 0; k < 16; k++) h = 31 * h + b.getInt(4 * k); return h; }
    static int getIntsGlobal(ByteBuffer b) { int h = 0; for (int k = 0; k < 16; k++) h = 31 * h + b.getInt(4 * k); return h; }
    static int getIntsAuto(ByteBuffer b) { int h = 0; for (int k = 0; k < 16; k++) h = 31 * h + b.getInt(4 * k); return h; }

    // ---------------------------------------------------------------- conf

    static ByteBuffer filled(ByteBuffer b) {
        for (int i = 0; i < b.capacity(); i++) b.put(i, (byte) (i * 7 + 3));
        return b;
    }

    static String exc(Runnable r) {
        try {
            r.run();
            return "no exception";
        } catch (RuntimeException e) {
            return e.getClass().getName() + ": " + e.getMessage();
        }
    }

    static void conf() {
        ByteBuffer be = filled(ByteBuffer.allocateDirect(128));
        ByteBuffer le = filled(ByteBuffer.allocateDirect(128)).order(ByteOrder.LITTLE_ENDIAN);

        // 1. Warm every kernel on direct buffers of both orders.
        long h = 0;
        for (int i = 0; i < REPS; i++) {
            ByteBuffer b = (i & 1) == 0 ? be : le;
            h = h * 31 + getInts(b);
            h = h * 31 + putInts(b, i);
            h = h * 31 + longs(b, i);
            h = h * 31 + narrow(b, i);
            h = h * 31 + at(b, (i * 4) & 63);
        }
        System.out.println("direct warm " + h);
        System.out.println("direct bytes " + getInts(be) + " " + getInts(le));

        // 2. Bounds from the compiled sites.
        System.out.println("tail int " + exc(() -> at(be, 125)));
        System.out.println("last int " + at(be, 124));
        System.out.println("neg int " + exc(() -> at(be, -1)));
        ByteBuffer small = filled(ByteBuffer.allocateDirect(16));
        System.out.println("small get " + exc(() -> getInts(small)));
        System.out.println("small put " + exc(() -> putInts(small, 1)));
        ByteBuffer empty = ByteBuffer.allocateDirect(0);
        System.out.println("empty get " + exc(() -> at(empty, 0)));

        // 3. Slices and duplicates address inside the parent's block.
        ByteBuffer slice = be.position(64).slice();
        be.position(0);
        ByteBuffer dup = le.duplicate().order(ByteOrder.LITTLE_ENDIAN);
        System.out.println("slice " + getInts(slice) + " " + exc(() -> at(slice, 61)));
        System.out.println("dup " + getInts(dup) + " " + putInts(dup, 9) + " " + getInts(le));
        ByteBuffer ro = be.asReadOnlyBuffer();
        System.out.println("ro " + getInts(ro) + " " + exc(() -> putInts(ro, 1)));

        // 4. Segment views: a confined arena (owner = this thread), a shared one
        // and the global one; a closed arena still throws from the compiled site.
        Arena confined = Arena.ofConfined();
        ByteBuffer cv = filled(confined.allocate(64).asByteBuffer());
        Arena shared = Arena.ofShared();
        ByteBuffer sv = filled(shared.allocate(64).asByteBuffer());
        ByteBuffer gv = filled(Arena.global().allocate(64).asByteBuffer());
        long g = 0;
        for (int i = 0; i < REPS; i++) {
            g = g * 31 + getInts(cv) + putInts(cv, i);
            g = g * 31 + getInts(sv) + putInts(sv, i);
            g = g * 31 + getInts(gv) + putInts(gv, i);
        }
        System.out.println("segments " + g);
        confined.close();
        shared.close();
        System.out.println("confined closed " + exc(() -> getInts(cv)));
        System.out.println("confined closed put " + exc(() -> putInts(cv, 1)));
        System.out.println("shared closed " + exc(() -> getInts(sv)));
        System.out.println("global " + getInts(gv));

        // 4b. (nio21 wave 2 L3) An automatic arena's view, and the two
        // never-closing arenas refusing a close (HotSpot's message), after
        // which both views still read.
        Arena auto = Arena.ofAuto();
        ByteBuffer av = filled(auto.allocate(64).asByteBuffer());
        long a = 0;
        for (int i = 0; i < REPS; i++) a = a * 31 + getInts(av) + putInts(av, i);
        System.out.println("auto " + a);
        System.out.println("auto close " + exc(auto::close));
        System.out.println("global close " + exc(() -> Arena.global().close()));
        System.out.println("auto alive " + auto.scope().isAlive() + " "
                + Arena.global().scope().isAlive() + " " + getInts(av) + " " + getInts(gv));

        // 5. Many blocks: more allocations than the JIT table has entries
        // (32768), each touched by the compiled kernels, most of them dead by
        // the time the next is made.
        long m = 0;
        ByteBuffer keep = null;
        for (int i = 0; i < 70_000; i++) {
            ByteBuffer b = ByteBuffer.allocateDirect(64);
            m = m * 31 + putInts(b, i) + getInts(b);
            if (i % 1000 == 0) keep = b;
        }
        System.out.println("many " + m + " " + getInts(keep));

        // 6. A big block (past one handle window's worth of small ones).
        ByteBuffer big = ByteBuffer.allocateDirect(1 << 20);
        long bh = 0;
        for (int i = 0; i < 4096; i++) {
            big.putInt((i * 256) & ((1 << 20) - 4), i);
            bh = bh * 31 + big.getInt((i * 256) & ((1 << 20) - 4));
        }
        System.out.println("big " + bh + " " + exc(() -> at(big, (1 << 20) - 3)));
    }

    // ---------------------------------------------------------------- rows

    static void rows(String which) {
        ByteBuffer direct = filled(ByteBuffer.allocateDirect(64));
        ByteBuffer directLe = filled(ByteBuffer.allocateDirect(64)).order(ByteOrder.LITTLE_ENDIAN);
        Arena arena = Arena.ofConfined();
        ByteBuffer seg = filled(arena.allocate(64).asByteBuffer());
        ByteBuffer segGlobal = filled(Arena.global().allocate(64).asByteBuffer());
        Arena auto = Arena.ofAuto();
        ByteBuffer segAuto = filled(auto.allocate(64).asByteBuffer());
        long sink = 0;
        for (String row : which.split(",")) {
            int n = 2_000_000;
            long best = Long.MAX_VALUE;
            for (int rep = 0; rep < 5; rep++) {
                long t0 = System.nanoTime();
                switch (row) {
                    case "directget" -> { for (int i = 0; i < n; i++) sink += getIntsRow(direct); }
                    case "directput" -> { for (int i = 0; i < n; i++) sink += putIntsRow(direct, i); }
                    case "directlong" -> { for (int i = 0; i < n; i++) sink += longsRow(direct, i); }
                    case "directle" -> { for (int i = 0; i < n; i++) sink += getIntsLe(directLe); }
                    case "segconfined" -> { for (int i = 0; i < n; i++) sink += getIntsSeg(seg); }
                    case "segglobal" -> { for (int i = 0; i < n; i++) sink += getIntsGlobal(segGlobal); }
                    case "segauto" -> { for (int i = 0; i < n; i++) sink += getIntsAuto(segAuto); }
                    default -> throw new IllegalArgumentException(row);
                }
                best = Math.min(best, System.nanoTime() - t0);
            }
            System.out.println(row + " " + (best / n) + " ns/op");
        }
        arena.close();
        System.out.println("sink " + (sink & 0xff) + " " + auto.scope().isAlive());
    }
}
