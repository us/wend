use super::*;
use serde_json::json;

/// A deterministic, high-entropy alphanumeric string (test secrets must clear
/// the rules' entropy thresholds).
fn noise(len: usize, seed: u64) -> String {
    const ALPHABET: &[u8] = b"aB3dE5fG7hJ9kL2mN4pQ6rS8tU0vW1xY2zA4cD6eF8gH0iK1jM3nP5qR7sT9uV";
    let mut x = seed
        .wrapping_mul(6364136223846793005)
        .wrapping_add(1442695040888963407);
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            ALPHABET[(x % ALPHABET.len() as u64) as usize] as char
        })
        .collect()
}

fn assert_redacted(input: &str, secret: &str) {
    let (out, hits) = text(input);
    assert!(!out.contains(secret), "secret survived in {out:?}");
    assert!(!hits.is_empty(), "no hit reported for {input:?}");
    for h in &hits {
        assert!(h.masked.ends_with("...") && h.masked.chars().count() <= 7);
    }
}

#[test]
fn every_vendored_rule_compiles() {
    let vendored: serde_json::Value = serde_json::from_str(include_str!("gitleaks.json")).unwrap();
    let n = vendored["rules"].as_array().unwrap().len();
    assert_eq!(
        rule_count(),
        n + engine::Engine::custom_count(),
        "a rule failed to compile"
    );
}

#[test]
fn seeded_secrets_per_family_are_redacted() {
    let n = noise;
    let cases: Vec<(String, String)> = vec![
        (format!("AKIA{}", "Q7XJ2LMN4PRS6TVW"), String::new()),
        (format!("ghp_{}", n(36, 1)), String::new()),
        (format!("github_pat_{}", n(82, 2)), String::new()),
        (format!("glpat-{}", n(20, 3)), String::new()),
        (
            format!("xoxb-1234567890-1234567890123-{}", n(24, 4)),
            String::new(),
        ),
        (format!("sk-ant-api03-{}AA", n(93, 5)), String::new()),
        (format!("sk-ant-oat01-{}", n(40, 6)), String::new()),
        (format!("sk-proj-{}", n(48, 7)), String::new()),
        (format!("sk_live_{}", n(32, 8)), String::new()),
        (format!("AIza{}", n(35, 9)), String::new()),
        (format!("sb_secret_{}", n(32, 10)), String::new()),
        (format!("wend_dev_{}", n(36, 11)), String::new()),
        (format!("npm_{}", n(36, 12)), String::new()),
    ];
    for (secret, _) in &cases {
        assert_redacted(&format!("token is {secret} ok"), secret);
    }
}

#[test]
fn structured_secrets_are_redacted() {
    let key = n_lines();
    assert_redacted(&key, "MIIEow");
    assert_redacted(
        "postgres://admin:hunter2pass@db.example.com:5432/app",
        "hunter2pass",
    );
    assert_redacted(
        "OPENAI_API_KEY=sk-abcdefghijklmnopqrstuvwxyz012345",
        "sk-abcdefghij",
    );
    assert_redacted("DATABASE_PASSWORD: s3cr3t-value-9", "s3cr3t-value-9");
    assert_redacted(
        "curl -H 'Authorization: Bearer eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9abc'",
        "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9abc",
    );
    assert_redacted(
        "anon key eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.dozjgNryP4J3jVmNHl0w5N",
        "dozjgNryP4J3jVmNHl0w5N",
    );
    // A vendor key no named rule knows: caught by entropy.
    let unknown = noise(40, 42);
    assert_redacted(&format!("key {unknown}"), &unknown);
    // A secret hidden in base64.
    use base64::Engine as _;
    let ghp = format!("ghp_{}", noise(36, 13));
    let encoded = base64::engine::general_purpose::STANDARD.encode(format!("token={ghp}"));
    assert_redacted(&format!("data: {encoded}"), &encoded);
}

