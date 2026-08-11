//! Derives a collapsible directory tree from [`DiffFile`]'s changed-file
//! list (issue #15) — a pure, `App`-free module the same way `diff::model`
//! keeps parsing separate from `App`'s cursor/scroll state: [`build`] turns
//! `App::files` into a [`FileTree`], [`flatten_visible`] turns that plus a
//! set of collapsed directory paths into the flat, indexable row sequence
//! [`crate::ui::sidebar`] renders (mirroring `diff::model::flatten`'s own
//! "parse once, flatten for display" split), and [`resolve_selection`]
//! re-anchors a remembered selection against a freshly rebuilt tree the same
//! way `refresh::restore_anchor` re-anchors the diff cursor.
//!
//! Every node's stable identity is a [`NodeId`] — a repo-relative path *and*
//! whether it's a directory — rather than a path alone, because a changed
//! file and a changed directory can legitimately share one path string (a
//! file `src/foo` deleted in the same diff that adds `src/foo/bar.rs`): the
//! tree keeps both as separate sibling nodes rather than picking one or
//! silently dropping the other (issue #15's acceptance criterion), and
//! `is_directory` is what a lookup needs to tell them apart.

use crate::diff::DiffFile;
use std::collections::{HashMap, HashSet};

/// A node's stable, rebuild-independent identity. `path` alone is not
/// enough — see this module's docs on the path-conflict case a directory
/// and a file can share — so every lookup (`==`, a `HashSet`/`HashMap` key)
/// compares both fields together.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct NodeId {
    pub path: String,
    pub is_directory: bool,
}

/// One node's payload: either it has children (a directory, real or
/// implied by a changed file nested beneath it), or it *is* a changed file,
/// carrying the index into `App::files`/the slice [`build`] was called with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeKind {
    Directory { children: Vec<Node> },
    File { file_idx: usize },
}

/// One node in a [`FileTree`] — a directory implied by a changed file's
/// path, or a changed file itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    /// Full repo-relative path from the tree's root to this node —
    /// [`NodeId::path`]'s source of truth, and what [`resolve_selection`]'s
    /// ancestor walk splits on.
    pub path: String,
    /// This node's own path component (the last `/`-delimited segment of
    /// `path`) — what [`crate::ui::sidebar`] actually renders; kept
    /// alongside the full `path` so rendering never has to re-split it.
    pub label: String,
    /// Nesting depth from the tree's roots (`0` for a root-level node),
    /// for indentation.
    pub depth: usize,
    pub kind: NodeKind,
}

/// The derived tree — a forest, not a single root, since a diff's changed
/// files can (and usually do) span more than one top-level directory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileTree {
    pub roots: Vec<Node>,
}

/// One [`VisibleRow`]'s payload — [`Node`]'s counterpart after collapse
/// state has been applied, carrying only what [`crate::ui::sidebar`] needs
/// to draw a row rather than a whole subtree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VisibleKind {
    Directory {
        expanded: bool,
        /// Changed *files* nested anywhere beneath this directory, counted
        /// regardless of its own collapsed state — req 6: a directory row
        /// shows how many changed files it contains, never a summed
        /// added/deleted line count (that would misrepresent a whole
        /// subtree's diff as if it were one file's).
        descendant_files: usize,
    },
    File {
        file_idx: usize,
    },
}

/// One row [`crate::ui::sidebar::render`] draws: a node's identity, depth,
/// label, and kind, already resolved against the current collapse state —
/// the sidebar never touches a [`Node`]/[`FileTree`] directly, only this
/// flattened, indexable sequence (mirroring `diff::model::RenderRow`'s own
/// relationship to `DiffFile`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisibleRow {
    pub id: NodeId,
    pub depth: usize,
    pub label: String,
    pub kind: VisibleKind,
}

