// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i2 wave 66, lane L4 -- a bound field handle links too
// (`CRATONVM_MH_FIELD_LINK_BOUND`, opt-in; proposal
// `i65-L4-proposal-bound-field-handles-link-too-20261029.md`) and the `bindTo`
// fast mint covers field handles (`CRATONVM_MH_BINDTO_FAST_MINT_FIELDS`,
// opt-in; proposal `i65-L4-proposal-bindto-fast-mint-covers-field-handles-20261029.md`).
//
// Every row is a behaviour both switches must leave alone: warmed bound
// getters / setters through `invokeExact` (the linked path), a write made
// behind a linked handle's back, a subclass and a field-SHADOWING subclass as
// the bound receiver (the base class's slot, never the shadow's), `long` /
// `double` / reference fields, a reference setter whose value check meets
// another class and `null`, a call site that is not the declared type, a
// `String` receiver refused by `bindTo` and by an `asType((Object)int)` copy
// cold and warm, and one source bound to many receivers (the fast mint's
// shape), plain and mixed with subclasses.
//
// HOTSPOT_EXPECTED_BEGIN (JDK 25.0.3, default flags)
// warm-exact: 3000
// after-write: 5
// setter-exact: 2999
// setter-then-get: 2999
// warm-sub-shadow: 4000
// shadow-own: 2
// shadow-base: 1
// warm-wide: 20000 1000.0
// ref-exact: str
// ref-wrong-site: java.lang.invoke.WrongMethodTypeException: handle's method type ()String but found ()Object
// cs-string: x
// cs-builder: sb
// cs-null: null
// bind-string: java.lang.ClassCastException: Cannot cast java.lang.String to L4W66BoundFieldLink$Base
// objtype-string-cold: java.lang.ClassCastException: Cannot cast java.lang.String to L4W66BoundFieldLink$Base
// warm-objtype: 5998000
// objtype-string-warm: java.lang.ClassCastException: Cannot cast java.lang.String to L4W66BoundFieldLink$Base
// bind-many: 100800
// bind-many-mixed: 3000
// bind-many-set: 4032
// bound-type: ()int
// rebind: java.lang.IllegalArgumentException: no leading reference parameter
// unreflect-bound: 1
// bound-iwa: 2999
// HOTSPOT_EXPECTED_END
//
// Switches: run `--compatible` and `--jdk-only`, each with the switches off
// (default), with `CRATONVM_MH_FIELD_LINK_BOUND=1`, with
// `CRATONVM_MH_BINDTO_FAST_MINT_FIELDS=1`, and with both plus
// `CRATONVM_MH_FIELD_SLOT_FROM_MINT=1`: no row may move. Positive control:
// `CRATONVM_DBG_MH_FIELD_LINK=1 --compatible` prints `[mh-field-link]
// --compatible instance link ...` lines for the bound handles only with
// `CRATONVM_MH_FIELD_LINK_BOUND=1` (and `CRATONVM_MH_FIELD_LINK_COMPATIBLE`,
// default on). The `objtype-string-*` rows sit next to the pre-existing
// `i65-orch-compatible-warmed-field-getter-accepts-a-string-receiver`
// defect (an UNBOUND warmed getter through `invoke`); a bound copy of an
// `asType` stamp carries a cast chain and is never linked.
//
// SETUP: none; `java|cratonvm [--compatible] [--nojit] -cp . L4W66BoundFieldLink`.
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;

public class L4W66BoundFieldLink {
    public static class Base {
        public int v = 1;
        public long w = 10L;
        public double d = 0.5;
        public String s = "str";
        public CharSequence cs = "init";
    }

    public static class Sub extends Base {
    }

    public static class Shadow extends Base {
        public int v = 2;
    }

    interface Row {
        Object run() throws Throwable;
    }