fn n_lines() -> String {
    format!(
        "-----BEGIN RSA PRIVATE KEY-----\nMIIEow{}\n-----END RSA PRIVATE KEY-----",
        noise(64, 20)
    )
}

#[test]
fn key_fragments_are_caught_in_every_piece() {
    let head = "-----BEGIN RSA PRIVATE KEY-----\nMIIEow0BAQEFAASCAmVeryLongBase64";
    let middle = noise(80, 70); // a real key body line: random base64, no marker
    let tail = "shortTail==\n-----END RSA PRIVATE KEY-----";
    assert!(!text(head).0.contains("MIIEow"));
    assert!(!text(&middle).0.contains(&middle[..20]));
    assert!(!text(tail).0.contains("shortTail"));
}

#[test]
fn ordinary_text_survives() {
    for s in [
        "dostum bu embedding modelini nasil degistirebiliriz acaba?",
        "commit 144f9c5b2d8e4f7a9c1b3d5e7f9a1b3c5d7e9f1a fixed it",
        "session 7242d5ee-3e5c-43aa-89e6-ce3b2754602c resumed",
        "crates/wend-core/src/store/mod.rs:226",
        "let api_key = std::env::var(\"OPENAI_API_KEY\")?;",
        "TOKEN=${GITHUB_TOKEN}",
        "password: <your-password>",
    ] {
        let (out, hits) = text(s);
        assert_eq!(out, s, "changed: {hits:?}");
    }
}

#[test]
fn scrubbing_is_idempotent() {
    let input = format!(
        "KEY=sk-proj-{} and ghp_{} and postgres://a:secretpw1@h/db",
        noise(48, 30),
        noise(36, 31)
    );
    let (once, _) = text(&input);
    let (twice, hits) = text(&once);
    assert_eq!(once, twice, "second pass changed the text: {hits:?}");
}

fn line(v: serde_json::Value) -> String {
    serde_json::to_string(&v).unwrap()
}

#[test]
fn claude_sensitive_read_drops_every_copy() {
    let secret = format!("sk-proj-{}", noise(48, 40));
    let plain = "SOME_SETTING=on"; // would survive regex scrubbing
    let mut s = Scrubber::new(Format::Claude);
    let call = line(
        json!({"type":"assistant","message":{"role":"assistant","content":[
        {"type":"tool_use","id":"toolu_1","name":"Read","input":{"file_path":"/repo/.env"}}]}}),
    );
    let result = line(json!({"type":"user","message":{"role":"user","content":[
        {"type":"tool_result","tool_use_id":"toolu_1","content":format!("{plain}\nOPENAI={secret}")}]},
        "toolUseResult":{"type":"text","file":{"filePath":"/repo/.env","content":format!("{plain}\nOPENAI={secret}")}}}));
    let (_, _) = s.line(&call);
    let (out, _) = s.line(&result);
    assert!(
        !out.contains(&secret) && !out.contains(plain),
        "leaked: {out}"
    );
    assert!(out.contains(SENSITIVE_READ));
}

#[test]
fn claude_bash_printenv_and_sensitive_write() {
    let mut s = Scrubber::new(Format::Claude);
    s.line(&line(json!({"type":"assistant","message":{"content":[
        {"type":"tool_use","id":"t1","name":"Bash","input":{"command":"printenv | sort"}},
        {"type":"tool_use","id":"t2","name":"Write","input":{"file_path":"/x/.env","content":"A=plainvalue"}}]}})));
    let (out, _) = s.line(&line(json!({"type":"user","message":{"content":[
        {"type":"tool_result","tool_use_id":"t1","content":"HOME=/Users/x\nA=plainvalue"}]},
        "toolUseResult":{"stdout":"HOME=/Users/x\nA=plainvalue","stderr":""}})));
    assert!(!out.contains("plainvalue"), "{out}");

    let mut s = Scrubber::new(Format::Claude);
    let (out, _) = s.line(&line(json!({"type":"assistant","message":{"content":[
        {"type":"tool_use","id":"t2","name":"Write","input":{"file_path":"/x/.env","content":"A=plainvalue"}}]}})));
    assert!(
        !out.contains("plainvalue") && out.contains(SENSITIVE_WRITE),
        "{out}"
    );
}

