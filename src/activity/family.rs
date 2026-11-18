//! Where each tower stands inside its district.
//!
//! A district is a grid of slots: slot `s` is column `s / rows` and depth row
//! `s % rows`, row 0 at the front. Filling slots in recency order puts
//! neighbouring sessions one behind the other, so a subagent started straight
//! after its parent would stand hidden behind it, and the delegation arc
//! between them would read as a loop over one tower. Here a parent and its
//! subagents are placed as a family along one row, side by side, while the
//! most recent session still takes the front slot of the first column.
use super::{city_links, Session};
use std::collections::BTreeSet;

/// Slot of each member, in the order given. `members` are session indices in
/// recency order; `sessions` is the day's list they index into. A subagent
/// joins its parent's row only when the parent is in the same district.
pub(super) fn slots(sessions: &[Session<'_>], members: &[usize], rows: usize) -> Vec<usize> {
    let parents: Vec<Option<usize>> = members
        .iter()
        .map(|&i| {
            let session = &sessions[i];
            let parent = city_links::parent_of(session)?;
            members.iter().position(|&m| {
                sessions[m].agent == session.agent && sessions[m].id == parent && m != i
            })
        })
        .collect();
    arrange(&parents, rows)
}

/// Slots for members whose parent (by position) is given. Each family is
/// placed when its most recent member comes up: that member takes the first
/// free slot, and the rest of the family follow along the same row, one
/// column apart, the parent first. Those slots are always free: an earlier
/// family in the same row ends before the first free slot, because each one
/// started at the first free slot of its own turn. Without families this is
/// the identity.
fn arrange(parents: &[Option<usize>], rows: usize) -> Vec<usize> {
    let rows = rows.max(1);
    let mut slot = vec![usize::MAX; parents.len()];
    let mut taken = BTreeSet::new();
    for i in 0..parents.len() {
        if slot[i] != usize::MAX {
            continue;
        }
        let root = parents[i].unwrap_or(i);
        let mut family = vec![i];
        if root != i {
            family.push(root);
        }
        family.extend((0..parents.len()).filter(|&j| j != i && parents[j] == Some(root)));
        family.retain(|&m| slot[m] == usize::MAX);
        let start = (0..).find(|s| !taken.contains(s)).unwrap_or(0);
        for (k, member) in family.into_iter().enumerate() {
            slot[member] = start + k * rows;
            taken.insert(slot[member]);
        }
    }
    slot
}

#[cfg(test)]
mod tests {
    use super::super::{city, layout, State, PITCH};
    use super::*;
    use crate::activity_data::{demo_events, Event};

    /// Towers of `child` and its parent stand side by side: same row, one pitch apart.
    fn assert_beside(events: &[Event], child: &str) {
        let built = city(events, &mut State::default());
        for stretch in [1.0, 1.6, 2.6] {
            let lay = layout(&built, stretch);
            let at = |id: &str| {
                let i = built.sessions.iter().position(|s| s.id == id).unwrap();
                let tower = lay.towers.iter().find(|t| t.session == i).unwrap();
                (tower.x, tower.z)
            };
            let parent = child.split_once('/').unwrap().0;
            let (a, b) = (at(parent), at(child));
            assert!((a.1 - b.1).abs() < 1e-9, "{child}: not in the parent's row");
            assert!(
                ((a.0 - b.0).abs() - PITCH * stretch).abs() < 1e-9,
                "{child}: {a:?} and {b:?} are not neighbours"
            );
            for tower in &lay.towers {
                assert!(tower.x.abs() <= lay.width / 2.0 && tower.z.abs() <= lay.depth / 2.0);
            }
        }
    }

    #[test]
    fn the_demo_subagent_stands_beside_its_parent_not_behind_it() {
        assert_beside(&demo_events(), "93bc1f6c-demo/7d1e0c55");
    }

    #[test]
    fn in_a_busy_district_a_subagent_still_stands_beside_its_parent() {
        let mut events: Vec<Event> = (0..8)
            .map(|i| Event {
                key: format!("{i:05}"),
                at: format!("2026-01-02T10:{:02}:00.000Z", 50 - i),
                agent: "claude".into(),
                session: format!("s{i}"),
                project: "/work/alpha".into(),
                kind: "tool".into(),
                text: "Read x".into(),
                source: "fixture".into(),
                parent: None,
                path: None,
            })
            .collect();
        // The subagent of the fourth most recent session is the least recent.
        events[7].session = "s3/kid".into();
        events[7].parent = Some("s3".into());
        assert_beside(&events, "s3/kid");
        let built = city(&events, &mut State::default());
        let lay = layout(&built, 1.0);
        let front = lay.towers.iter().find(|t| t.session == 0).unwrap();
        assert!(
            lay.towers.iter().all(|t| t.z <= front.z && t.x <= front.x),
            "the most recent session stands at the front of the first column"
        );
    }

    #[test]
    fn without_families_every_member_keeps_its_place() {
        assert_eq!(arrange(&[None; 7], 3), (0..7).collect::<Vec<_>>());
        assert!(arrange(&[], 3).is_empty());
    }

    #[test]
    fn a_subagent_stands_beside_its_parent_whichever_is_more_recent() {
        // Parent most recent: it takes the front slot, the subagent the next column.
        assert_eq!(arrange(&[None, Some(0)], 3), vec![0, 3]);
        // Subagent most recent: it keeps the front, its parent stands beside it.
        assert_eq!(arrange(&[Some(1), None, None], 3), vec![0, 3, 1]);
    }

    #[test]
    fn several_subagents_line_up_and_others_fill_the_gaps() {
        // 0 is a parent of 2 and 4; 1 and 3 are unrelated.
        let slots = arrange(&[None, None, Some(0), None, Some(0)], 3);
        assert_eq!(slots, vec![0, 1, 3, 2, 6]);
        let unique: BTreeSet<_> = slots.iter().collect();
        assert_eq!(unique.len(), slots.len(), "no two towers share a slot");
        for child in [2, 4] {
            assert_eq!(slots[child] % 3, slots[0] % 3, "same row as the parent");
        }
    }

    #[test]
    fn in_a_single_row_a_family_takes_neighbouring_slots() {
        // The subagent is least recent but stands next to its parent.
        let slots = arrange(&[None, None, Some(0)], 1);
        assert_eq!(slots, vec![0, 2, 1]);
        // Families that interleave across rows never share a slot.
        for rows in 1..5 {
            let parents = [None, None, Some(0), Some(1), Some(0), None, Some(5)];
            let slots = arrange(&parents, rows);
            let unique: BTreeSet<_> = slots.iter().collect();
            assert_eq!(unique.len(), slots.len(), "{rows} rows: {slots:?}");
        }
    }
}
