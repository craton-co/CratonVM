// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 43, lane L4: `MethodHandle.copyWith` is
// JDK-abstract, and CratonVM's synthetic handles implement none of the
// LambdaForm subclasses that override it, so every JDK route ending in
// `MethodHandle.viewAsType` threw `AbstractMethodError: ... copyWith ... has no
// Code attribute`. `StringConcatFactory.makeConcatWithConstants` / `makeConcat`
// end in `viewAsType` whenever they are CALLED (the VM links javac's concat
// `invokedynamic` itself), and `ObjectMethods.makeToString` calls them, so a
// direct `ObjectMethods.bootstrap(.., "toString", ..)` failed the same way
// (`L4W42ReflectiveSwitchBootstraps` rows `objectMethods-*`).
//
// Run: javac -d out L4W43CopyWithViews.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible] -cp out L4W43CopyWithViews
//
// Expected HotSpot 25 output (default and -Xint, measured locally), the same
// in --compatible:
//   concat-consts: <7|x>
//   concat-plain: 7x
//   concat-one: [42]
//   concat-long: 1-2.5-true-c-null
//   concat-type: (int,String)String
//   concat-direct-twice: 1a 2b
//   om-tostring: R[a=1, b=x]
//   om-tostring-callsite: R[a=2, b=null]
//   om-tostring-empty: E[]
//   om-hashcode: true
//   om-equals: true false
//   om-tostring-type: (R)String
//
// Before wave 43, CratonVM printed
// `java.lang.AbstractMethodError: ...copyWith...` (possibly wrapped) on every
// `concat-*` row and on `om-tostring`, `om-tostring-callsite` and
// `om-tostring-type`. `om-tostring-empty` (no concatenation: `constant` +
// `dropArguments`), `om-hashcode` and `om-equals` are
// controls: the JDK's `makeHashCode` / `makeEquals` do not concatenate, and
// they matched on the host in wave 40 (`L4W40BootstrapTypedVarargs`).
import java.lang.invoke.CallSite;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.invoke.StringConcatFactory;
import java.lang.runtime.ObjectMethods;

public class L4W43CopyWithViews {
    record R(int a, String b) {
    }

    record E() {
    }

    interface Row {
        Object run() throws Throwable;
    }

    static void row(String name, Row r) {
        String out;
        try {
            out = String.valueOf(r.run());
        } catch (Throwable t) {
            StringBuilder sb = new StringBuilder();
            for (Throwable c = t; c != null; c = c.getCause()) {
                if (sb.length() > 0) {
                    sb.append(" <- ");
                }
                sb.append(c.getClass().getName()).append(": ").append(c.getMessage());
            }
            out = sb.toString();
        }
        System.out.println(name + ": " + out);
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup lookup = MethodHandles.lookup();
        MethodType is = MethodType.methodType(String.class, int.class, String.class);
        row("concat-consts", () -> {
            CallSite cs = StringConcatFactory.makeConcatWithConstants(lookup, "x", is, "<\1|\1>");
            return (String) cs.getTarget().invokeExact(7, "x");
        });
        row("concat-plain", () -> {
            CallSite cs = StringConcatFactory.makeConcat(lookup, "x", is);
            return (String) cs.getTarget().invokeExact(7, "x");
        });
        row("concat-one", () -> {
            CallSite cs = StringConcatFactory.makeConcatWithConstants(lookup, "x",
                    MethodType.methodType(String.class, long.class), "[\1]");
            return (String) cs.getTarget().invokeExact(42L);
        });
        row("concat-long", () -> {
            CallSite cs = StringConcatFactory.makeConcatWithConstants(lookup, "x",
                    MethodType.methodType(String.class, int.class, double.class, boolean.class, char.class,
                            Object.class),
                    "\1-\1-\1-\1-\1");
            return (String) cs.getTarget().invokeExact(1, 2.5, true, 'c', (Object) null);
        });
        row("concat-type", () -> {
            CallSite cs = StringConcatFactory.makeConcatWithConstants(lookup, "x", is, "\1\1");
            return cs.getTarget().type().toString() + cs.type().toString().substring(0, 0);
        });
        row("concat-direct-twice", () -> {
            CallSite cs = StringConcatFactory.makeConcatWithConstants(lookup, "x", is, "\2\1\1", "");
            MethodHandle h = cs.getTarget();
            return (String) h.invokeExact(1, "a") + " " + (String) h.invokeExact(2, "b");
        });
        MethodHandle ga = lookup.findGetter(R.class, "a", int.class);
        MethodHandle gb = lookup.findGetter(R.class, "b", String.class);
        row("om-tostring", () -> {
            MethodHandle ts = (MethodHandle) ObjectMethods.bootstrap(lookup, "toString", MethodHandle.class,
                    R.class, "a;b", ga, gb);
            return (String) ts.invokeExact(new R(1, "x"));
        });
        row("om-tostring-callsite", () -> {
            CallSite cs = (CallSite) ObjectMethods.bootstrap(lookup, "toString",
                    MethodType.methodType(String.class, R.class), R.class, "a;b", ga, gb);
            return (String) cs.getTarget().invokeExact(new R(2, null));
        });
        row("om-tostring-empty", () -> {
            MethodHandle ts = (MethodHandle) ObjectMethods.bootstrap(lookup, "toString", MethodHandle.class,
                    E.class, "");
            return (String) ts.invokeExact(new E());
        });
        row("om-hashcode", () -> {
            MethodHandle hc = (MethodHandle) ObjectMethods.bootstrap(lookup, "hashCode", MethodHandle.class,
                    R.class, "a;b", ga, gb);
            R r = new R(3, "y");
            return (int) hc.invokeExact(r) == r.hashCode();
        });
        row("om-equals", () -> {
            MethodHandle eq = (MethodHandle) ObjectMethods.bootstrap(lookup, "equals", MethodHandle.class,
                    R.class, "a;b", ga, gb);
            return (boolean) eq.invokeExact(new R(1, "x"), (Object) new R(1, "x")) + " "
                    + (boolean) eq.invokeExact(new R(1, "x"), (Object) new R(1, "y"));
        });
        row("om-tostring-type", () -> {
            MethodHandle ts = (MethodHandle) ObjectMethods.bootstrap(lookup, "toString", MethodHandle.class,
                    R.class, "a;b", ga, gb);
            return ts.type().toString().replace("L4W43CopyWithViews$", "");
        });
    }
}
