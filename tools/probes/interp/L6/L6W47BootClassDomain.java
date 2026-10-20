// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 47, lane L6: the `ProtectionDomain` of a class the
// BOOTSTRAP loader defined. HotSpot gives such a class none: `Class.
// getProtectionDomain()` answers the JDK's shared all-permission domain
// (`Class$Holder.allPermDomain`), whose `CodeSource` is null, the same object
// for every bootstrap class. `--jdk-only` has answered that since the jrt-fs
// fix (`lang_class.rs` `is_boot_defined_class`); `--compatible` answered a
// synthesised, non-null CodeSource and a fresh domain per call (from the
// code: `String codeSource=` not null, `same domain object=false`) until
// wave 47 (lane L6), which removed the helper's `--jdk-only` gate.
//
// HotSpot 25.0.3 prints (the same with -Xint; measured):
//     String codeSource=null
//     Object codeSource=null
//     same domain object=true
//     all permissions=true
//
//     javac -d out L6W47BootClassDomain.java
//     java|cratonvm [--compatible] [--nojit] -cp out L6W47BootClassDomain
import java.security.AllPermission;
import java.security.ProtectionDomain;

public class L6W47BootClassDomain {
    public static void main(String[] args) {
        ProtectionDomain s = String.class.getProtectionDomain();
        ProtectionDomain o = Object.class.getProtectionDomain();
        System.out.println("String codeSource=" + s.getCodeSource());
        System.out.println("Object codeSource=" + o.getCodeSource());
        System.out.println("same domain object=" + (s == o));
        System.out.println("all permissions=" + s.implies(new AllPermission()));
    }
}
