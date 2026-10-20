// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 57, lane L5 -- the `Package` of a class loaded
// from a class-path JAR whose manifest says `Sealed: true` is sealed, with the
// JAR's URL as its seal base (`BuiltinClassLoader.definePackage(pn, man,
// url)`); every other manifest attribute already matched.
//
// Rows (the first call, and again last):
//   class-*    `getPackage()`: name, implementation / specification
//              attributes, `isSealed()`
//   defined-*  the application loader's `getDefinedPackage`, the same
//   sealurl-*  `isSealed(<the class's code source URL>)`
//
// SETUP and run (the class path must be the JAR):
//   javac -d out L5W57SealedJarPackage.java
//   printf 'Implementation-Title: l5w57-impl\nImplementation-Version: 1.2.3\nSpecification-Title: l5w57-spec\nSealed: true\n' > m.txt
//   jar cfm p.jar m.txt -C out l5w57seal
//   java -cp p.jar l5w57seal.L5W57SealedJarPackage
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp p.jar l5w57seal.L5W57SealedJarPackage
//
// Expected HotSpot 25.0.3 output (`java`, default flags; compare verbatim):
//   class-1=l5w57seal title=l5w57-impl ver=1.2.3 spec=l5w57-spec sealed=true
//   defined-1=l5w57seal title=l5w57-impl ver=1.2.3 spec=l5w57-spec sealed=true
//   sealurl-1=true
//   class-2=l5w57seal title=l5w57-impl ver=1.2.3 spec=l5w57-spec sealed=true
//   defined-2=l5w57seal title=l5w57-impl ver=1.2.3 spec=l5w57-spec sealed=true
//   sealurl-2=true
//
// CratonVM before wave 57 (debug build, Windows, both modes, measured): the
// same lines with `sealed=false` and `sealurl-N=false`.

package l5w57seal;

import java.net.URL;

public class L5W57SealedJarPackage {
    static String pkg(Package p) {
        if (p == null) {
            return "null";
        }
        return p.getName() + " title=" + p.getImplementationTitle() + " ver=" + p.getImplementationVersion()
                + " spec=" + p.getSpecificationTitle() + " sealed=" + p.isSealed();
    }

    static String attempt(java.util.concurrent.Callable<Object> c) {
        try {
            return String.valueOf(c.call());
        } catch (Throwable t) {
            return t.getClass().getName() + " msg=" + t.getMessage();
        }
    }

    public static void main(String[] args) {
        for (String tag : new String[] {"1", "2"}) {
            System.out.println("class-" + tag + "=" + attempt(() -> pkg(L5W57SealedJarPackage.class.getPackage())));
            System.out.println("defined-" + tag + "=" + attempt(() ->
                    pkg(ClassLoader.getSystemClassLoader().getDefinedPackage("l5w57seal"))));
            System.out.println("sealurl-" + tag + "=" + attempt(() -> {
                URL u = L5W57SealedJarPackage.class.getProtectionDomain().getCodeSource().getLocation();
                return L5W57SealedJarPackage.class.getPackage().isSealed(u);
            }));
        }
    }
}
