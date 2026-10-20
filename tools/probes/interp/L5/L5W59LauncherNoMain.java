// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 59, lane L5 -- the launcher's message for a main
// class with no `main` at all (`LauncherHelper.validateMainMethod`,
// `java.launcher.cls.error2`), and for a main class that does not exist
// (`loadMainClass`, `java.launcher.cls.error1`)
// (`docs/internal/fixed-bugs/interpreter-L5-the-launcher-rejects-an-instance-main-and-misreports-main-class-errors-FIXED-20261023.md`).
//
// Run (no setup; the class itself is the launch):
//   javac -d out L5W59LauncherNoMain.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W59LauncherNoMain; echo "exit=$?"
//
// Expected HotSpot 25.0.3 output (`java`, default flags; stderr, then the
// exit code; compare verbatim):
//   Error: Main method not found in class L5W59LauncherNoMain, please define the main method as:
//      public static void main(String[] args)
//   or a JavaFX application class must extend javafx.application.Application
//   exit=1
//
// Second launch, a class that does not exist:
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W59LauncherNoSuchClass; echo "exit=$?"
// HotSpot 25.0.3 (stderr):
//   Error: Could not find or load main class L5W59LauncherNoSuchClass
//   Caused by: java.lang.ClassNotFoundException: L5W59LauncherNoSuchClass
//   exit=1
//
// CratonVM before wave 59 (debug build, Windows, both modes): `clinit ran
// (HotSpot never prints this)`, `Exception in thread "main"
// java.lang.NoSuchMethodError: 'void
// L5W59LauncherNoMain.main(java.lang.String[])'` and `Could not find or load
// main class L5W59LauncherNoSuchClass: class file error: class not found:
// L5W59LauncherNoSuchClass`, exit 1.

public class L5W59LauncherNoMain {
    static {
        // HotSpot loads the class without initialising it before the check.
        System.out.println("clinit ran (HotSpot never prints this)");
    }

    public static void notMain(String[] args) {
        System.out.println("notMain ran");
    }
}
