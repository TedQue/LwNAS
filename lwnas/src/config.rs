use serde::{Deserialize, Serialize};
use std::error::Error;
use std::fmt;
use std::fs;
use std::path::Path;
use toml;

#[derive(Debug)]
pub enum ConfigError {
    Io(std::io::Error),
    Parse(toml::de::Error),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Io(e) => write!(f, "load config failed with io error: {}", e),
            ConfigError::Parse(e) => write!(f, "parse config failed with toml parse error: {}", e),
        }
    }
}

impl Error for ConfigError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            ConfigError::Io(err) => Some(err),
            ConfigError::Parse(err) => Some(err),
        }
    }
}

impl From<std::io::Error> for ConfigError {
    fn from(err: std::io::Error) -> Self {
        ConfigError::Io(err)
    }
}

impl From<toml::de::Error> for ConfigError {
    fn from(err: toml::de::Error) -> Self {
        ConfigError::Parse(err)
    }
}

// 默认情况下 struct 字段名与 toml 字段名相同,可通过以下属性宏(修饰字段)调整
// #[serde(rename = "toml中的键名")]
// #[serde(rename_all = "kebab-case")] // 自动将 max_connections 映射为 max-connections

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FileNameConflictResolutionStrategy {
    Overwrite,
    Skip,
    AppendUuid,
    AppendTimestamp,
}

// #[serde(default)], 处理缺失字段,调用 Default::default()
// #[serde(skip)]
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Config {
    pub addr: String,
    pub templates: String,
    pub thumb_enabled: bool,
    pub thumb_root: String,
    pub thumb_size: u32,
    pub thumb_max_parallel: u32,
    pub tmp_file_dir: String,
    pub max_upload_limit: u64,
    pub confirm_delete: bool,
    pub file_name_conflict_resolution_strategy: FileNameConflictResolutionStrategy,
    pub logger: LoggerDesc,
    pub root_paths: Vec<PathDesc>,
    pub shutdown_enabled: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct LoggerDesc {
    pub bound: usize,
    pub level: log::LevelFilter,
    pub stdout: bool,
    pub stderr: bool,
    pub rf_file_name: String,
    pub rf_file_size: u64,
    pub rf_file_count: usize,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PathDesc {
    pub local_path: String,
    pub uri_path: String,
    pub hide: Option<bool>,
    pub writable: Option<bool>,
    pub deletable: Option<bool>,
}

pub fn load_from_file<P: AsRef<Path>>(path: P) -> Result<Config, ConfigError> {
    let content = fs::read_to_string(path)?;
    let conf = toml::from_str(&content)?;
    Ok(conf)
}
