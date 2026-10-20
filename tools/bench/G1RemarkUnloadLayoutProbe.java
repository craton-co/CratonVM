// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.ByteArrayOutputStream;
import java.io.InputStream;
import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.lang.ref.WeakReference;
import java.util.function.Supplier;

/**
 * G1's final remark unloads a class whose DEAD instances it leaves in place.
 *
 * <p>Page: {@code docs/internal/gc/gengc-r5w3-unload7-g1-remark-may-unregister-layouts-of-unswept-instances-FIXED-20261005.md}.
 * The remark runs the class-unload transaction, and {@code cleanup} frees only
 * the regions that are wholly dead: a dead instance of the unloaded class in a
 * region that also holds live objects stays where it is until a mixed
 * collection evacuates that region. Every linear walk of such a region in
 * between -- a young pause's dirty-card scan over it above all -- has to size
 * that dead instance, and a compact instance is sized from its class's layout.
 *
 * <ol>
 *   <li>defines {@link Payload} in a throwaway loader and builds one long
 *       chain HOLDER -> PAYLOAD -> HOLDER -> ..., so every copy the young
 *       collections make lays the two kinds out interleaved, and churns until
 *       the chain is in the old generation;</li>
 *   <li>cuts the chain (every holder stays reachable from an array, every
 *       payload, the loader and the class become garbage), and keeps a
 *       {@value #BALLAST_MIB} MiB ballast live so the old generation is past
 *       G1's initiating occupancy;</li>
 *   <li>churns young garbage while storing a young object into a holder every
 *       {@value #STORE_EVERY} allocations, so every young pause scans dirty
 *       cards across the regions the dead payloads sit in; until the weak
 *       reference to the loader is cleared (the remark unloaded it) and then
 *       {@value #TAIL_ROUNDS} rounds more.</li>
 * </ol>
 *
 * <p>Deterministic stdout (HotSpot {@code -XX:+UseG1GC -Xmx256m} prints the same):
 * <pre>
 *   g1-remark-unload loader-unloaded=true holders-ok=true
 * </pre>
 * Stderr, CratonVM with {@code CRATONVM_DBG=gc-stats}: {@code [GC] g1 ...
 * source_walk_desync=0} (a dirty-card walk that could not size a dead payload
 * abandons its region: before the fix that count rose and the young objects
 * only the rest of that region referenced were freed while reachable) and
 * {@code [GC] unload_layouts: classes_unloaded>=1 layouts_retained>=1
 * g1_census_incomplete=0}, with {@code layouts_released>=1} once a mixed
 * collection has evacuated the payloads' regions.
 * <pre>
 *   javac -d tools/bench tools/bench/G1RemarkUnloadLayoutProbe.java
 *   java -XX:+UseG1GC -Xmx256m -cp tools/bench G1RemarkUnloadLayoutProbe
 *   CRATONVM_DBG=gc-stats cratonvm --java-home "$JDK" -XX:+UseG1GC -Xmx256m -cp tools/bench G1RemarkUnloadLayoutProbe
 * </pre>
 * Usage: {@code G1RemarkUnloadLayoutProbe [maxRounds]} (default 80).
 */
public final class G1RemarkUnloadLayoutProbe {

    static final int PAIRS = 300_000;
    static final int BALLAST_MIB = 110;
    static final int CHURN_MIB = 48;
    static final int STORE_EVERY = 64;
    static final int TAIL_ROUNDS = 12;

    /** The unloadable class. References nothing of the probe, so it links in
     *  its own loader; an instance makes the next one. */
    public static final class Payload implements Supplier<Object> {
        long a;
        long b;
        Object next;

        public Payload() {
            a = 0x5A5A5A5AL;
            b = ~a;
        }

        @Override
        public Object get() {
            return new Payload();
        }
    }

    /** Lives in the application loader and outlives every payload. */
    static final class Holder {
        Object next;
        long check;
        Object young;

        Holder(long check) {
            this.check = check;
        }
    }

    static final class Throwaway extends ClassLoader {
        private final String target;
        private final byte[] bytes;

