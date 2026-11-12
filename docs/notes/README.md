# Notes

Audits, reviews and design investigations, kept as they were written so
the reasoning behind past decisions can be found later. They are not user
documentation and may describe code that has since changed.

Each file is named `YYYY-MM-DD-<topic>.md` after the day the work was done,
and opens with front matter giving that date, its kind (`audit`, `review`
or `design`) and the revision it was written against. A design note that
kept changing after that day gives the last commit that changed it.

| Date       | Kind   | Revision   | Note                                                                       |
| ---------- | ------ | ---------- | -------------------------------------------------------------------------- |
| 2026-10-03 | design | `9b340c53` | [Preempting a guest that computes without exits](2026-10-03-preemption.md) |
| 2026-10-09 | audit  | `376da929` | [Code audit of 0.6.0](2026-10-09-code-audit.md)                            |
| 2026-11-04 | review | `7481c985` | [Memory blame review and handoff](2026-11-04-memory-blame-review.md)       |
| 2026-11-09 | review | `a552c79d` | [Review of Rewind 0.14.0](2026-11-09-review-0.14.0.md)                     |
| 2026-11-09 | design | `67e6cdbf` | [Deterministic multicore builds](2026-11-09-multicore.md)                  |
