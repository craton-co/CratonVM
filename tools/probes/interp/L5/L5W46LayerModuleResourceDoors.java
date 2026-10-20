// Interpreter round i1, wave 46, lane L5 -- the resource doors of a module of
// a user `ModuleLayer`, where the JDK's named-module answer differs from a
// loader's `getResource[AsStream]`
// (`docs/internal/fixed-bugs/interpreter-L5-a-class-of-a-named-module-in-a-user-layer-is-put-in-its-loaders-unnamed-module-FIXED-20261010.md`,
// "What remains": a layer module's resources).
//
// The probe writes two jars into fresh temporary directories:
//   * `m46r.jar` (the class `p46r.Hello` -- this file's nested `Hello`,
//     renamed -- and `p46r/data.txt`), wrapped in an EXPLICIT module `m46r`
//     that exports `p46r` and opens nothing (a ModuleReference whose
//     descriptor is built with `ModuleDescriptor.newModule`, reading the
//     jar through the automatic module's own reader);
//   * `m46a.jar` (`p46a.Hello` and `p46a/data.txt`), the AUTOMATIC module
//     `m46a` (every package open).
// Both are defined by one `defineModulesWithOneLoader`.
//
// Rows (one per door; `own-*` rows run inside `p46r.Hello`):
//   own-stream        Hello.class.getResourceAsStream("data.txt") from Hello
//   own-url           Hello.class.getResource("data.txt") from Hello: protocol
//   own-module        Hello's Module.getResourceAsStream("p46r/data.txt") from Hello
//   other-stream      the same class door from this (unnamed-module) class
//   other-module      the same Module door from this class
//   other-class-file  getResourceAsStream("Hello.class") from this class
//   parent-stream     getResourceAsStream("/L5W46LayerModuleResourceDoors.class")
//                     (a class-path resource of the PARENT loader; a named
//                     module's class asks its own module only)
//   parent-url        getResource of the same name
//   auto-module       m46a's Module.getResourceAsStream("p46a/data.txt")
//   loader-stream     the layer loader's getResourceAsStream("p46r/data.txt")
//                     (the loader serves only an unconditionally open package)
//
// Run (no setup; the jars are written under java.io.tmpdir):
//   javac -d out L5W46LayerModuleResourceDoors.java
//   cratonvm --java-home <jdk25> [--nojit] -cp out L5W46LayerModuleResourceDoors
//
// Expected HotSpot 25.0.3 output (`java` and `-Xint`, identical; compare
// verbatim):
//   own-stream=hello-r
//   own-url=jar
//   own-module=hello-r
//   other-stream=null
//   other-module=null
//   other-class-file=true
//   parent-stream=false
//   parent-url=null
//   auto-module=hello-a
//   loader-stream=null
//
// CratonVM base `55834015b` (`--jdk-only`, from the code, not run):
// `Class.getResource[AsStream]` asked the layer loader's
// `getResource[AsStream]` (parent first, then a package only when open to
// everyone) and `Module.getResourceAsStream` scanned the flat class path, so
//   own-stream=null, own-url=null, own-module=null, parent-stream=true,
//   parent-url=file (or jar), auto-module=null;
// the other rows as HotSpot. Positive control: `CRATONVM_DBG=access` prints
// `[LAYER-MODULE] resource p46r/data.txt -> the module loader's
// findResource(mn, name)` and `[LAYER-MODULE] resource p46r/data.txt of a
// layer module, caller L5W46LayerModuleResourceDoors -> encapsulated`
// (no `resource` line on the base).
// `--compatible`: the layer's classes do not load (as in `L5W43UserLayerModule`).

import java.io.*;
import java.lang.module.*;
import java.net.URI;
import java.nio.charset.StandardCharsets;
import java.nio.file.*;
import java.util.*;
import java.util.function.Supplier;
import java.util.jar.*;

public class L5W46LayerModuleResourceDoors {
    static final String P = "L5W46LayerModuleResourceDoors$";

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

    static String read(InputStream in) throws IOException {
        if (in == null) return "null";
        try (in) {
            return new String(in.readAllBytes(), StandardCharsets.UTF_8);
        }
    }

    static Path jar(String module, String pkg, String data, byte[] template) throws IOException {
        Path dir = Files.createTempDirectory("l5w46res");
        Path jar = dir.resolve(module + ".jar");
        try (JarOutputStream out = new JarOutputStream(Files.newOutputStream(jar))) {
            out.putNextEntry(new JarEntry(pkg + "/Hello.class"));
            out.write(rename(template, P + "Hello", pkg + "/Hello"));
            out.closeEntry();
            out.putNextEntry(new JarEntry(pkg + "/data.txt"));
            out.write(data.getBytes(StandardCharsets.UTF_8));
            out.closeEntry();
        }
        return dir;
    }

