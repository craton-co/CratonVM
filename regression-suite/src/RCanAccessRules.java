// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.reflect.Constructor;
import java.lang.reflect.Field;
import java.lang.reflect.Method;

/**
 * Regression: `AccessibleObject.canAccess` must answer exactly what
 * `Reflection.verifyMemberAccess` answers, for every arm of JLS 6.6.1.
 *
 * `canAccess` is the only consumer of the VM's `verify_member_access`, and it
 * is a pure predicate: a wrong answer throws nothing, logs nothing, and simply
 * makes the CALLER take its other branch. That is why the defect this vector
 * pins presented as a missing field:
 * `jakarta.el.StaticFieldELResolver.getValue` guards its read with
 * `... && Util.canAccess(null, field)` and, on `false`, builds its own
 * "No public static field named [X] was found" message with a NULL cause —
 * while `getField`, `getModifiers` and `Field.get` had all answered correctly.
 * See teststaticfieldelresolver-get-type-field-not-found-CLOSED.md.
 *
 * The arm that was wrong: a PUBLIC member of a declaring class that is NOT
 * public, read from a caller in that class's own package. The VM asked "is the
 * declaring class public?" where HotSpot asks "is the declaring class public OR
 * is the caller in its runtime package?", so every public member of every
 * package-private class was unreachable. Enum constants on a private nested
 * enum are the common shape (`ENUM_CONSTANT` below): javac makes them
 * `public static final` on a class whose file-level access flags carry no
 * ACC_PUBLIC.
 *
 * Two neighbouring arms are pinned with it, both previously wrong in the other
 * direction or refused outright:
 *   * a PRIVATE member is reachable from a NESTMATE (JEP 181);
 *   * a member of a package-private class is refused from a class the caller
 *     cannot reach at all — the class-level test must run BEFORE the member
 *     modifiers, not instead of them.
 *
 * Only booleans are printed. An identity hash or a `Field.toString()` would
 * make the output VM-specific and the HotSpot diff meaningless.
 */
public class RCanAccessRules {

    static int checks = 0;

    static void eq(boolean actual, boolean expected, String what) {
        if (actual != expected) {
            throw new AssertionError(what + ": expected " + expected + " got " + actual);
        }
        checks++;
    }

    // ------------------------------------------------------------------
    // A package-private declaring class, one member per access level.
    // ------------------------------------------------------------------
    static class PkgPrivate {
        public static String pub = "pub";
        static String pkg = "pkg";
        private static String priv = "priv";
        protected static String prot = "prot";

        public static void pubM() {
        }

        private static void privM() {
        }
    }

    /** The enum shape the Tomcat failure actually had: private nested enum. */
    private enum PrivateEnum {
        GET_VALUE,
        GET_TYPE
    }

    /** A public nested class, to separate "class is public" from "member is public". */
    public static class PublicHolder {
        public static String pub = "pub";
        private static String priv = "priv";

        public PublicHolder() {
        }

        private PublicHolder(int unused) {
        }
    }

    public static void main(String[] args) throws Exception {
        // 1. The regressed arm: public member, non-public class, same package.
        eq(field(PkgPrivate.class, "pub").canAccess(null), true,
                "public member of a package-private class, same package");
        eq(method(PkgPrivate.class, "pubM").canAccess(null), true,
                "public method of a package-private class, same package");

        // The exact shape jakarta.el.TestStaticFieldELResolver exercises: an
        // enum constant, which is public static final on a private nested enum.
        Field enumConstant = PrivateEnum.class.getField(PrivateEnum.GET_TYPE.toString());
        eq(enumConstant.canAccess(null), true, "enum constant of a private nested enum");
        eq(enumConstant.get(null) == PrivateEnum.GET_TYPE, true,
                "the read canAccess permits returns the constant");

        // 2. Package-private and protected members, same package: unchanged.
        eq(field(PkgPrivate.class, "pkg").canAccess(null), true,
                "package-private member, same package");
        eq(field(PkgPrivate.class, "prot").canAccess(null), true,
                "protected member, same package");

        // 3. Private members reach their NESTMATES, in both directions.
        eq(field(PkgPrivate.class, "priv").canAccess(null), true,
                "private member of a nestmate");
        eq(method(PkgPrivate.class, "privM").canAccess(null), true,
                "private method of a nestmate");
        eq(field(PublicHolder.class, "priv").canAccess(null), true,
                "private member of a public nested nestmate");
        eq(ctor(PublicHolder.class, int.class).canAccess(null), true,
                "private constructor of a nestmate");
        eq(ctor(PublicHolder.class).canAccess(null), true, "public constructor of a nestmate");

        // 4. A public member of a public class, everywhere.
        eq(field(PublicHolder.class, "pub").canAccess(null), true,
                "public member of a public nested class");
        eq(field(Integer.class, "MAX_VALUE").canAccess(null), true,
                "public member of a foreign public class");

        // 5. NOT a nestmate, NOT the same package: still refused. This is the
        //    conjunct that must not have been widened by the arms above.
        eq(field(Character.class, "TYPE").canAccess(null), true,
                "public member of a foreign public class (2)");
        eq(cratonvm.regr.outsider.RCanAccessOutsider.canReachPkgPrivatePublicMember(), false,
                "public member of a package-private class, FOREIGN package");
        eq(cratonvm.regr.outsider.RCanAccessOutsider.canReachPkgPrivateMember(), false,
                "package-private member, FOREIGN package");
        eq(cratonvm.regr.outsider.RCanAccessOutsider.canReachPublicHolderPublicMember(), true,
                "public member of a public nested class, FOREIGN package");

        System.out.println("CK RCanAccessRules checks=" + checks);
        System.out.println("PASS RCanAccessRules");
    }

    static Field field(Class<?> c, String n) throws Exception {
        return c.getDeclaredField(n);
    }

    static Method method(Class<?> c, String n) throws Exception {
        return c.getDeclaredMethod(n);
    }

    static Constructor<?> ctor(Class<?> c, Class<?>... p) throws Exception {
        return c.getDeclaredConstructor(p);
    }
}
