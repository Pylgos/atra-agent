use std::{
    fs,
    os::unix::fs::{OpenOptionsExt, PermissionsExt},
    path::PathBuf,
};

use anyhow::{Context, Result, ensure};

pub(super) struct ApiKeyAuth {
    home: PathBuf,
    env: &'static str,
    key: std::sync::RwLock<Option<String>>,
}

impl ApiKeyAuth {
    pub(super) fn new(home: PathBuf, env: &'static str) -> Self {
        let key = load(&home, env);
        Self {
            home,
            env,
            key: std::sync::RwLock::new(key),
        }
    }

    pub(super) fn key(&self) -> Result<String> {
        self.key.read().unwrap().clone().with_context(|| {
            format!(
                "login required; run `atra provider login {}`",
                self.provider()
            )
        })
    }

    pub(super) fn configured(&self) -> bool {
        self.key.read().unwrap().is_some()
    }

    pub(super) fn source(&self) -> Option<atra_protocol::CredentialSource> {
        if std::env::var(self.env).is_ok_and(|value| !value.trim().is_empty()) {
            Some(atra_protocol::CredentialSource::Environment)
        } else if self.home.join("api-key").is_file() {
            Some(atra_protocol::CredentialSource::File)
        } else {
            None
        }
    }

    pub(super) fn login(&self, key: String) -> Result<()> {
        ensure!(!key.trim().is_empty(), "API key must not be empty");
        fs::create_dir_all(&self.home)?;
        fs::set_permissions(&self.home, fs::Permissions::from_mode(0o700))?;
        let path = self.home.join("api-key");
        let mut file = fs::OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .mode(0o600)
            .open(&path)?;
        std::io::Write::write_all(&mut file, key.trim().as_bytes())?;
        self.reload();
        Ok(())
    }

    pub(super) fn reload(&self) {
        *self.key.write().unwrap() = load(&self.home, self.env);
    }

    pub(super) fn logout(&self) -> Result<()> {
        match fs::remove_file(self.home.join("api-key")) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        self.reload();
        Ok(())
    }

    fn provider(&self) -> &str {
        self.home
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("provider")
    }
}

fn load(home: &std::path::Path, env: &str) -> Option<String> {
    std::env::var(env)
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(|| {
            fs::read_to_string(home.join("api-key"))
                .ok()
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
        })
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    #[test]
    fn file_credentials_are_private_and_reloadable() {
        let directory = tempfile::tempdir().unwrap();
        let home = directory.path().join("provider");
        let auth = ApiKeyAuth::new(home.clone(), "ATRA_TEST_UNUSED_API_KEY");
        auth.login("secret".to_owned()).unwrap();

        assert_eq!(
            fs::metadata(&home).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            fs::metadata(home.join("api-key"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(
            ApiKeyAuth::new(home, "ATRA_TEST_UNUSED_API_KEY")
                .key()
                .unwrap(),
            "secret"
        );
    }
}
