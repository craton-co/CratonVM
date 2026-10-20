import java.nio.*;
import java.nio.channels.*;
public class R19PipeHang {
    public static void main(String[] a) throws Exception {
        Pipe p = Pipe.open();
        System.out.println("open"); System.out.flush();
        p.source().configureBlocking(false);
        System.out.println("nb"); System.out.flush();
        int empty = p.source().read(ByteBuffer.allocate(4));
        System.out.println("empty " + empty); System.out.flush();
        p.sink().write(ByteBuffer.wrap("pp".getBytes()));
        System.out.println("wrote"); System.out.flush();
        ByteBuffer b = ByteBuffer.allocate(4);
        int n = p.source().read(b);
        System.out.println("read " + n); System.out.flush();
        p.sink().close();
        System.out.println("closed sink"); System.out.flush();
        int eof = p.source().read(ByteBuffer.allocate(1));
        System.out.println("eof " + eof); System.out.flush();
        Pipe q = Pipe.open();
        q.sink().close();
        System.out.println("blocking eof " + q.source().read(ByteBuffer.allocate(1)));
    }
}
