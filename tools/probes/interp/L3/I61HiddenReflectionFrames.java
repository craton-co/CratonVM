// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company
// i61: retain real hidden accessor adapters, their ordering, descriptors,
// BCIs, declaring Class mirrors, filtering and depth under reflection.
// Only the VM-specific hidden-class address is normalized against HotSpot.
import java.lang.invoke.*;
import java.lang.reflect.*;
import java.util.*;
public class I61HiddenReflectionFrames {
 static StackWalker walker;
 static String trace;
 static boolean record=true;
 static void capture() {
  if(!record)return;
  trace=walker.walk(s->{
   var frames=s.dropWhile(f->!f.getMethodName().startsWith("target")&&!f.getMethodName().equals("<init>")).takeWhile(f->!f.getMethodName().startsWith("via")).toList();
   var out=new ArrayList<String>();
   for(var f:frames){
    var c=f.getDeclaringClass();
    if(!c.getName().equals(f.getClassName()))throw new AssertionError("wrong mirror");
    if(!f.getMethodType().toMethodDescriptorString().equals(f.getDescriptor()))throw new AssertionError("wrong type");
    out.add(f.getClassName().replaceAll("/0x[0-9a-fA-F]+","/0x?")+"."+f.getMethodName()+f.getDescriptor()+"@"+f.getByteCodeIndex()+" hidden="+c.isHidden());
   }
   return String.join(" < ",out);
  });
 }
 public static void target0(){capture();}
 public static int target1(int x){capture();return x;}
 public static long target4(int a,long b,Object c,double d){capture();return a+b;}
 public int targetVirtual(String s){capture();return s.length();}
 public I61HiddenReflectionFrames(){capture();}
 public I61HiddenReflectionFrames(int n){capture();}
 static Object viaReflect(Method m,Object receiver,Object...args)throws Throwable{return m.invoke(receiver,args);}
 static Object viaCtor(Constructor<?> c,Object...args)throws Throwable{return c.newInstance(args);}
 static void viaHandle(MethodHandle h)throws Throwable{h.invokeExact();}
 static void report(String label){System.out.println(label+": "+trace);}
 public static void main(String[] args)throws Throwable {
  var m0=I61HiddenReflectionFrames.class.getMethod("target0");
  var m1=I61HiddenReflectionFrames.class.getMethod("target1",int.class);
  var m4=I61HiddenReflectionFrames.class.getMethod("target4",int.class,long.class,Object.class,double.class);
  for(var option:new String[]{"hidden","reflect","plain"}){
   var options=EnumSet.of(StackWalker.Option.RETAIN_CLASS_REFERENCE);
   if(!option.equals("plain"))options.add(StackWalker.Option.SHOW_REFLECT_FRAMES);
   if(option.equals("hidden"))options.add(StackWalker.Option.SHOW_HIDDEN_FRAMES);
   walker=StackWalker.getInstance(options);
   viaReflect(m0,null);report(option+"-static0");
   viaReflect(m1,null,7);report(option+"-static1");
   viaReflect(m4,null,7,8L,"s",9.0);report(option+"-static4");
   record=false;var receiver=new I61HiddenReflectionFrames();record=true;
   viaReflect(I61HiddenReflectionFrames.class.getMethod("targetVirtual",String.class),receiver,"abc");report(option+"-virtual");
   viaCtor(I61HiddenReflectionFrames.class.getConstructor());report(option+"-ctor0");
   viaCtor(I61HiddenReflectionFrames.class.getConstructor(int.class),3);report(option+"-ctor1");
   viaHandle(MethodHandles.lookup().findStatic(I61HiddenReflectionFrames.class,"target0",MethodType.methodType(void.class)));report(option+"-handle-control");
  }
  walker=StackWalker.getInstance(EnumSet.of(StackWalker.Option.RETAIN_CLASS_REFERENCE,StackWalker.Option.SHOW_HIDDEN_FRAMES,StackWalker.Option.SHOW_REFLECT_FRAMES));
  record=false;for(int i=0;i<20000;i++)viaReflect(m0,null);record=true;
  viaReflect(m0,null);report("warm-static0");
  for(int i=0;i<4;i++)System.gc();
  viaReflect(m0,null);report("after-gc-static0");
 }
}
