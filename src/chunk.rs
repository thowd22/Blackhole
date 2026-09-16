//! Split item text into overlapping chunks for embedding.
//! Paragraph-aware: paragraphs are packed up to a word budget, and long
//! paragraphs are cut with an overlap so a sentence at a boundary is still
//! found by either side. Line breaks survive inside a chunk: a résumé's job
//! headers and a form's rows only mean something as lines.

pub const TARGET_WORDS: usize = 100;
pub const OVERLAP_WORDS: usize = 20;
pub const MAX_CHUNKS: usize = 600;

/// A word and the separator that preceded it in the source (" ", "\n" or "\n\n").
type Tok<'a> = (&'a str, &'a str);

fn join(cur: &[Tok]) -> String {
    let mut s = String::new();
    for (i, (w, sep)) in cur.iter().enumerate() {
        if i > 0 {
            s.push_str(sep);
        }
        s.push_str(w);
    }
    s
}

pub fn chunk(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut cur: Vec<Tok> = Vec::new();

    let flush = |cur: &mut Vec<Tok>, out: &mut Vec<String>| {
        if !cur.is_empty() {
            out.push(join(cur));
            let keep = cur.len().saturating_sub(OVERLAP_WORDS);
            cur.drain(..keep);
        }
    };

    for para in text.split("\n\n") {
        let mut words: Vec<Tok> = Vec::new();
        for (li, line) in para.lines().enumerate() {
            for (wi, w) in line.split_whitespace().enumerate() {
                let sep = if wi > 0 { " " } else if li > 0 { "\n" } else { "\n\n" };
                words.push((w, sep));
            }
        }
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
            out.push(join(&cur));
        }
    }
    out
}

/// The text from its `n`-th word on, keeping the separator that preceded it
/// (so merged chunks keep their line breaks).
pub fn skip_words(text: &str, n: usize) -> &str {
    let mut seen = 0;
    let mut in_word = false;
    let mut cut = text.len();
    for (i, c) in text.char_indices() {
        if c.is_whitespace() {
            if in_word {
                in_word = false;
                if seen == n {
                    cut = i;
                    break;
                }
            }
        } else if !in_word {
            in_word = true;
            seen += 1;
        }
    }
    &text[cut..]
}
