import java.io.*;
import java.lang.management.*;
import java.util.*;
import javax.management.*;

/** SBR2 W8 SMALL: BufferedOutputStream.close, FileDescriptor set, StackTraceElement.toString formats, MBean introspection. */
public class Sbr2W8SmallMisc {
    static void row(String n, Object v) { System.out.println(n + " = " + v); }
    interface T { Object run() throws Throwable; }
    static void t(String n, T r) { try { row(n, r.run()); } catch (Throwable e) { row(n, "EX " + e.getClass().getName() + ": " + e.getMessage()); } }

    public interface FooMBean { int getA(); void setA(int a); String op(String s); boolean isOn(); }
    public static class Foo implements FooMBean { int a = 5; public int getA() { return a; } public void setA(int x) { a = x; } public String op(String s) { return s + "!"; } public boolean isOn() { return true; } }
    public interface BarMXBean { java.util.List<String> getNames(); Map<String,Integer> getMap(); }
    public static class Bar implements BarMXBean { public List<String> getNames() { return List.of("x","y"); } public Map<String,Integer> getMap() { return Map.of("k", 1); } }

    static class Rec extends ByteArrayOutputStream { int closes; boolean flushed; public void close() throws IOException { closes++; super.close(); } public void flush() { flushed = true; } }
    static class FailFlush extends OutputStream { public void write(int b) { } public void flush() throws IOException { throw new IOException("flush-boom"); } public void close() { System.out.println("inner closed"); } }
    static class FailClose extends OutputStream { public void write(int b) { } public void close() throws IOException { throw new IOException("close-boom"); } }

    static Object st(Throwable e) { StringBuilder sb = new StringBuilder(); for (var f : e.getStackTrace()) { if (f.getClassName().startsWith("Sbr2W8SmallMisc")) sb.append(f.getClassName()).append('|').append(f.getMethodName()).append('|').append(f.getFileName()).append('|').append(f.getModuleName()).append('|').append(f.getClassLoaderName()).append('|').append(f.isNativeMethod()).append(' '); } return sb; }

