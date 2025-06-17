use std::{
    cell::{Ref, RefCell},
    collections::VecDeque,
    fmt::Debug,
    rc::Rc,
};

use crossterm::event::KeyEvent;

pub trait Op: Debug {
    fn state(&self) -> OpState;

    fn set_state(&mut self, state: OpState);

    fn handle_key_events(&mut self, event: KeyEvent) -> color_eyre::Result<()>;
}

#[derive(Debug, Clone, Copy, Default)]
pub enum OpState {
    #[default]
    Sleep,
    Wake,
    Active,
}

#[derive(Debug)]
struct OpTreeNodeInner {
    data: Box<dyn Op>,
    parent: Option<OpTreeNode>,
    children: Vec<OpTreeNode>,
}

#[derive(Debug, Clone)]
pub struct OpTreeNode {
    inner: Rc<RefCell<OpTreeNodeInner>>,
}

impl PartialEq for OpTreeNode {
    fn eq(&self, other: &Self) -> bool {
        std::ptr::eq(&self.inner.borrow().data, &other.inner.borrow().data)
    }
}

impl Eq for OpTreeNode {}

impl OpTreeNode {
    pub fn new(data: Box<dyn Op>) -> OpTreeNode {
        let inner = OpTreeNodeInner {
            data,
            parent: None,
            children: Vec::new(),
        };
        OpTreeNode {
            inner: Rc::new(RefCell::new(inner)),
        }
    }

    pub fn attach(&self, parent: &OpTreeNode) {
        self.inner.borrow_mut().parent = Some(parent.clone());
        parent.inner.borrow_mut().children.push(self.clone());
    }

    pub fn children(&self) -> Ref<'_, Vec<OpTreeNode>> {
        Ref::map(self.inner.borrow(), |inner| &inner.children)
    }
}

/// An op tree that manages the status of each node.
#[derive(Debug)]
pub struct OpTree {
    root: OpTreeNode,
    current: OpTreeNode,
}

impl OpTree {
    pub fn new(root: OpTreeNode) -> Self {
        let current = root.clone();
        let this = Self { root, current };

        for node in this.iter() {
            node.inner.borrow_mut().data.set_state(OpState::Sleep);
        }
        this.wakeup();
        this
    }

    pub fn root(&self) -> &OpTreeNode {
        &self.root
    }

    pub fn current(&self) -> &OpTreeNode {
        &self.current
    }

    pub fn iter(&self) -> OpTreeIter {
        OpTreeIter::new(self, self.root.clone())
    }

    pub fn iter_from_current(&self) -> OpTreeIter {
        OpTreeIter::new(self, self.current.clone())
    }

    fn wakeup(&self) {
        self.current.inner.borrow_mut().data.set_state(OpState::Active);
        for node in self.current.children().iter() {
            node.inner.borrow_mut().data.set_state(OpState::Wake);
        }
    }
}

pub struct OpTreeIter<'a> {
    tree: &'a OpTree,
    queue: VecDeque<OpTreeNode>,
}

impl<'a> OpTreeIter<'a> {
    fn new(tree: &'a OpTree, start: OpTreeNode) -> Self {
        let mut queue = VecDeque::new();
        queue.push_back(start);
        Self { tree, queue }
    }
}

impl<'a> Iterator for OpTreeIter<'a> {
    type Item = OpTreeNode;

    fn next(&mut self) -> Option<Self::Item> {
        let current = self.queue.pop_front()?;
        for child in current.inner.borrow().children.iter() {
            self.queue.push_back(child.clone());
        }
        Some(current)
    }
}
