//! The workspace folder: `<folder>/.regents/` holds the whole state of a
//! Regents workspace (ADR-0003). This module creates the initial structure and
//! reads `workspace.toml`, the source of the stable workspace ID.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// Directory inside the workspace folder that holds the workspace state.
pub(crate) const STATE_DIR: &str = ".regents";
const MANIFEST_FILE: &str = "workspace.toml";
const DEFAULT_MAX_ACTIVE_THREADS: u32 = 3;

/// Contents of `.regents/workspace.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct WorkspaceManifest {
    /// Stable workspace ID. It does not depend on the folder path.
    pub(crate) id: String,
    pub(crate) name: String,
    #[serde(default = "default_max_active_threads")]
    pub(crate) max_active_threads: u32,
}

fn default_max_active_threads() -> u32 {
    DEFAULT_MAX_ACTIVE_THREADS
}

/// Binding of a herdr workspace (the layout container) to a workspace folder.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct FolderBinding {
    /// ID from `workspace.toml`: the workspace identity.
    pub(crate) id: String,
    /// Folder that contains `.regents/`.
    pub(crate) root: PathBuf,
}

#[derive(Debug)]
pub(crate) struct OpenedFolder {
    pub(crate) manifest: WorkspaceManifest,
    /// True when the folder had no `.regents/` and received the initial structure.
    pub(crate) initialized: bool,
}

/// Reads `.regents/workspace.toml` of `root`.
pub(crate) fn read_manifest(root: &Path) -> io::Result<WorkspaceManifest> {
    let path = root.join(STATE_DIR).join(MANIFEST_FILE);
    let content = std::fs::read_to_string(&path)?;
    let manifest: WorkspaceManifest = toml::from_str(&content).map_err(|err| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            format!("invalid {}: {err}", path.display()),
        )
    })?;
    if manifest.id.trim().is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("{} has an empty id", path.display()),
        ));
    }
    Ok(manifest)
}

/// Opens the workspace in `root`. A folder without `.regents/` receives the
/// initial structure. A folder that has `.regents/` keeps its files: this
/// function only creates the items that are missing and never overwrites one.
pub(crate) fn open_or_init(root: &Path) -> io::Result<OpenedFolder> {
    let state_dir = root.join(STATE_DIR);
    let initialized = !state_dir.exists();
    for dir in [
        PathBuf::from("agents"),
        Path::new("channels").join("geral"),
        Path::new("wiki").join("raw"),
    ] {
        std::fs::create_dir_all(state_dir.join(dir))?;
    }
    let manifest = WorkspaceManifest {
        id: new_workspace_id(root),
        name: default_name(root),
        max_active_threads: DEFAULT_MAX_ACTIVE_THREADS,
    };
    let manifest_toml = toml::to_string(&manifest)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err.to_string()))?;
    write_if_missing(&state_dir.join(MANIFEST_FILE), &manifest_toml)?;
    write_if_missing(
        &state_dir.join("agents").join("chief-of-staff.md"),
        CHIEF_OF_STAFF,
    )?;
    let wiki = state_dir.join("wiki");
    write_if_missing(&wiki.join("index.md"), WIKI_INDEX)?;
    write_if_missing(&wiki.join("log.md"), WIKI_LOG)?;
    write_if_missing(&wiki.join("SCHEMA.md"), WIKI_SCHEMA)?;
    Ok(OpenedFolder {
        manifest: read_manifest(root)?,
        initialized,
    })
}

fn write_if_missing(path: &Path, content: &str) -> io::Result<()> {
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
    {
        Ok(mut file) => file.write_all(content.as_bytes()),
        Err(err) if err.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(err) => Err(err),
    }
}

fn default_name(root: &Path) -> String {
    root.file_name()
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("workspace")
        .to_string()
}

