// Port of tests/local_harness/harness_child.cpp. Stand-in for an installed
// coding tool. Reads the real handoff, speaks HTTP, and reports what it saw.
// No model or external network is involved. Never installed or shipped with
// Wally.
use serde_json::{json, Value};
use std::env;
use std::fs;
use std::path::Path;
use std::time::Duration;

fn env_str(name: &str) -> String {
    env::var(name).unwrap_or_default()
}

/// Value following `flag` in argv, if present.
fn argv_value(args: &[String], flag: &str) -> Option<String> {
    args.windows(2).find_map(|pair| {
        if pair[0] == flag {
            Some(pair[1].clone())
        } else {
            None
        }
    })
}

fn require(condition: bool, message: &str) -> Result<(), String> {
    if condition {
        Ok(())
    } else {
        Err(message.to_string())
    }
}

fn read_json(path: &str) -> Result<Value, String> {
    let text = fs::read_to_string(path)
        .map_err(|_| format!("config missing while child is running: {path}"))?;
    serde_json::from_str(&text).map_err(|error| format!("invalid JSON in {path}: {error}"))
}

fn field<'a>(value: &'a Value, key: &str) -> Result<&'a Value, String> {
    value
        .get(key)
        .ok_or_else(|| format!("missing field: {key}"))
}

fn nth(value: &Value, position: usize) -> Result<&Value, String> {
    value
        .get(position)
        .ok_or_else(|| format!("missing array entry: {position}"))
}

fn text_field(value: &Value, key: &str) -> Result<String, String> {
    field(value, key)?
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| format!("field is not a string: {key}"))
}

fn build_agent() -> ureq::Agent {
    let config = ureq::Agent::config_builder()
        .http_status_as_error(false)
        .timeout_connect(Some(Duration::from_secs(2)))
        .timeout_global(Some(Duration::from_secs(5)))
        .build();
    ureq::Agent::new_with_config(config)
}

fn read_body(response: &mut ureq::http::Response<ureq::Body>) -> Result<String, String> {
    response
        .body_mut()
        .with_config()
        .limit(10_000_000)
        .read_to_string()
        .map_err(|error| format!("reading response body failed: {error}"))
}

fn get(agent: &ureq::Agent, url: &str) -> Result<(u16, String), String> {
    let request = ureq::http::Request::builder()
        .method("GET")
        .uri(url)
        .body(String::new())
        .map_err(|error| format!("could not build request: {error}"))?;
    let mut response = agent
        .run(request)
        .map_err(|error| format!("request failed: {error}"))?;
    let status = response.status().as_u16();
    let body = read_body(&mut response)?;
    Ok((status, body))
}

fn post(
    agent: &ureq::Agent,
    url: &str,
    body: &Value,
    extra_headers: &[(&str, String)],
) -> Result<(u16, String), String> {
    let mut builder = ureq::http::Request::builder()
        .method("POST")
        .uri(url)
        .header("Content-Type", "application/json");
    for (name, value) in extra_headers {
        builder = builder.header(*name, value);
    }
    let request = builder
        .body(body.to_string())
        .map_err(|error| format!("could not build request: {error}"))?;
    let mut response = agent
        .run(request)
        .map_err(|error| format!("request failed: {error}"))?;
    let status = response.status().as_u16();
    let text = read_body(&mut response)?;
    Ok((status, text))
}

