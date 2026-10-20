// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 26, lane L3
// (docs/internal/fixed-bugs/interpreter-L3-a-rejected-redefinition-returns-normally-FIXED-20260928.md):
// what redefineClasses / retransformClasses do when the new class file is one
// the VM must refuse (here: it adds a method, which JVMTI's redefinition
// forbids).
//
// Both calls must throw, and the class must keep its old body. The new class
// file is Tsrget's, renamed to Target in place (same name length); Tsrget
// declares one method more than Target.
//
// HotSpot 25 prints (agent; -Xint too; run 2026-09-28):
//     redefine threw java.lang.UnsupportedOperationException value=old
//     retransform threw java.lang.UnsupportedOperationException value=old
// (both messages: "class redefinition failed: attempted to add a method").
// CratonVM (read from the code, not run): both natives log the refusal
// (`tracing::warn!`) and return normally:
//     redefine returned value=old
//     retransform returned value=old
// Since wave 27 (lane L3) both natives throw the refusal: HotSpot's lines.
//
// SETUP: a jar whose manifest has
//     Premain-Class: L3W26RejectedRedefinition$Agent
//     Can-Redefine-Classes: true
//     Can-Retransform-Classes: true
// containing L3W26RejectedRedefinition*.class, then
//     java|cratonvm --java-home <jdk25> [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar L3W26RejectedRedefinition
// Without the agent both VMs print "no agent".
import java.io.InputStream;
import java.lang.instrument.ClassDefinition;
import java.lang.instrument.ClassFileTransformer;
import java.lang.instrument.Instrumentation;
import java.security.ProtectionDomain;

public class L3W26RejectedRedefinition {
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

    /** Same shape as Target plus one method; renamed to Target below. */
    public static class Tsrget {
        public static String value() {
            return "new";
        }

        public static String extra() {
            return "extra";
        }
    }

    /** Tsrget's class file, declaring itself Target (same name length). */
    static byte[] donorAsTarget() throws Exception {
        byte[] b;
        try (InputStream in = L3W26RejectedRedefinition.class
                .getResourceAsStream("L3W26RejectedRedefinition$Tsrget.class")) {
            b = in.readAllBytes();
        }
        String from = "L3W26RejectedRedefinition$Tsrget";
        for (int i = 0; i + from.length() <= b.length; i++) {
            boolean match = true;
            for (int k = 0; k < from.length() && match; k++) {
                match = b[i + k] == (byte) from.charAt(k);
            }
            if (match) {
                b[i + from.length() - 5] = 'a';
            }
        }
        return b;
    }

    static final class AddMethod implements ClassFileTransformer {
        final byte[] replacement;

        AddMethod(byte[] replacement) {
            this.replacement = replacement;
        }

        @Override
        public byte[] transform(ClassLoader loader, String className, Class<?> redefined,
                ProtectionDomain domain, byte[] bytes) {
            if (redefined == null || !"L3W26RejectedRedefinition$Target".equals(className)) {
                return null;
            }
            return replacement.clone();
        }
    }

    public static void main(String[] args) throws Exception {
        Instrumentation i = inst;
        if (i == null || !i.isRedefineClassesSupported() || !i.isRetransformClassesSupported()) {
            System.out.println("no agent");
            return;
        }
        byte[] bytes = donorAsTarget();
        String first = Target.value();
        String outcome;
        try {
            i.redefineClasses(new ClassDefinition(Target.class, bytes));
            outcome = "returned";
        } catch (Throwable t) {
            outcome = "threw " + t.getClass().getName();
        }
        System.out.println("redefine " + outcome + " value=" + Target.value());
        i.addTransformer(new AddMethod(bytes), true);
        try {
            i.retransformClasses(Target.class);
            outcome = "returned";
        } catch (Throwable t) {
            outcome = "threw " + t.getClass().getName();
        }
        System.out.println("retransform " + outcome + " value=" + Target.value());
        if (!"old".equals(first)) {
            System.out.println("unexpected first=" + first);
        }
    }
}
