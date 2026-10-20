// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.io.InputStream;
import java.lang.module.ModuleDescriptor;
import java.lang.reflect.InvocationTargetException;
import java.net.URL;
import java.security.AccessControlContext;
import java.security.AccessController;
import java.security.CodeSource;
import java.security.PrivilegedAction;
import java.security.PrivilegedExceptionAction;
import java.security.ProtectionDomain;
import java.util.ArrayList;
import java.util.List;
import java.util.Set;
import java.util.TreeSet;
import java.util.function.Supplier;

/**
 * SBR wave 6, lane {@code module}: {@code java.lang.Module}, {@code ModuleLayer},
 * {@code jdk.internal.module.Builder}, {@code BootLoader}, {@code ClassLoader},
 * {@code Class.forName/getClassLoader0/desiredAssertionStatus/getResource*}/
 * {@code getPackage*}, {@code CodeSource}, {@code AccessController}.
 *
 * <p>Every row is evaluated ONCE and printed (the first-run value), then re-evaluated inside a hot
 * loop and compared with it; {@code drift} counts mismatches. Both ends of each predicate are
 * exercised (a row that answers only null/true/false everywhere would pass a default-value
 * agreement, which is the trap L0 recorded). Diff the whole output against HotSpot 25.
 *
 * <pre>
 *   java -cp . SbrW6Module                     (HotSpot oracle)
 *   cratonvm --jdk-only -cp . SbrW6Module
 * </pre>
 */
