// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 42, lane L3: the argument checks of the public
// `Instrumentation` methods, with an agent that has both capabilities.
//
// The `InstrumentationImpl` CratonVM hands an agent is a bare allocation, so
// the public methods whose Java body would read its missing
// `TransformerManager`s are registered natives (`instrument.rs`,
// `register_instrumentation_natives`), and those natives skipped the JDK
// method's checks: a `null` was ignored or answered `false` / `0`.
//
// HotSpot 25 prints (agent; the same with -Xint; measured, JDK 25.0.3; the
// null-element retransform also writes libinstrument's
// `*** java.lang.instrument ASSERTION FAILED ***` line to stderr):
//     addTransformer-null: java.lang.NullPointerException: null passed as 'transformer' in addTransformer
//     addTransformer2-null: java.lang.NullPointerException: null passed as 'transformer' in addTransformer
//     removeTransformer-null: java.lang.NullPointerException: null passed as 'transformer' in removeTransformer
//     removeTransformer-unknown: returned false
//     isModifiableClass-null: java.lang.NullPointerException: null passed as 'theClass' in isModifiableClass
//     getObjectSize-null: java.lang.NullPointerException: null passed as 'objectToSize' in getObjectSize
//     retransformClasses-null: java.lang.NullPointerException: Cannot read the array length because "classes" is null
//     retransformClasses-empty: returned null
//     retransformClasses-null-element: java.lang.NullPointerException: null
//     redefineClasses-null: java.lang.NullPointerException: null passed as 'definitions' in redefineClasses
//     redefineClasses-null-element: java.lang.NullPointerException: element of 'definitions' is null in redefineClasses
//     redefineClasses-empty: returned null
//     setNativeMethodPrefix: java.lang.UnsupportedOperationException: setNativeMethodPrefix is not supported in this environment
//     isModifiableModule-null: java.lang.NullPointerException: 'module' is null
//     appendToBootstrap-null: java.lang.NullPointerException: Cannot invoke "java.util.jar.JarFile.getName()" because "jarfile" is null
// CratonVM before wave 42 (read from the code, not run; both modes -- the
// registered natives answer in both): `addTransformer*-null` and
// `retransformClasses-null` / `-null-element` and `redefineClasses-null` /
// `-null-element` printed `returned null`, `removeTransformer-null` and
// `isModifiableClass-null` `returned false`, `getObjectSize-null`
// `returned 0`. The last three rows run the JDK's own Java and were not
// changed (`setNativeMethodPrefix` is refused because
// `isNativeMethodPrefixSupported` answers `false`, which HotSpot answers
// too without `Can-Set-Native-Method-Prefix`).
//
// SETUP: a jar whose manifest has
//     Premain-Class: L3W42InstrumentArgumentChecks$Agent
//     Can-Redefine-Classes: true
//     Can-Retransform-Classes: true
// containing L3W42InstrumentArgumentChecks*.class, then
//     java|cratonvm [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar L3W42InstrumentArgumentChecks
// Without the agent both VMs print "no agent".
import java.lang.instrument.ClassDefinition;
import java.lang.instrument.ClassFileTransformer;
import java.lang.instrument.Instrumentation;
import java.security.ProtectionDomain;

public class L3W42InstrumentArgumentChecks {
    static volatile Instrumentation inst;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    interface Call {
        Object run() throws Throwable;
    }

    static void row(String name, Call call) {
        String out;
        try {
            Object r = call.run();
            out = "returned " + r;
        } catch (Throwable t) {
            out = t.getClass().getName() + ": " + t.getMessage();
        }
        System.out.println(name + ": " + out);
    }

    static final class Nop implements ClassFileTransformer {
        @Override
        public byte[] transform(ClassLoader l, String n, Class<?> c, ProtectionDomain d, byte[] b) {
            return null;
        }
    }

    public static void main(String[] args) throws Exception {
        Instrumentation i = inst;
        if (i == null) {
            System.out.println("no agent");
            return;
        }
        row("addTransformer-null", () -> { i.addTransformer(null); return null; });
        row("addTransformer2-null", () -> { i.addTransformer(null, true); return null; });
        row("removeTransformer-null", () -> i.removeTransformer(null));
        row("removeTransformer-unknown", () -> i.removeTransformer(new Nop()));
        row("isModifiableClass-null", () -> i.isModifiableClass(null));
        row("getObjectSize-null", () -> i.getObjectSize(null));
        row("retransformClasses-null", () -> {
            i.retransformClasses((Class<?>[]) null);
            return null;
        });
        row("retransformClasses-empty", () -> { i.retransformClasses(); return null; });
        row("retransformClasses-null-element", () -> {
            i.retransformClasses(new Class<?>[] {null});
            return null;
        });
        row("redefineClasses-null", () -> {
            i.redefineClasses((ClassDefinition[]) null);
            return null;
        });
        row("redefineClasses-null-element", () -> {
            i.redefineClasses(new ClassDefinition[] {null});
            return null;
        });
        row("redefineClasses-empty", () -> { i.redefineClasses(); return null; });
        row("setNativeMethodPrefix", () -> { i.setNativeMethodPrefix(new Nop(), "p_"); return null; });
        row("isModifiableModule-null", () -> i.isModifiableModule(null));
        row("appendToBootstrap-null", () -> {
            i.appendToBootstrapClassLoaderSearch(null);
            return null;
        });
    }
}
