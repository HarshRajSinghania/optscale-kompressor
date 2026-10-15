//! The server-side retrieve loop on buffered responses, for all three families.
//!
//! The guarantee under test is always the same: the model asks for elided
//! content, the proxy fetches it, and the client — which never declared the
//! tool and could not answer it — sees one ordinary completion whose `usage`
//! accounts for every round it paid for.

mod support;

use serde_json::{Value, json};
use wiremock::matchers::any;
use wiremock::{Mock, Request, ResponseTemplate};

use support::{
    Harness, anthropic_body, chat_body, first_handle, log_payload, responses_body, round_of,
};

use kompressor_proxy::failopen::Stage;

const ANSWER: &str = "The disk filled at 03:04:17.";

fn body_text(request: &Request) -> String {
    String::from_utf8_lossy(&request.body).into_owned()
}

/// The handle the request is carrying, which is what a model reads out of a
/// marker.
fn handle_in(request: &Request) -> String {
    first_handle(&body_text(request)).expect("the outgoing prompt carries a handle")
}

fn chat_answer() -> Value {
    json!({
        "id": "chatcmpl-final",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": ANSWER},
                     "finish_reason": "stop"}],
        "usage": {"prompt_tokens": 1200, "completion_tokens": 30}
    })
}

fn chat_asks(handle: &str) -> Value {
    json!({
        "id": "chatcmpl-1",
        "choices": [{"index": 0, "message": {"role": "assistant", "content": null, "tool_calls": [
            {"id": "call_r1", "type": "function", "function": {
                "name": "optscale_kompressor_retrieve",
                "arguments": json!({"ref": handle}).to_string()}}
        ]}, "finish_reason": "tool_calls"}],
        "usage": {"prompt_tokens": 1000, "completion_tokens": 20}
    })
}

fn responses_asks(handle: &str) -> Value {
    json!({
        "id": "resp_1",
        "output": [{"type": "function_call", "id": "fc_1", "call_id": "call_r1",
                    "name": "optscale_kompressor_retrieve",
                    "arguments": json!({"ref": handle}).to_string()}],
        "usage": {"input_tokens": 1000, "output_tokens": 20}
    })
}

fn responses_answer() -> Value {
    json!({
        "id": "resp_final",
        "output": [{"type": "message", "role": "assistant",
                    "content": [{"type": "output_text", "text": ANSWER}]}],
        "usage": {"input_tokens": 1200, "output_tokens": 30}
    })
}

fn anthropic_asks(handle: &str) -> Value {
    json!({
        "id": "msg_1",
        "type": "message",
        "role": "assistant",
        "content": [{"type": "tool_use", "id": "toolu_r1", "name": "optscale_kompressor_retrieve",
                     "input": {"ref": handle}}],
        "stop_reason": "tool_use",
        "usage": {"input_tokens": 1000, "output_tokens": 20}
    })
}

fn anthropic_answer() -> Value {
    json!({
        "id": "msg_final",
        "type": "message",
        "role": "assistant",
        "content": [{"type": "text", "text": ANSWER}],
        "stop_reason": "end_turn",
        "usage": {"input_tokens": 1200, "output_tokens": 30}
    })
}

/// Ask once, then answer.
async fn mount_one_round(harness: &Harness, asks: fn(&str) -> Value, answer: fn() -> Value) {
    Mock::given(any())
        .respond_with(move |request: &Request| {
            let text = body_text(request);
            if round_of(&text) == 0 {
                ResponseTemplate::new(200).set_body_json(asks(&handle_in(request)))
            } else {
                ResponseTemplate::new(200).set_body_json(answer())
            }
        })
        .mount(&harness.server)
        .await;
}

