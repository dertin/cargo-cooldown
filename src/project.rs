//! Project discovery for crate roots, workspaces, and member-specific configs.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use cargo_metadata::Metadata;
use clap_cargo::{Manifest, Workspace};

/// Cargo project shape used to decide which config files can be generated.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectKind {
    Crate,
    Workspace,
}

/// Workspace member with enough path data to locate member overrides.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectMember {
    pub name: String,
    pub manifest_path: PathBuf,
    pub dir: PathBuf,
}

/// Resolved project context for config loading or `cargo cooldown init`.
#[derive(Debug, Clone)]
pub struct ProjectContext {
    pub cwd: PathBuf,
    pub kind: ProjectKind,
    pub workspace_root: PathBuf,
    pub target_directory: PathBuf,
    pub members: Vec<ProjectMember>,
    pub active_member: Option<ProjectMember>,
}

#[derive(Debug, Clone, Default)]
struct RuntimeSelection {
    manifest_path: Option<PathBuf>,
    packages: Vec<String>,
    workspace: bool,
    all: bool,
    exclude: Vec<String>,
}

impl ProjectContext {
    /// Discover project context for a runtime Cargo command.
    ///
    /// The caller passes the parsed Cargo manifest and workspace selectors from
    /// the CLI. Discovery resolves the workspace root, member list, and optional
    /// active member so configuration loading can pick the correct workspace and
    /// member `cooldown.toml` files.
    pub fn discover_for_runtime(manifest: &Manifest, workspace: &Workspace) -> Result<Self> {
        let selection = RuntimeSelection {
            manifest_path: manifest.manifest_path.clone(),
            packages: workspace.package.clone(),
            workspace: workspace.workspace,
            all: workspace.all,
            exclude: workspace.exclude.clone(),
        };
        if let Some(project) = Self::discover_simple(&selection)? {
            return Ok(project);
        }
        Self::discover(&selection)
    }

