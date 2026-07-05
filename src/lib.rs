//! Manifest OS — the install engine, shared by the `manifest` CLI, the Ratatui
//! TUI, and the GTK GUI front-ends. Each front-end collects a
//! [`probe::InstallPlan`] and calls [`installer::execute`]; everything else here
//! is the orchestration of standard Arch tools (pacman, paru, systemctl, …).

pub mod boot;
pub mod desktop;
pub mod diff;
pub mod dotfiles;
pub mod exec;
pub mod export;
pub mod files;
pub mod history;
pub mod install;
pub mod installer;
pub mod keybindings;
pub mod kernel;
pub mod manifest;
pub mod pacman;
pub mod probe;
pub mod segment;
pub mod snippets;
pub mod survey;
pub mod system;
pub mod theming;
pub mod tui;
pub mod users;
pub mod wallpaper;
