// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// JIT round 21 wave 3, lane barriers: GC barriers of compiled reference stores,
// every store form, under collection pressure, verified by content.
//
// Old holders (allocated first, then four System.gc() calls so they are old on
// every generational collector) receive pointers to FRESH (young) payload
// nodes from hot, compiled methods, one row per store form:
//
//   putfield      h.f0..f3 = n                         (putfield, compact cell)
//   aastore       h.arr[s] = n                         (aastore into an old array)
//   putstatic     St.s0..s3 = n                        (putstatic: statics block)
//   unsafe-put    Unsafe.putReference field / element
//   unsafe-putvol Unsafe.putReferenceVolatile / putReferenceRelease
//   unsafe-cas    Unsafe.compareAndSetReference field / element
//   unsafe-gas    Unsafe.getAndSetReference
//   vh-field      VarHandle set / setVolatile / setRelease / compareAndSet / getAndSet
//   vh-array      arrayElementVarHandle(Node[].class) setVolatile / compareAndSet
//   vh-static     static VarHandle setVolatile / compareAndSet, AtomicReference.set
//   updater       AtomicReferenceFieldUpdater set / lazySet / compareAndSet / getAndSet
//   atomic-array  AtomicReferenceArray set / lazySet / compareAndSet
//   arraycopy     System.arraycopy young Node[] -> old Node[], and within the old array
//   clone         Node[].clone() of an old array, published; LARGE clone / Arrays.copyOf
//   fresh-large   new Node[LARGE] (old/humongous on G1 and Generational) filled in a loop
//   shuffle       SATB: b.f1 = a.f0; a.f0 = fresh (and the array twin)
//   ctor          new Pair(fresh, old-or-fresh) published (constructor reference stores)
//   loop          call-free counted loops of aastores (32 slots; and 2^18 slots into an
//                 old array: the back-edge poll and every element's card)
//
// while a churn thread allocates (old ring -> young arrays) and calls
// System.gc(), and the main thread calls System.gc() every third round. After
// every round every row is walked; each reachable node is checked against its
// own invariant (sq == mix(id), data[k] == id*31+k) and folded into a
// position-sensitive checksum. A lost card / remembered-set edge / SATB log
// shows as a freed or moved node read through a stale slot: bad > 0, a changed
// sum, or a crash.
//
// Deterministic facts: the `row <name> live=<n> sum=<hex> bad=<b>` lines and
// `total bad=0`. They depend only on the program (one writer; the churn thread
// never touches the rows), not on when or how often a collection runs.
//
// Run (javac needs the export for jdk.internal.misc.Unsafe):
//   javac --add-exports java.base/jdk.internal.misc=ALL-UNNAMED -d out R21BarrierRows.java
//   X="--add-exports java.base/jdk.internal.misc=ALL-UNNAMED -Xmx512m -cp out"
//   java $X -XX:+UseZGC R21BarrierRows                    # HotSpot 25 (generational ZGC)
//   java $X -XX:+UseG1GC R21BarrierRows                   # HotSpot 25
//   java $X -XX:+UseSerialGC R21BarrierRows               # HotSpot 25, the generational control
//   cratonvm $X -XX:+UseZGC R21BarrierRows                # default collector
//   cratonvm $X -XX:+UseG1GC R21BarrierRows
//   cratonvm $X -XX:+UseGenerationalGC R21BarrierRows
//   cratonvm --nojit $X -XX:+UseGenerationalGC R21BarrierRows   # facts only (interpreter)
//   cratonvm --compatible $X -XX:+UseGenerationalGC R21BarrierRows
// Arms worth one run each (same expected lines):
//   CRATONVM_JIT_LEAN_CARD_POST=1            (Generational; opt-in lean POST card check)
//   CRATONVM_JIT_UNSAFE_REF_VOLATILE_FIELD=1 (Generational / G1; UPDATERS21-1 arms)
//   CRATONVM_G1_INLINE_BARRIER=1             (G1; F-08 inline post barrier)
//   CRATONVM_ZGC_CONC_START=auto             (ZGC concurrent mark: the SATB pre gate)
//   CRATONVM_ZGC_GENERATIONAL=1              (ZGC generational mode)
//   CRATONVM_GC=verify-rset                  (Generational: every young pause prints
//                                             `[rset-verify] ... missing=0`; read edges>0 too)
//   CRATONVM_JIT_INLINE_CARD_MARK=0 / CRATONVM_JIT_CARD_STORE_BARRIER=0 (the helper arms)
//   CRATONVM_JIT_SP_G1_GATED_AASTORE=1       (G1; BARRIERS21-1, the single-pass G1 aastore)
//   CRATONVM_JIT_IR_G1_GATED_AASTORE=1       (G1; BARRIERS21-2, the optimizing tier's G1 aastore)
// -Dr21.rounds=N (default 24) and -Dr21.ops=N (default 4000) scale the run;
// the expected lines are per (rounds, ops) pair, so compare like with like.
//
// Timing rows (wave 3, for BARRIERS21-1 / -2; -Dr21.time=false skips them).
// They print after the facts, as `time <row> <N> ns/op`: one op is one
// reference aastore, median of 9 reps after 5 warm-up reps. Rows:
//   g1-aastore-cross  young nodes into an OLD Node[] (cross-region on G1:
//                     the remembered-set barrier)
//   g1-aastore-same   young nodes into a young Node[] allocated right after
//                     them (mostly the same G1 region: no barrier owed)
//   g1-aastore-null   null into the old Node[] (no barrier owed)
//   g1-aastore-move   a[j] = a[j+1] inside the old Node[] (old values)
// The A/B the books ask for (interleave the arms; take medians):
//   cratonvm $X -XX:+UseG1GC R21BarrierRows                                  # call arms
//   CRATONVM_JIT_SP_G1_GATED_AASTORE=1 CRATONVM_JIT_IR_G1_GATED_AASTORE=1 \
//     cratonvm $X -XX:+UseG1GC R21BarrierRows                                # inline arms
//   java $X -XX:+UseG1GC R21BarrierRows                                      # HotSpot
// HotSpot G1 expectation: about 1 ns/op for null / same, and a few ns/op for
// cross / move (the card check of its post barrier).
//
// Engagement: RUST_LOG=cratonvm_jit=debug prints `inline generational card
// barrier emitted` per site (Generational); CRATONVM_DBG_JITC=1 prints the
// Unsafe / VarHandle inline arms (`ir-unsafe-ref-cas-inline`, ...) and
// `ir g1 gated aastore (BARRIERS21-2)` per IR site.
//
// HotSpot expectation: every non-`time` line identical on the three HotSpot
// collectors, every CratonVM collector, arm and --nojit; every bad=0;
// `total bad=0`.