/// Builds a [`FileTree`] from `files`, in [`DiffFile::display_path`] order
/// exploded on `/`. Grouped, not sorted, by construction: every changed
/// file whose path shares a leading directory component becomes a child of
/// the same [`Node::Directory`] regardless of the order `files` lists them
/// in, and [`sort_siblings`] then orders each level's children directories-
/// before-files, lexicographically by raw (untruncated) component — req 3.
pub fn build(files: &[DiffFile]) -> FileTree {
    let items: Vec<(Vec<String>, usize)> = files
        .iter()
        .enumerate()
        .map(|(idx, file)| {
            let components = file.display_path().split('/').map(str::to_owned).collect();
            (components, idx)
        })
        .collect();
    FileTree {
        roots: build_level(items, "", 0),
    }
}

/// One level of [`build`]'s recursion: `items` are `(remaining path
/// components, file_idx)` pairs for everything still to place somewhere
/// under `prefix`. An item whose only remaining component is its own name
/// becomes a [`Node::File`] leaf right here; an item with more than one
/// remaining component contributes its first component as a directory name
/// and recurses one level deeper with that component stripped — which is
/// exactly how a path like `src/foo` (a changed file) and `src/foo/bar.rs`
/// (implying directory `src/foo`) can both reach this function with first
/// component `foo` and land as two *separate* sibling nodes: one taken by
/// the `leaves` branch below (`comps.len() == 1`), the other by the `dirs`
/// branch (`comps.len() > 1`) — the path-conflict coexistence this module's
/// docs describe, requiring no special-case check anywhere, just two
/// different branches of the same grouping pass naturally both firing for
/// the same component string.
fn build_level(items: Vec<(Vec<String>, usize)>, prefix: &str, depth: usize) -> Vec<Node> {
    let mut leaves: Vec<(String, usize)> = Vec::new();
    let mut dirs: HashMap<String, Vec<(Vec<String>, usize)>> = HashMap::new();

    for (mut components, file_idx) in items {
        if components.len() == 1 {
            leaves.push((components.pop().expect("len == 1"), file_idx));
        } else {
            let head = components.remove(0);
            dirs.entry(head).or_default().push((components, file_idx));
        }
    }

    let mut nodes = Vec::with_capacity(leaves.len() + dirs.len());
    for (label, file_idx) in leaves {
        let path = join_path(prefix, &label);
        nodes.push(Node {
            path,
            label,
            depth,
            kind: NodeKind::File { file_idx },
        });
    }
    for (label, sub_items) in dirs {
        let path = join_path(prefix, &label);
        let children = build_level(sub_items, &path, depth + 1);
        nodes.push(Node {
            path,
            label,
            depth,
            kind: NodeKind::Directory { children },
        });
    }
    sort_siblings(&mut nodes);
    nodes
}

fn join_path(prefix: &str, name: &str) -> String {
    if prefix.is_empty() {
        name.to_owned()
    } else {
        format!("{prefix}/{name}")
    }
}

/// Directories before files (rank `0` vs `1`), then lexicographic by raw
/// `label` — req 3. Deliberately the node's own component text, never a
/// display-width-truncated form: sorting is a data-shape concern,
/// truncation a rendering one, and conflating them would make a narrow
/// terminal silently reorder the tree.
fn sort_siblings(nodes: &mut [Node]) {
    nodes.sort_by(|a, b| {
        let rank = |n: &Node| u8::from(matches!(n.kind, NodeKind::File { .. }));
        rank(a).cmp(&rank(b)).then_with(|| a.label.cmp(&b.label))
    });
}

/// Every changed file nested under `node`, regardless of any directory's
/// collapsed state — the post-order rollup [`flatten_visible`] reads for
/// [`VisibleKind::Directory::descendant_files`].
fn descendant_file_count(node: &Node) -> usize {
    match &node.kind {
        NodeKind::File { .. } => 1,
        NodeKind::Directory { children } => children.iter().map(descendant_file_count).sum(),
    }
}

