// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 58, lane L5 -- JDK 25's launcher starts an
// INSTANCE `main` with no parameters (JEP 512, final in JDK 25):
// `LauncherHelper.validateMainMethod` finds it through
// `MethodFinder.findMainMethod`, records `isStaticMain = false` /
// `noArgMain = true`, and `java.c` calls it on an instance made by the
// no-argument constructor
// (`docs/internal/fixed-bugs/interpreter-L5-the-launcher-rejects-an-instance-main-and-misreports-main-class-errors-FIXED-20261023.md`).
//
// Run (no setup; the class itself is the launch):
//   javac -d out L5W58LauncherInstanceMain.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W58LauncherInstanceMain; echo "exit=$?"
//
// Expected HotSpot 25.0.3 output (`java`, default flags; compare verbatim):
//   ctor-1
//   instance main ran L5W58LauncherInstanceMain this=true
//   exit=0
//
// CratonVM (debug build, Windows, both modes, wave 58, measured): `Exception
// in thread "main" java.lang.NoSuchMethodError: 'void
// L5W58LauncherInstanceMain.main(java.lang.String[])'`, exit 1.
// Since wave 59 (lane L5): as HotSpot, both modes, with `CRATONVM_LAUNCHER_HELPER`
// on and off.

public class L5W58LauncherInstanceMain {
    static int made;

    L5W58LauncherInstanceMain() {
        made++;
        System.out.println("ctor-" + made);
    }

    void main() {
        System.out.println("instance main ran " + getClass().getName() + " this=" + (made == 1));
    }
}
