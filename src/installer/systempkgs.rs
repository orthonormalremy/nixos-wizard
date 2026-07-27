use std::{
  collections::HashSet,
  process::Command,
  sync::{
    LazyLock,
    atomic::{AtomicU64, Ordering},
  },
};

use ratatui::{layout::Constraint, text::Line};
use serde_json::Value;

use crate::{
  installer::{Installer, Page, Signal},
  styled_block,
  widget::{ConfigWidget, PackagePicker, TableWidget},
};

use std::{
  sync::{Arc, RwLock},
  thread,
};

/// Progress of the background package-list fetch
///
/// Fetching means evaluating all of nixpkgs, which takes tens of seconds, so
/// the UI has to be able to render before the list is ready - and has to stay
/// usable if it never arrives.
#[derive(Debug, Clone)]
pub enum PkgList {
  Loading,
  Ready(Vec<String>),
  Failed(String),
}

pub static NIXPKGS: LazyLock<Arc<RwLock<PkgList>>> =
  LazyLock::new(|| Arc::new(RwLock::new(PkgList::Loading)));

/// Bumped whenever [`NIXPKGS`] changes, so open pages can cheaply notice that
/// the list arrived without cloning it every frame
static GENERATION: AtomicU64 = AtomicU64::new(0);

pub fn init_nixpkgs() {
  let pkgs_ref = NIXPKGS.clone();
  thread::spawn(move || {
    let state = match fetch_nixpkgs() {
      Ok(pkgs) => {
        log::debug!("Fetched {} packages from nixpkgs", pkgs.len());
        PkgList::Ready(pkgs)
      }
      Err(e) => {
        log::error!("Failed to fetch nixpkgs: {e}");
        PkgList::Failed(e.to_string())
      }
    };
    *pkgs_ref.write().unwrap() = state;
    GENERATION.fetch_add(1, Ordering::Release);
  });
}

/// Ask nix for every package available on *this* machine
///
/// `nix search` resolves `nixpkgs` through the flake registry, which the NixOS
/// installer ISO pins to the very nixpkgs it was built from. That gives us two
/// things for free: the list is correct for the host architecture, and it
/// matches exactly what `nixos-install` will later evaluate.
pub fn fetch_nixpkgs() -> anyhow::Result<Vec<String>> {
  let output = Command::new("nix")
    .args([
      "--extra-experimental-features",
      "nix-command flakes",
      "search",
      "--json",
      "nixpkgs",
      "^",
    ])
    .output()
    .map_err(|e| anyhow::anyhow!("could not run `nix search` (is nix on PATH?): {e}"))?;

  if !output.status.success() {
    let stderr = String::from_utf8_lossy(&output.stderr);
    anyhow::bail!("`nix search nixpkgs` failed: {}", stderr.trim());
  }

  let json: Value = serde_json::from_slice(&output.stdout)?;
  let pkgs_object = json
    .as_object()
    .ok_or_else(|| anyhow::anyhow!("Expected JSON object"))?;

  let mut pkgs = Vec::with_capacity(pkgs_object.len());
  let mut seen = HashSet::new();

  for key in pkgs_object.keys() {
    let name = strip_search_prefix(key);
    if seen.insert(name) {
      pkgs.push(name.to_string());
    }
  }

  Ok(pkgs)
}

/// Strip the `legacyPackages.<system>.` prefix `nix search` puts on every key
///
/// Splitting off exactly two leading components keeps nested attribute paths
/// like `python3Packages.requests` intact, and avoids hardcoding any
/// particular system string.
fn strip_search_prefix(key: &str) -> &str {
  let mut parts = key.splitn(3, '.');
  match (parts.next(), parts.next(), parts.next()) {
    (Some("legacyPackages" | "packages"), Some(_system), Some(rest)) => rest,
    _ => key,
  }
}

/// Current package list, or an empty one if it isn't ready
///
/// Never blocks - callers pair this with [`pkg_list_generation`] to refresh
/// themselves once the background fetch lands.
pub fn get_available_pkgs() -> Vec<String> {
  match &*NIXPKGS.read().unwrap() {
    PkgList::Ready(pkgs) => pkgs.clone(),
    PkgList::Loading | PkgList::Failed(_) => Vec::new(),
  }
}

pub fn pkg_list_generation() -> u64 {
  GENERATION.load(Ordering::Acquire)
}

/// Title for the "available packages" pane, doubling as fetch status
pub fn available_pkgs_title() -> String {
  match &*NIXPKGS.read().unwrap() {
    PkgList::Loading => "Available Packages (loading from nixpkgs...)".to_string(),
    PkgList::Ready(pkgs) => format!("Available Packages ({})", pkgs.len()),
    PkgList::Failed(err) => {
      // Keep the reason visible - it's usually the actionable part (no nix on
      // PATH, or the evaluation got OOM-killed) - but don't let it blow out
      // the width of the pane
      let reason = err.lines().next().unwrap_or("unknown error");
      let reason: String = reason.chars().take(60).collect();
      format!("Available Packages (unavailable: {reason} - type a name to add it)")
    }
  }
}

