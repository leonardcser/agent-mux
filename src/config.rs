use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub sidebar: SidebarConfig,
    pub new_window_command: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SidebarConfig {
    pub working: WorkingSection,
    pub working_section: SectionBehavior,
    pub stashed_section: SectionBehavior,
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
pub enum WorkingSection {
    #[default]
    Section,
    Manual,
    Inline,
    #[serde(rename = "inline-last")]
    InlineLast,
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
        for source in ["", "[sidebar]", "[sidebar]\nworking = 'section'"] {
            let config: Config = toml::from_str(source).unwrap();
            assert_eq!(config.sidebar.working, WorkingSection::Section);
            assert_eq!(config.sidebar.working_section, SectionBehavior::Collapse);
            assert_eq!(config.sidebar.stashed_section, SectionBehavior::Collapse);
            assert_eq!(config.sidebar.sort, None);
            assert_eq!(config.sidebar.select, StartupSelection::Unread);
        }
    }

    #[test]
    fn new_window_command_is_optional() {
        let config: Config = toml::from_str("").unwrap();
        assert_eq!(config.new_window_command, None);
        let config: Config = toml::from_str("new_window_command = 'smelt --resume'").unwrap();
        assert_eq!(config.new_window_command.as_deref(), Some("smelt --resume"));
        assert!(toml::from_str::<Config>("new_window_command = false").is_err());
    }

    #[test]
    fn section_settings_are_independent() {
        for (working, expected_working) in [
            ("section", WorkingSection::Section),
            ("manual", WorkingSection::Manual),
            ("inline", WorkingSection::Inline),
            ("inline-last", WorkingSection::InlineLast),
        ] {
            let source = format!(
                "[sidebar]\nworking = '{working}'\nworking_section = 'expand'\nstashed_section = 'collapse'"
            );
            let config: Config = toml::from_str(&source).unwrap();
            assert_eq!(config.sidebar.working, expected_working);
            assert_eq!(config.sidebar.working_section, SectionBehavior::Expand);
            assert_eq!(config.sidebar.stashed_section, SectionBehavior::Collapse);
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
            "unknown_option = true",
            "[unknown]",
            "[sidebar]\nworkng = 'collapse'",
            "[sidebar]\nworking = true",
            "[sidebar]\nworking = 'automatic'",
            "[sidebar]\nworking = 'collapse'",
            "[sidebar]\nworking = 'expand'",
            "[sidebar]\nstashed = 'collapse'",
            "[sidebar]\nstashed_section = 'inline'",
            "[sidebar]\nworking_section = 'inline'",
            "[sidebar]\nworking_section = false",
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
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("config.toml");
        let config = Config::load_from(&path).unwrap();
        assert_eq!(config.sidebar.working, WorkingSection::Section);
        assert_eq!(config.sidebar.stashed_section, SectionBehavior::Collapse);

        fs::write(&path, "[sidebar]\nworking = 'inline-last'").unwrap();
        assert_eq!(
            Config::load_from(&path).unwrap().sidebar.working,
            WorkingSection::InlineLast
        );
        fs::write(&path, "invalid = true").unwrap();
        assert!(
            Config::load_from(&path)
                .unwrap_err()
                .to_string()
                .contains(path.to_str().unwrap())
        );
        assert!(Config::load_from(root.path()).is_err());
    }
}
