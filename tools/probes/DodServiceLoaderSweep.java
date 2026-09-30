// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Why a classpath SPI provider disappears, one rung at a time.
//
// Under `--jdk-only` every `java/util/ServiceLoader` native is a refused
// SyntheticStub, so the REAL JDK ServiceLoader bytecode runs. That bytecode has
// exactly one rung which drops a provider SILENTLY -- no exception, no report
// row, just `continue`:
//
//     if (clazz.getModule().isNamed()) continue;   // ignore class if in named module
//
// A real JVM ignores a class-path jar's `module-info` outright, so every class
// it defines is in the UNNAMED module of its loader and that branch is never
// taken. This VM's module registry scans the application class path for
// `module-info.class` (so readability checks pass), and `Class.getModule()`
// used to report the scanned name -- which made SLF4J bind `NOPLoggerFactory`
// and killed every Spring Boot application under `--jdk-only`.
//
// The sweep walks the whole chain so a future regression names the rung rather
// than the symptom: MODULE identity, RESOURCE enumeration, the SPI lookup
// replicated by hand, ServiceLoader's own answer, and finally what SLF4J binds.
//
// Run in both modes against HotSpot as oracle. Every printed value is chosen by
// the program: no identity hashes, no addresses, no iteration order of a hash
// container, and resource URLs reduced to their jar basename because the full
// file: URL is a path two VMs may spell differently.
//
//   javac -d out -cp "$(cat <module>/build/cratonvm-test-cp.txt)" DodServiceLoaderSweep.java
//   cratonvm --java-home "$JDK" --jdk-only -cp "$CP:out" DodServiceLoaderSweep \
//       org.slf4j.spi.SLF4JServiceProvider java.sql.Driver
//
// `--add-opens java.base/java.util=ALL-UNNAMED` additionally prints
// ServiceLoader's own field state; without it those rows read `<inaccessible>`
// and nothing else changes.

import java.io.BufferedReader;
import java.io.InputStream;
import java.io.InputStreamReader;
import java.lang.reflect.Field;
import java.net.URL;
import java.net.URLConnection;
import java.nio.charset.StandardCharsets;
import java.util.ArrayList;
import java.util.Collections;
import java.util.Enumeration;
import java.util.Iterator;
import java.util.List;
import java.util.ServiceLoader;

public final class DodServiceLoaderSweep {

    private static final String PREFIX = "META-INF/services/";

    public static void main(String[] args) {
        ClassLoader cl = DodServiceLoaderSweep.class.getClassLoader();
        String[] spis = args.length > 0 ? args : new String[] {
            "org.slf4j.spi.SLF4JServiceProvider", "java.sql.Driver",
        };

        modules(cl);
        for (String spi : spis) {
            System.out.println("== SPI " + spi);
            Class<?> svc = load(spi, cl);
            if (svc == null) {
                System.out.println("   SPI-NOT-LOADABLE");
                continue;
            }
            resources(cl, svc);
            manual(svc, cl);
            viaServiceLoader(svc, cl);
        }
        binding();
        System.out.println("DOD RESULT OK");
    }

    // ------------------------------------------------------------- MODULE ---

    /**
     * A class from a `-cp` jar belongs to the UNNAMED module of its loader,
     * whatever `module-info.class` that jar happens to carry. A platform class
     * belongs to its real named module. Both halves are asserted here so a fix
     * to the first cannot silently take the second with it.
     */
    private static void modules(ClassLoader cl) {
        String[] names = {
            "DodServiceLoaderSweep",
            "ch.qos.logback.classic.spi.LogbackServiceProvider",
            "org.slf4j.spi.SLF4JServiceProvider",
            "org.springframework.boot.SpringApplication",
            "java.lang.String",
            "java.util.ServiceLoader",
        };
        for (String n : names) {
            Class<?> c = load(n, cl);
            if (c == null) {
                System.out.println("DOD MODULE " + n + " not-loadable");
                continue;
            }
            Module m = c.getModule();
            System.out.println("DOD MODULE " + n
                    + " isNamed=" + (m == null ? "NULL-MODULE" : m.isNamed())
                    + " name=" + (m == null ? "-" : String.valueOf(m.getName()))
                    + " bootLoader=" + (c.getClassLoader() == null));
        }
        Module unnamed = cl.getUnnamedModule();
        System.out.println("DOD MODULE loader.getUnnamedModule().isNamed="
                + (unnamed == null ? "NULL" : String.valueOf(unnamed.isNamed())));
        Class<?> lb = load("ch.qos.logback.classic.spi.LogbackServiceProvider", cl);
        if (lb != null) {
            System.out.println("DOD MODULE cpProvider.getModule()==loader.getUnnamedModule() : "
                    + (lb.getModule() == unnamed));
        }
    }

