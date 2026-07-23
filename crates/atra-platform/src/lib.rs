use std::{collections::HashMap, fs::File, io::Read, path::Path};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use zip::ZipArchive;

#[derive(Debug, Deserialize)]
struct Manifest {
    platform: String,
    runner: ManifestExecutable,
    tools: Vec<ManifestTool>,
}

#[derive(Debug, Deserialize)]
struct ManifestExecutable {
    digest: String,
    blob: String,
}

#[derive(Debug, Deserialize)]
struct ManifestTool {
    name: String,
    #[serde(flatten)]
    executable: ManifestExecutable,
}

pub struct PlatformBundle {
    platform: String,
    runner: Executable,
    tools: HashMap<String, Executable>,
}

pub struct Executable {
    digest: String,
    compressed: Vec<u8>,
}

impl PlatformBundle {
    pub fn load(path: &Path) -> Result<Self> {
        let file = File::open(path)
            .with_context(|| format!("failed to open platform bundle {}", path.display()))?;
        let mut archive = ZipArchive::new(file)
            .with_context(|| format!("failed to read platform bundle {}", path.display()))?;
        let manifest: Manifest = {
            let mut entry = archive
                .by_name("manifest.json")
                .context("platform bundle does not contain manifest.json")?;
            serde_json::from_reader(&mut entry)
                .context("failed to decode platform bundle manifest")?
        };

        let runner = read_executable(&mut archive, manifest.runner, "atra-runner")?;
        let mut tools = HashMap::new();
        for manifest_tool in manifest.tools {
            let executable =
                read_executable(&mut archive, manifest_tool.executable, &manifest_tool.name)?;
            if tools
                .insert(manifest_tool.name.clone(), executable)
                .is_some()
            {
                bail!("duplicate tool {} in bundle", manifest_tool.name);
            }
        }
        Ok(Self {
            platform: manifest.platform,
            runner,
            tools,
        })
    }

    pub fn platform(&self) -> &str {
        &self.platform
    }

    pub fn runner(&self) -> &Executable {
        &self.runner
    }

    pub fn tool_names(&self) -> Vec<String> {
        let mut names = self.tools.keys().cloned().collect::<Vec<_>>();
        names.sort();
        names
    }

    pub fn tool(&self, name: &str) -> Option<&Executable> {
        self.tools.get(name)
    }
}

impl Executable {
    pub fn digest(&self) -> &str {
        &self.digest
    }

    pub fn compressed(&self) -> &[u8] {
        &self.compressed
    }

    pub fn decompress(&self) -> Result<Vec<u8>> {
        let executable =
            zstd::decode_all(self.compressed.as_slice()).context("failed to decompress blob")?;
        let actual = format!("{:x}", Sha256::digest(&executable));
        if actual != self.digest {
            bail!(
                "executable digest mismatch: expected {}, got {actual}",
                self.digest
            );
        }
        Ok(executable)
    }
}

fn read_executable(
    archive: &mut ZipArchive<File>,
    manifest: ManifestExecutable,
    name: &str,
) -> Result<Executable> {
    let mut compressed = Vec::new();
    archive
        .by_name(&manifest.blob)
        .with_context(|| format!("platform bundle does not contain blob for {name}"))?
        .read_to_end(&mut compressed)
        .with_context(|| format!("failed to read blob for {name}"))?;
    let compressed_digest = format!("{:x}", Sha256::digest(&compressed));
    let expected_blob = format!("blobs/{compressed_digest}.zst");
    if manifest.blob != expected_blob {
        bail!("compressed blob digest mismatch for {name}");
    }
    Ok(Executable {
        digest: manifest.digest,
        compressed,
    })
}
