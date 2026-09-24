//! The indexes of a feature a build is made without (`Cargo.toml`), each a
//! type of no value with the fields and methods its real one has.
//!
//! None of them can be made -- `Collection` builds none, and the engine asks
//! [`crate::engine`]'s feature constants before it would -- so every method
//! is unreachable, and the compiler drops every line that would use one:
//! the graph, the text, sparse and ordered indexes, and the engine's paths
//! through them, out of a browser module that does not need them. A file
//! that declares one opens all the same, its documents read and written, and
//! a statement that needs the index is refused.

#![allow(
    dead_code,
    unused_variables,
    clippy::needless_lifetimes,
    clippy::new_without_default
)]

use crate::collate::Collation;
use crate::error::Result;
use crate::schema::{TextIndexSpec, VectorIndexSpec};
use crate::sorted::{Key, Range};
use crate::value::{DataType, DocId, Value, VecPrec};
use crate::vector::{GraphView, Persisted};

pub(crate) enum Never {}

/// What a constructor says when a path the engine should have guarded
/// reaches it.
fn absent(feature: &str) -> ! {
    unreachable!("this build was made without the `{feature}` feature")
}

pub struct VectorIndex {
    pub dim: usize,
    pub spec: VectorIndexSpec,
    never: Never,
}

impl VectorIndex {
    pub fn new(dim: usize, spec: VectorIndexSpec) -> VectorIndex {
        absent("vector")
    }
    pub fn with_precision(dim: usize, spec: VectorIndexSpec, prec: VecPrec) -> VectorIndex {
        absent("vector")
    }
    pub fn reserve(&mut self, n: usize) {
        match self.never {}
    }
    pub fn len(&self) -> usize {
        match self.never {}
    }
    pub fn is_empty(&self) -> bool {
        match self.never {}
    }
    pub fn dead(&self) -> usize {
        match self.never {}
    }
    pub fn arena_bytes(&self) -> usize {
        match self.never {}
    }
    pub fn graph_bytes(&self) -> usize {
        match self.never {}
    }
    pub fn unlinked(&self) -> usize {
        match self.never {}
    }
    pub fn changes(&self) -> u64 {
        match self.never {}
    }
    pub fn persisted(&self) -> &Persisted {
        match self.never {}
    }
    pub fn precision(&self) -> VecPrec {
        match self.never {}
    }
    pub fn quantized(&self) -> bool {
        match self.never {}
    }
    pub fn floor(&self, doc: DocId, score: f32, reach: f32) -> f32 {
        match self.never {}
    }
    pub(crate) fn view(&self) -> GraphView<'_> {
        match self.never {}
    }
    pub fn prepare_query(&self, q: &[f32]) -> Vec<f32> {
        match self.never {}
    }
    pub fn insert(&mut self, doc: DocId, raw: &[f32]) {
        match self.never {}
    }
    pub fn insert_batch(&mut self, items: &[(DocId, Vec<f32>)]) {
        match self.never {}
    }
    pub fn defer_batch(&mut self, items: &[(DocId, Vec<f32>)]) {
        match self.never {}
    }
    pub fn link_pending(
        &mut self,
        max: usize,
        lookup: &mut dyn FnMut(DocId, &mut Vec<f32>) -> bool,
    ) -> usize {
        match self.never {}
    }
    pub fn remove(&mut self, doc: DocId) {
        match self.never {}
    }
    pub fn search<F>(
        &self,
        query: &[f32],
        k: usize,
        ef: Option<usize>,
        accept: F,
    ) -> Vec<(DocId, f32)>
    where
        F: Fn(DocId) -> bool,
    {
        match self.never {}
    }
    pub fn serialize_graph(&self) -> Vec<u8> {
        match self.never {}
    }
    pub fn serialize_graph_kept(&self) -> Vec<u8> {
        match self.never {}
    }
    /// A graph in the file is derived data: without the index to restore it
    /// into, it is passed over, as a graph that does not validate is.
    pub fn restore_graph(
        bytes: &[u8],
        expect_dim: usize,
        expect_prec: VecPrec,
        lookup: impl FnMut(DocId, &mut Vec<f32>) -> bool,
    ) -> Option<VectorIndex> {
        None
    }
    pub fn search_ids(&self, query: &[f32], k: usize, ids: &[DocId]) -> Vec<(DocId, f32)> {
        match self.never {}
    }
    pub fn probe_budget(&self, ef: Option<usize>) -> usize {
        match self.never {}
    }
    pub fn search_exact<F>(&self, query: &[f32], k: usize, accept: F) -> Vec<(DocId, f32)>
    where
        F: Fn(DocId) -> bool,
    {
        match self.never {}
    }
}

