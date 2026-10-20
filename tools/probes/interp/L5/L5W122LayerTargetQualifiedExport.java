// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 122, lane L5 -- `i45-L5-proposal-one-module-access-question-by-identity`,
// the `Module.isExported(pn, other)` / `isOpen(pn, other)` natives asked about
// a boot-layer module with a TARGET module of a user `ModuleLayer` that has the
// same NAME as a boot-layer module (`jdk.unsupported` here). java.base's
// descriptor says `exports jdk.internal.misc to ... jdk.unsupported`; the JDK
// resolves that target in java.base's own configuration
// (`Module.initExportsAndOpens` -> `findModule`), so the layer's
// `jdk.unsupported` is NOT a target: HotSpot answers `false`. A name-keyed
// registry answers `true` for it. Unqualified exports and opens reach it.
//
// Self-contained: the layer module is built from a `ModuleDescriptor` and an
// in-memory `ModuleFinder` (no class files needed; nothing is loaded from it).
//
// Run: javac -d . L5W122LayerTargetQualifiedExport.java
//      java -cp . L5W122LayerTargetQualifiedExport                       (HotSpot)
//      cratonvm --jdk-only -cp . L5W122LayerTargetQualifiedExport
//      cratonvm --compatible -cp . L5W122LayerTargetQualifiedExport
//      CRATONVM_MODULE_ACCESS_EXPORT_ONE_QUESTION=1 cratonvm --jdk-only|--compatible -cp . L5W122LayerTargetQualifiedExport
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25, default flags)
// layer-module jdk.unsupported same-name-as-boot=true same-object=false
// boot-target qualified-export true
// layer-target qualified-export false
// layer-target unqualified-export true
// layer-target qualified-open false
// layer-target unqualified-open false
// layer-target foreign-package false
// boot-target unexported-package false
// layer-target unexported-package false
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

public class L5W122LayerTargetQualifiedExport {
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
        ModuleDescriptor d = ModuleDescriptor.newModule("jdk.unsupported").packages(Set.of("l5w122.zz")).build();
        ModuleLayer boot = ModuleLayer.boot();
        Configuration cf = boot.configuration().resolve(finderOf(d), ModuleFinder.of(), Set.of("jdk.unsupported"));
        ModuleLayer layer = boot.defineModulesWithOneLoader(cf, ClassLoader.getSystemClassLoader());
        Module layerMod = layer.findModule("jdk.unsupported").orElseThrow();
        Module bootMod = boot.findModule("jdk.unsupported").orElseThrow();
        Module base = Object.class.getModule();
        System.out.println("layer-module " + layerMod.getName() + " same-name-as-boot="
                + layerMod.getName().equals(bootMod.getName()) + " same-object=" + (layerMod == bootMod));
        System.out.println("boot-target qualified-export " + base.isExported("jdk.internal.misc", bootMod));
        System.out.println("layer-target qualified-export " + base.isExported("jdk.internal.misc", layerMod));
        System.out.println("layer-target unqualified-export " + base.isExported("java.lang", layerMod));
        System.out.println("layer-target qualified-open " + base.isOpen("jdk.internal.misc", layerMod));
        System.out.println("layer-target unqualified-open " + base.isOpen("java.lang", layerMod));
        System.out.println("layer-target foreign-package " + base.isExported("l5w122.zz", layerMod));
        System.out.println("boot-target unexported-package " + base.isExported("sun.nio.ch", bootMod));
        System.out.println("layer-target unexported-package " + base.isExported("sun.nio.ch", layerMod));
    }
}
