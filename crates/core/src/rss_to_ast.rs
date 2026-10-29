use crate::{
    ast::{Block, Document, Inline},
    document::{BrowsePage, ExtractedLink},
};
use url::Url;

/// RSS 2.0 / RSS 1.0 (RDF) / Atom 1.0 XML を BrowsePage に変換する。
/// 判別不能な XML の場合は None を返す。
pub fn parse_rss_to_ast(xml: &str, base_url: &Url) -> Option<BrowsePage> {
    // RSS 0.91 は DOCTYPE 宣言付きで配信されることが多く、roxmltree の既定（DTD 拒否）では失敗する。
    // roxmltree は外部 DTD を取得せず、エンティティ展開の再帰・回数も制限しているので許可してよい
    let options = roxmltree::ParsingOptions {
        allow_dtd: true,
        ..Default::default()
    };
    let xml_doc = roxmltree::Document::parse_with_options(xml, options).ok()?;
    let root = xml_doc.root_element();
    match root.tag_name().name() {
        "rss" => parse_rss2(root, base_url),
        "RDF" => parse_rss1(root, base_url),
        "feed" => parse_atom(root, base_url),
        _ => None,
    }
}

/// RSS 2.0 フィードを BrowsePage に変換する。
fn parse_rss2(root: roxmltree::Node, base_url: &Url) -> Option<BrowsePage> {
    let channel = root.children().find(|n| n.tag_name().name() == "channel")?;

    let feed_title = child_text(&channel, "title").unwrap_or_else(|| "RSS Feed".to_string());

    let mut blocks: Vec<Block> = Vec::new();
    let mut links: Vec<ExtractedLink> = Vec::new();
    let mut markdown_parts: Vec<String> = Vec::new();

    // フィードタイトルを H1 として追加する
    blocks.push(Block::Heading {
        level: 1,
        inlines: vec![Inline::Text(feed_title.clone())],
    });
    markdown_parts.push(format!("# {}\n", feed_title));

    for item in channel.children().filter(|n| n.tag_name().name() == "item") {
        let title = child_text(&item, "title").unwrap_or_default();
        let link_str = child_text(&item, "link");
        let pub_date = child_text(&item, "pubDate").unwrap_or_default();
        let author = child_text(&item, "author").unwrap_or_default();
        let description = child_text(&item, "description")
            .map(|d| strip_html_tags(&d))
            .unwrap_or_default();

        // タイトルをリンク付き見出しか、プレーンテキスト見出しとして追加する
        let title_inlines = if let Some(ref href_str) = link_str {
            if let Ok(href) = base_url.join(href_str) {
                links.push(ExtractedLink {
                    text: title.clone(),
                    href: href.clone(),
                    rel: None,
                });
                vec![Inline::Link {
                    text: title.clone(),
                    href: href.to_string(),
                }]
            } else {
                vec![Inline::Text(title.clone())]
            }
        } else {
            vec![Inline::Text(title.clone())]
        };
        blocks.push(Block::Heading {
            level: 2,
            inlines: title_inlines,
        });

        // 日付・著者メタ情報を段落として追加する
        let meta = if !pub_date.is_empty() && !author.is_empty() {
            format!("{} · {}", pub_date, author)
        } else if !pub_date.is_empty() {
            pub_date.clone()
        } else if !author.is_empty() {
            author.clone()
        } else {
            String::new()
        };
        if !meta.is_empty() {
            blocks.push(Block::Paragraph(vec![Inline::Text(meta.clone())]));
        }

        // 概要を段落として追加する
        if !description.is_empty() {
            blocks.push(Block::Paragraph(vec![Inline::Text(description.clone())]));
        }

        blocks.push(Block::Rule);

        // markdown dump 用テキストを構築する
        let md_link = link_str.as_deref().unwrap_or("");
        if md_link.is_empty() {
            markdown_parts.push(format!("## {}\n", title));
        } else {
            markdown_parts.push(format!("## [{}]({})\n", title, md_link));
        }
        if !meta.is_empty() {
            markdown_parts.push(format!("\n{}\n", meta));
        }
        if !description.is_empty() {
            markdown_parts.push(format!("\n{}\n", description));
        }
        markdown_parts.push("\n---\n".to_string());
    }

    Some(BrowsePage {
        title: feed_title,
        url: base_url.clone(),
        doc: Document { blocks },
        links,
        markdown: markdown_parts.join("\n"),
    })
}

