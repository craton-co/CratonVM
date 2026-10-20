// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.ByteArrayOutputStream;
import java.io.InputStream;
import java.lang.ref.WeakReference;
import java.util.function.Supplier;

/**
 * gci i9/yl: a {@code System.gc()} unloads a dead class loader that is still
 * YOUNG, whose class mirror is young too.
 *
 * <p>Page: {@code docs/known-issues/gc/gengc-core-system-gc-forces-non-moving-20260920.md}
 * (residual 3c of {@code docs/internal/gc-round-gci-20261011/i8-pa-report.md}).
 * Under {@code CRATONVM_GC_SYSTEM_GC_MOVING_YOUNG=1} with
 * {@code CRATONVM_GC_SYSTEM_GC_MOVING_SIDE_TABLES=1} the i8 build copied every
 * young side-table value of the moving {@code System.gc()}, so the dead
 * loader's young mirror survived the copy and kept the loader (its
 * {@code classLoader} field): the loader unloaded only once it was old, several
 * calls later. HotSpot Serial's {@code System.gc()} is a full collection and
 * unloads it on the first call.
 *
 * <p>A SETUP thread defines two loaders' copies of {@link Payload}, stores a
 * young value in each class's static {@code STASH} (a loader-owned metadata
 * value), drops DEAD and exits; nothing is churned, so both loaders are still
 * young (CratonVM tenures on the third survival). CONTROL stays reachable only
 * through one young instance. {@code main} never holds a DEAD reference
 * ({@code refersTo}, not {@code get()}), and calls {@code System.gc()} until
 * DEAD's loader and class are both cleared, at most {@value #CAP} times.
 *
 * <p>Deterministic stdout (HotSpot {@code -XX:+UseSerialGC} prints it):
 * <pre>
 *   yl9 dead-loader-unloaded=true dead-class-unloaded=true gcCalls=1 control-loader-alive=true control-static-ok=true control-ok=true
 * </pre>
 * {@code gcCalls} is the number of {@code System.gc()} calls after which DEAD
 * was first found cleared ({@code -1}: never within the cap). CONTROL's static
 * value is read back after every call, so a live young loader's metadata value
 * that the copy failed to follow reads back wrong or crashes.
 * <pre>
 *   javac -d tools/probes tools/probes/GciYl9YoungLoaderSysGcProbe.java
 *   java -XX:+UseSerialGC -Xmx256m -cp tools/probes GciYl9YoungLoaderSysGcProbe
 * </pre>
 * Usage: {@code GciYl9YoungLoaderSysGcProbe} (no arguments).
 */
public final class GciYl9YoungLoaderSysGcProbe {

    static final int CAP = 8;
    static final int STASH_LONGS = 64;

    /** The class both throwaway loaders define. */
    public static final class Payload implements Supplier<Object> {
        /** A loader-owned value: the class's static. Set by the setup thread. */
        public static long[] STASH;

        final int id;
        final long check;

        public Payload() {
            this(0);
        }

        Payload(int id) {
            this.id = id;
            this.check = mix(id);
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
            return check == mix(id) ? "ok" : "corrupt";
        }
    }

    /** Defines exactly one name itself and delegates everything else. */
    static final class Throwaway extends ClassLoader {
        private final String target;
        private final byte[] bytes;

        Throwaway(String target, byte[] bytes) {
            super(GciYl9YoungLoaderSysGcProbe.class.getClassLoader());
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

    // Set by the setup thread; DEAD's are nulled before it exits.
    static volatile Object controlHolder;
    static volatile Throwable setupFailure;
    static WeakReference<ClassLoader> deadLoaderRef;
    static WeakReference<Class<?>> deadClassRef;
    static WeakReference<ClassLoader> controlLoaderRef;

    public static void main(String[] args) throws Exception {
        Thread setup = new Thread(GciYl9YoungLoaderSysGcProbe::setup, "yl9-setup");
        setup.start();
        setup.join();
        if (setupFailure != null) {
            throw new IllegalStateException("setup failed", setupFailure);
        }
        int gcCalls = -1;
        boolean controlStaticOk = true;
        boolean controlOk = true;
        for (int call = 1; call <= CAP; call++) {
            System.gc();
            controlStaticOk &= stashOk(controlHolder.getClass(), 2);
            controlOk &= step();
            if (deadLoaderRef.refersTo(null) && deadClassRef.refersTo(null)) {
                gcCalls = call;
                break;
            }
        }
        boolean deadLoaderGone = deadLoaderRef.refersTo(null);
        boolean deadClassGone = deadClassRef.refersTo(null);
        Object holder = controlHolder;
        ClassLoader liveLoader = controlLoaderRef.get();
        boolean controlAlive = liveLoader != null && holder.getClass().getClassLoader() == liveLoader;
        System.out.println("yl9 dead-loader-unloaded=" + deadLoaderGone
                + " dead-class-unloaded=" + deadClassGone
                + " gcCalls=" + gcCalls
                + " control-loader-alive=" + controlAlive
                + " control-static-ok=" + controlStaticOk
                + " control-ok=" + controlOk);
    }

    /** The setup thread's body: define both, give each class a young static
     *  value, drop DEAD, exit. */
    static void setup() {
        try {
            String name = GciYl9YoungLoaderSysGcProbe.class.getName() + "$Payload";
            byte[] bytes = readClassBytes(name);
            Throwaway dead = new Throwaway(name, bytes);
            Throwaway control = new Throwaway(name, bytes);
            Class<?> dc = dead.loadClass(name);
            Class<?> cc = control.loadClass(name);
            if (dc == cc || dc.getClassLoader() != dead || cc.getClassLoader() != control) {
                throw new IllegalStateException("the payload was not defined by its own loader");
            }
            setStash(dc, 1);
            setStash(cc, 2);
            Object deadInstance = dc.getDeclaredConstructor().newInstance();
            if (!"ok".equals(deadInstance.toString()) || !stashOk(dc, 1)) {
                throw new IllegalStateException("the DEAD payload read back wrong");
            }
            controlHolder = cc.getDeclaredConstructor().newInstance();
            deadLoaderRef = new WeakReference<>(dead);
            deadClassRef = new WeakReference<>(dc);
            controlLoaderRef = new WeakReference<>(control);
        } catch (Throwable t) {
            setupFailure = t;
        }
    }

    static void setStash(Class<?> c, long seed) throws Exception {
        long[] v = new long[STASH_LONGS];
        for (int i = 0; i < v.length; i++) {
            v[i] = seed * 0x100000001L + i;
        }
        c.getField("STASH").set(null, v);
    }

    static boolean stashOk(Class<?> c, long seed) {
        try {
            long[] v = (long[]) c.getField("STASH").get(null);
            if (v == null || v.length != STASH_LONGS) {
                return false;
            }
            for (int i = 0; i < v.length; i++) {
                if (v[i] != seed * 0x100000001L + i) {
                    return false;
                }
            }
            return true;
        } catch (ReflectiveOperationException e) {
            return false;
        }
    }

    /** Replace CONTROL's instance with the next one it makes; `false` if the
     *  instance read back wrong. */
    static boolean step() {
        Object next = ((Supplier<?>) controlHolder).get();
        boolean ok = "ok".equals(next.toString());
        controlHolder = next;
        return ok;
    }

    static byte[] readClassBytes(String binaryName) throws Exception {
        String res = binaryName.replace('.', '/') + ".class";
        try (InputStream in =
                GciYl9YoungLoaderSysGcProbe.class.getClassLoader().getResourceAsStream(res)) {
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
}
