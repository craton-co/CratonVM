// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 38, lane L2: item 6 of the wave-37 compile-door
// review. The upgrade door (`try_jit_upgrade_with_gate`, the invocation
// counter's inline compile under `CRATONVM_BG_COMPILE=0`) refuses a method
// whose bytecode calls a native-shadowed target
// (`jit_method_calls_native_shadowed`; `Character.toLowerCase(C)C` has a
// registered native) and used to record that POLICY verdict in the backend
// bail list. `compile_gate::admit` honours the bail list at every door, so
// both OSR routes, which resolve native shadows per site and never ask the
// policy, refused the method's loop too: whether `work`'s loop was
// OSR-compiled depended on whether the upgrade door or an OSR offer came
// first. Since wave 38 the verdict is the upgrade door's own memo.
//
// `work(1, c)` is called often enough to reach the upgrade door, then
// `work(20_000_000, c)` runs one long loop.
//
// Run: javac -d out L2W38UpgradeDoorNativeShadowOsr.java
//      CRATONVM_BG_COMPILE=0 cratonvm --java-home <jdk25> [--compatible] -cp out L2W38UpgradeDoorNativeShadowOsr
//      cratonvm --java-home <jdk25> [--nojit] -cp out L2W38UpgradeDoorNativeShadowOsr
//
// Expected HotSpot 25 output (default and -Xint), every CratonVM mode alike:
//   short=-1468212096
//   long=2043782784
//
// Positive control (wave 38; a performance fix, the rows do not move). `main`
// is kept interpreted with the bisect lever, so its calls reach `work`
// through the interpreter's static door (a compiled `main` would call it
// through the callee door, which never asks the policy):
//   CRATONVM_BG_COMPILE=0 CRATONVM_DBG_JITC=1 CRATONVM_JIT_DENY=L2W38UpgradeDoorNativeShadowOsr.main \
//     cratonvm --java-home <jdk25> -cp out L2W38UpgradeDoorNativeShadowOsr 2>&1 \
//     | grep -E 'upgrade refused \(calls a native-shadowed|OSR-compile L2W38UpgradeDoorNativeShadowOsr.work|osr optimizing L2W38UpgradeDoorNativeShadowOsr.work'
// prints the `upgrade refused (...; not bail-listed) ...work(IC)I` line and
// then an OSR line for `work` (an `OSR-compile` or an `osr optimizing`
// build), where the wave-37 base printed the OSR door's `admission-refused`
// (`PermanentlyBailListed`) instead. If the `upgrade refused` line is absent
// the scan did not hit on this build and the probe says nothing.
public class L2W38UpgradeDoorNativeShadowOsr {
    static int work(int n, char c) {
        int acc = 0;
        for (int i = 0; i < n; i++) {
            acc = acc * 31 + Character.toLowerCase((char) (c + (i & 7)));
        }
        return acc;
    }

    public static void main(String[] args) {
        int shortSum = 0;
        for (int k = 0; k < 20_000; k++) {
            shortSum = shortSum * 7 + work(1, 'A');
        }
        System.out.println("short=" + shortSum);
        System.out.println("long=" + work(20_000_000, 'A'));
    }
}
