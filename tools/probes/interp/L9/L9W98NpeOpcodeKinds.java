// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
//
// Interpreter round i4 wave 98, lane L9 -- JEP 358 helpful NullPointerException
// messages across every null-dereferencing bytecode kind and the expression
// shapes HotSpot's ExceptionMessageBuilder describes, each taken twice:
//
//   cold   the first execution of the trapping method (interpreted)
//   warm   the same site after 12000 non-null executions (compiled where the
//          VM compiles), then one null; HotSpot deoptimizes and the
//          interpreter rebuilds the same text
//
// A row prints "<label>: <message>"; a warm message that differs from the cold
// one adds "<label> WARM: <message>" (HotSpot prints no WARM line at all).
// Also: messages HotSpot does NOT compute (an NPE constructed by code, a
// requireNonNull, a null thrown inside a hidden frame -- a lambda proxy of a
// method reference, a MethodHandle invokeExact), and the top frame of those.
//
// Compile WITHOUT -g (plain javac: no LocalVariableTable), so locals print as
// <localN> / <parameterN>:
//   $J/javac -d out tools/probes/interp/L9/L9W98NpeOpcodeKinds.java
//   $B [--compatible] [--nojit] --java-home $J/.. -cp out L9W98NpeOpcodeKinds
//   $J/java -cp out L9W98NpeOpcodeKinds
//
// Expected HotSpot 25 lines (a selection; the HotSpot run is the reference):
//   getfield: Cannot read field "val" because "<parameter1>" is null
//   putfield-long: Cannot assign field "lv" because "<parameter1>" is null
//   baload-boolean: Cannot load from byte/boolean array because "<parameter1>" is null
//   athrow-null: Cannot throw exception because "null" is null
//   static-chain: Cannot read field "val" because "L9W98NpeOpcodeKinds.SN.next" is null
//   return-value: Cannot read field "val" because the return value of "L9W98NpeOpcodeKinds.maybe(boolean)" is null
//   two-dim: Cannot load from int array because "<parameter1>[1]" is null
//   unbox-int: Cannot invoke "java.lang.Integer.intValue()" because "<parameter1>" is null
//   foreach-array: Cannot read the array length because "<local1>" is null
//   string-switch: Cannot invoke "String.hashCode()" because "<local1>" is null
//   ternary: Cannot invoke "String.length()"
//   params: Cannot invoke "L9W98NpeOpcodeKinds.take(int, String, Object[], long[][], java.lang.CharSequence, double)" because "<parameter1>" is null
//   ctor-plain: null
//   require-non-null: null
//   method-ref-hidden: null
//   handle-invoke-exact: null
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.util.ArrayList;
import java.util.List;
import java.util.Objects;
import java.util.function.Function;
import java.util.function.IntSupplier;

public class L9W98NpeOpcodeKinds {
    static final class Node {
        int val;
        long lv;
        Node next;
        String s;
    }

    enum Color {
        RED,
        GREEN
    }

    interface Shape {
        Object run(boolean nul) throws Throwable;
    }

    static String SF;
    static Node SN;
    Node inst;

    static final Node N = mk();
    static final List<String> LIST = new ArrayList<>(List.of("a"));

    static Node mk() {
        Node n = new Node();
        n.next = new Node();
        n.next.next = new Node();
        n.s = "abc";
        n.next.s = "abc";
        return n;
    }

