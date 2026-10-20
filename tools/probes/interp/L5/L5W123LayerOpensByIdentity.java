// Interpreter round i6, wave 123, lane L5 -- the `opens` question for a class
// of a named module in a user `ModuleLayer`, asked by the modules' identities
// (`i45-L5-proposal-one-module-access-question-by-identity`, "Wave 123";
// switch `CRATONVM_MODULE_ACCESS_EXPORT_ONE_QUESTION`, default off).
//
// With the switch the `setAccessible` gate asks the VM's one question
// (`module_access`, `Open`) first and, where it has no answer (a layer
// module's opens), the target class's own `Module` (its mirror's getModule)
// `implIsExportedOrOpen(pn, callerModule, true)`, instead of finding the
// Module through the loader's package-to-module map.
//
// Rows (every module in memory, as in `L5W45LayerModuleOpens`):
//   exported-private   m123a exports p123a, no opens: setAccessible on a
//                      private field from this unnamed-module class, refused
//                      (message, hash stripped)
//   try-private        trySetAccessible on the same field: false
//   exported-public    the public method of the same class: ok
//   open-module        `open module m123b`: ok (the registry answers, no Java)
//   two-layers         two layers each define a module m123c with package
//                      p123c; the first opens p123c, the second only exports
//                      it: ok, then refused
//   qualified-other    m123d `opens p123d to m123e`; a class of m123e in the
//                      SAME layer is admitted, a class of a DIFFERENT layer's
//                      module also named m123e is refused (the target is the
//                      resolved Module, not its name)
//   controller-open    Controller.addOpens(m123f, p123f, this unnamed module):
//                      refused, then ok
//
// Run (no setup):
//   javac -d out tools/probes/interp/L5/L5W123LayerOpensByIdentity.java
//   java -cp out L5W123LayerOpensByIdentity
//
// Positive control: `CRATONVM_DBG=access` with the switch on prints
// `[LAYER-MODULE] setAccessible opens p123a of module m123a to unnamed module
// by identity (Module.implIsExportedOrOpen) -> refused`, and `... by identity
// (registry) -> open` for `open-module` (when the VM's own gate reaches it).
//
// HOTSPOT_EXPECTED_BEGIN
// exported-private=java.lang.reflect.InaccessibleObjectException: Unable to make field private static java.lang.String p123a.Hello.secret accessible: module m123a does not "opens p123a" to unnamed module
// try-private=false
// exported-public=ok
// open-module=ok
// two-layers=ok java.lang.reflect.InaccessibleObjectException
// qualified-other=ok java.lang.reflect.InaccessibleObjectException
// controller-open=java.lang.reflect.InaccessibleObjectException ok
// HOTSPOT_EXPECTED_END

import java.io.*;
import java.lang.module.*;
import java.lang.reflect.AccessibleObject;
import java.lang.reflect.Field;
import java.lang.reflect.InaccessibleObjectException;
import java.net.URI;
import java.nio.ByteBuffer;
import java.util.*;

public class L5W123LayerOpensByIdentity {
    static final String P = "L5W123LayerOpensByIdentity$";
    static final ClassLoader APP = L5W123LayerOpensByIdentity.class.getClassLoader();

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

    static byte[] member(String simple, String pkg) throws IOException {
        return rename(own(P + simple), Map.of(P + simple, pkg + "/" + simple));
    }

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

    static AccessibleObject member(ClassLoader loader, String cn, String member) throws Exception {
        Class<?> c = Class.forName(cn, false, loader);
        return member.endsWith("()")
                ? c.getMethod(member.substring(0, member.length() - 2))
                : c.getDeclaredField(member);
    }

