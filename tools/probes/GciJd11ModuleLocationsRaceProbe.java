// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.InputStream;
import java.nio.charset.StandardCharsets;
import java.util.Collections;
import java.util.HashMap;
import java.util.List;
import java.util.Locale;
import java.util.Map;
import java.util.Objects;
import java.util.Set;
import java.util.concurrent.Callable;
import java.util.concurrent.CyclicBarrier;
import java.util.concurrent.atomic.AtomicBoolean;
import java.util.concurrent.atomic.AtomicLong;
import javax.tools.ForwardingJavaFileManager;
import javax.tools.JavaCompiler;
import javax.tools.JavaFileManager;
import javax.tools.StandardJavaFileManager;
import javax.tools.StandardLocation;
import javax.tools.ToolProvider;

/**
 * gci i11/jd: the second symptom of the {@code --compatible} 4-thread javac page,
 * {@code NullPointerException: ... the return value of
 * "javax.tools.JavaFileManager.listLocationsForModules(...)" is null}
 * ({@code ModuleFinder$ModuleLocationIterator.hasNext}, ModuleFinder.java:139).
 *
 * <p>Page: {@code docs/internal/gc/gci-i4orch-compatible-concurrent-javac-reads-a-module-info-class-as-source-FIXED-20261017.md}.
 * javac answers null there only when {@code Locations.getHandler(location)} misses
 * ({@code location instanceof LocationHandler h ? h : handlersForLocation.get(location)},
 * a {@code HashMap} keyed by {@code StandardLocation} constants, i.e. by
 * {@code Enum.hashCode()} = the lazily cached identity hash) or when the virtual
 * {@code h.listLocationsForModules()} runs {@code LocationHandler}'s own body
 * ({@code return null;}) instead of the receiver's override. Every javac
 * thread builds its own map in {@code Locations.initHandlers()} while the
 * others first-hash the same shared constants.
 *
 * <p>Two modes, each deterministic on stdout (HotSpot 25 prints all zeros):
 * <ul>
 * <li>{@code enum <threads> <iters>}: the javac shape without javac. Every
 *     iteration defines a FRESH copy of a 13-constant enum, a
 *     {@code LocationHandler}-shaped abstract class with four overriding
 *     subclasses and a worker, in a new child-first loader, and starts
 *     {@code threads} workers at one barrier, so each iteration is a new
 *     first-touch race on class initialisation, on {@code Enum.hashCode()}'s
 *     cache, on {@code instanceof} of an interface-typed enum against an
 *     abstract class, and on a polymorphic virtual call site. An allocator
 *     thread churns young garbage during the race so collections land in it.
 *     Counters: {@code null-list} (the S2 shape), {@code null-handler},
 *     {@code get-miss}, {@code hash-mismatch}
 *     ({@code e.hashCode() != System.identityHashCode(e)}),
 *     {@code instanceof-wrong}, {@code const-identity}
 *     ({@code valueOf(name()) != e}).</li>
 * <li>{@code fm <threads> <iters>}: the real thing. {@code threads} threads
 *     create a standard file manager at one barrier, wrap it in a
 *     {@code ForwardingJavaFileManager} (the javac probe's {@code MemFm}
 *     shape) and ask {@code listLocationsForModules} for ModuleFinder's four
 *     locations in ModuleFinder's order. Counters: {@code null}, {@code threw}.
 *     Only the first iteration races {@code StandardLocation}'s first use.</li>
 * </ul>
 *
 * <p>Diagnostics (which iteration, thread and location hit) go to stderr only.
 * Run: {@code cratonvm --compatible -cp <classes> GciJd11ModuleLocationsRaceProbe enum 4 200}.
 */
public class GciJd11ModuleLocationsRaceProbe {

    /** {@code JavaFileManager.Location}'s shape. Loaded by the parent loader. */
    public interface Loc0 {
        String getName();
    }

    /** {@code StandardLocation}'s shape: an enum implementing the location interface. */
    public enum Loc implements Loc0 {
        CLASS_OUTPUT,
        SOURCE_OUTPUT,
        CLASS_PATH,
        SOURCE_PATH,
        ANNOTATION_PROCESSOR_PATH,
        ANNOTATION_PROCESSOR_MODULE_PATH,
        PLATFORM_CLASS_PATH,
        NATIVE_HEADER_OUTPUT,
        MODULE_SOURCE_PATH,
        UPGRADE_MODULE_PATH,
        SYSTEM_MODULES,
        MODULE_PATH,
        PATCH_MODULE_PATH;

