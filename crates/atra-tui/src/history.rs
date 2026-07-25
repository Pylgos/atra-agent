use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::Path,
};

use anyhow::{Context, Result};

use crate::input::InputBuffer;

pub(super) fn load(path: &Path) -> Result<Vec<String>> {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(error)
                .with_context(|| format!("failed to read TUI history {}", path.display()));
        }
    };
    contents
        .lines()
        .enumerate()
        .map(|(index, line)| {
            serde_json::from_str(line).with_context(|| {
                format!(
                    "failed to decode TUI history {} at line {}",
                    path.display(),
                    index + 1
                )
            })
        })
        .collect()
}

pub(super) fn record(path: &Path, input: &mut InputBuffer, value: String) -> Result<()> {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
        .with_context(|| format!("failed to open TUI history {}", path.display()))?;
    writeln!(file, "{}", serde_json::to_string(&value)?)
        .with_context(|| format!("failed to write TUI history {}", path.display()))?;
    let mut permissions = file.metadata()?.permissions();
    permissions.set_mode(0o600);
    file.set_permissions(permissions)?;
    input.record_history(value);
    Ok(())
}
