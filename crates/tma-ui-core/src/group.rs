//! The watch table/list grouping fold: roll agent rows up by their resolved `repo` for the grouped
//! wide layouts. Pure — it borrows pre-annotated rows and returns display-ordered groups; the shell
//! draws the header lines and interleaves them. Groups order by triage urgency so the
//! longest-blocked pane's group stays on top; rows keep their state-sorted order within a group.

use std::cmp::Ordering;

use tma_core::{sort_rank, AgentRow};

/// The single bucket every unresolved row (no `repo`) folds into (a deliberate divergence from the
/// reference impl's per-directory buckets).
pub(crate) const NO_REPO: &str = "(no repo)";

/// One repo group for the grouped watch display: the dimmed header label plus the indices of its
/// member rows, in the input slice's order. When the rows are already state-sorted (the surfaces pass
/// them so), members stay state-sorted, and after the model reorders `rows` into grouped display order
/// each group's members are a contiguous ascending range.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Group {
    /// The header label: the repo name, or [`NO_REPO`] for the unresolved bucket.
    pub name: String,
    /// Indices into the rows slice `group_rows` was called with, in that slice's order.
    pub members: Vec<usize>,
}

/// Group `rows` by `repo` for the grouped watch layout. Every unresolved row folds into one
/// [`NO_REPO`] bucket. Groups order by their most urgent member: best `sort_rank`, then the smallest
/// `since` among members holding that rank, then the display name, then the repo key. Member order is
/// the input order, so state-sorted input yields state-sorted rows within each group and the
/// globally longest-blocked row leads the first group.
pub(crate) fn group_rows(rows: &[AgentRow]) -> Vec<Group> {
    let mut keys: Vec<Option<&str>> = Vec::new();
    let mut members: Vec<Vec<usize>> = Vec::new();
    for (i, r) in rows.iter().enumerate() {
        let key = r.repo.as_ref().map(|l| l.name.as_str());
        match keys.iter().position(|k| *k == key) {
            Some(p) => members[p].push(i),
            None => {
                keys.push(key);
                members.push(vec![i]);
            }
        }
    }
    let mut order: Vec<usize> = (0..keys.len()).collect();
    order.sort_by(|&a, &b| group_order(rows, &members[a], keys[a], &members[b], keys[b]));
    order
        .into_iter()
        .map(|i| Group {
            name: keys[i].unwrap_or(NO_REPO).to_string(),
            members: std::mem::take(&mut members[i]),
        })
        .collect()
}

/// The total group order: best member `sort_rank`, then the smallest `since` among members holding
/// that rank, then display name, then repo key.
fn group_order(
    rows: &[AgentRow],
    a: &[usize],
    a_key: Option<&str>,
    b: &[usize],
    b_key: Option<&str>,
) -> Ordering {
    let (ar, as_) = urgency(rows, a);
    let (br, bs) = urgency(rows, b);
    ar.cmp(&br)
        .then(as_.cmp(&bs))
        .then_with(|| a_key.unwrap_or(NO_REPO).cmp(b_key.unwrap_or(NO_REPO)))
        .then_with(|| a_key.cmp(&b_key))
}

/// A group's triage key: its best (lowest) member `sort_rank`, and the smallest `since` among the
/// members holding that rank. Members are never empty.
fn urgency(rows: &[AgentRow], members: &[usize]) -> (u8, u64) {
    let best = members
        .iter()
        .map(|&i| sort_rank(rows[i].state))
        .min()
        .unwrap_or(u8::MAX);
    let min_since = members
        .iter()
        .filter(|&&i| sort_rank(rows[i].state) == best)
        .map(|&i| rows[i].since)
        .min()
        .unwrap_or(0);
    (best, min_since)
}

/// The draw's list index for the selected row once group headers are interleaved: the flat `sel`
/// index plus one header line per group rendered at or before the selected row's group. Returns `sel`
/// unchanged when no group holds it (the flat, ungrouped view renders no headers). Crate-internal:
/// the draw reads [`WatchModel::display_selection`](crate::WatchModel::display_selection) instead of
/// pairing an index with a group slice itself.
pub(crate) fn display_index(groups: &[Group], sel: usize) -> usize {
    let mut headers = 0;
    for g in groups {
        headers += 1;
        if g.members.contains(&sel) {
            return sel + headers;
        }
    }
    sel
}

/// The inverse of [`display_index`]: the flat row index a draw line holds, or `None` when that line
/// is a group header (nothing to select) or past the end. What a mouse click needs — it lands on a
/// drawn line and has to name the row under it.
pub(crate) fn row_at_display(groups: &[Group], flat_len: usize, line: usize) -> Option<usize> {
    if groups.is_empty() {
        return (line < flat_len).then_some(line);
    }
    let mut cursor = 0;
    for g in groups {
        if line == cursor {
            return None; // the group's own header line
        }
        cursor += 1;
        if line < cursor + g.members.len() {
            return Some(line - cursor + g.members[0]);
        }
        cursor += g.members.len();
    }
    None
}

