use std::convert::TryFrom;
use std::fmt::Display;
use std::io::Write;

use anyhow::{bail, Error};
use serde::{Deserialize, Serialize};

use proxmox::api::api;

use crate::repositories::standard::APTRepositoryHandle;

#[api]
#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum APTRepositoryFileType {
    /// One-line-style format
    List,
    /// DEB822-style format
    Sources,
}

impl TryFrom<&str> for APTRepositoryFileType {
    type Error = Error;

    fn try_from(string: &str) -> Result<Self, Error> {
        match string {
            "list" => Ok(APTRepositoryFileType::List),
            "sources" => Ok(APTRepositoryFileType::Sources),
            _ => bail!("invalid file type '{}'", string),
        }
    }
}

impl Display for APTRepositoryFileType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            APTRepositoryFileType::List => write!(f, "list"),
            APTRepositoryFileType::Sources => write!(f, "sources"),
        }
    }
}

#[api]
#[derive(Debug, Copy, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "kebab-case")]
pub enum APTRepositoryPackageType {
    /// Debian package
    Deb,
    /// Debian source package
    DebSrc,
}

impl TryFrom<&str> for APTRepositoryPackageType {
    type Error = Error;

    fn try_from(string: &str) -> Result<Self, Error> {
        match string {
            "deb" => Ok(APTRepositoryPackageType::Deb),
            "deb-src" => Ok(APTRepositoryPackageType::DebSrc),
            _ => bail!("invalid package type '{}'", string),
        }
    }
}

impl Display for APTRepositoryPackageType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            APTRepositoryPackageType::Deb => write!(f, "deb"),
            APTRepositoryPackageType::DebSrc => write!(f, "deb-src"),
        }
    }
}

#[api(
    properties: {
        Key: {
            description: "Option key.",
            type: String,
        },
        Values: {
            description: "Option values.",
            type: Array,
            items: {
                description: "Value.",
                type: String,
            },
        },
    },
)]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")] // for consistency
/// Additional options for an APT repository.
/// Used for both single- and mutli-value options.
pub struct APTRepositoryOption {
    /// Option key.
    pub key: String,
    /// Option value(s).
    pub values: Vec<String>,
}

#[api(
    properties: {
        Types: {
            description: "List of package types.",
            type: Array,
            items: {
                type: APTRepositoryPackageType,
            },
        },
        URIs: {
            description: "List of repository URIs.",
            type: Array,
            items: {
                description: "Repository URI.",
                type: String,
            },
        },
        Suites: {
            description: "List of distributions.",
            type: Array,
            items: {
                description: "Package distribution.",
                type: String,
            },
        },
        Components: {
            description: "List of repository components.",
            type: Array,
            items: {
                description: "Repository component.",
                type: String,
            },
        },
        Options: {
            type: Array,
            optional: true,
            items: {
                type: APTRepositoryOption,
            },
        },
        Comment: {
            description: "Associated comment.",
            type: String,
            optional: true,
        },
        FileType: {
            type: APTRepositoryFileType,
        },
        Enabled: {
            description: "Whether the repository is enabled or not.",
            type: Boolean,
        },
    },
)]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "PascalCase")]
/// Describes an APT repository.
pub struct APTRepository {
    /// List of package types.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub types: Vec<APTRepositoryPackageType>,

    /// List of repository URIs.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    #[serde(rename = "URIs")]
    pub uris: Vec<String>,

    /// List of package distributions.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub suites: Vec<String>,

    /// List of repository components.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub components: Vec<String>,

    /// Additional options.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<APTRepositoryOption>,

    /// Associated comment.
    #[serde(skip_serializing_if = "String::is_empty")]
    pub comment: String,

    /// Format of the defining file.
    pub file_type: APTRepositoryFileType,

    /// Whether the repository is enabled or not.
    pub enabled: bool,
}

impl APTRepository {
    /// Crates an empty repository.
    pub fn new(file_type: APTRepositoryFileType) -> Self {
        Self {
            types: vec![],
            uris: vec![],
            suites: vec![],
            components: vec![],
            options: vec![],
            comment: String::new(),
            file_type,
            enabled: true,
        }
    }

    /// Changes the `enabled` flag and makes sure the `Enabled` option for
    /// `APTRepositoryPackageType::Sources` repositories is updated too.
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;

