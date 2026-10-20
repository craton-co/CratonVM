// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.ByteArrayOutputStream;
import java.io.InputStream;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.function.BooleanSupplier;

/**
 * gcd d10/r, the probe {@code Gcd1SideTableStaticsProbe} could not be: it puts
 * the user loaders at the END of a long old-generation linked list, so G1's
 * concurrent marker reaches them only after the cycle has been running for a
 * while -- after a young pause, if the mutator keeps allocating.
 *
 * <p>Page:
 * {@code docs/internal/gc/gcd-d10r-g1-mid-cycle-young-pause-drops-the-metadata-pin-rows-FIXED-20261004.md}.
 * A user-loader class's static values are not roots on G1's licensed
 * (initial-mark, remark) scans; they are {@code metadata_pin} rows under the
 * loader, which the marker follows when it scans the loader. An unlicensed
 * scan -- every young pause -- used to drop this VM's rows. A loader the marker
 * reaches after such a pause was scanned with no row, its statics stayed
 * unmarked, and cleanup freed a humongous static's region while the static
 * still named it. {@code Gcd1SideTableStaticsProbe} kept its holders in a
 * stack local, so the marker reached every loader straight after the initial
 * mark, before any young pause, and printed HotSpot's line on the base build.
 *
 * <p>The probe: {@value #LOADERS} throwaway loaders each define {@link Holder},
 * whose class initialiser fills a {@code long[]} just under 1 MiB (humongous on
 * G1) and a small {@code long[]} with a pattern keyed by the loader's seed. The
 * holder instances are reachable ONLY through the tail of a {@code depth}-node
 * linked list (default 1 500 000) that the first collections promote. Each
 * round replaces a quarter of 64 MiB of humongous ballast (so a freed
 * humongous region is re-issued, filled with {@code -1}), churns young garbage,
 * then walks the list and asks every holder to verify its statics.
 *
 * <p>Deterministic stdout (HotSpot 25, {@code -XX:+UseG1GC} and
 * {@code -XX:+UseSerialGC}, {@code -Xmx512m}):
 * <pre>
 *   deep-statics loaders=16 rounds=48 corrupt=0
 * </pre>
 * Stderr (not compared): {@code [probe] first_corrupt_round=R}, {@code -1}
 * when every check passed. {@code corrupt=} other than 0, or a crash, is the
 * defect.
 * <pre>
 *   javac -d tools/bench tools/bench/Gcd10rDeepStaticsProbe.java
 *   cratonvm --java-home "$JDK" -XX:+UseG1GC -Xmx512m -XX:InitiatingHeapOccupancyPercent=20 \
 *       -cp tools/bench Gcd10rDeepStaticsProbe
 * </pre>
 * Usage: {@code Gcd10rDeepStaticsProbe [rounds] [depth]}.
 */
public final class Gcd10rDeepStaticsProbe {

    static final int LOADERS = 16;
    // Just under 1 MiB with its header: one 1 MiB G1 region, and humongous.
    public static final int TABLE_LONGS = (1 << 17) - 64;
    public static final int SMALL_LONGS = 64;
    static final int BALLAST_SLOTS = 64; // 64 x ~1 MiB humongous
    static final int CHURN_MIB = 32;

    /** Seeds handed to each loader's {@link Holder} class initialiser, in order. */
    public static final AtomicInteger SEEDS = new AtomicInteger(1);

    public static long pattern(int seed, int i) {
        long x = (seed * 0x9E3779B97F4A7C15L) ^ (i * 0xC2B2AE3D27D4EB4FL);
        return x ^ (x >>> 31);
    }

    /** Defined once per throwaway loader: statics reachable only as statics. */
    public static final class Holder implements BooleanSupplier {
        static final int SEED = Gcd10rDeepStaticsProbe.SEEDS.getAndIncrement();
        static final long[] TABLE = fill(new long[TABLE_LONGS]);
        static final long[] SMALL = fill(new long[SMALL_LONGS]);

        static long[] fill(long[] a) {
            for (int i = 0; i < a.length; i++) {
                a[i] = Gcd10rDeepStaticsProbe.pattern(SEED, i);
            }
            return a;
        }

        static boolean intact(long[] a, int length) {
            if (a == null || a.length != length) {
                return false;
            }
            for (int i = 0; i < a.length; i++) {
                if (a[i] != Gcd10rDeepStaticsProbe.pattern(SEED, i)) {
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
            super(Gcd10rDeepStaticsProbe.class.getClassLoader());
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

    /** One link of the list the marker must walk before it reaches the loaders. */
    static final class Node {
        Node next;
        Object payload;
    }

    static byte[] readClassBytes(String binaryName) throws Exception {
        String res = binaryName.replace('.', '/') + ".class";
        try (InputStream in = Gcd10rDeepStaticsProbe.class.getClassLoader().getResourceAsStream(res)) {
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
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 48;
        int depth = args.length > 1 ? Integer.parseInt(args[1]) : 1_500_000;
        Node head = chain(depth, define());
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
            int bad = check(head);
            corrupt += bad;
            if (bad > 0 && firstCorruptRound < 0) {
                firstCorruptRound = r;
            }
        }
        sink = null;
        System.out.println("deep-statics loaders=" + LOADERS + " rounds=" + rounds
                + " corrupt=" + corrupt);
        System.err.println("[probe] first_corrupt_round=" + firstCorruptRound);
    }

    /** {@code depth} nodes; only the last one holds the holders. */
    static Node chain(int depth, BooleanSupplier[] holders) {
        Node head = new Node();
        Node cur = head;
        for (int i = 1; i < depth; i++) {
            Node n = new Node();
            cur.next = n;
            cur = n;
        }
        cur.payload = holders;
        return head;
    }

    /** Walk to the tail and count the holders whose statics read back wrong. */
    static int check(Node head) {
        Node cur = head;
        while (cur.next != null) {
            cur = cur.next;
        }
        int bad = 0;
        for (BooleanSupplier h : (BooleanSupplier[]) cur.payload) {
            if (!h.getAsBoolean()) {
                bad++;
            }
        }
        return bad;
    }

    /** One loader per holder; each holder is the only strong path to its class. */
    static BooleanSupplier[] define() throws Exception {
        String name = Gcd10rDeepStaticsProbe.class.getName() + "$Holder";
        byte[] bytes = readClassBytes(name);
        BooleanSupplier[] holders = new BooleanSupplier[LOADERS];
        for (int i = 0; i < LOADERS; i++) {
            Class<?> c = new Throwaway(name, bytes).loadClass(name);
            if (c.getClassLoader() == Gcd10rDeepStaticsProbe.class.getClassLoader()) {
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
