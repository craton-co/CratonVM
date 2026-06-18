import io.netty.buffer.*;
public class N2 {
  public static void main(String[] a){
    String w=a[0];
    try {
      ByteBuf b;
      switch(w){
        case "heap-alloc":   b=PooledByteBufAllocator.DEFAULT.heapBuffer(64); System.out.println("heap alloc ok dir="+b.isDirect()); b.release(); break;
        case "heap-write":   b=PooledByteBufAllocator.DEFAULT.heapBuffer(64); b.writeBytes("xy".getBytes()); System.out.println("heap write ok "+b.readableBytes()); b.release(); break;
        case "direct-alloc": b=PooledByteBufAllocator.DEFAULT.directBuffer(64); System.out.println("direct alloc ok hasAddr="+b.hasMemoryAddress()); b.release(); break;
        case "direct-write": b=PooledByteBufAllocator.DEFAULT.directBuffer(64); b.writeBytes("xy".getBytes()); System.out.println("direct write ok "+b.readableBytes()); b.release(); break;
        case "default-which":b=PooledByteBufAllocator.DEFAULT.buffer(64); System.out.println("default buffer isDirect="+b.isDirect()); b.release(); break;
      }
    } catch(Throwable t){ System.out.println(w+" THREW "+t.getClass().getName()+": "+t.getMessage()); }
    System.out.println("END");
  }
}
