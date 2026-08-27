// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Server configuration, loaded from a TOML file.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Anything that can go wrong loading a config file.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The file could not be read.
    #[error("could not read config file `{path}`")]
    Read {
        /// Path we tried to read.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// The file was read but is not valid TOML, or does not match the schema.
    #[error("could not parse config file `{path}`")]
    Parse {
        /// Path we tried to parse.
        path: PathBuf,
        /// Underlying deserialisation error.
        #[source]
        source: toml::de::Error,
    },

    /// A field held a syntactically valid but unusable value.
    #[error("invalid config in `{path}`: {message}")]
    Invalid {
        /// Path the bad value came from.
        path: PathBuf,
        /// What was wrong with it.
        message: String,
    },
}

/// Shortest RCON token accepted.
///
/// 16 characters of anything reasonable is far beyond guessing over a socket
/// that logs every failure, and short enough not to be a nuisance.
const MIN_RCON_TOKEN_BYTES: usize = 16;

/// Server configuration.
///
/// Unknown fields are rejected rather than ignored: a typo in a config key is
/// far more often a mistake than an intention, and silently running with a
/// default the operator did not choose is worse than refusing to start.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Address and port the server listens on.
    #[serde(default = "Config::default_bind_addr")]
    pub bind_addr: SocketAddr,

    /// Directory holding the world database.
    #[serde(default = "Config::default_world_path")]
    pub world_path: PathBuf,

    /// Maximum simultaneously connected players.
    #[serde(default = "Config::default_max_players")]
    pub max_players: u32,

    /// Seed for a new world. Ignored once a world exists.
    ///
    /// Absent draws one from system entropy. Set it to share a world's terrain
    /// with someone else, or to reproduce a bug report.
    #[serde(default)]
    pub seed: Option<u64>,

    /// Directory to load mods from.
    ///
    /// Absent means no mods. That is a legitimate configuration — the engine is
    /// mechanisms and the content is mods, so a server with none is empty
    /// rather than broken.
    #[serde(default)]
    pub mods_path: Option<PathBuf>,

    /// Which mods to load, by id. Absent loads every mod in `mods_path`.
    ///
    /// **The mod set is the server owner's decision**, whether that owner is a
    /// company or somebody running a world for two friends. A selection that
    /// leaves out something an enabled mod depends on fails to resolve and the
    /// server does not start: half a mod set is not a smaller mod set, and
    /// there is no correct subset to fall back to.
    #[serde(default)]
    pub enabled_mods: Option<Vec<String>>,

    /// How far players can see, in chunks (horizontal radius).
    ///
    /// Clamped into the supported range rather than obeyed literally —
    /// interest volume grows with the square of this, so an operator typing
    /// 200 would get a server that appeared to start and then could not keep
    /// up.
    #[serde(default = "Config::default_view_distance")]
    pub view_distance: u8,

    /// How far players can see vertically, in chunks.
    #[serde(default = "Config::default_vertical_view_distance")]
    pub vertical_view_distance: u8,

    /// Remote administration. Off unless configured.
    #[serde(default)]
    pub rcon: Option<RconConfig>,
}

/// Remote administration settings.
///
/// Absent from the config means **off**. There is no "enabled = false" to
/// forget to set: an operator who has not written this section has no admin
/// port open, which is the right default for something with `stop` and
/// `rebind` on it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RconConfig {
    /// Address to bind. Must be loopback; the server refuses anything else.
    #[serde(default = "RconConfig::default_bind_addr")]
    pub bind_addr: SocketAddr,

    /// The token an admin must present.
    ///
    /// No default. A default admin token is a published admin token.
    pub token: String,
}

impl RconConfig {
    fn default_bind_addr() -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], 47_812))
    }
}

impl Config {
    fn default_bind_addr() -> SocketAddr {
        // IPv4 wildcard; operators who want IPv6 or loopback-only say so.
        SocketAddr::from(([0, 0, 0, 0], 47_811))
    }

    fn default_world_path() -> PathBuf {
        PathBuf::from("world")
    }

    fn default_view_distance() -> u8 {
        tiamot_core::interest::ViewDistance::DEFAULT.horizontal
    }

    fn default_vertical_view_distance() -> u8 {
        tiamot_core::interest::ViewDistance::DEFAULT.vertical
    }

    fn default_max_players() -> u32 {
        16
    }