public class SbrW6Module {
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
            } catch (InvocationTargetException e) {
                Throwable c = e.getCause();
                return "!ITE(" + c.getClass().getName() + ": " + c.getMessage() + ")";
            } catch (Throwable t) {
                return "!" + t.getClass().getName() + ": " + t.getMessage();
            }
        });
    }

    static String safe(Supplier<String> s) {
        try {
            return s.get();
        } catch (Throwable t) {
            return "!" + t.getClass().getName() + ": " + t.getMessage();
        }
    }

    static String units(String s) {
        if (s == null) return "null";
        StringBuilder b = new StringBuilder();
        for (int i = 0; i < s.length(); i++) {
            char c = s.charAt(i);
            if (c < 0x20 || c > 0x7e) b.append(String.format("\\u%04x", (int) c));
            else b.append(c);
        }
        return b.toString();
    }

    static String mask(String s) {
        return s == null ? "null" : s.replaceAll("@[0-9a-f]{4,}", "@H").replaceAll("file:/[^ ,)\\]]*", "file:PATH");
    }

    static String nm(Object o) {
        return o == null ? "null" : o.getClass().getName();
    }

    static String ldr(ClassLoader l) {
        return l == null ? "null" : l.getName() + "/" + l.getClass().getName();
    }

    public static class Fixture {
        public static boolean inited;
        static {
            inited = true;
        }
    }

    public static class Fixture2 {
        public static boolean inited;
        static {
            inited = true;
        }
    }

    public static class Fixture3 {
        public static boolean inited;
        static {
            inited = true;
        }
    }

    public static class MyLoader extends ClassLoader {
        public MyLoader() {
            super();
        }

        public MyLoader(ClassLoader parent) {
            super(parent);
        }

        public MyLoader(String name, ClassLoader parent) {
            super(name, parent);
        }

        public Class<?> pubLoad(String n, boolean r) throws ClassNotFoundException {
            return loadClass(n, r);
        }

        public Package pubDefinePackage(String n, String st, String sv, String svd, String it, String iv, String ivd) {
            return definePackage(n, st, sv, svd, it, iv, ivd, null);
        }

        public Object pubLock(String n) {
            return getClassLoadingLock(n);
        }
    }

    public static class Def extends ClassLoader {
        final byte[] b;
        final String n;

        Def(ClassLoader p, String n, byte[] b) {
            super("def", p);
            this.n = n;
            this.b = b;
        }

        Object pubFindLoaded(String n) {
            return findLoadedClass(n);
        }

        Object pubLock(String n) {
            return getClassLoadingLock(n);
        }

        @Override
        protected Class<?> findClass(String name) throws ClassNotFoundException {
            if (name.equals(n)) return defineClass(name, b, 0, b.length);
            throw new ClassNotFoundException(name);
        }
    }

    static byte[] bytesOf(Class<?> c) throws Exception {
        try (InputStream in = c.getClassLoader().getResourceAsStream(c.getName().replace('.', '/') + ".class")) {
            return in.readAllBytes();
        }
    }

    static String setStr(Set<?> s) {
        List<String> l = new ArrayList<>();
        for (Object o : s) l.add(String.valueOf(o));
        return new TreeSet<>(l).toString();
    }

    public static void main(String[] args) throws Exception {
        Module base = Object.class.getModule();
        Module me = SbrW6Module.class.getModule();
        Module logging = java.util.logging.Logger.class.getModule();
        Module sql = null;
        try {
            sql = ModuleLayer.boot().findModule("java.sql").orElse(null);
        } catch (Throwable t) {
        }
        final Module sqlF = sql;

        // ---------------------------------------------------------------- Module
        for (Module m : new Module[] {base, me, logging}) {
            String k = m == base ? "base" : m == me ? "app" : "logging";
            rowx("module." + k + ".name", () -> m.getName());
            rowx("module." + k + ".isNamed", () -> m.isNamed());
            rowx("module." + k + ".toString", () -> m.toString());
            rowx("module." + k + ".loader", () -> ldr(m.getClassLoader()));
            rowx("module." + k + ".layer", () -> m.getLayer() == null ? "null" : (m.getLayer() == ModuleLayer.boot() ? "boot" : "other"));
            rowx("module." + k + ".descriptor", () -> {
                ModuleDescriptor d = m.getDescriptor();
                return d == null ? "null" : d.name() + " open=" + d.isOpen() + " auto=" + d.isAutomatic();
            });
            rowx("module." + k + ".descriptor.exports", () -> {
                ModuleDescriptor d = m.getDescriptor();
                return d == null ? "null" : String.valueOf(d.exports().stream().anyMatch(e -> e.source().equals("java.lang")))
                        + " " + d.exports().stream().anyMatch(e -> e.isQualified() && e.source().equals("jdk.internal.misc"));
            });
            rowx("module." + k + ".descriptor.requires", () -> {
                ModuleDescriptor d = m.getDescriptor();
                return d == null ? "null" : setStr(d.requires().stream().map(r -> r.name()).collect(java.util.stream.Collectors.toSet()));
            });
            rowx("module." + k + ".descriptor.uses", () -> {
                ModuleDescriptor d = m.getDescriptor();
                return d == null ? "null" : d.uses().size() > 0;
            });
            rowx("module." + k + ".descriptor.same", () -> m.getDescriptor() == m.getDescriptor());
            rowx("module." + k + ".exported.java.lang", () -> m.isExported("java.lang"));
            rowx("module." + k + ".exported.nonexistent", () -> m.isExported("no.such.pkg"));
            rowx("module." + k + ".exported.jdk.internal.misc", () -> m.isExported("jdk.internal.misc"));
            rowx("module." + k + ".exported.jim.to.me", () -> m.isExported("jdk.internal.misc", me));
            rowx("module." + k + ".exported.jim.to.base", () -> m.isExported("jdk.internal.misc", base));
            rowx("module." + k + ".exported.jl.to.me", () -> m.isExported("java.lang", me));
            rowx("module." + k + ".open.java.lang", () -> m.isOpen("java.lang"));
            rowx("module." + k + ".open.java.lang.to.me", () -> m.isOpen("java.lang", me));
            rowx("module." + k + ".open.java.lang.to.base", () -> m.isOpen("java.lang", base));
            rowx("module." + k + ".open.nonexistent", () -> m.isOpen("no.such.pkg"));
            rowx("module." + k + ".exported.null", () -> m.isExported(null));
            rowx("module." + k + ".open.null", () -> m.isOpen(null));
            rowx("module." + k + ".canRead.base", () -> m.canRead(base));
            rowx("module." + k + ".canRead.me", () -> m.canRead(me));
            rowx("module." + k + ".canRead.logging", () -> m.canRead(logging));
            rowx("module." + k + ".canRead.null", () -> m.canRead(null));
            rowx("module." + k + ".canUse.Runnable", () -> m.canUse(Runnable.class));
            rowx("module." + k + ".canUse.ToolProvider", () -> m.canUse(java.util.spi.ToolProvider.class));
            rowx("module." + k + ".canUse.null", () -> m.canUse(null));
            rowx("module." + k + ".packages", () -> m.getPackages().contains("java.lang") + " " + m.getPackages().contains(
                    SbrW6Module.class.getPackageName().isEmpty() ? "zzz" : SbrW6Module.class.getPackageName()) + " " + m.getPackages().size() / 1000);
            rowx("module." + k + ".getResourceAsStream.class", () -> {
                try (InputStream in = m.getResourceAsStream("java/lang/Object.class")) {
                    return in == null ? "null" : "stream";
                }
            });
            rowx("module." + k + ".getResourceAsStream.missing", () -> {
                try (InputStream in = m.getResourceAsStream("no/such/Thing.class")) {
                    return in == null ? "null" : "stream";
                }
            });
            rowx("module." + k + ".getResourceAsStream.null", () -> m.getResourceAsStream(null));
            rowx("module." + k + ".annotations", () -> m.getAnnotations().length);
        }
        rowx("module.unnamed.of.loader.same", () -> ClassLoader.getSystemClassLoader().getUnnamedModule() == me);
        rowx("module.unnamed.of.mine", () -> new MyLoader().getUnnamedModule().getName() + " " + new MyLoader().getUnnamedModule().isNamed());
        rowx("module.unnamed.of.mine.loader", () -> {
            MyLoader l = new MyLoader();
            return l.getUnnamedModule().getClassLoader() == l;
        });
        rowx("module.unnamed.canRead.base", () -> new MyLoader().getUnnamedModule().canRead(base));
        rowx("module.unnamed.canUse", () -> new MyLoader().getUnnamedModule().canUse(Runnable.class));
        rowx("module.unnamed.exported", () -> new MyLoader().getUnnamedModule().isExported("anything"));
        rowx("module.unnamed.open", () -> new MyLoader().getUnnamedModule().isOpen("anything"));
        rowx("module.unnamed.exported.to", () -> new MyLoader().getUnnamedModule().isExported("anything", base));
        rowx("module.unnamed.descriptor", () -> new MyLoader().getUnnamedModule().getDescriptor());
        rowx("module.unnamed.layer", () -> new MyLoader().getUnnamedModule().getLayer());
        rowx("module.unnamed.packages", () -> new MyLoader().getUnnamedModule().getPackages());
        rowx("module.sql.loader", () -> sqlF == null ? "none" : ldr(sqlF.getClassLoader()));
        rowx("module.sql.name", () -> sqlF == null ? "none" : sqlF.getName());
        rowx("module.sql.canRead.logging", () -> sqlF == null ? "none" : sqlF.canRead(logging));
        rowx("module.sql.canRead.me", () -> sqlF == null ? "none" : sqlF.canRead(me));
        rowx("module.sql.requires", () -> sqlF == null ? "none" : setStr(sqlF.getDescriptor().requires().stream().map(r -> r.name()).collect(java.util.stream.Collectors.toSet())));
        rowx("module.sql.uses", () -> sqlF == null ? "none" : setStr(sqlF.getDescriptor().uses()));
        rowx("module.sql.canUse.Driver", () -> sqlF == null ? "none" : sqlF.canUse(java.sql.Driver.class));
        rowx("module.sql.canUse.Runnable", () -> sqlF == null ? "none" : sqlF.canUse(Runnable.class));
        rowx("module.sql.exported.java.sql", () -> sqlF == null ? "none" : sqlF.isExported("java.sql"));
        rowx("module.sql.exported.javax.sql", () -> sqlF == null ? "none" : sqlF.isExported("javax.sql"));
        rowx("module.class.getModule.array", () -> String[].class.getModule().getName() + " " + int.class.getModule().getName());
        rowx("module.class.getModule.app", () -> Fixture.class.getModule() == me);
        rowx("module.class.getModule.lambda", () -> ((Runnable) () -> {}).getClass().getModule() == me);
        rowx("module.class.getModule.proxy", () -> java.lang.reflect.Proxy.newProxyInstance(
                Runnable.class.getClassLoader(), new Class<?>[] {Runnable.class}, (p, m, a) -> null).getClass().getModule().isNamed());

        // ---------------------------------------------------------------- ModuleLayer
        rowx("layer.boot.same", () -> ModuleLayer.boot() == ModuleLayer.boot());
        rowx("layer.boot.parents", () -> ModuleLayer.boot().parents());
        rowx("layer.boot.findModule.base", () -> ModuleLayer.boot().findModule("java.base").map(Module::getName));
        rowx("layer.boot.findModule.base.same", () -> ModuleLayer.boot().findModule("java.base").get() == base);
        rowx("layer.boot.findModule.missing", () -> ModuleLayer.boot().findModule("no.such.mod").isPresent());
        rowx("layer.boot.findModule.null", () -> ModuleLayer.boot().findModule(null));
        rowx("layer.boot.modules.hasBase", () -> ModuleLayer.boot().modules().contains(base) + " " + ModuleLayer.boot().modules().contains(me));
        rowx("layer.boot.modules.size", () -> ModuleLayer.boot().modules().size() > 10);
        rowx("layer.boot.modules.allNamed", () -> ModuleLayer.boot().modules().stream().allMatch(Module::isNamed));
        rowx("layer.boot.modules.layer", () -> ModuleLayer.boot().modules().stream().allMatch(m -> m.getLayer() == ModuleLayer.boot()));
        rowx("layer.boot.configuration", () -> ModuleLayer.boot().configuration().findModule("java.base").isPresent());
        rowx("layer.boot.findLoader.base", () -> ModuleLayer.boot().findLoader("java.base"));
        rowx("layer.boot.findLoader.missing", () -> ModuleLayer.boot().findLoader("no.such"));
        rowx("layer.empty", () -> ModuleLayer.empty().modules().size() + " " + ModuleLayer.empty().parents());
        rowx("layer.boot.toString", () -> ModuleLayer.boot().toString().startsWith("java.base"));

        // ---------------------------------------------------------------- ClassLoader
        ClassLoader sys = ClassLoader.getSystemClassLoader();
        ClassLoader plat = ClassLoader.getPlatformClassLoader();
        rowx("cl.system.name", () -> ldr(sys));
        rowx("cl.platform.name", () -> ldr(plat));
        rowx("cl.system.parent", () -> sys.getParent() == plat);
        rowx("cl.platform.parent", () -> plat.getParent());
        rowx("cl.system.same", () -> ClassLoader.getSystemClassLoader() == sys);
        rowx("cl.platform.same", () -> ClassLoader.getPlatformClassLoader() == plat);
        rowx("cl.app.class.loader", () -> SbrW6Module.class.getClassLoader() == sys);
        rowx("cl.system.unnamedModule", () -> sys.getUnnamedModule() == me);
        rowx("cl.platform.unnamedModule", () -> plat.getUnnamedModule().getName() + " " + (plat.getUnnamedModule().getClassLoader() == plat));
        rowx("cl.system.isRegisteredAsParallelCapable", () -> sys.isRegisteredAsParallelCapable());
        rowx("cl.system.definedPackage.app", () -> {
            Package p = sys.getDefinedPackage("java.lang");
            return p == null ? "null" : p.getName();
        });
        rowx("cl.system.getDefinedPackages.hasjava", () -> java.util.Arrays.stream(sys.getDefinedPackages()).anyMatch(p -> p.getName().equals("java.lang")));
        rowx("cl.plat.getDefinedPackages.hasjavasql", () -> java.util.Arrays.stream(plat.getDefinedPackages()).anyMatch(p -> p.getName().equals("java.sql")));
        rowx("cl.system.loadClass.String", () -> sys.loadClass("java.lang.String") == String.class);
        rowx("cl.system.loadClass.app", () -> sys.loadClass("SbrW6Module") == SbrW6Module.class);
        rowx("cl.system.loadClass.missing", () -> sys.loadClass("no.such.Klass"));
        rowx("cl.system.loadClass.null", () -> sys.loadClass(null));
        rowx("cl.system.loadClass.array", () -> sys.loadClass("[I"));
        rowx("cl.system.loadClass.noinit", () -> {
            Class<?> c = sys.loadClass("SbrW6Module$Fixture");
            return c.getSimpleName() + " " + Fixture.inited;
        });
        rowx("cl.platform.loadClass.sql", () -> plat.loadClass("java.sql.Driver").getName());
        rowx("cl.platform.loadClass.app", () -> plat.loadClass("SbrW6Module"));
        rowx("cl.platform.loadClass.String", () -> plat.loadClass("java.lang.String") == String.class);
        rowx("cl.boot.loadClass.viaClass", () -> Class.forName("java.lang.String", false, null) == String.class);
        rowx("cl.system.getResource.class", () -> sys.getResource("SbrW6Module.class") != null);
        rowx("cl.system.getResource.jdk", () -> sys.getResource("java/lang/Object.class") != null);
        rowx("cl.system.getResource.missing", () -> sys.getResource("no/such/file.txt"));
        rowx("cl.system.getResourceAsStream.class", () -> {
            try (InputStream in = sys.getResourceAsStream("SbrW6Module.class")) {
                return in.read();
            }
        });
        rowx("cl.static.getSystemResource", () -> ClassLoader.getSystemResource("SbrW6Module.class") != null);
        rowx("cl.static.getSystemResourceAsStream.missing", () -> ClassLoader.getSystemResourceAsStream("no/such"));
        rowx("cl.system.getResources.count", () -> java.util.Collections.list(sys.getResources("SbrW6Module.class")).size());
        rowx("cl.system.resources.stream", () -> sys.resources("SbrW6Module.class").count());
        rowx("cl.my.default.parent", () -> new MyLoader().getParent() == sys);
        rowx("cl.my.default.name", () -> new MyLoader().getName());
        rowx("cl.my.parent.null", () -> new MyLoader(null).getParent());
        rowx("cl.my.parent.plat", () -> new MyLoader(plat).getParent() == plat);
        rowx("cl.my.named", () -> new MyLoader("nm", plat).getName() + " " + (new MyLoader("nm", plat).getParent() == plat));
        rowx("cl.my.named.null", () -> new MyLoader(null, plat).getName());
        rowx("cl.my.named.empty", () -> new MyLoader("", plat));
        rowx("cl.my.toString", () -> mask(new MyLoader("nm", plat).toString()));
        rowx("cl.my.loadClass.String", () -> new MyLoader().loadClass("java.lang.String") == String.class);
        rowx("cl.my.loadClass.app", () -> new MyLoader().loadClass("SbrW6Module$Fixture2").getName());
        rowx("cl.my.loadClass.app.parentNull", () -> new MyLoader(null).loadClass("SbrW6Module$Fixture2"));
        rowx("cl.my.loadClass.String.parentNull", () -> new MyLoader(null).loadClass("java.lang.String") == String.class);
        rowx("cl.my.loadClass.missing", () -> new MyLoader().loadClass("no.such.Klass"));
        rowx("cl.my.loadClass.resolve", () -> new MyLoader().pubLoad("java.lang.String", true) == String.class);
        rowx("cl.my.definePackage", () -> {
            MyLoader l = new MyLoader();
            Package p = l.pubDefinePackage("my.pkg", "s", "v", "vd", "t", "tv", "tvd");
            return p.getName() + " " + p.getSpecificationTitle() + " " + (l.getDefinedPackage("my.pkg") == p);
        });
        rowx("cl.my.definePackage.twice", () -> {
            MyLoader l = new MyLoader();
            l.pubDefinePackage("my.pkg", "s", "v", "vd", "t", "tv", "tvd");
            return l.pubDefinePackage("my.pkg", "s", "v", "vd", "t", "tv", "tvd");
        });
        rowx("cl.def.findLoadedClass", () -> {
            Def d = new Def(sys, "SbrW6Module$Fixture3", bytesOf(Fixture3.class));
            Object before = d.pubFindLoaded("SbrW6Module$Fixture3");
            d.loadClass("SbrW6Module$Fixture3");
            Object after = d.pubFindLoaded("SbrW6Module$Fixture3");
            return before + " " + (after != null);
        });
        rowx("cl.system.getClassLoadingLock", () -> (new MyLoader().pubLock("x") != null) + " " + (new Def(sys, "a", new byte[0]).pubLock("x") != null));
        rowx("cl.my.lock.self", () -> {
            MyLoader l = new MyLoader();
            return l.pubLock("x") == l;
        });
        rowx("cl.my.unnamedModule", () -> new MyLoader().getUnnamedModule().isNamed());
        rowx("cl.my.getPackage.none", () -> new MyLoader().getDefinedPackage("java.lang"));
        rowx("cl.def.define", () -> {
            byte[] b = bytesOf(Fixture3.class);
            Def d = new Def(sys, "SbrW6Module$Fixture3", b);
            Class<?> c = d.loadClass("SbrW6Module$Fixture3");
            return (c != Fixture3.class) + " " + (c.getClassLoader() == d) + " " + c.getModule().isNamed() + " "
                    + (c.getModule() == d.getUnnamedModule()) + " " + c.getPackageName() + " " + c.getPackage().getName();
        });
        rowx("cl.def.define.protectionDomain", () -> {
            byte[] b = bytesOf(Fixture3.class);
            Def d = new Def(null, "SbrW6Module$Fixture3", b);
            Class<?> c = d.loadClass("SbrW6Module$Fixture3");
            ProtectionDomain pd = c.getProtectionDomain();
            return pd == null ? "null" : (pd.getCodeSource() == null ? "nocs" : "cs");
        });
        rowx("cl.def.same.twice", () -> {
            byte[] b = bytesOf(Fixture3.class);
            Def d = new Def(sys, "SbrW6Module$Fixture3", b);
            return d.loadClass("SbrW6Module$Fixture3") == d.loadClass("SbrW6Module$Fixture3");
        });
        rowx("cl.def.badBytes", () -> {
            Def d = new Def(sys, "Bad", new byte[] {1, 2, 3, 4});
            return d.loadClass("Bad");
        });
        rowx("cl.def.wrongName", () -> {
            byte[] b = bytesOf(Fixture3.class);
            Def d = new Def(sys, "Wrong", b);
            return d.loadClass("Wrong");
        });
        rowx("cl.checkCerts.viaDefine", () -> {
            byte[] b = bytesOf(Fixture3.class);
            Def d = new Def(sys, "SbrW6Module$Fixture3", b);
            Class<?> c = d.loadClass("SbrW6Module$Fixture3");
            Def d2 = new Def(sys, "SbrW6Module$Fixture3", b);
            Class<?> c2 = d2.loadClass("SbrW6Module$Fixture3");
            return c.getProtectionDomain().equals(c2.getProtectionDomain());
        });

        // ---------------------------------------------------------------- Class.*
        rowx("class.forName.1.String", () -> Class.forName("java.lang.String") == String.class);
        rowx("class.forName.1.app", () -> Class.forName("SbrW6Module$Fixture") + " " + Fixture.inited);
        rowx("class.forName.1.missing", () -> Class.forName("no.such.Klass"));
        rowx("class.forName.1.null", () -> Class.forName((String) null));
        rowx("class.forName.1.empty", () -> Class.forName(""));
        rowx("class.forName.1.slash", () -> Class.forName("java/lang/String"));
        rowx("class.forName.1.array", () -> Class.forName("[Ljava.lang.String;") == String[].class);
        rowx("class.forName.1.array.prim", () -> Class.forName("[I") == int[].class);
        rowx("class.forName.1.primitive", () -> Class.forName("int"));
        rowx("class.forName.1.badArray", () -> Class.forName("[Lno.such.K;"));
        rowx("class.forName.1.nested", () -> Class.forName("java.util.Map$Entry") == java.util.Map.Entry.class);
        rowx("class.forName.3.init.true", () -> Class.forName("SbrW6Module$Fixture2", true, sys) + " " + Fixture2.inited);
        rowx("class.forName.3.init.false", () -> {
            Class<?> c = Class.forName("SbrW6Module$Fixture3", false, sys);
            return c.getSimpleName();
        });
        rowx("class.forName.3.nullLoader.app", () -> Class.forName("SbrW6Module", false, null));
        rowx("class.forName.3.nullLoader.String", () -> Class.forName("java.lang.String", false, null) == String.class);
        rowx("class.forName.3.plat.sql", () -> Class.forName("java.sql.Driver", false, plat).getName());
        rowx("class.forName.3.plat.app", () -> Class.forName("SbrW6Module", false, plat));
        rowx("class.forName.3.my.loader", () -> Class.forName("SbrW6Module$Fixture2", false, new MyLoader()).getName());
        rowx("class.forName.3.def", () -> {
            Def d = new Def(sys, "SbrW6Module$Fixture3", bytesOf(Fixture3.class));
            Class<?> c = Class.forName("SbrW6Module$Fixture3", false, d);
            return c.getClassLoader() == d;
        });
        rowx("class.forName.3.def.array", () -> {
            Def d = new Def(sys, "SbrW6Module$Fixture3", bytesOf(Fixture3.class));
            Class<?> c = Class.forName("[LSbrW6Module$Fixture3;", false, d);
            return c.getComponentType().getClassLoader() == d;
        });
        rowx("class.forName.3.msg", () -> Class.forName("a.b.C", true, sys));
        rowx("class.forName.3.init.fails.twice", () -> {
            try {
                Class.forName("SbrW6Module$Boom2", true, sys);
            } catch (Throwable t) {
            }
            return Class.forName("SbrW6Module$Boom2", true, sys);
        });
        rowx("class.forName.2.module.String", () -> Class.forName(base, "java.lang.String") == String.class);
        rowx("class.forName.2.module.missing", () -> Class.forName(base, "no.such.Klass"));
        rowx("class.forName.2.module.wrong", () -> Class.forName(base, "SbrW6Module"));
        rowx("class.forName.2.module.app", () -> Class.forName(me, "SbrW6Module") == SbrW6Module.class);
        rowx("class.forName.2.module.logging", () -> Class.forName(logging, "java.util.logging.Logger") == java.util.logging.Logger.class);
        rowx("class.forName.2.module.nullmod", () -> Class.forName((Module) null, "java.lang.String"));
        rowx("class.forName.2.module.nullname", () -> Class.forName(base, null));
        rowx("class.forName.2.module.array", () -> Class.forName(base, "[I"));

        Class<?>[] cs = {String.class, SbrW6Module.class, java.util.logging.Logger.class, int.class, int[].class, String[].class,
            Fixture.class, java.util.Map.Entry.class, Runnable.class};
        for (Class<?> c : cs) {
            String k = c.getName();
            rowx("class." + k + ".loader", () -> ldr(c.getClassLoader()));
            rowx("class." + k + ".packageName", () -> c.getPackageName());
            rowx("class." + k + ".package", () -> {
                Package p = c.getPackage();
                return p == null ? "null" : p.getName() + " " + p.isSealed();
            });
            rowx("class." + k + ".package.same", () -> c.getPackage() == c.getPackage());
            rowx("class." + k + ".assertions", () -> c.desiredAssertionStatus());
            rowx("class." + k + ".resource.self", () -> {
                try (InputStream in = c.getResourceAsStream(c.getSimpleName() + ".class")) {
                    return in == null ? "null" : "stream";
                }
            });
            rowx("class." + k + ".resource.abs", () -> {
                try (InputStream in = c.getResourceAsStream("/java/lang/Object.class")) {
                    return in == null ? "null" : "stream";
                }
            });
            rowx("class." + k + ".resource.missing", () -> c.getResourceAsStream("no-such-resource"));
            rowx("class." + k + ".getResource.missing", () -> c.getResource("no-such-resource"));
            rowx("class." + k + ".getResource.self", () -> c.getResource(c.getSimpleName() + ".class") != null);
            rowx("class." + k + ".protectionDomain.cs", () -> {
                ProtectionDomain pd = c.getProtectionDomain();
                CodeSource cs2 = pd.getCodeSource();
                return cs2 == null ? "null" : String.valueOf(cs2.getLocation() != null);
            });
        }
        rowx("class.resource.null", () -> {
            try {
                return String.class.getResourceAsStream(null);
            } catch (NullPointerException e) {
                return "NPE";
            }
        });
        rowx("class.resource.app.data", () -> {
            try (InputStream in = SbrW6Module.class.getResourceAsStream("SbrW6Module$Fixture.class")) {
                byte[] b = in.readAllBytes();
                return (b[0] & 0xff) + " " + (b[1] & 0xff);
            }
        });
        rowx("class.assertions.nested", () -> Fixture.class.desiredAssertionStatus() + " " + SbrW6Module.class.desiredAssertionStatus());
        rowx("class.assertions.my.loader.class", () -> {
            Def d = new Def(sys, "SbrW6Module$Fixture3", bytesOf(Fixture3.class));
            d.setClassAssertionStatus("SbrW6Module$Fixture3", true);
            return d.loadClass("SbrW6Module$Fixture3").desiredAssertionStatus();
        });
        rowx("class.assertions.my.loader.pkg", () -> {
            Def d = new Def(sys, "SbrW6Module$Fixture3", bytesOf(Fixture3.class));
            d.setDefaultAssertionStatus(true);
            return d.loadClass("SbrW6Module$Fixture3").desiredAssertionStatus();
        });
        rowx("class.assertions.boot.default", () -> Object.class.desiredAssertionStatus() + " " + Thread.class.desiredAssertionStatus());
        rowx("class.getClassLoader.hidden", () -> {
            Runnable r = () -> {};
            return ldr(r.getClass().getClassLoader());
        });
        rowx("class.getClassLoader.proxy", () -> ldr(java.lang.reflect.Proxy.newProxyInstance(Runnable.class.getClassLoader(),
                new Class<?>[] {Runnable.class}, (p, m, a) -> null).getClass().getClassLoader()));

        // ---------------------------------------------------------------- CodeSource / ProtectionDomain
        rowx("cs.app.location", () -> {
            CodeSource c = SbrW6Module.class.getProtectionDomain().getCodeSource();
            return c == null ? "null" : mask(String.valueOf(c.getLocation())).replaceAll("PATH.*", "PATH");
        });
        rowx("cs.app.certs", () -> {
            CodeSource c = SbrW6Module.class.getProtectionDomain().getCodeSource();
            return c == null ? "null" : String.valueOf(c.getCertificates());
        });
        rowx("cs.app.signers", () -> {
            CodeSource c = SbrW6Module.class.getProtectionDomain().getCodeSource();
            return c == null ? "null" : String.valueOf(c.getCodeSigners());
        });
        rowx("cs.app.implies", () -> {
            CodeSource c = SbrW6Module.class.getProtectionDomain().getCodeSource();
            return c == null ? "null" : String.valueOf(c.implies(c));
        });
        rowx("cs.app.equals", () -> {
            CodeSource c = SbrW6Module.class.getProtectionDomain().getCodeSource();
            return c == null ? "null" : String.valueOf(c.equals(Fixture.class.getProtectionDomain().getCodeSource()));
        });
        rowx("cs.boot.pd", () -> {
            ProtectionDomain pd = String.class.getProtectionDomain();
            return pd == null ? "null" : (pd.getCodeSource() == null ? "nocs" : "cs") + " " + pd.getClassLoader() + " " + pd.getPrincipals().length;
        });
        rowx("cs.plat.pd", () -> {
            ProtectionDomain pd = java.sql.Driver.class.getProtectionDomain();
            return pd == null ? "null" : (pd.getCodeSource() == null ? "nocs" : "cs");
        });
        rowx("cs.new.location", () -> new CodeSource(new URL("file:/tmp/x.jar"), (java.security.cert.Certificate[]) null).getLocation());
        rowx("cs.new.null", () -> {
            CodeSource c = new CodeSource(null, (java.security.cert.Certificate[]) null);
            return c.getLocation() + " " + c.getCertificates() + " " + c.getCodeSigners();
        });
        rowx("cs.new.implies", () -> new CodeSource(new URL("file:/tmp/-"), (java.security.cert.Certificate[]) null)
                .implies(new CodeSource(new URL("file:/tmp/a/b.jar"), (java.security.cert.Certificate[]) null)));
        rowx("cs.new.implies.not", () -> new CodeSource(new URL("file:/tmp/a.jar"), (java.security.cert.Certificate[]) null)
                .implies(new CodeSource(new URL("file:/tmp/b.jar"), (java.security.cert.Certificate[]) null)));
        rowx("cs.new.toString", () -> new CodeSource(new URL("file:/tmp/x.jar"), (java.security.cert.Certificate[]) null));
        rowx("cs.pd.explicit", () -> {
            CodeSource c = new CodeSource(new URL("file:/tmp/x.jar"), (java.security.cert.Certificate[]) null);
            ProtectionDomain pd = new ProtectionDomain(c, null);
            return pd.getCodeSource().getLocation() + " " + (pd.getCodeSource() == c);
        });
        rowx("cs.pd.def.loader", () -> {
            Def d = new Def(sys, "SbrW6Module$Fixture3", bytesOf(Fixture3.class));
            Class<?> k = d.loadClass("SbrW6Module$Fixture3");
            return k.getProtectionDomain().getClassLoader() == d;
        });

        // ---------------------------------------------------------------- AccessController
        rowx("ac.doPrivileged.action", () -> AccessController.doPrivileged((PrivilegedAction<String>) () -> "ok"));
        rowx("ac.doPrivileged.exception.action", () -> AccessController.doPrivileged((PrivilegedExceptionAction<String>) () -> "ok2"));
        rowx("ac.doPrivileged.throws.unchecked", () -> AccessController.doPrivileged((PrivilegedAction<String>) () -> {
            throw new IllegalStateException("boom");
        }));
        rowx("ac.doPrivileged.throws.checked", () -> {
            try {
                return AccessController.doPrivileged((PrivilegedExceptionAction<String>) () -> {
                    throw new java.io.IOException("io");
                });
            } catch (java.security.PrivilegedActionException e) {
                return "PAE " + e.getException().getClass().getName() + " " + e.getCause().getMessage() + " " + e.getMessage();
            }
        });
        rowx("ac.doPrivileged.throws.runtime.viaException", () -> AccessController.doPrivileged((PrivilegedExceptionAction<String>) () -> {
            throw new IllegalArgumentException("iae");
        }));
        rowx("ac.doPrivileged.null", () -> AccessController.doPrivileged((PrivilegedAction<String>) null));
        rowx("ac.doPrivileged.null.exception", () -> AccessController.doPrivileged((PrivilegedExceptionAction<String>) null));
        rowx("ac.doPrivileged.null.result", () -> AccessController.doPrivileged((PrivilegedAction<String>) () -> null));
        rowx("ac.doPrivileged.ctx", () -> AccessController.doPrivileged((PrivilegedAction<String>) () -> "ctx", AccessController.getContext()));
        rowx("ac.doPrivileged.ctx.null", () -> AccessController.doPrivileged((PrivilegedAction<String>) () -> "nullctx", null));
        rowx("ac.doPrivileged.perms", () -> AccessController.doPrivileged((PrivilegedAction<String>) () -> "perms", null, new java.util.PropertyPermission("*", "read")));
        rowx("ac.doPrivilegedWithCombiner", () -> AccessController.doPrivilegedWithCombiner((PrivilegedAction<String>) () -> "comb"));
        rowx("ac.getContext.class", () -> nm(AccessController.getContext()));
        rowx("ac.checkPermission.null", () -> {
            AccessController.checkPermission(new java.util.PropertyPermission("user.dir", "read"));
            return "returned";
        });
        rowx("ac.context.ctor.null", () -> new AccessControlContext(null));
        rowx("ac.context.ctor.empty", () -> new AccessControlContext(new ProtectionDomain[0]).getClass().getName());
        rowx("ac.context.ctor.pd", () -> new AccessControlContext(new ProtectionDomain[] {SbrW6Module.class.getProtectionDomain()}).getClass().getName());
        rowx("ac.context.check", () -> {
            new AccessControlContext(new ProtectionDomain[] {SbrW6Module.class.getProtectionDomain()})
                    .checkPermission(new java.util.PropertyPermission("user.dir", "read"));
            return "ok";
        });
        rowx("ac.context.equals", () -> new AccessControlContext(new ProtectionDomain[0]).equals(new AccessControlContext(new ProtectionDomain[0])));
        rowx("ac.context.nested", () -> AccessController.doPrivileged((PrivilegedAction<String>) () -> AccessController.doPrivileged((PrivilegedAction<String>) () -> "in")));
        rowx("ac.context.thread", () -> {
            String[] out = new String[1];
            Thread t = new Thread(() -> out[0] = AccessController.doPrivileged((PrivilegedAction<String>) () -> "thr"));
            t.start();
            t.join();
            return out[0];
        });

        // ---------------------------------------------------------------- BootLoader (via public surface)
        rowx("boot.getSystemResource.jdk", () -> ClassLoader.getSystemResource("java/lang/Object.class") != null);
        rowx("boot.getSystemResource.jdk.scheme", () -> {
            URL u = ClassLoader.getSystemResource("java/lang/Object.class");
            return u == null ? "null" : u.getProtocol();
        });
        rowx("boot.Object.getResource", () -> {
            URL u = Object.class.getResource("Object.class");
            return u == null ? "null" : u.getProtocol();
        });
        rowx("boot.Object.getResourceAsStream", () -> {
            try (InputStream in = Object.class.getResourceAsStream("Object.class")) {
                return in == null ? "null" : String.valueOf(in.read());
            }
        });
        rowx("boot.findResource.missing", () -> ClassLoader.getSystemResource("java/lang/NoSuch.class"));
        rowx("boot.getPlatformResource", () -> plat.getResource("java/sql/Driver.class") != null);
        rowx("boot.plat.getResourceAsStream.jdk", () -> {
            try (InputStream in = plat.getResourceAsStream("java/lang/Object.class")) {
                return in == null ? "null" : "stream";
            }
        });
        rowx("boot.System.loadLibrary.missing", () -> {
            System.loadLibrary("no_such_lib_xyz");
            return "loaded";
        });
        rowx("boot.System.loadLibrary.null", () -> {
            System.loadLibrary(null);
            return "loaded";
        });
        rowx("boot.System.loadLibrary.java", () -> {
            System.loadLibrary("net");
            return "loaded";
        });

        // ---------------------------------------------------------------- Builder (descriptors)
        rowx("desc.build.simple", () -> {
            ModuleDescriptor d = ModuleDescriptor.newModule("m.a").requires("java.base").exports("p.a").exports("p.b", Set.of("m.b"))
                    .opens("p.c").provides("s.S", List.of("p.a.Impl")).uses("s.U").packages(Set.of("p.a", "p.b", "p.c")).build();
            return d.name() + " " + d.requires().size() + " " + d.exports().size() + " " + d.opens().size() + " " + d.provides().size()
                    + " " + d.uses() + " " + d.packages().size();
        });
        rowx("desc.build.toString", () -> ModuleDescriptor.newModule("m.a").requires("java.base").exports("p.a").build().toString());
        rowx("desc.build.exports.qualified", () -> {
            ModuleDescriptor d = ModuleDescriptor.newModule("m.a").exports("p.b", Set.of("m.b", "m.c")).build();
            ModuleDescriptor.Exports e = d.exports().iterator().next();
            return e.source() + " " + e.isQualified() + " " + new TreeSet<>(e.targets()) + " " + e.modifiers();
        });
        rowx("desc.build.requires.version", () -> {
            ModuleDescriptor d = ModuleDescriptor.newModule("m.a").requires(Set.of(ModuleDescriptor.Requires.Modifier.TRANSITIVE), "m.b",
                    ModuleDescriptor.Version.parse("1.2.3")).build();
            for (ModuleDescriptor.Requires r : d.requires()) if (r.name().equals("m.b")) return r + " " + r.compiledVersion() + " " + r.modifiers();
            return "none";
        });
        rowx("desc.build.version", () -> ModuleDescriptor.newModule("m.a").version("2.0-ea").build().version().map(Object::toString));
        rowx("desc.version.parse", () -> ModuleDescriptor.Version.parse("1.2.3-ea+b7").toString());
        rowx("desc.version.compare", () -> ModuleDescriptor.Version.parse("1.2.3").compareTo(ModuleDescriptor.Version.parse("1.2.10")));
        rowx("desc.version.equals", () -> ModuleDescriptor.Version.parse("1.2.3").equals(ModuleDescriptor.Version.parse("1.2.3")));
        rowx("desc.version.bad", () -> ModuleDescriptor.Version.parse("!x"));
        rowx("desc.exports.equals", () -> {
            ModuleDescriptor a = ModuleDescriptor.newModule("m.a").exports("p.a").build();
            ModuleDescriptor b = ModuleDescriptor.newModule("m.a").exports("p.a").build();
            return a.exports().equals(b.exports()) + " " + a.equals(b) + " " + (a.hashCode() == b.hashCode()) + " " + a.compareTo(b);
        });
        rowx("desc.provides.equals", () -> {
            ModuleDescriptor a = ModuleDescriptor.newModule("m.a").provides("s.S", List.of("p.I")).build();
            ModuleDescriptor b = ModuleDescriptor.newModule("m.a").provides("s.S", List.of("p.I")).build();
            return a.provides().equals(b.provides());
        });
        rowx("desc.exports.sorted", () -> {
            ModuleDescriptor d = ModuleDescriptor.newModule("m.a").exports("p.z").exports("p.a").exports("p.m").build();
            List<String> l = new ArrayList<>();
            d.exports().stream().sorted().forEach(e -> l.add(e.source()));
            return l;
        });
        rowx("desc.base.exports.contains", () -> {
            ModuleDescriptor d = base.getDescriptor();
            return d == null ? "null" : d.exports().stream().filter(e -> e.source().equals("java.util")).findFirst().map(Object::toString).orElse("none");
        });
        rowx("desc.base.requires", () -> base.getDescriptor() == null ? "null" : base.getDescriptor().requires().toString());
        rowx("desc.base.provides.count", () -> base.getDescriptor() == null ? "null" : String.valueOf(base.getDescriptor().provides().size() > 0));
        rowx("desc.base.uses.count", () -> base.getDescriptor() == null ? "null" : String.valueOf(base.getDescriptor().uses().size() > 3));
        rowx("desc.base.mainClass", () -> base.getDescriptor() == null ? "null" : String.valueOf(base.getDescriptor().mainClass()));
        rowx("desc.base.version", () -> base.getDescriptor() == null ? "null" : base.getDescriptor().version().isPresent());
        rowx("desc.base.packages", () -> base.getDescriptor() == null ? "null" : base.getDescriptor().packages().contains("java.lang"));
        rowx("desc.base.modifiers", () -> base.getDescriptor() == null ? "null" : base.getDescriptor().modifiers());
        rowx("desc.base.toString", () -> base.getDescriptor() == null ? "null" : base.getDescriptor().toString().startsWith("module { name: java.base"));
        rowx("desc.logging.requires", () -> logging.getDescriptor().requires().toString());
        rowx("desc.logging.exports", () -> logging.getDescriptor().exports().toString());
        rowx("desc.logging.provides", () -> logging.getDescriptor().provides().toString());

        String[] first = new String[ROWS.size()];
        for (int i = 0; i < ROWS.size(); i++) {
            first[i] = mask(safe(ROWS.get(i)));
            System.out.println("row " + LABELS.get(i) + " = " + units(first[i]));
        }
        int drift = 0;
        int iters = args.length > 0 ? Integer.parseInt(args[0]) : 300;
        for (int k = 0; k < iters; k++) {
            for (int i = 0; i < ROWS.size(); i++) {
                String v = mask(safe(ROWS.get(i)));
                if (!v.equals(first[i])) {
                    if (drift < 10) {
                        System.out.println("DRIFT at iter " + k + " " + LABELS.get(i) + " = " + units(v));
                    }
                    drift++;
                }
            }
        }
        System.out.println("rows " + ROWS.size() + " drift " + drift);
    }

    public static class Boom {
        static {
            if (true) throw new IllegalStateException("clinit");
        }
    }

    public static class Boom2 {
        static {
            if (true) throw new IllegalStateException("clinit2");
        }
    }
}
