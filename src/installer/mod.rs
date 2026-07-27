use std::{
  collections::VecDeque,
  fmt::{Debug, Display},
  io::Write,
  process::{Command, Stdio},
};

use ansi_to_tui::IntoText;
use ratatui::{
  Frame,
  crossterm::event::{KeyCode, KeyEvent, KeyModifiers},
  layout::{Constraint, Direction, Layout, Rect},
  prelude::Alignment,
  style::{Color, Modifier, Style},
  text::Line,
  widgets::{Block, Borders, Paragraph, Wrap},
};
use serde_json::Value;
use tempfile::NamedTempFile;

use crate::{
  command,
  drives::{Disk, DiskConfig, part_table_multi},
  installer::{systempkgs::get_available_pkgs, users::User},
  nixgen::highlight_nix,
  split_hor, split_vert, styled_block, ui_back, ui_close, ui_down, ui_enter, ui_left, ui_right,
  ui_up,
  widget::{
    Button, CheckBox, ConfigWidget, HelpModal, InfoBox, InstallSteps, LineEditor, LogBox,
    ProgressBar, StrList, WidgetBox, WidgetBoxBuilder,
  },
};

const HIGHLIGHT: Option<(Color, Modifier)> = Some((Color::Yellow, Modifier::BOLD));

pub mod drivepages;
pub mod networking;
pub mod systempkgs;
pub mod users;
use drivepages::Drives;
use networking::NetworkConfig;
use systempkgs::SystemPackages;
use users::UserAccounts;

#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct SshCfg {
  pub enable: bool,
  pub port: u16,
  pub password_auth: bool,
  pub root_login: bool,
}

impl Default for SshCfg {
  fn default() -> Self {
    Self {
      enable: false,
      port: 22,
      password_auth: true,
      root_login: false,
    }
  }
}

#[derive(Default, Clone, serde::Serialize, serde::Deserialize)]
pub struct Installer {
  pub flake_path: Option<String>,
  pub language: Option<String>,
  pub keyboard_layout: Option<String>,
  pub locale: Option<String>,
  pub enable_flakes: bool,
  pub bootloader: Option<String>,
  pub use_swap: bool,
  pub root_passwd_hash: Option<String>, // Hashed
  pub users: Vec<User>,
  pub profile: Option<String>,
  pub hostname: Option<String>,
  pub kernels: Option<Vec<String>>,
  pub audio_backend: Option<String>,
  pub greeter: Option<String>,
  pub system_pkgs: Vec<String>,
  pub desktop_environment: Option<String>,
  pub network_backend: Option<String>,
  pub ssh_config: Option<SshCfg>,
  pub timezone: Option<String>,

  #[serde(skip)]
  pub dry_run: bool,

  pub drives: Vec<Disk>,

  pub disk_config: DiskConfig,
  pub use_auto_disk_config: bool,
  /// Scratch space for the drive currently being edited by partition pages.
  /// Gets upserted into disk_config when the user confirms.
  pub editing_drive: Option<Disk>,

  /// Used as an escape hatch for inter-page communication
  /// If you can't find a good way to pass a value from one page to another
  /// Store it here, and use mem::take() on it in the receiving page
  pub shared_register: Option<Value>,
}

impl Installer {
  pub fn new() -> Self {
    Self::default()
  }

  pub fn has_all_requirements(&self) -> bool {
    self.root_passwd_hash.is_some()
      && !self.users.is_empty()
      && !self.disk_config.is_empty()
      && self.bootloader.is_some()
  }

  pub fn to_json(&mut self) -> anyhow::Result<serde_json::Value> {
    // Create the installer configuration JSON
    // This is used as an intermediate representation before being serialized into
    // Nix
    let sys_config = serde_json::json!({
      "hostname": self.hostname,
      "language": self.language,
      "keyboard_layout": self.keyboard_layout,
      "locale": self.locale,
      "timezone": self.timezone,
      "enable_flakes": self.enable_flakes,
      "bootloader": self.bootloader,
      "use_swap": self.use_swap,
      "profile": self.profile,
      "root_passwd_hash": self.root_passwd_hash,
      "audio_backend": self.audio_backend,
      "greeter": self.greeter,
      "desktop_environment": self.desktop_environment,
      "network_backend": self.network_backend,
      "ssh_config": self.ssh_config,
      "system_pkgs": self.system_pkgs,
      "users": self.users,
      "kernels": self.kernels
    });

    // drive configuration - collect disko configs from all configured drives
    let disko_cfgs: Vec<serde_json::Value> = self
      .disk_config
      .disks_mut()
      .map(|d| d.as_disko_cfg())
      .collect();

    // flake configuration if using flakes
    let flake_path = self.flake_path.clone();

    let config = serde_json::json!({
      "config": sys_config,
      "disko": disko_cfgs,
      "flake_path": flake_path,
    });

    Ok(config)
  }

  pub fn from_json(json: serde_json::Value) -> anyhow::Result<Self> {
    serde_json::from_value(json)
      .map_err(|e| anyhow::anyhow!("Failed to deserialize installer config: {}", e))
  }
}

pub enum Signal {
  Wait,
  Push(Box<dyn Page>),
  PopAndPush(Box<dyn Page>), // Pop the current page and push a new one in its place
  Pop,
  PopCount(usize),
  Quit,
  WriteCfg,
  Unwind,               // Pop until we get back to the menu
  Error(anyhow::Error), // Propagates errors
}

impl Debug for Signal {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    match self {
      Self::Wait => write!(f, "Signal::Wait"),
      Self::Push(_) => write!(f, "Signal::Push"),
      Self::Pop => write!(f, "Signal::Pop"),
      Self::PopCount(n) => write!(f, "Signal::PopCount({n})"),
      Self::Quit => write!(f, "Signal::Quit"),
      Self::WriteCfg => write!(f, "Signal::WriteCfg"),
      Self::Unwind => write!(f, "Signal::Unwind"),
      Self::Error(err) => write!(f, "Signal::Error({err})"),
      Self::PopAndPush(_) => write!(f, "Signal::PopAndPush"),
    }
  }
}

pub trait Page {
  fn render(&mut self, installer: &mut Installer, f: &mut Frame, area: Rect);
  fn handle_input(&mut self, installer: &mut Installer, event: KeyEvent) -> Signal;
  fn get_help_content(&self) -> (String, Vec<Line<'_>>) {
    (
      "Help".to_string(),
      vec![Line::from("No help available for this page.")],
    )
  }

  /// This is used as an escape hatch for pages that need to send a signal
  /// without user input This method is called on every redraw
  fn signal(&self) -> Option<Signal> {
    None
  }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MenuPages {
  SourceFlake,
  Language,
  KeyboardLayout,
  Locale,
  EnableFlakes,
  Drives,
  Bootloader,
  Swap,
  Hostname,
  RootPassword,
  UserAccounts,
  Profile,
  Greeter,
  DesktopEnvironment,
  Audio,
  Kernels,
  SystemPackages,
  Network,
  Timezone,
}

impl MenuPages {
  pub fn all_pages() -> &'static [MenuPages] {
    &[
      MenuPages::SourceFlake,
      MenuPages::Language,
      MenuPages::KeyboardLayout,
      MenuPages::Locale,
      MenuPages::EnableFlakes,
      MenuPages::Drives,
      MenuPages::Bootloader,
      MenuPages::Swap,
      MenuPages::Hostname,
      MenuPages::RootPassword,
      MenuPages::UserAccounts,
      MenuPages::Profile,
      MenuPages::Greeter,
      MenuPages::DesktopEnvironment,
      MenuPages::Audio,
      MenuPages::Kernels,
      MenuPages::SystemPackages,
      MenuPages::Network,
      MenuPages::Timezone,
    ]
  }
  pub fn supported_pages() -> &'static [MenuPages] {
    &[
      MenuPages::KeyboardLayout,
      MenuPages::Locale,
      MenuPages::EnableFlakes,
      MenuPages::Drives,
      MenuPages::Bootloader,
      MenuPages::Swap,
      MenuPages::Hostname,
      MenuPages::RootPassword,
      MenuPages::UserAccounts,
      MenuPages::DesktopEnvironment,
      MenuPages::Audio,
      MenuPages::SystemPackages,
      MenuPages::Network,
      MenuPages::Timezone,
    ]
  }
}

impl Display for MenuPages {
  fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
    let s = match self {
      MenuPages::SourceFlake => "Source Flake",
      MenuPages::Language => "Language",
      MenuPages::KeyboardLayout => "Keyboard Layout",
      MenuPages::Locale => "Locale",
      MenuPages::EnableFlakes => "Enable Flakes",
      MenuPages::Drives => "Drives",
      MenuPages::Bootloader => "Bootloader",
      MenuPages::Swap => "Swap",
      MenuPages::Hostname => "Hostname",
      MenuPages::RootPassword => "Root Password",
      MenuPages::UserAccounts => "User Accounts",
      MenuPages::Profile => "Profile",
      MenuPages::Greeter => "Greeter",
      MenuPages::DesktopEnvironment => "Desktop Environment",
      MenuPages::Audio => "Audio",
      MenuPages::Kernels => "Kernels",
      MenuPages::SystemPackages => "System Packages",
      MenuPages::Network => "Network",
      MenuPages::Timezone => "Timezone",
    };
    write!(f, "{s}")
  }
}

impl MenuPages {
  /// Get the display widget for this page, if any
  pub fn display_widget(self, installer: &mut Installer) -> Option<Box<dyn ConfigWidget>> {
    match self {
      MenuPages::SourceFlake => SourceFlake::display_widget(installer),
      MenuPages::Language => Language::display_widget(installer),
      MenuPages::KeyboardLayout => KeyboardLayout::display_widget(installer),
      MenuPages::Locale => Locale::display_widget(installer),
      MenuPages::EnableFlakes => EnableFlakes::display_widget(installer),
      MenuPages::Drives => {
        if installer.disk_config.is_empty() {
          None
        } else {
          Some(Box::new(part_table_multi(&installer.disk_config)) as Box<dyn ConfigWidget>)
        }
      }
      MenuPages::Bootloader => Bootloader::display_widget(installer),
      MenuPages::Swap => Swap::display_widget(installer),
      MenuPages::Hostname => Hostname::display_widget(installer),
      MenuPages::RootPassword => RootPassword::display_widget(installer),
      MenuPages::UserAccounts => UserAccounts::display_widget(installer),
      MenuPages::Profile => Profile::display_widget(installer),
      MenuPages::Greeter => Greeter::display_widget(installer),
      MenuPages::DesktopEnvironment => DesktopEnvironment::display_widget(installer),
      MenuPages::Audio => Audio::display_widget(installer),
      MenuPages::Kernels => Kernels::display_widget(installer),
      MenuPages::SystemPackages => SystemPackages::display_widget(installer),
      MenuPages::Network => NetworkConfig::display_widget(installer),
      MenuPages::Timezone => Timezone::display_widget(installer),
    }
  }

  /// Get the page info (title and description) for this page
  pub fn page_info<'a>(self) -> (String, Vec<Line<'a>>) {
    match self {
      MenuPages::SourceFlake => SourceFlake::page_info(),
      MenuPages::Language => Language::page_info(),
      MenuPages::KeyboardLayout => KeyboardLayout::page_info(),
      MenuPages::Locale => Locale::page_info(),
      MenuPages::EnableFlakes => EnableFlakes::page_info(),
      MenuPages::Drives => (
        "Drives".to_string(),
        styled_block(vec![
          vec![(
            None,
            "Select and configure the drives for your NixOS installation.",
          )],
          vec![(
            None,
            "This includes partitioning, formatting, and mount points.",
          )],
          vec![(
            None,
            "If you have already configured a drive, its current configuration will be shown below.",
          )],
        ]),
      ),
      MenuPages::Bootloader => Bootloader::page_info(),
      MenuPages::Swap => Swap::page_info(),
      MenuPages::Hostname => Hostname::page_info(),
      MenuPages::RootPassword => RootPassword::page_info(),
      MenuPages::UserAccounts => UserAccounts::page_info(),
      MenuPages::Profile => Profile::page_info(),
      MenuPages::Greeter => Greeter::page_info(),
      MenuPages::DesktopEnvironment => DesktopEnvironment::page_info(),
      MenuPages::Audio => Audio::page_info(),
      MenuPages::Kernels => Kernels::page_info(),
      MenuPages::SystemPackages => SystemPackages::page_info(),
      MenuPages::Network => NetworkConfig::page_info(),
      MenuPages::Timezone => Timezone::page_info(),
    }
  }

  /// Navigate to the page - returns a Signal to push the appropriate page
  pub fn navigate(self, installer: &mut Installer) -> Signal {
    match self {
      MenuPages::SourceFlake => Signal::Push(Box::new(SourceFlake::new())),
      MenuPages::Language => Signal::Push(Box::new(Language::new())),
      MenuPages::KeyboardLayout => Signal::Push(Box::new(KeyboardLayout::new())),
      MenuPages::Locale => Signal::Push(Box::new(Locale::new())),
      MenuPages::EnableFlakes => Signal::Push(Box::new(EnableFlakes::new(installer.enable_flakes))),
      MenuPages::Drives => Signal::Push(Box::new(Drives::new())),
      MenuPages::Bootloader => Signal::Push(Box::new(Bootloader::new())),
      MenuPages::Swap => Signal::Push(Box::new(Swap::new(installer.use_swap))),
      MenuPages::Hostname => Signal::Push(Box::new(Hostname::new())),
      MenuPages::RootPassword => Signal::Push(Box::new(RootPassword::new())),
      MenuPages::UserAccounts => Signal::Push(Box::new(UserAccounts::new(installer.users.clone()))),
      MenuPages::Profile => Signal::Push(Box::new(Profile::new())),
      MenuPages::Greeter => Signal::Push(Box::new(Greeter::new())),
      MenuPages::DesktopEnvironment => Signal::Push(Box::new(DesktopEnvironment::new())),
      MenuPages::Audio => Signal::Push(Box::new(Audio::new())),
      MenuPages::Kernels => Signal::Push(Box::new(Kernels::new())),
      MenuPages::SystemPackages => {
        // we actually need to go ask nixpkgs what packages it has now
        let pkgs = get_available_pkgs();
        Signal::Push(Box::new(SystemPackages::new(
          installer.system_pkgs.clone(),
          pkgs,
        )))
      }
      MenuPages::Network => Signal::Push(Box::new(NetworkConfig::new())),
      MenuPages::Timezone => Signal::Push(Box::new(Timezone::new())),
    }
  }
}

/// The main menu page
pub struct Menu {
  menu_items: StrList,
  border_flash_timer: u32,
  button_row: WidgetBox,
  help_modal: HelpModal<'static>,
}

