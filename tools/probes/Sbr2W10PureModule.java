// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.net.URLClassLoader;

/**
 * SBR2 wave 10 (PUREMOD): `Class.getModule()` through the JDK's own bytecode
 * (`return module;`), i.e. `Class.module` filled by the VM. Run under
 * `CRATONVM_PURE_BYTECODE=1` and by default; diff against HotSpot.
 */
public class Sbr2W10PureModule {
    static String d(Class<?> c) {
        Module m = c.getModule();
        return c.getName() + " -> " + (m == null ? "NULL" : (m.isNamed() ? "named:" + m.getName() : "unnamed") )
                + (m != null && m.getClassLoader() == null ? " loader=boot" : (m != null ? " loader=" + (m.getClassLoader() == ClassLoader.getSystemClassLoader() ? "app" : m.getClassLoader() == ClassLoader.getPlatformClassLoader() ? "platform" : "other") : ""));
    }
    public static void main(String[] a) throws Exception {
        for (Class<?> c : new Class<?>[] { Object.class, String.class, java.util.List.class, java.sql.Connection.class,
                java.net.http.HttpClient.class, Sbr2W10PureModule.class, int.class, int[].class, String[].class,
                Sbr2W10PureModule[].class, Runnable.class, ((Runnable) () -> {}).getClass().getInterfaces()[0] }) {
            System.out.println(d(c));
        }
        System.out.println("identity java.base " + (Object.class.getModule() == String.class.getModule()));
        System.out.println("identity unnamed " + (Sbr2W10PureModule.class.getModule() == ClassLoader.getSystemClassLoader().getUnnamedModule()));
        System.out.println("layer " + (Object.class.getModule().getLayer() == ModuleLayer.boot()));
        System.out.println("named pkg " + Object.class.getModule().getPackages().contains("java.lang"));
        System.out.println("canRead " + Sbr2W10PureModule.class.getModule().canRead(Object.class.getModule()));
        try (URLClassLoader u = new URLClassLoader(new java.net.URL[0], null)) {
            Module m = u.getUnnamedModule();
            System.out.println("custom unnamed " + (m != null && !m.isNamed() && m.getClassLoader() == u));
        }
        System.out.println(String.format("%5.2f %s", 3.14159, java.util.Locale.getDefault()));
        System.out.println(java.util.ServiceLoader.load(java.nio.file.spi.FileSystemProvider.class).iterator().hasNext());
    }
}
