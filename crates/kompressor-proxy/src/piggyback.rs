//! Handing the model back what the relay elided, on a transport that has no
//! tool call to ask with.
//!
//! Over HTTP the retrieve loop is a conversation: the proxy offers a tool, the
//! model calls it, the proxy answers and re-submits the round. None of that
//! exists on the codex websocket. Tools are declared once, in the first
//! `response.create`, as a tree of namespaces whose only executable member is
//! `exec` — a JavaScript isolate that runs *inside the client*. A tool this
//! proxy invented would be dispatched there, against a handler that does not
//! exist, and the call would never reach the wire. There is no round to run
//! either: the chain is stitched with `previous_response_id`, so a response the
//! proxy created on its own would be one the client never names again.
//!
//! What the relay does have is every `response.create` the client sends, and
//! those accept more than one input item. So retrieval here is a piggyback: the
//! relay remembers what it elided, watches the model's own output for a handle
//! coming back, and appends the original to the client's *next* frame. No tool,
//! no extra round, no rewritten ids.
//!
//! The asymmetry this leaves is real and worth naming: the fragment arrives as
//! new input rather than as a repair to the turn it was taken from. The elided
//! marker stays elided in the server's stored prefix, which this proxy cannot
//! address. The model gets the text; the history keeps the handle.

use kompressor_engine::refs::{RefId, refs_in};

/// How many handles one connection remembers eliding.
///
/// A bound, not a tuning knob: the map lives as long as the socket, and a long
/// session must not be able to grow it without limit.
const ELIDED_PER_CONNECTION: usize = 256;

/// How many fragments a single `response.create` may carry back.
///
/// The same ceiling the HTTP side puts on pre-emptive inlining. More than a
/// handful in one frame stops being a retrieval and starts being a resend.
const SERVED_PER_ROUND: usize = 4;

/// How much of the model's streamed text to keep between frames.
///
/// A handle is 18 characters and the upstream splits its output into deltas at
/// arbitrary points, so one can straddle two frames. Keeping the last stretch
/// of text means the second half arrives against a window that still holds the
/// first.
const TAIL_KEEP: usize = 64;

/// What one relayed connection elided, and what the model has asked back.
#[derive(Default)]
pub(crate) struct Piggyback {
    /// Handles this connection has put in front of the model.
    elided: Vec<RefId>,
    /// Handles already sent back, so a model that keeps repeating one is
    /// answered once.
    served: Vec<RefId>,
    /// Spotted in the model's output, due on the next frame going upstream.
    pending: Vec<RefId>,
    /// A rolling window over the model's streamed text.
    tail: String,
    /// Tokens handed back on this connection so far, against the budget the
    /// operator set for a request.
    spent: u32,
}

impl Piggyback {
    /// Remember handles the compressor just put into an outgoing frame.
    ///
    /// **Oldest out, not newest refused.** The bound is right; stopping at it
    /// was backwards. A connection that had elided 256 distinct handles
    /// remembered none of the ones it elided afterwards, so the set retained
    /// was the oldest — and the handles a model is about to quote are the ones
    /// it has just been given. This transport has no tool to fall back on, so a
    /// handle the relay minted and then forgot is content the model cannot
    /// recover by any route. `prefixcache` evicts oldest-first next door for
    /// exactly this reason.
    pub(crate) fn note_elided(&mut self, ids: impl IntoIterator<Item = RefId>) {
        for id in ids {
            if self.elided.contains(&id) {
                continue;
            }
            if self.elided.len() >= ELIDED_PER_CONNECTION {
                self.elided.remove(0);
            }
            self.elided.push(id);
        }
    }

    /// Feed a piece of the model's output and queue any handle it names.
    ///
    /// Only handles this connection actually elided count. The model quoting
    /// something that looks like a handle but was never ours is not a request
    /// for anything we hold.
    pub(crate) fn saw_output(&mut self, text: &str) {
        if self.elided.is_empty() {
            return;
        }
        self.tail.push_str(text);
        for id in refs_in(&self.tail) {
            if self.pending.len() >= SERVED_PER_ROUND {
                break;
            }
            if !self.elided.contains(&id) {
                continue;
            }
            if self.served.contains(&id) || self.pending.contains(&id) {
                continue;
            }
            self.pending.push(id);
        }
        if self.tail.len() > TAIL_KEEP {
            // Char boundaries matter: the deltas are UTF-8 and the cut lands
            // wherever the byte arithmetic puts it.
            let want = self.tail.len() - TAIL_KEEP;
            let cut = (want..=self.tail.len())
                .find(|at| self.tail.is_char_boundary(*at))
                .unwrap_or(self.tail.len());
            self.tail.drain(..cut);
        }
    }

