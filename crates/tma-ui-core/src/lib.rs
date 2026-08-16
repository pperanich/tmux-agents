//! tmux-agents UI core: the Elm-style pure fold shared by the picker and `watch` surfaces. Two folds
//! (`PickerModel`/`WatchModel`, each an `update(Event, now, res) -> Vec<Effect>`) over the
//! `Key`/`Event`/`Effect` alphabet, backed by `Selection`, the `common` module (the refresh gate,
//! the preview cache, and the arms both folds share) and the `render` row-format helpers. Each model
//! keeps its fields private, so the shell can only drive it by event. Purity contract: the crate
//! depends on neither
//! crossterm nor tma-runtime, so no terminal input and no `Tmux` handle can reach the core — the
//! folds perform no I/O, now compiler-enforced rather than by signature discipline plus a grep
//! gate; the shell executes the `Effect`s and feeds captured/refreshed data back as events.
#![deny(rustdoc::broken_intra_doc_links)]

mod ansi;
mod common;
mod effect;
mod event;
mod group;
mod key;
pub mod palette;
pub mod picker;
mod preview;
mod refresh_gate;
pub mod render;
mod selection;
pub mod watch;

pub use common::PREVIEW_MIN_WIDTH;
pub use effect::Effect;
pub use event::Event;
pub use key::Key;
pub use palette::RowPalette;
pub use picker::PickerModel;
pub use refresh_gate::RefreshGate;
pub use watch::WatchModel;
