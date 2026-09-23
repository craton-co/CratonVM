import java.nio.channels.AsynchronousServerSocketChannel;
import java.nio.channels.AsynchronousSocketChannel;
public class AioClassProbe {
    public static void main(String[] a) throws Exception {
        try (AsynchronousServerSocketChannel s = AsynchronousServerSocketChannel.open()) {
            System.out.println("assc.class = " + s.getClass().getName());
        }
        try (AsynchronousSocketChannel c = AsynchronousSocketChannel.open()) {
            System.out.println("asc.class  = " + c.getClass().getName());
        }
        System.out.println("RESULT done");
    }
}
