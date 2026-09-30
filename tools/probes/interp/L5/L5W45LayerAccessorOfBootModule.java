// Interpreter round i1, wave 45, lane L5 -- a class of a named module in a
// user `ModuleLayer` as the ACCESSOR of a boot-layer module: it is in a
// NAMED module, so an export or opening to `ALL-UNNAMED` does not reach it
// (`docs/internal/fixed-bugs/interpreter-L5-a-class-of-a-named-module-in-a-user-layer-is-put-in-its-loaders-unnamed-module-FIXED-20261010.md`,
// "What remains").
//
// One accessor class (`Acc`) is defined twice: as `acc45.Acc` by a child
// loader of the application loader (the unnamed module), and as `p45g.Acc`
// in the module `m45g` of a layer. Its `CONSTANT_Class` entry for the
// nested `Target` is renamed to `jdk/internal/misc/Unsafe`.
//
// Rows (unnamed accessor, then layer accessor):
//   ldc       `ldc jdk.internal.misc.Unsafe`: resolves, then
//             IllegalAccessError (HotSpot's message)
//   reflect   Method.invoke of the public static Unsafe.getUnsafe: runs, then
//             IllegalAccessException
//   open      setAccessible(true) on the private field String.value: ok, then
//             InaccessibleObjectException
//   unread    `ldc java.sql.Date` (module java.sql, which m45g does not
//             require): resolves, then IllegalAccessError "does not read"
//             (JVMS 5.4.4's readability clause; needs no flag)
//
// Run (both VMs need the two flags; without them the unnamed column is
// refused as well):
//   javac -d out L5W45LayerAccessorOfBootModule.java
//   java|cratonvm [--nojit] --add-exports java.base/jdk.internal.misc=ALL-UNNAMED \
//       --add-opens java.base/java.lang=ALL-UNNAMED -cp out L5W45LayerAccessorOfBootModule
//
// Expected HotSpot 25.0.3 output (`java` and `-Xint`, identical; compare
// verbatim):
//   ldc=ok jdk.internal.misc.Unsafe | java.lang.IllegalAccessError: class p45g.Acc (in module m45g) cannot access class jdk.internal.misc.Unsafe (in module java.base) because module java.base does not export jdk.internal.misc to module m45g
//   reflect=ok jdk.internal.misc.Unsafe | java.lang.IllegalAccessException
//   open=ok | java.lang.reflect.InaccessibleObjectException
//   unread=ok java.sql.Date | java.lang.IllegalAccessError: class p45g.Acc (in module m45g) cannot access class java.sql.Date (in module java.sql) because module m45g does not read module java.sql
//
// On the base `69568bea6` (from the code): the VM's access control reads the
// layer class's `module_name`, `None`, as the unnamed module (the layer
// module is known only as a TARGET since wave 44), so the `ALL-UNNAMED`
// edges admit it: `ldc=ok jdk.internal.misc.Unsafe | ok
// jdk.internal.misc.Unsafe`, `reflect=ok jdk.internal.misc.Unsafe | ok
// jdk.internal.misc.Unsafe`, `open=ok | ok`; and the VM knows no layer
// module's reads, so `unread=ok java.sql.Date | ok java.sql.Date`.
//
// Positive control (`--jdk-only`): `CRATONVM_DBG=access` prints
// `[LAYER-MODULE] defineModule0 m45g loader_ns=<n> packages=["p45g"]
// open=false` (as on the base) and `[LAYER-MODULE] addReads m45g
// loader_ns=<n> -> Named("java.base")` (new: the identity-keyed read record;
// no `addReads` line exists on the base); the refusals are the fix. `--compatible`
// keeps the base's answers by design (no layer module is recorded there; the
// layer class does not load, as in `L5W43UserLayerModule`).

import java.io.*;
import java.lang.module.*;
import java.lang.reflect.InaccessibleObjectException;
import java.lang.reflect.InvocationTargetException;
import java.net.URI;
import java.nio.ByteBuffer;
import java.util.*;

