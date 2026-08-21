import { createHighlighterCore } from "@shikijs/core";
import { createJavaScriptRegexEngine } from "@shikijs/engine-javascript";
import shellscript from "@shikijs/langs/shellscript";
import { ShikiStreamTokenizer } from "@shikijs/stream";
import githubDark from "@shikijs/themes/github-dark";
import githubLight from "@shikijs/themes/github-light";

const languageLoaders = {
  c: () => import("@shikijs/langs/c"),
  cpp: () => import("@shikijs/langs/cpp"),
  css: () => import("@shikijs/langs/css"),
  diff: () => import("@shikijs/langs/diff"),
  go: () => import("@shikijs/langs/go"),
  html: () => import("@shikijs/langs/html"),
  java: () => import("@shikijs/langs/java"),
  javascript: () => import("@shikijs/langs/javascript"),
  json: () => import("@shikijs/langs/json"),
  jsx: () => import("@shikijs/langs/jsx"),
  markdown: () => import("@shikijs/langs/markdown"),
  python: () => import("@shikijs/langs/python"),
  rust: () => import("@shikijs/langs/rust"),
  sql: () => import("@shikijs/langs/sql"),
  toml: () => import("@shikijs/langs/toml"),
  tsx: () => import("@shikijs/langs/tsx"),
  typescript: () => import("@shikijs/langs/typescript"),
  xml: () => import("@shikijs/langs/xml"),
  yaml: () => import("@shikijs/langs/yaml"),
};

const languageAliases = {
  atom: "xml",
  bash: "shellscript",
  clike: "c",
  js: "javascript",
  md: "markdown",
  mathml: "xml",
  plaintext: "text",
  py: "python",
  rss: "xml",
  sh: "shellscript",
  shell: "shellscript",
  ssml: "xml",
  svg: "xml",
  text: "text",
  ts: "typescript",
  webmanifest: "json",
  yml: "yaml",
  markup: "html",
};

const themes = {
  light: "github-light",
  dark: "github-dark",
};

const highlighterReady = createHighlighterCore({
  langs: [shellscript],
  themes: [githubLight, githubDark],
  engine: createJavaScriptRegexEngine(),
});

function normalizeLanguage(language) {
  const normalized = language.toLowerCase();
  return languageAliases[normalized] ?? normalized;
}

async function ensureLanguage(highlighter, requestedLanguage) {
  const language = normalizeLanguage(requestedLanguage);
  if (language === "text") return language;
  if (highlighter.getLoadedLanguages().includes(language)) return language;

  const loader = languageLoaders[language];
  if (!loader) return "text";
  await highlighter.loadLanguage(await loader());
  return language;
}

function createTokenNode(token) {
  const span = document.createElement("span");
  span.className = "shiki-token";
  span.textContent = token.content;
  for (const [property, value] of Object.entries(token.htmlStyle ?? {})) {
    span.style.setProperty(property, String(value));
  }
  return span;
}

function replaceTokens(element, lines) {
  const fragment = document.createDocumentFragment();
  lines.forEach((line, index) => {
    if (index > 0) fragment.append("\n");
    for (const token of line) fragment.append(createTokenNode(token));
  });
  element.replaceChildren(fragment);
  element.classList.add("highlighted");
}

const staticRevisions = new WeakMap();
const staticSignatures = new WeakMap();

async function highlightStatic(element, source, requestedLanguage) {
  const signature = `${requestedLanguage}\0${source}`;
  if (staticSignatures.get(element) === signature) return;
  staticSignatures.set(element, signature);
  const revision = (staticRevisions.get(element) ?? 0) + 1;
  staticRevisions.set(element, revision);

  try {
    const highlighter = await highlighterReady;
    const language = await ensureLanguage(highlighter, requestedLanguage);
    if (staticRevisions.get(element) !== revision || !element.isConnected) return;
    if (language === "text") {
      element.textContent = source;
      element.classList.add("highlighted");
      return;
    }

    const result = highlighter.codeToTokens(source, {
      lang: language,
      themes,
      defaultColor: false,
    });
    if (staticRevisions.get(element) !== revision || !element.isConnected) return;
    replaceTokens(element, result.tokens);
  } catch (error) {
    console.error("Atra syntax highlighting failed", error);
    if (staticRevisions.get(element) === revision && element.isConnected) {
      element.textContent = source;
      element.classList.add("highlighted");
    }
  }
}

