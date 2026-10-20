// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 56, lane L3 (review): `Field.set*` with
// `setAccessible(true)` on a "trusted final" -- HotSpot's
// `fieldDescriptor::is_trusted_final()`: final, and static OR declared by a
// record or a hidden class. JDK 25's `ReflectionFactory.newFieldAccessor`
// builds a read-only accessor for it whatever the override says.
// `--compatible`'s `Field` natives refused only the static case and WROTE a
// record's component field (`lang_class::check_final_for_set`).
//
// Each row prints the value read back after the attempt, or the throwable's
// class and message. The frames are not printed: `--compatible` lists none of
// the accessor's (see
// docs/internal/fixed-bugs/interpreter-L3-compatible-field-refusals-list-no-accessor-frames-FIXED-20261021.md).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags; the same with -Xint)
// record-set: java.lang.IllegalAccessException Can not set final int field L3W56TrustedFinalFieldWrite$Point.x to java.lang.Integer
// record-setInt: java.lang.IllegalAccessException Can not set final int field L3W56TrustedFinalFieldWrite$Point.x to (int)9
// record-set-ref: java.lang.IllegalAccessException Can not set final java.lang.String field L3W56TrustedFinalFieldWrite$Point.label to java.lang.String
// record-value: 1 a
// plain-final-set: 9
// static-final-set: java.lang.IllegalAccessException Can not set static final int field L3W56TrustedFinalFieldWrite.K to java.lang.Integer
// record-set-again: java.lang.IllegalAccessException Can not set final int field L3W56TrustedFinalFieldWrite$Point.x to java.lang.Integer
// HOTSPOT_EXPECTED_END
//
// CratonVM before wave 56 (`--compatible`): the three `record-*` rows printed
// the written value (`set`), and `record-value` printed `9 b`.
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W56TrustedFinalFieldWrite`.
import java.lang.reflect.Field;

public class L3W56TrustedFinalFieldWrite {
    static final int K = 1;

    record Point(int x, String label) {
    }

    static final class Plain {
        final int v = 1;
    }

    interface Call {
        Object call() throws Throwable;
    }

    static void row(String name, Call c) {
        try {
            System.out.println(name + ": " + c.call());
        } catch (Throwable t) {
            System.out.println(name + ": " + t.getClass().getName() + " " + t.getMessage());
        }
    }

    public static void main(String[] args) throws Exception {
        Point p = new Point(1, "a");
        Field x = Point.class.getDeclaredField("x");
        x.setAccessible(true);
        Field label = Point.class.getDeclaredField("label");
        label.setAccessible(true);
        row("record-set", () -> {
            x.set(p, 9);
            return "set";
        });
        row("record-setInt", () -> {
            x.setInt(p, 9);
            return "set";
        });
        row("record-set-ref", () -> {
            label.set(p, "b");
            return "set";
        });
        System.out.println("record-value: " + p.x() + " " + p.label());
        Plain plain = new Plain();
        Field v = Plain.class.getDeclaredField("v");
        v.setAccessible(true);
        row("plain-final-set", () -> {
            v.set(plain, 9);
            return v.getInt(plain);
        });
        Field k = L3W56TrustedFinalFieldWrite.class.getDeclaredField("K");
        k.setAccessible(true);
        row("static-final-set", () -> {
            k.set(null, 9);
            return "set";
        });
        row("record-set-again", () -> {
            x.set(p, 9);
            return "set";
        });
    }
}
