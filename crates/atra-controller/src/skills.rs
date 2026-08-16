use std::{
    collections::{HashMap, HashSet},
    env, fs,
    io::Cursor,
    os::unix::fs::PermissionsExt,
    path::{Component, Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use atra_store::{Store, TreeEntry, TreeManifest, object_digest};
use yaml_rust::YamlLoader;

const MAX_SKILL_FILE_BYTES: u64 = 256 * 1024;
const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_SKILL_BYTES: u64 = 256 * 1024 * 1024;
const MAX_SKILL_FILES: usize = 10_000;

const BUILTIN_SKILLS: &[EmbeddedSkill] = &[EmbeddedSkill {
    directory_name: "setup-atra-workspace",
    files: &[EmbeddedFile {
        path: "SKILL.md",
        content: include_bytes!("../builtin-skills/setup-atra-workspace/SKILL.md"),
        executable: false,
    }],
}];

pub(crate) struct SkillGeneration {
    pub(crate) manifest: TreeManifest,
    pub(crate) prompt: Option<String>,
    pub(crate) skills: Vec<SkillDefinition>,
}

#[derive(Clone)]
pub(crate) struct SkillDefinition {
    pub(crate) name: String,
    pub(crate) instructions: String,
}

struct Skill {
    name: String,
    description: String,
    instructions: String,
    disable_model_invocation: bool,
    files: SkillFiles,
    location: String,
}

enum SkillFiles {
    Directory(PathBuf),
    Embedded(&'static [EmbeddedFile]),
}

struct EmbeddedSkill {
    directory_name: &'static str,
    files: &'static [EmbeddedFile],
}

struct EmbeddedFile {
    path: &'static str,
    content: &'static [u8],
    executable: bool,
}

enum Source {
    Directory { root: PathBuf, label: &'static str },
    Builtin,
}

pub(crate) fn collect(
    workspace: &Path,
    data_home: &Path,
    store: &Store,
) -> Result<SkillGeneration> {
    let mut sources = vec![
        Source::Directory {
            root: workspace.join(".agents/skills"),
            label: "workspace",
        },
        Source::Builtin,
        Source::Directory {
            root: data_home.join("atra/skills"),
            label: "atra",
        },
    ];
    if let Some(home) = env::var_os("HOME") {
        sources.push(Source::Directory {
            root: PathBuf::from(home).join(".agents/skills"),
            label: "user",
        });
    }

    let mut skills: HashMap<String, Skill> = HashMap::new();
    for source in sources {
        let (discovered, label) = match source {
            Source::Directory { root, label } => (discover(&root)?, label),
            Source::Builtin => (discover_builtin()?, "builtin"),
        };
        for skill in discovered {
            if let Some(existing) = skills.get(&skill.name) {
                tracing::warn!(
                    skill = skill.name,
                    winner = existing.location,
                    loser = skill.location,
                    source = label,
                    "ignoring duplicate skill"
                );
            } else {
                skills.insert(skill.name.clone(), skill);
            }
        }
    }

    let mut skills = skills.into_values().collect::<Vec<_>>();
    skills.sort_unstable_by(|left, right| left.name.cmp(&right.name));
    let mut entries = Vec::new();
    skills.retain(|skill| {
        let mut skill_entries = Vec::new();
        let result = collect_skill(skill, store, &mut skill_entries).and_then(|()| {
            skill_entries.sort_unstable_by(|left, right| left.path().cmp(right.path()));
            let manifest = TreeManifest {
                entries: skill_entries,
            };
            manifest.validate().map_err(anyhow::Error::msg)?;
            Ok(manifest)
        });
        match result {
            Ok(manifest) => {
                entries.extend(manifest.entries);
                true
            }
            Err(error) => {
                tracing::warn!(
                    skill = skill.name,
                    path = skill.location,
                    error = %format!("{error:#}"),
                    "ignoring invalid skill"
                );
                false
            }
        }
    });
    entries.sort_unstable_by(|left, right| left.path().cmp(right.path()));
    let manifest = TreeManifest { entries };
    manifest.validate().map_err(anyhow::Error::msg)?;

    let model_invocable = skills
        .iter()
        .filter(|skill| !skill.disable_model_invocation)
        .collect::<Vec<_>>();
    let prompt = (!model_invocable.is_empty()).then(|| format_prompt(&model_invocable));
    let skills = skills
        .into_iter()
        .map(|skill| SkillDefinition {
            name: skill.name,
            instructions: skill.instructions,
        })
        .collect();
    Ok(SkillGeneration {
        manifest,
        prompt,
        skills,
    })
}

fn discover(root: &Path) -> Result<Vec<Skill>> {
    let metadata = match fs::metadata(root) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to inspect skill directory {}", root.display()));
        }
    };
    if !metadata.is_dir() {
        bail!("skill search path is not a directory: {}", root.display());
    }

    let mut skills = Vec::new();
    discover_directory(root, &mut skills, &mut HashSet::new())?;
    Ok(skills)
}

fn discover_directory(
    directory: &Path,
    skills: &mut Vec<Skill>,
    directories: &mut HashSet<PathBuf>,
) -> Result<()> {
    let canonical_directory = fs::canonicalize(directory)
        .with_context(|| format!("failed to resolve {}", directory.display()))?;
    if !directories.insert(canonical_directory.clone()) {
        bail!("skill search path contains a directory symlink cycle");
    }
    if directory.join("SKILL.md").is_file() {
        match read_skill(directory) {
            Ok(skill) => skills.push(skill),
            Err(error) => tracing::warn!(
                path = %directory.display(),
                error = %format!("{error:#}"),
                "ignoring invalid skill"
            ),
        }
        directories.remove(&canonical_directory);
        return Ok(());
    }

    let mut entries = fs::read_dir(directory)
        .with_context(|| format!("failed to list skill directory {}", directory.display()))?
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("failed to list skill directory {}", directory.display()))?;
    entries.sort_unstable_by_key(|entry| entry.file_name());
    for entry in entries {
        let name = entry.file_name();
        if name.as_encoded_bytes().starts_with(b".") || name == "node_modules" {
            continue;
        }
        let metadata = fs::metadata(entry.path())
            .with_context(|| format!("failed to inspect {}", entry.path().display()))?;
        if metadata.is_dir() {
            discover_directory(&entry.path(), skills, directories)?;
        }
    }
    directories.remove(&canonical_directory);
    Ok(())
}

fn read_skill(root: &Path) -> Result<Skill> {
    let path = root.join("SKILL.md");
    let metadata =
        fs::metadata(&path).with_context(|| format!("failed to inspect {}", path.display()))?;
    if metadata.len() > MAX_SKILL_FILE_BYTES {
        bail!("SKILL.md exceeds {MAX_SKILL_FILE_BYTES} bytes");
    }
    let content =
        fs::read_to_string(&path).with_context(|| format!("failed to read {}", path.display()))?;
    let directory_name = root
        .file_name()
        .and_then(|name| name.to_str())
        .context("skill directory name is not valid UTF-8")?;
    parse_skill(
        &content,
        directory_name,
        SkillFiles::Directory(root.to_owned()),
        root.display().to_string(),
    )
}

fn discover_builtin() -> Result<Vec<Skill>> {
    BUILTIN_SKILLS
        .iter()
        .map(|skill| {
            let skill_file = skill
                .files
                .iter()
                .find(|file| file.path == "SKILL.md")
                .context("builtin skill is missing SKILL.md")?;
            if skill_file.content.len() as u64 > MAX_SKILL_FILE_BYTES {
                bail!("builtin SKILL.md exceeds {MAX_SKILL_FILE_BYTES} bytes");
            }
            let content = std::str::from_utf8(skill_file.content)
                .context("builtin SKILL.md is not valid UTF-8")?;
            parse_skill(
                content,
                skill.directory_name,
                SkillFiles::Embedded(skill.files),
                format!("builtin:{}", skill.directory_name),
            )
        })
        .collect()
}

fn parse_skill(
    content: &str,
    directory_name: &str,
    files: SkillFiles,
    location: String,
) -> Result<Skill> {
    let (frontmatter, instructions) = parse_document(content)?;
    let name = frontmatter["name"]
        .as_str()
        .context("frontmatter name is required")?
        .to_owned();
    let description = frontmatter["description"]
        .as_str()
        .context("frontmatter description is required")?
        .trim()
        .to_owned();
    if description.is_empty() {
        bail!("frontmatter description must not be empty");
    }
    if description.len() > 1024 {
        bail!("frontmatter description exceeds 1024 bytes");
    }
    if name.len() > 64
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
        || name.starts_with('-')
        || name.ends_with('-')
        || name.contains("--")
    {
        bail!("frontmatter name must follow the Agent Skills name format");
    }
    if name != directory_name {
        bail!("frontmatter name must match parent directory {directory_name:?}");
    }
    let disable_model_invocation = match &frontmatter["disable-model-invocation"] {
        yaml_rust::Yaml::BadValue => false,
        yaml_rust::Yaml::Boolean(value) => *value,
        _ => bail!("frontmatter disable-model-invocation must be a boolean"),
    };
    Ok(Skill {
        name,
        description,
        instructions,
        disable_model_invocation,
        files,
        location,
    })
}

fn parse_document(content: &str) -> Result<(yaml_rust::Yaml, String)> {
    let normalized = content.replace("\r\n", "\n").replace('\r', "\n");
    let rest = normalized
        .strip_prefix("---\n")
        .context("SKILL.md must start with YAML frontmatter")?;
    let end = rest
        .lines()
        .scan(0, |offset, line| {
            let current = *offset;
            *offset += line.len() + 1;
            Some((current, line))
        })
        .find_map(|(offset, line)| (line == "---").then_some(offset))
        .context("SKILL.md frontmatter is not terminated")?;
    let documents = YamlLoader::load_from_str(&rest[..end]).context("invalid YAML frontmatter")?;
    let frontmatter = documents
        .into_iter()
        .next()
        .context("SKILL.md frontmatter is empty")?;
    let instructions = rest[end + 3..]
        .strip_prefix('\n')
        .unwrap_or(&rest[end + 3..])
        .to_owned();
    Ok((frontmatter, instructions))
}

fn collect_skill(skill: &Skill, store: &Store, entries: &mut Vec<TreeEntry>) -> Result<()> {
    match &skill.files {
        SkillFiles::Directory(root) => collect_filesystem_skill(skill, root, store, entries),
        SkillFiles::Embedded(files) => collect_embedded_skill(skill, files, store, entries),
    }
}

fn collect_filesystem_skill(
    skill: &Skill,
    root: &Path,
    store: &Store,
    entries: &mut Vec<TreeEntry>,
) -> Result<()> {
    let canonical_root =
        fs::canonicalize(root).with_context(|| format!("failed to resolve {}", root.display()))?;
    let mut files = 0;
    let mut bytes = 0;
    let mut directories = HashSet::new();
    collect_directory(
        skill,
        &canonical_root,
        root,
        Path::new(""),
        store,
        entries,
        &mut files,
        &mut bytes,
        &mut directories,
    )
}

fn collect_embedded_skill(
    skill: &Skill,
    embedded_files: &[EmbeddedFile],
    store: &Store,
    entries: &mut Vec<TreeEntry>,
) -> Result<()> {
    if embedded_files.len() > MAX_SKILL_FILES {
        bail!("skill contains more than {MAX_SKILL_FILES} files");
    }
    let mut bytes = 0_u64;
    for file in embedded_files {
        let length = file.content.len() as u64;
        if length > MAX_FILE_BYTES {
            bail!("{} exceeds {MAX_FILE_BYTES} bytes", file.path);
        }
        bytes = bytes.checked_add(length).context("skill size overflow")?;
        if bytes > MAX_SKILL_BYTES {
            bail!("skill exceeds {MAX_SKILL_BYTES} bytes");
        }
        let relative = Path::new(file.path);
        let digest = object_digest(file.content, file.executable);
        store
            .put_object(&digest, file.executable, Cursor::new(file.content))
            .with_context(|| format!("failed to store builtin:{}", file.path))?;
        entries.push(TreeEntry::File {
            path: logical_path(&skill.name, relative)?,
            object: digest,
        });
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn collect_directory(
    skill: &Skill,
    canonical_root: &Path,
    directory: &Path,
    relative: &Path,
    store: &Store,
    entries: &mut Vec<TreeEntry>,
    files: &mut usize,
    bytes: &mut u64,
    directories: &mut HashSet<PathBuf>,
) -> Result<()> {
    let canonical_directory = fs::canonicalize(directory)
        .with_context(|| format!("failed to resolve {}", directory.display()))?;
    if !directories.insert(canonical_directory.clone()) {
        bail!("skill contains a directory symlink cycle");
    }
    let mut children = fs::read_dir(directory)
        .with_context(|| format!("failed to list {}", directory.display()))?
        .collect::<std::io::Result<Vec<_>>>()
        .with_context(|| format!("failed to list {}", directory.display()))?;
    children.sort_unstable_by_key(|entry| entry.file_name());
    for child in children {
        let child_relative = relative.join(child.file_name());
        let path = child.path();
        let metadata = fs::symlink_metadata(&path)
            .with_context(|| format!("failed to inspect {}", path.display()))?;
        if metadata.file_type().is_symlink() {
            let canonical = fs::canonicalize(&path)
                .with_context(|| format!("failed to resolve symlink {}", path.display()))?;
            let target_metadata = fs::metadata(&path)
                .with_context(|| format!("failed to inspect symlink target {}", path.display()))?;
            if !target_metadata.is_dir() && !target_metadata.is_file() {
                bail!(
                    "skill contains unsupported symlink target: {}",
                    path.display()
                );
            }
            if let Ok(target) = canonical.strip_prefix(canonical_root) {
                entries.push(TreeEntry::Symlink {
                    path: logical_path(&skill.name, &child_relative)?,
                    target: logical_path(&skill.name, target)?,
                });
            } else if target_metadata.is_dir() {
                collect_directory(
                    skill,
                    canonical_root,
                    &canonical,
                    &child_relative,
                    store,
                    entries,
                    files,
                    bytes,
                    directories,
                )?;
            } else {
                collect_file(
                    skill,
                    &path,
                    &child_relative,
                    &target_metadata,
                    store,
                    entries,
                    files,
                    bytes,
                )?;
            }
        } else if metadata.is_dir() {
            collect_directory(
                skill,
                canonical_root,
                &path,
                &child_relative,
                store,
                entries,
                files,
                bytes,
                directories,
            )?;
        } else if metadata.is_file() {
            collect_file(
                skill,
                &path,
                &child_relative,
                &metadata,
                store,
                entries,
                files,
                bytes,
            )?;
        } else {
            bail!("skill contains unsupported file type: {}", path.display());
        }
    }
    directories.remove(&canonical_directory);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn collect_file(
    skill: &Skill,
    path: &Path,
    relative: &Path,
    metadata: &fs::Metadata,
    store: &Store,
    entries: &mut Vec<TreeEntry>,
    files: &mut usize,
    bytes: &mut u64,
) -> Result<()> {
    *files += 1;
    if *files > MAX_SKILL_FILES {
        bail!("skill contains more than {MAX_SKILL_FILES} files");
    }
    if metadata.len() > MAX_FILE_BYTES {
        bail!("{} exceeds {MAX_FILE_BYTES} bytes", path.display());
    }
    *bytes = bytes
        .checked_add(metadata.len())
        .context("skill size overflow")?;
    if *bytes > MAX_SKILL_BYTES {
        bail!("skill exceeds {MAX_SKILL_BYTES} bytes");
    }
    let content = fs::read(path).with_context(|| format!("failed to read {}", path.display()))?;
    let executable = metadata.permissions().mode() & 0o111 != 0;
    let digest = object_digest(&content, executable);
    store
        .put_object(&digest, executable, Cursor::new(content))
        .with_context(|| format!("failed to store {}", path.display()))?;
    entries.push(TreeEntry::File {
        path: logical_path(&skill.name, relative)?,
        object: digest,
    });
    Ok(())
}

fn logical_path(name: &str, relative: &Path) -> Result<String> {
    let mut path = PathBuf::from("skills").join(name);
    for component in relative.components() {
        match component {
            Component::Normal(component) => path.push(component),
            _ => bail!("skill contains an invalid path"),
        }
    }
    path.into_os_string()
        .into_string()
        .map_err(|_| anyhow::anyhow!("skill path is not valid UTF-8"))
        .map(|path| path.replace('\\', "/"))
}

fn format_prompt(skills: &[&Skill]) -> String {
    let mut lines = vec![
        "The following skills provide specialized instructions for specific tasks.".to_owned(),
        "When a skill applies, read $ATRA_SKILLS/<name>/SKILL.md with command. \
         $ATRA_SKILLS is available in every Runner; resolve relative references against the \
         directory containing SKILL.md."
            .to_owned(),
        String::new(),
        "Available skills:".to_owned(),
    ];
    for skill in skills {
        lines.push(format!(
            "{}: {}",
            skill.name,
            skill
                .description
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        ));
    }
    lines.join("\n")
}

pub(crate) fn resolve_invocations(
    message: &str,
    skills: &[SkillDefinition],
) -> (String, Vec<SkillDefinition>) {
    let skills = skills
        .iter()
        .map(|skill| (skill.name.as_str(), skill))
        .collect::<HashMap<_, _>>();
    let mut normalized = String::with_capacity(message.len());
    let mut selected = Vec::new();
    let mut selected_names = HashSet::new();
    let mut offset = 0;
    while offset < message.len() {
        let rest = &message[offset..];
        let (escaped, marker_offset) = if rest.starts_with("\\$") {
            (true, 1)
        } else if rest.starts_with('$') {
            (false, 0)
        } else {
            let character = rest.chars().next().expect("offset is in bounds");
            normalized.push(character);
            offset += character.len_utf8();
            continue;
        };
        let name_start = offset + marker_offset + 1;
        let name_length = message[name_start..]
            .bytes()
            .take_while(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || *byte == b'-')
            .count();
        let name_end = name_start + name_length;
        let name = &message[name_start..name_end];
        let Some(skill) = skills.get(name).copied() else {
            let character = rest.chars().next().expect("offset is in bounds");
            normalized.push(character);
            offset += character.len_utf8();
            continue;
        };
        if !escaped && selected_names.insert(name) {
            selected.push(skill.clone());
        }
        normalized.push('$');
        normalized.push_str(name);
        offset = name_end;
    }
    (normalized, selected)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn definition(name: &str) -> SkillDefinition {
        SkillDefinition {
            name: name.to_owned(),
            instructions: format!("instructions for {name}"),
        }
    }

    #[test]
    fn parses_instructions_and_disable_model_invocation() {
        let skill = parse_skill(
            "---\nname: example\ndescription: Example skill\ndisable-model-invocation: true\n---\nDo the work.\n",
            "example",
            SkillFiles::Embedded(&[]),
            "test".to_owned(),
        )
        .unwrap();

        assert!(skill.disable_model_invocation);
        assert_eq!(skill.instructions, "Do the work.\n");
    }

    #[test]
    fn rejects_non_boolean_disable_model_invocation() {
        let error = parse_skill(
            "---\nname: example\ndescription: Example skill\ndisable-model-invocation: yes\n---\nBody\n",
            "example",
            SkillFiles::Embedded(&[]),
            "test".to_owned(),
        )
        .err()
        .unwrap();

        assert!(
            error
                .to_string()
                .contains("disable-model-invocation must be a boolean")
        );
    }

    #[test]
    fn prompt_omits_model_disabled_skills() {
        let visible = parse_skill(
            "---\nname: visible\ndescription: Visible\n---\nBody\n",
            "visible",
            SkillFiles::Embedded(&[]),
            "test".to_owned(),
        )
        .unwrap();
        let hidden = parse_skill(
            "---\nname: hidden\ndescription: Hidden\ndisable-model-invocation: true\n---\nBody\n",
            "hidden",
            SkillFiles::Embedded(&[]),
            "test".to_owned(),
        )
        .unwrap();

        let prompt = format_prompt(&[&visible]);
        assert!(prompt.contains("visible: Visible"));
        assert!(!prompt.contains("hidden"));
        assert!(hidden.disable_model_invocation);
    }

    #[test]
    fn resolves_known_mentions_once_and_unescapes_explicit_literals() {
        let skills = vec![definition("review-code"), definition("test")];
        let (message, selected) = resolve_invocations(
            "$review-code run $test then $review-code; show \\$test and $unknown",
            &skills,
        );

        assert_eq!(
            message,
            "$review-code run $test then $review-code; show $test and $unknown"
        );
        assert_eq!(
            selected
                .into_iter()
                .map(|skill| skill.name)
                .collect::<Vec<_>>(),
            ["review-code", "test"]
        );
    }

    #[test]
    fn does_not_match_a_known_name_as_a_prefix() {
        let skills = vec![definition("test")];
        let (_, selected) = resolve_invocations("$test-extra $testing", &skills);

        assert!(selected.is_empty());
    }
}
