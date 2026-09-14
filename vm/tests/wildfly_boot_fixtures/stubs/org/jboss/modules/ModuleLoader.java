package org.jboss.modules;

/**
 * Compile-time stub for org.jboss.modules.ModuleLoader.
 *
 * Mirrors the synthetic class shape declared in
 * classloading/src/class_manager.rs at line ~3817 (single `root`
 * field).  Only the surface needed by JBossModulesProbe is declared
 * here; the cratonvm runtime substitutes the actual native
 * implementation registered in
 * native-builtins/src/jboss_module_loader.rs.
 */
public abstract class ModuleLoader {
    public Module loadModule(String name) throws Exception {
        return null;
    }
}
