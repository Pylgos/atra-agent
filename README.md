# atra-agent

Development and test commands are documented in
[`docs/testing.md`](docs/testing.md).

## Model providers

Atra can use Codex and Ollama Cloud in the same controller. Authenticate each provider once:

```sh
atra codex login
atra ollama login
```

Ollama API keys are stored in the user's private Atra data directory. After login, use `/model`
in the TUI or `atra thread model --provider ollama ...` to select an Ollama Cloud model. Ollama
Cloud turns support streaming responses, thinking, Runner commands, web search, web fetch, token
usage, and conversation compaction.

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