    /// Queue a handle the caller decided the model wants for some other reason
    /// — a new query that reads like it is about the fragment behind it.
    pub(crate) fn also_wants(&mut self, id: RefId) {
        if self.pending.len() >= SERVED_PER_ROUND
            || !self.elided.contains(&id)
            || self.served.contains(&id)
            || self.pending.contains(&id)
        {
            return;
        }
        self.pending.push(id);
    }

    /// Handles elided on this connection that have not been sent back.
    pub(crate) fn outstanding(&self) -> Vec<RefId> {
        self.elided
            .iter()
            .filter(|id| !self.served.contains(id) && !self.pending.contains(id))
            .copied()
            .collect()
    }

    /// Take what is due, marking it served.
    ///
    /// Served on being taken rather than on being delivered: a fragment the
    /// relay failed to send is not one to try again on every later frame, and
    /// repeating it is the more expensive mistake.
    pub(crate) fn take_pending(&mut self) -> Vec<RefId> {
        let taken = std::mem::take(&mut self.pending);
        for id in &taken {
            if self.served.contains(id) {
                continue;
            }
            // Oldest out, the same way `elided` evicts. This list used to be
            // bounded only as a side effect: nothing could be served that had
            // not been remembered, and `note_elided` refused new handles once
            // full. Making that one evict instead removed the only ceiling
            // `served` had, and it grows for the life of a connection.
            //
            // Evicting the oldest is the right end to lose. `served` exists to
            // stop the compressor eliding something already handed back, and
            // the handles at risk of being elided again are the ones in the
            // live tail — not the ones restored hundreds of frames ago, which
            // the conversation has usually moved past.
            if self.served.len() >= ELIDED_PER_CONNECTION {
                self.served.remove(0);
            }
            self.served.push(*id);
        }
        taken
    }

    /// Everything sent back so far, which the compressor must not elide again.
    pub(crate) fn served(&self) -> Vec<RefId> {
        self.served.clone()
    }

    /// Whether `tokens` more may be handed back under `budget`.
    ///
    /// `proxy.retrieve_max_tokens_per_request` bounds the HTTP path through
    /// `Budget`; the same number bounds this one. Per *connection* rather than
    /// per frame, because a connection is what a relayed session has instead of
    /// a request — the same reasoning that gives it one `proxy_requests` row.
    /// A budget of 0 is no budget, as on the HTTP side.
    ///
    /// Measured against the run that motivated it: of three relayed
    /// connections, one handed back 60,086 tokens against a default budget of
    /// 16,384 while the other two came in at 12,705 and 12,985. The cap binds
    /// where it should and leaves the ordinary case alone.
    pub(crate) fn may_spend(&self, tokens: u32, budget: u32) -> bool {
        budget == 0 || self.spent.saturating_add(tokens) <= budget
    }

    /// Record tokens actually handed back.
    pub(crate) fn spend(&mut self, tokens: u32) {
        self.spent = self.spent.saturating_add(tokens);
    }

