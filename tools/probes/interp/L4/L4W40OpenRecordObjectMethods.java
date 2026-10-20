// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 40, lane L4
// (`i39-L4-objectmethods-tostring-names-the-receivers-class`): a record class
// that is NOT `final` (hand-assembled: `L4W40OpenRec extends Record`, a
// `Record` attribute, javac's `ObjectMethods` sites for `toString` / `equals`
// / `hashCode`) and a subclass `L4W40OpenRecSub`. The JDK's `ObjectMethods`
// names the linked record class in `toString`, tests `isInstance` against it
// in `equals` and folds the record class's getters in `hashCode`. CratonVM
// answered from the RECEIVER's class: `L4W40OpenRecSub[x=1]`, `false`,
// `false`, `0` (rows sub-tostring, sub-equals-rec, rec-equals-sub,
// sub-hashcode), through the whole-record walk and the record intrinsics.
// `Class.isRecord()` of the non-`final` class is `false` (JDK 25 tests the
// `FINAL` modifier); the `--compatible` native answered `true` (row
// is-record; `--jdk-only` runs the JDK's bytecode, which already tests it).
//
// Run: javac -d out L4W40OpenRecordObjectMethods.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible] -cp out L4W40OpenRecordObjectMethods
//
// Expected HotSpot 25 output (default and -Xint; `--compatible` identical):
//   rec-tostring: L4W40OpenRec[x=1]
//   sub-tostring: L4W40OpenRec[x=1]
//   rec-equals-rec: true
//   sub-equals-rec: true
//   rec-equals-sub: true
//   sub-equals-other: false
//   rec-hashcode: 1
//   sub-hashcode: 1
//   is-record: false false
import java.lang.classfile.ClassFile;
import java.lang.classfile.attribute.RecordAttribute;
import java.lang.classfile.attribute.RecordComponentInfo;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.DirectMethodHandleDesc;
import java.lang.constant.DynamicCallSiteDesc;
import java.lang.constant.MethodHandleDesc;
import java.lang.constant.MethodTypeDesc;

public class L4W40OpenRecordObjectMethods {
    static final class Loader extends ClassLoader {
        Loader() {
            super(L4W40OpenRecordObjectMethods.class.getClassLoader());
        }

        Class<?> define(String name, byte[] b) {
            return defineClass(name, b, 0, b.length);
        }
    }

