// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 56, lane L5 -- the packages of classes the boot
// loader defines from `-Xbootclasspath/a`
// (`docs/internal/fixed-bugs/interpreter-L5-boot-appended-classes-packages-differ-from-hotspot-FIXED-20261020.md`).
// This probe's own class (the unnamed package) and `l5w56boot.L5W56BootBy`
// (a named package) are both on the appended directory, so the boot loader
// defines them (the application loader delegates to it first).
//
// Rows (the first call, and again last):
//   self-loader-*   this class's loader: null (the boot loader)
//   self-pkg-*      `getPackage()` of this unnamed-package class
//   by-*            `L5W56BootBy`'s loader and `getPackage()` name
//   listed-*        does `Package.getPackages()` list `l5w56boot`
//   module-pkgs-*   this class's (unnamed) module's `getPackages()`
//                   restricted to the probe's packages (`""`, `l5w56boot`)
//
// SETUP and run (the class path must be the same DIRECTORY):
//   javac -d out L5W56BootAppendPackages.java l5w56boot/L5W56BootBy.java
//   java -Xbootclasspath/a:out -cp out L5W56BootAppendPackages
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -Xbootclasspath/a:out -cp out L5W56BootAppendPackages
//
// Expected HotSpot 25.0.3 output (`java`, default flags; compare verbatim):
//   self-loader-1=null
//   self-pkg-1=null
//   by-1=null 'l5w56boot'
//   listed-1=true
//   module-pkgs-1=[]
//   self-loader-2=null
//   self-pkg-2=null
//   by-2=null 'l5w56boot'
//   listed-2=true
//   module-pkgs-2=[]

import java.util.Arrays;
import java.util.TreeSet;

public class L5W56BootAppendPackages {
    static String attempt(java.util.concurrent.Callable<Object> c) {
        try {
            return String.valueOf(c.call());
        } catch (Throwable t) {
            return t.getClass().getName() + " msg=" + t.getMessage();
        }
    }

    // The loader's class, not its identity hash: a run without the
    // -Xbootclasspath/a setup prints the application loader, and its
    // hash differs from run to run.
    static String loaderName(ClassLoader cl) {
        return cl == null ? "null" : cl.getClass().getName();
    }

    static void rows(String tag) {
        System.out.println("self-loader-" + tag + "=" + attempt(() -> loaderName(L5W56BootAppendPackages.class.getClassLoader())));
        System.out.println("self-pkg-" + tag + "=" + attempt(() -> {
            Package p = L5W56BootAppendPackages.class.getPackage();
            return p == null ? "null" : "'" + p.getName() + "'";
        }));
        System.out.println("by-" + tag + "=" + attempt(() -> {
            Class<?> c = Class.forName("l5w56boot.L5W56BootBy");
            Package p = c.getPackage();
            return loaderName(c.getClassLoader()) + " " + (p == null ? "null" : "'" + p.getName() + "'");
        }));
        System.out.println("listed-" + tag + "=" + attempt(() ->
                Arrays.stream(Package.getPackages()).anyMatch(p -> p.getName().equals("l5w56boot"))));
        System.out.println("module-pkgs-" + tag + "=" + attempt(() -> {
            TreeSet<String> s = new TreeSet<>();
            for (String p : L5W56BootAppendPackages.class.getModule().getPackages()) {
                if (p.isEmpty() || p.equals("l5w56boot")) {
                    s.add("'" + p + "'");
                }
            }
            return s;
        }));
    }

    public static void main(String[] args) {
        rows("1");
        rows("2");
    }
}
