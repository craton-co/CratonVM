// Interpreter round i1, wave 44, lane L5 -- JVMS §5.4.4's export clause for
// the classes of a named module in a user `ModuleLayer`, judged by the
// module's IDENTITY
// (`docs/known-issues/interpreter/i43-L5-proposal-a-layer-module-is-a-member-at-define-and-the-registry-knows-it-by-identity-20261007.md`,
// the remainder of
// `i42-L5-a-class-of-a-named-module-in-a-user-layer-is-put-in-its-loaders-unnamed-module`).
//
// Every module is in memory (a `ModuleDescriptor` built in Java, a
// `ModuleFinder` / `ModuleReader` written here), its classes taken from this
// file's nested classes and renamed into the module's packages. An ACCESSOR
// class (`Acc`, renamed `acc44.Acc`) is defined by a fresh child loader of the
// layer's loader, so it is in that child's unnamed module and names the layer
// module's classes through `CONSTANT_Class` entries (`ldc`, `new`); each
// attempt uses a new child loader, since a failed resolution is recorded
// against the constant (JVMS §5.4.3).
//
// Rows:
//   exported        ldc of a class of an exported package: resolves
//   unexported      ldc of a class of a package the module does not export:
//                   IllegalAccessError (HotSpot's message, identity hashes
//                   removed)
//   unexported-new  `new` of that class: IllegalAccessError
//   reflect         Method.invoke of a public static method of the
//                   unexported class, from this (unnamed-module) class:
//                   IllegalAccessException; of the exported class: runs
//   cast            a failed cast of a layer class names its module
//                   (HotSpot's `class_in_module_of_loader`)
//   set-accessible  setAccessible(true) on that method:
//                   InaccessibleObjectException
//   two-layers      two layers, each with a module `m44c`; the first exports
//                   q44c, the second does not: the first's accessor
//                   resolves, the second's is refused (a name-keyed record
//                   would let the first's export answer for the second)
//   qualified       `exports q44d to m44e`: a class of m44e (same layer)
//                   resolves q44d.Other; an unnamed accessor is refused
//   controller      ModuleLayer.Controller.addExports of q44f to the
//                   application loader's unnamed module leaves a child
//                   loader's accessor refused; to that child's unnamed
//                   module, admitted
//   open            an open module: an accessor resolves a package it does
//                   not export, and setAccessible(true) succeeds
//
// Run (no setup):
//   javac -d out L5W44LayerModuleAccess.java
//   cratonvm --java-home <jdk25> [--nojit] -cp out L5W44LayerModuleAccess
//
// Expected HotSpot 25.0.3 output (`java` and `-Xint`, identical; compare
// verbatim):
//   exported=ok p44a.Hello
//   unexported=java.lang.IllegalAccessError: class acc44.Acc (in unnamed module) cannot access class q44a.Other (in module m44a) because module m44a does not export q44a to unnamed module
//   unexported-new=java.lang.IllegalAccessError: class acc44.Acc (in unnamed module) cannot access class q44a.Other (in module m44a) because module m44a does not export q44a to unnamed module
//   reflect=java.lang.IllegalAccessException hi
//   cast=class p44a.Hello cannot be cast to class java.lang.String (p44a.Hello is in module m44a of loader jdk.internal.loader.Loader; java.lang.String is in module java.base of loader 'bootstrap')
//   set-accessible=java.lang.reflect.InaccessibleObjectException
//   two-layers=ok q44c.Other java.lang.IllegalAccessError
//   qualified=ok q44d.Other java.lang.IllegalAccessError
//   controller=java.lang.IllegalAccessError ok q44f.Other
//   open=ok q44g.Other true
//
// On the base `5248262b7` (from the code): the layer classes load and
// `Class.getModule()` answers the layer module (wave 43), but the VM's own
// access control reads the class's `module_name`, which the define path took
// from the name registry -- `None`, the unnamed module, which exports
// everything. So every refusal above is admitted: `unexported=ok q44a.Other`,
// `unexported-new=ok q44a.Other`, `reflect=other hi` (the reflective gates,
// `reflective_export_to_accessor` / `check_deep_reflection_access`, read the
// same `None`), `cast=... (p44a.Hello is in unnamed module of loader
// jdk.internal.loader.Loader; ...)`, `set-accessible=ok`, `two-layers=ok q44c.Other ok q44c.Other`,
// `qualified=ok q44d.Other ok q44d.Other`, `controller=ok q44f.Other ok
// q44f.Other`. `exported` and `open` match on the base too.
//
// Positive control (`--jdk-only`): `CRATONVM_DBG=access` prints
// `[LAYER-MODULE] defineModule0 m44a loader_ns=<n> packages=[...] open=false`
// and `[LAYER-MODULE] addExports m44a loader_ns=<n> p44a -> Everyone` (the
// identity-keyed export record; for `qualified`, `q44d -> Layer { loader_ns:
// <n>, name: "m44e" }`; for `controller`, `q44f -> Unnamed { loader_ns: <m> }`).
// Neither the `open=` field nor any `addExports` line exists on the base.
//
// `--compatible` keeps the base's answers by design (the layer's loader is a
// built-in loader there and no layer module is recorded; its classes do not
// load, as in `L5W43UserLayerModule`).

