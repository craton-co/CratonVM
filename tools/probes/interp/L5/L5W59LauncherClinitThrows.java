// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 59, lane L5 -- a main class whose `<clinit>`
// throws: the launcher's own `main` lookup initialises the class, and the
// `ExceptionInInitializerError` is reported once, as an uncaught exception of
// thread "main" (`Throwable.printStackTrace`: the error, then its `Caused
// by:` block) -- not twice
// (`docs/internal/fixed-bugs/interpreter-L5-the-launcher-rejects-an-instance-main-and-misreports-main-class-errors-FIXED-20261023.md`).
//
// Run (no setup; the class itself is the launch):
//   javac -d out L5W59LauncherClinitThrows.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W59LauncherClinitThrows; echo "exit=$?"
//
// Expected HotSpot 25.0.3 output (stderr, then the exit code; the line
// number is the `throw` line of this file):
//   Exception in thread "main" java.lang.ExceptionInInitializerError
//   Caused by: java.lang.IllegalStateException: boom
//   	at L5W59LauncherClinitThrows.<clinit>(L5W59LauncherClinitThrows.java:<n>)
//   exit=1
//
// CratonVM before wave 59 (debug build, Windows, measured): the same three
// lines, then the `Caused by:` block again, exit 1 -- the launcher's error
// arm printed the report a second time as the error's `Debug` rendering, for
// every uncaught exception of `main` (`L5W59LauncherInstanceArgsMain` on
// the base doubled its `\tat` lines the same way).

public class L5W59LauncherClinitThrows {
    static {
        if (Boolean.parseBoolean("true")) {
            throw new IllegalStateException("boom");
        }
    }

    public static void main(String[] args) {
        System.out.println("main ran (HotSpot never prints this)");
    }
}
