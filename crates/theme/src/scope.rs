//! TextMate scope selectors, and the specificity rule that resolves them.
//!
//! A VS Code theme's `tokenColors` is a list of rules, each naming one or more scopes and a
//! colour. Asking "what colour is `entity.other.attribute-name`?" is not a lookup, because
//! several rules can match one scope: `entity`, `entity.other`, and
//! `entity.other.attribute-name` are all matches, and they may name three different
//! colours.
//!
//! **Longest match wins.** That is the whole rule, and getting it wrong is not hypothetical
//! here: a script written during #53 reported One Dark Pro's attribute colour as `#e06c75`,
//! because it walked the rule list and took the first scope that matched. `entity.name.tag`
//! is listed before `entity.other.attribute-name` in that file, and a first-hit search
//! finds the tag rule and stops. The published colour is `#d19a66`. The bug was not in the
//! matching, it was in preferring file order over specificity — so specificity is what this
//! module sorts on, and file order is only a tie-break.

/// One `scope: colour` pair, flattened out of a `tokenColors` entry.
///
/// Flattened because a rule may name a list of scopes and each one competes separately —
/// `["comment", "punctuation.definition.comment"]` is two selectors that happen to share a
/// colour, and treating them as one unit would mean the specificity of the first deciding
/// for the second.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Rule {
    /// The selector as written, e.g. `entity.other.attribute-name` or `string variable`.
    pub selector: String,
    /// The `foreground` the rule sets, `#rrggbb`. Rules with no foreground never get here.
    pub foreground: String,
}

/// How specifically a selector matches a scope, or `None` if it does not match.
///
/// The number is the count of dot-separated segments in the selector, which is exactly the
/// "longest match" the TextMate rule means: `entity.other.attribute-name` scores 3 and beats
/// `entity`'s 1 for the same scope.
///
/// **Descendant selectors are not matched.** `string variable` means "a `variable` inside a
/// `string`", and answering that needs the *stack* of scopes at a position in a real
/// document — which an importer resolving one scope name in the abstract does not have.
/// Scoring them on their last segment would let `string variable` (`#79c0ff` in GitHub
/// Dark) win the general `variable` question, which is a colour that only ever applies
/// inside a string. Skipped deliberately rather than approximated: a wrong answer that
/// looks confident is worse than falling back.
fn specificity(selector: &str, scope: &str) -> Option<usize> {
    // A selector with whitespace is a descendant path; see above.
    if selector.split_whitespace().count() != 1 {
        return None;
    }

    // `scope` is covered by `selector` when the selector is the scope or a prefix of it on
    // a segment boundary. The boundary check is what stops `entity.name` matching a scope
    // called `entity.nameless`.
    if scope == selector || scope.starts_with(selector) && scope[selector.len()..].starts_with('.')
    {
        Some(selector.split('.').count())
    } else {
        None
    }
}

/// The colour a theme gives one scope, by the longest-match rule.
///
/// Ties — two rules of equal specificity matching the same scope — go to the one listed
/// first, which is what VS Code does and the only part of this where file order is allowed
/// to matter.
pub fn resolve<'a>(rules: &'a [Rule], scope: &str) -> Option<&'a str> {
    rules
        .iter()
        .filter_map(|rule| Some((specificity(&rule.selector, scope)?, rule)))
        // `max_by_key` on a tie keeps the *last* element, so the iterator is reversed to
        // turn that into the first. Cheaper and clearer than carrying an index.
        .rev()
        .max_by_key(|(specificity, _)| *specificity)
        .map(|(_, rule)| rule.foreground.as_str())
}

