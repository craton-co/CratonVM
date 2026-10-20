// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.ByteArrayOutputStream;
import java.io.InputStream;
import java.lang.management.ClassLoadingMXBean;
import java.lang.management.ManagementFactory;
import java.lang.ref.WeakReference;
import java.util.ArrayList;
import java.util.List;
import java.util.function.Supplier;

/**
 * gci i12/ul (2026-10-13): loader churn, then {@code System.gc()} until the
 * throwaway loaders are gone. Page:
 * {@code docs/internal/gc/gci-i12ul-class-unloading-and-side-tables-on-g1-review-findings-RETIRED-UNFIXED-20261018.md}.
 *
 * <p>Two kinds of throwaway loader, {@value #GENS} of each, all defined on a
 * setup thread that has EXITED before the first collection (so no stack word
 * of it can keep a loader):
 * <ul>
 *   <li>PLAIN: defines {@code PlainPayload} and calls it. The control: HotSpot
 *       and CratonVM unload it at a full collection.</li>
 *   <li>ARRAY: defines {@code ArrayPayload}, whose {@code get()} builds an
 *       {@code ArrayPayload[]} through {@code List.toArray} and asks the array
 *       class for its component type and its loader. F2: the array class's
 *       mirror is a root on CratonVM (no {@code mirror_pin} row names it), and
 *       its {@code componentType} names the element's mirror, whose
 *       {@code classLoader} is the throwaway loader, so the loader is never
 *       unloaded. F2b: with two loaders defining {@code ArrayPayload}, the
 *       array mirror's {@code classLoader} was looked up by the element's
 *       NAME and read {@code null}.</li>
 * </ul>
 * Around the collections it reads {@code ClassLoadingMXBean} (F1: the total
 * fell after an unload, and the loaded count was short by the unloaded
 * count, because {@code VMManagementImpl.getTotalClassCount} answered the
 * live count).
 *
 * <p>Deterministic stdout, HotSpot 25 (G1 or Serial):
 * <pre>
 *   plain loaders-unloaded=true
 *   array get-ok=true
 *   array loaders-unloaded=true
 *   mx total-monotonic=true
 *   mx unloaded-grew=true
 *   mx loaded-fell-by-at-most-unloaded=true
 * </pre>
 * Stderr (not compared): one {@code [probe]} line with the counts.
 * Usage: {@code GciUl12LoaderChurnProbe [maxRounds] [hold]} (default 20).
 * With {@code hold}, a one-element {@code ArrayPayload[]} (element null) of the
 * FIRST array loader stays reachable from a static; line 3 then asks for the
 * other seven, and two lines follow (HotSpot: {@code held array-loader-alive=true},
 * {@code held array-ok=true}): an {@code X[]} instance keeps its element's
 * loader, the soundness half of the F2 fix.
 */
public final class GciUl12LoaderChurnProbe {

    static final int GENS = 8;

    /** The control payload: no array class of itself is ever made. */
    public static final class PlainPayload implements Supplier<Object> {
        public PlainPayload() {
        }

        @Override
        public Object get() {
            return "ok";
        }
    }

    /** Makes {@code ArrayPayload[]}, and its mirror, through {@code toArray}. */
    public static final class ArrayPayload implements Supplier<Object> {
        public ArrayPayload() {
        }

        @Override
        public Object get() {
            List<ArrayPayload> list = new ArrayList<>();
            list.add(this);
            ArrayPayload[] arr = list.toArray(new ArrayPayload[0]);
            Class<?> ac = arr.getClass();
            if (arr.length != 1 || arr[0] != this) {
                return "bad-array";
            }
            if (ac.getComponentType() != ArrayPayload.class) {
                return "bad-component";
            }
            ClassLoader mine = ArrayPayload.class.getClassLoader();
            ClassLoader arrays = ac.getClassLoader();
            if (arrays != mine) {
                return arrays == null ? "bad-loader-null" : "bad-loader-other";
            }
            return "ok";
        }
    }

    /**
     * Mode {@code redeploy} (F3): a hot method that inlines another method of
     * its own class. Two loaders define it; the first one dies.
     */
    public static final class HotPayload implements Supplier<Object> {
        public HotPayload() {
        }

        static int step(int x) {
            return x * 31 + 7;
        }

        int spin(int n) {
            int acc = 0;
            for (int i = 0; i < n; i++) {
                acc = step(acc ^ i);
            }
            return acc;
        }

