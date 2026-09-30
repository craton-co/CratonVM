// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;
import java.lang.reflect.Field;
import java.util.Arrays;
import java.util.concurrent.atomic.AtomicReferenceArray;
import java.util.concurrent.atomic.AtomicReferenceFieldUpdater;

/**
 * Generational GC round 4, wave 4, lane "cards4": every reference-STORE FORM
 * the store-form table in
 * {@code docs/internal/reviews/gengc-round4-w4-cards4-20260924.md} names,
 * aimed at TENURED receivers, hot enough to be compiled, under frequent young
 * collections ({@code CRATONVM_DBG=gc-stress=250000}). After every round the
 * probe checks that every young child each form stored is still reachable and
 * intact; a lost card is a {@code FAIL} line naming the form.
 *
 * <pre>
 *   form=&lt;name&gt; ok                        one per form, in table order
 *   forms=F slots=N rounds=R failures=0 checksum=C
 * </pre>
 *
 * The last line is deterministic and HotSpot prints the same one.
 *
 * Forms: compiled putfield (plain, via a tiny inlinable setter, inside a
 * coarsenable synchronized loop), compiled aastore (wide and narrow arrays),
 * putstatic, VarHandle field set/CAS/getAndSet, VarHandle array
 * set/CAS/getAndSet, AtomicReferenceFieldUpdater, AtomicReferenceArray,
 * sun.misc.Unsafe putObject/CAS, reflection Field.set, System.arraycopy into
 * an old array, Arrays.copyOf/clone results stored into an old holder, and an
 * escape-analysis candidate that escapes into an old array only on a rare
 * branch (the materialisation path).
 *
 * Usage: GenR4W4StoreFormsProbe [slots] [rounds] [churnDepth]
 *   slots       tenured holders / array slots per form (default 2048)
 *   rounds      store+verify rounds (default 30)
 *   churnDepth  garbage tree depth per round (default 11)
 *
 * Commands:
 *   javac -d tools/bench tools/bench/GenR4W4StoreFormsProbe.java
 *   java -cp tools/bench GenR4W4StoreFormsProbe                     # oracle
 *   CRATONVM_DBG=gc-stress=250000 CRATONVM_GC_VERIFY_RSET=1 \
 *     CRATONVM_JIT_INLINE_CARD_MARK=1 cratonvm --java-home "$JDK" \
 *     -XX:+UseGenerationalGC -Xmx512m -c tools/bench GenR4W4StoreFormsProbe
 */
public final class GenR4W4StoreFormsProbe {
    /** A young child: its identity is checkable after any number of cycles. */
    static final class Child {
        final long id;
        final long check;
        Child(long id) {
            this.id = id;
            this.check = mix(id);
        }
    }

    static final class Holder {
        Object f;
        volatile Object v;
        Object r;
        Object s;
        Object t;
        void set(Object o) { this.t = o; } // tiny: the inliner's putfield body
    }

    /** An escape-analysis candidate: normally never leaves `escapeRound`. */
    static final class Box {
        Object payload;
        Box(Object payload) { this.payload = payload; }
    }

    static final class Tree {
        Tree a, b;
        int v;
        Tree(int v) { this.v = v; }
    }

    static final String[] FORMS = {
        "putfield", "putfield-inlined-setter", "putfield-coarsened-lock",
        "aastore-wide", "aastore-small", "putstatic",
        "varhandle-field-set", "varhandle-field-cas", "varhandle-field-getAndSet",
        "varhandle-array-set", "varhandle-array-cas", "varhandle-array-getAndSet",
        "field-updater", "AtomicReferenceArray", "Unsafe.putObject", "Unsafe.cas",
        "reflect-Field.set", "System.arraycopy", "Arrays.copyOf", "clone",
        "ea-materialised",
    };
    static final int F = FORMS.length;

    static final VarHandle VH_R;
    static final VarHandle VH_ARR = MethodHandles.arrayElementVarHandle(Object[].class);
    static final AtomicReferenceFieldUpdater<Holder, Object> UPD =
            AtomicReferenceFieldUpdater.newUpdater(Holder.class, Object.class, "v");
    static final Field REFLECT_S;
    static final sun.misc.Unsafe U;
    static final long F_OFFSET;
    static final long ARR_BASE;
    static final long ARR_SCALE;

