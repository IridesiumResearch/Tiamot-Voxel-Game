// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! Client configuration, loaded from `client.toml`.
//!
//! Deliberately shaped like [`server::config`]: unknown keys are a hard error,
//! every field has a default, and the shipped example file is parsed by a test
//! so it cannot drift out of sync with the schema.
//!
//! # `embedded` is a server address
//!
//! Singleplayer is an embedded server over loopback (charter rule 2), so
//! "singleplayer" is not a mode the client has — it is a value of the `server`
//! key. One field, two spellings, and exactly one code path behind both.

use std::net::SocketAddr;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The spelling that selects an embedded server.
const EMBEDDED: &str = "embedded";

/// Anything that can go wrong loading a client config.
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

/// Where the client gets its world from.
///
/// Stored as a string in TOML so the two cases read naturally:
/// `server = "embedded"` or `server = "example.com:47811"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ServerChoice {
    /// Start a server in this process and connect to it over loopback.
    #[default]
    Embedded,
    /// Connect to a server already running somewhere.
    Remote(SocketAddr),
}

impl std::fmt::Display for ServerChoice {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Embedded => formatter.write_str(EMBEDDED),
            Self::Remote(addr) => write!(formatter, "{addr}"),
        }
    }
}

impl std::str::FromStr for ServerChoice {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        let trimmed = text.trim();
        if trimmed.eq_ignore_ascii_case(EMBEDDED) {
            return Ok(Self::Embedded);
        }
        trimmed.parse().map(Self::Remote).map_err(|_| {
            format!(
                "`{trimmed}` is neither `{EMBEDDED}` nor a socket address like \
                 `127.0.0.1:47811`. A hostname without a port is the usual mistake."
            )
        })
    }
}

impl Serialize for ServerChoice {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for ServerChoice {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        text.parse().map_err(serde::de::Error::custom)
    }
}

/// How the world is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum RenderMode {
    /// Atlas-textured with lighting mode 1 directional shading.
    #[default]
    Textured,
    /// Directional shading only, no atlas sample.
    ///
    /// Useful for telling a texture problem from a geometry problem: if the
    /// world looks right in `flat` and wrong in `textured`, the mesher is fine.
    Flat,
    /// Quad edges only.
    ///
    /// Requires the `POLYGON_MODE_LINE` device feature. Adapters without it —
    /// which includes some mobile and web backends — fall back to `textured`
    /// with a warning rather than failing to start.
    Wireframe,
}

/// How the world is lit.
///
/// Task 10's presentation modes over one simulation-authoritative light value.
/// **Modes are settings, not forks**: the server sends the same light whichever
/// is selected, and nothing about the world changes when one is picked — only
/// how it is drawn.
///
/// Mode 3 is being built pass by pass, and each one is added as it lands
/// rather than reserved: a mode that can be selected does what it says today,
/// which is more useful than a mode that promises the whole list and shows
/// none of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum LightingMode {
    /// Mode 1. Directional face shading and ambient occlusion, no propagated
    /// light — Task 08's look, and the baseline the other modes are measured
    /// against.
    ///
    /// The mesher samples flat daylight in this mode rather than the real
    /// value, which is not a detail: two faces may only merge when their corner
    /// light agrees, so real light splits quads along every shadow edge.
    /// Feeding it a constant restores Task 08's merge rate exactly, which is
    /// what "mode 1's cost profile is unchanged" has to mean.
    Simple,
    /// Mode 2. Smooth propagated light, coloured, with sky-tinted distance fog.
    #[default]
    Classic,
    /// Mode 3. Mode 2, drawn into a float target and put through a post chain:
    /// bloom from anything brighter than white, then a filmic tonemap.
    ///
    /// Shadow maps and time-of-day grading are the passes still to come; the
    /// chain they slot into is `render::graph`.
    Beautiful,
}

impl LightingMode {
    /// The next mode in the cycle, for the key that switches them live.
    #[must_use]
    pub const fn next(self) -> Self {
        match self {
            Self::Simple => Self::Classic,
            Self::Classic => Self::Beautiful,
            Self::Beautiful => Self::Simple,
        }
    }

    /// What the shader is told this mode is.
    ///
    /// A number rather than a bool, and it has been both: while there were two
    /// modes the uniform was `mode == Classic`, and adding a third quietly made
    /// mode 3 draw as mode 1 — the flat-lit branch — because everything that
    /// was not Classic was false. Numbering them makes the next mode a compile
    /// error here rather than a wrong picture.
    #[must_use]
    pub const fn code(self) -> u32 {
        match self {
            Self::Simple => 0,
            Self::Classic => 1,
            Self::Beautiful => 2,
        }
    }

