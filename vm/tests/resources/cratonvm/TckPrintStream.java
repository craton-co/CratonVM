package cratonvm;
import java.io.*;
public class TckPrintStream {
    public static int baos_write_toByteArray() {
        ByteArrayOutputStream baos = new ByteArrayOutputStream();
        baos.write(65); baos.write(66); baos.write(67);
        byte[] out = baos.toByteArray();
        return (out.length == 3 && out[0] == 65 && out[1] == 66 && out[2] == 67) ? 1 : 0;
    }
    public static int baos_size() {
        ByteArrayOutputStream baos = new ByteArrayOutputStream();
        baos.write(1); baos.write(2);
        return baos.size() == 2 ? 1 : 0;
    }
    public static int baos_reset() {
        ByteArrayOutputStream baos = new ByteArrayOutputStream();
        baos.write(1);
        baos.reset();
        return baos.size() == 0 ? 1 : 0;
    }
    public static int baos_toString() {
        try {
            ByteArrayOutputStream baos = new ByteArrayOutputStream();
            baos.write("hello".getBytes("UTF-8"));
            return "hello".equals(baos.toString("UTF-8")) ? 1 : 0;
        } catch (Exception e) { return 0; }
    }
    public static int printstream_print() {
        ByteArrayOutputStream baos = new ByteArrayOutputStream();
        PrintStream ps = new PrintStream(baos);
        ps.print("hello");
        ps.flush();
        return baos.toString().equals("hello") ? 1 : 0;
    }
    public static int printstream_println() {
        ByteArrayOutputStream baos = new ByteArrayOutputStream();
        PrintStream ps = new PrintStream(baos);
        ps.println("hello");
        ps.flush();
        String result = baos.toString();
        return result.startsWith("hello") && result.length() > 5 ? 1 : 0;
    }
    public static int printwriter_basic() {
        StringWriter sw = new StringWriter();
        PrintWriter pw = new PrintWriter(sw);
        pw.print("test");
        pw.flush();
        return "test".equals(sw.toString()) ? 1 : 0;
    }
    public static int printwriter_println() {
        StringWriter sw = new StringWriter();
        PrintWriter pw = new PrintWriter(sw);
        pw.println("line");
        pw.flush();
        return sw.toString().startsWith("line") ? 1 : 0;
    }
    public static int data_io_int() {
        try {
            ByteArrayOutputStream baos = new ByteArrayOutputStream();
            DataOutputStream dos = new DataOutputStream(baos);
            dos.writeInt(12345);
            dos.flush();
            DataInputStream dis = new DataInputStream(new ByteArrayInputStream(baos.toByteArray()));
            return dis.readInt() == 12345 ? 1 : 0;
        } catch (Exception e) { return 0; }
    }
    public static int data_io_utf() {
        try {
            ByteArrayOutputStream baos = new ByteArrayOutputStream();
            DataOutputStream dos = new DataOutputStream(baos);
            dos.writeUTF("hello");
            dos.flush();
            DataInputStream dis = new DataInputStream(new ByteArrayInputStream(baos.toByteArray()));
            return "hello".equals(dis.readUTF()) ? 1 : 0;
        } catch (Exception e) { return 0; }
    }
}
