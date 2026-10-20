// Interpreter round i1, wave 43, lane L5 -- classes of a named module in a
// user `ModuleLayer` (`docs/internal/fixed-bugs/
// interpreter-L5-a-class-of-a-named-module-in-a-user-layer-is-put-in-its-loaders-unnamed-module-FIXED-20261010.md`)
// and a review of the `ModuleLayer` / `Module` / `Class.forName(Module,
// String)` surface around them. Every module is in memory: a
// `ModuleDescriptor` built with `ModuleDescriptor.newModule`, a
// `ModuleReference` / `ModuleReader` / `ModuleFinder` written in Java, and
// class bytes taken from this file's own nested classes, renamed into the
// module's packages. Each row builds its own layer over the boot layer with
// its own module name, so no row sees another's module.
//
// Rows:
//   one-loader       defineModulesWithOneLoader: the class loads, runs, is a
//                    member of the module (name, named, layer, identity with
//                    layer.findModule), Class.forName(Module, String) finds it
//                    and answers null for an absent class
//   unexported       a second, unexported package: isExported, and
//                    Class.forName(Module, String) defining its class
//   arrays           Class.getModule of an array of a layer class, int[] and
//                    String[][]
//   many-loaders     defineModulesWithManyLoaders: the loader's name and the
//                    class's module
//   function-loader  defineModules with a function naming the probe's own
//                    ClassLoader subclass: the class it defines is a member
//   controller       ModuleLayer.Controller addReads / addOpens / addExports
//                    to this probe's unnamed module, read back
//   forname-base     Class.forName(java.base, ...) for a java.base class and
//                    for a layer class
//   forname-noinit   Class.forName(Module, String) of this probe's own module
//                    loads a class without initializing it
//   hidden           a hidden class of this probe's package is in its
//                    lookup class's module
//   cf-modules       Configuration.modules(): one unmodifiable set
//   layer-modules    ModuleLayer.modules(): one unmodifiable set holding the
//                    layer's module
//   service          ServiceLoader.load(layer, Runnable.class) finds the
//                    layer module's provider
//
// Run (no setup):
//   javac -d out L5W43UserLayerModule.java
//   cratonvm --java-home <jdk25> [--nojit] -cp out L5W43UserLayerModule
//
// Expected HotSpot 25.0.3 output (`java` and `-Xint`, identical; compare
// verbatim):
//   one-loader=m43a true hi true true pkg=p43a named=true layer=true same=true find=true absent=null
//   unexported=false false q43b.Other true
//   arrays=true true true
//   many-loaders=Loader-m43d true m43d true
//   function-loader=m43e true true true
//   controller=false true false true false true false
//   forname-base=true null
//   forname-noinit=true initialized=false
//   hidden=true
//   cf-modules=true 1 java.lang.UnsupportedOperationException
//   layer-modules=true 1 true java.lang.UnsupportedOperationException
//   service=[p43i.Hello]
//
// On the base `134a197a3` (from the code, wave 42's host run of the same
// shape): the rows that load a class through the layer's loader (`one-loader`,
// `arrays`, `many-loaders`, `forname-base`, `service`) fail with
// `java.lang.ClassNotFoundException: p43?.Hello` -- `ClassLoader.loadClass`'s
// native treated `jdk.internal.loader.Loader` as a built-in loader (the
// `jdk/internal/loader/` prefix of `classloader::is_builtin_loader_class`) and
// never ran its `loadClass(String, boolean)`. `function-loader` loads (its
// loader is the probe's own) but its class is in the loader's unnamed module
// (`null false false ...`). `controller` starts `true` (the registry reads an
// unknown module as reading everything). `cf-modules` / `layer-modules`
// answer a fresh mutable `HashSet` per call (`false 1 added`).
// `forname-noinit` prints `initialized=true` (the native ran `<clinit>`).
//
// Positive control (`--jdk-only`): `CRATONVM_DBG=access` prints
// `[LAYER-MODULE] defineModule0 m43a loader_ns=<n> packages=["p43a"]` for
// each layer module and `[LAYER-MODULE] getModule p43a/Hello loader_ns=<n> ->
// layer module` when `Class.getModule()` answers from the record; neither
// line appears on the base.
//
// `--compatible` keeps the base's answers by design (the layer loader stays a
// built-in loader there, and nothing is recorded).

