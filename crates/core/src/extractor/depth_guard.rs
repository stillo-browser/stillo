//! 構築中の DOM の深さを追跡する TreeSink ラッパー。
//!
//! html5ever の tree builder は要素を挿入するたびに「開いている要素のスタック」を走査するため、
//! パース時間がネストの深さの二乗で増える（release ビルドで深さ 4 万・200KB の入力に約 4 秒）。
//! スタック操作の一部は sink を経由しないので、スタックの深さそのものは sink から観測できない。
//! 代わりに、新しい要素は原則として現在のノード（スタックの最上段）の子として追加されることを利用し、
//! 追加された要素の DOM 上の深さをスタックの深さの近似として記録する。

use std::borrow::Cow;
use std::cell::Cell;
use std::collections::HashMap;
use std::rc::Rc;

use html5ever::tendril::StrTendril;
use html5ever::tree_builder::{ElementFlags, NodeOrText, QuirksMode, TreeSink};
use html5ever::{Attribute, ExpandedName, QualName};
use markup5ever_rcdom::{Handle, Node, RcDom};

/// RcDom に処理を委譲しつつ、追加された要素の最大深さを `max_depth` に記録する。
pub(crate) struct DepthTrackingSink {
    inner: RcDom,
    /// ノードのアドレス → DOM 上の深さ。未登録のノード（document 等）は深さ 0 とみなす
    depths: HashMap<*const Node, usize>,
    /// パーサーに sink の所有権を渡した後も呼び出し側から読めるよう共有する
    max_depth: Rc<Cell<usize>>,
}

impl DepthTrackingSink {
    pub(crate) fn new(max_depth: Rc<Cell<usize>>) -> Self {
        Self {
            inner: RcDom::default(),
            depths: HashMap::new(),
            max_depth,
        }
    }

    fn depth_of(&self, node: &Handle) -> usize {
        self.depths.get(&Rc::as_ptr(node)).copied().unwrap_or(0)
    }

    fn record(&mut self, child: &NodeOrText<Handle>, depth: usize) {
        if let NodeOrText::AppendNode(node) = child {
            self.depths.insert(Rc::as_ptr(node), depth);
            if depth > self.max_depth.get() {
                self.max_depth.set(depth);
            }
        }
    }
}

impl TreeSink for DepthTrackingSink {
    type Handle = Handle;
    type Output = RcDom;

    fn finish(self) -> RcDom {
        self.inner
    }

    fn parse_error(&mut self, msg: Cow<'static, str>) {
        self.inner.parse_error(msg)
    }

    fn get_document(&mut self) -> Handle {
        self.inner.get_document()
    }

    /// <template> の中身は別の document fragment に追加される。深さを template 要素から引き継がないと、
    /// <template> を入れ子にするだけでスタックは深くなるのに DOM 上の深さが増えず、ガードをすり抜ける
    fn get_template_contents(&mut self, target: &Handle) -> Handle {
        let contents = self.inner.get_template_contents(target);
        let depth = self.depth_of(target);
        self.depths.insert(Rc::as_ptr(&contents), depth);
        contents
    }

    fn set_quirks_mode(&mut self, mode: QuirksMode) {
        self.inner.set_quirks_mode(mode)
    }

    fn same_node(&self, x: &Handle, y: &Handle) -> bool {
        self.inner.same_node(x, y)
    }

    fn elem_name<'a>(&'a self, target: &'a Handle) -> ExpandedName<'a> {
        self.inner.elem_name(target)
    }

    fn create_element(
        &mut self,
        name: QualName,
        attrs: Vec<Attribute>,
        flags: ElementFlags,
    ) -> Handle {
        self.inner.create_element(name, attrs, flags)
    }

    fn create_comment(&mut self, text: StrTendril) -> Handle {
        self.inner.create_comment(text)
    }

    fn create_pi(&mut self, target: StrTendril, data: StrTendril) -> Handle {
        self.inner.create_pi(target, data)
    }

    fn append(&mut self, parent: &Handle, child: NodeOrText<Handle>) {
        let depth = self.depth_of(parent) + 1;
        self.record(&child, depth);
        self.inner.append(parent, child)
    }

    fn append_before_sibling(&mut self, sibling: &Handle, child: NodeOrText<Handle>) {
        let depth = self.depth_of(sibling);
        self.record(&child, depth);
        self.inner.append_before_sibling(sibling, child)
    }

    /// RcDom の実装は element に親があれば element の兄弟、なければ prev_element の子として追加する。
    /// どちらになるかで深さが変わるが、ここでは深い方を採用する（過小評価するとガードをすり抜けるため）
    fn append_based_on_parent_node(
        &mut self,
        element: &Handle,
        prev_element: &Handle,
        child: NodeOrText<Handle>,
    ) {
        let depth = self.depth_of(element).max(self.depth_of(prev_element) + 1);
        self.record(&child, depth);
        self.inner
            .append_based_on_parent_node(element, prev_element, child)
    }

    fn append_doctype_to_document(
        &mut self,
        name: StrTendril,
        public_id: StrTendril,
        system_id: StrTendril,
    ) {
        self.inner
            .append_doctype_to_document(name, public_id, system_id)
    }

    fn add_attrs_if_missing(&mut self, target: &Handle, attrs: Vec<Attribute>) {
        self.inner.add_attrs_if_missing(target, attrs)
    }

    fn remove_from_parent(&mut self, target: &Handle) {
        self.inner.remove_from_parent(target)
    }

    fn reparent_children(&mut self, node: &Handle, new_parent: &Handle) {
        self.inner.reparent_children(node, new_parent)
    }

    fn is_mathml_annotation_xml_integration_point(&self, handle: &Handle) -> bool {
        self.inner
            .is_mathml_annotation_xml_integration_point(handle)
    }
}
