import java.awt.*;
import java.awt.image.*;
import java.io.*;
import javax.imageio.ImageIO;

/**
 * Sweep 17: a headless AWT smoke probe.
 *
 * G79-1 §4 measured that NO vector in the suite references java.awt,
 * javax.swing or javax.imageio — all three arms are blind to the subsystem the
 * P0 over-tagging row recommends retagging FIRST. This probe exists to find out
 * what a vector could honestly assert before one is written (G79-1 N3).
 *
 * Deliberately avoids anything a software rasterizer may legitimately render
 * differently from Java2D: no line antialiasing, no font metrics, no curves.
 * Solid fills read back through getRGB, image geometry, type constants and an
 * ImageIO round trip are contracts, not rendering choices.
 */
public class Sweep17AwtHeadless {
    interface C { Object g() throws Exception; }
    static void t(String l, C c) {
        try { System.out.println("A " + l + " = " + c.g()); }
        catch (Throwable x) {
            System.out.println("A " + l + " = " + x.getClass().getName() + " | " + x.getMessage());
        }
    }

    public static void main(String[] a) {
        t("headless", () -> GraphicsEnvironment.isHeadless());

        // ---- BufferedImage geometry and type constants ------------------------
        t("img_type_int_rgb", () -> BufferedImage.TYPE_INT_RGB);
        t("img_type_int_argb", () -> BufferedImage.TYPE_INT_ARGB);
        t("img_dims", () -> {
            BufferedImage b = new BufferedImage(7, 5, BufferedImage.TYPE_INT_RGB);
            return b.getWidth() + "x" + b.getHeight() + " type=" + b.getType();
        });
        t("img_initial_pixel", () -> {
            BufferedImage b = new BufferedImage(4, 4, BufferedImage.TYPE_INT_RGB);
            return Integer.toHexString(b.getRGB(0, 0));
        });
        t("img_argb_initial", () -> {
            BufferedImage b = new BufferedImage(4, 4, BufferedImage.TYPE_INT_ARGB);
            return Integer.toHexString(b.getRGB(0, 0));
        });
        t("img_setRGB_roundtrip", () -> {
            BufferedImage b = new BufferedImage(4, 4, BufferedImage.TYPE_INT_RGB);
            b.setRGB(2, 3, 0x00FF7F10);
            return Integer.toHexString(b.getRGB(2, 3));
        });
        t("img_getRGB_oob", () -> {
            BufferedImage b = new BufferedImage(4, 4, BufferedImage.TYPE_INT_RGB);
            return b.getRGB(9, 9);
        });

        // ---- Solid fills: a contract, not a rendering choice -------------------
        t("g2d_fillRect_inside", () -> {
            BufferedImage b = new BufferedImage(8, 8, BufferedImage.TYPE_INT_RGB);
            Graphics2D g = b.createGraphics();
            g.setColor(new Color(0x20, 0x40, 0x60));
            g.fillRect(2, 2, 4, 4);
            g.dispose();
            return Integer.toHexString(b.getRGB(3, 3));
        });
        t("g2d_fillRect_outside", () -> {
            BufferedImage b = new BufferedImage(8, 8, BufferedImage.TYPE_INT_RGB);
            Graphics2D g = b.createGraphics();
            g.setColor(new Color(0x20, 0x40, 0x60));
            g.fillRect(2, 2, 4, 4);
            g.dispose();
            return Integer.toHexString(b.getRGB(0, 0));
        });
        t("g2d_fillRect_edges", () -> {
            BufferedImage b = new BufferedImage(8, 8, BufferedImage.TYPE_INT_RGB);
            Graphics2D g = b.createGraphics();
            g.setColor(Color.WHITE);
            g.fillRect(2, 2, 4, 4);
            g.dispose();
            // inclusive top-left corner, exclusive bottom-right
            return Integer.toHexString(b.getRGB(2, 2)) + "|" + Integer.toHexString(b.getRGB(6, 6));
        });
        t("g2d_clearRect", () -> {
            BufferedImage b = new BufferedImage(4, 4, BufferedImage.TYPE_INT_RGB);
            Graphics2D g = b.createGraphics();
            g.setColor(Color.WHITE);
            g.fillRect(0, 0, 4, 4);
            g.setBackground(Color.BLACK);
            g.clearRect(1, 1, 2, 2);
            g.dispose();
            return Integer.toHexString(b.getRGB(1, 1)) + "|" + Integer.toHexString(b.getRGB(0, 0));
        });
        t("g2d_clip_blocks", () -> {
            BufferedImage b = new BufferedImage(8, 8, BufferedImage.TYPE_INT_RGB);
            Graphics2D g = b.createGraphics();
            g.setClip(0, 0, 2, 2);
            g.setColor(Color.WHITE);
            g.fillRect(0, 0, 8, 8);
            g.dispose();
            return Integer.toHexString(b.getRGB(1, 1)) + "|" + Integer.toHexString(b.getRGB(5, 5));
        });
        t("g2d_getClipBounds", () -> {
            BufferedImage b = new BufferedImage(8, 8, BufferedImage.TYPE_INT_RGB);
            Graphics2D g = b.createGraphics();
            g.setClip(1, 2, 3, 4);
            Rectangle r = g.getClipBounds();
            g.dispose();
            return r.x + "," + r.y + "," + r.width + "," + r.height;
        });
        t("g2d_color_roundtrip", () -> {
            BufferedImage b = new BufferedImage(4, 4, BufferedImage.TYPE_INT_RGB);
            Graphics2D g = b.createGraphics();
            g.setColor(new Color(1, 2, 3));
            Color c = g.getColor();
            g.dispose();
            return c.getRed() + "," + c.getGreen() + "," + c.getBlue();
        });

        // ---- Color contracts ---------------------------------------------------
        t("color_rgb", () -> Integer.toHexString(new Color(0x20, 0x40, 0x60).getRGB()));
        t("color_white", () -> Integer.toHexString(Color.WHITE.getRGB()));
        t("color_equals", () -> new Color(1, 2, 3).equals(new Color(1, 2, 3)));

        // ---- Raster / SampleModel ---------------------------------------------
        t("raster_bounds", () -> {
            BufferedImage b = new BufferedImage(6, 3, BufferedImage.TYPE_INT_RGB);
            Raster r = b.getRaster();
            return r.getWidth() + "x" + r.getHeight() + " bands=" + r.getNumBands();
        });
        t("samplemodel_kind", () -> {
            BufferedImage b = new BufferedImage(6, 3, BufferedImage.TYPE_INT_RGB);
            return b.getSampleModel().getClass().getName();
        });
        t("colormodel_pixelsize", () -> {
            BufferedImage b = new BufferedImage(6, 3, BufferedImage.TYPE_INT_ARGB);
            return b.getColorModel().getPixelSize() + " alpha=" + b.getColorModel().hasAlpha();
        });

        // ---- ImageIO round trip ------------------------------------------------
        t("imageio_png_roundtrip", () -> {
            BufferedImage b = new BufferedImage(5, 4, BufferedImage.TYPE_INT_RGB);
            Graphics2D g = b.createGraphics();
            g.setColor(new Color(0x11, 0x22, 0x33));
            g.fillRect(0, 0, 5, 4);
            g.dispose();
            ByteArrayOutputStream out = new ByteArrayOutputStream();
            boolean ok = ImageIO.write(b, "png", out);
            BufferedImage back = ImageIO.read(new ByteArrayInputStream(out.toByteArray()));
            return ok + " " + back.getWidth() + "x" + back.getHeight() + " px="
                    + Integer.toHexString(back.getRGB(1, 1));
        });
        t("imageio_png_magic", () -> {
            BufferedImage b = new BufferedImage(2, 2, BufferedImage.TYPE_INT_RGB);
            ByteArrayOutputStream out = new ByteArrayOutputStream();
            ImageIO.write(b, "png", out);
            byte[] d = out.toByteArray();
            StringBuilder sb = new StringBuilder();
            for (int i = 0; i < 8 && i < d.length; i++) sb.append(Integer.toHexString(d[i] & 0xFF)).append(' ');
            return sb.toString().trim();
        });
        t("imageio_unknown_format", () -> {
            BufferedImage b = new BufferedImage(2, 2, BufferedImage.TYPE_INT_RGB);
            return ImageIO.write(b, "no-such-format", new ByteArrayOutputStream());
        });

        System.out.println("A done = 1");
    }
}
