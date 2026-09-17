#!/usr/bin/env python3
"""Generate a synthetic vault for the scale test: ~400 items of mixed kinds and
lengths, plus questions whose answers are unique in the set.

Nothing here is personal: the prose is lorem-like filler, and every fact worth
asking about hangs off an invented name ("Vexlorn-217", "Ilsa Brantwood") that
occurs in exactly one item, so a question has exactly one right document and a
wrong retrieval cannot pass by luck.

    python3 eval/gen_vault.py <outdir> [items] [questions]

writes <outdir>/items.json and <outdir>/questions.json, which
`askeval --scale <vaultdir> <items.json> <questions.json>` turns into a vault
and measures (see RAG.md).
"""

import json
import random
import sys
from pathlib import Path

SEED = 20250916

# Invented, pronounceable, and not words anything else in the vault uses.
PROJECTS = [
    "Vexlorn", "Marrowgate", "Thistledown", "Quillfeather", "Bellhaven", "Corvane", "Dusklight", "Emberfall",
    "Fallowmere", "Gravemoor", "Hollowpine", "Ironvale", "Jadewick", "Kestrelby", "Lorrimer", "Mistfell",
    "Nightbourne", "Oakhurst", "Pellmoor", "Quarryside", "Ravenswood", "Silverdell", "Tarnwick", "Umbermill",
    "Vaneford", "Westmarch", "Yarrowden", "Zephyrine", "Ashcombe", "Brackenhall",
]
SURNAMES = [
    "Brantwood", "Calloway", "Duskmoor", "Everly", "Fairholm", "Glenroy", "Harrowsmith", "Islington",
    "Jarrow", "Kingsley", "Lambrick", "Merriwether", "Northcott", "Ollerton", "Pemberton", "Quintrell",
    "Ravensworth", "Stanhope", "Thornbury", "Underhill", "Vanbrugh", "Whitlock", "Yardley", "Ashford",
]
FIRSTS = ["Ilsa", "Bram", "Cora", "Dain", "Elspeth", "Fen", "Greer", "Halden", "Ivo", "Juno", "Kestrel", "Linnea"]
CITIES = ["Kobe", "Trondheim", "Valparaiso", "Gdansk", "Da Nang", "Antofagasta", "Rijeka", "Mombasa", "Tallinn", "Cartagena"]
KINDS = ["note"] * 150 + ["text"] * 120 + ["md"] * 60 + ["code"] * 40 + ["pdf"] * 30

FILLER = (
    "the quarterly review notes that throughput remained within the agreed envelope while the backlog was "
    "worked down steadily across the period under discussion and the team kept the change window short "
    "because nobody wanted another late evening spent chasing a rollback that should never have been needed "
    "in the first place and so the practice of small reversible steps continued through the season "
).split()


def filler(rng, n):
    """n words of unremarkable prose, so real terms have something to hide in."""
    start = rng.randrange(len(FILLER))
    return " ".join(FILLER[(start + i) % len(FILLER)] for i in range(n))


def paragraphs(rng, words):
    out, left = [], words
    while left > 0:
        take = min(left, rng.randrange(40, 110))
        out.append(filler(rng, take))
        left -= take
    return "\n\n".join(out)


def main():
    outdir = Path(sys.argv[1] if len(sys.argv) > 1 else "eval/scale")
    n_items = int(sys.argv[2]) if len(sys.argv) > 2 else 400
    n_questions = int(sys.argv[3]) if len(sys.argv) > 3 else 30
    rng = random.Random(SEED)
    outdir.mkdir(parents=True, exist_ok=True)

    items, facts = [], []
    for i in range(n_items):
        project = f"{PROJECTS[i % len(PROJECTS)]}-{100 + i}"
        person = f"{FIRSTS[i % len(FIRSTS)]} {SURNAMES[(i * 7) % len(SURNAMES)]}"
        city = CITIES[(i * 3) % len(CITIES)]
        code = 1000 + (i * 37) % 8999
        amount = 100 + (i * 131) % 9000
        kind = KINDS[i % len(KINDS)]
        # Three length bands: a one-liner, a page, a long document.
        band = i % 10
        words = 30 if band < 4 else (rng.randrange(150, 400) if band < 8 else rng.randrange(900, 1800))
        title = f"{project} {['brief', 'report', 'log', 'memo', 'ledger'][i % 5]}"
        fact_lines = [
            f"Project {project} is led by {person} out of the {city} office.",
            f"Its access code is {code} and the approved budget is {amount} USD.",
            f"The {project} review meets every {['Monday', 'Tuesday', 'Wednesday', 'Thursday', 'Friday'][i % 5]}.",
        ]
        body = paragraphs(rng, words // 2) + "\n\n" + "\n".join(fact_lines) + "\n\n" + paragraphs(rng, words - words // 2)
        items.append({"title": title, "kind": kind, "text": body, "tags": ["scale", kind]})
        facts.append({"title": title, "project": project, "person": person, "city": city, "code": code, "amount": amount})

    # Questions spread over the whole vault, in four shapes, each with one right answer.
    picks = [facts[i] for i in rng.sample(range(n_items), n_questions)]
    questions = []
    for i, f in enumerate(picks):
        shape = i % 4
        if shape == 0:
            q = f"What is the access code for project {f['project']}?"
            expect = str(f["code"])
        elif shape == 1:
            q = f"Who leads project {f['project']}?"
            expect = f["person"]
        elif shape == 2:
            q = f"Which office runs {f['project']}?"
            expect = f["city"]
        else:
            q = f"How big is the approved budget for {f['project']}?"
            expect = str(f["amount"])
        questions.append({"q": q, "expect_title": f["title"], "expect": [expect]})

    (outdir / "items.json").write_text(json.dumps(items, indent=1))
    (outdir / "questions.json").write_text(json.dumps(questions, indent=1))
    words = sum(len(i["text"].split()) for i in items)
    print(f"{len(items)} items ({words} words, {words // len(items)} avg), {len(questions)} questions -> {outdir}")


if __name__ == "__main__":
    main()
