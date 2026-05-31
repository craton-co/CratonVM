import java.nio.charset.StandardCharsets;
public class Surr {
    public static void main(String[] a){
        String s = new String(new char[]{0xD801,0xDC01}); // U+10401
        byte[] b = s.getBytes(StandardCharsets.UTF_8);
        StringBuilder sb = new StringBuilder();
        for(byte x: b) sb.append(String.format("%%%02x", x & 0xff));
        System.out.println("UTF-8 of U+10401 = "+sb+" (expect %f0%90%90%81)");
        // also � with US-ASCII
        byte[] c = "�".getBytes(StandardCharsets.US_ASCII);
        StringBuilder sb2 = new StringBuilder();
        for(byte x: c) sb2.append(String.format("%02x ", x & 0xff));
        System.out.println("US-ASCII of U+FFFD = "+sb2+"(HotSpot: 3f = '?')");
    }
}
