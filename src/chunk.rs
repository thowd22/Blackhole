//! Split item text into overlapping chunks for embedding.
//! Paragraph-aware: paragraphs are packed up to a word budget, and long
//! paragraphs are cut with an overlap so a sentence at a boundary is still
//! found by either side.

pub const TARGET_WORDS: usize = 100;
pub const OVERLAP_WORDS: usize = 20;
pub const MAX_CHUNKS: usize = 600;

pub fn chunk(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur: Vec<&str> = Vec::new();

    let flush = |cur: &mut Vec<&str>, out: &mut Vec<String>| {
        if !cur.is_empty() {
            out.push(cur.join(" "));
            let keep = cur.len().saturating_sub(OVERLAP_WORDS);
            cur.drain(..keep);
        }
    };

    for para in text.split("\n\n") {
        let words: Vec<&str> = para.split_whitespace().collect();
        if words.is_empty() {
            continue;
        }
        // A paragraph that doesn't fit into what's left starts a new chunk.
        if !cur.is_empty() && cur.len() + words.len() > TARGET_WORDS {
            flush(&mut cur, &mut out);
        }
        for w in words {
            cur.push(w);
            if cur.len() >= TARGET_WORDS {
                flush(&mut cur, &mut out);
                if out.len() >= MAX_CHUNKS {
                    return out;
                }
            }
        }
    }
    // Tail: only keep it if it adds words beyond the overlap already emitted.
    if cur.len() > OVERLAP_WORDS || out.is_empty() {
        if !cur.is_empty() {
            out.push(cur.join(" "));
        }
    }
    out
}