#[test]
fn claude_wend_output_is_dropped_but_other_repos_are_not() {
    let mut s = Scrubber::new(Format::Claude);
    s.line(&line(json!({"type":"assistant","message":{"content":[
        {"type":"tool_use","id":"w","name":"Bash","input":{"command":"wend search --cloud auth"}},
        {"type":"tool_use","id":"c","name":"Bash","input":{"command":"cd ~/coding/wend && cargo test"}}]}})));
    let (out, _) = s.line(&line(json!({"type":"user","message":{"content":[
        {"type":"tool_result","tool_use_id":"w","content":"Ayse decided X"},
        {"type":"tool_result","tool_use_id":"c","content":"test result: ok"}]}})));
    assert!(
        !out.contains("Ayse decided X") && out.contains(WEND_OUTPUT),
        "{out}"
    );
    assert!(out.contains("test result: ok"), "{out}");
}

#[test]
fn claude_images_and_thinking() {
    let mut s = Scrubber::new(Format::Claude);
    let (out, _) = s.line(&line(json!({"type":"user","message":{"content":[
        {"type":"image","source":{"type":"base64","media_type":"image/png","data":noise(200, 50)}}]}})));
    assert!(
        out.contains("[image omitted]") && !out.contains("base64"),
        "{out}"
    );

    let sig = noise(300, 51);
    let clean = line(json!({"type":"assistant","message":{"content":[
        {"type":"thinking","thinking":"plan the refactor","signature":sig}]}}));
    let (out, hits) = s.line(&clean);
    assert_eq!(
        out, clean,
        "clean thinking must stay byte-identical: {hits:?}"
    );

    let dirty = line(json!({"type":"assistant","message":{"content":[
        {"type":"thinking","thinking":format!("use ghp_{}", noise(36, 52)),"signature":sig},
        {"type":"text","text":"done"}]}}));
    let (out, _) = s.line(&dirty);
    assert!(
        !out.contains("ghp_") && !out.contains(&sig) && out.contains("done"),
        "{out}"
    );
}

#[test]
fn unchanged_lines_keep_their_bytes() {
    let mut s = Scrubber::new(Format::Claude);
    let raw = r#"{"type":"user","message":{"role":"user","content":"hello there"},"uuid":"u1"}"#;
    assert_eq!(s.line(raw).0, raw);
}

#[test]
fn codex_sensitive_exec_drops_outputs_and_keeps_encrypted_reasoning() {
    let secret = format!("ghp_{}", noise(36, 60));
    let mut s = Scrubber::new(Format::Codex);
    s.line(&line(
        json!({"type":"response_item","payload":{"type":"function_call","name":"exec_command",
        "arguments":"{\"cmd\":\"cat .env\"}","call_id":"call_9"}}),
    ));
    let (out, _) = s.line(&line(
        json!({"type":"response_item","payload":{"type":"function_call_output",
        "call_id":"call_9","output":"PLAIN=visible"}}),
    ));
    assert!(
        !out.contains("PLAIN=visible") && out.contains(SENSITIVE_READ),
        "{out}"
    );
    let (out, _) = s.line(&line(json!({"type":"event_msg","payload":{"type":"exec_command_end",
        "call_id":"call_9","stdout":format!("T={secret}"),"aggregated_output":"PLAIN=visible","formatted_output":"PLAIN=visible"}})));
    assert!(
        !out.contains("PLAIN=visible") && !out.contains(&secret),
        "{out}"
    );

    let enc = noise(400, 61);
    let reasoning = line(
        json!({"type":"response_item","payload":{"type":"reasoning","encrypted_content":enc}}),
    );
    assert_eq!(s.line(&reasoning).0, reasoning);
}

