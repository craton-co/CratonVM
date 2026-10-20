// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4, wave 87, lane L5 -- the launcher's --patch-module and
// --limit-modules. The probe writes its fixtures under a temp dir (classes
// with java.lang.classfile) and runs the SAME launcher as a child for each
// case, printing the child's stdout, stderr and exit status:
//
// * --patch-module java.base=<dir>: a class the patch adds to java.lang is
//   loadable, in module java.base;
// * --patch-module m.app=<dir> with -m m.app: the patch's app.Main replaces
//   the module's own, and its new app.Extra is in module m.app;
// * --patch-module of a module the configuration does not hold: the warning;
// * --limit-modules java.base / java.sql: the boot layer and Class.forName;
// * --limit-modules of an unknown module: the boot-layer FindException.
//
// The child is `ProcessHandle.current().info().command()`; the probe's own
// program arguments are passed to it first (CratonVM needs its mode flag and
// --java-home there, HotSpot nothing).
//
// Run:
//   $J/javac -d out L5W87ModuleOptions.java
//   $J/java -cp out L5W87ModuleOptions                                          # HotSpot
//   $B --java-home $J/.. -cp out L5W87ModuleOptions --java-home $J/..
//   $B --compatible --java-home $J/.. -cp out L5W87ModuleOptions --compatible --java-home $J/..
// Expected block read from the JDK 25 sources (ModuleBootstrap, ModulePatcher,
// limitFinder); confirm it on the host first.
//
// HOTSPOT_EXPECTED_BEGIN
// patch-java-base: exit=0 out=[child-base module=java.base hello=patched] err=[]
// patch-app-module: exit=0 out=[patched-main extra module=m.app] err=[]
// patch-unknown-module: exit=0 out=[child-plain ok] err=[WARNING: Unknown module: no.such specified to --patch-module]
// limit-java-base: exit=0 out=[child-limit java.base=true java.logging=false java.sql=false java.sql.Connection=java.lang.ClassNotFoundException] err=[]
// limit-java-sql: exit=0 out=[child-limit java.base=true java.logging=true java.sql=true java.sql.Connection=found] err=[]
// limit-unknown: exit=1 out=[Error occurred during initialization of boot layer | java.lang.module.FindException: Module no.such not found] err=[]
// HOTSPOT_EXPECTED_END

import java.io.IOException;
import java.lang.classfile.ClassFile;
import java.lang.classfile.attribute.ModuleAttribute;
import java.lang.classfile.attribute.ModulePackagesAttribute;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.constant.ModuleDesc;
import java.lang.constant.PackageDesc;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;
import java.util.stream.Collectors;

public class L5W87ModuleOptions {
    static final ClassDesc PRINT_STREAM = ClassDesc.of("java.io.PrintStream");
    static final ClassDesc SYSTEM = ClassDesc.of("java.lang.System");
    static final MethodTypeDesc PRINTLN = MethodTypeDesc.of(ConstantDescs.CD_void, ConstantDescs.CD_String);
    static final MethodTypeDesc STRING = MethodTypeDesc.of(ConstantDescs.CD_String);

