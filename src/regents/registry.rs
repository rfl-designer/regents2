//! Global registry of workspace folders (`<config dir>/workspaces.toml`).
//! It keeps the path of each workspace and the last known ID; the workspace
//! state stays in the folder (ADR-0003). The ID in the folder is the source of
//! truth: the registry copy only names a workspace whose folder is gone
//! (disconnected workspace), so that `relink` can find it again.

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
    /// Last known ID from `workspace.toml` of `path`. Absent in registries
    /// written before Regents recorded it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) id: Option<String>,
    /// Order of the last use (add, link or reopen); a higher value is more
    /// recent. Absent in registries written before Regents recorded it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) last_used: Option<u64>,
}

impl RegistryEntry {
    /// ID of the registered workspace: the last known ID, or the ID in the
    /// folder for an entry written before Regents recorded it. That case
    /// reads the folder manifest.
    pub(crate) fn resolve_workspace_id(&self) -> Option<String> {
        self.id.clone().or_else(|| {
            folder::read_manifest(&self.path)
                .ok()
                .map(|manifest| manifest.id)
        })
    }
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

    /// Entries from the most recently used to the least recently used.
    /// Entries without a use mark come last, in file order.
    pub(crate) fn by_recent_use(&self) -> Vec<&RegistryEntry> {
        let mut entries: Vec<&RegistryEntry> = self.workspaces.iter().collect();
        // A stable sort keeps the file order of entries with equal marks.
        entries.sort_by_key(|entry| std::cmp::Reverse(entry.last_used));
        entries
    }

    fn push(&mut self, root: PathBuf, id: String) {
        self.workspaces.push(RegistryEntry {
            path: root,
            id: Some(id),
            last_used: None,
        });
    }

    /// Marks `root` as the most recently used entry and records its ID.
    /// Returns false when the registry does not contain `root`.
    fn touch(&mut self, root: &Path, id: &str) -> bool {
        let next = self
            .workspaces
            .iter()
            .filter_map(|entry| entry.last_used)
            .max()
            .map_or(1, |last| last.saturating_add(1));
        match self.workspaces.iter_mut().find(|entry| entry.path == root) {
            Some(entry) => {
                entry.last_used = Some(next);
                entry.id = Some(id.to_owned());
                true
            }
            None => false,
        }
    }
}

/// Records that the registered folder `root` was used now, so that it is the
/// first candidate when Regents reopens a workspace. Call it only when a
/// workspace opens (add, link, reopen), never on a focus change.
pub(crate) fn mark_used(registry_path: &Path, root: &Path, id: &str) -> io::Result<()> {
    let mut registry = Registry::load(registry_path)?;
    if registry.touch(root, id) {
        registry.save(registry_path)?;
    }
    Ok(())
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
                "workspace id {id} is already registered at {}; if the folder moved, \
                 use `regents workspace relink {id} <folder>`",
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
        if let Some(entry) = registry
            .entries()
            .iter()
            .find(|entry| entry.resolve_workspace_id().as_deref() == Some(id.as_str()))
        {
            return Err(AddFolderError::IdConflict {
                id,
                registered_path: entry.path.clone(),
            });
        }
    }
    let opened = folder::open_or_init(&root)?;
    if !already_registered {
        registry.push(root.clone(), opened.manifest.id.clone());
    }
    registry.touch(&root, &opened.manifest.id);
    registry.save(registry_path)?;
    Ok(AddedFolder {
        root,
        folder: opened,
        already_registered,
    })
}

/// Why `relink` or `remove` cannot change the registry.
#[derive(Debug)]
pub(crate) enum RegistryChangeError {
    /// No registered workspace has this ID.
    NotFound(String),
    /// The folder is still at the registered path.
    NotDisconnected {
        id: String,
        path: PathBuf,
    },
    NotADirectory(PathBuf),
    /// The new folder does not hold the workspace: `found` is the ID in its
    /// `workspace.toml`, or `None` when it has no such file.
    IdMismatch {
        id: String,
        path: PathBuf,
        found: Option<String>,
    },
    /// Another registered workspace uses the new folder.
    FolderInUse(PathBuf),
    Io(io::Error),
}