/// RSS 1.0 (RDF形式) フィードを BrowsePage に変換する。
/// RSS 2.0 と異なり <item> は <channel> の子ではなく <rdf:RDF> 直下の兄弟要素として並ぶ。
fn parse_rss1(root: roxmltree::Node, base_url: &Url) -> Option<BrowsePage> {
    let channel = root.children().find(|n| n.tag_name().name() == "channel")?;
    let feed_title = child_text(&channel, "title").unwrap_or_else(|| "RSS Feed".to_string());

    let mut blocks: Vec<Block> = Vec::new();
    let mut links: Vec<ExtractedLink> = Vec::new();
    let mut markdown_parts: Vec<String> = Vec::new();

    blocks.push(Block::Heading {
        level: 1,
        inlines: vec![Inline::Text(feed_title.clone())],
    });
    markdown_parts.push(format!("# {}\n", feed_title));

    for item in root.children().filter(|n| n.tag_name().name() == "item") {
        let title = child_text(&item, "title").unwrap_or_default();

        // <link> 子要素を優先し、なければ rdf:about 属性をフォールバックとして使う
        let link_str = child_text(&item, "link").or_else(|| {
            item.attribute(("http://www.w3.org/1999/02/22-rdf-syntax-ns#", "about"))
                .map(|s| s.to_string())
        });

        // Dublin Core の dc:date を取得する（名前空間に関わらずローカル名で照合できる）
        let pub_date = child_text(&item, "date").unwrap_or_default();

        let description = child_text(&item, "description")
            .map(|d| strip_html_tags(&d))
            .unwrap_or_default();

        let title_inlines = if let Some(ref href_str) = link_str {
            if let Ok(href) = base_url.join(href_str) {
                links.push(ExtractedLink {
                    text: title.clone(),
                    href: href.clone(),
                    rel: None,
                });
                vec![Inline::Link {
                    text: title.clone(),
                    href: href.to_string(),
                }]
            } else {
                vec![Inline::Text(title.clone())]
            }
        } else {
            vec![Inline::Text(title.clone())]
        };
        blocks.push(Block::Heading {
            level: 2,
            inlines: title_inlines,
        });

        if !pub_date.is_empty() {
            blocks.push(Block::Paragraph(vec![Inline::Text(pub_date.clone())]));
        }
        if !description.is_empty() {
            blocks.push(Block::Paragraph(vec![Inline::Text(description.clone())]));
        }
        blocks.push(Block::Rule);

        let md_link = link_str.as_deref().unwrap_or("");
        if md_link.is_empty() {
            markdown_parts.push(format!("## {}\n", title));
        } else {
            markdown_parts.push(format!("## [{}]({})\n", title, md_link));
        }
        if !pub_date.is_empty() {
            markdown_parts.push(format!("\n{}\n", pub_date));
        }
        if !description.is_empty() {
            markdown_parts.push(format!("\n{}\n", description));
        }
        markdown_parts.push("\n---\n".to_string());
    }

    Some(BrowsePage {
        title: feed_title,
        url: base_url.clone(),
        doc: Document { blocks },
        links,
        markdown: markdown_parts.join("\n"),
    })
}

