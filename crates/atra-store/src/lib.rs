use std::{
    fs,
    io::{ErrorKind, Read, Write},
    os::unix::fs::{PermissionsExt, symlink},
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use rustix::fs::{CWD, RenameFlags, renameat_with};
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
                for ancestor in parent
                    .ancestors()
                    .filter(|path| !path.as_os_str().is_empty())
                {
                    let ancestor = ancestor.to_string_lossy();
                    if self
                        .entries
                        .binary_search_by_key(&ancestor.as_ref(), TreeEntry::path)
                        .is_ok()
                    {
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
                            || entry
                                .path()
                                .strip_prefix(target)
                                .is_some_and(|suffix| suffix.starts_with('/'))
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

pub fn object_digest(content: &[u8], executable: bool) -> String {
    let mut digest = object_hasher(executable);
    digest.update(content);
    format!("{:x}", digest.finalize())
}

pub enum PreparedTree {
    MissingObjects(Vec<String>),
    Ready { digest: String, path: PathBuf },
}

#[derive(Clone)]
pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn open(root: PathBuf) -> Result<Self> {
        fs::create_dir_all(root.join("objects"))
            .with_context(|| format!("failed to create object store {}", root.display()))?;
        fs::create_dir_all(root.join("trees"))
            .with_context(|| format!("failed to create tree store {}", root.display()))?;
        Ok(Self { root })
    }

    pub fn put_object(
        &self,
        expected: &str,
        executable: bool,
        mut source: impl Read,
    ) -> Result<()> {
        validate_digest(expected)?;
        let mut actual = object_hasher(executable);
        let mut temporary = tempfile::NamedTempFile::new_in(self.root.join("objects"))
            .context("failed to create temporary object")?;
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let length = source.read(&mut buffer).context("failed to read object")?;
            if length == 0 {
                break;
            }
            actual.update(&buffer[..length]);
            temporary
                .write_all(&buffer[..length])
                .context("failed to write temporary object")?;
        }
        let actual = format!("{:x}", actual.finalize());
        if actual != expected {
            bail!("object digest mismatch: expected {expected}, got {actual}");
        }

        let destination = self.root.join("objects").join(expected);
        if destination.exists() {
            set_object_permissions(&destination, executable)?;
            return Ok(());
        }
        set_object_permissions(temporary.path(), executable)?;
        match temporary.persist_noclobber(&destination) {
            Ok(_) => Ok(()),
            Err(error) if error.error.kind() == ErrorKind::AlreadyExists => {
                set_object_permissions(&destination, executable)
            }
            Err(error) => Err(error.error)
                .with_context(|| format!("failed to store object {}", destination.display())),
        }
    }

    pub fn object_path(&self, digest: &str) -> Result<(PathBuf, bool)> {
        let (path, executable) = self.object_metadata(digest)?;
        verify_object(&path, digest, executable)?;
        Ok((path, executable))
    }

    pub fn copy_object_to(&self, digest: &str, mut destination: impl Write) -> Result<bool> {
        let (path, executable) = self.object_metadata(digest)?;
        let mut source = fs::File::open(&path)
            .with_context(|| format!("failed to open object {}", path.display()))?;
        let mut actual = object_hasher(executable);
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            let length = source
                .read(&mut buffer)
                .with_context(|| format!("failed to read object {}", path.display()))?;
            if length == 0 {
                break;
            }
            actual.update(&buffer[..length]);
            destination
                .write_all(&buffer[..length])
                .context("failed to copy object")?;
        }
        let actual = format!("{:x}", actual.finalize());
        if actual != digest {
            bail!("object digest mismatch: expected {digest}, got {actual}");
        }
        Ok(executable)
    }

    fn object_metadata(&self, digest: &str) -> Result<(PathBuf, bool)> {
        validate_digest(digest)?;
        let path = self.root.join("objects").join(digest);
        let metadata = path
            .metadata()
            .with_context(|| format!("failed to inspect object {}", path.display()))?;
        if !metadata.is_file() {
            bail!("object {} is not a regular file", path.display());
        }
        let executable = metadata.permissions().mode() & 0o111 != 0;
        Ok((path, executable))
    }

    pub fn prepare_tree(&self, manifest: &TreeManifest) -> Result<PreparedTree> {
        manifest.validate()?;
        let digest = manifest.digest();
        let directory = self.root.join("trees").join(&digest);
        if directory.is_dir() {
            return Ok(PreparedTree::Ready {
                digest,
                path: directory.join("root"),
            });
        }

        let mut missing = manifest
            .entries
            .iter()
            .filter_map(|entry| match entry {
                TreeEntry::File { object, .. }
                    if !self.root.join("objects").join(object).is_file() =>
                {
                    Some(object.clone())
                }
                _ => None,
            })
            .collect::<Vec<_>>();
        missing.sort();
        missing.dedup();
        if !missing.is_empty() {
            return Ok(PreparedTree::MissingObjects(missing));
        }

        let temporary = tempfile::Builder::new()
            .prefix(".tree-")
            .tempdir_in(self.root.join("trees"))
            .context("failed to create temporary tree")?;
        let root = temporary.path().join("root");
        fs::create_dir(&root).context("failed to create tree root")?;
        fs::write(
            temporary.path().join("manifest.json"),
            serde_json::to_vec(manifest).context("failed to encode tree manifest")?,
        )
        .context("failed to write tree manifest")?;
        for entry in &manifest.entries {
            let path = root.join(entry.path());
            fs::create_dir_all(path.parent().expect("tree entry should have a parent"))
                .with_context(|| format!("failed to create directory for {}", path.display()))?;
            match entry {
                TreeEntry::File {
                    path: logical,
                    object,
                } => symlink(object_target(logical, object), &path),
                TreeEntry::Symlink {
                    path: logical,
                    target,
                } => symlink(tree_target(logical, target), &path),
            }
            .with_context(|| format!("failed to create tree entry {}", path.display()))?;
        }

        let temporary_path = temporary.keep();
        match renameat_with(
            CWD,
            &temporary_path,
            CWD,
            &directory,
            RenameFlags::NOREPLACE,
        ) {
            Ok(()) => {}
            Err(error) if error == rustix::io::Errno::EXIST => {
                fs::remove_dir_all(&temporary_path).with_context(|| {
                    format!(
                        "failed to remove temporary tree {}",
                        temporary_path.display()
                    )
                })?;
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("failed to publish tree {}", directory.display()));
            }
        }
        Ok(PreparedTree::Ready {
            digest,
            path: directory.join("root"),
        })
    }

    pub fn read_manifest(&self, digest: &str) -> Result<TreeManifest> {
        validate_digest(digest)?;
        let path = self.root.join("trees").join(digest).join("manifest.json");
        let manifest: TreeManifest = serde_json::from_reader(
            fs::File::open(&path)
                .with_context(|| format!("failed to open tree manifest {}", path.display()))?,
        )
        .with_context(|| format!("failed to decode tree manifest {}", path.display()))?;
        manifest.validate()?;
        if manifest.digest() != digest {
            bail!("tree manifest digest mismatch in {}", path.display());
        }
        Ok(manifest)
    }
}