    // ---- the trapping methods ------------------------------------------
    static int getfield(Node n) { return n.val; }
    static void putfield(Node n) { n.val = 1; }
    static void putfieldLong(Node n, long v) { n.lv = v; }
    static int invokevirtual(String s) { return s.length(); }
    static int invokeinterface(List<String> l) { return l.size(); }
    static int arraylength(int[] a) { return a.length; }
    static int iaload(int[] a) { return a[0]; }
    static long laload(long[] a) { return a[0]; }
    static float faload(float[] a) { return a[0]; }
    static double daload(double[] a) { return a[0]; }
    static Object aaload(Object[] a) { return a[0]; }
    static byte baload(byte[] a) { return a[0]; }
    static boolean zaload(boolean[] a) { return a[0]; }
    static char caload(char[] a) { return a[0]; }
    static short saload(short[] a) { return a[0]; }
    static void iastore(int[] a) { a[0] = 1; }
    static void lastore(long[] a) { a[0] = 1L; }
    static void fastore(float[] a) { a[0] = 1f; }
    static void dastore(double[] a) { a[0] = 1d; }
    static void aastore(Object[] a) { a[0] = "x"; }
    static void bastore(byte[] a) { a[0] = 1; }
    static void zastore(boolean[] a) { a[0] = true; }
    static void castore(char[] a) { a[0] = 'c'; }
    static void sastore(short[] a) { a[0] = 1; }
    static void athrow(RuntimeException e) { throw e; }
    static void athrowNull(boolean z) { if (z) throw null; }
    static void monitor(Object o) { synchronized (o) { SF = "in"; } }
    static int staticFieldInvoke() { return SF.length(); }
    static int staticChain() { return SN.next.val; }
    static Node maybe(boolean z) { return z ? null : N; }
    static int returnValue(boolean z) { return maybe(z).val; }
    static int returnValueChain(boolean z) { return maybe(z).next.val; }
    static int chain(Node n) { return n.next.next.val; }
    static int invokeChain(Node n) { return n.next.s.length(); }
    static void putChain(Node n) { n.next.val = 3; }
    static void putChainLong(Node n) { n.next.lv = 5L; }
    static int arrayElem(String[] a) { return a[2].length(); }
    static int arrayElemVar(String[] a, int i) { return a[i].length(); }
    static int arrayElemBig(String[] a) { return a[100].length(); }
    static int arrayElemSipush(String[] a) { return a[1000].length(); }
    static int twoDim(int[][] a) { return a[1][2]; }
    static int constIdx2(String[][] a) { return a[0][1].length(); }
    static int indexFromArray(int[] a, int[] idx) { return a[idx[0]]; }
    static int nestedIndex(String[] a, int[] idx) { return a[idx[0]].length(); }
    static void storeAfterInvoke(Object[][] a, boolean z) { a[0][0] = maybe(z); }
    static void storeLongTwoDim(long[][] a) { a[1][0] = 7L; }
    static int localVar(Object o) { String s = (String) o; return s.length(); }
    static int longLocal(String s) { long t = 5; String u = s; return u.length() + (int) t; }
    static int castChain(Object o) { return ((Node) o).val; }
    int thisField() { return inst.val; }
    static int unboxInt(Integer i) { return i; }
    static long unboxLong(Long l) { return l; }
    static boolean unboxBool(Boolean b) { return b; }
    static char unboxChar(Character c) { return c; }
    static double unboxDouble(Double d) { return d; }
    static int foreachArray(String[] arr) { int n = 0; for (String x : arr) n += x.length(); return n; }
    static int foreachList(List<String> l) { int n = 0; for (String x : l) n += x.length(); return n; }
    static int stringSwitch(String s) {
        switch (s) {
            case "a": return 1;
            case "b": return 2;
            default: return 0;
        }
    }
    static int enumSwitch(Color c) {
        switch (c) {
            case RED: return 1;
            default: return 0;
        }
    }
    static int longParam(long x, String s) { return s.length() + (int) x; }
    static int doubleParam(double x, String s, long y) { return s.length() + (int) (x + y); }
    static int ternary(boolean b, String x, String y) { return (b ? x : y).length(); }
    static Class<?> getClassOf(Object o) { return o.getClass(); }
    static int hashCodeOf(Object o) { return o.hashCode(); }
    static boolean equalsOf(Object o) { return o.equals("x"); }
    static String toStringOf(Object o) { return o.toString(); }
    int take(int a, String b, Object[] c, long[][] d, CharSequence e, double f) { return a; }
    static int params(L9W98NpeOpcodeKinds k) { return k.take(1, "s", null, null, null, 2.0); }
    static Object cloneArray(int[] a) { return a.clone(); }
    static int listGet(List<String> l, boolean z) { return (z ? nullList() : l).get(0).length(); }
    static List<String> nullList() { List<String> l = new ArrayList<>(); l.add(null); return l; }
    static String three(int a, long b, String c) { return c; }
    static int returnWithParams(boolean z) { return three(1, 2L, z ? null : "c").length(); }
    static int capturedLambda(String cap) { IntSupplier s = () -> cap.length(); return s.getAsInt(); }
    static int staticInLambda() { IntSupplier s = () -> SN.val; return s.getAsInt(); }
    static int charSeq(CharSequence cs) { return cs.length(); }
    static int interfaceDefault(Iterable<String> it) { int[] n = {0}; it.forEach(x -> n[0]++); return n[0]; }

    // ---- the harness -----------------------------------------------------
    static String message(Shape s) {
        try {
            s.run(true);
            return "NO EXCEPTION";
        } catch (NullPointerException e) {
            return e.getMessage();
        } catch (Throwable t) {
            return "UNEXPECTED " + t;
        }
    }