    /// Avoid a Cargo process for explicit, path-free workspace layouts. Complex
    /// membership, inherited target paths and external checkouts retain Cargo's
    /// authoritative metadata discovery.
    fn discover_simple(selection: &RuntimeSelection) -> Result<Option<Self>> {
        let cwd = fs::canonicalize(env::current_dir()?)?;
        let current_manifest = match &selection.manifest_path {
            Some(path) => path.clone(),
            None => match cwd
                .ancestors()
                .map(|dir| dir.join("Cargo.toml"))
                .find(|path| path.is_file())
            {
                Some(path) => path,
                None => return Ok(None),
            },
        };
        let current_manifest = fs::canonicalize(current_manifest)?;
        let current_dir = current_manifest
            .parent()
            .context("manifest has no parent")?;
        let current: toml::Value = toml::from_str(&fs::read_to_string(&current_manifest)?)?;
        if current
            .get("package")
            .and_then(|p| p.get("workspace"))
            .is_some()
        {
            return Ok(None);
        }
        let mut workspace_root = current_dir.to_path_buf();
        let mut root = current.clone();
        for ancestor in current_dir.ancestors() {
            let path = ancestor.join("Cargo.toml");
            if !path.is_file() {
                continue;
            }
            let manifest: toml::Value = toml::from_str(&fs::read_to_string(&path)?)?;
            if manifest.get("workspace").is_some() {
                workspace_root = ancestor.to_path_buf();
                root = manifest;
                break;
            }
        }
        let cargo_config = cargo_config(&cwd)?;
        if cargo_config.get("include").is_some()
            || cargo_config
                .get("build")
                .and_then(|b| b.get("target-dir"))
                .is_some()
            || env::var_os("CARGO_BUILD_TARGET_DIR").is_some()
        {
            return Ok(None);
        }
        let kind = if root.get("workspace").is_some() {
            ProjectKind::Workspace
        } else {
            ProjectKind::Crate
        };
        let mut manifests = Vec::new();
        if root.get("package").is_some() {
            manifests.push(workspace_root.join("Cargo.toml"));
        }
        if let Some(workspace) = root.get("workspace") {
            if workspace.get("exclude").is_some() {
                return Ok(None);
            }
            if let Some(members) = workspace.get("members") {
                let Some(members) = members.as_array() else {
                    return Ok(None);
                };
                for member in members {
                    let Some(member) = member.as_str() else {
                        return Ok(None);
                    };
                    if member.contains(['*', '?', '[', ']']) {
                        return Ok(None);
                    }
                    let path = fs::canonicalize(workspace_root.join(member).join("Cargo.toml"))?;
                    if !path.starts_with(&workspace_root) {
                        return Ok(None);
                    }
                    manifests.push(path);
                }
            }
        }
        manifests.sort();
        manifests.dedup();
        let mut members = Vec::new();
        for path in manifests {
            let manifest: toml::Value = toml::from_str(&fs::read_to_string(&path)?)?;
            if contains_path_key(&manifest) || contains_path_key(&root) {
                return Ok(None);
            }
            let Some(name) = manifest
                .get("package")
                .and_then(|p| p.get("name"))
                .and_then(|n| n.as_str())
            else {
                return Ok(None);
            };
            members.push(ProjectMember {
                name: name.to_string(),
                dir: path
                    .parent()
                    .context("manifest has no parent")?
                    .to_path_buf(),
                manifest_path: path,
            });
        }
        if current_manifest != workspace_root.join("Cargo.toml")
            && !members
                .iter()
                .any(|member| member.manifest_path == current_manifest)
        {
            return Ok(None);
        }
        let target_directory = match env::var_os("CARGO_TARGET_DIR").map(PathBuf::from) {
            Some(path) if path.is_absolute() => path,
            Some(path) => cwd.join(path),
            None => workspace_root.join("target"),
        };
        let target_directory = canonicalize_location(&target_directory)?;
        let active_member = determine_active_member(
            selection,
            &cwd,
            &current_manifest,
            &workspace_root,
            &members,
        );
        Ok(Some(Self {
            cwd,
            kind,
            workspace_root,
            target_directory,
            members,
            active_member,
        }))
    }

    /// Discover project context for `cargo cooldown init`.
    ///
    /// Init has no forwarded Cargo command, so discovery starts from the current
    /// directory and then verifies that the user is at the project root. The
    /// returned context tells the wizard whether it is configuring a crate or a
    /// workspace and where files should be created.
    pub fn discover_for_init() -> Result<Self> {
        let context = Self::discover(&RuntimeSelection::default())?;
        if !same_existing_path(&context.cwd, &context.workspace_root)? {
            bail!(
                "`cargo cooldown init` must run from the project root. Current directory: {}. Expected root: {}",
                context.cwd.display(),
                context.workspace_root.display()
            );
        }
        Ok(context)
    }

    fn discover(selection: &RuntimeSelection) -> Result<Self> {
        let cwd = fs::canonicalize(env::current_dir()?)
            .context("failed to determine current directory")?;
        let current_manifest = match &selection.manifest_path {
            Some(path) => path.clone(),
            None => cwd
                .ancestors()
                .map(|dir| dir.join("Cargo.toml"))
                .find(|path| path.is_file())
                .context("could not find Cargo.toml")?,
        };
        let current_manifest =
            fs::canonicalize(current_manifest).context("invalid manifest path")?;
        let metadata = read_project_metadata(selection.manifest_path.as_deref())?;
        let workspace_root = fs::canonicalize(&metadata.workspace_root)?;
        let workspace_manifest = workspace_root.join("Cargo.toml");
        let kind = if manifest_declares_workspace(&workspace_manifest)? {
            ProjectKind::Workspace
        } else {
            ProjectKind::Crate
        };
        let members = workspace_members(&metadata)?;
        let active_member = determine_active_member(
            selection,
            &cwd,
            &current_manifest,
            &workspace_root,
            &members,
        );

        Ok(Self {
            cwd,
            kind,
            workspace_root,
            target_directory: canonicalize_location(metadata.target_directory.as_std_path())?,
            members,
            active_member,
        })
    }