    static {
        try {
            VH_R = MethodHandles.lookup().findVarHandle(Holder.class, "r", Object.class);
            REFLECT_S = Holder.class.getDeclaredField("s");
            Field theUnsafe = sun.misc.Unsafe.class.getDeclaredField("theUnsafe");
            theUnsafe.setAccessible(true);
            U = (sun.misc.Unsafe) theUnsafe.get(null);
            F_OFFSET = U.objectFieldOffset(Holder.class.getDeclaredField("f"));
            ARR_BASE = U.arrayBaseOffset(Object[].class);
            ARR_SCALE = U.arrayIndexScale(Object[].class);
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    /** The putstatic form's target: statics are GC roots, not cards. */
    static Object staticRef;

    static void putstatic(Object o) { staticRef = o; }

    static long mix(long x) {
        x ^= x >>> 33;
        x *= 0xFF51AFD7ED558CCDL;
        x ^= x >>> 33;
        return x;
    }

    static long idFor(int form, int slot, int round) {
        return ((long) form << 48) ^ ((long) round << 24) ^ slot;
    }

    static Tree build(int depth, int v) {
        Tree t = new Tree(v);
        if (depth > 0) {
            t.a = build(depth - 1, v * 2);
            t.b = build(depth - 1, v * 2 + 1);
        }
        return t;
    }

    static long sum(Tree t) {
        return t == null ? 0 : t.v + sum(t.a) + sum(t.b);
    }

    // ---- the compiled forms: tiny, hot, one store each ----------------------

    static void putfield(Holder h, Object o) { h.f = o; }

    static void putfieldInlined(Holder h, Object o) {
        h.t = null; // the inliner's null-old-value fast path
        h.set(o);
    }

    static void aastore(Object[] a, int i, Object o) { a[i] = o; }

    static void coarsened(Holder[] hs, int from, int to, int form, int round) {
        for (int i = from; i < to; i++) {
            synchronized (hs[i]) {
                hs[i].s = new Child(idFor(form, i, round));
            }
        }
    }

    /**
     * `b` is scalar-replaceable on the common path, which stores only its
     * payload; one slot in 1024 publishes the Box itself into the old array,
     * which is where an eliminated allocation has to be materialised.
     */
    static void escapeRound(Object[] sink, int i, int round, int form) {
        Box b = new Box(new Child(idFor(form, i, round)));
        if ((i & 1023) == 7) {
            sink[i] = b;
        } else {
            sink[i] = b.payload;
        }
    }

    public static void main(String[] args) {
        int n = args.length > 0 ? Integer.parseInt(args[0]) : 2048;
        int rounds = args.length > 1 ? Integer.parseInt(args[1]) : 30;
        int churnDepth = args.length > 2 ? Integer.parseInt(args[2]) : 11;

        // Tenured side, one holder array or target array per form.
        Holder[][] hs = new Holder[F][];
        for (int p = 0; p < F; p++) {
            hs[p] = new Holder[n];
            for (int i = 0; i < n; i++) {
                hs[p][i] = new Holder();
            }
        }
        Object[] wide = new Object[n * 64]; // spans many cards: element cards
        Object[] small = new Object[n];
        Object[] vhArr = new Object[n];
        Object[] casArr = new Object[n];
        Object[] gasArr = new Object[n];
        Object[] copyDst = new Object[n];
        Object[] eaSink = new Object[n];
        AtomicReferenceArray<Object> ara = new AtomicReferenceArray<>(n);
        Object[] unsafeCas = new Object[n];

        long sink = 0;
        for (int w = 0; w < 8; w++) {
            sink += sum(build(churnDepth + 2, w));
        }
        System.gc();

        long checksum = 0;
        long failures = 0;
        boolean[] failed = new boolean[F];
        for (int r = 0; r < rounds; r++) {
            Object[] stage = new Object[n];
            for (int i = 0; i < n; i++) {
                putfield(hs[0][i], new Child(idFor(0, i, r)));
                putfieldInlined(hs[1][i], new Child(idFor(1, i, r)));
                aastore(wide, i * 64 + (i % 64), new Child(idFor(3, i, r)));
                aastore(small, i, new Child(idFor(4, i, r)));
                putstatic(new Child(idFor(5, i, r))); // the last one is checked
                VH_R.setVolatile(hs[6][i], new Child(idFor(6, i, r)));
                Object cur = VH_R.getVolatile(hs[7][i]);
                if (!VH_R.compareAndSet(hs[7][i], cur, new Child(idFor(7, i, r)))) {
                    failures++;
                }
                Object prev = VH_R.getAndSet(hs[8][i], new Child(idFor(8, i, r)));
                sink += prev == null ? 1 : 0;
                VH_ARR.setVolatile(vhArr, i, new Child(idFor(9, i, r)));
                Object old = VH_ARR.getVolatile(casArr, i);
                if (!VH_ARR.compareAndSet(casArr, i, old, new Child(idFor(10, i, r)))) {
                    failures++;
                }
                Object prevElem = VH_ARR.getAndSet(gasArr, i, new Child(idFor(11, i, r)));
                sink += prevElem == null ? 1 : 0;
                UPD.getAndSet(hs[12][i], new Child(idFor(12, i, r)));
                ara.set(i, new Child(idFor(13, i, r)));
                U.putObject(hs[14][i], F_OFFSET, new Child(idFor(14, i, r)));
                long off = ARR_BASE + (long) i * ARR_SCALE;
                Object o = U.getObjectVolatile(unsafeCas, off);
                if (!U.compareAndSwapObject(unsafeCas, off, o, new Child(idFor(15, i, r)))) {
                    failures++;
                }
                try {
                    REFLECT_S.set(hs[16][i], new Child(idFor(16, i, r)));
                } catch (IllegalAccessException e) {
                    failures++;
                }
                stage[i] = new Child(idFor(17, i, r));
                // Arrays.copyOf / clone produce YOUNG arrays; storing them into
                // an old holder is the edge. Their elements are young children.
                Object[] one = new Object[] {new Child(idFor(18, i, r))};
                hs[18][i].f = Arrays.copyOf(one, 2);
                Object[] two = new Object[] {new Child(idFor(19, i, r))};
                hs[19][i].f = two.clone();
                escapeRound(eaSink, i, r, 20);
            }
            coarsened(hs[2], 0, n, 2, r);
            System.arraycopy(stage, 0, copyDst, 0, n);
            stage = null;

            // Young collections: every child above is reachable only through
            // an old-to-young edge (or a static root, for form 5).
            sink += sum(build(churnDepth, r));

            for (int i = 0; i < n; i++) {
                Object[] got = new Object[F];
                got[0] = hs[0][i].f;
                got[1] = hs[1][i].t;
                got[2] = hs[2][i].s;
                got[3] = wide[i * 64 + (i % 64)];
                got[4] = small[i];
                got[5] = staticRef; // compared at the last slot only, below
                got[6] = hs[6][i].r;
                got[7] = hs[7][i].r;
                got[8] = hs[8][i].r;
                got[9] = vhArr[i];
                got[10] = casArr[i];
                got[11] = gasArr[i];
                got[12] = hs[12][i].v;
                got[13] = ara.get(i);
                got[14] = hs[14][i].f;
                got[15] = unsafeCas[i];
                got[16] = hs[16][i].s;
                got[17] = copyDst[i];
                Object c18 = hs[18][i].f;
                got[18] = c18 instanceof Object[] ? ((Object[]) c18)[0] : c18;
                Object c19 = hs[19][i].f;
                got[19] = c19 instanceof Object[] ? ((Object[]) c19)[0] : c19;
                Object b = eaSink[i];
                got[20] = b instanceof Box ? ((Box) b).payload : b;
                for (int p = 0; p < F; p++) {
                    if (p == 5 && i != n - 1) {
                        continue; // one static field: only the last store survives
                    }
                    long want = idFor(p, i, r);
                    Object o = got[p];
                    if (!(o instanceof Child) || ((Child) o).id != want
                            || ((Child) o).check != mix(want)) {
                        if (failures < 20) {
                            System.out.println("FAIL form=" + FORMS[p] + " slot=" + i
                                    + " round=" + r);
                        }
                        failed[p] = true;
                        failures++;
                    } else {
                        checksum += (want ^ p) & 0xFFFFF;
                    }
                }
            }
        }
        for (int p = 0; p < F; p++) {
            System.out.println("form=" + FORMS[p] + (failed[p] ? " FAILED" : " ok"));
        }
        if (sink == 42) {
            System.out.println("unreachable");
        }
        System.out.println("forms=" + F + " slots=" + n + " rounds=" + rounds
                + " failures=" + failures + " checksum=" + checksum);
    }
}
