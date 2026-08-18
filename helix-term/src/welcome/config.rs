//! Configuration for the welcome screen.
//!
//! Deliberately its own file — `$XDG_CONFIG_HOME/helix/welcome.toml` — rather
//! than a `[welcome]` section of `config.toml`. Helix's `ConfigRaw` is
//! `#[serde(deny_unknown_fields)]`, so a section there means edits in five
//! places inside functions upstream changes regularly. A separate file costs
//! one `fs::read` at startup and no merge conflicts at all.
//!
//! ```toml
//! # ~/.config/helix/welcome.toml
//! enable = true
//! banner = ["  my own", "  ascii art"]
//! footer = "carpe diem"
//! ```

use serde::Deserialize;
use std::fs;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields, rename_all = "kebab-case")]
pub struct Config {
    /// Show the welcome screen when starting with no file arguments.
    pub enable: bool,
    /// Replaces the built-in banner. Lines are centered individually, so they
    /// don't have to be the same length; an empty list hides the banner.
    pub banner: Option<Vec<String>>,
    /// Replaces the built-in `helix <version>` footer. An empty string hides it.
    pub footer: Option<String>,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enable: true,
            banner: None,
            footer: None,
        }
    }
}

impl Config {
    /// Reads `welcome.toml` from the Helix config directory.
    ///
    /// A missing file is the common case and means "use the defaults". A
    /// malformed one is logged and then also falls back: a typo in a cosmetic
    /// config file must never stop the editor from starting.
    pub fn load() -> Self {
        let path = helix_loader::config_dir().join("welcome.toml");

        let source = match fs::read_to_string(&path) {
            Ok(source) => source,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Self::default(),
            Err(err) => {
                log::error!("Failed to read {}: {err}", path.display());
                return Self::default();
            }
        };

        toml::from_str(&source).unwrap_or_else(|err| {
            log::error!("Failed to parse {}: {err}", path.display());
            Self::default()
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_config_is_the_default() {
        assert_eq!(toml::from_str::<Config>("").unwrap(), Config::default());
    }

    #[test]
    fn fields_override_the_defaults() {
        let config: Config = toml::from_str(
            r#"
            enable = false
            banner = ["a", "b"]
            footer = "hello"
            "#,
        )
        .unwrap();

        assert!(!config.enable);
        assert_eq!(config.banner.unwrap(), ["a", "b"]);
        assert_eq!(config.footer.unwrap(), "hello");
    }
}
