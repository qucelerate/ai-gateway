//! What each command asks of the gateway, as data: a method, a path, a
//! query, a body, and how to find the `If-Match` value a write needs. Running
//! it is `client.rs`'s job, so this mapping is tested without a server.

use serde_json::{Value, json};

use super::args::Args;

/// `{org}` in a path is replaced with the organization being worked on.
pub const ORG: &str = "/api/admin/v1/organizations/{org}";

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Auth {
    /// `GATEWAY_AGENT_TOKEN`: the admin API.
    Agent,
    /// `GATEWAY_API_KEY`: inference.
    ApiKey,
}

/// Where a write's `If-Match` comes from.
#[derive(Clone, Debug, PartialEq)]
pub enum Precondition {
    None,
    /// Given on the command line.
    Value(String),
    /// The resource's own `ETag` (or `version`) from a GET of this path;
    /// `*` when it does not exist.
    Fetch(String),
    /// As `Fetch`, but none at all when the path cannot be read (a
    /// write-only route answers GET with 405).
    IfReadable(String),
    /// The `version` of the item of a list whose fields match; `*` when no
    /// item does.
    Find {
        list: String,
        matches: Vec<(&'static str, String)>,
    },
    /// A top-level version field of a list's first page.
    ListField {
        list: String,
        field: &'static str,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct Call {
    pub auth: Auth,
    pub method: &'static str,
    pub path: String,
    pub query: Vec<(String, String)>,
    pub body: Option<Value>,
    pub precondition: Precondition,
    /// Follow `next_page_token` and print every item.
    pub all_pages: bool,
    pub idempotency_key: Option<String>,
    /// `--org`; otherwise the environment or the token's only organization.
    pub organization: Option<String>,
    /// The response is a file: returned as the text it came as, never
    /// parsed (a one-line JSONL file is valid JSON too).
    pub raw: bool,
}

impl Call {
    fn new(method: &'static str, path: impl Into<String>) -> Self {
        Self {
            auth: Auth::Agent,
            method,
            path: path.into(),
            query: Vec::new(),
            body: None,
            precondition: Precondition::None,
            all_pages: false,
            idempotency_key: None,
            organization: None,
            raw: false,
        }
    }

    fn get(path: impl Into<String>) -> Self {
        Self::new("GET", path)
    }

    fn body(mut self, body: Value) -> Self {
        self.body = Some(body);
        self
    }

    fn precondition(mut self, precondition: Precondition) -> Self {
        self.precondition = precondition;
        self
    }

    pub fn is_write(&self) -> bool {
        self.method != "GET"
    }
}

pub enum Plan {
    Help,
    Call(Call),
    /// `agreements accept KEY --all`: list the account's agreements, show
    /// what would be accepted (each model's offer and rates), ask unless
    /// `yes`, then accept exactly those named offers. The server has no
    /// "all".
    AcceptAllAgreements {
        list: Box<Call>,
        /// The accept call, whose body [`agreement_choice`] fills in.
        accept: Box<Call>,
        verify: bool,
        yes: bool,
    },
}

/// What `agreements accept --all` would accept, from the account's
/// agreements (`GET .../agreements/{key}`).
#[derive(Debug, Default, PartialEq)]
pub struct AgreementChoice {
    /// `{model, offer_id}` for each model needing an agreement whose one
    /// offer could be read, and that has none (or only a failed one).
    pub pairs: Vec<Value>,
    /// One line per chosen model: its offer and rates.
    pub lines: Vec<String>,
    /// Models left out, with why.
    pub skipped: Vec<String>,
}

pub fn agreement_choice(view: &Value) -> AgreementChoice {
    let mut choice = AgreementChoice::default();
    for model in view["models"].as_array().into_iter().flatten() {
        let id = model["model"].as_str().unwrap_or_default();
        if model["agreement_required"] != json!(true) {
            continue;
        }
        let status = model["agreement"]["status"].as_str();
        if status.is_some_and(|status| status != "failed") {
            continue;
        }
        let offers = model["offers"].as_array().cloned().unwrap_or_default();
        if !model["offers_error"].is_null() {
            choice.skipped.push(format!(
                "{id}: its offers could not be read ({})",
                model["offers_error"].as_str().unwrap_or("error")
            ));
            continue;
        }
        let [offer] = offers.as_slice() else {
            choice.skipped.push(if offers.is_empty() {
                format!("{id}: no offer listed")
            } else {
                format!(
                    "{id}: {} offers; name one with --models {id}=OFFER",
                    offers.len()
                )
            });
            continue;
        };
        let offer_id = offer["offer_id"].as_str().unwrap_or_default();
        choice
            .pairs
            .push(json!({ "model": id, "offer_id": offer_id }));
        choice
            .lines
            .push(format!("{id} {offer_id}: {}", rate_summary(offer)));
    }
    choice
}

/// An offer's rates in one line: the first three, how many more, and the
/// card's total when it was cut to the account's region.
fn rate_summary(offer: &Value) -> String {
    let rates = offer["rate_card"].as_array().cloned().unwrap_or_default();
    if rates.is_empty() {
        return "no rates listed".into();
    }
    let shown: Vec<String> = rates
        .iter()
        .take(3)
        .map(|rate| {
            let unit = rate["unit"]
                .as_str()
                .map(|unit| format!(" per {unit}"))
                .unwrap_or_default();
            format!(
                "{} {}{unit}",
                rate["dimension"].as_str().unwrap_or("?"),
                rate["price"].as_str().unwrap_or("?")
            )
        })
        .collect();
    let mut line = shown.join(", ");
    if rates.len() > shown.len() {
        line.push_str(&format!(", and {} more", rates.len() - shown.len()));
    }
    match offer["rate_card_region"].as_str() {
        Some(region) => line.push_str(&format!(
            " ({region}'s rates; {} on the whole card)",
            offer["rate_card_total"]
        )),
        None => line.push_str(&format!(" ({} on the card)", offer["rate_card_total"])),
    }
    line
}

pub const USAGE: &str = "\
gwctl: drive a QuCelerate gateway from scripts and agents. Prints JSON.

Environment:
  GATEWAY_URL            the gateway origin, such as https://gateway.example.com
  GATEWAY_AGENT_TOKEN    an agent token (gwa_...) for every admin command
  GATEWAY_ORGANIZATION   organization id; default: the only one the token sees
  GATEWAY_API_KEY        an API key (gw_...) for `chat` and `embed`

Common options: --org ID, --compact (one-line JSON), --all (every page of a list),
--idempotency-key KEY and --if-match VALUE (to retry a write safely, reuse both,
as printed when a write's outcome is unknown).

Commands:
  me | orgs
  api METHOD PATH [--data JSON|@FILE|-] [--if-match ETAG]
      PATH is absolute (/api/...) or relative to the organization (catalog/releases);
      PUT, PATCH and DELETE send PATH's current version (if PATH can be read)
      unless --if-match is given
  agents list | get ID | create --name N --role ROLE [--description D] | delete ID
  tokens create AGENT --name N [--expires-in-days D] [--service-account-id SA] | revoke AGENT TOKEN
  service-accounts list | get ID | create --name N | delete ID
  keys list SA | get SA KEY | create SA --scopes inference,embeddings[,catalog-read] [--expires-at TIME]
       | rotate SA KEY | revoke SA KEY
  end-users settings SA | configure SA --currency C [--requirement optional|required]
       [--default-plan P] | plans SA | plan-set SA PLAN --allowance MINOR
       [--period monthly|weekly|daily|days] [--period-days N] [--requests-per-minute N]
       [--tokens-per-minute N] [--max-concurrent N] | plan-delete SA PLAN
       | list SA | get SA USER | assign SA USER --plan P|default
       (plan-set replaces the whole plan: what it leaves out is monthly, unlimited)
  credits list SA USER | grant SA USER --reference R --amount MINOR [--expires-at TIME]
       | reverse SA USER GRANT --reference R --amount MINOR --reason TEXT
  connections list | trust | instructions aws|azure|google [--account-id ID] [--region R]
       [--project-id P] [--family F] | verify KEY --family F --credential JSON|@FILE
       | set KEY --credential JSON|@FILE | delete KEY
  offers get KEY | enable KEY [--location L] | disable KEY
  agreements list KEY | accept KEY --models MODEL=OFFER[,MODEL=OFFER] | --all [--yes] [--verify]
       (Bedrock models' AWS Marketplace agreements: accepting creates subscriptions billed
       per use; --all lists first, shows each model's offer and rates and asks unless --yes,
       then accepts exactly those offers; --verify also calls each model once, a 1-token
       request billed by AWS)
  discovery sync KEY | jobs | job ID | models | deployable | drift
  catalog releases | deployments | routes | manifest RELEASE | create --file MANIFEST
       | activate RELEASE
  prices sources | drift | observations | mappings | overrides | fetch SOURCE
       | map DEPLOYMENT SOURCE --item ITEM [--region R] | unmap DEPLOYMENT SOURCE
  usage --from T --to T [--group-by team,model] … | traces [--model A,B] [--status S] [--q TEXT] … | traces summary --group-by DIM[,DIM]
       | usage series --from T --to T [--granularity day] [--split-by api_key] … | usage events --from T --to T …
       | trace ID | trace ID content | reconciliation
  batches [list] [--status S] [--service-account-id SA] | batches get ID | cancel ID
       | output ID | errors ID (JSONL) | settings | configure --emulated on|off [--concurrency N]
       | staging | stage KEY --bucket B [--prefix P/] [--region R --role-arn ARN] | unstage KEY
  audit [--action A,B] [--outcome O] [--q TEXT] … | audit summary --group-by DIM[,DIM]
  model cards | card MODEL | note MODEL | note-set MODEL --note TEXT
       (MODEL as deployments name it; an empty --note clears it)
  model import MODEL --repository OWNER/NAME --reported-by provider|independent
       | import-show MODEL | import-remove MODEL
       (a Hugging Face model card, shown when no card file matches)
  model masters | master MASTER | route-proposal MASTER [--operations chat,embeddings]
       [--tools true] [--alias A]
       (MASTER is a card's id, anthropic/claude-sonnet-4-6, or org/SLUG; a proposal
       changes nothing: publish it with catalog create and activate)
  model master-set org/SLUG --name N [--modality chat|embeddings] | master-delete org/SLUG
  model link FAMILY MODEL --master MASTER|none | link-show FAMILY MODEL | unlink FAMILY MODEL
       (none detaches a deployment a card would match; unlink lets the card decide)
  evaluations sets | set ID | runs SET
  evaluations create --name N --items JSON [--description D]
       (items: JSON list of {id, expected, match: exact|contains})
  evaluations run SET --batch BATCH_ID
       (scores a completed chat batch whose custom_ids are ITEM[:anything])
  evaluations delete SET
       (hidden from lists and cards; its items, runs and scores are kept)
  notifications [--kind K] [--severity info|warning|action] [--read unread|read]
       | notifications count | notifications read --through ID | --ids ID[,ID]
  chat --model M MESSAGE... | embed --model M TEXT...
  mcp      serve these operations as MCP tools over stdio, for MCP clients

List and report commands pass any other --name value on as a query parameter
(--group-by model becomes group_by=model).
";

/// Reads a JSON argument: inline, `@file`, or `-` for standard input.
pub type JsonSource<'a> = &'a dyn Fn(&str) -> Result<Value, String>;

pub fn plan(mut args: Args, read_json: JsonSource) -> Result<Plan, String> {
    if args.flag("help") || args.positional.is_empty() {
        return Ok(Plan::Help);
    }
    let all_pages = args.flag("all");
    let idempotency_key = args.take("idempotency-key");
    let organization = args.take("org");
    // Overrides the version a typed write would look up, so a retry with the
    // same --idempotency-key sends the same request.
    let if_match = args.take("if-match");
    let words: Vec<String> = std::mem::take(&mut args.positional);
    let words: Vec<&str> = words.iter().map(String::as_str).collect();
    if let ["agreements", "accept", key] = words.as_slice()
        && all_pages
    {
        if args.take("models").is_some() {
            return Err("give --models or --all, not both".into());
        }
        let verify = args.flag("verify");
        let yes = args.flag("yes");
        args.finish()?;
        let mut list = Call::get(agreements(key));
        list.organization = organization.clone();
        let mut accept = Call::new("POST", format!("{}/accept", agreements(key)));
        accept.idempotency_key = idempotency_key;
        accept.organization = organization;
        return Ok(Plan::AcceptAllAgreements {
            list: Box::new(list),
            accept: Box::new(accept),
            verify,
            yes,
        });
    }
    let mut call = command(&words, &mut args, read_json)?;
    args.finish()?;
    call.all_pages = all_pages;
    call.idempotency_key = idempotency_key;
    call.organization = organization;
    if let Some(value) = if_match {
        call.precondition = Precondition::Value(value);
    }
    Ok(Plan::Call(call))
}

fn command(words: &[&str], args: &mut Args, read_json: JsonSource) -> Result<Call, String> {
    let call = match words {
        ["me"] => Call::get("/api/admin/v1/me"),
        ["orgs"] => Call::get("/api/admin/v1/organizations"),
        ["api", method, path] => {
            let method = match method.to_ascii_uppercase().as_str() {
                "GET" => "GET",
                "POST" => "POST",
                "PUT" => "PUT",
                "PATCH" => "PATCH",
                "DELETE" => "DELETE",
                other => return Err(format!("unsupported method {other}")),
            };
            let mut call = Call::new(method, absolute(path));
            // Like the typed writes, a replace or delete sends the resource's
            // current version unless --if-match says otherwise; a path that
            // cannot be read (write-only) gets none.
            if matches!(method, "PUT" | "PATCH" | "DELETE") {
                let path = call.path.clone();
                call = call.precondition(Precondition::IfReadable(path));
            }
            if let Some(data) = args.take("data") {
                call.body = Some(read_json(&data)?);
            }
            if !call.is_write() {
                call.query = query(args);
            }
            call
        }

        ["agents", "list"] => listed(format!("{ORG}/agents"), args),
        ["agents", "get", id] => Call::get(format!("{ORG}/agents/{}", segment(id))),
        ["agents", "create"] => {
            let mut body = json!({ "name": args.require("name")?, "role": args.require("role")? });
            if let Some(description) = args.take("description") {
                body["description"] = json!(description);
            }
            Call::new("POST", format!("{ORG}/agents")).body(body)
        }
        ["agents", "delete", id] => Call::new("DELETE", format!("{ORG}/agents/{}", segment(id))),
        ["tokens", "create", agent] => {
            let mut body = json!({ "name": args.require("name")? });
            if let Some(days) = args.take("expires-in-days") {
                body["expires_in_days"] = json!(number(&days, "--expires-in-days")?);
            }
            // Limits the token to that service account's end users.
            if let Some(account) = args.take("service-account-id") {
                body["service_account_id"] = json!(account);
            }
            Call::new("POST", format!("{ORG}/agents/{}/tokens", segment(agent))).body(body)
        }
        ["tokens", "revoke", agent, token] => Call::new(
            "DELETE",
            format!("{ORG}/agents/{}/tokens/{}", segment(agent), segment(token)),
        ),

        ["service-accounts", "list"] => listed(format!("{ORG}/service-accounts"), args),
        ["service-accounts", "get", id] => Call::get(service_account(id)),
        ["service-accounts", "create"] => Call::new("POST", format!("{ORG}/service-accounts"))
            .body(json!({ "name": args.require("name")? })),
        ["service-accounts", "delete", id] => Call::new("DELETE", service_account(id))
            .precondition(Precondition::Fetch(service_account(id))),

        ["keys", "list", account] => listed(format!("{}/keys", service_account(account)), args),
        ["keys", "get", account, key] => Call::get(api_key(account, key)),
        ["keys", "create", account] => {
            let scopes = args.require("scopes")?;
            let mut body = json!({ "scopes": scopes.split(',').map(wire_scope).collect::<Result<Vec<_>, _>>()? });
            body["expires_at"] = args.take("expires-at").map_or(Value::Null, Value::String);
            Call::new("POST", format!("{}/keys", service_account(account))).body(body)
        }
        ["keys", "rotate", account, key] => {
            Call::new("POST", format!("{}/rotate", api_key(account, key)))
        }
        ["keys", "revoke", account, key] => {
            Call::new("POST", format!("{}/revoke", api_key(account, key)))
                .precondition(Precondition::Fetch(api_key(account, key)))
        }

        ["end-users", "settings", account] => Call::get(end_user_settings(account)),
        ["end-users", "configure", account] => {
            let body = json!({
                "currency": args.require("currency")?,
                "requirement": args.take("requirement").unwrap_or_else(|| "optional".into()),
                "default_plan": args.take("default-plan"),
            });
            Call::new("PUT", end_user_settings(account))
                .body(body)
                .precondition(Precondition::Fetch(end_user_settings(account)))
        }
        ["end-users", "plans", account] => {
            Call::get(format!("{}/end-user-plans", service_account(account)))
        }
        ["end-users", "plan-set", account, name] => {
            let allowance = args.require("allowance")?;
            let mut body = json!({ "allowance_minor": number(&allowance, "--allowance")? });
            if let Some(period) = args.take("period") {
                body["period"] = json!(period);
            }
            for (flag, field) in [
                ("period-days", "period_days"),
                ("requests-per-minute", "requests_per_minute"),
                ("tokens-per-minute", "tokens_per_minute"),
                ("max-concurrent", "max_concurrent"),
            ] {
                if let Some(value) = args.take(flag) {
                    body[field] = json!(number(&value, &format!("--{flag}"))?);
                }
            }
            Call::new("PUT", end_user_plan(account, name))
                .body(body)
                .precondition(Precondition::Find {
                    list: format!("{}/end-user-plans", service_account(account)),
                    matches: vec![("name", (*name).to_owned())],
                })
        }
        ["end-users", "plan-delete", account, name] => {
            Call::new("DELETE", end_user_plan(account, name)).precondition(Precondition::Find {
                list: format!("{}/end-user-plans", service_account(account)),
                matches: vec![("name", (*name).to_owned())],
            })
        }
        ["end-users", "list", account] => {
            listed(format!("{}/end-users", service_account(account)), args)
        }
        ["end-users", "get", account, user] => Call::get(end_user(account, user)),
        ["end-users", "assign", account, user] => {
            let plan = args.require("plan")?;
            let plan = if plan == "default" {
                Value::Null
            } else {
                json!(plan)
            };
            Call::new("PUT", end_user(account, user))
                .body(json!({ "plan": plan }))
                .precondition(Precondition::Fetch(end_user(account, user)))
        }
        ["credits", "list", account, user] => {
            Call::get(format!("{}/credit-grants", end_user(account, user)))
        }
        ["credits", "grant", account, user] => {
            let amount = args.require("amount")?;
            Call::new("POST", format!("{}/credit-grants", end_user(account, user))).body(json!({
                "reference": args.require("reference")?,
                "amount_minor": number(&amount, "--amount")?,
                "expires_at": args.take("expires-at"),
            }))
        }
        ["credits", "reverse", account, user, grant] => {
            let amount = args.require("amount")?;
            Call::new(
                "POST",
                format!(
                    "{}/credit-grants/{}/reversals",
                    end_user(account, user),
                    segment(grant)
                ),
            )
            .body(json!({
                "reference": args.require("reference")?,
                "amount_minor": number(&amount, "--amount")?,
                "reason": args.require("reason")?,
            }))
        }

        ["connections", "list"] => listed(format!("{ORG}/catalog/credentials"), args),
        ["connections", "trust"] => Call::get(format!("{ORG}/catalog/cloud-trust")),
        [
            "connections",
            "instructions",
            cloud @ ("aws" | "azure" | "google"),
        ] => {
            let mut call = Call::get(format!("{ORG}/onboarding/{cloud}"));
            call.query = query(args);
            call
        }
        ["connections", "instructions"] => Call::get(format!("{ORG}/onboarding")),
        ["connections", "verify", key] => Call::new("POST", format!("{ORG}/onboarding/verify"))
            .body(json!({
                "adapter_instance_key": key,
                "family": args.require("family")?,
                "credential": read_json(&args.require("credential")?)?,
            })),
        ["connections", "set", key] => Call::new("PUT", credential(key))
            .body(read_json(&args.require("credential")?)?)
            .precondition(find_credential(key)),
        ["connections", "delete", key] => {
            Call::new("DELETE", credential(key)).precondition(find_credential(key))
        }

        ["offers", "get", key] => Call::get(auto_offer(key)),
        ["offers", "enable", key] => Call::new("PUT", auto_offer(key))
            .body(json!({ "location": args.take("location") }))
            .precondition(Precondition::Fetch(auto_offer(key))),
        ["offers", "disable", key] => {
            Call::new("DELETE", auto_offer(key)).precondition(Precondition::Fetch(auto_offer(key)))
        }

        ["agreements", "list", key] => Call::get(agreements(key)),
        ["agreements", "accept", key] => {
            let models = match args.take("models") {
                None => return Err("--models or --all is required".into()),
                Some(models) => models
                    .split(',')
                    .filter(|pair| !pair.trim().is_empty())
                    .map(|pair| {
                        let (model, offer) = pair.trim().split_once('=').ok_or_else(|| {
                            format!("--models takes MODEL=OFFER pairs, not {pair}")
                        })?;
                        Ok(json!({ "model": model, "offer_id": offer }))
                    })
                    .collect::<Result<Vec<Value>, String>>()?,
            };
            if models.is_empty() {
                return Err("--models takes MODEL=OFFER pairs".into());
            }
            Call::new("POST", format!("{}/accept", agreements(key)))
                .body(json!({ "models": models, "verify": args.flag("verify") }))
        }

        ["discovery", "sync", key] => Call::new("POST", format!("{ORG}/catalog/sync-jobs"))
            .body(json!({ "adapter_instance_key": key })),
        ["discovery", "jobs"] => listed(format!("{ORG}/catalog/sync-jobs"), args),
        ["discovery", "job", id] => Call::get(format!("{ORG}/catalog/sync-jobs/{}", segment(id))),
        ["discovery", "models"] => listed(format!("{ORG}/catalog/discovered-models"), args),
        ["discovery", "deployable"] => listed(format!("{ORG}/catalog/deployable-models"), args),
        ["discovery", "drift"] => listed(format!("{ORG}/catalog/discovery-drift"), args),

        [
            "catalog",
            collection @ ("releases" | "deployments" | "routes"),
        ] => listed(format!("{ORG}/catalog/{collection}"), args),
        ["catalog", "provider-accounts"] => {
            listed(format!("{ORG}/catalog/provider-accounts"), args)
        }
        ["catalog", "manifest", release] => Call::get(format!(
            "{ORG}/catalog/releases/{}/manifest",
            segment(release)
        )),
        ["catalog", "create"] => Call::new("POST", format!("{ORG}/catalog/releases"))
            .body(read_json(&format!("@{}", args.require("file")?))?),
        ["catalog", "activate", release] => Call::new(
            "POST",
            format!("{ORG}/catalog/releases/{}/activate", segment(release)),
        )
        .precondition(Precondition::ListField {
            list: format!("{ORG}/catalog/releases"),
            field: "activation_version",
        }),

        [
            "prices",
            collection @ ("sources" | "drift" | "observations" | "mappings" | "overrides"),
        ] => {
            let path = match *collection {
                "sources" => "price-sources",
                "drift" => "price-drift",
                "observations" => "price-observations",
                "mappings" => "price-mappings",
                _ => "price-overrides",
            };
            listed(format!("{ORG}/catalog/{path}"), args)
        }
        ["prices", "fetch", source] => Call::new(
            "POST",
            format!("{ORG}/catalog/price-sources/{}/fetch", segment(source)),
        ),
        ["prices", "map", deployment, source] => {
            let body = json!({ "item": args.require("item")?, "region": args.take("region") });
            Call::new("PUT", price_mapping(deployment, source))
                .body(body)
                .precondition(find_mapping(deployment, source))
        }
        ["prices", "unmap", deployment, source] => {
            Call::new("DELETE", price_mapping(deployment, source))
                .precondition(find_mapping(deployment, source))
        }

        ["usage"] => listed(format!("{ORG}/usage"), args),
        ["usage", "series"] => listed(format!("{ORG}/usage/series"), args),
        ["usage", "events"] => listed(format!("{ORG}/usage/events"), args),
        ["traces"] => listed(format!("{ORG}/traces"), args),
        ["traces", "summary"] => listed(format!("{ORG}/traces/summary"), args),
        ["trace", id] => Call::get(format!("{ORG}/traces/{}", segment(id))),
        ["trace", id, "content"] => Call::get(format!("{ORG}/traces/{}/content", segment(id))),
        ["reconciliation"] => listed(format!("{ORG}/reconciliation-items"), args),
        ["model", "cards"] => Call::get(format!("{ORG}/catalog/model-cards")),
        ["model", "card", model] => Call::get(model_card(model)),
        ["model", "note", model] => Call::get(format!("{}/note", model_card(model))),
        ["model", "note-set", model] => {
            let path = format!("{}/note", model_card(model));
            Call::new("PUT", path.clone())
                .body(json!({ "note": args.require("note")? }))
                .precondition(Precondition::Fetch(path))
        }
        ["model", "import", model] => {
            let path = format!("{}/import", model_card(model));
            let provider_repository = match args.require("reported-by")?.as_str() {
                "provider" => true,
                "independent" => false,
                _ => return Err("--reported-by must be provider or independent".into()),
            };
            Call::new("PUT", path.clone())
                .body(json!({
                    "repository": args.require("repository")?,
                    "provider_repository": provider_repository,
                }))
                .precondition(Precondition::Fetch(path))
        }
        ["model", "import-show", model] => Call::get(format!("{}/import", model_card(model))),
        ["model", "import-remove", model] => {
            let path = format!("{}/import", model_card(model));
            Call::new("DELETE", path.clone()).precondition(Precondition::Fetch(path))
        }
        ["model", "masters"] => Call::get(format!("{ORG}/catalog/model-masters")),
        ["model", "master", master] => Call::get(model_master(master)),
        ["model", "route-proposal", master] => {
            let mut call = Call::get(format!("{}/route-proposal", model_master(master)));
            call.query = query(args);
            call
        }
        ["model", "master-set", master] => {
            let mut body = json!({ "name": args.require("name")? });
            if let Some(modality) = args.take("modality") {
                body["modality"] = json!(modality);
            }
            Call::new("PUT", model_master(master))
                .body(body)
                .precondition(Precondition::Fetch(model_master(master)))
        }
        ["model", "master-delete", master] => Call::new("DELETE", model_master(master))
            .precondition(Precondition::Fetch(model_master(master))),
        ["model", "link", family, model] => {
            let master = match args.require("master")?.as_str() {
                "none" => Value::Null,
                master => json!(master),
            };
            Call::new("PUT", master_link(family, model))
                .body(json!({ "master": master }))
                .precondition(Precondition::Fetch(master_link(family, model)))
        }
        ["model", "link-show", family, model] => Call::get(master_link(family, model)),
        ["model", "unlink", family, model] => Call::new("DELETE", master_link(family, model))
            .precondition(Precondition::Fetch(master_link(family, model))),
        ["evaluations", "sets"] => Call::get(format!("{ORG}/catalog/evaluation-sets")),
        ["evaluations", "set", id] => {
            Call::get(format!("{ORG}/catalog/evaluation-sets/{}", segment(id)))
        }
        ["evaluations", "delete", id] => {
            let path = format!("{ORG}/catalog/evaluation-sets/{}", segment(id));
            Call::new("DELETE", path.clone()).precondition(Precondition::Fetch(path))
        }
        ["evaluations", "runs", set] => Call::get(format!(
            "{ORG}/catalog/evaluation-sets/{}/runs",
            segment(set)
        )),
        ["evaluations", "create"] => {
            let mut body = json!({
                "name": args.require("name")?,
                "items": read_json(&args.require("items")?)?,
            });
            if let Some(description) = args.take("description") {
                body["description"] = json!(description);
            }
            Call::new("POST", format!("{ORG}/catalog/evaluation-sets")).body(body)
        }
        ["evaluations", "run", set] => Call::new(
            "POST",
            format!("{ORG}/catalog/evaluation-sets/{}/runs", segment(set)),
        )
        .body(json!({ "batch_id": args.require("batch")? })),
        ["batches"] | ["batches", "list"] => listed(format!("{ORG}/batches"), args),
        ["batches", "get", id] => Call::get(batch(id)),
        ["batches", "cancel", id] => Call::new("POST", format!("{}/cancel", batch(id))),
        ["batches", which @ ("output" | "errors"), id] => {
            let mut call = Call::get(format!("{}/{which}", batch(id)));
            call.raw = true;
            call
        }
        ["batches", "settings"] => Call::get(format!("{ORG}/batch-settings")),
        ["batches", "configure"] => {
            let allowed = match args.require("emulated")?.as_str() {
                "on" | "true" => true,
                "off" | "false" => false,
                other => return Err(format!("--emulated takes on or off, not {other}")),
            };
            let concurrency = args
                .take("concurrency")
                .map(|value| number(&value, "--concurrency"))
                .transpose()?;
            Call::new("PUT", format!("{ORG}/batch-settings"))
                .body(json!({ "emulated_allowed": allowed, "emulated_concurrency": concurrency }))
                .precondition(Precondition::Fetch(format!("{ORG}/batch-settings")))
        }
        ["batches", "staging"] => Call::get(format!("{ORG}/batch-staging")),
        ["batches", "stage", connection] => Call::new("PUT", batch_staging(connection))
            .body(json!({
                "bucket": args.require("bucket")?,
                "prefix": args.take("prefix"),
                "region": args.take("region"),
                "role_arn": args.take("role-arn"),
            }))
            .precondition(find_staging(connection)),
        ["batches", "unstage", connection] => {
            Call::new("DELETE", batch_staging(connection)).precondition(find_staging(connection))
        }
        ["notifications"] => listed(format!("{ORG}/notifications"), args),
        ["notifications", "count"] => Call::get(format!("{ORG}/notifications/unread-count")),
        ["notifications", "read"] => {
            let body = match (args.take("through"), args.take("ids")) {
                (Some(through), None) => json!({ "through": through }),
                (None, Some(ids)) => json!({
                    "ids": ids
                        .split(',')
                        .map(str::trim)
                        .filter(|id| !id.is_empty())
                        .collect::<Vec<_>>()
                }),
                _ => return Err("give one of --through ID or --ids ID[,ID]".into()),
            };
            Call::new("POST", format!("{ORG}/notifications/read")).body(body)
        }
        ["audit"] => listed(format!("{ORG}/audit-events"), args),
        ["audit", "summary"] => listed(format!("{ORG}/audit-events/summary"), args),

        ["chat", message @ ..] if !message.is_empty() => {
            let mut call = Call::new("POST", "/api/inference/v1/chat/completions").body(json!({
                "model": args.require("model")?,
                "messages": [{ "role": "user", "content": message.join(" ") }],
            }));
            call.auth = Auth::ApiKey;
            call
        }
        ["embed", text @ ..] if !text.is_empty() => {
            let mut call = Call::new("POST", "/api/inference/v1/embeddings")
                .body(json!({ "model": args.require("model")?, "input": text.join(" ") }));
            call.auth = Auth::ApiKey;
            call
        }

        _ => {
            return Err(format!(
                "unknown command `{}`; see gwctl --help",
                words.join(" ")
            ));
        }
    };
    Ok(call)
}

/// A list or report: every remaining `--name value` is a query parameter.
fn listed(path: String, args: &mut Args) -> Call {
    let mut call = Call::get(path);
    call.query = query(args);
    call
}

fn query(args: &mut Args) -> Vec<(String, String)> {
    std::mem::take(&mut args.options)
        .into_iter()
        .map(|(name, value)| (name.replace('-', "_"), value))
        .collect()
}

/// An admin path from what was typed: absolute, or relative to the
/// organization.
fn absolute(path: &str) -> String {
    if path.starts_with('/') {
        path.to_owned()
    } else {
        format!("{ORG}/{}", path.trim_start_matches("./"))
    }
}

/// A path segment, percent-encoded, so an id can't change the path.
fn segment(value: &str) -> String {
    url::form_urlencoded::byte_serialize(value.as_bytes())
        .collect::<String>()
        .replace('+', "%20")
}

fn batch(id: &str) -> String {
    format!("{ORG}/batches/{}", segment(id))
}

fn batch_staging(connection: &str) -> String {
    format!("{ORG}/batch-staging/{}", segment(connection))
}

fn find_staging(connection: &str) -> Precondition {
    Precondition::Find {
        list: format!("{ORG}/batch-staging"),
        matches: vec![("connection", connection.to_owned())],
    }
}

fn service_account(id: &str) -> String {
    format!("{ORG}/service-accounts/{}", segment(id))
}

fn api_key(account: &str, key: &str) -> String {
    format!("{}/keys/{}", service_account(account), segment(key))
}

fn end_user_settings(account: &str) -> String {
    format!("{}/end-user-settings", service_account(account))
}

fn end_user_plan(account: &str, name: &str) -> String {
    format!(
        "{}/end-user-plans/{}",
        service_account(account),
        segment(name)
    )
}

fn end_user(account: &str, user: &str) -> String {
    format!("{}/end-users/{}", service_account(account), segment(user))
}

/// A model's card; the identifier is one path segment, so `:` and `/` in
/// Bedrock and Vertex identifiers are encoded.
fn model_card(model: &str) -> String {
    format!("{ORG}/catalog/models/{}/card", segment(model))
}

fn model_master(master: &str) -> String {
    format!("{ORG}/catalog/model-masters/{}", segment(master))
}

fn master_link(family: &str, model: &str) -> String {
    format!(
        "{ORG}/catalog/model-master-links/{}/{}",
        segment(family),
        segment(model)
    )
}

fn credential(key: &str) -> String {
    format!("{ORG}/catalog/credentials/{}", segment(key))
}

fn find_credential(key: &str) -> Precondition {
    Precondition::Find {
        list: format!("{ORG}/catalog/credentials"),
        matches: vec![("adapter_instance_key", key.to_owned())],
    }
}

fn agreements(key: &str) -> String {
    format!("{ORG}/catalog/agreements/{}", segment(key))
}

fn auto_offer(key: &str) -> String {
    format!("{ORG}/catalog/auto-offers/{}", segment(key))
}

fn price_mapping(deployment: &str, source: &str) -> String {
    format!(
        "{ORG}/catalog/price-mappings/{}/{}",
        segment(deployment),
        segment(source)
    )
}

fn find_mapping(deployment: &str, source: &str) -> Precondition {
    Precondition::Find {
        list: format!("{ORG}/catalog/price-mappings"),
        matches: vec![
            ("deployment", deployment.to_owned()),
            ("source", source.to_owned()),
        ],
    }
}

fn number(value: &str, name: &str) -> Result<u64, String> {
    value
        .parse()
        .map_err(|_| format!("{name} must be a whole number"))
}

/// An API key scope as the API spells it: `inference`, `catalog-read` and
/// `CatalogRead` all mean `CatalogRead`.
fn wire_scope(scope: &str) -> Result<&'static str, String> {
    match scope
        .trim()
        .to_ascii_lowercase()
        .replace(['-', '_'], "")
        .as_str()
    {
        "inference" => Ok("Inference"),
        "embeddings" => Ok("Embeddings"),
        "catalogread" => Ok("CatalogRead"),
        "adminread" => Ok("AdminRead"),
        _ => Err(format!(
            "unknown scope `{scope}`; expected inference, embeddings, catalog-read or admin-read"
        )),
    }
}

/// The `version` of the first item whose fields match, as an `If-Match`.
pub fn version_of(items: &[Value], matches: &[(&'static str, String)]) -> Option<String> {
    items
        .iter()
        .find(|item| {
            matches
                .iter()
                .all(|(field, value)| item.get(*field).and_then(Value::as_str) == Some(value))
        })
        .and_then(|item| item.get("version").and_then(Value::as_u64))
        .map(|version| format!("\"v{version}\""))
}

/// A page's collection: `items` on most endpoints, `data` on the end-user
/// ones. `None` for a response that is not a page.
pub fn page_items(page: &Value) -> Option<(&'static str, &Vec<Value>)> {
    ["items", "data"]
        .into_iter()
        .find_map(|key| match page.get(key) {
            Some(Value::Array(items)) => Some((key, items)),
            _ => None,
        })
}

/// Merges pages into one, under the collection's own key (`items` or
/// `data`), keeping the first page's other fields. A response that is not a
/// page is returned as it is.
pub fn merge_pages(pages: Vec<Value>) -> Value {
    let mut pages = pages.into_iter();
    let (mut first, key) = match pages.next() {
        Some(first) => match page_items(&first).map(|(key, _)| key) {
            Some(key) => match first {
                Value::Object(first) => (first, key),
                other => return other,
            },
            None => return first,
        },
        None => return Value::Null,
    };
    let mut items = match first.remove(key) {
        Some(Value::Array(items)) => items,
        _ => Vec::new(),
    };
    for page in pages {
        if let Some(Value::Array(more)) = page.get(key) {
            items.extend(more.iter().cloned());
        }
    }
    first.insert(key.into(), Value::Array(items));
    first.insert("next_page_token".into(), Value::Null);
    Value::Object(first)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan_of(words: &[&str]) -> Result<Call, String> {
        let args = Args::parse(words.iter().map(|word| word.to_string()))?;
        let read = |text: &str| -> Result<Value, String> {
            match text.strip_prefix('@') {
                Some(file) => Ok(json!({ "file": file })),
                None => serde_json::from_str(text).map_err(|error| error.to_string()),
            }
        };
        match plan(args, &read)? {
            Plan::Call(call) => Ok(call),
            Plan::Help => Err("help".into()),
            Plan::AcceptAllAgreements { .. } => Err("accept all".into()),
        }
    }

    #[test]
    fn creates_an_api_key_with_scopes_as_the_api_spells_them() {
        let call = plan_of(&[
            "keys",
            "create",
            "sa 1",
            "--scopes",
            "inference,catalog-read",
        ])
        .unwrap();
        assert_eq!(call.method, "POST");
        assert_eq!(call.path, format!("{ORG}/service-accounts/sa%201/keys"));
        assert_eq!(
            call.body,
            Some(json!({ "scopes": ["Inference", "CatalogRead"], "expires_at": null }))
        );
        assert!(plan_of(&["keys", "create", "sa", "--scopes", "everything"]).is_err());
    }

    #[test]
    fn manages_an_apps_end_users_plans_and_credits() {
        let call = plan_of(&["end-users", "assign", "sa", "user/7", "--plan", "default"]).unwrap();
        assert_eq!(call.method, "PUT");
        assert_eq!(
            call.path,
            format!("{ORG}/service-accounts/sa/end-users/user%2F7")
        );
        assert_eq!(call.body, Some(json!({ "plan": null })));
        assert_eq!(call.precondition, Precondition::Fetch(call.path.clone()));
        let call = plan_of(&["end-users", "plan-set", "sa", "pro", "--allowance", "5000"]).unwrap();
        assert_eq!(call.body, Some(json!({ "allowance_minor": 5000 })));
        let call = plan_of(&[
            "end-users",
            "plan-set",
            "sa",
            "daily",
            "--allowance",
            "100",
            "--period",
            "days",
            "--period-days",
            "30",
            "--requests-per-minute",
            "60",
            "--tokens-per-minute",
            "100000",
            "--max-concurrent",
            "4",
        ])
        .unwrap();
        assert_eq!(
            call.body,
            Some(json!({
                "allowance_minor": 100,
                "period": "days",
                "period_days": 30,
                "requests_per_minute": 60,
                "tokens_per_minute": 100000,
                "max_concurrent": 4,
            }))
        );
        assert!(
            plan_of(&[
                "end-users",
                "plan-set",
                "sa",
                "pro",
                "--allowance",
                "1",
                "--tokens-per-minute",
                "many",
            ])
            .is_err()
        );
        assert!(matches!(call.precondition, Precondition::Find { .. }));
        let call = plan_of(&[
            "credits",
            "grant",
            "sa",
            "u",
            "--reference",
            "order-1",
            "--amount",
            "10",
        ])
        .unwrap();
        assert_eq!(call.method, "POST");
        assert_eq!(
            call.body,
            Some(json!({ "reference": "order-1", "amount_minor": 10, "expires_at": null }))
        );
        let call = plan_of(&[
            "credits",
            "reverse",
            "sa",
            "u",
            "g-1",
            "--reference",
            "refund-1",
            "--amount",
            "4",
            "--reason",
            "refund",
        ])
        .unwrap();
        assert_eq!(
            call.path,
            format!("{ORG}/service-accounts/sa/end-users/u/credit-grants/g-1/reversals")
        );
        assert!(plan_of(&["credits", "grant", "sa", "u", "--amount", "10"]).is_err());
    }

    #[test]
    fn accepts_model_agreements_by_offer_or_all() {
        let call = plan_of(&["agreements", "list", "bedrock-main"]).unwrap();
        assert_eq!(call.method, "GET");
        assert_eq!(call.path, format!("{ORG}/catalog/agreements/bedrock-main"));
        let call = plan_of(&[
            "agreements",
            "accept",
            "bedrock-main",
            "--models",
            "anthropic.claude-haiku-5-5=offer-1, xai.grok-9=offer-2",
        ])
        .unwrap();
        assert_eq!(call.method, "POST");
        assert_eq!(
            call.path,
            format!("{ORG}/catalog/agreements/bedrock-main/accept")
        );
        assert_eq!(
            call.body,
            Some(json!({
                "verify": false,
                "models": [
                    {"model": "anthropic.claude-haiku-5-5", "offer_id": "offer-1"},
                    {"model": "xai.grok-9", "offer_id": "offer-2"}
                ]
            }))
        );
        for wrong in [
            &["agreements", "accept", "k"][..],
            &["agreements", "accept", "k", "--all", "--models", "a.b=o"],
            &["agreements", "accept", "k", "--models", "a.b"],
        ] {
            assert!(plan_of(wrong).is_err(), "{wrong:?}");
        }
        // --all lists, then sends the pairs it showed: never a server "all".
        let args = Args::parse(
            ["agreements", "accept", "k", "--all", "--verify"]
                .iter()
                .map(|word| word.to_string()),
        )
        .unwrap();
        let Ok(Plan::AcceptAllAgreements {
            list,
            accept,
            verify,
            yes,
        }) = plan(args, &|_| Err("no".into()))
        else {
            panic!("--all plans a listing first");
        };
        assert_eq!(list.method, "GET");
        assert_eq!(list.path, format!("{ORG}/catalog/agreements/k"));
        assert_eq!(accept.path, format!("{ORG}/catalog/agreements/k/accept"));
        assert_eq!(accept.body, None);
        assert!(verify && !yes);
    }

    #[test]
    fn accepting_all_names_only_single_readable_offers_not_yet_accepted() {
        let offer = |id: &str, rates: usize| {
            json!({
                "offer_id": id,
                "rate_card": (0..rates)
                    .map(|n| json!({"dimension": format!("USE2_rate_{n}"), "price": "0.5", "unit": "Units"}))
                    .collect::<Vec<_>>(),
                "rate_card_region": "USE2",
                "rate_card_total": 570
            })
        };
        let view = json!({"models": [
            {"model": "a.one", "agreement_required": true, "offers": [offer("o-1", 5)],
             "offers_error": null, "agreement": null},
            {"model": "a.two", "agreement_required": true, "offers": [offer("o-2", 1), offer("o-3", 1)],
             "offers_error": null, "agreement": null},
            {"model": "a.three", "agreement_required": true, "offers": [],
             "offers_error": "agreement_permission_missing", "agreement": null},
            {"model": "a.four", "agreement_required": true, "offers": [offer("o-4", 1)],
             "offers_error": null, "agreement": {"status": "pending"}},
            {"model": "a.five", "agreement_required": true, "offers": [offer("o-5", 0)],
             "offers_error": null, "agreement": {"status": "failed"}},
            {"model": "a.six", "agreement_required": false, "offers": [],
             "offers_error": null, "agreement": {"status": "verified"}}
        ]});
        let choice = agreement_choice(&view);
        assert_eq!(
            choice.pairs,
            [
                json!({"model": "a.one", "offer_id": "o-1"}),
                json!({"model": "a.five", "offer_id": "o-5"})
            ]
        );
        assert_eq!(
            choice.lines[0],
            "a.one o-1: USE2_rate_0 0.5 per Units, USE2_rate_1 0.5 per Units, USE2_rate_2 0.5 per Units, and 2 more (USE2's rates; 570 on the whole card)"
        );
        assert_eq!(choice.lines[1], "a.five o-5: no rates listed");
        assert_eq!(choice.skipped.len(), 2);
        assert!(choice.skipped[0].contains("a.two: 2 offers"));
        assert!(choice.skipped[1].contains("a.three"));
    }

    #[test]
    fn fetches_a_price_source_now() {
        let call = plan_of(&["prices", "fetch", "aws-price-list:bedrock"]).unwrap();
        assert_eq!(call.method, "POST");
        assert_eq!(
            call.path,
            format!("{ORG}/catalog/price-sources/aws-price-list%3Abedrock/fetch")
        );
        assert_eq!(call.body, None);
        assert_eq!(call.precondition, Precondition::None);
    }

    #[test]
    fn guards_writes_with_the_current_version() {
        let call = plan_of(&["keys", "revoke", "sa", "k"]).unwrap();
        assert_eq!(
            call.precondition,
            Precondition::Fetch(format!("{ORG}/service-accounts/sa/keys/k"))
        );
        let call = plan_of(&[
            "connections",
            "set",
            "bedrock-1",
            "--credential",
            r#"{"role_arn":"arn"}"#,
        ])
        .unwrap();
        assert_eq!(call.body, Some(json!({ "role_arn": "arn" })));
        assert_eq!(
            call.precondition,
            Precondition::Find {
                list: format!("{ORG}/catalog/credentials"),
                matches: vec![("adapter_instance_key", "bedrock-1".into())],
            }
        );
        let call = plan_of(&["catalog", "activate", "r-1"]).unwrap();
        assert!(matches!(
            call.precondition,
            Precondition::ListField {
                field: "activation_version",
                ..
            }
        ));
        // A retry names the version the first attempt sent.
        let call = plan_of(&[
            "keys",
            "revoke",
            "sa",
            "k",
            "--if-match",
            "\"v2\"",
            "--idempotency-key",
            "abc",
        ])
        .unwrap();
        assert_eq!(call.precondition, Precondition::Value("\"v2\"".into()));
        assert_eq!(call.idempotency_key.as_deref(), Some("abc"));
    }

    #[test]
    fn master_records_links_and_route_proposals() {
        let masters = format!("{ORG}/catalog/model-masters");
        assert_eq!(plan_of(&["model", "masters"]).unwrap().path, masters);
        let call = plan_of(&["model", "master", "anthropic/claude-sonnet-4-6"]).unwrap();
        assert_eq!(
            call.path,
            format!("{masters}/anthropic%2Fclaude-sonnet-4-6")
        );
        let call = plan_of(&[
            "model",
            "route-proposal",
            "anthropic/claude-sonnet-4-6",
            "--operations",
            "chat,embeddings",
            "--tools",
            "true",
        ])
        .unwrap();
        assert_eq!(call.method, "GET");
        assert_eq!(
            call.path,
            format!("{masters}/anthropic%2Fclaude-sonnet-4-6/route-proposal")
        );
        assert_eq!(
            call.query,
            [
                ("operations".to_owned(), "chat,embeddings".to_owned()),
                ("tools".to_owned(), "true".to_owned())
            ]
        );
        let call = plan_of(&["model", "master-set", "org/qwen", "--name", "Qwen"]).unwrap();
        assert_eq!(call.method, "PUT");
        assert_eq!(call.path, format!("{masters}/org%2Fqwen"));
        assert_eq!(call.body, Some(json!({"name": "Qwen"})));
        assert_eq!(
            call.precondition,
            Precondition::Fetch(format!("{masters}/org%2Fqwen"))
        );
        let call = plan_of(&[
            "model",
            "master-set",
            "org/e",
            "--name",
            "E",
            "--modality",
            "embeddings",
        ])
        .unwrap();
        assert_eq!(
            call.body,
            Some(json!({"name": "E", "modality": "embeddings"}))
        );
        assert_eq!(
            plan_of(&["model", "master-delete", "org/qwen"])
                .unwrap()
                .method,
            "DELETE"
        );
        let link = format!("{ORG}/catalog/model-master-links/bedrock/us.amazon.nova-2-lite-v1%3A0");
        let call = plan_of(&[
            "model",
            "link",
            "bedrock",
            "us.amazon.nova-2-lite-v1:0",
            "--master",
            "amazon/nova-2-lite",
        ])
        .unwrap();
        assert_eq!((call.method, call.path.as_str()), ("PUT", link.as_str()));
        assert_eq!(call.body, Some(json!({"master": "amazon/nova-2-lite"})));
        assert_eq!(call.precondition, Precondition::Fetch(link.clone()));
        let call = plan_of(&[
            "model",
            "link",
            "bedrock",
            "us.amazon.nova-2-lite-v1:0",
            "--master",
            "none",
        ])
        .unwrap();
        assert_eq!(call.body, Some(json!({"master": null})));
        assert!(plan_of(&["model", "link", "bedrock", "m"]).is_err());
        let call = plan_of(&["model", "unlink", "bedrock", "us.amazon.nova-2-lite-v1:0"]).unwrap();
        assert_eq!((call.method, call.path.as_str()), ("DELETE", link.as_str()));
        assert_eq!(
            plan_of(&[
                "model",
                "link-show",
                "bedrock",
                "us.amazon.nova-2-lite-v1:0"
            ])
            .unwrap()
            .path,
            link
        );
    }

    #[test]
    fn model_cards_are_read_and_notes_set_by_encoded_model() {
        let call = plan_of(&["model", "card", "us.amazon.nova-2-lite-v1:0"]).unwrap();
        assert_eq!(
            call.path,
            format!("{ORG}/catalog/models/us.amazon.nova-2-lite-v1%3A0/card")
        );
        assert_eq!(call.method, "GET");
        let call = plan_of(&["model", "card", "publishers/google/models/x"]).unwrap();
        assert!(
            call.path
                .ends_with("/models/publishers%2Fgoogle%2Fmodels%2Fx/card")
        );
        assert_eq!(
            plan_of(&["model", "cards"]).unwrap().path,
            format!("{ORG}/catalog/model-cards")
        );
        let call = plan_of(&["model", "note-set", "m", "--note", "Approved"]).unwrap();
        assert_eq!(call.method, "PUT");
        assert_eq!(call.path, format!("{ORG}/catalog/models/m/card/note"));
        assert_eq!(call.body, Some(json!({"note": "Approved"})));
        assert_eq!(
            call.precondition,
            Precondition::Fetch(format!("{ORG}/catalog/models/m/card/note"))
        );
        assert!(plan_of(&["model", "note-set", "m"]).is_err());
        let call = plan_of(&[
            "model",
            "import",
            "m",
            "--repository",
            "openai/gpt-oss-120b",
            "--reported-by",
            "provider",
        ])
        .unwrap();
        assert_eq!(call.method, "PUT");
        assert_eq!(
            call.body,
            Some(json!({"repository": "openai/gpt-oss-120b", "provider_repository": true}))
        );
        assert!(
            plan_of(&[
                "model",
                "import",
                "m",
                "--repository",
                "a/b",
                "--reported-by",
                "me"
            ])
            .is_err()
        );
        let call = plan_of(&["model", "import-remove", "m"]).unwrap();
        assert_eq!(call.method, "DELETE");
        assert_eq!(
            call.precondition,
            Precondition::Fetch(format!("{ORG}/catalog/models/m/card/import"))
        );
    }

    #[test]
    fn evaluations_are_created_and_scored() {
        let call = plan_of(&[
            "evaluations",
            "create",
            "--name",
            "Basics",
            "--items",
            r#"[{"id": "q1", "expected": "Paris", "match": "exact"}]"#,
        ])
        .unwrap();
        assert_eq!(call.method, "POST");
        assert_eq!(call.path, format!("{ORG}/catalog/evaluation-sets"));
        assert_eq!(call.body.as_ref().unwrap()["items"][0]["id"], "q1");
        let call = plan_of(&["evaluations", "run", "s-1", "--batch", "b-1"]).unwrap();
        assert_eq!(call.path, format!("{ORG}/catalog/evaluation-sets/s-1/runs"));
        assert_eq!(call.body, Some(json!({"batch_id": "b-1"})));
        let call = plan_of(&["evaluations", "delete", "s-1"]).unwrap();
        assert_eq!(call.method, "DELETE");
        assert_eq!(
            call.precondition,
            Precondition::Fetch(format!("{ORG}/catalog/evaluation-sets/s-1"))
        );
    }

    #[test]
    fn notifications_are_listed_counted_and_marked_read() {
        let call = plan_of(&["notifications", "--read", "unread", "--kind", "price.held"]).unwrap();
        assert_eq!(call.path, format!("{ORG}/notifications"));
        assert_eq!(
            call.query,
            [
                ("kind".to_owned(), "price.held".to_owned()),
                ("read".to_owned(), "unread".to_owned())
            ]
        );
        let call = plan_of(&["notifications", "count"]).unwrap();
        assert_eq!(call.path, format!("{ORG}/notifications/unread-count"));
        let call = plan_of(&["notifications", "read", "--through", "n-1"]).unwrap();
        assert_eq!(call.method, "POST");
        assert_eq!(call.path, format!("{ORG}/notifications/read"));
        assert_eq!(call.body, Some(json!({ "through": "n-1" })));
        let call = plan_of(&["notifications", "read", "--ids", "a, b"]).unwrap();
        assert_eq!(call.body, Some(json!({ "ids": ["a", "b"] })));
        assert!(plan_of(&["notifications", "read"]).is_err());
        assert!(plan_of(&["notifications", "read", "--through", "a", "--ids", "b"]).is_err());
    }

    #[test]
    fn passes_report_filters_on_as_query_parameters() {
        let call = plan_of(&[
            "usage",
            "--group-by",
            "model",
            "--from",
            "2026-10-01T00:00:00Z",
            "--all",
            "--org",
            "o",
        ])
        .unwrap();
        assert_eq!(call.path, format!("{ORG}/usage"));
        assert_eq!(
            call.query,
            [
                ("from".to_owned(), "2026-10-01T00:00:00Z".to_owned()),
                ("group_by".to_owned(), "model".to_owned())
            ]
        );
        assert!(call.all_pages);
        assert_eq!(call.organization.as_deref(), Some("o"));
    }

    #[test]
    fn rejects_unknown_options_on_typed_writes() {
        assert_eq!(
            plan_of(&[
                "agents", "create", "--name", "a", "--role", "admin", "--rol", "x"
            ])
            .unwrap_err(),
            "unknown option --rol"
        );
    }

    #[test]
    fn offers_a_raw_api_call_relative_to_the_organization() {
        let call = plan_of(&[
            "api",
            "put",
            "usage-export",
            "--data",
            r#"{"kind":"s3"}"#,
            "--if-match",
            "*",
        ])
        .unwrap();
        assert_eq!(
            (call.method, call.path.as_str()),
            ("PUT", format!("{ORG}/usage-export").as_str())
        );
        assert_eq!(call.precondition, Precondition::Value("*".into()));
        let call = plan_of(&["api", "get", "/api/admin/v1/me"]).unwrap();
        assert_eq!(call.path, "/api/admin/v1/me");
        assert_eq!(call.precondition, Precondition::None);
        let call = plan_of(&["api", "delete", "service-accounts/sa"]).unwrap();
        assert_eq!(
            call.precondition,
            Precondition::IfReadable(format!("{ORG}/service-accounts/sa"))
        );
        let call = plan_of(&["api", "post", "service-accounts"]).unwrap();
        assert_eq!(call.precondition, Precondition::None);
    }

    #[test]
    fn reads_and_summarizes_audit_events() {
        let call = plan_of(&["audit", "--outcome", "denied,failed", "--q", "api_key"]).unwrap();
        assert_eq!(call.path, format!("{ORG}/audit-events"));
        assert_eq!(
            call.query,
            [
                ("outcome".into(), "denied,failed".into()),
                ("q".into(), "api_key".into())
            ]
        );
        let call = plan_of(&["audit", "summary", "--group-by", "outcome,day"]).unwrap();
        assert_eq!(call.path, format!("{ORG}/audit-events/summary"));
        assert_eq!(call.query, [("group_by".into(), "outcome,day".into())]);
    }

    #[test]
    fn batches_are_listed_cancelled_and_staged() {
        let call = plan_of(&["batches", "--status", "completed"]).unwrap();
        assert_eq!(call.path, format!("{ORG}/batches"));
        assert_eq!(call.query, [("status".to_owned(), "completed".to_owned())]);
        let call = plan_of(&["batches", "cancel", "batch_1"]).unwrap();
        assert_eq!(
            (call.method, call.path.as_str()),
            ("POST", format!("{ORG}/batches/batch_1/cancel").as_str())
        );
        let call = plan_of(&["batches", "output", "batch_1"]).unwrap();
        assert_eq!(call.path, format!("{ORG}/batches/batch_1/output"));
        assert!(call.raw);
        assert!(!plan_of(&["batches", "get", "batch_1"]).unwrap().raw);
        let call = plan_of(&[
            "batches",
            "configure",
            "--emulated",
            "on",
            "--concurrency",
            "8",
        ])
        .unwrap();
        assert_eq!(
            call.body,
            Some(json!({"emulated_allowed": true, "emulated_concurrency": 8}))
        );
        assert_eq!(
            call.precondition,
            Precondition::Fetch(format!("{ORG}/batch-settings"))
        );
        assert!(plan_of(&["batches", "configure", "--emulated", "maybe"]).is_err());
        let call = plan_of(&[
            "batches",
            "stage",
            "bedrock-main",
            "--bucket",
            "b",
            "--region",
            "us-east-1",
            "--role-arn",
            "arn:aws:iam::123456789012:role/r",
        ])
        .unwrap();
        assert_eq!(call.path, format!("{ORG}/batch-staging/bedrock-main"));
        assert_eq!(
            call.body.as_ref().unwrap()["role_arn"],
            "arn:aws:iam::123456789012:role/r"
        );
        assert!(matches!(call.precondition, Precondition::Find { .. }));
        let call = plan_of(&["batches", "unstage", "bedrock-main"]).unwrap();
        assert_eq!(call.method, "DELETE");
    }

    #[test]
    fn reads_a_traces_content() {
        let call = plan_of(&["trace", "abc", "content"]).unwrap();
        assert_eq!(call.path, format!("{ORG}/traces/abc/content"));
    }

    #[test]
    fn charts_and_lists_usage() {
        let call = plan_of(&[
            "usage",
            "series",
            "--split-by",
            "api_key",
            "--granularity",
            "hour",
        ])
        .unwrap();
        assert_eq!(call.path, format!("{ORG}/usage/series"));
        assert_eq!(
            call.query,
            [
                ("granularity".to_owned(), "hour".to_owned()),
                ("split_by".to_owned(), "api_key".to_owned())
            ]
        );
        let call = plan_of(&["usage", "events", "--status", "failed"]).unwrap();
        assert_eq!(call.path, format!("{ORG}/usage/events"));
    }

    #[test]
    fn lists_and_summarizes_traces() {
        let call = plan_of(&["traces", "--model", "a,b", "--q", "gpt"]).unwrap();
        assert_eq!(call.path, format!("{ORG}/traces"));
        assert_eq!(
            call.query,
            [("model".into(), "a,b".into()), ("q".into(), "gpt".into())]
        );
        let call = plan_of(&["traces", "summary", "--group-by", "team,model"]).unwrap();
        assert_eq!(call.path, format!("{ORG}/traces/summary"));
        assert_eq!(call.query, [("group_by".into(), "team,model".into())]);
    }

    #[test]
    fn sends_chat_with_the_api_key() {
        let call = plan_of(&["chat", "--model", "nova", "say", "pong"]).unwrap();
        assert_eq!(call.auth, Auth::ApiKey);
        assert_eq!(call.body.unwrap()["messages"][0]["content"], "say pong");
    }

    #[test]
    fn merges_and_finds_versions_in_data_pages() {
        let merged = merge_pages(vec![
            json!({ "data": [{ "name": "a", "version": 1 }], "next_page_token": "t" }),
            json!({ "data": [{ "name": "b", "version": 4 }], "next_page_token": null }),
        ]);
        assert_eq!(
            merged,
            json!({
                "data": [{ "name": "a", "version": 1 }, { "name": "b", "version": 4 }],
                "next_page_token": null
            })
        );
        let (key, items) = page_items(&merged).unwrap();
        assert_eq!(key, "data");
        assert_eq!(
            version_of(items, &[("name", "b".into())]),
            Some("\"v4\"".into())
        );
        let plans = json!({ "data": [{ "name": "pro", "version": 2 }] });
        assert_eq!(
            version_of(page_items(&plans).unwrap().1, &[("name", "pro".into())]),
            Some("\"v2\"".into())
        );
        assert_eq!(page_items(&json!({ "id": "x" })), None);
    }

    #[test]
    fn finds_an_items_version_and_merges_pages() {
        let items = [json!({ "deployment": "d", "source": "s", "version": 3 })];
        assert_eq!(
            version_of(
                &items,
                &[("deployment", "d".into()), ("source", "s".into())]
            ),
            Some("\"v3\"".into())
        );
        assert_eq!(version_of(&items, &[("deployment", "x".into())]), None);
        let merged = merge_pages(vec![
            json!({ "items": [1], "next_page_token": "t", "release_id": "r" }),
            json!({ "items": [2], "next_page_token": null }),
        ]);
        assert_eq!(
            merged,
            json!({ "items": [1, 2], "next_page_token": null, "release_id": "r" })
        );
        // `orgs` answers with an array, not a page.
        assert_eq!(
            merge_pages(vec![json!([{ "id": "o" }])]),
            json!([{ "id": "o" }])
        );
    }
}