import java.io.*;
import java.lang.module.*;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.net.URI;
import java.nio.ByteBuffer;
import java.util.*;

public class L5W44LayerModuleAccess {
    static final String P = "L5W44LayerModuleAccess$";
    static final ClassLoader APP = L5W44LayerModuleAccess.class.getClassLoader();

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

    /** The bytes of `Hello` / `Other` renamed to `pkg/Hello` / `pkg/Other`. */
    static byte[] member(String simple, String pkg) throws IOException {
        return rename(own(P + simple), Map.of(P + simple, pkg + "/" + simple));
    }

    /** The accessor, renamed to `name`, naming `hello` for Hello and `other` for Other. */
    static byte[] accessor(String name, String hello, String other) throws IOException {
        return rename(own(P + "Acc"), Map.of(P + "Acc", name, P + "Hello", hello, P + "Other", other));
    }

    /** In-memory modules: class files by resource name, one finder for all. */
    static final class Mods {
        final Map<String, ModuleDescriptor> descriptors = new HashMap<>();
        final Map<String, Map<String, byte[]>> files = new HashMap<>();

        Mods add(ModuleDescriptor d, Map<String, byte[]> classFiles) {
            descriptors.put(d.name(), d);
            files.put(d.name(), classFiles);
            return this;
        }

        ModuleFinder finder() {
            Map<String, ModuleReference> refs = new HashMap<>();
            for (ModuleDescriptor d : descriptors.values()) {
                Map<String, byte[]> fs = files.get(d.name());
                refs.put(d.name(), new ModuleReference(d, null) {
                    public ModuleReader open() {
                        return new ModuleReader() {
                            public Optional<URI> find(String n) { return Optional.empty(); }
                            public Optional<ByteBuffer> read(String n) {
                                byte[] b = fs.get(n);
                                return b == null ? Optional.empty() : Optional.of(ByteBuffer.wrap(b));
                            }
                            public Optional<InputStream> open(String n) {
                                byte[] b = fs.get(n);
                                return b == null ? Optional.empty() : Optional.of(new ByteArrayInputStream(b));
                            }
                            public java.util.stream.Stream<String> list() { return fs.keySet().stream(); }
                            public void close() {}
                        };
                    }
                });
            }
            return new ModuleFinder() {
                public Optional<ModuleReference> find(String n) { return Optional.ofNullable(refs.get(n)); }
                public Set<ModuleReference> findAll() { return new HashSet<>(refs.values()); }
            };
        }

        Configuration configuration(String root) {
            return ModuleLayer.boot().configuration().resolve(finder(), ModuleFinder.of(), Set.of(root));
        }
    }

