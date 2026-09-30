// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.ByteArrayOutputStream;
import java.io.InputStream;
import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.lang.ref.WeakReference;
import java.util.function.Supplier;

/**
 * gen r5w5/conc9 (2026-09-27): the generational CONCURRENT cycle unloads a
 * dead class loader — {@code GenR5W3ConcUnloadProbe} with every reference to
 * the dead loader confined to a thread that has EXITED before the first cycle.
 *
 * <p>Page: {@code docs/known-issues/gc/gengc-r5w1-refs5-concurrent-cycle-cannot-unload-classes-20260926.md}.
 * On the wave-4 build {@code GenR5W3ConcUnloadProbe} opened a concurrent cycle
 * with {@code CRATONVM_GEN_CONC_CLASS_UNLOAD=1} whose remark ran
 * ({@code concunload_remarks=1}) and unloaded nothing
 * ({@code concunload_classes=0}), while HotSpot Serial unloads. A read of every
 * root source found no VM table that names a user loader outright under the
 * class-unload licence; the likeliest sources of that verdict are PER-THREAD
 * state of the thread that defined the loader, which in W3 is {@code main},
 * the thread that then drives the cycles:
 * <ul>
 *   <li>W3's round loop tested {@code deadLoaderRef.get() == null} every round,
 *       which puts DEAD into the compiled (OSR) {@code drive()} frame; the
 *       conservative JIT root scan roots such a dead word (both probes now use
 *       {@code refersTo(null)});</li>
 *   <li>stale words of {@code define()}'s native frames left below the compiled
 *       {@code promote()} / {@code drive()} frames, which the conservative JIT
 *       scan roots and PINS (a pinned young object is never promoted, so the
 *       concurrent cycle, which judges only old objects, can never find it
 *       dead);</li>
 *   <li>the thread's JMX lock stack, which records {@code synchronized(this)} on
 *       the loader ({@code Throwaway.loadClass} and {@code ClassLoader.loadClass}
 *       both lock it, re-entrantly from {@code defineClass}'s supertype
 *       resolution) and is a root while an entry is left behind;</li>
 *   <li>the thread's root snapshot and its activation records.</li>
 * </ul>
 * Here a SETUP thread defines both loaders, keeps them strongly reachable while
 * it churns enough young garbage to promote them, drops DEAD, and EXITS; its
 * stack, snapshot and lock stack go with it. {@code main} never holds a
 * reference to DEAD. So if this probe unloads DEAD and W3 does not, the
 * retention is main-thread state (read the {@code CRATONVM_DBG_MIRRORPIN_WHY}
 * lines of W3); if this one does not unload either, the same lines name the
 * root source or the side-table row that keeps DEAD.
 *
 * <p>The rounds are W3's: each replaces one {@value #BALLAST_MIB} MiB
 * {@code long[]} (humongous on CratonVM at {@code -Xmx256m}, so placed in the
 * old generation; tenured on HotSpot Serial) and churns {@value #CHURN_MIB} MiB
 * of young garbage, until DEAD's weak references are both cleared.
 *
 * <p>Deterministic stdout (HotSpot {@code -XX:+UseSerialGC -Xmx256m} prints the
 * same, as it does for W3):
 * <pre>
 *   conc-unload dead-loader-unloaded=true dead-class-unloaded=true control-loader-alive=true control-ok=true
 * </pre>
 * Stderr (not compared): {@code [probe] rounds=R old_collections_at_unload=N}.
 * The evidence that a CONCURRENT cycle unloaded DEAD on CratonVM is {@code N=0}
 * with {@code [GC] conc_unload: concunload_remarks>=1 ... concunload_loaders>=1
 * concunload_classes>=1}.
 * <pre>
 *   javac -d tools/bench tools/bench/GenR5W5ConcUnloadProbe.java
 *   java -XX:+UseSerialGC -Xmx256m -cp tools/bench GenR5W5ConcUnloadProbe
 *   CRATONVM_GEN_CONC_CLASS_UNLOAD=1 CRATONVM_GEN_CONC_REMARK_REFPROC_HOOK=1 \
 *   CRATONVM_GC_CONC_START_PERCENT=40 CRATONVM_DBG=gc-stats CRATONVM_DBG_MIRRORPIN_WHY=1 \
 *     cratonvm --java-home "$JDK" -XX:+UseGenerationalGC -Xmx256m -cp tools/bench GenR5W5ConcUnloadProbe
 * </pre>
 * {@code CRATONVM_GC_CONC_START_PERCENT=40} keeps every round's old-generation
 * collection concurrent (after one cycle the adaptive start rises to about
 * 72 %), so a loader that was still young at the first cycle meets a later
 * one. Usage: {@code GenR5W5ConcUnloadProbe [maxRounds]} (default 64).
 */