const commandStates = new WeakMap();

async function commandState(element) {
  let state = commandStates.get(element);
  if (state) return state;

  const highlighter = await highlighterReady;
  state = {
    failed: false,
    source: "",
    running: false,
    tokenizer: new ShikiStreamTokenizer({
      highlighter,
      lang: "shellscript",
      themes,
      defaultColor: false,
    }),
  };
  commandStates.set(element, state);
  return state;
}

function applyCommandPatch(element, patch) {
  for (let index = 0; index < patch.recall; index += 1) {
    element.lastChild?.remove();
  }
  const fragment = document.createDocumentFragment();
  for (const token of patch.stable) fragment.append(createTokenNode(token));
  for (const token of patch.unstable) fragment.append(createTokenNode(token));
  element.append(fragment);
  element.classList.add("highlighted");
}

async function updateCommand(element) {
  let state;
  try {
    state = await commandState(element);
  } catch (error) {
    console.error("Atra command highlighting failed", error);
    element.textContent = element.dataset.atraCommand ?? "";
    element.classList.add("highlighted");
    return;
  }
  if (state.running) return;
  state.running = true;
  try {
    while (element.isConnected) {
      const target = element.dataset.atraCommand ?? "";
      if (target === state.source) break;
      if (state.failed) {
        element.textContent = target;
        state.source = target;
        break;
      }
      if (!target.startsWith(state.source)) {
        state.tokenizer.clear();
        state.source = "";
        element.replaceChildren();
      }
      const chunk = target.slice(state.source.length);
      const patch = await state.tokenizer.enqueue(chunk);
      if (!element.isConnected) break;
      applyCommandPatch(element, patch);
      state.source = target;
    }
  } catch (error) {
    console.error("Atra command highlighting failed", error);
    const target = element.dataset.atraCommand ?? "";
    state.failed = true;
    state.source = target;
    element.textContent = target;
    element.classList.add("highlighted");
  } finally {
    state.running = false;
    if (element.isConnected && (element.dataset.atraCommand ?? "") !== state.source) {
      void updateCommand(element);
    }
  }
}

function visit(root) {
  if (!(root instanceof Element)) return;

  if (root.matches("[data-atra-command]")) void updateCommand(root);
  for (const element of root.querySelectorAll("[data-atra-command]")) {
    void updateCommand(element);
  }

  const staticElements = root.matches("[data-atra-highlight]")
    ? [root]
    : [];
  staticElements.push(...root.querySelectorAll("[data-atra-highlight]"));
  for (const element of staticElements) {
    void highlightStatic(
      element,
      element.dataset.atraSource ?? element.textContent ?? "",
      element.dataset.atraHighlight ?? "text",
    );
  }

  const markdownElements = root.matches('pre code[class*="language-"]')
    ? [root]
    : [];
  markdownElements.push(...root.querySelectorAll('pre code[class*="language-"]'));
  for (const element of markdownElements) {
    if (element.dataset.atraHighlight) continue;
    const language = element.className.match(/language-([a-zA-Z0-9_-]+)/)?.[1];
    if (!language) continue;
    element.dataset.atraHighlight = language;
    element.dataset.atraSource = element.textContent ?? "";
    void highlightStatic(element, element.dataset.atraSource, language);
  }
}

function setup() {
  if (globalThis.__atraShikiSetup) return;
  globalThis.__atraShikiSetup = true;
  visit(document.body);
  const observer = new MutationObserver((mutations) => {
    for (const mutation of mutations) {
      if (mutation.type === "attributes") {
        visit(mutation.target);
        continue;
      }
      for (const node of mutation.addedNodes) visit(node);
    }
  });
  observer.observe(document.body, {
    attributes: true,
    attributeFilter: [
      "class",
      "data-atra-command",
      "data-atra-highlight",
      "data-atra-source",
    ],
    childList: true,
    subtree: true,
  });
}

const api = { setup };
globalThis.AtraShiki = api;
globalThis.__atraResolveShiki?.(api);
globalThis.AtraShikiReady = Promise.resolve(api);
