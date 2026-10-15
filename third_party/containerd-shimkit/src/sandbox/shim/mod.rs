//! The task service (`Local`) and the `containerd_shim::Shim` that serves it.

mod events;
mod exec;
mod instance_data;
mod local;
#[allow(clippy::module_inception)]
mod shim;
mod task_state;
pub(crate) use shim::Shim;
