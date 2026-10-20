// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i1 wave 27, lane L4: `asType`'s reference -> reference
// cast for a reference that reaches a reference parameter or receiver of a
// LEAF handle through an adapter (`insertArguments`, `dropArguments`,
// `asSpreader`), through `invokeWithArguments` of an adapter, and through an
// `asType`-retyped direct handle called with `invokeExact`. The reference
// half of wave 26's primitive fix. Page:
// docs/internal/fixed-bugs/interpreter-L4-an-adapter-handle-passes-an-uncast-reference-to-a-reference-parameter-or-receiver-FIXED-20260928.md
//
// Rows 1-12 must throw HotSpot's ClassCastException; rows 13-21 are the
// controls a false refusal would break (a subclass receiver, an interface
// parameter, a null, a lambda, a java.lang.reflect.Proxy, an array into
// `Object[]`, an explicit cast to an interface).
//
// Each row runs WARM times in `drive` (the call site is in the row's lambda,
// so the JIT compiles it); a row prints its first outcome and, if any later
// iteration answered differently, how many did.
//
// Run: cratonvm --java-home <jdk25> [--nojit] [--compatible] -cp <dir> L4W27AdapterLeafReferenceCasts
// (all four combinations must print exactly HotSpot's lines)
//
// HotSpot 25 (25.0.3, default and -Xint) prints, verbatim:
//   1 insert setter String receiver: java.lang.ClassCastException: Cannot cast java.lang.String to L4W27AdapterLeafReferenceCasts$Cell
//   1w insert setter String receiver withArgs: java.lang.ClassCastException: Cannot cast java.lang.String to L4W27AdapterLeafReferenceCasts$Cell
//   2 drop getter String receiver: java.lang.ClassCastException: Cannot cast java.lang.String to L4W27AdapterLeafReferenceCasts$Cell
//   2w drop getter String receiver withArgs: java.lang.ClassCastException: Cannot cast java.lang.String to L4W27AdapterLeafReferenceCasts$Cell
//   3 drop virtual Integer receiver: java.lang.ClassCastException: Cannot cast java.lang.Integer to java.lang.String
//   3w drop virtual Integer receiver withArgs: java.lang.ClassCastException: Cannot cast java.lang.Integer to java.lang.String
//   4 insert static Integer to String: java.lang.ClassCastException: Cannot cast java.lang.Integer to java.lang.String
//   4w insert static Integer to String withArgs: java.lang.ClassCastException: Cannot cast java.lang.Integer to java.lang.String
//   5 insert setter Integer value: java.lang.ClassCastException: Cannot cast java.lang.Integer to java.lang.String
//   5w insert setter Integer value withArgs: java.lang.ClassCastException: Cannot cast java.lang.Integer to java.lang.String
//   6 spreader Integer to String: java.lang.ClassCastException: Cannot cast java.lang.Integer to java.lang.String
//   7 asType exact Integer to String: java.lang.ClassCastException: Cannot cast java.lang.Integer to java.lang.String
//   8 drop constructor Integer to String: java.lang.ClassCastException: Cannot cast java.lang.Integer to java.lang.String
//   8w drop constructor Integer to String withArgs: java.lang.ClassCastException: Cannot cast java.lang.Integer to java.lang.String
//   9 drop special String receiver: java.lang.ClassCastException: Cannot cast java.lang.String to L4W27AdapterLeafReferenceCasts
//   9w drop special String receiver withArgs: java.lang.ClassCastException: Cannot cast java.lang.String to L4W27AdapterLeafReferenceCasts
//   9t findSpecial type: (L4W27AdapterLeafReferenceCasts)int (int,L4W27AdapterLeafReferenceCasts)int
//   9d special String receiver: java.lang.ClassCastException: Cannot cast java.lang.String to L4W27AdapterLeafReferenceCasts
//   9c special caller receiver: 42
//   10 drop Integer to String[]: java.lang.ClassCastException: Cannot cast java.lang.Integer to [Ljava.lang.String;
//   10w drop Integer to String[] withArgs: java.lang.ClassCastException: Cannot cast java.lang.Integer to [Ljava.lang.String;
//   11 drop String[] to int[]: java.lang.ClassCastException: Cannot cast [Ljava.lang.String; to [I
//   11w drop String[] to int[] withArgs: java.lang.ClassCastException: Cannot cast [Ljava.lang.String; to [I
//   12 explicitCast insert Integer to String: java.lang.ClassCastException: Cannot cast java.lang.Integer to java.lang.String
//   13 drop virtual subclass receiver: 8
//   13w drop virtual subclass receiver withArgs: 8
//   14 drop getter subclass receiver: 4
//   15 drop interface param StringBuilder: 3
//   15w drop interface param StringBuilder withArgs: 4
//   16 drop interface param null: -1
//   17 drop lambda to functional interface: 6
//   18 drop Proxy to interface: ran
//   19 drop String[] to Object[]: 2
//   20 insert setter subclass receiver String value: s1
//   21 explicitCast String to interface: 5
//   22 insert setter String value: s2
//   23 collector withArgs Integer element: java.lang.ClassCastException: Cannot cast java.lang.Integer to java.lang.String
//   23i collector invoke Integer element: java.lang.ClassCastException: Cannot cast java.lang.Integer to java.lang.String
//   23b collector withArgs Strings: 2
//   24 drop virtual null receiver: java.lang.NullPointerException: null
//   24w drop virtual null receiver withArgs: java.lang.NullPointerException: null
//   24d direct virtual null receiver: java.lang.NullPointerException: null
//   24x direct virtual null receiver exact: java.lang.NullPointerException: null
//   25 drop getter null receiver: java.lang.NullPointerException: null
//   25w drop getter null receiver withArgs: java.lang.NullPointerException: null
//   26 insert setter null receiver: java.lang.NullPointerException: null
//
// CratonVM before the fix (read from the code, not run; both modes): rows 1,
// 1w and 5/5w stored through the wrong layout (5 into a slot of the String;
// the Integer into the String field, so 5/5w printed "3"); 2/2w read a slot of
// the String; 3/3w ran `length` by name on the Integer (NoSuchMethodError);
// 4-8w passed the Integer to the String parameter (whose `length()` then
// failed by name); 9/9w ran `twice` on the String; 9t printed
// `(L4W27Base)int (int,L4W27Base)int` and 9d named `L4W27Base`; 10w wrapped
// the Integer into a one-element String[] and printed 1; 10/11/11w handed the
// callee a non-array or an Object-component array for `int[]`; 23/23i
// stored the Integer into the String[] (`countStrings` printed 2); 24/24w and
// 25/25w answered `null` (the leaf arms' null-receiver fallback), 26 dropped
// the write and printed `stored`, and 24d/24x carried the message
// "MethodHandle receiver is null for java.lang.String.length".
// Where each refusal is taken now: rows 1-5, 8-11 and 23i (`invoke` of an adapter)
// at the adapter's entry (`adapter_entry_args`, against its own `type`); row
// 9d at the direct `invoke` door (`invoke_reference_cast_refusal`, reworded);
// every `w` row, 6, 7 and 12 at the leaf arm (`adapt_leaf_args`, or the
// receiver check of the GETTER / SETTER / VIRTUAL / SPECIAL arm); row 23 in
// the COLLECT arm's element loop.
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.reflect.Proxy;
import java.util.function.IntUnaryOperator;

