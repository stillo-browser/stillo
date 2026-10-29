use crate::document::{ExtractedContent, ExtractedLink, PageMetadata};
use crate::sanitize::is_navigable_url;
use chrono::{DateTime, Utc};
use markup5ever_rcdom::{Handle, NodeData};
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use url::Url;

const NOISE_TAGS: &[&str] = &[
    "nav", "header", "footer", "aside", "script", "style", "noscript", "iframe", "form",
];
const NOISE_CLASS_PATTERNS: &[&str] = &[
    "nav", "sidebar", "menu", "ad", "banner", "comment", "footer", "header", "widget",
];
const CONTENT_CLASS_PATTERNS: &[&str] = &[
    "article", "content", "main", "post", "entry", "body", "text",
];

/// 複数の article のうち 1 件がテキスト総量のこの割合を超えたら、その 1 件を本文とみなす。
/// 記事本文 + 小さな関連記事カードという構造で、共通の親まで広げてノイズを拾わないため。
const DOMINANT_ARTICLE_RATIO: f64 = 2.0 / 3.0;

pub struct ReadabilityExtractor {
    pub preserve_links: bool,
}

impl ReadabilityExtractor {
    pub fn extract(&self, root: &Handle, base_url: &Url) -> ExtractedContent {
        let title = extract_title(root);
        let metadata = extract_metadata(root, base_url);
        let index = DomIndex::build(root);
        let body = find_body(root);

        let main_node = body
            .as_ref()
            .and_then(|b| find_main_content(b, &index))
            .or(body.clone());

        let (mh, mt, ml) = main_node
            .as_ref()
            .map(|n| self.serialize_content(n, base_url, &index))
            .unwrap_or_else(|| (String::new(), String::new(), Vec::new()));

        // 選択されたノードのコンテンツが極端に少ない場合は body 全体を試みる。
        // ニュース一覧など「全てがリンク」な構造ではスコアリングが個別カードを選びがちなため。
        let (body_html, body_text, links) = if mt.trim().len() < 200 {
            if let Some(b) = body.as_ref() {
                let (bh, bt, bl) = self.serialize_content(b, base_url, &index);
                if bt.trim().len() > mt.trim().len() {
                    (bh, bt, bl)
                } else {
                    (mh, mt, ml)
                }
            } else {
                (mh, mt, ml)
            }
        } else {
            (mh, mt, ml)
        };

        ExtractedContent {
            url: base_url.clone(),
            title: title.unwrap_or_else(|| base_url.to_string()),
            byline: metadata.og_title.clone(),
            body_text: body_text.trim().to_owned(),
            body_html,
            links,
            metadata,
        }
    }

    fn serialize_content(
        &self,
        handle: &Handle,
        base_url: &Url,
        index: &DomIndex,
    ) -> (String, String, Vec<ExtractedLink>) {
        let mut s = Serializer {
            index,
            base_url,
            preserve_links: self.preserve_links,
            html: String::new(),
            text: String::new(),
            links: Vec::new(),
            seen_links: HashSet::new(),
            pre_depth: 0,
        };
        s.node(handle);
        (s.html, s.text, s.links)
    }
}

/// ノードを Rc のアドレスで識別するキー。抽出中は DOM を変更しないのでアドレスは安定する。
fn node_key(handle: &Handle) -> usize {
    Rc::as_ptr(handle) as usize
}

/// 部分木のテキスト量（script/style を除き、前後の空白を除いたバイト数）
#[derive(Debug, Clone, Copy, Default)]
struct TextStats {
    text: usize,
    link: usize,
}

/// 抽出 1 回分の DOM の事前解析結果。
///
/// ノイズ判定は祖先の情報（article 内か、main/article を含むか）を必要とし、
/// スコアリングは各ノードの部分木テキスト量を必要とする。
/// 候補ノードごとに部分木を再走査すると「ノード数 × 深さ」の計算量になるため、1 パスで集計しておく。
struct DomIndex {
    /// クラス/ID によるノイズ判定を適用しないノード（html/body/main/article とその祖先）。
    /// WordPress の `<body class="has-sidebar">` のように、本文を含む祖先要素にも
    /// レイアウト用のクラスが付くため。
    class_exempt: HashSet<usize>,
    /// article 内の header 要素。記事タイトルを含むのでタグ・クラスによるノイズ判定をしない。
    article_headers: HashSet<usize>,
    text_stats: HashMap<usize, TextStats>,
}

impl DomIndex {
    fn build(root: &Handle) -> Self {
        let mut index = DomIndex {
            class_exempt: HashSet::new(),
            article_headers: HashSet::new(),
            text_stats: HashMap::new(),
        };
        let mut ancestors = Vec::new();
        index.visit(root, false, false, &mut ancestors);
        index
    }

    fn visit(
        &mut self,
        handle: &Handle,
        in_article: bool,
        in_link: bool,
        ancestors: &mut Vec<usize>,
    ) -> TextStats {
        let key = node_key(handle);
        let (in_article, in_link) = match &handle.data {
            NodeData::Text { contents } => {
                let n = contents.borrow().trim().len();
                return TextStats {
                    text: n,
                    link: if in_link { n } else { 0 },
                };
            }
            NodeData::Element { name, .. } => {
                let tag = name.local.as_ref();
                if tag == "script" || tag == "style" {
                    return TextStats::default();
                }
                if matches!(tag, "html" | "body" | "main" | "article") {
                    self.class_exempt.insert(key);
                }
                if matches!(tag, "main" | "article") {
                    // 祖先は根に近い側から登録済みのことが多いので、既登録に当たったら打ち切る
                    for &a in ancestors.iter().rev() {
                        if !self.class_exempt.insert(a) {
                            break;
                        }
                    }
                }
                if tag == "header" && in_article {
                    self.article_headers.insert(key);
                }
                (in_article || tag == "article", in_link || tag == "a")
            }
            _ => (in_article, in_link),
        };

        let mut stats = TextStats::default();
        ancestors.push(key);
        for child in handle.children.borrow().iter() {
            let c = self.visit(child, in_article, in_link, ancestors);
            stats.text += c.text;
            stats.link += c.link;
        }
        ancestors.pop();
        self.text_stats.insert(key, stats);
        stats
    }