impl Menu {
  pub fn new() -> Self {
    let items = MenuPages::supported_pages()
      .iter()
      .map(|p| p.to_string())
      .collect::<Vec<_>>();
    let mut menu_items = StrList::new("Main Menu", items);
    let buttons: Vec<Box<dyn ConfigWidget>> = vec![
      Box::new(Button::new("Done")),
      Box::new(Button::new("Abort")),
    ];
    let button_row = WidgetBoxBuilder::new().children(buttons).build();
    menu_items.focus();
    let help_content = styled_block(vec![
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "↑/↓, j/k"),
        (None, " - Navigate menu options"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Enter"),
        (None, " - Select and configure option"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Tab, End, G"),
        (None, " - Move to action buttons"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Home, g"),
        (None, " - Return to menu options"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "q"),
        (None, " - Quit installer"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "?"),
        (None, " - Show this help"),
      ],
      vec![(None, "")],
      vec![(
        None,
        "Required options are shown in red when not configured.",
      )],
      vec![(None, "Configure all required options before proceeding.")],
    ]);
    let help_modal = HelpModal::new("Main Menu", help_content);
    Self {
      menu_items,
      button_row,
      help_modal,
      border_flash_timer: 0,
    }
  }
  pub fn info_box_for_item(&mut self, installer: &mut Installer, idx: usize) -> WidgetBox {
    // Get the actual page from supported_pages using the index
    let supported_pages = MenuPages::supported_pages();
    let page = supported_pages.get(idx).copied();

    let (display_widget, title, content) = if let Some(page) = page {
      let display_widget = page.display_widget(installer);
      let (title, content) = page.page_info();
      (display_widget, title, content)
    } else {
      (
        None,
        "Unknown Option".to_string(),
        styled_block(vec![vec![(
          None,
          "No information available for this option.",
        )]]),
      )
    };
    let mut info_box = Box::new(InfoBox::new(title, content));
    if self.border_flash_timer > 0 {
      match self.border_flash_timer % 2 {
        1 => info_box.highlighted(true),
        0 => info_box.highlighted(false),
        _ => unreachable!(),
      }
      self.border_flash_timer -= 1;
    }
    if let Some(widget) = display_widget {
      WidgetBoxBuilder::new()
        .layout(
          Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Percentage(50), Constraint::Percentage(50)].as_ref()),
        )
        .children(vec![info_box, widget])
        .build()
    } else {
      WidgetBoxBuilder::new().children(vec![info_box]).build()
    }
  }
  pub fn remaining_requirements(
    &self,
    installer: &mut Installer,
    border_flash_timer: u32,
  ) -> InfoBox<'_> {
    let mut lines = vec![];
    if installer.root_passwd_hash.is_none() {
      lines.push(vec![(
        Some((Color::Red, Modifier::BOLD)),
        " - Root Password",
      )]);
    }
    if installer.disk_config.is_empty() {
      lines.push(vec![(
        Some((Color::Red, Modifier::BOLD)),
        " - Drive Configuration",
      )]);
    }
    if installer.users.is_empty() {
      lines.push(vec![(
        Some((Color::Red, Modifier::BOLD)),
        " - At least one User Account",
      )]);
    }
    if installer.bootloader.is_none() {
      lines.push(vec![(Some((Color::Red, Modifier::BOLD)), " - Bootloader")]);
    }
    if lines.is_empty() {
      lines.push(vec![(
        Some((Color::Green, Modifier::BOLD)),
        "All required options have been configured!",
      )]);
    } else {
      lines.insert(
        0,
        vec![(
          None,
          "The following required options are not yet configured:",
        )],
      );
      lines.push(vec![(None, "Please configure them before proceeding.")]);
    }

    let mut info_box = InfoBox::new("Required Config", styled_block(lines));
    if border_flash_timer > 0 {
      match self.border_flash_timer % 2 {
        1 => info_box.highlighted(true),
        0 => info_box.highlighted(false),
        _ => unreachable!(),
      }
    }
    info_box
  }
}

impl Default for Menu {
  fn default() -> Self {
    Self::new()
  }
}

impl Page for Menu {
  fn render(&mut self, installer: &mut Installer, f: &mut Frame, area: Rect) {
    let chunks = split_hor!(
      area,
      1,
      [Constraint::Percentage(20), Constraint::Percentage(80)].as_ref()
    );

    // We use this for both the menu options and info box
    // so that it looks visually consistent :)
    let split_space = |layout: Layout, chunk: Rect| {
      layout
        .direction(Direction::Vertical)
        .constraints(
          [
            Constraint::Percentage(95), // Main content
            Constraint::Percentage(5),  // Footer
          ]
          .as_ref(),
        )
        .split(chunk)
    };

    let left_chunks = split_space(Layout::default(), chunks[0]);

    let right_chunks = split_space(Layout::default(), chunks[1]);

    self.menu_items.render(f, left_chunks[0]);
    self.button_row.render(f, left_chunks[1]);
    let border_flash_timer = self.border_flash_timer;
    let decrement_timer = border_flash_timer > 0;
    {
      // genuinely insane that this scoping trickery is actually necessary here
      let info_box: Box<dyn ConfigWidget> = if self.menu_items.is_focused() {
        Box::new(self.info_box_for_item(installer, self.menu_items.selected_idx))
          as Box<dyn ConfigWidget>
      } else {
        Box::new(self.remaining_requirements(installer, border_flash_timer))
          as Box<dyn ConfigWidget>
      };

      info_box.render(f, right_chunks[0]);

      // Render help modal on top of everything
      self.help_modal.render(f, area);
    }
    {
      if decrement_timer {
        self.border_flash_timer -= 1;
      }
    }
  }

  fn get_help_content(&self) -> (String, Vec<Line<'_>>) {
    let help_content = styled_block(vec![
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "↑/↓, j/k"),
        (None, " - Navigate menu options"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Enter"),
        (None, " - Select and configure option"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Tab, End, G"),
        (None, " - Move to action buttons"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Home, g"),
        (None, " - Return to menu options"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "q"),
        (None, " - Quit installer"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "?"),
        (None, " - Show this help"),
      ],
      vec![(None, "")],
      vec![(
        None,
        "Required options are shown in red when not configured.",
      )],
      vec![(None, "Configure all required options before proceeding.")],
    ]);
    ("Main Menu".to_string(), help_content)
  }
  fn handle_input(&mut self, installer: &mut Installer, event: KeyEvent) -> Signal {
    match event.code {
      KeyCode::Char('?') => {
        self.help_modal.toggle();
        Signal::Wait
      }
      ui_close!() if self.help_modal.visible => {
        self.help_modal.hide();
        Signal::Wait
      }
      _ if self.help_modal.visible => {
        // Help modal is open, don't process other inputs
        Signal::Wait
      }
      KeyCode::Char('q') => Signal::Quit,
      KeyCode::Home | KeyCode::Char('g') => {
        if self.menu_items.is_focused() {
          self.menu_items.first_item();
          Signal::Wait
        } else {
          self.menu_items.first_item();
          self.menu_items.focus();
          self.button_row.unfocus();
          Signal::Wait
        }
      }
      KeyCode::End | KeyCode::Char('G') => {
        if self.menu_items.is_focused() {
          self.button_row.focus();
          self.menu_items.unfocus();
        }
        Signal::Wait
      }
      ui_up!() => {
        if self.menu_items.is_focused() {
          if !self.menu_items.previous_item() {
            self.menu_items.unfocus();
            self.button_row.focus();
          }
          Signal::Wait
        } else {
          self.menu_items.last_item();
          self.menu_items.focus();
          self.button_row.unfocus();
          Signal::Wait
        }
      }
      ui_down!() => {
        if self.menu_items.is_focused() {
          if !self.menu_items.next_item() {
            self.menu_items.unfocus();
            self.button_row.focus();
          }
          Signal::Wait
        } else {
          self.menu_items.first_item();
          self.menu_items.focus();
          self.button_row.unfocus();
          Signal::Wait
        }
      }
      #[allow(unreachable_patterns)]
      ui_enter!() if self.menu_items.is_focused() => {
        let idx = self.menu_items.selected_idx;
        // Get the actual page from supported_pages using the index
        let supported_pages = MenuPages::supported_pages();
        if let Some(page) = supported_pages.get(idx).copied() {
          page.navigate(installer)
        } else {
          Signal::Wait
        }
      }
      // Button row
      ui_right!() => {
        if self.button_row.is_focused() {
          self.button_row.next_child();
        }
        Signal::Wait
      }
      ui_left!() => {
        if self.button_row.is_focused() {
          self.button_row.prev_child();
        }
        Signal::Wait
      }
      KeyCode::Enter => {
        if self.button_row.is_focused() {
          match self.button_row.selected_child() {
            Some(0) => {
              // Done - Show config preview
              if installer.has_all_requirements() {
                match ConfigPreview::new(installer) {
                  Ok(preview) => Signal::Push(Box::new(preview)),
                  Err(e) => Signal::Error(anyhow::anyhow!(
                    "Failed to generate configuration preview: {}",
                    e
                  )),
                }
              } else {
                self.border_flash_timer = 6;
                Signal::Wait
              }
            }
            Some(1) => Signal::Quit, // Abort
            _ => Signal::Wait,
          }
        } else {
          self.menu_items.focus();
          Signal::Wait
        }
      }
      _ => Signal::Wait,
    }
  }
}
/*
      MenuPages::SourceFlake,
      MenuPages::Language,
      MenuPages::KeyboardLayout,
      MenuPages::Locale,
      MenuPages::EnableFlakes,
      MenuPages::Drives,
      MenuPages::Bootloader,
      MenuPages::Swap,
      MenuPages::Hostname,
      MenuPages::RootPassword,
      MenuPages::UserAccounts,
      MenuPages::Profile,
      MenuPages::Greeter,
      MenuPages::DesktopEnvironment,
      MenuPages::Audio,
      MenuPages::Kernels,
      MenuPages::Virtualization,
      MenuPages::SystemPackages,
      MenuPages::Network,
      MenuPages::Timezone,
*/

pub struct SourceFlake {
  pub input: LineEditor,
  help_modal: HelpModal<'static>,
}

impl SourceFlake {
  pub fn new() -> Self {
    let mut input = LineEditor::new(
      "Source Config Flake",
      Some("e.g. '/path/to/flake#my-host' or 'github:user/repo#my-host'"),
    );
    input.focus();
    let help_content = styled_block(vec![
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Enter"),
        (None, " - Save configuration and return"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Esc"),
        (None, " - Cancel and return to menu"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "←/→"),
        (None, " - Move cursor"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Home/End"),
        (None, " - Jump to beginning/end"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Backspace/Del"),
        (None, " - Delete characters"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "?"),
        (None, " - Show this help"),
      ],
      vec![(None, "")],
      vec![(
        None,
        "Enter a flake path to use as system configuration source.",
      )],
      vec![(None, "Examples:")],
      vec![(None, "  /path/to/flake#my-host")],
      vec![(None, "  github:user/repo#my-host")],
    ]);
    let help_modal = HelpModal::new("Source Flake", help_content);
    Self { input, help_modal }
  }
  pub fn display_widget(installer: &mut Installer) -> Option<Box<dyn ConfigWidget>> {
    installer.flake_path.clone().map(|s| {
      let ib = InfoBox::new(
        "",
        styled_block(vec![
          vec![(None, "Current flake path set to:")],
          vec![(HIGHLIGHT, &s)],
        ]),
      );
      Box::new(ib) as Box<dyn ConfigWidget>
    })
  }
  pub fn page_info<'a>() -> (String, Vec<Line<'a>>) {
    (
      "Source Flake".to_string(),
      styled_block(vec![
        vec![(
          None,
          "Choose a flake output to use as a source for the system configuration.",
        )],
        vec![(
          None,
          "This can be used in place of manual configuration using this installer. You will still need to set up a disk partitioning plan, however.",
        )],
        vec![
          (None, "This can be "),
          (Some((Color::Reset, Modifier::ITALIC)), "any valid path"),
          (None, " to a flake output that produces a "),
          (Some((Color::Cyan, Modifier::BOLD)), "'nixosConfiguration'"),
          (None, " attribute."),
        ],
        vec![(None, "Examples include:")],
        vec![
          (None, " - A local flake: "),
          (HIGHLIGHT, "'/path/to/flake#my-host'"),
        ],
        vec![
          (None, " - A GitHub flake: "),
          (HIGHLIGHT, "'github:user/repo#my-host'"),
        ],
      ]),
    )
  }
}

impl Default for SourceFlake {
  fn default() -> Self {
    Self::new()
  }
}

impl Page for SourceFlake {
  fn render(&mut self, _installer: &mut Installer, f: &mut Frame, area: Rect) {
    let chunks = split_vert!(
      area,
      1,
      [
        Constraint::Percentage(40),
        Constraint::Length(5),
        Constraint::Percentage(40),
      ]
    );
    let hor_chunks = split_hor!(
      chunks[1],
      1,
      [
        Constraint::Percentage(10),
        Constraint::Percentage(80),
        Constraint::Percentage(10),
      ]
    );

    let info_box = InfoBox::new(
      "",
      styled_block(vec![
        vec![(
          None,
          "Choose a flake output to use as a source for the system configuration.",
        )],
        vec![(
          None,
          "This can be used in place of manual configuration using this installer. You will still need to set up a disk partitioning plan, however.",
        )],
        vec![
          (None, "This can be "),
          (Some((Color::Reset, Modifier::ITALIC)), "any valid path"),
          (None, " to a flake output that produces a "),
          (Some((Color::Cyan, Modifier::BOLD)), "'nixosConfiguration'"),
          (None, " attribute."),
        ],
        vec![(None, "Examples include:")],
        vec![
          (None, " - A local flake: "),
          (HIGHLIGHT, "'/path/to/flake#my-host'"),
        ],
        vec![
          (None, " - A GitHub flake: "),
          (HIGHLIGHT, "'github:user/repo#my-host'"),
        ],
      ]),
    );

    info_box.render(f, chunks[0]);
    self.input.render(f, hor_chunks[1]);

    // Render help modal on top
    self.help_modal.render(f, area);
  }

  fn get_help_content(&self) -> (String, Vec<Line<'_>>) {
    let help_content = styled_block(vec![
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Enter"),
        (None, " - Save configuration and return"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Esc"),
        (None, " - Cancel and return to menu"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "←/→"),
        (None, " - Move cursor"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Home/End"),
        (None, " - Jump to beginning/end"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Backspace/Del"),
        (None, " - Delete characters"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "?"),
        (None, " - Show this help"),
      ],
      vec![(None, "")],
      vec![(
        None,
        "Enter a flake path to use as system configuration source.",
      )],
      vec![(None, "Examples:")],
      vec![(None, "  /path/to/flake#my-host")],
      vec![(None, "  github:user/repo#my-host")],
    ]);
    ("Source Flake".to_string(), help_content)
  }

  fn handle_input(&mut self, installer: &mut Installer, event: KeyEvent) -> Signal {
    match event.code {
      KeyCode::Char('?') => {
        self.help_modal.toggle();
        Signal::Wait
      }
      ui_close!() if self.help_modal.visible => {
        self.help_modal.hide();
        Signal::Wait
      }
      _ if self.help_modal.visible => Signal::Wait,
      ui_back!() => Signal::Pop,
      KeyCode::Enter => {
        let flake_path = self
          .input
          .get_value()
          .unwrap()
          .as_str()
          .unwrap()
          .trim()
          .to_string();
        installer.flake_path = if flake_path.is_empty() {
          None
        } else {
          Some(flake_path)
        };
        Signal::PopCount(2)
      }
      _ => self.input.handle_input(event),
    }
  }
}

pub struct Language {
  langs: StrList,
  help_modal: HelpModal<'static>,
}

impl Language {
  pub fn new() -> Self {
    let languages = ["English"]
      .iter()
      .map(|s| s.to_string())
      .collect::<Vec<_>>();
    let mut langs = StrList::new("Select Language", languages);
    langs.focus();
    let help_content = styled_block(vec![
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "↑/↓, j/k"),
        (None, " - Navigate language options"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Enter"),
        (None, " - Select language and return"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Esc, q, ←, h"),
        (None, " - Cancel and return to menu"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "?"),
        (None, " - Show this help"),
      ],
      vec![(None, "")],
      vec![(None, "Select the language to be used for your system.")],
    ]);
    let help_modal = HelpModal::new("Language", help_content);
    Self { langs, help_modal }
  }
  pub fn display_widget(installer: &mut Installer) -> Option<Box<dyn ConfigWidget>> {
    installer.language.clone().map(|s| {
      let ib = InfoBox::new(
        "",
        styled_block(vec![
          vec![(None, "Current language set to:")],
          vec![(HIGHLIGHT, &s)],
        ]),
      );
      Box::new(ib) as Box<dyn ConfigWidget>
    })
  }
  pub fn page_info<'a>() -> (String, Vec<Line<'a>>) {
    (
      "Language".to_string(),
      styled_block(vec![vec![(
        None,
        "Select the language to be used for your system.",
      )]]),
    )
  }
}

impl Default for Language {
  fn default() -> Self {
    Self::new()
  }
}

impl Page for Language {
  fn render(&mut self, _installer: &mut Installer, f: &mut Frame, area: Rect) {
    let chunks = split_vert!(area, 1, [Constraint::Percentage(100)]);
    self.langs.render(f, chunks[0]);
    self.help_modal.render(f, area);
  }

