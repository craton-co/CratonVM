// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4, wave 89, lane L5 -- linking a hidden class defines no
// package. A hidden class's name is `<binary name>/0x<hex>`; the package step
// CratonVM runs at an application-loader class's link
// (`vm_util::class_path_package_check_key`) read a "package" at the last `/`,
// a fresh one per hidden class. HotSpot defines nothing: the hidden class is
// in its lookup class's package. The answers below must not move with
// `CRATONVM_LINK_SKIPS_HIDDEN_PACKAGE` (it only removes the work).
//
// Run:
//   $J/javac -d out L5W89HiddenClassPackage.java
//   $J/java -cp out L5W89HiddenClassPackage                                     # HotSpot
//   $B --java-home $J/.. -cp out L5W89HiddenClassPackage
//   CRATONVM_LINK_SKIPS_HIDDEN_PACKAGE=1 $B --java-home $J/.. -cp out L5W89HiddenClassPackage
//   (each also with --compatible)
//
// HOTSPOT_EXPECTED_BEGIN
// hidden package name: ""
// defined HiddenPkg: false
// defined lambda host: false
// lambda package name: ""
// HOTSPOT_EXPECTED_END

import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.invoke.MethodHandles;
import java.util.function.Supplier;

public class L5W89HiddenClassPackage {
    public static void main(String[] args) throws Throwable {
        MethodTypeDesc v = MethodTypeDesc.of(ConstantDescs.CD_void);
        byte[] bytes = ClassFile.of().build(ClassDesc.of("HiddenPkg"), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withMethodBody(ConstantDescs.INIT_NAME, v, ClassFile.ACC_PUBLIC,
                    c -> c.aload(0).invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME, v).return_());
        });
        Class<?> h = MethodHandles.lookup().defineHiddenClass(bytes, true).lookupClass();
        h.getConstructor().newInstance();
        ClassLoader app = L5W89HiddenClassPackage.class.getClassLoader();
        System.out.println("hidden package name: \"" + h.getPackageName() + "\"");
        System.out.println("defined HiddenPkg: " + (app.getDefinedPackage("HiddenPkg") != null));

        Supplier<String> lambda = () -> "x";
        lambda.get();
        String host = L5W89HiddenClassPackage.class.getName() + "$$Lambda";
        System.out.println("defined lambda host: " + (app.getDefinedPackage(host) != null));
        System.out.println("lambda package name: \"" + lambda.getClass().getPackageName() + "\"");
    }
}
