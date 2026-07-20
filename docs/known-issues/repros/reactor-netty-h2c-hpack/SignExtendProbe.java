import io.netty.buffer.ByteBuf;
import io.netty.buffer.UnpooledByteBufAllocator;

public class SignExtendProbe {
    public static void main(String[] args) {
        byte lit = (byte) 0x83;
        System.out.println("literal byte 0x83 < 0 ? " + (lit < 0) + "  (expected true)");
        System.out.println("literal byte 0x83 as int widened = " + (int) lit + " (expected -125)");

        byte[] arr = { (byte) 0x83 };
        byte fromArr = arr[0];
        System.out.println("array-sourced byte 0x83 < 0 ? " + (fromArr < 0) + "  (expected true)");

        ByteBuf buf = UnpooledByteBufAllocator.DEFAULT.directBuffer(16, 16);
        buf.writeByte(0x83);
        byte fromDirect = buf.readByte();
        System.out.println("direct-ByteBuf-sourced byte 0x83 < 0 ? " + (fromDirect < 0)
                + "  (expected true) int-widened=" + (int) fromDirect);

        ByteBuf outer = UnpooledByteBufAllocator.DEFAULT.directBuffer(64, 64);
        for (int i = 0; i < 9; i++) outer.writeByte(0);
        outer.writeByte(0x83);
        for (int i = 0; i < 10; i++) outer.writeByte(0);
        outer.readBytes(9);
        ByteBuf slice = outer.readSlice(11);
        byte fromSlice = slice.readByte();
        System.out.println("SLICED-ByteBuf-sourced byte 0x83 < 0 ? " + (fromSlice < 0)
                + "  (expected true) int-widened=" + (int) fromSlice);

        buf.release();
        outer.release();
    }
}
