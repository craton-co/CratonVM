import java.nio.file.*; import java.nio.*; import java.nio.channels.*; import java.util.stream.*; import java.io.*;
public class Tiny {
  static void t(String n, java.util.concurrent.Callable<?> c){ try { System.out.println(n+" = "+c.call()); } catch(Throwable e){ System.out.println(n+" THREW "+e.getClass().getSimpleName()+": "+e.getMessage()); } }
  public static void main(String[] a) throws Exception {
    t("multidim String[2][2].class", () -> new String[2][2].getClass().getName());            // [[Ljava.lang.String;
    t("multidim int[3][4].class", () -> new int[3][4].getClass().getName());                  // [[I
    t("Collectors.toList().supplier()", () -> Collectors.toList().supplier()!=null);           // true
    t("Files.isSameFile(.,.)", () -> { Path p=Paths.get("."); return Files.isSameFile(p,p); });// true
    t("Channels.newChannel(baos).write", () -> { var baos=new ByteArrayOutputStream(); var ch=Channels.newChannel(baos); ch.write(ByteBuffer.wrap("hi".getBytes())); return baos.toString(); }); // hi
    t("ScheduledFuture.getDelay", () -> { var ex=java.util.concurrent.Executors.newScheduledThreadPool(1); var f=ex.schedule(()->1,10,java.util.concurrent.TimeUnit.SECONDS); long d=f.getDelay(java.util.concurrent.TimeUnit.SECONDS); ex.shutdownNow(); return d>=0; }); // true
    System.out.println("DONE-TINY");
  }
}