pub struct SystemPackages {
  package_picker: PackagePicker,
  pkg_generation: u64,
}

impl SystemPackages {
  pub fn new(selected_pkgs: Vec<String>, available_pkgs: Vec<String>) -> Self {
    let package_picker = PackagePicker::new(
      "Selected Packages",
      &available_pkgs_title(),
      selected_pkgs,
      available_pkgs,
    );

    Self {
      package_picker,
      pkg_generation: pkg_list_generation(),
    }
  }

  /// Pick up the nixpkgs list if the background fetch landed while this page
  /// was already open
  fn sync_pkg_list(&mut self) {
    let generation = pkg_list_generation();
    if generation != self.pkg_generation {
      self.pkg_generation = generation;
      self
        .package_picker
        .refresh_available(get_available_pkgs(), &available_pkgs_title());
    }
  }
  pub fn display_widget(installer: &mut Installer) -> Option<Box<dyn ConfigWidget>> {
    let sys_pkgs: Vec<Vec<String>> = installer
      .system_pkgs
      .clone()
      .into_iter()
      .map(|item| vec![item])
      .collect();
    if sys_pkgs.is_empty() {
      return None;
    }
    Some(Box::new(TableWidget::new(
      "",
      vec![Constraint::Percentage(100)],
      vec!["Packages".into()],
      sys_pkgs,
    )) as Box<dyn ConfigWidget>)
  }
  pub fn page_info<'a>() -> (String, Vec<Line<'a>>) {
    (
      "System Packages".to_string(),
      styled_block(vec![vec![(
        None,
        "Select extra system packages to include in the configuration",
      )]]),
    )
  }
}

impl Page for SystemPackages {
  fn render(
    &mut self,
    _installer: &mut super::Installer,
    f: &mut ratatui::Frame,
    area: ratatui::prelude::Rect,
  ) {
    self.sync_pkg_list();
    self.package_picker.render(f, area);
  }

  fn handle_input(
    &mut self,
    installer: &mut super::Installer,
    event: ratatui::crossterm::event::KeyEvent,
  ) -> super::Signal {
    use ratatui::crossterm::event::KeyCode;

    // Handle quit/escape at the top level (unless search bar is focused)
    match event.code {
      KeyCode::Esc | KeyCode::Char('q') if !self.package_picker.search_bar.is_focused() => {
        return Signal::Pop;
      }
      _ => {}
    }

    // Store the current selected packages before handling input
    let previous_selection = self.package_picker.get_selected_packages();

    // Handle the input with the package picker
    let signal = self.package_picker.handle_input(event);

    // Update installer's system_pkgs if the selection changed
    let current_selection = self.package_picker.get_selected_packages();
    if previous_selection != current_selection {
      installer.system_pkgs = current_selection;
    }

    signal
  }

  fn get_help_content(&self) -> (String, Vec<Line<'_>>) {
    let help_content = styled_block(vec![
      vec![
        (
          Some((
            ratatui::style::Color::Yellow,
            ratatui::style::Modifier::BOLD,
          )),
          "Tab",
        ),
        (None, " - Switch between lists and search"),
      ],
      vec![
        (
          Some((
            ratatui::style::Color::Yellow,
            ratatui::style::Modifier::BOLD,
          )),
          "↑/↓, j/k",
        ),
        (None, " - Navigate package lists"),
      ],
      vec![
        (
          Some((
            ratatui::style::Color::Yellow,
            ratatui::style::Modifier::BOLD,
          )),
          "Enter",
        ),
        (None, " - Add/remove package to/from selection"),
      ],
      vec![
        (
          Some((
            ratatui::style::Color::Yellow,
            ratatui::style::Modifier::BOLD,
          )),
          "/",
        ),
        (None, " - Focus search bar"),
      ],
      vec![
        (
          Some((
            ratatui::style::Color::Yellow,
            ratatui::style::Modifier::BOLD,
          )),
          "Esc",
        ),
        (None, " - Return to main menu"),
      ],
      vec![
        (
          Some((
            ratatui::style::Color::Yellow,
            ratatui::style::Modifier::BOLD,
          )),
          "?",
        ),
        (None, " - Show this help"),
      ],
      vec![(None, "")],
      vec![(None, "Search filters packages in real-time as you type.")],
      vec![(None, "Filter persists when adding/removing packages.")],
      vec![(
        None,
        "Selected packages will be installed on your NixOS system.",
      )],
    ]);
    ("System Packages".to_string(), help_content)
  }
}