        @Override
        public Object get() {
            long sum = 0;
            for (int r = 0; r < 200; r++) {
                sum += spin(2000);
            }
            return Long.toString(sum);
        }
    }

    /** Defines exactly one name itself and delegates everything else. */
    static final class Throwaway extends ClassLoader {
        private final String target;
        private final byte[] bytes;

        Throwaway(String target, byte[] bytes) {
            super(GciUl12LoaderChurnProbe.class.getClassLoader());
            this.target = target;
            this.bytes = bytes;
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            if (name.equals(target)) {
                synchronized (getClassLoadingLock(name)) {
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
        try (InputStream in = GciUl12LoaderChurnProbe.class.getClassLoader().getResourceAsStream(res)) {
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

    static final List<WeakReference<ClassLoader>> PLAIN = new ArrayList<>();
    static final List<WeakReference<ClassLoader>> ARRAY = new ArrayList<>();
    static final String[] RESULTS = new String[GENS];
    // Every strong reference the setup made, dropped by main after the first
    // bean snapshot.
    static volatile Object[] strong;
    static volatile Throwable setupFailure;
    // Mode `hold`: a one-element ArrayPayload[] of the FIRST array loader,
    // element null, made by reflection and kept here. HotSpot keeps that loader
    // (the array's class names its element class, whose loader it is); the
    // other seven unload.
    static boolean hold;
    static volatile Object held;

    static void setup() {
        try {
            String plainName = GciUl12LoaderChurnProbe.class.getName() + "$PlainPayload";
            String arrayName = GciUl12LoaderChurnProbe.class.getName() + "$ArrayPayload";
            byte[] plainBytes = readClassBytes(plainName);
            byte[] arrayBytes = readClassBytes(arrayName);
            Object[] keep = new Object[GENS * 6];
            for (int i = 0; i < GENS; i++) {
                Throwaway pl = new Throwaway(plainName, plainBytes);
                Class<?> pc = pl.loadClass(plainName);
                Object p = pc.getDeclaredConstructor().newInstance();
                ((Supplier<?>) p).get();
                Throwaway al = new Throwaway(arrayName, arrayBytes);
                Class<?> ac = al.loadClass(arrayName);
                Object a = ac.getDeclaredConstructor().newInstance();
                RESULTS[i] = String.valueOf(((Supplier<?>) a).get());
                if (hold && i == 0) {
                    held = java.lang.reflect.Array.newInstance(ac, 1);
                }
                PLAIN.add(new WeakReference<>(pl));
                ARRAY.add(new WeakReference<>(al));
                keep[i * 6] = pl;
                keep[i * 6 + 1] = pc;
                keep[i * 6 + 2] = p;
                keep[i * 6 + 3] = al;
                keep[i * 6 + 4] = ac;
                keep[i * 6 + 5] = a;
            }
            strong = keep;
        } catch (Throwable t) {
            setupFailure = t;
        }
    }

    // Mode `redeploy`: the new loader's payload (kept) and the old loader.
    static volatile Supplier<?> redeployed;
    static volatile WeakReference<ClassLoader> oldLoader;
    static volatile String oldResult;

    static void setupRedeploy() {
        try {
            String hotName = GciUl12LoaderChurnProbe.class.getName() + "$HotPayload";
            byte[] hotBytes = readClassBytes(hotName);
            Throwaway oldL = new Throwaway(hotName, hotBytes);
            Supplier<?> oldP = (Supplier<?>) oldL.loadClass(hotName).getDeclaredConstructor().newInstance();
            String r = "";
            for (int i = 0; i < 5; i++) {
                r = String.valueOf(oldP.get());
            }
            oldResult = r;
            oldLoader = new WeakReference<>(oldL);
            Throwaway newL = new Throwaway(hotName, hotBytes);
            Supplier<?> newP = (Supplier<?>) newL.loadClass(hotName).getDeclaredConstructor().newInstance();
            for (int i = 0; i < 5; i++) {
                newP.get();
            }
            redeployed = newP;
        } catch (Throwable t) {
            setupFailure = t;
        }
    }

    static void redeploy(int maxRounds) throws Exception {
        Thread t = new Thread(GciUl12LoaderChurnProbe::setupRedeploy, "ul12-redeploy");
        t.start();
        t.join();
        if (setupFailure != null) {
            throw new IllegalStateException("setup failed", setupFailure);
        }
        int rounds = 0;
        while (rounds < maxRounds && !oldLoader.refersTo(null)) {
            System.gc();
            rounds++;
            Thread.sleep(20);
        }
        boolean oldGone = oldLoader.refersTo(null);
        String after = "";
        for (int i = 0; i < 5; i++) {
            after = String.valueOf(redeployed.get());
        }
        System.out.println("redeploy old-unloaded=" + oldGone);
        System.out.println("redeploy same-result=" + after.equals(oldResult));
        System.out.println("redeploy result=" + after);
        System.err.println("[probe] redeploy rounds=" + rounds);
    }

    static int cleared(List<WeakReference<ClassLoader>> refs) {
        int n = 0;
        for (int i = 0; i < refs.size(); i++) {
            if (refs.get(i).refersTo(null)) {
                n++;
            }
        }
        return n;
    }

    public static void main(String[] args) throws Exception {
        int maxRounds = args.length > 0 ? Integer.parseInt(args[0]) : 20;
        if (args.length > 1 && args[1].equals("redeploy")) {
            redeploy(maxRounds);
            return;
        }
        hold = args.length > 1 && args[1].equals("hold");
        int arrayTarget = hold ? GENS - 1 : GENS;
        ClassLoadingMXBean bean = ManagementFactory.getClassLoadingMXBean();
        // Warm-up: every class the measured window below touches is loaded
        // here, so the window loads nothing of its own.
        long warm = bean.getTotalLoadedClassCount() + bean.getUnloadedClassCount()
                + bean.getLoadedClassCount();
        System.gc();
        Thread.sleep(1);
        new WeakReference<Object>(new Object()).refersTo(null);
        cleared(PLAIN);

        Thread t = new Thread(GciUl12LoaderChurnProbe::setup, "ul12-setup");
        t.start();
        t.join();
        if (setupFailure != null) {
            throw new IllegalStateException("setup failed", setupFailure);
        }

        long t1 = bean.getTotalLoadedClassCount();
        long u1 = bean.getUnloadedClassCount();
        long l1 = bean.getLoadedClassCount();
        strong = null;
        int rounds = 0;
        int plainGone = 0;
        int arrayGone = 0;
        while (rounds < maxRounds) {
            System.gc();
            rounds++;
            plainGone = cleared(PLAIN);
            arrayGone = cleared(ARRAY);
            if (plainGone == GENS && arrayGone >= arrayTarget) {
                break;
            }
            Thread.sleep(20);
        }
        long t2 = bean.getTotalLoadedClassCount();
        long u2 = bean.getUnloadedClassCount();
        long l2 = bean.getLoadedClassCount();

        boolean getOk = true;
        StringBuilder results = new StringBuilder();
        for (int i = 0; i < GENS; i++) {
            if (!"ok".equals(RESULTS[i])) {
                getOk = false;
            }
            results.append(i == 0 ? "" : ",").append(RESULTS[i]);
        }
        System.out.println("plain loaders-unloaded=" + (plainGone == GENS));
        System.out.println("array get-ok=" + getOk);
        System.out.println("array loaders-unloaded=" + (arrayGone == arrayTarget));
        System.out.println("mx total-monotonic=" + (t2 >= t1));
        System.out.println("mx unloaded-grew=" + (u2 - u1 >= GENS));
        System.out.println("mx loaded-fell-by-at-most-unloaded=" + (l1 - l2 <= u2 - u1));
        String heldShape = "-";
        if (hold) {
            Object h = held;
            boolean alive = !ARRAY.get(0).refersTo(null);
            Class<?> hc = h.getClass();
            String arrayName = GciUl12LoaderChurnProbe.class.getName() + "$ArrayPayload";
            boolean shapeOk = hc.isArray()
                    && java.lang.reflect.Array.getLength(h) == 1
                    && java.lang.reflect.Array.get(h, 0) == null
                    && hc.getComponentType() != null
                    && hc.getComponentType().getName().equals(arrayName)
                    && hc.getClassLoader() == ARRAY.get(0).get();
            heldShape = hc.getName();
            System.out.println("held array-loader-alive=" + alive);
            System.out.println("held array-ok=" + shapeOk);
        }
        System.err.println("[probe] rounds=" + rounds + " plain_gone=" + plainGone
                + " array_gone=" + arrayGone + " t1=" + t1 + " u1=" + u1 + " l1=" + l1
                + " t2=" + t2 + " u2=" + u2 + " l2=" + l2 + " warm=" + (warm > 0)
                + " results=" + results + " held=" + heldShape);
    }
}
