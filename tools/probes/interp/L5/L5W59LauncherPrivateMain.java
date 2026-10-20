// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 59, lane L5 -- a PRIVATE `static void
// main(String[])` is not a main method (`MethodFinder.isValidMainMethod`):
// the launcher prints `java.launcher.cls.error2` and never runs it
// (`docs/internal/fixed-bugs/interpreter-L5-the-launcher-rejects-an-instance-main-and-misreports-main-class-errors-FIXED-20261023.md`).
//
// Run (no setup; the class itself is the launch):
//   javac -d out L5W59LauncherPrivateMain.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W59LauncherPrivateMain; echo "exit=$?"
//
// Expected HotSpot 25.0.3 output (stderr, then the exit code):
//   Error: Main method not found in class L5W59LauncherPrivateMain, please define the main method as:
//      public static void main(String[] args)
//   or a JavaFX application class must extend javafx.application.Application
//   exit=1
//
// CratonVM before wave 59 (both modes): `private main ran`, exit 0 (the
// launcher invoked the method by name, whatever its access).

public class L5W59LauncherPrivateMain {
    private static void main(String[] args) {
        System.out.println("private main ran");
    }
}
