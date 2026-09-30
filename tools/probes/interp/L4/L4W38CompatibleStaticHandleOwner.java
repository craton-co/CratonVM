// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 38, lane L4: a STATIC member handle of a class a
// user-defined loader defined, once a second user-defined loader has defined
// a class of the same binary name.
//
// Before wave 38, `--compatible` recorded no owner on such a handle
// (`lang_invoke.rs` `mh_static_owner_of_mirror` answered `None` outside
// `--jdk-only`), so every call resolved the handle's class BY NAME. Two
// user-defined loaders defining `Tw` make that name ambiguous
// (`ClassManager::get_loaded_class_id` refuses to guess), and the load's
// `ClassNotFound` came back from the native as an internal error: the VM
// printed `main-vm run() returned Err: Error in thread "main" class file
// error: class not found: Tw` after the `a-first` row and exited (the same
// crash as `tools/probes/interp/L2/L2W37InterfaceSelectionHot.java`'s second
// row under `--compatible`). `--jdk-only` (the default) already matched.
//
//   a-first         findStatic(A's Tw, "id") before B exists
//   b-static        findStatic(B's Tw, "id")
//   a-again         A's handle from the first row, after B exists
//   b-getter        findStaticGetter(B's Tw, "V", String)
//   b-setter        findStaticSetter(B's Tw, "V", String), then B's getter
//   a-unaffected    A's getter after B's setter ran
//   b-varhandle     findStaticVarHandle(B's Tw, "V", String).get()
//   b-unreflect     unreflect(B's Tw.getMethod("id")).invoke()
//   b-hot           B's `id` handle 20 000 times (a compiled caller)
//
// Run: javac -d out L4W38CompatibleStaticHandleOwner.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible] -cp out L4W38CompatibleStaticHandleOwner
//
// Expected HotSpot 25 output (default and -Xint); CratonVM prints the same in
// every mode, `--compatible` included:
//   a-first: A
//   b-static: B
//   a-again: A
//   b-getter: B
//   b-setter: B2
//   a-unaffected: A
//   b-varhandle: B2
//   b-unreflect: B
//   b-hot: 20000
import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.invoke.VarHandle;

public class L4W38CompatibleStaticHandleOwner {
    static final class Loader extends ClassLoader {
        final byte[] tw;

        Loader(byte[] tw) {
            super(L4W38CompatibleStaticHandleOwner.class.getClassLoader());
            this.tw = tw;
        }

        @Override
        protected Class<?> findClass(String name) throws ClassNotFoundException {
            if (!name.equals("Tw")) {
                throw new ClassNotFoundException(name);
            }
            return defineClass(name, tw, 0, tw.length);
        }
    }

    // public class Tw { public static String V = "<tag>"; public static String id() { return "<tag>"; } }
    static byte[] tw(String tag) {
        ClassDesc self = ClassDesc.of("Tw");
        return ClassFile.of().build(self, cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_SUPER);
            cb.withField("V", ConstantDescs.CD_String, ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC);
            cb.withMethodBody(ConstantDescs.CLASS_INIT_NAME, ConstantDescs.MTD_void, ClassFile.ACC_STATIC,
                    code -> code.ldc(tag).putstatic(self, "V", ConstantDescs.CD_String).return_());
            cb.withMethodBody("id", MethodTypeDesc.of(ConstantDescs.CD_String),
                    ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC, code -> code.ldc(tag).areturn());
        });
    }

    static final MethodType ID = MethodType.methodType(String.class);

    static String hot(MethodHandle h) throws Throwable {
        int ok = 0;
        for (int i = 0; i < 20_000; i++) {
            if ("B".equals((String) h.invokeExact())) {
                ok++;
            }
        }
        return String.valueOf(ok);
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup lookup = MethodHandles.lookup();
        Class<?> a = new Loader(tw("A")).loadClass("Tw");
        MethodHandle aId = lookup.findStatic(a, "id", ID);
        System.out.println("a-first: " + (String) aId.invokeExact());

        Class<?> b = new Loader(tw("B")).loadClass("Tw");
        MethodHandle bId = lookup.findStatic(b, "id", ID);
        System.out.println("b-static: " + (String) bId.invokeExact());
        System.out.println("a-again: " + (String) aId.invokeExact());

        MethodHandle bGet = lookup.findStaticGetter(b, "V", String.class);
        System.out.println("b-getter: " + (String) bGet.invokeExact());
        MethodHandle bSet = lookup.findStaticSetter(b, "V", String.class);
        bSet.invokeExact("B2");
        System.out.println("b-setter: " + (String) bGet.invokeExact());
        MethodHandle aGet = lookup.findStaticGetter(a, "V", String.class);
        System.out.println("a-unaffected: " + (String) aGet.invokeExact());

        VarHandle bVh = lookup.findStaticVarHandle(b, "V", String.class);
        System.out.println("b-varhandle: " + (String) bVh.get());

        MethodHandle bRefl = lookup.unreflect(b.getMethod("id"));
        System.out.println("b-unreflect: " + (String) bRefl.invoke());

        System.out.println("b-hot: " + hot(bId));
    }
}