    fn stats(&self, handle: &Handle) -> TextStats {
        self.text_stats
            .get(&node_key(handle))
            .copied()
            .unwrap_or_default()
    }

    fn is_noise(&self, handle: &Handle) -> bool {
        let NodeData::Element { name, attrs, .. } = &handle.data else {
            return false;
        };
        let key = node_key(handle);
        if self.article_headers.contains(&key) {
            return false;
        }
        if NOISE_TAGS.contains(&name.local.as_ref()) {
            return true;
        }
        if self.class_exempt.contains(&key) {
            return false;
        }
        attrs.borrow().iter().any(|attr| {
            let aname = attr.name.local.as_ref();
            if aname != "class" && aname != "id" {
                return false;
            }
            let val = attr.value.as_ref().to_lowercase();
            NOISE_CLASS_PATTERNS
                .iter()
                .any(|pattern| class_contains_pattern(&val, pattern))
        })
    }
}

fn find_body(root: &Handle) -> Option<Handle> {
    find_tag(root, "body")
}

fn find_tag(handle: &Handle, tag_name: &str) -> Option<Handle> {
    if let NodeData::Element { name, .. } = &handle.data {
        if name.local.as_ref() == tag_name {
            return Some(handle.clone());
        }
    }
    for child in handle.children.borrow().iter() {
        if let Some(found) = find_tag(child, tag_name) {
            return Some(found);
        }
    }
    None
}