    static void row(String label, Shape s) {
        String cold = message(s);
        for (int i = 0; i < 12000; i++) {
            try {
                s.run(false);
            } catch (Throwable t) {
                System.out.println(label + " WARMUP-THREW " + t);
                return;
            }
        }
        String warm = message(s);
        System.out.println(label + ": " + cold);
        // A null warm message is HotSpot's OmitStackTraceInFastThrow (the
        // preallocated, message-less NPE of a hot compiled site): not compared.
        if (warm != null && !Objects.equals(cold, warm)) {
            System.out.println(label + " WARM: " + warm);
        }
    }

    static final int[] IA = new int[3];
    static final long[] LA = new long[3];
    static final float[] FA = new float[3];
    static final double[] DA = new double[3];
    static final Object[] OA = new Object[3];
    static final byte[] BA = new byte[3];
    static final boolean[] ZA = new boolean[3];
    static final char[] CA = new char[3];
    static final short[] SA = new short[3];
    static final String[] STRS = filled(1001);
    static final int[][] I2 = {new int[3], new int[3]};
    static final String[][] S2 = {filled(2)};
    static final Object[][] O2 = {new Object[1]};
    static final long[][] L2 = {new long[1], new long[1]};
    static final int[] IDX = {1};

    static String[] filled(int n) {
        String[] a = new String[n];
        for (int i = 0; i < n; i++) a[i] = "s";
        return a;
    }

