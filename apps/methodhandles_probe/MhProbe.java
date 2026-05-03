import java.lang.invoke.*;
public class MhProbe {
    static int staticPrim(int a, int b) { return a + b; }
    static String staticObj(String s) { return s.toUpperCase(); }
    static int staticVarargs(int... xs) { int s = 0; for (int x : xs) s += x; return s; }
    int virtPrim(int a, int b) { return a * b; }
    String virtObj(String s) { return s + "!"; }
    public static void main(String[] a) throws Throwable {
        MethodHandles.Lookup L = MethodHandles.lookup();
        MhProbe self = new MhProbe();
        // findStatic primitive
        MethodHandle h1 = L.findStatic(MhProbe.class, "staticPrim", MethodType.methodType(int.class, int.class, int.class));
        System.out.println("findStatic.prim=" + (int) h1.invoke(3, 4));   // expect 7
        // findStatic object
        MethodHandle h2 = L.findStatic(MhProbe.class, "staticObj", MethodType.methodType(String.class, String.class));
        System.out.println("findStatic.obj=" + (String) h2.invoke("hello"));  // expect HELLO
        // findStatic varargs
        MethodHandle h3 = L.findStatic(MhProbe.class, "staticVarargs", MethodType.methodType(int.class, int[].class));
        System.out.println("findStatic.varargs=" + (int) h3.invoke(new int[]{1,2,3,4}));  // expect 10
        // findVirtual primitive
        MethodHandle h4 = L.findVirtual(MhProbe.class, "virtPrim", MethodType.methodType(int.class, int.class, int.class));
        System.out.println("findVirtual.prim=" + (int) h4.invoke(self, 5, 6));  // expect 30
        // findVirtual object
        MethodHandle h5 = L.findVirtual(MhProbe.class, "virtObj", MethodType.methodType(String.class, String.class));
        System.out.println("findVirtual.obj=" + (String) h5.invoke(self, "hi"));  // expect hi!
        // findStatic on JDK class
        MethodHandle h6 = L.findStatic(Integer.class, "parseInt", MethodType.methodType(int.class, String.class));
        System.out.println("findStatic.jdk=" + (int) h6.invoke("42"));  // expect 42
        // findVirtual on JDK class
        MethodHandle h7 = L.findVirtual(String.class, "length", MethodType.methodType(int.class));
        System.out.println("findVirtual.jdk=" + (int) h7.invoke("xyzzy"));  // expect 5
        System.out.println("OK");
    }
}