fn find_main_content(body: &Handle, index: &DomIndex) -> Option<Handle> {
    // <main>, <article> を優先
    for tag in ["main", "article"] {
        let mut found = Vec::new();
        collect_outermost(body, tag, index, &mut found);
        if let Some(node) = choose_semantic_container(&found, index) {
            return Some(node);
        }
    }

    // Readability.js 方式: リーフノードのスコアを祖先コンテナへ伝播し、
    // コンテンツが豊富な大きなブロックが選ばれるようにする。
    let mut candidates: HashMap<usize, (Handle, f64)> = HashMap::new();
    let mut ancestors: Vec<Handle> = Vec::new();
    collect_candidate_scores(body, &mut ancestors, &mut candidates, index);

    let best = candidates
        .into_values()
        .map(|(h, raw)| {
            let stats = index.stats(&h);
            let text_len = stats.text as f64;
            let link_len = stats.link as f64;
            let density = if text_len > 0.0 {
                link_len / text_len
            } else {
                1.0
            };
            let bonus = class_score(&h);
            let score = (raw + bonus) * (1.0 - density);
            (h, score)
        })
        .filter(|(_, s)| *s > 0.0)
        .max_by(|(_, a), (_, b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(node, _)| node);

    // シブリング展開: ベスト候補が「繰り返しパターンの1ユニット」であれば
    // その親コンテナへ展開する（例: ul>li>a>div → ul を返す）
    if let Some(ref node) = best {
        if let Some(expanded) = try_sibling_expand(node) {
            return Some(expanded);
        }
    }
    best
}

/// ノイズ部分木を除き、指定タグの要素のうち同じタグの祖先を持たないものを文書順に集める。
fn collect_outermost(handle: &Handle, tag_name: &str, index: &DomIndex, out: &mut Vec<Handle>) {
    if index.is_noise(handle) {
        return;
    }
    if let NodeData::Element { name, .. } = &handle.data {
        if name.local.as_ref() == tag_name {
            out.push(handle.clone());
            return;
        }
    }
    for child in handle.children.borrow().iter() {
        collect_outermost(child, tag_name, index, out);
    }
}

/// main/article の候補から本文コンテナを決める。
///
/// 一覧ページでは article が複数並ぶため、最初の 1 件だけを採ると 2 件目以降が消える。
/// 1 件がテキストの大半を占める場合（記事 + 関連記事カード）はその 1 件、
/// そうでなければ全件を含む最も近い共通の祖先を返す。
fn choose_semantic_container(nodes: &[Handle], index: &DomIndex) -> Option<Handle> {
    match nodes {
        [] => None,
        [only] => Some(only.clone()),
        _ => {
            let total: usize = nodes.iter().map(|n| index.stats(n).text).sum();
            let largest = nodes.iter().max_by_key(|n| index.stats(n).text)?;
            if total > 0 && index.stats(largest).text as f64 > total as f64 * DOMINANT_ARTICLE_RATIO
            {
                return Some(largest.clone());
            }
            lowest_common_ancestor(nodes)
        }
    }
}

fn parent_of(handle: &Handle) -> Option<Handle> {
    // rcdom の parent は Cell<Option<Weak<Node>>> なので take→set で安全に参照する
    let weak = handle.parent.take();
    handle.parent.set(weak.clone());
    weak?.upgrade()
}

/// 根から自身までの祖先列（自身を含む）
fn ancestor_chain(handle: &Handle) -> Vec<Handle> {
    let mut chain = vec![handle.clone()];
    let mut current = handle.clone();
    while let Some(p) = parent_of(&current) {
        chain.push(p.clone());
        current = p;
    }
    chain.reverse();
    chain
}

fn lowest_common_ancestor(nodes: &[Handle]) -> Option<Handle> {
    let chains: Vec<Vec<Handle>> = nodes.iter().map(ancestor_chain).collect();
    let first = chains.first()?;
    let mut common = None;
    for (depth, node) in first.iter().enumerate() {
        let shared = chains[1..]
            .iter()
            .all(|c| c.get(depth).is_some_and(|n| Rc::ptr_eq(n, node)));
        if !shared {
            break;
        }
        common = Some(node.clone());
    }
    common
}

/// ベスト候補が繰り返しパターンの1ユニットであれば親コンテナへ展開する。
///
/// 例: `ul > li > a > div`（div がベスト候補）の場合、
/// li が 3 つ以上あることを検出して ul を返す。
/// 最大 3 段階まで祖先を辿る。
fn try_sibling_expand(node: &Handle) -> Option<Handle> {
    let mut current = node.clone();
    for _ in 0..3 {
        let parent = parent_of(&current)?;

        let current_tag = match &current.data {
            NodeData::Element { name, .. } => name.local.as_ref().to_owned(),
            _ => return None,
        };

        let same_tag_count = parent
            .children
            .borrow()
            .iter()
            .filter(|c| {
                matches!(&c.data,
                NodeData::Element { name, .. } if name.local.as_ref() == current_tag)
            })
            .count();

        if same_tag_count >= 3 {
            return Some(parent);
        }

        current = parent;
    }
    None
}

/// p/pre/blockquote/td/li などコンテンツ信号になるノードを起点に、
/// 祖先コンテナへ最大 4 段階・重みを半減しながらスコアを伝播する。
fn collect_candidate_scores(
    handle: &Handle,
    ancestors: &mut Vec<Handle>,
    candidates: &mut HashMap<usize, (Handle, f64)>,
    index: &DomIndex,
) {
    if index.is_noise(handle) {
        return;
    }

    if let NodeData::Element { name, .. } = &handle.data {
        let tag = name.local.as_ref();
        let score = leaf_content_score(index.stats(handle).text, tag);

        if score > 0.0 {
            let mut weight = 1.0;
            let mut levels = 0usize;
            for ancestor in ancestors.iter().rev() {
                if let NodeData::Element { name: aname, .. } = &ancestor.data {
                    if is_candidate_tag(aname.local.as_ref()) {
                        candidates
                            .entry(node_key(ancestor))
                            .or_insert_with(|| (ancestor.clone(), 0.0))
                            .1 += score * weight;
                        weight *= 0.5;
                        levels += 1;
                        if levels >= 4 {
                            break;
                        }
                    }
                }
            }
        }
    }

    ancestors.push(handle.clone());
    for child in handle.children.borrow().iter() {
        collect_candidate_scores(child, ancestors, candidates, index);
    }
    ancestors.pop();
}

/// コンテンツ信号となるリーフノードの基礎スコア
fn leaf_content_score(text_len: usize, tag: &str) -> f64 {
    let text_len = text_len as f64;
    if text_len < 20.0 {
        return 0.0;
    }
    match tag {
        "p" => 1.0 + (text_len / 100.0).min(3.0),
        "pre" | "blockquote" => 3.0 + (text_len / 100.0).min(3.0),
        "td" => (text_len / 50.0).min(3.0),
        "li" => 0.5 + (text_len / 200.0).min(1.0),
        _ => 0.0,
    }
}

/// スコアを受け取るコンテナ候補として有効なタグ
fn is_candidate_tag(tag: &str) -> bool {
    matches!(
        tag,
        "div" | "section" | "article" | "main" | "blockquote" | "pre" | "td" | "tbody" | "p"
    )
}

fn class_score(handle: &Handle) -> f64 {
    let attrs = match &handle.data {
        NodeData::Element { attrs, .. } => attrs.borrow(),
        _ => return 0.0,
    };

    let mut score = 0.0;
    for attr in attrs.iter() {
        let name = attr.name.local.as_ref();
        if name != "class" && name != "id" {
            continue;
        }
        let val = attr.value.as_ref().to_lowercase();
        for pattern in CONTENT_CLASS_PATTERNS {
            if class_contains_pattern(&val, pattern) {
                score += 10.0;
            }
        }
        for pattern in NOISE_CLASS_PATTERNS {
            if class_contains_pattern(&val, pattern) {
                score -= 10.0;
            }
        }
    }
    score
}

/// CSSクラス文字列がパターンに一致するか、ハイフン区切りのコンポーネント単位で確認する。
/// "shadow-2xs" が "ad" にマッチする誤検出を防ぐため、
/// スペースで個々のクラス名に分割してからハイフンで分解して照合する。
fn class_contains_pattern(class_val: &str, pattern: &str) -> bool {
    class_val.split_whitespace().any(|token| {
        // Tailwind のレスポンシブプレフィックス (sm:, md:, lg: など) を除去
        let bare = token.split(':').next_back().unwrap_or(token);
        // ハイフン区切りのコンポーネントが完全一致するか確認
        bare.split('-').any(|part| part == pattern)
    })
}

/// 本文コンテナ以下を、html_to_ast / markdown が再パースする簡約 HTML と body_text に直列化する。
struct Serializer<'a> {
    index: &'a DomIndex,
    base_url: &'a Url,
    preserve_links: bool,
    html: String,
    text: String,
    links: Vec<ExtractedLink>,
    /// links の重複判定用。線形探索だとリンク数の二乗になり、数万リンクのページで秒単位かかる
    seen_links: HashSet<Url>,
    /// pre の内側では空白に意味があるため畳まない
    pre_depth: usize,
}

