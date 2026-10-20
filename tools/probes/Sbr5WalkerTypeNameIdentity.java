// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

/**
 * SBR round 5, lane {@code walker}: positive control for the reviewed {@code Class.getTypeName}
 * Intrinsic. On HotSpot a non-array {@code getTypeName()} is {@code getName()} itself (same
 * object), and a lambda class's array-free names agree. Prints {@code true} on HotSpot.
 */
public final class Sbr5WalkerTypeNameIdentity {
    interface I { Object c(); }

    public static void main(String[] args) {
        System.out.println(String.class.getTypeName() == String.class.getName());
        Class<?> lc = ((I) () -> null).getClass();
        System.out.println(lc.getTypeName() == lc.getName());
        System.out.println(Thread.State.class.getTypeName() == Thread.State.class.getName());
    }
}
