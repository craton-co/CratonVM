// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 49, lane L3 -- review evidence for
// docs/internal/fixed-bugs/interpreter-L5-registered-accesscontroller-natives-diverge-from-jdk-25-FIXED-20261014.md:
// JDK 25's `AccessController` is plain Java with the Security Manager gone
// (`getContext()` returns one static `NO_PERMISSIONS_ACC`,
// `checkPermission` always throws); this VM registers natives for
// `getContext` and `AccessControlContext.checkPermission` that emulate a
// policy engine instead.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, measured; the same with -Xint)
// context-same: true
// context-check: java.security.AccessControlException: checking permissions is not supported
// controller-check: java.security.AccessControlException: checking permissions is not supported
// HOTSPOT_EXPECTED_END
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L3W49AccessControllerSurface`.
import java.security.AccessControlContext;
import java.security.AccessController;
import java.util.PropertyPermission;

@SuppressWarnings("removal")
public class L3W49AccessControllerSurface {
    public static void main(String[] args) {
        AccessControlContext a = AccessController.getContext();
        AccessControlContext b = AccessController.getContext();
        System.out.println("context-same: " + (a == b));
        try {
            a.checkPermission(new PropertyPermission("user.home", "read"));
            System.out.println("context-check: allowed");
        } catch (Throwable t) {
            System.out.println("context-check: " + t);
        }
        try {
            AccessController.checkPermission(new PropertyPermission("user.home", "read"));
            System.out.println("controller-check: allowed");
        } catch (Throwable t) {
            System.out.println("controller-check: " + t);
        }
    }
}
