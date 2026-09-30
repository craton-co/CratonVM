// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 42, lane L3: an agent whose manifest grants no
// capability (no `Can-Redefine-Classes`, no `Can-Retransform-Classes`) is
// refused what it did not ask for.
//
// HotSpot's `InstrumentationImpl` answers `isRedefineClassesSupported` /
// `isRetransformClassesSupported` from the agent's manifest and refuses
// `redefineClasses`, `retransformClasses` and `addTransformer(t, true)` with
// `UnsupportedOperationException` when the capability is missing, before any
// other check. CratonVM's natives for those public methods answered `true`
// for every agent and never refused.
//
// HotSpot 25 prints (agent; the same with -Xint; measured, JDK 25.0.3):
//     isRedefineClassesSupported: returned false
//     isRetransformClassesSupported: returned false
//     isNativeMethodPrefixSupported: returned false
//     addTransformer-retransformable: java.lang.UnsupportedOperationException: adding retransformable transformers is not supported in this environment
//     addTransformer-plain: returned null
//     retransformClasses: java.lang.UnsupportedOperationException: retransformClasses is not supported in this environment
//     retransformClasses-empty: java.lang.UnsupportedOperationException: retransformClasses is not supported in this environment
//     redefineClasses: java.lang.UnsupportedOperationException: redefineClasses is not supported in this environment
//     redefineClasses-null: java.lang.UnsupportedOperationException: redefineClasses is not supported in this environment
//     isModifiableClass: returned true
//     value=old
// With `Can-Redefine-Classes: true` and `Can-Retransform-Classes: true` in
// the manifest HotSpot prints `returned true` for the first two rows,
// `returned null` for the five calls, and the same `redefineClasses-null`
// NPE as `L3W42InstrumentArgumentChecks`.
// CratonVM before wave 42 (read from the code, not run; both modes): the
// first two rows `returned true`, and every call `returned null` (the
// `redefineClasses-null` row too). Wave 42 records the manifest's answers
// on the `InstrumentationImpl` the VM builds (`agent_loader`'s
// `build_instrumentation_mirror` and the self-attach path,
// `instrument::stamp_capabilities`) and the natives read them
// (`instrument::agent_capability`).
//
// SETUP: a jar whose manifest has ONLY
//     Premain-Class: L3W42InstrumentCapabilities$Agent
// containing L3W42InstrumentCapabilities*.class, then
//     java|cratonvm [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar L3W42InstrumentCapabilities
// Without the agent both VMs print "no agent".
import java.io.InputStream;
import java.lang.instrument.ClassDefinition;
import java.lang.instrument.ClassFileTransformer;
import java.lang.instrument.Instrumentation;
import java.security.ProtectionDomain;

public class L3W42InstrumentCapabilities {
    static volatile Instrumentation inst;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    public static class Target {
        public static String value() {
            return "old";
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
        byte[] bytes;
        try (InputStream in = L3W42InstrumentCapabilities.class
                .getResourceAsStream("L3W42InstrumentCapabilities$Target.class")) {
            bytes = in.readAllBytes();
        }
        row("isRedefineClassesSupported", () -> i.isRedefineClassesSupported());
        row("isRetransformClassesSupported", () -> i.isRetransformClassesSupported());
        row("isNativeMethodPrefixSupported", () -> i.isNativeMethodPrefixSupported());
        row("addTransformer-retransformable", () -> { i.addTransformer(new Nop(), true); return null; });
        row("addTransformer-plain", () -> { i.addTransformer(new Nop()); return null; });
        row("retransformClasses", () -> { i.retransformClasses(Target.class); return null; });
        row("retransformClasses-empty", () -> { i.retransformClasses(); return null; });
        row("redefineClasses", () -> {
            i.redefineClasses(new ClassDefinition(Target.class, bytes));
            return null;
        });
        row("redefineClasses-null", () -> {
            i.redefineClasses((ClassDefinition[]) null);
            return null;
        });
        row("isModifiableClass", () -> i.isModifiableClass(Target.class));
        System.out.println("value=" + Target.value());
    }
}
