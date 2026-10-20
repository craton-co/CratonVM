// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 62, lane L5 -- the application loader defines a
// class-path package when it defines the package's first class
// (`BuiltinClassLoader.defineClass(cn, Resource)` -> `defineOrCheckPackage`),
// the next stage of
// `docs/internal/fixed-bugs/interpreter-i58-L5-proposal-a-class-path-package-is-defined-when-its-first-class-is-FIXED-20261014.md`.
//
// Rows (each first, and again last):
//   seal-*     `l5w62seal.A` comes from a JAR whose manifest seals the
//              package; `l5w62seal.B` from a second JAR. Loading B is a
//              sealing violation (`SecurityException` at B's definition), every
//              time it is asked for (HotSpot does not define it)
//   sealpkg-*  the sealed package's `isSealed()` and seal base
//   listed-*   the application loader's `getDefinedPackages()` names (sorted)
//              after classes of three packages were loaded, before any
//              `getPackage` call
//   pkgs-*     `Package.getPackages()` holds those names
//   manifest-* the implementation attributes of `l5w62ord.Ord`'s package
//              (main section of its JAR)
//
// SETUP and run (all paths relative to one directory):
//   mkdir -p src/l5w62seal src/l5w62ord src/l5w62other
//   printf 'package l5w62seal;\npublic class A { public static String hi() { return "A"; } }\n' > src/l5w62seal/A.java
//   printf 'package l5w62seal;\npublic class B { public static String hi() { return "B"; } }\n' > src/l5w62seal/B.java
//   printf 'package l5w62ord;\npublic class Ord {}\n' > src/l5w62ord/Ord.java
//   printf 'package l5w62other;\npublic class Other {}\n' > src/l5w62other/Other.java
//   javac -d out L5W62ClassPathPackageDefine.java src/l5w62seal/*.java src/l5w62ord/*.java src/l5w62other/*.java
//   printf 'Sealed: true\nImplementation-Title: seal-title\n' > ma.txt
//   printf 'Implementation-Title: ord-title\nImplementation-Version: 6.2\n' > mo.txt
//   jar cfm a.jar ma.txt -C out l5w62seal/A.class
//   jar cf b.jar -C out l5w62seal/B.class
//   jar cfm o.jar mo.txt -C out l5w62ord -C out l5w62other
//   jar cf main.jar -C out l5w62main
//   java -cp main.jar:a.jar:b.jar:o.jar l5w62main.L5W62ClassPathPackageDefine
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp main.jar:a.jar:b.jar:o.jar l5w62main.L5W62ClassPathPackageDefine
//   (`;` for `:` on Windows)
//
// Expected HotSpot 25.0.3 output (`java`, default flags; compare verbatim):
//   seal-1=A then=java.lang.SecurityException msg=sealing violation: package l5w62seal is sealed
//   sealpkg-1=sealed=true base=a.jar
//   listed-1=[l5w62main, l5w62ord, l5w62other, l5w62seal]
//   pkgs-1=true
//   manifest-1=ord-title 6.2
//   seal-2=A then=java.lang.SecurityException msg=sealing violation: package l5w62seal is sealed
//   sealpkg-2=sealed=true base=a.jar
//   listed-2=[l5w62main, l5w62ord, l5w62other, l5w62seal]
//   pkgs-2=true
//   manifest-2=ord-title 6.2
//
// CratonVM (debug build, Windows, wave 62): `--jdk-only` printed HotSpot's
// lines already (the JDK's own `BuiltinClassLoader.defineClass` defines `B`
// there); `--compatible` printed `seal-N=A then=l5w62seal.B` without
// `CRATONVM_CLASS_PATH_PACKAGE_AT_LINK` and HotSpot's lines with it (default
// and `--nojit`). Positive control: `CRATONVM_DBG_BUILTIN_LOADER_PACKAGES=1`
// prints `[builtin-loader-packages] at-link "l5w62seal" manifest=true`, then
// `... "l5w62seal" sealing violation` once per refused link of `B`.

package l5w62main;

import java.net.URL;
import java.util.ArrayList;
import java.util.Collections;
import java.util.List;
import java.util.concurrent.Callable;

public class L5W62ClassPathPackageDefine {
    static String attempt(Callable<Object> c) {
        try {
            return String.valueOf(c.call());
        } catch (Throwable t) {
            return t.getClass().getName() + " msg=" + t.getMessage();
        }
    }

    static List<String> ours(Package[] ps) {
        List<String> out = new ArrayList<>();
        for (Package p : ps) {
            if (p.getName().startsWith("l5w62")) {
                out.add(p.getName());
            }
        }
        Collections.sort(out);
        return out;
    }

    public static void main(String[] args) throws Exception {
        ClassLoader app = ClassLoader.getSystemClassLoader();
        for (String tag : new String[] {"1", "2"}) {
            System.out.println("seal-" + tag + "=" + attempt(() -> {
                Class<?> a = Class.forName("l5w62seal.A", true, app);
                String first = String.valueOf(a.getMethod("hi").invoke(null));
                String then = attempt(() -> Class.forName("l5w62seal.B", true, app).getName());
                return first + " then=" + then;
            }));
            System.out.println("sealpkg-" + tag + "=" + attempt(() -> {
                Package p = app.getDefinedPackage("l5w62seal");
                if (p == null) {
                    return "null";
                }
                URL base = Class.forName("l5w62seal.A", false, app)
                        .getProtectionDomain().getCodeSource().getLocation();
                String file = base.getPath().substring(base.getPath().lastIndexOf('/') + 1);
                return "sealed=" + p.isSealed() + " base=" + (p.isSealed(base) ? file : "other");
            }));
            if (tag.equals("1")) {
                Class.forName("l5w62ord.Ord", false, app);
                Class.forName("l5w62other.Other", false, app);
            }
            System.out.println("listed-" + tag + "=" + attempt(() -> ours(app.getDefinedPackages())));
            System.out.println("pkgs-" + tag + "=" + attempt(() ->
                    ours(Package.getPackages()).equals(ours(app.getDefinedPackages()))));
            System.out.println("manifest-" + tag + "=" + attempt(() -> {
                Package p = Class.forName("l5w62ord.Ord", false, app).getPackage();
                return p.getImplementationTitle() + " " + p.getImplementationVersion();
            }));
        }
    }
}
