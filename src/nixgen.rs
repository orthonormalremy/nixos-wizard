use serde_json::{Map, Value};
use std::process::{Command, Stdio};

use crate::{attrset, installer::users::User, merge_attrs};

/// Convert a value to a properly quoted Nix string literal
///
/// This helper function ensures proper escaping and quoting for Nix syntax.
/// Much cleaner than manually writing format!("\"{string}\"") everywhere.
pub fn nixstr(val: impl ToString) -> String {
  let val = val.to_string();
  format!("\"{val}\"")
}

/// Used for `system.stateVersion` when the running release can't be detected,
/// which normally means we aren't running on NixOS at all.
const FALLBACK_STATE_VERSION: &str = "25.11";

/// Detect the NixOS release series (e.g. `"26.11"`) of the live environment
///
/// `nixos-install` builds the target system from the live environment's
/// nixpkgs, so the release we are installing *from* is the correct
/// `system.stateVersion` for the system we are installing.
pub fn detect_state_version() -> String {
  let detected = nixos_version()
    .as_deref()
    .and_then(release_series)
    .or_else(|| os_release_version_id().as_deref().and_then(release_series));

  match detected {
    Some(version) => {
      log::debug!("Detected NixOS release series: {version}");
      version
    }
    None => {
      log::warn!(
        "Could not detect the running NixOS release, falling back to {FALLBACK_STATE_VERSION}"
      );
      FALLBACK_STATE_VERSION.to_string()
    }
  }
}