    /** A module `name` with packages `pkg` (Hello) and `hidden` (Other), exporting `pkg`. */
    static Mods plain(String name, String pkg, String hidden, boolean exportHidden) throws IOException {
        ModuleDescriptor.Builder b = ModuleDescriptor.newModule(name).packages(Set.of(pkg, hidden)).exports(pkg);
        if (exportHidden) b.exports(hidden);
        return new Mods().add(b.build(), Map.of(
                pkg + "/Hello.class", member("Hello", pkg),
                hidden + "/Other.class", member("Other", hidden)));
    }

    /** A child loader of `parent` that defines `acc44.Acc` from `bytes`. */
    static final class Child extends ClassLoader {
        final byte[] bytes;
        Child(ClassLoader parent, byte[] bytes) { super("child", parent); this.bytes = bytes; }
        @Override protected Class<?> findClass(String n) throws ClassNotFoundException {
            if (!n.equals("acc44.Acc")) throw new ClassNotFoundException(n);
            return defineClass(n, bytes, 0, bytes.length);
        }
    }

    /** Run `method` of `acc44.Acc` defined by `loader`: the class it names, or the error. */
    static String attempt(ClassLoader loader, String method) throws Throwable {
        Class<?> acc = loader.loadClass("acc44.Acc");
        try {
            Object r = acc.getMethod(method).invoke(null);
            return "ok " + (r instanceof Class<?> c ? c.getName() : r.getClass().getName());
        } catch (InvocationTargetException e) {
            Throwable t = e.getCause();
            if (t instanceof IllegalAccessError) return t.getClass().getName();
            throw t;
        }
    }

    static String attemptVerbose(ClassLoader loader, String method) throws Throwable {
        Class<?> acc = loader.loadClass("acc44.Acc");
        try {
            Object r = acc.getMethod(method).invoke(null);
            return "ok " + (r instanceof Class<?> c ? c.getName() : r.getClass().getName());
        } catch (InvocationTargetException e) {
            Throwable t = e.getCause();
            String m = t.getMessage();
            return t.getClass().getName() + ": " + (m == null ? "null" : m.replaceAll(" @(0x)?[0-9a-f]+", ""));
        }
    }

    interface Row { Object run() throws Throwable; }

    static void row(String label, Row r) {
        try {
            System.out.println(label + "=" + r.run());
        } catch (Throwable t) {
            String m = t.getMessage();
            System.out.println(label + "=" + t.getClass().getName() + ": " + (m == null ? "null" : m.replaceAll(" @(0x)?[0-9a-f]+", "")));
        }
    }