        @Override
        public String getName() {
            return name();
        }
    }

    /** {@code Locations.LocationHandler}'s shape: a concrete default that answers null. */
    public abstract static class Handler {
        final Loc0 location;

        public Handler(Loc0 location) {
            this.location = location;
        }

        public Iterable<String> list() {
            return null;
        }
    }

    /** {@code SimpleLocationHandler}: does not override. */
    public static class HSimple extends Handler {
        public HSimple(Loc0 location) {
            super(location);
        }
    }

    /** {@code OutputLocationHandler}. */
    public static class HOut extends Handler {
        public HOut(Loc0 location) {
            super(location);
        }

        @Override
        public Iterable<String> list() {
            return Collections.emptySet();
        }
    }

    /** {@code ModuleSourcePathLocationHandler}. */
    public static class HModSrc extends Handler {
        public HModSrc(Loc0 location) {
            super(location);
        }

        @Override
        public Iterable<String> list() {
            // gci i11/jd positive control for CRATONVM_DBG_BASE_BODY_ENTRY: an
            // invokespecial of the base body with an overriding receiver, the one
            // legitimate shape the witness reports (stdout unaffected).
            Iterable<String> base = super.list();
            return base == null ? Collections.emptySet() : base;
        }
    }

    /** {@code ModulePathLocationHandler}. */
    public static class HModPath extends Handler {
        public HModPath(Loc0 location) {
            super(location);
        }

        @Override
        public Iterable<String> list() {
            return Collections.singletonList("p:" + location.getName());
        }
    }

    /** {@code SystemModulesLocationHandler}. */
    public static class HSys extends Handler {
        public HSys(Loc0 location) {
            super(location);
        }

        @Override
        public Iterable<String> list() {
            return Collections.singleton("sys");
        }
    }

    static final int NULL_LIST = 0;
    static final int NULL_HANDLER = 1;
    static final int GET_MISS = 2;
    static final int HASH_MISMATCH = 3;
    static final int INSTANCEOF_WRONG = 4;
    static final int CONST_IDENTITY = 5;
    // A compile-time constant: the child-loaded Worker must not read a field of this
    // (parent-loaded, other runtime package) class at run time.
    static final int COUNTERS = 6;
    static final String[] COUNTER_NAMES = {
        "null-list", "null-handler", "get-miss", "hash-mismatch", "instanceof-wrong", "const-identity"
    };

    /** One javac thread's {@code Locations}: build the map, then ask ModuleFinder's four. */
    public static class Worker implements Callable<long[]> {
        static Handler getHandler(Map<Loc0, Handler> map, Loc0 location) {
            Objects.requireNonNull(location);
            return (location instanceof Handler h) ? h : map.get(location);
        }

        static void pass(long[] r) {
            Handler[] hs = {
                new HSimple(Loc.CLASS_PATH),
                new HSimple(Loc.SOURCE_PATH),
                new HSimple(Loc.ANNOTATION_PROCESSOR_PATH),
                new HSimple(Loc.ANNOTATION_PROCESSOR_MODULE_PATH),
                new HSimple(Loc.PLATFORM_CLASS_PATH),
                new HOut(Loc.CLASS_OUTPUT),
                new HOut(Loc.SOURCE_OUTPUT),
                new HOut(Loc.NATIVE_HEADER_OUTPUT),
                new HModSrc(Loc.MODULE_SOURCE_PATH),
                new HSimple(Loc.PATCH_MODULE_PATH),
                new HModPath(Loc.UPGRADE_MODULE_PATH),
                new HModPath(Loc.MODULE_PATH),
                new HSys(Loc.SYSTEM_MODULES),
            };
            Map<Loc0, Handler> map = new HashMap<>();
            for (Handler h : hs) {
                map.put(h.location, h);
            }
            Loc0[] order = {Loc.MODULE_SOURCE_PATH, Loc.UPGRADE_MODULE_PATH, Loc.SYSTEM_MODULES, Loc.MODULE_PATH};
            for (Loc0 l : order) {
                Handler h = getHandler(map, l);
                if (h == null) {
                    r[NULL_HANDLER]++;
                }
                Iterable<String> it = (h == null) ? null : h.list();
                if (it == null) {
                    r[NULL_LIST]++;
                }
            }
            for (Loc l : Loc.values()) {
                if (map.get(l) == null) {
                    r[GET_MISS]++;
                }
                if (l.hashCode() != System.identityHashCode(l)) {
                    r[HASH_MISMATCH]++;
                }
                Object o = l;
                if (o instanceof Handler) {
                    r[INSTANCEOF_WRONG]++;
                }
                if (!(o instanceof Loc0)) {
                    r[INSTANCEOF_WRONG]++;
                }
                if (Loc.valueOf(l.name()) != l) {
                    r[CONST_IDENTITY]++;
                }
            }
        }

