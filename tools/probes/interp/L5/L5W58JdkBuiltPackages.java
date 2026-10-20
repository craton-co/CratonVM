// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 58, lane L5 -- the application loader's `Package`
// of a class-path class is the one the JDK's `BuiltinClassLoader.
// definePackage(pn, man, url)` builds from the JAR's manifest
// (`CRATONVM_JDK_BUILT_PACKAGES`; the proposal
// `docs/internal/fixed-bugs/interpreter-i57-L5-proposal-packages-are-built-by-the-jdks-define-package-DONE-20261014.md`).
//
// Rows (the first call, and again last):
//   main-*     `getPackage()` of the main class: the per-entry section
//              (`Name: l5w58pk/`) wins over the main section, a folded
//              continuation line, an attribute name in lower case, a value
//              with an extra leading and two trailing spaces (kept verbatim)
//   same-*     `getPackage() == getDefinedPackage(name)`, the package's
//              module is the loader's unnamed module
//   early-*    `getDefinedPackage("l5w58pk.sub")` before any class of it is
//              loaded, then `Sub`'s package (main section only) and the
//              same question again
//   never-*    `getDefinedPackage("l5w58pk.never")`: its class file is in the
//              JAR, no class of it is ever loaded
//   listed-*   the unnamed module's `getPackages()` holds `l5w58pk`
//
// SETUP and run (the class path must be the JAR; `m.txt` keeps its blank
// line before `Name:`, the leading space of the continuation line and the
// spaces around `padded`):
//   printf 'package l5w58pk.sub;\npublic class Sub {}\n' > Sub.java
//   printf 'package l5w58pk.never;\npublic class Never {}\n' > Never.java
//   javac -d out L5W58JdkBuiltPackages.java Sub.java Never.java
//   printf 'Implementation-Title: main-title\nImplementation-Version: 1.0\nimplementation-vendor: lower-vendor\nSpecification-Title:  padded  \n\nName: l5w58pk/\nImplementation-Version: 9.9\nSpecification-Vendor: long ven\n dor\n' > m.txt
//   jar cfm p.jar m.txt -C out l5w58pk
//   java -cp p.jar l5w58pk.L5W58JdkBuiltPackages
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp p.jar l5w58pk.L5W58JdkBuiltPackages
//
// Expected HotSpot 25.0.3 output (`java`, default flags; compare verbatim):
//   main-1=l5w58pk title=main-title ver=9.9 vendor=lower-vendor specVendor=long vendor sealed=false class=java.lang.Package
//   same-1=true module=true
//   early-1=null then=l5w58pk.sub title=main-title ver=1.0 vendor=lower-vendor specVendor=null sealed=false again=true
//   never-1=null
//   listed-1=true
//   main-2=l5w58pk title=main-title ver=9.9 vendor=lower-vendor specVendor=long vendor sealed=false class=java.lang.Package
//   same-2=true module=true
//   early-2=l5w58pk.sub then=l5w58pk.sub title=main-title ver=1.0 vendor=lower-vendor specVendor=null sealed=false again=true
//   never-2=null
//   listed-2=true

package l5w58pk;

import java.util.concurrent.Callable;

public class L5W58JdkBuiltPackages {
    public static class Holder {
    }

    static String pkg(Package p) {
        if (p == null) {
            return "null";
        }
        return p.getName() + " title=" + p.getImplementationTitle() + " ver=" + p.getImplementationVersion()
                + " vendor=" + p.getImplementationVendor() + " specVendor=" + p.getSpecificationVendor()
                + " specTitle=[" + p.getSpecificationTitle() + "] sealed=" + p.isSealed();
    }

    static String attempt(Callable<Object> c) {
        try {
            return String.valueOf(c.call());
        } catch (Throwable t) {
            return t.getClass().getName() + " msg=" + t.getMessage();
        }
    }

    static String name(Package p) {
        return p == null ? "null" : p.getName();
    }

    public static void main(String[] args) throws Exception {
        ClassLoader app = ClassLoader.getSystemClassLoader();
        for (String tag : new String[] {"1", "2"}) {
            System.out.println("main-" + tag + "=" + attempt(() -> {
                Package p = L5W58JdkBuiltPackages.class.getPackage();
                return pkg(p) + " class=" + p.getClass().getName();
            }));
            System.out.println("same-" + tag + "=" + attempt(() -> {
                Package p = L5W58JdkBuiltPackages.class.getPackage();
                return (p == app.getDefinedPackage("l5w58pk")) + " module="
                        + (p.getClass().getMethod("getName") != null
                                && L5W58JdkBuiltPackages.class.getModule() == app.getUnnamedModule());
            }));
            System.out.println("early-" + tag + "=" + attempt(() -> {
                String before = name(app.getDefinedPackage("l5w58pk.sub"));
                Class<?> sub = Class.forName("l5w58pk.sub.Sub", false, app);
                Package p = sub.getPackage();
                return before + " then=" + pkg(p) + " again=" + (p == app.getDefinedPackage("l5w58pk.sub"));
            }));
            System.out.println("never-" + tag + "=" + attempt(() -> name(app.getDefinedPackage("l5w58pk.never"))));
            System.out.println("listed-" + tag + "=" + attempt(() ->
                    app.getUnnamedModule().getPackages().contains("l5w58pk")));
        }
    }
}
