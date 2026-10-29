// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.reflect.AccessibleObject;
import java.lang.reflect.Constructor;
import java.lang.reflect.Field;
import java.lang.reflect.Method;

/**
 * Regression: `AccessibleObject.canAccess(Object)` validates its RECEIVER
 * argument, and does so before it decides anything about access.
 *
 * `canAccess` answers two different kinds of question and only one of them is a
 * boolean. "Can this caller reach this member" is the boolean. "Is this even a
 * legal receiver for this member" is an argument error, and HotSpot raises it
 * first:
 *
 *   instance member, obj == null      -> IAE "null object for <member>"
 *   instance member, wrong type       -> IAE "object is not an instance of <Class>"
 *   static member,   obj != null      -> IAE "non-null object for <member>"
 *   constructor,     obj != null      -> IAE "non-null object for <member>"
 *
 * Every one of those answered a plain `false` in CratonVM until 2026-08-10, so
 * a caller that distinguishes "you may not" from "you asked wrong" could not.
 * `Integer.value.canAccess("x")` is the row that pins the ORDERING: the access
 * check on that member would legitimately answer `false`, and HotSpot still
 * throws, which is only possible if the argument test runs first.
 *
 * The `setAccessible(true)` rows matter for the same reason: the override
 * short-circuits the ACCESS half, and it must not short-circuit the argument
 * half with it.
 *
 * Messages are asserted in full, not just the exception type. They embed
 * `Member.toString()` and `Class.getName()`, both already byte-identical to
 * HotSpot's, so a message-only divergence would otherwise pass unnoticed — and
 * the message is the whole difference between the three arms.
 */
public class RCanAccessReceiver {

    static int checks = 0;

    static void eq(String actual, String expected, String what) {
        if (!expected.equals(actual)) {
            throw new AssertionError(what + ":\n  expected " + expected + "\n  got      " + actual);
        }
        checks++;
    }

    public static class Target {
        public int instField = 1;
        public static int staticField = 2;
        private int privInstField = 3;

        public Target() {
        }

        public void instMethod() {
        }

        public static void staticMethod() {
        }

        private void privInstMethod() {
        }
    }

    /** A subclass instance is an instance — `isInstance`, not identity. */
    public static class Sub extends Target {
    }

    public static class Unrelated {
    }

    /** A default method's declaring type is an INTERFACE, not a superclass. */
    public interface WithDefault {
        default void hello() {
        }
    }

    public static class Impl implements WithDefault {
    }

