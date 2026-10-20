// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2, wave 55, lane L5 -- the application loader's unnamed
// module lists the packages of the class-path classes that loader defined.
// `Module.getPackages()` of an unnamed module is the JDK's
// `loader.packages()`, a stream over the loader's own `packages` map, which
// HotSpot fills in `BuiltinClassLoader.defineClass` -> `getAndVerifyPackage`
// -> `definePackage`. CratonVM defines a class-path class itself and never
// wrote the map, so the set was empty
// (`docs/internal/fixed-bugs/interpreter-L5-the-application-loaders-unnamed-module-lists-no-packages-FIXED-20261019.md`).
//
// Rows (the unnamed-module rows first, on a cold VM, and again last):
//   unnamed-*   `Main.class.getModule().getPackages().contains("")`, and the
//               set's size (the probe's only class-path package)
//   defined-*   `getDefinedPackages()` of the application loader: is the
//               unnamed package listed (before anything asked for it by name)
//   platform-*  the platform loader after `java.sql.Date` is loaded: its
//               unnamed module's packages are its defined packages
//               (`java.sql` is a named module's: listed or not)
//   same-*      `getDefinedPackage("")` is the very object
//               `Class.getPackage()` answers (one `Package` per loader and
//               name, now that the loader's map holds it too)
//   all-*       the deprecated `Package.getPackages()` (the caller's loader's
//               `getPackages()`: its chain's `packages()` and the boot
//               loader's) lists the unnamed package -- the very object
//               `Class.getPackage()` answers (`same`) -- `java.lang`, `java.sql`
//
// Run (no setup):
//   javac -d out L5W55UnnamedModulePackages.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W55UnnamedModulePackages
//
// Expected HotSpot 25.0.3 output (`java`, default flags; compare verbatim):
//   unnamed-1=contains-empty=true size=1
//   defined-1=lists-empty=true
//   platform-1=unnamed-has-sql=false defined-has-sql=true
//   same-1=byName-null=false class-same=true
//   all-1=empty=true same=true java.lang=true java.sql=true
//   unnamed-2=contains-empty=true size=1
//   defined-2=lists-empty=true
//   platform-2=unnamed-has-sql=false defined-has-sql=true
//   same-2=byName-null=false class-same=true
//   all-2=empty=true same=true java.lang=true java.sql=true
// CratonVM before the fix (debug build of 63d2087232, Windows; `all-N`
// measured with `CRATONVM_BUILTIN_LOADER_PACKAGE_SYNC=0`: `empty=false`):
//   `--jdk-only`  unnamed-N=contains-empty=false size=0, defined-1=lists-empty=false,
//                 platform-N=... defined-has-sql=false
//   `--compatible` unnamed-N=contains-empty=false size=63 (the registry's
//                 answer for the module named ""), the same defined-1 and
//                 platform rows
// Positive control: `CRATONVM_DBG_BUILTIN_LOADER_PACKAGES=1` prints
// `[builtin-loader-packages] ns=<n>: <k> package(s) to sync` when a listing
// puts new packages into a built-in loader's map;
// `CRATONVM_BUILTIN_LOADER_PACKAGE_SYNC=0` restores the rows above.

import java.util.Set;
import java.util.TreeSet;

public class L5W55UnnamedModulePackages {
    static void unnamed(String tag) {
        Set<String> pkgs = L5W55UnnamedModulePackages.class.getModule().getPackages();
        System.out.println("unnamed-" + tag + "=contains-empty=" + pkgs.contains("")
                + " size=" + pkgs.size());
    }

    static void defined(String tag) {
        ClassLoader app = L5W55UnnamedModulePackages.class.getClassLoader();
        boolean listed = false;
        for (Package p : app.getDefinedPackages()) {
            if (p.getName().isEmpty()) {
                listed = true;
            }
        }
        System.out.println("defined-" + tag + "=lists-empty=" + listed);
    }

    static void platform(String tag) {
        ClassLoader platform = ClassLoader.getPlatformClassLoader();
        Set<String> mod = new TreeSet<>(platform.getUnnamedModule().getPackages());
        boolean sqlDefined = false;
        for (Package p : platform.getDefinedPackages()) {
            if (p.getName().equals("java.sql")) {
                sqlDefined = true;
            }
        }
        System.out.println("platform-" + tag + "=unnamed-has-sql=" + mod.contains("java.sql")
                + " defined-has-sql=" + sqlDefined);
    }

    static void same(String tag) {
        ClassLoader app = L5W55UnnamedModulePackages.class.getClassLoader();
        Package byName = app.getDefinedPackage("");
        Package viaClass = L5W55UnnamedModulePackages.class.getPackage();
        System.out.println("same-" + tag + "=byName-null=" + (byName == null)
                + " class-same=" + (byName == viaClass));
    }

    @SuppressWarnings("deprecation")
    static void all(String tag) {
        boolean empty = false, lang = false, sql = false, same = false;
        Package mine = L5W55UnnamedModulePackages.class.getPackage();
        for (Package p : Package.getPackages()) {
            empty |= p.getName().isEmpty();
            same |= p.getName().isEmpty() && p == mine;
            lang |= p.getName().equals("java.lang");
            sql |= p.getName().equals("java.sql");
        }
        System.out.println("all-" + tag + "=empty=" + empty + " same=" + same
                + " java.lang=" + lang + " java.sql=" + sql);
    }

    public static void main(String[] args) throws Exception {
        unnamed("1");
        defined("1");
        Class.forName("java.sql.Date");
        platform("1");
        same("1");
        all("1");
        unnamed("2");
        defined("2");
        platform("2");
        same("2");
        all("2");
    }
}