impl Serializer<'_> {
    fn node(&mut self, handle: &Handle) {
        if self.index.is_noise(handle) {
            return;
        }

        match &handle.data {
            NodeData::Text { contents } => {
                let t = contents.borrow();
                if self.pre_depth == 0 && t.trim().is_empty() {
                    // `<b>foo</b> <i>bar</i>` の間の空白ノードを捨てると語が連結するため、1 空白に畳んで残す
                    self.push_space();
                } else {
                    self.html.push_str(&html_escape(&t));
                    self.text.push_str(&t);
                }
            }
            NodeData::Element { name, attrs, .. } => {
                let tag = name.local.as_ref();
                match tag {
                    "script" | "style" | "noscript" | "iframe" => (),
                    "a" if self.preserve_links => self.anchor(handle, &attrs.borrow()),
                    _ if is_block_tag(tag) => self.block(handle, tag),
                    _ => self.children(handle),
                }
            }
            _ => {}
        }
    }

    fn children(&mut self, handle: &Handle) {
        for child in handle.children.borrow().iter() {
            self.node(child);
        }
    }

    fn block(&mut self, handle: &Handle, tag: &str) {
        let cell = matches!(tag, "td" | "th");
        self.break_text(cell);
        self.html.push('<');
        self.html.push_str(tag);
        self.html.push('>');
        if tag == "br" || tag == "hr" {
            return;
        }
        if tag == "pre" {
            self.pre_depth += 1;
        }
        self.children(handle);
        if tag == "pre" {
            self.pre_depth -= 1;
        }
        self.html.push_str("</");
        self.html.push_str(tag);
        self.html.push('>');
        self.break_text(cell);
    }

    fn anchor(&mut self, handle: &Handle, attrs: &[html5ever::Attribute]) {
        let attr = |n: &str| {
            attrs
                .iter()
                .find(|a| a.name.local.as_ref() == n)
                .map(|a| a.value.as_ref().to_owned())
        };
        let href = attr("href");
        let rel = attr("rel");

        // javascript: / data: 等はリンクにせず、アンカーテキストだけを本文として残す
        let resolved = href
            .as_deref()
            .and_then(|h| self.base_url.join(h).ok())
            .filter(is_navigable_url);
        let (Some(href), Some(href_url)) = (href, resolved) else {
            self.children(handle);
            return;
        };

        self.html
            .push_str(&format!("<a href=\"{}\">", html_escape(&href)));
        let mut link_text = String::new();
        for child in handle.children.borrow().iter() {
            self.node(child);
            collect_text(child, &mut link_text);
        }
        self.html.push_str("</a>");

        let href_url = resolve_ddg_redirect(href_url);
        let trimmed = link_text.trim().to_owned();
        // 画像リンク等でアンカーテキストが空、または同一URLの重複は除外
        if !trimmed.is_empty() && self.seen_links.insert(href_url.clone()) {
            self.links.push(ExtractedLink {
                text: trimmed,
                href: href_url,
                rel,
            });
        }
    }

    fn push_space(&mut self) {
        if !self.html.ends_with(char::is_whitespace) {
            self.html.push(' ');
        }
        if !self.text.is_empty() && !self.text.ends_with(char::is_whitespace) {
            self.text.push(' ');
        }
    }

    /// body_text でブロック同士が連結しないよう区切りを入れる。セルは空白、それ以外は改行。
    fn break_text(&mut self, cell: bool) {
        if self.text.is_empty() {
            return;
        }
        if cell {
            if !self.text.ends_with(char::is_whitespace) {
                self.text.push(' ');
            }
            return;
        }
        while self.text.ends_with(' ') {
            self.text.pop();
        }
        if !self.text.ends_with('\n') {
            self.text.push('\n');
        }
    }
}

/// 簡約 HTML にタグとして残すブロック要素
fn is_block_tag(tag: &str) -> bool {
    matches!(
        tag,
        "p" | "div"
            | "section"
            | "article"
            | "main"
            | "header"
            | "figure"
            | "figcaption"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "ul"
            | "ol"
            | "li"
            | "dl"
            | "dt"
            | "dd"
            | "blockquote"
            | "pre"
            | "br"
            | "hr"
            | "table"
            | "tr"
            | "td"
            | "th"
            | "thead"
            | "tbody"
    )
}