    public static void main(String[] args) throws Exception {
        ModuleLayer boot = ModuleLayer.boot();
        Mods a = plain("m44a", "p44a", "q44a", false);
        ModuleLayer layerA = boot.defineModulesWithOneLoader(a.configuration("m44a"), APP);
        ClassLoader la = layerA.findLoader("m44a");
        row("exported", () -> attemptVerbose(new Child(la, accessor("acc44/Acc", "p44a/Hello", "q44a/Other")), "hello"));
        row("unexported", () -> attemptVerbose(new Child(la, accessor("acc44/Acc", "p44a/Hello", "q44a/Other")), "other"));
        row("unexported-new", () -> attemptVerbose(new Child(la, accessor("acc44/Acc", "p44a/Hello", "q44a/Other")), "make"));
        row("reflect", () -> {
            String hidden;
            try {
                hidden = String.valueOf(Class.forName("q44a.Other", false, la).getMethod("hi").invoke(null));
            } catch (IllegalAccessException e) {
                hidden = e.getClass().getName();
            }
            return hidden + " " + Class.forName("p44a.Hello", false, la).getMethod("hi").invoke(null);
        });
        row("cast", () -> {
            Object o = Class.forName("p44a.Hello", true, la).getConstructor().newInstance();
            try {
                return (String) o;
            } catch (ClassCastException e) {
                return e.getMessage().replaceAll(" @(0x)?[0-9a-f]+", "");
            }
        });
        row("set-accessible", () -> {
            Method m = Class.forName("q44a.Other", false, la).getMethod("hi");
            try {
                m.setAccessible(true);
                return "ok";
            } catch (RuntimeException e) {
                return e.getClass().getName();
            }
        });
        row("two-layers", () -> {
            ModuleLayer x = boot.defineModulesWithOneLoader(plain("m44c", "p44c", "q44c", true).configuration("m44c"), APP);
            ModuleLayer y = boot.defineModulesWithOneLoader(plain("m44c", "p44c", "q44c", false).configuration("m44c"), APP);
            byte[] acc = accessor("acc44/Acc", "p44c/Hello", "q44c/Other");
            return attempt(new Child(x.findLoader("m44c"), acc), "other") + " "
                    + attempt(new Child(y.findLoader("m44c"), acc), "other");
        });
        row("qualified", () -> {
            Mods mods = new Mods()
                    .add(ModuleDescriptor.newModule("m44d").packages(Set.of("q44d"))
                                    .exports(Set.of(), "q44d", Set.of("m44e")).build(),
                            Map.of("q44d/Other.class", member("Other", "q44d")))
                    .add(ModuleDescriptor.newModule("m44e").packages(Set.of("p44e")).requires("m44d")
                                    .exports("p44e").build(),
                            Map.of("p44e/Acc.class", accessor("p44e/Acc", "p44e/Hello", "q44d/Other")));
            ModuleLayer layer = boot.defineModulesWithOneLoader(mods.configuration("m44e"), APP);
            ClassLoader l = layer.findLoader("m44e");
            Class<?> inModule = l.loadClass("p44e.Acc");
            String named;
            try {
                named = "ok " + ((Class<?>) inModule.getMethod("other").invoke(null)).getName();
            } catch (InvocationTargetException e) {
                named = e.getCause().getClass().getName();
            }
            return named + " " + attempt(new Child(l, accessor("acc44/Acc", "p44e/Hello", "q44d/Other")), "other");
        });
        row("controller", () -> {
            Mods mods = plain("m44f", "p44f", "q44f", false);
            ModuleLayer.Controller ctl = ModuleLayer.defineModulesWithOneLoader(mods.configuration("m44f"), List.of(boot), APP);
            ClassLoader l = ctl.layer().findLoader("m44f");
            Module m = ctl.layer().findModule("m44f").get();
            byte[] acc = accessor("acc44/Acc", "p44f/Hello", "q44f/Other");
            Child first = new Child(l, acc);
            ctl.addExports(m, "q44f", APP.getUnnamedModule());
            String toApp = attempt(first, "other");
            Child second = new Child(l, acc);
            ctl.addExports(m, "q44f", second.getUnnamedModule());
            return toApp + " " + attempt(second, "other");
        });
        row("open", () -> {
            Mods mods = new Mods().add(ModuleDescriptor.newOpenModule("m44g").packages(Set.of("p44g", "q44g")).exports("p44g").build(),
                    Map.of("p44g/Hello.class", member("Hello", "p44g"), "q44g/Other.class", member("Other", "q44g")));
            ModuleLayer layer = boot.defineModulesWithOneLoader(mods.configuration("m44g"), APP);
            ClassLoader l = layer.findLoader("m44g");
            Method m = Class.forName("q44g.Other", false, l).getMethod("hi");
            boolean accessible;
            try {
                m.setAccessible(true);
                accessible = true;
            } catch (RuntimeException e) {
                accessible = false;
            }
            return attempt(new Child(l, accessor("acc44/Acc", "p44g/Hello", "q44g/Other")), "other") + " " + accessible;
        });
    }

    public static class Hello {
        public static String hi() { return "hi"; }
    }

    public static class Other {
        public Other() {}
        public static String hi() { return "other"; }
    }

    /** Renamed into an accessor; each method names one class through its constant pool. */
    public static class Acc {
        public static Object hello() { return Hello.class; }
        public static Object other() { return Other.class; }
        public static Object make() { return new Other(); }
    }
}
