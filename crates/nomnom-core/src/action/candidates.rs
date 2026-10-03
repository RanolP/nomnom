//! Joining an assessment to a plan: which verdicts become delete actions.
//!
//! The user approves rules, not files: approving `built-in [Cargo target/]`
//! picks every candidate that rule produced, minus the persisted
//! [`Exclusions`]. A path named on its own is picked too. Nothing else is.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use super::ActionError;
use super::exclusions::Exclusions;
use super::plan::{Action, Justification, Plan};
use crate::verdict::{Assessment, Disposition, Entry, Provenance};

/// The entries a user may pick for a plan: every `reclaimable` one, and every
/// `review` one when `include_review`, in the assessment's group order.
///
/// A candidate is only offered. Nothing reaches a plan until the user approves
/// its rule or names it in the [`Approval`] [`plan_from`] takes.
pub fn candidates(assessment: &Assessment, include_review: bool) -> Vec<&Entry> {
    assessment
        .groups
        .iter()
        .flat_map(|group| &group.entries)
        .filter(|entry| included(entry.verdict.disposition, include_review))
        .collect()
}

/// What the user explicitly picked. Empty plans nothing.
///
/// Per assessment: a front-end clears it when a new assessment lands, since a
/// rule approved against other matches is not an approval of these.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Approval {
    /// Rules whose every candidate is picked.
    pub rules: BTreeSet<Provenance>,
    /// Candidates picked one by one.
    pub paths: BTreeSet<PathBuf>,
}

impl Approval {
    pub fn is_empty(&self) -> bool {
        self.rules.is_empty() && self.paths.is_empty()
    }

    fn picks(&self, entry: &Entry) -> bool {
        self.rules.contains(&entry.verdict.provenance)
            || self.paths.contains(Path::new(&entry.path))
    }
}

/// One rule's candidates, as the user approves them.
pub struct RuleGroup<'a> {
    pub provenance: &'a Provenance,
    /// Biggest first.
    pub entries: Vec<&'a Entry>,
    /// The entries' own sizes summed; rule targets never nest, so this is
    /// what approving the whole rule would plan before exclusions.
    pub bytes: u64,
}

/// `candidates` grouped by the rule that produced them, biggest rule first.
pub fn by_rule<'a>(candidates: &[&'a Entry]) -> Vec<RuleGroup<'a>> {
    let mut groups: Vec<RuleGroup<'a>> = Vec::new();
    for &entry in candidates {
        let provenance = &entry.verdict.provenance;
        match groups.iter_mut().find(|group| group.provenance == provenance) {
            Some(group) => {
                group.bytes += entry.bytes;
                group.entries.push(entry);
            }
            None => groups.push(RuleGroup { provenance, entries: vec![entry], bytes: entry.bytes }),
        }
    }
    for group in &mut groups {
        group.entries.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.path.cmp(&b.path)));
    }
    groups.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.provenance.cmp(b.provenance)));
    groups
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum RuleLookupError {
    #[error("no rule `{0}` has candidates here")]
    Unknown(String),
    #[error("`{text}` names a rule in more than one pack; write one of: {}", .candidates.join(", "))]
    Ambiguous { text: String, candidates: Vec<String> },
}

/// The rule among `groups` that `text` names: its full `pack [Title]` form, or
/// its bare title when exactly one pack has a rule by that title.
pub fn find_rule(groups: &[RuleGroup<'_>], text: &str) -> Result<Provenance, RuleLookupError> {
    let text = text.trim();
    if let Some(group) = groups.iter().find(|group| group.provenance.to_string() == text) {
        return Ok(group.provenance.clone());
    }
    let titled: Vec<&Provenance> = groups
        .iter()
        .map(|group| group.provenance)
        .filter(|provenance| provenance.rule == text)
        .collect();
    match titled.as_slice() {
        [] => Err(RuleLookupError::Unknown(text.to_string())),
        [one] => Ok((*one).clone()),
        many => Err(RuleLookupError::Ambiguous {
            text: text.to_string(),
            candidates: many.iter().map(|provenance| provenance.to_string()).collect(),
        }),
    }
}

/// The candidates `approval` picks that `exclusions` does not keep out, in
/// the assessment's order.
pub fn approved<'a>(
    assessment: &'a Assessment,
    approval: &Approval,
    exclusions: &Exclusions,
    include_review: bool,
) -> Vec<&'a Entry> {
    candidates(assessment, include_review)
        .into_iter()
        .filter(|entry| approval.picks(entry) && !exclusions.excludes(Path::new(&entry.path)))
        .collect()
}

