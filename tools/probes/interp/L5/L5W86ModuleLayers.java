// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4, wave 86, lane L5 -- named modules in a user layer as
// the interpreter sees them:
//
// * two modules (m.api exports api; m.impl requires m.api, exports impl,
//   keeps impl.hidden, provides api.Svc with impl.A and impl.B) are written
//   under a temp dir at run time: classes with java.lang.classfile, resources
//   as files, descriptors with ModuleDescriptor.newModule, read back through a
//   ModuleFinder over the directories;
// * ModuleLayer.defineModulesWithOneLoader and ...WithManyLoaders;
// * ServiceLoader.load(layer, svc): provider order (the provides
//   declaration's), Provider.type() (for a static provider() method: its
//   RETURN type, here api.Svc), a static provider() method;
// * Class.forName(Module, String);
// * Module.getResourceAsStream's encapsulation (a package resource needs the
//   package open to the caller; .class files, non-package paths and
//   META-INF are never encapsulated);
// * setAccessible on a member of a non-open package: InaccessibleObjectException
//   text, then Controller.addOpens and the access it grants.
//
// Identity hashes (`unnamed module @<hex>`) are printed `@?`.
//
// Run:
//   $J/javac -d out L5W86ModuleLayers.java
//   $J/java -cp out L5W86ModuleLayers                       # HotSpot
//   $B --java-home $J/.. -cp out L5W86ModuleLayers
//   $B --compatible --java-home $J/.. -cp out L5W86ModuleLayers
// Expected block read from the JDK 25 sources (ServiceLoader, Module,
// AccessibleObject.checkCanSetAccessible); confirm it on the host first.
//
// HOTSPOT_EXPECTED_BEGIN
// one-loader same=true svc-loader=true
// packages: [impl, impl.hidden]
// providers: [impl.A, api.Svc]
// names: [A, B-provider]
// forName impl.A: impl.A
// forName api.Svc in m.impl: null
// forName impl.hidden.Secret: impl.hidden.Secret
// exported impl=true impl.hidden=false open impl=false
// resource top.txt: true
// resource META-INF/info.txt: true
// resource impl/res.txt: false
// resource impl/hidden/res.txt: false
// resource impl/hidden/Secret.class: true
// setAccessible Secret.s: java.lang.reflect.InaccessibleObjectException: Unable to make field private static final java.lang.String impl.hidden.Secret.s accessible: module m.impl does not "opens impl.hidden" to unnamed module @?
// trySetAccessible A.count: false
// setAccessible String.value: java.lang.reflect.InaccessibleObjectException: Unable to make field private final byte[] java.lang.String.value accessible: module java.base does not "opens java.lang" to unnamed module @?
// after addOpens impl.hidden: secret open=true
// after addOpens impl: resource impl/res.txt: true
// many-loaders distinct=true svc-loader-is-api=true impl-loader-is-impl=true
// many-loaders providers: [impl.A, api.Svc]
// default boot layer java.se=false jdk.incubator.vector=false
// incubator class: java.lang.ClassNotFoundException
// HOTSPOT_EXPECTED_END

import java.io.IOException;
import java.io.InputStream;
import java.lang.classfile.ClassFile;
import java.lang.classfile.attribute.ConstantValueAttribute;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.module.Configuration;
import java.lang.module.ModuleDescriptor;
import java.lang.module.ModuleFinder;
import java.lang.module.ModuleReader;
import java.lang.module.ModuleReference;
import java.lang.reflect.Field;
import java.lang.reflect.Method;
import java.net.URI;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.List;
import java.util.Map;
import java.util.Optional;
import java.util.ServiceLoader;
import java.util.Set;
import java.util.TreeSet;
import java.util.stream.Collectors;
import java.util.stream.Stream;

public class L5W86ModuleLayers {
    static final ClassDesc SVC = ClassDesc.of("api.Svc");
    static final MethodTypeDesc NAME = MethodTypeDesc.of(ConstantDescs.CD_String);

    static void write(Path root, String path, byte[] bytes) throws IOException {
        Path p = root.resolve(path);
        Files.createDirectories(p.getParent() == null ? root : p.getParent());
        Files.write(p, bytes);
    }