    public static void main(String[] a) throws Throwable {
        t("bos.close", () -> { var r = new Rec(); var b = new BufferedOutputStream(r, 16); b.write(1); b.close(); return r.closes + "," + r.flushed + "," + r.size(); });
        t("bos.close twice", () -> { var r = new Rec(); var b = new BufferedOutputStream(r); b.close(); b.close(); return r.closes; });
        t("bos.close flush fails", () -> { var b = new BufferedOutputStream(new FailFlush()); b.write(1); b.close(); return "ok"; });
        t("bos.close close fails", () -> { var b = new BufferedOutputStream(new FailClose()); b.write(1); b.close(); return "ok"; });
        t("bos.write after close", () -> { var b = new BufferedOutputStream(new Rec(), 4); b.close(); b.write(1); b.flush(); return "ok"; });
        t("bos.file roundtrip", () -> { File f = File.createTempFile("w8s", ".bin"); try (var o = new BufferedOutputStream(new FileOutputStream(f))) { o.write(new byte[]{1,2,3}); } long n = f.length(); f.delete(); return n; });
        t("bos.closed file", () -> { File f = File.createTempFile("w8s", ".bin"); var fos = new FileOutputStream(f); var o = new BufferedOutputStream(fos); o.close(); try { fos.write(1); return "wrote"; } catch (IOException e) { return e.getMessage(); } finally { f.delete(); } });
        t("fis.skip", () -> { var f = new FilterInputStream(new ByteArrayInputStream(new byte[10])) { }; return f.skip(3) + "," + f.skip(100) + "," + f.skip(5) + "," + f.skip(-1) + "," + f.skip(0) + "," + f.read(); });
        t("fis.skip closed", () -> { var b = new ByteArrayInputStream(new byte[4]); var f = new FilterInputStream(b) { }; f.close(); return f.skip(2); });
        t("fis.skip throws", () -> { var f = new FilterInputStream(new InputStream() { public int read() throws IOException { throw new IOException("rd"); } public long skip(long n) throws IOException { throw new IOException("sk" + n); } }) { }; return f.skip(7); });
        t("fis.skip null in", () -> { var f = new FilterInputStream(null) { }; return f.skip(1); });
        t("fis.skip buffered", () -> { var f = new BufferedInputStream(new ByteArrayInputStream(new byte[20]), 4); f.read(); return f.skip(9) + "," + f.available(); });
        t("fis.skip datain", () -> { var f = new DataInputStream(new ByteArrayInputStream(new byte[20])); return f.skip(6) + "," + f.skipBytes(3); });
        t("fis.skip file", () -> { File t = File.createTempFile("w8s", ".bin"); try { try (var o = new FileOutputStream(t)) { o.write(new byte[50]); } try (var f = new FilterInputStream(new FileInputStream(t)) { }) { return f.skip(20) + "," + f.skip(100); } } finally { t.delete(); } });
        t("fd.std", () -> FileDescriptor.out.valid() + "," + FileDescriptor.err.valid() + "," + FileDescriptor.in.valid());
        t("fd.new", () -> { var fd = new FileDescriptor(); return fd.valid(); });
        t("fd.file", () -> { File f = File.createTempFile("w8s", ".bin"); try (var o = new FileOutputStream(f); var i = new FileInputStream(f)) { var fd = o.getFD(); boolean v = fd.valid(); fd.sync(); return v; } finally { f.delete(); } });
        t("fd.raf", () -> { File f = File.createTempFile("w8s", ".bin"); try (var r = new RandomAccessFile(f, "rw")) { r.write(7); r.seek(0); return r.read() + "," + r.getFD().valid(); } finally { f.delete(); } });
        t("fd.stdout write", () -> { var o = new FileOutputStream(FileDescriptor.out); o.write("fdout\n".getBytes()); o.flush(); return "ok"; });
        t("fd.closed write", () -> { File f = File.createTempFile("w8s", ".bin"); var o = new FileOutputStream(f); o.close(); try { o.write(1); return "wrote"; } catch (IOException e) { return e.getMessage(); } finally { f.delete(); } });
        t("fd.set pipe", () -> { var p = java.nio.channels.Pipe.open(); p.sink().write(java.nio.ByteBuffer.wrap(new byte[]{4,5})); var bb = java.nio.ByteBuffer.allocate(4); int n = p.source().read(bb); p.sink().close(); p.source().close(); return n + "," + bb.get(0); });
        t("fd.set socket", () -> { try (var ss = java.nio.channels.ServerSocketChannel.open()) { ss.bind(new java.net.InetSocketAddress(java.net.InetAddress.getLoopbackAddress(), 0)); try (var c = java.nio.channels.SocketChannel.open(ss.getLocalAddress()); var s = ss.accept()) { c.write(java.nio.ByteBuffer.wrap(new byte[]{9})); var bb = java.nio.ByteBuffer.allocate(2); int n = s.read(bb); return n + "," + bb.get(0) + "," + c.isConnected(); } } });
        t("fd.set classic socket", () -> { try (var ss = new java.net.ServerSocket(0, 1, java.net.InetAddress.getLoopbackAddress())) { try (var c = new java.net.Socket(java.net.InetAddress.getLoopbackAddress(), ss.getLocalPort()); var s = ss.accept()) { c.getOutputStream().write(3); return s.getInputStream().read(); } } });
        t("fd.set selector", () -> { try (var sel = java.nio.channels.Selector.open(); var ss = java.nio.channels.ServerSocketChannel.open()) { ss.bind(new java.net.InetSocketAddress(java.net.InetAddress.getLoopbackAddress(), 0)); ss.configureBlocking(false); ss.register(sel, java.nio.channels.SelectionKey.OP_ACCEPT); return sel.selectNow(); } });
        t("ste.toString app", () -> { try { throw new RuntimeException("x"); } catch (Exception e) { return e.getStackTrace()[0] + " :: " + st(e); } });
        t("ste.toString jdk", () -> { try { Object o = null; o.hashCode(); return "none"; } catch (Exception e) { return st(e); } });
        t("ste.jdk frames", () -> { try { List.of(1).get(3); return "none"; } catch (Exception e) { var f = e.getStackTrace()[0]; return f + " | " + f.getModuleName() + " | " + f.getModuleVersion() + " | " + f.getClassLoaderName(); } });
        t("ste.deep jdk", () -> { try { Integer.parseInt("zz"); return "none"; } catch (Exception e) { StringBuilder sb = new StringBuilder(); for (int i = 0; i < 2; i++) { var f = e.getStackTrace()[i]; sb.append(f).append(" / ").append(f.getModuleName()).append(" / ").append(f.getModuleVersion() != null).append(" ; "); } return sb; } });
        t("ste.ctor", () -> { var e = new StackTraceElement("com.x.Y", "m", "Y.java", 12); return e + " | " + e.getModuleName() + " | " + e.getClassLoaderName(); });
        t("ste.ctor full", () -> { var e = new StackTraceElement("app", "mod", "1.2", "com.x.Y", "m", "Y.java", 12); return e + " | " + e.getModuleVersion(); });
        t("ste.ctor full jdk", () -> new StackTraceElement(null, "java.base", "25", "java.util.X", "m", "X.java", -2));
        t("ste.ctor full loaderNoModule", () -> new StackTraceElement("app", null, null, "a.B", "m", null, -1));
        t("ste.ctor builtin loader", () -> new StackTraceElement("platform", "java.sql", "25.0.3", "a.B", "m", "B.java", 3) + " / " + new StackTraceElement("app", "m.x", "1.0", "a.B", "m", "B.java", 3));
        t("ste.equals", () -> { var x = new StackTraceElement("a.B", "m", "B.java", 1); return x.equals(new StackTraceElement("a.B", "m", "B.java", 1)) + "," + (x.hashCode() == new StackTraceElement("a.B", "m", "B.java", 1).hashCode()); });
        t("ste.thread dump", () -> { StringBuilder sb = new StringBuilder(); for (var f : Thread.currentThread().getStackTrace()) sb.append(f.getMethodName()).append(' '); return sb; });
        t("ste.stackwalker", () -> StackWalker.getInstance().walk(s -> s.map(f -> f.toStackTraceElement().toString()).limit(2).toList()));
        t("ste.printStackTrace", () -> { var sw = new StringWriter(); new RuntimeException("pp").printStackTrace(new PrintWriter(sw)); return sw.toString().lines().limit(3).toList(); });
        t("ste.lambda", () -> { Runnable r = () -> { throw new IllegalStateException("lam"); }; try { r.run(); return "none"; } catch (Exception e) { return st(e); } });
        t("mbean.standard", () -> { var s = MBeanServerFactory.newMBeanServer(); var n = new ObjectName("w8s:type=Foo"); s.registerMBean(new Foo(), n); var i = s.getMBeanInfo(n); var at = new TreeSet<String>(); for (var x : i.getAttributes()) at.add(x.getName() + ":" + x.getType() + ":" + x.isReadable() + x.isWritable() + x.isIs()); var op = new TreeSet<String>(); for (var x : i.getOperations()) op.add(x.getName()); s.setAttribute(n, new Attribute("A", 9)); return at + " " + op + " " + s.getAttribute(n, "A") + " " + s.invoke(n, "op", new Object[]{"q"}, new String[]{"java.lang.String"}) + " " + i.getConstructors().length; });
        t("mbean.mx", () -> { var s = MBeanServerFactory.newMBeanServer(); var n = new ObjectName("w8s:type=Bar"); s.registerMBean(new Bar(), n); var i = s.getMBeanInfo(n); var at = new TreeSet<String>(); for (var x : i.getAttributes()) at.add(x.getName() + ":" + x.getType()); return at + " " + s.getAttribute(n, "Names").getClass().getSimpleName() + " " + (s.getAttribute(n, "Map") instanceof javax.management.openmbean.TabularData); });
        t("mbean.notcompliant", () -> { var s = MBeanServerFactory.newMBeanServer(); try { s.registerMBean(new Object(), new ObjectName("w8s:type=Bad")); return "reg"; } catch (NotCompliantMBeanException e) { return "NCME " + e.getMessage(); } });
        t("mbean.std wrapper", () -> { var m = new StandardMBean(new Foo(), FooMBean.class); return m.getMBeanInfo().getAttributes().length + "," + m.getAttribute("A"); });
        t("mbean.mx wrapper", () -> { var m = new StandardMBean(new Bar(), BarMXBean.class, true); return m.getMBeanInfo().getAttributes().length; });
        t("mbean.platform", () -> { var s = ManagementFactory.getPlatformMBeanServer(); var n = new ObjectName("java.lang:type=Runtime"); var at = s.getMBeanInfo(n).getAttributes().length > 10; return at + "," + (s.getAttribute(n, "VmName") != null) + "," + (s.getAttribute(new ObjectName("java.lang:type=Memory"), "HeapMemoryUsage") instanceof javax.management.openmbean.CompositeData); });
        t("mbean.objmethods", () -> { var s = MBeanServerFactory.newMBeanServer(); var n = new ObjectName("w8s:type=Foo"); s.registerMBean(new Foo(), n); var ops = new TreeSet<String>(); for (var x : s.getMBeanInfo(n).getOperations()) ops.add(x.getName()); return ops.contains("hashCode") + "," + ops.contains("toString") + "," + ops.contains("getClass"); });
    }
}
