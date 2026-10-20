// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 57, lane L5 -- HotSpot's launcher fails before
// `main` when a PUBLIC method of the main class names a missing type: its
// `MethodFinder.findMainMethod` asks `getMethod("main", String[].class)`,
// whose public-method listing resolves every listed signature type through
// the class's loader (`interpreter-L5-getinitiatedclasses-residual-recording-differences-FIXED-20261007`,
// item 3's launcher half).
//
// SETUP and run (`Gone` must be absent at run time):
//   javac -d out L5W57LauncherSignatureType.java
//   rm 'out/L5W57LauncherSignatureType$Gone.class'
//   java -cp out L5W57LauncherSignatureType; echo "exit=$?"
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W57LauncherSignatureType; echo "exit=$?"
//
// Expected HotSpot 25.0.3 output (stderr, then the exit code):
//   Error: Unable to initialize main class L5W57LauncherSignatureType
//   Caused by: java.lang.NoClassDefFoundError: L5W57LauncherSignatureType$Gone
//   exit=1
//
// CratonVM (debug build, Windows, both values of
// `CRATONVM_REFLECTION_LOADS_SIGNATURE_TYPES`, measured): `main ran`,
// `exit=0` -- the launcher finds `main` without a listing.
// Since wave 58 (lane L5): HotSpot's two lines and `exit=1` in both modes
// with `CRATONVM_REFLECTION_LOADS_SIGNATURE_TYPES=1` (the launcher runs
// `MethodFinder`'s listings, `Vm::launcher_main_listing_failure`); `main
// ran` / `exit=0` with it off, the switch's default.

public class L5W57LauncherSignatureType {
    public static class Gone {
    }

    public static void use(Gone g) {
    }

    public static void main(String[] args) {
        System.out.println("main ran");
    }
}
