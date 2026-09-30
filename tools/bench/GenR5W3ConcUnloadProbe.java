// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.ByteArrayOutputStream;
import java.io.InputStream;
import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.lang.ref.WeakReference;
import java.util.function.Supplier;

/**
 * gen r5w3/unload7 (2026-09-26), reworked gen r5w4/conc8: the generational
 * CONCURRENT cycle unloads a dead class loader, and does not unload a live one
 * whose only instances are young.
 *
 * <p>Page: {@code docs/known-issues/gc/gengc-r5w1-refs5-concurrent-cycle-cannot-unload-classes-20260926.md}.
 * Under {@code CRATONVM_GEN_CONC_CLASS_UNLOAD=1} (with
 * {@code CRATONVM_GEN_CONC_REMARK_REFPROC_HOOK=1}) the concurrent cycle's
 * initial mark and remark defer user-loader mirrors and statics to their
 * loader, the marker follows the loader side tables, and the remark unloads the
 * classes of every loader nothing reached.
 *
 * <p>Wave 3's version never opened a concurrent cycle: its "promotion ring"
 * retained 16 MiB of blocks for about 25 MiB of allocation, less than one
 * young semi-space (64 MiB at {@code -Xmx256m}), so every block died young and
 * the old generation never reached the concurrent start (45 % of it before
 * the first cycle is measured, {@code ConcurrentStartPolicy::threshold}). HotSpot
 * Serial, for the same reason, ran no full collection and printed
 * {@code false} too. This version drives the old generation directly:
 *
 * <ol>
 *   <li>defines the same class ({@link Payload}) in two throwaway loaders,
 *       DEAD and CONTROL, and keeps both loaders, both classes, an instance of
 *       each and a weak reference to each loader and class strongly reachable
 *       while it churns enough young garbage for all of them to be PROMOTED
 *       (so the concurrent cycle, which judges only old objects, is the
 *       collector that must decide);</li>
 *   <li>drops every strong reference to DEAD; for CONTROL it keeps only ONE
 *       instance, replaced by a fresh one (allocated by the instance itself)
 *       every 256 allocations, so the instance that keeps CONTROL's loader
 *       alive is always YOUNG — the case the concurrent trace cannot see
 *       without the young-instance loader roots (hazard 2 on the page);</li>
 *   <li>each round replaces ONE {@value #BALLAST_MIB} MiB {@code long[]} (the
 *       "ballast") and then allocates {@value #CHURN_MIB} MiB of young
 *       garbage, until both of DEAD's weak references are cleared or the round
 *       budget runs out. No {@code System.gc()}.</li>
 * </ol>
 *
 * <h2>Why that reaches the concurrent start deterministically</h2>
 *
 * On CratonVM at {@code -Xmx256m} the generational heap has a 64 MiB young
 * semi-space and a 128 MiB old generation ({@code GenerationalHeap::with_capacity}).
 * An array larger than half a young semi-space (32 MiB) is humongous and is
 * placed straight in the old generation ({@code HUMONGOUS_YOUNG_FRACTION_PERCENT}).
 * So round 0 puts one 34 MiB ballast in the old generation (about 30 % of it
 * with what start-up promoted) and round 1 a second one while the first is
 * garbage (about 57 %): past the 45 % initiating occupancy the first cycle
 * uses, and below both the 75 % stop-the-world floor and the 90 % deferral
 * ceiling. The {@value #CHURN_MIB} MiB of churn that follows forces at least
 * one young collection, whose epilogue (the allocation-failure door for
 * compiled code, {@code maybe_gc} for interpreted) asks the concurrent trigger:
 * the cycle opens with DEAD already unreachable, and its remark unloads DEAD.
 * No environment knob is needed; see the page for the one that makes LATER
 * cycles deterministic too ({@code CRATONVM_GC_CONC_START_PERCENT}).
 *
 * <p>On HotSpot Serial the ballast either does not fit eden (initial heap) and
 * is allocated in the tenured generation, or does not fit a survivor space and
 * is promoted by the young collection that finds it live. Either way every
 * round leaves 34 MiB of garbage in the tenured generation, which fills within
 * a handful of rounds; the full collection that follows unloads DEAD and cannot
 * unload CONTROL (its young instance's klass is a strong root of the young
 * collection and a live object of the full one).
 *
 * <p>Deterministic stdout (HotSpot {@code -XX:+UseSerialGC -Xmx256m} prints the
 * same):
 * <pre>
 *   conc-unload dead-loader-unloaded=true dead-class-unloaded=true control-loader-alive=true control-ok=true
 * </pre>
 * Stderr (not compared): {@code [probe] rounds=R old_collections_at_unload=N},
 * where {@code R} is the 0-based round whose end first saw DEAD unloaded and
 * {@code N} the old-generation ("MarkSweepCompact") collector bean's count
 * then. On CratonVM with the flags on, the evidence that the CONCURRENT cycle
 * did it is {@code N=0} (expected {@code R} 1 or 2) plus, under
 * {@code CRATONVM_DBG=gc-stats}, {@code [GC] conc_unload: concunload_remarks>=1
 * ... concunload_loaders>=1 concunload_classes>=1 concunload_layouts_retained>=1}
 * and {@code [GC] conc_driver: ... concdrv_cycles_completed>=1}. Without the
 * flags DEAD is unloaded only by a stop-the-world major ({@code N>=1}), or not
 * within the budget.
 *
 * <p>The loop runs {@value #TAIL_ROUNDS} rounds past the unload so that a later
 * concurrent cycle can release the compact layout the unloading remark
 * retained (gen r5w4/conc8). The first cycle raises the adaptive start close
 * to the stop-the-world floor, so to make those later cycles concurrent too add
 * {@code CRATONVM_GC_CONC_START_PERCENT=40}; then the {@code [GC] conc_unload:}
 * line also shows {@code concunload_layout_censuses>=1 concunload_layouts_released>=1}
 * (and {@code released <= retained}).
 * <pre>
 *   javac -d tools/bench tools/bench/GenR5W3ConcUnloadProbe.java
 *   java -XX:+UseSerialGC -Xmx256m -cp tools/bench GenR5W3ConcUnloadProbe
 *   CRATONVM_GEN_CONC_CLASS_UNLOAD=1 CRATONVM_GEN_CONC_REMARK_REFPROC_HOOK=1 CRATONVM_DBG=gc-stats \
 *     cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m -cp tools/bench GenR5W3ConcUnloadProbe
 * </pre>
 * The sizes are for {@code -Xmx256m}: a larger heap has a larger young
 * semi-space, and the ballast is then young on CratonVM too.
 * Usage: {@code GenR5W3ConcUnloadProbe [maxRounds]} (default 64).
 */
