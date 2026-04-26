//! Project discovery for crate roots, workspaces, and member-specific configs.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

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
        Self::discover(&selection)
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
        let cwd = env::current_dir().context("failed to determine current directory")?;
        let current_manifest = locate_project(selection.manifest_path.as_deref(), false)?;
        let workspace_manifest = locate_project(selection.manifest_path.as_deref(), true)?;
        let workspace_root = workspace_manifest
            .parent()
            .map(Path::to_path_buf)
            .with_context(|| {
                format!(
                    "workspace manifest does not have a parent directory: {}",
                    workspace_manifest.display()
                )
            })?;
        let metadata = read_project_metadata(selection.manifest_path.as_deref())?;
        let kind = if manifest_declares_workspace(&workspace_manifest)? {
            ProjectKind::Workspace
        } else {
            ProjectKind::Crate
        };
        let members = workspace_members(&metadata);
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
            target_directory: metadata.target_directory.clone().into_std_path_buf(),
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

fn same_existing_path(left: &Path, right: &Path) -> Result<bool> {
    let left = fs::canonicalize(left)
        .with_context(|| format!("failed to canonicalize {}", left.display()))?;
    let right = fs::canonicalize(right)
        .with_context(|| format!("failed to canonicalize {}", right.display()))?;
    Ok(left == right)
}

fn locate_project(manifest_path: Option<&Path>, workspace: bool) -> Result<PathBuf> {
    let mut command = Command::new("cargo");
    command.arg("locate-project");
    if workspace {
        command.arg("--workspace");
    }
    command.args(["--message-format", "plain"]);
    if let Some(path) = manifest_path {
        command.arg("--manifest-path").arg(path);
    }

    let output = command
        .output()
        .context("failed to run `cargo locate-project`")?;
    if !output.status.success() {
        bail!(
            "`cargo locate-project{}` failed: {}",
            if workspace { " --workspace" } else { "" },
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let manifest = String::from_utf8(output.stdout)
        .context("`cargo locate-project` returned non-utf8 output")?;
    Ok(PathBuf::from(manifest.trim()))
}

fn read_project_metadata(manifest_path: Option<&Path>) -> Result<Metadata> {
    let mut command = cargo_metadata::MetadataCommand::new();
    if let Some(path) = manifest_path {
        command.manifest_path(path);
    }
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

fn workspace_members(metadata: &Metadata) -> Vec<ProjectMember> {
    metadata
        .workspace_packages()
        .iter()
        .map(|package| {
            let manifest_path = package.manifest_path.clone().into_std_path_buf();
            let dir = manifest_path
                .parent()
                .map(Path::to_path_buf)
                .expect("workspace package manifest should have a parent directory");
            ProjectMember {
                name: package.name.to_string(),
                manifest_path,
                dir,
            }
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