        @Override
        public long[] call() {
            long[] r = new long[COUNTERS];
            // First pass: the first touch of this loader's Loc, raced by every worker.
            pass(r);
            // Second pass after allocating, so a collection can have moved the constants.
            byte[][] junk = new byte[64][];
            for (int i = 0; i < junk.length; i++) {
                junk[i] = new byte[2048];
            }
            pass(r);
            if (junk[junk.length - 1].length != 2048) {
                r[NULL_LIST] += 1000;
            }
            return r;
        }
    }

    /** Defines the listed classes itself; everything else comes from the parent. */
    static final class ChildFirst extends ClassLoader {
        private final Map<String, byte[]> bytes;

        ChildFirst(Map<String, byte[]> bytes, ClassLoader parent) {
            super(parent);
            this.bytes = bytes;
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            synchronized (getClassLoadingLock(name)) {
                Class<?> c = findLoadedClass(name);
                if (c == null) {
                    byte[] b = bytes.get(name);
                    if (b == null) {
                        return super.loadClass(name, resolve);
                    }
                    c = defineClass(name, b, 0, b.length);
                }
                if (resolve) {
                    resolveClass(c);
                }
                return c;
            }
        }
    }

    static byte[] readClass(String name) throws Exception {
        String res = name.replace('.', '/') + ".class";
        try (InputStream in = GciJd11ModuleLocationsRaceProbe.class.getClassLoader().getResourceAsStream(res)) {
            if (in == null) {
                throw new IllegalStateException("missing " + res);
            }
            return in.readAllBytes();
        }
    }

    static void enumMode(int threads, int iters) throws Exception {
        String base = GciJd11ModuleLocationsRaceProbe.class.getName();
        String[] local = {
            base + "$Loc", base + "$Handler", base + "$HSimple", base + "$HOut",
            base + "$HModSrc", base + "$HModPath", base + "$HSys", base + "$Worker",
        };
        Map<String, byte[]> bytes = new HashMap<>();
        for (String n : local) {
            bytes.put(n, readClass(n));
        }
        long[] total = new long[COUNTER_NAMES.length];
        long failures = 0;
        long hitIters = 0;
        for (int it = 0; it < iters; it++) {
            ClassLoader cl = new ChildFirst(bytes, GciJd11ModuleLocationsRaceProbe.class.getClassLoader());
            Class<?> wc = Class.forName(base + "$Worker", false, cl);
            Object[] workers = new Object[threads];
            for (int t = 0; t < threads; t++) {
                workers[t] = wc.getDeclaredConstructor().newInstance();
            }
            CyclicBarrier start = new CyclicBarrier(threads + 1);
            long[][] res = new long[threads][];
            Throwable[] fail = new Throwable[1];
            AtomicBoolean stop = new AtomicBoolean();
            Thread churn = new Thread(() -> {
                try {
                    start.await();
                } catch (Exception e) {
                    return;
                }
                Object[] keep = new Object[32];
                int k = 0;
                while (!stop.get()) {
                    keep[k++ & 31] = new byte[4096];
                }
            }, "enum-race-churn");
            churn.start();
            Thread[] ts = new Thread[threads];
            for (int t = 0; t < threads; t++) {
                final int tt = t;
                @SuppressWarnings("unchecked")
                Callable<long[]> w = (Callable<long[]>) workers[t];
                ts[t] = new Thread(() -> {
                    try {
                        start.await();
                        res[tt] = w.call();
                    } catch (Throwable e) {
                        synchronized (fail) {
                            if (fail[0] == null) {
                                fail[0] = e;
                            }
                        }
                    }
                }, "enum-race-" + t);
                ts[t].start();
            }
            for (Thread x : ts) {
                x.join();
            }
            stop.set(true);
            churn.join();
            if (fail[0] != null) {
                failures++;
                System.err.println("[enum-race] iter=" + it + " threw " + fail[0]);
            }
            boolean hit = false;
            for (int t = 0; t < threads; t++) {
                if (res[t] == null) {
                    continue;
                }
                StringBuilder line = null;
                for (int c = 0; c < COUNTER_NAMES.length; c++) {
                    total[c] += res[t][c];
                    if (res[t][c] != 0) {
                        if (line == null) {
                            line = new StringBuilder("[enum-race] iter=" + it + " thread=" + t);
                        }
                        line.append(' ').append(COUNTER_NAMES[c]).append('=').append(res[t][c]);
                    }
                }
                if (line != null) {
                    hit = true;
                    System.err.println(line);
                }
            }
            if (hit) {
                hitIters++;
            }
        }
        System.out.println("enum-race threads=" + threads + " iters=" + iters);
        for (int c = 0; c < COUNTER_NAMES.length; c++) {
            System.out.println(COUNTER_NAMES[c] + "=" + total[c]);
        }
        System.out.println("hit-iters=" + hitIters);
        System.out.println("failures=" + failures);
    }

