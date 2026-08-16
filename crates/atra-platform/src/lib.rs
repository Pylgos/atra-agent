use std::{
    collections::HashMap,
    ffi::OsStr,
    fs::{self, File},
    io::Read,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use atra_store::{PreparedTree, Store, TreeEntry, TreeManifest};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use zip::ZipArchive;

#[derive(Debug, Deserialize)]
struct BundleManifest {
    platform: String,
    runner: String,
    tools: TreeManifest,
    objects: Vec<ManifestObject>,
}

#[derive(Debug, Deserialize)]
struct ManifestObject {
    digest: String,
    executable: bool,
    blob: String,
}

#[derive(Deserialize, Serialize)]
struct PlatformProfile {
    runner: String,
    tools: String,
}

pub struct PlatformBundle {
    platform: String,
    runner: String,
    tools: TreeManifest,
    objects: HashMap<String, Object>,
}

struct Object {
    executable: bool,
    compressed: Vec<u8>,
}

pub struct PlatformStore {
    store: Store,
    profile: PlatformProfile,
}

impl PlatformBundle {
    pub fn load(path: &Path) -> Result<Self> {
        let file = File::open(path)
            .with_context(|| format!("failed to open platform bundle {}", path.display()))?;
        let mut archive = ZipArchive::new(file)
            .with_context(|| format!("failed to read platform bundle {}", path.display()))?;
        let manifest: BundleManifest = {
            let mut entry = archive
                .by_name("manifest.json")
                .context("platform bundle does not contain manifest.json")?;
            serde_json::from_reader(&mut entry)
                .context("failed to decode platform bundle manifest")?
        };
        if manifest.platform.is_empty()
            || !manifest.platform.bytes().all(|byte| {
                byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'-' | b'_')
            })
        {
            bail!("invalid platform name {:?}", manifest.platform);
        }
        manifest.tools.validate().map_err(anyhow::Error::msg)?;

        let mut objects = HashMap::new();
        for object in manifest.objects {
            let digest = object.digest.clone();
            if objects
                .insert(digest.clone(), read_object(&mut archive, object)?)
                .is_some()
            {
                bail!("duplicate object {digest} in bundle");
            }
        }
        match objects.get(&manifest.runner) {
            Some(object) if object.executable => {}
            Some(_) => bail!("platform bundle runner object is not executable"),
            None => bail!("platform bundle does not contain runner object"),
        }
        for entry in &manifest.tools.entries {
            if let TreeEntry::File { object, .. } = entry
                && !objects.contains_key(object)
            {
                bail!("platform bundle does not contain tool object {object}");
            }
        }

        Ok(Self {
            platform: manifest.platform,
            runner: manifest.runner,
            tools: manifest.tools,
            objects,
        })
    }

    pub fn install(&self, root: &Path) -> Result<PathBuf> {
        let store = Store::open(root.to_owned())?;
        for (digest, object) in &self.objects {
            let decoder = zstd::Decoder::new(object.compressed.as_slice())
                .context("failed to decompress object")?;
            store.put_object(digest, object.executable, decoder)?;
        }
        let tree_digest = self.tools.digest();
        match store.prepare_tree(&self.tools)? {
            PreparedTree::Ready { digest, .. } if digest == tree_digest => {}
            PreparedTree::Ready { digest, .. } => {
                bail!("stored tree digest {digest}, expected {tree_digest}")
            }
            PreparedTree::MissingObjects(_) => {
                bail!("platform bundle is missing objects for its tool tree")
            }
        }

        let platform_directory = root.join("platforms").join(&self.platform);
        fs::create_dir_all(&platform_directory).with_context(|| {
            format!(
                "failed to create platform directory {}",
                platform_directory.display()
            )
        })?;
        let profile = PlatformProfile {
            runner: self.runner.clone(),
            tools: tree_digest,
        };
        let destination = platform_directory.join("default.json");
        let temporary =
            platform_directory.join(format!(".default.json.tmp-{}", std::process::id()));
        fs::write(
            &temporary,
            serde_json::to_vec_pretty(&profile).context("failed to encode platform profile")?,
        )
        .with_context(|| format!("failed to write platform profile {}", temporary.display()))?;
        fs::rename(&temporary, &destination)
            .with_context(|| format!("failed to install profile {}", destination.display()))?;
        Ok(destination)
    }
}

