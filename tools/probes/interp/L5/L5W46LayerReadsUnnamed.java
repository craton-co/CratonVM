// Interpreter round i1, wave 46, lane L5 -- JVMS 5.4.4's readability clause
// for a class of a user `ModuleLayer` module that names a class of an
// UNNAMED module
// (`docs/internal/fixed-bugs/interpreter-L5-a-class-of-a-named-module-in-a-user-layer-is-put-in-its-loaders-unnamed-module-FIXED-20261010.md`,
// "What remains": readability of an unnamed target from a layer class).
//
// Three modules in one layer (`defineModulesWithOneLoader` through the
// static form, which hands back a `Controller`), each holding a copy of this
// file's nested `Hello` (renamed), whose `get()` executes `new Target()` --
// `Target` is this file's nested class, defined by the application loader in
// its unnamed module:
//   m46u  an explicit module (reads java.base only)
//   m46v  an explicit module, then `controller.addReads(m46v, <the
//         application loader's unnamed module>)`
//   m46w  an automatic module (reads every unnamed module)
// Row `reflect` asks core reflection from m46u (the JDK assumes readability
// there: `Target.class.getConstructor().newInstance()` inside m46u's Hello
// through `Supplier`, not by name).
//
// Run (no setup; the jars are written under java.io.tmpdir):
//   javac -d out L5W46LayerReadsUnnamed.java
//   cratonvm --java-home <jdk25> [--nojit] -cp out L5W46LayerReadsUnnamed
//
// Expected HotSpot 25.0.3 output (`java` and `-Xint`, identical; the
// message is cut after "(in unnamed module": HotSpot goes on with
// "@0x<hash>) because module m46u does not read unnamed module @0x<hash>",
// an identity hash no VM reproduces):
//   explicit=java.lang.IllegalAccessError: class p46u.Hello (in module m46u) cannot access class L5W46LayerReadsUnnamed$Target (in unnamed module
//   add-reads=ok
//   automatic=ok
//   reflect=ok
//
// CratonVM base `55834015b` (`--jdk-only`, from the code, not run):
// `access_control::layer_accessor_readability_denial` never asked an
// unnamed target, so `explicit=ok`. Positive control: `CRATONVM_DBG=access`
// prints `[LAYER-MODULE] addReads m46v loader_ns=<n> -> Unnamed { loader_ns:
// 2 }` and `[LAYER-MODULE] addReads m46w loader_ns=<n> -> AllUnnamed` (both
// on the base too: the record is wave 45's), and the `explicit` row's
// `IllegalAccessError` is the new refusal.
// `--compatible`: the layer's classes do not load (as in `L5W43UserLayerModule`).

import java.io.*;
import java.lang.module.*;
import java.nio.charset.StandardCharsets;
import java.nio.file.*;
import java.util.*;
import java.util.function.Supplier;
import java.util.jar.*;

public class L5W46LayerReadsUnnamed {
    static final String P = "L5W46LayerReadsUnnamed$";