  fn get_help_content(&self) -> (String, Vec<Line<'_>>) {
    let help_content = styled_block(vec![
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "↑/↓, j/k"),
        (None, " - Navigate language options"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Enter"),
        (None, " - Select language and return"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Esc, q, ←, h"),
        (None, " - Cancel and return to menu"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "?"),
        (None, " - Show this help"),
      ],
      vec![(None, "")],
      vec![(None, "Select the language to be used for your system.")],
    ]);
    ("Language".to_string(), help_content)
  }

  fn handle_input(&mut self, installer: &mut Installer, event: KeyEvent) -> Signal {
    match event.code {
      KeyCode::Char('?') => {
        self.help_modal.toggle();
        Signal::Wait
      }
      ui_close!() if self.help_modal.visible => {
        self.help_modal.hide();
        Signal::Wait
      }
      _ if self.help_modal.visible => Signal::Wait,
      ui_back!() => Signal::Pop,
      KeyCode::Enter => {
        installer.language = Some(self.langs.items[self.langs.selected_idx].clone());
        Signal::Pop
      }
      _ => self.langs.handle_input(event),
    }
  }
}

pub struct KeyboardLayout {
  layouts: StrList,
  help_modal: HelpModal<'static>,
}

impl KeyboardLayout {
  pub fn new() -> Self {
    let layouts = vec![
      "us(qwerty)",
      "us(dvorak)",
      "us(colemak)",
      "uk",
      "de",
      "fr",
      "es",
      "it",
      "ru",
      "cn",
      "jp",
      "kr",
      "in",
      "br",
      "nl",
      "se",
      "no",
      "fi",
      "dk",
      "pl",
      "tr",
      "gr",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect::<Vec<_>>();
    let mut layouts = StrList::new("Select Keyboard Layout", layouts);
    layouts.focus();
    let help_content = styled_block(vec![
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "↑/↓, j/k"),
        (None, " - Navigate keyboard layout options"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Enter"),
        (None, " - Select keyboard layout and return"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Esc, q, ←, h"),
        (None, " - Cancel and return to menu"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "?"),
        (None, " - Show this help"),
      ],
      vec![(None, "")],
      vec![(
        None,
        "Choose the keyboard layout that matches your physical keyboard.",
      )],
    ]);
    let help_modal = HelpModal::new("Keyboard Layout", help_content);
    Self {
      layouts,
      help_modal,
    }
  }
  pub fn display_widget(installer: &mut Installer) -> Option<Box<dyn ConfigWidget>> {
    installer.keyboard_layout.clone().map(|s| {
      let ib = InfoBox::new(
        "",
        styled_block(vec![
          vec![(None, "Current keyboard layout set to:")],
          vec![(HIGHLIGHT, &s)],
        ]),
      );
      Box::new(ib) as Box<dyn ConfigWidget>
    })
  }
  pub fn page_info<'a>() -> (String, Vec<Line<'a>>) {
    (
      "Keyboard Layout".to_string(),
      styled_block(vec![vec![(
        None,
        "Choose the keyboard layout that matches your physical keyboard.",
      )]]),
    )
  }
}

impl Default for KeyboardLayout {
  fn default() -> Self {
    Self::new()
  }
}

impl Page for KeyboardLayout {
  fn render(&mut self, _installer: &mut Installer, f: &mut Frame, area: Rect) {
    let chunks = split_vert!(area, 1, [Constraint::Percentage(100)]);
    self.layouts.render(f, chunks[0]);
    self.help_modal.render(f, area);
  }

  fn get_help_content(&self) -> (String, Vec<Line<'_>>) {
    let help_content = styled_block(vec![
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "↑/↓, j/k"),
        (None, " - Navigate keyboard layout options"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Enter"),
        (None, " - Select keyboard layout and return"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Esc, q, ←, h"),
        (None, " - Cancel and return to menu"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "?"),
        (None, " - Show this help"),
      ],
      vec![(None, "")],
      vec![(
        None,
        "Choose the keyboard layout that matches your physical keyboard.",
      )],
    ]);
    ("Keyboard Layout".to_string(), help_content)
  }

  fn handle_input(&mut self, installer: &mut Installer, event: KeyEvent) -> Signal {
    match event.code {
      KeyCode::Char('?') => {
        self.help_modal.toggle();
        Signal::Wait
      }
      ui_close!() if self.help_modal.visible => {
        self.help_modal.hide();
        Signal::Wait
      }
      _ if self.help_modal.visible => Signal::Wait,
      ui_back!() => Signal::Pop,
      KeyCode::Enter => {
        installer.keyboard_layout = Some(self.layouts.items[self.layouts.selected_idx].clone());
        Signal::Pop
      }
      ui_up!() => {
        if !self.layouts.previous_item() {
          self.layouts.last_item();
        }
        Signal::Wait
      }
      ui_down!() => {
        if !self.layouts.next_item() {
          self.layouts.first_item();
        }
        Signal::Wait
      }
      _ => self.layouts.handle_input(event),
    }
  }
}

pub struct Locale {
  locales: StrList,
  help_modal: HelpModal<'static>,
}

impl Locale {
  pub fn new() -> Self {
    let locales = vec![
      "en_US.UTF-8",
      "en_GB.UTF-8",
      "de_DE.UTF-8",
      "fr_FR.UTF-8",
      "es_ES.UTF-8",
      "it_IT.UTF-8",
      "ru_RU.UTF-8",
      "zh_CN.UTF-8",
      "ja_JP.UTF-8",
      "ko_KR.UTF-8",
      "pt_BR.UTF-8",
      "nl_NL.UTF-8",
      "sv_SE.UTF-8",
      "no_NO.UTF-8",
      "fi_FI.UTF-8",
      "da_DK.UTF-8",
      "pl_PL.UTF-8",
      "tr_TR.UTF-8",
      "el_GR.UTF-8",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect::<Vec<_>>();
    let mut locales = StrList::new("Select Locale", locales);
    locales.focus();
    let help_content = styled_block(vec![
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "↑/↓, j/k"),
        (None, " - Navigate locale options"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Enter"),
        (None, " - Select locale and return"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Esc, q, ←, h"),
        (None, " - Cancel and return to menu"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "?"),
        (None, " - Show this help"),
      ],
      vec![(None, "")],
      vec![(None, "Set the locale for your system, which determines")],
      vec![(None, "language and regional settings.")],
    ]);
    let help_modal = HelpModal::new("Locale", help_content);
    Self {
      locales,
      help_modal,
    }
  }
  pub fn display_widget(installer: &mut Installer) -> Option<Box<dyn ConfigWidget>> {
    installer.locale.clone().map(|s| {
      let ib = InfoBox::new(
        "",
        styled_block(vec![
          vec![(None, "Current locale set to:")],
          vec![(HIGHLIGHT, &s)],
        ]),
      );
      Box::new(ib) as Box<dyn ConfigWidget>
    })
  }
  pub fn page_info<'a>() -> (String, Vec<Line<'a>>) {
    (
      "Locale".to_string(),
      styled_block(vec![vec![(
        None,
        "Set the locale for your system, which determines language and regional settings.",
      )]]),
    )
  }
}

impl Default for Locale {
  fn default() -> Self {
    Self::new()
  }
}

impl Page for Locale {
  fn render(&mut self, _installer: &mut Installer, f: &mut Frame, area: Rect) {
    let chunks = split_vert!(area, 1, [Constraint::Percentage(100)]);
    self.locales.render(f, chunks[0]);
    self.help_modal.render(f, area);
  }

  fn get_help_content(&self) -> (String, Vec<Line<'_>>) {
    let help_content = styled_block(vec![
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "↑/↓, j/k"),
        (None, " - Navigate locale options"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Enter"),
        (None, " - Select locale and return"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Esc, q, ←, h"),
        (None, " - Cancel and return to menu"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "?"),
        (None, " - Show this help"),
      ],
      vec![(None, "")],
      vec![(None, "Set the locale for your system, which determines")],
      vec![(None, "language and regional settings.")],
    ]);
    ("Locale".to_string(), help_content)
  }

  fn handle_input(&mut self, installer: &mut Installer, event: KeyEvent) -> Signal {
    match event.code {
      KeyCode::Char('?') => {
        self.help_modal.toggle();
        Signal::Wait
      }
      ui_close!() if self.help_modal.visible => {
        self.help_modal.hide();
        Signal::Wait
      }
      _ if self.help_modal.visible => Signal::Wait,
      ui_back!() => Signal::Pop,
      ui_up!() => {
        if !self.locales.previous_item() {
          self.locales.last_item();
        }
        Signal::Wait
      }
      ui_down!() => {
        if !self.locales.next_item() {
          self.locales.first_item();
        }
        Signal::Wait
      }
      KeyCode::Enter => {
        installer.locale = Some(self.locales.items[self.locales.selected_idx].clone());
        Signal::Pop
      }
      _ => self.locales.handle_input(event),
    }
  }
}

pub struct EnableFlakes {
  buttons: WidgetBox,
  help_modal: HelpModal<'static>,
}

impl EnableFlakes {
  pub fn new(checked: bool) -> Self {
    let toggle = CheckBox::new("Enable Flakes Support", checked);
    let back_btn = Button::new("Back");
    let mut buttons = WidgetBox::button_menu(vec![Box::new(toggle), Box::new(back_btn)]);
    buttons.focus();
    let help_content = styled_block(vec![
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "↑/↓, j/k"),
        (None, " - Navigate options"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Enter"),
        (None, " - Toggle option or select Back"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Esc, q, ←, h"),
        (None, " - Cancel and return to menu"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "?"),
        (None, " - Show this help"),
      ],
      vec![(None, "")],
      vec![(None, "Enable or disable experimental Nix flakes support.")],
      vec![(
        None,
        "Flakes provide reproducible builds and easier dependency management.",
      )],
    ]);
    let help_modal = HelpModal::new("Enable Flakes", help_content);
    Self {
      buttons,
      help_modal,
    }
  }
  pub fn display_widget(installer: &mut Installer) -> Option<Box<dyn ConfigWidget>> {
    let status = if installer.enable_flakes {
      "enabled"
    } else {
      "disabled"
    };
    let ib = InfoBox::new(
      "",
      styled_block(vec![
        vec![(None, "Flakes support is currently:")],
        vec![(HIGHLIGHT, status)],
      ]),
    );
    Some(Box::new(ib) as Box<dyn ConfigWidget>)
  }
  pub fn page_info<'a>() -> (String, Vec<Line<'a>>) {
    (
      "Enable Flakes".to_string(),
      styled_block(vec![
        vec![(
          None,
          "Nix flakes are an experimental feature of the Nix package manager that provide a new way to manage and distribute Nix packages and configurations.",
        )],
        vec![(
          None,
          "Enabling flakes support allows you to use flake-based configurations and take advantage of features like reproducible builds and easier dependency management.",
        )],
        vec![(
          None,
          "Note that flakes are still considered experimental and may not be suitable for all users or use cases.",
        )],
      ]),
    )
  }
}

impl Default for EnableFlakes {
  fn default() -> Self {
    Self::new(false)
  }
}

impl Page for EnableFlakes {
  fn render(&mut self, _installer: &mut Installer, f: &mut Frame, area: Rect) {
    let chunks = split_vert!(
      area,
      1,
      [Constraint::Percentage(40), Constraint::Percentage(60)]
    );
    let hor_chunks = split_hor!(
      chunks[1],
      1,
      [
        Constraint::Percentage(30),
        Constraint::Percentage(40),
        Constraint::Percentage(30),
      ]
    );
    let info_box = InfoBox::new(
      "",
      styled_block(vec![
        vec![(
          None,
          "Nix flakes are an experimental feature of the Nix package manager that provide a new way to manage and distribute Nix packages and configurations.",
        )],
        vec![(
          None,
          "Enabling flakes support allows you to use flake-based configurations and take advantage of features like reproducible builds and easier dependency management.",
        )],
        vec![(
          None,
          "Note that flakes are still considered experimental and may not be suitable for all users or use cases.",
        )],
      ]),
    );
    info_box.render(f, chunks[0]);
    self.buttons.render(f, hor_chunks[1]);
    self.help_modal.render(f, area);
  }

  fn get_help_content(&self) -> (String, Vec<Line<'_>>) {
    let help_content = styled_block(vec![
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "↑/↓, j/k"),
        (None, " - Navigate options"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Enter"),
        (None, " - Toggle option or select Back"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Esc, q, ←, h"),
        (None, " - Cancel and return to menu"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "?"),
        (None, " - Show this help"),
      ],
      vec![(None, "")],
      vec![(None, "Enable or disable experimental Nix flakes support.")],
      vec![(
        None,
        "Flakes provide reproducible builds and easier dependency management.",
      )],
    ]);
    ("Enable Flakes".to_string(), help_content)
  }

  fn handle_input(&mut self, installer: &mut Installer, event: KeyEvent) -> Signal {
    match event.code {
      KeyCode::Char('?') => {
        self.help_modal.toggle();
        Signal::Wait
      }
      ui_close!() if self.help_modal.visible => {
        self.help_modal.hide();
        Signal::Wait
      }
      _ if self.help_modal.visible => Signal::Wait,
      ui_back!() => Signal::Pop,
      ui_up!() => {
        self.buttons.prev_child();
        Signal::Wait
      }
      ui_down!() => {
        self.buttons.next_child();
        Signal::Wait
      }
      KeyCode::Enter => {
        match self.buttons.selected_child() {
          Some(0) => {
            let Some(chkbox) = self.buttons.focused_child_mut() else {
              return Signal::Wait;
            };
            chkbox.interact();
            let Some(Value::Bool(checked)) = chkbox.get_value() else {
              return Signal::Wait;
            };
            installer.enable_flakes = checked;
            Signal::Wait
          }
          Some(1) => Signal::Pop, // Back
          _ => Signal::Wait,
        }
      }
      _ => Signal::Wait,
    }
  }
}

pub struct Bootloader {
  loaders: StrList,
  help_modal: HelpModal<'static>,
}

impl Bootloader {
  pub fn new() -> Self {
    let loaders = ["GRUB", "systemd-boot"]
      .iter()
      .map(|s| s.to_string())
      .collect::<Vec<_>>();
    let mut loaders = StrList::new("Select Bootloader", loaders);
    loaders.focus();
    let help_content = styled_block(vec![
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "↑/↓, j/k"),
        (None, " - Navigate bootloader options"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Enter"),
        (None, " - Select bootloader and return"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Esc, q, ←, h"),
        (None, " - Cancel and return to menu"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "?"),
        (None, " - Show this help"),
      ],
      vec![(None, "")],
      vec![(
        None,
        "Select the bootloader responsible for loading the operating system.",
      )],
    ]);
    let help_modal = HelpModal::new("Bootloader", help_content);
    Self {
      loaders,
      help_modal,
    }
  }
  pub fn get_bootloader_info<'a>(idx: usize) -> InfoBox<'a> {
    match idx {
      0 => InfoBox::new(
        "GRUB",
        styled_block(vec![
          vec![
            (HIGHLIGHT, "GRUB"),
            (None, " (GRand Unified Bootloader) is a "),
            (HIGHLIGHT, "traditional and widely-used bootloader"),
            (None, ", configured here for "),
            (HIGHLIGHT, "UEFI systems"),
            (None, "."),
          ],
          vec![
            (None, "It offers "),
            (HIGHLIGHT, "extensive configuration options"),
            (None, " and can boot "),
            (HIGHLIGHT, "other operating systems"),
            (None, ", though detecting them requires setting "),
            (HIGHLIGHT, "boot.loader.grub.useOSProber"),
            (None, " yourself."),
          ],
          vec![
            (None, "GRUB provides a "),
            (HIGHLIGHT, "menu interface"),
            (None, " at boot time and supports "),
            (HIGHLIGHT, "advanced features"),
            (None, " like custom themes and boot parameters."),
          ],
        ]),
      ),
      1 => InfoBox::new(
        "systemd-boot",
        styled_block(vec![
          vec![
            (HIGHLIGHT, "systemd-boot"),
            (None, " is a "),
            (HIGHLIGHT, "lightweight and simple bootloader"),
            (None, " that is part of the systemd project."),
          ],
          vec![
            (None, "It only supports "),
            (HIGHLIGHT, "UEFI systems"),
            (None, " and provides a "),
            (HIGHLIGHT, "minimalist approach"),
            (None, " to booting with "),
            (HIGHLIGHT, "faster boot times"),
            (None, "."),
          ],
          vec![
            (None, "systemd-boot is "),
            (HIGHLIGHT, "easier to configure"),
            (None, " than GRUB but has "),
            (HIGHLIGHT, "fewer features"),
            (None, " and is best suited for single-OS installations."),
          ],
        ]),
      ),
      _ => InfoBox::new(
        "Unknown Bootloader",
        styled_block(vec![vec![(
          None,
          "No information available for this bootloader.",
        )]]),
      ),
    }
  }
  pub fn display_widget(installer: &mut Installer) -> Option<Box<dyn ConfigWidget>> {
    installer.bootloader.clone().map(|s| {
      let ib = InfoBox::new(
        "",
        styled_block(vec![
          vec![(None, "Current bootloader set to:")],
          vec![(HIGHLIGHT, &s)],
        ]),
      );
      Box::new(ib) as Box<dyn ConfigWidget>
    })
  }
  pub fn page_info<'a>() -> (String, Vec<Line<'a>>) {
    (
      "Bootloader".to_string(),
      styled_block(vec![
        vec![(
          None,
          "Select the bootloader to be installed on your system.",
        )],
        vec![(
          None,
          "The bootloader is responsible for loading the operating system when the computer starts.",
        )],
      ]),
    )
  }
}

impl Default for Bootloader {
  fn default() -> Self {
    Self::new()
  }
}

impl Page for Bootloader {
  fn render(&mut self, _installer: &mut Installer, f: &mut Frame, area: Rect) {
    let vert_chunks = Layout::default()
      .direction(Direction::Vertical)
      .constraints([Constraint::Percentage(50), Constraint::Percentage(50)].as_ref())
      .split(area);
    let hor_chunks = split_hor!(
      vert_chunks[0],
      1,
      [
        Constraint::Percentage(40),
        Constraint::Percentage(20),
        Constraint::Percentage(40),
      ]
    );

    let idx = self.loaders.selected_idx;
    let info_box = Self::get_bootloader_info(idx);
    self.loaders.render(f, hor_chunks[1]);
    info_box.render(f, vert_chunks[1]);

    self.help_modal.render(f, area);
  }

