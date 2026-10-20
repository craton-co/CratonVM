// Interpreter round i1, wave 49, lane L5 -- JVMS §5.3: a loader the VM asked
// for a name, and that answered with another loader's class, is that class's
// initiating loader for good (HotSpot's dictionary entry); a later resolution
// of the name from another class of the same loader does NOT ask the loader
// again. The positive control of `CRATONVM_RECORD_REPLACES_MEMO`
// (`docs/internal/proposals/interpreter/i38-L5-proposal-the-initiating-record-replaces-the-memo-under-jdk-only-20261002.md`).
//
// Loader `c` (child) delegates every `l5w49m.N<i>` to loader `p`, which
// defines it (generated, empty). Two classes of `c`, `l5w49m.R1` and
// `l5w49m.R2`, each `ldc` every `N<i>` (4400 names, above the memo's
// 4096-row cap per loader). `c` counts the requests it gets for those names.
// HotSpot: 4400 after R1 (one per name), still 4400 after R2 (R2's
// resolutions find `c`'s dictionary entries). Rows `once-*` repeat the case on
// a small scale (no eviction) first and last.
//
// CratonVM before wave 49 (from the code): R2's resolutions read the capped,
// evicted memo (`ClassRealm::initiating_resolution_cache`), so every evicted
// name re-asked `c`: `second > 4400`. With `CRATONVM_RECORD_REPLACES_MEMO`
// (`--jdk-only`) they read the uncapped initiating record. `--compatible`
// keeps the memo (the switch is `--jdk-only` only), so `--compatible` still
// differs on the `evicted` row until its records replace its memo too.
//
// Run (no setup):
//   javac -d out L5W49InitiatingRecordEviction.java
//   cratonvm --java-home <jdk25> [--nojit] -cp out L5W49InitiatingRecordEviction
//
// Positive control: `CRATONVM_DBG=access` prints
// `[ACCESS-DBG] INITIATING-MEMO-ONLY ...` for no row of this probe (every
// memo row here has a record behind it), and the exit `LOADER census` line
// ends in `initiating-memo-only=0`.
//
// Expected HotSpot 25.0.3 output (`java`, default flags; compare verbatim):
//   once-1 first=3 second=3
//   evicted first=4400 second=4400
//   once-2 first=3 second=3

import java.lang.classfile.ClassFile;
import java.lang.classfile.CodeBuilder;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;

public class L5W49InitiatingRecordEviction {
    static final String PKG = "l5w49m.";

    static byte[] empty(String name) {
        return ClassFile.of().build(ClassDesc.of(name), cb -> cb
                .withFlags(ClassFile.ACC_PUBLIC)
                .withSuperclass(ConstantDescs.CD_Object));
    }

    static byte[] referrer(String name, int count) {
        return ClassFile.of().build(ClassDesc.of(name), cb -> cb
                .withFlags(ClassFile.ACC_PUBLIC)
                .withSuperclass(ConstantDescs.CD_Object)
                .withMethodBody("touch", MethodTypeDesc.of(ConstantDescs.CD_void),
                        ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC,
                        code -> body(code, count)));
    }

    static void body(CodeBuilder code, int count) {
        for (int i = 0; i < count; i++) {
            code.ldc(ClassDesc.of(PKG + "N" + i)).pop();
        }
        code.return_();
    }

    static final class Parent extends ClassLoader {
        Parent() {
            super("p", L5W49InitiatingRecordEviction.class.getClassLoader());
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
        final Parent parent;
        final int count;
        int asks;

        Child(Parent parent, int count) {
            super("c", L5W49InitiatingRecordEviction.class.getClassLoader());
            this.parent = parent;
            this.count = count;
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            synchronized (getClassLoadingLock(name)) {
                Class<?> c = findLoadedClass(name);
                if (c != null) {
                    return c;
                }
                if (name.startsWith(PKG + "N")) {
                    asks++;
                    return parent.loadClass(name);
                }
                if (name.equals(PKG + "R1") || name.equals(PKG + "R2")) {
                    byte[] b = referrer(name, count);
                    return defineClass(name, b, 0, b.length);
                }
                return super.loadClass(name, resolve);
            }
        }
    }

    static void row(String label, int count) throws Exception {
        Child c = new Child(new Parent(), count);
        c.loadClass(PKG + "R1").getMethod("touch").invoke(null);
        int first = c.asks;
        c.loadClass(PKG + "R2").getMethod("touch").invoke(null);
        System.out.println(label + " first=" + first + " second=" + c.asks);
    }

    public static void main(String[] args) throws Exception {
        row("once-1", 3);
        row("evicted", 4400);
        row("once-2", 3);
    }
}
