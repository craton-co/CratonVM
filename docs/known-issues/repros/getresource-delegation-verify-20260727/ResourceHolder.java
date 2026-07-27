package probe.lib;

public class ResourceHolder {
    public static String readOwnResource() throws Exception {
        java.io.InputStream in = ResourceHolder.class.getResourceAsStream("data.txt");
        if (in == null) {
            return null;
        }
        java.io.ByteArrayOutputStream bos = new java.io.ByteArrayOutputStream();
        byte[] buf = new byte[256];
        int n;
        while ((n = in.read(buf)) != -1) {
            bos.write(buf, 0, n);
        }
        in.close();
        return bos.toString("UTF-8").trim();
    }

    public static String resourceUrl() {
        java.net.URL u = ResourceHolder.class.getResource("data.txt");
        return u == null ? null : u.toString();
    }
}
