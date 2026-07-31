//! file: safe-Rust implementation. Entry points live in `crate::ffi`.
//!
//! Nothing here may use `unsafe`; pointer handling stays in the FFI shim so
//! the port's unsafe surface is small and countable.
#![forbid(unsafe_code)]
