// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 57, lane L5 -- the `Package` the boot loader
// defines for a class of an appended JAR in a directory whose name has a
// space: its location is the appended entry, so `BootLoader` reads the jar's
// manifest (`docs/internal/fixed-bugs/interpreter-L5-proposal-record-boot-append-package-locations-at-append-FIXED-20261021.md`,
// `CRATONVM_BOOT_APPEND_PACKAGE_TABLE`).
//
// Rows (the first call, and again last):
//   by-*      `l5w57boot.L5W57BootJarBy`'s loader (null: the boot loader)
//   listed-*  the `Package.getPackages()` element named `l5w57boot`: absent,
//             or its implementation title and vendor (the jar's manifest)
//
// SETUP and run (the appended jar's directory name has a space):
//   javac -d out L5W57BootAppendJarPackage.java
//   javac -d bootout l5w57boot/L5W57BootJarBy.java
//   printf 'Implementation-Title: l5w57-title\nImplementation-Vendor: l5w57-vendor\n' > m.txt
//   mkdir "boot dir" && jar cfm "boot dir/b.jar" m.txt -C bootout l5w57boot
//   java "-Xbootclasspath/a:boot dir/b.jar" -cp out L5W57BootAppendJarPackage
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] "-Xbootclasspath/a:boot dir/b.jar" -cp out L5W57BootAppendJarPackage
//
// Expected HotSpot 25.0.3 output (`java`, default flags; compare verbatim):
//   by-1=null
//   listed-1=l5w57-title l5w57-vendor
//   by-2=null
//   listed-2=l5w57-title l5w57-vendor
//
// CratonVM with `CRATONVM_BOOT_APPEND_PACKAGE_TABLE=0` (debug build, Windows,
// both modes, measured): `listed-N=null null` -- the location was parsed back
// out of a resource URL with its `%20` kept, so `BootLoader` found no jar to
// read a manifest from (the same jar under a space-free path matched). With
// the switch, HotSpot's lines.

import java.util.Arrays;

public class L5W57BootAppendJarPackage {
    static String attempt(java.util.concurrent.Callable<Object> c) {
        try {
            return String.valueOf(c.call());
        } catch (Throwable t) {
            return t.getClass().getName() + " msg=" + t.getMessage();
        }
    }

    static void rows(String tag) {
        System.out.println("by-" + tag + "=" + attempt(() ->
                Class.forName("l5w57boot.L5W57BootJarBy").getClassLoader()));
        System.out.println("listed-" + tag + "=" + attempt(() -> Arrays.stream(Package.getPackages())
                .filter(p -> p.getName().equals("l5w57boot"))
                .map(p -> p.getImplementationTitle() + " " + p.getImplementationVendor())
                .findFirst()
                .orElse("absent")));
    }

    public static void main(String[] args) {
        rows("1");
        rows("2");
    }
}