#[tokio::test]
async fn the_proxy_answers_an_openai_chat_retrieve_call_and_the_client_sees_only_the_answer() {
    let harness = Harness::new().await;
    mount_one_round(&harness, chat_asks, chat_answer).await;

    let reply = harness
        .post("/v1/chat/completions", &chat_body(&log_payload(400)))
        .await;

    assert_eq!(reply.status, 200);
    let answer = reply.json();
    assert_eq!(answer["choices"][0]["message"]["content"], ANSWER);
    assert!(
        answer["choices"][0]["message"]["tool_calls"].is_null(),
        "the client never asked for that tool and must not be handed its call"
    );
    assert_eq!(reply.header("x-kompressor-retrieve-rounds"), "1");
    assert_eq!(
        answer["usage"]["prompt_tokens"], 2200,
        "usage is the sum across rounds: the client paid for both"
    );
    assert_eq!(answer["usage"]["completion_tokens"], 50);

    let rounds = harness.upstream_json().await;
    assert_eq!(rounds.len(), 2);
    let second = &rounds[1]["messages"];
    let messages = second.as_array().unwrap();
    assert_eq!(messages[4]["tool_calls"][0]["id"], "call_r1");
    assert_eq!(messages[5]["role"], "tool");
    assert_eq!(messages[5]["tool_call_id"], "call_r1");
    let result: Value =
        serde_json::from_str(messages[5]["content"].as_str().unwrap()).expect("a JSON result");
    assert_eq!(result["found"], true);
    assert!(
        result["content"]
            .as_str()
            .unwrap()
            .contains("heartbeat seq="),
        "what comes back is the fragment itself, not a description of it"
    );
    assert_eq!(
        rounds[1]["messages"][3], rounds[0]["messages"][3],
        "the compressed base prompt is unchanged, so the provider's cache prefix holds"
    );
}

#[tokio::test]
async fn the_proxy_answers_an_openai_responses_retrieve_call_in_that_familys_own_shape() {
    let harness = Harness::new().await;
    mount_one_round(&harness, responses_asks, responses_answer).await;

    let reply = harness
        .post("/v1/responses", &responses_body(&log_payload(400)))
        .await;

    assert_eq!(reply.json()["output"][0]["content"][0]["text"], ANSWER);
    assert_eq!(reply.header("x-kompressor-retrieve-rounds"), "1");
    assert_eq!(reply.json()["usage"]["input_tokens"], 2200);

    let rounds = harness.upstream_json().await;
    let input = rounds[1]["input"].as_array().unwrap();
    assert_eq!(input[3]["type"], "function_call");
    assert_eq!(input[4]["type"], "function_call_output");
    assert_eq!(input[4]["call_id"], "call_r1");
    let result: Value =
        serde_json::from_str(input[4]["output"].as_str().unwrap()).expect("a JSON result");
    assert_eq!(result["found"], true);
}

#[tokio::test]
async fn the_proxy_answers_an_anthropic_retrieve_call_with_a_tool_result_block() {
    let harness = Harness::new().await;
    mount_one_round(&harness, anthropic_asks, anthropic_answer).await;

    let reply = harness
        .post("/v1/messages", &anthropic_body(&log_payload(400)))
        .await;

    assert_eq!(reply.json()["content"][0]["text"], ANSWER);
    assert_eq!(reply.json()["usage"]["input_tokens"], 2200);
    assert_eq!(reply.json()["usage"]["output_tokens"], 50);

    let rounds = harness.upstream_json().await;
    let messages = rounds[1]["messages"].as_array().unwrap();
    assert_eq!(messages[3]["role"], "assistant");
    assert_eq!(messages[3]["content"][0]["type"], "tool_use");
    assert_eq!(messages[4]["role"], "user");
    assert_eq!(messages[4]["content"][0]["type"], "tool_result");
    assert_eq!(messages[4]["content"][0]["tool_use_id"], "toolu_r1");
}

#[tokio::test]
async fn a_mixed_turn_hands_over_only_the_clients_tool() {
    let harness = Harness::new().await;
    Mock::given(any())
        .respond_with(move |request: &Request| {
            let handle = handle_in(request);
            ResponseTemplate::new(200).set_body_json(json!({
                "id": "chatcmpl-mixed",
                "choices": [{"message": {"role": "assistant", "tool_calls": [
                    {"id": "call_a", "type": "function", "function": {
                        "name": "optscale_kompressor_retrieve",
                        "arguments": json!({"ref": handle}).to_string()}},
                    {"id": "call_b", "type": "function", "function": {
                        "name": "read_file", "arguments": "{\"path\":\"/etc/fstab\"}"}}
                ]}, "finish_reason": "tool_calls"}]
            }))
        })
        .mount(&harness.server)
        .await;

    let reply = harness
        .post("/v1/chat/completions", &chat_body(&log_payload(400)))
        .await;

    let calls = reply.json()["choices"][0]["message"]["tool_calls"].clone();
    assert_eq!(calls.as_array().unwrap().len(), 1);
    assert_eq!(calls[0]["function"]["name"], "read_file");
    assert_eq!(
        harness.upstream_bodies().await.len(),
        1,
        "half a turn cannot be satisfied, so none of it is"
    );
    assert_eq!(reply.header("x-kompressor-retrieve-rounds"), "0");
}

