// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 124, lane L5 -- `i45-L5-proposal-one-module-access-question-by-identity`,
// the residual the wave-123 review found: a COMMAND-LINE `--add-exports
// java.base/p=java.sql` (or `--add-opens`) names the boot layer's `java.sql`
// (`ModuleBootstrap.addExtraExportsOrOpens` -> `bootLayer.findModule`). A
// module of a user `ModuleLayer` that is also called `java.sql` is a different
// module: HotSpot answers `false` for it. The name-keyed registry
// (`is_package_exported_to_layer_module` / `is_package_open_to_layer_module`)
// answered `true`.
//
// Self-contained: the layer module is built from a `ModuleDescriptor` and an
// in-memory `ModuleFinder` (no class files; nothing is loaded from it).
//
// Run: javac -d . L5W124CommandLineEdgeLayerTarget.java
//      java --add-exports java.base/sun.security.x509=java.sql --add-opens java.base/sun.security.util=java.sql -cp . L5W124CommandLineEdgeLayerTarget   (HotSpot)
//      cratonvm --jdk-only|--compatible --add-exports java.base/sun.security.x509=java.sql --add-opens java.base/sun.security.util=java.sql -cp . L5W124CommandLineEdgeLayerTarget
//      CRATONVM_MODULE_ACCESS_EXPORT_ONE_QUESTION=1 (same command lines)
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags and -Xint)
// layer-module java.sql same-name-as-boot=true same-object=false
// boot-target cl-export true
// layer-target cl-export false
// boot-target cl-open true
// layer-target cl-open false
// boot-target cl-open-exports true
// layer-target cl-open-exports false
// boot-target cl-export-not-open false
// layer-target unqualified-export true
// boot-target no-edge false
// layer-target no-edge false
// HOTSPOT_EXPECTED_END

import java.lang.module.Configuration;
import java.lang.module.ModuleDescriptor;
import java.lang.module.ModuleFinder;
import java.lang.module.ModuleReader;
import java.lang.module.ModuleReference;
import java.net.URI;
import java.nio.ByteBuffer;
import java.util.Optional;
import java.util.Set;
import java.util.stream.Stream;

public class L5W124CommandLineEdgeLayerTarget {
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

    public static void main(String[] args) {
        ModuleDescriptor d = ModuleDescriptor.newModule("java.sql").packages(Set.of("l5w124.zz")).build();
        ModuleLayer boot = ModuleLayer.boot();
        Configuration cf = boot.configuration().resolve(finderOf(d), ModuleFinder.of(), Set.of("java.sql"));
        ModuleLayer layer = boot.defineModulesWithOneLoader(cf, ClassLoader.getSystemClassLoader());
        Module layerMod = layer.findModule("java.sql").orElseThrow();
        Module bootMod = boot.findModule("java.sql").orElseThrow();
        Module base = Object.class.getModule();
        System.out.println("layer-module " + layerMod.getName() + " same-name-as-boot="
                + layerMod.getName().equals(bootMod.getName()) + " same-object=" + (layerMod == bootMod));
        System.out.println("boot-target cl-export " + base.isExported("sun.security.x509", bootMod));
        System.out.println("layer-target cl-export " + base.isExported("sun.security.x509", layerMod));
        System.out.println("boot-target cl-open " + base.isOpen("sun.security.util", bootMod));
        System.out.println("layer-target cl-open " + base.isOpen("sun.security.util", layerMod));
        System.out.println("boot-target cl-open-exports " + base.isExported("sun.security.util", bootMod));
        System.out.println("layer-target cl-open-exports " + base.isExported("sun.security.util", layerMod));
        System.out.println("boot-target cl-export-not-open " + base.isOpen("sun.security.x509", bootMod));
        System.out.println("layer-target unqualified-export " + base.isExported("java.lang", layerMod));
        System.out.println("boot-target no-edge " + base.isExported("sun.nio.ch", bootMod));
        System.out.println("layer-target no-edge " + base.isExported("sun.nio.ch", layerMod));
    }
}
