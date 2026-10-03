//! Joining an assessment to a plan: which verdicts become trash actions.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use super::ActionError;
use super::plan::{Action, Justification, Plan};
use crate::verdict::{Assessment, Disposition, Entry};

/// The entries a user may pick for a plan: every `reclaimable` one, and every
/// `review` one when `include_review`, in the assessment's group order.
///
/// A candidate is only offered. Nothing reaches a plan until the user names it
/// in the selection [`plan_from`] takes.
pub fn candidates(assessment: &Assessment, include_review: bool) -> Vec<&Entry> {
    assessment
        .groups
        .iter()
        .flat_map(|group| &group.entries)
        .filter(|entry| included(entry.verdict.disposition, include_review))
        .collect()
}

/// A trash plan anchored at the assessment's root, holding exactly the
/// [`candidates`] whose path the user put in `selection`, biggest first.
///
/// The selection is required and opt-in: an empty one is an empty plan. A
/// selected path that is not a candidate under `include_review` is left out.
///
/// `Err` only when the root cannot anchor a plan. A path the guards refuse is
/// information, not a stop: the other actions are still sound, so it comes back
/// beside the plan for the caller to show.
pub fn plan_from(
    assessment: &Assessment,
    selection: &HashSet<PathBuf>,
    include_review: bool,
) -> Result<(Plan, Vec<(PathBuf, ActionError)>), ActionError> {
    let mut plan = Plan::new(&assessment.root)?;

    let mut chosen: Vec<&Entry> = candidates(assessment, include_review)
        .into_iter()
        .filter(|entry| selection.contains(Path::new(&entry.path)))
        .collect();
    // By the path's text rather than `Path`'s component order: the order a
    // printed plan has always listed ties in.
    chosen.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.path.cmp(&b.path)));

    let mut refused = Vec::new();
    for entry in chosen {
        let verdict = &entry.verdict;
        let justification = Justification::new(
            verdict.reason.clone(),
            verdict.provenance.pack.clone(),
            verdict.provenance.rule.clone(),
        );
        let path = PathBuf::from(&entry.path);
        if let Err(error) =
            plan.push(Action::Trash { path: path.clone() }, entry.bytes, justification)
        {
            refused.push((path, error));
        }
    }
    Ok((plan, refused))
}

fn included(disposition: Disposition, include_review: bool) -> bool {
    match disposition {
        Disposition::Reclaimable => true,
        Disposition::Review => include_review,
        Disposition::Keep => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::verdict::{Group, Label, Provenance, Verdict};

    fn entry(path: &Path, disposition: Disposition) -> Entry {
        std::fs::create_dir_all(path).unwrap();
        Entry {
            path: path.display().to_string(),
            bytes: 1,
            verdict: Verdict {
                label: Label::CACHE,
                disposition,
                confidence: 1.0,
                reason: "test".into(),
                provenance: Provenance::new("p", "r"),
                capped: None,
            },
            reach: None,
        }
    }

    fn planned(plan: &Plan) -> Vec<String> {
        let mut names: Vec<String> = plan
            .actions()
            .iter()
            .map(|e| e.action.path().file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
    }

    // Catches a plan filling itself with every candidate the user never picked,
    // a `review` verdict reaching a plan the user never widened with
    // --include-review, and a `keep` verdict being plannable at all.
    #[test]
    fn only_selected_candidates_plan_and_review_needs_opt_in() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let entries = vec![
            entry(&root.join("gone"), Disposition::Reclaimable),
            entry(&root.join("maybe"), Disposition::Review),
            entry(&root.join("kept"), Disposition::Keep),
        ];
        let assessment = Assessment {
            root: root.clone(),
            groups: vec![Group { label: Label::CACHE, bytes: 3, entries }],
            reclaimable_bytes: 1,
            shared: Vec::new(),
        };

        let (plan, refused) = plan_from(&assessment, &HashSet::new(), true).unwrap();
        assert!(refused.is_empty());
        assert!(plan.is_empty());
        assert_eq!(plan.total_bytes(), 0);

        let all = HashSet::from([root.join("gone"), root.join("maybe"), root.join("kept")]);
        let (plan, _) = plan_from(&assessment, &all, false).unwrap();
        assert_eq!(planned(&plan), ["gone"]);

        let (plan, _) = plan_from(&assessment, &all, true).unwrap();
        assert_eq!(planned(&plan), ["gone", "maybe"]);

        let chosen = HashSet::from([root.join("maybe")]);
        let (plan, _) = plan_from(&assessment, &chosen, true).unwrap();
        assert_eq!(planned(&plan), ["maybe"]);
    }
}
