//! Where the prompt the provider cached stops matching the one it is sent next.
//!
//! DESIGN §20, open question 8, records a prefix break measured on eleven live benchmark runs
//! and says the aggregates cannot say what moves: the turn that mints the first
//! handle is also the turn the retrieve tool is injected *and* the turn the
//! proxy stops being a byte pipe and starts re-serializing. Three candidates,
//! one event. This reproduces the transition hermetically and reports the first
//! byte that differs, which separates them.

mod support;

use support::{log_payload, Harness};
use wiremock::matchers::any;
use wiremock::{Mock, ResponseTemplate};

fn answered() -> ResponseTemplate {
    ResponseTemplate::new(200).set_body_json(serde_json::json!({
        "id": "resp_1",
        "object": "response",
        "output": [{"type": "message", "role": "assistant",
                    "content": [{"type": "output_text", "text": "ok"}]}],
        "usage": {"input_tokens": 100, "output_tokens": 5}
    }))
}

/// The upstream body with our own retrieve tool taken back out.
fn without_our_tool(body: &str) -> String {
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(body) else {
        return body.to_string();
    };
    let Some(tools) = value
        .get_mut("tools")
        .and_then(serde_json::Value::as_array_mut)
    else {
        return body.to_string();
    };
    tools.retain(|tool| tool["name"] != "optscale_kompressor_retrieve");
    if tools.is_empty() {
        value.as_object_mut().map(|object| object.remove("tools"));
    }
    value.to_string()
}

/// Longest common prefix of two byte strings.
fn shared(a: &[u8], b: &[u8]) -> usize {
    a.iter().zip(b).take_while(|(x, y)| x == y).count()
}

fn around(bytes: &[u8], at: usize, span: usize) -> String {
    let from = at.saturating_sub(span);
    let to = (at + span).min(bytes.len());
    String::from_utf8_lossy(&bytes[from..to]).into_owned()
}

#[tokio::test]
async fn the_first_handle_turn_diverges_from_the_cached_prefix_at() {
    let harness = Harness::new().await;
    Mock::given(any())
        .respond_with(answered())
        .mount(&harness.server)
        .await;

    // Turn one: nothing worth compressing, so the proxy is a pipe.
    let small = serde_json::json!({
        "model": "gpt-5",
        "instructions": "You are a careful assistant.",
        "input": [
            {"type": "message", "role": "user",
             "content": [{"type": "input_text", "text": "What went wrong in this log?"}]},
            {"type": "function_call_output", "call_id": "call_0", "output": log_payload(2)}
        ]
    });
    harness.post("/v1/responses", &small).await;

    // Turn two: the same opening, plus a payload that compresses. This is the
    // turn that mints the first handle.
    let mut big = small.clone();
    big["input"].as_array_mut().unwrap().push(serde_json::json!(
        {"type": "function_call_output", "call_id": "call_1", "output": log_payload(400)}
    ));
    harness.post("/v1/responses", &big).await;

    let sent = harness.upstream_raw_bodies().await;
    assert_eq!(sent.len(), 2, "both turns must have reached the upstream");
    let (first, second) = (&sent[0], &sent[1]);

    let client_first = serde_json::to_vec(&small).unwrap();
    let client_second = serde_json::to_vec(&big).unwrap();

    println!(
        "\n  turn 1: client sent {} bytes, upstream saw {} -- {}",
        client_first.len(),
        first.len(),
        if *first == client_first {
            "BYTE-IDENTICAL (pipe)"
        } else {
            "REWRITTEN"
        }
    );
    println!(
        "  turn 2: client sent {} bytes, upstream saw {} -- {}",
        client_second.len(),
        second.len(),
        if *second == client_second {
            "byte-identical"
        } else {
            "rewritten"
        }
    );

    // The conversation only. `tools` is serialized after `input` on this family,
    // so a growing input pushes the tools block to a later offset -- that is the
    // array growing, not a cached byte being rewritten, and counting it as
    // divergence reports every turn as broken.
    let conversation = |b: &[u8]| {
        let text = String::from_utf8_lossy(b).into_owned();
        match text.find("\"tools\"") {
            Some(at) => b[..at].to_vec(),
            None => b.to_vec(),
        }
    };
    let (conv_first, conv_second) = (conversation(first), conversation(second));
    let n = shared(&conv_first, &conv_second);
    println!("\n  the two prompts the provider saw share their first {n} bytes");
    println!(
        "  of turn 1's {} conversation bytes, that is {:.1}%",
        conv_first.len(),
        100.0 * n as f64 / conv_first.len() as f64
    );
    println!(
        "\n  turn 1 at the divergence:\n    ...{}...",
        around(first, n, 90)
    );
    println!(
        "\n  turn 2 at the divergence:\n    ...{}...",
        around(second, n, 90)
    );

    let tools_at = |b: &[u8]| String::from_utf8_lossy(b).find("\"tools\"");
    println!(
        "\n  \"tools\" key: turn 1 at {:?}, turn 2 at {:?}",
        tools_at(first),
        tools_at(second)
    );
    println!("  divergence at {n}");

    // The two findings this pins, both negative, both load-bearing for that item.
    //
    // Compared with our own tool removed. This used to assert plain byte
    // identity, from when the tool was offered only once a handle appeared —
    // the very behaviour that turned out to be moving the prefix. The point of
    // the assertion is unchanged: whatever the proxy sends under the gate, it
    // must not be a re-serialization of the client's own bytes, or the
    // comparison below is measuring us rather than the provider.
    assert_eq!(
        without_our_tool(&String::from_utf8_lossy(first)),
        String::from_utf8_lossy(&client_first),
        "under the gate the proxy must still be a pipe apart from the tools block"
    );
    assert!(
        n >= conv_first.len() - 4,
        "the conversation bytes the provider cached on turn one survived into \
         turn two: re-serialization is not what moves the prefix"
    );
    assert!(
        tools_at(second).is_some_and(|at| at > n),
        "on this family `tools` is serialized after `input`, so injecting the \
         retrieve tool cannot move a conversation byte -- if this fails, the \
         key order changed and the reasoning in that item needs redoing"
    );
}