    /// Reads and validates a config file.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] if the file cannot be read, is not valid TOML,
    /// contains unknown keys, or holds an unusable value.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
            path: path.to_path_buf(),
            source,
        })?;

        let config: Self = toml::from_str(&text).map_err(|source| ConfigError::Parse {
            path: path.to_path_buf(),
            source,
        })?;

        config.validate(path)?;
        Ok(config)
    }

    fn validate(&self, path: &Path) -> Result<(), ConfigError> {
        if self.max_players == 0 {
            return Err(ConfigError::Invalid {
                path: path.to_path_buf(),
                message: "max_players must be at least 1".to_owned(),
            });
        }
        if let Some(rcon) = &self.rcon {
            // Refused at load, not at bind. An operator who typed 0.0.0.0
            // should learn about it from a startup error rather than from
            // someone else running `stop`.
            if !rcon.bind_addr.ip().is_loopback() {
                return Err(ConfigError::Invalid {
                    path: path.to_path_buf(),
                    message: format!(
                        "rcon.bind_addr must be a loopback address, not {}. This protocol has no                          transport encryption; tunnel over SSH for remote access.",
                        rcon.bind_addr
                    ),
                });
            }
            // An empty token is not authentication. Refusing is kinder than
            // starting a server whose admin port anyone can drive.
            if rcon.token.trim().is_empty() {
                return Err(ConfigError::Invalid {
                    path: path.to_path_buf(),
                    message: "rcon.token must not be empty".to_owned(),
                });
            }
            if rcon.token.len() < MIN_RCON_TOKEN_BYTES {
                return Err(ConfigError::Invalid {
                    path: path.to_path_buf(),
                    message: format!(
                        "rcon.token must be at least {MIN_RCON_TOKEN_BYTES} characters; a short                          token on a port with `stop` and `rebind` on it is worth guessing"
                    ),
                });
            }
        }
        Ok(())
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            bind_addr: Self::default_bind_addr(),
            world_path: Self::default_world_path(),
            max_players: Self::default_max_players(),
            seed: None,
            mods_path: None,
            enabled_mods: None,
            view_distance: Self::default_view_distance(),
            vertical_view_distance: Self::default_vertical_view_distance(),
            rcon: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Writes `text` to a uniquely named temp file and returns its path.
    ///
    /// Named after the test so parallel test threads cannot collide.
    fn temp_config(name: &str, text: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("tiamot-test-{name}.toml"));
        let mut file = std::fs::File::create(&path).expect("create temp config");
        file.write_all(text.as_bytes()).expect("write temp config");
        path
    }

    #[test]
    fn parses_a_full_config() {
        let path = temp_config(
            "full",
            r#"
bind_addr = "127.0.0.1:1234"
world_path = "/srv/worlds/alpha"
max_players = 64
"#,
        );

        let config = Config::load(&path).expect("valid config should load");
        assert_eq!(config.bind_addr, "127.0.0.1:1234".parse().expect("addr"));
        assert_eq!(config.world_path, PathBuf::from("/srv/worlds/alpha"));
        assert_eq!(config.max_players, 64);
    }

    #[test]
    fn omitted_fields_fall_back_to_defaults() {
        let path = temp_config("partial", "max_players = 2\n");

        let config = Config::load(&path).expect("partial config should load");
        assert_eq!(config.max_players, 2);
        assert_eq!(config.bind_addr, Config::default_bind_addr());
        assert_eq!(config.world_path, Config::default_world_path());
    }

    #[test]
    fn round_trips_through_toml() {
        let original = Config::default();
        let text = toml::to_string(&original).expect("serialise");
        let parsed: Config = toml::from_str(&text).expect("deserialise");
        assert_eq!(original, parsed);
    }

    #[test]
    fn rejects_unknown_keys() {
        let path = temp_config("unknown", "max_playerz = 4\n");

        let err = Config::load(&path).expect_err("typo should be rejected");
        assert!(matches!(err, ConfigError::Parse { .. }), "got {err:?}");
    }

    #[test]
    fn rejects_zero_max_players() {
        let path = temp_config("zero", "max_players = 0\n");

        let err = Config::load(&path).expect_err("zero players should be rejected");
        assert!(matches!(err, ConfigError::Invalid { .. }), "got {err:?}");
    }

    #[test]
    fn reports_a_missing_file_as_a_read_error() {
        let path = std::env::temp_dir().join("tiamot-test-definitely-absent.toml");
        let _ = std::fs::remove_file(&path);

        let err = Config::load(&path).expect_err("missing file should be an error");
        assert!(matches!(err, ConfigError::Read { .. }), "got {err:?}");
    }

    #[test]
    fn the_shipped_example_config_is_valid() {
        // Guards against the example drifting out of sync with the schema —
        // it is the first thing a new operator copies.
        let example = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../server.example.toml")
            .canonicalize()
            .expect("server.example.toml should exist at the repo root");

        let config = Config::load(&example).expect("shipped example config should be valid");

        // **And it loads MODS.** Valid is not the same as usable: with
        // `mods_path` commented out — which is how this file shipped — the
        // server the README's own quick-start command starts registers no
        // blocks, no tools, no worldgen and no sounds, and what a player joins
        // is unbuildable air that looks like a broken build.
        //
        // Reported from a Mac by somebody following the README.
        let mods = config
            .mods_path
            .as_deref()
            .expect("the example must load mods, or a first run is an empty world");
        assert!(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("../..")
                .join(mods)
                .is_dir(),
            "the example names `{}`, which is not a directory in this repository",
            mods.display()
        );
    }
}
