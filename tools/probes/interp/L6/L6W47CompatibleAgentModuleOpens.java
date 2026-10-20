// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 47, lane L6: JaCoCo 0.8.15's agent sequence and
// the protection domains a `ClassFileTransformer` is handed, under
// `--compatible` (held to HotSpot since wave 46; `--jdk-only` answered these
// rows already):
//
//   1. `Instrumentation.redefineModule(java.base, …, extraOpens = {java.lang:
//      {this unnamed module}})`, then `MethodHandles.privateLookupIn(Object.class,
//      lookup())` and `Lookup.defineClass` of a `java.lang` class
//      (`docs/internal/fixed-bugs/interpreter-L6-module-opens-to-an-unnamed-module-and-java-lang-lookup-defines-FIXED-20261011.md`);
//   2. a class a USER loader defines through `ClassLoader.defineClass(…, pd)`,
//      and one `Lookup.defineClass` defines through an application lookup
//      (`docs/internal/fixed-bugs/interpreter-L6-transformers-are-handed-a-null-protection-domain-FIXED-20261011.md`).
//
// CratonVM `--compatible` before wave 47 (from the code and the wave-46 host
// run of `L5/L5W46RedefineModuleOpensToAgent`): `privateLookupIn` asked no
// module question (`before=java.lang.Object`, `lookup-modes=31`), the opening
// to an unnamed module was recorded as granting nobody (`open-after=false`),
// the `java.lang` define landed in the application loader (`inject-loader=`
// the app loader, `boot-forName=java.lang.ClassNotFoundException`), the
// `Lookup.defineClass` define was nameless and never offered to a transformer
// (`inject-offered=not offered`, `app-lookup-offered=not offered`), and a
// user-loader define handed the transformer `null`
// (`user-offered=null`, `user-offered-is-define-pd=false`,
// `user-class-pd-is-define-pd=false`).
//
// HotSpot 25.0.3 prints (agent; the same with -Xint; measured):
//     before=java.lang.IllegalAccessException
//     open-after=true
//     open-other=false
//     lookup=java.lang.Object
//     lookup-modes=15
//     inject=java.lang.L6W47Injected
//     inject-loader=null
//     inject-module=java.base
//     boot-forName-same=true
//     inject-offered=null
//     second-define=java.lang.LinkageError
//     user-offered=location
//     user-offered-is-define-pd=true
//     user-class-pd-is-define-pd=true
//     app-lookup-offered=location
//     app-lookup-offered-is-lookup-pd=true
//     app-lookup-class-pd-is-lookup-pd=true
//
// Positive controls: `CRATONVM_DBG_DEFINE=1` prints
// `[DEFINE-DBG] Lookup.defineClass java/lang/L6W47Injected in the bootstrap namespace`
// under `--compatible`; `CRATONVM_DBG=access` prints one `privateLookupIn
// refused` line (for `before`) and none for `lookup`.
//
// SETUP: a jar whose manifest has
//     Premain-Class: L6W47CompatibleAgentModuleOpens$Agent
// containing L6W47CompatibleAgentModuleOpens*.class, then
//     java|cratonvm [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar L6W47CompatibleAgentModuleOpens
// Without the agent both VMs print "no agent".
import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.lang.instrument.ClassFileTransformer;
import java.lang.instrument.Instrumentation;
import java.lang.invoke.MethodHandles;
import java.net.URI;
import java.security.CodeSource;
import java.security.ProtectionDomain;
import java.security.cert.Certificate;
import java.util.Map;
import java.util.Set;
import java.util.concurrent.ConcurrentHashMap;

public class L6W47CompatibleAgentModuleOpens {
    static volatile Instrumentation inst;
    /** What the transformer was handed, per internal class name. */
    static final Map<String, Object> OFFERED = new ConcurrentHashMap<>();
    static final Object NULL_PD = new Object();