/// The number of lines the grouped display draws: every row plus one header per group. Flat
/// (no groups) is just the rows.
pub(crate) fn display_len(groups: &[Group], flat_len: usize) -> usize {
    if groups.is_empty() {
        flat_len
    } else {
        flat_len + groups.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tma_core::{AgentState, RepoLabel};

    fn row(pane: &str, repo: Option<&str>, state: AgentState, since: u64) -> AgentRow {
        AgentRow {
            pane_id: pane.to_string(),
            agent: "claude".to_string(),
            state,
            detail: None,
            since,
            turn_at: 0,
            session: "s".to_string(),
            window_index: 0,
            pane_index: 0,
            title: "t".to_string(),
            attention: false,
            agent_session: None,
            context_pct: None,
            context_at: None,
            tokens: None,
            quota: None,
            cost_usd: None,
            muted: false,
            model: None,
            cwd: None,
            repo: repo.map(|name| RepoLabel {
                name: name.to_string(),
                branch: String::new(),
                worktree: false,
            }),
            pending: None,
        }
    }

    /// The group display names, in order.
    fn names(groups: &[Group]) -> Vec<String> {
        groups.iter().map(|g| g.name.clone()).collect()
    }

    #[test]
    fn worktrees_roll_up_under_one_repo_name() {
        // Two panes on different branches of the same repo (worktrees) share one group; a third repo
        // is its own group. No branch key involved — rollup is by repo name alone.
        let rows = vec![
            row("%0", Some("app"), AgentState::Idle, 10),
            row("%1", Some("app"), AgentState::Idle, 20),
            row("%2", Some("lib"), AgentState::Idle, 5),
        ];
        let groups = group_rows(&rows);
        assert_eq!(groups.len(), 2, "two repos, worktrees rolled up");
        let app = groups.iter().find(|g| g.name == "app").unwrap();
        assert_eq!(app.members, vec![0, 1], "both app panes in one group");
    }

    #[test]
    fn every_unresolved_row_folds_into_one_no_repo_bucket() {
        let rows = vec![
            row("%0", None, AgentState::Idle, 10),
            row("%1", Some("app"), AgentState::Idle, 10),
            row("%2", None, AgentState::Idle, 20),
        ];
        let groups = group_rows(&rows);
        let no_repo: Vec<&Group> = groups.iter().filter(|g| g.name == NO_REPO).collect();
        assert_eq!(no_repo.len(), 1, "exactly one (no repo) bucket");
        assert_eq!(no_repo[0].members, vec![0, 2]);
    }

    #[test]
    fn groups_order_by_most_urgent_member_rank() {
        // `lib` holds a blocked pane, `app` only idle: the blocked group leads regardless of name.
        let rows = vec![
            row("%0", Some("app"), AgentState::Idle, 5),
            row("%1", Some("lib"), AgentState::Blocked, 50),
        ];
        assert_eq!(names(&group_rows(&rows)), vec!["lib", "app"]);
    }

    #[test]
    fn rank_tie_broken_by_smallest_since_at_that_rank() {
        // Both groups have a blocked pane; the one whose blocked pane has been blocked longer (the
        // smaller `since`) leads. `app`'s idle pane has an even smaller `since` but does not count —
        // only members at the best rank (blocked) do.
        let rows = vec![
            row("%0", Some("app"), AgentState::Idle, 1),
            row("%1", Some("app"), AgentState::Blocked, 300),
            row("%2", Some("lib"), AgentState::Blocked, 100),
        ];
        assert_eq!(names(&group_rows(&rows)), vec!["lib", "app"]);
    }

    #[test]
    fn full_tie_broken_by_display_name() {
        // Identical rank and since: the name orders them. `zed` after `abc`.
        let rows = vec![
            row("%0", Some("zed"), AgentState::Blocked, 100),
            row("%1", Some("abc"), AgentState::Blocked, 100),
        ];
        assert_eq!(names(&group_rows(&rows)), vec!["abc", "zed"]);
    }

    #[test]
    fn no_repo_bucket_participates_in_urgency_order() {
        // The unresolved bucket is not pinned last: its blocked pane, longest-blocked, puts it first.
        let rows = vec![
            row("%0", Some("app"), AgentState::Blocked, 200),
            row("%1", None, AgentState::Blocked, 50),
        ];
        assert_eq!(names(&group_rows(&rows)), vec![NO_REPO, "app"]);
    }

    #[test]
    fn globally_longest_blocked_leads_the_first_group() {
        // Rows arrive state-sorted (the surfaces sort before grouping), so the longest-blocked pane
        // is already first in its repo. Its group leads the list, and it leads the group: reordering
        // by group then member keeps it on top.
        let rows = vec![
            row("%2", Some("lib"), AgentState::Blocked, 30), // globally longest-blocked
            row("%1", Some("lib"), AgentState::Blocked, 40),
            row("%0", Some("app"), AgentState::Working, 10),
        ];
        let groups = group_rows(&rows);
        assert_eq!(groups[0].name, "lib");
        assert_eq!(
            rows[groups[0].members[0]].pane_id, "%2",
            "the longest-blocked pane leads its group"
        );
    }

    #[test]
    fn display_index_skips_the_headers_at_and_before_the_row() {
        // Two groups of two rows each; the flat rows are [g0r0, g0r1, g1r0, g1r1] after reorder.
        let groups = vec![
            Group {
                name: "a".to_string(),
                members: vec![0, 1],
            },
            Group {
                name: "b".to_string(),
                members: vec![2, 3],
            },
        ];
        // First group: one header precedes.
        assert_eq!(display_index(&groups, 0), 1);
        assert_eq!(display_index(&groups, 1), 2);
        // Second group: its own header plus the first group's.
        assert_eq!(display_index(&groups, 2), 4);
        assert_eq!(display_index(&groups, 3), 5);
    }

    #[test]
    fn display_index_no_groups_is_the_flat_index() {
        assert_eq!(display_index(&[], 3), 3);
    }

    /// Two groups over three rows draw as: ▸app, %0, %1, ▸lib, %2. A click lands on a drawn line,
    /// so the mapping back to a row has to skip the headers and refuse them.
    #[test]
    fn row_at_display_maps_lines_back_to_rows_and_refuses_headers() {
        let groups = vec![
            Group {
                name: "app".to_string(),
                members: vec![0, 1],
            },
            Group {
                name: "lib".to_string(),
                members: vec![2],
            },
        ];
        assert_eq!(display_len(&groups, 3), 5);
        assert_eq!(row_at_display(&groups, 3, 0), None, "▸app");
        assert_eq!(row_at_display(&groups, 3, 1), Some(0));
        assert_eq!(row_at_display(&groups, 3, 2), Some(1));
        assert_eq!(row_at_display(&groups, 3, 3), None, "▸lib");
        assert_eq!(row_at_display(&groups, 3, 4), Some(2));
        assert_eq!(row_at_display(&groups, 3, 5), None, "past the last line");
        // Flat (no groups): a line is its own row.
        assert_eq!(display_len(&[], 3), 3);
        assert_eq!(row_at_display(&[], 3, 2), Some(2));
        assert_eq!(row_at_display(&[], 3, 3), None);
    }

    /// The two mappings are inverses on every row, which is what keeps a click on the highlighted
    /// line selecting the row already highlighted.
    #[test]
    fn display_index_and_row_at_display_round_trip() {
        let rows = vec![
            row("%0", Some("app"), AgentState::Blocked, 5),
            row("%1", Some("lib"), AgentState::Working, 6),
            row("%2", Some("app"), AgentState::Idle, 7),
        ];
        // Reorder into grouped display order the way the model does, so members are contiguous.
        let groups = group_rows(&rows);
        let mut contiguous = Vec::new();
        let mut cached = Vec::new();
        for g in groups {
            let start = contiguous.len();
            for i in &g.members {
                contiguous.push(rows[*i].clone());
            }
            cached.push(Group {
                name: g.name,
                members: (start..contiguous.len()).collect(),
            });
        }
        for r in 0..contiguous.len() {
            assert_eq!(
                row_at_display(&cached, contiguous.len(), display_index(&cached, r)),
                Some(r)
            );
        }
    }

    // --- properties -----------------------------------------------------------------------------

    use proptest::prelude::*;

    fn arb_state() -> impl Strategy<Value = AgentState> {
        prop_oneof![
            Just(AgentState::Blocked),
            Just(AgentState::Working),
            Just(AgentState::Idle),
            Just(AgentState::Unknown),
        ]
    }

    /// A repo key that exercises every fold: unresolved (`None`), an empty-name label (its own
    /// key, distinct from `None`), and a handful of real repo names that collide into groups.
    fn arb_repo() -> impl Strategy<Value = Option<String>> {
        prop_oneof![
            Just(None),
            Just(Some(String::new())),
            "[a-c]".prop_map(Some),
        ]
    }

    fn arb_rows() -> impl Strategy<Value = Vec<AgentRow>> {
        prop::collection::vec((arb_repo(), arb_state(), 0u64..100), 0..12).prop_map(|specs| {
            specs
                .into_iter()
                .enumerate()
                .map(|(i, (repo, state, since))| {
                    row(&format!("%{i}"), repo.as_deref(), state, since)
                })
                .collect()
        })
    }

    proptest! {
        /// The groups' members are an exact partition of `0..n` (every input index exactly once),
        /// and each group keeps its members in ascending input order (the documented member order).
        #[test]
        fn group_members_partition_indices_in_input_order(rows in arb_rows()) {
            let groups = group_rows(&rows);
            let mut seen: Vec<usize> = groups.iter().flat_map(|g| g.members.iter().copied()).collect();
            seen.sort_unstable();
            prop_assert_eq!(seen, (0..rows.len()).collect::<Vec<_>>());
            for g in &groups {
                let mut ascending = g.members.clone();
                ascending.sort_unstable();
                prop_assert_eq!(&g.members, &ascending);
            }
        }
    }
}