impl RegistryChangeError {
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::NotFound(_) => "workspace_not_found",
            Self::NotDisconnected { .. } => "workspace_not_disconnected",
            Self::NotADirectory(_) => "workspace_folder_not_found",
            Self::IdMismatch { .. } => "workspace_id_mismatch",
            Self::FolderInUse(_) => "workspace_folder_in_use",
            Self::Io(_) => "workspace_registry_failed",
        }
    }
}

impl std::fmt::Display for RegistryChangeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound(id) => write!(f, "no registered workspace has the id {id}"),
            Self::NotDisconnected { id, path } => write!(
                f,
                "workspace {id} is not disconnected: its folder is still at {}",
                path.display()
            ),
            Self::NotADirectory(path) => write!(f, "{} is not a directory", path.display()),
            Self::IdMismatch {
                id,
                path,
                found: Some(found),
            } => write!(
                f,
                "{} does not hold workspace {id}: its workspace.toml has the id {found}",
                path.display()
            ),
            Self::IdMismatch {
                id,
                path,
                found: None,
            } => write!(
                f,
                "{} does not hold workspace {id}: it has no .regents/workspace.toml",
                path.display()
            ),
            Self::FolderInUse(path) => write!(
                f,
                "{} is already registered as another workspace",
                path.display()
            ),
            Self::Io(err) => write!(f, "{err}"),
        }
    }
}

impl From<io::Error> for RegistryChangeError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

impl Registry {
    fn position_of(&self, id: &str) -> Result<usize, RegistryChangeError> {
        self.workspaces
            .iter()
            .position(|entry| entry.resolve_workspace_id().as_deref() == Some(id))
            .ok_or_else(|| RegistryChangeError::NotFound(id.to_owned()))
    }
}

/// Registers the disconnected workspace `id` at `path` and returns the
/// updated entry. The folder in `path` must hold the same ID in its
/// `workspace.toml`; this function never writes in a folder.
pub(crate) fn relink(
    registry_path: &Path,
    id: &str,
    path: &Path,
) -> Result<RegistryEntry, RegistryChangeError> {
    let mut registry = Registry::load(registry_path)?;
    let index = registry.position_of(id)?;
    let old_path = &registry.workspaces[index].path;
    if folder::holds_workspace(old_path, id) {
        return Err(RegistryChangeError::NotDisconnected {
            id: id.to_owned(),
            path: old_path.clone(),
        });
    }
    if !path.is_dir() {
        return Err(RegistryChangeError::NotADirectory(path.to_path_buf()));
    }
    let root = crate::platform::canonical_path(path)?;
    match folder::read_manifest(&root) {
        Ok(manifest) if manifest.id == id => {}
        Ok(manifest) => {
            return Err(RegistryChangeError::IdMismatch {
                id: id.to_owned(),
                path: root,
                found: Some(manifest.id),
            })
        }
        Err(err) if err.kind() == io::ErrorKind::NotFound => {
            return Err(RegistryChangeError::IdMismatch {
                id: id.to_owned(),
                path: root,
                found: None,
            })
        }
        Err(err) => return Err(err.into()),
    }
    if registry
        .workspaces
        .iter()
        .enumerate()
        .any(|(other, entry)| other != index && entry.path == root)
    {
        return Err(RegistryChangeError::FolderInUse(root));
    }
    registry.workspaces[index].path = root.clone();
    registry.touch(&root, id);
    registry.save(registry_path)?;
    Ok(registry.workspaces[index].clone())
}