    static String open(ClassLoader loader, String cn, String member) throws Exception {
        try {
            member(loader, cn, member).setAccessible(true);
            return "ok";
        } catch (InaccessibleObjectException e) {
            return e.getClass().getName();
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
        Mods a = new Mods().add(ModuleDescriptor.newModule("m123a").packages(Set.of("p123a")).exports("p123a").build(),
                Map.of("p123a/Hello.class", member("Hello", "p123a")));
        ClassLoader la = layer(a, "m123a").findLoader("m123a");
        row("exported-private", () -> {
            member(la, "p123a.Hello", "secret").setAccessible(true);
            return "ok";
        });
        row("try-private", () -> member(la, "p123a.Hello", "secret").trySetAccessible());
        row("exported-public", () -> open(la, "p123a.Hello", "hi()"));
        row("open-module", () -> {
            Mods b = new Mods().add(ModuleDescriptor.newOpenModule("m123b").packages(Set.of("p123b")).build(),
                    Map.of("p123b/Hello.class", member("Hello", "p123b")));
            return open(layer(b, "m123b").findLoader("m123b"), "p123b.Hello", "secret");
        });
        row("two-layers", () -> {
            Mods c1 = new Mods().add(ModuleDescriptor.newModule("m123c").packages(Set.of("p123c")).opens("p123c").build(),
                    Map.of("p123c/Hello.class", member("Hello", "p123c")));
            Mods c2 = new Mods().add(ModuleDescriptor.newModule("m123c").packages(Set.of("p123c")).exports("p123c").build(),
                    Map.of("p123c/Hello.class", member("Hello", "p123c")));
            ClassLoader l1 = layer(c1, "m123c").findLoader("m123c");
            ClassLoader l2 = layer(c2, "m123c").findLoader("m123c");
            return open(l1, "p123c.Hello", "secret") + " " + open(l2, "p123c.Hello", "secret");
        });
        row("qualified-other", () -> {
            Mods d = new Mods()
                    .add(ModuleDescriptor.newModule("m123d").packages(Set.of("p123d"))
                                    .opens(Set.of(), "p123d", Set.of("m123e")).build(),
                            Map.of("p123d/Hello.class", member("Hello", "p123d")))
                    .add(ModuleDescriptor.newModule("m123e").packages(Set.of("p123e")).requires("m123d").exports("p123e").build(),
                            Map.of("p123e/Opener.class", member("Opener", "p123e")));
            ClassLoader ld = layer(d, "m123e").findLoader("m123e");
            Class<?> target = Class.forName("p123d.Hello", false, ld);
            Mods other = new Mods().add(ModuleDescriptor.newModule("m123e").packages(Set.of("p123e")).exports("p123e").build(),
                    Map.of("p123e/Opener.class", member("Opener", "p123e")));
            ClassLoader lo = layer(other, "m123e").findLoader("m123e");
            String same = (String) ld.loadClass("p123e.Opener").getMethod("open", Class.class, String.class)
                    .invoke(null, target, "secret");
            String diff = (String) lo.loadClass("p123e.Opener").getMethod("open", Class.class, String.class)
                    .invoke(null, target, "secret");
            return same + " " + diff;
        });
        row("controller-open", () -> {
            Mods f = new Mods().add(ModuleDescriptor.newModule("m123f").packages(Set.of("p123f")).exports("p123f").build(),
                    Map.of("p123f/Hello.class", member("Hello", "p123f")));
            ModuleLayer.Controller ctl = ModuleLayer.defineModulesWithOneLoader(f.configuration("m123f"),
                    List.of(ModuleLayer.boot()), APP);
            ClassLoader l = ctl.layer().findLoader("m123f");
            String before = open(l, "p123f.Hello", "secret");
            ctl.addOpens(ctl.layer().findModule("m123f").get(), "p123f", L5W123LayerOpensByIdentity.class.getModule());
            return before + " " + open(l, "p123f.Hello", "secret");
        });
    }

    public static class Hello {
        private static String secret = "s";

        public static String hi() { return "hi"; }
    }

    /** Renamed into p123e (module m123e): setAccessible from a class of that module. */
    public static class Opener {
        public static String open(Class<?> c, String field) throws Exception {
            Field f = c.getDeclaredField(field);
            try {
                f.setAccessible(true);
                return "ok";
            } catch (InaccessibleObjectException e) {
                return e.getClass().getName();
            }
        }
    }
}
