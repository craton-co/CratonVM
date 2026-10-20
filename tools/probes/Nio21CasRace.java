// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;
import java.util.Arrays;
import java.util.concurrent.CountDownLatch;
import java.util.concurrent.atomic.AtomicIntegerArray;
import java.util.concurrent.atomic.AtomicIntegerFieldUpdater;
import java.util.concurrent.atomic.AtomicLongArray;
import java.util.concurrent.atomic.AtomicLongFieldUpdater;
import java.util.concurrent.atomic.AtomicReferenceArray;

/**
 * nio21 wave 5, lane L3: witness for
 * docs/internal/fixed-bugs/r21-primitive-array-unsafe-cas-is-lock-based-not-hardware-FIXED-20261010.md.
 *
 * <p>A CAS on a primitive-array element must be atomic against a PLAIN store to the same element.
 * Before the fix the VM ran an aligned {@code compareAndSet{Int,Long}} on an {@code int[]} /
 * {@code long[]} as a locked read-compare-write, and a plain store that takes no lock could land
 * between the read and the write and be overwritten while the CAS reported success.
 *
 * <p>Race rows. One writer stores markers into element 0 with PLAIN stores ({@code iastore} /
 * {@code lastore}, or {@code AtomicIntegerArray.set}): marker {@code j} is {@code j << SHIFT}.
 * Two incrementers add 1 to the same element ({@code compareAndSet(v, v + 1)} loops, or
 * {@code getAndAdd(1)}). The incrementers do fewer than {@code 2^SHIFT} increments in total, so
 * the element always reads {@code marker + k} with {@code k < 2^SHIFT}, and its top bits
 * ({@code value >>> SHIFT}) name the last marker stored. HotSpot's invariant: once the writer has
 * stored marker {@code j}, nobody ever reads top bits below {@code j} again -- an increment that
 * read an older value must fail its CAS, not overwrite the marker. A read below the last stored
 * marker is a LOST STORE. Each row prints {@code lost=0} and the final top bits ({@code top=}
 * the marker count) on HotSpot, every run.
 *
 * <p>Conformance rows (single-threaded, each body hot enough to compile): array-element CAS,
 * compareAndExchange, getAndAdd, getAndSet and getAndBitwiseOr through {@code VarHandle}s on
 * every primitive array type, {@code AtomicIntegerArray} / {@code AtomicLongArray}, and
 * {@code sun.misc.Unsafe} compareAndSwap / getAndAdd / getAndSet on {@code int[]} / {@code long[]}
 * including a mismatched element type; values only.
 *
 * <pre>
 *   javac -d out tools/probes/Nio21CasRace.java
 *   java -cp out Nio21CasRace                                   > hs.txt
 *   cratonvm --jdk-only   -cp out Nio21CasRace                  > jo.txt
 *   cratonvm --compatible -cp out Nio21CasRace                  > co.txt
 *   cratonvm --jdk-only --nojit -cp out Nio21CasRace            (interpreter only)
 *   CRATONVM_UNSAFE_ARRAY_HW_CAS=0 cratonvm --jdk-only -cp out Nio21CasRace   (pre-fix: lost > 0)
 *   CRATONVM_JIT_UNSAFE_RMW_INLINE=0 cratonvm --jdk-only -cp out Nio21CasRace
 *   cratonvm --jdk-only -cp out -Dnio21.rows=bench Nio21CasRace
 * </pre>
 *
 * {@code -Dnio21.rows=conf} or {@code race} runs one half (default: both); {@code bench} prints
 * {@code <row> <N> ns/op} for an uncontended {@code cas} / {@code xadd} / {@code xchg} on an
 * {@code int[]} element and {@code lcas} / {@code lxadd} on a {@code long[]} one.
 * {@code -Dnio21.markers=N} (default 400) sets the writer's marker count.
 *
 * <p>nio21 wave 6, lane L3 added: race rows {@code ref-array} / {@code ara} (a reference-array
 * element CAS against plain {@code aastore} / {@code AtomicReferenceArray.lazySet}, page
 * {@code r21-reference-array-unsafe-cas-is-lock-based-FIXED-20261008.md}; the pre-fix arm is
 * {@code CRATONVM_UNSAFE_REF_ARRAY_HW_CAS=0}), {@code field-int} / {@code field-ref} (instance-field
 * CAS against plain {@code putfield}), and {@code misaligned} (a {@code sun.misc.Unsafe} int CAS at
 * {@code byte[]} base + 1 against plain byte stores into its top byte, page
 * {@code docs/internal/fixed-bugs/r21-misaligned-raw-unsafe-cas-is-process-locked-FIXED-20261010.md}; pre-fix arm
 * {@code CRATONVM_UNSAFE_MISALIGNED_HW_CAS=0}); conformance rows {@code conf fields},
 * {@code conf ref-array} and {@code conf misaligned}; bench rows {@code bor} (getAndBitwiseOr on an
 * {@code int[]}), {@code fcas} / {@code fxadd} (an {@code int} field) and {@code refcas} (an
 * {@code Object[]} element).
 *
 * <p>nio21 wave 7, lane L3 added the INSTANCE-FIELD rows for the compiled field arm of the
 * {@code Unsafe} RMW prefixes (a published per-VM cell, proposal W6-L3-1) and the
 * {@code compareAndExchange} rows (W6-L3-2): race rows {@code field-xadd} (field {@code getAndAdd}
 * against plain {@code putfield}), {@code field-long-cas} and {@code field-unsafe}
 * ({@code sun.misc.Unsafe.getAndAddInt} on an object field); conformance row {@code conf fields-w7}
 * ({@code sun.misc.Unsafe} compareAndSwap / getAndAdd / getAndSet on int and long fields, field and
 * array {@code compareAndExchange}, the field updaters); bench rows {@code ufcas} / {@code ufxadd}
 * ({@code sun.misc.Unsafe} on an {@code int} field), {@code lfcas} (a {@code long} field CAS),
 * {@code fcae} (a field {@code compareAndExchange}), {@code cae} (an {@code int[]} one),
 * {@code ifu} / {@code lfu} (the field updaters). The kill switches:
 * {@code CRATONVM_JIT_UNSAFE_FIELD_RMW_INLINE=0} (no field arm) and
 * {@code CRATONVM_JIT_UNSAFE_CAE_INLINE=0} (no exchange rows); both must print the same
 * conformance block. {@code CRATONVM_DBG_JITC=1} names each site with {@code field_arm=true}.
 *
 * <p>nio21 wave 8, lane L3: the w6 host run lost markers in {@code race ref-array} / {@code race ara}
 * in both modes and with the JIT off. ZGC is the DEFAULT collector, and the w6 hardware
 * reference-element CAS declined ZGC, so every default run raced the locked read-compare-write;
 * wave 7 admitted the (always, today) unarmed ZGC heap. Run the reference rows on every backend,
 * in both modes; each prints {@code lost=0 top=400}:
 *
 * <pre>
 *   cratonvm --jdk-only   -Dnio21.rows=race -cp out Nio21CasRace                         (ZGC, the default)
 *   cratonvm --compatible -Dnio21.rows=race -cp out Nio21CasRace
 *   cratonvm --jdk-only   -XX:+UseGenerationalGC -Dnio21.rows=race -cp out Nio21CasRace
 *   cratonvm --jdk-only   -XX:+UseG1GC          -Dnio21.rows=race -cp out Nio21CasRace
 *   cratonvm --jdk-only --nojit -Dnio21.rows=race -cp out Nio21CasRace
 *   CRATONVM_UNSAFE_REF_ARRAY_HW_CAS=0 cratonvm --jdk-only -Dnio21.rows=race -cp out Nio21CasRace  (lock arm: lost &gt; 0 expected)
 * </pre>
 */
