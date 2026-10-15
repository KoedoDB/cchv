//! A small library for building containerd shims whose containers run elsewhere (here: in a VM). See KOEDODB.md.

pub mod sandbox;
pub(crate) mod sys;

pub use containerd_shim::Config;
pub use sandbox::async_utils::AmbientRuntime;