    /// Whether this mode uses the propagated light the server sends.
    ///
    /// Mode 1 does not, and the mesher is handed a flat value instead — see the
    /// variant's own documentation for why that is a meshing decision rather
    /// than a shading one.
    #[must_use]
    pub const fn uses_propagated_light(self) -> bool {
        !matches!(self, Self::Simple)
    }

    /// Whether this mode draws through the post chain.
    #[must_use]
    pub const fn uses_post(self) -> bool {
        matches!(self, Self::Beautiful)
    }

    /// What to call it on the HUD.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Simple => "1 simple",
            Self::Classic => "2 classic",
            Self::Beautiful => "3 beautiful",
        }
    }
}

/// Client configuration.
///
/// Unknown fields are rejected rather than ignored, for the same reason the
/// server rejects them: a typo in a config key is far more often a mistake than
/// an intention.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    /// Which server to play on.
    #[serde(default)]
    pub server: ServerChoice,

    /// The display name to claim on join.
    ///
    /// A display string only. Identity is the Ed25519 key and the UUID derived
    /// from it (charter rule 13); nothing keys on this.
    #[serde(default = "Config::default_display_name")]
    pub display_name: String,

    /// World directory for an embedded server. Ignored when connecting out.
    #[serde(default = "Config::default_world_path")]
    pub world_path: PathBuf,

    /// How far to render, in chunks (horizontal radius).
    ///
    /// Clamped into the engine's supported range rather than obeyed literally.
    #[serde(default = "Config::default_view_distance")]
    pub view_distance: u8,

    /// How far to render vertically, in chunks.
    #[serde(default = "Config::default_vertical_view_distance")]
    pub vertical_view_distance: u8,

    /// How the world is drawn.
    #[serde(default)]
    pub render_mode: RenderMode,

    /// How the world is lit.
    ///
    /// Switchable while the game is running, so this is the mode it starts in
    /// rather than the mode it stays in.
    #[serde(default)]
    pub lighting_mode: LightingMode,

    /// Whether to wait for vertical blank.
    ///
    /// On by default. Charter rule 18 makes frame *pacing* the metric rather
    /// than average frame rate, and an unsynchronised loop trades pacing for a
    /// number that looks better in a benchmark.
    #[serde(default = "Config::default_vsync")]
    pub vsync: bool,

    /// Vertical field of view, in degrees.
    #[serde(default = "Config::default_fov_degrees")]
    pub fov_degrees: f32,

    /// Mouse look sensitivity, in radians of turn per pixel of movement.
    #[serde(default = "Config::default_mouse_sensitivity")]
    pub mouse_sensitivity: f32,

    /// Free-fly speed, in blocks per second.
    #[serde(default = "Config::default_fly_speed")]
    pub fly_speed: f32,

    /// Where to keep the identity key and the content cache.
    ///
    /// Absent means the platform data directory — see [`data_dir`].
    #[serde(default)]
    pub data_path: Option<PathBuf>,
}

impl Config {
    fn default_display_name() -> String {
        "player".to_owned()
    }

    fn default_world_path() -> PathBuf {
        PathBuf::from("singleplayer")
    }

    fn default_view_distance() -> u8 {
        tiamot_core::interest::ViewDistance::DEFAULT.horizontal
    }

    fn default_vertical_view_distance() -> u8 {
        tiamot_core::interest::ViewDistance::DEFAULT.vertical
    }

    const fn default_vsync() -> bool {
        true
    }

    const fn default_fov_degrees() -> f32 {
        70.0
    }

    const fn default_mouse_sensitivity() -> f32 {
        0.002
    }

    const fn default_fly_speed() -> f32 {
        12.0
    }

    /// Reads and validates a config file.
    ///
    /// # Errors
    ///
    /// [`ConfigError`] if the file cannot be read, is not valid TOML, contains
    /// unknown keys, or holds an unusable value.
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

