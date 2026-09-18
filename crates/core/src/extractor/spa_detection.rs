use crate::document::{JsFramework, SpaDetection};
use markup5ever_rcdom::{Handle, NodeData};

pub fn detect_spa(root: &Handle, text_length: usize, min_content_length: usize) -> SpaDetection {
    if text_length < min_content_length {
        return SpaDetection::SuspectedSpa { text_length };
    }
    if let Some(framework) = detect_js_framework(root) {
        return SpaDetection::FrameworkDetected { framework };
    }
    SpaDetection::Static
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

            // script タグ内のフレームワーク検出
            if tag == "script" {
                for attr in attrs.iter() {
                    let val = attr.value.as_ref();
                    if val.contains("react") || val.contains("React") {
                        return Some(JsFramework::React);
                    }
                    if val.contains("vue") || val.contains("Vue") {
                        return Some(JsFramework::Vue);
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

pub fn extract_text_length(handle: &Handle) -> usize {
    let mut len = 0;
    collect_text_length(handle, &mut len);
    len
}

fn collect_text_length(handle: &Handle, len: &mut usize) {
    match &handle.data {
        NodeData::Text { contents } => {
            let text = contents.borrow();
            *len += text.trim().len();
        }
        NodeData::Element { name, .. } => {
            let tag = name.local.as_ref();
            // スクリプト・スタイルは除外
            if tag == "script" || tag == "style" {
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
