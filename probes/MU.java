// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Reproducer for the `sun.reflect.misc.MethodUtil` load failure fixed
// 2026-08-10 (fixed-suite-bugs/spring/gc-variant-fullsuite-classpath-gap-and-fails-20260810-FIXED.md
// in the internal docs tree).
//
//   javac -d /tmp/probe probes/MU.java
//   /path/to/jdk-25/bin/java              -cp /tmp/probe MU     # the oracle
//   cratonvm --java-home /path/to/jdk-25  -cp /tmp/probe MU
//
// Before the fix, CratonVM printed:
//
//   FAIL  sun.reflect.misc.MethodUtil  -> java.lang.InternalError: bouncer cannot be found
//           caused by java.lang.reflect.InaccessibleObjectException: Unable to make member
//           accessible: module java.base does not "opens sun.reflect.misc" to unnamed module
//
// because `resolve_caller_class_id` skipped the `sun/reflect/misc/MethodUtil$1`
// frame as reflection plumbing (a `sun/reflect/` prefix left over from the
// pre-JDK-9 accessor classes) and walked out to the application class, so a
// same-module `setAccessible` was judged to come from the unnamed module.
// `MethodUtil.<clinit>` wraps that failure, so every later use surfaced as a
// bare `NoClassDefFoundError: sun/reflect/misc/MethodUtil` -- pointing nowhere
// near reflection. The JDK's `RequiredModelMBean` routes every managed
// operation through `MethodUtil`, so this one frame took the whole
// `javax.management` surface with it (58 failure lines across 22
// `org.springframework.jmx.*` classes).
//
// The `Trampoline` line is expected to FAIL on both VMs -- `Trampoline` refuses
// to be defined by the bootstrap loader by design. It is in the probe on
// purpose: a version of this check that only listed classes which must load
// would pass against a VM that resolves everything, and could not tell a real
// agreement from a degenerate one.
public class MU {
    public static void main(String[] a) throws Exception {
        for (String n : new String[] {
                "sun.reflect.misc.MethodUtil",
                "sun.reflect.misc.ReflectUtil",
                "sun.reflect.misc.Trampoline",
                "javax.management.modelmbean.RequiredModelMBean" }) {
            try {
                Class<?> c = Class.forName(n);
                System.out.println("OK    " + n + "  loader=" + c.getClassLoader()
                        + "  super=" + c.getSuperclass());
            } catch (Throwable t) {
                System.out.println("FAIL  " + n + "  -> " + t.getClass().getName()
                        + ": " + t.getMessage());
                for (Throwable cz = t.getCause(); cz != null; cz = cz.getCause()) {
                    System.out.println("        caused by " + cz.getClass().getName()
                            + ": " + cz.getMessage());
                }
            }
        }
    }
}