pub(crate) fn collect_text(handle: &Handle, out: &mut String) {
    match &handle.data {
        NodeData::Text { contents } => {
            out.push_str(contents.borrow().as_ref());
        }
        _ => {
            for child in handle.children.borrow().iter() {
                collect_text(child, out);
            }
        }
    }
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn extract_title(root: &Handle) -> Option<String> {
    // <title> タグを優先、次に <h1> を試みる
    if let Some(title_node) = find_tag(root, "title") {
        let mut text = String::new();
        collect_text(&title_node, &mut text);
        let trimmed = text.trim().to_owned();
        if !trimmed.is_empty() {
            return Some(trimmed);
        }
    }
    if let Some(h1) = find_tag(root, "h1") {
        let mut text = String::new();
        collect_text(&h1, &mut text);
        let trimmed = text.trim().to_owned();
        if !trimmed.is_empty() {
            return Some(trimmed);
        }
    }
    None
}

fn extract_metadata(root: &Handle, base_url: &Url) -> PageMetadata {
    let mut meta = PageMetadata {
        description: None,
        og_title: None,
        og_image: None,
        canonical: None,
        published_at: None,
        author: None,
        date_modified: None,
    };
    collect_meta(root, &mut meta, base_url);
    apply_json_ld(root, &mut meta);
    meta
}

fn collect_meta(handle: &Handle, meta: &mut PageMetadata, base_url: &Url) {
    if let NodeData::Element { name, attrs, .. } = &handle.data {
        let tag = name.local.as_ref();
        let attrs_ref = attrs.borrow();

        if tag == "meta" {
            let name_attr = attrs_ref
                .iter()
                .find(|a| a.name.local.as_ref() == "name")
                .map(|a| a.value.as_ref().to_lowercase());
            let property_attr = attrs_ref
                .iter()
                .find(|a| a.name.local.as_ref() == "property")
                .map(|a| a.value.as_ref().to_lowercase());
            let content = attrs_ref
                .iter()
                .find(|a| a.name.local.as_ref() == "content")
                .map(|a| a.value.as_ref().to_owned());

            match (name_attr.as_deref(), property_attr.as_deref(), content) {
                (Some("description"), _, Some(c)) => meta.description = Some(c),
                (_, Some("og:description"), Some(c)) => {
                    meta.description.get_or_insert(c);
                }
                (_, Some("og:title"), Some(c)) => meta.og_title = Some(c),
                (_, Some("og:image"), Some(c)) => meta.og_image = Some(c),
                _ => {}
            };
        } else if tag == "link" {
            let is_canonical = attrs_ref
                .iter()
                .any(|a| a.name.local.as_ref() == "rel" && a.value.as_ref() == "canonical");
            if is_canonical {
                if let Some(href) = attrs_ref
                    .iter()
                    .find(|a| a.name.local.as_ref() == "href")
                    .and_then(|a| base_url.join(a.value.as_ref()).ok())
                {
                    meta.canonical = Some(href);
                }
            }
        }
    }

    for child in handle.children.borrow().iter() {
        collect_meta(child, meta, base_url);
    }
}

/// `<script type="application/ld+json">` から構造化メタデータを抽出して meta に補完する。
/// OGP/meta タグで既に設定されているフィールドは上書きしない。
fn apply_json_ld(root: &Handle, meta: &mut PageMetadata) {
    let mut scripts: Vec<String> = Vec::new();
    collect_json_ld_scripts(root, &mut scripts);

    for script in scripts {
        if let Ok(value) = serde_json::from_str::<serde_json::Value>(&script) {
            let objects: &[serde_json::Value] = if value.is_array() {
                value.as_array().map(|a| a.as_slice()).unwrap_or(&[])
            } else {
                std::slice::from_ref(&value)
            };
            for obj in objects {
                merge_json_ld_object(obj, meta);
            }
        }
    }
}

fn collect_json_ld_scripts(handle: &Handle, out: &mut Vec<String>) {
    if let NodeData::Element { name, attrs, .. } = &handle.data {
        if name.local.as_ref() == "script" {
            let is_json_ld = attrs.borrow().iter().any(|a| {
                a.name.local.as_ref() == "type" && a.value.as_ref() == "application/ld+json"
            });
            if is_json_ld {
                let mut text = String::new();
                collect_text(handle, &mut text);
                if !text.trim().is_empty() {
                    out.push(text);
                }
                return;
            }
        }
    }
    for child in handle.children.borrow().iter() {
        collect_json_ld_scripts(child, out);
    }
}

fn merge_json_ld_object(obj: &serde_json::Value, meta: &mut PageMetadata) {
    if meta.author.is_none() {
        meta.author = extract_json_ld_author(obj);
    }
    if meta.published_at.is_none() {
        meta.published_at = obj["datePublished"].as_str().and_then(parse_iso_date);
    }
    if meta.date_modified.is_none() {
        meta.date_modified = obj["dateModified"].as_str().and_then(parse_iso_date);
    }
    if meta.description.is_none() {
        if let Some(desc) = obj["description"].as_str() {
            meta.description = Some(desc.to_owned());
        }
    }
}

fn extract_json_ld_author(obj: &serde_json::Value) -> Option<String> {
    let author = &obj["author"];
    // "author": "Name"
    if let Some(s) = author.as_str() {
        return Some(s.to_owned());
    }
    // "author": {"name": "Name"}
    if let Some(s) = author["name"].as_str() {
        return Some(s.to_owned());
    }
    // "author": [{"name": "Name"}, ...]
    if let Some(arr) = author.as_array() {
        return arr
            .first()
            .and_then(|a| a["name"].as_str())
            .map(|s| s.to_owned());
    }
    None
}

/// ISO 8601 日付文字列を DateTime<Utc> に変換する。
/// "2026-05-19" と "2026-05-19T10:00:00Z" の両形式に対応する。
fn parse_iso_date(s: &str) -> Option<DateTime<Utc>> {
    if let Ok(dt) = DateTime::parse_from_rfc3339(s) {
        return Some(dt.with_timezone(&Utc));
    }
    // 日付のみ (YYYY-MM-DD) は UTC 00:00 として扱う
    if let Ok(nd) = chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d") {
        return nd.and_hms_opt(0, 0, 0).map(|ndt| ndt.and_utc());
    }
    None
}

/// `duckduckgo.com/l/?uddg=<encoded_url>` のリダイレクト URL を実際の URL に解決する。
/// DDG 以外の URL はそのまま返す。
fn resolve_ddg_redirect(url: Url) -> Url {
    if url.host_str() != Some("duckduckgo.com") || url.path() != "/l/" {
        return url;
    }
    url.query_pairs()
        .find(|(k, _)| k == "uddg")
        .and_then(|(_, v)| v.parse::<Url>().ok())
        .unwrap_or(url)
}

