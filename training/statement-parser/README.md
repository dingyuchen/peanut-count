# Statement parser training

This directory contains a dependency-free Python 3 baseline for classifying
text rows from bank and brokerage statements. It labels rows as headers,
transactions, or other text. Row classification is the first stage of parsing.
PDF and OCR ingestion, field extraction, and posting to the ledger are future work.

Run from this directory:

```sh
python3 train.py --train examples/train.jsonl --eval examples/eval.jsonl --output artifacts/baseline.json
```

The command prints held-out accuracy and writes a versioned JSON model.
It requires a new output path so previous artifacts are preserved. The
checked-in inputs are synthetic smoke fixtures.

Each JSONL record has a `text` field and a `label` of `header`,
`transaction`, or `other`. For a real dataset, split by statement before
extracting rows so rows from one statement stay in the same split. Keep
private source statements and labeled examples under the ignored `data/`
directory. Keep trained models under the ignored `artifacts/` directory,
since this baseline stores token frequencies from the input text.
