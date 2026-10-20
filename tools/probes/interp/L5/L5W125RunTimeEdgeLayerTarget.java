// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 125, lane L5 -- `i45-L5-proposal-one-module-access-question-by-identity`,
// the wave-124 next step: a RUN-TIME edge `Module.addExports(pn, other)` /
// `addOpens(pn, other)` names the `Module` object `other` itself (HotSpot's
// `addExports0` takes the module, the JDK's `ReflectionData.exports` is keyed by
// the module pair). A module of another `ModuleLayer` that shares the name, or
// the boot layer's module of that name, is a different module. CratonVM's
// natives recorded the edge by the target's NAME, so it reached every module of
// that name.
//
// The edges are added from java.base itself, through the JDK's package-private
// `Module.implAddExports` / `implAddOpens` (the bodies `addExports` /
// `addOpens` run once the caller check passed), so the probe needs
// `--add-opens java.base/java.lang=ALL-UNNAMED`.
//
// Self-contained: the layer modules are built from `ModuleDescriptor`s and an
// in-memory `ModuleFinder` (no class files; nothing is loaded from them).
//
// Run: javac -d . L5W125RunTimeEdgeLayerTarget.java
//      java --add-opens java.base/java.lang=ALL-UNNAMED -cp . L5W125RunTimeEdgeLayerTarget   (HotSpot)
//      cratonvm --jdk-only|--compatible --add-opens java.base/java.lang=ALL-UNNAMED -cp . L5W125RunTimeEdgeLayerTarget
//      CRATONVM_MODULE_ACCESS_EXPORT_ONE_QUESTION=1 (same command lines)
//
// With the argument `reverse` one more row prints (NOT part of the expected
// block): an edge to a layer's `java.sql` asked of the BOOT layer's `java.sql`.
// HotSpot: `reverse-boot-target export false`. CratonVM still answers `true`
// there (the name-keyed question; see the page's "still by name" list).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags and -Xint)
// layers m125a distinct=true java.sql layer-is-boot=false
// layer-a rt-export true
// layer-b rt-export false
// layer-b rt-export-unexported false
// layer-a rt-open true
// layer-b rt-open false
// layer-a rt-open-exports true
// layer-b rt-open-exports false
// boot-target rt-export-to-boot true
// layer-target rt-export-to-boot false
// boot-target rt-open-to-boot true
// layer-target rt-open-to-boot false
// layer-a unqualified-export true
// HOTSPOT_EXPECTED_END

import java.lang.module.Configuration;
import java.lang.module.ModuleDescriptor;
import java.lang.module.ModuleFinder;
import java.lang.module.ModuleReader;
import java.lang.module.ModuleReference;
import java.lang.reflect.Method;
import java.net.URI;
import java.nio.ByteBuffer;
import java.util.Optional;
import java.util.Set;
import java.util.stream.Stream;

public class L5W125RunTimeEdgeLayerTarget {
    static ModuleFinder finderOf(ModuleDescriptor d) {
        ModuleReference ref = new ModuleReference(d, URI.create("memory:///" + d.name())) {
            @Override
            public ModuleReader open() {
                return new ModuleReader() {
                    public Optional<URI> find(String name) {
                        return Optional.empty();
                    }

                    public Optional<ByteBuffer> read(String name) {
                        return Optional.empty();
                    }

                    public Stream<String> list() {
                        return Stream.empty();
                    }

                    public void close() {
                    }
                };
            }
        };
        return new ModuleFinder() {
            public Optional<ModuleReference> find(String name) {
                return name.equals(d.name()) ? Optional.of(ref) : Optional.empty();
            }

            public Set<ModuleReference> findAll() {
                return Set.of(ref);
            }
        };
    }

    static Module layerModule(String name, String pkg) {
        ModuleDescriptor d = ModuleDescriptor.newModule(name).packages(Set.of(pkg)).build();
        ModuleLayer boot = ModuleLayer.boot();
        Configuration cf = boot.configuration().resolve(finderOf(d), ModuleFinder.of(), Set.of(name));
        ModuleLayer layer = boot.defineModulesWithOneLoader(cf, ClassLoader.getSystemClassLoader());
        return layer.findModule(name).orElseThrow();
    }

    public static void main(String[] args) throws Exception {
        Module base = Object.class.getModule();
        Method addExports = Module.class.getDeclaredMethod("implAddExports", String.class, Module.class);
        Method addOpens = Module.class.getDeclaredMethod("implAddOpens", String.class, Module.class);
        addExports.setAccessible(true);
        addOpens.setAccessible(true);

        Module a = layerModule("m125a", "l5w125.a");
        Module b = layerModule("m125a", "l5w125.b");
        Module sqlLayer = layerModule("java.sql", "l5w125.sql");
        Module sqlBoot = ModuleLayer.boot().findModule("java.sql").orElseThrow();
        System.out.println("layers " + a.getName() + " distinct=" + (a != b && a.getName().equals(b.getName()))
                + " java.sql layer-is-boot=" + (sqlLayer == sqlBoot));

        // A run-time export to layer A's m125a reaches it alone.
        addExports.invoke(base, "sun.security.x509", a);
        System.out.println("layer-a rt-export " + base.isExported("sun.security.x509", a));
        System.out.println("layer-b rt-export " + base.isExported("sun.security.x509", b));
        System.out.println("layer-b rt-export-unexported " + base.isExported("sun.security.provider", b));

        // A run-time opening to layer A's m125a (an export too).
        addOpens.invoke(base, "sun.security.util", a);
        System.out.println("layer-a rt-open " + base.isOpen("sun.security.util", a));
        System.out.println("layer-b rt-open " + base.isOpen("sun.security.util", b));
        System.out.println("layer-a rt-open-exports " + base.isExported("sun.security.util", a));
        System.out.println("layer-b rt-open-exports " + base.isExported("sun.security.util", b));

        // A run-time edge to the BOOT layer's java.sql does not reach a layer's java.sql.
        addExports.invoke(base, "sun.security.timestamp", sqlBoot);
        System.out.println("boot-target rt-export-to-boot " + base.isExported("sun.security.timestamp", sqlBoot));
        System.out.println("layer-target rt-export-to-boot " + base.isExported("sun.security.timestamp", sqlLayer));
        addOpens.invoke(base, "sun.security.jca", sqlBoot);
        System.out.println("boot-target rt-open-to-boot " + base.isOpen("sun.security.jca", sqlBoot));
        System.out.println("layer-target rt-open-to-boot " + base.isOpen("sun.security.jca", sqlLayer));

        System.out.println("layer-a unqualified-export " + base.isExported("java.lang", a));

        if (args.length > 0 && args[0].equals("reverse")) {
            addExports.invoke(base, "sun.security.pkcs", sqlLayer);
            System.out.println("reverse-boot-target export " + base.isExported("sun.security.pkcs", sqlBoot));
        }
    }
}
