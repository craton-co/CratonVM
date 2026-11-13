// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

// j27 wave 5 (lane FBLEVEL): the JDK's own boot state at `main`, which the
// --compatible fast boot (CRATONVM_COMPAT_FAST_BOOT_JIT, default ON) must leave
// as HotSpot does: VM.initLevel at SYSTEM_BOOTED (4) and ClassLoader.scl
// published (CRATONVM_COMPAT_FAST_BOOT_JDK_BOOTED, opt-in since the j27 orchestrator,
// =1; with CRATONVM_COMPAT_FAST_BOOT_BOOT_LAYER=1 since j27 wave 6).
// Page: docs/feature-designs/real-jdk-mode.md
//
// The level rows run FIRST, before anything else can move the level.
//
//   javac --add-exports java.base/jdk.internal.misc=ALL-UNNAMED -d <dir> J27FastBootLevel.java
//   java --add-exports java.base/jdk.internal.misc=ALL-UNNAMED -cp <dir> J27FastBootLevel > hs.txt
//   cratonvm --add-exports java.base/jdk.internal.misc=ALL-UNNAMED --cp <dir> J27FastBootLevel | diff - hs.txt
//     x {--compatible, --real-jdk}
//     x {default, CRATONVM_COMPAT_FAST_BOOT_JDK_BOOTED=0 (expect the page's old
//        answers under --compatible: level 1, and booted true from the
//        `isBooted` stand-in, which lane VMALIGN's `jdk-vm-init-level` row
//        keeps registered under that switch), CRATONVM_COMPAT_FAST_BOOT_JIT=0,
//        CRATONVM_COMPAT_ALIGN=-jdk-vm-init-level (expect HotSpot's lines)}
//
//   j27 wave 6 (lane T1LAM2): the level-4 write is opt-in and runs only with a
//   boot layer. Add the arms
//     CRATONVM_COMPAT_FAST_BOOT_JDK_BOOTED=1 CRATONVM_COMPAT_FAST_BOOT_BOOT_LAYER=1  (expect HotSpot's lines)
//     CRATONVM_COMPAT_FAST_BOOT_JDK_BOOTED=1 alone   (level withheld: initLevel 1, booted true from the
//                                                      kept stand-in, bootLayer false; traces still fine)
//     CRATONVM_COMPAT_FAST_BOOT_BOOT_LAYER=1 alone   (bootLayer true, initLevel 1)
//   A trace row printing java.lang.ExceptionInInitializerError / NoClassDefFoundError is the
//   orchestrator's regression (level >= 2 without a layer).
//
// Expected HotSpot 25 output:
//   fact booted true
//   fact initLevel 4
//   fact moduleSystemInited true
//   fact scl true
//   fact sclIsApp true
//   fact proxy hello
//   fact bootLayer true
//   fact bootLayerHasBase true
//   fact objectLayerIsBoot true
//   fact trace0 java.lang.RuntimeException: x
//   fact trace1 at J27FastBootLevel.make(J27FastBootLevel.java:N)
//   fact jdkTrace0 java.lang.NumberFormatException: For input string: "z"
//   fact jdkTrace1 at java.base/java.lang.NumberFormatException.forInputString(NumberFormatException.java:N)
//   fact threadFrame java.base/java.lang.Thread.getStackTrace(Thread.java:N)
//   fact walkerFrame J27FastBootLevel.traceRows(J27FastBootLevel.java:N)
public class J27FastBootLevel {
    public static void main(String[] args) throws Exception {
        boolean booted = jdk.internal.misc.VM.isBooted();
        int level = jdk.internal.misc.VM.initLevel();
        boolean moduleSystem = jdk.internal.misc.VM.isModuleSystemInited();
        ClassLoader scl = ClassLoader.getSystemClassLoader();
        System.out.println("fact booted " + booted);
        System.out.println("fact initLevel " + level);
        System.out.println("fact moduleSystemInited " + moduleSystem);
        System.out.println("fact scl " + (scl != null));
        System.out.println("fact sclIsApp " + (scl == J27FastBootLevel.class.getClassLoader()));
        // Proxy's builder throws InternalError below MODULE_SYSTEM_INITED (2).
        String proxied;
        try {
            Runnable r = (Runnable) java.lang.reflect.Proxy.newProxyInstance(
                    J27FastBootLevel.class.getClassLoader(),
                    new Class<?>[] {Runnable.class},
                    (p, m, a) -> m.getName().equals("run") ? null : "hello");
            r.run();
            proxied = r.toString();
        } catch (Throwable t) {
            proxied = t.getClass().getName();
        }
        System.out.println("fact proxy " + proxied);
        layerRows();
        traceRows();
    }

    // j27 wave 6 (lane T1LAM2): the boot layer the level needs. At level >= 2
    // `StackTraceElement` formatting initialises `StackTraceElement$HashedModules`,
    // whose <clinit> reads ModuleLayer.boot().configuration(): a layer-less level 4
    // turns every printStackTrace into ExceptionInInitializerError /
    // NoClassDefFoundError (the `trace` rows then print that class name).
    static void layerRows() {
        String layer, base, objectLayer;
        try {
            ModuleLayer boot = ModuleLayer.boot();
            layer = String.valueOf(boot != null);
            base = boot == null ? "nolayer" : String.valueOf(boot.findModule("java.base").isPresent());
            objectLayer = String.valueOf(Object.class.getModule().getLayer() == boot);
        } catch (Throwable t) {
            layer = base = objectLayer = t.getClass().getName();
        }
        System.out.println("fact bootLayer " + layer);
        System.out.println("fact bootLayerHasBase " + base);
        System.out.println("fact objectLayerIsBoot " + objectLayer);
    }

    static RuntimeException make() {
        return new RuntimeException("x");
    }

    // Line numbers are normalised to N (the JDK's differ per update release).
    static String norm(String s) {
        return s.replaceAll(":\\d+\\)", ":N)");
    }

    static void traceRows() {
        try {
            java.io.StringWriter sw = new java.io.StringWriter();
            make().printStackTrace(new java.io.PrintWriter(sw, true));
            String[] lines = sw.toString().split("\\R");
            System.out.println("fact trace0 " + norm(lines[0]));
            System.out.println("fact trace1 " + norm(lines[1].trim()));
        } catch (Throwable t) {
            System.out.println("fact trace0 " + t.getClass().getName());
        }
        try {
            Integer.parseInt("z");
            System.out.println("fact jdkTrace0 none");
        } catch (NumberFormatException e) {
            try {
                java.io.StringWriter sw = new java.io.StringWriter();
                e.printStackTrace(new java.io.PrintWriter(sw, true));
                String[] lines = sw.toString().split("\\R");
                System.out.println("fact jdkTrace0 " + norm(lines[0]));
                System.out.println("fact jdkTrace1 " + norm(lines[1].trim()));
            } catch (Throwable t) {
                System.out.println("fact jdkTrace0 " + t.getClass().getName());
            }
        }
        try {
            StackTraceElement top = Thread.currentThread().getStackTrace()[0];
            System.out.println("fact threadFrame " + norm(top.toString()));
        } catch (Throwable t) {
            System.out.println("fact threadFrame " + t.getClass().getName());
        }
        try {
            StackTraceElement here = StackWalker.getInstance()
                    .walk(s -> s.findFirst()).get().toStackTraceElement();
            System.out.println("fact walkerFrame " + norm(here.toString()));
        } catch (Throwable t) {
            System.out.println("fact walkerFrame " + t.getClass().getName());
        }
    }
}
