// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 60, lane L5 -- the `-jar` manifest attributes
// `LauncherHelper.getMainClassFromJar` acts on before `main`:
// `Launcher-Agent-Class` (the agent's `agentmain` runs first, through
// `sun.instrument.InstrumentationImpl.loadAgent`), `Add-Opens` /
// `Add-Exports` (to every unnamed module) and `Enable-Native-Access:
// ALL-UNNAMED`. The VM's own `-jar` path read only `Main-Class` and
// `Class-Path`; with `CRATONVM_LAUNCHER_HELPER_JAR` the launcher hands the
// jar to `LauncherHelper.checkAndLoadMain(true, LM_JAR, <jar>)` as `java.c`
// does. Every answer is printed twice (cold first call).
//
// SETUP and run (Linux paths):
//   javac -d out L5W60LauncherJarAttributes.java
//   printf 'Main-Class: L5W60LauncherJarAttributes\nLauncher-Agent-Class: L5W60LauncherJarAttributes\nAdd-Opens: java.base/java.lang java.base/java.util\nAdd-Exports: java.base/jdk.internal.misc\nEnable-Native-Access: ALL-UNNAMED\n' > m.mf
//   jar cfm attrs.jar m.mf -C out L5W60LauncherJarAttributes.class
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -jar attrs.jar
//
// Expected HotSpot 25.0.3 output (`java -jar attrs.jar`, default flags; the
// agent's argument string is empty, not null):
//   agentmain=true args= instrumentation=true
//   opens-lang=true
//   opens-util=true
//   opens-io=false
//   exports-misc=true
//   native-access=true
//   agentmain-again=true
//   opens-lang-again=true
// Control (a jar whose manifest has `Main-Class` only): every answer false,
// `args=unset`.

import java.lang.instrument.Instrumentation;

public class L5W60LauncherJarAttributes {
    static volatile boolean agentRan;
    static volatile String agentArgs = "unset";
    static volatile boolean agentInst;

    public static void agentmain(String args, Instrumentation inst) {
        agentRan = true;
        agentArgs = args;
        agentInst = inst != null;
    }

    public static void main(String[] args) {
        Module me = L5W60LauncherJarAttributes.class.getModule();
        Module base = Object.class.getModule();
        System.out.println("agentmain=" + agentRan + " args=" + agentArgs + " instrumentation=" + agentInst);
        System.out.println("opens-lang=" + base.isOpen("java.lang", me));
        System.out.println("opens-util=" + base.isOpen("java.util", me));
        System.out.println("opens-io=" + base.isOpen("java.io", me));
        System.out.println("exports-misc=" + base.isExported("jdk.internal.misc", me));
        System.out.println("native-access=" + me.isNativeAccessEnabled());
        System.out.println("agentmain-again=" + agentRan);
        System.out.println("opens-lang-again=" + base.isOpen("java.lang", me));
    }
}
