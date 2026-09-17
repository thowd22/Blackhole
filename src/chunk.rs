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

#[cfg(test)]
mod tests {
    use super::*;

    fn words(n: usize) -> String {
        (0..n).map(|i| format!("w{i}")).collect::<Vec<_>>().join(" ")
    }

    #[test]
    fn short_text_is_one_chunk_verbatim() {
        let t = "one two three";
        assert_eq!(chunk(t), vec![t.to_string()]);
    }

    #[test]
    fn line_breaks_survive_inside_a_chunk() {
        // A résumé's job headers only mean anything as lines.
        let t = "Maxar Technologies\nPrincipal MLOps Engineer\n10/2021 - Present";
        assert_eq!(chunk(t), vec![t.to_string()]);
    }

    #[test]
    fn paragraph_breaks_survive_inside_a_chunk() {
        let t = "first para line\n\nsecond para line";
        let out = chunk(t);
        assert_eq!(out.len(), 1);
        assert!(out[0].contains("\n\n"), "paragraph separator lost: {:?}", out[0]);
    }

    #[test]
    fn runs_of_whitespace_collapse_but_structure_does_not() {
        // Runs of spaces and tabs become one space; a blank line becomes a break.
        assert_eq!(chunk("a   b\t c\n\n\nd"), vec!["a b c\nd".to_string()]);
        assert_eq!(chunk("a\n\nb"), vec!["a\n\nb".to_string()]);
    }

    #[test]
    fn long_text_splits_at_the_word_budget() {
        let out = chunk(&words(250));
        assert!(out.len() >= 2, "expected several chunks, got {}", out.len());
        for c in &out[..out.len() - 1] {
            assert_eq!(c.split_whitespace().count(), TARGET_WORDS);
        }
    }

    #[test]
    fn consecutive_chunks_overlap_by_overlap_words() {
        let out = chunk(&words(300));
        for pair in out.windows(2) {
            let prev: Vec<&str> = pair[0].split_whitespace().collect();
            let next: Vec<&str> = pair[1].split_whitespace().collect();
            let tail = &prev[prev.len() - OVERLAP_WORDS..];
            assert_eq!(&next[..OVERLAP_WORDS], tail, "chunks must share the last {OVERLAP_WORDS} words");
        }
    }

    #[test]
    fn nothing_is_lost_across_the_boundary() {
        let out = chunk(&words(250));
        let mut seen: Vec<&str> = out[0].split_whitespace().collect();
        for c in &out[1..] {
            seen.extend(c.split_whitespace().skip(OVERLAP_WORDS));
        }
        assert_eq!(seen, words(250).split_whitespace().collect::<Vec<_>>());
    }

    #[test]
    fn a_tail_that_is_only_overlap_is_dropped_not_duplicated() {
        // Exactly one chunk's worth: the 20 words held back for the next chunk add
        // nothing new, so they are not emitted again.
        assert_eq!(chunk(&words(TARGET_WORDS)).len(), 1);
        // Five more words do earn a (short, overlapping) second chunk.
        let out = chunk(&words(TARGET_WORDS + 5));
        assert_eq!(out.len(), 2);
        assert_eq!(out[1].split_whitespace().count(), OVERLAP_WORDS + 5);
    }

    #[test]
    fn a_paragraph_that_does_not_fit_starts_a_new_chunk() {
        let t = format!("{}\n\n{}", words(60), words(60));
        let out = chunk(&t);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].split_whitespace().count(), 60);
    }

    #[test]
    fn empty_text_yields_no_chunk_and_no_panic() {
        assert!(chunk("").is_empty() || chunk("") == vec![String::new()]);
        assert!(chunk("   \n\n  ").is_empty() || chunk("   \n\n  ") == vec![String::new()]);
    }

    #[test]
    fn chunk_count_is_capped() {
        let out = chunk(&words(TARGET_WORDS * (MAX_CHUNKS + 20)));
        assert!(out.len() <= MAX_CHUNKS, "got {} chunks", out.len());
    }

    #[test]
    fn skip_words_keeps_the_separator_that_preceded_the_word() {
        assert_eq!(skip_words("a b\nc d", 2), "\nc d");
        assert_eq!(skip_words("a b\n\nc", 2), "\n\nc");
        assert_eq!(skip_words("a b c", 1), " b c");
        // Past the end, and the n = 0 edge (no separator precedes the first word):
        // the only caller passes OVERLAP_WORDS, which is never 0.
        assert_eq!(skip_words("a b c", 9), "");
        assert_eq!(skip_words("a b c", 0), "");
    }
}
