package org.jboss.modules;

/**
 * Compile-time stub for org.jboss.modules.DefaultBootModuleLoaderHolder.
 *
 * The static INSTANCE field is populated at runtime by the
 * post-clinit fixup hook in vm/src/vm/vm_util.rs:1022.  At compile
 * time we just declare it null so javac can resolve the field
 * reference; the cratonvm clinit-fixup writes a real
 * LocalModuleLoader after the synthetic clinit completes.
 */
public final class DefaultBootModuleLoaderHolder {
    public static final ModuleLoader INSTANCE = null;
    private DefaultBootModuleLoaderHolder() {}
}
