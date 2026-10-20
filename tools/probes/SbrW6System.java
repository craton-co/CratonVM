// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.nio.charset.StandardCharsets;
import java.nio.file.Paths;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.EnumMap;
import java.util.EnumSet;
import java.util.List;
import java.util.Properties;
import java.util.StringJoiner;
import java.util.concurrent.atomic.AtomicReference;
import java.util.function.Supplier;

/**
 * SBR wave 6, lane {@code system}: {@code java.lang.System} (properties,
 * {@code lineSeparator}, {@code getenv}, {@code gc}, {@code load*},
 * {@code getLogger}) and the {@code JavaLangAccess} surface ({@code System$1})
 * as reached through public API: {@code UnixPath} (byte/string NoRepl),
 * {@code StringJoiner} ({@code join}), enums ({@code getEnumConstantsShared}),
 * virtual/carrier threads ({@code currentCarrierThread}, {@code start}),
 * {@code ModuleLayer} ({@code layers}), annotations ({@code getConstantPool}),
 * {@code Class.forName} ({@code findBootstrapClassOrNull}).
 *
 * <p>Every row is evaluated ONCE and printed (the first-run value -- a native
 * that wins from the first call shows here), then re-evaluated inside a hot
 * loop and compared with that first value; {@code drift} counts mismatches, so
 * cold == warm is asserted rather than assumed. Diff the whole output against
 * HotSpot 25. Nothing printed depends on identity hashes.
 *
 * <pre>
 *   java SbrW6System                       (HotSpot oracle)
 *   cratonvm --jdk-only -cp . SbrW6System
 * </pre>
 */
public class SbrW6System {
    static final List<String> LABELS = new ArrayList<>();
    static final List<Supplier<String>> ROWS = new ArrayList<>();

    static void row(String label, Supplier<String> s) {
        LABELS.add(label);
        ROWS.add(s);
    }

    interface X {
        Object get() throws Throwable;
    }

    static void rowx(String label, X x) {
        row(label, () -> {
            try {
                return String.valueOf(x.get());
            } catch (Throwable t) {
                return "!" + t.getClass().getName() + ": " + t.getMessage();
            }
        });
    }

    static String hex(String s) {
        if (s == null) return "null";
        StringBuilder b = new StringBuilder();
        for (int i = 0; i < s.length(); i++) b.append(Integer.toHexString(s.charAt(i))).append(' ');
        return b.toString().trim();
    }

    enum Color { RED, GREEN, BLUE }

    static class Tiny {
        static int f() {
            return 7;
        }
    }

    static final String K = "sbrw6.system.k";

