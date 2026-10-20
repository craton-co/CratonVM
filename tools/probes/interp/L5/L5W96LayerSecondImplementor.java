// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4, wave 96, lane L5 -- the SECOND class a module
// layer's loader defines over a supertype that another loader of the layer
// defines (`L5W86ModuleLayers`, `many-loaders`: `ServiceConfigurationError:
// api.Svc: Unable to load impl.B` caused by `NoClassDefFoundError: api/Svc`
// in both modes).
//
// `ModuleLayer.defineModulesWithManyLoaders` gives m.api and m.impl one
// `jdk.internal.loader.Loader` each; m.impl's loader answers `api.Svc`
// through its `remotePackageToLoader` map (m.api's loader), outside its
// parent chain. The define pre-pass asked m.impl's loader for `api/Svc` when
// it defined `impl.A` and linked the answer by identity; for `impl.B` its
// supertype dictionary (`CRATONVM_DEFINE_SUPERTYPE_DICTIONARY`, default on)
// remembered only that the name was answered, not the class, so the define
// linked `api/Svc` by name through the flat store, which cannot see m.api's
// loader. Since wave 96 the dictionary row keeps the answered class.
//
// Run (no setup, no args; writes two module directories under a temp dir):
//   $J/javac -d out L5W96LayerSecondImplementor.java
//   $J/java -cp out L5W96LayerSecondImplementor                       # HotSpot
//   $B --java-home $J/.. [--nojit] [--compatible] -cp out L5W96LayerSecondImplementor
//
// Expected block derived from the JDK 25 sources (`Loader.loadClass` /
// `findClassInModuleOrNull`, `ServiceLoader` provider order); the many-loaders
// lines are the ones `L5W86ModuleLayers` measured on HotSpot.
//
// HOTSPOT_EXPECTED_BEGIN
// distinct=true
// impl.A svc-is-api=true loader-is-impl=true
// impl.B svc-is-api=true loader-is-impl=true
// impl.C svc-is-api=true loader-is-impl=true
// providers: [impl.A, impl.B, impl.C]
// HOTSPOT_EXPECTED_END

import java.io.IOException;
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.module.Configuration;
import java.lang.module.ModuleDescriptor;
import java.lang.module.ModuleFinder;
import java.lang.module.ModuleReader;
import java.lang.module.ModuleReference;
import java.net.URI;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.List;
import java.util.Map;
import java.util.Optional;
import java.util.ServiceLoader;
import java.util.Set;
import java.util.stream.Collectors;
import java.util.stream.Stream;

public class L5W96LayerSecondImplementor {
    static final ClassDesc SVC = ClassDesc.of("api.Svc");
    static final MethodTypeDesc NAME = MethodTypeDesc.of(ConstantDescs.CD_String);

    static void write(Path root, String path, byte[] bytes) throws IOException {
        Path p = root.resolve(path);
        Files.createDirectories(p.getParent());
        Files.write(p, bytes);
    }

    static byte[] svc() {
        return ClassFile.of().build(SVC, cb -> cb
                .withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_INTERFACE | ClassFile.ACC_ABSTRACT)
                .withMethod("name", NAME, ClassFile.ACC_PUBLIC | ClassFile.ACC_ABSTRACT, mb -> {}));
    }

    static byte[] impl(String simple) {
        return ClassFile.of().build(ClassDesc.of("impl." + simple), cb -> cb
                .withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER)
                .withInterfaceSymbols(SVC)
                .withMethodBody("<init>", ConstantDescs.MTD_void, ClassFile.ACC_PUBLIC, c -> c
                        .aload(0)
                        .invokespecial(ConstantDescs.CD_Object, "<init>", ConstantDescs.MTD_void)
                        .return_())
                .withMethodBody("name", NAME, ClassFile.ACC_PUBLIC, c -> c.ldc(simple).areturn()));
    }

    static ModuleReference ref(ModuleDescriptor d, Path root) {
        return new ModuleReference(d, root.toUri()) {
            @Override
            public ModuleReader open() {
                return new ModuleReader() {
                    @Override
                    public Optional<URI> find(String name) {
                        Path p = root.resolve(name);
                        return Files.isRegularFile(p) ? Optional.of(p.toUri()) : Optional.empty();
                    }

                    @Override
                    public Stream<String> list() throws IOException {
                        try (Stream<Path> s = Files.walk(root)) {
                            List<String> names = s.filter(Files::isRegularFile)
                                    .map(p -> root.relativize(p).toString().replace('\\', '/'))
                                    .collect(Collectors.toList());
                            return names.stream();
                        }
                    }

                    @Override
                    public void close() {}
                };
            }
        };
    }

    public static void main(String[] args) throws Exception {
        Path tmp = Files.createTempDirectory("l5w96");
        Path apiDir = tmp.resolve("m.api");
        Path implDir = tmp.resolve("m.impl");
        write(apiDir, "api/Svc.class", svc());
        for (String s : List.of("A", "B", "C")) {
            write(implDir, "impl/" + s + ".class", impl(s));
        }
        ModuleDescriptor api = ModuleDescriptor.newModule("m.api").exports("api").build();
        ModuleDescriptor impl = ModuleDescriptor.newModule("m.impl")
                .requires("m.api")
                .exports("impl")
                .provides("api.Svc", List.of("impl.A", "impl.B", "impl.C"))
                .build();
        Map<String, ModuleReference> refs = Map.of("m.api", ref(api, apiDir), "m.impl", ref(impl, implDir));
        ModuleFinder f = new ModuleFinder() {
            @Override
            public Optional<ModuleReference> find(String name) {
                return Optional.ofNullable(refs.get(name));
            }

            @Override
            public Set<ModuleReference> findAll() {
                return Set.copyOf(refs.values());
            }
        };
        ModuleLayer boot = ModuleLayer.boot();
        Configuration cf = boot.configuration().resolve(f, ModuleFinder.of(), Set.of("m.impl"));
        ModuleLayer many = ModuleLayer.defineModulesWithManyLoaders(
                cf, List.of(boot), ClassLoader.getSystemClassLoader()).layer();
        ClassLoader apiLoader = many.findLoader("m.api");
        ClassLoader implLoader = many.findLoader("m.impl");
        System.out.println("distinct=" + (apiLoader != implLoader));
        Class<?> svc = apiLoader.loadClass("api.Svc");
        Module implModule = many.findModule("m.impl").orElseThrow();
        for (String s : List.of("A", "B", "C")) {
            try {
                Class<?> c = Class.forName(implModule, "impl." + s);
                System.out.println("impl." + s + " svc-is-api=" + (c.getInterfaces()[0] == svc)
                        + " loader-is-impl=" + (c.getClassLoader() == implLoader));
            } catch (Throwable t) {
                System.out.println("impl." + s + " " + t);
            }
        }
        System.out.println("providers: " + ServiceLoader.load(many, svc).stream()
                .map(p -> p.type().getName())
                .collect(Collectors.toList()));
    }
}