    // ----------------------------------------------------------- RESOURCE ---

    /**
     * Four doors onto the same file, plus the one `ServiceLoader.parse` actually
     * uses (`openConnection` with caches off, not `openStream`).
     */
    private static void resources(ClassLoader cl, Class<?> svc) {
        String full = PREFIX + svc.getName();
        enumerate("app", cl, full);
        enumerate("system", ClassLoader.getSystemClassLoader(), full);
        enumerate("tccl", Thread.currentThread().getContextClassLoader(), full);
        enumerate("bootstrap", null, full);
        try {
            Enumeration<URL> e = cl.getResources(full);
            while (e.hasMoreElements()) {
                URL u = e.nextElement();
                try {
                    URLConnection uc = u.openConnection();
                    uc.setUseCaches(false);
                    try (InputStream in = uc.getInputStream();
                         BufferedReader r = new BufferedReader(
                                 new InputStreamReader(in, StandardCharsets.UTF_8))) {
                        System.out.println("   R-openConnection.firstLine=" + r.readLine());
                    }
                } catch (Throwable t) {
                    System.out.println("   R-openConnection THREW "
                            + t.getClass().getName() + ": " + t.getMessage());
                }
            }
        } catch (Throwable t) {
            System.out.println("   R-openConnection-enumerate THREW " + t.getClass().getName());
        }
    }

    private static void enumerate(String label, ClassLoader cl, String name) {
        try {
            Enumeration<URL> e = (cl == null)
                    ? ClassLoader.getSystemResources(name)
                    : cl.getResources(name);
            List<String> hits = new ArrayList<>();
            while (e.hasMoreElements()) {
                hits.add(basename(e.nextElement()));
            }
            Collections.sort(hits);
            System.out.println("   R-" + label + " count=" + hits.size() + " " + hits);
        } catch (Throwable t) {
            System.out.println("   R-" + label + " THREW " + t.getClass().getName()
                    + ": " + t.getMessage());
        }
    }

    // -------------------------------------------------------------- MANUAL --

    /** Everything `LazyClassPathLookupIterator` does, by hand. */
    private static void manual(Class<?> svc, ClassLoader cl) {
        List<String> names = new ArrayList<>();
        try {
            Enumeration<URL> configs = cl.getResources(PREFIX + svc.getName());
            while (configs.hasMoreElements()) {
                URL u = configs.nextElement();
                try (InputStream in = u.openStream();
                     BufferedReader r = new BufferedReader(
                             new InputStreamReader(in, StandardCharsets.UTF_8))) {
                    String line;
                    while ((line = r.readLine()) != null) {
                        int hash = line.indexOf('#');
                        if (hash >= 0) {
                            line = line.substring(0, hash);
                        }
                        line = line.trim();
                        if (!line.isEmpty()) {
                            names.add(line);
                        }
                    }
                }
            }
        } catch (Throwable t) {
            System.out.println("   M-READ THREW " + t.getClass().getName() + ": " + t.getMessage());
            return;
        }
        Collections.sort(names);
        System.out.println("   M-names=" + names);
        for (String cn : names) {
            try {
                Class<?> c = Class.forName(cn, false, cl);
                boolean named = c.getModule() != null && c.getModule().isNamed();
                boolean assignable = svc.isAssignableFrom(c);
                Object inst = assignable ? c.getDeclaredConstructor().newInstance() : null;
                // `named` is the rung ServiceLoader skips on. Printing it beside
                // the two that succeed is what makes the skip visible at all.
                System.out.println("   M-PROVIDER " + cn
                        + " moduleIsNamed=" + named
                        + " assignable=" + assignable
                        + " instantiated=" + (inst != null)
                        + (named ? "   <- ServiceLoader SKIPS this, silently" : ""));
            } catch (Throwable t) {
                System.out.println("   M-PROVIDER " + cn + " THREW "
                        + t.getClass().getName() + ": " + t.getMessage());
            }
        }
    }

