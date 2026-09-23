/**
 * Force-load every class `native-awt` registers against, so that a
 * `--dump-native-registry` taken from this run has its `real_declaring_method`
 * census fields populated.
 *
 * The P0 "wholesale Bridge over-tagging" row wants a census before any retag
 * (jdk-only-native-review §7). The registry dump already carries the census
 * fields — `loaded` / `declared` / `acc_native` / `has_code` per registration —
 * but they are only meaningful for classes the run actually loaded, and no
 * existing vector touches AWT. This does nothing but load them.
 *
 * `Class.forName(name, false, loader)` deliberately does NOT initialise: the
 * census needs the class's METHOD TABLE, not its static state, and running
 * AWT clinits headless is exactly the kind of side effect a census must avoid.
 */
public class AwtCategoryCensus {
    static final String[] CLASSES = {
        "com/sun/imageio/plugins/jpeg/JPEGImageReader",
        "com/sun/imageio/plugins/jpeg/JPEGImageWriter",
        "com/sun/imageio/plugins/png/PNGImageWriter",
        "java/awt/Component",
        "java/awt/EventQueue",
        "java/awt/Font",
        "java/awt/FontMetrics",
        "java/awt/Frame",
        "java/awt/Graphics",
        "java/awt/Graphics2D",
        "java/awt/GraphicsEnvironment",
        "java/awt/Toolkit",
        "java/awt/datatransfer/Clipboard",
        "java/awt/event/InvocationEvent",
        "java/awt/image/BufferedImage",
        "java/awt/image/ColorModel",
        "java/awt/image/ComponentSampleModel",
        "java/awt/image/IndexColorModel",
        "java/awt/image/Kernel",
        "java/awt/image/Raster",
        "java/awt/image/SampleModel",
        "java/awt/image/SinglePixelPackedSampleModel",
        "javax/imageio/ImageIO",
        "javax/swing/JFileChooser",
        "javax/swing/JOptionPane",
        "javax/swing/SwingUtilities",
        "javax/swing/UIManager",
        "sun/awt/PlatformGraphicsInfo",
        "sun/awt/SunToolkit",
        "sun/awt/image/ByteComponentRaster",
        "sun/awt/image/BytePackedRaster",
        "sun/awt/image/GifImageDecoder",
        "sun/awt/image/IntegerComponentRaster",
        "sun/awt/image/ShortComponentRaster",
        "sun/java2d/Disposer",
        "sun/java2d/SunGraphics2D",
    };

    public static void main(String[] a) {
        int ok = 0;
        int missing = 0;
        ClassLoader cl = AwtCategoryCensus.class.getClassLoader();
        for (String c : CLASSES) {
            String dotted = c.replace('/', '.');
            try {
                Class.forName(dotted, false, cl);
                ok++;
            } catch (Throwable t) {
                missing++;
                System.out.println("MISSING " + dotted + " : " + t.getClass().getName());
            }
        }
        System.out.println("CENSUS loaded=" + ok + " missing=" + missing
                + " of " + CLASSES.length);
    }
}