fn update_digest_field(digest: &mut Sha256, value: &[u8]) {
    digest.update((value.len() as u64).to_be_bytes());
    digest.update(value);
}

fn object_hasher(executable: bool) -> Sha256 {
    let mut digest = Sha256::new();
    digest.update(b"atra-object\0");
    digest.update([u8::from(executable)]);
    digest
}

fn verify_object(path: &Path, digest: &str, executable: bool) -> Result<()> {
    let mut source = fs::File::open(path)
        .with_context(|| format!("failed to open object {}", path.display()))?;
    let mut actual = object_hasher(executable);
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let length = source
            .read(&mut buffer)
            .with_context(|| format!("failed to read object {}", path.display()))?;
        if length == 0 {
            break;
        }
        actual.update(&buffer[..length]);
    }
    let actual = format!("{:x}", actual.finalize());
    if actual != digest {
        bail!("object digest mismatch: expected {digest}, got {actual}");
    }
    Ok(())
}

fn set_object_permissions(path: &Path, executable: bool) -> Result<()> {
    fs::set_permissions(
        path,
        fs::Permissions::from_mode(if executable { 0o555 } else { 0o444 }),
    )
    .with_context(|| format!("failed to set object permissions on {}", path.display()))
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
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
    {
        bail!("invalid digest");
    }
    Ok(())
}

