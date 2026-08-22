use std::path::{Component, Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::{CoreError, CoreResult};

/// A filesystem root selected by the composition root.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceRoot(PathBuf);

impl WorkspaceRoot {
    /// Construct a root. The caller that performs filesystem access must canonicalize it first.
    pub fn new(path: impl Into<PathBuf>) -> CoreResult<Self> {
        let path = path.into();
        if !path.is_absolute() {
            return Err(CoreError::invalid("workspace root", "workspace root must be absolute"));
        }
        Ok(Self(path))
    }

    /// Return the root path.
    pub fn as_path(&self) -> &Path {
        &self.0
    }

    /// Resolve a path lexically after rejecting absolute paths and parent traversal.
    pub fn resolve(&self, path: impl AsRef<Path>) -> CoreResult<(WorkspacePath, PathBuf)> {
        let relative = WorkspacePath::new(path)?;
        Ok((relative.clone(), self.0.join(relative.as_path())))
    }

    /// Check lexical containment of a candidate path.
    pub fn contains_lexically(&self, candidate: impl AsRef<Path>) -> bool {
        candidate.as_ref().starts_with(&self.0)
    }
}

/// A normalized, workspace-relative path.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct WorkspacePath(PathBuf);

impl WorkspacePath {
    /// Normalize a relative path without touching the filesystem.
    pub fn new(path: impl AsRef<Path>) -> CoreResult<Self> {
        let path = path.as_ref();
        if path.as_os_str().is_empty() {
            return Ok(Self(PathBuf::from(".")));
        }
        let mut normalized = PathBuf::new();
        for component in path.components() {
            match component {
                Component::CurDir => {}
                Component::Normal(part) => normalized.push(part),
                Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                    return Err(CoreError::PathEscape {
                        path: path.to_string_lossy().into_owned(),
                    });
                }
            }
        }
        if normalized.as_os_str().is_empty() {
            normalized.push(".");
        }
        Ok(Self(normalized))
    }

    /// Return the normalized relative path.
    pub fn as_path(&self) -> &Path {
        &self.0
    }

    /// Return a display-safe path string.
    pub fn display(&self) -> String {
        self.0.to_string_lossy().into_owned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lexical_containment_rejects_absolute_and_parent_paths() {
        assert!(WorkspacePath::new("src/main.rs").is_ok());
        assert!(WorkspacePath::new("../outside").is_err());
        assert!(WorkspacePath::new("/outside").is_err());
    }
}
