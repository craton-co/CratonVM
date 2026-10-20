// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 48, lane L6: `Lookup.in` between classes of two
// named modules of ONE user `ModuleLayer` loader
// (`i44-L5-proposal-a-classs-module-is-an-identity-every-reader-sees`, the
// `lk_module_key` reader). `Lookup.in` keyed a class's module by
// `module_name_of_class`, which names no layer module, so every class of the
// layer's loader was "its loader's unnamed module": a move from m48a to m48b
// was a same-module move (modes 17, no previous lookup class) and a move to a
// module m48a does not read kept PUBLIC. Under
// `CRATONVM_LOOKUP_IN_MODULE_BY_IDENTITY` (wave 48) the questions are asked of
// the module registry by identity (`module_access`).
//
// Modules (in memory, one loader, `defineModulesWithOneLoader`):
//   m48a  packages p48a, p48q; requires m48b; exports both
//   m48b  package p48b; exports it
//   m48c  package p48c; exports it; not read by m48a
//
// Rows: the lookup modes and previous lookup class of
//   same-module     p48a.Hello's lookup().in(p48q.Mate)  (same module, other package)
//   read-module     .in(p48b.Other)                      (m48a reads m48b)
//   unread-module   .in(p48c.Far)                        (m48a does not read m48c)
//   back            .in(p48b.Other).in(p48a.Hello)       (m48b does not read m48a)
//
// HotSpot 25.0.3 prints (measured; the same with -Xint):
//   same-module=17 null
//   read-module=1 p48a.Hello
//   unread-module=0 p48a.Hello
//   back=0 p48b.Other
//
// Before wave 48 (from the code): `read-module=17 null`, `unread-module=17
// null`, `back=17 null` under `--jdk-only`; `--compatible` loaded no layer class
// (every row a `ClassNotFoundException`) until the same wave recorded layer
// modules there too. Positive control: `CRATONVM_DBG=access` prints
// `[ACCESS-DBG] Lookup.in by identity: p48a/Hello -> p48b/Other Same=false`.
//
//     javac -d out L6W48LookupInLayerModules.java
//     java|cratonvm [--compatible] [--nojit] -cp out L6W48LookupInLayerModules

import java.io.*;
import java.lang.invoke.MethodHandles;
import java.lang.module.*;
import java.net.URI;
import java.nio.ByteBuffer;
import java.util.*;

public class L6W48LookupInLayerModules {
    static final String P = "L6W48LookupInLayerModules$";
    static final ClassLoader APP = L6W48LookupInLayerModules.class.getClassLoader();

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
                .add(ModuleDescriptor.newModule("m48a").packages(Set.of("p48a", "p48q")).requires("m48b")
                                .exports("p48a").exports("p48q").build(),
                        Map.of("p48a/Hello.class", member("Hello", "p48a"), "p48q/Mate.class", member("Mate", "p48q")))
                .add(ModuleDescriptor.newModule("m48b").packages(Set.of("p48b")).exports("p48b").build(),
                        Map.of("p48b/Other.class", member("Other", "p48b")))
                .add(ModuleDescriptor.newModule("m48c").packages(Set.of("p48c")).exports("p48c").build(),
                        Map.of("p48c/Far.class", member("Far", "p48c")));
        ModuleLayer layer = ModuleLayer.boot().defineModulesWithOneLoader(mods.configuration("m48a", "m48c"), APP);
        ClassLoader l = layer.findLoader("m48a");
        Row base = () -> Class.forName("p48a.Hello", true, l).getMethod("lk").invoke(null);
        row("same-module", () -> show(((MethodHandles.Lookup) base.run()).in(Class.forName("p48q.Mate", false, l))));
        row("read-module", () -> show(((MethodHandles.Lookup) base.run()).in(Class.forName("p48b.Other", false, l))));
        row("unread-module", () -> show(((MethodHandles.Lookup) base.run()).in(Class.forName("p48c.Far", false, l))));
        row("back", () -> show(((MethodHandles.Lookup) base.run()).in(Class.forName("p48b.Other", false, l))
                .in(Class.forName("p48a.Hello", false, l))));
    }

    public static class Hello {
        public static MethodHandles.Lookup lk() {
            return MethodHandles.lookup();
        }
    }

    public static class Mate {
    }

    public static class Other {
    }

    public static class Far {
    }
}
