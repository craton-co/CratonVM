// Interpreter round i1, wave 45, lane L5 -- `opens` against `exports` for a
// class of a named module in a user `ModuleLayer`, and the module a stack
// trace element of such a class names
// (`docs/internal/fixed-bugs/interpreter-L5-a-class-of-a-named-module-in-a-user-layer-is-put-in-its-loaders-unnamed-module-FIXED-20261010.md`,
// "What remains"; `i44-L5-proposal-a-classs-module-is-an-identity-every-reader-sees`).
//
// Every module is in memory (a `ModuleDescriptor` built in Java, a
// `ModuleFinder` / `ModuleReader` written here), its classes taken from this
// file's nested classes and renamed into the module's packages, as in
// `L5W44LayerModuleAccess`.
//
// Rows:
//   exported-private  m45a exports p45a and does not open it: this
//                     (unnamed-module) class's setAccessible(true) on the
//                     private static field p45a.Hello.secret is refused
//   exported-public   the same class's public static method: admitted
//                     (`checkCanSetAccessible`'s exported-and-public arm)
//   opened            m45b opens p45b (and does not export it): the private
//                     field is admitted
//   duplicate         Lookup.defineClass (a privateLookupIn of p45b.Hello)
//                     of p45b.Hello again: the LinkageError names module m45b
//   open-qualified    m45c `opens p45c to m45d`: a class of m45d is admitted,
//                     this class is refused
//   controller-open   ModuleLayer.Controller.addOpens(m45f, p45f, this
//                     class's unnamed module): refused before, admitted
//                     after
//   ste               the top stack trace element of an exception thrown by
//                     a class of m45a (no version) and of m45e (version 2.1):
//                     getModuleName, getModuleVersion, toString
//
// Run (no setup):
//   javac -d out L5W45LayerModuleOpens.java
//   cratonvm --java-home <jdk25> [--nojit] -cp out L5W45LayerModuleOpens
//
// Expected HotSpot 25.0.3 output (`java` and `-Xint`, identical; compare
// verbatim):
//   exported-private=java.lang.reflect.InaccessibleObjectException
//   exported-public=ok
//   opened=ok
//   duplicate=java.lang.LinkageError: loader jdk.internal.loader.Loader attempted duplicate class definition for p45b.Hello. (p45b.Hello is in module m45b of loader jdk.internal.loader.Loader, parent loader 'app')
//   open-qualified=ok java.lang.reflect.InaccessibleObjectException
//   controller-open=java.lang.reflect.InaccessibleObjectException ok
//   ste=m45a null m45a/p45a.Hello.boom(L5W45LayerModuleOpens.java:258) | m45e 2.1 m45e@2.1/p45e.Hello.boom(L5W45LayerModuleOpens.java:258)
//
// On the base `69568bea6` (from the code): the VM's gate
// (`check_deep_reflection_access`) answers a layer target from the module's
// EXPORT record (wave 44), so a private member of an exported package is
// admitted -- `exported-private=ok` and `controller-open=ok ok`; and the
// duplicate-definition message and a stack trace element read
// `module_name_of_class`, `None` for a layer class -- `duplicate=... (p45b.Hello
// is in unnamed module of loader ...)` and
// `ste=null null p45a.Hello.boom(L5W45LayerModuleOpens.java:258) | null null
// p45e.Hello.boom(L5W45LayerModuleOpens.java:258)`. The other rows match on
// the base.
//
// Positive control (`--jdk-only`): `CRATONVM_DBG=access` prints
// `[LAYER-MODULE] setAccessible opens p45a of module m45a to unnamed module
// -> refused` for `exported-private`, `... opens p45b of module m45b ... ->
// open` for `opened`, and `... opens p45c of module m45c to module m45d ->
// open` for `open-qualified`; no `setAccessible opens` line exists on the
// base. `--compatible` keeps the base's answers by design (no layer module
// is recorded there; the layer classes do not load, as in
// `L5W43UserLayerModule`).

import java.io.*;
import java.lang.invoke.MethodHandles;
import java.lang.module.*;
import java.lang.reflect.Field;
import java.lang.reflect.InaccessibleObjectException;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.net.URI;
import java.nio.ByteBuffer;
import java.util.*;

public class L5W45LayerModuleOpens {
    static final String P = "L5W45LayerModuleOpens$";
    static final ClassLoader APP = L5W45LayerModuleOpens.class.getClassLoader();

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

    /** The bytes of nested class `simple` renamed to `pkg/simple`. */
    static byte[] member(String simple, String pkg) throws IOException {
        return rename(own(P + simple), Map.of(P + simple, pkg + "/" + simple));
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

        Configuration configuration(String... roots) {
            return ModuleLayer.boot().configuration().resolve(finder(), ModuleFinder.of(), Set.of(roots));
        }
    }

