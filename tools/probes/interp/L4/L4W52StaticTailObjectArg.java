// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 52, lane L4: by-name STATIC calls whose first
// argument is an object of a concrete subtype of the method's owner (an
// interface or an abstract class) -- the shape the "never retarget a static"
// walk of `invoke_on_class_shared_inner` guarded, now answered by the static
// tail's own kind (`CRATONVM_BYNAME_STATIC_KIND`, stage 4 of
// `i14-L5-proposal-by-name-routes-carry-the-invoke-kind`). Every row must
// run the owner's static, never the argument's same-named method. The first
// row is repeated last. Positive control: `CRATONVM_DBG_DISPATCH_TALLY=1`
// lists `invoke_on_class_shared_inner.static-kind` rows.
//
// Run: javac -d out L4W52StaticTailObjectArg.java
//      cratonvm --java-home <jdk25> [--nojit | --compatible] -cp out L4W52StaticTailObjectArg
//
// Expected HotSpot 25 output (25.0.3, default flags):
//   iface-static-mh: Iface.check(Impl)
//   iface-static-ref: Iface.check(Impl)
//   abstract-static-mh: Abs.check(Conc)
//   abstract-static-ref: Abs.check(Conc)
//   inherited-static-mh: Abs.check(Conc)
//   hot-iface-static-mh: 20000
//   iface-static-mh-again: Iface.check(Impl)
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.function.Function;

public class L4W52StaticTailObjectArg {
    public interface Iface {
        static String check(Iface i) {
            return "Iface.check(" + i.name() + ")";
        }

        String name();
    }

    public static class Impl implements Iface {
        public String name() {
            return "Impl";
        }

        // Same name and shape as the interface's static, as an instance method.
        public String check(Iface i) {
            return "Impl.check(wrong)";
        }
    }

    public abstract static class Abs {
        public static String check(Abs a) {
            return "Abs.check(" + a.name() + ")";
        }

        abstract String name();
    }

    public static class Conc extends Abs {
        String name() {
            return "Conc";
        }
    }

    public static class Sub extends Conc {
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup l = MethodHandles.lookup();
        MethodHandle ifaceMh = l.findStatic(Iface.class, "check", MethodType.methodType(String.class, Iface.class));
        MethodHandle absMh = l.findStatic(Abs.class, "check", MethodType.methodType(String.class, Abs.class));
        MethodHandle inheritedMh = l.findStatic(Sub.class, "check", MethodType.methodType(String.class, Abs.class));
        Function<Iface, String> ifaceRef = Iface::check;
        Function<Abs, String> absRef = Abs::check;
        Impl impl = new Impl();
        Conc conc = new Conc();

        System.out.println("iface-static-mh: " + (String) ifaceMh.invokeExact((Iface) impl));
        System.out.println("iface-static-ref: " + ifaceRef.apply(impl));
        System.out.println("abstract-static-mh: " + (String) absMh.invokeExact((Abs) conc));
        System.out.println("abstract-static-ref: " + absRef.apply(conc));
        System.out.println("inherited-static-mh: " + (String) inheritedMh.invokeExact((Abs) conc));
        int ok = 0;
        for (int i = 0; i < 20000; i++) {
            if (((String) ifaceMh.invokeExact((Iface) impl)).startsWith("Iface.")) ok++;
        }
        System.out.println("hot-iface-static-mh: " + ok);
        System.out.println("iface-static-mh-again: " + (String) ifaceMh.invokeExact((Iface) impl));
    }
}