  fn get_help_content(&self) -> (String, Vec<Line<'_>>) {
    let help_content = styled_block(vec![
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "↑/↓, j/k"),
        (None, " - Navigate bootloader options"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Enter"),
        (None, " - Select bootloader and return"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Esc, q, ←, h"),
        (None, " - Cancel and return to menu"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "?"),
        (None, " - Show this help"),
      ],
      vec![(None, "")],
      vec![(
        None,
        "Select the bootloader responsible for loading the operating system.",
      )],
    ]);
    ("Bootloader".to_string(), help_content)
  }

  fn handle_input(&mut self, installer: &mut Installer, event: KeyEvent) -> Signal {
    match event.code {
      KeyCode::Char('?') => {
        self.help_modal.toggle();
        Signal::Wait
      }
      ui_close!() if self.help_modal.visible => {
        self.help_modal.hide();
        Signal::Wait
      }
      _ if self.help_modal.visible => Signal::Wait,
      ui_back!() => Signal::Pop,
      KeyCode::Enter => {
        installer.bootloader = Some(self.loaders.items[self.loaders.selected_idx].clone());
        Signal::Pop
      }
      ui_up!() => {
        if !self.loaders.previous_item() {
          self.loaders.last_item();
        }
        Signal::Wait
      }
      ui_down!() => {
        if !self.loaders.next_item() {
          self.loaders.first_item();
        }
        Signal::Wait
      }
      _ => self.loaders.handle_input(event),
    }
  }
}

pub struct Swap {
  buttons: WidgetBox,
  help_modal: HelpModal<'static>,
}

impl Swap {
  pub fn new(checked: bool) -> Self {
    let toggle = CheckBox::new("Enable Swap", checked);
    let back_btn = Button::new("Back");
    let mut buttons = WidgetBox::button_menu(vec![Box::new(toggle), Box::new(back_btn)]);
    buttons.focus();
    let help_content = styled_block(vec![
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "↑/↓, j/k"),
        (None, " - Navigate options"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Enter"),
        (None, " - Toggle option or select Back"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Esc, q, ←, h"),
        (None, " - Cancel and return to menu"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "?"),
        (None, " - Show this help"),
      ],
      vec![(None, "")],
      vec![(None, "Enable or disable swap space for virtual memory.")],
      vec![(None, "Recommended for systems with less than 8GB RAM.")],
    ]);
    let help_modal = HelpModal::new("Swap", help_content);
    Self {
      buttons,
      help_modal,
    }
  }
  pub fn display_widget(installer: &mut Installer) -> Option<Box<dyn ConfigWidget>> {
    let status = if installer.use_swap {
      "enabled"
    } else {
      "disabled"
    };
    let ib = InfoBox::new(
      "",
      styled_block(vec![
        vec![(None, "Swap is currently:")],
        vec![(HIGHLIGHT, status)],
      ]),
    );
    Some(Box::new(ib) as Box<dyn ConfigWidget>)
  }
  pub fn page_info<'a>() -> (String, Vec<Line<'a>>) {
    (
      "Swap".to_string(),
      styled_block(vec![
        vec![(
          None,
          "Swap space is a portion of the hard drive that is used as virtual memory when the system's RAM is full.",
        )],
        vec![(
          None,
          "Enabling swap can help improve system performance and stability, especially on systems with limited RAM.",
        )],
        vec![(
          None,
          "However, using swap can also lead to slower performance compared to using RAM, as accessing data from the hard drive is slower than accessing data from RAM.",
        )],
        vec![(
          None,
          "It's generally recommended to enable swap on systems with less than 8GB of RAM, but the optimal swap size and configuration can vary depending on your specific use case and workload.",
        )],
      ]),
    )
  }
}

impl Default for Swap {
  fn default() -> Self {
    Self::new(false)
  }
}

impl Page for Swap {
  fn render(&mut self, _installer: &mut Installer, f: &mut Frame, area: Rect) {
    let chunks = split_vert!(
      area,
      1,
      [Constraint::Percentage(40), Constraint::Percentage(60)]
    );
    let hor_chunks = split_hor!(
      chunks[1],
      1,
      [
        Constraint::Percentage(30),
        Constraint::Percentage(40),
        Constraint::Percentage(30),
      ]
    );
    let info_box = InfoBox::new(
      "",
      styled_block(vec![
        vec![(
          None,
          "Swap space is a portion of the hard drive that is used as virtual memory when the system's RAM is full.",
        )],
        vec![(
          None,
          "Enabling swap can help improve system performance and stability, especially on systems with limited RAM.",
        )],
        vec![(
          None,
          "However, using swap can also lead to slower performance compared to using RAM, as accessing data from the hard drive is slower than accessing data from RAM.",
        )],
        vec![(
          None,
          "It's generally recommended to enable swap on systems with less than 8GB of RAM, but the optimal swap size and configuration can vary depending on your specific use case and workload.",
        )],
      ]),
    );
    info_box.render(f, chunks[0]);
    self.buttons.render(f, hor_chunks[1]);
    self.help_modal.render(f, area);
  }
  fn get_help_content(&self) -> (String, Vec<Line<'_>>) {
    let help_content = styled_block(vec![
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "↑/↓, j/k"),
        (None, " - Navigate options"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Enter"),
        (None, " - Toggle option or select Back"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Esc, q, ←, h"),
        (None, " - Cancel and return to menu"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "?"),
        (None, " - Show this help"),
      ],
      vec![(None, "")],
      vec![(None, "Enable or disable swap space for virtual memory.")],
      vec![(None, "Recommended for systems with less than 8GB RAM.")],
    ]);
    ("Swap".to_string(), help_content)
  }

  fn handle_input(&mut self, installer: &mut Installer, event: KeyEvent) -> Signal {
    match event.code {
      KeyCode::Char('?') => {
        self.help_modal.toggle();
        Signal::Wait
      }
      ui_close!() if self.help_modal.visible => {
        self.help_modal.hide();
        Signal::Wait
      }
      _ if self.help_modal.visible => Signal::Wait,
      ui_back!() => Signal::Pop,
      ui_up!() => {
        self.buttons.prev_child();
        Signal::Wait
      }
      ui_down!() => {
        self.buttons.next_child();
        Signal::Wait
      }
      KeyCode::Enter => {
        match self.buttons.selected_child() {
          Some(0) => {
            let Some(chkbox) = self.buttons.focused_child_mut() else {
              return Signal::Wait;
            };
            chkbox.interact();
            let Some(Value::Bool(checked)) = chkbox.get_value() else {
              return Signal::Wait;
            };
            installer.use_swap = checked;
            Signal::Wait
          }
          Some(1) => Signal::Pop, // Back
          _ => Signal::Wait,
        }
      }
      _ => Signal::Wait,
    }
  }
}

pub struct Hostname {
  input: LineEditor,
  help_modal: HelpModal<'static>,
}

impl Hostname {
  pub fn new() -> Self {
    let mut input = LineEditor::new("Set Hostname", Some("e.g. 'my-computer'"));
    input.focus();
    let help_content = styled_block(vec![
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Enter"),
        (None, " - Save hostname and return"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Esc"),
        (None, " - Cancel and return to menu"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "←/→"),
        (None, " - Move cursor"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Home/End"),
        (None, " - Jump to beginning/end"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Backspace/Del"),
        (None, " - Delete characters"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "?"),
        (None, " - Show this help"),
      ],
      vec![(None, "")],
      vec![(
        None,
        "Set a unique hostname for your computer on the network.",
      )],
    ]);
    let help_modal = HelpModal::new("Hostname", help_content);
    Self { input, help_modal }
  }
  pub fn display_widget(installer: &mut Installer) -> Option<Box<dyn ConfigWidget>> {
    installer.hostname.clone().map(|s| {
      let ib = InfoBox::new(
        "",
        styled_block(vec![
          vec![(None, "Current hostname set to:")],
          vec![(HIGHLIGHT, &s)],
        ]),
      );
      Box::new(ib) as Box<dyn ConfigWidget>
    })
  }
  pub fn page_info<'a>() -> (String, Vec<Line<'a>>) {
    (
      "Hostname".to_string(),
      styled_block(vec![
        vec![(
          None,
          "The hostname is a unique identifier for your computer on a network.",
        )],
        vec![(
          None,
          "It is used to distinguish your computer from other devices and can be helpful for network management and troubleshooting.",
        )],
        vec![(
          None,
          "Choose a hostname that is easy to remember and reflects the purpose or identity of your computer.",
        )],
      ]),
    )
  }
}

impl Default for Hostname {
  fn default() -> Self {
    Self::new()
  }
}

impl Page for Hostname {
  fn render(&mut self, _installer: &mut Installer, f: &mut Frame, area: Rect) {
    let chunks = split_vert!(
      area,
      1,
      [
        Constraint::Percentage(40),
        Constraint::Length(5),
        Constraint::Percentage(40),
      ]
    );
    let hor_chunks = split_hor!(
      chunks[1],
      0,
      [
        Constraint::Percentage(10),
        Constraint::Percentage(80),
        Constraint::Percentage(10),
      ]
    );

    let info_box = InfoBox::new(
      "",
      styled_block(vec![
        vec![(
          None,
          "The hostname is a unique identifier for your computer on a network.",
        )],
        vec![(
          None,
          "It is used to distinguish your computer from other devices and can be helpful for network management and troubleshooting.",
        )],
        vec![(
          None,
          "Choose a hostname that is easy to remember and reflects the purpose or identity of your computer.",
        )],
      ]),
    );

    info_box.render(f, chunks[0]);
    self.input.render(f, hor_chunks[1]);
    self.help_modal.render(f, area);
  }

