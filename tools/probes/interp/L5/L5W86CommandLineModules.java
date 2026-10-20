// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4, wave 86, lane L5 -- the launcher's module options as
// a class-path program sees them: --add-opens and --add-exports to
// ALL-UNNAMED (Module.isOpen/isExported, setAccessible, a public class of an
// exported internal package), --add-modules of a module outside the default
// root set, and the refusal text for a package that stays closed.
// (--limit-modules is probed by L5W87ModuleOptions:
// docs/internal/fixed-bugs/interpreter-i86-L5-limit-modules-patch-module-and-the-default-root-set-FIXED-20261120.md.)
//
// Run (the options are part of the probe):
//   $J/javac -d out L5W86CommandLineModules.java
//   OPTS="--add-opens java.base/java.lang=ALL-UNNAMED --add-exports java.base/jdk.internal.misc=ALL-UNNAMED --add-modules jdk.incubator.vector"
//   $J/java $OPTS -cp out L5W86CommandLineModules                  # HotSpot
//   $B --java-home $J/.. $OPTS -cp out L5W86CommandLineModules
//   $B --compatible --java-home $J/.. $OPTS -cp out L5W86CommandLineModules
// The first expected line is the boot layer's stderr warning (the battery
// compares stdout and stderr merged; measured on Temurin 25.0.3).
//
// HOTSPOT_EXPECTED_BEGIN
// WARNING: Using incubator modules: jdk.incubator.vector
// java.lang open=true exported=true
// String.value accessible: [B
// jdk.internal.misc exported=true open=false
// VM.isBooted: true
// ArrayList.elementData: java.lang.reflect.InaccessibleObjectException: Unable to make field transient java.lang.Object[] java.util.ArrayList.elementData accessible: module java.base does not "opens java.util" to unnamed module @?
// boot layer java.logging=true java.sql=true jdk.incubator.vector=true java.desktop=true
// HOTSPOT_EXPECTED_END

import java.lang.reflect.Field;

public class L5W86CommandLineModules {
    /** Identity hashes (`@6ff3c5b5`, `@0x6ff3c5b5`) are printed `@?`. */
    static String stable(Object o) {
        return String.valueOf(o).replaceAll("@(0x)?[0-9a-fA-F]+", "@?");
    }

    public static void main(String[] args) {
        try {
            run();
        } catch (Throwable t) {
            System.out.println("uncaught: " + t.getClass().getName() + ": " + stable(t.getMessage()));
        }
    }

    static void run() throws Exception {
        Module me = L5W86CommandLineModules.class.getModule();
        Module base = Object.class.getModule();
        System.out.println("java.lang open=" + base.isOpen("java.lang", me)
                + " exported=" + base.isExported("java.lang", me));
        Field value = String.class.getDeclaredField("value");
        value.setAccessible(true);
        System.out.println("String.value accessible: " + value.get("x").getClass().getName());

        System.out.println("jdk.internal.misc exported=" + base.isExported("jdk.internal.misc", me)
                + " open=" + base.isOpen("jdk.internal.misc", me));
        Class<?> vm = Class.forName("jdk.internal.misc.VM");
        System.out.println("VM.isBooted: " + vm.getMethod("isBooted").invoke(null));

        Field elementData = java.util.ArrayList.class.getDeclaredField("elementData");
        try {
            elementData.setAccessible(true);
            System.out.println("ArrayList.elementData: ok");
        } catch (Throwable t) {
            System.out.println("ArrayList.elementData: " + t.getClass().getName() + ": "
                    + stable(t.getMessage()));
        }

        ModuleLayer boot = ModuleLayer.boot();
        System.out.println("boot layer java.logging=" + boot.findModule("java.logging").isPresent()
                + " java.sql=" + boot.findModule("java.sql").isPresent()
                + " jdk.incubator.vector=" + boot.findModule("jdk.incubator.vector").isPresent()
                + " java.desktop=" + boot.findModule("java.desktop").isPresent());
    }
}