public class L5W45LayerAccessorOfBootModule {
    static final String P = "L5W45LayerAccessorOfBootModule$";
    static final ClassLoader APP = L5W45LayerAccessorOfBootModule.class.getClassLoader();

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

    static byte[] accessor(String name) throws IOException {
        byte[] bytes;
        try (InputStream in = ClassLoader.getSystemResourceAsStream(P + "Acc.class")) { bytes = in.readAllBytes(); }
        // `$Target` first: the two keys overlap only in the shared prefix.
        Map<String, String> renames = new LinkedHashMap<>();
        renames.put(P + "Target", "jdk/internal/misc/Unsafe");
        renames.put(P + "Sql", "java/sql/Date");
        renames.put(P + "Acc", name);
        return rename(bytes, renames);
    }

    static ModuleFinder finder(ModuleDescriptor d, Map<String, byte[]> fs) {
        ModuleReference ref = new ModuleReference(d, null) {
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
        };
        return new ModuleFinder() {
            public Optional<ModuleReference> find(String n) { return n.equals(d.name()) ? Optional.of(ref) : Optional.empty(); }
            public Set<ModuleReference> findAll() { return Set.of(ref); }
        };
    }

    static final class Child extends ClassLoader {
        final byte[] bytes;
        Child(byte[] bytes) { super("child", APP); this.bytes = bytes; }
        @Override protected Class<?> findClass(String n) throws ClassNotFoundException {
            if (!n.equals("acc45.Acc")) throw new ClassNotFoundException(n);
            return defineClass(n, bytes, 0, bytes.length);
        }
    }

    static String call(Class<?> acc, String method) throws Throwable {
        try {
            return (String) acc.getMethod(method).invoke(null);
        } catch (InvocationTargetException e) {
            Throwable t = e.getCause();
            if (t instanceof IllegalAccessError) {
                return t.getClass().getName() + ": " + t.getMessage().replaceAll(" @(0x)?[0-9a-f]+", "");
            }
            throw t;
        }
    }

    public static void main(String[] args) throws Throwable {
        Class<?> unnamed = new Child(accessor("acc45/Acc")).loadClass("acc45.Acc");
        ModuleDescriptor d = ModuleDescriptor.newModule("m45g").packages(Set.of("p45g")).exports("p45g").build();
        Configuration cf = ModuleLayer.boot().configuration()
                .resolve(finder(d, Map.of("p45g/Acc.class", accessor("p45g/Acc"))), ModuleFinder.of(), Set.of("m45g"));
        ModuleLayer layer = ModuleLayer.boot().defineModulesWithOneLoader(cf, APP);
        Class<?> named = layer.findLoader("m45g").loadClass("p45g.Acc");
        for (String row : List.of("ldc", "reflect", "open", "unread")) {
            String a;
            String b;
            try {
                a = call(unnamed, row);
            } catch (Throwable t) {
                a = t.getClass().getName();
            }
            try {
                b = call(named, row);
            } catch (Throwable t) {
                b = t.getClass().getName();
            }
            System.out.println(row + "=" + a + " | " + b);
        }
    }

    /** Stands for jdk.internal.misc.Unsafe in the renamed accessor. */
    public static class Target {}

    /** Stands for java.sql.Date (module java.sql, which m45g does not read). */
    public static class Sql {}

    public static class Acc {
        public static String ldc() {
            return "ok " + Target.class.getName();
        }

        public static String reflect() {
            try {
                Object u = Class.forName("jdk.internal.misc.Unsafe").getMethod("getUnsafe").invoke(null);
                return "ok " + u.getClass().getName();
            } catch (IllegalAccessException e) {
                return e.getClass().getName();
            } catch (ReflectiveOperationException e) {
                return "other " + e;
            }
        }

        public static String unread() {
            return "ok " + Sql.class.getName();
        }

        public static String open() {
            try {
                String.class.getDeclaredField("value").setAccessible(true);
                return "ok";
            } catch (InaccessibleObjectException e) {
                return e.getClass().getName();
            } catch (NoSuchFieldException e) {
                return "other " + e;
            }
        }
    }
}