public final class GenR5W3ConcUnloadProbe {

    /** One ballast array, in MiB: above half of a 64 MiB young semi-space. */
    static final int BALLAST_MIB = 34;
    /** Young garbage per round, in MiB: more than one young generation. */
    static final int CHURN_MIB = 80;
    /** Rounds run after the one that first saw DEAD unloaded. */
    static final int TAIL_ROUNDS = 4;

    /** The class both throwaway loaders define. It references nothing of the
     *  probe (no outer-class member, no private access), so it links in a
     *  loader of its own; its instances make their successors. */
    public static final class Payload implements Supplier<Object> {
        final int id;
        final long check;
        final long[] body = new long[4];

        public Payload() {
            this(0);
        }

        Payload(int id) {
            this.id = id;
            this.check = mix(id);
            for (int i = 0; i < body.length; i++) {
                body[i] = check ^ i;
            }
        }

        static long mix(int id) {
            long x = id * 0x9E3779B97F4A7C15L;
            return x ^ (x >>> 29);
        }

        @Override
        public Object get() {
            return new Payload((id + 1) & 0xFFFF);
        }

        @Override
        public String toString() {
            if (check != mix(id)) {
                return "corrupt";
            }
            for (int i = 0; i < body.length; i++) {
                if (body[i] != (check ^ i)) {
                    return "corrupt";
                }
            }
            return "ok";
        }
    }

    /** Defines exactly one name itself and delegates everything else. */
    static final class Throwaway extends ClassLoader {
        private final String target;
        private final byte[] bytes;