/// Does anything make the proxy rewrite a message it has already sent?
///
/// The first experiment showed the shared bytes surviving intact, so the break
/// measured in the field is not re-serialization and not the tools block. This
/// one hunts for the shape that does move them: a message sent inline on one
/// turn and elided on the next.
#[tokio::test]
async fn which_shapes_rewrite_a_message_that_was_already_sent() {
    for (name, first_payload, second_payload) in [
        // A payload that compresses on its own, sent when the request is small
        // enough to be under the proxy's token gate.
        (
            "under the gate, then over it",
            log_payload(60),
            log_payload(400),
        ),
        // A payload too small to compress, repeated. `cross_turn` matches only
        // strictly earlier messages, so this should rewrite the later copy.
        (
            "a repeat the cross-turn pass can fold",
            log_payload(8),
            log_payload(8),
        ),
        // A payload that compresses on its own from the very first turn.
        (
            "compressible from turn one",
            log_payload(400),
            log_payload(400),
        ),
    ] {
        let harness = Harness::new().await;
        Mock::given(any())
            .respond_with(answered())
            .mount(&harness.server)
            .await;

        let first = serde_json::json!({
            "model": "gpt-5",
            "instructions": "You are a careful assistant.",
            "input": [
                {"type": "function_call_output", "call_id": "call_0", "output": first_payload}
            ]
        });
        harness.post("/v1/responses", &first).await;

        let mut second = first.clone();
        second["input"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!(
                {"type": "function_call_output", "call_id": "call_1", "output": second_payload}
            ));
        harness.post("/v1/responses", &second).await;

        let sent = harness.upstream_raw_bodies().await;
        let (a, b) = (&sent[0], &sent[1]);
        let n = shared(a, b);
        // Compare the conversation only. `tools` is serialized *after* `input`
        // on this family, so appending an input item shifts the tools block to
        // a later offset -- that is the array growing, not a cached byte being
        // rewritten, and counting it as a break reports every turn as broken.
        let conversation = |body: &[u8]| {
            let text = String::from_utf8_lossy(body).into_owned();
            text.find(r#""tools""#).unwrap_or(text.len())
        };
        let intact = n >= conversation(a).saturating_sub(4);
        println!(
            "\n  {name}\n    turn 1 {:>6} bytes ({} of conversation), turn 2 {:>6}, shared {:>6}  -> {}",
            a.len(), conversation(a), b.len(), n,
            if intact { "prefix intact" } else { "PREFIX MOVED" }
        );
        assert!(
            intact,
            "{name}: a byte the provider had already cached was rewritten\n  \
             turn 1: ...{}...\n  turn 2: ...{}...",
            around(a, n, 70),
            around(b, n, 70)
        );
    }
}

/// Does the warm-to-cold flip rewrite content the provider already cached?
///
/// The fourth suspect, and the last one that was still standing on paper.
/// `handler.rs` lifts `Protection::WhileCached` when the prefix tracker reports
/// `PrefixCache::Cold`, which is a mechanism for the same message being
/// protected on one turn and compressible on the next — exactly the shape the
/// field losses have, and exactly what the three earlier cases here could not
/// reproduce because none of them flipped that state.
///
/// It cannot be the cause on this family, and this pins why rather than leaving
/// it to be re-proposed: **`Protection::WhileCached` is set only by the
/// Anthropic walker**. No OpenAI slot ever carries it, so the `Cold` branch has
/// nothing to lift, which is also why every Harbor run reports `0 inside the
/// frozen prefix`. The flip is driven here for real — turn one served from
/// cache, turn two reporting a zero read, which is what makes the tracker say
/// `Cold` — and the bytes are compared across it.
#[tokio::test]
async fn the_warm_to_cold_flip_does_not_rewrite_what_was_already_sent() {
    use wiremock::matchers::method;

    let harness = Harness::new().await;
    let usage = |read: u32| {
        ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "id": "resp_1",
            "object": "response",
            "output": [{"type": "message", "role": "assistant",
                        "content": [{"type": "output_text", "text": "ok"}]}],
            "usage": {"input_tokens": 1000, "output_tokens": 2,
                      "input_tokens_details": {"cached_tokens": read}}
        }))
    };
    // Warm first, so the tracker records `was_warm`; then a zero read, which is
    // the only thing it accepts as evidence of a lapse.
    Mock::given(method("POST"))
        .respond_with(usage(41_116))
        .up_to_n_times(1)
        .mount(&harness.server)
        .await;
    Mock::given(method("POST"))
        .respond_with(usage(0))
        .mount(&harness.server)
        .await;

    let turn = |n: usize| {
        let mut input = vec![serde_json::json!(
            {"type": "function_call_output", "call_id": "call_0", "output": log_payload(400)}
        )];
        for extra in 1..=n {
            input.push(serde_json::json!({
                "type": "function_call_output",
                "call_id": format!("call_{extra}"),
                "output": log_payload(120)
            }));
        }
        serde_json::json!({
            "model": "gpt-5",
            "instructions": "You are a careful assistant.",
            "input": input
        })
    };

    // Three turns: warm, then the zero read lands, then the turn that runs with
    // the tracker saying `Cold`.
    for n in 0..3 {
        harness.post("/v1/responses", &turn(n)).await;
    }

    // The precondition, asserted rather than assumed. Without it this test
    // passes whether or not the tracker ever went cold, which would make its
    // result worth nothing: `PrefixCache::state` answers `Cold` only for a
    // prefix that was once served from cache and then reported a read of
    // exactly zero, so both numbers have to have reached the proxy.
    let reads: Vec<Option<u32>> = harness
        .events
        .drain()
        .into_iter()
        .filter_map(|event| match event {
            kompressor_stats::events::Event::ProxyRequest(request) => {
                Some(request.cache_read_tokens)
            }
            _ => None,
        })
        .collect();
    assert!(
        reads.first() == Some(&Some(41_116)) && reads.get(1) == Some(&Some(0)),
        "the warm-then-lapsed sequence never reached the tracker, so nothing \
         flipped and this test proves nothing: {reads:?}"
    );

    let sent = harness.upstream_raw_bodies().await;
    assert_eq!(
        sent.len(),
        3,
        "all three turns must have reached the upstream"
    );

    let conversation = |body: &[u8]| {
        let text = String::from_utf8_lossy(body).into_owned();
        text.find(r#""tools""#).unwrap_or(text.len())
    };
    // Turn three is the one that ran cold. If the flip rewrote anything the
    // provider had already been sent, its shared prefix with turn two stops
    // short of turn two's own conversation.
    let n = shared(&sent[1], &sent[2]);
    assert!(
        n >= conversation(&sent[1]).saturating_sub(4),
        "the warm-to-cold flip rewrote a byte that had already gone upstream:\n  \
         turn 2: ...{}...\n  turn 3: ...{}...",
        around(&sent[1], n, 70),
        around(&sent[2], n, 70)
    );
}
