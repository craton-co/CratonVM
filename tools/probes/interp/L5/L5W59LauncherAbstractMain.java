// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 59, lane L5 -- an INSTANCE `main` in an abstract
// class cannot be started: `LauncherHelper.validateMainMethod` prints
// `java.launcher.cls.error8` (a static `main` in an abstract class still runs)
// (`docs/internal/fixed-bugs/interpreter-L5-the-launcher-rejects-an-instance-main-and-misreports-main-class-errors-FIXED-20261023.md`).
//
// Run (no setup; the class itself is the launch):
//   javac -d out L5W59LauncherAbstractMain.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W59LauncherAbstractMain; echo "exit=$?"
//
// Expected HotSpot 25.0.3 output (stderr, then the exit code):
//   Error: abstract class L5W59LauncherAbstractMain can not be instantiated
//   please use a concrete class
//   exit=1
//
// CratonVM before wave 59 (both modes): `Exception in thread "main"
// java.lang.NoSuchMethodError: 'void
// L5W59LauncherAbstractMain.main(java.lang.String[])'`, exit 1.

public abstract class L5W59LauncherAbstractMain {
    void main() {
        System.out.println("abstract instance main ran");
    }
}