  fn get_help_content(&self) -> (String, Vec<Line<'_>>) {
    let help_content = styled_block(vec![
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Enter"),
        (None, " - Save hostname and return"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Esc"),
        (None, " - Cancel and return to menu"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "←/→"),
        (None, " - Move cursor"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Home/End"),
        (None, " - Jump to beginning/end"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Backspace/Del"),
        (None, " - Delete characters"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "?"),
        (None, " - Show this help"),
      ],
      vec![(None, "")],
      vec![(
        None,
        "Set a unique hostname for your computer on the network.",
      )],
    ]);
    ("Hostname".to_string(), help_content)
  }

  fn handle_input(&mut self, installer: &mut Installer, event: KeyEvent) -> Signal {
    match event.code {
      KeyCode::Char('?') => {
        self.help_modal.toggle();
        Signal::Wait
      }
      ui_close!() if self.help_modal.visible => {
        self.help_modal.hide();
        Signal::Wait
      }
      _ if self.help_modal.visible => Signal::Wait,
      KeyCode::Esc => Signal::Pop,
      KeyCode::Enter => {
        let hostname = self
          .input
          .get_value()
          .unwrap()
          .as_str()
          .unwrap()
          .trim()
          .to_string();
        if !hostname.is_empty() {
          installer.hostname = Some(hostname);
        }
        Signal::Pop
      }
      _ => self.input.handle_input(event),
    }
  }
}

pub struct RootPassword {
  input: LineEditor,
  confirm: LineEditor,
  help_modal: HelpModal<'static>,
}

impl RootPassword {
  pub fn new() -> Self {
    let mut input =
      LineEditor::new("Set Root Password", Some("Password will be hidden")).secret(true);
    let confirm = LineEditor::new("Confirm Password", Some("Password will be hidden")).secret(true);
    input.focus();
    let help_content = styled_block(vec![
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Enter"),
        (None, " - Move to next field or save when complete"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Tab"),
        (None, " - Switch between password fields"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Esc"),
        (None, " - Cancel and return to menu"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "←/→"),
        (None, " - Move cursor"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Home/End"),
        (None, " - Jump to beginning/end"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Backspace/Del"),
        (None, " - Delete characters"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "?"),
        (None, " - Show this help"),
      ],
      vec![(None, "")],
      vec![(None, "Set a strong root password for system security.")],
    ]);
    let help_modal = HelpModal::new("Root Password", help_content);
    Self {
      input,
      confirm,
      help_modal,
    }
  }
  pub fn page_info<'a>() -> (String, Vec<Line<'a>>) {
    (
      "Root Password".to_string(),
      styled_block(vec![
        vec![(
          None,
          "The root user is the superuser account on a Unix-like operating system, including Linux.",
        )],
        vec![(
          None,
          "It has full administrative privileges and can perform any action on the system, including installing software, modifying system settings, and accessing all files and directories.",
        )],
        vec![(
          None,
          "Setting a strong password for the root user is important for system security, as it helps prevent unauthorized access to sensitive system functions and data.",
        )],
        vec![(
          None,
          "Choose a password that is difficult to guess and contains a mix of uppercase and lowercase letters, numbers, and special characters.",
        )],
      ]),
    )
  }
  pub fn display_widget(installer: &mut Installer) -> Option<Box<dyn ConfigWidget>> {
    installer.root_passwd_hash.as_ref().map(|_| {
      let ib = InfoBox::new(
        "",
        styled_block(vec![vec![(HIGHLIGHT, "Root password is set.")]]),
      );
      Box::new(ib) as Box<dyn ConfigWidget>
    })
  }
  pub fn mkpasswd(passwd: String) -> anyhow::Result<String> {
    let mut child = Command::new("mkpasswd")
      .arg("--method=SHA-512")
      .arg("--rounds=4096")
      .arg("--stdin")
      .stdin(Stdio::piped())
      .stdout(Stdio::piped())
      .spawn()?;
    {
      let stdin = child
        .stdin
        .as_mut()
        .ok_or_else(|| anyhow::anyhow!("Failed to open stdin"))?;
      stdin.write_all(passwd.as_bytes())?;
    }
    let output = child.wait_with_output()?;
    if output.status.success() {
      let hashed = String::from_utf8_lossy(&output.stdout).trim().to_string();
      Ok(hashed)
    } else {
      Err(anyhow::anyhow!(
        "mkpasswd failed: {}",
        String::from_utf8_lossy(&output.stderr)
      ))
    }
  }
}

impl Default for RootPassword {
  fn default() -> Self {
    Self::new()
  }
}

impl Page for RootPassword {
  fn render(&mut self, _installer: &mut Installer, f: &mut Frame, area: Rect) {
    let chunks = split_vert!(
      area,
      1,
      [
        Constraint::Percentage(40),
        Constraint::Length(12),
        Constraint::Percentage(40),
      ]
    );
    let hor_chunks = split_hor!(
      chunks[1],
      1,
      [
        Constraint::Percentage(20),
        Constraint::Percentage(60),
        Constraint::Percentage(20),
      ]
    );
    let vert_chunks = split_vert!(
      hor_chunks[1],
      0,
      [Constraint::Length(5), Constraint::Length(5)]
    );

    let info_box = InfoBox::new(
      "",
      styled_block(vec![
        vec![(
          None,
          "The root user is the superuser account on a Unix-like operating system, including Linux.",
        )],
        vec![(
          None,
          "It has full administrative privileges and can perform any action on the system, including installing software, modifying system settings, and accessing all files and directories.",
        )],
        vec![(
          None,
          "Setting a strong password for the root user is important for system security, as it helps prevent unauthorized access to sensitive system functions and data.",
        )],
        vec![(
          None,
          "Choose a password that is difficult to guess and contains a mix of uppercase and lowercase letters, numbers, and special characters.",
        )],
      ]),
    );

    info_box.render(f, chunks[0]);
    self.input.render(f, vert_chunks[0]);
    self.confirm.render(f, vert_chunks[1]);
    self.help_modal.render(f, area);
  }

  fn get_help_content(&self) -> (String, Vec<Line<'_>>) {
    let help_content = styled_block(vec![
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Enter"),
        (None, " - Move to next field or save when complete"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Tab"),
        (None, " - Switch between password fields"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Esc"),
        (None, " - Cancel and return to menu"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "←/→"),
        (None, " - Move cursor"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Home/End"),
        (None, " - Jump to beginning/end"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Backspace/Del"),
        (None, " - Delete characters"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "?"),
        (None, " - Show this help"),
      ],
      vec![(None, "")],
      vec![(None, "Set a strong root password for system security.")],
    ]);
    ("Root Password".to_string(), help_content)
  }

  fn handle_input(&mut self, installer: &mut Installer, event: KeyEvent) -> Signal {
    match event.code {
      KeyCode::Char('?') => {
        self.help_modal.toggle();
        Signal::Wait
      }
      ui_close!() if self.help_modal.visible => {
        self.help_modal.hide();
        Signal::Wait
      }
      _ if self.help_modal.visible => Signal::Wait,
      KeyCode::Esc => Signal::Pop,
      KeyCode::Tab => {
        if self.input.is_focused() {
          self.input.unfocus();
          self.confirm.focus();
        } else {
          self.confirm.unfocus();
          self.input.focus();
        }
        Signal::Wait
      }
      KeyCode::Enter => {
        if self.input.is_focused() {
          self.input.unfocus();
          self.confirm.focus();
          Signal::Wait
        } else {
          let passwd = self
            .input
            .get_value()
            .unwrap()
            .as_str()
            .unwrap()
            .trim()
            .to_string();
          let confirm = self
            .confirm
            .get_value()
            .unwrap()
            .as_str()
            .unwrap()
            .trim()
            .to_string();
          if passwd.is_empty() {
            Signal::Wait // Ignore empty passwords
          } else if passwd != confirm {
            self.input.clear();
            self.confirm.clear();
            self.confirm.unfocus();
            self.input.focus();
            self.input.error("Passwords do not match");
            Signal::Wait // Passwords do not match
          } else {
            match Self::mkpasswd(passwd) {
              Ok(hashed) => {
                installer.root_passwd_hash = Some(hashed);
                Signal::Pop
              }
              Err(e) => {
                self.input.clear();
                self.confirm.clear();
                self.confirm.unfocus();
                self.input.focus();
                self.input.error(format!("Error hashing password: {e}"));
                Signal::Wait
              }
            }
          }
        }
      }
      _ => {
        if self.input.is_focused() {
          self.input.handle_input(event)
        } else {
          self.confirm.handle_input(event)
        }
      }
    }
  }
}

pub struct Profile {
  profiles: StrList,
  help_modal: HelpModal<'static>,
}

impl Profile {
  pub fn new() -> Self {
    let profiles = ["Minimal", "Desktop", "Server", "Custom"]
      .iter()
      .map(|s| s.to_string())
      .collect::<Vec<_>>();
    let mut profiles = StrList::new("Select Profile", profiles);
    profiles.focus();
    let help_content = styled_block(vec![
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "↑/↓, j/k"),
        (None, " - Navigate profile options"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Enter"),
        (None, " - Select profile and return"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Esc, q, ←, h"),
        (None, " - Cancel and return to menu"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "?"),
        (None, " - Show this help"),
      ],
      vec![(None, "")],
      vec![(
        None,
        "Select a predefined profile that matches your intended use case.",
      )],
    ]);
    let help_modal = HelpModal::new("Profile", help_content);
    Self {
      profiles,
      help_modal,
    }
  }
  pub fn display_widget(installer: &mut Installer) -> Option<Box<dyn ConfigWidget>> {
    installer.profile.clone().map(|s| {
      let ib = InfoBox::new(
        "",
        styled_block(vec![
          vec![(None, "Current profile set to:")],
          vec![(HIGHLIGHT, &s)],
        ]),
      );
      Box::new(ib) as Box<dyn ConfigWidget>
    })
  }
  pub fn page_info<'a>() -> (String, Vec<Line<'a>>) {
    (
      "Profile".to_string(),
      styled_block(vec![
        vec![(
          None,
          "Select a predefined profile that best matches your intended use case for the system.",
        )],
        vec![(
          None,
          "Profiles are collections of settings and packages that are tailored for specific use cases, such as desktop or server environments.",
        )],
        vec![(
          None,
          "Choosing a profile can help simplify the installation process and ensure that your system is configured appropriately for your needs.",
        )],
        vec![(
          None,
          "You can always customize the configuration further after the installation is complete.",
        )],
      ]),
    )
  }
}

impl Default for Profile {
  fn default() -> Self {
    Self::new()
  }
}

impl Page for Profile {
  fn render(&mut self, _installer: &mut Installer, f: &mut Frame, area: Rect) {
    let chunks = split_vert!(area, 1, [Constraint::Percentage(100)]);
    self.profiles.render(f, chunks[0]);
    self.help_modal.render(f, area);
  }

  fn get_help_content(&self) -> (String, Vec<Line<'_>>) {
    let help_content = styled_block(vec![
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "↑/↓, j/k"),
        (None, " - Navigate profile options"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Enter"),
        (None, " - Select profile and return"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Esc, q, ←, h"),
        (None, " - Cancel and return to menu"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "?"),
        (None, " - Show this help"),
      ],
      vec![(None, "")],
      vec![(
        None,
        "Select a predefined profile that matches your intended use case.",
      )],
    ]);
    ("Profile".to_string(), help_content)
  }

  fn handle_input(&mut self, installer: &mut Installer, event: KeyEvent) -> Signal {
    match event.code {
      KeyCode::Char('?') => {
        self.help_modal.toggle();
        Signal::Wait
      }
      ui_close!() if self.help_modal.visible => {
        self.help_modal.hide();
        Signal::Wait
      }
      _ if self.help_modal.visible => Signal::Wait,
      ui_down!() => Signal::Pop,
      KeyCode::Enter => {
        installer.profile = Some(self.profiles.items[self.profiles.selected_idx].clone());
        Signal::Pop
      }
      _ => self.profiles.handle_input(event),
    }
  }
}

pub struct Greeter {
  greeters: StrList,
  help_modal: HelpModal<'static>,
}

impl Greeter {
  pub fn new() -> Self {
    let greeters = ["LightDM", "GDM", "SDDM", "None"]
      .iter()
      .map(|s| s.to_string())
      .collect::<Vec<_>>();
    let mut greeters = StrList::new("Select Greeter", greeters);
    greeters.focus();
    let help_content = styled_block(vec![
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "↑/↓, j/k"),
        (None, " - Navigate greeter options"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Enter"),
        (None, " - Select greeter and return"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Esc, q, ←, h"),
        (None, " - Cancel and return to menu"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "?"),
        (None, " - Show this help"),
      ],
      vec![(None, "")],
      vec![(
        None,
        "Select the display manager for the graphical login screen.",
      )],
    ]);
    let help_modal = HelpModal::new("Greeter", help_content);
    Self {
      greeters,
      help_modal,
    }
  }
  pub fn display_widget(installer: &mut Installer) -> Option<Box<dyn ConfigWidget>> {
    installer.greeter.clone().map(|s| {
      let ib = InfoBox::new(
        "",
        styled_block(vec![
          vec![(None, "Current greeter set to:")],
          vec![(HIGHLIGHT, &s)],
        ]),
      );
      Box::new(ib) as Box<dyn ConfigWidget>
    })
  }
  pub fn page_info<'a>() -> (String, Vec<Line<'a>>) {
    (
      "Greeter".to_string(),
      styled_block(vec![
        vec![(
          None,
          "Select the display manager (greeter) to be installed on your system.",
        )],
        vec![(
          None,
          "The display manager is responsible for providing the graphical login screen and managing user sessions.",
        )],
      ]),
    )
  }
}

impl Default for Greeter {
  fn default() -> Self {
    Self::new()
  }
}

impl Page for Greeter {
  fn render(&mut self, _installer: &mut Installer, f: &mut Frame, area: Rect) {
    let chunks = split_vert!(area, 1, [Constraint::Percentage(100)]);
    self.greeters.render(f, chunks[0]);
    self.help_modal.render(f, area);
  }

  fn get_help_content(&self) -> (String, Vec<Line<'_>>) {
    let help_content = styled_block(vec![
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "↑/↓, j/k"),
        (None, " - Navigate greeter options"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Enter"),
        (None, " - Select greeter and return"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Esc, q, ←, h"),
        (None, " - Cancel and return to menu"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "?"),
        (None, " - Show this help"),
      ],
      vec![(None, "")],
      vec![(
        None,
        "Select the display manager for the graphical login screen.",
      )],
    ]);
    ("Greeter".to_string(), help_content)
  }

  fn handle_input(&mut self, installer: &mut Installer, event: KeyEvent) -> Signal {
    match event.code {
      KeyCode::Char('?') => {
        self.help_modal.toggle();
        Signal::Wait
      }
      ui_close!() if self.help_modal.visible => {
        self.help_modal.hide();
        Signal::Wait
      }
      _ if self.help_modal.visible => Signal::Wait,
      ui_back!() => Signal::Pop,
      KeyCode::Enter => {
        installer.greeter = Some(self.greeters.items[self.greeters.selected_idx].clone());
        Signal::Pop
      }
      _ => self.greeters.handle_input(event),
    }
  }
}

pub struct DesktopEnvironment {
  desktops: StrList,
  help_modal: HelpModal<'static>,
}

impl DesktopEnvironment {
  pub fn new() -> Self {
    let desktops = [
      "GNOME",
      "KDE Plasma",
      "Hyprland",
      "XFCE",
      "Cinnamon",
      "MATE",
      "lxqt",
      "Budgie",
      "i3",
      "None",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect::<Vec<_>>();
    let mut desktops = StrList::new("Select Desktop Environment", desktops);
    desktops.focus();
    let help_content = styled_block(vec![
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "↑/↓, j/k"),
        (None, " - Navigate desktop environment options"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Enter"),
        (None, " - Select desktop environment and return"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Esc, q, ←, h"),
        (None, " - Cancel and return to menu"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "?"),
        (None, " - Show this help"),
      ],
      vec![(None, "")],
      vec![(
        None,
        "Select the desktop environment for your graphical interface.",
      )],
    ]);
    let help_modal = HelpModal::new("Desktop Environment", help_content);
    Self {
      desktops,
      help_modal,
    }
  }
  pub fn get_desktop_info<'a>(idx: usize) -> InfoBox<'a> {
    match idx {
      0 => InfoBox::new(
        "GNOME",
        styled_block(vec![
          vec![
            (HIGHLIGHT, "GNOME"),
            (None, " is a "),
            (HIGHLIGHT, "modern and popular desktop environment"),
            (None, " that provides a "),
            (HIGHLIGHT, "user-friendly experience"),
            (None, " with a focus on "),
            (HIGHLIGHT, "simplicity and elegance"),
            (None, "."),
          ],
          vec![
            (None, "It features a "),
            (HIGHLIGHT, "clean interface"),
            (None, " with "),
            (HIGHLIGHT, "activities overview"),
            (None, ", "),
            (HIGHLIGHT, "workspaces"),
            (None, ", and extensive "),
            (HIGHLIGHT, "customization options"),
            (None, " through extensions."),
          ],
          vec![
            (None, "GNOME is "),
            (HIGHLIGHT, "resource-intensive"),
            (None, " but offers "),
            (HIGHLIGHT, "excellent accessibility"),
            (None, " and "),
            (HIGHLIGHT, "touch support"),
            (None, "."),
          ],
        ]),
      ),
      1 => InfoBox::new(
        "KDE Plasma",
        styled_block(vec![
          vec![
            (HIGHLIGHT, "KDE Plasma"),
            (None, " is a "),
            (HIGHLIGHT, "highly customizable desktop environment"),
            (None, " that offers "),
            (HIGHLIGHT, "extensive configuration options"),
            (None, " and a "),
            (HIGHLIGHT, "traditional desktop experience"),
            (None, "."),
          ],
          vec![
            (None, "It provides "),
            (HIGHLIGHT, "powerful widgets"),
            (None, ", "),
            (HIGHLIGHT, "multiple panel layouts"),
            (None, ", and "),
            (HIGHLIGHT, "advanced system settings"),
            (None, " with a familiar Windows-like interface."),
          ],
          vec![
            (None, "KDE Plasma is "),
            (HIGHLIGHT, "feature-rich"),
            (None, " and "),
            (HIGHLIGHT, "resource-efficient"),
            (
              None,
              ", making it suitable for both power users and beginners.",
            ),
          ],
        ]),
      ),
      2 => InfoBox::new(
        "Hyprland",
        styled_block(vec![
          vec![
            (HIGHLIGHT, "Hyprland"),
            (None, " is a "),
            (HIGHLIGHT, "dynamic tiling Wayland compositor"),
            (None, " that focuses on "),
            (HIGHLIGHT, "eye candy and customization"),
            (None, "."),
          ],
          vec![
            (None, "It features "),
            (HIGHLIGHT, "beautiful animations"),
            (None, ", "),
            (HIGHLIGHT, "automatic window tiling"),
            (None, ", and "),
            (HIGHLIGHT, "extensive configuration"),
            (None, " through text files."),
          ],
          vec![
            (None, "Hyprland is "),
            (HIGHLIGHT, "highly efficient"),
            (None, " and perfect for users who prefer "),
            (HIGHLIGHT, "keyboard-driven workflows"),
            (None, " and "),
            (HIGHLIGHT, "minimal resource usage"),
            (None, "."),
          ],
        ]),
      ),
      3 => InfoBox::new(
        "XFCE",
        styled_block(vec![
          vec![
            (HIGHLIGHT, "XFCE"),
            (None, " is a "),
            (HIGHLIGHT, "lightweight and fast desktop environment"),
            (None, " that aims to be "),
            (HIGHLIGHT, "visually appealing and user-friendly"),
            (None, " while being "),
            (HIGHLIGHT, "resource-efficient"),
            (None, "."),
          ],
          vec![
            (None, "It provides a "),
            (HIGHLIGHT, "traditional desktop experience"),
            (None, " with "),
            (HIGHLIGHT, "customizable panels"),
            (None, ", "),
            (HIGHLIGHT, "file manager"),
            (None, ", and "),
            (HIGHLIGHT, "application menu"),
            (None, "."),
          ],
          vec![
            (None, "XFCE is "),
            (HIGHLIGHT, "perfect for older hardware"),
            (None, " or users who want a "),
            (HIGHLIGHT, "simple, stable"),
            (None, " desktop without sacrificing functionality."),
          ],
        ]),
      ),
      4 => InfoBox::new(
        "Cinnamon",
        styled_block(vec![
          vec![
            (HIGHLIGHT, "Cinnamon"),
            (None, " is a "),
            (HIGHLIGHT, "modern desktop environment"),
            (None, " that provides a "),
            (HIGHLIGHT, "familiar and intuitive experience"),
            (None, " similar to traditional desktops."),
          ],
          vec![
            (None, "It features a "),
            (HIGHLIGHT, "taskbar-style panel"),
            (None, ", "),
            (HIGHLIGHT, "system tray"),
            (None, ", and "),
            (HIGHLIGHT, "start menu"),
            (None, " with "),
            (HIGHLIGHT, "smooth animations"),
            (None, " and effects."),
          ],
          vec![
            (None, "Cinnamon balances "),
            (HIGHLIGHT, "modern features"),
            (None, " with "),
            (HIGHLIGHT, "traditional usability"),
            (
              None,
              ", making it great for users transitioning from other operating systems.",
            ),
          ],
        ]),
      ),
      5 => InfoBox::new(
        "MATE",
        styled_block(vec![
          vec![
            (HIGHLIGHT, "MATE"),
            (None, " is a "),
            (HIGHLIGHT, "traditional desktop environment"),
            (None, " that continues the legacy of "),
            (HIGHLIGHT, "GNOME 2"),
            (None, " with a "),
            (HIGHLIGHT, "classic interface"),
            (None, "."),
          ],
          vec![
            (None, "It provides "),
            (HIGHLIGHT, "stability"),
            (None, ", "),
            (HIGHLIGHT, "reliability"),
            (None, ", and "),
            (HIGHLIGHT, "low resource usage"),
            (None, " while maintaining "),
            (HIGHLIGHT, "familiar desktop metaphors"),
            (None, "."),
          ],
          vec![
            (None, "MATE is "),
            (HIGHLIGHT, "ideal for users"),
            (None, " who prefer "),
            (HIGHLIGHT, "conventional desktop layouts"),
            (None, " and "),
            (HIGHLIGHT, "proven workflows"),
            (None, "."),
          ],
        ]),
      ),
      6 => InfoBox::new(
        "LXQt",
        styled_block(vec![
          vec![
            (HIGHLIGHT, "LXQt"),
            (None, " is a "),
            (HIGHLIGHT, "lightweight Qt-based desktop environment"),
            (None, " that focuses on "),
            (HIGHLIGHT, "efficiency and performance"),
            (None, "."),
          ],
          vec![
            (None, "It provides a "),
            (HIGHLIGHT, "simple and clean interface"),
            (None, " with "),
            (HIGHLIGHT, "low memory footprint"),
            (None, " and "),
            (HIGHLIGHT, "fast startup times"),
            (None, "."),
          ],
          vec![
            (None, "LXQt is "),
            (HIGHLIGHT, "perfect for older computers"),
            (None, " or users who want "),
            (HIGHLIGHT, "minimal resource consumption"),
            (None, " without sacrificing modern features."),
          ],
        ]),
      ),
      7 => InfoBox::new(
        "Budgie",
        styled_block(vec![
          vec![
            (HIGHLIGHT, "Budgie"),
            (None, " is a "),
            (HIGHLIGHT, "elegant and modern desktop environment"),
            (None, " that emphasizes "),
            (HIGHLIGHT, "simplicity and user experience"),
            (None, "."),
          ],
          vec![
            (None, "It features a "),
            (HIGHLIGHT, "clean sidebar panel"),
            (None, ", "),
            (HIGHLIGHT, "notification system"),
            (None, ", and "),
            (HIGHLIGHT, "built-in applets"),
            (None, " with a focus on productivity."),
          ],
          vec![
            (None, "Budgie offers a "),
            (HIGHLIGHT, "balance between simplicity and functionality"),
            (None, ", making it suitable for users who want a "),
            (HIGHLIGHT, "modern desktop"),
            (None, " without complexity."),
          ],
        ]),
      ),
      8 => InfoBox::new(
        "i3",
        styled_block(vec![
          vec![
            (HIGHLIGHT, "i3"),
            (None, " is a "),
            (HIGHLIGHT, "tiling window manager"),
            (None, " designed for "),
            (HIGHLIGHT, "power users and developers"),
            (None, " who prefer "),
            (HIGHLIGHT, "keyboard-driven workflows"),
            (None, "."),
          ],
          vec![
            (None, "It automatically "),
            (HIGHLIGHT, "tiles windows"),
            (None, " to make "),
            (HIGHLIGHT, "efficient use of screen space"),
            (None, " and provides "),
            (HIGHLIGHT, "extensive customization"),
            (None, " through configuration files."),
          ],
          vec![
            (None, "i3 has "),
            (HIGHLIGHT, "minimal resource usage"),
            (None, " and offers "),
            (HIGHLIGHT, "maximum productivity"),
            (None, " for users comfortable with command-line interfaces."),
          ],
        ]),
      ),
      _ => InfoBox::new(
        "Unknown Desktop Environment",
        styled_block(vec![vec![(
          None,
          "No information available for this desktop environment.",
        )]]),
      ),
    }
  }
  pub fn display_widget(installer: &mut Installer) -> Option<Box<dyn ConfigWidget>> {
    installer.desktop_environment.clone().map(|s| {
      let ib = InfoBox::new(
        "",
        styled_block(vec![
          vec![(None, "Current desktop environment set to:")],
          vec![(HIGHLIGHT, &s)],
        ]),
      );
      Box::new(ib) as Box<dyn ConfigWidget>
    })
  }
  pub fn page_info<'a>() -> (String, Vec<Line<'a>>) {
    (
      "Desktop Environment".to_string(),
      styled_block(vec![
        vec![(
          None,
          "Select the desktop environment to be installed on your system.",
        )],
        vec![(
          None,
          "The desktop environment provides the graphical user interface (GUI) for your system, including the window manager, panels, and application launchers.",
        )],
        vec![(
          None,
          "Choosing a desktop environment can help tailor the user experience to your preferences and workflow.",
        )],
      ]),
    )
  }
}

impl Default for DesktopEnvironment {
  fn default() -> Self {
    Self::new()
  }
}

impl Page for DesktopEnvironment {
  fn render(&mut self, _installer: &mut Installer, f: &mut Frame, area: Rect) {
    let vert_chunks = Layout::default()
      .direction(Direction::Vertical)
      .constraints([Constraint::Percentage(50), Constraint::Percentage(50)].as_ref())
      .split(area);
    let hor_chunks = split_hor!(
      vert_chunks[0],
      1,
      [
        Constraint::Percentage(38),
        Constraint::Length(28),
        Constraint::Percentage(38),
      ]
    );

    let idx = self.desktops.selected_idx;
    let info_box = Self::get_desktop_info(idx);
    self.desktops.render(f, hor_chunks[1]);
    if idx < 9 {
      info_box.render(f, vert_chunks[1]);
    }

    self.help_modal.render(f, area);
  }

  fn get_help_content(&self) -> (String, Vec<Line<'_>>) {
    let help_content = styled_block(vec![
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "↑/↓, j/k"),
        (None, " - Navigate desktop environment options"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Enter"),
        (None, " - Select desktop environment and return"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Esc, q, ←, h"),
        (None, " - Cancel and return to menu"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "?"),
        (None, " - Show this help"),
      ],
      vec![(None, "")],
      vec![(
        None,
        "Select the desktop environment for your graphical interface.",
      )],
    ]);
    ("Desktop Environment".to_string(), help_content)
  }

  fn handle_input(&mut self, installer: &mut Installer, event: KeyEvent) -> Signal {
    match event.code {
      KeyCode::Char('?') => {
        self.help_modal.toggle();
        Signal::Wait
      }
      ui_close!() if self.help_modal.visible => {
        self.help_modal.hide();
        Signal::Wait
      }
      _ if self.help_modal.visible => Signal::Wait,
      ui_back!() => Signal::Pop,
      KeyCode::Enter => {
        installer.desktop_environment =
          Some(self.desktops.items[self.desktops.selected_idx].clone());
        Signal::Pop
      }
      ui_up!() => {
        if !self.desktops.previous_item() {
          self.desktops.last_item();
        }
        Signal::Wait
      }
      ui_down!() => {
        if !self.desktops.next_item() {
          self.desktops.first_item();
        }
        Signal::Wait
      }
      _ => self.desktops.handle_input(event),
    }
  }
}

pub struct Kernels {
  kernels: StrList,
  help_modal: HelpModal<'static>,
}

impl Kernels {
  pub fn new() -> Self {
    let kernels = ["linux", "linux-lts", "linux-zen", "linux-hardened", "None"]
      .iter()
      .map(|s| s.to_string())
      .collect::<Vec<_>>();
    let mut kernels = StrList::new("Select Kernel", kernels);
    kernels.focus();
    let help_content = styled_block(vec![
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "↑/↓, j/k"),
        (None, " - Navigate kernel options"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Enter"),
        (None, " - Select kernel and return"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Esc, q, ←, h"),
        (None, " - Cancel and return to menu"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "?"),
        (None, " - Show this help"),
      ],
      vec![(None, "")],
      vec![(
        None,
        "Select the Linux kernel to optimize system performance.",
      )],
    ]);
    let help_modal = HelpModal::new("Kernel", help_content);
    Self {
      kernels,
      help_modal,
    }
  }
  pub fn display_widget(installer: &mut Installer) -> Option<Box<dyn ConfigWidget>> {
    installer.kernels.clone().map(|s| {
      let ib = InfoBox::new(
        "",
        styled_block(vec![
          vec![(None, "Currently selected kernels:".to_string())],
          s.clone()
            .into_iter()
            .map(|k| (HIGHLIGHT, k))
            .collect::<Vec<_>>(),
        ]),
      );
      Box::new(ib) as Box<dyn ConfigWidget>
    })
  }
  pub fn page_info<'a>() -> (String, Vec<Line<'a>>) {
    (
      "Kernel".to_string(),
      styled_block(vec![
        vec![(
          None,
          "Select the Linux kernel to be installed on your system.",
        )],
        vec![(
          None,
          "The kernel is the core component of the operating system that manages hardware resources and provides essential services for other software.",
        )],
        vec![(
          None,
          "Choosing a kernel can help optimize system performance and compatibility with your hardware.",
        )],
      ]),
    )
  }
}

impl Default for Kernels {
  fn default() -> Self {
    Self::new()
  }
}

impl Page for Kernels {
  fn render(&mut self, _installer: &mut Installer, f: &mut Frame, area: Rect) {
    let chunks = split_vert!(area, 1, [Constraint::Percentage(100)]);
    self.kernels.render(f, chunks[0]);
    self.help_modal.render(f, area);
  }

  fn get_help_content(&self) -> (String, Vec<Line<'_>>) {
    let help_content = styled_block(vec![
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "↑/↓, j/k"),
        (None, " - Navigate kernel options"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Enter"),
        (None, " - Select kernel and return"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Esc, q, ←, h"),
        (None, " - Cancel and return to menu"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "?"),
        (None, " - Show this help"),
      ],
      vec![(None, "")],
      vec![(
        None,
        "Select the Linux kernel to optimize system performance.",
      )],
    ]);
    ("Kernel".to_string(), help_content)
  }

  fn handle_input(&mut self, installer: &mut Installer, event: KeyEvent) -> Signal {
    match event.code {
      KeyCode::Char('?') => {
        self.help_modal.toggle();
        Signal::Wait
      }
      ui_close!() if self.help_modal.visible => {
        self.help_modal.hide();
        Signal::Wait
      }
      _ if self.help_modal.visible => Signal::Wait,
      ui_back!() => Signal::Pop,
      KeyCode::Enter => {
        // TODO: Implement multi selection for StrList
        installer.kernels = Some(vec![self.kernels.items[self.kernels.selected_idx].clone()]);
        Signal::Pop
      }
      _ => self.kernels.handle_input(event),
    }
  }
}

pub struct Audio {
  backends: StrList,
  help_modal: HelpModal<'static>,
}

impl Audio {
  pub fn new() -> Self {
    let backends = ["PipeWire", "PulseAudio", "None"]
      .iter()
      .map(|s| s.to_string())
      .collect::<Vec<_>>();
    let mut backends = StrList::new("Select Audio Backend", backends);
    backends.focus();
    let help_content = styled_block(vec![
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "↑/↓, j/k"),
        (None, " - Navigate audio backend options"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Enter"),
        (None, " - Select audio backend and return"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Esc, q, ←, h"),
        (None, " - Cancel and return to menu"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "?"),
        (None, " - Show this help"),
      ],
      vec![(None, "")],
      vec![(
        None,
        "Select the audio management backend for sound devices.",
      )],
    ]);
    let help_modal = HelpModal::new("Audio", help_content);
    Self {
      backends,
      help_modal,
    }
  }
  pub fn get_audio_info<'a>(idx: usize) -> InfoBox<'a> {
    match idx {
      0 => InfoBox::new(
        "PipeWire",
        styled_block(vec![
          vec![
            (HIGHLIGHT, "PipeWire"),
            (None, " is a "),
            (HIGHLIGHT, "modern, low-latency audio server"),
            (None, " that serves as a replacement for both "),
            (HIGHLIGHT, "PulseAudio and JACK"),
            (None, "."),
          ],
          vec![
            (None, "It provides "),
            (HIGHLIGHT, "professional audio capabilities"),
            (None, " with "),
            (HIGHLIGHT, "minimal latency"),
            (None, " and supports "),
            (HIGHLIGHT, "advanced audio routing"),
            (None, " and "),
            (HIGHLIGHT, "real-time processing"),
            (None, "."),
          ],
          vec![
            (None, "PipeWire offers "),
            (HIGHLIGHT, "excellent compatibility"),
            (None, " with existing applications and is the "),
            (HIGHLIGHT, "recommended choice"),
            (None, " for modern Linux systems."),
          ],
        ]),
      ),
      1 => InfoBox::new(
        "PulseAudio",
        styled_block(vec![
          vec![
            (HIGHLIGHT, "PulseAudio"),
            (None, " is a "),
            (HIGHLIGHT, "mature and stable audio server"),
            (
              None,
              " that has been the standard audio system on Linux for many years.",
            ),
          ],
          vec![
            (None, "It provides "),
            (HIGHLIGHT, "network audio streaming"),
            (None, ", "),
            (HIGHLIGHT, "device management"),
            (None, ", and "),
            (HIGHLIGHT, "per-application volume control"),
            (None, " with good application compatibility."),
          ],
          vec![
            (None, "PulseAudio is "),
            (HIGHLIGHT, "well-tested and reliable"),
            (None, " but may have "),
            (HIGHLIGHT, "higher latency"),
            (None, " compared to newer solutions like PipeWire."),
          ],
        ]),
      ),
      _ => InfoBox::new(
        "Unknown Audio Backend",
        styled_block(vec![vec![(
          None,
          "No information available for this audio backend.",
        )]]),
      ),
    }
  }
  pub fn display_widget(installer: &mut Installer) -> Option<Box<dyn ConfigWidget>> {
    installer.audio_backend.clone().map(|s| {
      let ib = InfoBox::new(
        "",
        styled_block(vec![
          vec![(None, "Current audio backend set to:")],
          vec![(HIGHLIGHT, &s)],
        ]),
      );
      Box::new(ib) as Box<dyn ConfigWidget>
    })
  }
  pub fn page_info<'a>() -> (String, Vec<Line<'a>>) {
    (
      "Audio".to_string(),
      styled_block(vec![
        vec![(
          None,
          "Select the audio management backend to be installed on your system.",
        )],
        vec![(
          None,
          "The audio backend is responsible for managing sound devices and providing audio services to applications.",
        )],
        vec![(
          None,
          "Choosing an audio backend can help ensure that your system is able to handle audio playback and recording effectively.",
        )],
      ]),
    )
  }
}

impl Default for Audio {
  fn default() -> Self {
    Self::new()
  }
}

impl Page for Audio {
  fn render(&mut self, _installer: &mut Installer, f: &mut Frame, area: Rect) {
    let vert_chunks = Layout::default()
      .direction(Direction::Vertical)
      .constraints([Constraint::Percentage(50), Constraint::Percentage(50)].as_ref())
      .split(area);
    let hor_chunks = split_hor!(
      vert_chunks[0],
      1,
      [
        Constraint::Percentage(40),
        Constraint::Percentage(20),
        Constraint::Percentage(40),
      ]
    );

    let idx = self.backends.selected_idx;
    let info_box = Self::get_audio_info(idx);
    self.backends.render(f, hor_chunks[1]);
    if idx < 2 {
      info_box.render(f, vert_chunks[1]);
    }

    self.help_modal.render(f, area);
  }

  fn get_help_content(&self) -> (String, Vec<Line<'_>>) {
    let help_content = styled_block(vec![
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "↑/↓, j/k"),
        (None, " - Navigate audio backend options"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Enter"),
        (None, " - Select audio backend and return"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Esc, q, ←, h"),
        (None, " - Cancel and return to menu"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "?"),
        (None, " - Show this help"),
      ],
      vec![(None, "")],
      vec![(
        None,
        "Select the audio management backend for sound devices.",
      )],
    ]);
    ("Audio".to_string(), help_content)
  }

  fn handle_input(&mut self, installer: &mut Installer, event: KeyEvent) -> Signal {
    match event.code {
      KeyCode::Char('?') => {
        self.help_modal.toggle();
        Signal::Wait
      }
      ui_close!() if self.help_modal.visible => {
        self.help_modal.hide();
        Signal::Wait
      }
      _ if self.help_modal.visible => Signal::Wait,
      ui_back!() => Signal::Pop,
      KeyCode::Enter => {
        installer.audio_backend = Some(self.backends.items[self.backends.selected_idx].clone());
        Signal::Pop
      }
      ui_up!() => {
        if !self.backends.previous_item() {
          self.backends.last_item();
        }
        Signal::Wait
      }
      ui_down!() => {
        if !self.backends.next_item() {
          self.backends.first_item();
        }
        Signal::Wait
      }
      _ => self.backends.handle_input(event),
    }
  }
}

pub struct Network {
  backends: StrList,
  help_modal: HelpModal<'static>,
}

impl Network {
  pub fn new() -> Self {
    let backends = [
      "NetworkManager",
      "wpa_supplicant",
      "systemd-networkd",
      "None",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect::<Vec<_>>();
    let mut backends = StrList::new("Select Network Backend", backends);
    backends.focus();
    let help_content = styled_block(vec![
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "↑/↓, j/k"),
        (None, " - Navigate network backend options"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Enter"),
        (None, " - Select network backend and return"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Esc, q, ←, h"),
        (None, " - Cancel and return to menu"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "?"),
        (None, " - Show this help"),
      ],
      vec![(None, "")],
      vec![(
        None,
        "Select the network management backend for connections.",
      )],
    ]);
    let help_modal = HelpModal::new("Network", help_content);
    Self {
      backends,
      help_modal,
    }
  }
  pub fn get_network_info<'a>(idx: usize) -> InfoBox<'a> {
    match idx {
      0 => InfoBox::new(
        "NetworkManager",
        styled_block(vec![
          vec![
            (HIGHLIGHT, "NetworkManager"),
            (None, " is a "),
            (HIGHLIGHT, "comprehensive network management daemon"),
            (None, " that provides "),
            (HIGHLIGHT, "automatic network configuration"),
            (None, " and "),
            (HIGHLIGHT, "seamless connectivity management"),
            (None, "."),
          ],
          vec![
            (None, "It supports "),
            (HIGHLIGHT, "WiFi, Ethernet, VPN, and mobile broadband"),
            (None, " connections with "),
            (HIGHLIGHT, "automatic switching"),
            (None, " between available networks."),
          ],
          vec![
            (None, "NetworkManager provides "),
            (HIGHLIGHT, "GUI integration"),
            (None, " and is the "),
            (HIGHLIGHT, "most user-friendly option"),
            (None, " for desktop environments."),
          ],
        ]),
      ),
      1 => InfoBox::new(
        "wpa_supplicant",
        styled_block(vec![
          vec![
            (HIGHLIGHT, "wpa_supplicant"),
            (None, " is a "),
            (HIGHLIGHT, "lightweight WiFi authentication client"),
            (None, " that handles "),
            (HIGHLIGHT, "WPA/WPA2 and WPA3 security protocols"),
            (None, "."),
          ],
          vec![
            (None, "It provides "),
            (HIGHLIGHT, "minimal overhead"),
            (None, " and "),
            (HIGHLIGHT, "direct control"),
            (None, " over wireless connections but requires "),
            (HIGHLIGHT, "manual configuration"),
            (None, " for most setups."),
          ],
          vec![
            (None, "wpa_supplicant is "),
            (HIGHLIGHT, "ideal for servers"),
            (None, " or users who prefer "),
            (HIGHLIGHT, "command-line network management"),
            (None, " with minimal dependencies."),
          ],
        ]),
      ),
      2 => InfoBox::new(
        "systemd-networkd",
        styled_block(vec![
          vec![
            (HIGHLIGHT, "systemd-networkd"),
            (None, " is a "),
            (HIGHLIGHT, "systemd-native network manager"),
            (None, " that provides "),
            (HIGHLIGHT, "efficient and lightweight"),
            (None, " network configuration."),
          ],
          vec![
            (None, "It offers "),
            (HIGHLIGHT, "declarative configuration"),
            (
              None,
              " through configuration files and integrates well with ",
            ),
            (HIGHLIGHT, "systemd-resolved"),
            (None, " for DNS management."),
          ],
          vec![
            (None, "systemd-networkd is "),
            (HIGHLIGHT, "perfect for servers"),
            (None, " and "),
            (HIGHLIGHT, "headless systems"),
            (
              None,
              " but has limited support for complex desktop networking scenarios.",
            ),
          ],
        ]),
      ),
      _ => InfoBox::new(
        "Unknown Network Backend",
        styled_block(vec![vec![(
          None,
          "No information available for this network backend.",
        )]]),
      ),
    }
  }
  pub fn display_widget(installer: &mut Installer) -> Option<Box<dyn ConfigWidget>> {
    installer.network_backend.clone().map(|s| {
      let ib = InfoBox::new(
        "",
        styled_block(vec![
          vec![(None, "Current network backend set to:")],
          vec![(HIGHLIGHT, &s)],
        ]),
      );
      Box::new(ib) as Box<dyn ConfigWidget>
    })
  }
  pub fn page_info<'a>() -> (String, Vec<Line<'a>>) {
    (
      "Network".to_string(),
      styled_block(vec![
        vec![(
          None,
          "Select the network management backend to be installed on your system.",
        )],
        vec![(
          None,
          "The network backend is responsible for managing network connections and settings on your system.",
        )],
        vec![(
          None,
          "Choosing a network backend can help ensure that your system is able to connect to and manage network interfaces effectively.",
        )],
      ]),
    )
  }
}

impl Default for Network {
  fn default() -> Self {
    Self::new()
  }
}

impl Page for Network {
  fn render(&mut self, _installer: &mut Installer, f: &mut Frame, area: Rect) {
    let vert_chunks = Layout::default()
      .direction(Direction::Vertical)
      .constraints([Constraint::Percentage(50), Constraint::Percentage(50)].as_ref())
      .split(area);
    let hor_chunks = split_hor!(
      vert_chunks[0],
      1,
      [
        Constraint::Percentage(40),
        Constraint::Percentage(20),
        Constraint::Percentage(40),
      ]
    );

    let idx = self.backends.selected_idx;
    let info_box = Self::get_network_info(idx);
    self.backends.render(f, hor_chunks[1]);
    if idx < 3 {
      info_box.render(f, vert_chunks[1]);
    }

    self.help_modal.render(f, area);
  }

  fn get_help_content(&self) -> (String, Vec<Line<'_>>) {
    let help_content = styled_block(vec![
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "↑/↓, j/k"),
        (None, " - Navigate network backend options"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Enter"),
        (None, " - Select network backend and return"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Esc, q, ←, h"),
        (None, " - Cancel and return to menu"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "?"),
        (None, " - Show this help"),
      ],
      vec![(None, "")],
      vec![(
        None,
        "Select the network management backend for connections.",
      )],
    ]);
    ("Network".to_string(), help_content)
  }

  fn handle_input(&mut self, installer: &mut Installer, event: KeyEvent) -> Signal {
    match event.code {
      KeyCode::Char('?') => {
        self.help_modal.toggle();
        Signal::Wait
      }
      ui_close!() if self.help_modal.visible => {
        self.help_modal.hide();
        Signal::Wait
      }
      _ if self.help_modal.visible => Signal::Wait,
      ui_back!() => Signal::Pop,
      KeyCode::Enter => {
        installer.network_backend = Some(self.backends.items[self.backends.selected_idx].clone());
        Signal::Pop
      }
      ui_up!() => {
        if !self.backends.previous_item() {
          self.backends.last_item();
        }
        Signal::Wait
      }
      ui_down!() => {
        if !self.backends.next_item() {
          self.backends.first_item();
        }
        Signal::Wait
      }
      _ => self.backends.handle_input(event),
    }
  }
}

pub struct Timezone {
  timezones: StrList,
  help_modal: HelpModal<'static>,
}

impl Timezone {
  pub fn new() -> Self {
    let timezones = vec![
      "UTC",
      "America/New_York",
      "America/Los_Angeles",
      "America/Chicago",
      "America/Denver",
      "Europe/London",
      "Europe/Berlin",
      "Europe/Paris",
      "Europe/Moscow",
      "Asia/Tokyo",
      "Asia/Shanghai",
      "Asia/Kolkata",
      "Asia/Dubai",
      "Australia/Sydney",
      "Australia/Melbourne",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect::<Vec<_>>();
    let mut timezones = StrList::new("Select Timezone", timezones);
    timezones.focus();
    let help_content = styled_block(vec![
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "↑/↓, j/k"),
        (None, " - Navigate timezone options"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Enter"),
        (None, " - Select timezone and return"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Esc, q, ←, h"),
        (None, " - Cancel and return to menu"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "?"),
        (None, " - Show this help"),
      ],
      vec![(None, "")],
      vec![(
        None,
        "Select the timezone that matches your physical location.",
      )],
    ]);
    let help_modal = HelpModal::new("Timezone", help_content);
    Self {
      timezones,
      help_modal,
    }
  }
  pub fn display_widget(installer: &mut Installer) -> Option<Box<dyn ConfigWidget>> {
    installer.timezone.clone().map(|s| {
      let ib = InfoBox::new(
        "",
        styled_block(vec![
          vec![(None, "Current timezone set to:")],
          vec![(HIGHLIGHT, &s)],
        ]),
      );
      Box::new(ib) as Box<dyn ConfigWidget>
    })
  }
  pub fn page_info<'a>() -> (String, Vec<Line<'a>>) {
    (
      "Timezone".to_string(),
      styled_block(vec![
        vec![(None, "Select the timezone for your system.")],
        vec![(
          None,
          "The timezone setting determines the local time displayed on your system and is important for scheduling tasks and logging events.",
        )],
        vec![(
          None,
          "Choose a timezone that matches your physical location or the location where the system will primarily be used.",
        )],
      ]),
    )
  }
}

impl Default for Timezone {
  fn default() -> Self {
    Self::new()
  }
}

impl Page for Timezone {
  fn render(&mut self, _installer: &mut Installer, f: &mut Frame, area: Rect) {
    let chunks = split_vert!(area, 1, [Constraint::Percentage(100)]);
    self.timezones.render(f, chunks[0]);
    self.help_modal.render(f, area);
  }

  fn get_help_content(&self) -> (String, Vec<Line<'_>>) {
    let help_content = styled_block(vec![
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "↑/↓, j/k"),
        (None, " - Navigate timezone options"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Enter"),
        (None, " - Select timezone and return"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Esc, q, ←, h"),
        (None, " - Cancel and return to menu"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "?"),
        (None, " - Show this help"),
      ],
      vec![(None, "")],
      vec![(
        None,
        "Select the timezone that matches your physical location.",
      )],
    ]);
    ("Timezone".to_string(), help_content)
  }

  fn handle_input(&mut self, installer: &mut Installer, event: KeyEvent) -> Signal {
    match event.code {
      KeyCode::Char('?') => {
        self.help_modal.toggle();
        Signal::Wait
      }
      ui_close!() if self.help_modal.visible => {
        self.help_modal.hide();
        Signal::Wait
      }
      _ if self.help_modal.visible => Signal::Wait,
      ui_back!() => Signal::Pop,
      KeyCode::Enter => {
        installer.timezone = Some(self.timezones.items[self.timezones.selected_idx].clone());
        Signal::Pop
      }
      ui_up!() => {
        if !self.timezones.previous_item() {
          self.timezones.last_item();
        }
        Signal::Wait
      }
      ui_down!() => {
        if !self.timezones.next_item() {
          self.timezones.first_item();
        }
        Signal::Wait
      }
      _ => self.timezones.handle_input(event),
    }
  }
}

pub struct ConfigPreview {
  system_config: String,
  disko_config: String,
  _flake_path: Option<String>,
  scroll_position: usize,
  button_row: WidgetBox,
  current_view: ConfigView,
  help_modal: HelpModal<'static>,
  visible_lines: usize,
}

#[derive(Clone, Copy, PartialEq)]
enum ConfigView {
  System,
  Disko,
}

impl ConfigPreview {
  /// Maximum scroll distance for config preview window
  fn get_max_scroll(&self, visible_lines: usize) -> usize {
    let config_content = match self.current_view {
      ConfigView::System => &self.system_config,
      ConfigView::Disko => &self.disko_config,
    };
    let lines = config_content.lines().count();
    lines.saturating_sub(visible_lines)
  }

  pub fn new(installer: &mut Installer) -> anyhow::Result<Self> {
    // Generate the configuration like the main app does
    let config_json = installer.to_json()?;
    let serializer = crate::nixgen::NixWriter::new(config_json);

    let configs = serializer.write_configs()?;

    let buttons: Vec<Box<dyn ConfigWidget>> = vec![
      Box::new(Button::new("Begin Installation")),
      Box::new(Button::new("Back")),
    ];
    let button_row = WidgetBox::button_menu(buttons);
    let help_content = styled_block(vec![
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "1/2"),
        (None, " - Switch between System/Disko config"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "↑/↓, j/k"),
        (None, " - Scroll config content"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Page Up/Down"),
        (None, " - Scroll page by page"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Tab"),
        (None, " - Switch to buttons"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Enter"),
        (None, " - Activate selected button"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Esc"),
        (None, " - Go back to menu"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "?"),
        (None, " - Show this help"),
      ],
      vec![(None, "")],
      vec![(
        None,
        "Review the generated NixOS configuration before saving.",
      )],
    ]);
    let help_modal = HelpModal::new("Config Preview", help_content);

    Ok(Self {
      system_config: configs.system,
      disko_config: configs.disko,
      _flake_path: configs.flake_path,
      scroll_position: 0,
      button_row,
      current_view: ConfigView::System,
      help_modal,
      visible_lines: 10, // Default value, will be updated during rendering
    })
  }
}

impl Page for ConfigPreview {
  fn render(&mut self, _installer: &mut Installer, f: &mut Frame, area: Rect) {
    let chunks = split_vert!(
      area,
      1,
      [
        Constraint::Length(3), // Tab bar
        Constraint::Min(0),    // Config content
        Constraint::Length(3), // Buttons
      ]
    );

    // Tab bar for switching between system and disko config
    let tab_chunks = split_hor!(
      chunks[0],
      0,
      [Constraint::Percentage(50), Constraint::Percentage(50)]
    );

    // System config tab
    let system_tab_style = if self.current_view == ConfigView::System {
      Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD)
    } else {
      Style::default().fg(Color::Gray)
    };
    let system_tab = Paragraph::new("System Config [1]")
      .style(system_tab_style)
      .alignment(Alignment::Center)
      .block(Block::default().borders(Borders::ALL));
    f.render_widget(system_tab, tab_chunks[0]);

    // Disko config tab
    let disko_tab_style = if self.current_view == ConfigView::Disko {
      Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD)
    } else {
      Style::default().fg(Color::Gray)
    };
    let disko_tab = Paragraph::new("Disko Config [2]")
      .style(disko_tab_style)
      .alignment(Alignment::Center)
      .block(Block::default().borders(Borders::ALL));
    f.render_widget(disko_tab, tab_chunks[1]);

    // Config content
    let config_content = match self.current_view {
      ConfigView::System => highlight_nix(&self.system_config).unwrap_or_default(),
      ConfigView::Disko => highlight_nix(&self.disko_config).unwrap_or_default(),
    };
    log::debug!("Rendering config preview with text {config_content:?}");

    let lines: Vec<Line<'_>> = config_content.into_text().unwrap().lines;
    let visible_lines = chunks[1].height as usize - 2; // Account for borders
    self.visible_lines = visible_lines;

    let start_line = self.scroll_position;
    let end_line = std::cmp::min(start_line + visible_lines, lines.len());
    let display_lines = lines[start_line..end_line].to_vec();

    let config_paragraph = Paragraph::new(display_lines)
      .block(Block::default().borders(Borders::ALL).title(format!(
        "Preview - {} Config (Scroll: {}/{})",
        match self.current_view {
          ConfigView::System => "System",
          ConfigView::Disko => "Disko",
        },
        start_line + 1,
        self.get_max_scroll(visible_lines) + 1
      )))
      .wrap(Wrap { trim: false });
    f.render_widget(config_paragraph, chunks[1]);

    // Buttons
    self.button_row.render(f, chunks[2]);

    // Help modal
    self.help_modal.render(f, area);
  }

  fn get_help_content(&self) -> (String, Vec<Line<'_>>) {
    let help_content = styled_block(vec![
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "1/2"),
        (None, " - Switch between System/Disko config"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "↑/↓, j/k"),
        (None, " - Scroll config content"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Page Up/Down"),
        (None, " - Scroll page by page"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Tab"),
        (None, " - Switch to buttons"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Enter"),
        (None, " - Activate selected button"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Esc"),
        (None, " - Go back to menu"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "?"),
        (None, " - Show this help"),
      ],
      vec![(None, "")],
      vec![(
        None,
        "Review the generated NixOS configuration before saving.",
      )],
    ]);
    ("Config Preview".to_string(), help_content)
  }

  fn handle_input(&mut self, _installer: &mut Installer, event: KeyEvent) -> Signal {
    match event.code {
      KeyCode::Char('?') => {
        self.help_modal.toggle();
        Signal::Wait
      }
      ui_close!() if self.help_modal.visible => {
        self.help_modal.hide();
        Signal::Wait
      }
      _ if self.help_modal.visible => Signal::Wait,
      KeyCode::Char('1') => {
        self.button_row.unfocus();
        self.current_view = ConfigView::System;
        self.scroll_position = 0;
        Signal::Wait
      }
      KeyCode::Char('2') => {
        self.button_row.unfocus();
        self.current_view = ConfigView::Disko;
        self.scroll_position = 0;
        Signal::Wait
      }
      ui_up!() => {
        if self.button_row.is_focused() {
          if !self.button_row.prev_child() {
            self.button_row.unfocus();
          }
        } else if self.scroll_position > 0 {
          self.scroll_position -= 1;
        }
        Signal::Wait
      }
      ui_down!() => {
        if self.button_row.is_focused() {
          self.button_row.next_child();
        } else {
          let max_scroll = self.get_max_scroll(self.visible_lines);
          if self.scroll_position < max_scroll {
            self.scroll_position += 1;
          } else if !self.button_row.is_focused() {
            self.button_row.focus();
          }
        }
        Signal::Wait
      }
      ui_right!() => {
        if self.button_row.is_focused() {
          if !self.button_row.next_child() {
            self.button_row.first_child();
          }
        } else if self.current_view == ConfigView::System {
          self.current_view = ConfigView::Disko;
          self.scroll_position = 0;
        } else if self.current_view == ConfigView::Disko {
          self.current_view = ConfigView::System;
          self.scroll_position = 0;
        }

        Signal::Wait
      }
      ui_left!() => {
        if self.button_row.is_focused() {
          if !self.button_row.prev_child() {
            self.button_row.last_child();
          }
        } else if self.current_view == ConfigView::Disko {
          self.current_view = ConfigView::System;
          self.scroll_position = 0;
        } else if self.current_view == ConfigView::System {
          self.current_view = ConfigView::Disko;
          self.scroll_position = 0;
        }

        Signal::Wait
      }
      KeyCode::PageUp => {
        self.scroll_position = self.scroll_position.saturating_sub(10);
        Signal::Wait
      }
      KeyCode::PageDown => {
        let max_scroll = self.get_max_scroll(self.visible_lines);
        self.scroll_position = std::cmp::min(self.scroll_position + 10, max_scroll);
        Signal::Wait
      }
      KeyCode::Tab => {
        self.button_row.focus();
        Signal::Wait
      }
      KeyCode::Enter => {
        if self.button_row.is_focused() {
          match self.button_row.selected_child() {
            Some(0) => Signal::WriteCfg, // Save & Exit
            Some(1) => Signal::Pop,      // Back
            _ => Signal::Wait,
          }
        } else {
          Signal::Wait
        }
      }
      KeyCode::Esc => Signal::Pop,
      _ => {
        if self.button_row.is_focused() {
          self.button_row.handle_input(event)
        } else {
          Signal::Wait
        }
      }
    }
  }
}

pub struct InstallProgress<'a> {
  _installer: Installer,
  steps: InstallSteps<'a>,
  log_box: LogBox<'a>,
  progress_bar: ProgressBar,
  help_modal: HelpModal<'static>,
  signal: Option<Signal>,

  // we only hold onto these to keep them alive during installation
  _system_cfg: NamedTempFile,
  _disko_cfg: NamedTempFile,
  _log_file: NamedTempFile,
}

impl<'a> InstallProgress<'a> {
  pub fn new(
    installer: Installer,
    system_cfg: NamedTempFile,
    disko_cfg: NamedTempFile,
  ) -> anyhow::Result<Self> {
    let log_file = NamedTempFile::new()?;
    let log_path = log_file
      .path()
      .to_str()
      .ok_or_else(|| anyhow::anyhow!("Invalid log file path"))?
      .to_string();
    let install_steps = Self::install_commands(
      &installer,
      system_cfg
        .path()
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("Invalid system config path"))?
        .to_string(),
      disko_cfg
        .path()
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("Invalid disko config path"))?
        .to_string(),
      log_path.clone(),
    )?;
    let steps = InstallSteps::new("Install Steps", install_steps);
    let progress_bar = ProgressBar::new("Progress", 0);

    let help_content = styled_block(vec![
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "↑/↓, j/k"),
        (None, " - Navigate through installation steps"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Esc"),
        (None, " - Exit installation (if completed)"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "?"),
        (None, " - Show this help"),
      ],
      vec![(None, "")],
      vec![(
        None,
        "This page shows the progress of the NixOS installation process.",
      )],
      vec![(
        None,
        "Installation steps are executed sequentially and their status is shown above.",
      )],
    ]);
    let help_modal = HelpModal::new("Installation Progress", help_content);

    let mut log_box = LogBox::new("Logs".into());
    log_box.open_log(log_path)?;

    Ok(Self {
      _installer: installer,
      steps,
      progress_bar,
      log_box,
      help_modal,
      signal: None,
      _system_cfg: system_cfg,
      _disko_cfg: disko_cfg,
      _log_file: log_file,
    })
  }

  pub fn is_complete(&self) -> bool {
    self.steps.is_complete()
  }

  pub fn has_error(&self) -> bool {
    self.steps.has_error()
  }

  /// The actual installation steps
  fn install_commands(
    installer: &Installer,
    system_cfg_path: String,
    disk_cfg_path: String,
    log_file_path: String,
  ) -> anyhow::Result<Vec<(Line<'static>, VecDeque<Command>)>> {
    if installer.dry_run {
      return Ok(vec![
        (Line::from("[DRY RUN] Beginning NixOS Installation..."),
        vec![
        command!("sh", "-c", format!("echo '[DRY RUN] Beginning NixOS Installation...' > {log_file_path}")),
        command!("sleep", "1"),
        ].into()),
        (Line::from("[DRY RUN] Configuring disk layout (skipped)..."),
        vec![
        command!("sh", "-c", format!("echo '[DRY RUN] Would run: disko --yes-wipe-all-disks --mode destroy,format,mount {disk_cfg_path}' > {log_file_path}")),
        command!("sh", "-c", format!("echo '[DRY RUN] Disko config contents:' >> {log_file_path} && cat {disk_cfg_path} >> {log_file_path}")),
        ].into()),
        (Line::from("[DRY RUN] Building NixOS configuration (skipped)..."),
        vec![
        command!("sh", "-c", format!("echo '[DRY RUN] Would run: nixos-generate-config --root /mnt' > {log_file_path}")),
        command!("sh", "-c", format!("echo '[DRY RUN] Would run: cp {system_cfg_path} /mnt/etc/nixos/configuration.nix' >> {log_file_path}")),
        command!("sh", "-c", format!("echo '[DRY RUN] System config contents:' >> {log_file_path} && cat {system_cfg_path} >> {log_file_path}")),
        ].into()),
        (Line::from("[DRY RUN] Installing NixOS (skipped)..."),
        vec![
        command!("sh", "-c", format!("echo '[DRY RUN] Would run: nixos-install --root /mnt' > {log_file_path}")),
        ].into()),
        (Line::from("[DRY RUN] Finalizing..."),
        vec![
        command!("sh", "-c", format!("echo '[DRY RUN] Installation dry run complete! No changes were made.' > {log_file_path}")),
        ].into()),
      ]);
    }

    Ok(vec![
			(Line::from("Beginning NixOS Installation..."),
			vec![
			command!("sh", "-c", format!("echo Beginning NixOS Installation... 2>&1 > {log_file_path}")),
			command!("sleep", "1"),
			].into()),
			(Line::from("Configuring disk layout..."),
			vec![
			command!("sh", "-c", format!("echo Partitioning disks... 2>&1 > {log_file_path}")),
			command!("sh", "-c", format!("disko --yes-wipe-all-disks --mode destroy,format,mount {disk_cfg_path} 2>&1 > {log_file_path}")),
			].into()),
			(Line::from("Building NixOS configuration..."),
			vec![
			command!("sh", "-c", format!("echo Building NixOS configuration... 2>&1 > {log_file_path}")),
			command!("sh", "-c", format!("nixos-generate-config --root /mnt 2>&1 > {log_file_path}")),
			command!("sh", "-c", format!("cp -v {system_cfg_path} /mnt/etc/nixos/configuration.nix 2>&1 > {log_file_path}")),
			command!("sh", "-c", format!("echo Build completed 2>&1 > {log_file_path}")),
			].into()),
			(Line::from("Installing NixOS..."),
			vec![
			command!("sh", "-c", format!("echo Installing NixOS... 2>&1 > {log_file_path}")),
			command!("sh", "-c", format!("nixos-install --root /mnt 2>&1 > {log_file_path}")),
			].into()),
			(Line::from("Importing channels..."),
			vec![
			command!("sh", "-c", format!("echo Importing NixOS channels... 2>&1 > {log_file_path}")),
			command!("sh", "-c", format!("nixos-enter -- nix-channel --add https://nixos.org/channels/nixos-unstable nixos 2>&1 > {log_file_path}")),
			command!("sh", "-c", format!("nixos-enter -- nix-channel --update 2>&1 > {log_file_path}")),
			].into()),
			(Line::from("Finalizing installation..."),
			vec![
			command!("sleep", "1"),
			command!("sh", "-c", format!("echo Installation complete! 2>&1 > {log_file_path}")),
			].into()),
			])
  }
}

impl<'a> Page for InstallProgress<'a> {
  fn render(&mut self, _installer: &mut Installer, f: &mut Frame, area: Rect) {
    // Tick the steps to update animation and process commands
    let _ = self.steps.tick();
    let _ = self.log_box.poll_log();

    let chunks = split_vert!(area, 1, [Constraint::Min(0), Constraint::Length(3)]);
    let hor_chunks = split_hor!(
      chunks[0],
      1,
      [Constraint::Percentage(30), Constraint::Percentage(70)]
    );

    // Render InstallSteps widget in the main area
    self.steps.render(f, hor_chunks[0]);
    self.log_box.render(f, hor_chunks[1]);

    // Update progress bar with completion percentage
    let progress = (self.steps.progress() * 100.0) as u32;
    if progress == 100 || self.steps.is_complete() {
      self.signal = Some(Signal::Push(Box::new(InstallComplete::new())));
    }
    self.progress_bar.set_progress(progress);
    self.progress_bar.render(f, chunks[1]);

    // Help modal
    self.help_modal.render(f, area);
  }

