// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 94, lane L3 -- the named-module half of the
// caller-sensitive matrix (`L3W94CallerSensitiveMatrix`; stage 3 of
// docs/internal/fixed-bugs/interpreter-i49-L3-proposal-one-caller-class-rule-over-the-walkers-capture-DONE-20261128.md):
// a caller in a NAMED module, `m94` (a user `ModuleLayer` over the boot
// layer, one loader whose parent is the application loader; `exports m94p`,
// opens nothing), against the application (unnamed-module) caller asking the
// same doors of `m94`'s members. Every module is in memory, its class this
// file's nested `Caller` renamed to `m94p.Caller`, as in
// `tools/probes/interp/L5/L5W45LayerModuleOpens.java`.
//
// Rows:
//   named-open-util-direct   m94p.Caller: setAccessible(true) on ArrayList.size:
//                            refused, the message naming `module m94`;
//   named-open-util-reflect  the same through Method.invoke made by Caller;
//   named-open-own           Caller opens its own private field: ok;
//   named-get-app-private    Caller: Field.get of a private field of an
//                            application class, no setAccessible: refused,
//                            the message naming `(in module m94)`;
//   named-get-app-reflect    the same through Method.invoke made by Caller;
//   named-lookup             MethodHandles.lookup().lookupClass() in Caller;
//   named-res-own            Caller.class.getResource("data.txt") asked by
//                            Caller (m94p/data.txt, package not open): found;
//   app-res-named            the same resource asked by this class: null
//                            (`Class.getResource`'s `isOpenToCaller`);
//   app-res-named-reflect    the same through Method.invoke: null;
//   app-open-named-private   this class: setAccessible(true) on Caller's private
//                            static field: refused (m94 does not open m94p);
//   app-open-named-public    on Caller's public static method: ok (exported,
//                            public class, public member).
//
// HOTSPOT_EXPECTED_BEGIN (derived from the JDK 25 sources; to be measured)
// named-open-util-direct: java.lang.reflect.InaccessibleObjectException: Unable to make field private int java.util.ArrayList.size accessible: module java.base does not "opens java.util" to module m94
// named-open-util-reflect: java.lang.reflect.InaccessibleObjectException: Unable to make field private int java.util.ArrayList.size accessible: module java.base does not "opens java.util" to module m94
// named-open-own: ok
// named-get-app-private: java.lang.IllegalAccessException: class m94p.Caller (in module m94) cannot access a member of class L3W94NamedOther with modifiers "private"
// named-get-app-reflect: java.lang.IllegalAccessException: class m94p.Caller$$InjectedInvoker/0x (in module m94) cannot access a member of class L3W94NamedOther with modifiers "private"
// named-lookup: m94p.Caller
// named-res-own: found
// app-res-named: null
// app-res-named-reflect: null
// app-open-named-private: java.lang.reflect.InaccessibleObjectException: Unable to make field private static java.lang.String m94p.Caller.secret accessible: module m94 does not "opens m94p" to unnamed module
// app-open-named-public: ok
// HOTSPOT_EXPECTED_END
//
// `--compatible` may not load layer classes at all (see `L5W45LayerModuleOpens`'s
// notes); its rows then read the failure, which this probe records rather than
// asserts. The rows a door-native caller change can move are the named-* and
// app-* refusals: each names its caller's module.
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W94CallerSensitiveNamed`,
// and each cratonvm run again with `CRATONVM_REFLECTION_CS_CALLER_RULE=1`.
import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import java.io.DataInputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.lang.invoke.MethodHandles;
import java.lang.module.Configuration;
import java.lang.module.ModuleDescriptor;
import java.lang.module.ModuleFinder;
import java.lang.module.ModuleReader;
import java.lang.module.ModuleReference;
import java.lang.reflect.Field;
import java.lang.reflect.InvocationTargetException;
import java.lang.reflect.Method;
import java.net.URI;
import java.net.URL;
import java.nio.ByteBuffer;
import java.nio.charset.StandardCharsets;
import java.util.HashSet;
import java.util.Map;
import java.util.Optional;
import java.util.Set;

public class L3W94CallerSensitiveNamed {
    static final String OUTER = "L3W94CallerSensitiveNamed$";

    interface Call { Object run() throws Throwable; }

    static String norm(String s) {
        if (s == null) {
            return "null";
        }
        return s.replaceAll(" @[0-9a-f]+", "").replaceAll("/0x[0-9a-f]+", "/0x");
    }

    static void row(String label, Call call) {
        String out;
        try {
            out = norm(String.valueOf(call.run()));
        } catch (Throwable t) {
            Throwable c = t;
            while (c instanceof InvocationTargetException && c.getCause() != null) {
                c = c.getCause();
            }
            out = c.getClass().getName() + ": " + norm(c.getMessage());
        }
        System.out.println(label + ": " + out);
    }

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

    static byte[] own(String n) throws IOException {
        try (InputStream in = ClassLoader.getSystemResourceAsStream(n + ".class")) {
            return in.readAllBytes();
        }
    }

    /** One in-memory module: its files by resource name; `find` answers a URI so `getResource` has a URL. */
    static ModuleFinder finder(ModuleDescriptor d, Map<String, byte[]> files) {
        ModuleReference ref = new ModuleReference(d, null) {
            public ModuleReader open() {
                return new ModuleReader() {
                    public Optional<URI> find(String n) {
                        return files.containsKey(n) ? Optional.of(URI.create("file:///m94/" + n)) : Optional.empty();
                    }
                    public Optional<ByteBuffer> read(String n) {
                        byte[] b = files.get(n);
                        return b == null ? Optional.empty() : Optional.of(ByteBuffer.wrap(b));
                    }
                    public Optional<InputStream> open(String n) {
                        byte[] b = files.get(n);
                        return b == null ? Optional.empty() : Optional.of(new ByteArrayInputStream(b));
                    }
                    public java.util.stream.Stream<String> list() { return files.keySet().stream(); }
                    public void close() { }
                };
            }
        };
        return new ModuleFinder() {
            public Optional<ModuleReference> find(String n) {
                return n.equals(d.name()) ? Optional.of(ref) : Optional.empty();
            }
            public Set<ModuleReference> findAll() { return new HashSet<>(Set.of(ref)); }
        };
    }

    public static void main(String[] args) throws Throwable {
        ModuleDescriptor d = ModuleDescriptor.newModule("m94").packages(Set.of("m94p")).exports("m94p").build();
        Map<String, byte[]> files = Map.of(
                "m94p/Caller.class", rename(own(OUTER + "Caller"), OUTER + "Caller", "m94p/Caller"),
                "m94p/data.txt", "d".getBytes(StandardCharsets.US_ASCII));
        Configuration cf = ModuleLayer.boot().configuration().resolve(finder(d, files), ModuleFinder.of(), Set.of("m94"));
        ModuleLayer layer = ModuleLayer.boot().defineModulesWithOneLoader(cf, L3W94CallerSensitiveNamed.class.getClassLoader());
        Class<?> caller = Class.forName("m94p.Caller", true, layer.findLoader("m94"));
        Field other = L3W94NamedOther.class.getDeclaredField("secret");
        Object o = new L3W94NamedOther();
        Method classRes = Class.class.getMethod("getResource", String.class);

        row("named-open-util-direct", () -> caller.getMethod("openUtil").invoke(null));
        row("named-open-util-reflect", () -> caller.getMethod("openUtilReflective").invoke(null));
        row("named-open-own", () -> caller.getMethod("openOwn").invoke(null));
        row("named-get-app-private", () -> caller.getMethod("getApp", Field.class, Object.class).invoke(null, other, o));
        row("named-get-app-reflect", () -> caller.getMethod("getAppReflective", Field.class, Object.class).invoke(null, other, o));
        row("named-lookup", () -> caller.getMethod("lookupName").invoke(null));
        row("named-res-own", () -> caller.getMethod("res", String.class).invoke(null, "data.txt"));
        row("app-res-named", () -> { URL u = caller.getResource("data.txt"); return u == null ? "null" : "found"; });
        row("app-res-named-reflect", () -> { URL u = (URL) classRes.invoke(caller, "data.txt"); return u == null ? "null" : "found"; });
        row("app-open-named-private", () -> { caller.getDeclaredField("secret").setAccessible(true); return "ok"; });
        row("app-open-named-public", () -> { caller.getMethod("openOwn").setAccessible(true); return "ok"; });
    }

    /** Renamed `m94p.Caller` (module m94). No lambdas: its bytes stand alone. */
    public static class Caller {
        private static String secret = "s";

        public static String openUtil() throws Exception {
            java.util.ArrayList.class.getDeclaredField("size").setAccessible(true);
            return "ok";
        }

        public static String openUtilReflective() throws Exception {
            Field.class.getMethod("setAccessible", boolean.class)
                    .invoke(java.util.ArrayList.class.getDeclaredField("size"), true);
            return "ok";
        }

        public static String openOwn() throws Exception {
            Caller.class.getDeclaredField("secret").setAccessible(true);
            return "ok";
        }

        public static Object getApp(Field f, Object o) throws Exception {
            return f.get(o);
        }

        public static Object getAppReflective(Field f, Object o) throws Exception {
            return Field.class.getMethod("get", Object.class).invoke(f, o);
        }

        public static String lookupName() {
            return MethodHandles.lookup().lookupClass().getName();
        }

        public static String res(String name) {
            return Caller.class.getResource(name) == null ? "null" : "found";
        }
    }
}

/** An application class with a private field (package-private, not a nestmate). */
class L3W94NamedOther {
    private int secret = 7;
}
