// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Reproducer for the `ModuleLayer.findModule` dynamic-layer defect fixed
// 2026-08-10. Replicates
// `com.sun.org.apache.xalan.internal.xsltc.trax.TemplatesImpl.createModule`
// line for line — that method ends in `layer.findModule(mn).get()`, so an
// empty Optional there surfaces as `NoSuchElementException: No value present`
// out of XSLTC and takes every `javax.xml.transform` consumer with it.
//
//   javac -d /tmp/probe probes/MLProbe.java
//   java     --add-opens=java.base/java.lang=ALL-UNNAMED -cp /tmp/probe MLProbe
//   cratonvm --add-opens=java.base/java.lang=ALL-UNNAMED -cp /tmp/probe MLProbe
//
// Before the fix:
//
//   HotSpot   nameToModule = {cratonvm.dyn.translet=module cratonvm.dyn.translet}
//             findModule(...) = Optional[module cratonvm.dyn.translet]
//   CratonVM  nameToModule = {cratonvm.dyn.translet=module cratonvm.dyn.translet}
//             findModule(...) = Optional.empty
//
// The `nameToModule` line is the load-bearing one and the reason this probe
// prints it: the JDK's own authority for the layer WAS populated, so this is
// not "defineModules does not work". Only our `findModule` override, which
// answered from the boot ModuleRegistry, disagreed with it. `layer.modules()`
// is printed for the same reason — it already read that map, so it listed the
// very module `findModule` could not find, and the two disagreeing is the
// tell.
import java.lang.module.*;
import java.lang.reflect.Field;
import java.util.*;

public class MLProbe {
    public static void main(String[] a) throws Exception {
        String mn = "cratonvm.dyn.translet";
        ModuleDescriptor descriptor = ModuleDescriptor.newModule(mn)
                .requires("java.base").exports("p").build();
        ModuleReference mref = new ModuleReference(descriptor, null) {
            @Override public ModuleReader open() { throw new UnsupportedOperationException(); }
        };
        ModuleFinder finder = new ModuleFinder() {
            @Override public Optional<ModuleReference> find(String name) {
                return name.equals(mn) ? Optional.of(mref) : Optional.empty();
            }
            @Override public Set<ModuleReference> findAll() { return Set.of(mref); }
        };
        ModuleLayer bootLayer = ModuleLayer.boot();
        System.out.println("bootLayer class = " + bootLayer.getClass().getName());
        Configuration cf = bootLayer.configuration().resolve(finder, ModuleFinder.of(), Set.of(mn));
        System.out.println("cf = " + cf + "  modules=" + cf.modules().size());
        ClassLoader ld = MLProbe.class.getClassLoader();
        ModuleLayer layer = bootLayer.defineModules(cf, x -> ld);
        System.out.println("layer class = " + layer.getClass().getName());
        try {
            Field f = ModuleLayer.class.getDeclaredField("nameToModule");
            f.setAccessible(true);
            System.out.println("nameToModule = " + f.get(layer));
        } catch (Throwable t) {
            System.out.println("nameToModule read failed: " + t);
        }
        System.out.println("findModule(" + mn + ") = " + layer.findModule(mn));
        System.out.println("layer.modules() = " + layer.modules());
    }
}