#[cfg(test)]
mod tests {
    use super::*;
    use html5ever::parse_document;
    use html5ever::tendril::TendrilSink;
    use markup5ever_rcdom::RcDom;

    fn parse_html(html: &str) -> RcDom {
        parse_document(RcDom::default(), Default::default())
            .from_utf8()
            .read_from(&mut html.as_bytes())
            .unwrap()
    }

    #[test]
    fn test_json_ld_article() {
        let html = r#"<!DOCTYPE html><html><head>
<script type="application/ld+json">
{
  "@context": "https://schema.org",
  "@type": "Article",
  "headline": "テスト記事",
  "author": {"@type": "Person", "name": "山田太郎"},
  "datePublished": "2026-05-19T10:00:00Z",
  "dateModified": "2026-05-20T08:00:00Z",
  "description": "これはテスト記事の説明です"
}
</script>
</head><body><p>本文テキスト</p></body></html>"#;

        let dom = parse_html(html);
        let base_url = Url::parse("https://example.com/article").unwrap();
        let extractor = ReadabilityExtractor {
            preserve_links: true,
        };
        let content = extractor.extract(&dom.document, &base_url);

        assert_eq!(content.metadata.author.as_deref(), Some("山田太郎"));
        assert!(content.metadata.published_at.is_some());
        assert!(content.metadata.date_modified.is_some());
        assert_eq!(
            content.metadata.description.as_deref(),
            Some("これはテスト記事の説明です")
        );

        let published = content.metadata.published_at.unwrap();
        assert_eq!(published.format("%Y-%m-%d").to_string(), "2026-05-19");

        let modified = content.metadata.date_modified.unwrap();
        assert_eq!(modified.format("%Y-%m-%d").to_string(), "2026-05-20");
    }

    #[test]
    fn test_json_ld_author_string() {
        let html = r#"<!DOCTYPE html><html><head>
<script type="application/ld+json">
{"@type": "Article", "author": "鈴木花子", "datePublished": "2026-01-01"}
</script>
</head><body><p>本文</p></body></html>"#;

        let dom = parse_html(html);
        let base_url = Url::parse("https://example.com/").unwrap();
        let extractor = ReadabilityExtractor {
            preserve_links: false,
        };
        let content = extractor.extract(&dom.document, &base_url);

        assert_eq!(content.metadata.author.as_deref(), Some("鈴木花子"));
        assert_eq!(
            content
                .metadata
                .published_at
                .unwrap()
                .format("%Y-%m-%d")
                .to_string(),
            "2026-01-01"
        );
    }

    #[test]
    fn test_json_ld_author_array() {
        let html = r#"<!DOCTYPE html><html><head>
<script type="application/ld+json">
{"@type": "Article", "author": [{"name": "著者A"}, {"name": "著者B"}]}
</script>
</head><body><p>本文</p></body></html>"#;

        let dom = parse_html(html);
        let base_url = Url::parse("https://example.com/").unwrap();
        let extractor = ReadabilityExtractor {
            preserve_links: false,
        };
        let content = extractor.extract(&dom.document, &base_url);

        // 配列の場合は最初の著者を採用
        assert_eq!(content.metadata.author.as_deref(), Some("著者A"));
    }

    #[test]
    #[ignore = "requires /tmp/rust_wiki.html (run: curl -s https://en.wikipedia.org/wiki/Rust_\\(programming_language\\) > /tmp/rust_wiki.html)"]
    fn test_json_ld_wikipedia_real_html() {
        let html =
            std::fs::read_to_string("/tmp/rust_wiki.html").expect("need /tmp/rust_wiki.html");
        let dom = parse_html(&html);
        let base_url =
            Url::parse("https://en.wikipedia.org/wiki/Rust_(programming_language)").unwrap();
        let extractor = ReadabilityExtractor {
            preserve_links: false,
        };
        let content = extractor.extract(&dom.document, &base_url);

        let meta = &content.metadata;
        println!("author       = {:?}", meta.author);
        println!("published_at = {:?}", meta.published_at);
        println!("date_modified= {:?}", meta.date_modified);
        println!("description  = {:?}", meta.description);
        println!("og_title     = {:?}", meta.og_title);

        assert!(meta.author.is_some(), "author が取れていない");
        assert!(meta.published_at.is_some(), "datePublished が取れていない");
        assert!(meta.date_modified.is_some(), "dateModified が取れていない");
    }

    #[test]
    fn test_json_ld_does_not_override_ogp() {
        // OGP の description が先に設定された場合、JSON-LD で上書きしない
        let html = r#"<!DOCTYPE html><html><head>
<meta property="og:description" content="OGPの説明">
<script type="application/ld+json">
{"@type": "Article", "description": "JSON-LDの説明"}
</script>
</head><body><p>本文</p></body></html>"#;

        let dom = parse_html(html);
        let base_url = Url::parse("https://example.com/").unwrap();
        let extractor = ReadabilityExtractor {
            preserve_links: false,
        };
        let content = extractor.extract(&dom.document, &base_url);

        assert_eq!(content.metadata.description.as_deref(), Some("OGPの説明"));
    }

    // -----------------------------------------------------------------------
    // is_noise / class_contains_pattern 回帰テスト
    // -----------------------------------------------------------------------

