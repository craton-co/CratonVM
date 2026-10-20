// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.classfile.ClassFile;
import java.lang.constant.ClassDesc;
import java.lang.constant.ConstantDescs;
import java.lang.constant.DirectMethodHandleDesc;
import java.lang.constant.DynamicCallSiteDesc;
import java.lang.constant.MethodTypeDesc;
import java.lang.invoke.CallSite;
import java.lang.invoke.ConstantCallSite;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.ref.WeakReference;

/**
 * gci i33/sc: the {@code invokedynamic} twin of {@link GciSc33CondyStringArgProbe}
 * (page {@code docs/known-issues/gc/gci-i33sc-indy-string-static-args-unrecorded-and-lossy-20261016.md}).
 * A String static argument of a user bootstrap method is passed EXACTLY (a lone
 * surrogate is not turned into U+FFFD), is the interned literal, is owned by its
 * class's constant pool while the class lives, and (the lone-surrogate one) is
 * collected with the class.
 *
 * <p>A class {@code GciSc33Indy} is generated with the ClassFile API and defined by
 * a throw-away loader. Its static methods {@code s()} / {@code w()} are each one
 * {@code invokedynamic} bootstrapped by {@link #ibsm} with one String static
 * argument: an ordinary one and one carrying a lone surrogate. The bootstrap keeps
 * the argument only WEAKLY and links a constant {@code Boolean.TRUE}. This class
 * never names either text as a literal (they are built from chars).
 *
 * <p>Deterministic stdout (HotSpot 25 prints exactly this, Serial and G1):
 * <pre>
 *   sc33i exact: str=true wide=true
 *   sc33i identity: str=true wide=true
 *   sc33i kept: str=true wide=true
 *   sc33i unloaded=true wide-cleared=true
 *   sc33i record: R[a=1, b=x]
 * </pre>
 * {@code exact}: the argument equals the text the class file holds. The last line
 * is a guard (records' {@code toString} links through {@code ObjectMethods} with
 * a String static argument). Stderr (not compared):
 * {@code [sc33i] rounds=R str-cleared=B}.
 */
public final class GciSc33IndyStringArgProbe {

    static final String CDY = "GciSc33Indy";

    record R(int a, String b) {
    }

    static WeakReference<String> strRef;
    static WeakReference<String> wideRef;
    static boolean strExact;
    static boolean wideExact;
    static boolean strCanonical;
    static boolean wideCanonical;
    static Class<?> keep;

    static String strText() {
        return new String(new char[] {'s', 'c', '3', '3', '-', 'i', 'n', 'd', 'y', '-', 'a'});
    }

    static String wideText() {
        return new String(new char[] {'\uD801', 's', 'c', '3', '3', '-', 'i'});
    }

    /** The indy bootstrap: records the argument weakly, links a constant. */
    public static CallSite ibsm(MethodHandles.Lookup lookup, String name, MethodType type, String arg) {
        if (name.equals("s")) {
            strExact = arg.equals(strText());
            strCanonical = arg == strText().intern();
            strRef = new WeakReference<>(arg);
        } else {
            wideExact = arg.equals(wideText());
            wideCanonical = arg == wideText().intern();
            wideRef = new WeakReference<>(arg);
        }
        return new ConstantCallSite(MethodHandles.constant(Object.class, Boolean.TRUE));
    }

    static final class OneLoader extends ClassLoader {
        OneLoader() {
            super(GciSc33IndyStringArgProbe.class.getClassLoader());
        }

        Class<?> define(String name, byte[] bytes) {
            return defineClass(name, bytes, 0, bytes.length);
        }
    }

    static byte[] build() {
        ClassDesc probe = ClassDesc.of(GciSc33IndyStringArgProbe.class.getName());
        DirectMethodHandleDesc bsm = ConstantDescs.ofCallsiteBootstrap(
                probe, "ibsm", ConstantDescs.CD_CallSite, ConstantDescs.CD_String);
        MethodTypeDesc get = MethodTypeDesc.of(ConstantDescs.CD_Object);
        DynamicCallSiteDesc s = DynamicCallSiteDesc.of(bsm, "s", get, strText());
        DynamicCallSiteDesc w = DynamicCallSiteDesc.of(bsm, "w", get, wideText());
        int flags = ClassFile.ACC_PUBLIC | ClassFile.ACC_STATIC;
        return ClassFile.of().build(ClassDesc.of(CDY), cb -> {
            cb.withFlags(ClassFile.ACC_PUBLIC | ClassFile.ACC_FINAL);
            cb.withMethodBody("s", get, flags, code -> code.invokedynamic(s).areturn());
            cb.withMethodBody("w", get, flags, code -> code.invokedynamic(w).areturn());
        });
    }

    /** Defines the class in a fresh loader and links both sites; answers the loader weakly. */
    static WeakReference<ClassLoader> defineAndLink() throws Exception {
        OneLoader loader = new OneLoader();
        Class<?> c = loader.define(CDY, build());
        Object a = c.getMethod("s").invoke(null);
        Object b = c.getMethod("w").invoke(null);
        if (a != Boolean.TRUE || b != Boolean.TRUE) {
            throw new IllegalStateException("indy answered " + a + " / " + b);
        }
        keep = c;
        return new WeakReference<>(loader);
    }

    public static void main(String[] args) throws Exception {
        WeakReference<ClassLoader> loader = defineAndLink();
        System.out.println("sc33i exact: str=" + strExact + " wide=" + wideExact);
        System.out.println("sc33i identity: str=" + strCanonical + " wide=" + wideCanonical);
        for (int r = 0; r < 3; r++) {
            System.gc();
        }
        System.out.println("sc33i kept: str=" + !strRef.refersTo(null)
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
        System.out.println("sc33i unloaded=" + unloaded + " wide-cleared=" + wideCleared);
        System.err.println("[sc33i] rounds=" + rounds + " str-cleared=" + strRef.refersTo(null));
        System.out.println("sc33i record: " + new R(1, "x"));
    }
}
