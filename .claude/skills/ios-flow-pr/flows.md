# Recorded flows and standing checks

`harness/Tests/Flow.swift` records these clips in order from a fresh install, with
sync off. The stills (`stills/NN-*.png`) are the evidence for the checks. When a
change adds or alters a screen, add the steps to `Flow.swift` and add its checks
here in the same PR.

Fixtures (in `fixtures/`, regenerate with `swift make-pdf.swift <txt> <pdf>`):

- **A `sample-statement.pdf`**: 26 rows, ISO dates, Jun to Nov 2026, USD checking account.
- **B `ambiguous-dates.pdf`**: 3 rows with slash dates (`03/04/2026`, `05/06/2026`, `07/08/2026`). No day is above 12, so the order can't be inferred. It's seeded into Files as `z-ambiguous-dates.pdf`.

| Clip | Steps | Standing checks (stills) |
| --- | --- | --- |
| `01-first-launch` | Cold launch | Empty ledger shows "Import statement PDF" and the three steps; nothing is clipped (01) |
| `02-sync-sheet` | Open Sync, scroll, then Done | Storage card says the ledger is encrypted on this iPhone and sync is off; sign-in is optional (02, 03) |
| `03-pick-pdf-a` | Import, then Browse, On My iPhone, fixture A | The picker opens; Statement details appears (04, 05) |
| `04-details-iso` | Type the account and currency, then Find | ISO statement shows the YYYY-MM-DD note, not a format picker; review reports 26 rows (05 to 07) |
| `05-review` | Scroll all rows; switch Coffee Corner off and on; Add 26 | Each included row has a "Category" label and the first has a helper line; the button reads 25 and then 26 (08 to 12) |
| `06-home-chart` | Read November; tap the August column; ‹ to July; › back | Raised cents with no decimal point, top-aligned to the whole digits (within 1px at 3×); shares are 36/34/25/5 and total 100%; the list shows only the selected month; the card and list change together (12 to 17) |
| `07-pick-pdf-b-ambiguous` | Import fixture B; choose DD/MM/YYYY; type the account and currency; Add 3 | The date-format question and its hint stay visible above the keyboard while either field is focused; 3 rows are parsed as Apr 3, Jun 5 and Aug 7 (18 to 23) |
| `08-dark-mode` | Switch to dark; ‹ ‹ | The unselected columns are clearly visible against the card; text keeps its contrast (24 to 26) |

November totals for fixture A: Groceries 102.30, Transport 96.00, Dining 71.35 and
Uncategorized 15.99, for a total of USD 285.64.