    public static void main(String[] args) throws Exception {
        Target t = new Target();
        Sub s = new Sub();
        Unrelated u = new Unrelated();

        Field inst = Target.class.getDeclaredField("instField");
        Field stat = Target.class.getDeclaredField("staticField");
        Field priv = Target.class.getDeclaredField("privInstField");
        Method instM = Target.class.getDeclaredMethod("instMethod");
        Method statM = Target.class.getDeclaredMethod("staticMethod");
        Method privM = Target.class.getDeclaredMethod("privInstMethod");
        Constructor<?> ctor = Target.class.getDeclaredConstructor();

        String targetName = Target.class.getName();

        // ---- instance field ----
        eq(row(inst, null), "!IllegalArgumentException: null object for " + inst,
                "instance field, null receiver");
        eq(row(inst, t), "=true", "instance field, exact receiver");
        eq(row(inst, s), "=true", "instance field, subclass receiver");
        eq(row(inst, u), "!IllegalArgumentException: object is not an instance of " + targetName,
                "instance field, unrelated receiver");
        eq(row(inst, "x"), "!IllegalArgumentException: object is not an instance of " + targetName,
                "instance field, String receiver");

        // ---- static field: the receiver must be ABSENT ----
        eq(row(stat, null), "=true", "static field, null receiver");
        eq(row(stat, t), "!IllegalArgumentException: non-null object for " + stat,
                "static field, non-null receiver");
        eq(row(stat, u), "!IllegalArgumentException: non-null object for " + stat,
                "static field, unrelated non-null receiver");

        // ---- private instance field: same argument rules ----
        eq(row(priv, null), "!IllegalArgumentException: null object for " + priv,
                "private instance field, null receiver");
        eq(row(priv, t), "=true", "private instance field, nestmate caller");
        eq(row(priv, u), "!IllegalArgumentException: object is not an instance of " + targetName,
                "private instance field, unrelated receiver");

        // ---- methods ----
        eq(row(instM, null), "!IllegalArgumentException: null object for " + instM,
                "instance method, null receiver");
        eq(row(instM, t), "=true", "instance method, exact receiver");
        eq(row(instM, s), "=true", "instance method, subclass receiver");
        eq(row(instM, u), "!IllegalArgumentException: object is not an instance of " + targetName,
                "instance method, unrelated receiver");
        eq(row(statM, null), "=true", "static method, null receiver");
        eq(row(statM, t), "!IllegalArgumentException: non-null object for " + statM,
                "static method, non-null receiver");
        eq(row(privM, null), "!IllegalArgumentException: null object for " + privM,
                "private instance method, null receiver");
        eq(row(privM, t), "=true", "private instance method, nestmate caller");

        // ---- constructor: Modifier.isStatic is FALSE, yet the receiver must
        //      still be absent. Deriving the rule from isStatic alone gets this
        //      one wrong in the quiet direction. ----
        eq(row(ctor, null), "=true", "constructor, null receiver");
        eq(row(ctor, t), "!IllegalArgumentException: non-null object for " + ctor,
                "constructor, non-null receiver");
        eq(row(ctor, u), "!IllegalArgumentException: non-null object for " + ctor,
                "constructor, unrelated non-null receiver");

        // ---- a default method: the declaring type is an interface, so an
        //      `isInstance` implemented as a superclass walk fails here. ----
        Method dflt = WithDefault.class.getDeclaredMethod("hello");
        eq(row(dflt, new Impl()), "=true", "default method, implementing receiver");
        eq(row(dflt, u), "!IllegalArgumentException: object is not an instance of "
                + WithDefault.class.getName(), "default method, non-implementing receiver");
        eq(row(dflt, null), "!IllegalArgumentException: null object for " + dflt,
                "default method, null receiver");

        // ---- ORDERING: a member whose access check would answer `false`
        //      still throws for a bad receiver. ----
        Field foreign = Integer.class.getDeclaredField("value");
        eq(row(foreign, null), "!IllegalArgumentException: null object for " + foreign,
                "unreachable member, null receiver -> argument error, not false");
        eq(row(foreign, Integer.valueOf(7)), "=false",
                "unreachable member, correct receiver -> false");
        eq(row(foreign, "x"), "!IllegalArgumentException: object is not an instance of "
                + Integer.class.getName(),
                "unreachable member, wrong receiver -> argument error, not false");

        // ---- setAccessible(true) grants ACCESS, never argument validation ----
        Field opened = Target.class.getDeclaredField("privInstField");
        opened.setAccessible(true);
        eq(row(opened, null), "!IllegalArgumentException: null object for " + opened,
                "opened member, null receiver");
        eq(row(opened, t), "=true", "opened member, correct receiver");
        eq(row(opened, u), "!IllegalArgumentException: object is not an instance of " + targetName,
                "opened member, unrelated receiver");

        Field openedStatic = Target.class.getDeclaredField("staticField");
        openedStatic.setAccessible(true);
        eq(row(openedStatic, t), "!IllegalArgumentException: non-null object for " + openedStatic,
                "opened static member, non-null receiver");

        System.out.println("CK RCanAccessReceiver checks=" + checks);
        System.out.println("PASS RCanAccessReceiver");
    }

    static String row(AccessibleObject ao, Object obj) {
        try {
            return "=" + ao.canAccess(obj);
        } catch (Throwable e) {
            return "!" + e.getClass().getSimpleName() + ": " + e.getMessage();
        }
    }
}
