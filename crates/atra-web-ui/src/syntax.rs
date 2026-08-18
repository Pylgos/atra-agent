use wasm_bindgen::prelude::*;

#[wasm_bindgen(inline_js = r#"
export function atraHighlight(source, token) {
    const prism = globalThis.Prism;
    const grammar = prism && prism.languages[token];
    if (!grammar) {
        return source
            .replaceAll("&", "&amp;")
            .replaceAll("<", "&lt;")
            .replaceAll(">", "&gt;")
            .replaceAll('"', "&quot;");
    }
    if (token === "diff") {
        let oldLine = null;
        let newLine = null;
        return source.split("\n").map((line) => {
            const hunk = line.match(/^@@ -(\\d+)(?:,\\d+)? \\+(\\d+)(?:,\\d+)? @@/);
            if (hunk) {
                oldLine = Number(hunk[1]);
                newLine = Number(hunk[2]);
            }
            let oldLabel = "";
            let newLabel = "";
            if (line.startsWith("+") && !line.startsWith("+++")) {
                newLabel = String(newLine++);
            } else if (line.startsWith("-") && !line.startsWith("---")) {
                oldLabel = String(oldLine++);
            } else if (!line.startsWith("@@") && !line.startsWith("---") && !line.startsWith("+++")) {
                oldLabel = oldLine == null ? "" : String(oldLine++);
                newLabel = newLine == null ? "" : String(newLine++);
            }
            const html = prism.highlight(line, grammar, token) || " ";
            return `<span class="diff-line" data-old="${oldLabel}" data-new="${newLabel}">${html}</span>`;
        }).join("");
    }
    return prism.highlight(source, grammar, token);
}

export function atraSetupMarkdownHighlighting() {
    const highlight = (root) => {
        const prism = globalThis.Prism;
        if (!prism) return;
        root.querySelectorAll('pre code[class*="language-"]').forEach((el) => {
            if (el.classList.contains("highlighted")) return;
            const lang = el.className.match(/language-([a-zA-Z0-9_-]+)/)?.[1];
            if (!lang) return;
            const grammar = prism.languages[lang];
            if (!grammar) return;
            el.innerHTML = prism.highlight(el.textContent, grammar, lang);
            el.classList.add("highlighted");
        });
    };
    const observer = new MutationObserver((mutations) => {
        for (const mutation of mutations) {
            for (const node of mutation.addedNodes) {
                if (node.nodeType === Node.ELEMENT_NODE) {
                    highlight(node);
                }
            }
        }
    });
    observer.observe(document.body, { childList: true, subtree: true });
}
"#)]
extern "C" {
    #[wasm_bindgen(js_name = atraHighlight)]
    fn atra_highlight(source: &str, token: &str) -> String;

    #[wasm_bindgen(js_name = atraSetupMarkdownHighlighting)]
    fn atra_setup_markdown_highlighting();
}

pub(crate) fn highlight(source: &str, token: &str) -> String {
    atra_highlight(source, token)
}

pub(crate) fn setup_markdown_highlighting() {
    atra_setup_markdown_highlighting();
}
