// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 59, lane L5 -- a STATIC `main(String[])` inherited
// from a superclass starts, and the launcher initialises the MAIN class (JNI
// `GetStaticMethodID` on it), not only the declaring superclass; an instance
// `main()` inherited as an interface DEFAULT method starts on an instance of
// the main class (second launch)
// (`docs/internal/fixed-bugs/interpreter-L5-the-launcher-rejects-an-instance-main-and-misreports-main-class-errors-FIXED-20261023.md`).
//
// Run (no setup):
//   javac -d out L5W59LauncherInheritedMain.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W59LauncherInheritedMain; echo "exit=$?"
//
// Expected HotSpot 25.0.3 output (stdout, then the exit code):
//   base clinit
//   main clinit
//   inherited static main ran
//   exit=0
//
// Second launch, the default method:
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out 'L5W59LauncherInheritedMain$ViaDefault'; echo "exit=$?"
// HotSpot 25.0.3 (stdout):
//   default main ran on L5W59LauncherInheritedMain$ViaDefault
//   exit=0
//
// CratonVM before wave 59 (debug build, Windows, measured): the first launch
// as HotSpot (a guard: the new launcher must keep initialising the main
// class); the second `Exception in thread "main"
// java.lang.NoSuchMethodError: 'void
// L5W59LauncherInheritedMain$ViaDefault.main(java.lang.String[])'`, exit 1.

public class L5W59LauncherInheritedMain extends L5W59LauncherInheritedMainBase {
    static {
        System.out.println("main clinit");
    }

    public interface HasMain {
        default void main() {
            System.out.println("default main ran on " + getClass().getName());
        }
    }

    public static class ViaDefault implements HasMain {
    }
}

class L5W59LauncherInheritedMainBase {
    static {
        System.out.println("base clinit");
    }

    public static void main(String[] args) {
        System.out.println("inherited static main ran");
    }
}