public final class Nio21CasRace {
    static final VarHandle INTS = MethodHandles.arrayElementVarHandle(int[].class);
    static final VarHandle LONGS = MethodHandles.arrayElementVarHandle(long[].class);
    static final VarHandle SHORTS = MethodHandles.arrayElementVarHandle(short[].class);
    static final VarHandle BYTES = MethodHandles.arrayElementVarHandle(byte[].class);
    static final VarHandle CHARS = MethodHandles.arrayElementVarHandle(char[].class);
    static final VarHandle BOOLS = MethodHandles.arrayElementVarHandle(boolean[].class);
    static final VarHandle FLOATS = MethodHandles.arrayElementVarHandle(float[].class);
    static final VarHandle DOUBLES = MethodHandles.arrayElementVarHandle(double[].class);
    static final VarHandle REFS = MethodHandles.arrayElementVarHandle(Object[].class);

    /** An immutable marker for the reference rows: {@code j} the writer's marker, {@code k} a count. */
    static final class Mark {
        final int j;
        final int k;

        Mark(int j, int k) {
            this.j = j;
            this.k = k;
        }

        long code() {
            return ((long) j << INT_SHIFT) | k;
        }
    }

    /** The instance-field rows' holder: plain fields reached through field VarHandles. */
    static final class Holder {
        int pi;
        long pl;
        Object ref;
        volatile int ai;
    }

    static final VarHandle H_PI;
    static final VarHandle H_PL;
    static final VarHandle H_REF;
    static final AtomicIntegerFieldUpdater<Holder> H_AI =
            AtomicIntegerFieldUpdater.newUpdater(Holder.class, "ai");

    /** nio21 wave 7 L3: the instance-field rows' holder (several fields, so offsets differ). */
    static final class Cell {
        long pad;
        int a;
        long b;
        volatile int ai;
        volatile long al;
        int c;
    }

    static final VarHandle C_A;
    static final VarHandle C_B;
    static final AtomicIntegerFieldUpdater<Cell> C_AI =
            AtomicIntegerFieldUpdater.newUpdater(Cell.class, "ai");
    static final AtomicLongFieldUpdater<Cell> C_AL =
            AtomicLongFieldUpdater.newUpdater(Cell.class, "al");

    static {
        try {
            MethodHandles.Lookup l = MethodHandles.lookup();
            H_PI = l.findVarHandle(Holder.class, "pi", int.class);
            H_PL = l.findVarHandle(Holder.class, "pl", long.class);
            H_REF = l.findVarHandle(Holder.class, "ref", Object.class);
            C_A = l.findVarHandle(Cell.class, "a", int.class);
            C_B = l.findVarHandle(Cell.class, "b", long.class);
        } catch (ReflectiveOperationException e) {
            throw new ExceptionInInitializerError(e);
        }
    }

    /** {@code sun.misc.Unsafe.objectFieldOffset} of a {@link Cell} field, or -1. */
    @SuppressWarnings("removal")
    static long cellOffset(sun.misc.Unsafe u, String name) {
        try {
            return u.objectFieldOffset(Cell.class.getDeclaredField(name));
        } catch (ReflectiveOperationException | RuntimeException e) {
            return -1;
        }
    }

    static final int INT_SHIFT = 21;
    static final int LONG_SHIFT = 40;
    /** Increments per incrementer: two of them stay below 2^21 together. */
    static final int QUOTA = 400_000;
    static final int MARKERS = Integer.getInteger("nio21.markers", 400);
    /** Writer reads per marker: how long each marker stays under fire. */
    static final int SPINS = Integer.getInteger("nio21.spins", 512);
    static volatile long sink;
    static volatile boolean raceDone;

    // ------------------------------------------------------------------ race

    interface Store {
        void store(int j);
    }

    interface Incr {
        /** One increment; answers a value it read (for the incrementer's own check). */
        long incr();
    }

    interface Read {
        long read();
    }