/// A random 128-bit ID as 32 lowercase hex characters.
fn new_workspace_id(root: &Path) -> String {
    use sha2::{Digest, Sha256};
    use std::hash::BuildHasher;

    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or(0);
    // `RandomState` has random keys for each process and each instance.
    let random = std::collections::hash_map::RandomState::new().hash_one(nanos);
    let mut hasher = Sha256::new();
    hasher.update(nanos.to_le_bytes());
    hasher.update(random.to_le_bytes());
    hasher.update(std::process::id().to_le_bytes());
    hasher.update(root.as_os_str().as_encoded_bytes());
    hasher
        .finalize()
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

const CHIEF_OF_STAFF: &str = r#"+++
name = "chief"
cli = "claude"
# model = ""  (sem valor: a CLI usa o modelo padrão dela)
isolation = "shared"
approval = "default"
reserved = true
+++
Você é o Chief of Staff deste workspace do Regents.

- Você conhece a estrutura do workspace: agentes, canais e wiki.
- Só você (e o humano) muda a estrutura: cria e remove agentes e canais e
  muda a configuração do workspace.
- Você é o único agente que escreve na wiki. Siga `wiki/SCHEMA.md`.
- Você responde às mensagens sem menção no canal `#geral`.
"#;

const WIKI_INDEX: &str = "# Índice\n\nA wiki ainda não tem páginas.\n";

const WIKI_LOG: &str =
    "# Log\n\nRegistro das ingestões e das mudanças da wiki, da mais antiga para a mais nova.\n";

const WIKI_SCHEMA: &str = r#"# Convenções da wiki

Esta wiki segue o padrão LLM Wiki.

- `raw/` guarda as fontes originais. Uma fonte é imutável.
- Cada página é um arquivo Markdown nesta pasta, com links relativos para outras páginas.
- `index.md` lista todas as páginas, com uma linha de resumo para cada uma.
- `log.md` registra cada ingestão e cada mudança, com data.
- Só o Chief of Staff escreve na wiki. Os outros agentes só leem, a partir do `index.md`.
"#;

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_folder(name: &str) -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "regents-folder-{name}-{}-{stamp}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    fn files_under(root: &Path) -> Vec<(PathBuf, Option<Vec<u8>>)> {
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
    fn empty_folder_receives_the_initial_structure() {
        let root = temp_folder("empty");

        let opened = open_or_init(&root).unwrap();

        assert!(opened.initialized);
        let state = root.join(".regents");
        for dir in ["agents", "channels/geral", "wiki/raw"] {
            assert!(state.join(dir).is_dir(), "missing directory {dir}");
        }
        for file in [
            "workspace.toml",
            "agents/chief-of-staff.md",
            "wiki/index.md",
            "wiki/log.md",
            "wiki/SCHEMA.md",
        ] {
            assert!(state.join(file).is_file(), "missing file {file}");
        }
        assert_eq!(
            std::fs::read_dir(state.join("channels/geral"))
                .unwrap()
                .count(),
            0,
            "the channel database belongs to the channel slice"
        );
        assert_eq!(opened.manifest.id.len(), 32);
        assert_eq!(
            opened.manifest.name,
            root.file_name().unwrap().to_str().unwrap()
        );
        assert_eq!(opened.manifest.max_active_threads, 3);
        assert_eq!(read_manifest(&root).unwrap(), opened.manifest);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn two_folders_get_different_ids() {
        let first = temp_folder("id-a");
        let second = temp_folder("id-b");

        let first_id = open_or_init(&first).unwrap().manifest.id;
        let second_id = open_or_init(&second).unwrap().manifest.id;

        assert_ne!(first_id, second_id);
        std::fs::remove_dir_all(first).unwrap();
        std::fs::remove_dir_all(second).unwrap();
    }

    #[test]
    fn existing_state_keeps_its_id_and_files() {
        let root = temp_folder("existing");
        let state = root.join(".regents");
        std::fs::create_dir_all(state.join("wiki")).unwrap();
        std::fs::write(
            state.join("workspace.toml"),
            "id = \"from-another-machine\"\nname = \"Projeto\"\nmax_active_threads = 7\n",
        )
        .unwrap();
        std::fs::write(state.join("wiki/index.md"), "# my pages\n").unwrap();
        let before = files_under(&root);

        let opened = open_or_init(&root).unwrap();

        assert!(!opened.initialized);
        assert_eq!(
            opened.manifest,
            WorkspaceManifest {
                id: "from-another-machine".into(),
                name: "Projeto".into(),
                max_active_threads: 7,
            }
        );
        let after = files_under(&root);
        for (path, content) in &before {
            assert!(
                after.contains(&(path.clone(), content.clone())),
                "{} changed",
                path.display()
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn opening_twice_changes_nothing() {
        let root = temp_folder("twice");
        let first = open_or_init(&root).unwrap();
        let before = files_under(&root);

        let second = open_or_init(&root).unwrap();

        assert!(!second.initialized);
        assert_eq!(second.manifest, first.manifest);
        assert_eq!(files_under(&root), before);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn invalid_manifest_is_an_error_and_is_not_replaced() {
        let root = temp_folder("invalid");
        std::fs::create_dir_all(root.join(".regents")).unwrap();
        std::fs::write(root.join(".regents/workspace.toml"), "id = [").unwrap();

        let err = open_or_init(&root).unwrap_err();

        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert_eq!(
            std::fs::read_to_string(root.join(".regents/workspace.toml")).unwrap(),
            "id = ["
        );
        std::fs::remove_dir_all(root).unwrap();
    }
}
