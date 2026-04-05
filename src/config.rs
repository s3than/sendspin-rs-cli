use log::{info, warn};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct AppConfig {
    #[serde(default)]
    pub player: PlayerConfig,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct PlayerConfig {
    pub volume: Option<u8>,
}

fn config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("sendspin-rs-cli").join("config.toml"))
}

impl AppConfig {
    pub fn load() -> Self {
        let Some(path) = config_path() else {
            warn!("Could not determine config directory");
            return Self::default();
        };

        match std::fs::read_to_string(&path) {
            Ok(contents) => match toml::from_str(&contents) {
                Ok(config) => {
                    info!("Loaded config from {}", path.display());
                    config
                }
                Err(e) => {
                    warn!("Failed to parse config {}: {}", path.display(), e);
                    Self::default()
                }
            },
            Err(_) => Self::default(),
        }
    }

    pub fn save(&self) -> Result<(), String> {
        let path = config_path().ok_or("could not determine config directory")?;

        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("failed to create config dir: {}", e))?;
        }

        let contents = toml::to_string_pretty(self)
            .map_err(|e| format!("failed to serialize config: {}", e))?;

        std::fs::write(&path, contents)
            .map_err(|e| format!("failed to write config {}: {}", path.display(), e))?;

        info!("Saved config to {}", path.display());
        Ok(())
    }
}

/// Save volume to config file (intended to be called from tokio::task::spawn_blocking)
pub fn save_volume(vol: u8) {
    let mut config = AppConfig::load();
    config.player.volume = Some(vol);
    if let Err(e) = config.save() {
        warn!("Failed to save volume to config: {}", e);
    }
}