    /**
     * One race row: a writer storing markers 1..MARKERS with `store`, two incrementers running
     * `incr` up to QUOTA times or until the writer is done. Lost = reads below the last marker the
     * reader knows was stored (the writer checks after each store; each incrementer checks that
     * the top bits it reads never go down).
     */
    static void race(String name, int shift, Store store, Incr incr, Read read) throws Exception {
        race(name, shift, MARKERS, store, incr, read);
    }

    static void race(String name, int shift, int markers, Store store, Incr incr, Read read)
            throws Exception {
        final long[] lost = new long[3];
        raceDone = false;
        final CountDownLatch go = new CountDownLatch(1);
        Thread[] incs = new Thread[2];
        for (int t = 0; t < 2; t++) {
            final int slot = t + 1;
            incs[t] = new Thread(() -> {
                try {
                    go.await();
                } catch (InterruptedException e) {
                    return;
                }
                long top = 0;
                for (int k = 0; k < QUOTA && !raceDone; k++) {
                    long seen = incr.incr() >>> shift;
                    if (seen < top) lost[slot]++;
                    top = Math.max(top, seen);
                }
            });
            incs[t].start();
        }
        Thread writer = new Thread(() -> {
            try {
                go.await();
            } catch (InterruptedException e) {
                return;
            }
            for (int j = 1; j <= markers; j++) {
                store.store(j);
                for (int s = 0; s < SPINS; s++) {
                    if ((read.read() >>> shift) < j) lost[0]++;
                }
            }
            raceDone = true;
        });
        writer.start();
        go.countDown();
        writer.join();
        for (Thread t : incs) t.join();
        long top = read.read() >>> shift;
        System.out.println("race " + name + " markers=" + markers + " lost=" + (lost[0] + lost[1] + lost[2])
                + " top=" + top);
    }

    static void races() throws Exception {
        // int[]: plain iastore against VarHandle CAS loops (Unsafe.compareAndSetInt).
        final int[] ia = new int[4];
        race("int-cas", INT_SHIFT,
                j -> ia[0] = j << INT_SHIFT,
                () -> {
                    int v;
                    do {
                        v = (int) INTS.getVolatile(ia, 0);
                    } while (!INTS.compareAndSet(ia, 0, v, v + 1));
                    return v & 0xFFFF_FFFFL;
                },
                () -> ((int) INTS.getVolatile(ia, 0)) & 0xFFFF_FFFFL);
        // int[]: plain iastore against getAndAdd (Unsafe.getAndAddInt).
        final int[] ib = new int[4];
        race("int-xadd", INT_SHIFT,
                j -> ib[0] = j << INT_SHIFT,
                () -> ((int) INTS.getAndAdd(ib, 0, 1)) & 0xFFFF_FFFFL,
                () -> ((int) INTS.getVolatile(ib, 0)) & 0xFFFF_FFFFL);
        // long[]: plain lastore against CAS loops (Unsafe.compareAndSetLong).
        final long[] la = new long[4];
        race("long-cas", LONG_SHIFT,
                j -> la[1] = ((long) j) << LONG_SHIFT,
                () -> {
                    long v;
                    do {
                        v = (long) LONGS.getVolatile(la, 1);
                    } while (!LONGS.compareAndSet(la, 1, v, v + 1));
                    return v;
                },
                () -> (long) LONGS.getVolatile(la, 1));
        // AtomicIntegerArray: set (a volatile store) against incrementAndGet / compareAndSet.
        final AtomicIntegerArray aia = new AtomicIntegerArray(4);
        race("aia", INT_SHIFT,
                j -> aia.set(2, j << INT_SHIFT),
                () -> (aia.incrementAndGet(2) - 1) & 0xFFFF_FFFFL,
                () -> aia.get(2) & 0xFFFF_FFFFL);
        // AtomicLongArray: setPlain (Unsafe.putLong) against compareAndSet.
        final AtomicLongArray ala = new AtomicLongArray(4);
        race("ala", LONG_SHIFT,
                j -> ala.setPlain(3, ((long) j) << LONG_SHIFT),
                () -> {
                    long v;
                    do {
                        v = ala.get(3);
                    } while (!ala.compareAndSet(3, v, v + 1));
                    return v;
                },
                () -> ala.get(3));
        // nio21 wave 6 L3 -- Object[]: plain aastore of a fresh marker against
        // VarHandle CAS loops (Unsafe.compareAndSetReference on the element).
        final Object[] oa = {new Mark(0, 0), null};
        race("ref-array", INT_SHIFT,
                j -> oa[0] = new Mark(j, 0),
                () -> {
                    Mark m;
                    do {
                        m = (Mark) REFS.getVolatile(oa, 0);
                    } while (!REFS.compareAndSet(oa, 0, m, new Mark(m.j, m.k + 1)));
                    return m.code();
                },
                () -> ((Mark) REFS.getVolatile(oa, 0)).code());
        // AtomicReferenceArray: lazySet (Unsafe.putReferenceRelease) against compareAndSet.
        final AtomicReferenceArray<Mark> ara = new AtomicReferenceArray<>(4);
        ara.set(1, new Mark(0, 0));
        race("ara", INT_SHIFT,
                j -> ara.lazySet(1, new Mark(j, 0)),
                () -> {
                    Mark m;
                    do {
                        m = ara.get(1);
                    } while (!ara.compareAndSet(1, m, new Mark(m.j, m.k + 1)));
                    return m.code();
                },
                () -> ara.get(1).code());
        // Instance fields: plain putfield against field VarHandle CAS loops.
        final Holder h = new Holder();
        race("field-int", INT_SHIFT,
                j -> h.pi = j << INT_SHIFT,
                () -> {
                    int v;
                    do {
                        v = (int) H_PI.getVolatile(h);
                    } while (!H_PI.compareAndSet(h, v, v + 1));
                    return v & 0xFFFF_FFFFL;
                },
                () -> ((int) H_PI.getVolatile(h)) & 0xFFFF_FFFFL);
        h.ref = new Mark(0, 0);
        race("field-ref", INT_SHIFT,
                j -> h.ref = new Mark(j, 0),
                () -> {
                    Mark m;
                    do {
                        m = (Mark) H_REF.getVolatile(h);
                    } while (!H_REF.compareAndSet(h, m, new Mark(m.j, m.k + 1)));
                    return m.code();
                },
                () -> ((Mark) H_REF.getVolatile(h)).code());
        // nio21 wave 7 L3: field getAndAdd (one LOCK XADD in the field arm) and a
        // long field CAS loop, against plain putfield.
        final Cell fc = new Cell();
        race("field-xadd", INT_SHIFT,
                j -> fc.a = j << INT_SHIFT,
                () -> ((int) C_A.getAndAdd(fc, 1)) & 0xFFFF_FFFFL,
                () -> ((int) C_A.getVolatile(fc)) & 0xFFFF_FFFFL);
        race("field-long-cas", LONG_SHIFT,
                j -> fc.b = ((long) j) << LONG_SHIFT,
                () -> {
                    long v;
                    do {
                        v = (long) C_B.getVolatile(fc);
                    } while (!C_B.compareAndSet(fc, v, v + 1));
                    return v;
                },
                () -> (long) C_B.getVolatile(fc));
        // A misaligned raw int CAS: bytes 1..4 of a byte[] (base + 1), against plain
        // byte stores of the marker into its top byte b[4]. The increments stay
        // below 2^24, so the top byte is the marker.
        Object o = theUnsafe();
        if (o == null) {
            System.out.println("race misaligned unavailable");
            return;
        }
        final sun.misc.Unsafe u = (sun.misc.Unsafe) o;
        final byte[] mb = new byte[16];
        @SuppressWarnings("removal")
        final long off = u.arrayBaseOffset(byte[].class) + 1;
        race("misaligned", 24, Math.min(MARKERS, 250),
                j -> mb[4] = (byte) j,
                () -> {
                    int v;
                    do {
                        v = u.getIntVolatile(mb, off);
                    } while (!u.compareAndSwapInt(mb, off, v, v + 1));
                    return v & 0xFFFF_FFFFL;
                },
                () -> u.getIntVolatile(mb, off) & 0xFFFF_FFFFL);
        // nio21 wave 7 L3: sun.misc.Unsafe getAndAddInt on an object field (the
        // jdk.internal.misc.Unsafe.getAndAddInt body the field arm serves), against
        // plain putfield.
        final Cell uc = new Cell();
        final long ucOff = cellOffset(u, "c");
        if (ucOff < 0) {
            System.out.println("race field-unsafe unavailable");
            return;
        }
        race("field-unsafe", INT_SHIFT,
                j -> uc.c = j << INT_SHIFT,
                () -> u.getAndAddInt(uc, ucOff, 1) & 0xFFFF_FFFFL,
                () -> u.getIntVolatile(uc, ucOff) & 0xFFFF_FFFFL);
    }

