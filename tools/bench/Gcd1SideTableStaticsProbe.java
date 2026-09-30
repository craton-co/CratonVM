// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.ByteArrayOutputStream;
import java.io.InputStream;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.function.BooleanSupplier;

/**
 * gcd d10/r (2026-09-28): does a user-loader class's STATIC value, reachable
 * only through the static field, survive a concurrent old-generation cycle
 * during which young collections run?
 *
 * <p>Page:
 * {@code docs/known-issues/gc/gcd-d10r-g1-mid-cycle-young-pause-drops-the-metadata-pin-rows-20260928.md}.
 * With class unloading on (the default), a root scan LICENSED to treat a
 * user loader's metadata as conditional does not root that loader's static
 * values; it files them in the {@code metadata_pin} side table under the
 * loader, and the concurrent marker follows the row when it scans the loader.
 * On G1 only the initial-mark and remark scans are licensed. Every other root
 * scan -- the young pauses that run while the cycle's marker is still
 * tracing -- DROPS this VM's rows ({@code vm/src/memory/roots.rs::collect_roots},
 * {@code set_metadata_weak_mode(vm, false)} and
 * {@code replace_metadata_pins(vm, &[])}), and roots the statics for that pause
 * only. A loader the marker reaches after such a pause is scanned with no row,
 * so its static values are never marked this cycle; the remark rebuilds the
 * rows but does not rescan a loader that is already marked. A HUMONGOUS static
 * then sits in a region the cleanup judges empty and frees while the static
 * still names it.
 *
 * <p>The probe: {@value #LOADERS} throwaway loaders each define {@link Holder},
 * whose class initialiser fills a {@value #TABLE_LONGS}-element {@code long[]}
 * (just under 1 MiB: one humongous region on G1 at {@code -Xmx256m}) and a small {@code long[]}
 * with a pattern keyed by the loader's seed. Only one {@code Holder} INSTANCE
 * per loader is kept (it keeps its class and loader alive); nothing but the
 * static fields refers to the arrays. The rounds keep about 96 MiB of
 * old-generation ballast (humongous garbage of the same size, replaced a
 * quarter at a time, so freed humongous regions are re-issued), churn young
 * garbage, and after every round ask every holder to verify its statics.
 *
 * <p>Deterministic stdout -- HotSpot ({@code java -XX:+UseSerialGC -Xmx256m
 * -cp tools/bench Gcd1SideTableStaticsProbe}, and {@code -XX:+UseG1GC}) prints:
 * <pre>
 *   sidetable-statics loaders=24 rounds=32 corrupt=0
 * </pre>
 * Stderr (not compared): {@code [probe] first_corrupt_round=R} when a check
 * failed ({@code -1} otherwise). A VM crash, or {@code corrupt=} other than 0,
 * is the defect.
 * <pre>
 *   javac -d tools/bench tools/bench/Gcd1SideTableStaticsProbe.java
 *   cratonvm --java-home "$JDK" -XX:+UseG1GC -Xmx256m -cp tools/bench Gcd1SideTableStaticsProbe
 * </pre>
 * Usage: {@code Gcd1SideTableStaticsProbe [rounds]} (default 32).
 */
public final class Gcd1SideTableStaticsProbe {

    static final int LOADERS = 24;
    // Just under 1 MiB with its header: one 1 MiB G1 region, and humongous.
    public static final int TABLE_LONGS = (1 << 17) - 64;
    public static final int SMALL_LONGS = 64;
    static final int BALLAST_SLOTS = 96; // 96 x ~1 MiB humongous
    static final int CHURN_MIB = 24;

    /** Seeds handed to each loader's {@link Holder} class initialiser, in order. */
    public static final AtomicInteger SEEDS = new AtomicInteger(1);

    public static long pattern(int seed, int i) {
        long x = (seed * 0x9E3779B97F4A7C15L) ^ (i * 0xC2B2AE3D27D4EB4FL);
        return x ^ (x >>> 31);
    }

    /** Defined once per throwaway loader: statics reachable only as statics. */
    public static final class Holder implements BooleanSupplier {
        static final int SEED = Gcd1SideTableStaticsProbe.SEEDS.getAndIncrement();
        static final long[] TABLE = fill(new long[TABLE_LONGS]);
        static final long[] SMALL = fill(new long[SMALL_LONGS]);

        static long[] fill(long[] a) {
            for (int i = 0; i < a.length; i++) {
                a[i] = Gcd1SideTableStaticsProbe.pattern(SEED, i);
            }
            return a;
        }

