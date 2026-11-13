// SPDX-License-Identifier: GPL-2.0-only WITH Classpath-exception-2.0
// Copyright 2024-2026 Craton Software Company

// j27 wave 8 (lane W9BOOT): two `--compatible` fast-boot regressions.
// Pages: docs/feature-designs/native-dispatch-architecture.md,
//        docs/feature-designs/native-dispatch-architecture.md.
//
// A: the first reflective field access of a program that never used a method
//    handle (`Field.setInt` / `getInt` on a static, `Field.get` on an
//    instance field). Broken: `InternalError: J27W8BootReflectMgmtRows::TAG
//    cannot be accessed reflectively before java.lang.invoke is initialized`.
//    The field rows run FIRST, before anything that could initialise
//    java.lang.invoke (no lambda, no string concat with indy: `+` on a String
//    is indy in javac >= 9, so the field rows print through StringBuilder).
// B: `ManagementFactory` (runs `VMManagementImpl.<clinit>` -> `getVersion0`).
//    Broken: `ServiceConfigurationError` (layer-less default arm) or
//    `UnsatisfiedLinkError` for `getVersion0` (boot-layer arm).
// Each row prints its answer or the class of the throwable, never a stack.
//
//   javac -d <dir> J27W8BootReflectMgmtRows.java
//   java -cp <dir> J27W8BootReflectMgmtRows > hs.txt
//   cratonvm --compatible --cp <dir> J27W8BootReflectMgmtRows | diff - hs.txt
//     x {default,
//        CRATONVM_COMPAT_FAST_BOOT_BOOT_LAYER=1 CRATONVM_COMPAT_FAST_BOOT_JDK_BOOTED=1,
//        CRATONVM_COMPAT_FAST_BOOT_JIT=0}
//   cratonvm --real-jdk --cp <dir> J27W8BootReflectMgmtRows | diff - hs.txt
//
// Expected HotSpot 25 (every arm, both modes):
//   fact reflect-static-set 7
//   fact reflect-static-get 7
//   fact reflect-instance-get 5
//   fact lang-invoke-inited true
//   fact mgmt-runtime-spec 25
//   fact mgmt-gc-beans-nonempty true
//   fact mgmt-heap-usage-nonnull true
// Known residual (not this lane's fix): in the layer-less DEFAULT fast-boot arm
// the three mgmt rows stay `threw java.util.ServiceConfigurationError`, because
// ManagementFactory finds its MXBeans through ServiceLoader, which refuses
// non-java.base callers while VM.isBooted() is false (the level stays 1 without
// CRATONVM_COMPAT_FAST_BOOT_BOOT_LAYER=1 + CRATONVM_COMPAT_FAST_BOOT_JDK_BOOTED=1).
// The reflect rows must be SAME in every arm.
public class J27W8BootReflectMgmtRows {
    static int TAG = 1;
    int inst = 5;

    static void fact(String name, String value) {
        StringBuilder sb = new StringBuilder("fact ");
        sb.append(name).append(' ').append(value);
        System.out.println(sb.toString());
    }

    static String failure(Throwable t) {
        return new StringBuilder("threw ").append(t.getClass().getName()).toString();
    }

    static void reflectRows() {
        try {
            java.lang.reflect.Field f = J27W8BootReflectMgmtRows.class.getDeclaredField("TAG");
            f.setInt(null, 7);
            fact("reflect-static-set", String.valueOf(TAG));
            fact("reflect-static-get", String.valueOf(f.getInt(null)));
        } catch (Throwable t) {
            fact("reflect-static-set", failure(t));
        }
        try {
            java.lang.reflect.Field g = J27W8BootReflectMgmtRows.class.getDeclaredField("inst");
            fact("reflect-instance-get", String.valueOf(g.get(new J27W8BootReflectMgmtRows())));
        } catch (Throwable t) {
            fact("reflect-instance-get", failure(t));
        }
    }

    static void mgmtRows() {
        try {
            fact("mgmt-runtime-spec",
                java.lang.management.ManagementFactory.getRuntimeMXBean().getSpecVersion());
        } catch (Throwable t) {
            fact("mgmt-runtime-spec", failure(t));
        }
        try {
            fact("mgmt-gc-beans-nonempty", String.valueOf(
                !java.lang.management.ManagementFactory.getGarbageCollectorMXBeans().isEmpty()));
        } catch (Throwable t) {
            fact("mgmt-gc-beans-nonempty", failure(t));
        }
        try {
            fact("mgmt-heap-usage-nonnull", String.valueOf(
                java.lang.management.ManagementFactory.getMemoryMXBean().getHeapMemoryUsage() != null));
        } catch (Throwable t) {
            fact("mgmt-heap-usage-nonnull", failure(t));
        }
    }

    public static void main(String[] a) {
        reflectRows();
        // HotSpot initialises java.lang.invoke before main: a method handle
        // lookup works and the flag the field rows needed is set.
        try {
            java.lang.invoke.MethodHandles.lookup()
                .findStaticGetter(J27W8BootReflectMgmtRows.class, "TAG", int.class);
            fact("lang-invoke-inited", "true");
        } catch (Throwable t) {
            fact("lang-invoke-inited", failure(t));
        }
        mgmtRows();
    }
}