    public static void main(String[] args) throws Throwable {
        // ---- getProperty / setProperty / clearProperty ----------------------
        rowx("gp os.name", () -> System.getProperty("os.name"));
        rowx("gp file.separator", () -> System.getProperty("file.separator"));
        rowx("gp path.separator", () -> System.getProperty("path.separator"));
        rowx("gp line.separator", () -> hex(System.getProperty("line.separator")));
        rowx("gp java.specification.version", () -> System.getProperty("java.specification.version"));
        rowx("gp java.vm.specification.name", () -> System.getProperty("java.vm.specification.name"));
        rowx("gp user.language", () -> System.getProperty("user.language"));
        rowx("gp missing", () -> System.getProperty("sbrw6.absent"));
        rowx("gp missing default", () -> System.getProperty("sbrw6.absent", "dflt"));
        rowx("gp present default", () -> System.getProperty("file.separator", "dflt"));
        rowx("gp null", () -> System.getProperty(null));
        rowx("gp empty", () -> System.getProperty(""));
        rowx("gp null default", () -> System.getProperty(null, "d"));
        rowx("gp empty default", () -> System.getProperty("", "d"));
        rowx("sp null key", () -> System.setProperty(null, "v"));
        rowx("sp empty key", () -> System.setProperty("", "v"));
        rowx("sp null value", () -> System.setProperty(K + ".nv", null));
        rowx("cp null", () -> System.clearProperty(null));
        rowx("cp empty", () -> System.clearProperty(""));
        rowx("sp roundtrip", () -> {
            String a = System.setProperty(K, "one");
            String b = System.getProperty(K);
            String c = System.setProperty(K, "two");
            String d = System.getProperty(K, "x");
            String e = System.clearProperty(K);
            String f = System.getProperty(K);
            String g = System.clearProperty(K);
            return a + "|" + b + "|" + c + "|" + d + "|" + e + "|" + f + "|" + g;
        });
        rowx("sp visible through getProperties", () -> {
            System.setProperty(K + ".p", "pv");
            try {
                Properties p = System.getProperties();
                return p.getProperty(K + ".p") + "|" + p.containsKey(K + ".p") + "|" + p.get(K + ".p");
            } finally {
                System.clearProperty(K + ".p");
            }
        });
        rowx("cp visible through getProperties", () -> {
            System.setProperty(K + ".c", "cv");
            Properties p = System.getProperties();
            String before = p.getProperty(K + ".c");
            System.clearProperty(K + ".c");
            return before + "|" + p.getProperty(K + ".c") + "|" + p.containsKey(K + ".c");
        });
        rowx("getProperties identity", () -> System.getProperties() == System.getProperties());
        rowx("getProperties write then getProperty", () -> {
            Properties p = System.getProperties();
            p.setProperty(K + ".w", "wv");
            try {
                return System.getProperty(K + ".w") + "|" + System.getProperty(K + ".w", "d");
            } finally {
                p.remove(K + ".w");
            }
        });
        rowx("getProperties remove then getProperty", () -> {
            Properties p = System.getProperties();
            System.setProperty(K + ".r", "rv");
            p.remove(K + ".r");
            return System.getProperty(K + ".r") + "|" + System.getProperties().containsKey(K + ".r");
        });
        rowx("getProperties put non-string", () -> {
            Properties p = System.getProperties();
            p.put(K + ".o", 42);
            try {
                return System.getProperty(K + ".o") + "|" + p.get(K + ".o");
            } finally {
                p.remove(K + ".o");
            }
        });
        rowx("getProperties has java.home", () -> System.getProperties().getProperty("java.home") != null);
        rowx("getProperties size positive", () -> System.getProperties().size() > 20);
        rowx("stringPropertyNames has os.name", () -> System.getProperties().stringPropertyNames().contains("os.name"));
        rowx("setProperties round trip", () -> {
            Properties old = System.getProperties();
            Properties p = new Properties();
            p.setProperty("a.b", "c");
            System.setProperties(p);
            try {
                return System.getProperty("a.b") + "|" + System.getProperty("os.name") + "|"
                    + (System.getProperties() == p);
            } finally {
                System.setProperties(old);
            }
        });
        rowx("setProperties restored", () -> System.getProperty("os.name") != null
            && System.getProperty("a.b") == null);
        rowx("line.separator write does not move lineSeparator", () -> {
            String before = System.lineSeparator();
            String old = System.getProperty("line.separator");
            System.setProperty("line.separator", "XY");
            try {
                return hex(System.lineSeparator()) + "|" + hex(before) + "|" + (System.lineSeparator() == before);
            } finally {
                System.setProperty("line.separator", old);
            }
        });
        rowx("lineSeparator", () -> hex(System.lineSeparator()));
        rowx("lineSeparator identity", () -> System.lineSeparator() == System.lineSeparator());
        rowx("format %n", () -> hex(String.format("%n")));
        rowx("java.io.tmpdir type", () -> System.getProperty("java.io.tmpdir") != null);

        // ---- getenv ---------------------------------------------------------
        rowx("getenv PATH nonnull", () -> System.getenv("PATH") != null);
        rowx("getenv absent", () -> System.getenv("SBRW6_NO_SUCH_VAR"));
        rowx("getenv null", () -> System.getenv(null));
        rowx("getenv() identity", () -> System.getenv() == System.getenv());
        rowx("getenv() contains PATH", () -> System.getenv().containsKey("PATH"));
        rowx("getenv() agrees", () -> System.getenv().get("PATH").equals(System.getenv("PATH")));
        rowx("getenv() put", () -> System.getenv().put("A", "b"));
        rowx("getenv() remove", () -> System.getenv().remove("PATH"));
        rowx("getenv() class", () -> System.getenv().getClass().getName());
        rowx("getenv() get non-string", () -> System.getenv().get(1));
        rowx("getenv() get null", () -> System.getenv().get(null));
        rowx("getenv() containsKey null", () -> System.getenv().containsKey(null));
        rowx("getenv() size positive", () -> System.getenv().size() > 0);
        rowx("getenv empty name", () -> System.getenv(""));

        // ---- gc -------------------------------------------------------------
        rowx("gc", () -> {
            System.gc();
            return Runtime.getRuntime().freeMemory() > 0;
        });

        // ---- load / loadLibrary ---------------------------------------------
        rowx("loadLibrary missing", () -> { System.loadLibrary("sbrw6_no_such_lib"); return "loaded"; });
        rowx("loadLibrary null", () -> { System.loadLibrary(null); return "loaded"; });
        rowx("loadLibrary slash", () -> { System.loadLibrary("a/b"); return "loaded"; });
        rowx("load missing", () -> { System.load("/sbrw6/no/such/lib.so"); return "loaded"; });
        rowx("load relative", () -> { System.load("sbrw6rel.so"); return "loaded"; });
        rowx("load null", () -> { System.load(null); return "loaded"; });
        rowx("mapLibraryName", () -> System.mapLibraryName("foo"));
        rowx("mapLibraryName null", () -> System.mapLibraryName(null));

        // ---- getLogger ------------------------------------------------------
        rowx("getLogger name", () -> System.getLogger("sbrw6.logger").getName());
        rowx("getLogger null", () -> System.getLogger(null));
        rowx("getLogger class", () -> System.getLogger("sbrw6.logger").getClass().getName());
        rowx("getLogger isLoggable ALL", () -> System.getLogger("sbrw6.logger").isLoggable(System.Logger.Level.ALL));
        rowx("getLogger isLoggable WARNING", () -> System.getLogger("sbrw6.logger").isLoggable(System.Logger.Level.WARNING));
        rowx("getLogger isLoggable TRACE", () -> System.getLogger("sbrw6.logger").isLoggable(System.Logger.Level.TRACE));
        rowx("getLogger isLoggable INFO", () -> System.getLogger("sbrw6.logger").isLoggable(System.Logger.Level.INFO));
        rowx("getLogger isLoggable DEBUG", () -> System.getLogger("sbrw6.logger").isLoggable(System.Logger.Level.DEBUG));
        rowx("getLogger isLoggable OFF", () -> System.getLogger("sbrw6.logger").isLoggable(System.Logger.Level.OFF));
        rowx("getLogger same twice", () -> System.getLogger("sbrw6.same") == System.getLogger("sbrw6.same"));
        rowx("getLogger bundle null", () -> System.getLogger("sbrw6.b", null).getName());
        rowx("getLogger log DEBUG quiet", () -> {
            System.getLogger("sbrw6.q").log(System.Logger.Level.DEBUG, "quiet");
            return "ok";
        });

        // ---- JavaLangAccess through public API --------------------------------
        rowx("path ascii", () -> Paths.get("/tmp/sbrw6/a.txt").toString());
        rowx("path non-ascii", () -> Paths.get("/tmp/sbrw6/é中.txt").getFileName().toString());
        rowx("path non-ascii hex", () -> hex(Paths.get("/tmp/sbrw6/é中.txt").getFileName().toString()));
        rowx("path to bytes", () -> Arrays.toString(Paths.get("/tmp/é").toString().getBytes(StandardCharsets.UTF_8)));
        rowx("path resolve", () -> Paths.get("/a/b").resolve("c/../d").normalize());
        rowx("path relativize", () -> Paths.get("/a/b/c").relativize(Paths.get("/a/x")));
        rowx("path getName count", () -> Paths.get("/a/b/c").getNameCount());
        rowx("path startsWith", () -> Paths.get("/a/b/c").startsWith(Paths.get("/a/b")));
        rowx("path invalid nul", () -> Paths.get("/a\u0000b"));
        rowx("path toUri", () -> Paths.get("/tmp/sbrw6/é").toUri());
        rowx("string join", () -> String.join("-", "a", "b", "c"));
        rowx("string join empty", () -> String.join("-", new ArrayList<String>()));
        rowx("string join one", () -> String.join("-", List.of("x")));
        rowx("string join null elem", () -> String.join(",", "a", null, "c"));
        rowx("StringJoiner", () -> {
            StringJoiner j = new StringJoiner(", ", "[", "]");
            j.add("a").add("b").add("c");
            return j + "|" + j.length();
        });
        rowx("StringJoiner empty", () -> new StringJoiner(",", "<", ">").toString());
        rowx("StringJoiner emptyValue", () -> new StringJoiner(",", "<", ">").setEmptyValue("EMPTY").toString());
        rowx("StringJoiner mixed", () -> {
            StringJoiner j = new StringJoiner("/");
            j.add("").add("a").add("");
            return j.toString() + "|" + j.length();
        });
        rowx("Collectors.joining", () -> List.of("p", "q", "r").stream().collect(java.util.stream.Collectors.joining(",", "{", "}")));
        rowx("enum valueOf", () -> Enum.valueOf(Color.class, "GREEN"));
        rowx("enum valueOf bad", () -> Enum.valueOf(Color.class, "PINK"));
        rowx("enum values via EnumSet", () -> EnumSet.allOf(Color.class));
        rowx("enum complementOf", () -> EnumSet.complementOf(EnumSet.of(Color.RED)));
        rowx("EnumMap", () -> {
            EnumMap<Color, Integer> m = new EnumMap<>(Color.class);
            m.put(Color.BLUE, 3);
            m.put(Color.RED, 1);
            return m;
        });
        rowx("Class.getEnumConstants", () -> Arrays.toString(Color.class.getEnumConstants()));
        rowx("Class.getEnumConstants non-enum", () -> Arrays.toString(String.class.getEnumConstants()));
        rowx("string new UTF8", () -> new String(new byte[] {(byte) 0xc3, (byte) 0xa9, 65}, StandardCharsets.UTF_8).length());
        rowx("string new UTF8 malformed", () -> hex(new String(new byte[] {(byte) 0xc3, 65}, StandardCharsets.UTF_8)));
        rowx("string getBytes UTF8", () -> Arrays.toString("éA😀".getBytes(StandardCharsets.UTF_8)));
        rowx("string getBytes lone surrogate", () -> Arrays.toString("a\ud83db".getBytes(StandardCharsets.UTF_8)));
        rowx("string getBytes latin1", () -> Arrays.toString("éA".getBytes(StandardCharsets.ISO_8859_1)));
        rowx("string getBytes ascii", () -> Arrays.toString("éA".getBytes(StandardCharsets.US_ASCII)));
        rowx("string getBytes utf16", () -> Arrays.toString("A".getBytes(StandardCharsets.UTF_16)));
        rowx("string new bytes ascii", () -> new String(new byte[] {(byte) 0xe9, 65}, StandardCharsets.US_ASCII).length());

        // Threads: carrier thread, container start
        rowx("virtual thread runs", () -> {
            AtomicReference<String> r = new AtomicReference<>();
            Thread t = Thread.ofVirtual().name("v-sbrw6").start(() -> r.set(Thread.currentThread().getName()
                + "|" + Thread.currentThread().isVirtual()));
            t.join();
            return r.get();
        });
        rowx("platform thread runs", () -> {
            AtomicReference<String> r = new AtomicReference<>();
            Thread t = Thread.ofPlatform().name("p-sbrw6").start(() -> r.set(Thread.currentThread().getName()
                + "|" + Thread.currentThread().isVirtual()));
            t.join();
            return r.get();
        });
        rowx("thread per task executor", () -> {
            try (var ex = java.util.concurrent.Executors.newThreadPerTaskExecutor(
                    Thread.ofVirtual().name("tpt-", 0).factory())) {
                var f = ex.submit(() -> Thread.currentThread().getName());
                return f.get();
            }
        });
        rowx("virtual sleep", () -> {
            AtomicReference<String> r = new AtomicReference<>();
            Thread t = Thread.startVirtualThread(() -> {
                try {
                    Thread.sleep(2);
                    r.set("slept");
                } catch (InterruptedException e) {
                    r.set("int");
                }
            });
            t.join();
            return r.get();
        });

        // Modules: layers, ModuleLayer
        rowx("boot layer has java.base", () -> ModuleLayer.boot().findModule("java.base").isPresent());
        rowx("java.base layer", () -> Object.class.getModule().getLayer() == ModuleLayer.boot());
        rowx("unnamed module layer", () -> SbrW6System.class.getModule().getLayer());
        rowx("unnamed module name", () -> SbrW6System.class.getModule().getName());
        rowx("module isNamed", () -> Object.class.getModule().isNamed());
        rowx("module addReads unnamed", () -> {
            Module m = SbrW6System.class.getModule();
            return m.addReads(Object.class.getModule()) == m;
        });
        rowx("module addExports self", () -> {
            Module m = SbrW6System.class.getModule();
            return m.addExports("", m) == m;
        });
        rowx("module exports java.lang", () -> Object.class.getModule().isExported("java.lang"));
        rowx("module isOpen jdk.internal", () -> Object.class.getModule().isOpen("jdk.internal.misc"));
        rowx("classloader unnamed module", () -> SbrW6System.class.getClassLoader().getUnnamedModule() == SbrW6System.class.getModule());

        // Annotations use JLA.getConstantPool through the real parser
        rowx("annotation parse", () -> Color.class.getAnnotations().length);
        rowx("deprecated annotation", () -> Thread.class.getMethod("stop").isAnnotationPresent(Deprecated.class));
        rowx("FunctionalInterface annotation", () -> Runnable.class.isAnnotationPresent(FunctionalInterface.class));
        // Bootstrap class lookups (findBootstrapClassOrNull)
        rowx("Class.forName boot", () -> Class.forName("java.util.ArrayList").getName());
        rowx("Class.forName module boot", () -> Class.forName(Object.class.getModule(), "java.util.HashMap").getName());
        rowx("Class.forName module missing", () -> Class.forName(Object.class.getModule(), "no.Such"));
        rowx("charset lookup", () -> java.nio.charset.Charset.forName("UTF-8").name());
        rowx("hidden lookup define", () -> {
            byte[] b = SbrW6System.class.getResourceAsStream("SbrW6System$Tiny.class").readAllBytes();
            var l = java.lang.invoke.MethodHandles.lookup();
            Class<?> c = l.defineHiddenClass(b, false).lookupClass();
            return c.isHidden() + "|" + c.getName().startsWith("SbrW6System$Tiny/0x")
                + "|" + c.getDeclaredMethods().length;
        });

        // ---- run: first evaluation printed, then hot loop compared ---------
        int n = ROWS.size();
        String[] first = new String[n];
        for (int i = 0; i < n; i++) {
            first[i] = safe(ROWS.get(i));
            System.out.println(LABELS.get(i) + " = " + first[i]);
        }
        int drift = 0;
        for (int it = 0; it < 300; it++) {
            for (int i = 0; i < n; i++) {
                String l = LABELS.get(i);
                // The load / logger / thread rows are slow or allocate a lot; sample them.
                if ((l.startsWith("load") || l.startsWith("gc") || l.contains("hread")
                        || l.contains("virtual") || l.contains("hidden") || l.contains("executor"))
                        && it % 30 != 0) {
                    continue;
                }
                String v = safe(ROWS.get(i));
                if (!v.equals(first[i])) {
                    drift++;
                    if (drift < 8) System.out.println("DRIFT " + l + ": " + first[i] + " -> " + v);
                }
            }
        }
        System.out.println("rows=" + n + " drift=" + drift);
    }

    static String safe(Supplier<String> s) {
        try {
            return s.get();
        } catch (Throwable t) {
            return "!!" + t;
        }
    }
}