        Throwaway(String target, byte[] bytes) {
            super(GenR5W3ConcUnloadProbe.class.getClassLoader());
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
        try (InputStream in = GenR5W3ConcUnloadProbe.class.getClassLoader().getResourceAsStream(res)) {
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

    // Strong holders during the promotion phase; DEAD's are nulled after it.
    static ClassLoader deadLoader;
    static Class<?> deadClass;
    static Object deadInstance;
    static ClassLoader controlLoader;
    static Class<?> controlClass;
    // The one reference that keeps CONTROL alive afterwards: a young instance.
    static volatile Object controlHolder;
    static volatile Object sink;
    // The current round's ballast; the previous one is garbage.
    static volatile long[] ballast;

    static WeakReference<ClassLoader> deadLoaderRef;
    static WeakReference<Class<?>> deadClassRef;
    static WeakReference<ClassLoader> controlLoaderRef;

    public static void main(String[] args) throws Exception {
        int maxRounds = args.length > 0 ? Integer.parseInt(args[0]) : 64;
        String name = GenR5W3ConcUnloadProbe.class.getName() + "$Payload";
        byte[] bytes = readClassBytes(name);

        define(name, bytes);
        promote();
        // DEAD: nothing strong left. CONTROL: only the young instance.
        deadLoader = null;
        deadClass = null;
        deadInstance = null;
        controlLoader = null;
        controlClass = null;

        long[] outcome = drive(maxRounds);
        int rounds = (int) outcome[0];
        long oldAtUnload = outcome[1];
        boolean controlOk = outcome[2] != 0;
        boolean deadLoaderGone = deadLoaderRef.refersTo(null);
        boolean deadClassGone = deadClassRef.refersTo(null);
        controlOk &= step();
        Object holder = controlHolder;
        ClassLoader liveLoader = controlLoaderRef.get();
        boolean controlAlive = liveLoader != null && holder.getClass().getClassLoader() == liveLoader;
        System.out.println("conc-unload dead-loader-unloaded=" + deadLoaderGone
                + " dead-class-unloaded=" + deadClassGone
                + " control-loader-alive=" + controlAlive
                + " control-ok=" + controlOk);
        System.err.println("[probe] rounds=" + rounds + " old_collections_at_unload=" + oldAtUnload);
        ballast = null;
    }

    /** The rounds, in a frame of their own that never held a DEAD reference:
     *  returns {rounds, old collections when DEAD was found unloaded (-1 if
     *  never), control-ok as 0/1}. */
    static long[] drive(int maxRounds) {
        final int ballastLongs = (BALLAST_MIB << 20) / 8;
        // long[126]: 16-byte header + 1008 bytes = 1 KiB per churn object.
        final int churnObjects = CHURN_MIB << 10;
        long allocations = 0;
        long oldAtUnload = -1;
        boolean controlOk = true;
        int rounds = 0;
        int unloadRound = -1;
        for (; rounds < maxRounds; rounds++) {
            // Replace the ballast: the previous array becomes old garbage.
            ballast = new long[ballastLongs];
            // Young churn (at least one young collection per round), with
            // CONTROL's instance replaced as it goes.
            for (int k = 0; k < churnObjects; k++) {
                sink = new long[126];
                if ((++allocations & 255) == 0) {
                    controlOk &= step();
                }
            }
            // gen r5w5/conc9: `refersTo(null)`, not `get() == null`. Until
            // DEAD is cleared, `get()` puts the DEAD loader and class into
            // this compiled (OSR) frame every round; CratonVM's conservative
            // JIT root scan can find such a dead register or spill word and
            // root (and pin) it, so the probe itself may keep DEAD alive where
            // HotSpot's precise oop maps do not — the leading suspect for the
            // wave-4 run's `concunload_classes=0`, unmeasured.
            // (`get()` during an open cycle is also an SATB keep-alive.)
            if (unloadRound < 0 && deadLoaderRef.refersTo(null) && deadClassRef.refersTo(null)) {
                unloadRound = rounds;
                oldAtUnload = oldCollections();
            }
            // A few rounds past the unload, so a LATER old-generation
            // collection can release what the unloading one retained (the
            // retained-layout census; CratonVM stderr only).
            if (unloadRound >= 0 && rounds - unloadRound >= TAIL_ROUNDS) {
                break;
            }
        }
        sink = null;
        return new long[] {unloadRound >= 0 ? unloadRound : rounds, oldAtUnload, controlOk ? 1 : 0};
    }

    /** Both loaders and classes, an instance of each, and the weak references,
     *  in their own frame so no dead local outlives it. */
    static void define(String name, byte[] bytes) throws Exception {
        Throwaway dead = new Throwaway(name, bytes);
        Throwaway control = new Throwaway(name, bytes);
        Class<?> dc = dead.loadClass(name);
        Class<?> cc = control.loadClass(name);
        if (dc == cc || dc.getClassLoader() != dead || cc.getClassLoader() != control) {
            throw new IllegalStateException("the payload was not defined by its own loader");
        }
        deadLoader = dead;
        deadClass = dc;
        deadInstance = dc.getDeclaredConstructor().newInstance();
        if (!"ok".equals(deadInstance.toString())) {
            throw new IllegalStateException("the DEAD payload read back wrong");
        }
        controlLoader = control;
        controlClass = cc;
        controlHolder = cc.getDeclaredConstructor().newInstance();
        deadLoaderRef = new WeakReference<>(dead);
        deadClassRef = new WeakReference<>(dc);
        controlLoaderRef = new WeakReference<>(control);
    }

    /** Churn young garbage while everything above is held, so the loaders,
     *  classes, instances and weak references are all promoted (CratonVM
     *  tenures on the third survival; 16 x 32 MiB is eight 64 MiB semi-spaces).
     *  It touches none of them, so no compiled frame of it ever holds one. */
    static void promote() {
        for (int pass = 0; pass < 16; pass++) {
            for (int k = 0; k < 65536; k++) {
                sink = new long[64];
            }
        }
        sink = null;
    }

    /** Replace CONTROL's instance with the next one it makes; `false` if the
     *  instance read back wrong. */
    static boolean step() {
        Object next = ((Supplier<?>) controlHolder).get();
        boolean ok = "ok".equals(next.toString());
        controlHolder = next;
        return ok;
    }

    /** The old-generation collector bean's count ("MarkSweepCompact" on the
     *  Serial-shaped beans both VMs expose here), or -1 if there is none. */
    static long oldCollections() {
        for (GarbageCollectorMXBean gc : ManagementFactory.getGarbageCollectorMXBeans()) {
            String n = gc.getName();
            if (n.contains("MarkSweep") || n.contains("Old")) {
                return gc.getCollectionCount();
            }
        }
        return -1;
    }
}