#[tokio::test]
async fn the_round_cap_trips_and_the_model_is_told_to_finish_with_what_it_has() {
    let harness = Harness::with(|config| config.proxy.retrieve_max_rounds = 1).await;
    Mock::given(any())
        .respond_with(move |request: &Request| {
            let text = body_text(request);
            if text.contains("retrieval budget exhausted") {
                return ResponseTemplate::new(200).set_body_json(chat_answer());
            }
            ResponseTemplate::new(200).set_body_json(chat_asks(&handle_in(request)))
        })
        .mount(&harness.server)
        .await;

    let reply = harness
        .post("/v1/chat/completions", &chat_body(&log_payload(400)))
        .await;

    assert_eq!(reply.json()["choices"][0]["message"]["content"], ANSWER);
    assert_eq!(reply.header("x-kompressor-retrieve-rounds"), "2");
    let rounds = harness.upstream_json().await;
    assert_eq!(rounds.len(), 3, "one real round, one refusal, one answer");
    let last = rounds[2]["messages"].as_array().unwrap().last().unwrap();
    let refusal: Value = serde_json::from_str(last["content"].as_str().unwrap()).unwrap();
    assert_eq!(refusal["found"], false);
    assert_eq!(refusal["error"], "retrieval budget exhausted");
    assert!(
        refusal.get("content").is_none(),
        "the refusal is a refusal, not a smaller fragment"
    );
}

/// The token cap bounds the round it is in, not merely the one after it.
///
/// This used to assert three upstream calls: the original, one that answered a
/// retrieval in full, and only then a refusal. That is the defect, not the
/// behaviour — the loop checked `is_spent()` before the round and charged the
/// total after it, so the first answer went upstream whatever it weighed.
/// Measured with this very configuration, `retrieve_max_tokens_per_request = 1`
/// admitted a single round carrying **12,999 injected tokens** and succeeded.
///
/// With the allowance reserved per answer, a fragment that does not fit is
/// refused rather than sent, so the model learns on the first round and the
/// run costs two calls instead of three. A ceiling of 1 token means nothing
/// can be retrieved, which is what a ceiling of 1 token should mean.
#[tokio::test]
async fn the_token_cap_trips_on_its_own_even_with_rounds_to_spare() {
    let harness = Harness::with(|config| {
        config.proxy.retrieve_max_rounds = 10;
        config.proxy.retrieve_max_tokens_per_request = 1;
    })
    .await;
    Mock::given(any())
        .respond_with(move |request: &Request| {
            let text = body_text(request);
            if text.contains("retrieval budget exhausted") {
                return ResponseTemplate::new(200).set_body_json(chat_answer());
            }
            ResponseTemplate::new(200).set_body_json(chat_asks(&handle_in(request)))
        })
        .mount(&harness.server)
        .await;

    let reply = harness
        .post("/v1/chat/completions", &chat_body(&log_payload(400)))
        .await;

    assert_eq!(reply.json()["choices"][0]["message"]["content"], ANSWER);
    assert_eq!(
        harness.upstream_bodies().await.len(),
        2,
        "a retrieval re-enters the prompt and is paid for again, so an answer \
         that does not fit the remaining allowance is refused rather than sent: \
         the original call and the refusal, and nothing carrying a fragment"
    );
    // And nothing over-budget reached the provider.
    let sent = harness.upstream_bodies().await;
    assert!(
        sent[1].contains("retrieval budget exhausted"),
        "the second call carries the refusal, not the fragment: {}",
        &sent[1][..sent[1].len().min(400)]
    );
}

#[tokio::test]
async fn a_handle_that_does_not_resolve_is_answered_with_regenerate_rather_than_invent() {
    let harness = Harness::new().await;
    Mock::given(any())
        .respond_with(move |request: &Request| {
            let text = body_text(request);
            if round_of(&text) == 0 {
                // a handle from another conversation, or one the model invented
                return ResponseTemplate::new(200).set_body_json(chat_asks("k_AAAAAAAAAAAA"));
            }
            ResponseTemplate::new(200).set_body_json(chat_answer())
        })
        .mount(&harness.server)
        .await;

    let reply = harness
        .post("/v1/chat/completions", &chat_body(&log_payload(400)))
        .await;

    assert_eq!(reply.json()["choices"][0]["message"]["content"], ANSWER);
    let second = &harness.upstream_json().await[1];
    let result: Value = serde_json::from_str(second["messages"][5]["content"].as_str().unwrap())
        .expect("a JSON result");
    assert_eq!(result["found"], false);
    assert!(result.get("content").is_none(), "nothing is invented");
    assert!(result["hint"].as_str().unwrap().contains("Re-run"));
}

