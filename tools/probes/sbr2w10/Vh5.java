import java.lang.invoke.*;
public class Vh5 { static int S = 7; static long L = 9;
 public static void main(String[] a) throws Throwable { MethodHandles.Lookup l = MethodHandles.lookup();
  VarHandle vl = l.findStaticVarHandle(Vh5.class, "L", long.class); System.out.println("L " + (long) vl.get());
  VarHandle vi = l.findStaticVarHandle(Vh5.class, "S", int.class); System.out.println("S " + (int) vi.get()); } }
