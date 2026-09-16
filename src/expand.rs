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
    ("cost", &["total", "amount", "fee", "paid"]),
    ("cv", &["engineer", "experience", "senior", "principal"]),
    ("employer", &["engineer", "manager", "administrator", "architect", "analyst", "director", "technologies", "inc"]),
    ("employers", &["engineer", "manager", "administrator", "architect", "analyst", "director", "technologies", "inc"]),
    ("fees", &["fee", "charge", "total"]),
    ("import", &["entry", "customs", "shipment", "lading"]),
    ("invoice", &["total", "paid", "amount", "payment"]),
    ("job", &["engineer", "manager", "administrator", "architect", "analyst", "senior", "principal"]),
    ("jobs", &["engineer", "manager", "administrator", "architect", "analyst", "senior", "principal"]),
    ("pay", &["paid", "payment", "amount", "total"]),
    ("port", &["arrival", "discharge", "terminal", "lading"]),
    ("receipt", &["total", "paid", "amount", "payment"]),
    ("resume", &["engineer", "experience", "senior", "principal"]),
    ("salary", &["pay", "compensation", "wage", "income"]),
    ("ship", &["vessel", "voyage", "carrier"]),
    ("vessel", &["voyage", "carrier"]),
    ("work", &["engineer", "manager", "administrator", "architect", "senior", "principal"]),
    ("worked", &["engineer", "manager", "administrator", "architect", "senior", "principal"]),
    ("working", &["engineer", "manager", "administrator", "architect", "senior", "principal"]),
];

/// Synonyms for the query's words, deduplicated against the query and each
/// other, keeping only those `in_vault` accepts.
pub fn expand(query: &str, in_vault: impl Fn(&str) -> bool) -> Vec<&'static str> {
    let terms: Vec<String> = query
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| !t.is_empty())
        .map(|t| t.to_lowercase())
        .collect();
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
