//! Client-side filler-word removal.
//!
//! AssemblyAI's `disfluencies` boolean is a no-op on the Universal-3 Pro model (the default for
//! English): U3P controls disfluencies through prompting, and its default "clean" output is only
//! probabilistic — residual fillers like a sentence-initial "Uh," slip through. The prompt-based
//! control can't be used either, since `prompt` and `keyterms_prompt` (our vocabulary boosting)
//! are mutually exclusive. So when filler removal is on we strip a closed set of filler tokens
//! here: deterministic, free, and independent of provider/model.

/// Standalone filler tokens, matched case-insensitively as whole words (internal hyphens kept, so
/// "uh-huh" matches). Deliberately conservative — single letters and ambiguous interjections
/// ("ah", "huh", "mm") are excluded to avoid eating real words.
const FILLERS: &[&str] = &[
    "um", "umm", "uh", "uhh", "uh-huh", "er", "erm", "hmm", "mhm", "mm-hmm",
];

fn capitalize_first(s: &str) -> String {
    let mut chars = s.chars();
    match chars.next() {
        Some(c) => c.to_uppercase().collect::<String>() + chars.as_str(),
        None => String::new(),
    }
}

/// Remove filler words from `text`. Drops whole tokens whose alphanumeric core is a known filler
/// (e.g. "Uh," → gone), tidies the punctuation/spacing left behind, and re-capitalizes the word
/// that becomes sentence-initial. Returns the original text if nothing changed or it would empty it.
pub fn strip_fillers(text: &str) -> String {
    let kept: Vec<&str> = text
        .split_whitespace()
        .filter(|tok| {
            let core = tok
                .trim_matches(|c: char| !c.is_alphanumeric() && c != '-')
                .to_lowercase();
            !FILLERS.contains(&core.as_str())
        })
        .collect();

    if kept.len() == text.split_whitespace().count() {
        return text.to_string(); // no fillers found — leave it untouched
    }

    let mut out = kept.join(" ");
    // Tidy punctuation orphaned by a removed token (" ," → ",").
    for p in [",", ".", ";", ":", "!", "?"] {
        out = out.replace(&format!(" {p}"), p);
    }
    let out = out
        .trim_start_matches(|c: char| matches!(c, ',' | '.' | ';' | ':' | ' '))
        .trim()
        .to_string();

    if out.is_empty() {
        return text.to_string();
    }
    capitalize_first(&out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_leading_uh() {
        assert_eq!(
            strip_fillers("Uh, dispatch an agent to fix the secret CLI."),
            "Dispatch an agent to fix the secret CLI."
        );
    }

    #[test]
    fn strips_midsentence() {
        assert_eq!(strip_fillers("I think, um, we should go."), "I think, we should go.");
    }

    #[test]
    fn keeps_real_words() {
        let s = "The umbrella is uphill near Ahmed.";
        assert_eq!(strip_fillers(s), s);
    }

    #[test]
    fn handles_uh_huh() {
        assert_eq!(strip_fillers("Uh-huh, exactly."), "Exactly.");
    }
}