/// Atom 1.0 フィードを BrowsePage に変換する。
fn parse_atom(root: roxmltree::Node, base_url: &Url) -> Option<BrowsePage> {
    // Atom ではルート要素が <feed> なので直接子要素を参照する
    let feed_title = child_text(&root, "title").unwrap_or_else(|| "Atom Feed".to_string());

    let mut blocks: Vec<Block> = Vec::new();
    let mut links: Vec<ExtractedLink> = Vec::new();
    let mut markdown_parts: Vec<String> = Vec::new();

    blocks.push(Block::Heading {
        level: 1,
        inlines: vec![Inline::Text(feed_title.clone())],
    });
    markdown_parts.push(format!("# {}\n", feed_title));

    for entry in root.children().filter(|n| n.tag_name().name() == "entry") {
        let title = child_text(&entry, "title").unwrap_or_default();

        let href_str = atom_entry_link(&entry);

        let published = child_text(&entry, "published")
            .or_else(|| child_text(&entry, "updated"))
            .unwrap_or_default();

        // <author><name> を取得する
        let author = entry
            .children()
            .find(|n| n.tag_name().name() == "author")
            .and_then(|a| child_text(&a, "name"))
            .unwrap_or_default();

        let summary = child_text(&entry, "summary")
            .or_else(|| child_text(&entry, "content"))
            .map(|s| strip_html_tags(&s))
            .unwrap_or_default();

        let title_inlines = if let Some(ref href_str) = href_str {
            if let Ok(href) = base_url.join(href_str) {
                links.push(ExtractedLink {
                    text: title.clone(),
                    href: href.clone(),
                    rel: None,
                });
                vec![Inline::Link {
                    text: title.clone(),
                    href: href.to_string(),
                }]
            } else {
                vec![Inline::Text(title.clone())]
            }
        } else {
            vec![Inline::Text(title.clone())]
        };
        blocks.push(Block::Heading {
            level: 2,
            inlines: title_inlines,
        });

        let meta = if !published.is_empty() && !author.is_empty() {
            format!("{} · {}", published, author)
        } else if !published.is_empty() {
            published.clone()
        } else if !author.is_empty() {
            author.clone()
        } else {
            String::new()
        };
        if !meta.is_empty() {
            blocks.push(Block::Paragraph(vec![Inline::Text(meta.clone())]));
        }

        if !summary.is_empty() {
            blocks.push(Block::Paragraph(vec![Inline::Text(summary.clone())]));
        }

        blocks.push(Block::Rule);

        let md_link = href_str.as_deref().unwrap_or("");
        if md_link.is_empty() {
            markdown_parts.push(format!("## {}\n", title));
        } else {
            markdown_parts.push(format!("## [{}]({})\n", title, md_link));
        }
        if !meta.is_empty() {
            markdown_parts.push(format!("\n{}\n", meta));
        }
        if !summary.is_empty() {
            markdown_parts.push(format!("\n{}\n", summary));
        }
        markdown_parts.push("\n---\n".to_string());
    }

    Some(BrowsePage {
        title: feed_title,
        url: base_url.clone(),
        doc: Document { blocks },
        links,
        markdown: markdown_parts.join("\n"),
    })
}

/// 指定した名前の直接子要素のテキストコンテンツを返す。
fn child_text(node: &roxmltree::Node, tag: &str) -> Option<String> {
    node.children()
        .find(|n| n.tag_name().name() == tag)
        .map(|n| n.text().unwrap_or("").trim().to_string())
        .filter(|s| !s.is_empty())
}

/// Atom の entry から記事本体へのリンクを選ぶ。
///
/// rel 省略は alternate と同義（RFC 4287 4.2.7.2）。Blogger は rel="replies"（コメントフィード）を
/// alternate より前に置くため、先頭の link を採るとコメントフィードを指してしまう。
/// alternate がなければ、href を持つ最初の link にフォールバックする。
fn atom_entry_link(entry: &roxmltree::Node) -> Option<String> {
    let links: Vec<roxmltree::Node> = entry
        .children()
        .filter(|n| n.tag_name().name() == "link" && n.attribute("href").is_some())
        .collect();
    links
        .iter()
        .find(|n| n.attribute("rel").is_none_or(|r| r.trim() == "alternate"))
        .or(links.first())
        .and_then(|n| n.attribute("href"))
        .map(|s| s.to_string())
}

