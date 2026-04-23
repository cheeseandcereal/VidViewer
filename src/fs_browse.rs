//! Filesystem browsing for the directory picker modal.
//!
//! Exposes a minimal read-only listing of subdirectories for the server's filesystem.
//! Security surface is small — the app is localhost-only — but we still clamp to
//! directories only and enforce absolute paths.

use std::path::{Path, PathBuf};

use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Clone, Serialize)]
pub struct Listing {
    pub path: String,
    pub parent: Option<String>,
    pub entries: Vec<Entry>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Entry {
    pub name: String,
    pub path: String,
    pub is_dir: bool,
    pub readable: bool,
}

#[derive(Debug, Clone, Error, Serialize)]
#[serde(tag = "error", rename_all = "snake_case")]
pub enum ListError {
    #[error("path must be absolute")]
    PathNotAbsolute,
    #[error("path does not exist")]
    PathNotFound,
    #[error("path is not a directory")]
    PathNotADirectory,
    #[error("path is not readable")]
    PathNotReadable,
    #[error("{message}")]
    Internal { message: String },
}

impl ListError {
    pub fn status(&self) -> axum::http::StatusCode {
        use axum::http::StatusCode;
        match self {
            ListError::Internal { .. } => StatusCode::INTERNAL_SERVER_ERROR,
            _ => StatusCode::BAD_REQUEST,
        }
    }
}

/// List the immediate subdirectories of `path`. Files are filtered out.
pub fn list_dirs(path: &Path) -> Result<Listing, ListError> {
    if !path.is_absolute() {
        return Err(ListError::PathNotAbsolute);
    }
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            return Err(ListError::PathNotFound);
        }
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
            return Err(ListError::PathNotReadable);
        }
        Err(err) => {
            return Err(ListError::Internal {
                message: err.to_string(),
            });
        }
    };
    if !meta.is_dir() {
        return Err(ListError::PathNotADirectory);
    }

    let read_dir = match std::fs::read_dir(path) {
        Ok(rd) => rd,
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
            return Err(ListError::PathNotReadable);
        }
        Err(err) => {
            return Err(ListError::Internal {
                message: err.to_string(),
            });
        }
    };

    let mut entries: Vec<Entry> = Vec::new();
    for ent in read_dir.flatten() {
        let is_dir = ent.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if !is_dir {
            // Also check if it's a symlink to a directory.
            let resolved_is_dir = std::fs::metadata(ent.path())
                .map(|m| m.is_dir())
                .unwrap_or(false);
            if !resolved_is_dir {
                continue;
            }
        }
        let name = crate::util::path::path_to_db_string(ent.file_name().as_ref());
        let full: PathBuf = ent.path();
        let full_str = crate::util::path::path_to_db_string(&full);
        let readable = std::fs::read_dir(&full).is_ok();
        entries.push(Entry {
            name,
            path: full_str,
            is_dir: true,
            readable,
        });
    }
    entries.sort_by_key(|a| a.name.to_lowercase());

    let parent = path.parent().map(crate::util::path::path_to_db_string);
    Ok(Listing {
        path: crate::util::path::path_to_db_string(path),
        parent,
        entries,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_subdirs_not_files() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("subA")).unwrap();
        std::fs::create_dir(tmp.path().join("subB")).unwrap();
        std::fs::write(tmp.path().join("file.mp4"), b"x").unwrap();

        let l = list_dirs(tmp.path()).unwrap();
        let names: Vec<_> = l.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(names, vec!["subA", "subB"]);
    }

    #[test]
    fn rejects_relative() {
        let err = list_dirs(Path::new("rel")).unwrap_err();
        assert!(matches!(err, ListError::PathNotAbsolute));
    }

    #[test]
    fn rejects_missing() {
        let err = list_dirs(Path::new("/no/such/path/for/vidviewer/test")).unwrap_err();
        assert!(matches!(err, ListError::PathNotFound));
    }
}