    /// class_contains_pattern の直接テスト。
    /// コンポーネント単位の完全一致で、部分文字列一致しないことを確認する。
    #[test]
    fn test_class_contains_pattern_component_matching() {
        // 回帰: Tailwind "shadow-2xs" が "ad" にヒットしてはならない（dev.classmethod.jp 障害）
        assert!(!class_contains_pattern("shadow-2xs", "ad"));
        assert!(!class_contains_pattern("header-image-container", "ad"));

        // コンポーネント完全一致はヒットする
        assert!(class_contains_pattern("ad", "ad"));
        assert!(class_contains_pattern("ad-banner sidebar", "ad"));
        assert!(class_contains_pattern("p-sidebar-nav", "nav"));

        // Tailwind レスポンシブプレフィックスは除去して判定
        assert!(class_contains_pattern("md:ad-banner", "ad"));
        assert!(!class_contains_pattern("md:shadow-2xs", "ad"));

        // 空文字列・無関係トークン
        assert!(!class_contains_pattern("", "ad"));
        assert!(!class_contains_pattern("card badge loading", "ad"));
    }

    /// ノード 1 つのノイズ判定。祖先情報が要るため、そのノードを含む文書全体で DomIndex を作る
    fn is_noise(handle: &Handle) -> bool {
        let root = ancestor_chain(handle)[0].clone();
        DomIndex::build(&root).is_noise(handle)
    }

    /// is_noise のタグ/クラス判定。shadow-2xs クラスのdivがノイズ扱いされないこと。
    #[test]
    fn test_is_noise_regression_shadow_2xs() {
        // 回帰: shadow-2xs が "ad" に誤ヒットして記事ラッパー全体が
        // スキップされ、本文が13行しか取れなかった障害の再現防止
        let html = r#"<html><body><div class="shadow-2xs bg-white rounded"><p>本文テキスト</p></div></body></html>"#;
        let dom = parse_html(html);
        let div = find_tag(&dom.document, "div").expect("div exists");
        assert!(!is_noise(&div), "shadow-2xs はノイズではない");
    }

    /// ノイズタグ（nav/aside等）とノイスクラスの検出を確認する。
    #[test]
    fn test_is_noise_tags_and_classes() {
        let html = r#"<html><body>
<nav>メニュー</nav>
<aside class="sidebar">関連</aside>
<div id="ad-container">広告</div>
<main><p>本文</p></main>
</body></html>"#;
        let dom = parse_html(html);

        let nav = find_tag(&dom.document, "nav").unwrap();
        assert!(is_noise(&nav), "nav タグはノイズ");

        let aside = find_tag(&dom.document, "aside").unwrap();
        assert!(is_noise(&aside), "sidebar クラスはノイズ");

        let ad_div = find_tag(&dom.document, "div").unwrap();
        assert!(
            is_noise(&ad_div),
            "id=ad-container はノイズ（ad コンポーネント一致）"
        );

        let main = find_tag(&dom.document, "main").unwrap();
        assert!(!is_noise(&main), "main はノイズではない");
    }

    /// id="advertisement" は "ad" にヒットしない（コンポーネント完全一致の設計意図）。
    /// 部分文字列一致に戻すと shadow-2xs→ad のような誤検出が再発するため、
    /// この挙動を回帰テストとして固定する。
    #[test]
    fn test_advertisement_id_is_not_noise_by_design() {
        let html = r#"<html><body><div id="advertisement"><p>本文テキスト</p></div></body></html>"#;
        let dom = parse_html(html);
        let div = find_tag(&dom.document, "div").unwrap();
        assert!(
            !is_noise(&div),
            "advertisement は ad コンポーネントに完全一致しないためノイズ扱いしない"
        );
    }

    /// shadow-2xs ラッパー内の本文が抽出されること（エンドツーエンド回帰）。
    #[test]
    fn test_extract_content_inside_shadow_wrapper() {
        let article =
            "これは記事の本文です。十分な長さを持たせて抽出対象になるようにしています。".repeat(3);
        let html = format!(
            r#"<!DOCTYPE html><html><head><title>テスト記事</title></head><body>
<div class="shadow-2xs"><article><h1>記事タイトル</h1><p>{}</p></article></div>
</body></html>"#,
            article
        );
        let dom = parse_html(&html);
        let base_url = Url::parse("https://dev.example.jp/articles/1").unwrap();
        let extractor = ReadabilityExtractor {
            preserve_links: true,
        };
        let content = extractor.extract(&dom.document, &base_url);

        assert!(
            content.body_text.contains("これは記事の本文です"),
            "shadow-2xs ラッパー内の本文が抽出されるべき: got {:?}",
            // バイト位置で切ると日本語の文字境界で panic するため文字数で切る
            content.body_text.chars().take(100).collect::<String>()
        );
    }

    fn extract_html(html: &str) -> ExtractedContent {
        let dom = parse_html(html);
        let base_url = Url::parse("https://example.com/blog/").unwrap();
        ReadabilityExtractor {
            preserve_links: true,
        }
        .extract(&dom.document, &base_url)
    }

    /// WordPress Twenty Seventeen は body に has-sidebar を付ける。body ごとノイズ扱いしてはならない
    #[test]
    fn test_body_with_sidebar_class_is_not_noise() {
        let html = format!(
            r#"<html><body class="page-template-default page has-sidebar">
<div id="content" class="site-content"><div id="primary" class="content-area"><p>{}</p></div></div>
</body></html>"#,
            "本文の段落です。".repeat(20)
        );
        let content = extract_html(&html);
        assert!(content.body_text.contains("本文の段落です"), "{content:?}");
        assert!(content.body_html.contains("本文の段落です"));
    }

