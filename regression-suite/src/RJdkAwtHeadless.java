import java.awt.Color;
import java.awt.Graphics2D;
import java.awt.GraphicsEnvironment;
import java.awt.Rectangle;
import java.awt.image.BufferedImage;
import java.io.ByteArrayInputStream;
import java.io.ByteArrayOutputStream;
import javax.imageio.ImageIO;

/**
 * JDK-only corpus: headless AWT image and Graphics2D CONTRACTS.
 *
 * This vector exists because the suite had none. G79-1 measured that no vector
 * in regression-suite/src referenced java.awt, javax.swing or javax.imageio, so
 * all three arms were blind to the subsystem the P0 "wholesale Bridge
 * over-tagging" row nominates for retagging FIRST. A retag decides which
 * implementation answers a call; doing that with no differential coverage means
 * the arms staying green proves nothing about it. This is the safety net.
 *
 * WHAT IT DOES NOT ASK. Nothing here touches rasterization quality -- no
 * antialiasing, no strokes, no curves, no font metrics, no text. A software
 * renderer may legitimately differ from Java2D on those, and a vector that
 * pinned them would be asserting an implementation rather than a contract.
 * Every row below is a contract: a value the platform specifies.
 *
 * WHAT IT DELIBERATELY OMITS. G80-1 measured three rows that still diverge and
 * they are NOT asserted here: BufferedImage.getRaster(), getSampleModel() and
 * getColorModel() answer null under --jdk-only, because our BufferedImage is a
 * handle into a native side table whose fields the real object never sees.
 * Writing the present behaviour in would freeze a defect into the suite. When
 * G80-1 N1 is settled, those rows belong here.
 *
 * Determinism: no display, no fonts, no timing, no file system. Headless is set
 * before any AWT class initialises.
 */
public class RJdkAwtHeadless {
    static int checks;

    static void check(boolean c, String m) {
        checks++;
        if (!c) {
            throw new AssertionError("RJdkAwtHeadless: " + m);
        }
    }

    static void ckI(int got, int want, String what) {
        check(got == want, what + " = " + got + " want " + want);
    }

    static void ckH(int got, int want, String what) {
        check(got == want, what + " = " + Integer.toHexString(got)
                + " want " + Integer.toHexString(want));
    }

    static BufferedImage rgb(int w, int h) {
        return new BufferedImage(w, h, BufferedImage.TYPE_INT_RGB);
    }

    // ---- geometry and the type constants -----------------------------------
    static void geometry() {
        ckI(BufferedImage.TYPE_INT_RGB, 1, "TYPE_INT_RGB");
        ckI(BufferedImage.TYPE_INT_ARGB, 2, "TYPE_INT_ARGB");
        BufferedImage b = rgb(7, 5);
        ckI(b.getWidth(), 7, "getWidth");
        ckI(b.getHeight(), 5, "getHeight");
        ckI(b.getType(), BufferedImage.TYPE_INT_RGB, "getType");
        System.out.println("CK RJdkAwtHeadless geometry=" + b.getWidth() + "x" + b.getHeight());
    }

    // ---- the alpha contract -------------------------------------------------
    // An OPAQUE image has no alpha channel to report, so getRGB must SET it:
    // the JDK returns the ColorModel's RGB and an opaque model answers 0xFF for
    // alpha whatever the backing store holds. G80-1 measured this returning the
    // raw 24-bit value for every pixel the rasterizer had touched, while a
    // pristine image read correctly -- which looked like a drawing bug and was a
    // read bug. Both halves are pinned here so the split cannot come back.
    static void alphaContract() {
        ckH(rgb(4, 4).getRGB(0, 0), 0xFF000000, "pristine opaque pixel carries alpha");
        ckH(new BufferedImage(4, 4, BufferedImage.TYPE_INT_ARGB).getRGB(0, 0), 0,
                "a pristine ARGB pixel is fully transparent");

        BufferedImage b = rgb(4, 4);
        b.setRGB(2, 3, 0x00FF7F10);
        ckH(b.getRGB(2, 3), 0xFFFF7F10, "setRGB then getRGB on an opaque image forces alpha");

        BufferedImage c = rgb(8, 8);
        Graphics2D g = c.createGraphics();
        g.setColor(new Color(0x20, 0x40, 0x60));
        g.fillRect(2, 2, 4, 4);
        g.dispose();
        ckH(c.getRGB(3, 3), 0xFF204060, "a filled pixel carries alpha");
        ckH(c.getRGB(0, 0), 0xFF000000, "an UNFILLED pixel of a drawn-on image carries alpha");
        System.out.println("CK RJdkAwtHeadless alpha=" + Integer.toHexString(c.getRGB(3, 3)));
    }

    // ---- fill geometry: inclusive origin, exclusive extent ------------------
    static void fills() {
        BufferedImage b = rgb(8, 8);
        Graphics2D g = b.createGraphics();
        g.setColor(Color.WHITE);
        g.fillRect(2, 2, 4, 4);
        g.dispose();
        ckH(b.getRGB(2, 2), 0xFFFFFFFF, "fillRect includes its origin");
        ckH(b.getRGB(5, 5), 0xFFFFFFFF, "fillRect includes origin+extent-1");
        ckH(b.getRGB(6, 6), 0xFF000000, "fillRect EXCLUDES origin+extent");

        BufferedImage c = rgb(4, 4);
        Graphics2D g2 = c.createGraphics();
        g2.setColor(Color.WHITE);
        g2.fillRect(0, 0, 4, 4);
        g2.setBackground(Color.BLACK);
        g2.clearRect(1, 1, 2, 2);
        g2.dispose();
        ckH(c.getRGB(1, 1), 0xFF000000, "clearRect paints the BACKGROUND colour");
        ckH(c.getRGB(0, 0), 0xFFFFFFFF, "clearRect leaves pixels outside it alone");
        System.out.println("CK RJdkAwtHeadless fills=ok");
    }

