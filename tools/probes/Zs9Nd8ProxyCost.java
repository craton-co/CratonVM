// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.management.GarbageCollectorMXBean;
import java.lang.management.ManagementFactory;
import java.lang.management.ThreadMXBean;
import java.lang.reflect.Proxy;
import java.util.ArrayList;
import java.util.Map;
import java.util.TreeMap;

/**
 * zs s9/nd8: where the CPU of the {@code Zs3Nd3UpcallMints proxy} row goes
 * (ZGC round zs, wave s9, lane nd8; host s6a ND5-P2: HotSpot 16-23 s CPU,
 * CratonVM 72-145 s for 20000 iterations).
 *
 * <p>Each iteration is the row's {@code proxy} step: a fresh
 * {@code ClassLoader} and {@code Proxy.getProxyClass(fresh, Runnable.class)}
 * (a dynamic module per loader, a {@code ProxyGenerator} class file, a
 * define), then 512 bytes of garbage. {@code fill} grows the live set until
 * {@code freeKb} KiB are left, as the row does; {@code room} does not, so
 * the two arms split the cost into the define path and the near-full heap.
 * {@code define} (a fresh loader defining one prebuilt class) and
 * {@code generate} (a proxy-shaped class file through the public class-file
 * API, nothing defined) split the define path further; both run with room.
 *
 * <p>stdout (deterministic): {@code mode=<m> defines=<n> bad=<n>}. stderr: the
 * main thread's and the process's CPU per define (ThreadMXBean,
 * com.sun.management.OperatingSystemMXBean), the process's collector counts
 * and times (GarbageCollectorMXBeans), and a stack-sample histogram of the
 * main thread taken every {@code periodMs} by a daemon thread, attributing
 * each sample to the innermost frame of one of these phases:
 * {@code generate} (ProxyGenerator / the class-file API), {@code define}
 * (ClassLoader.defineClass*), {@code module} (the dynamic module:
 * ModuleDescriptor, Module.defineModule0 and friends), {@code reflect}
 * (getDeclaredConstructors0 / getMethods / Class.forName), {@code loader}
 * (the fresh loader's constructor), {@code other}.
 * Usage: {@code java -XX:+UseZGC -Xmx64m Zs9Nd8ProxyCost fill|room|define|generate [iters]
 * [freeKb] [periodMs]} (defaults 20000, 4096, 10).
 */
public class Zs9Nd8ProxyCost {
    static volatile Object sink;
    static volatile boolean done;

    static String phase(StackTraceElement[] st) {
        for (StackTraceElement e : st) {
            String c = e.getClassName();
            String m = e.getMethodName();
            if (c.startsWith("java.lang.reflect.ProxyGenerator") || c.startsWith("java.lang.classfile")
                    || c.startsWith("jdk.internal.classfile")) {
                return "generate";
            }
            if (c.equals("java.lang.ClassLoader") && m.startsWith("defineClass")) {
                return "define";
            }
            if (c.startsWith("java.lang.module") || c.equals("java.lang.Module")
                    || c.startsWith("jdk.internal.module") || m.equals("getDynamicModule")) {
                return "module";
            }
            if (m.equals("getDeclaredConstructors0") || m.equals("getDeclaredMethods0")
                    || m.equals("forName0") || m.startsWith("getMethod") || m.startsWith("getConstructor")) {
                return "reflect";
            }
            if (c.equals("java.lang.ClassLoader") && m.equals("<init>")) {
                return "loader";
            }
        }
        return "other";
    }

    static long gcCount() {
        long n = 0;
        for (GarbageCollectorMXBean b : ManagementFactory.getGarbageCollectorMXBeans()) {
            n += Math.max(0, b.getCollectionCount());
        }
        return n;
    }

    static long gcMillis() {
        long n = 0;
        for (GarbageCollectorMXBean b : ManagementFactory.getGarbageCollectorMXBeans()) {
            n += Math.max(0, b.getCollectionTime());
        }
        return n;
    }

    static long processCpuNanos() {
        try {
            java.lang.management.OperatingSystemMXBean os = ManagementFactory.getOperatingSystemMXBean();
            if (os instanceof com.sun.management.OperatingSystemMXBean sun) {
                return sun.getProcessCpuTime();
            }
        } catch (Throwable t) {
            // not available
        }
        return -1;
    }

    static final class Ldr extends ClassLoader {
        Ldr() {
            super(Zs9Nd8ProxyCost.class.getClassLoader());
        }

        Class<?> def(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }
    }

    /** A proxy-shaped class file (four methods and a constructor) through the class-file API. */
    static byte[] generated(int i) {
        java.lang.constant.ClassDesc self = java.lang.constant.ClassDesc.of("zs9nd8.Gen" + i);
        java.lang.constant.ClassDesc obj = java.lang.constant.ConstantDescs.CD_Object;
        java.lang.constant.MethodTypeDesc v = java.lang.constant.MethodTypeDesc.of(
                java.lang.constant.ConstantDescs.CD_void);
        return java.lang.classfile.ClassFile.of().build(self, cb -> {
            cb.withFlags(java.lang.classfile.ClassFile.ACC_PUBLIC | java.lang.classfile.ClassFile.ACC_FINAL);
            cb.withSuperclass(obj);
            cb.withInterfaceSymbols(java.lang.constant.ClassDesc.of("java.lang.Runnable"));
            cb.withMethodBody("<init>", v, java.lang.classfile.ClassFile.ACC_PUBLIC, code -> code
                    .aload(0).invokespecial(obj, "<init>", v).return_());
            cb.withMethodBody("run", v, java.lang.classfile.ClassFile.ACC_PUBLIC, code -> code.return_());
            cb.withMethodBody("hashCode", java.lang.constant.MethodTypeDesc.of(java.lang.constant.ConstantDescs.CD_int),
                    java.lang.classfile.ClassFile.ACC_PUBLIC, code -> code.iconst_1().ireturn());
            cb.withMethodBody("toString", java.lang.constant.MethodTypeDesc.of(java.lang.constant.ConstantDescs.CD_String),
                    java.lang.classfile.ClassFile.ACC_PUBLIC, code -> code.ldc("zs9nd8").areturn());
            cb.withMethodBody("equals", java.lang.constant.MethodTypeDesc.of(java.lang.constant.ConstantDescs.CD_boolean, obj),
                    java.lang.classfile.ClassFile.ACC_PUBLIC, code -> code.iconst_0().ireturn());
        });
    }

