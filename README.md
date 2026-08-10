# atra-agent

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
