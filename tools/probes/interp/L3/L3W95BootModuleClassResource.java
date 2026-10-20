// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4, wave 95, lane L3 (`L3W94CallerSensitiveMatrix` row
// `res-class-direct`): `Class.getResource` of a class of a named BOOT module.
// JDK 25's body (it runs in both modes; the VM's `Class.getResource` row is a
// `Bridge` the bytecode beats) calls `BootLoader.findResource(mn, name)`,
// which asks the boot `BuiltinClassLoader`'s `nameToModule`. Only `--jdk-only`
// fills that map, so `--compatible` answered null for every name. HotSpot
// answers module `mn`'s `ModuleReader.find(name)`: the `jrt:` URL of a
// resource the module holds, null for one it does not (even when another
// module holds it).
//
// Run (both modes must print the HotSpot lines):
//   javac -d out L3W95BootModuleClassResource.java
//   cratonvm --jdk-only   -cp out L3W95BootModuleClassResource
//   cratonvm --compatible -cp out L3W95BootModuleClassResource
//   java                  -cp out L3W95BootModuleClassResource
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// object-class: found
// object-class-url: jrt:/java.base/java/lang/Object.class
// string-absolute: found
// thread-class: found
// logger-class: found
// missing-class: null
// other-module-class: null
// HOTSPOT_EXPECTED_END
import java.net.URL;

public class L3W95BootModuleClassResource {
    interface Row {
        Object run() throws Throwable;
    }

    static void row(String name, Row r) {
        String out;
        try {
            out = String.valueOf(r.run());
        } catch (Throwable t) {
            out = t.getMessage() == null ? t.getClass().getName()
                    : t.getClass().getName() + ": " + t.getMessage();
        }
        System.out.println(name + ": " + out);
    }

    static String found(URL u) {
        return u == null ? "null" : "found";
    }

    public static void main(String[] args) {
        row("object-class", () -> found(Object.class.getResource("Object.class")));
        row("object-class-url", () -> Object.class.getResource("Object.class"));
        row("string-absolute", () -> found(Object.class.getResource("/java/lang/String.class")));
        row("thread-class", () -> found(Thread.class.getResource("Thread.class")));
        row("logger-class", () -> found(java.util.logging.Logger.class.getResource("Logger.class")));
        row("missing-class", () -> found(Object.class.getResource("NoSuchClass95.class")));
        // java.sql's class asked through java.base: not java.base's resource.
        row("other-module-class", () -> found(Object.class.getResource("/java/sql/Date.class")));
    }
}
