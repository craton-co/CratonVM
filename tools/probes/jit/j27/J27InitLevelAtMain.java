// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

// j27 wave 9 (lane W9RPL): the JDK's VM.initLevel at main
// (page docs/feature-designs/real-jdk-mode.md).
//
// javac --add-exports java.base/jdk.internal.misc=ALL-UNNAMED -d <out> J27InitLevelAtMain.java
// java    --add-exports java.base/jdk.internal.misc=ALL-UNNAMED -cp <out> J27InitLevelAtMain
// cratonvm {--real-jdk,--compatible} --add-exports java.base/jdk.internal.misc=ALL-UNNAMED -cp <out> J27InitLevelAtMain
//   x {unset, CRATONVM_INIT_FINALIZER_BEFORE_PHASE1=0}
// Expected HotSpot 25 output:
//   fact init-level 4
//   fact booted true
public class J27InitLevelAtMain {
    public static void main(String[] a) {
        System.out.println("fact init-level " + jdk.internal.misc.VM.initLevel());
        System.out.println("fact booted " + jdk.internal.misc.VM.isBooted());
    }
}