    /// Reads a config file, or returns the defaults if it does not exist.
    ///
    /// A missing config is the normal first-run state and must not be an error:
    /// the client has to start with no files at all. A config that exists and
    /// is *wrong* still is an error — silently ignoring a broken file would
    /// leave the player wondering why their settings do nothing.
    ///
    /// # Errors
    ///
    /// [`ConfigError`] for anything other than the file being absent.
    pub fn load_or_default(path: &Path) -> Result<Self, ConfigError> {
        match Self::load(path) {
            Err(ConfigError::Read { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                Ok(Self::default())
            }
            other => other,
        }
    }

    fn validate(&self, path: &Path) -> Result<(), ConfigError> {
        let invalid = |message: String| ConfigError::Invalid {
            path: path.to_path_buf(),
            message,
        };

        if self.display_name.trim().is_empty() {
            return Err(invalid("display_name must not be empty".to_owned()));
        }
        if self.display_name.len() > tiamot_core::proto::MAX_NAME_BYTES {
            return Err(invalid(format!(
                "display_name is {} bytes, over the protocol's {}-byte limit — the server would \
                 refuse the join",
                self.display_name.len(),
                tiamot_core::proto::MAX_NAME_BYTES
            )));
        }
        // Non-finite values here are not a determinism concern (rendering is
        // exempt from charter rule 4) but they do produce a degenerate
        // projection matrix and a black screen, which is a miserable thing to
        // debug from a config typo.
        if !self.fov_degrees.is_finite() || self.fov_degrees <= 1.0 || self.fov_degrees >= 179.0 {
            return Err(invalid(format!(
                "fov_degrees must be between 1 and 179, not {}",
                self.fov_degrees
            )));
        }
        if !self.mouse_sensitivity.is_finite() || self.mouse_sensitivity <= 0.0 {
            return Err(invalid(format!(
                "mouse_sensitivity must be positive, not {}",
                self.mouse_sensitivity
            )));
        }
        if !self.fly_speed.is_finite() || self.fly_speed <= 0.0 {
            return Err(invalid(format!(
                "fly_speed must be positive, not {}",
                self.fly_speed
            )));
        }
        Ok(())
    }

    /// The view distance, clamped into the range the engine supports.
    #[must_use]
    pub const fn view(&self) -> tiamot_core::interest::ViewDistance {
        tiamot_core::interest::ViewDistance::clamped(
            self.view_distance,
            self.vertical_view_distance,
        )
    }

    /// The directory holding the identity key and the content cache.
    #[must_use]
    pub fn data_dir(&self) -> PathBuf {
        self.data_path.clone().unwrap_or_else(data_dir)
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: ServerChoice::default(),
            display_name: Self::default_display_name(),
            world_path: Self::default_world_path(),
            view_distance: Self::default_view_distance(),
            vertical_view_distance: Self::default_vertical_view_distance(),
            render_mode: RenderMode::default(),
            lighting_mode: LightingMode::default(),
            vsync: Self::default_vsync(),
            fov_degrees: Self::default_fov_degrees(),
            mouse_sensitivity: Self::default_mouse_sensitivity(),
            fly_speed: Self::default_fly_speed(),
            data_path: None,
        }
    }
}

