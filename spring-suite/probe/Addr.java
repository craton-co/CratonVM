import io.netty.buffer.*;
public class Addr {
  static void show(String n, ByteBuf b){
    try { System.out.println(n+": hasAddr="+b.hasMemoryAddress()+(b.hasMemoryAddress()?" memAddr=0x"+Long.toHexString(b.memoryAddress()):"")); }
    catch(Throwable t){ System.out.println(n+" THREW "+t); }
  }
  public static void main(String[] a){
    show("pooled.directBuffer(64)", PooledByteBufAllocator.DEFAULT.directBuffer(64));
    show("unpooled.directBuffer(64)", UnpooledByteBufAllocator.DEFAULT.directBuffer(64));
    // Netty's view of platform direct support:
    try {
      Class<?> pd = Class.forName("io.netty.util.internal.PlatformDependent");
      System.out.println("hasUnsafe="+pd.getMethod("hasUnsafe").invoke(null));
      System.out.println("useDirectBufferNoCleaner="+pd.getMethod("useDirectBufferNoCleaner").invoke(null));
      System.out.println("directBufferPreferred="+pd.getMethod("directBufferPreferred").invoke(null));
    } catch(Throwable t){ System.out.println("PD introspection THREW "+t); }
    System.out.println("END");
  }
}