/// Removes the workspace `id` from the registry and returns its entry. The
/// folder and its `.regents/` stay unchanged.
pub(crate) fn remove(registry_path: &Path, id: &str) -> Result<RegistryEntry, RegistryChangeError> {
    let mut registry = Registry::load(registry_path)?;
    let index = registry.position_of(id)?;
    let removed = registry.workspaces.remove(index);
    registry.save(registry_path)?;
    Ok(removed)
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
    fn added_folder_is_registered_by_path_and_last_known_id() {
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
                path: added.root.clone(),
                id: Some(added.folder.manifest.id.clone()),
                last_used: Some(1),
            }]
        );
        std::fs::remove_dir_all(base).unwrap();
    }

    /// ADR-0003: the registry keeps the path, a copy of the ID and the use
    /// order of each workspace, and no other workspace state.
    #[test]
    fn the_registry_file_keeps_only_the_path_the_id_copy_and_the_use_order() {
        let base = temp_dir("only-path");
        let registry_path = base.join("workspaces.toml");
        for name in ["first", "second"] {
            let folder = base.join(name);
            std::fs::create_dir_all(&folder).unwrap();
            add_folder(&registry_path, &folder).unwrap();
        }
        let first = crate::platform::canonical_path(&base.join("first")).unwrap();
        let id = folder::read_manifest(&first).unwrap().id;
        mark_used(&registry_path, &first, &id).unwrap();

        let file: toml::Table =
            toml::from_str(&std::fs::read_to_string(&registry_path).unwrap()).unwrap();

        assert_eq!(file.keys().collect::<Vec<_>>(), vec!["workspaces"]);
        let entries = file["workspaces"].as_array().unwrap();
        assert_eq!(entries.len(), 2);
        for entry in entries {
            let mut keys: Vec<&str> = entry
                .as_table()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect();
            keys.sort_unstable();
            assert_eq!(keys, vec!["id", "last_used", "path"], "{entry}");
        }
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn relink_returns_the_updated_registry_entry() {
        let base = temp_dir("relink-entry");
        let registry_path = base.join("workspaces.toml");
        let folder = base.join("project");
        std::fs::create_dir_all(&folder).unwrap();
        let added = add_folder(&registry_path, &folder).unwrap();
        let moved = base.join("moved");
        std::fs::rename(&added.root, &moved).unwrap();

        let entry = relink(&registry_path, &added.folder.manifest.id, &moved).unwrap();

        assert_eq!(
            Registry::load(&registry_path).unwrap().entries(),
            std::slice::from_ref(&entry)
        );
        assert_eq!(entry.path, crate::platform::canonical_path(&moved).unwrap());
        assert_eq!(entry.id.as_deref(), Some(added.folder.manifest.id.as_str()));
        assert_eq!(entry.last_used, Some(2));
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
    fn reopen_order_starts_with_the_most_recently_used_folder() {
        let base = temp_dir("recent");
        let registry_path = base.join("workspaces.toml");
        let mut roots = Vec::new();
        for name in ["a", "b", "c"] {
            let folder = base.join(name);
            std::fs::create_dir_all(&folder).unwrap();
            roots.push(add_folder(&registry_path, &folder).unwrap().root);
        }

        let id = folder::read_manifest(&roots[0]).unwrap().id;
        mark_used(&registry_path, &roots[0], &id).unwrap();

        let order: Vec<PathBuf> = Registry::load(&registry_path)
            .unwrap()
            .by_recent_use()
            .into_iter()
            .map(|entry| entry.path.clone())
            .collect();
        assert_eq!(
            order,
            vec![roots[0].clone(), roots[2].clone(), roots[1].clone()]
        );
        std::fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn a_registry_without_use_marks_keeps_the_file_order() {
        let base = temp_dir("unmarked");
        let registry_path = base.join("workspaces.toml");
        std::fs::write(
            &registry_path,
            "[[workspaces]]\npath = \"/work/first\"\n\n[[workspaces]]\npath = \"/work/second\"\n",
        )
        .unwrap();

        let registry = Registry::load(&registry_path).unwrap();

        let order: Vec<PathBuf> = registry
            .by_recent_use()
            .into_iter()
            .map(|entry| entry.path.clone())
            .collect();
        assert_eq!(
            order,
            vec![PathBuf::from("/work/first"), PathBuf::from("/work/second")]
        );
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
