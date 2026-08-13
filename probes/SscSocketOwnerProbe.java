import java.net.InetSocketAddress;
import java.net.ServerSocket;
import java.nio.channels.ServerSocketChannel;

/** W7-88: reach ServerSocketChannel.socket() so the census can name its owner
 *  by invocation count, and read the view back through real JDK accessors. */
public class SscSocketOwnerProbe {
    public static void main(String[] a) throws Exception {
        ServerSocketChannel ch = ServerSocketChannel.open();
        ch.bind(new InetSocketAddress("127.0.0.1", 0));
        ServerSocket ss = ch.socket();
        System.out.println("socket.class      = " + ss.getClass().getName());
        System.out.println("socket.isClosed   = " + ss.isClosed());
        System.out.println("socket.isBound    = " + ss.isBound());
        System.out.println("socket.localPort>0= " + (ss.getLocalPort() > 0));
        System.out.println("channel.isOpen    = " + ch.isOpen());
        ch.close();
        System.out.println("channel.isOpen(closed) = " + ch.isOpen());
    }
}
