// Interpreter round i1, wave 29, lane L5 -- JaCoCo's agent start-up shape
// without JaCoCo: a child-first loader, made in `premain`, defines an agent
// class AND its package-private nested class, whose application-loader copies
// already exist; the class then calls the nested class and injects a
// `java.lang` class through a private lookup on `Object`.
//
// JaCoCo 0.8.x `PreMain.createRuntime`: `AgentModule` builds a `ClassLoader`
// subclass whose `loadClass` DEFINES every name in a scope set (the class and
// its `getDeclaredClasses()`, read from the application copy, which loads
// them) and delegates the rest; `Instrumentation.redefineModule` opens
// `java.base/java.lang` to that loader's unnamed module; the loader's
// `InjectedClassRuntime` calls its nested `$Lookup` (package-private,
// `invokestatic`), whose `privateLookupIn(Object.class, ...).defineClass`
// defines `java.lang.$JaCoCo`. On HotSpot both classes are the child loader's.
//
// CratonVM `--jdk-only` before wave 29 (`i28-L5-an-agent-jars-classes-are-split-
// between-two-loader-identities`): `resolve_method_metadata` judged the
// `invokestatic Helper.value` owner against the APPLICATION copy of `Helper`
// (`owner_in_referencing_namespace` -> `find_class_by_name_for_class`'s
// "recorded parents, then the built-in chain" guess) before the door asked the
// child loader, so the call failed with `IllegalAccessError`
// (`CRATONVM_DBG=access`: `DENY accessor=L5W29ChildFirstAgentLoader$Runtime
// accessor.loader_id=UserDefined(n) target=L5W29ChildFirstAgentLoader$Runtime$Helper
// target.loader_id=Application`). And a `Lookup.defineClass` of a `java.lang`
// class reached `defineClass0` with the null loader, mapped to `Application`,
// and was refused as a prohibited package.
//
// Setup: one jar holds every class of this file; manifest
// `Premain-Class: L5W29ChildFirstAgentLoader$Agent`.
//   javac -d out L5W29ChildFirstAgentLoader.java
//   jar cfm p.jar manifest.txt -C out .
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -javaagent:p.jar -cp p.jar L5W29ChildFirstAgentLoader
//
// Expected HotSpot 25 output (compare verbatim):
//   app helper call=7
//   runtime loader=child
//   helper call=7
//   helper loader same=true
//   open java.lang=true
//   inject java.lang.L5W29Injected loader=null data=ok
//
// CratonVM before wave 29 (from the code and the JaCoCo trace, not run):
// `helper call=java.lang.IllegalAccessError`, and the `inject` row
// `inject call=java.lang.SecurityException` (or an `IllegalArgumentException`)
// for the prohibited package. That `inject` row is a genuine bug in
// `--compatible` too (the `lookup_define.rs` `Lookup.defineClass` native,
// the JaCoCo page's `--compatible` half), fixed there in wave 29 as well; the
// other rows' `--compatible` output is not predicted (its owner-access checks
// are counted, not enforced).

import java.io.ByteArrayOutputStream;
import java.io.DataOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.lang.instrument.Instrumentation;
import java.lang.invoke.MethodHandles;
import java.util.ArrayList;
import java.util.HashSet;
import java.util.List;
import java.util.Map;
import java.util.Set;
import java.util.function.Function;

public class L5W29ChildFirstAgentLoader {
    static final List<String> rows = new ArrayList<>();

    /** JaCoCo's `AgentModule$1`: defines every name in its scope, on every request. */
    static final class ChildFirst extends ClassLoader {
        final Set<String> scope = new HashSet<>();

        ChildFirst() {
            super(ClassLoader.getSystemClassLoader());
        }

