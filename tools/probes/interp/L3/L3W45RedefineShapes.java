// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 45, lane L3 -- review of `java.lang.instrument`
// redefinition against HotSpot for class shapes the earlier probes did not
// cover: an interface's default method, a nest mate calling its host's
// private method, a nest member redefined with bytes that name no nest host,
// a record, a class with an enum switch (javac's `$SwitchMap$` class), an
// enum, and transformers that throw.
//
// Each donor class is renamed in place to its target (the names differ in one
// character), so the donor's bytes declare the target class. One line per
// row: `<row>: <returned | the exception> value=<the target's answer after>`.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, measured; the same with -Xint)
// default-method: returned value=new
// default-method-added: java.lang.UnsupportedOperationException: class redefinition failed: attempted to add a method value=old
// nest-mate-private: returned value=new-secret
// nest-host-dropped: java.lang.UnsupportedOperationException: class redefinition failed: attempted to change the class NestHost, NestMembers, Record, or PermittedSubclasses attribute value=old
// nest-host-other: java.lang.UnsupportedOperationException: class redefinition failed: attempted to change the class NestHost, NestMembers, Record, or PermittedSubclasses attribute value=old
// permits-reordered: returned value=new
// permits-changed: java.lang.UnsupportedOperationException: class redefinition failed: attempted to change the class NestHost, NestMembers, Record, or PermittedSubclasses attribute value=old
// record-reordered: java.lang.UnsupportedOperationException: class redefinition failed: attempted to change the class NestHost, NestMembers, Record, or PermittedSubclasses attribute value=old x=1 y=2
// record-body: returned value=new x=1 true 1
// record-component-added: java.lang.UnsupportedOperationException: class redefinition failed: attempted to change the class NestHost, NestMembers, Record, or PermittedSubclasses attribute value=old x=1
// enum-switch: returned value=mon-new,other-new,wed-new
// enum-body: returned value=new-A,new-B,2
// enum-constant-added: java.lang.UnsupportedOperationException: class redefinition failed: attempted to change the schema (add/remove fields) value=old-A,2
// retransform-transformer-throws: returned value=old calls=1
// retransform-throws-then-swap: returned value=new calls=1
// redefine-transformer-throws: returned value=new calls=1
// load-transformer-throws: returned value=loaded calls=1
// HOTSPOT_EXPECTED_END
//
// CratonVM on the base 69568bea6 (read from the code, both modes:
// `ClassManager::check_redefinition_structure` compared no class attribute):
//     nest-host-dropped: ...: attempted to change the class modifiers value=old
//     nest-host-other: ...: attempted to change the class modifiers value=old
//     permits-changed: returned value=new
//     record-reordered: returned value=new x=1 y=2
//     record-component-added: ...: attempted to change the schema (add/remove fields) value=old x=1
// (the two `nest-host-*` donors are also package-private, which the base
// checked next). Since wave 45 the check compares the NestHost, NestMembers,
// Record and PermittedSubclasses attributes after the hierarchy and before the
// class modifiers, as HotSpot orders them (`RedefinitionNesting::changed_by`,
// `RedefinitionRefusal::ClassAttributeChanged`), and every row is expected to
// print HotSpot's line. The other rows are expected to match on the base too
// (a review, not a fix); the host run is the check. The rows themselves are
// the positive control: the base cannot print the attribute message.
//
// HotSpot's order, measured with throwaway donors on the lane's machine: a
// donor that drops the NestHost and also adds a method, a field or `final`
// is refused for the attribute; one that also adds an interface, for the
// hierarchy. `permits` is compared as a set (reordered: accepted), record
// components in order; a component's annotation is not compared.
//
// SETUP: a jar whose manifest has
//     Premain-Class: L3W45RedefineShapes$Agent
//     Can-Redefine-Classes: true
//     Can-Retransform-Classes: true
// containing L3W45RedefineShapes*.class, then
//     java|cratonvm [--compatible] [--nojit] -javaagent:probe.jar -cp probe.jar L3W45RedefineShapes
// Without the agent both VMs print "no agent".
import java.io.InputStream;
import java.lang.instrument.ClassDefinition;
import java.lang.instrument.ClassFileTransformer;
import java.lang.instrument.Instrumentation;
import java.security.ProtectionDomain;
import java.time.DayOfWeek;
import java.util.function.Supplier;