    // ----------------------------------------------------------- conformance

    static long arrayRound(int k, int[] ia, long[] la, short[] sa, byte[] ba, char[] ca,
            boolean[] za, float[] fa, double[] da) {
        int i = k & 3;
        long acc = 0;
        // int
        ia[i] = k;
        acc += INTS.compareAndSet(ia, i, k, k + 7) ? 1 : 0;
        acc += INTS.compareAndSet(ia, i, k, -1) ? 2 : 0;
        acc += (int) INTS.compareAndExchange(ia, i, k + 7, k * 3);
        acc += (int) INTS.getAndAdd(ia, i, 5);
        acc += (int) INTS.getAndSet(ia, i, ~k);
        acc += (int) INTS.getAndBitwiseOr(ia, i, 0x100);
        // A weak CAS may fail spuriously (LL/SC hosts): retry until it holds.
        while (!INTS.weakCompareAndSetPlain(ia, i, ia[i], 42)) {
            Thread.onSpinWait();
        }
        acc = acc * 31 + ia[i];
        // long
        la[i] = (long) k << 33;
        acc += LONGS.compareAndSet(la, i, (long) k << 33, -3L) ? 1 : 0;
        acc += LONGS.compareAndSet(la, i, (long) k << 33, 9L) ? 2 : 0;
        acc += (long) LONGS.getAndAdd(la, i, 0x1_0000_0001L);
        acc += (long) LONGS.getAndSet(la, i, (long) k * k);
        acc += (long) LONGS.compareAndExchange(la, i, (long) k * k, 77L);
        acc = acc * 31 + la[i];
        // short / byte / char / boolean: Unsafe's narrow Java bodies (a CAS on the
        // containing int) or natives.
        sa[i] = (short) -k;
        acc += SHORTS.compareAndSet(sa, i, (short) -k, (short) 0x7FFF) ? 1 : 0;
        acc += (short) SHORTS.getAndAdd(sa, i, (short) 2);
        acc += (short) SHORTS.compareAndExchange(sa, i, (short) 0x8001, (short) k);
        acc = acc * 31 + sa[i];
        ba[i] = (byte) k;
        acc += BYTES.compareAndSet(ba, i, (byte) k, (byte) -128) ? 1 : 0;
        acc += (byte) BYTES.getAndAdd(ba, i, (byte) -1);
        acc += (byte) BYTES.getAndSet(ba, i, (byte) (k >> 2));
        acc = acc * 31 + ba[i] + ba[(i + 1) & 3];
        ca[i] = (char) (0xFFF0 + (k & 15));
        acc += CHARS.compareAndSet(ca, i, ca[i], (char) 0xFFFF) ? 1 : 0;
        acc += (char) CHARS.getAndAdd(ca, i, (char) 1);
        acc = acc * 31 + ca[i];
        za[i] = (k & 1) == 0;
        acc += BOOLS.compareAndSet(za, i, (k & 1) == 0, (k & 2) == 0) ? 1 : 0;
        acc += ((boolean) BOOLS.getAndSet(za, i, true)) ? 2 : 0;
        acc = acc * 31 + (za[i] ? 1 : 0);
        // float / double: CAS compares raw bits.
        fa[i] = -0.0f;
        acc += FLOATS.compareAndSet(fa, i, 0.0f, 1.0f) ? 1 : 0;
        acc += FLOATS.compareAndSet(fa, i, -0.0f, 1.5f) ? 2 : 0;
        acc += (long) ((float) FLOATS.getAndAdd(fa, i, 0.25f) * 4);
        acc = acc * 31 + Float.floatToRawIntBits(fa[i]);
        da[i] = Double.NaN;
        acc += DOUBLES.compareAndSet(da, i, Double.NaN, 2.0) ? 1 : 0;
        acc += (long) ((double) DOUBLES.getAndSet(da, i, (double) -k));
        acc = acc * 31 + Double.doubleToRawLongBits(da[i]);
        return acc;
    }

