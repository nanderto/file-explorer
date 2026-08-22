//! file-explorer application library.
//!
//! Exposes the workspace, panes, actions, keymap, and theme so both the app
//! binary and the visual test runner can boot identical windows.

pub mod actions;
pub mod address_bar;
pub mod app_state;
pub mod dir_view;
pub mod input;
pub mod keymap;
pub mod pane;
pub mod settings;
pub mod sidebar;
pub mod theme;
pub mod views;
pub mod visual_diff;
pub mod workspace;

pub use theme::Theme;
pub use workspace::Workspace;
