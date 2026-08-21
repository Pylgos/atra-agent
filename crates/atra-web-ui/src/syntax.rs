use wasm_bindgen::prelude::*;

#[wasm_bindgen(inline_js = r#"
function atraShikiReady() {
    if (globalThis.AtraShiki) return Promise.resolve(globalThis.AtraShiki);
    if (!globalThis.AtraShikiReady) {
        globalThis.AtraShikiReady = new Promise((resolve) => {
            globalThis.__atraResolveShiki = resolve;
        });
    }
    return globalThis.AtraShikiReady;
}

export function atraSetupSyntaxHighlighting() {
    void atraShikiReady().then((shiki) => shiki.setup());
}
"#)]
extern "C" {
    #[wasm_bindgen(js_name = atraSetupSyntaxHighlighting)]
    fn atra_setup_syntax_highlighting();
}

pub(crate) fn setup_syntax_highlighting() {
    atra_setup_syntax_highlighting();
}
