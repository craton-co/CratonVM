package craton.gpu;

/** Selects the grid/launch shape the GPU lowering should target for a kernel. */
public enum GridShape { ELEMENTWISE, ROW_PER_THREAD, BLOCK_REDUCTION }
