// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i6 wave 117, lane L5 -- a `-m` launch through
// `LauncherHelper.checkAndLoadMain(.., LM_MODULE, ..)`, as `java.c` makes it
// (`i58-L5-proposal-the-launcher-runs-launcherhelper`, "Wave 117"): the
// `java.launcher.module.error2` / `error3` texts the helper prints, and the
// `jdk.module.main` / `jdk.module.main.class` properties a module launch sets.
// L5W86ModuleLaunch covers the launcher-side refusals (absent module, no
// ModuleMainClass). The probe writes the modules under a temp dir (exploded and
// one modular jar, built with java.lang.classfile and java.util.jar) and runs
// the SAME launcher as a child per case, printing its stdout, stderr and exit.
//
// Cases:
//   props            exploded m.p, main prints the two properties
//   props-jar        the same from a modular jar
//   jar-missing      `-m m.pj/pj.Nope` on the modular jar: module.error2
//   wrong-name       w/Bad.class holds class w.Other: module.error3 (NoClassDefFoundError)
//   missing-super    s.Main extends s.Gone, absent: module.error3 (NoClassDefFoundError)
//   no-main-method   n.Main has no main: cls.error2
//
// Run:
//   $J/javac -d out L5W117ModuleLaunchHelperTexts.java
//   $J/java -cp out L5W117ModuleLaunchHelperTexts                                   # HotSpot
//   $B --jdk-only -cp out L5W117ModuleLaunchHelperTexts --jdk-only
//   $B --compatible -cp out L5W117ModuleLaunchHelperTexts --compatible
//   (CratonVM needs its mode flag passed on to the child; add --java-home <jdk> to both if
//    the JDK is not found otherwise)
//
// HOTSPOT_EXPECTED_BEGIN
// props: exit=0 out=[m.p p.Main] err=[]
// props-jar: exit=0 out=[m.pj pj.Main] err=[]
// jar-missing: exit=1 out=[] err=[Error: Could not find or load main class pj.Nope in module m.pj]
// wrong-name: exit=1 out=[] err=[Error: Unable to load main class w.Bad in module m.w | Caused by: java.lang.NoClassDefFoundError: w/Bad (wrong name: w/Other)]
// missing-super: exit=1 out=[] err=[Error: Unable to load main class s.Main in module m.s | Caused by: java.lang.NoClassDefFoundError: s/Gone]
// no-main-method: exit=1 out=[] err=[Error: Main method not found in class n.Main, please define the main method as: |    public static void main(String[] args) | or a JavaFX application class must extend javafx.application.Application]
// HOTSPOT_EXPECTED_END

import java.io.IOException;
import java.io.OutputStream;
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
import java.util.jar.JarEntry;
import java.util.jar.JarOutputStream;
import java.util.stream.Collectors;

public class L5W117ModuleLaunchHelperTexts {
    static final ClassDesc PRINT_STREAM = ClassDesc.of("java.io.PrintStream");
    static final ClassDesc SYSTEM = ClassDesc.of("java.lang.System");
    static final ClassDesc STRING_BUILDER = ClassDesc.of("java.lang.StringBuilder");
    static final MethodTypeDesc PRINTLN = MethodTypeDesc.of(ConstantDescs.CD_void, ConstantDescs.CD_String);
    static final MethodTypeDesc APPEND = MethodTypeDesc.of(STRING_BUILDER, ConstantDescs.CD_String);
    static final MethodTypeDesc GET_PROPERTY = MethodTypeDesc.of(ConstantDescs.CD_String, ConstantDescs.CD_String);
    static final MethodTypeDesc MAIN = MethodTypeDesc.of(ConstantDescs.CD_void, ConstantDescs.CD_String.arrayType());

    static byte[] moduleInfo(String name, String pkg) {
        ModuleAttribute attr = ModuleAttribute.of(ModuleDesc.of(name), mb -> {
            mb.requires(ModuleDesc.of("java.base"), ClassFile.ACC_MANDATED, null);
            mb.exports(PackageDesc.of(pkg), 0);
        });
        return ClassFile.of().buildModule(attr, cb -> cb.with(ModulePackagesAttribute.ofNames(PackageDesc.of(pkg))));
    }

