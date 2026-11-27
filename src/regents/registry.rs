//! Global registry of workspace folders (`<config dir>/workspaces.toml`).
//! It keeps only the path of each workspace; the workspace state stays in the
//! folder (ADR-0003).

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::folder::{self, OpenedFolder};

const REGISTRY_FILE: &str = "workspaces.toml";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Registry {
    #[serde(default)]
    workspaces: Vec<RegistryEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct RegistryEntry {
    pub(crate) path: PathBuf,
}

/// Registry file of the running product, for example
/// `~/.config/regents/workspaces.toml`.
pub(crate) fn default_path() -> PathBuf {
    crate::config::config_dir().join(REGISTRY_FILE)
}

impl Registry {
    /// Reads the registry. A missing file is an empty registry.
    pub(crate) fn load(path: &Path) -> io::Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(content) => toml::from_str(&content).map_err(|err| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("invalid {}: {err}", path.display()),
                )
            }),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(err),
        }
    }

    /// Writes the registry atomically.
    pub(crate) fn save(&self, path: &Path) -> io::Result<()> {
        let content = toml::to_string(self)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err.to_string()))?;
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(parent)?;
        let mut tmp_name = path.as_os_str().to_owned();
        tmp_name.push(".tmp");
        let tmp_path = PathBuf::from(tmp_name);
        let mut temporary = std::fs::File::create(&tmp_path)?;
        let result = temporary
            .write_all(content.as_bytes())
            .and_then(|()| temporary.sync_all());
        drop(temporary);
        if let Err(err) = result.and_then(|()| crate::platform::replace_file(&tmp_path, path)) {
            let _ = std::fs::remove_file(&tmp_path);
            return Err(err);
        }
        crate::platform::sync_parent_directory(parent)
    }

    pub(crate) fn entries(&self) -> &[RegistryEntry] {
        &self.workspaces
    }

    pub(crate) fn contains(&self, root: &Path) -> bool {
        self.workspaces.iter().any(|entry| entry.path == root)
    }

    fn push(&mut self, root: PathBuf) {
        self.workspaces.push(RegistryEntry { path: root });
    }
}

#[derive(Debug)]
pub(crate) struct AddedFolder {
    /// Canonical folder path, as the registry keeps it.
    pub(crate) root: PathBuf,
    pub(crate) folder: OpenedFolder,
    pub(crate) already_registered: bool,
}

#[derive(Debug)]
pub(crate) enum AddFolderError {
    NotADirectory(PathBuf),
    /// Another registered folder has the same workspace ID, for example a copy.
    IdConflict {
        id: String,
        registered_path: PathBuf,
    },
    Io(io::Error),
}

impl AddFolderError {
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::NotADirectory(_) => "workspace_folder_not_found",
            Self::IdConflict { .. } => "workspace_id_conflict",
            Self::Io(_) => "workspace_add_failed",
        }
    }
}

impl std::fmt::Display for AddFolderError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotADirectory(path) => write!(f, "{} is not a directory", path.display()),
            Self::IdConflict {
                id,
                registered_path,
            } => write!(
                f,
                "workspace id {id} is already registered at {}",
                registered_path.display()
            ),
            Self::Io(err) => write!(f, "{err}"),
        }
    }
}

impl From<io::Error> for AddFolderError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