    static void row(String name, Row r) {
        String out;
        try {
            out = String.valueOf(r.run());
        } catch (Throwable t) {
            out = String.valueOf(t);
        }
        if (out.startsWith("MethodHandle(")) {
            out = "MethodHandle(...)";
        }
        System.out.println(name + ": " + out);
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup lk = MethodHandles.lookup();
        Base box = new Base();
        Sub sub = new Sub();
        Shadow sh = new Shadow();
        MethodHandle getV = lk.findGetter(Base.class, "v", int.class);
        MethodHandle setV = lk.findSetter(Base.class, "v", int.class);
        MethodHandle getW = lk.findGetter(Base.class, "w", long.class);
        MethodHandle getD = lk.findGetter(Base.class, "d", double.class);
        MethodHandle getS = lk.findGetter(Base.class, "s", String.class);
        MethodHandle setCs = lk.findSetter(Base.class, "cs", CharSequence.class);

        MethodHandle bv = getV.bindTo(box);
        long n = 0;
        for (int i = 0; i < 3000; i++) {
            n += (int) bv.invokeExact();
        }
        System.out.println("warm-exact: " + n);
        box.v = 5;
        row("after-write", () -> (int) bv.invokeExact());
        MethodHandle bsetV = setV.bindTo(box);
        for (int i = 0; i < 3000; i++) {
            bsetV.invokeExact(i);
        }
        row("setter-exact", () -> box.v);
        row("setter-then-get", () -> (int) bv.invokeExact());

        MethodHandle bsub = getV.bindTo(sub);
        MethodHandle bsh = getV.bindTo(sh);
        long m = 0;
        for (int i = 0; i < 2000; i++) {
            m += (int) bsub.invokeExact();
            m += (int) bsh.invokeExact();
        }
        System.out.println("warm-sub-shadow: " + m);
        row("shadow-own", () -> sh.v);
        row("shadow-base", () -> (int) bsh.invokeExact());

        MethodHandle bw = getW.bindTo(box);
        MethodHandle bd = getD.bindTo(box);
        long lw = 0;
        double dd = 0;
        for (int i = 0; i < 2000; i++) {
            lw += (long) bw.invokeExact();
            dd += (double) bd.invokeExact();
        }
        System.out.println("warm-wide: " + lw + " " + dd);

        MethodHandle bs = getS.bindTo(box);
        String last = null;
        for (int i = 0; i < 2000; i++) {
            last = (String) bs.invokeExact();
        }
        String lastS = last;
        row("ref-exact", () -> lastS);
        row("ref-wrong-site", () -> {
            Object got = (Object) bs.invokeExact();
            return got;
        });

        MethodHandle bcs = setCs.bindTo(box);
        for (int i = 0; i < 2000; i++) {
            bcs.invokeExact((CharSequence) "x");
        }
        row("cs-string", () -> box.cs);
        row("cs-builder", () -> {
            bcs.invokeExact((CharSequence) new StringBuilder("sb"));
            return box.cs;
        });
        row("cs-null", () -> {
            bcs.invokeExact((CharSequence) null);
            return box.cs;
        });

        row("bind-string", () -> getV.bindTo("x"));
        MethodHandle objV = getV.asType(MethodType.methodType(int.class, Object.class));
        row("objtype-string-cold", () -> (int) objV.bindTo("x").invoke());
        MethodHandle objBound = objV.bindTo(box);
        long o = 0;
        for (int i = 0; i < 2000; i++) {
            o += (int) objBound.invokeExact();
        }
        System.out.println("warm-objtype: " + o);
        row("objtype-string-warm", () -> (int) objV.bindTo("x").invokeExact());

        Base[] boxes = new Base[64];
        for (int j = 0; j < boxes.length; j++) {
            boxes[j] = new Base();
            boxes[j].v = j;
        }
        long many = 0;
        for (int r = 0; r < 50; r++) {
            for (int j = 0; j < boxes.length; j++) {
                many += (int) getV.bindTo(boxes[j]).invokeExact();
            }
        }
        System.out.println("bind-many: " + many);
        Base[] ring = {new Base(), new Sub(), new Shadow()};
        long mix = 0;
        for (int i = 0; i < 3000; i++) {
            mix += (int) getV.bindTo(ring[i % 3]).invokeExact();
        }
        System.out.println("bind-many-mixed: " + mix);
        for (int j = 0; j < boxes.length; j++) {
            setV.bindTo(boxes[j]).invokeExact(j * 2);
        }
        long set = 0;
        for (Base b : boxes) {
            set += b.v;
        }
        System.out.println("bind-many-set: " + set);

        row("bound-type", () -> bv.type());
        row("rebind", () -> bv.bindTo(box));
        MethodHandle ug = lk.unreflectGetter(Base.class.getField("v")).bindTo(sh);
        row("unreflect-bound", () -> (int) ug.invokeExact());
        row("bound-iwa", () -> bv.invokeWithArguments());
    }
}
