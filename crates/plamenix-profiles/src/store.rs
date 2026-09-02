//! [`ProfileStore`] trait + JSON-file-backed implementation.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::ProfileError;
use crate::profile::{Profile, ProfileId};

/// Dyn-compatible profile-store interface.
///
/// Implementations include [`JsonFileStore`] (single file on disk) and
/// could grow a cloud-backed variant later. The interface stays small:
/// list, get, save (insert or update), delete.
pub trait ProfileStore: Send + Sync {
    /// Returns every saved profile in the order they appear in the
    /// backing store.
    ///
    /// # Errors
    ///
    /// Returns [`ProfileError::Io`] when the backing file is
    /// unreachable, or [`ProfileError::InvalidFile`] when its contents
    /// fail to parse.
    fn list(&self) -> Result<Vec<Profile>, ProfileError>;

    /// Returns the profile with the supplied id.
    ///
    /// # Errors
    ///
    /// Returns [`ProfileError::NotFound`] when no profile matches,
    /// otherwise the backing store's failure mode.
    fn get(&self, id: ProfileId) -> Result<Profile, ProfileError>;

    /// Inserts a new profile or updates the existing one with the same
    /// id. Returns the saved profile (callers typically discard).
    ///
    /// # Errors
    ///
    /// Returns the backing store's failure mode if the write cannot be
    /// completed.
    fn save(&self, profile: Profile) -> Result<Profile, ProfileError>;

    /// Removes the profile with the supplied id. Idempotent: returns
    /// `Ok(())` even when no profile matched.
    ///
    /// # Errors
    ///
    /// Returns the backing store's failure mode if the write fails.
    fn delete(&self, id: ProfileId) -> Result<(), ProfileError>;

    /// Stamps `last_used_at` on the profile with the supplied id and
    /// persists the change. Callers invoke this after a successful
    /// `DbDriver::connect` against the profile. No-op when the id is
    /// unknown — failure to touch is never worth propagating up the
    /// connect path.
    ///
    /// # Errors
    ///
    /// Returns the backing store's failure mode if the write fails.
    fn touch(&self, id: ProfileId, at_epoch_ms: i64) -> Result<(), ProfileError>;

    /// Stamps `last_disconnected_at` on the profile with the supplied
    /// id and persists the change. Callers invoke this after an
    /// explicit `DbDriver::close` against a session that was opened
    /// from this profile. No-op when the id is unknown.
    ///
    /// # Errors
    ///
    /// Returns the backing store's failure mode if the write fails.
    fn touch_disconnected(&self, id: ProfileId, at_epoch_ms: i64) -> Result<(), ProfileError>;
}

/// JSON-on-disk implementation of [`ProfileStore`].
///
/// Stores the entire profile array as a single `profiles.json` file.
/// Writes go through a sibling temp file and an atomic `rename`, so an
/// interrupted save never leaves a partially written file.
#[derive(Clone, Debug)]
pub struct JsonFileStore {
    path: PathBuf,
}

impl JsonFileStore {
    /// Creates a new store backed by the given path. The file is not
    /// created until the first [`Self::save`] call; [`Self::list`] on
    /// an absent file returns an empty vector.
    #[must_use]
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// Returns the on-disk path the store reads and writes.
    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    fn read_all(&self) -> Result<Vec<Profile>, ProfileError> {
        match std::fs::read(&self.path) {
            Ok(bytes) => {
                let file: ProfilesFile =
                    serde_json::from_slice(&bytes).map_err(|err| ProfileError::InvalidFile {
                        path: self.path.clone(),
                        message: err.to_string(),
                    })?;
                Ok(file.profiles)
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(source) => Err(ProfileError::Io {
                path: self.path.clone(),
                source,
            }),
        }
    }

    fn write_all(&self, profiles: &[Profile]) -> Result<(), ProfileError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| ProfileError::Io {
                path: parent.to_path_buf(),
                source,
            })?;
        }

        let payload = serde_json::to_vec_pretty(&ProfilesFile {
            profiles: profiles.to_vec(),
        })
        .map_err(|err| ProfileError::InvalidFile {
            path: self.path.clone(),
            message: err.to_string(),
        })?;

        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, payload).map_err(|source| ProfileError::Io {
            path: tmp.clone(),
            source,
        })?;
        std::fs::rename(&tmp, &self.path).map_err(|source| ProfileError::Io {
            path: self.path.clone(),
            source,
        })?;
        Ok(())
    }
}

impl ProfileStore for JsonFileStore {
    fn list(&self) -> Result<Vec<Profile>, ProfileError> {
        self.read_all()
    }

    fn get(&self, id: ProfileId) -> Result<Profile, ProfileError> {
        self.read_all()?
            .into_iter()
            .find(|p| p.id == id)
            .ok_or_else(|| ProfileError::NotFound(format!("{:?}", id.0)))
    }

    fn save(&self, profile: Profile) -> Result<Profile, ProfileError> {
        let mut profiles = self.read_all()?;
        if let Some(existing) = profiles.iter_mut().find(|p| p.id == profile.id) {
            *existing = profile.clone();
        } else {
            profiles.push(profile.clone());
        }
        self.write_all(&profiles)?;
        Ok(profile)
    }

    fn delete(&self, id: ProfileId) -> Result<(), ProfileError> {
        let mut profiles = self.read_all()?;
        let original_len = profiles.len();
        profiles.retain(|p| p.id != id);
        if profiles.len() != original_len {
            self.write_all(&profiles)?;
        }
        Ok(())
    }

    fn touch(&self, id: ProfileId, at_epoch_ms: i64) -> Result<(), ProfileError> {
        let mut profiles = self.read_all()?;
        let Some(target) = profiles.iter_mut().find(|p| p.id == id) else {
            return Ok(());
        };
        target.last_used_at = Some(at_epoch_ms);
        self.write_all(&profiles)
    }

    fn touch_disconnected(&self, id: ProfileId, at_epoch_ms: i64) -> Result<(), ProfileError> {
        let mut profiles = self.read_all()?;
        let Some(target) = profiles.iter_mut().find(|p| p.id == id) else {
            return Ok(());
        };
        target.last_disconnected_at = Some(at_epoch_ms);
        self.write_all(&profiles)
    }
}

/// Versioned envelope written to disk. Future schema migrations bump
/// the `version` field while reusing this outer shape so old files
/// still parse during transition.
#[derive(Debug, Serialize, Deserialize)]
struct ProfilesFile {
    #[serde(default = "Vec::new")]
    profiles: Vec<Profile>,
}