/// The platform directory for this client's persistent data.
///
/// Written by hand rather than pulled from a crate: it is twenty lines, the
/// conventions have not moved in a decade, and a dependency here would have to
/// clear the licence gate for something the standard library nearly provides.
///
/// Falls back to `./tiamot-data` if the environment has none of the usual
/// variables, which happens in containers and on CI. A client that refused to
/// start because `$HOME` was unset would be wrong.
#[must_use]
pub fn data_dir() -> PathBuf {
    let base = if cfg!(target_os = "windows") {
        std::env::var_os("APPDATA").map(PathBuf::from)
    } else if cfg!(target_os = "macos") {
        std::env::var_os("HOME").map(|home| PathBuf::from(home).join("Library/Application Support"))
    } else {
        std::env::var_os("XDG_DATA_HOME")
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".local/share"))
            })
    };

    base.map_or_else(|| PathBuf::from("tiamot-data"), |base| base.join("tiamot"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    fn temp_config(name: &str, text: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("tiamot-client-test-{name}.toml"));
        let mut file = std::fs::File::create(&path).expect("create temp config");
        file.write_all(text.as_bytes()).expect("write temp config");
        path
    }

    #[test]
    fn embedded_is_the_default_server() {
        // Singleplayer must work with no config file at all — that is the
        // first-run experience.
        assert_eq!(Config::default().server, ServerChoice::Embedded);
    }

    #[test]
    fn a_server_is_either_the_word_embedded_or_an_address() {
        let path = temp_config("remote", "server = \"127.0.0.1:47811\"\n");
        let config = Config::load(&path).expect("valid config");
        assert_eq!(
            config.server,
            ServerChoice::Remote("127.0.0.1:47811".parse().expect("addr"))
        );

        let path = temp_config("embedded", "server = \"embedded\"\n");
        assert_eq!(
            Config::load(&path).expect("valid config").server,
            ServerChoice::Embedded
        );
    }

    #[test]
    fn a_hostname_without_a_port_is_refused_with_an_explanation() {
        // The mistake everyone makes once. The message has to name it, because
        // "invalid value" sends people looking at the wrong key.
        let path = temp_config("no-port", "server = \"example.com\"\n");
        let err = Config::load(&path).expect_err("a bare hostname is not an address");
        assert!(err.to_string().contains("could not parse"), "got {err}");
        let source = std::error::Error::source(&err)
            .map(ToString::to_string)
            .unwrap_or_default();
        assert!(source.contains("without a port"), "got {source}");
    }

    #[test]
    fn a_missing_config_is_the_defaults_but_a_broken_one_is_an_error() {
        // The distinction that matters on first run: no file means "use the
        // defaults", a file with a typo in it means "you asked for something
        // and did not get it".
        let absent = std::env::temp_dir().join("tiamot-client-definitely-absent.toml");
        let _ = std::fs::remove_file(&absent);
        assert_eq!(
            Config::load_or_default(&absent).expect("absent is fine"),
            Config::default()
        );

        let broken = temp_config("broken", "view_distanc = 4\n");
        assert!(
            Config::load_or_default(&broken).is_err(),
            "a typo must not be silently ignored"
        );
    }

    #[test]
    fn round_trips_through_toml() {
        let original = Config::default();
        let text = toml::to_string(&original).expect("serialise");
        let parsed: Config = toml::from_str(&text).expect("deserialise");
        assert_eq!(original, parsed);
    }

    #[test]
    fn an_absurd_view_distance_is_clamped_rather_than_obeyed() {
        // Interest volume grows with the square of this. A client that obeyed
        // 200 would appear to start and then never finish streaming.
        let config = Config {
            view_distance: 200,
            vertical_view_distance: 200,
            ..Config::default()
        };
        let view = config.view();
        assert!(view.horizontal <= tiamot_core::interest::ViewDistance::MAXIMUM.horizontal);
        assert!(view.vertical <= tiamot_core::interest::ViewDistance::MAXIMUM.vertical);
    }

    #[test]
    fn a_degenerate_field_of_view_is_refused() {
        // Zero or 180 degrees produces a projection matrix that maps everything
        // to nothing, and the symptom is a black screen with no error.
        for bad in ["0.0", "180.0", "nan"] {
            let path = temp_config("fov", &format!("fov_degrees = {bad}\n"));
            assert!(
                Config::load(&path).is_err(),
                "fov_degrees = {bad} should be refused"
            );
        }
    }

    #[test]
    fn a_display_name_the_server_would_refuse_is_refused_here_first() {
        // The protocol caps names, and a client that let one through would get
        // a disconnect at join time with no obvious cause.
        let long = "x".repeat(tiamot_core::proto::MAX_NAME_BYTES + 1);
        let path = temp_config("name", &format!("display_name = \"{long}\"\n"));
        let err = Config::load(&path).expect_err("an over-long name should be refused");
        assert!(err.to_string().contains("byte limit"), "got {err}");
    }

    #[test]
    fn the_data_directory_never_fails_to_resolve() {
        // Containers and CI runners routinely have no HOME. A client that
        // refused to start there would be untestable in exactly the place it
        // most needs testing.
        assert!(!data_dir().as_os_str().is_empty());
    }

    #[test]
    fn the_shipped_example_config_is_valid() {
        // The first thing a player copies. Guards against it drifting out of
        // sync with the schema.
        let example = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../client.example.toml")
            .canonicalize()
            .expect("client.example.toml should exist at the repo root");

        Config::load(&example).expect("shipped example config should be valid");
    }

    #[test]
    fn a_lighting_mode_can_be_written_down_and_read_back() {
        // The setting is a string in a file a player edits by hand, so the
        // names matter as much as the variants do.
        let config: Config =
            toml::from_str("lighting_mode = \"simple\"").expect("simple should parse");
        assert_eq!(config.lighting_mode, LightingMode::Simple);

        let config: Config =
            toml::from_str("lighting_mode = \"classic\"").expect("classic should parse");
        assert_eq!(config.lighting_mode, LightingMode::Classic);

        let config: Config =
            toml::from_str("lighting_mode = \"beautiful\"").expect("beautiful should parse");
        assert_eq!(config.lighting_mode, LightingMode::Beautiful);

        assert!(
            toml::from_str::<Config>("lighting_mode = \"cinematic\"").is_err(),
            "a mode that does not exist must fail rather than silently \
             selecting something else"
        );

        // The default is what a config with no opinion gets, and it is the mode
        // the world's light was computed for.
        assert_eq!(Config::default().lighting_mode, LightingMode::Classic);
    }

    #[test]
    fn cycling_the_lighting_mode_visits_every_mode_and_comes_back() {
        // Two full turns, so the cycle both visits everything and comes home.
        // A count that is not a multiple of the number of modes proves the
        // first half and quietly fails the second.
        let mut mode = LightingMode::default();
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..6 {
            seen.insert(format!("{mode:?}"));
            mode = mode.next();
        }
        assert_eq!(seen.len(), 3, "the cycle skipped a mode: {seen:?}");
        assert_eq!(
            mode,
            LightingMode::default(),
            "cycling twice round must come home"
        );
    }
}
