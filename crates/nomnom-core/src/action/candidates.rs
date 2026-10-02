//! Joining an assessment to a plan: which verdicts become trash actions.

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use super::ActionError;
use super::plan::{Action, Justification, Plan};
use crate::verdict::{Assessment, Disposition, Entry};

/// A trash plan anchored at the assessment's root, holding every
/// `reclaimable` entry — and every `review` one when `include_review` — biggest
/// first.
///
/// `selection`, when given, restricts the plan to entries whose path is in the
/// set; `None` takes every entry the dispositions allow.
///
/// `Err` only when the root cannot anchor a plan. A path the guards refuse is
/// information, not a stop: the other actions are still sound, so it comes back
/// beside the plan for the caller to show.
pub fn plan_from(
    assessment: &Assessment,
    selection: Option<&HashSet<PathBuf>>,
    include_review: bool,
) -> Result<(Plan, Vec<(PathBuf, ActionError)>), ActionError> {
    let mut plan = Plan::new(&assessment.root)?;

    let mut candidates: Vec<&Entry> = assessment
        .groups
        .iter()
        .flat_map(|group| &group.entries)
        .filter(|entry| included(entry.verdict.disposition, include_review))
        .filter(|entry| selection.is_none_or(|chosen| chosen.contains(Path::new(&entry.path))))
        .collect();
    // By the path's text rather than `Path`'s component order: the order a
    // printed plan has always listed ties in.
    candidates.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.path.cmp(&b.path)));

    let mut refused = Vec::new();
    for entry in candidates {
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
                unit: true,
                provenance: Provenance::new("p", "r"),
                capped: None,
            },
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

    // Catches a `review` verdict reaching a plan the user never widened with
    // --include-review, and a GUI selection being ignored.
    #[test]
    fn review_needs_opt_in_keep_never_plans_and_selection_restricts() {
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
        };

        let (plan, refused) = plan_from(&assessment, None, false).unwrap();
        assert!(refused.is_empty());
        assert_eq!(planned(&plan), ["gone"]);

        let (plan, _) = plan_from(&assessment, None, true).unwrap();
        assert_eq!(planned(&plan), ["gone", "maybe"]);

        let chosen = HashSet::from([root.join("maybe")]);
        let (plan, _) = plan_from(&assessment, Some(&chosen), true).unwrap();
        assert_eq!(planned(&plan), ["maybe"]);
    }
}
