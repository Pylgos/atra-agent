use std::path::Path;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct TreeManifest {
    pub entries: Vec<TreeEntry>,
}

impl TreeManifest {
    pub fn digest(&self) -> String {
        let mut digest = Sha256::new();
        digest.update(b"atra-tree\0");
        for entry in &self.entries {
            match entry {
                TreeEntry::File { path, object } => {
                    digest.update([0]);
                    update_digest_field(&mut digest, path.as_bytes());
                    update_digest_field(&mut digest, object.as_bytes());
                }
                TreeEntry::Symlink { path, target } => {
                    digest.update([1]);
                    update_digest_field(&mut digest, path.as_bytes());
                    update_digest_field(&mut digest, target.as_bytes());
                }
            }
        }
        format!("{:x}", digest.finalize())
    }

    pub fn validate(&self) -> Result<()> {
        let mut previous = None;
        for entry in &self.entries {
            validate_tree_path(entry.path())?;
            if previous.is_some_and(|path: &str| path >= entry.path()) {
                bail!("tree entries are not in unique path order");
            }
            if let Some(parent) = Path::new(entry.path()).parent() {
                for ancestor in parent.ancestors().filter(|path| !path.as_os_str().is_empty()) {
                    let ancestor = ancestor.to_string_lossy();
                    if self.entries.binary_search_by_key(&ancestor.as_ref(), TreeEntry::path).is_ok() {
                        bail!("tree entry {} has a non-directory parent", entry.path());
                    }
                }
            }
            match entry {
                TreeEntry::File { object, .. } => validate_digest(object)?,
                TreeEntry::Symlink { target, .. } => {
                    validate_tree_path(target)?;
                    let exists = self.entries.iter().any(|entry| {
                        entry.path() == target
                            || entry.path().strip_prefix(target).is_some_and(|suffix| suffix.starts_with('/'))
                    });
                    if !exists {
                        bail!("tree symlink target {target} does not exist");
                    }
                }
            }
            previous = Some(entry.path());
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum TreeEntry {
    File { path: String, object: String },
    Symlink { path: String, target: String },
}

impl TreeEntry {
    pub fn path(&self) -> &str {
        match self {
            Self::File { path, .. } | Self::Symlink { path, .. } => path,
        }
    }
}

fn update_digest_field(digest: &mut Sha256, value: &[u8]) {
    digest.update((value.len() as u64).to_be_bytes());
    digest.update(value);
}

fn validate_tree_path(path: &str) -> Result<()> {
    if path.is_empty()
        || path.starts_with('/')
        || path.ends_with('/')
        || path
            .split('/')
            .any(|component| component.is_empty() || matches!(component, "." | ".."))
    {
        bail!("invalid tree path {path:?}");
    }
    Ok(())
}

fn validate_digest(digest: &str) -> Result<()> {
    if digest.len() != 64 || !digest.bytes().all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()) {
        bail!("invalid object digest {digest}");
    }
    Ok(())
}
