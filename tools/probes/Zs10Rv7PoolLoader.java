// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.ref.WeakReference;
import java.lang.reflect.Method;
import java.net.URL;
import java.net.URLClassLoader;
import java.util.ArrayList;

/**
 * zs s10/rv7 (row RV7-1, residual 1 of
 * {@code zgc-zs10rv7-wave-s9-review-residuals-20261016.md}): does a
 * {@code Java heap space} error thrown inside a child loader's code keep that
 * loader alive after the program dropped everything? CratonVM's ZGC traced
 * OOME pool ({@code CRATONVM_ZGC_OOME_TRACED_POOL}) keeps the errors it hands
 * out rooted; HotSpot's preallocated errors are ordinary garbage once dropped.
 *
 * <p>Two variants, each in a fresh child loader whose parent is the platform
 * loader (so this file's own {@code Filler} is defined again by the child):
 * <ul>
 *   <li>{@code trace}: the child fills the heap to the error, catches it and
 *       reads its stack trace (which materialises {@code stackTrace});</li>
 *   <li>{@code drop}: the child catches the error and keeps nothing.</li>
 * </ul>
 * Then main drops the loader, the class and the method, calls
 * {@code System.gc()} three times and prints whether a {@code WeakReference}
 * to the loader was cleared. Deterministic stdout:
 * <pre>
 * trace traced=true cleared=true
 * drop cleared=true
 * control cleared=false
 * </pre>
 * The {@code control} line is the positive control: a loader whose class the
 * probe still holds is not cleared.
 * Usage: {@code java -XX:+UseZGC -Xmx64m Zs10Rv7PoolLoader}.
 */
public class Zs10Rv7PoolLoader {
    /** Defined again by each child loader. */
    public static class Filler {
        static ArrayList<long[]> fill;

        /** Fill to {@code Java heap space}; 1 when the caught error had a trace. */
        public static int run(boolean readTrace) {
            fill = new ArrayList<>();
            int traced = 0;
            try {
                while (true) {
                    fill.add(new long[8 * 1024]);
                }
            } catch (OutOfMemoryError e) {
                fill = null;
                if (readTrace) {
                    traced = e.getStackTrace().length > 0 ? 1 : 0;
                }
            }
            fill = null;
            return traced;
        }
    }

    static WeakReference<ClassLoader> runInChild(boolean readTrace, int[] traced) throws Exception {
        URL here = Zs10Rv7PoolLoader.class.getProtectionDomain().getCodeSource().getLocation();
        URLClassLoader child = new URLClassLoader(new URL[] {here}, ClassLoader.getPlatformClassLoader());
        Class<?> c = child.loadClass("Zs10Rv7PoolLoader$Filler");
        if (c.getClassLoader() != child) {
            throw new IllegalStateException("Filler was not defined by the child loader");
        }
        Method m = c.getMethod("run", boolean.class);
        traced[0] = (Integer) m.invoke(null, readTrace);
        child.close();
        return new WeakReference<>(child);
    }

    static boolean clearedAfterGc(WeakReference<ClassLoader> ref) throws InterruptedException {
        for (int i = 0; i < 3 && ref.get() != null; i++) {
            System.gc();
            Thread.sleep(100);
        }
        return ref.get() == null;
    }

    /** Positive control: the probe keeps the child's class, so its loader must stay. */
    static Class<?> kept;

    public static void main(String[] args) throws Exception {
        int[] traced = new int[1];
        WeakReference<ClassLoader> a = runInChild(true, traced);
        boolean aCleared = clearedAfterGc(a);
        System.out.println("trace traced=" + (traced[0] == 1) + " cleared=" + aCleared);
        WeakReference<ClassLoader> b = runInChild(false, traced);
        boolean bCleared = clearedAfterGc(b);
        System.out.println("drop cleared=" + bCleared);
        // The control: a loader whose class is still referenced is never
        // cleared, so a `cleared=true` above is not the harness failing to
        // hold anything.
        URL here = Zs10Rv7PoolLoader.class.getProtectionDomain().getCodeSource().getLocation();
        URLClassLoader child = new URLClassLoader(new URL[] {here}, ClassLoader.getPlatformClassLoader());
        kept = child.loadClass("Zs10Rv7PoolLoader$Filler");
        WeakReference<ClassLoader> c = new WeakReference<>(child);
        child = null;
        System.out.println("control cleared=" + clearedAfterGc(c));
    }
}
