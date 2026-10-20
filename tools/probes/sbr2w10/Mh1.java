import java.lang.invoke.*;
public class Mh1 {
 static int add(int a, int b){ return a+b; }
 static String cat(String a, String b){ return a+b; }
 static String up(String s){ return s.toUpperCase(); }
 static void t(String n, Callable c){ try { System.out.println(n+" = "+c.call()); } catch (Throwable e) { System.out.println(n+" threw "+e); } }
 interface Callable { Object call() throws Throwable; }
 public static void main(String[] x) throws Throwable {
  MethodHandles.Lookup l = MethodHandles.lookup();
  MethodHandle add = l.findStatic(Mh1.class,"add",MethodType.methodType(int.class,int.class,int.class));
  MethodHandle cat = l.findStatic(Mh1.class,"cat",MethodType.methodType(String.class,String.class,String.class));
  MethodHandle up = l.findStatic(Mh1.class,"up",MethodType.methodType(String.class,String.class));
  t("insert", () -> (int) MethodHandles.insertArguments(add,0,5).invokeExact(3));
  t("drop", () -> (int) MethodHandles.dropArguments(add,0,String.class).invokeExact("z",1,2));
  t("filterArg", () -> (String) MethodHandles.filterArguments(cat,0,up).invokeExact("a","b"));
  t("filterRet", () -> (String) MethodHandles.filterReturnValue(cat,up).invokeExact("a","b"));
  t("bindTo", () -> (String) cat.bindTo("x").invokeExact("y"));
  t("asType", () -> (Object) cat.asType(MethodType.methodType(Object.class,Object.class,Object.class)).invokeExact((Object)"p",(Object)"q"));
  t("const", () -> (int) MethodHandles.constant(int.class,42).invokeExact());
  t("identity", () -> (int) MethodHandles.identity(int.class).invokeExact(7));
  t("permute", () -> (String) MethodHandles.permuteArguments(cat,MethodType.methodType(String.class,String.class,String.class),1,0).invokeExact("a","b"));
  t("invoke", () -> add.invoke(1,2));
  t("invokeWith", () -> add.invokeWithArguments(4,5));
  t("spreader", () -> (int) add.asSpreader(Object[].class,2).invokeExact((Object[])new Object[]{1,2}));
  t("guard", () -> (int) MethodHandles.guardWithTest(MethodHandles.constant(boolean.class,true), MethodHandles.dropArguments(MethodHandles.constant(int.class,1),0,int.class), MethodHandles.dropArguments(MethodHandles.constant(int.class,2),0,int.class)).invokeExact(0));
 }}