    static byte[] tiny;
    static int serial;

    /** One iteration of {@code mode}; true when it served. */
    @SuppressWarnings("deprecation")
    static boolean step(String mode) {
        switch (mode) {
            case "define": {
                // A fresh loader defining one prebuilt class: the define + loader cost alone.
                Ldr l = new Ldr();
                return l.def("zs9nd8.Tiny", tiny).getClassLoader() == l;
            }
            case "generate":
                // The class-file API alone: no loader, no define.
                return generated(serial++ & 1023).length > 0;
            default: {
                ClassLoader fresh = new ClassLoader(Zs9Nd8ProxyCost.class.getClassLoader()) {};
                Class<?> c = Proxy.getProxyClass(fresh, Runnable.class);
                return Proxy.isProxyClass(c) && c.getClassLoader() == fresh;
            }
        }
    }

    public static void main(String[] args) throws Throwable {
        String mode = args.length > 0 ? args[0] : "fill";
        int iters = args.length > 1 ? Integer.parseInt(args[1]) : 20_000;
        int freeKb = args.length > 2 ? Integer.parseInt(args[2]) : 4096;
        int periodMs = args.length > 3 ? Integer.parseInt(args[3]) : 10;
        tiny = generated(-1);
        tiny = java.lang.classfile.ClassFile.of().transformClass(java.lang.classfile.ClassFile.of().parse(tiny),
                java.lang.constant.ClassDesc.of("zs9nd8.Tiny"), java.lang.classfile.ClassTransform.ACCEPT_ALL);
        for (int i = 0; i < 200; i++) {
            step(mode);
        }
        System.gc();
        ArrayList<byte[]> live = new ArrayList<>();
        if (mode.equals("fill")) {
            Runtime rt = Runtime.getRuntime();
            long room = rt.maxMemory() - (rt.totalMemory() - rt.freeMemory()) - (long) freeKb * 1024;
            try {
                for (long n = Math.max(0, room / 65536); n > 0; n--) {
                    live.add(new byte[65536 - 16]);
                }
            } catch (OutOfMemoryError e) {
                for (int i = 0; i < 4 && !live.isEmpty(); i++) {
                    live.remove(live.size() - 1);
                }
            }
        } else if (!mode.equals("room") && !mode.equals("define") && !mode.equals("generate")) {
            throw new IllegalArgumentException(mode);
        }
        Thread main = Thread.currentThread();
        Map<String, Integer> phases = new TreeMap<>();
        Thread sampler = new Thread(() -> {
            while (!done) {
                try {
                    Thread.sleep(periodMs);
                } catch (InterruptedException e) {
                    return;
                }
                StackTraceElement[] st = main.getStackTrace();
                if (st.length > 0) {
                    phases.merge(phase(st), 1, Integer::sum);
                }
            }
        }, "zs9nd8-sampler");
        sampler.setDaemon(true);
        ThreadMXBean tmx = ManagementFactory.getThreadMXBean();
        long gc0 = gcCount();
        long gcMs0 = gcMillis();
        long cpu0 = tmx.getCurrentThreadCpuTime();
        long proc0 = processCpuNanos();
        long wall0 = System.nanoTime();
        sampler.start();
        int bad = 0;
        int defines = 0;
        for (int i = 0; i < iters; i++) {
            try {
                if (step(mode)) {
                    defines++;
                } else {
                    bad++;
                }
                sink = new byte[512];
            } catch (OutOfMemoryError e) {
                bad++;
            }
        }
        long cpu1 = tmx.getCurrentThreadCpuTime();
        long proc1 = processCpuNanos();
        long wall1 = System.nanoTime();
        done = true;
        sampler.interrupt();
        sampler.join();
        System.out.println("mode=" + mode + " defines=" + defines + " bad=" + bad);
        System.err.println("zs9nd8 proxy-cost: mode=" + mode + " main_cpu_us_per_define="
                + (cpu1 - cpu0) / 1000 / Math.max(1, iters) + " process_cpu_us_per_define="
                + (proc0 < 0 || proc1 < 0 ? -1 : (proc1 - proc0) / 1000 / Math.max(1, iters))
                + " wall_us_per_define="
                + (wall1 - wall0) / 1000 / Math.max(1, iters) + " gcs=" + (gcCount() - gc0)
                + " gc_ms=" + (gcMillis() - gcMs0) + " live_chunks=" + live.size());
        StringBuilder sb = new StringBuilder("zs9nd8 proxy-cost samples:");
        for (Map.Entry<String, Integer> e : phases.entrySet()) {
            sb.append(' ').append(e.getKey()).append('=').append(e.getValue());
        }
        System.err.println(sb);
        sink = live;
    }
}
