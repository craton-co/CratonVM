import java.lang.invoke.*;
public class Vh6 { static int S = 7; static final VarHandle V;
 static { try { V = MethodHandles.lookup().findStaticVarHandle(Vh6.class, "S", int.class); } catch (Exception e) { throw new Error(e); } }
 static void f() { System.out.println("S " + (int) V.get()); }
 public static void main(String[] a) throws Throwable { f(); } }
