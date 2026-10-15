-- Manual queries for a kompressor stats store (schema v5).
-- Run it from the directory holding the databases, so no path here is true on
-- only one machine:
--
--   cd ~/.local/share/kompressor        # or /var/lib/kompressor, or a run dump
--   sqlite3 -header -column stats.db < <repo>/scripts/db/queries.sql
-- Per-run stores live beside each dump; scripts/db/merge.py combines them; scripts/db/refs.sql reads the fragment store.

.print === sessions in this store ===
SELECT api_family, model, COUNT(*) req,
       datetime(MIN(time_ms)/1000,'unixepoch','localtime') first,
       datetime(MAX(time_ms)/1000,'unixepoch','localtime') last
FROM proxy_requests GROUP BY 1,2 ORDER BY 3 DESC;

.print
.print === compression, discarded rewrites excluded (they saved nothing) ===
SELECT model, COUNT(*) n, SUM(tokens_in) tin, SUM(tokens_out) tout,
       ROUND(100.0*(SUM(tokens_in)-SUM(tokens_out))/NULLIF(SUM(tokens_in),0),1) removed_pct,
       SUM(refs_emitted) handles, ROUND(AVG(engine_ms),1) engine_ms
FROM optimization_events WHERE outcome='compressed' GROUP BY 1;

.print
.print === why the engine declined things (the V5 skip attribution) ===
SELECT * FROM v_skip_reasons ORDER BY tokens DESC;

.print
.print === per conversation: cache behaviour. EXCESS WRITES = write - final prefix.
.print === Anthropic only -- OpenAI reports cache_write as NULL, so it reads as 0.
SELECT conversation, turns, cache_write, cache_read, completion FROM v_conversations;

.print
.print === excess writes per conversation (run against an ANTHROPIC store) ===
WITH last AS (
  SELECT conversation,
         COALESCE(cache_read_tokens,0)+COALESCE(cache_write_tokens,0) AS final_prefix,
         ROW_NUMBER() OVER (PARTITION BY conversation ORDER BY time_ms DESC) rn
  FROM proxy_requests WHERE conversation IS NOT NULL)
SELECT p.conversation, COUNT(*) turns,
       SUM(COALESCE(p.cache_write_tokens,0)) written, l.final_prefix,
       SUM(COALESCE(p.cache_write_tokens,0)) - l.final_prefix AS excess_writes
FROM proxy_requests p JOIN last l
  ON l.conversation=p.conversation AND l.rn=1
GROUP BY 1;

.print
.print === the daily economy that survives retention ===
-- Raw rows are pruned at the retention horizon; these archives are what is left
-- after that, so a window older than the horizon has to be read from here.
-- Per-turn ordering does not survive, so excess writes cannot be computed from
-- this -- only the daily totals.
SELECT day, tenant, requests, prompt_tokens, cache_read_tokens,
       cache_write_tokens, completion_tokens, conversations
FROM v_daily_requests ORDER BY day DESC LIMIT 14;

.print
.print === retrieval: how often the model actually asks for a fragment back ===
SELECT (SELECT SUM(refs_emitted) FROM optimization_events) handles_minted,
       COUNT(*) retrievals,
       SUM(found) hits, SUM(ranged) narrowed, SUM(returned_tokens) put_back,
       ROUND(1.0*(SELECT SUM(refs_emitted) FROM optimization_events)/NULLIF(COUNT(*),0),0)
         AS handles_per_retrieval
FROM retrieval_events;

.print
.print === what kinds of content the engine found ===
SELECT * FROM v_type_coverage;

.print
.print === request economy and outcomes ===
SELECT * FROM v_outcomes;
