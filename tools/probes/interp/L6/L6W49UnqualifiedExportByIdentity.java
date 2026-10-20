// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 49, lane L6: "does a module export this package
// UNCONDITIONALLY" (`Module.isExported(pn)`) about a class of a user
// `ModuleLayer`, asked by identity (`ModuleQuestion::ExportAll`;
// `interpreter-L6-proposal-an-unqualified-export-question-by-identity-FIXED-20261013.md`). Two readers
// asked it by module NAME, and a layer module has none there, so its
// qualified-only package counted as exported to everyone:
//   * `Lookup.in` within one module for a lookup without `MODULE`
//     (`VerifyAccess.isClassAccessible` -> `refModule.isExported(pn)`);
//   * `Proxy`'s `nonExported` test (`ProxyBuilder.proxyClassContext`), which
//     puts the proxy of an interface of a non-exported package in
//     `com.sun.proxy.<dynamic module>` rather than `<dynamic module>`.
//
// Modules (in memory, one loader, `defineModulesWithOneLoader`):
//   m49a  packages p49a, p49q; exports p49a; exports p49q to m49f only
//   m49f  package p49f; requires m49a
//
// Rows:
//   qualified     p49a.Hello's lookup().dropLookupMode(MODULE).in(p49q.Mate)
//   unqualified   the same .in(p49a.Pal)       (control: exported to all)
//   full          lookup().in(p49q.Mate)       (control: MODULE kept)
//   proxy-q       a Proxy of p49q.Iface: in `com.sun.proxy.*`? its module named?
//   proxy-u       a Proxy of p49a.Face (control)
// The `qualified` row runs first (cold VM) and again last.
//
// HotSpot 25.0.3 prints (measured; the same with -Xint):
//   qualified=0 null
//   unqualified=1 null
//   full=17 null
//   proxy-q=true jdk.proxy
//   proxy-u=false jdk.proxy
//   qualified-again=0 null
//
// Before wave 49 (from the code): `qualified=1 null` in both modes
// (`lk_class_accessible_in_modules` read the layer class's module name,
// `None`, as "unnamed, exports everything"), and `proxy-q=false`
// (`proxy_interfaces_non_exported` likewise). Positive control:
// `CRATONVM_DBG=access` prints `[ACCESS-DBG] Lookup.in by identity: p49q/Mate
// -> p49q/Mate ExportAll=false`.
//
//     javac -d out L6W49UnqualifiedExportByIdentity.java
//     java|cratonvm [--compatible] [--nojit] -cp out L6W49UnqualifiedExportByIdentity

import java.io.*;
import java.lang.invoke.MethodHandles;
import java.lang.module.*;
import java.lang.reflect.Proxy;
import java.net.URI;
import java.nio.ByteBuffer;
import java.util.*;

public class L6W49UnqualifiedExportByIdentity {
    static final String P = "L6W49UnqualifiedExportByIdentity$";
    static final ClassLoader APP = L6W49UnqualifiedExportByIdentity.class.getClassLoader();

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

    static String show(MethodHandles.Lookup l) {
        Class<?> prev = l.previousLookupClass();
        return l.lookupModes() + " " + (prev == null ? "null" : prev.getName());
    }

    static String proxy(ClassLoader l, String iface) throws Exception {
        Class<?> i = Class.forName(iface, false, l);
        Object p = Proxy.newProxyInstance(l, new Class<?>[] {i}, (o, m, a) -> null);
        Module m = p.getClass().getModule();
        String mn = m.isNamed() ? m.getName().replaceAll("[0-9]+$", "") : "unnamed";
        return p.getClass().getName().startsWith("com.sun.proxy.") + " " + mn;
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
        Mods mods = new Mods()
                .add(ModuleDescriptor.newModule("m49a").packages(Set.of("p49a", "p49q"))
                                .exports("p49a").exports("p49q", Set.of("m49f")).build(),
                        Map.of("p49a/Hello.class", member("Hello", "p49a"),
                                "p49a/Pal.class", member("Pal", "p49a"),
                                "p49a/Face.class", member("Face", "p49a"),
                                "p49q/Mate.class", member("Mate", "p49q"),
                                "p49q/Iface.class", member("Iface", "p49q")))
                .add(ModuleDescriptor.newModule("m49f").packages(Set.of("p49f")).requires("m49a").build(),
                        Map.of());
        ModuleLayer layer = ModuleLayer.boot().defineModulesWithOneLoader(mods.configuration("m49a", "m49f"), APP);
        ClassLoader l = layer.findLoader("m49a");
        Row base = () -> Class.forName("p49a.Hello", true, l).getMethod("lk").invoke(null);
        Row noModule = () -> ((MethodHandles.Lookup) base.run()).dropLookupMode(MethodHandles.Lookup.MODULE);
        row("qualified", () -> show(((MethodHandles.Lookup) noModule.run()).in(Class.forName("p49q.Mate", false, l))));
        row("unqualified", () -> show(((MethodHandles.Lookup) noModule.run()).in(Class.forName("p49a.Pal", false, l))));
        row("full", () -> show(((MethodHandles.Lookup) base.run()).in(Class.forName("p49q.Mate", false, l))));
        row("proxy-q", () -> proxy(l, "p49q.Iface"));
        row("proxy-u", () -> proxy(l, "p49a.Face"));
        row("qualified-again", () -> show(((MethodHandles.Lookup) noModule.run()).in(Class.forName("p49q.Mate", false, l))));
    }

    public static class Hello {
        public static MethodHandles.Lookup lk() {
            return MethodHandles.lookup();
        }
    }

    public static class Pal {
    }

    public static class Mate {
    }

    public interface Iface {
        void go();
    }

    public interface Face {
        void go();
    }
}