import java.io.*;
import java.lang.invoke.MethodHandles;
import java.lang.module.*;
import java.lang.reflect.Array;
import java.net.URI;
import java.nio.ByteBuffer;
import java.util.*;

public class L5W43UserLayerModule {
    static final String P = "L5W43UserLayerModule$";

    static byte[] rename(byte[] bytes, Map<String, String> renames) throws IOException {
        DataInputStream in = new DataInputStream(new ByteArrayInputStream(bytes));
        ByteArrayOutputStream buf = new ByteArrayOutputStream();
        DataOutputStream out = new DataOutputStream(buf);
        out.writeInt(in.readInt());
        out.writeShort(in.readUnsignedShort());
        out.writeShort(in.readUnsignedShort());
        int count = in.readUnsignedShort();
        out.writeShort(count);
        for (int i = 1; i < count; i++) {
            int tag = in.readUnsignedByte();
            out.writeByte(tag);
            switch (tag) {
                case 1 -> {
                    String s = in.readUTF();
                    for (Map.Entry<String, String> e : renames.entrySet()) s = s.replace(e.getKey(), e.getValue());
                    out.writeUTF(s);
                }
                case 3, 4 -> out.writeInt(in.readInt());
                case 5, 6 -> { out.writeLong(in.readLong()); i++; }
                case 7, 8, 16, 19, 20 -> out.writeShort(in.readUnsignedShort());
                case 9, 10, 11, 12, 17, 18 -> out.writeInt(in.readInt());
                case 15 -> { out.writeByte(in.readUnsignedByte()); out.writeShort(in.readUnsignedShort()); }
                default -> throw new IOException("tag " + tag);
            }
        }
        in.transferTo(out);
        out.flush();
        return buf.toByteArray();
    }

    static byte[] own(String n) throws IOException {
        try (InputStream in = ClassLoader.getSystemResourceAsStream(n + ".class")) { return in.readAllBytes(); }
    }

    /** One in-memory module: its name, its two packages and their class bytes. */
    static final class Mod {
        final String name, pkg, hidden;
        final Map<String, byte[]> files = new HashMap<>();
        final ModuleDescriptor descriptor;

        Mod(String tag, boolean provides) throws IOException {
            name = "m43" + tag;
            pkg = "p43" + tag;
            hidden = "q43" + tag;
            files.put(pkg + "/Hello.class", rename(own(P + "Hello"), Map.of(P + "Hello", pkg + "/Hello")));
            files.put(hidden + "/Other.class", rename(own(P + "Other"), Map.of(P + "Other", hidden + "/Other")));
            ModuleDescriptor.Builder b = ModuleDescriptor.newModule(name).packages(Set.of(pkg, hidden)).exports(pkg);
            if (provides) b.provides("java.lang.Runnable", List.of(pkg + ".Hello"));
            descriptor = b.build();
        }

        byte[] bytes(String className) {
            return files.get(className.replace('.', '/') + ".class");
        }

        ModuleFinder finder() {
            ModuleReference ref = new ModuleReference(descriptor, null) {
                public ModuleReader open() {
                    return new ModuleReader() {
                        public Optional<URI> find(String n) { return Optional.empty(); }
                        public Optional<ByteBuffer> read(String n) {
                            byte[] b = files.get(n);
                            return b == null ? Optional.empty() : Optional.of(ByteBuffer.wrap(b));
                        }
                        public Optional<InputStream> open(String n) {
                            byte[] b = files.get(n);
                            return b == null ? Optional.empty() : Optional.of(new ByteArrayInputStream(b));
                        }
                        public java.util.stream.Stream<String> list() { return files.keySet().stream(); }
                        public void close() {}
                    };
                }
            };
            return new ModuleFinder() {
                public Optional<ModuleReference> find(String n) { return n.equals(name) ? Optional.of(ref) : Optional.empty(); }
                public Set<ModuleReference> findAll() { return Set.of(ref); }
            };
        }