    static long atomicArrayRound(int k, AtomicIntegerArray aia, AtomicLongArray ala) {
        int i = k & 7;
        long acc = aia.getAndIncrement(i);
        acc += aia.addAndGet(i, k);
        acc += aia.compareAndSet(i, aia.get(i), -k) ? 1 : 0;
        acc += aia.getAndUpdate(i, x -> x * 3);
        acc += aia.accumulateAndGet(i, 5, Integer::sum);
        acc += ala.getAndAdd(i, k);
        acc += ala.incrementAndGet(i);
        acc += ala.compareAndExchange(i, ala.get(i), (long) k << 40);
        while (!ala.weakCompareAndSetVolatile(i, (long) k << 40, 3L)) {
            Thread.onSpinWait();
        }
        return acc * 17 + aia.get(i) + ala.get(i);
    }

    static Object theUnsafe() {
        try {
            java.lang.reflect.Field f = Class.forName("sun.misc.Unsafe").getDeclaredField("theUnsafe");
            f.setAccessible(true);
            return f.get(null);
        } catch (ReflectiveOperationException | RuntimeException e) {
            return null;
        }
    }

    @SuppressWarnings("removal")
    static long unsafeRound(sun.misc.Unsafe u, int k, int[] ia, long[] la, float[] fa) {
        long ib = u.arrayBaseOffset(int[].class);
        long is = u.arrayIndexScale(int[].class);
        long lb = u.arrayBaseOffset(long[].class);
        long ls = u.arrayIndexScale(long[].class);
        long fb = u.arrayBaseOffset(float[].class);
        int i = k & 3;
        long acc = 0;
        ia[i] = k;
        acc += u.compareAndSwapInt(ia, ib + i * is, k, k + 1) ? 1 : 0;
        acc += u.compareAndSwapInt(ia, ib + i * is, k, 0) ? 2 : 0;
        acc += u.getAndAddInt(ia, ib + i * is, -3);
        acc += u.getAndSetInt(ia, ib + i * is, k * 5);
        acc = acc * 31 + ia[i];
        la[i] = k;
        acc += u.compareAndSwapLong(la, lb + i * ls, k, -k) ? 1 : 0;
        acc += u.getAndAddLong(la, lb + i * ls, 1L << 50);
        acc += u.getAndSetLong(la, lb + i * ls, ~(long) k);
        acc = acc * 31 + la[i];
        // A mismatched element type: an int CAS over a float[] element's raw bits.
        fa[i] = 1.0f;
        acc += u.compareAndSwapInt(fa, fb + i * 4L, Float.floatToRawIntBits(1.0f),
                Float.floatToRawIntBits(-2.5f)) ? 1 : 0;
        acc += u.getAndAddInt(fa, fb + i * 4L, 1);
        acc = acc * 31 + Float.floatToRawIntBits(fa[i]);
        return acc;
    }

    /** nio21 wave 6 L3: instance-field CAS / exchange / getAndAdd / bitwise, int, long, reference. */
    static long fieldRound(int k, Holder h) {
        long acc = 0;
        h.pi = k;
        acc += H_PI.compareAndSet(h, k, k + 1) ? 1 : 0;
        acc += H_PI.compareAndSet(h, k, 0) ? 2 : 0;
        acc += (int) H_PI.compareAndExchange(h, k + 1, k * 3);
        acc += (int) H_PI.getAndAdd(h, 7);
        acc += (int) H_PI.getAndSet(h, ~k);
        acc += (int) H_PI.getAndBitwiseOr(h, 0x30);
        acc += (int) H_PI.getAndBitwiseAnd(h, 0xFF0F);
        acc += (int) H_PI.getAndBitwiseXor(h, k);
        acc = acc * 31 + h.pi;
        h.pl = (long) k << 35;
        acc += H_PL.compareAndSet(h, (long) k << 35, -1L) ? 1 : 0;
        acc += (long) H_PL.getAndAdd(h, 3L);
        acc += (long) H_PL.getAndBitwiseXor(h, 0x5555_0000_0000L);
        acc += (long) H_PL.getAndBitwiseOrRelease(h, 0x10L);
        acc += (long) H_PL.compareAndExchange(h, h.pl, (long) k);
        acc = acc * 31 + h.pl;
        acc += H_AI.getAndIncrement(h);
        acc += H_AI.compareAndSet(h, H_AI.get(h), k) ? 1 : 0;
        acc += H_AI.accumulateAndGet(h, 3, Integer::sum);
        Mark m = new Mark(k, 1);
        h.ref = null;
        acc += H_REF.compareAndSet(h, null, m) ? 1 : 0;
        acc += H_REF.compareAndSet(h, null, m) ? 2 : 0;
        acc += H_REF.compareAndExchange(h, m, null) == m ? 4 : 0;
        acc += h.ref == null ? 8 : 0;
        return acc;
    }