/// Flattens `tree` into the rows [`crate::ui::sidebar`] actually draws,
/// applying `collapsed` (directory paths currently collapsed — see
/// `App::collapsed_dirs`): a collapsed directory still emits its own row
/// (req 7 — a reviewer must be able to select and re-expand it) but none of
/// its descendants', regardless of how deep they'd otherwise nest. Defaults
/// to fully expanded for an empty `collapsed` — req 4 — since every
/// directory's descendants render unless explicitly named in that set.
pub fn flatten_visible(tree: &FileTree, collapsed: &HashSet<String>) -> Vec<VisibleRow> {
    let mut rows = Vec::new();
    for node in &tree.roots {
        flatten_node(node, collapsed, &mut rows);
    }
    rows
}

fn flatten_node(node: &Node, collapsed: &HashSet<String>, rows: &mut Vec<VisibleRow>) {
    match &node.kind {
        NodeKind::File { file_idx } => rows.push(VisibleRow {
            id: NodeId {
                path: node.path.clone(),
                is_directory: false,
            },
            depth: node.depth,
            label: node.label.clone(),
            kind: VisibleKind::File {
                file_idx: *file_idx,
            },
        }),
        NodeKind::Directory { children } => {
            let expanded = !collapsed.contains(&node.path);
            rows.push(VisibleRow {
                id: NodeId {
                    path: node.path.clone(),
                    is_directory: true,
                },
                depth: node.depth,
                label: node.label.clone(),
                kind: VisibleKind::Directory {
                    expanded,
                    descendant_files: descendant_file_count(node),
                },
            });
            if expanded {
                for child in children {
                    flatten_node(child, collapsed, rows);
                }
            }
        }
    }
}

/// Drops any path from `collapsed` that no longer names a directory in
/// `tree` — a watch refresh, scope swap, or unit-filter change can rename,
/// remove, or (rarer) turn a collapsed directory into a plain file's own
/// path, and a stale entry left behind would silently do nothing forever
/// (an entry no [`flatten_node`] call ever looks up again) rather than
/// actively cause harm; pruning it keeps `App::collapsed_dirs` an accurate
/// record of what's actually collapsed right now, mirroring
/// `App::prune_stale_folds`'s equivalent cleanup for expanded diff folds.
pub fn prune_collapsed(tree: &FileTree, collapsed: &mut HashSet<String>) {
    if collapsed.is_empty() {
        return;
    }
    let mut live = HashSet::new();
    collect_directory_paths(&tree.roots, &mut live);
    collapsed.retain(|path| live.contains(path));
}

fn collect_directory_paths(nodes: &[Node], out: &mut HashSet<String>) {
    for node in nodes {
        if let NodeKind::Directory { children } = &node.kind {
            out.insert(node.path.clone());
            collect_directory_paths(children, out);
        }
    }
}

/// Every directory path nested anywhere under `dir_path` — never `dir_path`
/// itself — reusing the same [`collect_directory_paths`] walk
/// [`prune_collapsed`] already does, just rooted at one directory's
/// children instead of the whole forest. Issue #23's context menu derives
/// its "Expand/Collapse all descendants" entries' bounded-ness from this
/// set's length (a leaf directory — zero nested directories — omits both
/// entries entirely, see `ui::context_menu::tree_dir_entries`) and
/// [`crate::ui::app::App::set_descendants_collapsed`] bulk-applies against
/// it — bounded strictly by the in-memory tree already built for the
/// sidebar, never a filesystem walk, which is what keeps "expand/collapse
/// all descendants" safe to offer at all (issue #23 req 4). An empty set —
/// not `None` — for a `dir_path` that doesn't name a directory in `tree` at
/// all; defensive only, since every real caller reads `dir_path` straight
/// off a [`VisibleRow`] this same `tree` just produced.
pub fn descendant_dir_paths(tree: &FileTree, dir_path: &str) -> HashSet<String> {
    let mut out = HashSet::new();
    if let Some(NodeKind::Directory { children }) = find_directory(&tree.roots, dir_path) {
        collect_directory_paths(children, &mut out);
    }
    out
}