  fn signal(&self) -> Option<Signal> {
    // This lets us return a signal without any input
    if let Some(ref signal) = self.signal {
      match signal {
        Signal::Wait => Some(Signal::Wait),
        Signal::Push(_) => Some(Signal::Push(Box::new(InstallComplete::new()))),
        Signal::Pop => Some(Signal::Pop),
        Signal::PopCount(n) => Some(Signal::PopCount(*n)),
        Signal::Quit => Some(Signal::Quit),
        Signal::WriteCfg => Some(Signal::WriteCfg),
        Signal::Unwind => Some(Signal::Unwind),
        Signal::Error(_) => Some(Signal::Wait),
        Signal::PopAndPush(_) => Some(Signal::PopAndPush(Box::new(InstallComplete::new()))),
      }
    } else {
      None
    }
  }

  fn get_help_content(&self) -> (String, Vec<Line<'_>>) {
    let help_content = styled_block(vec![
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "↑/↓, j/k"),
        (None, " - Scroll through command output"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Page Up/Down"),
        (None, " - Scroll output page by page"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Home/End"),
        (None, " - Jump to beginning/end of output"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "Esc"),
        (None, " - Exit installation (if completed)"),
      ],
      vec![
        (Some((Color::Yellow, Modifier::BOLD)), "?"),
        (None, " - Show this help"),
      ],
      vec![(None, "")],
      vec![(None, "Watch the progress as NixOS installs. Commands run")],
      vec![(None, "sequentially and their output is logged above.")],
    ]);
    ("Installation Progress".to_string(), help_content)
  }