public class L3W45RedefineShapes {
    static volatile Instrumentation inst;

    public static class Agent {
        public static void premain(String args, Instrumentation instrumentation) {
            inst = instrumentation;
        }
    }

    private static String secret() {
        return "secret";
    }

    // ---- default method ----
    public interface I0 {
        default String value() { return "old"; }
    }
    public interface J0 {
        default String value() { return "new"; }
    }
    public static final class Impl0 implements I0 {
    }

    // ---- an interface that gains a default method ----
    public interface I1 {
        default String value() { return "old"; }
    }
    public interface J1 {
        default String value() { return "new"; }
        default String extra() { return "extra"; }
    }
    public static final class Impl1 implements I1 {
    }

    // ---- nest mate calling the host's private method ----
    public static class M0 {
        public static String value() { return "old"; }
    }
    public static class N0 {
        public static String value() { return "new-" + secret(); }
    }

    // ---- a nest member redefined with a top-level class's bytes ----
    public static class M1 {
        public static String value() { return "old"; }
    }

    // ---- a nest member redefined with a member of another nest ----
    public static class M2 {
        public static String value() { return "old"; }
    }

    // ---- sealed interfaces: permits reordered / one subclass swapped ----
    public sealed interface S1 permits P0, P1 {
        static String value() { return "old"; }
    }
    public sealed interface T1 permits P1, P0 {
        static String value() { return "new"; }
    }
    public sealed interface S2 permits P0, P1 {
        static String value() { return "old"; }
    }
    public sealed interface T2 permits P0, P2 {
        static String value() { return "new"; }
    }
    public static final class P0 implements S1, T1, S2, T2 {
    }
    public static final class P1 implements S1, T1, S2 {
    }
    public static final class P2 implements T2 {
    }

    // ---- record, body only ----
    public record R0(int x) {
        public String value() { return "old"; }
    }
    public record Q0(int x) {
        public String value() { return "new"; }
    }

    // ---- record, one component more ----
    public record R1(int x) {
        public String value() { return "old"; }
    }
    public record Q1(int x, int y) {
        public String value() { return "new"; }
    }

    // ---- record, components reordered ----
    public record R2(int x, int y) {
        public String value() { return "old"; }
    }
    public record Q2(int y, int x) {
        public String value() { return "new"; }
    }

    // ---- enum switch, through javac's switch map ----
    // An enum of another compilation unit: javac reads the case through the
    // top-level class's synthetic `$SwitchMap$` array (`L3W45RedefineShapes$1`,
    // shared by S0 and T0; an enum of this file is switched on its ordinal).
    // The donor switches on a constant S0 never named.
    public static class S0 {
        public static String arm(DayOfWeek d) {
            switch (d) {
                case MONDAY: return "mon-old";
                case TUESDAY: return "tue-old";
                default: return "other-old";
            }
        }
    }
    public static class T0 {
        public static String arm(DayOfWeek d) {
            switch (d) {
                case WEDNESDAY: return "wed-new";
                case MONDAY: return "mon-new";
                default: return "other-new";
            }
        }
    }

    // ---- enum, body only / one constant more ----
    public enum E0 {
        A, B;
        public String value() { return "old-" + name(); }
    }
    public enum F0 {
        A, B;
        public String value() { return "new-" + name(); }
    }
    public enum E1 {
        A, B;
        public String value() { return "old-" + name(); }
    }
    public enum F1 {
        A, B, C;
        public String value() { return "new-" + name(); }
    }

    // ---- transformers that throw ----
    public static class X0 {
        public static String value() { return "old"; }
    }
    public static class X1 {
        public static String value() { return "old"; }
    }
    public static class Y1 {
        public static String value() { return "new"; }
    }
    public static class X2 {
        public static String value() { return "old"; }
    }
    public static class Y2 {
        public static String value() { return "new"; }
    }
    public static class X3 {
        public static String value() { return "loaded"; }
    }