    /// Tokens handed back on this connection.
    pub(crate) fn spent(&self) -> u32 {
        self.spent
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kompressor_engine::refs::ref_for;

    fn handle(text: &str) -> RefId {
        ref_for(text)
    }

    #[test]
    fn a_handle_the_model_echoes_is_queued_once_and_then_marked_served() {
        let id = handle("a fragment");
        let mut pig = Piggyback::default();
        pig.note_elided([id]);

        pig.saw_output(&format!("let me look at ref:{id} again"));
        assert_eq!(pig.take_pending(), vec![id]);

        // Said twice, answered once: the model repeating itself is not a second
        // request, and re-sending costs the tokens all over again.
        pig.saw_output(&format!("still ref:{id}"));
        assert!(pig.take_pending().is_empty());
        assert_eq!(pig.served(), vec![id]);
    }

    #[test]
    fn a_handle_split_across_two_deltas_is_still_found() {
        // The reason the tail exists. The upstream breaks output into deltas at
        // arbitrary points, and a handle cut in half by one is exactly the case
        // a per-frame scan misses.
        let id = handle("split across frames");
        let rendered = format!("ref:{id}");
        let (head, rest) = rendered.split_at(9);

        let mut pig = Piggyback::default();
        pig.note_elided([id]);
        pig.saw_output(head);
        assert!(
            pig.take_pending().is_empty(),
            "half a handle is not a handle"
        );
        pig.saw_output(rest);
        assert_eq!(pig.take_pending(), vec![id]);
    }

    #[test]
    fn a_handle_this_connection_never_elided_is_not_served() {
        // A model quoting a handle from somewhere else — an earlier session, a
        // file it read — is not asking this relay for anything.
        let mine = handle("mine");
        let theirs = handle("someone else's");
        let mut pig = Piggyback::default();
        pig.note_elided([mine]);
        pig.saw_output(&format!("ref:{theirs} and ref:{mine}"));
        assert_eq!(pig.take_pending(), vec![mine]);
    }

    #[test]
    fn no_more_than_the_per_round_ceiling_is_queued() {
        let ids: Vec<RefId> = (0..SERVED_PER_ROUND + 3)
            .map(|n| handle(&format!("fragment {n}")))
            .collect();
        let mut pig = Piggyback::default();
        pig.note_elided(ids.iter().copied());
        let echoed: String = ids.iter().map(|id| format!("ref:{id} ")).collect();
        pig.saw_output(&echoed);
        assert_eq!(pig.take_pending().len(), SERVED_PER_ROUND);
    }

    #[test]
    fn the_remembered_set_is_bounded() {
        let mut pig = Piggyback::default();
        pig.note_elided((0..ELIDED_PER_CONNECTION + 50).map(|n| handle(&format!("f{n}"))));
        assert_eq!(pig.outstanding().len(), ELIDED_PER_CONNECTION);
    }

    /// `served` is bounded too, and for the same reason.
    ///
    /// It used to be bounded only as a side effect of `note_elided` refusing
    /// new handles once full — nothing could be served that had not been
    /// remembered. Making that one evict instead removed the ceiling, and a
    /// long connection grew this list without limit.
    #[test]
    fn the_served_set_is_bounded_and_drops_the_oldest() {
        let mut pig = Piggyback::default();
        let ids: Vec<_> = (0..ELIDED_PER_CONNECTION + 40)
            .map(|n| handle(&format!("s{n}")))
            .collect();
        // Serve them a round at a time, which is the only way they are served.
        for chunk in ids.chunks(SERVED_PER_ROUND) {
            pig.note_elided(chunk.iter().copied());
            let echoed: String = chunk.iter().map(|id| format!("ref:{id} ")).collect();
            pig.saw_output(&echoed);
            pig.take_pending();
        }
        let served = pig.served();
        assert!(
            served.len() <= ELIDED_PER_CONNECTION,
            "served grew to {} against a bound of {ELIDED_PER_CONNECTION}",
            served.len()
        );
        assert!(
            served.contains(ids.last().unwrap()),
            "the most recently served handle must still be known"
        );
    }

    /// And the bound drops the oldest, not the newest.
    ///
    /// Refusing to remember anything once full keeps the handles least likely
    /// to be asked for and forgets the ones just handed to the model. There is
    /// no retrieve tool on this transport, so a forgotten handle is content
    /// that cannot be recovered at all.
    #[test]
    fn a_full_set_forgets_the_oldest_handle_not_the_newest() {
        let mut pig = Piggyback::default();
        let ids: Vec<_> = (0..ELIDED_PER_CONNECTION + 50)
            .map(|n| handle(&format!("f{n}")))
            .collect();
        pig.note_elided(ids.iter().copied());

        let held = pig.outstanding();
        assert_eq!(held.len(), ELIDED_PER_CONNECTION);
        assert!(
            held.contains(ids.last().unwrap()),
            "the most recently elided handle must still be known"
        );
        assert!(
            !held.contains(&ids[0]),
            "and the oldest is the one that gave way"
        );
    }

    #[test]
    fn outstanding_drops_what_is_pending_or_served() {
        let a = handle("a");
        let b = handle("b");
        let mut pig = Piggyback::default();
        pig.note_elided([a, b]);
        pig.also_wants(a);
        assert_eq!(pig.outstanding(), vec![b], "a is already queued");
        pig.take_pending();
        assert_eq!(pig.outstanding(), vec![b], "and now it is served");
    }
}
