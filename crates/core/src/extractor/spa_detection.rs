use crate::document::{JsFramework, SpaDetection};
use markup5ever_rcdom::{Handle, NodeData};

/// 本文テキスト量とフレームワークの痕跡から、ヘッドレスブラウザへの委譲が必要かを判定する。
///
/// フレームワーク検出は本文が少ないときの補助シグナルにとどめる。Next.js などの SSR ページは
/// フレームワークの痕跡があっても本文が HTML に揃っており、毎回委譲すると遅く、委譲先がない環境では無駄になる。
pub fn detect_spa(root: &Handle, text_length: usize, min_content_length: usize) -> SpaDetection {
    if text_length >= min_content_length {
        return SpaDetection::Static;
    }
    match detect_js_framework(root) {
        Some(framework) => SpaDetection::FrameworkDetected { framework },
        // 本文が短いだけの静的ページを SPA と誤判定すると、委譲チェーンへの
        // 無駄な問い合わせと警告が毎回走る（example.com でも Jina に飛んでいた）。
        // マウント要素や大きなインラインスクリプトという JS 実行の痕跡がある場合のみ SPA とする
        None if has_dynamic_markers(root) => SpaDetection::SuspectedSpa { text_length },
        None => SpaDetection::Static,
    }
}

/// 本文が短いページについて、JS で後から描画される可能性があるかを判定する。
/// 外部スクリプトは解析タグでも成り立つため根拠にしない。マウント要素と
/// 大きなインラインスクリプトのみを見る（フレームワーク判定は呼び出し側で先に行う）。
fn has_dynamic_markers(root: &Handle) -> bool {
    has_script_or_mount(root)
}

fn has_script_or_mount(handle: &Handle) -> bool {
    if let NodeData::Element { name, attrs, .. } = &handle.data {
        let tag = name.local.as_ref();
        let attr_ref = attrs.borrow();

        if tag == "script" {
            let inline_len: usize = handle
                .children
                .borrow()
                .iter()
                .map(|c| match &c.data {
                    NodeData::Text { contents } => contents.borrow().len(),
                    _ => 0,
                })
                .sum();
            if inline_len > 512 {
                return true;
            }
        }

        if matches!(tag, "div" | "main" | "body") {
            for attr in attr_ref.iter() {
                if attr.name.local.as_ref() != "id" {
                    continue;
                }
                if matches!(
                    attr.value.as_ref(),
                    "root"
                        | "app"
                        | "app-root"
                        | "react-root"
                        | "__next"
                        | "__nuxt"
                        | "svelte"
                        | "___gatsby"
                ) {
                    return true;
                }
            }
        }
    }

    handle.children.borrow().iter().any(has_script_or_mount)
}

fn detect_js_framework(root: &Handle) -> Option<JsFramework> {
    detect_in_node(root)
}

fn detect_in_node(handle: &Handle) -> Option<JsFramework> {
    match &handle.data {
        NodeData::Element { name, attrs, .. } => {
            let tag = name.local.as_ref();
            let attrs = attrs.borrow();

            // Next.js: <div id="__next">
            // React: data-reactroot attribute
            for attr in attrs.iter() {
                let attr_name = attr.name.local.as_ref();
                let attr_val = attr.value.as_ref();

                if attr_name == "id" && attr_val == "__next" {
                    return Some(JsFramework::Next);
                }
                if attr_name == "id" && attr_val == "__nuxt" {
                    return Some(JsFramework::Nuxt);
                }
                if attr_name == "data-reactroot" {
                    return Some(JsFramework::React);
                }
                // Vue: data-v-* attributes
                if attr_name.starts_with("data-v-") {
                    return Some(JsFramework::Vue);
                }
                // Angular: ng-version attribute
                if attr_name == "ng-version" {
                    return Some(JsFramework::Angular);
                }
            }

            if tag == "script" {
                if let Some(src) = attrs.iter().find(|a| a.name.local.as_ref() == "src") {
                    if let Some(fw) = framework_from_script_src(src.value.as_ref()) {
                        return Some(fw);
                    }
                }
            }
        }
        NodeData::Text { contents } => {
            let text = contents.borrow();
            let t = text.as_ref();
            // インラインスクリプト内でのフレームワーク検出
            if t.contains("__NEXT_DATA__") {
                return Some(JsFramework::Next);
            }
            if t.contains("__nuxt") {
                return Some(JsFramework::Nuxt);
            }
        }
        _ => {}
    }

    for child in handle.children.borrow().iter() {
        if let Some(fw) = detect_in_node(child) {
            return Some(fw);
        }
    }

    None
}

/// script の src のファイル名を英数字以外で区切った語に `react` / `vue` があればそのフレームワークとみなす。
///
/// 部分一致だと `preact.min.js`・`reactions.js`・`revue.js` を誤検出するため語単位で照合する。
/// ディレクトリ名（`/react-app/main.js` など）はサイト固有の命名で根拠にならないので見ない。
fn framework_from_script_src(src: &str) -> Option<JsFramework> {
    let path = src.split(['?', '#']).next().unwrap_or(src);
    let file = path.rsplit('/').next().unwrap_or(path).to_ascii_lowercase();
    let mut words = file.split(|c: char| !c.is_ascii_alphanumeric());
    words.find_map(|w| match w {
        "react" => Some(JsFramework::React),
        "vue" => Some(JsFramework::Vue),
        _ => None,
    })
}

