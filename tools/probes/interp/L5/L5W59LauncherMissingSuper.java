// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 59, lane L5 -- a main class whose SUPERCLASS is
// missing at run time: `LauncherHelper.loadMainClass`'s `Class.forName`
// throws `NoClassDefFoundError` (the superclass's internal name), which the
// launcher reports as `java.launcher.cls.error1` with that cause
// (`docs/internal/fixed-bugs/interpreter-L5-the-launcher-rejects-an-instance-main-and-misreports-main-class-errors-FIXED-20261023.md`).
//
// SETUP and run (`L5W59LauncherMissingSuperGone` must be absent at run time):
//   javac -d out L5W59LauncherMissingSuper.java
//   rm 'out/L5W59LauncherMissingSuperGone.class'
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W59LauncherMissingSuper; echo "exit=$?"
//
// Expected HotSpot 25.0.3 output (stderr, then the exit code):
//   Error: Could not find or load main class L5W59LauncherMissingSuper
//   Caused by: java.lang.NoClassDefFoundError: L5W59LauncherMissingSuperGone
//   exit=1
//
// Without the setup step HotSpot prints `main ran`, exit 0.
//
// CratonVM before wave 59 (debug build, Windows, measured): `Could not find
// or load main class L5W59LauncherMissingSuper: class file error: class not
// found: L5W59LauncherMissingSuperGone`, exit 1.

public class L5W59LauncherMissingSuper extends L5W59LauncherMissingSuperGone {
    public static void main(String[] args) {
        System.out.println("main ran");
    }
}

class L5W59LauncherMissingSuperGone {
}
