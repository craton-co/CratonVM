// Interpreter round i1, wave 42, lane L5 -- a review probe of the
// `ClassLoader` surface against HotSpot 25, written without a CratonVM run
// (the lane does not build): `findLoadedClass` for a defined class, a slash
// name, arrays, a primitive name, a class the parent loaded, and `null`;
// `getPackage` / `getDefinedPackage` / `definePackage` / `getDefinedPackages`
// after a define and for the unnamed package; `getResource(s)` /
// `getResourceAsStream` through a null parent and an application parent;
// `registerAsParallelCapable` on a subclass pair and `getClassLoadingLock`;
// and `ModuleLayer.defineModulesWithOneLoader` over an in-memory module
// (`ModuleFinder` / `ModuleReader` implemented in Java), whose class is
// loaded, run and looked up with `Class.forName(Module, String)`.
//
// Every row is traced to JDK bytecode under `--jdk-only` except the natives
// `findLoadedClass0` (`native-builtins/src/lib.rs`
// `native_classloader_find_loaded_class`) and the define natives; a row that
// differs on the host is a finding for the next wave.
//
// Host run of wave 42 (merged head `1965c67ef`, default and `--nojit`): four
// rows differed, the rest matched.
//   find-loaded-own=true class l5w42.Hello   (a '/' name answered; fixed in
//     the wave-42 follow-up: `findLoadedClass` / `findLoadedClass0` refuse it)
//   define-package-after-class=package l5w42 and defined-packages=[l5w42y]
//     (the class's define never put its package in the loader's `packages`
//     map; fixed in the follow-up: `getNamedPackage` runs the JDK's body)
//   module-layer=java.lang.ClassNotFoundException: m42p.Hello  (open:
//     docs/internal/fixed-bugs/interpreter-L5-a-class-of-a-named-module-in-a-user-layer-is-put-in-its-loaders-unnamed-module-FIXED-20261010.md)
//
// Run (no setup):
//   javac -d out L5W42LoaderSurface.java
//   cratonvm --java-home <jdk25> [--nojit] -cp out L5W42LoaderSurface
//
// Expected HotSpot 25.0.3 output (`java` and `-Xint`, identical; compare
// verbatim):
//   find-loaded-own=true null
//   find-loaded-array=null null null
//   find-loaded-parent=class java.lang.String null null
//   find-loaded-null=null
//   package-after-define=l5w42 true true sealed=false spec=null
//   define-package-twice=java.lang.IllegalArgumentException: l5w42x
//   define-package-after-class=java.lang.IllegalArgumentException: l5w42
//   defined-packages=[l5w42, l5w42y]
//   package-unnamed=[] true
//   resource-null-parent-boot=true
//   resource-null-parent-app=null
//   resource-app-parent=true
//   resources-app-parent=1
//   resource-stream-null-parent=null
//   parallel=true false false true true
//   open-not-parallel=false true
//   module-layer=m42 true hi true true pkg=m42p named=true layer=true find=true unexported=null

import java.io.*;
import java.lang.module.*;
import java.net.URI;
import java.nio.ByteBuffer;
import java.util.*;

public class L5W42LoaderSurface {
    static final String P = "L5W42LoaderSurface$";

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

    static byte[] hello(String as) throws IOException {
        return rename(own(P + "Hello"), Map.of(P + "Hello", as));
    }

    static String norm(String s) { return s == null ? "null" : s.replaceAll("@[0-9a-f]+", "@H"); }

    static class Open extends ClassLoader {
        Open(String name, ClassLoader parent) { super(name, parent); }
        Class<?> def(String name, byte[] b) { return defineClass(name, b, 0, b.length); }
        Class<?> def(String name, byte[] b, int off, int len) { return defineClass(name, b, off, len); }
        Class<?> defBuf(String name, ByteBuffer bb) { return defineClass(name, bb, null); }
        Class<?> loaded(String name) { return findLoadedClass(name); }
        Package defPkg(String name) { return definePackage(name, null, null, null, null, null, null, null); }
        Package pkg(String name) { return getPackage(name); }
        Object lock(String name) { return getClassLoadingLock(name); }
    }

    static class Parallel extends ClassLoader {
        static { registerAsParallelCapable(); }
        Parallel() { super(null); }
        Object lock(String name) { return getClassLoadingLock(name); }
    }

    static class NotParallel extends Parallel {
        Object lock2(String name) { return getClassLoadingLock(name); }
    }

    interface Row { Object run() throws Throwable; }

    static void row(String label, Row r) {
        try {
            System.out.println(label + "=" + r.run());
        } catch (Throwable t) {
            System.out.println(label + "=" + t.getClass().getName() + ": " + norm(t.getMessage()));
        }
    }

