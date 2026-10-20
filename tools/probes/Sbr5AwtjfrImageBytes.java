import java.awt.*;
import java.awt.image.*;
import java.io.*;
import java.util.zip.CRC32;
import javax.imageio.*;

/** Image-file bytes ImageIO produces, as size + CRC32, for HotSpot-vs-CratonVM diffs. */
public class Sbr5AwtjfrImageBytes {
    static BufferedImage img(int type, int w, int h) {
        BufferedImage b = new BufferedImage(w, h, type);
        for (int y = 0; y < h; y++) for (int x = 0; x < w; x++)
            b.setRGB(x, y, 0xff000000 | (((x * 7 + y * 3) & 255) << 16) | (((x * 3 + y * 11) & 255) << 8) | ((x * y) & 255));
        Graphics2D g = b.createGraphics();
        g.setColor(new Color(200, 30, 90, 160)); g.fillOval(3, 3, w - 6, h - 6);
        g.setColor(Color.WHITE); g.drawLine(0, 0, w - 1, h / 2);
        g.dispose();
        return b;
    }
    static String enc(BufferedImage b, String fmt) throws Exception {
        ByteArrayOutputStream o = new ByteArrayOutputStream();
        boolean ok = ImageIO.write(b, fmt, o);
        CRC32 c = new CRC32(); c.update(o.toByteArray());
        return ok + " n=" + o.size() + " crc=" + Long.toHexString(c.getValue());
    }
    public static void main(String[] a) throws Exception {
        int[] types = {BufferedImage.TYPE_INT_RGB, BufferedImage.TYPE_INT_ARGB, BufferedImage.TYPE_3BYTE_BGR, BufferedImage.TYPE_BYTE_GRAY};
        String[] tn = {"RGB", "ARGB", "3BYTE_BGR", "GRAY"};
        for (int i = 0; i < types.length; i++) {
            BufferedImage b = img(types[i], 64, 48);
            for (String f : new String[]{"png", "jpg", "bmp", "gif"}) {
                String r;
                try { r = enc(b, f); } catch (Throwable t) { r = "THROWS " + t.getClass().getName(); }
                System.out.println(tn[i] + " " + f + " " + r);
            }
        }
        // decode round trip pixels
        BufferedImage s = img(BufferedImage.TYPE_INT_RGB, 64, 48);
        for (String f : new String[]{"png", "jpg"}) {
            ByteArrayOutputStream o = new ByteArrayOutputStream(); ImageIO.write(s, f, o);
            BufferedImage r = ImageIO.read(new ByteArrayInputStream(o.toByteArray()));
            CRC32 c = new CRC32();
            for (int y = 0; y < r.getHeight(); y++) for (int x = 0; x < r.getWidth(); x++) c.update(r.getRGB(x, y));
            System.out.println("decode " + f + " type=" + r.getType() + " crc=" + Long.toHexString(c.getValue()));
        }
    }
}
