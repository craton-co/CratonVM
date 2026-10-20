// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.ByteArrayOutputStream;
import java.io.InputStream;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Method;

/**
 * gcj j3/rt3 (`gcj-j2jt2-mh-site-memos-keyed-by-a-reusable-jit-invoke-info-address`): compiled
 * `MethodHandle.invokeExact` sites churned through fresh class loaders. Each round defines its own copy of
 * {@link Site} and {@link Arg} in a new child-first loader, runs the site hot enough to compile it (its
 * descriptor names the loader's own {@code Arg}), drops the loader and collects, so code is retired and a later
 * round's site can be compiled at a re-issued `JitInvokeInfo` address. A site that inherited another site's
 * "already linked" row would skip its own first link; the loader check of the door then sees a class the
 * caller's loader never resolved. Pass: stdout equal to HotSpot's (no exception, the arithmetic checksum).
 *
 * <p>Args: {@code <rounds> <calls-per-round>}. stdout: one deterministic line; per-round timings on stderr.
 */
public final class Gcj3Rt3MhSiteChurnProbe {
    public static final class Arg {
        final int v;

        public Arg(int v) {
            this.v = v;
        }
    }

    public static final class Site {
        static final MethodHandle MH;

        static {
            try {
                MH = MethodHandles.lookup()
                        .findStatic(Site.class, "target", MethodType.methodType(int.class, Arg.class, int.class));
            } catch (ReflectiveOperationException e) {
                throw new ExceptionInInitializerError(e);
            }
        }

        static int target(Arg a, int i) {
            return a.v + i;
        }

        public static long run(int n) throws Throwable {
            Arg a = new Arg(3);
            long s = 0;
            for (int i = 0; i < n; i++) {
                s += (int) MH.invokeExact(a, i);
            }
            return s;
        }
    }

    /** Child-first for the two nested classes, parent-first for everything else. */
    static final class ChildFirst extends ClassLoader {
        ChildFirst(ClassLoader parent) {
            super(parent);
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            synchronized (getClassLoadingLock(name)) {
                Class<?> c = findLoadedClass(name);
                if (c == null) {
                    if (name.equals(Site.class.getName()) || name.equals(Arg.class.getName())) {
                        byte[] b = bytesOf(name);
                        c = defineClass(name, b, 0, b.length);
                    } else {
                        return super.loadClass(name, resolve);
                    }
                }
                if (resolve) {
                    resolveClass(c);
                }
                return c;
            }
        }

        private static byte[] bytesOf(String name) throws ClassNotFoundException {
            String res = "/" + name.replace('.', '/') + ".class";
            try (InputStream in = Gcj3Rt3MhSiteChurnProbe.class.getResourceAsStream(res)) {
                if (in == null) {
                    throw new ClassNotFoundException(name);
                }
                ByteArrayOutputStream out = new ByteArrayOutputStream();
                byte[] buf = new byte[4096];
                for (int k; (k = in.read(buf)) > 0; ) {
                    out.write(buf, 0, k);
                }
                return out.toByteArray();
            } catch (java.io.IOException e) {
                throw new ClassNotFoundException(name, e);
            }
        }
    }

    public static void main(String[] args) throws Throwable {
        int rounds = args.length > 0 ? Integer.parseInt(args[0]) : 40;
        int n = args.length > 1 ? Integer.parseInt(args[1]) : 50000;
        long expected = 3L * n + (long) n * (n - 1) / 2;
        long checksum = 0;
        boolean ok = true;
        for (int r = 0; r < rounds; r++) {
            long t0 = System.nanoTime();
            ClassLoader loader = new ChildFirst(Gcj3Rt3MhSiteChurnProbe.class.getClassLoader());
            Class<?> site = loader.loadClass(Site.class.getName());
            if (site == Site.class) {
                ok = false;
            }
            Method run = site.getMethod("run", int.class);
            long got = (Long) run.invoke(null, n);
            if (got != expected) {
                ok = false;
                System.out.println("round " + r + " got=" + got + " expected=" + expected);
            }
            checksum = checksum * 31 + got;
            loader = null;
            site = null;
            run = null;
            System.gc();
            System.err.println("[gcj3rt3-mh] round=" + r + " us=" + (System.nanoTime() - t0) / 1000);
        }
        System.out.println(
                "gcj3rt3 mh-site-churn rounds=" + rounds + " n=" + n + " ok=" + ok + " checksum=" + checksum);
    }
}