fn object_target(path: &str, object: &str) -> PathBuf {
    let depth = Path::new(path).components().count() - 1;
    let mut target = PathBuf::new();
    for _ in 0..depth + 3 {
        target.push("..");
    }
    target.join("objects").join(object)
}

fn tree_target(path: &str, destination: &str) -> PathBuf {
    let depth = Path::new(path).components().count() - 1;
    let mut target = PathBuf::new();
    for _ in 0..depth {
        target.push("..");
    }
    target.join(destination)
}

#[cfg(test)]
mod tests {
    use std::{
        io::Cursor,
        os::unix::fs::PermissionsExt,
        sync::{Arc, Barrier},
    };

    use super::*;

    #[test]
    fn streams_objects_and_materializes_the_same_tree_concurrently() {
        let temporary = tempfile::tempdir().unwrap();
        let store = Store::open(temporary.path().join("store")).unwrap();
        let content = b"shared content";
        let object = object_digest(content, false);
        let manifest = TreeManifest {
            entries: vec![
                TreeEntry::File {
                    path: "file.txt".to_owned(),
                    object: object.clone(),
                },
                TreeEntry::Symlink {
                    path: "link.txt".to_owned(),
                    target: "file.txt".to_owned(),
                },
            ],
        };

        match store.prepare_tree(&manifest).unwrap() {
            PreparedTree::MissingObjects(digests) => assert_eq!(digests, vec![object.clone()]),
            PreparedTree::Ready { .. } => panic!("tree was ready without its object"),
        }
        store
            .put_object(&object, false, Cursor::new(content))
            .unwrap();
        let mut copied = Vec::new();
        assert!(!store.copy_object_to(&object, &mut copied).unwrap());
        assert_eq!(copied, content);

        let barrier = Arc::new(Barrier::new(2));
        let threads: [std::thread::JoinHandle<PreparedTree>; 2] = std::array::from_fn(|_| {
            let store = store.clone();
            let manifest = manifest.clone();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                barrier.wait();
                store.prepare_tree(&manifest).unwrap()
            })
        });
        let paths = threads.map(|thread| match thread.join().unwrap() {
            PreparedTree::Ready { path, .. } => path,
            PreparedTree::MissingObjects(_) => panic!("stored object was reported missing"),
        });

        assert_eq!(paths[0], paths[1]);
        assert_eq!(fs::read(paths[0].join("link.txt")).unwrap(), content);
        assert_eq!(
            fs::metadata(temporary.path().join("store/objects").join(object))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o444
        );
        assert_eq!(
            store.read_manifest(&manifest.digest()).unwrap().digest(),
            manifest.digest()
        );
    }

    #[test]
    fn object_path_returns_the_verified_object() {
        let temporary = tempfile::tempdir().unwrap();
        let store = Store::open(temporary.path().join("store")).unwrap();
        let content = b"executable content";
        let object = object_digest(content, true);
        store
            .put_object(&object, true, Cursor::new(content))
            .unwrap();

        let (path, executable) = store.object_path(&object).unwrap();
        assert!(executable);
        assert_eq!(path, temporary.path().join("store/objects").join(&object));
        assert_eq!(fs::read(&path).unwrap(), content);
    }

    #[test]
    fn object_path_rejects_a_missing_object() {
        let temporary = tempfile::tempdir().unwrap();
        let store = Store::open(temporary.path().join("store")).unwrap();
        let object = object_digest(b"missing", true);
        assert!(store.object_path(&object).is_err());
    }

    #[test]
    fn restores_permissions_when_putting_an_existing_object() {
        let temporary = tempfile::tempdir().unwrap();
        let store = Store::open(temporary.path().join("store")).unwrap();
        let content = b"skill content";
        let object = object_digest(content, false);
        let path = temporary.path().join("store/objects").join(&object);

        store
            .put_object(&object, false, Cursor::new(content))
            .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o454)).unwrap();

        store
            .put_object(&object, false, Cursor::new(content))
            .unwrap();

        assert_eq!(
            fs::metadata(path).unwrap().permissions().mode() & 0o777,
            0o444
        );
        let mut copied = Vec::new();
        assert!(!store.copy_object_to(&object, &mut copied).unwrap());
        assert_eq!(copied, content);
    }
}