#[test]
fn codex_apply_patch_to_env_is_dropped() {
    let mut s = Scrubber::new(Format::Codex);
    let (out, _) = s.line(&line(json!({"type":"response_item","payload":{"type":"custom_tool_call","name":"apply_patch",
        "call_id":"p1","input":"*** Begin Patch\n*** Add File: .env\n+DB=plainvalue\n*** End Patch"}})));
    assert!(
        !out.contains("plainvalue") && out.contains(SENSITIVE_WRITE),
        "{out}"
    );
}

#[test]
fn secret_named_fields_are_redacted() {
    let mut s = Scrubber::new(Format::Claude);
    let (out, _) = s.line(&line(json!({"type":"assistant","message":{"content":[
        {"type":"tool_use","id":"m","name":"mcp__x__call","input":{"api_key":"plainkeyvalue1","author":"Ayse Kaya","auth_token":"plaintokenvalue"}}]}})));
    assert!(
        !out.contains("plainkeyvalue1") && !out.contains("plaintokenvalue"),
        "{out}"
    );
    assert!(out.contains("Ayse Kaya"), "{out}");
}

#[test]
#[ignore = "throughput benchmark: cargo test -p wend-core --release scrub_throughput -- --ignored --nocapture"]
fn scrub_throughput() {
    let mut sample = String::new();
    while sample.len() < 200 << 20 {
        sample.push_str(&format!(
            "{{\"type\":\"user\",\"message\":{{\"content\":\"refactor the store module and run cargo test; commit {} later\"}}}}\n",
            noise(12, sample.len() as u64)
        ));
    }
    let start = std::time::Instant::now();
    let mut s = Scrubber::new(Format::Claude);
    for l in sample.lines() {
        let _ = s.line(l);
    }
    let secs = start.elapsed().as_secs_f64();
    println!(
        "scrub throughput: {:.1} MB/s",
        (sample.len() as f64 / 1e6) / secs
    );
}

/// Corpus report for tuning: rule counts and, per rule, a few hits shown only
/// as length + masked prefix + the (already scrubbed) text just before them.
/// `WEND_SCRUB_CORPUS=<dir> cargo test --release -p wend-core scrub_corpus_report -- --ignored --nocapture`
#[test]
#[ignore = "reads a local transcript directory"]
fn scrub_corpus_report() {
    let Ok(dir) = std::env::var("WEND_SCRUB_CORPUS") else {
        return;
    };
    let mut counts: std::collections::BTreeMap<String, usize> = Default::default();
    let mut samples: std::collections::BTreeMap<String, Vec<String>> = Default::default();
    let (mut lines, mut bytes, mut changed) = (0usize, 0usize, 0usize);
    let start = std::time::Instant::now();
    let mut stack = vec![std::path::PathBuf::from(dir)];
    while let Some(p) = stack.pop() {
        if p.is_dir() {
            stack.extend(std::fs::read_dir(&p).unwrap().flatten().map(|e| e.path()));
            continue;
        }
        if p.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(content) = std::fs::read_to_string(&p) else {
            continue;
        };
        let mut s = Scrubber::new(Format::Claude);
        for l in content.lines() {
            lines += 1;
            bytes += l.len();
            let (out, hits) = s.line(l);
            if out != l {
                changed += 1;
            }
            for h in hits {
                *counts.entry(h.rule.clone()).or_default() += 1;
                let ctx = out
                    .find(&format!("[REDACTED:{}]", h.rule))
                    .map(|i| {
                        let mut start = i.saturating_sub(40);
                        while !out.is_char_boundary(start) {
                            start += 1;
                        }
                        out[start..i].replace('\n', " ")
                    })
                    .unwrap_or_default();
                let v = samples.entry(h.rule).or_default();
                if v.len() < 4 {
                    v.push(format!("{} | ...{ctx}", h.masked));
                }
            }
        }
    }
    println!(
        "lines {lines}, {:.1} MB, changed lines {changed}, {:.1}s",
        bytes as f64 / 1e6,
        start.elapsed().as_secs_f64()
    );
    for (rule, n) in &counts {
        println!("{n:>7}  {rule}");
        for s in &samples[rule] {
            println!("           {s}");
        }
    }
}