  fn handle_input(&mut self, _installer: &mut Installer, event: KeyEvent) -> Signal {
    if event.code == KeyCode::Char('c') && event.modifiers.contains(KeyModifiers::CONTROL) {
      return Signal::Quit;
    }
    if self.has_error() {
      match event.code {
        KeyCode::Esc => Signal::Pop,
        KeyCode::Char('q') => Signal::Pop,
        _ => Signal::Wait,
      }
    } else {
      Signal::Wait
    }
  }
}

pub struct InstallComplete {
  text_box: InfoBox<'static>,
}

impl InstallComplete {
  pub fn new() -> Self {
    let content = styled_block(vec![
      vec![(
        None,
        "NixOS has been successfully installed on your system!",
      )],
      vec![(None, "")],
      vec![(
        None,
        "You can now reboot your computer and remove the installation media.",
      )],
      vec![(None, "")],
      vec![(
        None,
        "The installation remains mounted on /mnt if you wish to perform any manual configuration on the new system.",
      )],
      vec![(
        None,
        "Such manual configuration can be performed using the 'nixos-enter' command.",
      )],
      vec![(None, "")],
      vec![(None, "Press any key to exit the installer.")],
    ]);
    let text_box = InfoBox::new("Installation Complete", content);
    Self { text_box }
  }
}

impl Default for InstallComplete {
  fn default() -> Self {
    Self::new()
  }
}

impl Page for InstallComplete {
  fn render(&mut self, _installer: &mut Installer, f: &mut Frame, area: Rect) {
    let chunks = split_vert!(area, 1, [Constraint::Percentage(100)]);
    self.text_box.render(f, chunks[0]);
  }

  fn handle_input(&mut self, _installer: &mut Installer, _event: KeyEvent) -> Signal {
    Signal::Quit
  }
}