    public static void main(String[] args) throws Exception {
        row("find-loaded-own", () -> {
            Open o = new Open("o", null);
            Class<?> c = o.def("l5w42.Hello", hello("l5w42/Hello"));
            return (o.loaded("l5w42.Hello") == c) + " " + o.loaded("l5w42/Hello");
        });
        row("find-loaded-array", () -> {
            Open o = new Open("o", null);
            o.def("l5w42.Hello", hello("l5w42/Hello"));
            return o.loaded("[Ll5w42.Hello;") + " " + o.loaded("[I") + " " + o.loaded("int");
        });
        row("find-loaded-parent", () -> {
            Open o = new Open("o", L5W42LoaderSurface.class.getClassLoader());
            Class<?> viaParent = o.loadClass("java.lang.String");
            return viaParent + " " + o.loaded("java.lang.String") + " " + o.loaded("java.lang.Object");
        });
        row("find-loaded-null", () -> new Open("o", null).loaded(null));
        row("package-after-define", () -> {
            Open o = new Open("o", null);
            Class<?> c = o.def("l5w42.Hello", hello("l5w42/Hello"));
            Package p = c.getPackage();
            return p.getName() + " " + (o.getDefinedPackage("l5w42") == p) + " " + (o.pkg("l5w42") == p)
                    + " sealed=" + p.isSealed() + " spec=" + p.getSpecificationTitle();
        });
        row("define-package-twice", () -> {
            Open o = new Open("o", null);
            o.defPkg("l5w42x");
            return o.defPkg("l5w42x");
        });
        row("define-package-after-class", () -> {
            Open o = new Open("o", null);
            o.def("l5w42.Hello", hello("l5w42/Hello"));
            return o.defPkg("l5w42");
        });
        row("defined-packages", () -> {
            Open o = new Open("o", null);
            o.def("l5w42.Hello", hello("l5w42/Hello"));
            o.defPkg("l5w42y");
            List<String> names = new ArrayList<>();
            for (Package p : o.getDefinedPackages()) names.add(p.getName());
            Collections.sort(names);
            return names;
        });
        row("package-unnamed", () -> {
            Open o = new Open("o", null);
            Class<?> c = o.def("L5W42Top", hello("L5W42Top"));
            return "[" + c.getPackage().getName() + "] " + c.getPackageName().isEmpty();
        });
        row("resource-null-parent-boot", () -> new Open("o", null).getResource("java/lang/Object.class") != null);
        row("resource-null-parent-app", () -> new Open("o", null).getResource(P.replace('.', '/') + "Hello.class"));
        row("resource-app-parent", () -> new Open("o", L5W42LoaderSurface.class.getClassLoader())
                .getResource("L5W42LoaderSurface$Hello.class") != null);
        row("resources-app-parent", () -> Collections.list(new Open("o", L5W42LoaderSurface.class.getClassLoader())
                .getResources("L5W42LoaderSurface$Hello.class")).size());
        row("resource-stream-null-parent", () -> new Open("o", null).getResourceAsStream("L5W42LoaderSurface$Hello.class"));
        row("parallel", () -> {
            Parallel p = new Parallel();
            NotParallel n = new NotParallel();
            return p.isRegisteredAsParallelCapable() + " " + n.isRegisteredAsParallelCapable()
                    + " " + (p.lock("x") == p) + " " + (n.lock2("x") == n)
                    + " " + (p.lock("x") == p.lock("x"));
        });
        row("open-not-parallel", () -> {
            Open o = new Open("o", null);
            return o.isRegisteredAsParallelCapable() + " " + (o.lock("x") == o);
        });
        row("module-layer", () -> {
            byte[] b = hello("m42p/Hello");
            ModuleDescriptor d = ModuleDescriptor.newModule("m42").packages(Set.of("m42p")).exports("m42p").build();
            ModuleReference ref = new ModuleReference(d, null) {
                public ModuleReader open() {
                    return new ModuleReader() {
                        public Optional<URI> find(String name) { return Optional.empty(); }
                        public Optional<ByteBuffer> read(String name) {
                            return name.equals("m42p/Hello.class") ? Optional.of(ByteBuffer.wrap(b)) : Optional.empty();
                        }
                        public Optional<InputStream> open(String name) {
                            return name.equals("m42p/Hello.class") ? Optional.of(new ByteArrayInputStream(b)) : Optional.empty();
                        }
                        public java.util.stream.Stream<String> list() { return java.util.stream.Stream.of("m42p/Hello.class"); }
                        public void close() {}
                    };
                }
            };
            ModuleFinder finder = new ModuleFinder() {
                public Optional<ModuleReference> find(String name) { return name.equals("m42") ? Optional.of(ref) : Optional.empty(); }
                public Set<ModuleReference> findAll() { return Set.of(ref); }
            };
            ModuleLayer boot = ModuleLayer.boot();
            Configuration cf = boot.configuration().resolve(finder, ModuleFinder.of(), Set.of("m42"));
            ModuleLayer layer = boot.defineModulesWithOneLoader(cf, L5W42LoaderSurface.class.getClassLoader());
            ClassLoader l = layer.findLoader("m42");
            Class<?> c = l.loadClass("m42p.Hello");
            Object r = c.getMethod("hi").invoke(null);
            return c.getModule().getName() + " " + (c.getClassLoader() == l) + " " + r + " "
                    + c.getModule().isExported("m42p") + " " + (l.getParent() == L5W42LoaderSurface.class.getClassLoader())
                    + " pkg=" + c.getPackage().getName() + " named=" + c.getModule().isNamed()
                    + " layer=" + (c.getModule().getLayer() == layer)
                    + " find=" + (Class.forName(c.getModule(), "m42p.Hello") == c)
                    + " unexported=" + Class.forName(c.getModule(), "m42p.Nope");
        });
    }

    public static class Hello {
        public static String hi() { return "hi"; }
    }
}