#[test]
fn codex_wend_calls_in_every_shape() {
    for call in [
        json!({"type":"event_msg","payload":{"type":"exec_command_begin","call_id":"c1","command":["bash","-lc","wend search x"]}}),
        json!({"type":"event_msg","payload":{"type":"exec_command_begin","call_id":"c1","command":["/bin/zsh","-lc","wend search x"]}}),
        json!({"type":"response_item","payload":{"type":"function_call","name":"shell","call_id":"c1",
            "arguments":"{\"command\":[\"bash\",\"-lc\",\"wend search x\"]}"}}),
        json!({"type":"response_item","payload":{"type":"local_shell_call","call_id":"c1",
            "action":{"type":"exec","command":["bash","-lc","wend search x"]}}}),
    ] {
        let mut s = Scrubber::new(Format::Codex);
        s.line(&line(call.clone()));
        let (out, _) = s.line(&line(
            json!({"type":"event_msg","payload":{"type":"exec_command_end",
            "call_id":"c1","stdout":"PRIVATE NOTE","aggregated_output":"PRIVATE NOTE"}}),
        ));
        assert!(
            !out.contains("PRIVATE NOTE") && out.contains(WEND_OUTPUT),
            "{call} -> {out}"
        );
    }
}

#[test]
fn codex_local_shell_call_reading_env_is_dropped() {
    let mut s = Scrubber::new(Format::Codex);
    s.line(&line(
        json!({"type":"response_item","payload":{"type":"local_shell_call","call_id":"l1",
        "action":{"type":"exec","command":["bash","-lc","cat .env"]}}}),
    ));
    let (out, _) = s.line(&line(
        json!({"type":"response_item","payload":{"type":"function_call_output",
        "call_id":"l1","output":"PLAIN=visible"}}),
    ));
    assert!(!out.contains("PLAIN=visible"), "{out}");
}

#[test]
fn code_that_mentions_key_or_pem_is_not_a_sensitive_call() {
    let mut s = Scrubber::new(Format::Claude);
    let edit = line(json!({"type":"assistant","message":{"content":[
        {"type":"tool_use","id":"e","name":"Edit","input":{"file_path":"/repo/src/map.rs",
         "old_string":"return item.key ;","new_string":"return item.key; // load cert.pem via printenv docs"}}]}}));
    assert_eq!(s.line(&edit).0, edit);
    let (out, _) = s.line(&line(json!({"type":"user","message":{"content":[
        {"type":"tool_result","tool_use_id":"e","content":"edited"}]}})));
    assert!(out.contains("edited"));
}

#[test]
fn heredoc_into_env_is_blanked() {
    let mut s = Scrubber::new(Format::Claude);
    let (out, _) = s.line(&line(json!({"type":"assistant","message":{"content":[
        {"type":"tool_use","id":"h","name":"Bash","input":{"command":"cat > .env <<EOF\nSESSION=plainvalue12\nEOF"}}]}})));
    assert!(
        !out.contains("plainvalue12") && out.contains(SENSITIVE_WRITE),
        "{out}"
    );
}

#[test]
fn images_in_tool_use_result_and_codex_inputs_are_replaced() {
    let data = noise(300, 80);
    let mut s = Scrubber::new(Format::Claude);
    let (out, _) = s.line(&line(json!({"type":"user","message":{"content":[{"type":"text","text":"see"}]},
        "toolUseResult":[{"type":"image","source":{"type":"base64","media_type":"image/png","data":data}}]})));
    assert!(
        !out.contains(&data[..40]) && out.contains("[image omitted]"),
        "{out}"
    );
    let mut s = Scrubber::new(Format::Codex);
    let (out, _) = s.line(&line(
        json!({"type":"response_item","payload":{"type":"message","role":"user",
        "content":[{"type":"input_image","image_url":format!("data:image/png;base64,{data}")}]}}),
    ));
    assert!(!out.contains(&data[..40]), "{out}");
}

