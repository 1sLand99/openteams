//! Typed prompt builders for workflow runtime scenarios.
//!
//! Design: `.openteams/specs/2026-08-05-workflow-prompt-builders-design.md`.
//! Each builder renders a fixed-section Markdown prompt plus the single JSON
//! Schema allowed for the scenario, following the cache-friendly layout in
//! §7.1 of the design.

pub mod common;
pub mod plan_generation;
pub mod step_review;
pub mod task_execution;