    interface Row { Object run() throws Throwable; }

    static void row(String label, Row r) {
        try {
            System.out.println(label + "=" + r.run());
        } catch (Throwable t) {
            System.out.println(label + "=" + t.getClass().getName() + ": " + t.getMessage());
        }
    }

    public static void main(String[] args) throws Exception {
        byte[] hello;
        try (InputStream in = ClassLoader.getSystemResourceAsStream(P + "Hello.class")) {
            hello = in.readAllBytes();
        }
        Path dirR = jar("m46r", "p46r", "hello-r", hello);
        Path dirA = jar("m46a", "p46a", "hello-a", hello);

        ModuleReference auto = ModuleFinder.of(dirR).find("m46r").orElseThrow();
        ModuleDescriptor desc = ModuleDescriptor.newModule("m46r")
                .exports("p46r")
                .packages(Set.of("p46r"))
                .build();
        ModuleReference explicit = new ModuleReference(desc, auto.location().orElse(null)) {
            @Override
            public ModuleReader open() throws IOException {
                return auto.open();
            }
        };
        ModuleFinder one = new ModuleFinder() {
            @Override
            public Optional<ModuleReference> find(String name) {
                return name.equals("m46r") ? Optional.of(explicit) : Optional.empty();
            }

            @Override
            public Set<ModuleReference> findAll() {
                return Set.of(explicit);
            }
        };
        Configuration cf = ModuleLayer.boot().configuration().resolve(
                ModuleFinder.compose(one, ModuleFinder.of(dirA)), ModuleFinder.of(),
                Set.of("m46r", "m46a"));
        ModuleLayer layer = ModuleLayer.boot().defineModulesWithOneLoader(cf,
                L5W46LayerModuleResourceDoors.class.getClassLoader());
        ClassLoader loader = layer.findLoader("m46r");
        Class<?> c = loader.loadClass("p46r.Hello");
        @SuppressWarnings("unchecked")
        Supplier<String> own = (Supplier<String>) c.getConstructor().newInstance();
        String[] ownRows = own.get().split(";");
        System.out.println("own-stream=" + ownRows[0]);
        System.out.println("own-url=" + ownRows[1]);
        System.out.println("own-module=" + ownRows[2]);
        row("other-stream", () -> read(c.getResourceAsStream("data.txt")));
        row("other-module", () -> read(c.getModule().getResourceAsStream("p46r/data.txt")));
        row("other-class-file", () -> {
            InputStream in = c.getResourceAsStream("Hello.class");
            boolean found = in != null;
            if (in != null) in.close();
            return found;
        });
        row("parent-stream", () -> {
            InputStream in = c.getResourceAsStream("/L5W46LayerModuleResourceDoors.class");
            boolean found = in != null;
            if (in != null) in.close();
            return found;
        });
        row("parent-url", () -> {
            java.net.URL u = c.getResource("/L5W46LayerModuleResourceDoors.class");
            return u == null ? "null" : u.getProtocol();
        });
        row("auto-module", () -> read(layer.findModule("m46a").orElseThrow()
                .getResourceAsStream("p46a/data.txt")));
        row("loader-stream", () -> read(loader.getResourceAsStream("p46r/data.txt")));
    }

    // Renamed to `p46r.Hello` / `p46a.Hello`; reads its own module's resource
    // through the three doors, from inside the module (no lambda: its
    // bootstrap would name this probe's class).
    public static class Hello implements Supplier<String> {
        public Hello() {
        }

        @Override
        public String get() {
            StringBuilder sb = new StringBuilder();
            try {
                sb.append(read(Hello.class.getResourceAsStream("data.txt")));
                sb.append(';');
                java.net.URL u = Hello.class.getResource("data.txt");
                sb.append(u == null ? "null" : u.getProtocol());
                sb.append(';');
                String pkg = Hello.class.getPackageName().replace('.', '/');
                sb.append(read(Hello.class.getModule().getResourceAsStream(pkg.concat("/data.txt"))));
            } catch (Throwable t) {
                sb.append(t.getClass().getName()).append(": ").append(t.getMessage());
            }
            return sb.toString();
        }

        static String read(InputStream in) throws IOException {
            if (in == null) return "null";
            try (in) {
                return new String(in.readAllBytes(), StandardCharsets.UTF_8);
            }
        }
    }
}