    /** nio21 wave 6 L3: reference-array element CAS through VarHandle and AtomicReferenceArray. */
    static long refArrayRound(int k, Object[] oa, AtomicReferenceArray<Mark> ara, Mark[] ms) {
        int i = k & 3;
        Mark a = ms[k & 7];
        Mark b = ms[(k + 1) & 7];
        oa[i] = a;
        long acc = REFS.compareAndSet(oa, i, a, b) ? 1 : 0;
        acc += REFS.compareAndSet(oa, i, a, null) ? 2 : 0;
        acc += REFS.compareAndExchange(oa, i, b, null) == b ? 4 : 0;
        acc += REFS.compareAndSet(oa, i, null, a) ? 8 : 0;
        acc += REFS.getAndSet(oa, i, b) == a ? 16 : 0;
        acc += ((Mark) oa[i]).j;
        ara.set(i, a);
        acc += ara.compareAndSet(i, a, b) ? 32 : 0;
        acc += ara.compareAndSet(i, a, b) ? 64 : 0;
        acc += ara.getAndUpdate(i, x -> x == b ? a : b) == b ? 128 : 0;
        acc += ara.get(i).j;
        return acc;
    }

    /**
     * Fresh young objects CAS-ed into an array that a full GC has already promoted, with
     * allocation churn between the stores: the element's card (or remembered set) must keep each
     * one alive across the young collections the churn triggers.
     */
    static long refArrayOldToYoung() {
        Object[] old = new Object[64];
        System.gc();
        long sinkLocal = 0;
        for (int r = 0; r < old.length; r++) {
            REFS.compareAndSet(old, r, null, new Mark(r, r * 2));
            for (int c = 0; c < 2_000; c++) {
                sinkLocal += new int[8 + (c & 7)].length;
            }
        }
        System.gc();
        long acc = 0;
        for (Object o : old) {
            Mark m = (Mark) o;
            acc = acc * 7 + m.j * 3L + m.k;
        }
        sink += sinkLocal;
        return acc;
    }

    /** nio21 wave 6 L3: raw {@code Unsafe} int / long atomics at misaligned byte[] offsets. */
    @SuppressWarnings("removal")
    static long misalignedRound(sun.misc.Unsafe u, int k, byte[] b) {
        long base = u.arrayBaseOffset(byte[].class);
        long off = base + 1 + (k & 3);
        int cur = u.getInt(b, off);
        long acc = u.compareAndSwapInt(b, off, cur, cur + 0x0101_0101) ? 1 : 0;
        acc += u.compareAndSwapInt(b, off, cur, 0) ? 2 : 0;
        acc += u.getAndAddInt(b, off, k);
        acc += u.getAndSetInt(b, off, ~k);
        long loff = base + 3 + (k & 3);
        long lc = u.getLong(b, loff);
        acc += u.compareAndSwapLong(b, loff, lc, lc ^ 0x0102_0304_0506_0708L) ? 4 : 0;
        acc += u.getAndAddLong(b, loff, 1L << 33);
        return acc * 31 + Arrays.hashCode(b);
    }

    static void conformanceW6() {
        Holder h = new Holder();
        long acc = 0;
        for (int k = 0; k < 20_000; k++) {
            acc = acc * 7 + fieldRound(k, h);
        }
        System.out.println("conf fields " + acc + " " + h.pi + " " + h.pl + " " + h.ai + " " + h.ref);
        Object[] oa = new Object[4];
        AtomicReferenceArray<Mark> ara = new AtomicReferenceArray<>(4);
        Mark[] ms = new Mark[8];
        for (int i = 0; i < ms.length; i++) {
            ms[i] = new Mark(i * 11, i);
        }
        long acc2 = 0;
        for (int k = 0; k < 20_000; k++) {
            acc2 = acc2 * 5 + refArrayRound(k, oa, ara, ms);
        }
        System.out.println("conf ref-array " + acc2 + " " + refArrayOldToYoung());
        Object o = theUnsafe();
        if (o == null) {
            System.out.println("conf misaligned unavailable");
            return;
        }
        sun.misc.Unsafe u = (sun.misc.Unsafe) o;
        byte[] b = new byte[16];
        long acc3 = 0;
        for (int k = 0; k < 20_000; k++) {
            acc3 = acc3 * 3 + misalignedRound(u, k, b);
        }
        System.out.println("conf misaligned " + acc3 + " " + Arrays.toString(b));
    }

    /**
     * nio21 wave 7 L3: instance-field atomics through {@code sun.misc.Unsafe} (int and long),
     * field and array {@code compareAndExchange}, and the field updaters. Values only.
     */
    @SuppressWarnings("removal")
    static long fieldRoundW7(sun.misc.Unsafe u, long oa, long ob, int k, Cell c, int[] ia, long[] la) {
        long acc = 0;
        c.a = k;
        acc += u.compareAndSwapInt(c, oa, k, k + 9) ? 1 : 0;
        acc += u.compareAndSwapInt(c, oa, k, 0) ? 2 : 0;
        acc += u.getAndAddInt(c, oa, -k);
        acc += u.getAndSetInt(c, oa, k ^ 0x5A5A);
        acc += (int) C_A.compareAndExchange(c, k ^ 0x5A5A, k * 7);
        acc += (int) C_A.compareAndExchange(c, -1, 3);
        acc += (int) C_A.compareAndExchangeAcquire(c, k * 7, k + 1);
        acc = acc * 31 + c.a;
        c.b = (long) k << 36;
        acc += u.compareAndSwapLong(c, ob, (long) k << 36, ~(long) k) ? 1 : 0;
        acc += u.getAndAddLong(c, ob, 1L << 40);
        acc += u.getAndSetLong(c, ob, (long) k * k);
        acc += (long) C_B.compareAndExchange(c, (long) k * k, -7L);
        acc += (long) C_B.compareAndExchangeRelease(c, 0L, 1L);
        acc = acc * 31 + c.b;
        acc += C_AI.incrementAndGet(c);
        acc += C_AI.compareAndSet(c, C_AI.get(c), k) ? 1 : 0;
        acc += C_AL.getAndAdd(c, k);
        acc += C_AL.compareAndSet(c, C_AL.get(c), -k) ? 1 : 0;
        int i = k & 3;
        ia[i] = k;
        acc += (int) INTS.compareAndExchange(ia, i, k, k - 1);
        acc += (int) INTS.compareAndExchange(ia, i, k, 5);
        la[i] = k;
        acc += (long) LONGS.compareAndExchangeAcquire(la, i, (long) k, 1L << 50);
        return acc * 17 + c.ai + c.al + ia[i] + la[i];
    }

