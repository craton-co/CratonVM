// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 60, lane L5 -- `-jar` launches whose manifest
// cannot name a main class: a zip with no `META-INF/MANIFEST.MF` is refused
// by `java.c`'s `JLI_ParseManifest` (`java.launcher` has no say), a manifest
// without `Main-Class` by `LauncherHelper` (`java.launcher.jar.error3`, whose
// `{0}` is `JarFile.getName()`: `java.io.File`'s normalised path). Both
// texts reach stderr with no `[cratonvm] main-vm run() returned Err: `
// prefix; exit code 1.
//
// SETUP and run (Linux paths; on Windows the error3 path prints with `\`):
//   javac -d out L5W60LauncherJarManifest.java
//   printf 'Created-By: probe\n' > nomf.mf
//   jar cfm nomf.jar nomf.mf -C out L5W60LauncherJarManifest.class
//   jar cfM nomanifest.jar -C out L5W60LauncherJarManifest.class
//   mkdir -p sub && cp nomf.jar sub/
//   printf 'Main-Class: L5W60LauncherJarManifest\n' > ok.mf
//   jar cfm ok.jar ok.mf -C out L5W60LauncherJarManifest.class
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -jar nomanifest.jar; echo "exit=$?"
//   cratonvm ... -jar nomf.jar; echo "exit=$?"
//   cratonvm ... -jar sub//nomf.jar; echo "exit=$?"
//   cratonvm ... -jar ok.jar; echo "exit=$?"
//
// Expected HotSpot 25.0.3 (stdout / stderr, exit code per launch; Windows
// measured, Linux paths keep `/`):
//   Error: Invalid or corrupt jarfile nomanifest.jar
//   exit=1
//   no main manifest attribute, in nomf.jar
//   exit=1
//   no main manifest attribute, in sub/nomf.jar
//   exit=1
//   jar main ran
//   exit=0
//
// CratonVM before wave 60 (debug build, Windows): the first launch printed
// `no main manifest attribute, in nomanifest.jar`, and every text followed
// the `[cratonvm] main-vm run() returned Err: ` prefix.

public class L5W60LauncherJarManifest {
    public static void main(String[] args) {
        System.out.println("jar main ran");
    }
}