        Configuration configuration() {
            return ModuleLayer.boot().configuration().resolve(finder(), ModuleFinder.of(), Set.of(name));
        }
    }

    /** A loader that defines a module's classes itself (the `defineModules` function row). */
    static final class ModLoader extends ClassLoader {
        final Mod mod;
        ModLoader(Mod mod) { super("mod-loader", L5W43UserLayerModule.class.getClassLoader()); this.mod = mod; }
        @Override protected Class<?> findClass(String n) throws ClassNotFoundException {
            byte[] b = mod.bytes(n);
            if (b == null) throw new ClassNotFoundException(n);
            return defineClass(n, b, 0, b.length);
        }
    }

    interface Row { Object run() throws Throwable; }

    static void row(String label, Row r) {
        try {
            System.out.println(label + "=" + r.run());
        } catch (Throwable t) {
            String m = t.getMessage();
            System.out.println(label + "=" + t.getClass().getName() + ": " + (m == null ? "null" : m.replaceAll("@[0-9a-f]+", "@H")));
        }
    }

    static final ClassLoader APP = L5W43UserLayerModule.class.getClassLoader();

    public static void main(String[] args) throws Exception {
        ModuleLayer boot = ModuleLayer.boot();
        row("one-loader", () -> {
            Mod mod = new Mod("a", false);
            ModuleLayer layer = boot.defineModulesWithOneLoader(mod.configuration(), APP);
            ClassLoader l = layer.findLoader(mod.name);
            Class<?> c = l.loadClass(mod.pkg + ".Hello");
            Object r = c.getMethod("hi").invoke(null);
            Module m = c.getModule();
            return m.getName() + " " + (c.getClassLoader() == l) + " " + r + " " + m.isExported(mod.pkg)
                    + " " + (l.getParent() == APP) + " pkg=" + c.getPackage().getName()
                    + " named=" + m.isNamed() + " layer=" + (m.getLayer() == layer)
                    + " same=" + (layer.findModule(mod.name).get() == m)
                    + " find=" + (Class.forName(m, mod.pkg + ".Hello") == c)
                    + " absent=" + Class.forName(m, mod.pkg + ".Nope");
        });
        row("unexported", () -> {
            Mod mod = new Mod("b", false);
            ModuleLayer layer = boot.defineModulesWithOneLoader(mod.configuration(), APP);
            Module m = layer.findModule(mod.name).get();
            Class<?> other = Class.forName(m, mod.hidden + ".Other");
            return m.isExported(mod.hidden) + " " + m.isExported(mod.hidden, L5W43UserLayerModule.class.getModule())
                    + " " + (other == null ? "null" : other.getName()) + " " + (other != null && other.getModule() == m);
        });
        row("arrays", () -> {
            Mod mod = new Mod("c", false);
            ModuleLayer layer = boot.defineModulesWithOneLoader(mod.configuration(), APP);
            Class<?> c = layer.findLoader(mod.name).loadClass(mod.pkg + ".Hello");
            Class<?> arr = Array.newInstance(c, 0).getClass();
            return (arr.getModule() == c.getModule()) + " " + (int[].class.getModule() == Object.class.getModule())
                    + " " + (String[][].class.getModule() == Object.class.getModule());
        });
        row("many-loaders", () -> {
            Mod mod = new Mod("d", false);
            ModuleLayer layer = boot.defineModulesWithManyLoaders(mod.configuration(), APP);
            ClassLoader l = layer.findLoader(mod.name);
            Class<?> c = l.loadClass(mod.pkg + ".Hello");
            return l.getName() + " " + (c.getClassLoader() == l) + " " + c.getModule().getName()
                    + " " + (c.getModule() == layer.findModule(mod.name).get());
        });
        row("function-loader", () -> {
            Mod mod = new Mod("e", false);
            ModLoader loader = new ModLoader(mod);
            ModuleLayer layer = ModuleLayer.defineModules(mod.configuration(), List.of(boot), mn -> loader).layer();
            Class<?> c = loader.loadClass(mod.pkg + ".Hello");
            Module m = c.getModule();
            return m.getName() + " " + m.isNamed() + " " + (m == layer.findModule(mod.name).get())
                    + " " + (layer.findLoader(mod.name) == loader);
        });
        row("controller", () -> {
            Mod mod = new Mod("f", false);
            ModuleLayer.Controller ctl = ModuleLayer.defineModulesWithOneLoader(mod.configuration(), List.of(boot), APP);
            Module m = ctl.layer().findModule(mod.name).get();
            Module me = L5W43UserLayerModule.class.getModule();
            StringBuilder sb = new StringBuilder();
            sb.append(m.canRead(me));
            ctl.addReads(m, me);
            sb.append(' ').append(m.canRead(me));
            sb.append(' ').append(m.isOpen(mod.pkg, me));
            ctl.addOpens(m, mod.pkg, me);
            sb.append(' ').append(m.isOpen(mod.pkg, me));
            sb.append(' ').append(m.isExported(mod.hidden, me));
            ctl.addExports(m, mod.hidden, me);
            sb.append(' ').append(m.isExported(mod.hidden, me));
            sb.append(' ').append(m.isExported(mod.hidden));
            return sb;
        });
        row("forname-base", () -> {
            Mod mod = new Mod("g", false);
            ModuleLayer layer = boot.defineModulesWithOneLoader(mod.configuration(), APP);
            layer.findLoader(mod.name).loadClass(mod.pkg + ".Hello");
            Module base = Object.class.getModule();
            return (Class.forName(base, "java.lang.String") == String.class) + " " + Class.forName(base, mod.pkg + ".Hello");
        });
        row("forname-noinit", () -> {
            Class<?> c = Class.forName(L5W43UserLayerModule.class.getModule(), P + "Init");
            return (c != null) + " initialized=" + Flag.initialized;
        });
        row("hidden", () -> {
            byte[] b = own(P + "Hid");
            Class<?> h = MethodHandles.lookup().defineHiddenClass(b, false).lookupClass();
            return h.getModule() == L5W43UserLayerModule.class.getModule();
        });
        row("cf-modules", () -> {
            Configuration cf = new Mod("j", false).configuration();
            Set<ResolvedModule> s = cf.modules();
            String add;
            try {
                s.add(s.iterator().next());
                add = "added";
            } catch (UnsupportedOperationException e) {
                add = e.getClass().getName();
            }
            return (s == cf.modules()) + " " + s.size() + " " + add;
        });
        row("layer-modules", () -> {
            Mod mod = new Mod("k", false);
            ModuleLayer layer = boot.defineModulesWithOneLoader(mod.configuration(), APP);
            Set<Module> s = layer.modules();
            String add;
            try {
                s.add(Object.class.getModule());
                add = "added";
            } catch (UnsupportedOperationException e) {
                add = e.getClass().getName();
            }
            return (s == layer.modules()) + " " + s.size() + " " + s.contains(layer.findModule(mod.name).get()) + " " + add;
        });
        row("service", () -> {
            Mod mod = new Mod("i", true);
            ModuleLayer layer = boot.defineModulesWithOneLoader(mod.configuration(), APP);
            List<String> names = new ArrayList<>();
            for (Runnable r : ServiceLoader.load(layer, Runnable.class)) names.add(r.getClass().getName());
            return names;
        });
    }

    public static class Hello implements Runnable {
        public Hello() {}
        public static String hi() { return "hi"; }
        public void run() {}
    }

    public static class Other {
        public static String hi() { return "other"; }
    }

    static class Hid {
    }

    /** Set by its own initializer; read without initializing (a static field of another class). */
    static final class Init {
        static { Flag.initialized = true; }
    }

    static final class Flag {
        static boolean initialized;
    }
}