    static void conformanceW7() {
        Object o = theUnsafe();
        if (o == null) {
            System.out.println("conf fields-w7 unavailable");
            return;
        }
        sun.misc.Unsafe u = (sun.misc.Unsafe) o;
        long oa = cellOffset(u, "a");
        long ob = cellOffset(u, "b");
        if (oa < 0 || ob < 0) {
            System.out.println("conf fields-w7 unavailable");
            return;
        }
        Cell c = new Cell();
        int[] ia = new int[4];
        long[] la = new long[4];
        long acc = 0;
        for (int k = 0; k < 20_000; k++) {
            acc = acc * 7 + fieldRoundW7(u, oa, ob, k, c, ia, la);
        }
        System.out.println("conf fields-w7 " + acc + " " + c.a + " " + c.b + " " + c.ai + " " + c.al + " "
                + Arrays.toString(ia) + " " + Arrays.toString(la));
    }

    static void conformance() {
        int[] ia = new int[4];
        long[] la = new long[4];
        short[] sa = new short[4];
        byte[] ba = new byte[4];
        char[] ca = new char[4];
        boolean[] za = new boolean[4];
        float[] fa = new float[4];
        double[] da = new double[4];
        long acc = 0;
        for (int k = 0; k < 20_000; k++) {
            acc = acc * 7 + arrayRound(k, ia, la, sa, ba, ca, za, fa, da);
        }
        System.out.println("conf varhandle " + acc + " " + Arrays.toString(ia) + " " + Arrays.toString(la)
                + " " + Arrays.toString(sa) + " " + Arrays.toString(ba) + " "
                + Arrays.toString(new String(ca).chars().toArray()) + " " + Arrays.toString(za) + " "
                + Arrays.toString(fa) + " " + Arrays.toString(da));
        AtomicIntegerArray aia = new AtomicIntegerArray(8);
        AtomicLongArray ala = new AtomicLongArray(8);
        long acc2 = 0;
        for (int k = 0; k < 20_000; k++) {
            acc2 = acc2 * 5 + atomicArrayRound(k, aia, ala);
        }
        System.out.println("conf atomic-array " + acc2 + " " + aia + " " + ala);
        Object o = theUnsafe();
        if (o == null) {
            System.out.println("conf unsafe unavailable");
            return;
        }
        sun.misc.Unsafe u = (sun.misc.Unsafe) o;
        long acc3 = 0;
        int[] ub = new int[4];
        long[] ul = new long[4];
        float[] uf = new float[4];
        for (int k = 0; k < 20_000; k++) {
            acc3 = acc3 * 3 + unsafeRound(u, k, ub, ul, uf);
        }
        System.out.println("conf unsafe " + acc3 + " " + Arrays.toString(ub) + " " + Arrays.toString(ul) + " "
                + Arrays.toString(uf));
    }

    // ----------------------------------------------------------------- bench

