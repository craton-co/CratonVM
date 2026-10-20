// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 93, lane L2 -- the cold `getstatic` of a JDK-named
// owner that a child-first loader defines itself, compiled against the JDK's
// field (`i38-L2-proposal-constant-pool-indexed-field-sites`, "Wave 92": "that
// site RESOLVES at compile time (to the wrong owner)").
//
// A child-first loader (parent: the platform loader) defines its OWN
// `javax.security.auth.callback.ConfirmationCallback` with `public static int
// OK = 707`; java.base's copy (`OK = 3`, a `static final int`) is loaded and
// initialised first. `l2h.User.applyAsInt(k)` is hot for k < 20000 and only
// then reads `ConfirmationCallback.OK` (a `getstatic` the bytecode names, not a
// javac-folded constant). JVMS 5.3 / 5.4.3.2: the field reference resolves
// through `l2h.User`'s defining loader, i.e. to the loader's own class -- 707,
// whatever tier runs the site.
//
// The defect: the compile doors' owner check
// (`jit_bridge.rs` `jit_field_owner_refusal`) judged a JDK-global owner name
// under `--jdk-only` only, while the interpreter asks the user loader for it
// in `--compatible` too (the `global-name-ask` row, on by default there since
// interpreter round i1 wave 49). Under `--compatible` the hot method compiled
// with the site bound to java.base's `OK`, and the cold rows read 3. Fixed
// (default ON; `CRATONVM_JIT_FIELD_OWNER_ASK_PREDICATE=0` reverts): the check
// asks the interpreter's predicate (`loader_global_name_ask_enabled`), so the
// site is refused (switch off: the method stays interpreted) or deferred
// (`CRATONVM_JIT_DEFERRED_FIELD_SITES=1`: resolved through the loader at its
// first execution).
//
// Run (stdout must be the block, byte for byte, in every arm):
//   $J/javac -d out tools/probes/interp/L2/L2W93JdkNameColdGetstatic.java
//   $B --java-home $J/.. -cp out L2W93JdkNameColdGetstatic
//   $B --compatible --java-home $J/.. -cp out L2W93JdkNameColdGetstatic
//   CRATONVM_JIT_DEFERRED_FIELD_SITES=1 $B [--compatible] --java-home $J/.. -cp out L2W93JdkNameColdGetstatic
//   $B --nojit ... (control)
//   $J/java -cp out L2W93JdkNameColdGetstatic
// Expected before the fix: `--compatible` printed `707=0 other=10000` (read 3)
// whenever `applyAsInt` compiled before k reached 20000; `--jdk-only` was
// already right (the method was refused). Revert check:
//   CRATONVM_JIT_FIELD_OWNER_ASK_PREDICATE=0 $B --compatible ... -> the old rows.
// Census (positive control that the check fired):
//   CRATONVM_DBG_JITC=1 $B --compatible ... 2>&1 | grep 'field site refused'
// prints `field site refused class_id=<User> cp=<n>: field-owner-deferred`.
//
// Derived from the JDK 25 sources (JVMS 5.3.2/5.4.3.2 loader-driven field
// resolution; `ClassLoader.defineClass` permits a non-`java.` package; the
// `ConfirmationCallback.OK` constant is 3), not measured on a HotSpot build at
// the time of writing.
//
// HOTSPOT_EXPECTED_BEGIN
// jdk OK 3
// cold getstatic: 707=10000 other=0 errors=0
// HOTSPOT_EXPECTED_END
import java.lang.classfile.ClassFile;
import java.lang.classfile.Label;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.MethodTypeDesc;
import java.util.HashMap;
import java.util.Map;
import java.util.function.IntUnaryOperator;

public class L2W93JdkNameColdGetstatic {
    static final String OWNER_NAME = "javax.security.auth.callback.ConfirmationCallback";
    static final ClassDesc OWNER = ClassDesc.of(OWNER_NAME);
    static final ClassDesc USER = ClassDesc.of("l2h.User");
    static final ClassDesc I = ConstantDescs.CD_int;
    static final MethodTypeDesc INT_INT = MethodTypeDesc.of(I, I);
    static final int PUBLIC = ClassFile.ACC_PUBLIC;
    static final int STATIC = ClassFile.ACC_STATIC;

    /// The loader's own `ConfirmationCallback`: `public static int OK`, set to
    /// 707 by its `<clinit>`.
    static byte[] owned() {
        return ClassFile.of().build(OWNER, clb -> clb
                .withFlags(PUBLIC)
                .withField("OK", I, PUBLIC | STATIC)
                .withMethodBody(ConstantDescs.CLASS_INIT_NAME, ConstantDescs.MTD_void, STATIC,
                        cb -> cb.sipush(707).putstatic(OWNER, "OK", I).return_()));
    }

    /// `applyAsInt(k)`: k < 20000 -> k & 1; otherwise `ConfirmationCallback.OK`.
    static byte[] user() {
        return ClassFile.of().build(USER, clb -> clb
                .withFlags(PUBLIC)
                .withInterfaceSymbols(ClassDesc.of("java.util.function.IntUnaryOperator"))
                .withMethodBody(ConstantDescs.INIT_NAME, ConstantDescs.MTD_void, PUBLIC,
                        cb -> cb.aload(0)
                                .invokespecial(ConstantDescs.CD_Object, ConstantDescs.INIT_NAME,
                                        ConstantDescs.MTD_void)
                                .return_())
                .withMethodBody("applyAsInt", INT_INT, PUBLIC, cb -> {
                    Label cold = cb.newLabel();
                    cb.iload(1).sipush(20_000).if_icmpge(cold);
                    cb.iload(1).iconst_1().iand().ireturn();
                    cb.labelBinding(cold);
                    cb.getstatic(OWNER, "OK", I).ireturn();
                }));
    }

    /// Child-first for its own names, parent = the platform loader.
    static final class Loader extends ClassLoader {
        final Map<String, byte[]> bytes = new HashMap<>();

        Loader() {
            super(ClassLoader.getPlatformClassLoader());
            bytes.put(OWNER_NAME, owned());
            bytes.put("l2h.User", user());
        }

        @Override
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            synchronized (getClassLoadingLock(name)) {
                Class<?> c = findLoadedClass(name);
                if (c != null) {
                    return c;
                }
                byte[] b = bytes.get(name);
                if (b == null) {
                    return super.loadClass(name, resolve);
                }
                return defineClass(name, b, 0, b.length);
            }
        }
    }

    public static void main(String[] args) throws Exception {
        // java.base's copy, loaded and initialised first (reflectively: a
        // javac-folded constant would not load it).
        Class<?> jdk = Class.forName(OWNER_NAME);
        System.out.println("jdk OK " + jdk.getField("OK").getInt(null));
        IntUnaryOperator op = (IntUnaryOperator) new Loader().loadClass("l2h.User")
                .getConstructor().newInstance();
        int hit = 0;
        int other = 0;
        int errors = 0;
        String first = "";
        for (int k = 0; k < 30_000; k++) {
            try {
                int r = op.applyAsInt(k);
                if (k >= 20_000) {
                    if (r == 707) {
                        hit++;
                    } else {
                        other++;
                    }
                }
            } catch (Throwable t) {
                if (errors++ == 0) {
                    first = " first=" + t;
                }
            }
        }
        System.out.println("cold getstatic: 707=" + hit + " other=" + other + " errors=" + errors + first);
    }
}
