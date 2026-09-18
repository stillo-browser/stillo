/// ページのセマンティック構造を表す中間表現。
/// body_html → html_to_ast がビルドし、renderer が消費する。
#[derive(Debug, Clone, Default)]
pub struct Document {
    pub blocks: Vec<Block>,
}

#[derive(Debug, Clone)]
pub enum Block {
    Heading {
        level: u8,
        inlines: Vec<Inline>,
    },
    Paragraph(Vec<Inline>),
    /// 深さ情報付きリストアイテム（フラット構造で深さをインデントで表現）
    ListItem {
        depth: usize,
        ordered: bool,
        number: usize,
        inlines: Vec<Inline>,
    },
    CodeBlock {
        lang: Option<String>,
        content: String,
    },
    Blockquote(Vec<Inline>),
    Rule,
}

#[derive(Debug, Clone)]
pub enum Inline {
    Text(String),
    Bold(String),
    Italic(String),
    BoldItalic(String),
    Code(String),
    Link { text: String, href: String },
    SoftBreak,
}