    static byte[] svc() {
        return ClassFile.of().build(SVC, cb -> cb
                .withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_INTERFACE | ClassFile.ACC_ABSTRACT)
                .withMethod("name", NAME, ClassFile.ACC_PUBLIC | ClassFile.ACC_ABSTRACT, mb -> {}));
    }

    static byte[] implA() {
        ClassDesc a = ClassDesc.of("impl.A");
        return ClassFile.of().build(a, cb -> cb
                .withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER)
                .withInterfaceSymbols(SVC)
                .withField("count", ConstantDescs.CD_int, ClassFile.ACC_PRIVATE)
                .withMethodBody("<init>", ConstantDescs.MTD_void, ClassFile.ACC_PUBLIC, c -> c
                        .aload(0)
                        .invokespecial(ConstantDescs.CD_Object, "<init>", ConstantDescs.MTD_void)
                        .return_())
                .withMethodBody("name", NAME, ClassFile.ACC_PUBLIC, c -> c.ldc("A").areturn()));
    }

    static byte[] implB() {
        ClassDesc b = ClassDesc.of("impl.B");
        return ClassFile.of().build(b, cb -> cb
                .withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER)
                .withInterfaceSymbols(SVC)
                .withMethodBody("<init>", ConstantDescs.MTD_void, ClassFile.ACC_PRIVATE, c -> c
                        .aload(0)
                        .invokespecial(ConstantDescs.CD_Object, "<init>", ConstantDescs.MTD_void)
                        .return_())
                .withMethodBody("provider", MethodTypeDesc.of(SVC), ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                        c -> c.new_(b).dup().invokespecial(b, "<init>", ConstantDescs.MTD_void).areturn())
                .withMethodBody("name", NAME, ClassFile.ACC_PUBLIC, c -> c.ldc("B-provider").areturn()));
    }

    static byte[] secret() {
        return ClassFile.of().build(ClassDesc.of("impl.hidden.Secret"), cb -> cb
                .withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER)
                .withField("s", ConstantDescs.CD_String, fb -> fb
                        .withFlags(ClassFile.ACC_PRIVATE | ClassFile.ACC_STATIC | ClassFile.ACC_FINAL)
                        .with(ConstantValueAttribute.of("secret"))));
    }

    /** A ModuleReference over a directory, its descriptor given. */
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

    static ModuleFinder finder(Map<String, ModuleReference> refs) {
        return new ModuleFinder() {
            @Override
            public Optional<ModuleReference> find(String name) {
                return Optional.ofNullable(refs.get(name));
            }

            @Override
            public Set<ModuleReference> findAll() {
                return Set.copyOf(refs.values());
            }
        };
    }

    static String stable(Throwable t) {
        return t.getClass().getName() + ": " + String.valueOf(t.getMessage()).replaceAll("@[0-9a-fA-F]+", "@?");
    }

    static boolean resource(Module m, String name) throws IOException {
        try (InputStream in = m.getResourceAsStream(name)) {
            return in != null;
        }
    }

    static List<String> providerTypes(ModuleLayer layer, Class<?> svc) {
        return ServiceLoader.load(layer, svc).stream()
                .map(p -> p.type().getName())
                .collect(Collectors.toList());
    }

    public static void main(String[] args) throws Exception {
        Path tmp = Files.createTempDirectory("l5w86");
        Path apiDir = tmp.resolve("m.api");
        Path implDir = tmp.resolve("m.impl");
        write(apiDir, "api/Svc.class", svc());
        write(implDir, "impl/A.class", implA());
        write(implDir, "impl/B.class", implB());
        write(implDir, "impl/hidden/Secret.class", secret());
        write(implDir, "impl/res.txt", "r".getBytes());
        write(implDir, "impl/hidden/res.txt", "h".getBytes());
        write(implDir, "top.txt", "t".getBytes());
        write(implDir, "META-INF/info.txt", "i".getBytes());

        ModuleDescriptor api = ModuleDescriptor.newModule("m.api").exports("api").build();
        ModuleDescriptor impl = ModuleDescriptor.newModule("m.impl")
                .requires("m.api")
                .exports("impl")
                .packages(Set.of("impl.hidden"))
                .provides("api.Svc", List.of("impl.A", "impl.B"))
                .build();
        ModuleFinder f = finder(Map.of("m.api", ref(api, apiDir), "m.impl", ref(impl, implDir)));
        ModuleLayer boot = ModuleLayer.boot();
        Configuration cf = boot.configuration().resolve(f, ModuleFinder.of(), Set.of("m.impl"));
        ClassLoader scl = ClassLoader.getSystemClassLoader();

        ModuleLayer.Controller ctl = ModuleLayer.defineModulesWithOneLoader(cf, List.of(boot), scl);
        ModuleLayer layer = ctl.layer();
        Module implModule = layer.findModule("m.impl").orElseThrow();
        Module me = L5W86ModuleLayers.class.getModule();
        Class<?> svc = layer.findLoader("m.api").loadClass("api.Svc");
        System.out.println("one-loader same=" + (layer.findLoader("m.api") == layer.findLoader("m.impl"))
                + " svc-loader=" + (svc.getClassLoader() == layer.findLoader("m.api")));
        System.out.println("packages: " + new TreeSet<>(implModule.getPackages()));

        System.out.println("providers: " + providerTypes(layer, svc));
        Method name = svc.getMethod("name");
        List<String> names = new ArrayList<>();
        for (Object o : ServiceLoader.load(layer, svc)) {
            names.add((String) name.invoke(o));
        }
        System.out.println("names: " + names);

        Class<?> a = Class.forName(implModule, "impl.A");
        System.out.println("forName impl.A: " + (a == null ? null : a.getName()));
        Class<?> notHere = Class.forName(implModule, "api.Svc");
        System.out.println("forName api.Svc in m.impl: " + (notHere == null ? null : notHere.getName()));
        Class<?> secret = Class.forName(implModule, "impl.hidden.Secret");
        System.out.println("forName impl.hidden.Secret: " + (secret == null ? null : secret.getName()));

        System.out.println("exported impl=" + implModule.isExported("impl")
                + " impl.hidden=" + implModule.isExported("impl.hidden")
                + " open impl=" + implModule.isOpen("impl", me));

        for (String r : List.of("top.txt", "META-INF/info.txt", "impl/res.txt", "impl/hidden/res.txt",
                "impl/hidden/Secret.class")) {
            System.out.println("resource " + r + ": " + resource(implModule, r));
        }

        Field s = secret.getDeclaredField("s");
        try {
            s.setAccessible(true);
            System.out.println("setAccessible Secret.s: ok");
        } catch (Throwable t) {
            System.out.println("setAccessible Secret.s: " + stable(t));
        }
        Field count = a.getDeclaredField("count");
        System.out.println("trySetAccessible A.count: " + count.trySetAccessible());
        Field value = String.class.getDeclaredField("value");
        try {
            value.setAccessible(true);
            System.out.println("setAccessible String.value: ok");
        } catch (Throwable t) {
            System.out.println("setAccessible String.value: " + stable(t));
        }

        ctl.addOpens(implModule, "impl.hidden", me);
        s.setAccessible(true);
        System.out.println("after addOpens impl.hidden: " + s.get(null) + " open=" + implModule.isOpen("impl.hidden", me));
        ctl.addOpens(implModule, "impl", me);
        System.out.println("after addOpens impl: resource impl/res.txt: " + resource(implModule, "impl/res.txt"));

        ModuleLayer many = ModuleLayer.defineModulesWithManyLoaders(cf, List.of(boot), scl).layer();
        Class<?> svc2 = many.findLoader("m.api").loadClass("api.Svc");
        Class<?> a2 = Class.forName(many.findModule("m.impl").orElseThrow(), "impl.A");
        System.out.println("many-loaders distinct=" + (many.findLoader("m.api") != many.findLoader("m.impl"))
                + " svc-loader-is-api=" + (svc2.getClassLoader() == many.findLoader("m.api"))
                + " impl-loader-is-impl=" + (a2.getClassLoader() == many.findLoader("m.impl")));
        System.out.println("many-loaders providers: " + providerTypes(many, svc2));

        // The default root set of a class-path launch (JEP 261): an incubator
        // module is not resolved without --add-modules, so neither is its class.
        System.out.println("default boot layer java.se=" + boot.findModule("java.se").isPresent()
                + " jdk.incubator.vector=" + boot.findModule("jdk.incubator.vector").isPresent());
        try {
            Class.forName("jdk.incubator.vector.IntVector");
            System.out.println("incubator class: found");
        } catch (Throwable t) {
            System.out.println("incubator class: " + t.getClass().getName());
        }
    }
}
