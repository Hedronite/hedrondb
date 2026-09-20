# Jev native — intent vs evidence

Named-ask **gate** over `{intent, evidence_digest}`. HedronDB still owns
schema, HQL, tip SHA, and `protect-main`. Jev classifies whether to apply;
it does not write `desired_states`, `events`, or nodes.

Transport is the Facet TypeSafe / System One recipe (suite SoT). HedronDB
does not link a TypeSafe SDK and does not store `$TYPESAFE_API_KEY` in
fixtures, vault notes, or the SQLite file.

**Named asks only.** No weekday Duha / Asr / Maghrib Jev clocks.

## CLI

```bash
hedron jev-intent --intent 'replicas=2' --evidence-digest '<blake3>'
hedron jev-intent --db ./intent.db --vault prod --name deploy \
  --evidence-digest '<blake3>'
```

`--name` + `--db` loads the named desired-state spec **read-only** (HQL
`RoStore`). That is still a named ask, not a reconcile.

## MCP

`hedron mcp` exposes `jev_intent` with the same arguments and the same JSON
as the CLI. There is no HTTP API (HedronDB is not a server).

## Shadow

Always on for this spike. A Choice / Noul is **not** an authorization to
call `Store::reconcile` or to insert rows.

| Outcome | Meaning |
| --- | --- |
| empty / missing / low confidence | `choice=escalate`. `applied=false`. |
| `apply` with confidence ≥ 0.6 | Recommendation only. `applied` stays false. |
| `wait` / `ignore` / `escalate` | Returned as-is when confidence is high enough. |
| no `facet` on `$PATH` | Gate does not run. `status=unavailable`, `choice=escalate`. No network. |

`applied` is always `false` while shadow is on. Low confidence **never**
auto-applies.

## Transport

1. `facet` on `$PATH` → `facet request run` against the bundled collection
   (`docs/examples/typesafe/opencollection.yml`, selector `items/0/items/0`),
   `--environment typesafe --no-record`. Key from Facet env store.
2. Else unavailable. Offline tests use a fixture JSON body
   (`HEDRON_JEV_TRANSPORT=fixture` + `HEDRON_JEV_FIXTURE`).

`HEDRON_JEV_TRANSPORT=none|facet|fixture` forces a backend. HedronDB never
reads `$TYPESAFE_API_KEY` (Facet hydrates it). Do not curl TypeSafe with a
second key copy.

## Recipe (`{intent, evidence_digest}`)

One System One call per named ask:

| Question | Primitive | Use |
| --- | --- | --- |
| `apply` | Choice `apply` / `wait` / `escalate` / `ignore` | Whether to apply |
| `sufficient` | Noul | Evidence enough to consider apply? |

`state` binds the intent text (clipped) and the evidence digest only. No
secrets, tokens, or raw evidence blobs.

## Out

Jev writing SQLite rows · auto-merge on fuzzy match · resurrecting daily
HedronDB grind clocks · Stanley / Pi · a second TYPESAFE key in fixtures
or vault notes.
