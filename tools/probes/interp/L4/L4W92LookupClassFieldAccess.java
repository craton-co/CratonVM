// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 92, lane L4 (proposal
// i40-L4-proposal-lookup-finders-ask-the-lookup-class): a full-power Lookup
// (`MethodHandles.lookup()`, PRIVATE mode) is NOT a universal key. HotSpot's
// `Lookup.checkAccess` asks `VerifyAccess.isMemberAccessible` with the lookup
// class (fields), and member resolution asks the lookup class (methods).
// `--compatible` used to admit every member for a PRIVATE-mode Lookup
// (`lk_enforce_find_access`); fixed by `lk_enforce_lookup_class_field_access`
// and the package-private arm of `lk_enforce_private_nest_access`
// (native-builtins/src/lang_invoke.rs), CRATONVM_MH_COMPAT_FINDER_LOOKUP_CLASS_ACCESS
// (default on, =0 off). `--jdk-only` runs the JDK's own Lookup bytecode.
//
// L4W92Other is a separate TOP-LEVEL class of the same (unnamed) package: the
// same run-time package, but NOT a nestmate. The module hash of the `from`
// clause is masked to `@H`.
//
// Run: javac -d . L4W92LookupClassFieldAccess.java
//      java -cp . L4W92LookupClassFieldAccess
//      cratonvm --compatible -cp . L4W92LookupClassFieldAccess
//      cratonvm --jdk-only -cp . L4W92LookupClassFieldAccess
//      CRATONVM_MH_COMPAT_FINDER_LOOKUP_CLASS_ACCESS=0 cratonvm --compatible -cp . L4W92LookupClassFieldAccess
//          (base behaviour: rows priv-*, pkg-other, prot-other, pkg-method-other print "ok")
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, derived from the JDK 25 sources)
// priv-getter IllegalAccessException: member is private: L4W92Other.secret/java.lang.String/getField, from class L4W92LookupClassFieldAccess (unnamed module @H)
// priv-setter IllegalAccessException: member is private: L4W92Other.secret/java.lang.String/putField, from class L4W92LookupClassFieldAccess (unnamed module @H)
// priv-static-getter IllegalAccessException: member is private: L4W92Other.ps/int/getStatic, from class L4W92LookupClassFieldAccess (unnamed module @H)
// priv-static-setter IllegalAccessException: member is private: L4W92Other.ps/int/putStatic, from class L4W92LookupClassFieldAccess (unnamed module @H)
// priv-varhandle IllegalAccessException: member is private: L4W92Other.secret/java.lang.String/getField, from class L4W92LookupClassFieldAccess (unnamed module @H)
// priv-static-varhandle IllegalAccessException: member is private: L4W92Other.ps/int/getStatic, from class L4W92LookupClassFieldAccess (unnamed module @H)
// pkg-same ok
// prot-same-pkg ok
// self-private ok
// private-lookup-in ok
// pkg-other IllegalAccessException: member is private to package: java.util.ArrayList.elementData/[Ljava.lang.Object;/getField, from class L4W92LookupClassFieldAccess (unnamed module @H)
// prot-other IllegalAccessException: member is protected: java.util.AbstractList.modCount/int/getField, from class L4W92LookupClassFieldAccess (unnamed module @H)
// prot-subclass ok
// pkg-method-other IllegalAccessException: no such method: java.util.ArrayList.elementData(int)Object/invokeVirtual
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.AbstractList;
import java.util.ArrayList;

public class L4W92LookupClassFieldAccess {
    private int mine = 7;

    interface Row {
        Object run() throws Throwable;
    }

    static void row(String name, Row r) {
        String out;
        try {
            r.run();
            out = "ok";
        } catch (Throwable t) {
            String m = String.valueOf(t.getMessage()).replaceAll("@[0-9a-f]+", "@H");
            out = t.getClass().getSimpleName() + ": " + m;
        }
        System.out.println(name + " " + out);
    }

    public static void main(String[] args) {
        MethodHandles.Lookup lk = MethodHandles.lookup();
        row("priv-getter", () -> lk.findGetter(L4W92Other.class, "secret", String.class));
        row("priv-setter", () -> lk.findSetter(L4W92Other.class, "secret", String.class));
        row("priv-static-getter", () -> lk.findStaticGetter(L4W92Other.class, "ps", int.class));
        row("priv-static-setter", () -> lk.findStaticSetter(L4W92Other.class, "ps", int.class));
        row("priv-varhandle", () -> lk.findVarHandle(L4W92Other.class, "secret", String.class));
        row("priv-static-varhandle",
                () -> lk.findStaticVarHandle(L4W92Other.class, "ps", int.class));
        row("pkg-same", () -> lk.findGetter(L4W92Other.class, "pkg", String.class));
        row("prot-same-pkg", () -> lk.findGetter(L4W92Other.class, "prot", int.class));
        row("self-private",
                () -> lk.findGetter(L4W92LookupClassFieldAccess.class, "mine", int.class));
        row("private-lookup-in", () -> MethodHandles.privateLookupIn(L4W92Other.class, lk)
                .findGetter(L4W92Other.class, "secret", String.class));
        row("pkg-other", () -> lk.findGetter(ArrayList.class, "elementData", Object[].class));
        row("prot-other", () -> lk.findGetter(AbstractList.class, "modCount", int.class));
        row("prot-subclass", L4W92Sub::findModCount);
        row("pkg-method-other", () -> lk.findVirtual(ArrayList.class, "elementData",
                MethodType.methodType(Object.class, int.class)));
    }
}

class L4W92Other {
    private String secret = "s";
    private static int ps = 1;
    String pkg = "p";
    protected int prot = 2;
}

class L4W92Sub extends AbstractList<Object> {
    @Override
    public Object get(int index) {
        throw new IndexOutOfBoundsException();
    }

    @Override
    public int size() {
        return 0;
    }

    static Object findModCount() throws Throwable {
        return MethodHandles.lookup().findGetter(AbstractList.class, "modCount", int.class);
    }
}