fn run() -> Result<i32, String> {
    let args: Vec<String> = env::args().collect();
    let tool = Path::new(&args[0])
        .file_stem()
        .and_then(|stem| stem.to_str())
        .unwrap_or_default()
        .to_string();
    // A version probe, as the real tool answers it: a version line and no
    // session. Wally asks OpenCode for its version before a launch (OpenCode 2
    // needs --standalone); WALLY_TEST_OPENCODE_VERSION picks the answer.
    if args.len() == 2 && args[1] == "--version" {
        let version = env_str("WALLY_TEST_OPENCODE_VERSION");
        println!(
            "{}",
            if version.is_empty() {
                "1.18.34"
            } else {
                version.as_str()
            }
        );
        return Ok(0);
    }
    let mut url = String::new();
    let mut model = String::new();
    let mut report = json!({"tool": tool, "args": args[1..], "temporary_files": []});

    if env_str("WALLY_TEST_PASSTHROUGH") == "1" {
        report["inherited_config"] = json!(env_str("OPENCODE_CONFIG_CONTENT"));
    } else if tool == "opencode" {
        let config: Value = serde_json::from_str(&env_str("OPENCODE_CONFIG_CONTENT"))
            .map_err(|error| format!("invalid OPENCODE_CONFIG_CONTENT: {error}"))?;
        let provider = field(field(&config, "provider")?, "runanywhere")?;
        url = text_field(field(provider, "options")?, "baseURL")?;
        require(
            field(provider, "options")?.get("apiKey") == Some(&json!("local")),
            "local placeholder missing",
        )?;
        let selected = text_field(&config, "model")?;
        model = selected
            .strip_prefix("runanywhere/")
            .unwrap_or(&selected)
            .to_string();
        report["limits"] = field(field(provider, "models")?, &model)?
            .get("limit")
            .cloned()
            .unwrap_or(Value::Null);
    } else if tool == "openwork" {
        // OpenWork's engine reads the same variable; the local model is its
        // own provider so a hosted catalog can sit beside it.
        let config: Value = serde_json::from_str(&env_str("OPENCODE_CONFIG_CONTENT"))
            .map_err(|error| format!("invalid OPENCODE_CONFIG_CONTENT: {error}"))?;
        let providers = field(&config, "provider")?;
        let provider = field(providers, "runanywhere-local")?;
        url = text_field(field(provider, "options")?, "baseURL")?;
        let selected = text_field(&config, "model")?;
        model = selected
            .strip_prefix("runanywhere-local/")
            .ok_or_else(|| format!("the local model is not selected: {selected}"))?
            .to_string();
        report["limits"] = field(field(provider, "models")?, &model)?
            .get("limit")
            .cloned()
            .unwrap_or(Value::Null);
        report["providers"] = json!(providers
            .as_object()
            .map(|map| map.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default());
        report["hosted_models"] = json!(providers
            .get("runanywhere")
            .and_then(|hosted| hosted.get("models"))
            .and_then(Value::as_object)
            .map(|models| models.keys().cloned().collect::<Vec<_>>())
            .unwrap_or_default());
        report["profile"] = json!({
            "user_data": env_str("OPENWORK_ELECTRON_USERDATA"),
            "runtime_db": env_str("OPENWORK_RUNTIME_DB"),
            "opencode_db": env_str("OPENCODE_DB"),
            "engine_v2": env_str("OPENWORK_ENGINE_V2_PREVIEW"),
        });
    } else if tool == "dsh" {
        let mut patch = String::new();
        let mut index = 1;
        while index + 1 < args.len() {
            if args[index] == "--patch" {
                patch = args[index + 1].clone();
            }
            index += 1;
        }
        let text = fs::read_to_string(&patch).map_err(|_| "DeepSeek patch missing".to_string())?;
        let marker = "- id: llm-pi-ai\n  config: ";
        let begin = text
            .find(marker)
            .ok_or_else(|| "DeepSeek llm-pi-ai row missing".to_string())?
            + marker.len();
        let end = text[begin..]
            .find('\n')
            .map(|offset| begin + offset)
            .ok_or_else(|| "DeepSeek llm-pi-ai row missing".to_string())?;
        let llm_config: Value = serde_json::from_str(&text[begin..end])
            .map_err(|_| "DeepSeek llm-pi-ai config is not JSON".to_string())?;
        let provider = field(field(&llm_config, "providers")?, "runanywhere")?;
        url = text_field(provider, "baseURL")?;
        let first_model = nth(field(provider, "models")?, 0)?;
        model = text_field(first_model, "id")?;
        let key = text_field(provider, "apiKeyEnv")?;
        require(env_str(&key) == "local", "DeepSeek placeholder missing")?;
        report["temporary_files"] = json!([patch]);
        report["limits"] = json!({
            "context": field(first_model, "contextWindow")?,
            "output": field(first_model, "maxTokens")?,
        });
    } else if tool == "openclaw" {
        let path = env_str("OPENCLAW_CONFIG_PATH");
        let config = read_json(&path)?;
        let provider = field(
            field(field(&config, "models")?, "providers")?,
            "runanywhere",
        )?;
        url = text_field(provider, "baseUrl")?;
        let first_model = nth(field(provider, "models")?, 0)?;
        model = text_field(first_model, "id")?;
        report["temporary_files"] = json!([path]);
        report["limits"] = json!({
            "context": field(first_model, "contextWindow")?,
            "output": field(first_model, "maxTokens")?,
        });
    } else if tool == "hermes" {
        url = env_str("CUSTOM_BASE_URL");
        model = env_str("HERMES_INFERENCE_MODEL");
    } else if tool == "claude" {
        url = env_str("ANTHROPIC_BASE_URL");
        // The launcher passes the model as a `--model` flag; ANTHROPIC_MODEL
        // is still honored as a fallback for older launches.
        model = argv_value(&args, "--model").unwrap_or_else(|| env_str("ANTHROPIC_MODEL"));
        require(
            !env_str("CLAUDE_CODE_MAX_CONTEXT_TOKENS").is_empty(),
            "local context hint missing",
        )?;
    }

    if !url.is_empty() {
        require(
            url.starts_with("http://127.0.0.1:"),
            "endpoint must be loopback",
        )?;
        let base_end = url[7..]
            .find('/')
            .map(|offset| offset + 7)
            .unwrap_or(url.len());
        let base = &url[..base_end];
        let agent = build_agent();
        if tool == "claude" {
            let body = json!({
                "model": model,
                "max_tokens": 32,
                "messages": [{"role": "user", "content": "first turn"}],
            });
            let (status, text) = post(
                &agent,
                &format!("{base}/v1/messages"),
                &body,
                &[("x-api-key", env_str("ANTHROPIC_AUTH_TOKEN"))],
            )
            .map_err(|_| "Anthropic local proxy failed".to_string())?;
            require(status == 200, "Anthropic local proxy failed")?;
            require(
                text.contains("local harness reply"),
                "Anthropic proxy did not return backend output",
            )?;
        } else {
            let (status, text) = get(&agent, &format!("{base}/v1/models"))
                .map_err(|_| "SDK server missing while child is running".to_string())?;
            require(status == 200, "SDK server missing while child is running")?;
            let parsed: Value = serde_json::from_str(&text)
                .map_err(|error| format!("invalid /v1/models response: {error}"))?;
            let advertised_id = nth(field(&parsed, "data")?, 0)?
                .get("id")
                .and_then(Value::as_str);
            require(
                advertised_id == Some(model.as_str()),
                "advertised model differs from selected alias",
            )?;

            let mut messages: Vec<Value> = vec![json!({"role": "user", "content": "first turn"})];
            for _turn in 0..2 {
                let request_body = json!({"model": model, "messages": messages, "max_tokens": 32});
                let (status, text) = post(
                    &agent,
                    &format!("{base}/v1/chat/completions"),
                    &request_body,
                    &[],
                )
                .map_err(|_| "local completion failed".to_string())?;
                require(status == 200, "local completion failed")?;
                let body: Value = serde_json::from_str(&text)
                    .map_err(|error| format!("invalid chat completion response: {error}"))?;
                let content = body
                    .get("choices")
                    .and_then(|choices| choices.get(0))
                    .and_then(|choice| choice.get("message"))
                    .and_then(|message| message.get("content"))
                    .and_then(Value::as_str);
                require(
                    content == Some("local harness reply"),
                    "local backend not used",
                )?;
                messages.push(json!({"role": "assistant", "content": "local harness reply"}));
                messages.push(json!({"role": "user", "content": "second turn"}));
            }
            let streaming_body = json!({
                "model": model,
                "messages": messages,
                "max_tokens": 32,
                "stream": true,
            });
            let (status, text) = post(
                &agent,
                &format!("{base}/v1/chat/completions"),
                &streaming_body,
                &[],
            )
            .map_err(|_| "local streaming response was incomplete".to_string())?;
            require(
                status == 200 && text.contains("data: [DONE]"),
                "local streaming response was incomplete",
            )?;
            require(
                text.contains("local harness reply"),
                "stream output missing",
            )?;
        }
        report["url"] = json!(url);
        report["model"] = json!(model);
    }

    fs::write(
        env_str("WALLY_TEST_CHILD_REPORT"),
        serde_json::to_string_pretty(&report).unwrap_or_default(),
    )
    .map_err(|error| format!("failed to write child report: {error}"))?;
    let exit_code = env_str("WALLY_TEST_CHILD_EXIT");
    if exit_code.is_empty() {
        Ok(0)
    } else {
        exit_code
            .parse::<i32>()
            .map_err(|error| format!("invalid WALLY_TEST_CHILD_EXIT: {error}"))
    }
}

fn main() {
    match run() {
        Ok(code) => std::process::exit(code),
        Err(message) => {
            eprintln!("harness fixture: {message}");
            std::process::exit(91);
        }
    }
}
