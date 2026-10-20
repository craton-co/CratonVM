// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i5, wave 108, lane L3 (page
// `gap-hidden-walks-lack-hotspots-spun-lambda-form-frames-20261016.md`): a
// method handle of a STATICALLY BOUND instance method -- a method of a final
// class, a final method, a private method -- is HotSpot's `direct_call`, which
// `MethodHandleNatives.resolve` reports as `REF_invokeSpecial`
// (`init_method_MemberName`): `DirectMethodHandle.preparedLambdaForm` then
// links the handle to the `invokeSpecial` form, whatever the lookup was
// (`findVirtual`, `unreflect`, `Method.invoke`'s accessor). CratonVM's resolve
// kept `REF_invokeVirtual`, so a `SHOW_HIDDEN_FRAMES` walk listed the spun
// `LambdaForm$DMH::invokeVirtual` (L3W104 row `handle-to-Method.invoke
// hidden`). `revealDirect` maps the kind back (`invokeVirtual`), as HotSpot.
//
// Each `below` row prints the frame right below the target in a
// SHOW_HIDDEN_FRAMES walk, its class folded to `DMH` when it is the direct
// handle's form (`DirectMethodHandle$Holder` or a spun `LambdaForm$DMH`), so
// the row does not depend on which shapes the JDK image pregenerates.
//
// Run (both modes, JIT and --nojit; no setup):
//   javac -d out L3W108FinalVirtualHandleKind.java
//   cratonvm --jdk-only   -cp out L3W108FinalVirtualHandleKind
//   cratonvm --compatible -cp out L3W108FinalVirtualHandleKind
//   java                  -cp out L3W108FinalVirtualHandleKind
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3; derived from HotSpot's LinkResolver /
// init_method_MemberName and the JDK 25 DirectMethodHandle sources)
// final-class findVirtual below: DMH::invokeSpecial
// final-method findVirtual below: DMH::invokeSpecial
// private findVirtual below: DMH::invokeSpecial
// plain findVirtual below: DMH::invokeVirtual
// final-method unreflect below: DMH::invokeSpecial
// final-method Method.invoke below: DMH::invokeSpecial
// final-class reveal: invokeVirtual L3W108FinalVirtualHandleKind$Fin.m:()void
// final-method reveal: invokeVirtual L3W108FinalVirtualHandleKind$Open.fm:()void
// final-class null receiver: java.lang.NullPointerException
// final-class result: 7
// HOTSPOT_EXPECTED_END
import java.lang.StackWalker.Option;
import java.lang.StackWalker.StackFrame;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.List;
import java.util.stream.Collectors;

public class L3W108FinalVirtualHandleKind {
    static final StackWalker HIDDEN = StackWalker.getInstance(Option.SHOW_HIDDEN_FRAMES);
    static String below = "?";

    /** The frame right below the caller of this method (the target). */
    static void capture() {
        List<StackFrame> frames = HIDDEN.walk(s -> s.limit(4).collect(Collectors.toList()));
        // frames[0] = capture, frames[1] = the target, frames[2] = below it.
        StackFrame f = frames.size() > 2 ? frames.get(2) : null;
        below = f == null ? "none" : fold(f.getClassName()) + "::" + f.getMethodName();
    }

    static String fold(String c) {
        if (c.equals("java.lang.invoke.DirectMethodHandle$Holder")
                || c.startsWith("java.lang.invoke.LambdaForm$DMH")) {
            return "DMH";
        }
        return c.replaceAll("/0x[0-9a-f]+", "/ADDR");
    }

    static final class Fin {
        void m() { capture(); }
        int v() { return 7; }
    }

    static class Open {
        final void fm() { capture(); }
        Object plain() { capture(); return this; }
        private void priv() { capture(); }
    }

    interface Row { Object run() throws Throwable; }

    static void row(String name, Row r) {
        String out;
        try {
            out = String.valueOf(r.run());
        } catch (Throwable t) {
            out = t.getClass().getName();
        }
        System.out.println(name + ": " + out);
    }

    public static void main(String[] args) {
        MethodHandles.Lookup l = MethodHandles.lookup();
        MethodType v = MethodType.methodType(void.class);
        row("final-class findVirtual below", () -> {
            MethodHandle mh = l.findVirtual(Fin.class, "m", v);
            mh.invokeExact(new Fin());
            return below;
        });
        row("final-method findVirtual below", () -> {
            MethodHandle mh = l.findVirtual(Open.class, "fm", v);
            mh.invokeExact(new Open());
            return below;
        });
        row("private findVirtual below", () -> {
            MethodHandle mh = l.findVirtual(Open.class, "priv", v);
            mh.invokeExact(new Open());
            return below;
        });
        // An Object-returning target: `DirectMethodHandle$Holder.invokeVirtual`
        // is pregenerated for `(Object,Object)Object` in JDK 25.0.3's image, not
        // for `(Object,Object)void` (HotSpot spins a `LambdaForm$DMH` for a
        // void target, which this VM cannot name: see the gap page).
        row("plain findVirtual below", () -> {
            MethodHandle mh = l.findVirtual(Open.class, "plain",
                    MethodType.methodType(Object.class));
            Object r = (Object) mh.invokeExact(new Open());
            return below;
        });
        row("final-method unreflect below", () -> {
            MethodHandle mh = l.unreflect(Open.class.getDeclaredMethod("fm"));
            mh.invokeExact(new Open());
            return below;
        });
        row("final-method Method.invoke below", () -> {
            Open.class.getDeclaredMethod("fm").invoke(new Open());
            return below;
        });
        row("final-class reveal", () -> l.revealDirect(l.findVirtual(Fin.class, "m", v)));
        row("final-method reveal", () -> l.revealDirect(l.findVirtual(Open.class, "fm", v)));
        row("final-class null receiver", () -> {
            MethodHandle mh = l.findVirtual(Fin.class, "m", v);
            mh.invokeExact((Fin) null);
            return "no exception";
        });
        row("final-class result", () -> {
            MethodHandle mh = l.findVirtual(Fin.class, "v", MethodType.methodType(int.class));
            int r = (int) mh.invokeExact(new Fin());
            return r;
        });
    }
}
