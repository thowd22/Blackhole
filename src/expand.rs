//! Query expansion without an LLM: a small synonym table for words users type
//! that documents rarely use literally. Applied to the DENSE query text only
//! (keyword-side expansion measured zero effect), only in ask mode, and only
//! for synonyms that actually occur somewhere in the vault — unanchored
//! synonyms measurably raised the scores of unanswerable questions.
//!
//! Evidence is thin (the gain came from job-title words matching one résumé),
//! so this is a latency buy for the reranker (k=10 instead of k=20), not a
//! retrieval fix in its own right.

const MAX_TERMS: usize = 12;
const MIN_QUERY_WORDS: usize = 3;

/// Sorted by key for binary search.
static SYNONYMS: &[(&str, &[&str])] = &[
    ("bill", &["total", "paid", "amount", "invoice"]),
    ("boat", &["vessel", "voyage", "carrier", "ship"]),
    ("broker", &["brokers", "customs", "filer"]),
    ("car", &["vehicle", "unit", "vin"]),
    ("career", &["engineer", "manager", "experience", "senior", "principal"]),
    ("checkout", &["git", "github", "clone"]),
    ("clone", &["git", "github"]),
    ("cost", &["total", "amount", "fee", "paid"]),
    ("cv", &["engineer", "experience", "senior", "principal"]),
    ("employed", &["engineer", "manager", "administrator", "architect", "experience", "senior", "principal"]),
    ("employer", &["engineer", "manager", "administrator", "architect", "analyst", "director", "technologies", "inc"]),
    ("employers", &["engineer", "manager", "administrator", "architect", "analyst", "director", "technologies", "inc"]),
    ("employment", &["engineer", "manager", "administrator", "architect", "experience", "senior", "principal"]),
    ("fees", &["fee", "charge", "total"]),
    ("import", &["entry", "customs", "shipment", "lading"]),
    ("invoice", &["total", "paid", "amount", "payment"]),
    ("job", &["engineer", "manager", "administrator", "architect", "analyst", "senior", "principal"]),
    ("jobs", &["engineer", "manager", "administrator", "architect", "analyst", "senior", "principal"]),
    ("pay", &["paid", "payment", "amount", "total"]),
    ("port", &["arrival", "discharge", "terminal", "lading"]),
    ("receipt", &["total", "paid", "amount", "payment"]),
    ("repo", &["git", "github"]),
    ("repository", &["git", "github"]),
    ("resume", &["engineer", "experience", "senior", "principal"]),
    ("salary", &["pay", "compensation", "wage", "income"]),
    ("ship", &["vessel", "voyage", "carrier"]),
    ("ssh", &["git", "github"]),
    ("vessel", &["voyage", "carrier"]),
    ("work", &["engineer", "manager", "administrator", "architect", "senior", "principal"]),
    ("worked", &["engineer", "manager", "administrator", "architect", "senior", "principal"]),
    ("working", &["engineer", "manager", "administrator", "architect", "senior", "principal"]),
];

/// Synonyms for the query's words, deduplicated against the query and each
/// other, keeping only those `in_vault` accepts.
pub fn expand(query: &str, in_vault: impl Fn(&str) -> bool) -> Vec<&'static str> {
    let terms: Vec<String> = query.split(|c: char| !c.is_alphanumeric()).filter(|t| !t.is_empty()).map(|t| t.to_lowercase()).collect();
    if terms.len() < MIN_QUERY_WORDS {
        return Vec::new();
    }
    let mut out: Vec<&'static str> = Vec::new();
    for t in &terms {
        let Ok(i) = SYNONYMS.binary_search_by(|(k, _)| k.cmp(&t.as_str())) else { continue };
        for &syn in SYNONYMS[i].1 {
            if terms.iter().any(|q| q == syn) || out.contains(&syn) || !in_vault(syn) {
                continue;
            }
            out.push(syn);
            if out.len() >= MAX_TERMS {
                return out;
            }
        }
    }
    out
}

/// The text to embed for dense retrieval: the question plus its expansions.
pub fn dense_query(query: &str, expansions: &[&str]) -> String {
    if expansions.is_empty() {
        query.to_string()
    } else {
        format!("{query} {}", expansions.join(" "))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every synonym is in the vault.
    fn all(_: &str) -> bool {
        true
    }
    /// Nothing is.
    fn none(_: &str) -> bool {
        false
    }

    #[test]
    fn table_is_sorted_for_binary_search() {
        for pair in SYNONYMS.windows(2) {
            assert!(pair[0].0 < pair[1].0, "{} must sort before {}", pair[0].0, pair[1].0);
        }
    }

    #[test]
    fn expands_a_known_word() {
        let out = expand("where did I work last", all);
        assert!(out.contains(&"engineer"), "{out:?}");
        assert!(out.contains(&"manager"), "{out:?}");
    }

    #[test]
    fn short_queries_are_left_alone() {
        // Two words or fewer: too little context for the table to help.
        assert!(expand("my job", all).is_empty());
        assert!(expand("resume", all).is_empty());
        assert_eq!(expand("what is my job", all).is_empty(), false);
    }

    #[test]
    fn unanchored_synonyms_are_dropped() {
        // A synonym that occurs nowhere in the vault measurably raised the score of
        // unanswerable questions, so `in_vault` is the gate.
        assert!(expand("where did I work last", none).is_empty());
    }

    #[test]
    fn only_some_synonyms_may_be_in_the_vault() {
        let out = expand("where did I work last", |t| t == "manager");
        assert_eq!(out, vec!["manager"]);
    }

    #[test]
    fn words_already_in_the_query_are_not_repeated() {
        let out = expand("which engineer job did I have", all);
        assert!(!out.contains(&"engineer"), "{out:?}");
        assert!(out.contains(&"manager"), "{out:?}");
    }

    #[test]
    fn synonyms_are_deduplicated_across_query_words() {
        let out = expand("my job and my work history", all);
        let mut sorted = out.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(sorted.len(), out.len(), "duplicate expansions: {out:?}");
    }

    #[test]
    fn expansion_is_capped() {
        let out = expand("my job my work my employer my career and employment", all);
        assert!(out.len() <= MAX_TERMS, "{} terms", out.len());
    }

    #[test]
    fn unknown_words_expand_to_nothing() {
        assert!(expand("what colour is the accretion disk", all).is_empty());
    }

    #[test]
    fn punctuation_and_case_do_not_hide_a_word() {
        let out = expand("What was my SALARY, roughly?", all);
        assert!(out.contains(&"compensation"), "{out:?}");
    }

    #[test]
    fn dense_query_appends_expansions_only_when_there_are_some() {
        assert_eq!(dense_query("my job", &[]), "my job");
        assert_eq!(dense_query("my job", &["engineer", "manager"]), "my job engineer manager");
    }
}
