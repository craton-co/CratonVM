// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 59, lane L5 -- the launcher takes any main-class
// name the loader can answer: a Java identifier may hold non-ASCII letters
// (`Café`), and a name that is no class at all (`my-pkg.Main`, a trailing
// `/`) is the system loader's `ClassNotFoundException` in
// `java.launcher.cls.error1`, not a launcher-side syntax error.
//
// Run (no setup; each line is one launch; on Windows the non-ASCII launch
// needs a UTF-8 console):
//   javac -encoding UTF-8 -d out L5W59LauncherClassNames.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W59LauncherClassNames; echo "exit=$?"
//   cratonvm ... -cp out 'L5W59LauncherClassNames$Café'; echo "exit=$?"
//   cratonvm ... -cp out my-pkg.Main; echo "exit=$?"
//   cratonvm ... -cp out L5W59LauncherClassNames/; echo "exit=$?"
//
// Expected HotSpot 25.0.3 output (stdout, stderr, exit code per launch):
//   top main ran
//   exit=0
//   café main ran
//   exit=0
//   Error: Could not find or load main class my-pkg.Main
//   Caused by: java.lang.ClassNotFoundException: my-pkg.Main
//   exit=1
//   Error: Could not find or load main class L5W59LauncherClassNames.
//   Caused by: java.lang.ClassNotFoundException: L5W59LauncherClassNames.
//   exit=1
//
// CratonVM before wave 59 (debug build, Windows, measured): the first launch
// as HotSpot; the other three `Invalid class name '...': segment ...` (the
// launcher's own name check: ASCII-only identifier characters, no empty
// segment), exit 1.

public class L5W59LauncherClassNames {
    public static void main(String[] args) {
        System.out.println("top main ran");
    }

    public static class Café {
        public static void main(String[] args) {
            System.out.println("café main ran");
        }
    }
}