import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;
import java.util.Arrays;
import java.util.concurrent.atomic.AtomicReference;
import java.util.concurrent.atomic.AtomicReferenceArray;
import java.util.concurrent.atomic.AtomicReferenceFieldUpdater;

import jdk.internal.misc.Unsafe;

public class R21BarrierRows {
    static final int H = 16;
    static final int SLOTS = 32;
    static final int LARGE = 1 << 18;
    static final int POOL = 64;

    static final String[] ROW_NAMES = {
        "putfield", "aastore", "putstatic", "unsafe-put", "unsafe-putvol", "unsafe-cas",
        "unsafe-gas", "vh-field", "vh-array", "vh-static", "updater", "atomic-array",
        "arraycopy", "clone", "fresh-large", "shuffle", "ctor", "loop",
    };
    static final int ROWS = ROW_NAMES.length;

    static final class Node {
        final int id;
        final int sq;
        final int[] data;
        Node next;

        Node(int id, Node next) {
            this.id = id;
            this.sq = mix(id);
            this.data = fill(id);
            this.next = next;
        }
    }

    static final class Pair {
        final Node a;
        final Node b;

        Pair(Node a, Node b) {
            this.a = a;
            this.b = b;
        }
    }

    static final class Holder {
        Node f0, f1, f2, f3;
        volatile Node v;
        Node[] arr = new Node[SLOTS];
        Pair p;
        final AtomicReference<Node> ar = new AtomicReference<>();
        final AtomicReferenceArray<Node> ara = new AtomicReferenceArray<>(SLOTS);
    }

    static final class St {
        static Node s0, s1, s2, s3;
        static volatile Node vs;
        static Node[] big, big2, big3, big4, pool;
        static Object junk;
    }