        static boolean intact(long[] a, int length) {
            if (a == null || a.length != length) {
                return false;
            }
            for (int i = 0; i < a.length; i++) {
                if (a[i] != Gcd1SideTableStaticsProbe.pattern(SEED, i)) {
                    return false;
                }
            }
            return true;
        }

        /** Are both statics still the arrays the initialiser filled? */
        @Override
        public boolean getAsBoolean() {
            return intact(TABLE, TABLE_LONGS) && intact(SMALL, SMALL_LONGS);
        }
    }

    /** Defines exactly one name itself and delegates everything else. */
    static final class Throwaway extends ClassLoader {
        private final String target;
        private final byte[] bytes;

        Throwaway(String target, byte[] bytes) {
            super(Gcd1SideTableStaticsProbe.class.getClassLoader());
            this.target = target;
            this.bytes = bytes;
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            if (name.equals(target)) {
                synchronized (this) {
                    Class<?> c = findLoadedClass(name);
                    if (c == null) {
                        c = defineClass(name, bytes, 0, bytes.length);
                    }
                    if (resolve) {
                        resolveClass(c);
                    }
                    return c;
                }
            }
            return super.loadClass(name, resolve);
        }
    }

    static byte[] readClassBytes(String binaryName) throws Exception {
        String res = binaryName.replace('.', '/') + ".class";
        try (InputStream in = Gcd1SideTableStaticsProbe.class.getClassLoader().getResourceAsStream(res)) {
            if (in == null) {
                throw new IllegalStateException("class bytes not on the classpath: " + res);
            }
            ByteArrayOutputStream out = new ByteArrayOutputStream();
            byte[] buf = new byte[4096];
            int n;
            while ((n = in.read(buf)) > 0) {
                out.write(buf, 0, n);
            }
            return out.toByteArray();
        }
    }

    static volatile Object sink;

    public static void main(String[] args) throws Exception {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 32;
        BooleanSupplier[] holders = define();
        long[][] ballast = new long[BALLAST_SLOTS][];
        for (int s = 0; s < BALLAST_SLOTS; s++) {
            ballast[s] = garbage();
        }
        int corrupt = 0;
        int firstCorruptRound = -1;
        int next = 0;
        for (int r = 0; r < rounds; r++) {
            // A quarter of the ballast becomes garbage and is re-issued, so a
            // freed humongous region is handed out again within the round.
            for (int k = 0; k < BALLAST_SLOTS / 4; k++) {
                ballast[next] = garbage();
                next = (next + 1) % BALLAST_SLOTS;
                churn(CHURN_MIB / 8);
            }
            churn(CHURN_MIB);
            for (BooleanSupplier h : holders) {
                if (!h.getAsBoolean()) {
                    corrupt++;
                    if (firstCorruptRound < 0) {
                        firstCorruptRound = r;
                    }
                }
            }
        }
        sink = null;
        System.out.println("sidetable-statics loaders=" + LOADERS + " rounds=" + rounds
                + " corrupt=" + corrupt);
        System.err.println("[probe] first_corrupt_round=" + firstCorruptRound);
    }

    /** One loader per holder; each holder is the only strong path to its class. */
    static BooleanSupplier[] define() throws Exception {
        String name = Gcd1SideTableStaticsProbe.class.getName() + "$Holder";
        byte[] bytes = readClassBytes(name);
        BooleanSupplier[] holders = new BooleanSupplier[LOADERS];
        for (int i = 0; i < LOADERS; i++) {
            Class<?> c = new Throwaway(name, bytes).loadClass(name);
            if (c.getClassLoader() == Gcd1SideTableStaticsProbe.class.getClassLoader()) {
                throw new IllegalStateException("the holder was not defined by its own loader");
            }
            holders[i] = (BooleanSupplier) c.getDeclaredConstructor().newInstance();
            if (!holders[i].getAsBoolean()) {
                throw new IllegalStateException("holder " + i + " read back wrong at definition");
            }
        }
        return holders;
    }

    /** A ~1 MiB array with a pattern no holder uses (every word -1). */
    static long[] garbage() {
        long[] a = new long[TABLE_LONGS];
        java.util.Arrays.fill(a, -1L);
        return a;
    }

    /** About {@code mib} MiB of short-lived young garbage. */
    static void churn(int mib) {
        int objects = mib << 10; // long[126] = 1 KiB each
        for (int k = 0; k < objects; k++) {
            sink = new long[126];
        }
    }
}