    static final String PREFIX = "L3W45RedefineShapes$";

    static byte[] bytesOf(String binaryName) throws Exception {
        try (InputStream in = L3W45RedefineShapes.class.getClassLoader()
                .getResourceAsStream(binaryName + ".class")) {
            return in.readAllBytes();
        }
    }

    /** `b` with every occurrence of `from` turned into `to` (same length). */
    static byte[] rename(byte[] b, String from, String to) {
        if (from.length() != to.length()) {
            throw new IllegalArgumentException(from + " / " + to);
        }
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

    /** The nested donor's class file, renamed to the nested target. */
    static byte[] donorAs(String donor, String target) throws Exception {
        return rename(bytesOf(PREFIX + donor), PREFIX + donor, PREFIX + target);
    }

    static String describe(Throwable t) {
        return t.getClass().getName() + ": " + t.getMessage();
    }

    static String attempt(ThrowingRunnable r) {
        try {
            r.run();
            return "returned";
        } catch (Throwable t) {
            return describe(t);
        }
    }

    interface ThrowingRunnable {
        void run() throws Throwable;
    }

    static void row(String name, String outcome, Supplier<String> value) {
        String v;
        try {
            v = value.get();
        } catch (Throwable t) {
            v = "threw " + describe(t);
        }
        System.out.println(name + ": " + outcome + " value=" + v);
    }

    /** Throws for one class's offers; counts them. */
    static final class Thrower implements ClassFileTransformer {
        final String name;
        int calls;

        Thrower(String name) {
            this.name = name;
        }

        @Override
        public byte[] transform(ClassLoader loader, String className, Class<?> redefined,
                ProtectionDomain domain, byte[] bytes) {
            if (name.equals(className)) {
                calls++;
                throw new IllegalStateException("transformer refuses " + className);
            }
            return null;
        }
    }

    /** Hands back `replacement` for one class's retransform / redefine offers. */
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

    public static void main(String[] args) throws Exception {
        Instrumentation i = inst;
        if (i == null || !i.isRedefineClassesSupported() || !i.isRetransformClassesSupported()) {
            System.out.println("no agent");
            return;
        }
        Impl0 impl0 = new Impl0();
        Impl1 impl1 = new Impl1();
        impl0.value();
        impl1.value();
        M0.value();
        M1.value();
        M2.value();
        S1.value();
        S2.value();
        R2 r2 = new R2(1, 2);
        r2.value();
        R0 r0 = new R0(1);
        R1 r1 = new R1(1);
        r0.value();
        r1.value();
        S0.arm(DayOfWeek.MONDAY);
        E0.A.value();
        E1.A.value();
        X0.value();
        X1.value();
        X2.value();

        String out = attempt(() -> i.redefineClasses(new ClassDefinition(I0.class, donorAs("J0", "I0"))));
        row("default-method", out, impl0::value);

        out = attempt(() -> i.redefineClasses(new ClassDefinition(I1.class, donorAs("J1", "I1"))));
        row("default-method-added", out, impl1::value);

        out = attempt(() -> i.redefineClasses(new ClassDefinition(M0.class, donorAs("N0", "M0"))));
        row("nest-mate-private", out, M0::value);

        out = attempt(() -> i.redefineClasses(new ClassDefinition(M1.class,
                rename(bytesOf("L3W45RedefineShapes_M1"), "L3W45RedefineShapes_M1", PREFIX + "M1"))));
        row("nest-host-dropped", out, M1::value);

        out = attempt(() -> i.redefineClasses(new ClassDefinition(M2.class,
                rename(bytesOf("L3W45RedefineShapeX$M2"), "L3W45RedefineShapeX$M2", PREFIX + "M2"))));
        row("nest-host-other", out, M2::value);

        out = attempt(() -> i.redefineClasses(new ClassDefinition(S1.class, donorAs("T1", "S1"))));
        row("permits-reordered", out, S1::value);

        out = attempt(() -> i.redefineClasses(new ClassDefinition(S2.class, donorAs("T2", "S2"))));
        row("permits-changed", out, S2::value);

        out = attempt(() -> i.redefineClasses(new ClassDefinition(R2.class, donorAs("Q2", "R2"))));
        row("record-reordered", out, () -> r2.value() + " x=" + r2.x() + " y=" + r2.y());

        out = attempt(() -> i.redefineClasses(new ClassDefinition(R0.class, donorAs("Q0", "R0"))));
        // Not `toString`: HotSpot names the record by the NEW class file's
        // InnerClasses entry, which the in-place rename leaves as the donor's.
        row("record-body", out, () -> r0.value() + " x=" + r0.x() + " " + r0.equals(new R0(1))
                + " " + r0.hashCode());

        out = attempt(() -> i.redefineClasses(new ClassDefinition(R1.class, donorAs("Q1", "R1"))));
        row("record-component-added", out, () -> r1.value() + " x=" + r1.x());

        out = attempt(() -> i.redefineClasses(new ClassDefinition(S0.class, donorAs("T0", "S0"))));
        row("enum-switch", out,
                () -> S0.arm(DayOfWeek.MONDAY) + "," + S0.arm(DayOfWeek.TUESDAY) + ","
                        + S0.arm(DayOfWeek.WEDNESDAY));

        out = attempt(() -> i.redefineClasses(new ClassDefinition(E0.class, donorAs("F0", "E0"))));
        row("enum-body", out, () -> E0.A.value() + "," + E0.valueOf("B").value() + "," + E0.values().length);

        out = attempt(() -> i.redefineClasses(new ClassDefinition(E1.class, donorAs("F1", "E1"))));
        row("enum-constant-added", out, () -> E1.A.value() + "," + E1.values().length);

        // A retransform-capable transformer that throws: the JDK's
        // TransformerManager swallows it; the class keeps its bytes.
        Thrower thrower = new Thrower(PREFIX + "X0");
        i.addTransformer(thrower, true);
        out = attempt(() -> i.retransformClasses(X0.class));
        i.removeTransformer(thrower);
        row("retransform-transformer-throws", out, () -> X0.value() + " calls=" + thrower.calls);

        // A throwing transformer first, a swapping one after it: the second
        // still runs.
        Thrower first = new Thrower(PREFIX + "X1");
        Swap swap = new Swap(PREFIX + "X1", donorAs("Y1", "X1"));
        i.addTransformer(first, true);
        i.addTransformer(swap, true);
        out = attempt(() -> i.retransformClasses(X1.class));
        i.removeTransformer(first);
        i.removeTransformer(swap);
        row("retransform-throws-then-swap", out, () -> X1.value() + " calls=" + first.calls);

        // redefineClasses offers the new bytes to the (non-retransform)
        // transformers too; one that throws does not stop the redefinition.
        Thrower onRedefine = new Thrower(PREFIX + "X2");
        i.addTransformer(onRedefine);
        out = attempt(() -> i.redefineClasses(new ClassDefinition(X2.class, donorAs("Y2", "X2"))));
        i.removeTransformer(onRedefine);
        row("redefine-transformer-throws", out, () -> X2.value() + " calls=" + onRedefine.calls);

        // A load-time transformer that throws: the class loads as it is.
        Thrower onLoad = new Thrower(PREFIX + "X3");
        i.addTransformer(onLoad);
        out = attempt(() -> X3.value());
        i.removeTransformer(onLoad);
        row("load-transformer-throws", out, () -> X3.value() + " calls=" + onLoad.calls);
    }
}

/** The donor of the `nest-host-dropped` row: a top-level class, no NestHost. */
class L3W45RedefineShapes_M1 {
    public static String value() { return "new"; }
}

/** The donor of the `nest-host-other` row: a member of another nest. */
class L3W45RedefineShapeX {
    static class M2 {
        public static String value() { return "new"; }
    }
}