/// The directory [`Node`] at `path`, depth-first — [`descendant_dir_paths`]'s
/// one lookup, kept as its own function rather than inlined so it stays a
/// plain tree search with no `collected` accumulator to thread through the
/// recursion (unlike [`collect_directory_paths`], which needs one).
fn find_directory<'a>(nodes: &'a [Node], path: &str) -> Option<&'a NodeKind> {
    for node in nodes {
        if let NodeKind::Directory { children } = &node.kind {
            if node.path == path {
                return Some(&node.kind);
            }
            if let Some(found) = find_directory(children, path) {
                return Some(found);
            }
        }
    }
    None
}

/// Re-anchors a files-pane selection against a freshly flattened `rows` —
/// req 8. Tries `previous` first (typically the selection from just before
/// a rebuild), then `fallback` (typically the diff cursor's own file, so a
/// vanished selection lands somewhere still relevant to what's on screen
/// rather than an arbitrary row); each candidate is resolved through the
/// same two tiers: the exact node if it's still visible, else the nearest
/// visible ancestor directory reached by repeatedly trimming the last `/`
/// segment off its path (this is what makes "collapsing a directory that
/// contains the selection moves the selection to that directory" — req
/// 7 — fall out for free: once collapsed, the selected descendant's row is
/// gone from `rows`, but its ancestor directory's own row is still there,
/// found on the very first step of this same walk. No special-cased
/// "was this a collapse?" check anywhere). `None` only when neither
/// candidate resolves to anything — including trivially when `rows` itself
/// is empty.
pub fn resolve_selection(
    rows: &[VisibleRow],
    previous: Option<&NodeId>,
    fallback: Option<&NodeId>,
) -> Option<NodeId> {
    previous
        .and_then(|id| resolve_one(rows, id))
        .or_else(|| fallback.and_then(|id| resolve_one(rows, id)))
}

fn resolve_one(rows: &[VisibleRow], id: &NodeId) -> Option<NodeId> {
    if let Some(row) = rows.iter().find(|row| &row.id == id) {
        return Some(row.id.clone());
    }
    nearest_visible_ancestor(rows, &id.path)
}

