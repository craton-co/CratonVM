// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.DirectMethodHandleDesc;
import java.lang.constant.DynamicConstantDesc;
import java.lang.constant.MethodTypeDesc;
import java.lang.invoke.MethodHandles;
import java.lang.ref.WeakReference;

/**
 * gci i33/sc: a String used only as a condy ({@code CONSTANT_Dynamic}) static
 * argument is owned by its class's constant record, as HotSpot's resolved
 * references own it (`i32-sl-report.md` residual; page
 * {@code docs/known-issues/gc/gci-i20mr-merge-review-findings-20261015.md}).
 *
 * <p>A class {@code GciSc33Cdy} is generated with the ClassFile API and defined
 * by a throw-away loader. Its static methods {@code s()} / {@code w()} each
 * {@code ldc} a condy whose bootstrap is {@link #bsm} and whose one static
 * argument is a String constant: an ordinary one, and one carrying a lone
 * surrogate. The bootstrap keeps the argument only WEAKLY and returns
 * {@code Boolean.TRUE}, so nothing but the defining class's constant pool
 * entry (and the String table, weakly) holds it. This class never names either
 * text as a literal (they are built from chars), or its own constant pool
 * would keep them alive.
 *
 * <p>Deterministic stdout (HotSpot 25 prints exactly this, Serial and G1):
 * <pre>
 *   sc33 identity: str=true wide=true
 *   sc33 kept: str=true wide=true
 *   sc33 unloaded=true wide-cleared=true
 * </pre>
 * {@code identity}: the argument is the interned literal. {@code kept}: while
 * the class lives, System.gc() keeps both arguments. {@code unloaded}: the
 * defining loader is collected; {@code wide-cleared}: the lone-surrogate
 * argument is collected with it (within 10 System.gc() rounds). Stderr (not
 * compared): {@code [sc33] rounds=R str-cleared=B} (the ordinary argument
 * clears only where the String table is weak: HotSpot, our Generational
 * default).
 */
public final class GciSc33CondyStringArgProbe {

    static final String CDY = "GciSc33Cdy";

    static WeakReference<String> strRef;
    static WeakReference<String> wideRef;
    static boolean strCanonical;
    static boolean wideCanonical;
    static Class<?> keep;

    static String strText() {
        return new String(new char[] {'s', 'c', '3', '3', '-', 'c', 'o', 'n', 'd', 'y', '-', 'a'});
    }

    static String wideText() {
        return new String(new char[] {'\uD800', 's', 'c', '3', '3', '-', 'w'});
    }

    /** The condy bootstrap: records the argument weakly, answers a constant. */
    public static Object bsm(MethodHandles.Lookup lookup, String name, Class<?> type, String arg) {
        if (name.equals("s")) {
            strCanonical = arg == strText().intern();
            strRef = new WeakReference<>(arg);
        } else {
            wideCanonical = arg == wideText().intern();
            wideRef = new WeakReference<>(arg);
        }
        return Boolean.TRUE;
    }

    static final class OneLoader extends ClassLoader {
        OneLoader() {
            super(GciSc33CondyStringArgProbe.class.getClassLoader());
        }

        Class<?> define(String name, byte[] bytes) {
            return defineClass(name, bytes, 0, bytes.length);
        }
    }

    static byte[] build() {
        ClassDesc probe = ClassDesc.of(GciSc33CondyStringArgProbe.class.getName());
        DirectMethodHandleDesc bsm = ConstantDescs.ofConstantBootstrap(
                probe, "bsm", ConstantDescs.CD_Object, ConstantDescs.CD_String);
        DynamicConstantDesc<Object> s =
                DynamicConstantDesc.ofNamed(bsm, "s", ConstantDescs.CD_Object, strText());
        DynamicConstantDesc<Object> w =
                DynamicConstantDesc.ofNamed(bsm, "w", ConstantDescs.CD_Object, wideText());
        MethodTypeDesc get = MethodTypeDesc.of(ConstantDescs.CD_Object);
        int flags = ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC;
        return ClassFile.of().build(ClassDesc.of(CDY), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_FINAL);
            cb.withMethodBody("s", get, flags, code -> code.ldc(s).areturn());
            cb.withMethodBody("w", get, flags, code -> code.ldc(w).areturn());
        });
    }

    /** Defines the class in a fresh loader and resolves both condys; answers the loader weakly. */
    static WeakReference<ClassLoader> defineAndResolve() throws Exception {
        OneLoader loader = new OneLoader();
        Class<?> c = loader.define(CDY, build());
        Object a = c.getMethod("s").invoke(null);
        Object b = c.getMethod("w").invoke(null);
        if (a != Boolean.TRUE || b != Boolean.TRUE) {
            throw new IllegalStateException("condy answered " + a + " / " + b);
        }
        keep = c;
        return new WeakReference<>(loader);
    }

    public static void main(String[] args) throws Exception {
        WeakReference<ClassLoader> loader = defineAndResolve();
        System.out.println("sc33 identity: str=" + strCanonical + " wide=" + wideCanonical);
        for (int r = 0; r < 3; r++) {
            System.gc();
        }
        System.out.println("sc33 kept: str=" + !strRef.refersTo(null)
                + " wide=" + !wideRef.refersTo(null));

        keep = null;
        int rounds = 0;
        boolean unloaded = false;
        boolean wideCleared = false;
        for (int r = 0; r < 10; r++) {
            System.gc();
            rounds++;
            unloaded = loader.refersTo(null);
            wideCleared = wideRef.refersTo(null);
            if (unloaded && wideCleared && strRef.refersTo(null)) {
                break;
            }
            Thread.sleep(20);
        }
        System.out.println("sc33 unloaded=" + unloaded + " wide-cleared=" + wideCleared);
        System.err.println("[sc33] rounds=" + rounds + " str-cleared=" + strRef.refersTo(null));
    }
}
