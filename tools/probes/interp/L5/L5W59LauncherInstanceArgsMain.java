// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 59, lane L5 -- JDK 25 starts an INSTANCE
// `main(String[])`, and prefers it to a STATIC `main()` with no parameters
// (`MethodFinder.findMainMethod` looks for `main(String[])` first, of any
// non-private access): the instance is made by the no-argument constructor,
// after the class's `<clinit>`, and receives the command-line arguments
// (`docs/internal/fixed-bugs/interpreter-L5-the-launcher-rejects-an-instance-main-and-misreports-main-class-errors-FIXED-20261023.md`).
//
// Run (no setup; the class itself is the launch):
//   javac -d out L5W59LauncherInstanceArgsMain.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W59LauncherInstanceArgsMain a b; echo "exit=$?"
//
// Expected HotSpot 25.0.3 output (`java`, default flags; compare verbatim):
//   clinit
//   ctor
//   instance main(String[]) args=2 first=a
//   exit=0
// (With no arguments: `instance main(String[]) args=0 first=-`.)
//
// CratonVM before wave 59 (debug build, Windows, measured): `clinit`, then
// an uncaught exception out of `main(String[])` invoked with the argument
// array as its receiver, exit 1.

public class L5W59LauncherInstanceArgsMain {
    static {
        System.out.println("clinit");
    }

    L5W59LauncherInstanceArgsMain() {
        System.out.println("ctor");
    }

    static void main() {
        System.out.println("static no-arg main ran (wrong: main(String[]) comes first)");
    }

    protected void main(String[] args) {
        System.out.println("instance main(String[]) args=" + args.length
                + " first=" + (args.length > 0 ? args[0] : "-"));
    }
}
