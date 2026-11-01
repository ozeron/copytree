//! Copytree: fast git worktrees for parallel coding agents.
//!
//! Each workspace is a normal linked git worktree. Copytree adds what
//! worktrees lack: dependencies installed from the package manager's shared
//! store, build caches cloned with copy-on-write, `.env` carried over, and a
//! saved ref before anything is deleted.
//!
//! The library is the same code the CLI runs, so agent orchestrators can embed
//! it: see [`workspace::new`], [`workspace::list`] and [`workspace::rm`].

pub mod cli;
pub mod clone;
pub mod config;
pub mod deps;
pub mod git;
pub mod hook;
pub mod seed;
pub mod state;
pub mod workspace;
