// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i3 wave 72, lane L4 -- one default-off switch:
//   CRATONVM_MH_SITE_TYPE_TABLE=1 (proposal
//     `interpreter-i72-L4-proposal-jdk-only-field-handle-mint-and-converting-invoke-cost-DONE-20261014`):
//     `invoke` on a REAL JDK method handle (what `--jdk-only` finders, and
//     `bindTo` on their handles, answer) at a call site of ANOTHER type takes the site's
//     `MethodType` from the VM's bound-type table, keyed by the descriptor
//     and the caller's built-in loader, instead of running
//     `MethodType.fromMethodDescriptorString` (a `Class.forName` per
//     reference component) on every call. HotSpot resolves it once, when the
//     `invokehandle` site links.
//
// Every row runs its converting call TWICE (the first fills the row, the
// second is served by it) and prints both answers. The rows change what the
// site's type asks of `asType`: a boxed / widened / dropped return, a cast of
// a reference argument (accepted and refused), an impossible return
// (`WrongMethodTypeException`), a static and a bound handle, and the JDK's
// own `asType` cache, which compares the site type by IDENTITY
// (`asType-cache-shared`: the type the table serves must be the interned
// object `MethodType.methodType` answers, or the cache misses and `a == b`
// prints `false`).
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.4, default flags)
// get-object: 7 7
// get-integer: 7 7
// get-long: 7 7
// get-void: void void
// get-object-arg: 7 7
// get-object-arg-wrong: java.lang.ClassCastException: Cannot cast java.lang.String to L4W72SiteTypeTable$Box
// get-string-wmte: java.lang.invoke.WrongMethodTypeException: cannot convert MethodHandle(Box)int to (Box)String
// set-boxed: 11 12
// static-object: 40 40
// bound-object: 7 7
// bound-fresh-object: 7 7
// asType-cache-shared: true
// HOTSPOT_EXPECTED_END
//
// Switch sets, `--compatible` and `--jdk-only` (each with `--nojit` too):
//   (none)
//   CRATONVM_MH_SITE_TYPE_TABLE=1
// Every row must be SAME in both sets, in both modes.
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L4W72SiteTypeTable`.
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;

public class L4W72SiteTypeTable {
    public static class Box {
        public static long S = 40L;
        public int v = 7;
    }

    interface Row {
        Object run() throws Throwable;
    }

    static void row(String name, Row r) {
        String out;
        try {
            Object first = r.run();
            Object second = r.run();
            out = first + " " + second;
        } catch (Throwable t) {
            out = String.valueOf(t);
        }
        System.out.println(name + ": " + out);
    }

    static final MethodHandles.Lookup LK = MethodHandles.lookup();

    public static void main(String[] args) throws Throwable {
        Box box = new Box();
        MethodHandle get = LK.findGetter(Box.class, "v", int.class);
        MethodHandle set = LK.findSetter(Box.class, "v", int.class);
        MethodHandle sget = LK.findStaticGetter(Box.class, "S", long.class);

        row("get-object", () -> (Object) get.invoke(box));
        row("get-integer", () -> (Integer) get.invoke(box));
        row("get-long", () -> (long) get.invoke(box));
        row("get-void", () -> {
            get.invoke(box);
            return "void";
        });
        row("get-object-arg", () -> (Object) get.invoke((Object) box));
        row("get-object-arg-wrong", () -> (Object) get.invoke((Object) "x"));
        row("get-string-wmte", () -> (String) get.invoke(box));
        int[] next = {11};
        row("set-boxed", () -> {
            set.invoke(box, (Integer) next[0]++);
            return box.v;
        });
        box.v = 7;
        row("static-object", () -> (Object) sget.invoke());
        MethodHandle bound = get.bindTo(box);
        row("bound-object", () -> (Object) bound.invoke());
        row("bound-fresh-object", () -> (Object) get.bindTo(box).invoke());

        MethodType t = MethodType.methodType(Object.class, Box.class);
        Object r1 = (Object) get.invoke(box);
        MethodHandle a = get.asType(t);
        Object r2 = (Object) get.invoke(box);
        MethodHandle b = get.asType(t);
        System.out.println("asType-cache-shared: " + (a == b && r1.equals(r2)));
    }
}
