//! Interactive setup wizard for creating project `cooldown.toml` files.

use std::fmt::{Display, Write as _};
use std::fs;
use std::io::{self, Write};
use std::path::PathBuf;

use anyhow::{Context, Result, bail};

use crate::project::{ProjectContext, ProjectKind, ProjectMember};

#[derive(Debug, Clone, PartialEq, Eq)]
struct ConfigTemplate {
    cooldown_minutes: Option<u64>,
    enforcement: Option<String>,
    lockfile_baseline: Option<String>,
    skip_registries: Vec<String>,
    include_allow_examples: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InitFile {
    path: PathBuf,
    contents: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct InitPlan {
    files: Vec<InitFile>,
}

/// Run the setup wizard for the already discovered project.
///
/// The caller provides project shape and paths from discovery. The wizard shows
/// what it found, asks a small set of configuration questions, builds the target
/// `cooldown.toml` files, refuses to overwrite existing files, and writes the
/// plan only after user confirmation.
pub fn run(project: &ProjectContext) -> Result<()> {
    print_project_summary(project);

    let plan = match project.kind {
        ProjectKind::Crate => build_crate_plan(project)?,
        ProjectKind::Workspace => build_workspace_plan(project)?,
    };

    if plan.files.is_empty() {
        bail!("nothing to create");
    }

    let conflicts: Vec<PathBuf> = plan
        .files
        .iter()
        .map(|file| file.path.clone())
        .filter(|path| path.exists())
        .collect();
    if !conflicts.is_empty() {
        eprintln!("Aborting because the following files already exist:");
        for path in conflicts {
            eprintln!("  - {}", path.display());
        }
        bail!("refusing to overwrite existing cooldown.toml files");
    }

    print_plan_preview(&plan);
    if !prompt_confirm("Write these files?", true)? {
        bail!("aborted by user");
    }

    for file in &plan.files {
        fs::write(&file.path, &file.contents)
            .with_context(|| format!("failed to write {}", file.path.display()))?;
    }

    eprintln!("Created {} file(s).", plan.files.len());
    Ok(())
}

fn build_crate_plan(project: &ProjectContext) -> Result<InitPlan> {
    let template = prompt_base_template()?;
    Ok(InitPlan {
        files: vec![InitFile {
            path: project.workspace_config_path(),
            contents: render_config_file(&template, false),
        }],
    })
}

fn build_workspace_plan(project: &ProjectContext) -> Result<InitPlan> {
    let override_candidates = project
        .members
        .iter()
        .filter(|member| member.dir != project.workspace_root)
        .cloned()
        .collect::<Vec<_>>();

    let use_member_overrides = if override_candidates.is_empty() {
        eprintln!(
            "No member directories are available for per-member overrides. Using a workspace-wide configuration."
        );
        false
    } else {
        prompt_select(
            "How should this workspace be configured?",
            &[
                "Workspace-wide defaults only",
                "Workspace defaults plus per-member overrides",
            ],
            0,
        )? == 1
    };

    let base_template = prompt_base_template()?;
    let mut files = vec![InitFile {
        path: project.workspace_config_path(),
        contents: render_config_file(&base_template, false),
    }];

    if !use_member_overrides {
        return Ok(InitPlan { files });
    }

    let selected_members = prompt_multi_select(
        "Select members to initialize with overrides",
        &override_candidates,
    )?;
    for member in selected_members {
        let template = prompt_member_template(&member)?;
        if let Some(template) = template {
            files.push(InitFile {
                path: member.dir.join("cooldown.toml"),
                contents: render_config_file(&template, true),
            });
        }
    }

    Ok(InitPlan { files })
}

fn prompt_base_template() -> Result<ConfigTemplate> {
    let cooldown_minutes = prompt_u64("Cooldown minutes", 1440)?;
    let enforcement = select_enforcement("Enforcement", "cargo_compatible")?;
    let lockfile_baseline = select_lockfile_baseline("Cargo.lock baseline", "floor")?;
    let skip_registries =
        prompt_registry_list("Registries to skip (comma-separated, leave blank for none)")?;
    let include_allow_examples = prompt_confirm("Include commented allow rule examples?", true)?;

    Ok(ConfigTemplate {
        cooldown_minutes: Some(cooldown_minutes),
        enforcement: Some(enforcement),
        lockfile_baseline: Some(lockfile_baseline),
        skip_registries,
        include_allow_examples,
    })
}

fn prompt_member_template(member: &ProjectMember) -> Result<Option<ConfigTemplate>> {
    eprintln!();
    eprintln!("Configuring member override for `{}`", member.name);
    let customize_config = prompt_confirm("Customize this member now?", true)?;
    let include_allow_examples = prompt_confirm(
        "Include commented allow rule examples for this member?",
        true,
    )?;

    let cooldown_minutes = if customize_config {
        prompt_optional_u64("Override cooldown minutes (leave blank to inherit)")?
    } else {
        None
    };
    let enforcement = if customize_config {
        select_optional_enforcement("Override enforcement (leave blank to inherit)")?
    } else {
        None
    };
    let lockfile_baseline = if customize_config {
        select_optional_lockfile_baseline("Override Cargo.lock baseline (leave blank to inherit)")?
    } else {
        None
    };
    let skip_registries = if customize_config {
        prompt_registry_list("Member registries to skip (comma-separated, leave blank to inherit)")?
    } else {
        Vec::new()
    };

    if cooldown_minutes.is_none()
        && enforcement.is_none()
        && lockfile_baseline.is_none()
        && skip_registries.is_empty()
        && !include_allow_examples
    {
        return Ok(None);
    }

    Ok(Some(ConfigTemplate {
        cooldown_minutes,
        enforcement,
        lockfile_baseline,
        skip_registries,
        include_allow_examples,
    }))
}

fn render_config_file(template: &ConfigTemplate, is_override: bool) -> String {
    let mut output = String::new();
    output.push_str("# Generated by `cargo cooldown init`.\n");
    if is_override {
        output.push_str(
            "# This file overrides the workspace defaults when this member is the unique target.\n",
        );
    } else {
        output.push_str("# Edit values as needed for your project.\n");
    }
    output.push('\n');

    if let Some(cooldown_minutes) = template.cooldown_minutes {
        writeln!(&mut output, "cooldown_minutes = {cooldown_minutes}")
            .expect("writing to String should not fail");
    }
    if let Some(enforcement) = &template.enforcement {
        writeln!(&mut output, "enforcement = \"{enforcement}\"")
            .expect("writing to String should not fail");
        if enforcement == "cargo_compatible" {
            output.push_str(
                "# `prompt` asks before accepting fresh versions Cargo still requires; `auto` keeps the current best Cargo-valid lockfile without asking.\n",
            );
            output.push_str("cargo_compatible_accept = \"prompt\"\n");
        }
    }
    if let Some(lockfile_baseline) = &template.lockfile_baseline {
        output.push_str(
            "# `floor` keeps initial Cargo.lock versions as the minimum; `ignore` allows lower versions.\n",
        );
        writeln!(&mut output, "lockfile_baseline = \"{lockfile_baseline}\"")
            .expect("writing to String should not fail");
    }
    if !template.skip_registries.is_empty() {
        let quoted = template
            .skip_registries
            .iter()
            .map(|value| format!("\"{value}\""))
            .collect::<Vec<_>>()
            .join(", ");
        writeln!(&mut output, "skip_registries = [{quoted}]")
            .expect("writing to String should not fail");
    }

    if template.include_allow_examples {
        if !output.ends_with("\n\n") {
            output.push('\n');
        }
        output.push_str("# Optional allow rules.\n");
        output.push_str("# Add as many `allow.exact` and `allow.package` entries as you need.\n");
        output.push_str("# `allow.exact` fully allows one exact crate version.\n");
        output.push_str("# `allow.package` lowers the cooldown for one crate name.\n");
        output.push_str(
            "# Use `minutes = 0` in `allow.package` to exclude that crate from cooldown.\n",
        );
        output.push('\n');
        output.push_str("# [allow.global]\n");
        output.push_str("# minutes = 1440\n\n");
        output.push_str("# [[allow.package]]\n");
        output.push_str("# crate = \"tokio\"\n");
        output.push_str("# minutes = 60\n\n");
        output.push_str("# [[allow.package]]\n");
        output.push_str("# crate = \"openssl\"\n");
        output.push_str("# minutes = 0\n\n");
        output.push_str("# [[allow.exact]]\n");
        output.push_str("# crate = \"serde\"\n");
        output.push_str("# version = \"1.0.218\"\n");
        output.push_str("# [[allow.exact]]\n");
        output.push_str("# crate = \"serde_json\"\n");
        output.push_str("# version = \"1.0.145\"\n");
    }

    output
}

fn print_project_summary(project: &ProjectContext) {
    eprintln!(
        "Detected project root: {}",
        project.workspace_root.display()
    );
    match project.kind {
        ProjectKind::Crate => eprintln!("Project type: crate"),
        ProjectKind::Workspace => {
            eprintln!("Project type: workspace");
            eprintln!("Workspace members: {}", project.members.len());
        }
    }
    eprintln!();
}

fn print_plan_preview(plan: &InitPlan) {
    eprintln!("Files to create:");
    for file in &plan.files {
        eprintln!();
        eprintln!("--- {} ---", file.path.display());
        eprintln!("{}", file.contents.trim_end());
    }
    eprintln!();
}

fn prompt_input(prompt: &str, default: Option<&str>) -> Result<String> {
    loop {
        if let Some(default) = default {
            print!("{prompt} [{default}]: ");
        } else {
            print!("{prompt}: ");
        }
        io::stdout().flush().context("failed to flush stdout")?;

        let mut input = String::new();
        io::stdin()
            .read_line(&mut input)
            .context("failed to read stdin")?;
        let trimmed = input.trim();

        if trimmed.is_empty() {
            if let Some(default) = default {
                return Ok(default.to_string());
            }
        } else {
            return Ok(trimmed.to_string());
        }
    }
}

fn prompt_confirm(prompt: &str, default: bool) -> Result<bool> {
    let suffix = if default { "Y/n" } else { "y/N" };
    loop {
        let input = prompt_input(&format!("{prompt} ({suffix})"), Some(""))?;
        if input.is_empty() {
            return Ok(default);
        }
        match input.to_ascii_lowercase().as_str() {
            "y" | "yes" => return Ok(true),
            "n" | "no" => return Ok(false),
            _ => eprintln!("Please answer yes or no."),
        }
    }
}

fn prompt_u64(prompt: &str, default: u64) -> Result<u64> {
    loop {
        let input = prompt_input(prompt, Some(&default.to_string()))?;
        match input.parse::<u64>() {
            Ok(value) => return Ok(value),
            Err(_) => eprintln!("Please enter a valid integer."),
        }
    }
}

fn prompt_optional_u64(prompt: &str) -> Result<Option<u64>> {
    loop {
        let input = prompt_input(prompt, Some(""))?;
        if input.is_empty() {
            return Ok(None);
        }
        match input.parse::<u64>() {
            Ok(value) => return Ok(Some(value)),
            Err(_) => eprintln!("Please enter a valid integer or leave the field blank."),
        }
    }
}

fn prompt_registry_list(prompt: &str) -> Result<Vec<String>> {
    let input = prompt_input(prompt, Some(""))?;
    Ok(input
        .split(',')
        .map(str::trim)
        .filter(|entry| !entry.is_empty())
        .map(ToOwned::to_owned)
        .collect())
}

fn prompt_select<T: Display>(prompt: &str, options: &[T], default_index: usize) -> Result<usize> {
    eprintln!("{prompt}:");
    for (index, option) in options.iter().enumerate() {
        eprintln!("  {}. {}", index + 1, option);
    }

    loop {
        let input = prompt_input("Choose an option", Some(&(default_index + 1).to_string()))?;
        match input.parse::<usize>() {
            Ok(value) if (1..=options.len()).contains(&value) => return Ok(value - 1),
            _ => eprintln!("Please enter a valid option number."),
        }
    }
}

fn prompt_multi_select(prompt: &str, options: &[ProjectMember]) -> Result<Vec<ProjectMember>> {
    if options.is_empty() {
        return Ok(Vec::new());
    }

    eprintln!("{prompt}:");
    for (index, member) in options.iter().enumerate() {
        eprintln!(
            "  {}. {} ({})",
            index + 1,
            member.name,
            member.dir.display()
        );
    }
    let input = prompt_input(
        "Enter comma-separated member numbers (leave blank for none)",
        Some(""),
    )?;
    if input.is_empty() {
        return Ok(Vec::new());
    }

    let mut selected = Vec::new();
    for part in input
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
    {
        let index = parse_member_selection_index(part, options.len())?;
        let member = options
            .get(index)
            .cloned()
            .with_context(|| format!("member selection `{part}` is out of range"))?;
        if selected
            .iter()
            .any(|existing: &ProjectMember| existing.manifest_path == member.manifest_path)
        {
            continue;
        }
        selected.push(member);
    }

    Ok(selected)
}

fn parse_member_selection_index(part: &str, options_len: usize) -> Result<usize> {
    let index = part
        .parse::<usize>()
        .with_context(|| format!("invalid member selection `{part}`"))?;
    let index = index
        .checked_sub(1)
        .with_context(|| format!("member selection `{part}` is out of range"))?;
    if index >= options_len {
        bail!("member selection `{part}` is out of range");
    }
    Ok(index)
}

fn select_enforcement(prompt: &str, default: &str) -> Result<String> {
    let options = ["strict", "cargo_compatible", "off"];
    let default_index = options
        .iter()
        .position(|value| *value == default)
        .unwrap_or(0);
    Ok(options[prompt_select(prompt, &options, default_index)?].to_string())
}

fn select_optional_enforcement(prompt: &str) -> Result<Option<String>> {
    loop {
        let input = prompt_input(prompt, Some(""))?;
        if input.is_empty() {
            return Ok(None);
        }
        match input.as_str() {
            "strict" | "cargo_compatible" | "off" => return Ok(Some(input)),
            _ => eprintln!(
                "Please enter one of: strict, cargo_compatible, off, or leave the field blank."
            ),
        }
    }
}

fn select_lockfile_baseline(prompt: &str, default: &str) -> Result<String> {
    let values = ["floor", "ignore"];
    let labels = [
        "floor - use initial Cargo.lock versions as the minimum floor",
        "ignore - allow cooldown below initial Cargo.lock versions",
    ];
    let default_index = values
        .iter()
        .position(|value| *value == default)
        .unwrap_or(0);
    Ok(values[prompt_select(prompt, &labels, default_index)?].to_string())
}

fn select_optional_lockfile_baseline(prompt: &str) -> Result<Option<String>> {
    loop {
        let input = prompt_input(prompt, Some(""))?;
        if input.is_empty() {
            return Ok(None);
        }
        match input.as_str() {
            "floor" | "ignore" => return Ok(Some(input)),
            _ => eprintln!("Please enter `floor`, `ignore`, or leave the field blank."),
        }
    }
}

/// Unit tests for generated config templates and workspace init planning.
#[cfg(test)]
mod tests {
    use super::*;

    use std::path::Path;

    use crate::project::ProjectKind;

    fn member(name: &str, path: &str) -> ProjectMember {
        ProjectMember {
            name: name.to_string(),
            manifest_path: PathBuf::from(path).join("Cargo.toml"),
            dir: PathBuf::from(path),
        }
    }

    fn project(path: &str, members: Vec<ProjectMember>) -> ProjectContext {
        ProjectContext {
            cwd: PathBuf::from(path),
            kind: ProjectKind::Workspace,
            workspace_root: PathBuf::from(path),
            target_directory: PathBuf::from(path).join("target"),
            members,
            active_member: None,
        }
    }

    #[test]
    fn render_config_file_includes_allow_examples_when_requested() {
        let rendered = render_config_file(
            &ConfigTemplate {
                cooldown_minutes: Some(1440),
                enforcement: Some("cargo_compatible".to_string()),
                lockfile_baseline: Some("floor".to_string()),
                skip_registries: vec!["crates-io".to_string()],
                include_allow_examples: true,
            },
            false,
        );

        assert!(rendered.contains("cooldown_minutes = 1440"));
        assert!(rendered.contains("skip_registries = [\"crates-io\"]"));
        assert!(rendered.contains("[allow.global]"));
    }

    #[test]
    fn render_override_file_omits_unset_scalars() {
        let rendered = render_config_file(
            &ConfigTemplate {
                cooldown_minutes: None,
                enforcement: None,
                lockfile_baseline: None,
                skip_registries: Vec::new(),
                include_allow_examples: false,
            },
            true,
        );

        assert!(!rendered.contains("cooldown_minutes"));
        assert!(rendered.contains("overrides the workspace defaults"));
    }

    #[test]
    fn workspace_plan_uses_member_files_under_member_directories() {
        let context = project(
            "/tmp/workspace",
            vec![member("member-a", "/tmp/workspace/member-a")],
        );
        let path = context.members[0].dir.join("cooldown.toml");
        assert_eq!(path, Path::new("/tmp/workspace/member-a/cooldown.toml"));
    }

    #[test]
    fn member_selection_rejects_zero() {
        let err = parse_member_selection_index("0", 3).unwrap_err();

        assert!(format!("{err:#}").contains("out of range"));
    }
}