    static byte[] rename(byte[] bytes, String from, String to) throws IOException {
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
                case 1 -> out.writeUTF(in.readUTF().replace(from, to));
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

    static Path jar(String module, String pkg, byte[] hello, byte[] reflect) throws IOException {
        Path dir = Files.createTempDirectory("l5w46reads");
        Path jar = dir.resolve(module + ".jar");
        try (JarOutputStream out = new JarOutputStream(Files.newOutputStream(jar))) {
            out.putNextEntry(new JarEntry(pkg + "/Hello.class"));
            out.write(rename(hello, P + "Hello", pkg + "/Hello"));
            out.closeEntry();
            out.putNextEntry(new JarEntry(pkg + "/Reflect.class"));
            out.write(rename(reflect, P + "Reflect", pkg + "/Reflect"));
            out.closeEntry();
        }
        return dir;
    }

    static ModuleReference explicit(Path dir, String module, String pkg) {
        ModuleReference auto = ModuleFinder.of(dir).find(module).orElseThrow();
        ModuleDescriptor desc = ModuleDescriptor.newModule(module)
                .exports(pkg)
                .packages(Set.of(pkg))
                .build();
        return new ModuleReference(desc, auto.location().orElse(null)) {
            @Override
            public ModuleReader open() throws IOException {
                return auto.open();
            }
        };
    }

    static byte[] bytes(String simple) throws IOException {
        try (InputStream in = ClassLoader.getSystemResourceAsStream(P + simple + ".class")) {
            return in.readAllBytes();
        }
    }

    interface Row { Object run() throws Throwable; }

    static void row(String label, Row r) {
        try {
            System.out.println(label + "=" + r.run());
        } catch (Throwable t) {
            String m = String.valueOf(t.getMessage());
            String cut = "(in unnamed module";
            int at = m.indexOf(cut);
            System.out.println(label + "=" + t.getClass().getName() + ": "
                    + (at < 0 ? m : m.substring(0, at + cut.length())));
        }
    }

    @SuppressWarnings("unchecked")
    static String call(ClassLoader loader, String cn) throws Throwable {
        Class<?> c = loader.loadClass(cn);
        return ((Supplier<String>) c.getConstructor().newInstance()).get();
    }

    public static void main(String[] args) throws Exception {
        byte[] hello = bytes("Hello");
        byte[] reflect = bytes("Reflect");
        Path dirU = jar("m46u", "p46u", hello, reflect);
        Path dirV = jar("m46v", "p46v", hello, reflect);
        Path dirW = jar("m46w", "p46w", hello, reflect);
        ModuleReference u = explicit(dirU, "m46u", "p46u");
        ModuleReference v = explicit(dirV, "m46v", "p46v");
        ModuleFinder two = new ModuleFinder() {
            @Override
            public Optional<ModuleReference> find(String name) {
                return name.equals("m46u") ? Optional.of(u)
                        : name.equals("m46v") ? Optional.of(v) : Optional.empty();
            }

            @Override
            public Set<ModuleReference> findAll() {
                return Set.of(u, v);
            }
        };
        Configuration cf = ModuleLayer.boot().configuration().resolve(
                ModuleFinder.compose(two, ModuleFinder.of(dirW)), ModuleFinder.of(),
                Set.of("m46u", "m46v", "m46w"));
        ClassLoader app = L5W46LayerReadsUnnamed.class.getClassLoader();
        ModuleLayer.Controller controller = ModuleLayer.defineModulesWithOneLoader(cf,
                List.of(ModuleLayer.boot()), app);
        ModuleLayer layer = controller.layer();
        controller.addReads(layer.findModule("m46v").orElseThrow(), app.getUnnamedModule());
        ClassLoader loader = layer.findLoader("m46u");
        row("explicit", () -> call(loader, "p46u.Hello"));
        row("add-reads", () -> call(loader, "p46v.Hello"));
        row("automatic", () -> call(loader, "p46w.Hello"));
        row("reflect", () -> call(loader, "p46u.Reflect"));
    }

    public static class Target {
        public Target() {
        }
    }

    // Renamed into each module; `new Target()` is the CONSTANT_Class
    // resolution the readability clause judges.
    public static class Hello implements Supplier<String> {
        public Hello() {
        }

        @Override
        public String get() {
            Object t = new Target();
            return t != null ? "ok" : "null";
        }
    }

    // Renamed into each module; reflection only (the class is named by
    // string, never by a constant of this class).
    public static class Reflect implements Supplier<String> {
        public Reflect() {
        }

        @Override
        public String get() {
            try {
                Class<?> c = Class.forName("L5W46LayerReadsUnnamed$Target", false,
                        ClassLoader.getSystemClassLoader());
                return c.getConstructor().newInstance() != null ? "ok" : "null";
            } catch (ReflectiveOperationException e) {
                return e.toString();
            }
        }
    }
}
