import java.lang.invoke.*;
public class Mh2 {
 static boolean isPos(int x){ return x>0; }
 static void t(String n, Callable c){ try { System.out.println(n+" = "+c.call()); } catch (Throwable e) { System.out.println(n+" threw "+e); } }
 interface Callable { Object call() throws Throwable; }
 public static void main(String[] x) throws Throwable {
  MethodHandles.Lookup l = MethodHandles.lookup();
  MethodHandle ct = MethodHandles.constant(boolean.class,true), cf = MethodHandles.constant(boolean.class,false);
  t("constTrue", () -> (boolean) ct.invokeExact());
  t("constFalse", () -> (boolean) cf.invokeExact());
  MethodHandle pos = l.findStatic(Mh2.class,"isPos",MethodType.methodType(boolean.class,int.class));
  t("posT", () -> (boolean) pos.invokeExact(3));
  t("posF", () -> (boolean) pos.invokeExact(-3));
  MethodHandle one = MethodHandles.dropArguments(MethodHandles.constant(int.class,1),0,int.class), two = MethodHandles.dropArguments(MethodHandles.constant(int.class,2),0,int.class);
  t("gwtPos", () -> (int) MethodHandles.guardWithTest(pos, one, two).invokeExact(5));
  t("gwtNeg", () -> (int) MethodHandles.guardWithTest(pos, one, two).invokeExact(-5));
  MethodHandle idz = MethodHandles.identity(boolean.class);
  t("idz", () -> (boolean) idz.invokeExact(true));
  MethodHandle sh = MethodHandles.identity(short.class), ch = MethodHandles.identity(char.class), by = MethodHandles.identity(byte.class);
  t("idShort", () -> (short) sh.invokeExact((short)-3));
  t("idChar", () -> (char) ch.invokeExact('c'));
  t("idByte", () -> (byte) by.invokeExact((byte)-2));
  t("idLong", () -> (long) MethodHandles.identity(long.class).invokeExact(1L<<40));
  t("idDouble", () -> (double) MethodHandles.identity(double.class).invokeExact(2.5));
  t("exactMismatch", () -> (String) cf.invokeExact());
 }}