        if self.file_type == APTRepositoryFileType::Sources {
            let enabled_string = match enabled {
                true => "true".to_string(),
                false => "false".to_string(),
            };
            for option in self.options.iter_mut() {
                if option.key == "Enabled" {
                    option.values = vec![enabled_string];
                    return;
                }
            }
            self.options.push(APTRepositoryOption {
                key: "Enabled".to_string(),
                values: vec![enabled_string],
            });
        }
    }

    /// Makes sure that all basic properties of a repository are present and
    /// not obviously invalid.
    pub fn basic_check(&self) -> Result<(), Error> {
        if self.types.is_empty() {
            bail!("missing package type(s)");
        }
        if self.uris.is_empty() {
            bail!("missing URI(s)");
        }
        if self.suites.is_empty() {
            bail!("missing suite(s)");
        }

        for uri in self.uris.iter() {
            if !uri.contains(':') || uri.len() < 3 {
                bail!("invalid URI: '{}'", uri);
            }
        }

        for suite in self.suites.iter() {
            if !suite.ends_with('/') && self.components.is_empty() {
                bail!("missing component(s)");
            } else if suite.ends_with('/') && !self.components.is_empty() {
                bail!("absolute suite '{}' does not allow component(s)", suite);
            }
        }

        if self.file_type == APTRepositoryFileType::List {
            if self.types.len() > 1 {
                bail!("more than one package type");
            }
            if self.uris.len() > 1 {
                bail!("more than one URI");
            }
            if self.suites.len() > 1 {
                bail!("more than one suite");
            }
        }

        Ok(())
    }

    /// Checks if the repository is the one referenced by the handle.
    pub fn is_referenced_repository(&self, handle: APTRepositoryHandle, product: &str) -> bool {
        let (package_type, uri, component) = handle.info(product);

        self.types.contains(&package_type)
            && self
                .uris
                .iter()
                .any(|self_uri| self_uri.trim_end_matches('/') == uri)
            && self.components.contains(&component)
    }

    /// Check if a variant of the given suite is configured in this repository
    pub fn has_suite_variant(&self, base_suite: &str) -> bool {
        self.suites
            .iter()
            .any(|suite| suite_variant(suite).0 == base_suite)
    }

    /// Checks if an official host is configured in the repository.
    pub fn has_official_uri(&self) -> bool {
        for uri in self.uris.iter() {
            if let Some(host) = host_from_uri(uri) {
                if host == "proxmox.com"
                    || host.ends_with(".proxmox.com")
                    || host == "debian.org"
                    || host.ends_with(".debian.org")
                {
                    return true;
                }
            }
        }

        false
    }

    /// Writes a repository in the corresponding format followed by a blank.
    ///
    /// Expects that `basic_check()` for the repository was successful.
    pub fn write(&self, w: &mut dyn Write) -> Result<(), Error> {
        match self.file_type {
            APTRepositoryFileType::List => write_one_line(self, w),
            APTRepositoryFileType::Sources => write_stanza(self, w),
        }
    }
}

/// Get the host part from a given URI.
fn host_from_uri(uri: &str) -> Option<&str> {
    let host = uri.strip_prefix("http")?;
    let host = host.strip_prefix("s").unwrap_or(host);
    let mut host = host.strip_prefix("://")?;

    if let Some(end) = host.find('/') {
        host = &host[..end];
    }

    if let Some(begin) = host.find('@') {
        host = &host[(begin + 1)..];
    }

    if let Some(end) = host.find(':') {
        host = &host[..end];
    }

    Some(host)
}

/// Splits the suite into its base part and variant.
fn suite_variant(suite: &str) -> (&str, &str) {
    let variants = ["-backports-sloppy", "-backports", "-updates", "/updates"];

    for variant in variants.iter() {
        if let Some(base) = suite.strip_suffix(variant) {
            return (base, variant);
        }
    }

    (suite, "")
}

/// Writes a repository in one-line format followed by a blank line.
///
/// Expects that `repo.file_type == APTRepositoryFileType::List`.
///
/// Expects that `basic_check()` for the repository was successful.
fn write_one_line(repo: &APTRepository, w: &mut dyn Write) -> Result<(), Error> {
    if repo.file_type != APTRepositoryFileType::List {
        bail!("not a .list repository");
    }

    if !repo.comment.is_empty() {
        for line in repo.comment.lines() {
            writeln!(w, "#{}", line)?;
        }
    }

    if !repo.enabled {
        write!(w, "# ")?;
    }

    write!(w, "{} ", repo.types[0])?;

    if !repo.options.is_empty() {
        write!(w, "[ ")?;
        repo.options
            .iter()
            .try_for_each(|option| write!(w, "{}={} ", option.key, option.values.join(",")))?;
        write!(w, "] ")?;
    };

    write!(w, "{} ", repo.uris[0])?;
    write!(w, "{} ", repo.suites[0])?;
    writeln!(w, "{}", repo.components.join(" "))?;

    writeln!(w)?;

    Ok(())
}

/// Writes a single stanza followed by a blank line.
///
/// Expects that `repo.file_type == APTRepositoryFileType::Sources`.
fn write_stanza(repo: &APTRepository, w: &mut dyn Write) -> Result<(), Error> {
    if repo.file_type != APTRepositoryFileType::Sources {
        bail!("not a .sources repository");
    }

    if !repo.comment.is_empty() {
        for line in repo.comment.lines() {
            writeln!(w, "#{}", line)?;
        }
    }

    write!(w, "Types:")?;
    repo.types
        .iter()
        .try_for_each(|package_type| write!(w, " {}", package_type))?;
    writeln!(w)?;

    writeln!(w, "URIs: {}", repo.uris.join(" "))?;
    writeln!(w, "Suites: {}", repo.suites.join(" "))?;

    if !repo.components.is_empty() {
        writeln!(w, "Components: {}", repo.components.join(" "))?;
    }

    for option in repo.options.iter() {
        writeln!(w, "{}: {}", option.key, option.values.join(" "))?;
    }

    writeln!(w)?;

    Ok(())
}