fn nearest_visible_ancestor(rows: &[VisibleRow], path: &str) -> Option<NodeId> {
    let mut current = path;
    while let Some((parent, _)) = current.rsplit_once('/') {
        if let Some(row) = rows
            .iter()
            .find(|row| row.id.is_directory && row.id.path == parent)
        {
            return Some(row.id.clone());
        }
        current = parent;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::diff::DiffFile;

    fn file(path: &str) -> DiffFile {
        DiffFile {
            new_path: Some(path.to_owned()),
            old_path: Some(path.to_owned()),
            ..Default::default()
        }
    }

    fn dir_labels(nodes: &[Node]) -> Vec<&str> {
        nodes.iter().map(|n| n.label.as_str()).collect()
    }

    // ---- build ------------------------------------------------------------

    #[test]
    fn build_of_no_files_produces_an_empty_forest() {
        let tree = build(&[]);
        assert!(tree.roots.is_empty());
    }

    #[test]
    fn build_of_a_single_flat_file_produces_one_root_file_node() {
        let files = [file("README.md")];
        let tree = build(&files);
        assert_eq!(tree.roots.len(), 1);
        let node = &tree.roots[0];
        assert_eq!(node.path, "README.md");
        assert_eq!(node.label, "README.md");
        assert_eq!(node.depth, 0);
        assert!(matches!(node.kind, NodeKind::File { file_idx: 0 }));
    }

    #[test]
    fn build_groups_files_under_shared_directory_prefixes() {
        let files = [file("src/lib.rs"), file("src/main.rs"), file("README.md")];
        let tree = build(&files);
        assert_eq!(tree.roots.len(), 2, "one `src` dir, one root file");
        let src = tree
            .roots
            .iter()
            .find(|n| n.label == "src")
            .expect("src directory present");
        let NodeKind::Directory { children } = &src.kind else {
            panic!("src must be a directory node");
        };
        assert_eq!(dir_labels(children), vec!["lib.rs", "main.rs"]);
    }

    #[test]
    fn build_sorts_directories_before_files_then_lexicographically() {
        let files = [
            file("src/zeta.rs"),
            file("README.md"),
            file("src/alpha.rs"),
            file("assets/logo.png"),
        ];
        let tree = build(&files);
        // Two directories ("assets", "src") must both sort ahead of the
        // one root-level file ("README.md"), and among themselves
        // lexicographically.
        assert_eq!(dir_labels(&tree.roots), vec!["assets", "src", "README.md"]);
        let src = tree.roots.iter().find(|n| n.label == "src").unwrap();
        let NodeKind::Directory { children } = &src.kind else {
            panic!("src must be a directory")
        };
        assert_eq!(dir_labels(children), vec!["alpha.rs", "zeta.rs"]);
    }

    #[test]
    fn build_handles_a_deeply_nested_path_with_depth_incrementing_each_level() {
        let files = [file("a/b/c/d/e.rs")];
        let tree = build(&files);
        let mut node = &tree.roots[0];
        assert_eq!(node.depth, 0);
        assert_eq!(node.label, "a");
        for (expected_label, expected_depth) in [("b", 1), ("c", 2), ("d", 3)] {
            let NodeKind::Directory { children } = &node.kind else {
                panic!("{} must be a directory", node.label);
            };
            assert_eq!(children.len(), 1);
            node = &children[0];
            assert_eq!(node.label, expected_label);
            assert_eq!(node.depth, expected_depth);
        }
        let NodeKind::Directory { children } = &node.kind else {
            panic!("d must be a directory");
        };
        assert_eq!(children[0].label, "e.rs");
        assert_eq!(children[0].depth, 4);
        assert!(matches!(children[0].kind, NodeKind::File { file_idx: 0 }));
    }

    /// The path-conflict case this module's docs describe: `src/foo` is a
    /// changed file in the same diff that also changes `src/foo/bar.rs`,
    /// implying a directory of the very same name. Both must survive as
    /// distinct sibling nodes under `src` — neither dropped, neither
    /// silently merged.
    #[test]
    fn build_lets_a_path_be_both_a_changed_file_and_an_implied_directory() {
        let files = [file("src/foo"), file("src/foo/bar.rs")];
        let tree = build(&files);
        let src = &tree.roots[0];
        let NodeKind::Directory { children } = &src.kind else {
            panic!("src must be a directory");
        };
        assert_eq!(children.len(), 2, "the file and the directory coexist");
        let file_node = children
            .iter()
            .find(|n| matches!(n.kind, NodeKind::File { .. }))
            .expect("the file sibling is present");
        let dir_node = children
            .iter()
            .find(|n| matches!(n.kind, NodeKind::Directory { .. }))
            .expect("the directory sibling is present");
        assert_eq!(file_node.path, "src/foo");
        assert_eq!(dir_node.path, "src/foo");
        assert!(matches!(file_node.kind, NodeKind::File { file_idx: 0 }));
        let NodeKind::Directory { children } = &dir_node.kind else {
            unreachable!()
        };
        assert_eq!(children[0].label, "bar.rs");
    }

    #[test]
    fn build_never_sums_line_stats_into_descendant_counts() {
        // Descendant counts must come from `flatten_visible`'s file-count
        // rollup, not from anything resembling `DiffFile::stat()` — this is
        // pinned down at the flatten layer below, where the count actually
        // materializes; `build` itself has no line-stat field to leak one
        // through in the first place, which this test documents.
        let files = [file("src/a.rs"), file("src/b.rs")];
        let tree = build(&files);
        let rows = flatten_visible(&tree, &HashSet::new());
        let src_row = rows.iter().find(|r| r.label == "src").unwrap();
        assert_eq!(
            src_row.kind,
            VisibleKind::Directory {
                expanded: true,
                descendant_files: 2,
            }
        );
    }

    // ---- flatten_visible ----------------------------------------------

    #[test]
    fn flatten_visible_starts_fully_expanded_with_an_empty_collapsed_set() {
        let files = [file("src/a.rs"), file("src/nested/b.rs")];
        let tree = build(&files);
        let rows = flatten_visible(&tree, &HashSet::new());
        let labels: Vec<&str> = rows.iter().map(|r| r.label.as_str()).collect();
        // "nested" (a directory) sorts ahead of "a.rs" (a file) under "src"
        // — req 3, directories before files at every level.
        assert_eq!(labels, vec!["src", "nested", "b.rs", "a.rs"]);
    }

    #[test]
    fn flatten_visible_computes_post_order_descendant_file_counts() {
        let files = [
            file("src/a.rs"),
            file("src/nested/b.rs"),
            file("src/nested/c.rs"),
        ];
        let tree = build(&files);
        let rows = flatten_visible(&tree, &HashSet::new());
        let src = rows.iter().find(|r| r.label == "src").unwrap();
        assert_eq!(
            src.kind,
            VisibleKind::Directory {
                expanded: true,
                descendant_files: 3,
            }
        );
        let nested = rows.iter().find(|r| r.label == "nested").unwrap();
        assert_eq!(
            nested.kind,
            VisibleKind::Directory {
                expanded: true,
                descendant_files: 2,
            }
        );
    }

    #[test]
    fn flatten_visible_collapsed_directory_hides_descendants_but_keeps_its_own_row() {
        let files = [file("src/a.rs"), file("src/nested/b.rs")];
        let tree = build(&files);
        let mut collapsed = HashSet::new();
        collapsed.insert("src".to_owned());
        let rows = flatten_visible(&tree, &collapsed);
        let labels: Vec<&str> = rows.iter().map(|r| r.label.as_str()).collect();
        assert_eq!(
            labels,
            vec!["src"],
            "src's own row stays, its subtree hides"
        );
        assert_eq!(
            rows[0].kind,
            VisibleKind::Directory {
                expanded: false,
                descendant_files: 2,
            },
            "descendant count still reports even while collapsed"
        );
    }

    #[test]
    fn flatten_visible_collapsing_an_inner_directory_leaves_its_own_ancestor_expanded() {
        let files = [file("src/a.rs"), file("src/nested/b.rs")];
        let tree = build(&files);
        let mut collapsed = HashSet::new();
        collapsed.insert("src/nested".to_owned());
        let rows = flatten_visible(&tree, &collapsed);
        let labels: Vec<&str> = rows.iter().map(|r| r.label.as_str()).collect();
        // "nested" still sorts ahead of "a.rs" even collapsed — only its
        // own descendants ("b.rs") are hidden.
        assert_eq!(labels, vec!["src", "nested", "a.rs"]);
    }

    // ---- descendant_dir_paths (issue #23) ----------------------------------

    #[test]
    fn descendant_dir_paths_of_a_leaf_directory_is_empty() {
        let files = [file("src/a.rs")];
        let tree = build(&files);
        assert!(descendant_dir_paths(&tree, "src").is_empty());
    }

    #[test]
    fn descendant_dir_paths_collects_every_nested_directory_but_never_itself() {
        let files = [
            file("src/nested/deep/a.rs"),
            file("src/nested/b.rs"),
            file("src/other/c.rs"),
        ];
        let tree = build(&files);
        let paths = descendant_dir_paths(&tree, "src");
        assert_eq!(
            paths,
            HashSet::from([
                "src/nested".to_owned(),
                "src/nested/deep".to_owned(),
                "src/other".to_owned(),
            ])
        );
        assert!(!paths.contains("src"), "must never include dir_path itself");
    }

    #[test]
    fn descendant_dir_paths_of_an_unknown_path_is_empty() {
        let files = [file("src/a.rs")];
        let tree = build(&files);
        assert!(descendant_dir_paths(&tree, "does/not/exist").is_empty());
    }

    #[test]
    fn descendant_dir_paths_of_a_file_path_is_empty() {
        // "src/a.rs" names a *file*, not a directory — `find_directory`
        // must never match it even though the path string exists in the
        // tree.
        let files = [file("src/a.rs")];
        let tree = build(&files);
        assert!(descendant_dir_paths(&tree, "src/a.rs").is_empty());
    }

    // ---- prune_collapsed ------------------------------------------------

    #[test]
    fn prune_collapsed_drops_paths_that_no_longer_name_a_directory() {
        let old_files = [file("src/a.rs")];
        let old_tree = build(&old_files);
        let mut collapsed = HashSet::new();
        collapsed.insert("src".to_owned());
        // Sanity: "src" really is a directory in the old tree.
        assert!(matches!(old_tree.roots[0].kind, NodeKind::Directory { .. }));

        // A refresh where `src/` no longer has any changed file under it —
        // "src" (as a directory) has vanished from the new tree entirely.
        let new_files = [file("other.rs")];
        let new_tree = build(&new_files);
        prune_collapsed(&new_tree, &mut collapsed);
        assert!(collapsed.is_empty());
    }

    #[test]
    fn prune_collapsed_keeps_paths_that_still_name_a_directory() {
        let files = [file("src/a.rs"), file("src/b.rs")];
        let tree = build(&files);
        let mut collapsed = HashSet::new();
        collapsed.insert("src".to_owned());
        prune_collapsed(&tree, &mut collapsed);
        assert!(collapsed.contains("src"));
    }

    // ---- resolve_selection ------------------------------------------------

    fn node_id(path: &str, is_directory: bool) -> NodeId {
        NodeId {
            path: path.to_owned(),
            is_directory,
        }
    }

    #[test]
    fn resolve_selection_returns_none_for_empty_rows() {
        assert_eq!(resolve_selection(&[], None, None), None);
        assert_eq!(
            resolve_selection(&[], Some(&node_id("a.rs", false)), None),
            None
        );
    }

    #[test]
    fn resolve_selection_finds_the_exact_previous_node_first() {
        let files = [file("a.rs"), file("b.rs")];
        let tree = build(&files);
        let rows = flatten_visible(&tree, &HashSet::new());
        let previous = node_id("b.rs", false);
        assert_eq!(
            resolve_selection(&rows, Some(&previous), None),
            Some(previous)
        );
    }

    #[test]
    fn resolve_selection_walks_up_to_the_nearest_visible_ancestor_when_collapsed() {
        let files = [file("src/nested/deep.rs")];
        let tree = build(&files);
        let mut collapsed = HashSet::new();
        collapsed.insert("src/nested".to_owned());
        let rows = flatten_visible(&tree, &collapsed);
        let previous = node_id("src/nested/deep.rs", false);
        assert_eq!(
            resolve_selection(&rows, Some(&previous), None),
            Some(node_id("src/nested", true)),
            "the file's own row is hidden; its collapsed parent directory is the fallback"
        );
    }

    #[test]
    fn resolve_selection_falls_back_to_the_fallback_id_when_previous_is_gone() {
        let files = [file("a.rs"), file("b.rs")];
        let tree = build(&files);
        let rows = flatten_visible(&tree, &HashSet::new());
        // "deleted.rs" no longer exists anywhere in `rows` at all — not
        // even an ancestor to walk up to (a root-level path has none).
        let previous = node_id("deleted.rs", false);
        let fallback = node_id("b.rs", false);
        assert_eq!(
            resolve_selection(&rows, Some(&previous), Some(&fallback)),
            Some(fallback)
        );
    }

    #[test]
    fn resolve_selection_falls_back_to_the_fallbacks_ancestor_when_it_is_also_hidden() {
        let files = [file("src/nested/deep.rs"), file("other.rs")];
        let tree = build(&files);
        let mut collapsed = HashSet::new();
        collapsed.insert("src/nested".to_owned());
        let rows = flatten_visible(&tree, &collapsed);
        let previous = node_id("gone.rs", false);
        let fallback = node_id("src/nested/deep.rs", false);
        assert_eq!(
            resolve_selection(&rows, Some(&previous), Some(&fallback)),
            Some(node_id("src/nested", true))
        );
    }

    #[test]
    fn resolve_selection_is_none_when_nothing_resolves() {
        let files = [file("a.rs")];
        let tree = build(&files);
        let rows = flatten_visible(&tree, &HashSet::new());
        let previous = node_id("gone.rs", false);
        assert_eq!(resolve_selection(&rows, Some(&previous), None), None);
    }
}