    /// Path to the workspace or crate root `cooldown.toml`.
    pub fn workspace_config_path(&self) -> PathBuf {
        self.workspace_root.join("cooldown.toml")
    }

    /// Path to the active member override, when the run targets exactly one member.
    pub fn member_config_path(&self) -> Option<PathBuf> {
        self.active_member.as_ref().and_then(|member| {
            let path = member.dir.join("cooldown.toml");
            (path != self.workspace_config_path()).then_some(path)
        })
    }
}

fn contains_path_key(value: &toml::Value) -> bool {
    match value {
        toml::Value::Table(table) => {
            table.contains_key("path") || table.values().any(contains_path_key)
        }
        toml::Value::Array(values) => values.iter().any(contains_path_key),
        _ => false,
    }
}

fn same_existing_path(left: &Path, right: &Path) -> Result<bool> {
    let left = fs::canonicalize(left)
        .with_context(|| format!("failed to canonicalize {}", left.display()))?;
    let right = fs::canonicalize(right)
        .with_context(|| format!("failed to canonicalize {}", right.display()))?;
    Ok(left == right)
}

fn read_project_metadata(manifest_path: Option<&Path>) -> Result<Metadata> {
    crate::backend::record_cargo_invocation();
    let mut command = cargo_metadata::MetadataCommand::new();
    if let Some(path) = manifest_path {
        command.manifest_path(path);
    }
    crate::backend::configure_metadata(&mut command);
    command.no_deps();
    command
        .exec()
        .context("failed to read Cargo project metadata")
}

fn manifest_declares_workspace(path: &Path) -> Result<bool> {
    let contents = fs::read_to_string(path)
        .with_context(|| format!("failed to read project manifest {}", path.display()))?;
    let manifest: toml::Value = toml::from_str(&contents)
        .with_context(|| format!("failed to parse project manifest {}", path.display()))?;
    Ok(manifest.get("workspace").is_some())
}

