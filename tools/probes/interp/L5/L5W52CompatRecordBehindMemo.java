// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1, wave 52, lane L5 -- JVMS §5.3: a loader that answered
// a name with another loader's class is that class's initiating loader for
// good (HotSpot's dictionary entry), so a later resolution of the name from a
// class of that loader does NOT call its `loadClass` again. The positive
// control of the `record-behind-memo` HotSpot-alignment row (`--compatible`;
// the i38-L5 record-replaces-memo proposal's `--compatible` stage).
//
// Loader `c` counts EVERY `loadClass` call for an `l5w52r.N<i>` name (before
// its `findLoadedClass` check, so a re-ask the VM makes is seen even when the
// loader then answers from its record) and delegates it to `p`, which defines
// it. Rows:
//
//   forname-*  `Class.forName(N, false, c)` (one ask), then a class `R` of
//              `c` does `ldc N`. HotSpot: 1 (R's resolution finds c's
//              dictionary entry). `--compatible` without the row: 2 (the
//              forName record is in the record table, never in the capped
//              memo the resolution read, so the VM asked `c` again).
//   evicted    `R1` and then `R2` (both of `c`) `ldc` 4400 names, above the
//              memo's 4096-row cap per loader. HotSpot: 4400 after R1, 4400
//              after R2. Without the row the evicted memo rows re-asked `c`.
//
// Run (no setup):
//   javac -d out L5W52CompatRecordBehindMemo.java
//   cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp out L5W52CompatRecordBehindMemo
//
// Positive control (`--compatible`): `CRATONVM_COMPAT_ALIGN=-record-behind-memo`
// prints the row-off counts; with the row on, `CRATONVM_DBG=access` makes the
// exit line `[ACCESS-DBG] ALIGNMENT census: ...` count `record-behind-memo=<n>`
// with n > 0.
//
// Expected HotSpot 25.0.3 output (`java`, default flags; compare verbatim):
//   forname-1 asks=1
//   evicted first=4400 second=4400
//   forname-2 asks=1

import java.lang.classfile.ClassFile;
import java.lang.classfile.CodeBuilder;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;

public class L5W52CompatRecordBehindMemo {
    static final String PKG = "l5w52r.";

    static byte[] empty(String name) {
        return ClassFile.of().build(ClassDesc.of(name), cb -> cb
                .withFlags(ClassFile.ACC_PUBLIC)
                .withSuperclass(ConstantDescs.CD_Object));
    }

    static byte[] referrer(String name, String[] targets) {
        return ClassFile.of().build(ClassDesc.of(name), cb -> cb
                .withFlags(ClassFile.ACC_PUBLIC)
                .withSuperclass(ConstantDescs.CD_Object)
                .withMethodBody("touch", MethodTypeDesc.of(ConstantDescs.CD_void),
                        ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                        code -> body(code, targets)));
    }

    static void body(CodeBuilder code, String[] targets) {
        for (String t : targets) {
            code.ldc(ClassDesc.of(t)).pop();
        }
        code.return_();
    }

    static final class Parent extends ClassLoader {
        Parent() {
            super("p", L5W52CompatRecordBehindMemo.class.getClassLoader());
        }

        @Override
        protected Class<?> findClass(String name) throws ClassNotFoundException {
            if (!name.startsWith(PKG + "N")) {
                throw new ClassNotFoundException(name);
            }
            byte[] b = empty(name);
            return defineClass(name, b, 0, b.length);
        }
    }

    static final class Child extends ClassLoader {
        final Parent parent = new Parent();
        final java.util.Map<String, String[]> referrers = new java.util.HashMap<>();
        int asks;

        Child() {
            super("c", L5W52CompatRecordBehindMemo.class.getClassLoader());
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            if (name.startsWith(PKG + "N")) {
                asks++;
            }
            synchronized (getClassLoadingLock(name)) {
                Class<?> c = findLoadedClass(name);
                if (c != null) {
                    return c;
                }
                if (name.startsWith(PKG + "N")) {
                    return parent.loadClass(name);
                }
                String[] targets = referrers.get(name);
                if (targets != null) {
                    byte[] b = referrer(name, targets);
                    return defineClass(name, b, 0, b.length);
                }
                return super.loadClass(name, resolve);
            }
        }
    }

    static void forName(String label) throws Exception {
        Child c = new Child();
        String n = PKG + "N0";
        c.referrers.put(PKG + "R", new String[] {n});
        Class.forName(n, false, c);
        c.loadClass(PKG + "R").getMethod("touch").invoke(null);
        System.out.println(label + " asks=" + c.asks);
    }

    static void evicted() throws Exception {
        Child c = new Child();
        String[] names = new String[4400];
        for (int i = 0; i < names.length; i++) {
            names[i] = PKG + "N" + i;
        }
        c.referrers.put(PKG + "R1", names);
        c.referrers.put(PKG + "R2", names);
        c.loadClass(PKG + "R1").getMethod("touch").invoke(null);
        int first = c.asks;
        c.loadClass(PKG + "R2").getMethod("touch").invoke(null);
        System.out.println("evicted first=" + first + " second=" + c.asks);
    }

    public static void main(String[] args) throws Exception {
        forName("forname-1");
        evicted();
        forName("forname-2");
    }
}
