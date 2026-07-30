// SPDX-FileCopyrightText: Iridesium
// SPDX-License-Identifier: GPL-3.0-only

//! `mod.toml` parsing and mod directory discovery.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// The file that makes a directory a mod.
pub const MANIFEST_FILE: &str = "mod.toml";

/// The entry script every mod must have.
pub const ENTRY_FILE: &str = "init.lua";

/// A mod's declared identity and dependencies.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModManifest {
    /// Lowercase `snake_case` identifier. Also the mod's registration namespace.
    pub id: String,

    /// Human-readable name.
    pub name: String,

    /// Semantic version.
    pub version: String,

    /// Hard dependencies, as `"other_mod >=1.0, <2"`.
    #[serde(default)]
    pub depends: Vec<String>,

    /// Dependencies that only affect load order if present.
    ///
    /// The distinction is load order, not features: an optional dependency that
    /// is installed must load *first*, so the dependant can see what it
    /// registered. A mod that adds recipes for another mod's blocks needs
    /// exactly this.
    #[serde(default)]
    pub optional_depends: Vec<String>,

    /// Aliases this mod satisfies.
    ///
    /// Lets a fork or replacement stand in for the mod it replaces without
    /// every dependant being edited.
    #[serde(default)]
    pub provides: Vec<String>,

    /// One-line description.
    #[serde(default)]
    pub description: String,

    /// SPDX licence expression.
    #[serde(default)]
    pub license: String,
}

/// A manifest could not be read or is not valid.
#[derive(Debug, thiserror::Error)]
pub enum ManifestError {
    /// The manifest file could not be read.
    #[error("could not read `{path}`")]
    Read {
        /// Path attempted.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// The manifest is not valid TOML, or has unknown fields.
    #[error("could not parse `{path}`")]
    Parse {
        /// Path attempted.
        path: PathBuf,
        /// Underlying error.
        #[source]
        source: toml::de::Error,
    },

    /// A mod id breaks the naming rules.
    #[error(
        "mod at `{path}` has id `{id}`: ids must be lowercase, start with a letter, and contain \
         only letters, digits and underscores"
    )]
    BadId {
        /// Path of the offending mod.
        path: PathBuf,
        /// The offending id.
        id: String,
    },

    /// A version is not valid semver.
    #[error("mod `{id}` has version `{version}`, which is not valid semver")]
    BadVersion {
        /// The mod.
        id: String,
        /// The offending version.
        version: String,
    },

    /// A dependency requirement could not be parsed.
    #[error("mod `{id}` declares dependency `{requirement}`, which is not a valid requirement")]
    BadRequirement {
        /// The mod declaring it.
        id: String,
        /// The offending requirement.
        requirement: String,
    },

    /// The mod has no `init.lua`.
    #[error("mod `{id}` at `{path}` has no {ENTRY_FILE}")]
    MissingEntry {
        /// The mod.
        id: String,
        /// Its directory.
        path: PathBuf,
    },

    /// A directory could not be scanned.
    #[error("could not scan mod directory `{path}`")]
    Scan {
        /// Path attempted.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
}

/// Whether an id obeys the naming rules.
///
/// Lowercase `snake_case`, starting with a letter. Strict because the id is also
/// the mod's registration namespace and appears in every string id it creates —
/// `MyMod:White` and `mymod:white` looking like different blocks would be a
/// permanent source of confusion.
#[must_use]
pub fn is_valid_id(id: &str) -> bool {
    let mut chars = id.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !first.is_ascii_lowercase() {
        return false;
    }
    id.chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
}

impl ModManifest {
    /// Reads and validates a manifest from a mod directory.
    ///
    /// # Errors
    ///
    /// [`ManifestError`] if the file is missing, malformed, or declares
    /// something invalid.
    pub fn load(dir: &Path) -> Result<Self, ManifestError> {
        let path = dir.join(MANIFEST_FILE);
        let text = std::fs::read_to_string(&path).map_err(|source| ManifestError::Read {
            path: path.clone(),
            source,
        })?;
        let manifest: Self = toml::from_str(&text).map_err(|source| ManifestError::Parse {
            path: path.clone(),
            source,
        })?;
        manifest.validate(dir)?;
        Ok(manifest)
    }