    static void fmMode(int threads, int iters) throws Exception {
        JavaCompiler jc = ToolProvider.getSystemJavaCompiler();
        if (jc == null) {
            System.out.println("FAIL fm-race no system compiler");
            return;
        }
        AtomicLong calls = new AtomicLong();
        AtomicLong nulls = new AtomicLong();
        AtomicLong threw = new AtomicLong();
        for (int it = 0; it < iters; it++) {
            final int iter = it;
            CyclicBarrier start = new CyclicBarrier(threads);
            Thread[] ts = new Thread[threads];
            for (int t = 0; t < threads; t++) {
                final int tt = t;
                ts[t] = new Thread(() -> {
                    try {
                        start.await();
                        // StandardLocation is first touched HERE, after the barrier, as in the
                        // javac probe (main only asked ToolProvider for the compiler).
                        StandardJavaFileManager sfm =
                                jc.getStandardFileManager(null, Locale.ROOT, StandardCharsets.UTF_8);
                        JavaFileManager fm = new ForwardingJavaFileManager<StandardJavaFileManager>(sfm) {};
                        List<StandardLocation> order = List.of(
                                StandardLocation.MODULE_SOURCE_PATH,
                                StandardLocation.UPGRADE_MODULE_PATH,
                                StandardLocation.SYSTEM_MODULES,
                                StandardLocation.MODULE_PATH);
                        for (StandardLocation l : order) {
                            calls.incrementAndGet();
                            try {
                                Iterable<Set<JavaFileManager.Location>> r = fm.listLocationsForModules(l);
                                if (r == null) {
                                    nulls.incrementAndGet();
                                    System.err.println("[fm-race] iter=" + iter + " thread=" + tt
                                            + " listLocationsForModules(" + l + ") == null");
                                } else {
                                    for (Set<JavaFileManager.Location> s : r) {
                                        s.size();
                                    }
                                }
                            } catch (Exception e) {
                                threw.incrementAndGet();
                                System.err.println("[fm-race] iter=" + iter + " thread=" + tt
                                        + " listLocationsForModules(" + l + ") threw " + e);
                            }
                        }
                        fm.close();
                    } catch (Throwable e) {
                        threw.incrementAndGet();
                        System.err.println("[fm-race] iter=" + iter + " thread=" + tt + " threw " + e);
                    }
                }, "fm-race-" + t);
                ts[t].start();
            }
            for (Thread x : ts) {
                x.join();
            }
        }
        System.out.println("fm-race threads=" + threads + " iters=" + iters);
        System.out.println("calls=" + calls.get());
        System.out.println("null=" + nulls.get());
        System.out.println("threw=" + threw.get());
    }

    public static void main(String[] args) throws Exception {
        String mode = args.length > 0 ? args[0] : "enum";
        int threads = args.length > 1 ? Integer.parseInt(args[1]) : 4;
        int iters = args.length > 2 ? Integer.parseInt(args[2]) : 200;
        if (mode.equals("fm")) {
            fmMode(threads, iters);
        } else {
            enumMode(threads, iters);
        }
    }
}