    static final Unsafe U = Unsafe.getUnsafe();
    static final long OFF_F0 = U.objectFieldOffset(Holder.class, "f0");
    static final long OFF_F1 = U.objectFieldOffset(Holder.class, "f1");
    static final long OFF_F2 = U.objectFieldOffset(Holder.class, "f2");
    static final long OFF_F3 = U.objectFieldOffset(Holder.class, "f3");
    static final long OFF_V = U.objectFieldOffset(Holder.class, "v");
    static final long ABASE = U.arrayBaseOffset(Node[].class);
    static final long ASCALE = U.arrayIndexScale(Node[].class);

    static final VarHandle VH_F0;
    static final VarHandle VH_ARR;
    static final VarHandle VH_VS;

    static {
        try {
            MethodHandles.Lookup l = MethodHandles.lookup();
            VH_F0 = l.findVarHandle(Holder.class, "f0", Node.class);
            VH_VS = l.findStaticVarHandle(St.class, "vs", Node.class);
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
        VH_ARR = MethodHandles.arrayElementVarHandle(Node[].class);
    }

    static final AtomicReferenceFieldUpdater<Holder, Node> ARFU =
            AtomicReferenceFieldUpdater.newUpdater(Holder.class, Node.class, "v");

    static final Holder[][] HS = new Holder[ROWS][];
    static final int[] CTR = new int[ROWS];
    static final long[] BAD = new long[ROWS];
    static final long[] VBAD = new long[ROWS];
    static volatile boolean STOP;

    // ---- payload ---------------------------------------------------------

    static int mix(int id) {
        int h = id * 0x9E3779B1;
        return h ^ (h >>> 15);
    }

    static int[] fill(int id) {
        int[] d = new int[4 + (id & 7)];
        for (int k = 0; k < d.length; k++) {
            d[k] = id * 31 + k;
        }
        return d;
    }

    static boolean okNode(Node n) {
        if (n.sq != mix(n.id)) {
            return false;
        }
        int[] d = n.data;
        if (d == null || d.length != 4 + (n.id & 7)) {
            return false;
        }
        for (int k = 0; k < d.length; k++) {
            if (d[k] != n.id * 31 + k) {
                return false;
            }
        }
        return true;
    }

    static Node fresh(int r) {
        int id = ++CTR[r] + r * 10_000_000;
        if ((id & 15) == 0) {
            St.junk = new byte[64 + (id & 255)];
        }
        Node child = (id % 3 == 0) ? new Node(-id, null) : null;
        return new Node(id, child);
    }

    static int slot(int r, int i) {
        return (i * 13 + r) % SLOTS;
    }

    static Holder holder(int r, int i) {
        return HS[r][(int) ((i * 7L + r) % H)];
    }

    // ---- the rows ----------------------------------------------------------

    static void rowPutfield(int i) {
        Holder h = holder(0, i);
        Node n = fresh(0);
        switch (i & 3) {
            case 0: h.f0 = n; break;
            case 1: h.f1 = n; break;
            case 2: h.f2 = n; break;
            default: h.f3 = n; break;
        }
    }

    static void rowAastore(int i) {
        holder(1, i).arr[slot(1, i)] = fresh(1);
    }

    static void rowPutstatic(int i) {
        Node n = fresh(2);
        switch (i & 3) {
            case 0: St.s0 = n; break;
            case 1: St.s1 = n; break;
            case 2: St.s2 = n; break;
            default: St.s3 = n; break;
        }
    }

    static void rowUnsafePut(int i) {
        Holder h = holder(3, i);
        Node n = fresh(3);
        if ((i & 1) == 0) {
            U.putReference(h, OFF_F1, n);
        } else {
            U.putReference(h.arr, ABASE + (long) slot(3, i) * ASCALE, n);
        }
    }

    static void rowUnsafePutVol(int i) {
        Holder h = holder(4, i);
        Node n = fresh(4);
        if ((i & 1) == 0) {
            U.putReferenceVolatile(h, OFF_V, n);
        } else {
            U.putReferenceRelease(h, OFF_F2, n);
        }
    }

    static void rowUnsafeCas(int i) {
        Holder h = holder(5, i);
        Node n = fresh(5);
        if ((i & 1) == 0) {
            Object cur = U.getReference(h, OFF_F3);
            if (!U.compareAndSetReference(h, OFF_F3, cur, n)) {
                BAD[5]++;
            }
        } else {
            long off = ABASE + (long) slot(5, i) * ASCALE;
            Object cur = U.getReferenceVolatile(h.arr, off);
            if (!U.compareAndSetReference(h.arr, off, cur, n)) {
                BAD[5]++;
            }
        }
    }

    static void rowUnsafeGas(int i) {
        Holder h = holder(6, i);
        Object prev = U.getAndSetReference(h, OFF_F0, fresh(6));
        if (prev != null && !(prev instanceof Node && okNode((Node) prev))) {
            BAD[6]++;
        }
    }

    static void rowVhField(int i) {
        Holder h = holder(7, i);
        Node n = fresh(7);
        switch (i % 5) {
            case 0:
                VH_F0.set(h, n);
                break;
            case 1:
                VH_F0.setVolatile(h, n);
                break;
            case 2:
                VH_F0.setRelease(h, n);
                break;
            case 3: {
                Node cur = (Node) VH_F0.getVolatile(h);
                if (!VH_F0.compareAndSet(h, cur, n)) {
                    BAD[7]++;
                }
                break;
            }
            default: {
                Node prev = (Node) VH_F0.getAndSet(h, n);
                if (prev != null && !okNode(prev)) {
                    BAD[7]++;
                }
                break;
            }
        }
    }

    static void rowVhArray(int i) {
        Holder h = holder(8, i);
        Node[] a = h.arr;
        int s = slot(8, i);
        Node n = fresh(8);
        if ((i & 1) == 0) {
            VH_ARR.setVolatile(a, s, n);
        } else {
            Node cur = (Node) VH_ARR.getVolatile(a, s);
            if (!VH_ARR.compareAndSet(a, s, cur, n)) {
                BAD[8]++;
            }
        }
    }

    static void rowVhStatic(int i) {
        Node n = fresh(9);
        switch (i % 3) {
            case 0:
                VH_VS.setVolatile(n);
                break;
            case 1: {
                Node cur = (Node) VH_VS.getVolatile();
                if (!VH_VS.compareAndSet(cur, n)) {
                    BAD[9]++;
                }
                break;
            }
            default:
                holder(9, i).ar.set(n);
                break;
        }
    }

    static void rowUpdater(int i) {
        Holder h = holder(10, i);
        Node n = fresh(10);
        switch (i & 3) {
            case 0:
                ARFU.set(h, n);
                break;
            case 1:
                ARFU.lazySet(h, n);
                break;
            case 2: {
                Node cur = ARFU.get(h);
                if (!ARFU.compareAndSet(h, cur, n)) {
                    BAD[10]++;
                }
                break;
            }
            default: {
                Node prev = ARFU.getAndSet(h, n);
                if (prev != null && !okNode(prev)) {
                    BAD[10]++;
                }
                break;
            }
        }
    }

    static void rowAtomicArray(int i) {
        AtomicReferenceArray<Node> a = holder(11, i).ara;
        int s = slot(11, i);
        Node n = fresh(11);
        switch (i % 3) {
            case 0:
                a.set(s, n);
                break;
            case 1:
                a.lazySet(s, n);
                break;
            default: {
                Node cur = a.get(s);
                if (!a.compareAndSet(s, cur, n)) {
                    BAD[11]++;
                }
                break;
            }
        }
    }

    static void rowArraycopy(int i) {
        Holder h = holder(12, i);
        Node[] src = new Node[8];
        for (int k = 0; k < 8; k++) {
            src[k] = fresh(12);
        }
        System.arraycopy(src, 0, h.arr, (i & 3) * 8, 8);
        if (i % 7 == 0) {
            // Within the old array (overlapping, backward copy).
            System.arraycopy(h.arr, 0, h.arr, 1, 8);
        }
    }

    static void rowClone(int i) {
        Holder h = holder(13, i);
        Node[] c = h.arr.clone();
        c[slot(13, i)] = fresh(13);
        h.arr = c;
        if (i % 1500 == 0) {
            Node[] b = new Node[LARGE];
            for (int k = 0; k < LARGE; k += 512) {
                b[k] = fresh(13);
            }
            St.big = b.clone();
            St.big2 = Arrays.copyOf(b, LARGE);
        }
    }

    static void fillLarge(Node[] a) {
        for (int k = 0; k < a.length; k += 257) {
            a[k] = fresh(14);
        }
    }

    static void rowFreshLarge(int i) {
        if (i % 1000 == 0) {
            Node[] a = new Node[LARGE];
            fillLarge(a);
            St.big3 = a;
        } else {
            Node[] a = new Node[SLOTS];
            for (int k = 0; k < SLOTS; k += 3) {
                a[k] = fresh(14);
            }
            holder(14, i).arr = a;
        }
    }

    static void rowShuffle(int i) {
        Holder a = HS[15][i % H];
        Holder b = HS[15][(i + 5) % H];
        Node x = a.f0;
        b.f1 = x;
        a.f0 = fresh(15);
        int s1 = (i * 13) % SLOTS;
        int s2 = (i * 7 + 3) % SLOTS;
        Node y = a.arr[s1];
        b.arr[s2] = y;
        a.arr[s1] = fresh(15);
    }

    static void rowCtor(int i) {
        Holder h = holder(16, i);
        Node second = (i & 1) == 0 ? fresh(16) : h.f0;
        h.p = new Pair(fresh(16), second);
        h.f0 = fresh(16);
    }

    static void storeLoop(Node[] dst, Node[] pool, int i) {
        for (int j = 0; j < dst.length; j++) {
            dst[j] = pool[(j + i) & (POOL - 1)];
        }
    }

    static void rowLoop(int i) {
        if (St.pool == null || (i & 63) == 0) {
            Node[] p = new Node[POOL];
            for (int k = 0; k < POOL; k++) {
                p[k] = fresh(17);
            }
            St.pool = p;
        }
        storeLoop(holder(17, i).arr, St.pool, i);
        if (i % 500 == 0) {
            storeLoop(St.big4, St.pool, i);
        }
    }

    static void dispatch(int r, int i) {
        switch (r) {
            case 0: rowPutfield(i); break;
            case 1: rowAastore(i); break;
            case 2: rowPutstatic(i); break;
            case 3: rowUnsafePut(i); break;
            case 4: rowUnsafePutVol(i); break;
            case 5: rowUnsafeCas(i); break;
            case 6: rowUnsafeGas(i); break;
            case 7: rowVhField(i); break;
            case 8: rowVhArray(i); break;
            case 9: rowVhStatic(i); break;
            case 10: rowUpdater(i); break;
            case 11: rowAtomicArray(i); break;
            case 12: rowArraycopy(i); break;
            case 13: rowClone(i); break;
            case 14: rowFreshLarge(i); break;
            case 15: rowShuffle(i); break;
            case 16: rowCtor(i); break;
            default: rowLoop(i); break;
        }
    }

    // ---- verification ------------------------------------------------------

    static final class Acc {
        long sum;
        long live;
        long bad;
    }

    static void visit(Acc a, Object o) {
        if (o == null) {
            a.sum = a.sum * 31 + 7;
            return;
        }
        if (!(o instanceof Node)) {
            a.bad++;
            return;
        }
        Node n = (Node) o;
        for (int depth = 0; n != null && depth < 4; depth++) {
            if (!okNode(n)) {
                a.bad++;
            }
            a.live++;
            a.sum = a.sum * 0x100000001B3L + (n.id & 0xffffffffL);
            n = n.next;
        }
    }

    static void visitArray(Acc a, Object[] arr) {
        if (arr == null) {
            a.sum = a.sum * 31 + 3;
            return;
        }
        for (Object o : arr) {
            visit(a, o);
        }
    }

    static void visitHolder(Acc a, Holder h) {
        visit(a, h.f0);
        visit(a, h.f1);
        visit(a, h.f2);
        visit(a, h.f3);
        visit(a, h.v);
        visitArray(a, h.arr);
        Pair p = h.p;
        if (p == null) {
            a.sum = a.sum * 31 + 5;
        } else {
            visit(a, p.a);
            visit(a, p.b);
        }
        visit(a, h.ar.get());
        for (int k = 0; k < SLOTS; k++) {
            visit(a, h.ara.get(k));
        }
    }

    static Acc verifyRow(int r) {
        Acc a = new Acc();
        for (Holder h : HS[r]) {
            visitHolder(a, h);
        }
        switch (r) {
            case 2:
                visit(a, St.s0);
                visit(a, St.s1);
                visit(a, St.s2);
                visit(a, St.s3);
                break;
            case 9:
                visit(a, St.vs);
                break;
            case 13:
                visitArray(a, St.big);
                visitArray(a, St.big2);
                break;
            case 14:
                visitArray(a, St.big3);
                break;
            case 17:
                visitArray(a, St.big4);
                visitArray(a, St.pool);
                break;
            default:
                break;
        }
        return a;
    }

    // ---- timing (BARRIERS21-1 / -2) ----------------------------------------

    static final int TLEN = 4096;
    static Node[] T_OLD;

    static void tCopy(Node[] dst, Node[] src) {
        for (int j = 0; j < dst.length; j++) {
            dst[j] = src[j & (POOL - 1)];
        }
    }

    static void tNull(Node[] dst) {
        for (int j = 0; j < dst.length; j++) {
            dst[j] = null;
        }
    }

    static void tMove(Node[] a) {
        int n = a.length - 1;
        for (int j = 0; j < n; j++) {
            a[j] = a[j + 1];
        }
    }

    static Node[] tPool() {
        Node[] p = new Node[POOL];
        for (int k = 0; k < POOL; k++) {
            p[k] = new Node(k, null);
        }
        return p;
    }

    static double tRep(int row, int iters) {
        Node[] pool = tPool();
        Node[] young = row == 1 ? new Node[TLEN] : null;
        long t0 = System.nanoTime();
        for (int k = 0; k < iters; k++) {
            switch (row) {
                case 0: tCopy(T_OLD, pool); break;
                case 1: tCopy(young, pool); break;
                case 2: tNull(T_OLD); break;
                default: tMove(T_OLD); break;
            }
        }
        long dt = System.nanoTime() - t0;
        return (double) dt / ((double) iters * TLEN);
    }

    static void timeRows() {
        String[] names = {"g1-aastore-cross", "g1-aastore-same", "g1-aastore-null", "g1-aastore-move"};
        int iters = 200;
        T_OLD = new Node[TLEN];
        tCopy(T_OLD, tPool());
        System.gc();
        System.gc();
        for (int row = 0; row < names.length; row++) {
            for (int w = 0; w < 5; w++) {
                tRep(row, iters);
            }
            double[] reps = new double[9];
            for (int r = 0; r < reps.length; r++) {
                reps[r] = tRep(row, iters);
            }
            Arrays.sort(reps);
            System.out.println("time " + names[row] + " "
                    + String.format(java.util.Locale.ROOT, "%.2f", reps[reps.length / 2]) + " ns/op");
        }
    }

    // ---- driver ------------------------------------------------------------

    static void churn() {
        Object[] ring = new Object[1024];
        int k = 0;
        long n = 0;
        while (!STOP) {
            Object[] o = new Object[8];
            for (int j = 0; j < 8; j++) {
                o[j] = new byte[24 + j * 8];
            }
            ring[k] = o;
            k = (k + 1) & 1023;
            if (++n % 500_000 == 0) {
                System.gc();
            }
        }
    }

    public static void main(String[] args) throws Exception {
        int rounds = Integer.getInteger("r21.rounds", 24);
        int ops = Integer.getInteger("r21.ops", 4000);
        System.out.println("R21BarrierRows rounds=" + rounds + " ops=" + ops);
        for (int r = 0; r < ROWS; r++) {
            Holder[] hs = new Holder[H];
            for (int k = 0; k < H; k++) {
                hs[k] = new Holder();
            }
            HS[r] = hs;
        }
        St.big4 = new Node[LARGE];
        for (int k = 0; k < 4; k++) {
            System.gc();
        }
        Thread churn = new Thread(R21BarrierRows::churn, "r21-churn");
        churn.setDaemon(true);
        churn.start();
        for (int round = 0; round < rounds; round++) {
            for (int r = 0; r < ROWS; r++) {
                for (int i = 0; i < ops; i++) {
                    dispatch(r, round * ops + i);
                }
            }
            if (round % 3 == 2) {
                System.gc();
            }
            for (int r = 0; r < ROWS; r++) {
                VBAD[r] += verifyRow(r).bad;
            }
        }
        STOP = true;
        churn.join();
        System.gc();
        long total = 0;
        for (int r = 0; r < ROWS; r++) {
            Acc a = verifyRow(r);
            long bad = BAD[r] + VBAD[r] + a.bad;
            total += bad;
            System.out.println("row " + ROW_NAMES[r] + " live=" + a.live
                    + " sum=" + Long.toHexString(a.sum) + " bad=" + bad);
        }
        System.out.println("total bad=" + total);
        if (Boolean.parseBoolean(System.getProperty("r21.time", "true"))) {
            timeRows();
        }
    }
}
