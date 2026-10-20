import java.awt.image.*;
import java.io.*;
import java.util.*;
import java.util.zip.CRC32;
import javax.imageio.*;
import javax.imageio.stream.*;

/** SBR5 awtjfr: JPEG encoder/decoder oracle. Prints hex of ImageIO output and the decode CRC. */
public class Sbr5JpegOracle {
    static int gen(int x, int y, int c) { return (x * 13 + y * 29 + c * 71 + ((x * y) % 17) * 5) & 0xFF; }
    static BufferedImage mk(int type, int w, int h) {
        BufferedImage b = new BufferedImage(w, h, type);
        for (int y = 0; y < h; y++) for (int x = 0; x < w; x++)
            b.setRGB(x, y, 0xff000000 | gen(x, y, 0) << 16 | gen(x, y, 1) << 8 | gen(x, y, 2));
        return b;
    }
    static String hex(byte[] b) { StringBuilder s = new StringBuilder(); for (byte v : b) s.append(String.format("%02x", v & 0xff)); return s.toString(); }
    static String crc(BufferedImage b) {
        CRC32 c = new CRC32();
        for (int y = 0; y < b.getHeight(); y++) for (int x = 0; x < b.getWidth(); x++) { int p = b.getRGB(x, y); c.update(p >>> 24); c.update(p >>> 16); c.update(p >>> 8); c.update(p); }
        return Long.toHexString(c.getValue());
    }
    static byte[] enc(BufferedImage b, float q) throws Exception { return enc(b, q, false); }
    static byte[] enc(BufferedImage b, float q, boolean progressive) throws Exception { return enc(b, q, progressive, false); }
    static byte[] enc(BufferedImage b, float q, boolean progressive, boolean opt) throws Exception {
        ImageWriter w = ImageIO.getImageWritersByFormatName("jpeg").next();
        javax.imageio.plugins.jpeg.JPEGImageWriteParam p = (javax.imageio.plugins.jpeg.JPEGImageWriteParam) w.getDefaultWriteParam();
        if (opt) p.setOptimizeHuffmanTables(true);
        p.setCompressionMode(ImageWriteParam.MODE_EXPLICIT);
        p.setCompressionQuality(q);
        if (progressive) p.setProgressiveMode(ImageWriteParam.MODE_DEFAULT);
        ByteArrayOutputStream bo = new ByteArrayOutputStream();
        try (ImageOutputStream ios = ImageIO.createImageOutputStream(bo)) {
            w.setOutput(ios);
            w.write(null, new IIOImage(b, null, null), p);
        }
        w.dispose();
        return bo.toByteArray();
    }
    public static void main(String[] a) throws Exception {
        boolean full = a.length > 0;
        int[][] sizes = {{16, 16}, {23, 17}, {8, 8}, {1, 1}, {33, 9}, {17, 33}};
        for (int type : new int[]{BufferedImage.TYPE_INT_RGB, BufferedImage.TYPE_BYTE_GRAY}) {
            for (int[] s : sizes) for (float q : new float[]{0.75f, 0.3f, 0.95f}) {
                BufferedImage b = mk(type, s[0], s[1]);
                byte[] j = enc(b, q);
                BufferedImage d = ImageIO.read(new ByteArrayInputStream(j));
                CRC32 c = new CRC32(); c.update(j);
                System.out.println("ENC type=" + type + " " + s[0] + "x" + s[1] + " q=" + q + " len=" + j.length + " crc=" + Long.toHexString(c.getValue())
                    + " dec=" + crc(d) + " dectype=" + d.getType() + (full && j.length < 2400 ? " hex=" + hex(j) : ""));
            }
        }
        for (int[] s : sizes) {
            BufferedImage b = mk(BufferedImage.TYPE_INT_RGB, s[0], s[1]);
            byte[] j = enc(b, 0.75f, false, true);
            CRC32 c = new CRC32(); c.update(j);
            System.out.println("OPT type=1 " + s[0] + "x" + s[1] + " q=0.75 len=" + j.length + " crc=" + Long.toHexString(c.getValue()) + (full && j.length < 2400 ? " hex=" + hex(j) : ""));
        }
        for (int[] s : sizes) {
            BufferedImage b = mk(BufferedImage.TYPE_INT_RGB, s[0], s[1]);
            byte[] j = enc(b, 0.75f, true);
            BufferedImage d = ImageIO.read(new ByteArrayInputStream(j));
            CRC32 c = new CRC32(); c.update(j);
            System.out.println("PROG type=1 " + s[0] + "x" + s[1] + " q=0.75 len=" + j.length + " crc=" + Long.toHexString(c.getValue())
                + " dec=" + crc(d) + " dectype=" + d.getType() + (full && j.length < 2400 ? " hex=" + hex(j) : ""));
        }
    }
}