    static final String INJECTED = "java.lang.L6W47Injected";
    static final String USER_CLASS = "L6W47CompatibleAgentModuleOpens$UserDefined";
    static final String GEN_CLASS = "L6W47CompatibleAgentModuleOpens$Gen";

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
            instrumentation.addTransformer(new ClassFileTransformer() {
                @Override
                public byte[] transform(ClassLoader loader, String name, Class<?> redefined,
                        ProtectionDomain pd, byte[] bytes) {
                    if (redefined == null && name != null
                            && (name.equals("java/lang/L6W47Injected")
                                || name.equals(USER_CLASS) || name.equals(GEN_CLASS))) {
                        OFFERED.put(name, pd == null ? NULL_PD : pd);
                    }
                    return null;
                }
            });
        }
    }

    /** Defined only by {@link UserLoader} and by `Lookup.defineClass`. */
    public static class UserDefined { }

    public static class Gen {
        public static int value() {
            return 3;
        }
    }

    static final class UserLoader extends ClassLoader {
        UserLoader() {
            super(L6W47CompatibleAgentModuleOpens.class.getClassLoader());
        }

        Class<?> define(String name, byte[] bytes, ProtectionDomain pd) {
            return defineClass(name, bytes, 0, bytes.length, pd);
        }
    }

    interface Row { Object run() throws Throwable; }

    static void row(String label, Row r) {
        try {
            System.out.println(label + "=" + r.run());
        } catch (Throwable t) {
            System.out.println(label + "=" + t.getClass().getName());
        }
    }

    static String describe(Object offered) {
        if (offered == null) {
            return "not offered";
        }
        if (offered == NULL_PD) {
            return "null";
        }
        ProtectionDomain pd = (ProtectionDomain) offered;
        return pd.getCodeSource() == null ? "no-codesource"
                : pd.getCodeSource().getLocation() == null ? "no-location" : "location";
    }

    static byte[] resource(String binaryName) throws IOException {
        try (InputStream in = L6W47CompatibleAgentModuleOpens.class
                .getResourceAsStream(binaryName + ".class")) {
            return in.readAllBytes();
        }
    }

    public static void main(String[] args) throws Throwable {
        Instrumentation i = inst;
        if (i == null) {
            System.out.println("no agent");
            return;
        }
        Module base = Object.class.getModule();
        Module mine = L6W47CompatibleAgentModuleOpens.class.getModule();
        Module other = new ClassLoader(null) { }.getUnnamedModule();

        // 1. JaCoCo's module opening and `java.lang` define.
        row("before", () -> MethodHandles.privateLookupIn(Object.class, MethodHandles.lookup())
                .lookupClass().getName());
        i.redefineModule(base, Set.of(), Map.of(), Map.of("java.lang", Set.of(mine)),
                Set.of(), Map.of());
        row("open-after", () -> base.isOpen("java.lang", mine));
        row("open-other", () -> base.isOpen("java.lang", other));
        MethodHandles.Lookup[] boot = new MethodHandles.Lookup[1];
        row("lookup", () -> {
            boot[0] = MethodHandles.privateLookupIn(Object.class, MethodHandles.lookup());
            return boot[0].lookupClass().getName();
        });
        if (boot[0] != null) {
            System.out.println("lookup-modes=" + boot[0].lookupModes());
            Class<?>[] injected = new Class<?>[1];
            row("inject", () -> {
                injected[0] = boot[0].defineClass(injected());
                return injected[0].getName();
            });
            if (injected[0] != null) {
                Class<?> c = injected[0];
                System.out.println("inject-loader=" + c.getClassLoader());
                System.out.println("inject-module=" + c.getModule().getName());
                row("boot-forName-same", () -> Class.forName(INJECTED, false, null) == c);
                System.out.println("inject-offered=" + describe(OFFERED.get("java/lang/L6W47Injected")));
            }
            row("second-define", () -> boot[0].defineClass(injected()).getName());
        }

        // 2. A user loader's define, with a domain of its own.
        ProtectionDomain userPd = new ProtectionDomain(
                new CodeSource(URI.create("file:/l6w47/user/").toURL(), (Certificate[]) null), null);
        Class<?> user = new UserLoader().define(USER_CLASS, resource(USER_CLASS), userPd);
        System.out.println("user-offered=" + describe(OFFERED.get(USER_CLASS)));
        System.out.println("user-offered-is-define-pd=" + (OFFERED.get(USER_CLASS) == userPd));
        System.out.println("user-class-pd-is-define-pd=" + (user.getProtectionDomain() == userPd));

        // 3. `Lookup.defineClass` through an application lookup.
        ProtectionDomain lookupPd = L6W47CompatibleAgentModuleOpens.class.getProtectionDomain();
        Class<?> gen = MethodHandles.lookup().defineClass(resource(GEN_CLASS));
        System.out.println("app-lookup-offered=" + describe(OFFERED.get(GEN_CLASS)));
        System.out.println("app-lookup-offered-is-lookup-pd=" + (OFFERED.get(GEN_CLASS) == lookupPd));
        System.out.println("app-lookup-class-pd-is-lookup-pd=" + (gen.getProtectionDomain() == lookupPd));
    }

    /** `public class java.lang.L6W47Injected { public static Object data; }`, class file 49. */
    static byte[] injected() throws IOException {
        ByteArrayOutputStream bytes = new ByteArrayOutputStream();
        DataOutputStream out = new DataOutputStream(bytes);
        out.writeInt(0xCAFEBABE);
        out.writeShort(0);
        out.writeShort(49);
        out.writeShort(7);
        out.writeByte(1);
        out.writeUTF("java/lang/L6W47Injected");
        out.writeByte(7);
        out.writeShort(1);
        out.writeByte(1);
        out.writeUTF("java/lang/Object");
        out.writeByte(7);
        out.writeShort(3);
        out.writeByte(1);
        out.writeUTF("data");
        out.writeByte(1);
        out.writeUTF("Ljava/lang/Object;");
        out.writeShort(0x0021);
        out.writeShort(2);
        out.writeShort(4);
        out.writeShort(0);
        out.writeShort(1);
        out.writeShort(0x0009);
        out.writeShort(5);
        out.writeShort(6);
        out.writeShort(0);
        out.writeShort(0);
        out.writeShort(0);
        out.flush();
        return bytes.toByteArray();
    }
}