// Cargo may retain aliases such as /var versus /private/var or Windows short
// paths. Resolve the existing prefix even when the target directory is absent.
fn canonicalize_location(path: &Path) -> Result<PathBuf> {
    for ancestor in path.ancestors() {
        match fs::canonicalize(ancestor) {
            Ok(resolved) => return Ok(resolved.join(path.strip_prefix(ancestor)?)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => continue,
            Err(err) => {
                return Err(err).with_context(|| format!("invalid path {}", path.display()));
            }
        }
    }
    bail!("path has no existing ancestor: {}", path.display())
}

fn workspace_members(metadata: &Metadata) -> Result<Vec<ProjectMember>> {
    metadata
        .workspace_packages()
        .iter()
        .map(|package| {
            let manifest_path = fs::canonicalize(&package.manifest_path)?;
            let dir = manifest_path
                .parent()
                .map(Path::to_path_buf)
                .expect("workspace package manifest should have a parent directory");
            Ok(ProjectMember {
                name: package.name.to_string(),
                manifest_path,
                dir,
            })
        })
        .collect()
}

fn determine_active_member(
    selection: &RuntimeSelection,
    cwd: &Path,
    current_manifest: &Path,
    workspace_root: &Path,
    members: &[ProjectMember],
) -> Option<ProjectMember> {
    if selection.workspace
        || selection.all
        || selection.packages.len() > 1
        || !selection.exclude.is_empty()
    {
        return None;
    }

    let member_from_package = selection
        .packages
        .first()
        .and_then(|name| members.iter().find(|member| member.name == *name))
        .cloned();
    // The CLI manifest path can be relative; Cargo has already resolved it here.
    let member_from_manifest = selection
        .manifest_path
        .as_ref()
        .and_then(|_| {
            members
                .iter()
                .find(|member| member.manifest_path == current_manifest)
        })
        .cloned();

    match (member_from_package, member_from_manifest) {
        (Some(package_member), Some(manifest_member))
            if package_member.manifest_path == manifest_member.manifest_path =>
        {
            Some(package_member)
        }
        (Some(package_member), None) => Some(package_member),
        (None, Some(manifest_member)) => Some(manifest_member),
        (None, None) if cwd != workspace_root => members
            .iter()
            .find(|member| member.manifest_path == current_manifest)
            .cloned(),
        (Some(_), Some(_)) | (None, None) => None,
    }
}

/// Unit tests for workspace member detection and config path selection.
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn active_member_is_none_for_workspace_wide_runs() {
        let cwd = PathBuf::from("/tmp/workspace");
        let root = PathBuf::from("/tmp/workspace");
        let current_manifest = PathBuf::from("/tmp/workspace/Cargo.toml");
        let selection = RuntimeSelection {
            packages: vec!["member-a".to_string(), "member-b".to_string()],
            workspace: true,
            ..RuntimeSelection::default()
        };
        let members = vec![ProjectMember {
            name: "member-a".to_string(),
            manifest_path: PathBuf::from("/tmp/workspace/member-a/Cargo.toml"),
            dir: PathBuf::from("/tmp/workspace/member-a"),
        }];

        let active = determine_active_member(&selection, &cwd, &current_manifest, &root, &members);

        assert!(active.is_none());
    }

    #[test]
    fn active_member_prefers_single_package_selection() {
        let cwd = PathBuf::from("/tmp/workspace");
        let root = PathBuf::from("/tmp/workspace");
        let current_manifest = PathBuf::from("/tmp/workspace/Cargo.toml");
        let selection = RuntimeSelection {
            packages: vec!["member-a".to_string()],
            ..RuntimeSelection::default()
        };
        let members = vec![ProjectMember {
            name: "member-a".to_string(),
            manifest_path: PathBuf::from("/tmp/workspace/member-a/Cargo.toml"),
            dir: PathBuf::from("/tmp/workspace/member-a"),
        }];

        let active =
            determine_active_member(&selection, &cwd, &current_manifest, &root, &members).unwrap();

        assert_eq!(active.name, "member-a");
    }

    #[test]
    fn active_member_uses_member_directory_when_no_selector_is_present() {
        let cwd = PathBuf::from("/tmp/workspace/member-a/src");
        let root = PathBuf::from("/tmp/workspace");
        let current_manifest = PathBuf::from("/tmp/workspace/member-a/Cargo.toml");
        let selection = RuntimeSelection::default();
        let members = vec![ProjectMember {
            name: "member-a".to_string(),
            manifest_path: PathBuf::from("/tmp/workspace/member-a/Cargo.toml"),
            dir: PathBuf::from("/tmp/workspace/member-a"),
        }];

        let active =
            determine_active_member(&selection, &cwd, &current_manifest, &root, &members).unwrap();

        assert_eq!(active.name, "member-a");
    }

    #[test]
    fn active_member_uses_relative_manifest_path_from_workspace_root() {
        let cwd = PathBuf::from("/tmp/workspace");
        let root = PathBuf::from("/tmp/workspace");
        let current_manifest = PathBuf::from("/tmp/workspace/member-a/Cargo.toml");
        let selection = RuntimeSelection {
            manifest_path: Some(PathBuf::from("member-a/Cargo.toml")),
            ..RuntimeSelection::default()
        };
        let members = vec![ProjectMember {
            name: "member-a".to_string(),
            manifest_path: PathBuf::from("/tmp/workspace/member-a/Cargo.toml"),
            dir: PathBuf::from("/tmp/workspace/member-a"),
        }];

        let active =
            determine_active_member(&selection, &cwd, &current_manifest, &root, &members).unwrap();

        assert_eq!(active.name, "member-a");
    }

    #[test]
    fn member_config_path_skips_duplicate_root_paths() {
        let context = ProjectContext {
            cwd: PathBuf::from("/tmp/workspace"),
            kind: ProjectKind::Workspace,
            workspace_root: PathBuf::from("/tmp/workspace"),
            target_directory: PathBuf::from("/tmp/workspace/target"),
            members: Vec::new(),
            active_member: Some(ProjectMember {
                name: "root".to_string(),
                manifest_path: PathBuf::from("/tmp/workspace/Cargo.toml"),
                dir: PathBuf::from("/tmp/workspace"),
            }),
        };

        assert!(context.member_config_path().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn same_existing_path_matches_symlinked_root() {
        let temp_dir = tempfile::tempdir().unwrap();
        let real_root = temp_dir.path().join("workspace");
        let link_root = temp_dir.path().join("workspace-link");
        fs::create_dir(&real_root).unwrap();
        std::os::unix::fs::symlink(&real_root, &link_root).unwrap();

        assert!(same_existing_path(&link_root, &real_root).unwrap());
    }
}

pub(crate) fn cargo_config(cwd: &Path) -> Result<toml::Table> {
    let mut dirs: Vec<PathBuf> = cwd.ancestors().map(|p| p.join(".cargo")).collect();
    dirs.reverse();
    if let Some(home) = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| dirs::home_dir().map(|p| p.join(".cargo")))
    {
        dirs.insert(0, home);
    }
    let mut merged = toml::Table::new();
    for dir in dirs {
        let path = if dir.join("config").exists() {
            dir.join("config")
        } else {
            dir.join("config.toml")
        };
        if path.exists() {
            let table: toml::Table = toml::from_str(&fs::read_to_string(path)?)?;
            merge_tables(&mut merged, table);
        }
    }
    Ok(merged)
}

fn merge_tables(base: &mut toml::Table, overlay: toml::Table) {
    for (key, value) in overlay {
        if let (Some(existing), Some(table)) = (
            base.get_mut(&key).and_then(|v| v.as_table_mut()),
            value.as_table(),
        ) {
            merge_tables(existing, table.clone());
        } else {
            base.insert(key, value);
        }
    }
}

#[cfg(test)]
mod simple_discovery_tests {
    use super::*;

    #[test]
    fn explicit_workspace_discovery_matches_cargo_metadata() {
        let temp = tempfile::tempdir().unwrap();
        fs::write(
            temp.path().join("Cargo.toml"),
            "[workspace]\nmembers=['one','two']\nresolver='2'\n",
        )
        .unwrap();
        for name in ["one", "two"] {
            let dir = temp.path().join(name);
            fs::create_dir_all(dir.join("src")).unwrap();
            fs::write(dir.join("src/lib.rs"), "").unwrap();
            fs::write(
                dir.join("Cargo.toml"),
                format!("[package]\nname='{name}'\nversion='0.1.0'\nedition='2024'\n"),
            )
            .unwrap();
        }
        let manifests = vec![temp.path().join("one/Cargo.toml")];
        #[cfg(unix)]
        let links = tempfile::tempdir().unwrap();
        #[cfg(unix)]
        let manifests = {
            let alias = links.path().join("workspace-link");
            std::os::unix::fs::symlink(temp.path(), &alias).unwrap();
            let mut manifests = manifests;
            manifests.push(alias.join("one/Cargo.toml"));
            manifests
        };
        for manifest in manifests {
            let selection = RuntimeSelection {
                manifest_path: Some(manifest),
                ..RuntimeSelection::default()
            };
            let Some(simple) = ProjectContext::discover_simple(&selection).unwrap() else {
                // A user-level target-dir config legitimately requires Cargo discovery.
                return;
            };
            let cargo = ProjectContext::discover(&selection).unwrap();
            assert_eq!(simple.workspace_root, cargo.workspace_root);
            assert_eq!(simple.target_directory, cargo.target_directory);
            assert_eq!(simple.active_member, cargo.active_member);
            assert_eq!(simple.members, cargo.members);
            assert_eq!(cargo.active_member.as_ref().unwrap().name, "one");
        }
    }
}