        Throwaway(String target, byte[] bytes) {
            super(G1RemarkUnloadLayoutProbe.class.getClassLoader());
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
        try (InputStream in = G1RemarkUnloadLayoutProbe.class.getClassLoader().getResourceAsStream(res)) {
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

    static Holder chainHead;
    static Holder[] holders;
    static long[][] ballast;
    static volatile Object sink;
    static WeakReference<ClassLoader> loaderRef;
    /** `Payload.next`, while the chain is built and cut; nulled after. */
    static java.lang.reflect.Field nextField;

    public static void main(String[] args) throws Exception {
        int maxRounds = args.length > 0 ? Integer.parseInt(args[0]) : 80;
        String name = G1RemarkUnloadLayoutProbe.class.getName() + "$Payload";
        build(name, readClassBytes(name));
        promote();
        cut();
        long[] outcome = drive(maxRounds);
        boolean ok = verify();
        System.out.println("g1-remark-unload loader-unloaded=" + loaderRef.refersTo(null)
                + " holders-ok=" + ok);
        System.err.println("[probe] unload_round=" + outcome[0] + " rounds=" + outcome[1]
                + " gcs=" + gcCount());
        ballast = null;
    }

    /** The loader, its class and the interleaved chain, in a frame of their
     *  own so no dead local outlives it. */
    static void build(String name, byte[] bytes) throws Exception {
        Throwaway loader = new Throwaway(name, bytes);
        Class<?> c = loader.loadClass(name);
        if (c.getClassLoader() != loader) {
            throw new IllegalStateException("the payload was not defined by its own loader");
        }
        @SuppressWarnings("unchecked")
        Supplier<Object> factory = (Supplier<Object>) c.getDeclaredConstructor().newInstance();
        loaderRef = new WeakReference<>(loader);
        nextField = c.getDeclaredField("next");
        nextField.setAccessible(true);
        Holder head = new Holder(0);
        Holder h = head;
        for (int i = 1; i <= PAIRS; i++) {
            Object p = factory.get();
            Holder next = new Holder(i);
            h.next = p;
            nextField.set(p, next);
            h = next;
        }
        chainHead = head;
    }

    /** Young garbage until the chain is tenured; touches none of it. */
    static void promote() {
        for (int pass = 0; pass < 24; pass++) {
            for (int k = 0; k < 65536; k++) {
                sink = new long[64];
            }
        }
        sink = null;
    }

    /** Every holder into an array, every holder->payload edge cut: the
     *  payloads, their class and their loader are garbage now. */
    static void cut() {
        holders = new Holder[PAIRS + 1];
        Object o = chainHead;
        int i = 0;
        while (o != null) {
            Holder h = (Holder) o;
            holders[i++] = h;
            Object p = h.next;
            h.next = null;
            o = p == null ? null : nextOf(p);
        }
        if (i != PAIRS + 1) {
            throw new IllegalStateException("chain length " + i);
        }
        chainHead = null;
        nextField = null;
        ballast = new long[BALLAST_MIB][];
        for (int k = 0; k < BALLAST_MIB; k++) {
            ballast[k] = new long[(1 << 20) / 8 - 4];
        }
    }

    static Object nextOf(Object payload) {
        try {
            return nextField.get(payload);
        } catch (ReflectiveOperationException e) {
            throw new IllegalStateException(e);
        }
    }

    /** {unload round or -1, rounds run}. */
    static long[] drive(int maxRounds) {
        final int churn = CHURN_MIB << 10; // long[126] = 1 KiB
        int unloadRound = -1;
        int rounds = 0;
        int cursor = 0;
        for (; rounds < maxRounds; rounds++) {
            for (int k = 0; k < churn; k++) {
                sink = new long[126];
                if ((k % STORE_EVERY) == 0) {
                    holders[cursor].young = new long[] {cursor};
                    cursor = (cursor + 997) % holders.length;
                }
            }
            if (unloadRound < 0 && loaderRef.refersTo(null)) {
                unloadRound = rounds;
            }
            if (unloadRound >= 0 && rounds - unloadRound >= TAIL_ROUNDS) {
                rounds++;
                break;
            }
        }
        sink = null;
        return new long[] {unloadRound, rounds};
    }

    /** Every holder still reads back its own index, and every young object a
     *  holder names still reads back the index it was stored at. */
    static boolean verify() {
        for (int i = 0; i < holders.length; i++) {
            Holder h = holders[i];
            if (h == null || h.check != i || h.next != null) {
                return false;
            }
            if (h.young != null && (((long[]) h.young).length != 1 || ((long[]) h.young)[0] != i)) {
                return false;
            }
        }
        return true;
    }

    static long gcCount() {
        long n = 0;
        for (GarbageCollectorMXBean gc : ManagementFactory.getGarbageCollectorMXBeans()) {
            n += Math.max(0, gc.getCollectionCount());
        }
        return n;
    }
}