impl PlatformStore {
    pub fn load(root: PathBuf, platform: &str) -> Result<Option<Self>> {
        let path = root.join("platforms").join(platform).join("default.json");
        let profile: PlatformProfile = match File::open(&path) {
            Ok(file) => serde_json::from_reader(file)
                .with_context(|| format!("failed to decode platform profile {}", path.display()))?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!("failed to open platform profile {}", path.display())
                });
            }
        };
        Ok(Some(Self {
            store: Store::open(root)?,
            profile,
        }))
    }

    pub fn runner(&self) -> Result<Vec<u8>> {
        let mut content = Vec::new();
        if !self
            .store
            .copy_object_to(&self.profile.runner, &mut content)?
        {
            bail!("platform runner object is not executable");
        }
        Ok(content)
    }

    pub fn runner_path(&self) -> Result<PathBuf> {
        let (path, executable) = self.store.object_path(&self.profile.runner)?;
        if !executable {
            bail!("platform runner object is not executable");
        }
        Ok(path)
    }

    pub fn tool_path(&self, name: &OsStr) -> Result<PathBuf> {
        let name = name
            .to_str()
            .with_context(|| format!("platform tool name {name:?} is not valid UTF-8"))?;
        let manifest = self.tools()?;
        let object = manifest
            .entries
            .iter()
            .find_map(|entry| match entry {
                TreeEntry::File { path, object } if path == &format!("bin/{name}") => {
                    Some(object.as_str())
                }
                _ => None,
            })
            .with_context(|| format!("platform tool {name:?} is not available"))?;
        let (path, executable) = self.store.object_path(object)?;
        if !executable {
            bail!("platform tool {name:?} is not executable");
        }
        Ok(path)
    }

    pub fn tools(&self) -> Result<TreeManifest> {
        self.store.read_manifest(&self.profile.tools)
    }

    pub fn copy_object_to(&self, digest: &str, destination: impl std::io::Write) -> Result<bool> {
        self.store.copy_object_to(digest, destination)
    }
}

fn read_object(archive: &mut ZipArchive<File>, manifest: ManifestObject) -> Result<Object> {
    let mut compressed = Vec::new();
    archive
        .by_name(&manifest.blob)
        .with_context(|| format!("platform bundle does not contain blob {}", manifest.blob))?
        .read_to_end(&mut compressed)
        .with_context(|| format!("failed to read blob {}", manifest.blob))?;
    let compressed_digest = format!("{:x}", Sha256::digest(&compressed));
    let expected_blob = format!("blobs/{compressed_digest}.zst");
    if manifest.blob != expected_blob {
        bail!(
            "compressed blob digest mismatch for object {}",
            manifest.digest
        );
    }
    Ok(Object {
        executable: manifest.executable,
        compressed,
    })
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    fn setup_platform(root: &Path) -> (String, String) {
        let store = Store::open(root.to_owned()).unwrap();
        let runner_content = b"runner";
        let runner_digest = atra_store::object_digest(runner_content, true);
        store
            .put_object(&runner_digest, true, Cursor::new(runner_content))
            .unwrap();
        let tool_content = b"tool";
        let tool_digest = atra_store::object_digest(tool_content, true);
        store
            .put_object(&tool_digest, true, Cursor::new(tool_content))
            .unwrap();
        let manifest = TreeManifest {
            entries: vec![TreeEntry::File {
                path: "bin/bwrap".to_owned(),
                object: tool_digest.clone(),
            }],
        };
        let tree_digest = manifest.digest();
        match store.prepare_tree(&manifest).unwrap() {
            PreparedTree::Ready { .. } => {}
            PreparedTree::MissingObjects(_) => panic!("tree was not ready"),
        }
        let profile = PlatformProfile {
            runner: runner_digest.clone(),
            tools: tree_digest,
        };
        let platform_directory = root.join("platforms").join("test-linux-static");
        fs::create_dir_all(&platform_directory).unwrap();
        fs::write(
            platform_directory.join("default.json"),
            serde_json::to_vec(&profile).unwrap(),
        )
        .unwrap();
        (runner_digest, tool_digest)
    }

    fn load(root: &Path) -> PlatformStore {
        PlatformStore::load(root.to_owned(), "test-linux-static")
            .unwrap()
            .unwrap()
    }

    #[test]
    fn runner_path_returns_the_executable_object() {
        let temporary = tempfile::tempdir().unwrap();
        let (runner_digest, _) = setup_platform(temporary.path());
        let path = load(temporary.path()).runner_path().unwrap();
        assert!(path.ends_with(&runner_digest));
    }

    #[test]
    fn tool_path_returns_the_executable_object() {
        let temporary = tempfile::tempdir().unwrap();
        let (_, tool_digest) = setup_platform(temporary.path());
        let path = load(temporary.path())
            .tool_path(OsStr::new("bwrap"))
            .unwrap();
        assert!(path.ends_with(&tool_digest));
    }

    #[test]
    fn tool_path_rejects_a_missing_tool() {
        let temporary = tempfile::tempdir().unwrap();
        setup_platform(temporary.path());
        assert!(
            load(temporary.path())
                .tool_path(OsStr::new("missing"))
                .is_err()
        );
    }
}