/// A delete plan anchored at the assessment's root, holding exactly the
/// [`approved`] entries, biggest first.
///
/// Opt-in: an empty approval is an empty plan, and an exclusion only ever
/// removes an entry. A picked path that is not a candidate under
/// `include_review` is left out.
///
/// `Err` only when the root cannot anchor a plan. A path the guards refuse is
/// information, not a stop: the other actions are still sound, so it comes back
/// beside the plan for the caller to show.
pub fn plan_from(
    assessment: &Assessment,
    approval: &Approval,
    exclusions: &Exclusions,
    include_review: bool,
) -> Result<(Plan, Vec<(PathBuf, ActionError)>), ActionError> {
    let mut plan = Plan::new(&assessment.root)?;

    let mut chosen = approved(assessment, approval, exclusions, include_review);
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
        if let Err(error) = plan.push(Action::Delete { path: path.clone() }, entry.bytes, justification)
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
    use crate::verdict::{Group, Label, Verdict};

    fn entry(path: &Path, disposition: Disposition, rule: &str) -> Entry {
        std::fs::create_dir_all(path).unwrap();
        Entry {
            path: path.display().to_string(),
            bytes: 1,
            verdict: Verdict {
                label: Label::CACHE,
                disposition,
                confidence: 1.0,
                reason: "test".into(),
                provenance: Provenance::new("p", rule),
                capped: None,
            },
            reach: None,
        }
    }

    fn assessment(root: &Path, entries: Vec<Entry>) -> Assessment {
        Assessment {
            root: root.to_path_buf(),
            groups: vec![Group { label: Label::CACHE, bytes: entries.len() as u64, entries }],
            reclaimable_bytes: 0,
            shared: Vec::new(),
        }
    }

    fn planned(plan: &Plan) -> Vec<String> {
        let mut names: Vec<String> = plan
            .actions()
            .iter()
            .map(|e| {
                let path = e.action.path();
                let parent = path.parent().unwrap().file_name().unwrap().to_string_lossy();
                format!("{parent}/{}", path.file_name().unwrap().to_string_lossy())
            })
            .collect();
        names.sort();
        names
    }

    fn rules(names: &[&str]) -> Approval {
        Approval {
            rules: names.iter().map(|rule| Provenance::new("p", *rule)).collect(),
            paths: BTreeSet::new(),
        }
    }

    // Catches a plan filling itself with every candidate the user never picked,
    // a `review` verdict reaching a plan the user never widened with
    // --include-review, and a `keep` verdict being plannable at all.
    #[test]
    fn only_picked_candidates_plan_and_review_needs_opt_in() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let assessment = assessment(
            &root,
            vec![
                entry(&root.join("a").join("gone"), Disposition::Reclaimable, "r"),
                entry(&root.join("a").join("maybe"), Disposition::Review, "r"),
                entry(&root.join("a").join("kept"), Disposition::Keep, "r"),
            ],
        );
        let none = Exclusions::default();

        let (plan, refused) = plan_from(&assessment, &Approval::default(), &none, true).unwrap();
        assert!(refused.is_empty());
        assert!(plan.is_empty());

        let (plan, _) = plan_from(&assessment, &rules(&["r"]), &none, false).unwrap();
        assert_eq!(planned(&plan), ["a/gone"]);

        let (plan, _) = plan_from(&assessment, &rules(&["r"]), &none, true).unwrap();
        assert_eq!(planned(&plan), ["a/gone", "a/maybe"]);

        let one = Approval { rules: BTreeSet::new(), paths: [root.join("a").join("maybe")].into() };
        let (plan, _) = plan_from(&assessment, &one, &none, true).unwrap();
        assert_eq!(planned(&plan), ["a/maybe"]);
    }

    // Catches approving one rule pulling in another rule's matches, or
    // dropping some of its own: approval with no exclusions must plan exactly
    // the rule's matches.
    #[test]
    fn approving_a_rule_plans_exactly_its_matches() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let assessment = assessment(
            &root,
            vec![
                entry(&root.join("one").join("target"), Disposition::Reclaimable, "Cargo target/"),
                entry(&root.join("two").join("target"), Disposition::Reclaimable, "Cargo target/"),
                entry(&root.join("web").join("node_modules"), Disposition::Reclaimable, "nm"),
            ],
        );
        let (plan, _) =
            plan_from(&assessment, &rules(&["Cargo target/"]), &Exclusions::default(), false)
                .unwrap();
        assert_eq!(planned(&plan), ["one/target", "two/target"]);
    }

    // Catches an excluded path still being planned when its rule is approved,
    // whether excluded itself or through a directory above it, including after
    // the exclusion list is reloaded from disk.
    #[test]
    fn an_excluded_path_is_not_planned_even_after_a_reload() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let assessment = assessment(
            &root,
            vec![
                entry(&root.join("one").join("target"), Disposition::Reclaimable, "t"),
                entry(&root.join("two").join("target"), Disposition::Reclaimable, "t"),
                entry(&root.join("three").join("target"), Disposition::Reclaimable, "t"),
            ],
        );
        let mut exclusions = Exclusions::default();
        exclusions.add(&root, &root.join("one").join("target")).unwrap();
        exclusions.add(&root, &root.join("two")).unwrap();
        exclusions.save(&root).unwrap();
        let exclusions = Exclusions::load(&root).unwrap();

        let (plan, _) = plan_from(&assessment, &rules(&["t"]), &exclusions, false).unwrap();
        assert_eq!(planned(&plan), ["three/target"]);

        // A path named on its own is kept out too.
        let named =
            Approval { rules: BTreeSet::new(), paths: [root.join("one").join("target")].into() };
        let (plan, _) = plan_from(&assessment, &named, &exclusions, false).unwrap();
        assert!(plan.is_empty());
    }

    // Catches `--rule "<Title>"` silently picking one pack's rule when two
    // packs share the title, and the full `pack [Title]` form not resolving.
    #[test]
    fn a_title_two_packs_share_needs_the_pack_named() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let mut a = entry(&root.join("a"), Disposition::Reclaimable, "same");
        let mut b = entry(&root.join("b"), Disposition::Reclaimable, "same");
        a.verdict.provenance = Provenance::new("alpha", "same");
        b.verdict.provenance = Provenance::new("beta", "same");
        let assessment = assessment(&root, vec![a, b]);
        let offered = candidates(&assessment, false);
        let groups = by_rule(&offered);
        assert!(matches!(find_rule(&groups, "same"), Err(RuleLookupError::Ambiguous { .. })));
        assert_eq!(find_rule(&groups, "beta [same]"), Ok(Provenance::new("beta", "same")));
        assert!(matches!(find_rule(&groups, "other"), Err(RuleLookupError::Unknown(_))));
    }
}
