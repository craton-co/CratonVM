// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 27, lane L3
// (docs/internal/fixed-bugs/interpreter-L3-a-rejected-redefinition-returns-normally-FIXED-20260928.md):
// the throwable java.lang.instrument raises for each kind of redefinition the
// VM must refuse, that the refused class keeps its old body, and that a call
// on several classes changes none of them when one is refused.
//
// Each donor class is renamed in place to its target (the names differ in one
// character), so the donor's bytes declare the target class.
//
// HotSpot 25 prints (agent; -Xint too; run 2026-09-28):
//     added: java.lang.UnsupportedOperationException: class redefinition failed: attempted to add a method value=old
//     deleted: java.lang.UnsupportedOperationException: class redefinition failed: attempted to delete a method value=old
//     field: java.lang.UnsupportedOperationException: class redefinition failed: attempted to change the schema (add/remove fields) value=old
//     fieldmods: java.lang.UnsupportedOperationException: class redefinition failed: attempted to change the schema (add/remove fields) value=old
//     iface: java.lang.UnsupportedOperationException: class redefinition failed: attempted to change superclass or interfaces value=old
//     methodmods: java.lang.UnsupportedOperationException: class redefinition failed: attempted to change method modifiers value=old
//     classmods: java.lang.UnsupportedOperationException: class redefinition failed: attempted to change the class modifiers value=old
//     name: java.lang.NoClassDefFoundError: class names don't match value=old
//     format: java.lang.ClassFormatError: null value=old
//     batch: java.lang.UnsupportedOperationException: class redefinition failed: attempted to add a method first=old second=old
//     retransform: java.lang.UnsupportedOperationException: class redefinition failed: attempted to add a method value=old
//     verify: java.lang.VerifyError: null value=old
//     unmodifiable-array: java.lang.instrument.UnmodifiableClassException: null
//     unmodifiable-prim: java.lang.instrument.UnmodifiableClassException: null
//     unmodifiable-batch: java.lang.instrument.UnmodifiableClassException: null calls=0 value=old
//     ok: returned value=new
// CratonVM before wave 27 (read from the code): every refused row printed
// `returned` with value=old, `batch` printed `returned first=new second=old`,
// `classmods` was accepted (`returned value=new`), and `unmodifiable-batch`
// printed `returned calls=1 value=old`.
//
// SETUP: a jar whose manifest has
//     Premain-Class: L3W27RedefinitionRefusals$Agent
//     Can-Redefine-Classes: true
//     Can-Retransform-Classes: true
// containing L3W27RedefinitionRefusals*.class, then
//     java|cratonvm --java-home <jdk25> [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar L3W27RedefinitionRefusals
// Without the agent both VMs print "no agent".
import java.io.InputStream;
import java.lang.instrument.ClassDefinition;
import java.lang.instrument.ClassFileTransformer;
import java.lang.instrument.Instrumentation;
import java.security.ProtectionDomain;

public class L3W27RedefinitionRefusals {
    static volatile Instrumentation inst;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    // ---- targets ($A<n>) and donors ($D<n>, renamed to $A<n>) ----

    public static class A0 {
        public static String value() { return "old"; }
    }
    public static class D0 {
        public static String value() { return "new"; }
        public static String extra() { return "extra"; }
    }

    public static class A1 {
        public static String value() { return "old"; }
        public static String extra() { return "extra"; }
    }
    public static class D1 {
        public static String value() { return "new"; }
    }

    public static class A2 {
        public static String value() { return "old"; }
    }
    public static class D2 {
        static int added;
        public static String value() { return "new"; }
    }

    public static class A3 {
        static int f;
        public static String value() { return "old"; }
    }
    public static class D3 {
        static volatile int f;
        public static String value() { return "new"; }
    }

    public static class A4 {
        public static String value() { return "old"; }
    }
    public static class D4 implements java.io.Serializable {
        public static String value() { return "new"; }
    }

    public static class A5 {
        public static String value() { return "old"; }
    }
    public static class D5 {
        public static synchronized String value() { return "new"; }
    }

    public static class A6 {
        public static String value() { return "old"; }
    }
    public static final class D6 {
        public static String value() { return "new"; }
    }

    public static class A7 {
        public static String value() { return "old"; }
    }
    public static class B0 {
        public static String value() { return "new"; }
    }

    public static class A8 {
        public static String value() { return "old"; }
    }

    // batch: A9 gets a good new body, A10 one method more; neither may change.
    public static class A9 {
        public static String value() { return "old"; }
    }
    public static class D9 {
        public static String value() { return "new"; }
    }
    public static class Aa {
        public static String value() { return "old"; }
    }
    public static class Da {
        public static String value() { return "new"; }
        public static String extra() { return "extra"; }
    }

    // retransform: Ab's transformer hands back Db's bytes (one method more).
    public static class Ab {
        public static String value() { return "old"; }
    }
    public static class Db {
        public static String value() { return "new"; }
        public static String extra() { return "extra"; }
    }

    // verify: Dd's value() with its areturn patched to ireturn.
    public static class Ad {
        public static String value() { return "old"; }
    }
    public static class Dd {
        public static String value() { return "new"; }
    }

