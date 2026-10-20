import java.lang.invoke.*;
public class Vh3 {
 static class H { boolean z; byte b; short s; char c; int i; long j; float f; double d; String r; }
 static void p(String n, Object o){ System.out.println(n+"="+o); }
 public static void main(String[] a) throws Throwable {
  MethodHandles.Lookup l = MethodHandles.lookup(); H h = new H();
  VarHandle z=l.findVarHandle(H.class,"z",boolean.class), b=l.findVarHandle(H.class,"b",byte.class), s=l.findVarHandle(H.class,"s",short.class), c=l.findVarHandle(H.class,"c",char.class), i=l.findVarHandle(H.class,"i",int.class), j=l.findVarHandle(H.class,"j",long.class), f=l.findVarHandle(H.class,"f",float.class), d=l.findVarHandle(H.class,"d",double.class), r=l.findVarHandle(H.class,"r",String.class);
  z.set(h,true); p("z",(boolean)z.get(h)); b.set(h,(byte)-7); p("b",(byte)b.get(h)); s.set(h,(short)-300); p("s",(short)s.get(h)); c.set(h,'q'); p("c",(char)c.get(h));
  i.set(h,-5); p("i",(int)i.get(h)); j.set(h,1L<<40); p("j",(long)j.get(h)); f.set(h,1.5f); p("f",(float)f.get(h)); d.set(h,2.5); p("d",(double)d.get(h)); r.set(h,"x"); p("r",(String)r.get(h));
  p("cas", i.compareAndSet(h,-5,9)+" "+(int)i.get(h)); p("gaa", (int)i.getAndAdd(h,3)+" "+(int)i.get(h));
 }}