    // ---- clip ---------------------------------------------------------------
    static void clip() {
        BufferedImage b = rgb(8, 8);
        Graphics2D g = b.createGraphics();
        g.setClip(0, 0, 2, 2);
        g.setColor(Color.WHITE);
        g.fillRect(0, 0, 8, 8);
        g.dispose();
        ckH(b.getRGB(1, 1), 0xFFFFFFFF, "a fill inside the clip lands");
        ckH(b.getRGB(5, 5), 0xFF000000, "a fill outside the clip is blocked");

        Graphics2D g2 = rgb(8, 8).createGraphics();
        g2.setClip(1, 2, 3, 4);
        Rectangle r = g2.getClipBounds();
        g2.dispose();
        check(r.x == 1 && r.y == 2 && r.width == 3 && r.height == 4,
                "getClipBounds round-trips setClip, got " + r.x + "," + r.y + ","
                        + r.width + "," + r.height);
        System.out.println("CK RJdkAwtHeadless clip=ok");
    }

    // ---- colour state on the Graphics ---------------------------------------
    // getColor and setBackground were NOT REGISTERED at all before G80-1, and
    // because java.awt.Graphics is abstract the call raised AbstractMethodError
    // rather than falling through to anything. Each sat directly beside its own
    // other half (setColor above getColor; clearRect beside setBackground),
    // which is the arrangement that hides a missing method from a reader.
    static void colourState() {
        Graphics2D g = rgb(4, 4).createGraphics();
        g.setColor(new Color(1, 2, 3));
        Color c = g.getColor();
        check(c.getRed() == 1 && c.getGreen() == 2 && c.getBlue() == 3,
                "getColor returns what setColor was given, got " + c.getRed() + ","
                        + c.getGreen() + "," + c.getBlue());
        g.setBackground(Color.WHITE);
        ckH(g.getBackground().getRGB(), 0xFFFFFFFF, "getBackground round-trips setBackground");
        g.dispose();

        ckH(new Color(0x20, 0x40, 0x60).getRGB(), 0xFF204060, "Color.getRGB packs opaque ARGB");
        ckH(Color.WHITE.getRGB(), 0xFFFFFFFF, "Color.WHITE");
        check(new Color(1, 2, 3).equals(new Color(1, 2, 3)), "Color equality is by value");
        System.out.println("CK RJdkAwtHeadless colour=ok");
    }

    // ---- bounds --------------------------------------------------------------
    // The JDK reports no index here: BufferedImage.getRGB bottoms out in the
    // raster's own check, which throws ArrayIndexOutOfBoundsException with the
    // message "Coordinate out of bounds!". G80-1 found an invented message next
    // to a comment asserting the opposite.
    static void bounds() {
        try {
            rgb(4, 4).getRGB(9, 9);
            check(false, "getRGB out of bounds must throw");
        } catch (ArrayIndexOutOfBoundsException e) {
            check("Coordinate out of bounds!".equals(e.getMessage()),
                    "getRGB OOB message = " + e.getMessage());
        }
        try {
            rgb(4, 4).getRGB(-1, 0);
            check(false, "getRGB negative must throw");
        } catch (ArrayIndexOutOfBoundsException e) {
            check("Coordinate out of bounds!".equals(e.getMessage()),
                    "getRGB negative message = " + e.getMessage());
        }
        System.out.println("CK RJdkAwtHeadless bounds=ok");
    }

    // ---- ImageIO -------------------------------------------------------------
    static void imageio() throws Exception {
        BufferedImage b = rgb(5, 4);
        Graphics2D g = b.createGraphics();
        g.setColor(new Color(0x11, 0x22, 0x33));
        g.fillRect(0, 0, 5, 4);
        g.dispose();

        ByteArrayOutputStream out = new ByteArrayOutputStream();
        check(ImageIO.write(b, "png", out), "ImageIO.write(png) reports a writer was found");
        byte[] png = out.toByteArray();

        // The PNG signature, which is a format constant and not a rendering choice.
        int[] magic = {0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A};
        boolean sig = png.length > 8;
        for (int i = 0; sig && i < 8; i++) {
            sig = (png[i] & 0xFF) == magic[i];
        }
        check(sig, "the written bytes carry the PNG signature");

        BufferedImage back = ImageIO.read(new ByteArrayInputStream(png));
        ckI(back.getWidth(), 5, "round-tripped width");
        ckI(back.getHeight(), 4, "round-tripped height");
        ckH(back.getRGB(1, 1), 0xFF112233, "round-tripped pixel");

        check(!ImageIO.write(b, "no-such-format", new ByteArrayOutputStream()),
                "an unknown format reports false rather than throwing");
        System.out.println("CK RJdkAwtHeadless imageio=ok");
    }

    public static void main(String[] args) throws Exception {
        System.setProperty("java.awt.headless", "true");
        check(GraphicsEnvironment.isHeadless(), "the vector must run headless");

        geometry();
        alphaContract();
        fills();
        clip();
        colourState();
        bounds();
        imageio();

        System.out.println("CK RJdkAwtHeadless checks=" + checks);
        System.out.println("PASS RJdkAwtHeadless (" + checks + " checks)");
    }
}