    #[test]
    fn test_article_header_title_is_kept() {
        let html = format!(
            r#"<html><body><header class="site-header">サイト名</header>
<article><header class="entry-header"><h1 class="entry-title">記事タイトル</h1></header>
<div class="entry-content"><p>{}</p></div></article></body></html>"#,
            "本文です。".repeat(30)
        );
        let content = extract_html(&html);
        assert!(
            content.body_html.contains("記事タイトル"),
            "{}",
            content.body_html
        );
        assert!(
            !content.body_text.contains("サイト名"),
            "{}",
            content.body_text
        );
    }

    /// 一覧ページでは article が並ぶ。2 件目以降も本文に含める
    #[test]
    fn test_multiple_articles_are_all_extracted() {
        let html = format!(
            r#"<html><body class="blog has-sidebar"><div class="posts with-sidebar">
<article><h2>一件目</h2><p>{a}</p></article>
<article><h2>二件目</h2><p>{a}</p></article>
<article><h2>三件目</h2><p>{a}</p></article>
</div><aside class="sidebar">サイドバー</aside></body></html>"#,
            a = "記事の要約です。".repeat(10)
        );
        let content = extract_html(&html);
        for t in ["一件目", "二件目", "三件目"] {
            assert!(content.body_text.contains(t), "{t}: {}", content.body_text);
        }
        assert!(
            !content.body_text.contains("サイドバー"),
            "{}",
            content.body_text
        );
    }

    #[test]
    fn test_whitespace_between_inline_elements_is_kept() {
        let content = extract_html("<html><body><p><b>foo</b> <i>bar</i></p></body></html>");
        assert!(
            content.body_text.contains("foo bar"),
            "{:?}",
            content.body_text
        );
        assert!(
            content.body_html.contains("foo bar"),
            "{:?}",
            content.body_html
        );
    }

    #[test]
    fn test_body_text_separates_blocks() {
        let content = extract_html(
            "<html><body><div>alpha</div><div>beta</div><table><tr><td>c1</td><td>c2</td></tr></table></body></html>",
        );
        assert!(
            !content.body_text.contains("alphabeta"),
            "{:?}",
            content.body_text
        );
        assert!(
            !content.body_text.contains("c1c2"),
            "{:?}",
            content.body_text
        );
    }

    #[test]
    fn test_pre_whitespace_is_preserved() {
        let content = extract_html("<html><body><pre><b>a</b>\n    <b>b</b></pre></body></html>");
        assert!(
            content.body_html.contains("a\n    b"),
            "{:?}",
            content.body_html
        );
    }

    #[test]
    fn test_unsafe_scheme_links_are_dropped() {
        let content = extract_html(
            r#"<html><body><p><a href="javascript:alert(1)">js</a> <a href="data:text/html,x">data</a> <a href="/ok">ok</a> <a href="mailto:a@example.com">mail</a></p></body></html>"#,
        );
        let hrefs: Vec<&str> = content.links.iter().map(|l| l.href.as_str()).collect();
        assert_eq!(hrefs, ["https://example.com/ok", "mailto:a@example.com"]);
        assert!(
            !content.body_html.contains("javascript:"),
            "{}",
            content.body_html
        );
        assert!(
            !content.body_html.contains("data:"),
            "{}",
            content.body_html
        );
    }

    /// リンク重複除去が O(n²) だと 4 万リンクで数秒かかる
    #[test]
    fn test_many_links_extract_quickly() {
        let n = 40_000;
        let mut html = String::from("<html><body><div>");
        for i in 0..n {
            html.push_str(&format!("<p><a href=\"/p/{i}\">link {i}</a></p>"));
        }
        html.push_str("</div></body></html>");
        let dom = parse_html(&html);
        let base_url = Url::parse("https://example.com/").unwrap();
        let ex = ReadabilityExtractor {
            preserve_links: true,
        };
        let start = std::time::Instant::now();
        let content = ex.extract(&dom.document, &base_url);
        let elapsed = start.elapsed();
        assert_eq!(content.links.len(), n);
        // 修正後は debug ビルドで 0.4 秒程度。O(n²) の旧実装は release ビルドでも約 2 秒かかった（debug ではさらに遅い）。
        // CI ランナーの性能差で誤検知しないよう、二乗オーダーへの退行だけを検出できる緩い閾値にしている
        assert!(elapsed.as_secs_f64() < 10.0, "{elapsed:?}");
    }

    /// div と p の深いネストで候補ごとに部分木を再走査すると、ノード数 × 深さの計算量になる
    #[test]
    fn test_deep_div_p_nesting_is_not_quadratic() {
        let depth = 400;
        let para = format!("<p>{}</p>", "<span>テキスト</span>".repeat(100));
        let mut html = String::from("<html><body>");
        for _ in 0..depth {
            html.push_str("<div>");
            html.push_str(&para);
        }
        for _ in 0..depth {
            html.push_str("</div>");
        }
        html.push_str("</body></html>");
        let dom = crate::extractor::parse_html(&html);
        let base_url = Url::parse("https://example.com/").unwrap();
        let ex = ReadabilityExtractor {
            preserve_links: false,
        };
        let start = std::time::Instant::now();
        let content = ex.extract(&dom.document, &base_url);
        let elapsed = start.elapsed();
        assert!(content.body_text.contains("テキスト"));
        // 閾値を緩くしている理由は test_many_links_extract_quickly と同じ
        assert!(elapsed.as_secs_f64() < 10.0, "{elapsed:?}");
    }
}
