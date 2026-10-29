package cratonvm;

/**
 * Differential fixture for JEP 358 helpful {@code NullPointerException}
 * messages: the action half for every null-dereferencing opcode, and the
 * {@code because "<expr>" is null} clause — including the control-flow *merge*
 * blocks (ternaries, loops, switch arms) where the operand-stack reconstruction
 * has to survive a block leader whose incoming stack is non-empty.
 *
 * Deliberately lambda-free and reflection-free so the whole fixture runs through
 * the plain interpreter path of an in-process {@code Vm::invoke}.
 */
public class DiffNpeMessage {

    static class Node {
        Node next;
        int x;
        String name;
        Node self() { return this; }
    }

    static Node snode;
    static Node[] snodes = new Node[2];
    static int sink;

    static Node mk(boolean nul) { return nul ? null : new Node(); }

    static void report(String tag, Throwable t) {
        if (t == null) {
            System.out.println(tag + ": NO NPE");
        } else if (t instanceof NullPointerException) {
            System.out.println(tag + ": " + t.getMessage());
        } else {
            System.out.println(tag + ": OTHER " + t.getClass().getName());
        }
    }

    // --- straight-line shapes -------------------------------------------
    static void invokeLocal() { String s = null; sink += s.length(); }
    static void invokeStaticField() { sink += snode.hashCode(); }
    static void invokeReturnValue() { sink += mk(true).self().x; }
    static void readField() { Node n = null; sink += n.x; }
    static void readFieldChain() { Node n = new Node(); sink += n.next.x; }
    static void assignField() { Node n = null; n.x = 1; }
    static void readStaticFieldChain() { sink += snode.x; }
    static void readArrayElementField() { sink += snodes[0].x; }
    static void arrayLength() { int[] a = null; sink += a.length; }
    static void loadInt() { int[] a = null; sink += a[0]; }
    static void storeInt() { int[] a = null; a[0] = 1; }
    static void loadObject() { Object[] a = null; sink += a[0].hashCode(); }
    static void storeObject() { Object[] a = null; a[0] = null; }
    static void loadByte() { byte[] a = null; sink += a[0]; }
    static void storeByte() { byte[] a = null; a[0] = 1; }
    static void loadBoolean() { boolean[] a = null; sink += a[0] ? 1 : 0; }
    static void loadChar() { char[] a = null; sink += a[0]; }
    static void storeChar() { char[] a = null; a[0] = 'x'; }
    static void loadShort() { short[] a = null; sink += a[0]; }
    static void loadLong() { long[] a = null; sink += (int) a[0]; }
    static void loadFloat() { float[] a = null; sink += (int) a[0]; }
    static void loadDouble() { double[] a = null; sink += (int) a[0]; }
    static void storeDouble() { double[] a = null; a[0] = 1.5; }
    static void throwNull() { RuntimeException e = null; throw e; }
    static void monitor() { Object o = null; synchronized (o) { sink++; } }
    static void unbox() { Integer i = null; sink += i; }
    static void nestedArray() { int[][] a = new int[2][]; sink += a[0][1]; }

    // --- merge-point shapes ---------------------------------------------
    static void ternaryField(boolean b) { Node n = b ? null : new Node(); sink += n.x; }
    static void ternaryAssign(boolean b) { Node n = b ? null : new Node(); n.x = 2; }
    static void ternaryArrayLoad(boolean b) { int[] a = b ? null : new int[1]; sink += a[0]; }
    static void ternaryArrayStore(boolean b) { int[] a = b ? null : new int[1]; a[0] = 3; }
    static void ternaryArrayLength(boolean b) { int[] a = b ? null : new int[1]; sink += a.length + 2; }
    static void ternaryInvoke(boolean b) { String s = b ? null : "x"; sink += s.length(); }
    static void ternaryMonitor(boolean b) { Object o = b ? null : new Object(); synchronized (o) { sink++; } }
    static void shortCircuit(boolean a, boolean b) { Node n = (a || b) ? null : new Node(); sink += n.x; }

    static void loopBody() {
        for (int i = 0; i < 3; i++) {
            Node n = (i == 2) ? null : new Node();
            sink += n.x;
        }
    }

    static void whileAccumulate() {
        int i = 0;
        while (i < 3) {
            int[] a = (i == 2) ? null : new int[1];
            sink += a[0];
            i++;
        }
    }

    static void switchArm(int k) {
        switch (k) {
            case 0: {
                String s = null;
                sink += s.length();
                break;
            }
            default:
                break;
        }
    }

