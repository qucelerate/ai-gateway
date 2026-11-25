//! `gwctl`: a client CLI over the gateway's admin and inference APIs, for
//! scripts and autonomous agents (plan §10, Phase 5). It authenticates with
//! an agent token, so every change it makes is permission-checked and
//! audited under that agent, and prints JSON.
//!
//! Exit codes: 0 success; 1 the gateway refused (its problem document is
//! printed to standard error); 2 a usage error; 3 the gateway could not be
//! reached.

use std::{io::Read, process::ExitCode};

use serde_json::Value;

use gateway::agent_tools::{
    args::Args,
    client::{Client, Failure, Settings},
    commands::{self, Plan, USAGE},
    mcp,
};

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let args = match Args::parse(std::env::args().skip(1)) {
        Ok(args) => args,
        Err(message) => return usage_error(&message),
    };
    let compact = args.flag("compact");
    // `gwctl mcp` serves tools over stdio; everything else is one call.
    let call = if args.positional == ["mcp"] && args.options.is_empty() && !args.flag("help") {
        None
    } else {
        match commands::plan(args, &read_json) {
            Ok(Plan::Help) => {
                print!("{USAGE}");
                return ExitCode::SUCCESS;
            }
            Ok(Plan::Call(call)) => Some(call),
            Ok(Plan::AcceptAllAgreements {
                list,
                accept,
                verify,
                yes,
            }) => {
                return accept_all(*list, *accept, verify, yes, compact).await;
            }
            Err(message) => return usage_error(&message),
        }
    };
    let settings = match settings() {
        Ok(settings) => settings,
        Err(code) => return code,
    };
    let result = match (Client::new(settings), call) {
        (Ok(client), Some(call)) => client.run(call).await,
        (Ok(client), None) => {
            mcp::serve(client).await;
            return ExitCode::SUCCESS;
        }
        (Err(failure), _) => Err(failure),
    };
    finish(result, compact)
}

fn settings() -> Result<Settings, ExitCode> {
    let env = |name: &str| {
        std::env::var(name)
            .ok()
            .filter(|value| !value.trim().is_empty())
    };
    let Some(url) = env("GATEWAY_URL") else {
        return Err(usage_error(
            "set GATEWAY_URL to the gateway origin, such as https://gateway.example.com",
        ));
    };
    Ok(Settings {
        url,
        agent_token: env("GATEWAY_AGENT_TOKEN"),
        api_key: env("GATEWAY_API_KEY"),
        organization: env("GATEWAY_ORGANIZATION"),
    })
}

/// `agreements accept KEY --all`: list, show what would be accepted with
/// each offer's rates on standard error, ask unless `--yes`, then accept
/// exactly those named offers.
async fn accept_all(
    list: commands::Call,
    mut accept: commands::Call,
    verify: bool,
    yes: bool,
    compact: bool,
) -> ExitCode {
    let settings = match settings() {
        Ok(settings) => settings,
        Err(code) => return code,
    };
    let client = match Client::new(settings) {
        Ok(client) => client,
        Err(failure) => return finish(Err(failure), compact),
    };
    let view = match client.run(list).await {
        Ok(view) => view,
        Err(failure) => return finish(Err(failure), compact),
    };
    let choice = commands::agreement_choice(&view);
    for line in &choice.skipped {
        eprintln!("gwctl: skipped {line}");
    }
    if choice.pairs.is_empty() {
        eprintln!("gwctl: nothing to accept");
        return ExitCode::SUCCESS;
    }
    eprintln!(
        "gwctl: accepting {} AWS Marketplace agreement(s), each a subscription billed per use at its offer's rates:",
        choice.pairs.len()
    );
    for line in &choice.lines {
        eprintln!("  {line}");
    }
    if verify {
        eprintln!("gwctl: --verify also calls each model once, a 1-token request billed by AWS");
    }
    if !yes {
        eprint!("Accept these {} agreement(s)? [y/N] ", choice.pairs.len());
        let mut answer = String::new();
        if std::io::stdin().read_line(&mut answer).is_err()
            || !matches!(answer.trim(), "y" | "Y" | "yes")
        {
            eprintln!("gwctl: nothing accepted");
            return ExitCode::from(1);
        }
    }
    accept.body = Some(serde_json::json!({ "models": choice.pairs, "verify": verify }));
    finish(client.run(accept).await, compact)
}

fn finish(result: Result<Value, Failure>, compact: bool) -> ExitCode {
    match result {
        // A file (a batch's JSONL output or errors, kept as text whatever
        // it holds), or a body that is not JSON, is printed as it came.
        Ok(Value::String(text)) => {
            print!("{text}");
            ExitCode::SUCCESS
        }
        Ok(body) => {
            if !body.is_null() {
                println!("{}", render(&body, compact));
            }
            ExitCode::SUCCESS
        }
        Err(Failure::Problem { status, body }) => {
            let body = match body {
                Value::Null => serde_json::json!({ "status": status.as_u16() }),
                body => body,
            };
            eprintln!("{}", render(&body, compact));
            ExitCode::from(1)
        }
        Err(Failure::Usage(message)) => usage_error(&message),
        Err(Failure::Transport(message)) => {
            eprintln!("gwctl: the gateway could not be reached: {message}");
            ExitCode::from(3)
        }
        Err(Failure::Unknown {
            message,
            idempotency_key,
            if_match,
        }) => {
            let mut retry = format!("--idempotency-key {idempotency_key}");
            if let Some(value) = if_match {
                retry.push_str(&format!(" --if-match '{value}'"));
            }
            eprintln!(
                "gwctl: the gateway could not be reached: {message}; the write may have been applied. Retry the same command with {retry}"
            );
            ExitCode::from(3)
        }
    }
}

fn render(value: &Value, compact: bool) -> String {
    let rendered = if compact {
        serde_json::to_string(value)
    } else {
        serde_json::to_string_pretty(value)
    };
    rendered.unwrap_or_default()
}

fn usage_error(message: &str) -> ExitCode {
    eprintln!("gwctl: {message}");
    ExitCode::from(2)
}

/// A JSON argument: inline, `@file`, or `-` for standard input.
fn read_json(source: &str) -> Result<Value, String> {
    let text = if source == "-" {
        let mut text = String::new();
        std::io::stdin()
            .read_to_string(&mut text)
            .map_err(|error| format!("standard input: {error}"))?;
        text
    } else if let Some(file) = source.strip_prefix('@') {
        std::fs::read_to_string(file).map_err(|error| format!("{file}: {error}"))?
    } else {
        source.to_owned()
    };
    serde_json::from_str(&text).map_err(|error| format!("not JSON: {error}"))
}
