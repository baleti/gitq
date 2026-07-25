//! GitQ — a typed query language for git.
//!
//! One query language over git's object graph: describe the data you want by
//! composing small, typed operations, and end the pipeline with what should
//! happen to the result.
//!
//! Port of the Haskell implementation (itself a port of the original Emacs
//! Lisp in `git-branch-off`).  Module layout mirrors the Haskell one so the
//! two can be diffed during the port.

pub mod ast;
pub mod frame;
pub mod parse;
pub mod registry;
pub mod tokenize;
