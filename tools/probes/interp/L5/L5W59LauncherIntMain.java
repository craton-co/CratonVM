// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 59, lane L5 -- a `main(String[])` that does not
// return `void` is not a main method (`MethodFinder.isValidMainMethod`), and
// with no `main()` either the launcher prints `java.launcher.cls.error2`
// (`docs/internal/fixed-bugs/interpreter-L5-the-launcher-rejects-an-instance-main-and-misreports-main-class-errors-FIXED-20261023.md`).
//
// Run (no setup; the class itself is the launch):
//   javac -d out L5W59LauncherIntMain.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W59LauncherIntMain; echo "exit=$?"
//
// Expected HotSpot 25.0.3 output (stderr, then the exit code):
//   Error: Main method not found in class L5W59LauncherIntMain, please define the main method as:
//      public static void main(String[] args)
//   or a JavaFX application class must extend javafx.application.Application
//   exit=1
//
// CratonVM before wave 59 (both modes): `Exception in thread "main"
// java.lang.NoSuchMethodError: 'void
// L5W59LauncherIntMain.main(java.lang.String[])'`, exit 1.

public class L5W59LauncherIntMain {
    public static int main(String[] args) {
        System.out.println("int main ran");
        return 0;
    }
}