/// 本文として読めるテキストの文字数。
///
/// バイト数で数えると日本語は 3 倍に数えられ、本文が少ないページを見逃す。
/// ナビゲーション・フッター等は全ページ共通の定型文で、本文の有無の判断材料にならないため除外する。
pub fn extract_text_length(handle: &Handle) -> usize {
    let mut len = 0;
    collect_text_length(handle, &mut len);
    len
}

fn collect_text_length(handle: &Handle, len: &mut usize) {
    match &handle.data {
        NodeData::Text { contents } => {
            let text = contents.borrow();
            *len += text.trim().chars().count();
        }
        NodeData::Element { name, .. } => {
            let tag = name.local.as_ref();
            if matches!(
                tag,
                "head"
                    | "script"
                    | "style"
                    | "noscript"
                    | "template"
                    | "nav"
                    | "header"
                    | "footer"
                    | "aside"
            ) {
                return;
            }
            for child in handle.children.borrow().iter() {
                collect_text_length(child, len);
            }
        }
        _ => {
            for child in handle.children.borrow().iter() {
                collect_text_length(child, len);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::extractor::parse_html;

    const MIN: usize = 500;

    fn detect(html: &str) -> SpaDetection {
        let dom = parse_html(html);
        let len = extract_text_length(&dom.document);
        detect_spa(&dom.document, len, MIN)
    }

    #[test]
    fn test_framework_with_enough_text_is_static() {
        // Next.js の SSR ページは本文が揃っているので委譲不要
        let html = format!(
            r#"<html><body><div id="__next"><p>{}</p></div><script id="__NEXT_DATA__">{{}}</script></body></html>"#,
            "word ".repeat(200)
        );
        assert_eq!(detect(&html), SpaDetection::Static);
    }

    #[test]
    fn test_framework_with_little_text_is_detected() {
        let html = r#"<html><body><div id="__next"></div></body></html>"#;
        assert_eq!(
            detect(html),
            SpaDetection::FrameworkDetected {
                framework: JsFramework::Next
            }
        );
        let html = r#"<html><body><div id="app"></div><script src="/js/react-dom.production.min.js"></script></body></html>"#;
        assert_eq!(
            detect(html),
            SpaDetection::FrameworkDetected {
                framework: JsFramework::React
            }
        );
    }

    #[test]
    fn test_script_name_partial_match_is_not_framework() {
        for src in [
            "/js/preact.min.js",
            "/js/reactions.js",
            "/js/revue.js",
            "/react-app/main.js",
        ] {
            let html = format!(
                r#"<html><body><div id="app"></div><script src="{src}"></script></body></html>"#
            );
            let dom = parse_html(&html);
            assert_eq!(detect_js_framework(&dom.document), None, "{src}");
        }
        for (src, fw) in [
            (
                "https://unpkg.com/react@18/umd/react.production.min.js",
                JsFramework::React,
            ),
            ("/js/vue.global.prod.js", JsFramework::Vue),
            ("/js/vue-router.js", JsFramework::Vue),
        ] {
            let html = format!(r#"<html><body><script src="{src}"></script></body></html>"#);
            let dom = parse_html(&html);
            assert_eq!(detect_js_framework(&dom.document), Some(fw), "{src}");
        }
    }

    #[test]
    fn test_text_length_counts_chars_not_bytes() {
        // 200 文字 = 600 バイト。文字数では最小長 500 に届かない。
        // 委譲対象になり得る構造（マウント要素）を持たせて SPA 判定まで通す
        let html = format!(
            "<html><body><div id=\"app\"><p>{}</p></div></body></html>",
            "あ".repeat(200)
        );
        assert_eq!(
            detect(&html),
            SpaDetection::SuspectedSpa { text_length: 200 }
        );
    }

    #[test]
    fn test_text_length_excludes_navigation() {
        let html = format!(
            "<html><body><div id=\"app\"><nav>{}</nav><footer>{}</footer><main><p>短い本文</p></main></div></body></html>",
            "menu ".repeat(200),
            "foot ".repeat(200)
        );
        assert!(
            matches!(detect(&html), SpaDetection::SuspectedSpa { .. }),
            "{:?}",
            detect(&html)
        );
    }

    /// 本文が短いだけの静的ページを SPA と誤判定しない（example.com 相当）。
    /// 誤判定すると毎回 Chrome/Jina への委譲が走り、実行環境によっては別の内容が返る。
    #[test]
    fn test_short_static_page_is_not_spa() {
        let html = r#"<html><body><h1>Example Domain</h1>
            <p>This domain is for use in illustrative examples in documents.</p>
            </body></html>"#;
        assert_eq!(detect(html), SpaDetection::Static);
    }

    /// 外部スクリプトは解析タグでも成り立つので SPA の根拠にしない。
    #[test]
    fn test_short_page_with_only_external_script_is_not_spa() {
        let html = r#"<html><body><p>Hello.</p><script src="/s.js"></script></body></html>"#;
        assert_eq!(detect(html), SpaDetection::Static);
    }

    /// マウント要素があれば短いページでも委譲を試みる。
    #[test]
    fn test_short_page_with_mount_point_is_suspected_spa() {
        let html = r#"<html><body><div id="app"></div></body></html>"#;
        assert!(matches!(detect(html), SpaDetection::SuspectedSpa { .. }));
    }

    /// バンドルと思われる大きなインラインスクリプトも JS 実行の痕跡として扱う。
    #[test]
    fn test_large_inline_script_is_treated_as_bundle() {
        let html = format!(
            "<html><body><script>{}</script></body></html>",
            "x".repeat(600)
        );
        assert!(matches!(detect(&html), SpaDetection::SuspectedSpa { .. }));
    }
}
