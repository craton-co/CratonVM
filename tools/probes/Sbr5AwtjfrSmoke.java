import java.awt.*;
import java.awt.image.*;

/** SBR5 awtjfr: first-light smoke test of the real Java2D stack. */
public class Sbr5AwtjfrSmoke {
    public static void main(String[] a) throws Exception {
        System.setProperty("java.awt.headless", "true");
        try {
            BufferedImage b = new BufferedImage(16, 8, BufferedImage.TYPE_INT_ARGB);
            System.out.println("created " + b.getWidth() + "x" + b.getHeight() + " type=" + b.getType());
            b.setRGB(1, 1, 0x80FF0040);
            System.out.println("rgb=" + Integer.toHexString(b.getRGB(1, 1)));
            Graphics2D g = b.createGraphics();
            System.out.println("g2d=" + g.getClass().getName());
            g.setColor(Color.RED);
            g.fillRect(0, 0, 4, 4);
            System.out.println("fill=" + Integer.toHexString(b.getRGB(1, 1)));
            g.setColor(new Color(0, 0, 255, 128));
            g.fillRect(2, 2, 4, 4);
            System.out.println("alpha=" + Integer.toHexString(b.getRGB(3, 3)));
            g.fillOval(4, 0, 10, 8);
            System.out.println("oval=" + Integer.toHexString(b.getRGB(8, 4)));
            g.dispose();
        } catch (Throwable t) {
            t.printStackTrace(System.out);
        }
    }
}
