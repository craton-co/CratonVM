import java.lang.invoke.*;
public class Vh2 {
 static class Holder { String ref = "x"; int n; static long s; }
 static int len(String s){ return s.length(); }
 public static void main(String[] a) throws Throwable {
  MethodHandles.Lookup l = MethodHandles.lookup();
  try { VarHandle v = l.findVarHandle(Holder.class, "ref", String.class); Holder h = new Holder(); v.set(h, "y"); System.out.println("fvh " + v.get(h)); } catch (Throwable t) { System.out.println("findVarHandle " + t); }
  try { VarHandle v = l.findStaticVarHandle(Holder.class, "s", long.class); v.set(5L); System.out.println("sv " + v.get()); } catch (Throwable t) { System.out.println("findStaticVarHandle " + t); }
  try { MethodHandle m = l.findStatic(Vh2.class, "len", MethodType.methodType(int.class, String.class)); System.out.println("mh " + (int) m.invokeExact("abc")); } catch (Throwable t) { System.out.println("findStatic " + t); }
  try { MethodHandle m = l.findVirtual(String.class, "length", MethodType.methodType(int.class)); System.out.println("virt " + (int) m.invokeExact("abcd")); } catch (Throwable t) { System.out.println("findVirtual " + t); }
 }}