    static long casRow(int[] a, int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) {
            int k = i & 7;
            int v = (int) INTS.getVolatile(a, k);
            acc += INTS.compareAndSet(a, k, v, v + 1) ? 1 : 0;
        }
        return acc;
    }

    static long xaddRow(int[] a, int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) {
            acc += (int) INTS.getAndAdd(a, i & 7, 1);
        }
        return acc;
    }

    static long xchgRow(int[] a, int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) {
            acc += (int) INTS.getAndSet(a, i & 7, i);
        }
        return acc;
    }

    static long lcasRow(long[] a, int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) {
            int k = i & 7;
            long v = (long) LONGS.getVolatile(a, k);
            acc += LONGS.compareAndSet(a, k, v, v + 3) ? 1 : 0;
        }
        return acc;
    }

    static long lxaddRow(long[] a, int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) {
            acc += (long) LONGS.getAndAdd(a, i & 7, 5L);
        }
        return acc;
    }

    static long borRow(int[] a, int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) {
            acc += (int) INTS.getAndBitwiseOr(a, i & 7, i);
        }
        return acc;
    }

    static long fcasRow(Holder h, int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) {
            int v = (int) H_PI.getVolatile(h);
            acc += H_PI.compareAndSet(h, v, v + 1) ? 1 : 0;
        }
        return acc;
    }

    static long fxaddRow(Holder h, int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) {
            acc += (int) H_PI.getAndAdd(h, 1);
        }
        return acc;
    }

    static long refcasRow(Object[] a, Mark[] ms, int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) {
            int k = i & 7;
            Object v = REFS.getVolatile(a, k);
            acc += REFS.compareAndSet(a, k, v, ms[i & 3]) ? 1 : 0;
        }
        return acc;
    }

    /** nio21 wave 6 L3's bench rows. */
    static void benchW6(int n) {
        int[] a = new int[8];
        Holder h = new Holder();
        Object[] oa = new Object[8];
        Mark[] ms = {new Mark(1, 1), new Mark(2, 2), new Mark(3, 3), new Mark(4, 4)};
        String[] names = {"bor", "fcas", "fxadd", "refcas"};
        for (int w = 0; w < 5; w++) {
            sink += borRow(a, n / 10) + fcasRow(h, n / 10) + fxaddRow(h, n / 10) + refcasRow(oa, ms, n / 10);
        }
        for (int r = 0; r < names.length; r++) {
            long best = Long.MAX_VALUE;
            for (int rep = 0; rep < 5; rep++) {
                long t0 = System.nanoTime();
                switch (r) {
                    case 0: sink += borRow(a, n); break;
                    case 1: sink += fcasRow(h, n); break;
                    case 2: sink += fxaddRow(h, n); break;
                    default: sink += refcasRow(oa, ms, n); break;
                }
                best = Math.min(best, System.nanoTime() - t0);
            }
            System.out.println(names[r] + " " + (best / n) + " ns/op");
        }
    }

    // nio21 wave 7 L3: instance-field and compareAndExchange bench rows.

    @SuppressWarnings("removal")
    static long ufcasRow(sun.misc.Unsafe u, long off, Cell c, int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) {
            int v = u.getIntVolatile(c, off);
            acc += u.compareAndSwapInt(c, off, v, v + 1) ? 1 : 0;
        }
        return acc;
    }

    @SuppressWarnings("removal")
    static long ufxaddRow(sun.misc.Unsafe u, long off, Cell c, int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) {
            acc += u.getAndAddInt(c, off, 1);
        }
        return acc;
    }

    static long lfcasRow(Cell c, int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) {
            long v = (long) C_B.getVolatile(c);
            acc += C_B.compareAndSet(c, v, v + 3) ? 1 : 0;
        }
        return acc;
    }

    static long fcaeRow(Cell c, int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) {
            int v = (int) C_A.getVolatile(c);
            acc += (int) C_A.compareAndExchange(c, v, v + 1);
        }
        return acc;
    }

    static long caeRow(int[] a, int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) {
            int k = i & 7;
            int v = (int) INTS.getVolatile(a, k);
            acc += (int) INTS.compareAndExchange(a, k, v, v + 1);
        }
        return acc;
    }

    static long ifuRow(Cell c, int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) {
            acc += C_AI.incrementAndGet(c);
        }
        return acc;
    }

    static long lfuRow(Cell c, int n) {
        long acc = 0;
        for (int i = 0; i < n; i++) {
            acc += C_AL.getAndAdd(c, 3L);
        }
        return acc;
    }

    /** nio21 wave 7 L3's bench rows. */
    static void benchW7(int n) {
        Object o = theUnsafe();
        sun.misc.Unsafe u = o == null ? null : (sun.misc.Unsafe) o;
        long off = u == null ? -1 : cellOffset(u, "c");
        Cell c = new Cell();
        int[] a = new int[8];
        String[] names = {"ufcas", "ufxadd", "lfcas", "fcae", "cae", "ifu", "lfu"};
        for (int w = 0; w < 5; w++) {
            if (off >= 0) {
                sink += ufcasRow(u, off, c, n / 10) + ufxaddRow(u, off, c, n / 10);
            }
            sink += lfcasRow(c, n / 10) + fcaeRow(c, n / 10) + caeRow(a, n / 10) + ifuRow(c, n / 10)
                    + lfuRow(c, n / 10);
        }
        for (int r = 0; r < names.length; r++) {
            if (r < 2 && off < 0) {
                System.out.println(names[r] + " unavailable");
                continue;
            }
            long best = Long.MAX_VALUE;
            for (int rep = 0; rep < 5; rep++) {
                long t0 = System.nanoTime();
                switch (r) {
                    case 0: sink += ufcasRow(u, off, c, n); break;
                    case 1: sink += ufxaddRow(u, off, c, n); break;
                    case 2: sink += lfcasRow(c, n); break;
                    case 3: sink += fcaeRow(c, n); break;
                    case 4: sink += caeRow(a, n); break;
                    case 5: sink += ifuRow(c, n); break;
                    default: sink += lfuRow(c, n); break;
                }
                best = Math.min(best, System.nanoTime() - t0);
            }
            System.out.println(names[r] + " " + (best / n) + " ns/op");
        }
    }

    static void bench() {
        int n = Integer.getInteger("nio21.iters", 2_000_000);
        int[] a = new int[8];
        long[] l = new long[8];
        String[] names = {"cas", "xadd", "xchg", "lcas", "lxadd"};
        for (int w = 0; w < 5; w++) {
            sink += casRow(a, n / 10) + xaddRow(a, n / 10) + xchgRow(a, n / 10) + lcasRow(l, n / 10)
                    + lxaddRow(l, n / 10);
        }
        for (int r = 0; r < names.length; r++) {
            long best = Long.MAX_VALUE;
            for (int rep = 0; rep < 5; rep++) {
                long t0 = System.nanoTime();
                switch (r) {
                    case 0: sink += casRow(a, n); break;
                    case 1: sink += xaddRow(a, n); break;
                    case 2: sink += xchgRow(a, n); break;
                    case 3: sink += lcasRow(l, n); break;
                    default: sink += lxaddRow(l, n); break;
                }
                best = Math.min(best, System.nanoTime() - t0);
            }
            System.out.println(names[r] + " " + (best / n) + " ns/op");
        }
        benchW6(n);
        benchW7(n);
    }

    public static void main(String[] args) throws Exception {
        String rows = System.getProperty("nio21.rows", "all");
        if (rows.equals("bench")) {
            bench();
            return;
        }
        if (rows.equals("all") || rows.equals("conf")) {
            conformance();
            conformanceW6();
            conformanceW7();
        }
        if (rows.equals("all") || rows.equals("race")) {
            races();
        }
    }
}
