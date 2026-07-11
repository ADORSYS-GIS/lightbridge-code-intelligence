//! Bootstrap — how a Job comes up before it does any repository work: load its [`config`] and open
//! the shared client to the control plane (which hands back the task context plus a short-lived
//! installation token). The client is the only standing credential the runner holds.

pub mod config;