/// Registers `path` as a workspace folder. The folder receives the initial
/// structure when it has no `.regents/`. Adding a registered folder again
/// changes nothing.
pub(crate) fn add_folder(registry_path: &Path, path: &Path) -> Result<AddedFolder, AddFolderError> {
    if !path.is_dir() {
        return Err(AddFolderError::NotADirectory(path.to_path_buf()));
    }
    let root = crate::platform::canonical_path(path)?;
    let mut registry = Registry::load(registry_path)?;
    let already_registered = registry.contains(&root);
    // Check the existing ID before anything is written. A folder without a
    // manifest gets a new ID, which cannot conflict.
    let existing_id = match folder::read_manifest(&root) {
        Ok(manifest) => Some(manifest.id),
        Err(err) if err.kind() == io::ErrorKind::NotFound => None,
        Err(err) => return Err(err.into()),
    };
    if let (false, Some(id)) = (already_registered, existing_id) {
        if let Some(entry) = registry.entries().iter().find(|entry| {
            folder::read_manifest(&entry.path).is_ok_and(|manifest| manifest.id == id)
        }) {
            return Err(AddFolderError::IdConflict {
                id,
                registered_path: entry.path.clone(),
            });
        }
    }
    let opened = folder::open_or_init(&root)?;
    if !already_registered {
        registry.push(root.clone());
        registry.save(registry_path)?;
    }
    Ok(AddedFolder {
        root,
        folder: opened,
        already_registered,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "regents-registry-{name}-{}-{stamp}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    /// Every path under `root`, with the contents of each file.
    fn tree(root: &Path) -> Vec<(PathBuf, Option<Vec<u8>>)> {
        fn walk(root: &Path, dir: &Path, out: &mut Vec<(PathBuf, Option<Vec<u8>>)>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                let relative = path.strip_prefix(root).unwrap().to_path_buf();
                if path.is_dir() {
                    out.push((relative, None));
                    walk(root, &path, out);
                } else {
                    out.push((relative, Some(std::fs::read(&path).unwrap())));
                }
            }
        }
        let mut out = Vec::new();
        walk(root, root, &mut out);
        out.sort();
        out
    }

    #[test]
    fn missing_registry_is_empty() {
        let base = temp_dir("missing");

        let registry = Registry::load(&base.join("workspaces.toml")).unwrap();

        assert!(registry.entries().is_empty());
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn added_folder_is_registered_by_path_only() {
        let base = temp_dir("add");
        let folder = base.join("project");
        std::fs::create_dir_all(&folder).unwrap();
        let registry_path = base.join("config").join("workspaces.toml");

        let added = add_folder(&registry_path, &folder).unwrap();

        assert!(!added.already_registered);
        assert!(added.folder.initialized);
        assert_eq!(
            added.root,
            crate::platform::canonical_path(&folder).unwrap()
        );
        let registry = Registry::load(&registry_path).unwrap();
        assert_eq!(
            registry.entries(),
            &[RegistryEntry {
                path: added.root.clone()
            }]
        );
        let content = std::fs::read_to_string(&registry_path).unwrap();
        assert!(
            !content.contains(&added.folder.manifest.id),
            "the registry keeps only paths: {content}"
        );
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn adding_the_same_folder_twice_registers_it_once() {
        let base = temp_dir("twice");
        let folder = base.join("project");
        std::fs::create_dir_all(folder.join("sub")).unwrap();
        let registry_path = base.join("workspaces.toml");

        let first = add_folder(&registry_path, &folder).unwrap();
        let second = add_folder(&registry_path, &folder.join("sub").join("..")).unwrap();

        assert!(second.already_registered);
        assert!(!second.folder.initialized);
        assert_eq!(second.folder.manifest.id, first.folder.manifest.id);
        assert_eq!(Registry::load(&registry_path).unwrap().entries().len(), 1);
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn a_copy_with_the_same_id_is_refused() {
        let base = temp_dir("copy");
        let original = base.join("original");
        let copy = base.join("copy");
        std::fs::create_dir_all(&original).unwrap();
        let registry_path = base.join("workspaces.toml");
        let added = add_folder(&registry_path, &original).unwrap();
        std::fs::create_dir_all(copy.join(".regents")).unwrap();
        std::fs::copy(
            original.join(".regents/workspace.toml"),
            copy.join(".regents/workspace.toml"),
        )
        .unwrap();

        let before = tree(&copy);

        let err = add_folder(&registry_path, &copy).unwrap_err();

        assert_eq!(err.code(), "workspace_id_conflict");
        assert!(err.to_string().contains(&added.folder.manifest.id), "{err}");
        assert_eq!(Registry::load(&registry_path).unwrap().entries().len(), 1);
        assert_eq!(tree(&copy), before, "a refused copy must not change");
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn a_missing_folder_is_refused() {
        let base = temp_dir("not-dir");
        let registry_path = base.join("workspaces.toml");

        let err = add_folder(&registry_path, &base.join("nope")).unwrap_err();

        assert_eq!(err.code(), "workspace_folder_not_found");
        assert!(!registry_path.exists());
        std::fs::remove_dir_all(base).unwrap();
    }
}
