import io.netty.buffer.*;
public class Netty {
  static void t(String n, Runnable r){ try { r.run(); System.out.println(n+" OK"); } catch(Throwable e){ System.out.println(n+" THREW "+e.getClass().getName()+": "+e.getMessage()); } }
  public static void main(String[] a){
    t("Pooled.buffer write/read", () -> {
      ByteBuf b = PooledByteBufAllocator.DEFAULT.buffer(64);
      b.writeBytes("sample data".getBytes());
      byte[] out = new byte[b.readableBytes()]; b.readBytes(out);
      System.out.println("  pooled got='"+new String(out)+"' refCnt="+b.refCnt());
      b.release();
    });
    t("Unpooled.buffer write/read", () -> {
      ByteBuf b = Unpooled.buffer(64); b.writeBytes("hello".getBytes());
      byte[] out=new byte[b.readableBytes()]; b.readBytes(out); System.out.println("  unpooled='"+new String(out)+"'");
    });
    t("Pooled.directBuffer write/read", () -> {
      ByteBuf b = PooledByteBufAllocator.DEFAULT.directBuffer(64);
      b.writeBytes("direct data".getBytes());
      byte[] out=new byte[b.readableBytes()]; b.readBytes(out); System.out.println("  direct='"+new String(out)+"' hasMemAddr="+b.hasMemoryAddress());
      b.release();
    });
    System.out.println("DONE-NETTY");
  }
}
