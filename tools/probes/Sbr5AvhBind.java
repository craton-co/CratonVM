// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.ArrayList;
import java.util.Arrays;
import java.util.List;

/**
 * SBR round 5, lane avh: HotSpot-diffed probe of MethodHandle.bindTo on real handles:
 * repeat binds of one receiver (the answer is filed after the first), the cast rule
 * (subclass ok, wrong class refused, null ok), bound results used and re-bound, identity,
 * and binds across garbage collections.
 */
public class Sbr5AvhBind {
    interface Row { Object run() throws Throwable; }

    static void row(String name, Row r) {
        String out;
        try {
            Object v = r.run();
            out = v instanceof Object[] a ? Arrays.deepToString(a) : String.valueOf(v);
            if (v != null) out += " (" + v.getClass().getName() + ")";
        } catch (Throwable t) {
            out = t.getClass().getName() + ": " + t.getMessage();
        }
        System.out.println(name + " -> " + out);
    }

    static String cat(String a, String b) { return a + b; }
    static String cat3(String a, Object b, int c) { return a + b + c; }
    static int len(CharSequence s) { return s.length(); }
    static int add(int a, int b) { return a + b; }
    static String varjoin(String sep, String... rest) { return String.join(sep, rest); }
    String inst(String x, int y) { return "i" + x + y; }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup L = MethodHandles.lookup();
        Class<?> C = Sbr5AvhBind.class;
        MethodHandle CAT = L.findStatic(C, "cat", MethodType.methodType(String.class, String.class, String.class));
        MethodHandle CAT3 = L.findStatic(C, "cat3", MethodType.methodType(String.class, String.class, Object.class, int.class));
        MethodHandle LEN = L.findStatic(C, "len", MethodType.methodType(int.class, CharSequence.class));
        MethodHandle ADD = L.findStatic(C, "add", MethodType.methodType(int.class, int.class, int.class));
        MethodHandle VJ = L.findStatic(C, "varjoin", MethodType.methodType(String.class, String.class, String[].class));
        MethodHandle INST = L.findVirtual(C, "inst", MethodType.methodType(String.class, String.class, int.class));

        row("cat.once", () -> CAT.bindTo("a").invoke("b"));
        row("cat.twice", () -> CAT.bindTo("c").invoke("d"));
        row("cat.thrice.same.class", () -> { String s = ""; for (int i = 0; i < 5; i++) s += CAT.bindTo("x" + i).invoke("y") + ","; return s; });
        row("cat.null", () -> CAT.bindTo(null).invoke("q"));
        row("cat.null.then.str", () -> CAT.bindTo(null).type() + " " + CAT.bindTo("z").invoke("r"));
        row("cat.wrong.class", () -> CAT.bindTo(42));
        row("cat.wrong.class.after", () -> { CAT.bindTo("ok"); return CAT.bindTo(new StringBuilder("sb")); });
        row("len.subclass.string", () -> (int) LEN.bindTo("hello").invoke());
        row("len.subclass.sb", () -> (int) LEN.bindTo(new StringBuilder("hello!")).invoke());
        row("len.sb.then.string", () -> { LEN.bindTo(new StringBuilder("a")); return (int) LEN.bindTo("abc").invoke(); });
        row("len.int.wrong", () -> LEN.bindTo(5));
        row("cat3.obj", () -> CAT3.bindTo("a").bindTo(1).invoke(7));
        row("cat3.obj.repeat", () -> { String s = ""; for (int i = 0; i < 4; i++) s += CAT3.bindTo("a").bindTo(new Object[]{1}.length + i).invoke(i); return s; });
        row("cat3.types", () -> CAT3.bindTo("a").type() + " " + CAT3.bindTo("a").bindTo("b").type());
        row("add.prim.lead", () -> ADD.bindTo(1));
        row("add.prim.lead.repeat", () -> { ADD.bindTo(1); return "x"; });
        row("vj.bind", () -> VJ.bindTo("-").invoke("a", "b", "c"));
        row("vj.bind.repeat", () -> VJ.bindTo(";").invoke("a", "b"));
        row("vj.bind.type", () -> VJ.bindTo("-").type() + " " + VJ.bindTo("-").isVarargsCollector());
        row("inst.bind.recv", () -> INST.bindTo(new Sbr5AvhBind()).invoke("p", 3));
        row("inst.bind.recv.repeat", () -> INST.bindTo(new Sbr5AvhBind()).invoke("q", 4));
        row("inst.bind.wrong", () -> INST.bindTo("not a receiver"));
        row("identity.distinct", () -> CAT.bindTo("k") != CAT.bindTo("k"));
        row("identity.fresh.each", () -> { MethodHandle a = CAT.bindTo("k"), b = CAT.bindTo("k"); return a.equals(b) + " " + (a.invoke("1").equals(b.invoke("1"))); });
        row("bound.independent", () -> { MethodHandle a = CAT.bindTo("A"), b = CAT.bindTo("B"); return a.invoke("1") + "" + b.invoke("2") + a.invoke("3"); });
        row("bound.of.bound", () -> CAT3.bindTo("x").bindTo("y").bindTo(5));
        row("asType.then.bind", () -> CAT.asType(MethodType.methodType(Object.class, String.class, Object.class)).bindTo("m").invoke((Object) "n"));
        row("insert.then.bind", () -> MethodHandles.insertArguments(CAT3, 1, "mid").bindTo("first").invoke(9));
        row("gc.between.binds", () -> {
            List<byte[]> junk = new ArrayList<>();
            StringBuilder sb = new StringBuilder();
            MethodHandle h = CAT.bindTo("seed");
            for (int round = 0; round < 6; round++) {
                for (int i = 0; i < 2000; i++) junk.add(new byte[2048]);
                junk.clear();
                System.gc();
                sb.append(CAT.bindTo("r" + round).invoke("-")).append(h.invoke("!")).append(';');
            }
            return sb.toString();
        });
        row("many.binds", () -> { long n = 0; for (int i = 0; i < 20000; i++) n += ((String) CAT.bindTo("p").invoke("q")).length(); return n; });
        row("many.binds.alt.receivers", () -> {
            long n = 0;
            MethodHandle[] hs = { CAT, CAT3, LEN, INST, VJ };
            for (int i = 0; i < 10000; i++) {
                MethodHandle h = hs[i % hs.length];
                Object x = h == INST ? new Sbr5AvhBind() : h == LEN ? (i % 2 == 0 ? "s" : new StringBuilder("t")) : "v";
                n += h.bindTo(x).type().parameterCount();
            }
            return n;
        });
    }
}
