// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.security.AccessController;
import java.security.CodeSource;
import java.security.PrivilegedAction;
import java.security.ProtectionDomain;

/**
 * SBR wave 6, lane {@code module}: ns/op of the hot rows the lane considers retiring.
 * Best of 5 rounds of N calls each; prints {@code name ns/op}. Interleave A/B runs by hand.
 *
 * <pre>
 *   cratonvm --jdk-only -cp . Sbr2W7ModuleBench [N]
 * </pre>
 */
public class Sbr2W7ModuleBench {
    interface Op {
        Object run() throws Throwable;
    }

    static volatile Object sink;

    static void bench(String name, int n, Op op) throws Throwable {
        for (int i = 0; i < Math.min(n, 20000); i++) sink = op.run();
        long best = Long.MAX_VALUE;
        for (int r = 0; r < 5; r++) {
            long t0 = System.nanoTime();
            for (int i = 0; i < n; i++) sink = op.run();
            long t = System.nanoTime() - t0;
            if (t < best) best = t;
        }
        System.out.println(name + " " + (best / n) + " ns/op");
    }

    public static void main(String[] a) throws Throwable {
        int n = a.length > 0 ? Integer.parseInt(a[0]) : 200000;
        Module base = Object.class.getModule();
        Module me = Sbr2W7ModuleBench.class.getModule();
        ClassLoader sys = ClassLoader.getSystemClassLoader();
        ProtectionDomain pd = Sbr2W7ModuleBench.class.getProtectionDomain();
        CodeSource cs = pd.getCodeSource();
        PrivilegedAction<String> pa = () -> "x";
        bench("doPrivileged", n, () -> AccessController.doPrivileged(pa));
        bench("CodeSource.getLocation", n, () -> cs.getLocation());
        bench("CodeSource.getCertificates", n, () -> cs.getCertificates());
        bench("PD.getCodeSource", n, () -> pd.getCodeSource());
        bench("Module.getName", n, () -> base.getName());
        bench("Module.getClassLoader", n, () -> base.getClassLoader());
        bench("Module.isExported", n, () -> base.isExported("java.lang"));
        bench("Module.isExported.to", n, () -> base.isExported("java.lang", me));
        bench("Module.isOpen", n, () -> me.isOpen("x"));
        bench("Module.canRead", n, () -> me.canRead(base));
        bench("Module.canUse", n, () -> base.canUse(Runnable.class));
        bench("Module.getDescriptor", n, () -> base.getDescriptor());
        bench("Module.getLayer", n, () -> base.getLayer());
        bench("Class.getPackageName", n, () -> String.class.getPackageName());
        bench("Class.forName(String)", n, () -> Class.forName("java.lang.String"));
        bench("Class.forName(app)", n, () -> Class.forName("Sbr2W7ModuleBench"));
        bench("Class.forName(3)", n, () -> Class.forName("Sbr2W7ModuleBench", false, sys));
        bench("Class.forName(module)", n, () -> Class.forName(base, "java.lang.String"));
        bench("ClassLoader.loadClass", n, () -> sys.loadClass("Sbr2W7ModuleBench"));
        bench("ClassLoader.getSystemClassLoader", n, () -> ClassLoader.getSystemClassLoader());
        bench("ClassLoader.getPlatformClassLoader", n, () -> ClassLoader.getPlatformClassLoader());
        bench("Class.desiredAssertionStatus", n, () -> Sbr2W7ModuleBench.class.desiredAssertionStatus());
        bench("Class.getClassLoader", n, () -> Sbr2W7ModuleBench.class.getClassLoader());
        bench("Class.getPackage", n, () -> Sbr2W7ModuleBench.class.getPackage());
        bench("Class.getResourceAsStream", Math.max(n / 20, 1000), () -> {
            try (java.io.InputStream in = Sbr2W7ModuleBench.class.getResourceAsStream("Sbr2W7ModuleBench.class")) {
                return in;
            }
        });
        bench("ModuleLayer.boot", n, () -> ModuleLayer.boot());
        bench("ModuleLayer.findModule", n, () -> ModuleLayer.boot().findModule("java.base"));
        bench("new MyLoader", n / 10, () -> new ClassLoader() {});
    }
}