#[test]
fn scrubbed_lines_are_stable_and_keep_key_order() {
    let mut s = Scrubber::new(Format::Claude);
    let raw = format!(
        r#"{{"zeta":1,"type":"user","alpha":1.0,"message":{{"content":"use ghp_{} now"}}}}"#,
        noise(36, 90)
    );
    let (once, _) = s.line(&raw);
    assert!(once.find("zeta") < once.find("alpha"), "{once}");
    assert!(once.contains("1.0"), "{once}");
    let mut s2 = Scrubber::new(Format::Claude);
    assert_eq!(s2.line(&once).0, once, "second pass changed the line");
}

#[test]
fn masks_never_reveal_short_secrets() {
    let (_, hits) = text("postgres://admin:abc123@host/db");
    for h in hits {
        assert!(!h.masked.contains("abc1"), "{h:?}");
    }
}

#[test]
fn codex_exec_event_flags_itself_and_every_output_field() {
    let mut s = Scrubber::new(Format::Codex);
    let (out, _) = s.line(&line(
        json!({"type":"event_msg","payload":{"type":"exec_command_end",
        "call_id":"x1","command":["/bin/zsh","-lc","cat .env"],
        "stdout":"PLAIN=visible","stderr":"PLAIN=visible","aggregated_output":"PLAIN=visible",
        "formatted_output":"PLAIN=visible"}}),
    ));
    assert!(!out.contains("PLAIN=visible"), "{out}");
}

#[test]
fn request_copies_carry_the_same_drops() {
    let mut s = Scrubber::new(Format::Claude);
    let (out, _) = s.line(&line(
        json!({"type":"api-request-blob","message":{"role":"user","content":[
        {"type":"tool_use","id":"r1","name":"Read","input":{"file_path":"/x/.env"}},
        {"type":"tool_result","tool_use_id":"r1","content":"PLAIN=visible"}]}}),
    ));
    assert!(
        !out.contains("PLAIN=visible") && out.contains(SENSITIVE_READ),
        "{out}"
    );
}

#[test]
fn bang_commands_and_mentioned_files() {
    let mut s = Scrubber::new(Format::Claude);
    s.line(&line(json!({"type":"user","message":{"role":"user","content":"<bash-input>cat .env</bash-input>"}})));
    let (out, _) = s.line(&line(json!({"type":"user","message":{"role":"user",
        "content":"<bash-stdout>PLAIN=visible</bash-stdout><bash-stderr></bash-stderr>"}})));
    assert!(!out.contains("PLAIN=visible"), "{out}");
    let (out, _) = s.line(&line(
        json!({"type":"attachment","attachment":{"type":"file",
        "filename":"/repo/.env","content":"PLAIN=visible"}}),
    ));
    assert!(!out.contains("PLAIN=visible"), "{out}");
}

#[test]
fn names_that_are_not_secrets_survive() {
    let mut s = Scrubber::new(Format::Claude);
    for v in [
        json!({"type":"x","tokenizer":"cl100k_base","secretName":"prod-db-main","password_policy":"min-length-12"}),
        json!({"type":"assistant","message":{"content":[{"type":"tool_use","id":"r","name":"Edit",
            "input":{"file_path":"/repo/README.md","old_string":"setup","new_string":"cp .env.example .env"}}]}}),
    ] {
        let l = line(v);
        assert_eq!(s.line(&l).0, l);
    }
}

#[cfg(feature = "azure")]
#[test]
fn azure_requests_go_through_the_scrubber() {
    let key = format!("ghp_{}", noise(36, 99));
    assert!(!crate::embed::azure::redact(&format!("x {key}")).contains("ghp_"));
}
