// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

package cratonvm.regr.outsider;

import java.lang.reflect.Field;

/**
 * The FOREIGN-package half of {@link RCanAccessRules} — not a vector of its own
 * (it has no `main`; it is named in run.sh's UNREGISTERED_CLASSES with this
 * reason).
 *
 * `RCanAccessRules` lives in the unnamed package, and a class in the unnamed
 * package cannot be named from any other package, so the "caller is NOT in the
 * declaring class's runtime package" arm cannot be written there at all. It has
 * to be asked from here, purely reflectively.
 *
 * Both questions below must answer `false`: the reordering that let a public
 * member of a package-private class through from the SAME package must not have
 * let it through from a different one. Without this file, that widening would
 * be invisible — every arm of the fix would still read green.
 */
public final class RCanAccessOutsider {

    private RCanAccessOutsider() {
    }

    /** `public static` member, declaring class package-private, caller elsewhere. */
    public static boolean canReachPkgPrivatePublicMember() throws Exception {
        return canAccessStatic("RCanAccessRules$PkgPrivate", "pub");
    }

    /** Package-private member, declaring class package-private, caller elsewhere. */
    public static boolean canReachPkgPrivateMember() throws Exception {
        return canAccessStatic("RCanAccessRules$PkgPrivate", "pkg");
    }

    /** `public static` member of a PUBLIC nested class — reachable from anywhere. */
    public static boolean canReachPublicHolderPublicMember() throws Exception {
        return canAccessStatic("RCanAccessRules$PublicHolder", "pub");
    }

    private static boolean canAccessStatic(String binaryName, String fieldName) throws Exception {
        Class<?> c = Class.forName(binaryName, false, RCanAccessOutsider.class.getClassLoader());
        Field f = c.getDeclaredField(fieldName);
        return f.canAccess(null);
    }
}
