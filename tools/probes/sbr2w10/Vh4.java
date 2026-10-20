import java.lang.invoke.*;
public class Vh4 { static int S = 0xFEEDBEEF; static final VarHandle V;
 static { try { V = MethodHandles.lookup().findStaticVarHandle(Vh4.class, "S", int.class); } catch (Exception e) { throw new Error(e); } }
 public static void main(String[] a) { int st = 0; for (int n = 0; n < Integer.parseInt(a[0]); n++) { st = (int) V.get(); } System.out.println(st); } }
