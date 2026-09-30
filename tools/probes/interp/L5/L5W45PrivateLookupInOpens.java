// Interpreter round i1, wave 45, lane L5 (review) -- `MethodHandles.privateLookupIn`
// asks whether the target's module OPENS its package to the caller
// (`docs/internal/fixed-bugs/interpreter-L5-privatelookupin-admits-a-target-whose-package-is-not-open-FIXED-20261010.md`).
//
// Rows (the caller is this class, in the unnamed module):
//   own        privateLookupIn(this class): ok, full modes
//   string     privateLookupIn(java.lang.String): java.base does not open
//              java.lang -- IllegalAccessException
//   layer-exp  a class of a layer module m45p that exports p45p and does not
//              open it: IllegalAccessException
//   layer-open a class of a layer module m45q that opens p45q: ok, with the
//              cross-module modes (PRIVATE|PROTECTED|PACKAGE|PUBLIC = 15) and
//              this class as the previous lookup class
//
// Run (no setup):
//   javac -d out L5W45PrivateLookupInOpens.java
//   cratonvm --java-home <jdk25> [--nojit] -cp out L5W45PrivateLookupInOpens
//
// Expected HotSpot 25.0.3 output (`java` and `-Xint`, identical):
//   own=ok 31 null
//   string=java.lang.IllegalAccessException: module java.base does not open java.lang to unnamed module
//   layer-exp=java.lang.IllegalAccessException: module m45p does not open p45p to unnamed module
//   layer-open=ok 15 L5W45PrivateLookupInOpens
//
// On the base `69568bea6` (from the code, every mode): `privateLookupIn` is
// a native (`lang_invoke.rs`, `pli_enforce`) that checks the caller's modes
// and the target's shape but not the module, and always answers modes 0x1F
// with no previous lookup class: `string=ok 31 null`, `layer-exp=ok 31
// null`, `layer-open=ok 31 null`. Wave 45 (lane L5, `--jdk-only`) asks the
// module half (`classloader::private_lookup_in_module_refusal`): `string`
// and `layer-exp` match; `layer-open` still prints `ok 31 null` (the
// cross-module modes and previous lookup class are not built; see the page).
// Wave 46 (lane L4) builds them: `layer-open` matches HotSpot under
// `--jdk-only` (`L4W46PrivateLookupInAcrossModules` has more rows).
// `--compatible` keeps the base's answers.

import java.io.*;
import java.lang.invoke.MethodHandles;
import java.lang.module.*;
import java.net.URI;
import java.nio.ByteBuffer;
import java.util.*;

public class L5W45PrivateLookupInOpens {
    static final String P = "L5W45PrivateLookupInOpens$";

    static byte[] hello(String pkg) throws IOException {
        byte[] bytes;
        try (InputStream in = ClassLoader.getSystemResourceAsStream(P + "Hello.class")) { bytes = in.readAllBytes(); }
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
                case 1 -> out.writeUTF(in.readUTF().replace(P + "Hello", pkg + "/Hello"));
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

    static Class<?> layerClass(ModuleDescriptor d, String pkg) throws Exception {
        byte[] b = hello(pkg);
        ModuleReference ref = new ModuleReference(d, null) {
            public ModuleReader open() {
                return new ModuleReader() {
                    public Optional<URI> find(String n) { return Optional.empty(); }
                    public Optional<ByteBuffer> read(String n) {
                        return n.equals(pkg + "/Hello.class") ? Optional.of(ByteBuffer.wrap(b)) : Optional.empty();
                    }
                    public java.util.stream.Stream<String> list() { return java.util.stream.Stream.of(pkg + "/Hello.class"); }
                    public void close() {}
                };
            }
        };
        ModuleFinder finder = new ModuleFinder() {
            public Optional<ModuleReference> find(String n) { return n.equals(d.name()) ? Optional.of(ref) : Optional.empty(); }
            public Set<ModuleReference> findAll() { return Set.of(ref); }
        };
        Configuration cf = ModuleLayer.boot().configuration().resolve(finder, ModuleFinder.of(), Set.of(d.name()));
        ModuleLayer layer = ModuleLayer.boot().defineModulesWithOneLoader(cf,
                L5W45PrivateLookupInOpens.class.getClassLoader());
        return layer.findLoader(d.name()).loadClass(pkg.replace('/', '.') + ".Hello");
    }

    static String pli(Class<?> target) {
        try {
            MethodHandles.Lookup l = MethodHandles.privateLookupIn(target, MethodHandles.lookup());
            Class<?> prev = l.previousLookupClass();
            return "ok " + l.lookupModes() + " " + (prev == null ? "null" : prev.getName());
        } catch (IllegalAccessException e) {
            return e.getClass().getName() + ": " + e.getMessage().replaceAll(" @(0x)?[0-9a-f]+", "");
        }
    }

    public static void main(String[] args) throws Exception {
        System.out.println("own=" + pli(L5W45PrivateLookupInOpens.class));
        System.out.println("string=" + pli(String.class));
        System.out.println("layer-exp=" + pli(layerClass(
                ModuleDescriptor.newModule("m45p").packages(Set.of("p45p")).exports("p45p").build(), "p45p")));
        System.out.println("layer-open=" + pli(layerClass(
                ModuleDescriptor.newModule("m45q").packages(Set.of("p45q")).opens("p45q").build(), "p45q")));
    }

    public static class Hello {
        public static String hi() { return "hi"; }
    }
}