    /** `main` prints `System.getProperty("jdk.module.main") + " " + ...("jdk.module.main.class")`. */
    static byte[] propsMain(String cls) {
        return ClassFile.of().build(ClassDesc.of(cls), cb -> cb
                .withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER)
                .withMethodBody("main", MAIN, ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC, c -> c
                        .getstatic(SYSTEM, "out", PRINT_STREAM)
                        .new_(STRING_BUILDER)
                        .dup()
                        .invokespecial(STRING_BUILDER, "<init>", MethodTypeDesc.of(ConstantDescs.CD_void))
                        .ldc("jdk.module.main")
                        .invokestatic(SYSTEM, "getProperty", GET_PROPERTY)
                        .invokevirtual(STRING_BUILDER, "append", APPEND)
                        .ldc(" ")
                        .invokevirtual(STRING_BUILDER, "append", APPEND)
                        .ldc("jdk.module.main.class")
                        .invokestatic(SYSTEM, "getProperty", GET_PROPERTY)
                        .invokevirtual(STRING_BUILDER, "append", APPEND)
                        .invokevirtual(STRING_BUILDER, "toString", MethodTypeDesc.of(ConstantDescs.CD_String))
                        .invokevirtual(PRINT_STREAM, "println", PRINTLN)
                        .return_()));
    }

    /** A class `cls` extending `superName`, with an empty static `main`. */
    static byte[] plainMain(String cls, String superName, boolean withMain) {
        return ClassFile.of().build(ClassDesc.of(cls), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER).withSuperclass(ClassDesc.of(superName));
            if (withMain) {
                cb.withMethodBody("main", MAIN, ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC, c -> c.return_());
            } else {
                cb.withMethodBody("helper", MethodTypeDesc.of(ConstantDescs.CD_void),
                        ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC, c -> c.return_());
            }
        });
    }

    static void exploded(Path dir, String module, String pkg, String file, byte[] bytes) throws IOException {
        Path root = dir.resolve(module);
        Files.createDirectories(root.resolve(pkg));
        Files.write(root.resolve("module-info.class"), moduleInfo(module, pkg));
        Files.write(root.resolve(pkg).resolve(file), bytes);
    }

    static void modularJar(Path jar, String module, String pkg, String file, byte[] bytes) throws IOException {
        try (OutputStream os = Files.newOutputStream(jar); JarOutputStream out = new JarOutputStream(os)) {
            out.putNextEntry(new JarEntry("module-info.class"));
            out.write(moduleInfo(module, pkg));
            out.closeEntry();
            out.putNextEntry(new JarEntry(pkg + "/" + file));
            out.write(bytes);
            out.closeEntry();
        }
    }

    static String lines(byte[] bytes) {
        return new String(bytes, StandardCharsets.UTF_8).lines()
                .filter(l -> !l.isBlank())
                .collect(Collectors.joining(" | "));
    }

    static void run(String label, List<String> prefix, Path mods, String... tail) throws Exception {
        List<String> cmd = new ArrayList<>(prefix);
        cmd.add("-p");
        cmd.add(mods.toString());
        cmd.addAll(Arrays.asList(tail));
        Process p = new ProcessBuilder(cmd).start();
        p.getOutputStream().close();
        byte[] out = p.getInputStream().readAllBytes();
        byte[] err = p.getErrorStream().readAllBytes();
        int exit = p.waitFor();
        System.out.println(label + ": exit=" + exit + " out=[" + lines(out) + "] err=[" + lines(err) + "]");
    }

    public static void main(String[] args) throws Exception {
        Path mods = Files.createTempDirectory("l5w117-mods");
        exploded(mods, "m.p", "p", "Main.class", propsMain("p.Main"));
        exploded(mods, "m.w", "w", "Bad.class", plainMain("w.Other", "java.lang.Object", true));
        exploded(mods, "m.s", "s", "Main.class", plainMain("s.Main", "s.Gone", true));
        exploded(mods, "m.n", "n", "Main.class", plainMain("n.Main", "java.lang.Object", false));
        Path jars = Files.createTempDirectory("l5w117-jars");
        modularJar(jars.resolve("m.pj.jar"), "m.pj", "pj", "Main.class", propsMain("pj.Main"));

        List<String> prefix = new ArrayList<>();
        prefix.add(ProcessHandle.current().info().command().orElseThrow());
        prefix.addAll(Arrays.asList(args));

        run("props", prefix, mods, "-m", "m.p/p.Main");
        run("props-jar", prefix, jars, "-m", "m.pj/pj.Main");
        run("jar-missing", prefix, jars, "-m", "m.pj/pj.Nope");
        run("wrong-name", prefix, mods, "-m", "m.w/w.Bad");
        run("missing-super", prefix, mods, "-m", "m.s/s.Main");
        run("no-main-method", prefix, mods, "-m", "m.n/n.Main");
    }
}
