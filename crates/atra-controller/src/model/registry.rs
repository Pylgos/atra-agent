use std::{collections::BTreeMap, env, path::Path, sync::Arc};

use anyhow::{Context, Result, ensure};

use super::Provider;

pub(crate) struct ProviderRegistry {
    default_provider: &'static str,
    providers: BTreeMap<&'static str, Arc<Provider>>,
}

impl ProviderRegistry {
    pub(crate) async fn load(auth_home: &Path) -> Result<Self> {
        if let Some(path) = env::var_os("ATRA_FAKE_MODEL_SCRIPT") {
            let provider = super::fake(Path::new(&path))?;
            return Self::new(provider.id(), [provider]);
        }

        let codex = super::codex(auth_home.join(super::CODEX_PROVIDER)).await;
        let default_provider = codex.id();
        Self::new(
            default_provider,
            [
                codex,
                super::ollama(auth_home.join(super::OLLAMA_PROVIDER)),
                super::opencode_go(auth_home.join(super::OPENCODE_GO_PROVIDER)),
            ],
        )
    }

    pub(crate) fn new(
        default_provider: &'static str,
        providers: impl IntoIterator<Item = Arc<Provider>>,
    ) -> Result<Self> {
        let mut registered = BTreeMap::new();
        for provider in providers {
            let id = provider.id();
            ensure!(
                registered.insert(id, provider).is_none(),
                "duplicate model provider {id}"
            );
        }
        ensure!(!registered.is_empty(), "provider registry cannot be empty");
        ensure!(
            registered.contains_key(default_provider),
            "default model provider {default_provider} is not registered"
        );
        Ok(Self {
            default_provider,
            providers: registered,
        })
    }

    pub(crate) fn default_provider(&self) -> &'static str {
        self.default_provider
    }

    pub(crate) fn get(&self, id: &str) -> Result<&Arc<Provider>> {
        self.providers
            .get(id)
            .with_context(|| format!("unknown model provider {id}"))
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = &Arc<Provider>> {
        self.providers.values()
    }

    pub(crate) fn len(&self) -> usize {
        self.providers.len()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;

    fn fake_provider() -> (tempfile::TempDir, Arc<Provider>) {
        let directory = tempfile::tempdir().unwrap();
        let script = directory.path().join("script.json");
        std::fs::write(&script, "[]").unwrap();
        let provider = super::super::fake(&script).unwrap();
        (directory, provider)
    }

    #[test]
    fn rejects_invalid_provider_inventories() {
        let (_directory, provider) = fake_provider();
        assert!(
            ProviderRegistry::new(
                provider.id(),
                [Arc::clone(&provider), Arc::clone(&provider)],
            )
            .is_err()
        );
        assert!(ProviderRegistry::new("missing", [provider]).is_err());
    }
}