#[tokio::test]
async fn an_upstream_that_fails_mid_loop_reports_the_failure_rather_than_an_earlier_turn() {
    let harness = Harness::new().await;
    Mock::given(any())
        .respond_with(move |request: &Request| {
            let text = body_text(request);
            if round_of(&text) == 0 {
                return ResponseTemplate::new(200).set_body_json(chat_asks(&handle_in(request)));
            }
            ResponseTemplate::new(500).set_body_json(json!({"error": {"message": "boom"}}))
        })
        .mount(&harness.server)
        .await;

    let reply = harness
        .post("/v1/chat/completions", &chat_body(&log_payload(400)))
        .await;

    // The loop only runs past a round when that round asked to retrieve, so the
    // only earlier response there can be is one carrying a
    // `optscale_kompressor_retrieve` call. Relaying it would hand the client a tool it
    // never declared — the failure that made `stream_buffer_timeout` ship at 0.
    assert_eq!(
        reply.status, 500,
        "the provider's own failure is reported; an earlier turn is not a \
         substitute for it, because that turn is a tool call"
    );
    assert!(
        reply.json()["choices"][0]["message"]["tool_calls"][0]["function"]["name"].is_null(),
        "no optscale_kompressor_retrieve call reaches a client that never declared one"
    );
    assert_eq!(harness.failopen.count(Stage::RetrieveRound), 1);
}

#[tokio::test]
async fn client_side_execution_hands_the_call_over_instead_of_running_it() {
    let harness = Harness::with(|config| {
        config.proxy.retrieve_execution = kompressor_core::config::RetrieveExecution::Client;
    })
    .await;
    mount_one_round(&harness, chat_asks, chat_answer).await;

    let reply = harness
        .post("/v1/chat/completions", &chat_body(&log_payload(400)))
        .await;

    assert_eq!(
        reply.json()["choices"][0]["message"]["tool_calls"][0]["function"]["name"],
        "optscale_kompressor_retrieve",
        "the operator asked for the client's own loop"
    );
    assert_eq!(harness.upstream_bodies().await.len(), 1);
    assert!(
        harness.upstream_json().await[0]["tools"]
            .as_array()
            .is_some(),
        "the tool is still offered; only its execution moved"
    );
}

#[tokio::test]
async fn a_model_that_ignores_the_refusal_never_gets_its_call_handed_to_the_client() {
    // The regression this guards: the round after a refusal used to be relayed
    // whatever it said. A model that answered "budget exhausted" by calling
    // optscale_kompressor_retrieve again therefore put a tool the client never declared
    // into the client's hands. Codex answers `unsupported call:
    // optscale_kompressor_retrieve` and the turn is spent for nothing.
    let harness = Harness::with(|config| config.proxy.retrieve_max_rounds = 1).await;
    Mock::given(any())
        .respond_with(move |request: &Request| {
            // never takes the hint
            ResponseTemplate::new(200).set_body_json(chat_asks(&handle_in(request)))
        })
        .mount(&harness.server)
        .await;

    let reply = harness
        .post("/v1/chat/completions", &chat_body(&log_payload(400)))
        .await;

    assert_eq!(
        reply.status, 502,
        "a turn the client cannot act on is reported as the failure it is"
    );
    let body = reply.json();
    assert_eq!(body["error"]["type"], "kompressor_retrieve_loop");
    assert!(
        !reply
            .text()
            .contains("\"name\":\"optscale_kompressor_retrieve\""),
        "the client must never be handed a call for a tool it did not declare"
    );
}

