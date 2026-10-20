// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 48, lane L5: an `IllegalAccessError` naming a
// class of a module the JDK's own table defines to the PLATFORM loader
// (`java.sql`) says `of loader 'platform'`, as HotSpot's platform loader
// defines it; a `java.base` class keeps `'bootstrap'`
// (docs/internal/fixed-bugs/interpreter-L5-platform-module-classes-are-named-loader-bootstrap-in-hotspot-worded-messages-FIXED-20261012.md;
// the access texts' twin of `L4W47PlatformLoaderCastText`).
//
// The accessing classes are generated with the `java.lang.classfile` API
// (javac refuses the accesses) and defined in the probe's package:
//   class   -- `ldc java/sql/DriverInfo` (a package-private class of java.sql);
//   field   -- `getfield java/sql/Timestamp.nanos:I` (a private field);
//   base    -- `ldc java/util/ImmutableCollections` (package-private, java.base:
//              the control, `'bootstrap'`).
//
// HotSpot 25 prints (measured, JDK 25.0.3; the same with -Xint):
//     class IllegalAccessError: failed to access class java.sql.DriverInfo from class L5W48PlatformLoaderAccessText$Class (java.sql.DriverInfo is in module java.sql of loader 'platform'; L5W48PlatformLoaderAccessText$Class is in unnamed module of loader 'app')
//     field IllegalAccessError: class L5W48PlatformLoaderAccessText$Field tried to access private field java.sql.Timestamp.nanos (L5W48PlatformLoaderAccessText$Field is in unnamed module of loader 'app'; java.sql.Timestamp is in module java.sql of loader 'platform')
//     base IllegalAccessError: failed to access class java.util.ImmutableCollections from class L5W48PlatformLoaderAccessText$Base (java.util.ImmutableCollections is in module java.base of loader 'bootstrap'; L5W48PlatformLoaderAccessText$Base is in unnamed module of loader 'app')
// CratonVM before wave 48 (from the code): `of loader 'bootstrap'` for the
// two java.sql classes (`access_control::builtin_loader_name_and_id`).
// Needs a real JDK image (the table is `ModuleLoaderMap$Modules`).
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.invoke.MethodHandles;
import java.util.function.Supplier;

public class L5W48PlatformLoaderAccessText {
    static final ClassDesc SUPPLIER = ClassDesc.of("java.util.function.Supplier");

    /** A `Supplier` whose `get()` runs `body`. */
    static Supplier<?> make(String simple, java.util.function.Consumer<java.lang.classfile.CodeBuilder> body)
            throws Exception {
        ClassDesc me = ClassDesc.of(L5W48PlatformLoaderAccessText.class.getName() + "$" + simple);
        byte[] bytes = ClassFile.of().build(me, cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withInterfaceSymbols(SUPPLIER);
            cb.withMethodBody(ConstantDescs.INIT_NAME, ConstantDescs.MTD_void, ClassFile.ACC_PUBLIC,
                    code -> code.aload(0)
                            .invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME,
                                    ConstantDescs.MTD_void)
                            .return_());
            cb.withMethodBody("get", MethodTypeDesc.of(ConstantDescs.CD_Object), ClassFile.ACC_PUBLIC,
                    body);
        });
        Class<?> c = MethodHandles.lookup().defineClass(bytes);
        return (Supplier<?>) c.getConstructor().newInstance();
    }

    static void row(String label, Supplier<?> s) {
        try {
            s.get();
            System.out.println(label + " no error");
        } catch (Throwable t) {
            System.out.println(label + " " + t.getClass().getSimpleName() + ": " + t.getMessage());
        }
    }

    public static void main(String[] args) throws Exception {
        row("class", make("Class", code -> code.ldc(ClassDesc.of("java.sql.DriverInfo")).areturn()));
        row("field", make("Field", code -> code
                .new_(ClassDesc.of("java.sql.Timestamp"))
                .dup()
                .lconst_0()
                .invokespecial(ClassDesc.of("java.sql.Timestamp"), ConstantDescs.INIT_NAME,
                        MethodTypeDesc.of(ConstantDescs.CD_void, ConstantDescs.CD_long))
                .getfield(ClassDesc.of("java.sql.Timestamp"), "nanos", ConstantDescs.CD_int)
                .invokestatic(ConstantDescs.CD_Integer, "valueOf",
                        MethodTypeDesc.of(ConstantDescs.CD_Integer, ConstantDescs.CD_int))
                .areturn()));
        row("base", make("Base", code -> code.ldc(ClassDesc.of("java.util.ImmutableCollections")).areturn()));
    }
}
