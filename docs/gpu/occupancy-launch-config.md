# Occupancy-tuned GPU launch configuration

GPU dispatch uses the occupancy recommendation supplied by
`cuOccupancyMaxPotentialBlockSize`, via
`cuda-bridge/src/backend_cuda.rs::DeviceModule::elementwise_for_kernel`.

This replaces the former fixed 256-thread `LaunchConfig::elementwise` launch
shape. The runtime still owns work sizing; occupancy tuning selects the block
configuration used to cover that work efficiently on the active device.

See also the historical validation record.