pub struct TextIndex {
    pub spec: TextIndexSpec,
    never: Never,
}

impl TextIndex {
    pub fn new(spec: TextIndexSpec) -> TextIndex {
        absent("text")
    }
    pub fn len(&self) -> usize {
        match self.never {}
    }
    pub fn is_empty(&self) -> bool {
        match self.never {}
    }
    pub fn terms(&self) -> usize {
        match self.never {}
    }
    pub fn postings_count(&self) -> usize {
        match self.never {}
    }
    pub fn memory_bytes(&self) -> usize {
        match self.never {}
    }
    pub fn insert(&mut self, doc: DocId, text: &str) {
        match self.never {}
    }
    pub fn remove(&mut self, doc: DocId, text: &str) {
        match self.never {}
    }
    pub fn shrink_to_fit(&mut self) {
        match self.never {}
    }
    pub fn clear(&mut self) {
        match self.never {}
    }
    pub fn search<F>(&self, query: &str, k: usize, accept: F) -> Vec<(DocId, f32)>
    where
        F: Fn(DocId) -> bool,
    {
        match self.never {}
    }
}

pub struct SparseIndex {
    never: Never,
}

impl SparseIndex {
    pub fn new() -> SparseIndex {
        absent("sparse")
    }
    pub fn len(&self) -> usize {
        match self.never {}
    }
    pub fn is_empty(&self) -> bool {
        match self.never {}
    }
    pub fn dimensions(&self) -> usize {
        match self.never {}
    }
    pub fn postings_count(&self) -> usize {
        match self.never {}
    }
    pub fn memory_bytes(&self) -> usize {
        match self.never {}
    }
    pub fn insert(&mut self, doc: DocId, entries: &[(u32, f32)]) {
        match self.never {}
    }
    pub fn remove(&mut self, doc: DocId, entries: &[(u32, f32)]) {
        match self.never {}
    }
    pub fn shrink_to_fit(&mut self) {
        match self.never {}
    }
    pub fn clear(&mut self) {
        match self.never {}
    }
    pub fn search(
        &self,
        query: &[(u32, f32)],
        k: usize,
        accept: &dyn Fn(DocId) -> bool,
    ) -> Vec<(DocId, f32)> {
        match self.never {}
    }
}

pub enum SortedIndex {}

impl SortedIndex {
    /// The types the real index is for; `Collection` builds none of them
    /// here, and a `@sorted` field's comparisons and orders are the scan's,
    /// which are the same answers.
    pub fn supports(ty: &DataType) -> bool {
        crate::sorted::orderable(ty)
    }
    pub fn new(ty: &DataType, coll: Option<Collation>) -> SortedIndex {
        absent("sorted")
    }
    pub fn build(
        ty: &DataType,
        coll: Option<Collation>,
        rows: &mut dyn Iterator<Item = (DocId, Option<Value>)>,
    ) -> Self {
        absent("sorted")
    }
    pub fn insert(&mut self, id: DocId, v: Option<&Value>) {
        match *self {}
    }
    pub fn remove(&mut self, id: DocId, v: Option<&Value>) {
        match *self {}
    }
    pub fn has_nan(&self) -> bool {
        match *self {}
    }
    pub fn memory_bytes(&self) -> usize {
        match *self {}
    }
    pub fn bound(ty: &DataType, v: &Value) -> Option<Key> {
        None
    }
    pub fn range_ids(&self, r: &Range, cap: usize) -> Option<Vec<DocId>> {
        match *self {}
    }
    pub fn walk(
        &self,
        desc: bool,
        range: Option<&Range>,
        emit: impl FnMut(DocId) -> Result<bool>,
    ) -> Result<()> {
        match *self {}
    }
}
