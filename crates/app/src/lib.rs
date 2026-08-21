//! file-explorer application library.
//!
//! Exposes the workspace view and theme so both the app binary and the
//! visual test runner can render identical windows.

pub mod theme;
pub mod visual_diff;
pub mod workspace_view;

pub use theme::Theme;
pub use workspace_view::WorkspaceView;