/// The colour a theme gives the first of several candidate scopes that it styles at all.
///
/// The candidate list is how one of this editor's styles names the scopes it could come
/// from — `type` is `entity.name.type` in One Dark Pro and has no rule of its own in
/// GitHub Dark, where the answer is the broader `entity`. Order is preference: the first
/// candidate the theme has any opinion about wins, and a later candidate is a fallback, not
/// a competitor.
///
/// **Preference beats specificity across candidates, and that is deliberate.** Picking the
/// most specific match over the whole list instead would mean a theme that styles a
/// fallback scope precisely and the preferred scope broadly gets the fallback — which is
/// the wrong answer to "what colour is a type?" when the theme has said something about
/// types. Within one candidate, specificity decides; between candidates, the mapping's own
/// order does.
pub fn resolve_any<'a>(rules: &'a [Rule], candidates: &[&str]) -> Option<&'a str> {
    candidates.iter().find_map(|scope| resolve(rules, scope))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(pairs: &[(&str, &str)]) -> Vec<Rule> {
        pairs
            .iter()
            .map(|(selector, foreground)| Rule {
                selector: selector.to_string(),
                foreground: foreground.to_string(),
            })
            .collect()
    }

    /// The #53 bug, as a test.
    ///
    /// These are One Dark Pro's real rules in One Dark Pro's real order: the tag rule is
    /// listed first, and a first-hit search returns its `#e06c75`. The published attribute
    /// colour is `#d19a66`, and only the specificity rule gets there.
    #[test]
    fn a_longer_selector_wins_over_one_listed_earlier() {
        let rules = rules(&[
            ("entity.name.tag", "#e06c75"),
            ("entity", "#e5c07b"),
            ("entity.other.attribute-name", "#d19a66"),
        ]);

        assert_eq!(resolve(&rules, "entity.other.attribute-name"), Some("#d19a66"));
    }

    #[test]
    fn a_scope_with_no_exact_rule_falls_to_its_nearest_ancestor() {
        // GitHub Dark has no `entity.other.attribute-name`; `entity` is the answer.
        let rules = rules(&[("entity", "#79c0ff"), ("comment", "#8b949e")]);

        assert_eq!(resolve(&rules, "entity.other.attribute-name"), Some("#79c0ff"));
    }

    #[test]
    fn an_unstyled_scope_resolves_to_nothing_rather_than_to_a_guess() {
        let rules = rules(&[("comment", "#8b949e")]);
        assert_eq!(resolve(&rules, "keyword.operator"), None);
    }

    /// The boundary check. Without it, `entity.name` would claim a scope that merely starts
    /// with those letters.
    #[test]
    fn a_prefix_that_is_not_a_whole_segment_does_not_match() {
        let rules = rules(&[("entity.name", "#ffa657")]);

        assert_eq!(resolve(&rules, "entity.nameless.thing"), None);
        assert_eq!(resolve(&rules, "entity.name.function"), Some("#ffa657"), "a real segment does");
    }

    #[test]
    fn equal_specificity_goes_to_whichever_rule_is_listed_first() {
        let rules = rules(&[("keyword", "#ff7b72"), ("keyword", "#000000")]);
        assert_eq!(resolve(&rules, "keyword"), Some("#ff7b72"));
    }

    /// A descendant selector answers a question this importer is not asking.
    ///
    /// `string variable` is GitHub Dark's colour for a variable *interpolated into a
    /// string*. Letting it answer the general `variable` question would paint every
    /// variable in the file that colour.
    #[test]
    fn a_descendant_selector_never_answers_a_bare_scope() {
        let rules = rules(&[("string variable", "#79c0ff"), ("variable", "#ffa657")]);

        assert_eq!(resolve(&rules, "variable"), Some("#ffa657"));
    }

    #[test]
    fn a_descendant_selector_alone_resolves_to_nothing() {
        let rules = rules(&[("string variable", "#79c0ff")]);
        assert_eq!(resolve(&rules, "variable"), None);
    }

    #[test]
    fn the_first_candidate_the_theme_styles_wins() {
        let rules = rules(&[("entity", "#79c0ff"), ("storage.type", "#ff7b72")]);

        // `entity.name.type` has no rule, but `entity` covers it, so the first candidate
        // answers and `storage.type` is never consulted.
        assert_eq!(resolve_any(&rules, &["entity.name.type", "storage.type"]), Some("#79c0ff"));
    }

    #[test]
    fn a_later_candidate_answers_when_the_theme_is_silent_on_the_first() {
        let rules = rules(&[("storage.type", "#ff7b72")]);
        assert_eq!(resolve_any(&rules, &["entity.name.type", "storage.type"]), Some("#ff7b72"));
    }

    #[test]
    fn no_candidate_matching_is_not_an_error() {
        let rules = rules(&[("comment", "#8b949e")]);
        assert_eq!(resolve_any(&rules, &["entity.name.type", "storage.type"]), None);
    }
}
