// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 28, lane L3
// (docs/known-issues/interpreter/i27-L3-proposal-verify-a-redefinition-before-installing-any-class-20260928.md,
// stage 1):
// a redefineClasses / retransformClasses call on two classes whose SECOND
// class fails verification changes neither: JVMTI verifies every new class
// version before it installs the first (VM_RedefineClasses::load_new_class_versions).
//
// Each donor class is renamed in place to its target (the names differ in one
// character), so the donor's bytes declare the target class. The unverifiable
// body is value()'s `ldc; areturn` with the areturn turned into ireturn.
//
// HotSpot 25 prints (agent; -Xint too):
//     redefine-second-bad: java.lang.VerifyError: null first=old second=old
//     redefine-first-bad: java.lang.VerifyError: null first=old second=old
//     retransform-second-bad: java.lang.VerifyError: null first=old second=old
//     redefine-both-good: returned first=new second=new
// CratonVM before wave 28 (read from the code: verification ran only inside
// each class's redefinition, after the earlier classes were installed):
//     redefine-second-bad: java.lang.VerifyError: null first=new second=old
//     retransform-second-bad: java.lang.VerifyError: null first=new second=old
// (the other two rows as HotSpot).
//
// SETUP: a jar whose manifest has
//     Premain-Class: L3W28VerifyBeforeInstall$Agent
//     Can-Redefine-Classes: true
//     Can-Retransform-Classes: true
// containing L3W28VerifyBeforeInstall*.class, then
//     java|cratonvm --java-home <jdk25> [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar L3W28VerifyBeforeInstall
// Without the agent both VMs print "no agent".
import java.io.InputStream;
import java.lang.instrument.ClassDefinition;
import java.lang.instrument.ClassFileTransformer;
import java.lang.instrument.Instrumentation;
import java.security.ProtectionDomain;

public class L3W28VerifyBeforeInstall {
    static volatile Instrumentation inst;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    // Targets A<n>; donors D<n> (renamed to A<n>) return "new".
    public static class A0 { public static String value() { return "old"; } }
    public static class A1 { public static String value() { return "old"; } }
    public static class A2 { public static String value() { return "old"; } }
    public static class A3 { public static String value() { return "old"; } }
    public static class A4 { public static String value() { return "old"; } }
    public static class A5 { public static String value() { return "old"; } }
    public static class A6 { public static String value() { return "old"; } }
    public static class A7 { public static String value() { return "old"; } }
    public static class D0 { public static String value() { return "new"; } }
    public static class D1 { public static String value() { return "new"; } }
    public static class D2 { public static String value() { return "new"; } }
    public static class D3 { public static String value() { return "new"; } }
    public static class D4 { public static String value() { return "new"; } }
    public static class D5 { public static String value() { return "new"; } }
    public static class D6 { public static String value() { return "new"; } }
    public static class D7 { public static String value() { return "new"; } }

    static byte[] bytesOf(String simple) throws Exception {
        try (InputStream in = L3W28VerifyBeforeInstall.class
                .getResourceAsStream("L3W28VerifyBeforeInstall$" + simple + ".class")) {
            return in.readAllBytes();
        }
    }

    /** The donor's class file, every occurrence of its name turned into the target's. */
    static byte[] donorAs(String donor, String target) throws Exception {
        byte[] b = bytesOf(donor);
        String from = "L3W28VerifyBeforeInstall$" + donor;
        String to = "L3W28VerifyBeforeInstall$" + target;
        for (int i = 0; i + from.length() <= b.length; i++) {
            boolean match = true;
            for (int k = 0; k < from.length() && match; k++) {
                match = b[i + k] == (byte) from.charAt(k);
            }
            if (match) {
                for (int k = 0; k < to.length(); k++) {
                    b[i + k] = (byte) to.charAt(k);
                }
            }
        }
        return b;
    }

    /** `ldc #n; areturn` (value()'s whole body) with the areturn turned into ireturn. */
    static byte[] unverifiable(byte[] b) {
        for (int i = 0; i + 7 < b.length; i++) {
            if (b[i] == 0 && b[i + 1] == 0 && b[i + 2] == 0 && b[i + 3] == 3
                    && b[i + 4] == (byte) 0x12 && b[i + 6] == (byte) 0xB0) {
                b[i + 6] = (byte) 0xAC;
                return b;
            }
        }
        throw new IllegalStateException("no ldc/areturn body found");
    }

    static String describe(Throwable t) {
        return t.getClass().getName() + ": " + t.getMessage();
    }

    static String value(Class<?> c) throws Exception {
        return (String) c.getMethod("value").invoke(null);
    }

    static void batch(String label, ClassDefinition first, ClassDefinition second)
            throws Exception {
        String outcome;
        try {
            inst.redefineClasses(first, second);
            outcome = "returned";
        } catch (Throwable t) {
            outcome = describe(t);
        }
        System.out.println(label + ": " + outcome + " first=" + value(first.getDefinitionClass())
                + " second=" + value(second.getDefinitionClass()));
    }

    /** Hands back a prepared class file for each of two classes when retransformed. */
    static final class Swap implements ClassFileTransformer {
        final String firstName;
        final byte[] firstBytes;
        final String secondName;
        final byte[] secondBytes;

        Swap(String firstName, byte[] firstBytes, String secondName, byte[] secondBytes) {
            this.firstName = firstName;
            this.firstBytes = firstBytes;
            this.secondName = secondName;
            this.secondBytes = secondBytes;
        }

        @Override
        public byte[] transform(ClassLoader loader, String className, Class<?> redefined,
                ProtectionDomain domain, byte[] bytes) {
            if (redefined == null) {
                return null;
            }
            if (firstName.equals(className)) {
                return firstBytes.clone();
            }
            if (secondName.equals(className)) {
                return secondBytes.clone();
            }
            return null;
        }
    }

    public static void main(String[] args) throws Exception {
        Instrumentation i = inst;
        if (i == null || !i.isRedefineClassesSupported() || !i.isRetransformClassesSupported()) {
            System.out.println("no agent");
            return;
        }
        Class<?>[] targets = {A0.class, A1.class, A2.class, A3.class, A4.class, A5.class,
                A6.class, A7.class};
        for (Class<?> c : targets) {
            value(c);
        }

        batch("redefine-second-bad",
                new ClassDefinition(A0.class, donorAs("D0", "A0")),
                new ClassDefinition(A1.class, unverifiable(donorAs("D1", "A1"))));
        batch("redefine-first-bad",
                new ClassDefinition(A2.class, unverifiable(donorAs("D2", "A2"))),
                new ClassDefinition(A3.class, donorAs("D3", "A3")));

        Swap swap = new Swap("L3W28VerifyBeforeInstall$A4", donorAs("D4", "A4"),
                "L3W28VerifyBeforeInstall$A5", unverifiable(donorAs("D5", "A5")));
        i.addTransformer(swap, true);
        String outcome;
        try {
            i.retransformClasses(A4.class, A5.class);
            outcome = "returned";
        } catch (Throwable t) {
            outcome = describe(t);
        }
        i.removeTransformer(swap);
        System.out.println("retransform-second-bad: " + outcome + " first=" + value(A4.class)
                + " second=" + value(A5.class));

        batch("redefine-both-good",
                new ClassDefinition(A6.class, donorAs("D6", "A6")),
                new ClassDefinition(A7.class, donorAs("D7", "A7")));
    }
}