    static byte[] record(ClassDesc rec) {
        ClassDesc objectMethods = ClassDesc.of("java.lang.runtime.ObjectMethods");
        DirectMethodHandleDesc bsm = MethodHandleDesc.ofMethod(DirectMethodHandleDesc.Kind.STATIC, objectMethods,
                "bootstrap",
                MethodTypeDesc.of(ConstantDescs.CD_Object, ClassDesc.of("java.lang.invoke.MethodHandles$Lookup"),
                        ConstantDescs.CD_String, ClassDesc.of("java.lang.invoke.TypeDescriptor"),
                        ConstantDescs.CD_Class, ConstantDescs.CD_String,
                        ClassDesc.of("java.lang.invoke.MethodHandle").arrayType()));
        DirectMethodHandleDesc getter = MethodHandleDesc.ofField(DirectMethodHandleDesc.Kind.GETTER, rec, "x",
                ConstantDescs.CD_int);
        DynamicCallSiteDesc toString = DynamicCallSiteDesc.of(bsm, "toString",
                MethodTypeDesc.of(ConstantDescs.CD_String, rec), rec, "x", getter);
        DynamicCallSiteDesc equals = DynamicCallSiteDesc.of(bsm, "equals",
                MethodTypeDesc.of(ConstantDescs.CD_boolean, rec, ConstantDescs.CD_Object), rec, "x", getter);
        DynamicCallSiteDesc hashCode = DynamicCallSiteDesc.of(bsm, "hashCode",
                MethodTypeDesc.of(ConstantDescs.CD_int, rec), rec, "x", getter);
        return ClassFile.of().build(rec, cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withSuperclass(ClassDesc.of("java.lang.Record"));
            cb.withField("x", ConstantDescs.CD_int, ClassFile.ACC_PRIVATE | ClassFile.ACC_FINAL);
            cb.with(RecordAttribute.of(RecordComponentInfo.of("x", ConstantDescs.CD_int)));
            cb.withMethodBody("<init>", MethodTypeDesc.of(ConstantDescs.CD_void, ConstantDescs.CD_int),
                    ClassFile.ACC_PUBLIC, code -> code.aload(0)
                            .invokespecial(ClassDesc.of("java.lang.Record"), "<init>",
                                    MethodTypeDesc.of(ConstantDescs.CD_void))
                            .aload(0).iload(1).putfield(rec, "x", ConstantDescs.CD_int).return_());
            cb.withMethodBody("x", MethodTypeDesc.of(ConstantDescs.CD_int), ClassFile.ACC_PUBLIC,
                    code -> code.aload(0).getfield(rec, "x", ConstantDescs.CD_int).ireturn());
            cb.withMethodBody("toString", MethodTypeDesc.of(ConstantDescs.CD_String), ClassFile.ACC_PUBLIC,
                    code -> code.aload(0).invokedynamic(toString).areturn());
            cb.withMethodBody("equals", MethodTypeDesc.of(ConstantDescs.CD_boolean, ConstantDescs.CD_Object),
                    ClassFile.ACC_PUBLIC, code -> code.aload(0).aload(1).invokedynamic(equals).ireturn());
            cb.withMethodBody("hashCode", MethodTypeDesc.of(ConstantDescs.CD_int), ClassFile.ACC_PUBLIC,
                    code -> code.aload(0).invokedynamic(hashCode).ireturn());
        });
    }

    static byte[] subclass(ClassDesc sub, ClassDesc rec) {
        return ClassFile.of().build(sub, cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withSuperclass(rec);
            cb.withMethodBody("<init>", MethodTypeDesc.of(ConstantDescs.CD_void, ConstantDescs.CD_int),
                    ClassFile.ACC_PUBLIC, code -> code.aload(0).iload(1)
                            .invokespecial(rec, "<init>", MethodTypeDesc.of(ConstantDescs.CD_void, ConstantDescs.CD_int))
                            .return_());
        });
    }

    interface Row {
        Object run() throws Throwable;
    }

    static void row(String name, Row r) {
        String out;
        try {
            out = String.valueOf(r.run());
        } catch (Throwable t) {
            out = t.getClass().getName() + ": " + t.getMessage();
        }
        System.out.println(name + ": " + out);
    }

    public static void main(String[] args) throws Throwable {
        ClassDesc rec = ClassDesc.of("L4W40OpenRec");
        ClassDesc sub = ClassDesc.of("L4W40OpenRecSub");
        Loader loader = new Loader();
        Class<?> recClass = loader.define("L4W40OpenRec", record(rec));
        Class<?> subClass = loader.define("L4W40OpenRecSub", subclass(sub, rec));
        Object r1 = recClass.getConstructor(int.class).newInstance(1);
        Object r1b = recClass.getConstructor(int.class).newInstance(1);
        Object s1 = subClass.getConstructor(int.class).newInstance(1);
        row("rec-tostring", () -> r1.toString());
        row("sub-tostring", () -> s1.toString());
        row("rec-equals-rec", () -> r1.equals(r1b));
        row("sub-equals-rec", () -> s1.equals(r1));
        row("rec-equals-sub", () -> r1.equals(s1));
        row("sub-equals-other", () -> s1.equals("x"));
        row("rec-hashcode", () -> r1.hashCode());
        row("sub-hashcode", () -> s1.hashCode());
        row("is-record", () -> recClass.isRecord() + " " + subClass.isRecord());
    }
}
