//! Provider-neutral kernel for slowcode, vendored from dsh-rs.
//!
//! The agent loop, provider, model, session, and tools are upstream code. The
//! `capability` module is the slowcode seam filled by role fibers.

#![deny(unsafe_code)]

// The upstream UI layers used many crate-private helpers that the kernel does
// not consume. Keep their warnings quiet without hiding warnings in the
// slowcode-owned `capability` module.
#[allow(dead_code, unused_imports)]
mod attachment;
#[allow(dead_code, unused_imports)]
mod entropy;
#[allow(dead_code, unused_imports)]
mod goal;
#[allow(dead_code, unused_imports)]
mod json_value;
#[allow(dead_code, unused_imports)]
mod plan_mode;
#[allow(dead_code, unused_imports)]
mod resident_credit;
#[allow(dead_code, unused_imports)]
pub mod skills;
#[allow(dead_code, unused_imports)]
mod time_context;
#[allow(dead_code, unused_imports)]
mod user_question;
#[allow(dead_code, unused_imports)]
mod workspace_authority;
#[allow(dead_code, unused_imports)]
pub mod workspace_instructions;

#[allow(dead_code, unused_imports)]
pub mod agent;
pub mod capability;
#[allow(dead_code, unused_imports)]
pub mod model;
#[allow(dead_code, unused_imports)]
pub mod provider;
#[allow(dead_code, unused_imports)]
pub mod session;
pub mod skill_roots;
#[allow(dead_code, unused_imports)]
pub mod tools;
pub mod ui;