    /** `public static String <method>() { return "<value>"; }` in class `name`. */
    static byte[] constantClass(String name, String method, String value) {
        return ClassFile.of().build(ClassDesc.of(name), cb -> cb
                .withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER)
                .withMethodBody(method, STRING, ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                        c -> c.ldc(value).areturn()));
    }

    /** `app.Main.main` printing `<prefix>` alone, or `<prefix> <Extra.hi()> module=<module of Extra>`. */
    static byte[] appMain(String prefix, boolean callExtra) {
        ClassDesc main = ClassDesc.of("app.Main");
        ClassDesc extra = ClassDesc.of("app.Extra");
        ClassDesc sb = ClassDesc.of("java.lang.StringBuilder");
        MethodTypeDesc append = MethodTypeDesc.of(sb, ConstantDescs.CD_String);
        return ClassFile.of().build(main, cb -> cb
                .withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER)
                .withMethodBody("main", MethodTypeDesc.of(ConstantDescs.CD_void, ConstantDescs.CD_String.arrayType()),
                        ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC, c -> {
                            c.getstatic(SYSTEM, "out", PRINT_STREAM);
                            if (callExtra) {
                                c.new_(sb).dup().ldc(prefix + " ")
                                        .invokespecial(sb, "<init>", MethodTypeDesc.of(ConstantDescs.CD_void, ConstantDescs.CD_String))
                                        .invokestatic(extra, "hi", STRING)
                                        .invokevirtual(sb, "append", append)
                                        .ldc(" module=")
                                        .invokevirtual(sb, "append", append)
                                        .ldc(extra)
                                        .invokevirtual(ConstantDescs.CD_Class, "getModule", MethodTypeDesc.of(ClassDesc.of("java.lang.Module")))
                                        .invokevirtual(ClassDesc.of("java.lang.Module"), "getName", STRING)
                                        .invokevirtual(sb, "append", append)
                                        .invokevirtual(sb, "toString", STRING);
                            } else {
                                c.ldc(prefix);
                            }
                            c.invokevirtual(PRINT_STREAM, "println", PRINTLN).return_();
                        }));
    }

    static byte[] moduleInfo(String name, String pkg) {
        ModuleAttribute attr = ModuleAttribute.of(ModuleDesc.of(name), mb -> {
            mb.requires(ModuleDesc.of("java.base"), ClassFile.ACC_MANDATED, null);
            mb.exports(PackageDesc.of(pkg), 0);
        });
        return ClassFile.of().buildModule(attr, cb -> cb.with(ModulePackagesAttribute.ofNames(PackageDesc.of(pkg))));
    }

    static void write(Path root, String path, byte[] bytes) throws IOException {
        Path p = root.resolve(path);
        Files.createDirectories(p.getParent());
        Files.write(p, bytes);
    }

    static String lines(byte[] bytes) {
        return new String(bytes, StandardCharsets.UTF_8).lines()
                .filter(l -> !l.isBlank())
                .collect(Collectors.joining(" | "));
    }

    static void run(String label, List<String> prefix, String... tail) throws Exception {
        List<String> cmd = new ArrayList<>(prefix);
        cmd.addAll(Arrays.asList(tail));
        Process p = new ProcessBuilder(cmd).start();
        p.getOutputStream().close();
        byte[] out = p.getInputStream().readAllBytes();
        byte[] err = p.getErrorStream().readAllBytes();
        int exit = p.waitFor();
        System.out.println(label + ": exit=" + exit + " out=[" + lines(out) + "] err=[" + lines(err) + "]");
    }

    static boolean present(String module) {
        return ModuleLayer.boot().findModule(module).isPresent();
    }

    static void child(String mode) throws Exception {
        switch (mode) {
            case "child-base" -> {
                Class<?> c = Class.forName("java.lang.PatchedHello");
                System.out.println("child-base module=" + c.getModule().getName()
                        + " hello=" + c.getMethod("hello").invoke(null));
            }
            case "child-plain" -> System.out.println("child-plain ok");
            case "child-limit" -> {
                String connection;
                try {
                    Class.forName("java.sql.Connection");
                    connection = "found";
                } catch (ClassNotFoundException e) {
                    connection = e.getClass().getName();
                }
                System.out.println("child-limit java.base=" + present("java.base")
                        + " java.logging=" + present("java.logging")
                        + " java.sql=" + present("java.sql")
                        + " java.sql.Connection=" + connection);
            }
            default -> System.out.println("unknown child mode " + mode);
        }
    }

    public static void main(String[] args) throws Exception {
        if (args.length == 1 && args[0].startsWith("child-")) {
            child(args[0]);
            return;
        }
        Path tmp = Files.createTempDirectory("l5w87");
        Path basePatch = tmp.resolve("base-patch");
        write(basePatch, "java/lang/PatchedHello.class", constantClass("java.lang.PatchedHello", "hello", "patched"));
        Path mods = tmp.resolve("mods");
        write(mods.resolve("m.app"), "module-info.class", moduleInfo("m.app", "app"));
        write(mods.resolve("m.app"), "app/Main.class", appMain("original", false));
        Path appPatch = tmp.resolve("app-patch");
        write(appPatch, "app/Main.class", appMain("patched-main", true));
        write(appPatch, "app/Extra.class", constantClass("app.Extra", "hi", "extra"));

        List<String> prefix = new ArrayList<>();
        prefix.add(ProcessHandle.current().info().command().orElseThrow());
        prefix.addAll(Arrays.asList(args));
        String cp = System.getProperty("java.class.path");
        String self = L5W87ModuleOptions.class.getName();

        run("patch-java-base", prefix, "--patch-module", "java.base=" + basePatch, "-cp", cp, self, "child-base");
        run("patch-app-module", prefix, "-p", mods.toString(), "--patch-module", "m.app=" + appPatch,
                "-m", "m.app/app.Main");
        run("patch-unknown-module", prefix, "--patch-module", "no.such=" + appPatch, "-cp", cp, self, "child-plain");
        run("limit-java-base", prefix, "--limit-modules", "java.base", "-cp", cp, self, "child-limit");
        run("limit-java-sql", prefix, "--limit-modules", "java.sql", "-cp", cp, self, "child-limit");
        run("limit-unknown", prefix, "--limit-modules", "no.such", "-cp", cp, self, "child-limit");
    }
}