    // unmodifiable-batch: Ae comes first, int[] second; no transformer may run.
    public static class Ae {
        public static String value() { return "old"; }
    }

    // ok: a legal body change, for the positive control.
    public static class Ac {
        public static String value() { return "old"; }
    }
    public static class Dc {
        public static String value() { return "new"; }
    }

    static byte[] bytesOf(String simple) throws Exception {
        try (InputStream in = L3W27RedefinitionRefusals.class
                .getResourceAsStream("L3W27RedefinitionRefusals$" + simple + ".class")) {
            return in.readAllBytes();
        }
    }

    /** The donor's class file, every occurrence of its name turned into the target's. */
    static byte[] donorAs(String donor, String target) throws Exception {
        byte[] b = bytesOf(donor);
        String from = "L3W27RedefinitionRefusals$" + donor;
        String to = "L3W27RedefinitionRefusals$" + target;
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
            // code_length (u4) = 3, then ldc, index, areturn
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

    static void redefine(String label, Class<?> target, byte[] bytes) throws Exception {
        String outcome;
        try {
            inst.redefineClasses(new ClassDefinition(target, bytes));
            outcome = "returned";
        } catch (Throwable t) {
            outcome = describe(t);
        }
        System.out.println(label + ": " + outcome + " value=" + value(target));
    }

    static final class Swap implements ClassFileTransformer {
        final String name;
        final byte[] replacement;

        Swap(String name, byte[] replacement) {
            this.name = name;
            this.replacement = replacement;
        }

        @Override
        public byte[] transform(ClassLoader loader, String className, Class<?> redefined,
                ProtectionDomain domain, byte[] bytes) {
            if (redefined == null || !name.equals(className)) {
                return null;
            }
            return replacement.clone();
        }
    }

    /** Counts the offers of one class to a retransform-capable transformer. */
    static final class Counter implements ClassFileTransformer {
        final String name;
        int calls;

        Counter(String name) {
            this.name = name;
        }

        @Override
        public byte[] transform(ClassLoader loader, String className, Class<?> redefined,
                ProtectionDomain domain, byte[] bytes) {
            if (redefined != null && name.equals(className)) {
                calls++;
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
        // Load every target first (value() once each).
        Class<?>[] targets = {A0.class, A1.class, A2.class, A3.class, A4.class, A5.class,
                A6.class, A7.class, A8.class, A9.class, Aa.class, Ab.class, Ac.class, Ad.class,
                Ae.class};
        for (Class<?> c : targets) {
            value(c);
        }
        redefine("added", A0.class, donorAs("D0", "A0"));
        redefine("deleted", A1.class, donorAs("D1", "A1"));
        redefine("field", A2.class, donorAs("D2", "A2"));
        redefine("fieldmods", A3.class, donorAs("D3", "A3"));
        redefine("iface", A4.class, donorAs("D4", "A4"));
        redefine("methodmods", A5.class, donorAs("D5", "A5"));
        redefine("classmods", A6.class, donorAs("D6", "A6"));
        redefine("name", A7.class, bytesOf("B0"));
        byte[] truncated = java.util.Arrays.copyOf(bytesOf("A8"), 40);
        redefine("format", A8.class, truncated);

        String outcome;
        try {
            i.redefineClasses(new ClassDefinition(A9.class, donorAs("D9", "A9")),
                    new ClassDefinition(Aa.class, donorAs("Da", "Aa")));
            outcome = "returned";
        } catch (Throwable t) {
            outcome = describe(t);
        }
        System.out.println("batch: " + outcome + " first=" + value(A9.class)
                + " second=" + value(Aa.class));

        Swap swap = new Swap("L3W27RedefinitionRefusals$Ab", donorAs("Db", "Ab"));
        i.addTransformer(swap, true);
        try {
            i.retransformClasses(Ab.class);
            outcome = "returned";
        } catch (Throwable t) {
            outcome = describe(t);
        }
        i.removeTransformer(swap);
        System.out.println("retransform: " + outcome + " value=" + value(Ab.class));

        redefine("verify", Ad.class, unverifiable(donorAs("Dd", "Ad")));

        try {
            i.retransformClasses(String[].class);
            outcome = "returned";
        } catch (Throwable t) {
            outcome = describe(t);
        }
        System.out.println("unmodifiable-array: " + outcome);
        try {
            i.redefineClasses(new ClassDefinition(int.class, bytesOf("A8")));
            outcome = "returned";
        } catch (Throwable t) {
            outcome = describe(t);
        }
        System.out.println("unmodifiable-prim: " + outcome);
        Counter counter = new Counter("L3W27RedefinitionRefusals$Ae");
        i.addTransformer(counter, true);
        try {
            i.retransformClasses(Ae.class, int[].class);
            outcome = "returned";
        } catch (Throwable t) {
            outcome = describe(t);
        }
        i.removeTransformer(counter);
        System.out.println("unmodifiable-batch: " + outcome + " calls=" + counter.calls
                + " value=" + value(Ae.class));

        redefine("ok", Ac.class, donorAs("Dc", "Ac"));
    }
}
