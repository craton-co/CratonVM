// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 59, lane L5 -- an INSTANCE `main` in a non-static
// member (inner) class cannot be started: `LauncherHelper.validateMainMethod`
// prints `java.launcher.cls.error7`. The top-level class's own launch runs a
// STATIC `main()` with no parameters (JEP 512), which starts
// (`docs/internal/fixed-bugs/interpreter-L5-the-launcher-rejects-an-instance-main-and-misreports-main-class-errors-FIXED-20261023.md`).
//
// Run (no setup):
//   javac -d out L5W59LauncherInnerMain.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W59LauncherInnerMain; echo "exit=$?"
//
// Expected HotSpot 25.0.3 output (stdout, then the exit code):
//   static no-arg main ran
//   exit=0
//
// Second launch, the inner class:
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out 'L5W59LauncherInnerMain$Inner'; echo "exit=$?"
// HotSpot 25.0.3 (stderr; the first line ends in one space, as in
// `launcher.properties`):
//   Error: non-static inner class L5W59LauncherInnerMain$Inner constructor can not be invoked 
//   make inner class static or move inner class out to separate source file
//   exit=1
//
// CratonVM before wave 59 (both modes): `Exception in thread "main"
// java.lang.NoSuchMethodError: 'void
// L5W59LauncherInnerMain.main(java.lang.String[])'` (and the same for
// `$Inner`), exit 1.

public class L5W59LauncherInnerMain {
    static void main() {
        System.out.println("static no-arg main ran");
    }

    class Inner {
        void main() {
            System.out.println("inner instance main ran");
        }
    }
}
