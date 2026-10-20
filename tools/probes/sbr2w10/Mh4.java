import java.lang.invoke.*;
public class Mh4 { static int sumInts(int... a){ int s=0; for(int v:a) s+=v; return s; }
 public static void main(String[] x) throws Throwable {
  MethodHandle h = MethodHandles.lookup().findStatic(Mh4.class,"sumInts",MethodType.methodType(int.class,int[].class));
  MethodHandle va = h.asVarargsCollector(int[].class);
  System.out.println(va.getClass().getName() + " " + va.isVarargsCollector());
  try { MethodHandle a = va.asType(MethodType.methodType(int.class,int.class,int.class,int.class,int.class)); System.out.println(a.type()+" "+(int)a.invokeExact(1,2,3,4)); } catch (Throwable t) { System.out.println("asType "+t); }
  try { System.out.println((int) va.invoke(1,2,3,4)); } catch (Throwable t) { System.out.println("invoke "+t); }
 }}
