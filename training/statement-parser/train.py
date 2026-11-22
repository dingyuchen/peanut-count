"""Train a baseline that labels text lines from bank or brokerage statements."""

import argparse
from collections import Counter
import json
import math
from pathlib import Path
import re

LABELS = {"header", "transaction", "other"}


def tokens(text):
    # Keep statement punctuation and normalize changing values such as dates and totals.
    normalized = re.sub(r"\d+", "0", text.casefold())
    words = re.findall(r"[^\W\d_]+|0|[^\w\s]", normalized)
    if re.match(r"^\s*0[-/.]0(?:[-/.]0)?\s", normalized):
        words.append("<leading-date>")
    return words


def read_examples(path):
    with path.open(encoding="utf-8") as source:
        for line_number, line in enumerate(source, 1):
            try:
                row = json.loads(line)
                text, label = row["text"], row["label"]
                if not isinstance(text, str) or not text.strip() or label not in LABELS:
                    raise ValueError("expected nonempty text and a supported label")
            except (ValueError, KeyError, TypeError) as error:
                raise ValueError(f"{path}:{line_number}: {error}") from error
            yield text, label


def train(examples):
    documents = Counter()
    words = {label: Counter() for label in sorted(LABELS)}
    for text, label in examples:
        documents[label] += 1
        words[label].update(tokens(text))
    if set(documents) != LABELS:
        raise ValueError("training data must contain examples of all three labels")
    return {"version": 1, "documents": dict(documents), "words": words}


def evaluate(model, examples):
    words = model["words"]
    vocabulary = set().union(*(counts.keys() for counts in words.values()))
    denominators = {
        label: sum(counts.values()) + len(vocabulary) for label, counts in words.items()
    }
    correct = total = 0
    for text, expected in examples:
        known = [word for word in tokens(text) if word in vocabulary]
        predicted = max(
            words,
            key=lambda label: math.log(model["documents"][label])
            + sum(math.log((words[label][word] + 1) / denominators[label]) for word in known),
        )
        total += 1
        correct += predicted == expected
    if total == 0:
        raise ValueError("evaluation data is empty")
    return {"correct": correct, "total": total, "accuracy": correct / total}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--train", type=Path, required=True)
    parser.add_argument("--eval", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    try:
        if len({path.resolve() for path in (args.train, args.eval, args.output)}) != 3:
            raise ValueError("training, evaluation, and output paths must be different")
        model = train(read_examples(args.train))
        metrics = evaluate(model, read_examples(args.eval))
        args.output.parent.mkdir(parents=True, exist_ok=True)
        with args.output.open("x", encoding="utf-8") as destination:
            json.dump(model, destination, indent=2, sort_keys=True)
            destination.write("\n")
    except (OSError, ValueError) as error:
        parser.exit(1, f"{error}\n")
    print(json.dumps(metrics, sort_keys=True))


if __name__ == "__main__":
    main()