class L4W27Base {
    int twice() {
        return 2 * ((L4W27AdapterLeafReferenceCasts) this).k;
    }
}

public class L4W27AdapterLeafReferenceCasts extends L4W27Base {
    static final int WARM = 3000;

    int k = 21;

    interface Body {
        Object run() throws Throwable;
    }

    static class Cell {
        int i = 4;
        String s = "s0";

        Cell() {
        }

        Cell(String s) {
            this.s = s;
        }

        int doubled() {
            return 2 * i;
        }

        @Override
        public String toString() {
            return "Cell(" + s + ")";
        }
    }

    static class SubCell extends Cell {
    }

    static int takesString(int a, String s) {
        return a + s.length();
    }

    static int lenOrMinus(CharSequence cs) {
        return cs == null ? -1 : cs.length();
    }

    static int countStrings(String[] ss) {
        return ss.length;
    }

    static int countInts(int[] xs) {
        return xs.length;
    }

    static int countAll(Object[] xs) {
        return xs.length;
    }

    static int applyOp(IntUnaryOperator op) {
        return op.applyAsInt(5);
    }

    static String runIt(Runnable r) {
        r.run();
        return "ran";
    }

    static String outcome(Body b) {
        try {
            Object r = b.run();
            return String.valueOf(r);
        } catch (Throwable t) {
            return t.getClass().getName() + ": " + t.getMessage();
        }
    }

    static void drive(String label, Body b) {
        String first = outcome(b);
        int differing = 0;
        for (int i = 1; i < WARM; i++) {
            if (!first.equals(outcome(b))) {
                differing++;
            }
        }
        System.out.println(label + ": " + first + (differing == 0 ? "" : "  [" + differing + " later iterations differ]"));
    }

