package org.jboss.modules;

/** Compile-time stub for org.jboss.modules.Module. */
public class Module {
    public String getName() { return null; }
    public ClassLoader getClassLoader() { return null; }
    public ModuleLoader getModuleLoader() { return null; }
    public Class<?> loadClass(String name) throws ClassNotFoundException { return null; }
    public static void initBootModuleLoader(ModuleLoader loader) {}
}
