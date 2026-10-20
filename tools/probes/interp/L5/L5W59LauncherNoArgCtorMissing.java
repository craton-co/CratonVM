// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 59, lane L5 -- an INSTANCE `main` in a class with
// no no-argument constructor cannot be started:
// `LauncherHelper.validateMainMethod`'s `getDeclaredConstructor()` fails and
// the launcher prints `java.launcher.cls.error6`
// (`docs/internal/fixed-bugs/interpreter-L5-the-launcher-rejects-an-instance-main-and-misreports-main-class-errors-FIXED-20261023.md`).
//
// Run (no setup; the class itself is the launch):
//   javac -d out L5W59LauncherNoArgCtorMissing.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W59LauncherNoArgCtorMissing; echo "exit=$?"
//
// Expected HotSpot 25.0.3 output (stderr, then the exit code):
//   Error: no non-private zero argument constructor found in class L5W59LauncherNoArgCtorMissing
//   remove private from existing constructor or define as:
//      public L5W59LauncherNoArgCtorMissing()
//   exit=1
//
// CratonVM before wave 59 (debug build, Windows, measured): `instance main
// ran`, exit 0 -- the launcher invoked `main(String[])` by name with the
// argument array in the receiver's slot.

public class L5W59LauncherNoArgCtorMissing {
    L5W59LauncherNoArgCtorMissing(int x) {
        System.out.println("ctor " + x);
    }

    void main(String[] args) {
        System.out.println("instance main ran");
    }
}