    /// Checks everything that can be checked without seeing the other mods.
    ///
    /// # Errors
    ///
    /// [`ManifestError`] describing the first problem found.
    pub fn validate(&self, dir: &Path) -> Result<(), ManifestError> {
        if !is_valid_id(&self.id) {
            return Err(ManifestError::BadId {
                path: dir.to_path_buf(),
                id: self.id.clone(),
            });
        }

        semver::Version::parse(&self.version).map_err(|_| ManifestError::BadVersion {
            id: self.id.clone(),
            version: self.version.clone(),
        })?;

        for requirement in self.depends.iter().chain(&self.optional_depends) {
            parse_requirement(requirement).ok_or_else(|| ManifestError::BadRequirement {
                id: self.id.clone(),
                requirement: requirement.clone(),
            })?;
        }

        for alias in &self.provides {
            if !is_valid_id(alias) {
                return Err(ManifestError::BadId {
                    path: dir.to_path_buf(),
                    id: alias.clone(),
                });
            }
        }

        if !dir.join(ENTRY_FILE).is_file() {
            return Err(ManifestError::MissingEntry {
                id: self.id.clone(),
                path: dir.to_path_buf(),
            });
        }

        Ok(())
    }

    /// The parsed version. Valid after [`Self::validate`].
    ///
    /// # Errors
    ///
    /// If the version is not semver, which [`Self::validate`] would have caught.
    pub fn semver(&self) -> Result<semver::Version, ManifestError> {
        semver::Version::parse(&self.version).map_err(|_| ManifestError::BadVersion {
            id: self.id.clone(),
            version: self.version.clone(),
        })
    }
}

/// Splits `"other_mod >=1.0, <2"` into an id and a version requirement.
///
/// A bare `"other_mod"` means any version.
#[must_use]
pub fn parse_requirement(text: &str) -> Option<(String, semver::VersionReq)> {
    let text = text.trim();
    let (id, range) = match text.find(char::is_whitespace) {
        None => (text, "*"),
        Some(split) => (&text[..split], text[split..].trim()),
    };

    if !is_valid_id(id) {
        return None;
    }
    let requirement = semver::VersionReq::parse(range).ok()?;
    Some((id.to_owned(), requirement))
}

/// A mod found on disk: its manifest and where it lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredMod {
    /// The parsed manifest.
    pub manifest: ModManifest,
    /// The mod's directory.
    pub dir: PathBuf,
}

/// Scans a directory of mod directories.
///
/// **Results are sorted by id**, so the caller never sees filesystem order.
/// `read_dir` order varies between filesystems and even between runs on the
/// same one; letting it reach the resolver would make load order — and
/// therefore every numeric material id — machine-dependent.
///
/// # Errors
///
/// [`ManifestError`] if the directory cannot be read, or if any mod inside it
/// has an invalid manifest. A malformed mod is a hard failure rather than a
/// skip: silently ignoring a mod the operator installed is worse than refusing
/// to start.
pub fn scan_directory(root: &Path) -> Result<Vec<DiscoveredMod>, ManifestError> {
    let entries = std::fs::read_dir(root).map_err(|source| ManifestError::Scan {
        path: root.to_path_buf(),
        source,
    })?;

    let mut found = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|source| ManifestError::Scan {
            path: root.to_path_buf(),
            source,
        })?;
        let dir = entry.path();
        if !dir.is_dir() || !dir.join(MANIFEST_FILE).is_file() {
            // A directory without a manifest is not a mod — documentation,
            // assets, a stray checkout. Not an error.
            continue;
        }
        found.push(DiscoveredMod {
            manifest: ModManifest::load(&dir)?,
            dir,
        });
    }

    found.sort_by(|a, b| a.manifest.id.cmp(&b.manifest.id));
    Ok(found)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_rules_are_strict() {
        for good in ["core", "my_mod", "mod2", "a"] {
            assert!(is_valid_id(good), "{good} should be valid");
        }
        for bad in ["", "Core", "my-mod", "2mod", "my mod", "my.mod", "_leading"] {
            assert!(!is_valid_id(bad), "{bad} should be invalid");
        }
    }

    #[test]
    fn a_bare_requirement_means_any_version() {
        let (id, requirement) = parse_requirement("other_mod").expect("parse");
        assert_eq!(id, "other_mod");
        assert!(requirement.matches(&semver::Version::parse("0.1.0").expect("v")));
        assert!(requirement.matches(&semver::Version::parse("9.9.9").expect("v")));
    }

    #[test]
    fn a_ranged_requirement_parses_and_bounds() {
        let (id, requirement) = parse_requirement("other_mod >=1.0, <2").expect("parse");
        assert_eq!(id, "other_mod");
        assert!(!requirement.matches(&semver::Version::parse("0.9.0").expect("v")));
        assert!(requirement.matches(&semver::Version::parse("1.5.0").expect("v")));
        assert!(!requirement.matches(&semver::Version::parse("2.0.0").expect("v")));
    }

    #[test]
    fn a_malformed_requirement_is_rejected() {
        assert!(parse_requirement("Bad_Id >=1.0").is_none());
        assert!(parse_requirement("ok_id !!!").is_none());
        assert!(parse_requirement("").is_none());
    }
}
