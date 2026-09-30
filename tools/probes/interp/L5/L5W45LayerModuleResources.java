// Interpreter round i1, wave 45, lane L5 -- the resources of a module of a
// user `ModuleLayer` (`docs/internal/fixed-bugs/interpreter-L5-a-class-of-a-named-module-in-a-user-layer-is-put-in-its-loaders-unnamed-module-FIXED-20261010.md`,
// "What remains": a layer module's resources, never probed before).
//
// The probe writes `m45r.jar` into a fresh temporary directory: the class
// `p45r.Hello` (this file's nested `Hello`, renamed) and the resource
// `p45r/data.txt`. `ModuleFinder.of(dir)` makes it the automatic module
// `m45r` (named from the jar), defined by `defineModulesWithOneLoader`. An
// automatic module opens every package, so no row is refused by
// encapsulation; each row asks one of the JDK's resource doors.
//
// Rows:
//   class-stream   Hello.class.getResourceAsStream("data.txt")  (the JDK:
//                  the module is named, so `Loader.findResource(mn, name)`)
//   class-url      Hello.class.getResource("data.txt"): its protocol
//   own-class      Hello.class.getResourceAsStream("Hello.class") != null
//   module-stream  the Module's getResourceAsStream("p45r/data.txt")
//   loader-url     the layer's loader.getResource("p45r/data.txt"): protocol
//   loader-stream  the layer's loader.getResourceAsStream("p45r/data.txt")
//   loader-list    Collections.list(loader.getResources("p45r/data.txt")).size()
//   module-name    Hello.class.getModule().getName(), isNamed()
//
// Run (no setup; the jar is written under java.io.tmpdir):
//   javac -d out L5W45LayerModuleResources.java
//   cratonvm --java-home <jdk25> [--nojit] -cp out L5W45LayerModuleResources
//
// Expected HotSpot 25.0.3 output (`java` and `-Xint`, identical; compare
// verbatim):
//   class-stream=hello-resource
//   class-url=jar
//   own-class=true
//   module-stream=hello-resource
//   loader-url=jar
//   loader-stream=hello-resource
//   loader-list=1
//   module-name=m45r true
//
// CratonVM (not run by the lane; the base and this wave are the same code
// here): `Class.getResourceAsStream` / `getResource` are natives that hand a
// non-built-in loader's class to the loader's own `getResourceAsStream` /
// `getResource` (`lang_class.rs`), and `Module.getResourceAsStream` is a
// native (`jboss_jdkspecific::native_module_get_resource_as_stream`); none of
// them asks the layer's `Loader.findResource(mn, name)` / `ModuleReader` the
// way the JDK does. A row that differs names the door to fix. `--compatible`:
// the layer's classes do not load (as in `L5W43UserLayerModule`).
//
// Host, wave-45 build (`--jdk-only`, JIT and `--nojit`): every row HotSpot's
// but `module-stream=null` (the native scanned the flat class path). Fixed
// in wave 46, lane L5: `Module.getResourceAsStream` of a layer module runs
// the JDK's steps (`jboss_jdkspecific::layer_module_resource`); the other
// doors follow the JDK's named-module arm too (probe
// `L5W46LayerModuleResourceDoors` for the rows where the old paths differ).

import java.io.*;
import java.lang.module.*;
import java.nio.charset.StandardCharsets;
import java.nio.file.*;
import java.util.*;
import java.util.jar.*;

public class L5W45LayerModuleResources {
    static final String P = "L5W45LayerModuleResources$";

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
            hello = rename(in.readAllBytes(), P + "Hello", "p45r/Hello");
        }
        Path dir = Files.createTempDirectory("l5w45res");
        Path jar = dir.resolve("m45r.jar");
        try (JarOutputStream out = new JarOutputStream(Files.newOutputStream(jar))) {
            out.putNextEntry(new JarEntry("p45r/Hello.class"));
            out.write(hello);
            out.closeEntry();
            out.putNextEntry(new JarEntry("p45r/data.txt"));
            out.write("hello-resource".getBytes(StandardCharsets.UTF_8));
            out.closeEntry();
        }
        Configuration cf = ModuleLayer.boot().configuration()
                .resolve(ModuleFinder.of(dir), ModuleFinder.of(), Set.of("m45r"));
        ModuleLayer layer = ModuleLayer.boot().defineModulesWithOneLoader(cf,
                L5W45LayerModuleResources.class.getClassLoader());
        ClassLoader loader = layer.findLoader("m45r");
        Class<?> c = loader.loadClass("p45r.Hello");
        row("class-stream", () -> read(c.getResourceAsStream("data.txt")));
        row("class-url", () -> {
            java.net.URL u = c.getResource("data.txt");
            return u == null ? "null" : u.getProtocol();
        });
        row("own-class", () -> {
            InputStream in = c.getResourceAsStream("Hello.class");
            boolean found = in != null;
            if (in != null) in.close();
            return found;
        });
        row("module-stream", () -> read(c.getModule().getResourceAsStream("p45r/data.txt")));
        row("loader-url", () -> {
            java.net.URL u = loader.getResource("p45r/data.txt");
            return u == null ? "null" : u.getProtocol();
        });
        row("loader-stream", () -> read(loader.getResourceAsStream("p45r/data.txt")));
        row("loader-list", () -> Collections.list(loader.getResources("p45r/data.txt")).size());
        row("module-name", () -> c.getModule().getName() + " " + c.getModule().isNamed());
    }

    public static class Hello {
        public static String hi() { return "hi"; }
    }
}
