//! Minimal in-house modal dialogs (ARCHITECTURE.md §8 "Dialogs"): the
//! workspace holds `modal: Option<…>` rendered as a `deferred` overlay +
//! scrim — no generic dialog framework. [`confirm::ConfirmDialog`] guards
//! destructive actions (delete permanently); [`conflict::ConflictDialog`] is
//! the Explorer-style Replace / Skip / Keep both / Apply-to-all prompt for a
//! parked [`fs_core::JobEvent::NeedsDecision`].

pub mod confirm;
pub mod conflict;

pub use confirm::{ConfirmDialog, ConfirmDialogEvent};
pub use conflict::{ConflictDialog, ConflictDialogEvent};
