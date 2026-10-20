// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 59, lane L5 -- an INSTANCE `main` in a class whose
// no-argument constructor is PRIVATE cannot be started:
// `LauncherHelper.validateMainMethod` prints `java.launcher.cls.error6`
// (`docs/internal/fixed-bugs/interpreter-L5-the-launcher-rejects-an-instance-main-and-misreports-main-class-errors-FIXED-20261023.md`).
//
// Run (no setup; the class itself is the launch):
//   javac -d out L5W59LauncherPrivateCtor.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W59LauncherPrivateCtor; echo "exit=$?"
//
// Expected HotSpot 25.0.3 output (stderr, then the exit code):
//   Error: no non-private zero argument constructor found in class L5W59LauncherPrivateCtor
//   remove private from existing constructor or define as:
//      public L5W59LauncherPrivateCtor()
//   exit=1
//
// CratonVM before wave 59 (both modes): `Exception in thread "main"
// java.lang.NoSuchMethodError: 'void
// L5W59LauncherPrivateCtor.main(java.lang.String[])'`, exit 1.

public class L5W59LauncherPrivateCtor {
    private L5W59LauncherPrivateCtor() {
        System.out.println("private ctor ran");
    }

    void main() {
        System.out.println("instance main ran");
    }
}