    public static void main(String[] args) throws Throwable {
        MethodHandles.Lookup l = MethodHandles.lookup();
        Class<?> me = L4W27AdapterLeafReferenceCasts.class;
        MethodType is = MethodType.methodType(int.class, int.class, String.class);
        MethodHandle ts = l.findStatic(me, "takesString", is);
        Cell cell = new Cell();

        // A field setter / getter applied to an object of another class.
        MethodHandle setI = MethodHandles.insertArguments(l.findSetter(Cell.class, "i", int.class), 1, 5);
        drive("1 insert setter String receiver", () -> {
            setI.invoke((Object) "str");
            return "stored";
        });
        drive("1w insert setter String receiver withArgs", () -> setI.invokeWithArguments("str"));
        MethodHandle getI = MethodHandles.dropArguments(l.findGetter(Cell.class, "i", int.class), 0, int.class);
        drive("2 drop getter String receiver", () -> (int) getI.invoke(1, (Object) "str"));
        drive("2w drop getter String receiver withArgs", () -> getI.invokeWithArguments(1, "str"));
        // A virtual method run on a receiver of another class.
        MethodHandle len = MethodHandles.dropArguments(
                l.findVirtual(String.class, "length", MethodType.methodType(int.class)), 0, int.class);
        drive("3 drop virtual Integer receiver", () -> (int) len.invoke(1, (Object) 3));
        drive("3w drop virtual Integer receiver withArgs", () -> len.invokeWithArguments(1, 3));
        // A reference parameter of a static method.
        MethodHandle tsIns = MethodHandles.insertArguments(ts, 0, 1);
        drive("4 insert static Integer to String", () -> (int) tsIns.invoke((Object) 3));
        drive("4w insert static Integer to String withArgs", () -> tsIns.invokeWithArguments(3));
        // A reference field given a value of another type.
        MethodHandle setS = MethodHandles.insertArguments(l.findSetter(Cell.class, "s", String.class), 0, cell);
        drive("5 insert setter Integer value", () -> {
            setS.invoke((Object) 3);
            return cell.s;
        });
        drive("5w insert setter Integer value withArgs", () -> {
            setS.invokeWithArguments(3);
            return cell.s;
        });
        // A spreader's element, an asType copy called exactly.
        MethodHandle spread = ts.asSpreader(Object[].class, 2);
        drive("6 spreader Integer to String", () -> (int) spread.invoke(new Object[] {1, 3}));
        MethodHandle retyped = ts.asType(MethodType.methodType(int.class, int.class, Object.class));
        drive("7 asType exact Integer to String", () -> (int) retyped.invokeExact(1, (Object) 3));
        // A constructor and a special (super) call behind an adapter.
        MethodHandle ctor = MethodHandles.dropArguments(
                l.findConstructor(Cell.class, MethodType.methodType(void.class, String.class)), 0, int.class);
        drive("8 drop constructor Integer to String", () -> String.valueOf(ctor.invoke(1, (Object) 3)));
        drive("8w drop constructor Integer to String withArgs", () -> String.valueOf(ctor.invokeWithArguments(1, 3)));
        MethodHandle spd = l.findSpecial(L4W27Base.class, "twice", MethodType.methodType(int.class), me);
        MethodHandle sp = MethodHandles.dropArguments(spd, 0, int.class);
        drive("9 drop special String receiver", () -> (int) sp.invoke(1, (Object) "x"));
        drive("9w drop special String receiver withArgs", () -> sp.invokeWithArguments(1, "x"));
        // The receiver type of a `findSpecial` handle is its specialCaller.
        drive("9t findSpecial type", () -> spd.type() + " " + sp.type());
        drive("9d special String receiver", () -> (int) spd.invoke((Object) "x"));
        drive("9c special caller receiver", () -> (int) spd.invoke((Object) new L4W27AdapterLeafReferenceCasts()));
        // Array parameters: a non-array, and a reference array into `int[]`.
        MethodHandle cs = MethodHandles.dropArguments(
                l.findStatic(me, "countStrings", MethodType.methodType(int.class, String[].class)), 0, int.class);
        drive("10 drop Integer to String[]", () -> (int) cs.invoke(1, (Object) 3));
        drive("10w drop Integer to String[] withArgs", () -> cs.invokeWithArguments(1, 3));
        MethodHandle ci = MethodHandles.dropArguments(
                l.findStatic(me, "countInts", MethodType.methodType(int.class, int[].class)), 0, int.class);
        drive("11 drop String[] to int[]", () -> (int) ci.invoke(1, (Object) new String[] {"a"}));
        drive("11w drop String[] to int[] withArgs", () -> ci.invokeWithArguments(1, new String[] {"a"}));
        // An explicit cast to a CLASS still casts.
        MethodHandle ecs = MethodHandles.explicitCastArguments(tsIns, MethodType.methodType(int.class, Object.class));
        drive("12 explicitCast insert Integer to String", () -> (int) ecs.invoke((Object) 3));

        // Controls: nothing below may throw.
        SubCell sub = new SubCell();
        MethodHandle dbl = MethodHandles.dropArguments(
                l.findVirtual(Cell.class, "doubled", MethodType.methodType(int.class)), 0, int.class);
        drive("13 drop virtual subclass receiver", () -> (int) dbl.invoke(1, (Object) sub));
        drive("13w drop virtual subclass receiver withArgs", () -> dbl.invokeWithArguments(1, sub));
        drive("14 drop getter subclass receiver", () -> (int) getI.invoke(1, (Object) sub));
        MethodHandle lom = MethodHandles.dropArguments(
                l.findStatic(me, "lenOrMinus", MethodType.methodType(int.class, CharSequence.class)), 0, int.class);
        drive("15 drop interface param StringBuilder", () -> (int) lom.invoke(1, (Object) new StringBuilder("abc")));
        drive("15w drop interface param StringBuilder withArgs", () -> lom.invokeWithArguments(1, new StringBuilder("abcd")));
        drive("16 drop interface param null", () -> (int) lom.invoke(1, (Object) null));
        IntUnaryOperator inc = x -> x + 1;
        MethodHandle ap = MethodHandles.dropArguments(
                l.findStatic(me, "applyOp", MethodType.methodType(int.class, IntUnaryOperator.class)), 0, int.class);
        drive("17 drop lambda to functional interface", () -> (int) ap.invoke(1, (Object) inc));
        Runnable proxy = (Runnable) Proxy.newProxyInstance(me.getClassLoader(), new Class<?>[] {Runnable.class},
                (p, m, a) -> null);
        MethodHandle ri = MethodHandles.dropArguments(
                l.findStatic(me, "runIt", MethodType.methodType(String.class, Runnable.class)), 0, int.class);
        drive("18 drop Proxy to interface", () -> (String) ri.invoke(1, (Object) proxy));
        MethodHandle ca = MethodHandles.dropArguments(
                l.findStatic(me, "countAll", MethodType.methodType(int.class, Object[].class)), 0, int.class);
        drive("19 drop String[] to Object[]", () -> (int) ca.invoke(1, (Object) new String[] {"a", "b"}));
        MethodHandle setSub = MethodHandles.insertArguments(l.findSetter(Cell.class, "s", String.class), 0, sub);
        drive("20 insert setter subclass receiver String value", () -> {
            setSub.invoke((Object) "s1");
            return sub.s;
        });
        MethodHandle elom = MethodHandles.explicitCastArguments(
                l.findStatic(me, "lenOrMinus", MethodType.methodType(int.class, CharSequence.class)),
                MethodType.methodType(int.class, Object.class));
        drive("21 explicitCast String to interface", () -> (int) elom.invoke((Object) "abcde"));
        drive("22 insert setter String value", () -> {
            setS.invoke((Object) "s2");
            return cell.s;
        });

        // A reference collector's element (the reference half of wave 26's
        // rows 21-21c), and its control.
        MethodHandle col = l.findStatic(me, "countStrings", MethodType.methodType(int.class, String[].class))
                .asCollector(String[].class, 2);
        drive("23 collector withArgs Integer element", () -> col.invokeWithArguments("a", 3));
        drive("23i collector invoke Integer element", () -> (int) col.invoke("a", (Object) 3));
        drive("23b collector withArgs Strings", () -> col.invokeWithArguments("a", "b"));

        // A null unbound receiver: HotSpot's message-less NPE
        // (`Objects.requireNonNull` in checkReceiver / checkBase), through an
        // adapter, `invokeWithArguments`, and the direct `invoke` door.
        MethodHandle nm = MethodHandles.dropArguments(
                l.findVirtual(Cell.class, "toString", MethodType.methodType(String.class)), 0, int.class);
        drive("24 drop virtual null receiver", () -> (String) nm.invoke(1, (Object) null));
        drive("24w drop virtual null receiver withArgs", () -> nm.invokeWithArguments(1, null));
        MethodHandle lenD = l.findVirtual(String.class, "length", MethodType.methodType(int.class));
        drive("24d direct virtual null receiver", () -> (int) lenD.invoke((Object) null));
        drive("24x direct virtual null receiver exact", () -> (int) lenD.invokeExact((String) null));
        drive("25 drop getter null receiver", () -> (int) getI.invoke(1, (Object) null));
        drive("25w drop getter null receiver withArgs", () -> getI.invokeWithArguments(1, null));
        drive("26 insert setter null receiver", () -> {
            setI.invoke((Object) null);
            return "stored";
        });
    }
}
