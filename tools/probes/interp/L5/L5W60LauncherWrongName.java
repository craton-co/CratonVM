// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 60, lane L5 -- the launcher's `Caused by:` for a
// main class launched from inside its package directory (`java -cp
// out/l5w60pkg L5W60LauncherWrongName` for `l5w60pkg.L5W60LauncherWrongName`):
// the application loader finds the file under the requested name, and the
// definer's `NoClassDefFoundError: <requested> (wrong name: <declared>)` is
// the answer, not a `ClassNotFoundException`. A launcher probe: it never
// reaches `main` in the failing launch; the control launch runs `main`.
//
// Run:
//   javac -d out L5W60LauncherWrongName.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out/l5w60pkg L5W60LauncherWrongName
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out l5w60pkg.L5W60LauncherWrongName
// (`out\l5w60pkg` on Windows.)
//
// Expected HotSpot 25.0.3 (`java`, default flags), first launch: stdout
// empty, exit code 1, stderr:
//   Error: Could not find or load main class L5W60LauncherWrongName
//   Caused by: java.lang.NoClassDefFoundError: L5W60LauncherWrongName (wrong name: l5w60pkg/L5W60LauncherWrongName)
// Second launch (control): exit code 0, stdout:
//   main=l5w60pkg.L5W60LauncherWrongName
//
// CratonVM before the fix (debug build, Windows, base 8004f6b7e1,
// `--compatible`): `Caused by: java.lang.ClassNotFoundException:
// L5W60LauncherWrongName`.

package l5w60pkg;

public class L5W60LauncherWrongName {
    public static void main(String[] args) {
        System.out.println("main=" + L5W60LauncherWrongName.class.getName());
    }
}
