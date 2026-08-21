# atra-agent

## Model providers

Atra can use Codex, Ollama Cloud, and OpenCode Go in the same controller. Authenticate each provider once:

```sh
atra provider login codex
atra provider login ollama
atra provider login opencode-go
```

API keys are read from `OLLAMA_API_KEY` or `OPENCODE_API_KEY` first and otherwise stored in the
user's private Atra data directory. Use `atra provider list` for provider status and the complete
model catalog, then use `/model` in the TUI or `atra thread model ...` to select a model and one of
that model's exact reasoning options.

## Skills

Atra discovers `SKILL.md` files and makes them available in every Runner under
`$ATRA_SKILLS/<name>`. Invoke a skill explicitly by mentioning its name in a message:

```text
$review-code inspect this change
```

Repeated mentions in the same message invoke the skill once. Mentioning it again in a later
message loads the current instructions again. Use `\$review-code` to write a known skill name
without invoking it.

Set `disable-model-invocation: true` in the YAML frontmatter to hide a skill from the model's
automatic skill list while keeping explicit invocation available:

```yaml
---
name: deploy-production
description: Deploy the application to production
disable-model-invocation: true
---
```
