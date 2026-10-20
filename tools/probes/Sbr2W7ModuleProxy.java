// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.reflect.InvocationHandler;
import java.lang.reflect.Proxy;

/**
 * SBR2 wave 7, lane MODULE: the dynamic proxy module ({@code jdk.proxyN}) asked of the JDK's own
 * {@code Module} predicates. The proxy module is minted by the VM (not by {@code defineModules}),
 * so its tables were the open question of the module-table retirement.
 */
public class Sbr2W7ModuleProxy {
    static String tf(boolean b) {
        return b ? "1" : "0";
    }

    public interface Pub {
        void run();
    }

    interface Hidden {
        void run();
    }

    public static void main(String[] args) throws Throwable {
        InvocationHandler h = (p, m, a) -> null;
        Module base = Object.class.getModule();
        Module self = Sbr2W7ModuleProxy.class.getModule();
        for (Class<?> itf : new Class<?>[] {Runnable.class, Pub.class, Hidden.class}) {
            Object o = Proxy.newProxyInstance(itf.getClassLoader(), new Class<?>[] {itf}, h);
            Class<?> c = o.getClass();
            Module pm = c.getModule();
            String pkg = c.getPackageName();
            String shown = pkg.replaceAll("jdk.proxy[0-9]+", "jdk.proxyN");
            System.out.println(itf.getSimpleName() + " named=" + tf(pm.isNamed()) + " name=" + String.valueOf(pm.getName()).replaceAll("jdk.proxy[0-9]+", "jdk.proxyN")
                    + " pkg=" + shown + " layerNull=" + tf(pm.getLayer() == null));
            System.out.println("  isExported " + tf(pm.isExported(pkg)) + tf(pm.isExported(pkg, self)) + tf(pm.isExported(pkg, base))
                    + " isOpen " + tf(pm.isOpen(pkg)) + tf(pm.isOpen(pkg, self)) + tf(pm.isOpen(pkg, base))
                    + " canRead " + tf(pm.canRead(base)) + tf(pm.canRead(self)) + tf(self.canRead(pm)));
            if (pm.getDescriptor() == null) continue;
            System.out.println("  exports " + pm.getDescriptor().exports().size() + " opens " + pm.getDescriptor().opens().size()
                    + " packages " + pm.getPackages().size());
        }
    }
}