    static void afterTryCatch() {
        try {
            sink += Integer.parseInt("1");
        } catch (RuntimeException e) {
            sink += 1;
        }
        Node n = null;
        n.x = 4;
    }

    static void indexedArithmetic(int i) {
        int[] a = (i > 0) ? null : new int[4];
        sink += a[i + 1];
    }

    public static void main(String[] args) {
        try { invokeLocal(); report("invoke-local", null); } catch (Throwable t) { report("invoke-local", t); }
        try { invokeStaticField(); report("invoke-static-field", null); } catch (Throwable t) { report("invoke-static-field", t); }
        try { invokeReturnValue(); report("invoke-retval", null); } catch (Throwable t) { report("invoke-retval", t); }
        try { readField(); report("getfield", null); } catch (Throwable t) { report("getfield", t); }
        try { readFieldChain(); report("getfield-chain", null); } catch (Throwable t) { report("getfield-chain", t); }
        try { assignField(); report("putfield", null); } catch (Throwable t) { report("putfield", t); }
        try { readStaticFieldChain(); report("getfield-static", null); } catch (Throwable t) { report("getfield-static", t); }
        try { readArrayElementField(); report("getfield-arrayelem", null); } catch (Throwable t) { report("getfield-arrayelem", t); }
        try { arrayLength(); report("arraylength", null); } catch (Throwable t) { report("arraylength", t); }
        try { loadInt(); report("iaload", null); } catch (Throwable t) { report("iaload", t); }
        try { storeInt(); report("iastore", null); } catch (Throwable t) { report("iastore", t); }
        try { loadObject(); report("aaload", null); } catch (Throwable t) { report("aaload", t); }
        try { storeObject(); report("aastore", null); } catch (Throwable t) { report("aastore", t); }
        try { loadByte(); report("baload", null); } catch (Throwable t) { report("baload", t); }
        try { storeByte(); report("bastore", null); } catch (Throwable t) { report("bastore", t); }
        try { loadBoolean(); report("zaload", null); } catch (Throwable t) { report("zaload", t); }
        try { loadChar(); report("caload", null); } catch (Throwable t) { report("caload", t); }
        try { storeChar(); report("castore", null); } catch (Throwable t) { report("castore", t); }
        try { loadShort(); report("saload", null); } catch (Throwable t) { report("saload", t); }
        try { loadLong(); report("laload", null); } catch (Throwable t) { report("laload", t); }
        try { loadFloat(); report("faload", null); } catch (Throwable t) { report("faload", t); }
        try { loadDouble(); report("daload", null); } catch (Throwable t) { report("daload", t); }
        try { storeDouble(); report("dastore", null); } catch (Throwable t) { report("dastore", t); }
        try { throwNull(); report("athrow", null); } catch (Throwable t) { report("athrow", t); }
        try { monitor(); report("monitor", null); } catch (Throwable t) { report("monitor", t); }
        try { unbox(); report("unbox", null); } catch (Throwable t) { report("unbox", t); }
        try { nestedArray(); report("nested-array", null); } catch (Throwable t) { report("nested-array", t); }

        try { ternaryField(true); report("ternary-getfield", null); } catch (Throwable t) { report("ternary-getfield", t); }
        try { ternaryAssign(true); report("ternary-putfield", null); } catch (Throwable t) { report("ternary-putfield", t); }
        try { ternaryArrayLoad(true); report("ternary-iaload", null); } catch (Throwable t) { report("ternary-iaload", t); }
        try { ternaryArrayStore(true); report("ternary-iastore", null); } catch (Throwable t) { report("ternary-iastore", t); }
        try { ternaryArrayLength(true); report("ternary-arraylength", null); } catch (Throwable t) { report("ternary-arraylength", t); }
        try { ternaryInvoke(true); report("ternary-invoke", null); } catch (Throwable t) { report("ternary-invoke", t); }
        try { ternaryMonitor(true); report("ternary-monitor", null); } catch (Throwable t) { report("ternary-monitor", t); }
        try { shortCircuit(true, false); report("short-circuit", null); } catch (Throwable t) { report("short-circuit", t); }
        try { loopBody(); report("loop-body", null); } catch (Throwable t) { report("loop-body", t); }
        try { whileAccumulate(); report("while-accumulate", null); } catch (Throwable t) { report("while-accumulate", t); }
        try { switchArm(0); report("switch-arm", null); } catch (Throwable t) { report("switch-arm", t); }
        try { afterTryCatch(); report("after-try-catch", null); } catch (Throwable t) { report("after-try-catch", t); }
        try { indexedArithmetic(1); report("indexed-arithmetic", null); } catch (Throwable t) { report("indexed-arithmetic", t); }
    }
}
