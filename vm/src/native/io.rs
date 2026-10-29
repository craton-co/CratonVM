// SPDX-License-Identifier: Apache-2.0
// Copyright 2024-2026 Craton Software Company

// Re-exported from cratonvm-native-api (FileDescriptorTable) and cratonvm-native-io (register_io_natives).
// All crate::native::io::FileDescriptorTable and crate::native::io::register_io_natives paths work.
pub use cratonvm_native_api::fd_table::{FdId, FileDescriptorTable};
pub use cratonvm_native_io::register_io_natives;