/// The retrieve loop across *client turns*, and the thing that ends it.
///
/// The bug: a server-side retrieve answers the model inside a round the client
/// never sees, so the fetched bytes never enter the client's history. Next turn
/// the client re-sends the same conversation, the engine elides the same
/// content to the same handle, and the model asks again. Measured before the
/// fix: 12 retrieval events over 5 distinct fragments, the same three handles
/// fetched on three consecutive turns, each spending the whole round budget.
///
/// A *second* fetch from a different request is what ends it, not the first.
/// One fetch is a fragment the model happened to need; acting on it charges
/// every later turn for content that may never be wanted again, measured at
/// 24% of everything the engine looked at on a run where all eleven fetched
/// handles turned out to be one-offs.
#[tokio::test]
async fn a_fragment_fetched_in_two_turns_is_shipped_whole_in_the_third() {
    let harness = Harness::new().await;

    // The upstream asks for whatever handle it can see, and answers once there
    // is none left to ask about. That is the model's own logic: it asks because
    // a marker is in front of it.
    //
    // "Can see" means in the conversation, not in the tool definition. That
    // definition carries `ref:k_AbC123dEf456` in its examples — four times, to
    // show the marker shapes — so a stub that scans the whole body finds a
    // handle on every turn and asks for one that was never elided. Now that the
    // tool is offered from the first turn rather than from the first handle,
    // that is every turn. Real models have never fallen for it: across 21
    // benchmark runs the example handle was fetched zero times.
    Mock::given(any())
        .respond_with(move |request: &Request| {
            let text = body_text(request);
            let conversation = match text.find(r#""tools""#) {
                Some(at) => text[..at].to_string(),
                None => text.clone(),
            };
            match (round_of(&text), first_handle(&conversation)) {
                (0, Some(handle)) => ResponseTemplate::new(200).set_body_json(chat_asks(&handle)),
                _ => ResponseTemplate::new(200).set_body_json(chat_answer()),
            }
        })
        .mount(&harness.server)
        .await;

    let body = chat_body(&log_payload(400));
    let lines_in = |body: &str| body.matches("heartbeat seq=").count();

    // Turn one: the log is elided, the model asks, the proxy fetches it once.
    assert_eq!(
        harness.post("/v1/chat/completions", &body).await.status,
        200
    );
    let turn_one = harness.upstream_json().await;
    let handle =
        first_handle(&turn_one[0].to_string()).expect("turn one elided the log behind a handle");
    let elided = lines_in(&turn_one[0].to_string());
    assert!(
        elided < 100,
        "turn one must really have elided the log ({elided} of 400 lines present)"
    );

    // Turn two: still elided, because one fetch is not a loop. The model asks
    // again — and that second ask, from a different request, is the evidence.
    assert_eq!(
        harness.post("/v1/chat/completions", &body).await.status,
        200
    );
    let turn_two = harness.upstream_json().await;
    let second_out = turn_two[turn_one.len()].to_string();
    assert!(
        second_out.contains(&handle),
        "one fetch must not be enough: turn two still elides, and the model asks again"
    );

    // Turn three: two fetches in two different requests, so the content now
    // ships whole and there is nothing left to ask for.
    assert_eq!(
        harness.post("/v1/chat/completions", &body).await.status,
        200
    );
    let rounds = harness.upstream_json().await;
    let third_out = rounds[turn_two.len()].to_string();

    assert!(
        !third_out.contains(&handle),
        "fetched in two separate turns, it must not be elided a third time: {handle}"
    );
    let whole = lines_in(&third_out);
    assert!(
        whole > elided * 2,
        "it ships whole instead (turn one carried {elided} lines, turn three {whole})"
    );
    assert_eq!(
        rounds.len(),
        turn_two.len() + 1,
        "and turn three needed no retrieve round at all"
    );
}

/// The control. A promotion belongs to the conversation that earned it: another
/// conversation carrying the same bytes still gets the compression. Without
/// this, the test above would pass for an engine that simply stopped eliding
/// logs once anything had been retrieved anywhere.
#[tokio::test]
async fn another_conversation_carrying_the_same_bytes_is_still_compressed() {
    let harness = Harness::new().await;
    Mock::given(any())
        .respond_with(move |request: &Request| {
            let text = body_text(request);
            match (round_of(&text), first_handle(&text)) {
                (0, Some(handle)) => ResponseTemplate::new(200).set_body_json(chat_asks(&handle)),
                _ => ResponseTemplate::new(200).set_body_json(chat_answer()),
            }
        })
        .mount(&harness.server)
        .await;

    let payload = log_payload(400);
    let body = chat_body(&payload);

    // Two turns, so this conversation really does earn its promotion — a one
    // turn version would prove nothing now that one fetch is not enough.
    for _ in 0..2 {
        assert_eq!(
            harness.post("/v1/chat/completions", &body).await.status,
            200
        );
    }
    let earned = harness.upstream_json().await.len();

    // A different opening, so a different conversation, carrying the same log.
    let mut other = chat_body(&payload);
    other["messages"][1]["content"] = json!("A different question entirely, asked first.");
    assert_eq!(
        harness.post("/v1/chat/completions", &other).await.status,
        200
    );

    let rounds = harness.upstream_json().await;
    let fresh = rounds[earned].to_string();
    assert!(
        first_handle(&fresh).is_some(),
        "a conversation that never retrieved anything still gets the elision"
    );
    assert!(
        fresh.matches("heartbeat seq=").count() < 100,
        "and so does not ship the log whole"
    );
}
