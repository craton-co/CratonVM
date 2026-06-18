import java.lang.reflect.Field;
public class Probe {
    public static void main(String[] a) throws Exception {
        Runnable r = () -> {};
        Thread t = new Thread(r, "probe");
        Field hf = Thread.class.getDeclaredField("holder");
        hf.setAccessible(true);
        Object holder = hf.get(t);
        System.out.println("holder=" + (holder==null?"NULL":holder.getClass().getName()));
        if (holder != null) {
            Field tf = holder.getClass().getDeclaredField("task");
            tf.setAccessible(true);
            Object task = tf.get(holder);
            System.out.println("holder.task=" + (task==null?"NULL":task.getClass().getName()));
            System.out.println("same-as-r=" + (task==r));
        }
    }
}
