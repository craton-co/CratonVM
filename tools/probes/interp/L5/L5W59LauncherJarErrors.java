// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 59, lane L5 -- `-jar` launches the launcher
// refuses before any class loads print HotSpot's `java.c` texts: a jar that
// does not exist, and a file that is no zip.
//
// SETUP and run (Linux paths; the path is printed as given):
//   javac -d out L5W59LauncherJarErrors.java
//   printf 'Main-Class: L5W59LauncherJarErrors\n' > m.mf
//   jar cfm ok.jar m.mf -C out L5W59LauncherJarErrors.class
//   printf 'hello' > corrupt.jar
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -jar ok.jar; echo "exit=$?"
//   cratonvm ... -jar missing.jar; echo "exit=$?"
//   cratonvm ... -jar corrupt.jar; echo "exit=$?"
//
// Expected HotSpot 25.0.3 output (stdout / stderr, exit code per launch):
//   jar main ran
//   exit=0
//   Error: Unable to access jarfile missing.jar
//   exit=1
//   Error: Invalid or corrupt jarfile corrupt.jar
//   exit=1
//
// CratonVM before wave 59 (debug build, Windows, measured): the first launch
// as HotSpot; `JAR file not found: missing.jar` and `Cannot read manifest
// from corrupt.jar` (behind the launcher's `[cratonvm] main-vm run()
// returned Err: ` prefix, which is still there), exit 1.
// Still different after wave 59: a zip with NO manifest -- HotSpot `Error:
// Invalid or corrupt jarfile`, CratonVM `no main manifest attribute, in ...`
// (the manifest reader answers an empty manifest). Wave 60 (lane L5) fixed
// it and dropped the prefix from these texts: `L5W60LauncherJarManifest`.

public class L5W59LauncherJarErrors {
    public static void main(String[] args) {
        System.out.println("jar main ran");
    }
}