    public static void main(String[] args) throws Throwable {
        row("getfield", z -> getfield(z ? null : N));
        row("putfield", z -> { putfield(z ? null : N); return null; });
        row("putfield-long", z -> { putfieldLong(z ? null : N, 3L); return null; });
        row("invokevirtual", z -> invokevirtual(z ? null : "a"));
        row("invokeinterface", z -> invokeinterface(z ? null : LIST));
        row("arraylength", z -> arraylength(z ? null : IA));
        row("iaload", z -> iaload(z ? null : IA));
        row("laload", z -> laload(z ? null : LA));
        row("faload", z -> faload(z ? null : FA));
        row("daload", z -> daload(z ? null : DA));
        row("aaload", z -> aaload(z ? null : OA));
        row("baload-byte", z -> baload(z ? null : BA));
        row("baload-boolean", z -> zaload(z ? null : ZA));
        row("caload", z -> caload(z ? null : CA));
        row("saload", z -> saload(z ? null : SA));
        row("iastore", z -> { iastore(z ? null : IA); return null; });
        row("lastore", z -> { lastore(z ? null : LA); return null; });
        row("fastore", z -> { fastore(z ? null : FA); return null; });
        row("dastore", z -> { dastore(z ? null : DA); return null; });
        row("aastore", z -> { aastore(z ? null : OA); return null; });
        row("bastore-byte", z -> { bastore(z ? null : BA); return null; });
        row("bastore-boolean", z -> { zastore(z ? null : ZA); return null; });
        row("castore", z -> { castore(z ? null : CA); return null; });
        row("sastore", z -> { sastore(z ? null : SA); return null; });
        RuntimeException re = new RuntimeException("x");
        row("athrow", z -> {
            try {
                athrow(z ? null : re);
            } catch (RuntimeException e) {
                if (e != re) throw e;
            }
            return null;
        });
        row("athrow-null", z -> { athrowNull(z); return null; });
        row("monitorenter", z -> { monitor(z ? null : N); return null; });
        row("static-field", z -> { SF = z ? null : "abc"; return staticFieldInvoke(); });
        row("static-chain", z -> { SN = z ? new Node() : N; return staticChain(); });
        row("return-value", z -> returnValue(z));
        row("return-value-chain", z -> returnValueChain(z));
        Node broken = new Node();
        broken.next = new Node();
        row("chain", z -> chain(z ? broken : N));
        row("invoke-chain", z -> invokeChain(z ? broken : N));
        row("put-chain", z -> { putChain(z ? new Node() : N); return null; });
        row("put-chain-long", z -> { putChainLong(z ? new Node() : N); return null; });
        String[] holes = new String[1001];
        row("array-elem", z -> arrayElem(z ? holes : STRS));
        row("array-elem-var", z -> arrayElemVar(z ? holes : STRS, 7));
        row("array-elem-bipush", z -> arrayElemBig(z ? holes : STRS));
        row("array-elem-sipush", z -> arrayElemSipush(z ? holes : STRS));
        row("two-dim", z -> twoDim(z ? new int[2][] : I2));
        row("two-dim-const", z -> constIdx2(z ? new String[][] {new String[2]} : S2));
        row("index-from-array", z -> indexFromArray(z ? null : IA, IDX));
        row("nested-index", z -> nestedIndex(z ? holes : STRS, IDX));
        row("store-after-invoke", z -> { storeAfterInvoke(z ? new Object[1][] : O2, z); return null; });
        row("store-long-two-dim", z -> { storeLongTwoDim(z ? new long[2][] : L2); return null; });
        row("local-var", z -> localVar(z ? null : "a"));
        row("long-local", z -> longLocal(z ? null : "a"));
        row("cast-chain", z -> castChain(z ? null : N));
        L9W98NpeOpcodeKinds self = new L9W98NpeOpcodeKinds();
        row("this-field", z -> { self.inst = z ? null : N; return self.thisField(); });
        row("unbox-int", z -> unboxInt(z ? null : 1));
        row("unbox-long", z -> unboxLong(z ? null : 1L));
        row("unbox-boolean", z -> unboxBool(z ? null : Boolean.TRUE));
        row("unbox-char", z -> unboxChar(z ? null : 'c'));
        row("unbox-double", z -> unboxDouble(z ? null : 1.0));
        row("foreach-array", z -> foreachArray(z ? null : STRS));
        row("foreach-list", z -> foreachList(z ? null : LIST));
        row("string-switch", z -> stringSwitch(z ? null : "b"));
        row("enum-switch", z -> enumSwitch(z ? null : Color.GREEN));
        row("long-param", z -> longParam(1L, z ? null : "a"));
        row("double-param", z -> doubleParam(1.0, z ? null : "a", 2L));
        row("ternary", z -> ternary(z, null, z ? null : "a"));
        row("getclass", z -> getClassOf(z ? null : "a"));
        row("hashcode", z -> hashCodeOf(z ? null : "a"));
        row("equals", z -> equalsOf(z ? null : "a"));
        row("tostring", z -> toStringOf(z ? null : "a"));
        row("params", z -> params(z ? null : self));
        row("clone-array", z -> cloneArray(z ? null : IA));
        row("list-get-return", z -> listGet(LIST, z));
        row("return-with-params", z -> returnWithParams(z));
        row("captured-lambda", z -> capturedLambda(z ? null : "a"));
        row("static-in-lambda", z -> { SN = z ? null : N; return staticInLambda(); });
        row("charsequence", z -> charSeq(z ? null : "a"));
        row("interface-default", z -> interfaceDefault(z ? null : LIST));

        // Messages HotSpot does not compute.
        System.out.println("ctor-plain: " + new NullPointerException().getMessage());
        System.out.println("ctor-text: " + new NullPointerException("given").getMessage());
        System.out.println("require-non-null: " + message(z -> Objects.requireNonNull(z ? null : "a")));
        System.out.println("require-non-null-text: "
                + message(z -> Objects.requireNonNull(z ? null : "a", "named")));
        String nullString = null;
        System.out.println("bound-method-ref: " + message(z -> {
            IntSupplier r = nullString::length;
            return r.getAsInt();
        }));
        Function<String, Integer> length = String::length;
        NullPointerException hidden = npe(() -> length.apply(null));
        System.out.println("method-ref-hidden: " + hidden.getMessage());
        System.out.println("method-ref-hidden top: " + top(hidden));
        MethodHandle mh = MethodHandles.lookup()
                .findVirtual(String.class, "length", MethodType.methodType(int.class));
        NullPointerException viaHandle = npe(() -> {
            int n = (int) mh.invokeExact((String) null);
        });
        System.out.println("handle-invoke-exact: " + viaHandle.getMessage());
        System.out.println("handle-invoke-exact top: " + top(viaHandle));
        NullPointerException reflective = npe(() -> {
            try {
                String.class.getMethod("length").invoke(null);
            } catch (java.lang.reflect.InvocationTargetException e) {
                throw e.getCause();
            }
        });
        System.out.println("reflect-null-receiver: " + reflective.getMessage());

        // An implicit NPE survives a second fillInStackTrace (the JDK computes
        // the message first), and a cloned trace does not change it.
        NullPointerException refilled = npe(() -> getfield(null));
        refilled.fillInStackTrace();
        System.out.println("refilled: " + refilled.getMessage());
        NullPointerException reset = npe(() -> getfield(null));
        reset.setStackTrace(new StackTraceElement[0]);
        System.out.println("set-trace-then-read: " + reset.getMessage());
        System.out.println("tostring-npe: " + npe(() -> getfield(null)));
    }

    interface Thrower {
        void run() throws Throwable;
    }

    static NullPointerException npe(Thrower t) {
        try {
            t.run();
        } catch (NullPointerException e) {
            return e;
        } catch (Throwable other) {
            throw new AssertionError(other);
        }
        throw new AssertionError("no NPE");
    }

    static String top(Throwable t) {
        StackTraceElement[] st = t.getStackTrace();
        if (st.length == 0) return "<none>";
        return st[0].getClassName().replaceAll("\\$\\$Lambda.*", "\\$\\$Lambda") + "." + st[0].getMethodName();
    }
}