    static ModuleLayer layer(Mods mods, String... roots) {
        return ModuleLayer.boot().defineModulesWithOneLoader(mods.configuration(roots), APP);
    }

    /** setAccessible(true) on `member` (a field, or a method when it ends in `()`) of `cn` of `loader`. */
    static String open(ClassLoader loader, String cn, String member) throws Exception {
        Class<?> c = Class.forName(cn, false, loader);
        java.lang.reflect.AccessibleObject ao = member.endsWith("()")
                ? c.getMethod(member.substring(0, member.length() - 2))
                : c.getDeclaredField(member);
        try {
            ao.setAccessible(true);
            return "ok";
        } catch (InaccessibleObjectException e) {
            return e.getClass().getName();
        }
    }

    static String ste(ClassLoader loader, String cn) throws Exception {
        try {
            Class.forName(cn, true, loader).getMethod("boom").invoke(null);
            return "no exception";
        } catch (InvocationTargetException e) {
            StackTraceElement top = e.getCause().getStackTrace()[0];
            return top.getModuleName() + " " + top.getModuleVersion() + " " + top;
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
        Mods a = new Mods().add(ModuleDescriptor.newModule("m45a").packages(Set.of("p45a")).exports("p45a").build(),
                Map.of("p45a/Hello.class", member("Hello", "p45a")));
        ClassLoader la = layer(a, "m45a").findLoader("m45a");
        row("exported-private", () -> open(la, "p45a.Hello", "secret"));
        row("exported-public", () -> open(la, "p45a.Hello", "hi()"));
        Mods b = new Mods().add(ModuleDescriptor.newModule("m45b").packages(Set.of("p45b")).opens("p45b").build(),
                Map.of("p45b/Hello.class", member("Hello", "p45b")));
        ClassLoader lb = layer(b, "m45b").findLoader("m45b");
        row("opened", () -> open(lb, "p45b.Hello", "secret"));
        row("duplicate", () -> {
            MethodHandles.Lookup l = MethodHandles.privateLookupIn(Class.forName("p45b.Hello", false, lb),
                    MethodHandles.lookup());
            try {
                l.defineClass(member("Hello", "p45b"));
                return "defined";
            } catch (LinkageError e) {
                return e.getClass().getName() + ": " + e.getMessage().replaceAll(" @(0x)?[0-9a-f]+", "");
            }
        });
        row("open-qualified", () -> {
            Mods c = new Mods()
                    .add(ModuleDescriptor.newModule("m45c").packages(Set.of("p45c"))
                                    .opens(Set.of(), "p45c", Set.of("m45d")).build(),
                            Map.of("p45c/Hello.class", member("Hello", "p45c")))
                    .add(ModuleDescriptor.newModule("m45d").packages(Set.of("p45d")).requires("m45c").exports("p45d").build(),
                            Map.of("p45d/Opener.class", member("Opener", "p45d")));
            ClassLoader l = layer(c, "m45d").findLoader("m45d");
            String inModule = (String) l.loadClass("p45d.Opener").getMethod("open", String.class, String.class)
                    .invoke(null, "p45c.Hello", "secret");
            return inModule + " " + open(l, "p45c.Hello", "secret");
        });
        row("controller-open", () -> {
            Mods f = new Mods().add(ModuleDescriptor.newModule("m45f").packages(Set.of("p45f")).exports("p45f").build(),
                    Map.of("p45f/Hello.class", member("Hello", "p45f")));
            ModuleLayer.Controller ctl = ModuleLayer.defineModulesWithOneLoader(f.configuration("m45f"),
                    List.of(ModuleLayer.boot()), APP);
            ClassLoader l = ctl.layer().findLoader("m45f");
            String before = open(l, "p45f.Hello", "secret");
            ctl.addOpens(ctl.layer().findModule("m45f").get(), "p45f", L5W45LayerModuleOpens.class.getModule());
            return before + " " + open(l, "p45f.Hello", "secret");
        });
        row("ste", () -> {
            Mods e = new Mods().add(ModuleDescriptor.newModule("m45e").version("2.1").packages(Set.of("p45e")).exports("p45e").build(),
                    Map.of("p45e/Hello.class", member("Hello", "p45e")));
            return ste(la, "p45a.Hello") + " | " + ste(layer(e, "m45e").findLoader("m45e"), "p45e.Hello");
        });
    }

    public static class Hello {
        private static String secret = "s";

        public static String hi() { return "hi"; }

        public static String boom() {
            throw new IllegalStateException("boom");
        }
    }

    /** Renamed into p45d (module m45d): setAccessible from a class of that module. */
    public static class Opener {
        public static String open(String cn, String field) throws Exception {
            Field f = Class.forName(cn, false, Opener.class.getClassLoader()).getDeclaredField(field);
            try {
                f.setAccessible(true);
                return "ok";
            } catch (InaccessibleObjectException e) {
                return e.getClass().getName();
            }
        }
    }
}