/// Ask NixOS itself, e.g. `26.11pre1040357.e2587caef70c (Xantusia)`
fn nixos_version() -> Option<String> {
  let output = Command::new("nixos-version").output().ok()?;
  if !output.status.success() {
    return None;
  }
  Some(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Fall back to `VERSION_ID="26.11pre1040357.e2587caef70c"` in os-release
///
/// Only trusted when the file actually describes NixOS. Every distro ships an
/// os-release, so reading `VERSION_ID` unconditionally would happily turn
/// Ubuntu's `24.04` into a plausible-looking but completely wrong
/// `system.stateVersion`.
fn os_release_version_id() -> Option<String> {
  let contents = std::fs::read_to_string("/etc/os-release").ok()?;

  let mut id = None;
  let mut version_id = None;
  for line in contents.lines() {
    if let Some(value) = line.strip_prefix("ID=") {
      id = Some(unquote(value));
    } else if let Some(value) = line.strip_prefix("VERSION_ID=") {
      version_id = Some(unquote(value));
    }
  }

  if id.as_deref() != Some("nixos") {
    log::warn!("/etc/os-release is not NixOS (ID={id:?}), ignoring its VERSION_ID");
    return None;
  }
  version_id
}

fn unquote(value: &str) -> String {
  value.trim().trim_matches('"').to_string()
}

/// Pull the `<major>.<minor>` release series out of a full NixOS version string
///
/// Handles both channel builds (`26.11pre1040357.e2587caef70c`) and local
/// builds (`26.11.20260723.e2587ca`); both yield `26.11`.
fn release_series(raw: &str) -> Option<String> {
  let (major, rest) = raw.trim().split_once('.')?;
  if major.is_empty() || !major.bytes().all(|b| b.is_ascii_digit()) {
    return None;
  }
  let minor: String = rest.chars().take_while(char::is_ascii_digit).collect();
  if minor.is_empty() {
    return None;
  }
  Some(format!("{major}.{minor}"))
}
/// Format Nix code using the nixfmt tool for proper indentation and style
///
/// Assumes nixfmt is available in the environment (provided by the Nix flake)
pub fn fmt_nix(nix: String) -> anyhow::Result<String> {
  // Spawn nixfmt process with piped input/output for formatting
  let mut nixfmt_child = Command::new("nixfmt")
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .spawn()?;

  // Send the unformatted Nix code to nixfmt's stdin
  if let Some(stdin) = nixfmt_child.stdin.as_mut() {
    use std::io::Write;
    stdin.write_all(nix.as_bytes())?;
  }

  // Wait for nixfmt to complete and capture the formatted output
  let output = nixfmt_child.wait_with_output()?;
  if output.status.success() {
    let formatted = String::from_utf8(output.stdout)?;
    Ok(formatted)
  } else {
    let err = String::from_utf8_lossy(&output.stderr);
    Err(anyhow::anyhow!("nixfmt failed: {}", err))
  }
}
/// Add syntax highlighting to Nix code using the bat tool
///
/// Useful for displaying formatted Nix configurations in the UI
pub fn highlight_nix(nix: &str) -> anyhow::Result<String> {
  // Spawn bat with Nix syntax highlighting
  let mut bat_child = Command::new("bat")
    .arg("-p") // Plain output (no line numbers)
    .arg("-f") // Force colored output
    .arg("-l")
    .arg("nix") // Use Nix syntax highlighting
    .stdin(Stdio::piped())
    .stdout(Stdio::piped())
    .spawn()?;
  if let Some(stdin) = bat_child.stdin.as_mut() {
    use std::io::Write;
    stdin.write_all(nix.as_bytes())?;
  }

  let output = bat_child.wait_with_output()?;
  if output.status.success() {
    let highlighted = String::from_utf8(output.stdout)?;
    Ok(highlighted)
  } else {
    let err = String::from_utf8_lossy(&output.stderr);
    Err(anyhow::anyhow!("bat failed: {}", err))
  }
}
// Example JSON configuration structure that this module processes:
// {
//   "config": {
//     "audio_backend": "PulseAudio",
//     "bootloader": "systemd-boot",
//     "desktop_environment": "KDE Plasma",
//     "hostname": "hostname",
//     "kernels": ["linux"],
//     "keyboard_layout": "us",
//     "locale": "en_US.UTF-8",
//     "network_backend": "NetworkManager",
//     "timezone": "America/New_York",
//     "users": [...],
//     "system_pkgs": [...]
//   },
//   "disko": { ... }
// }
/// Container for generated NixOS configuration files
#[derive(Debug)]
pub struct Configs {
  pub system: String,             // NixOS system configuration
  pub disko: String,              // Disk partitioning configuration
  pub flake_path: Option<String>, // Optional flake path for advanced users
}

/// Converts JSON configuration to NixOS configuration files
///
/// Takes structured configuration data and generates:
/// - NixOS system configuration (configuration.nix)
/// - Disko disk partitioning configuration
pub struct NixWriter {
  config: Value, // JSON configuration from the installer UI
}

impl NixWriter {
  pub fn new(config: Value) -> Self {
    Self { config }
  }
  /// Generate both system and disko configurations from the JSON config
  pub fn write_configs(&self) -> anyhow::Result<Configs> {
    // Generate disko (disk partitioning) configuration
    let disko = {
      let config = self.config["disko"].clone();
      self.write_disko_config(config)?
    };

    // Generate NixOS system configuration
    let sys_cfg = {
      let config = self.config["config"].clone();
      self.write_sys_config(config)?
    };

    // Extract optional flake path for advanced users
    let flake_path = self
      .config
      .get("flake_path")
      .and_then(|v| v.as_str().map(|s| s.to_string()));

    Ok(Configs {
      system: sys_cfg,
      disko,
      flake_path,
    })
  }
  /// Generate the main NixOS system configuration (configuration.nix)
  ///
  /// Processes each configuration option and converts it to appropriate Nix
  /// syntax
  pub fn write_sys_config(&self, config: Value) -> anyhow::Result<String> {
    // Ensure we have a valid JSON object to work with
    let Value::Object(ref cfg) = config else {
      return Err(anyhow::anyhow!("Config must be a JSON object"));
    };

    let mut cfg_attrs = String::from("{}"); // Start with empty attribute set
    let mut install_home_manager = false; // Track if home-manager is needed
    // Process each configuration key and generate corresponding Nix attributes
    for (key, value) in cfg.iter() {
      log::debug!("Processing config key: {key}");
      log::debug!("Config value: {value}");

      // Match configuration keys to their Nix configuration generators
      let parsed_config = match key.trim().to_lowercase().as_str() {
        "audio_backend" => value.as_str().map(Self::parse_audio),
        "bootloader" => {
          // Bootloader parsing can fail, so handle errors explicitly
          let res = value.as_str().map(Self::parse_bootloader);
          match res {
            Some(Ok(cfg)) => Some(cfg),
            Some(Err(e)) => return Err(e),
            None => None,
          }
        }
        "desktop_environment" => value.as_str().map(Self::parse_desktop_environment),
        "enable_flakes" => value
          .as_bool()
          .filter(|&b| b)
          .map(|_| Self::parse_enable_flakes()),
        "greeter" => None,
        "hostname" => value.as_str().map(Self::parse_hostname),
        "kernels" => value.as_array().map(Self::parse_kernels),
        "keyboard_layout" => value.as_str().map(Self::parse_kb_layout),
        "locale" => value.as_str().map(Self::parse_locale),
        "network_backend" => value.as_str().map(Self::parse_network_backend),
        "profile" => None,
        "root_passwd_hash" => Some(Self::parse_root_pass_hash(value)?),
        "ssh_config" => value.as_object().and_then(Self::parse_ssh_config),
        "system_pkgs" => value.as_array().map(Self::parse_system_packages),
        "timezone" => value.as_str().map(Self::parse_timezone),
        "use_swap" => value.as_bool().filter(|&b| b).map(|_| Self::parse_swap()),
        "users" => {
          // Parse user configurations and check if home-manager is needed
          let users: Vec<User> = serde_json::from_value(value.clone())?;
          install_home_manager = users.iter().any(|user| user.home_manager_cfg.is_some());
          Some(self.parse_users(users)?)
        }
        _ => {
          log::warn!("Unknown configuration key '{key}' - skipping");
          None
        }
      };

      // Merge the generated configuration into the main attribute set
      if let Some(config) = parsed_config {
        cfg_attrs = merge_attrs!(cfg_attrs, config);
      }
    }
    // Set up imports based on whether home-manager is needed
    let imports = if install_home_manager {
      String::from(r#"{imports = [ "${homeManagerSrc}/nixos" ./hardware-configuration.nix ];}"#)
    } else {
      String::from("{imports = [./hardware-configuration.nix];}")
    };

    // Set the NixOS state version (required for all configurations), matching
    // the release of the live environment we're installing from
    let state_version = attrset! {
      "system.stateVersion" = nixstr(detect_state_version());
    };

    // Combine all configuration attributes
    cfg_attrs = merge_attrs!(imports, cfg_attrs, state_version);

    // Take home-manager from nixpkgs' own pinned copy rather than fetching a
    // release branch ourselves: it's hash-pinned, substitutable from the binary
    // cache, always the revision this nixpkgs was tested against, and it tracks
    // the channel on later rebuilds - so there's no release string for us to
    // keep up to date.
    //
    // It has to be bound in a `let` rather than read off the `pkgs` module
    // argument: `imports` is resolved before module arguments exist, so
    // referencing `pkgs` there is an infinite recursion.
    let let_stmt = if install_home_manager {
      "let homeManagerSrc = (import <nixpkgs> { }).home-manager.src; in "
    } else {
      ""
    };

    // Generate the final Nix function and format it
    let raw = format!("{{ config, pkgs, ... }}: {let_stmt}{cfg_attrs}");

    // Format the generated Nix code for readability
    fmt_nix(raw)
  }
  /// Generate Disko configuration for disk partitioning
  ///
  /// Converts the disk layout into Disko's declarative partition format
  pub fn write_disko_config(&self, config: Value) -> anyhow::Result<String> {
    log::debug!("Writing Disko config: {config}");

    let disks = config
      .as_array()
      .ok_or_else(|| anyhow::anyhow!("Expected disko config to be an array of disk configs"))?;

    let mut disk_attrs = Vec::new();
    for disk in disks {
      let device = disk["device"].as_str().unwrap_or("/dev/sda");
      let disk_type = disk["type"].as_str().unwrap_or("disk");
      let content = Self::parse_disko_content(&disk["content"])?;

      // Derive a disko-friendly name from the device path (e.g. /dev/nvme0n1 -> nvme0n1)
      let disk_name = device.rsplit('/').next().unwrap_or("main");

      let disko_config = attrset! {
        "device" = nixstr(device);
        "type" = nixstr(disk_type);
        "content" = content;
      };

      disk_attrs.push(format!("disko.devices.disk.{disk_name} = {disko_config};"));
    }

    let raw = format!("{{ {} }}", disk_attrs.join(" "));
    fmt_nix(raw)
  }

  fn parse_root_pass_hash(content: &Value) -> anyhow::Result<String> {
    let hash = content
      .as_str()
      .ok_or_else(|| anyhow::anyhow!("Root password hash must be a string"))?;
    Ok(attrset! {
      "users.users.root.hashedPassword" = nixstr(hash);
    })
  }

  /// Parse the disk content structure for Disko
  ///
  /// Processes partition definitions and filesystem configurations
  fn parse_disko_content(content: &Value) -> anyhow::Result<String> {
    let content_type = content["type"].as_str().unwrap_or("gpt");
    let partitions = &content["partitions"];

    // Process each partition definition
    if let Some(partitions_obj) = partitions.as_object() {
      let mut partition_attrs = Vec::new();

      for (name, partition) in partitions_obj {
        let partition_config = Self::parse_partition(partition)?;
        partition_attrs.push(format!("{} = {};", nixstr(name), partition_config));
      }

      let partitions_attr = format!("{{ {} }}", partition_attrs.join(" "));

      Ok(attrset! {
        "type" = nixstr(content_type);
        "partitions" = partitions_attr;
      })
    } else {
      Ok(attrset! {
        "type" = nixstr(content_type);
      })
    }
  }

  fn parse_partition(partition: &Value) -> anyhow::Result<String> {
    let format = partition["format"]
      .as_str()
      .ok_or_else(|| anyhow::anyhow!("Missing required 'format' field in partition"))?;
    let mountpoint = partition["mountpoint"]
      .as_str()
      .ok_or_else(|| anyhow::anyhow!("Missing required 'mountpoint' field in partition"))?;
    let size = partition["size"]
      .as_str()
      .ok_or_else(|| anyhow::anyhow!("Missing required 'size' field in partition"))?;
    let part_type = partition.get("type").and_then(|v| v.as_str());
    log::debug!(
      "Parsing partition: format={format}, mountpoint={mountpoint}, size={size}, type={part_type:?}"
    );

    if let Some(part_type) = part_type {
      Ok(attrset! {
        type = nixstr(part_type);
        size = nixstr(size);
        content = attrset! {
          type = nixstr("filesystem");
          format = nixstr(format);
          mountpoint = nixstr(mountpoint);
        };
      })
    } else {
      Ok(attrset! {
        size = nixstr(size);
        content = attrset! {
          type = nixstr("filesystem");
          format = nixstr(format);
          mountpoint = nixstr(mountpoint);
        };
      })
    }
  }
  fn parse_ssh_config(value: &Map<String, Value>) -> Option<String> {
    /*
    The SshCfg struct has these fields:
    - enable: bool → services.openssh.enable
    - port: u16 → services.openssh.ports
    - password_auth: bool → services.openssh.settings.PasswordAuthentication
    - root_login: bool → services.openssh.settings.PermitRootLogin

    With default values of:
    - enable: false
    - port: 22
    - password_auth: true
    - root_login: false
    {
      # SSH Configuration
      services.openssh = {
        enable = true;           # corresponds to SshCfg.enable
        ports = [ 2222 ];        # corresponds to SshCfg.port
    (default 22)
        settings = {
          PasswordAuthentication = true;   # corresponds to
    SshCfg.password_auth
          PermitRootLogin = "yes";        # corresponds to
    SshCfg.root_login
        };
      };
    }
      */
    let enable = value["enable"].as_bool().unwrap_or(false);
    if !enable {
      return None;
    }
    let port = value["port"].as_u64().unwrap_or(22) as u16;
    let password_auth = value["password_auth"].as_bool().unwrap_or(true);
    let root_login = value["root_login"].as_bool().unwrap_or(false);
    let root_login_option = match root_login {
      true => "yes".to_string(),
      false => "no".to_string(),
    };

    let options = attrset! {
      enable = enable;
      ports = format!("[{}]", port);
      settings = attrset! {
        PasswordAuthentication = password_auth;
        PermitRootLogin = nixstr(root_login_option);
      };
    };

    Some(format!("{{ services.openssh = {options}; }}"))
  }
  fn parse_timezone(value: &str) -> String {
    attrset! {
      "time.timeZone" = nixstr(value);
    }
  }
  pub fn parse_network_backend(value: &str) -> String {
    match value.to_lowercase().as_str() {
      "networkmanager" => attrset! {
        "networking.networkmanager.enable" = true;
      },
      "wpa_supplicant" => attrset! {
        "networking.wireless.enable" = true;
      },
      "systemd-networkd" => attrset! {
        "networking.useNetworkd" = true;
        "systemd.network.enable" = true;
      },
      _ => String::new(),
    }
  }
  pub fn parse_locale(value: &str) -> String {
    attrset! {
      "i18n.defaultLocale" = nixstr(value);
    }
  }
  fn parse_kb_layout(value: &str) -> String {
    let (xkb, console) = match value {
      "us(qwerty)" => ("us", "us"),
      "us(dvorak)" => ("us", "dvorak"),
      "us(colemak)" => ("us", "colemak"),
      "uk" => ("gb", "uk"),
      "de" => ("de", "de"),
      "fr" => ("fr", "fr"),
      "es" => ("es", "es"),
      "it" => ("it", "it"),
      "ru" => ("ru", "ru"),
      "cn" => ("cn", "us"),
      "jp" => ("jp", "us"),
      "kr" => ("kr", "us"),
      "in" => ("in", "us"),
      "br" => ("br", "br-abnt2"),
      "nl" => ("nl", "nl"),
      "se" => ("se", "us"),
      "no" => ("no", "no"),
      "fi" => ("fi", "fi"),
      "dk" => ("dk", "dk"),
      "pl" => ("pl", "pl"),
      "tr" => ("tr", "trq"),
      "gr" => ("gr", "gr"),
      _ => ("us", "us"),
    };

    attrset! {
      "services.xserver.xkb.layout" = nixstr(xkb);
      "console.keyMap" = nixstr(console);
    }
  }

  #[allow(clippy::ptr_arg)]
  fn parse_kernels(kernels: &Vec<Value>) -> String {
    if kernels.is_empty() {
      return String::from("{}");
    }

    // Take the first kernel as the primary one
    if let Some(Value::String(kernel)) = kernels.first() {
      let kernel_pkg = match kernel.to_lowercase().as_str() {
        "linux" => "pkgs.linuxPackages",
        "linux_zen" => "pkgs.linuxPackages_zen",
        "linux_hardened" => "pkgs.linuxPackages_hardened",
        "linux_lts" => "pkgs.linuxPackages_lts",
        _ => "pkgs.linuxPackages", // Default fallback
      };
      attrset! {
        "boot.kernelPackages" = kernel_pkg;
      }
    } else {
      String::from("{}")
    }
  }
  fn parse_hostname(value: &str) -> String {
    attrset! {
      "networking.hostName" = nixstr(value);
    }
  }
  fn _parse_greeter(value: &str, de: Option<&str>) -> String {
    match value.to_lowercase().as_str() {
      "sddm" => {
        if let Some(de) = de {
          match de {
            "hyprland" => attrset! {
              "services.displayManager.sddm" = attrset! {
                "wayland.enable" = true;
                "enable" = true;
              };
            },
            _ => attrset! {
              "services.displayManager.sddm.enable" = true;
            },
          }
        } else {
          attrset! {
            "services.displayManager.sddm.enable" = true;
          }
        }
      }
      "gdm" => attrset! {
        "services.xserver.displayManager.gdm.enable" = true;
      },
      "lightdm" => attrset! {
        "services.xserver.displayManager.lightdm.enable" = true;
      },
      _ => String::new(),
    }
  }
  fn parse_desktop_environment(value: &str) -> String {
    match value.to_lowercase().as_str() {
      "gnome" => attrset! {
        "services.xserver.enable" = true;
        "services.xserver.desktopManager.gnome.enable" = true;
      },
      "hyprland" => attrset! {
        "programs.hyprland.enable" = true;
      },
      "plasma" | "kde plasma" => attrset! {
        "services.xserver.enable" = true;
        "services.xserver.desktopManager.plasma5.enable" = true;
      },
      "xfce" => attrset! {
        "services.xserver.enable" = true;
        "services.xserver.desktopManager.xfce.enable" = true;
      },
      "cinnamon" => attrset! {
        "services.xserver.enable" = true;
        "services.xserver.desktopManager.cinnamon.enable" = true;
      },
      "mate" => attrset! {
        "services.xserver.enable" = true;
        "services.xserver.desktopManager.mate.enable" = true;
      },
      "lxqt" => attrset! {
        "services.xserver.enable" = true;
        "services.xserver.desktopManager.lxqt.enable" = true;
      },
      "budgie" => attrset! {
        "services.xserver.enable" = true;
        "services.xserver.desktopManager.budgie.enable" = true;
      },
      "i3" => attrset! {
        "services.xserver.enable" = true;
        "services.xserver.windowManager.i3.enable" = true;
      },
      _ => String::new(),
    }
  }
  fn parse_audio(value: &str) -> String {
    match value.to_lowercase().as_str() {
      "pulseaudio" => attrset! {
        "services.pulseaudio.enable" = true;
        "services.pipewire.enable" = false;
      },
      "pipewire" => attrset! {
        "services.pipewire.enable" = true;
      },
      _ => String::new(),
    }
  }
  fn parse_bootloader(value: &str) -> anyhow::Result<String> {
    let bootloader_attrs = match value.to_lowercase().as_str() {
      "systemd-boot" => attrset! {
        "systemd-boot.enable" = true;
        "efi.canTouchEfiVariables" = true;
      },

      "grub" => attrset! {
        grub = attrset! {
          device = nixstr("nodev");
          enable = true;
          efiSupport = true;
        };
        "efi.canTouchEfiVariables" = true;
      },
      _ => String::new(),
    };
    Ok(attrset! {
      "boot.loader" = bootloader_attrs;
    })
  }

  fn parse_users(&self, users: Vec<User>) -> anyhow::Result<String> {
    if users.is_empty() {
      return Ok(String::from("{}"));
    }

    let mut user_configs = Vec::new();
    let mut hm_configs = Vec::new();

    for user in users {
      let groups_list = if user.groups.is_empty() {
        "[]".to_string()
      } else {
        let group_strings: Vec<String> = user.groups.iter().map(nixstr).collect();
        format!("[ {} ]", group_strings.join(" "))
      };
      let user_config = attrset! {
        "isNormalUser" = "true";
        "extraGroups" = groups_list;
        "hashedPassword" = nixstr(user.password_hash);
      };
      user_configs.push(format!("\"{}\" = {};", user.username, user_config));

      if let Some(cfg) = user.home_manager_cfg {
        let pkg_list = if cfg.packages.is_empty() {
          "with pkgs; []".to_string()
        } else {
          let pkgs: Vec<String> = cfg.packages.iter().map(|s| s.to_string()).collect();
          format!("with pkgs; [ {} ]", pkgs.join(" "))
        };
        // `home.stateVersion` is a closed enum with no default, so it can't be
        // derived from the NixOS release - on an unstable system that release
        // isn't in the enum yet and evaluation fails outright. Ask the pinned
        // home-manager which release it *is* instead; that value is always a
        // member of its own enum.
        let hm_state_version =
          r#"(builtins.fromJSON (builtins.readFile "${homeManagerSrc}/release.json")).release"#;
        let hm_config_body = attrset! {
          home = attrset! {
            packages = pkg_list;
            stateVersion = hm_state_version;
          };
        };
        let hm_config_expr = format!("{{pkgs, ...}}: {hm_config_body}");
        let user_hm_config = format!("\"{}\" = {};", user.username, hm_config_expr);
        hm_configs.push(user_hm_config);
      }
    }

    let users = if !hm_configs.is_empty() {
      attrset! {
        "users.users" = format!("{{ {} }}", user_configs.join(" "));
        "home-manager.users" = format!("{{ {} }}", hm_configs.join(" "));
      }
    } else {
      attrset! {
        "users.users" = format!("{{ {} }}", user_configs.join(" "));
      }
    };

    log::debug!("Parsed users config: {users}");

    Ok(users)
  }

  #[allow(clippy::ptr_arg)]
  fn parse_system_packages(packages: &Vec<Value>) -> String {
    if packages.is_empty() {
      return String::from("{}");
    }

    let pkg_list: Vec<String> = packages
      .iter()
      .filter_map(&Value::as_str)
      .map(&str::to_string)
      .collect();

    if pkg_list.is_empty() {
      return String::from("{}");
    }

    let packages_attr = format!("with pkgs; [ {} ]", pkg_list.join(" "));
    attrset! {
      "environment.systemPackages" = packages_attr;
    }
  }

  fn parse_enable_flakes() -> String {
    attrset! {
      "nix.settings.experimental-features" = "[ \"nix-command\" \"flakes\" ]";
    }
  }

  fn parse_swap() -> String {
    attrset! {
      "swapDevices" = "[ { device = \"/swapfile\"; size = 4096; } ]";
    }
  }
}