    // ------------------------------------------------------ SERVICELOADER ---

    private static void viaServiceLoader(Class<?> svc, ClassLoader cl) {
        ServiceLoader<?> sl;
        try {
            @SuppressWarnings("unchecked")
            ServiceLoader<?> tmp = ServiceLoader.load((Class<Object>) svc, cl);
            sl = tmp;
        } catch (Throwable t) {
            System.out.println("   S-LOAD THREW " + t.getClass().getName() + ": " + t.getMessage());
            return;
        }
        for (String f : new String[] { "serviceName", "loader", "layer" }) {
            System.out.println("   S-field " + f + " = " + fieldOf(sl, f));
        }
        try {
            List<String> found = new ArrayList<>();
            Iterator<?> it = sl.iterator();
            // The iterator's own CLASS, not just what it yields. HotSpot hands
            // back `java.util.ServiceLoader$2`, the lazy iterator that raises a
            // `ServiceConfigurationError` at the offending provider rather than
            // at `load`. Compatible mode's `ServiceLoader.iterator` native
            // materialises the providers into a list first and returns
            // `java.util.ArrayList$Itr` -- same provider SET, different
            // semantics, and a probe that only counted providers could not see
            // it.
            System.out.println("   S-iteratorClass=" + it.getClass().getName());
            while (it.hasNext()) {
                found.add(it.next().getClass().getName());
            }
            Collections.sort(found);
            System.out.println("   S-iterator count=" + found.size() + " " + found);
        } catch (Throwable t) {
            System.out.println("   S-ITERATE THREW " + t.getClass().getName()
                    + ": " + t.getMessage());
        }
        try {
            System.out.println("   S-streamCount=" + sl.stream().count());
        } catch (Throwable t) {
            System.out.println("   S-STREAM THREW " + t.getClass().getName()
                    + ": " + t.getMessage());
        }
    }

    // ------------------------------------------------------------- BINDING --

    /** The consequence an application actually meets. */
    private static void binding() {
        try {
            Class<?> lf = Class.forName("org.slf4j.LoggerFactory");
            Object factory = lf.getMethod("getILoggerFactory").invoke(null);
            System.out.println("DOD BIND ILoggerFactory=" + factory.getClass().getName());
        } catch (Throwable t) {
            Throwable c = t.getCause() == null ? t : t.getCause();
            System.out.println("DOD BIND LoggerFactory THREW " + c.getClass().getName()
                    + ": " + c.getMessage());
        }
    }

    // --------------------------------------------------------------- utils --

    private static Class<?> load(String n, ClassLoader cl) {
        try {
            return Class.forName(n, false, cl);
        } catch (Throwable t) {
            return null;
        }
    }

    /** Field value rendered as a TYPE, never an identity. */
    private static String fieldOf(Object o, String name) {
        Class<?> c = o.getClass();
        while (c != null) {
            try {
                Field f = c.getDeclaredField(name);
                f.setAccessible(true);
                Object v = f.get(o);
                if (v == null) {
                    return "null";
                }
                if (v instanceof Class<?>) {
                    return "Class:" + ((Class<?>) v).getName();
                }
                if (v instanceof String) {
                    return "String:" + v;
                }
                return v.getClass().getName();
            } catch (NoSuchFieldException e) {
                c = c.getSuperclass();
            } catch (Throwable t) {
                return "<inaccessible: " + t.getClass().getSimpleName() + ">";
            }
        }
        return "<no such field>";
    }

    /** Last path segment, so a host-specific directory never enters the diff. */
    private static String basename(URL u) {
        String s = u.toString();
        int bang = s.indexOf("!/");
        String left = bang < 0 ? s : s.substring(0, bang);
        int slash = left.lastIndexOf('/');
        return slash < 0 ? left : left.substring(slash + 1);
    }
}