        void addToScopeWithInnerClasses(Class<?> c) {
            scope.add(c.getName());
            for (Class<?> inner : c.getDeclaredClasses()) {
                addToScopeWithInnerClasses(inner);
            }
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            if (!scope.contains(name)) {
                return super.loadClass(name, resolve);
            }
            byte[] bytes;
            try (InputStream in = getResourceAsStream(name.replace('.', '/') + ".class")) {
                bytes = in.readAllBytes();
            } catch (IOException e) {
                throw new RuntimeException(e);
            }
            return defineClass(name, bytes, 0, bytes.length,
                    L5W29ChildFirstAgentLoader.class.getProtectionDomain());
        }
    }

    public static class Agent {
        public static void premain(String args, Instrumentation inst) {
            ChildFirst child = new ChildFirst();
            // The application copies of both classes exist first, as in JaCoCo.
            row(() -> "app helper call=" + Runtime.Helper.value());
            child.addToScopeWithInnerClasses(Runtime.class);
            row(() -> {
                Module base = Object.class.getModule();
                inst.redefineModule(base, Set.of(), Map.of(),
                        Map.of("java.lang", Set.of(child.getUnnamedModule())), Set.of(), Map.of());
                return null;
            });
            Function<String, String> runtime;
            try {
                Class<?> c = child.loadClass(Runtime.class.getName());
                rows.add("runtime loader="
                        + (c.getClassLoader() == child ? "child" : String.valueOf(c.getClassLoader())));
                @SuppressWarnings("unchecked")
                Function<String, String> f =
                        (Function<String, String>) c.getConstructor().newInstance();
                runtime = f;
            } catch (Throwable t) {
                rows.add("runtime threw " + t.getClass().getName());
                return;
            }
            row(() -> runtime.apply("helper"));
            row(() -> runtime.apply("helper-loader"));
            row(() -> "open java.lang="
                    + Object.class.getModule().isOpen("java.lang", child.getUnnamedModule()));
            row(() -> runtime.apply("inject"));
        }
    }

    interface Row {
        String get() throws Throwable;
    }

    static void row(Row r) {
        try {
            String s = r.get();
            if (s != null) {
                rows.add(s);
            }
        } catch (Throwable t) {
            rows.add("row threw " + t.getClass().getName());
        }
    }

    public static void main(String[] args) {
        for (String r : rows) {
            System.out.println(r);
        }
    }

    /** JaCoCo's `InjectedClassRuntime`: defined by the child loader. */
    public static class Runtime implements Function<String, String> {
        public Runtime() {
        }

        @Override
        public String apply(String what) {
            try {
                switch (what) {
                    case "helper":
                        // JaCoCo's `invokestatic InjectedClassRuntime$Lookup.lookup`.
                        return "helper call=" + Helper.value();
                    case "helper-loader":
                        return "helper loader same="
                                + (Helper.class.getClassLoader() == getClass().getClassLoader());
                    default:
                        // JaCoCo's `$Lookup.privateLookupIn(Object.class, ...).defineClass`.
                        MethodHandles.Lookup lookup =
                                MethodHandles.privateLookupIn(Object.class, MethodHandles.lookup());
                        Class<?> c = lookup.defineClass(injected());
                        c.getField("data").set(null, "ok");
                        return "inject " + c.getName() + " loader=" + c.getClassLoader()
                                + " data=" + c.getField("data").get(null);
                }
            } catch (Throwable t) {
                return what + " call=" + t.getClass().getName();
            }
        }

        /** `public class java.lang.L5W29Injected { public static Object data; }`, class file 49. */
        static byte[] injected() throws IOException {
            ByteArrayOutputStream bytes = new ByteArrayOutputStream();
            DataOutputStream out = new DataOutputStream(bytes);
            out.writeInt(0xCAFEBABE);
            out.writeShort(0);
            out.writeShort(49);
            out.writeShort(7);
            out.writeByte(1);
            out.writeUTF("java/lang/L5W29Injected");
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

        /** JaCoCo's `InjectedClassRuntime$Lookup`: package-private, nested. */
        static class Helper {
            static int value() {
                return 7;
            }
        }
    }
}
