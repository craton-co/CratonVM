// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5 wave 100, lane L4: `Lookup.revealDirect` of a
// `VarHandle.toMethodHandle(mode)` handle.
//
// JDK 25 `VarHandle.toMethodHandle(mode)` answers
// `getMethodHandle(mode.ordinal()).bindTo(asDirect())` (or, for an
// unsupported mode, `varHandleInvoker(mode, type).bindTo(this)`): a BOUND
// handle in every case -- `getMethodHandleUncached` notes the mode handle
// "must not be publicly exposed directly otherwise it can be cracked". So
// `revealDirect` of any of them is `IllegalArgumentException: not a direct
// method handle`. CratonVM answered GET and SET of a field handle with a
// direct field getter / setter, which `revealDirect` cracked as
// `REF_getField` / `REF_putField` (switch `CRATONVM_VH_TO_METHOD_HANDLE_BOUND`,
// default on, `=0` restores the old answer). The handles still work (value
// rows) and keep their types.
//
// Which arms reach the native: in a real-JDK boot the `HotSpotAlignment::JdkInvoke`
// row (default ON in both modes; `native-api/src/retired_shadow.rs` family
// `java/lang/invoke/`, table `RETIRED_SHADOW_SBR4_MHCONS_TRIPLES`) refuses the
// `java.lang.invoke` registrations, `toMethodHandle` and `findVarHandle`
// included, so the JDK's own bytecode answers and the default arms are SAME
// whatever the switch says. The native (and the switch) is live only with that
// row off or over the synthetic JDK. The discriminating arm:
//   CRATONVM_COMPAT_ALIGN=-jdk-invoke cratonvm --compatible -cp . L4W100VarHandleToMethodHandleReveal
//     (expected SAME)
//   CRATONVM_COMPAT_ALIGN=-jdk-invoke CRATONVM_VH_TO_METHOD_HANDLE_BOUND=0 cratonvm --compatible ...
//     (expected DIFF on the six reveal-* rows: a MethodHandleInfo instead of the IAE)
//
// Run: javac L4W100VarHandleToMethodHandleReveal.java && java L4W100VarHandleToMethodHandleReveal
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25; derived from the sources)
// type-get (Box)int
// type-set (Box,int)void
// type-static-get ()int
// type-cas (Box,int,int)boolean
// reveal-get java.lang.IllegalArgumentException: not a direct method handle
// reveal-set java.lang.IllegalArgumentException: not a direct method handle
// reveal-static-get java.lang.IllegalArgumentException: not a direct method handle
// reveal-static-set java.lang.IllegalArgumentException: not a direct method handle
// reveal-final-set java.lang.IllegalArgumentException: not a direct method handle
// reveal-get-volatile java.lang.IllegalArgumentException: not a direct method handle
// reveal-findGetter control REF_getField
// value-get 7
// value-set 9
// value-static-get 5
// value-cas true 11
// final-set java.lang.UnsupportedOperationException
// HOTSPOT_EXPECTED_END
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandleInfo;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.VarHandle;

public class L4W100VarHandleToMethodHandleReveal {
    public static class Box {
        public static int S = 5;
        public int v = 7;
        public final int f = 3;
    }

    interface Row {
        Object run() throws Throwable;
    }

    static void row(String name, Row r) {
        String out;
        try {
            out = String.valueOf(r.run());
        } catch (Throwable t) {
            out = t.getClass().getName() + (t.getMessage() == null ? "" : ": " + t.getMessage());
        }
        System.out.println(name + " " + out);
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup l = MethodHandles.lookup();
        VarHandle v = l.findVarHandle(Box.class, "v", int.class);
        VarHandle s = l.findStaticVarHandle(Box.class, "S", int.class);
        VarHandle f = l.findVarHandle(Box.class, "f", int.class);
        MethodHandle get = v.toMethodHandle(VarHandle.AccessMode.GET);
        MethodHandle set = v.toMethodHandle(VarHandle.AccessMode.SET);
        MethodHandle sGet = s.toMethodHandle(VarHandle.AccessMode.GET);
        MethodHandle sSet = s.toMethodHandle(VarHandle.AccessMode.SET);
        MethodHandle fSet = f.toMethodHandle(VarHandle.AccessMode.SET);
        MethodHandle getVol = v.toMethodHandle(VarHandle.AccessMode.GET_VOLATILE);
        MethodHandle cas = v.toMethodHandle(VarHandle.AccessMode.COMPARE_AND_SET);

        row("type-get", () -> get.type());
        row("type-set", () -> set.type());
        row("type-static-get", () -> sGet.type());
        row("type-cas", () -> cas.type());
        row("reveal-get", () -> l.revealDirect(get));
        row("reveal-set", () -> l.revealDirect(set));
        row("reveal-static-get", () -> l.revealDirect(sGet));
        row("reveal-static-set", () -> l.revealDirect(sSet));
        row("reveal-final-set", () -> l.revealDirect(fSet));
        row("reveal-get-volatile", () -> l.revealDirect(getVol));
        row("reveal-findGetter control", () -> MethodHandleInfo.referenceKindToString(
                l.revealDirect(l.findGetter(Box.class, "v", int.class)).getReferenceKind()));

        Box box = new Box();
        row("value-get", () -> (int) get.invokeExact(box));
        row("value-set", () -> {
            set.invokeExact(box, 9);
            return box.v;
        });
        row("value-static-get", () -> (int) sGet.invokeExact());
        row("value-cas", () -> {
            boolean ok = (boolean) cas.invokeExact(box, 9, 11);
            return ok + " " + box.v;
        });
        row("final-set", () -> {
            fSet.invokeExact(box, 4);
            return "no-throw " + box.f;
        });
    }
}