/// description / summary に混入する HTML をプレーンテキストにする。
///
/// 文字単位で `<`〜`>` を除去する方式では、`s &amp; t < u` のようなタグでない `<` 以降を飲み込み、
/// エンティティもデコードされない。HTML パーサーでテキスト化してブラウザと同じ解釈にする。
fn strip_html_tags(html: &str) -> String {
    let dom = crate::extractor::parse_html(html);
    let mut text = String::new();
    collect_visible_text(&dom.document, &mut text);
    crate::html_to_ast::collapse_whitespace(&text)
        .trim()
        .to_string()
}

fn collect_visible_text(handle: &markup5ever_rcdom::Handle, out: &mut String) {
    use markup5ever_rcdom::NodeData;
    match &handle.data {
        NodeData::Text { contents } => out.push_str(&contents.borrow()),
        NodeData::Element { name, .. } => {
            let tag = name.local.as_ref();
            if matches!(tag, "script" | "style" | "template") {
                return;
            }
            // ブロック要素の境界で語が連結しないよう空白を挟む（後で 1 つに畳む）
            let block = matches!(tag, "br" | "p" | "li" | "tr" | "td" | "th")
                || crate::html_to_ast::is_generic_block(tag);
            if block {
                out.push(' ');
            }
            for child in handle.children.borrow().iter() {
                collect_visible_text(child, out);
            }
            if block {
                out.push(' ');
            }
        }
        _ => {
            for child in handle.children.borrow().iter() {
                collect_visible_text(child, out);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> Url {
        Url::parse("https://blog.example.com/feeds/posts/default").unwrap()
    }

    #[test]
    fn test_atom_prefers_alternate_link() {
        // Blogger は rel="replies" を rel="alternate" より前に置く
        let xml = r#"<?xml version="1.0"?>
<feed xmlns="http://www.w3.org/2005/Atom"><title>Blog</title>
<entry><title>Post</title>
<link rel="replies" type="application/atom+xml" href="https://blog.example.com/feeds/1/comments"/>
<link rel="edit" href="https://blog.example.com/edit/1"/>
<link rel="alternate" type="text/html" href="https://blog.example.com/2026/01/post.html"/>
</entry>
<entry><title>NoRel</title>
<link rel="replies" href="https://blog.example.com/feeds/2/comments"/>
<link href="https://blog.example.com/2026/02/norel.html"/>
</entry></feed>"#;
        let page = parse_rss_to_ast(xml, &base()).unwrap();
        let hrefs: Vec<&str> = page.links.iter().map(|l| l.href.as_str()).collect();
        assert_eq!(
            hrefs,
            [
                "https://blog.example.com/2026/01/post.html",
                "https://blog.example.com/2026/02/norel.html"
            ]
        );
    }

    #[test]
    fn test_rss091_with_doctype_parses() {
        let xml = r#"<?xml version="1.0"?>
<!DOCTYPE rss PUBLIC "-//Netscape Communications//DTD RSS 0.91//EN" "http://my.netscape.com/publish/formats/rss-0.91.dtd">
<rss version="0.91"><channel><title>Old Feed</title>
<item><title>Item</title><link>https://example.com/item</link></item>
</channel></rss>"#;
        let page = parse_rss_to_ast(xml, &base()).expect("DOCTYPE 付き RSS 0.91 をパースできる");
        assert_eq!(page.title, "Old Feed");
        assert_eq!(page.links.len(), 1);
    }

    #[test]
    fn test_strip_html_tags_handles_bare_lt_and_entities() {
        assert_eq!(strip_html_tags("s &amp; t < u"), "s & t < u");
        assert_eq!(
            strip_html_tags("<p>a&nbsp;b</p><p>c</p><script>x()</script>"),
            "a\u{a0}b c"
        );
    }
}