public final class GenR5W5ConcUnloadProbe {

    static final int BALLAST_MIB = 34;
    static final int CHURN_MIB = 80;
    static final int TAIL_ROUNDS = 4;

    /** The class both throwaway loaders define (W3's, unchanged). */
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
            super(GenR5W5ConcUnloadProbe.class.getClassLoader());
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
        try (InputStream in = GenR5W5ConcUnloadProbe.class.getClassLoader().getResourceAsStream(res)) {
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

    // Strong holders while the setup thread promotes; all nulled before it exits.
    static ClassLoader deadLoader;
    static Class<?> deadClass;
    static Object deadInstance;
    static ClassLoader controlLoader;
    static Class<?> controlClass;
    // The one reference that keeps CONTROL alive afterwards: a young instance.
    static volatile Object controlHolder;
    static volatile Object sink;
    static volatile long[] ballast;
    static volatile Throwable setupFailure;

    static WeakReference<ClassLoader> deadLoaderRef;
    static WeakReference<Class<?>> deadClassRef;
    static WeakReference<ClassLoader> controlLoaderRef;

    public static void main(String[] args) throws Exception {
        int maxRounds = args.length > 0 ? Integer.parseInt(args[0]) : 64;
        Thread setup = new Thread(GenR5W5ConcUnloadProbe::setup, "conc-unload-setup");
        setup.start();
        setup.join();
        if (setupFailure != null) {
            throw new IllegalStateException("setup failed", setupFailure);
        }

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

    /** The setup thread's body: define, promote, drop DEAD, exit. */
    static void setup() {
        try {
            String name = GenR5W5ConcUnloadProbe.class.getName() + "$Payload";
            define(name, readClassBytes(name));
            promote();
            // DEAD: nothing strong left. CONTROL: only its instance.
            deadLoader = null;
            deadClass = null;
            deadInstance = null;
            controlLoader = null;
            controlClass = null;
        } catch (Throwable t) {
            setupFailure = t;
        }
    }

    /** The rounds, on main, which never held a DEAD reference: returns
     *  {rounds, old collections when DEAD was found unloaded (-1 if never),
     *  control-ok as 0/1}. {@code refersTo}, not {@code get()}: a {@code get()}
     *  during an open cycle is a keep-alive of its own. */
    static long[] drive(int maxRounds) {
        final int ballastLongs = (BALLAST_MIB << 20) / 8;
        final int churnObjects = CHURN_MIB << 10; // long[126] = 1 KiB each
        long allocations = 0;
        long oldAtUnload = -1;
        boolean controlOk = true;
        int rounds = 0;
        int unloadRound = -1;
        for (; rounds < maxRounds; rounds++) {
            ballast = new long[ballastLongs];
            for (int k = 0; k < churnObjects; k++) {
                sink = new long[126];
                if ((++allocations & 255) == 0) {
                    controlOk &= step();
                }
            }
            if (unloadRound < 0 && deadLoaderRef.refersTo(null) && deadClassRef.refersTo(null)) {
                unloadRound = rounds;
                oldAtUnload = oldCollections();
            }
            if (unloadRound >= 0 && rounds - unloadRound >= TAIL_ROUNDS) {
                break;
            }
        }
        sink = null;
        return new long[] {unloadRound >= 0 ? unloadRound : rounds, oldAtUnload, controlOk ? 1 : 0};
    }

    /** Both loaders and classes, an instance of each, and the weak references,
     *  in a frame of the setup thread. */
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

    /** Churn young garbage while everything is held, so it is all PROMOTED
     *  (CratonVM tenures on the third survival). Touches none of it. */
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

    /** The old-generation collector bean's count, or -1 if there is none. */
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
