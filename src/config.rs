use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub sidebar: SidebarConfig,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SidebarConfig {
    pub working: SectionBehavior,
    pub stashed: SectionBehavior,
    pub sort: Option<SortMode>,
    pub select: StartupSelection,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SortMode {
    #[serde(rename = "creation")]
    Order,
    #[default]
    Recent,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum StartupSelection {
    #[default]
    Unread,
    Last,
    First,
}

#[derive(Debug, Clone, Copy, Default, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SectionBehavior {
    #[default]
    Collapse,
    Expand,
}

impl Config {
    pub fn load() -> Result<Self> {
        let Some(path) = config_path(
            std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from),
            std::env::var_os("HOME").map(PathBuf::from),
        ) else {
            return Ok(Self::default());
        };
        Self::load_from(&path)
    }

    fn load_from(path: &Path) -> Result<Self> {
        let source = match fs::read_to_string(path) {
            Ok(source) => source,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(err) => return Err(err).with_context(|| format!("read config {}", path.display())),
        };
        toml::from_str(&source).with_context(|| format!("parse config {}", path.display()))
    }
}

fn config_path(config_home: Option<PathBuf>, home: Option<PathBuf>) -> Option<PathBuf> {
    config_home
        .filter(|path| path.is_absolute())
        .or_else(|| {
            home.filter(|path| path.is_absolute())
                .map(|path| path.join(".config"))
        })
        .map(|path| path.join("agent-mux/config.toml"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn omitted_settings_keep_sections_collapsible() {
        for source in ["", "[sidebar]", "[sidebar]\nworking = 'collapse'"] {
            let config: Config = toml::from_str(source).unwrap();
            assert_eq!(config.sidebar.working, SectionBehavior::Collapse);
            assert_eq!(config.sidebar.stashed, SectionBehavior::Collapse);
            assert_eq!(config.sidebar.sort, None);
            assert_eq!(config.sidebar.select, StartupSelection::Unread);
        }
        let config: Config = toml::from_str("[sidebar]\nworking = 'expand'").unwrap();
        assert_eq!(config.sidebar.working, SectionBehavior::Expand);
        assert_eq!(config.sidebar.stashed, SectionBehavior::Collapse);
    }

    #[test]
    fn section_settings_are_independent() {
        let behaviors = [
            ("collapse", SectionBehavior::Collapse),
            ("expand", SectionBehavior::Expand),
        ];
        for (working, expected_working) in behaviors {
            for (stashed, expected_stashed) in behaviors {
                let source = format!("[sidebar]\nworking = '{working}'\nstashed = '{stashed}'");
                let config: Config = toml::from_str(&source).unwrap();
                assert_eq!(config.sidebar.working, expected_working);
                assert_eq!(config.sidebar.stashed, expected_stashed);
            }
        }
    }

    #[test]
    fn sorting_and_selection_settings_accept_supported_modes() {
        for (sort, expected_sort) in [("recent", SortMode::Recent), ("creation", SortMode::Order)] {
            for (select, expected_select) in [
                ("unread", StartupSelection::Unread),
                ("last", StartupSelection::Last),
                ("first", StartupSelection::First),
            ] {
                let source = format!("[sidebar]\nsort = '{sort}'\nselect = '{select}'");
                let config: Config = toml::from_str(&source).unwrap();
                assert_eq!(config.sidebar.sort, Some(expected_sort));
                assert_eq!(config.sidebar.select, expected_select);
            }
        }
    }

    #[test]
    fn invalid_settings_report_errors() {
        for source in [
            "[unknown]",
            "[sidebar]\nworkng = 'collapse'",
            "[sidebar]\nworking = true",
            "[sidebar]\nworking = 'automatic'",
            "[sidebar]\nsort = 'manual'",
            "[sidebar]\nsort = true",
            "[sidebar]\nselect = 'attention'",
            "[sidebar]\nselect = false",
            "[sidebar",
        ] {
            assert!(toml::from_str::<Config>(source).is_err(), "{source}");
        }
    }

    #[test]
    fn config_path_uses_xdg_directory_or_home_fallback() {
        assert_eq!(
            config_path(Some("/config".into()), Some("/home/test".into())),
            Some("/config/agent-mux/config.toml".into())
        );
        for config_home in [None, Some("".into()), Some("relative".into())] {
            assert_eq!(
                config_path(config_home, Some("/home/test".into())),
                Some("/home/test/.config/agent-mux/config.toml".into())
            );
        }
        assert_eq!(
            config_path(Some("/config".into()), None),
            Some("/config/agent-mux/config.toml".into())
        );
        assert_eq!(config_path(None, None), None);
        assert_eq!(config_path(None, Some("".into())), None);
        assert_eq!(config_path(None, Some("relative".into())), None);
    }

    #[test]
    fn file_loading_defaults_only_when_file_is_missing() {
        let root = std::env::temp_dir().join(format!(
            "agent-mux-config-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let path = root.join("config.toml");
        let config = Config::load_from(&path).unwrap();
        assert_eq!(config.sidebar.working, SectionBehavior::Collapse);
        assert_eq!(config.sidebar.stashed, SectionBehavior::Collapse);

        fs::create_dir_all(&root).unwrap();
        fs::write(&path, "[sidebar]\nworking = 'expand'").unwrap();
        assert_eq!(
            Config::load_from(&path).unwrap().sidebar.working,
            SectionBehavior::Expand
        );
        fs::write(&path, "invalid = true").unwrap();
        assert!(
            Config::load_from(&path)
                .unwrap_err()
                .to_string()
                .contains(path.to_str().unwrap())
        );
        assert!(Config::load_from(&root).is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
