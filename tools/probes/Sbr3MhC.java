import java.lang.invoke.*;
public class Sbr3MhC { static boolean isPos(int x){ return x>0; }
 public static void main(String[] x) throws Throwable {
  MethodHandles.Lookup l = MethodHandles.lookup();
  MethodHandle pos = l.findStatic(Sbr3MhC.class,"isPos",MethodType.methodType(boolean.class,int.class));
  MethodHandle one = MethodHandles.dropArguments(MethodHandles.constant(int.class,1),0,int.class), two = MethodHandles.dropArguments(MethodHandles.constant(int.class,2),0,int.class);
  System.out.println((int) MethodHandles.guardWithTest(pos, one, two).invokeExact(5));
 }}
