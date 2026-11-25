# Model card files

Curated public facts for model cards (Phase 8, the architecture docs).
They are compiled into the gateway; every `.json` file here must also be
listed in `BUILTIN_FILES` in `src/domain/model_card_files.rs` (a test checks).
Operators add or replace files with `GATEWAY_MODEL_CARDS_DIR`, in the same
layout (the operations guide, *Card files*).

## Rules

- **Every fact has a source.** `source_url` (https), `retrieved_at` (the
  date you read it) and `reported_by`: `provider` when the model's provider
  says it about its own model, `independent` otherwise (an evaluator, or a
  hub computing it from the weights). Leave a fact out rather than guess.
- **Exact matches only.** `matches` lists catalog model identifiers per
  provider family, exactly as deployments name them
  (`openai.gpt-oss-120b-1:0` on `bedrock`). Two cards may not match the same
  model on the same family.
- **As published.** Copy what the source says; do not round, rate or
  summarize beyond what it states.

## Card: `<vendor>/<model>.json`

```json
{
  "schema": "qucelerate.model-card.v1",
  "id": "openai/gpt-oss-120b",
  "name": "gpt-oss-120b",
  "vendor": "OpenAI",
  "matches": { "bedrock": ["openai.gpt-oss-120b-1:0"] },
  "overview": {
    "license": { "value": "apache-2.0", "source_url": "https://huggingface.co/openai/gpt-oss-120b",
                 "retrieved_at": "2026-11-14", "reported_by": "provider" }
  },
  "evaluations": [],
  "safety": {}
}
```

`id` is the path without `.json`: lowercase letters, digits, `-`, `.`, `_`.
The vendor `org` is reserved for organizations' own master records
(`org/<slug>`), so no card file may use it. A card's id is also the master
record its deployments link to (Phase 13, D-033; the operations guide,
*Master model records and route proposals*).

- `modality` (optional): `chat` (the default) or `embeddings`. A deployment
  links only to a master of its own modality, so set it on embedding
  models' cards.

- `overview` keys: `release_date`, `version`, `family`, `modalities_in`,
  `modalities_out`, `context_window_tokens`, `max_output_tokens`,
  `knowledge_cutoff`, `license`, `open_weight`, `parameters`, `retirement`
  (the provider's published commitment, as worded: "Not sooner than
  September 22, 2027"; Bedrock and Gemini list no retirement date for an
  account, so this is where the card gets one).
- `safety` keys: `model_card_url`, `safety_evaluations_url`,
  `acceptable_use_url`, `intended_uses`, `prohibited_uses`,
  `known_limitations`. A fact may carry a `title` for its link.
- `evaluations`: `{name, score, setting?, evaluator, source_url,
  retrieved_at, reported_by}`, with the score as published.

## Data handling: `data-handling/<provider family>[-<service>].json`

```json
{
  "schema": "qucelerate.data-handling.v1",
  "provider_family": "bedrock",
  "name": "Amazon Bedrock",
  "service": "optional: the family's service these terms cover",
  "applies_to": "optional: which of the family's models the terms cover",
  "facts": { "training_use": { "value": "...", "source_url": "https://...",
                               "retrieved_at": "2026-11-14", "reported_by": "provider" } }
}
```

`facts` keys: `training_use`, `shared_with_model_provider`,
`provider_access`, `retention`, `abuse_monitoring`, `zero_data_retention`,
`residency`, `encryption`, `terms_url`.

A family whose terms differ by service has one file per service, with
`service` set (lowercase letters, digits and `-`), and may keep one file
without it as the default:

- `gemini`: `gemini-api` (the Gemini API, with a key) and `agent-platform`
  (Gemini Enterprise Agent Platform, with a Google Cloud identity). The
  gateway takes the service from the adapter's credential: a Google service
  account is Agent Platform, a key the Gemini API; the process's own adapter
  follows its configuration. When it cannot tell, the card shows both.
- `azure-ai-foundry`: the service is the deployment's model format from
  Azure's listing, lowercased (`anthropic`: Claude is a Marketplace offer
  with Anthropic's own terms); other formats use the default file (models
  sold directly by Azure).

## Refreshing

Re-read each source, update the value and `retrieved_at` together, and
review the change like code.

CI (`model-card-dates` in `.github/workflows/rust-ci.yml`, also run weekly)
fails when a fact's `retrieved_at` is more than 180 days old or in the
future: `python3 -I scripts/check-model-card-dates.py` lists them. 180 days
is about two provider release cycles; providers revise limits, retirement
dates and terms more often than that, so a fact that old is likely out of
date. `--max-age-days N` or `MODEL_CARD_MAX_AGE_DAYS` changes it for a local
run. Only re-date a fact whose source still says it.

## Licensing of quoted text

The gateway's code and this directory's file format are MIT-licensed, but
the facts here are copied, as published, from the sources each one names in
`source_url`. Text quoted or closely paraphrased from those sources (for
example the data-handling files' `value` and `applies_to` fields, and the
cards' `safety` entries) belongs to its publishers (the model providers,
cloud vendors and evaluators named) and is not covered by the repository's
MIT license. It is included for reference, with its source; follow the
source's own terms when reusing it.
