-- Manual queries for a kompressor FRAGMENT store (refs.db).
--
-- Run it from the directory that holds the databases, so nothing here needs a
-- path that is true on only one machine:
--
--   cd ~/.local/share/kompressor          # or /var/lib/kompressor, or a run dump
--   sqlite3 -header -column refs.db < <repo>/scripts/db/refs.sql
--
-- The companion of scripts/db/queries.sql, which reads the STATS store. They
-- are different files written side by side: refs.db holds the content behind
-- `ref:k_…` handles, stats.db holds one row per request. `kompressor wrap`
-- writes a pair beside every run's dump, so the global pair only holds what ran
-- under the global config — which is why the working directory, rather than a
-- fixed path, is what selects which deployment you are looking at.
--
-- Timestamps are epoch SECONDS here, not the milliseconds stats.db uses.
--
-- Fragments are the payloads an agent actually sent — source, logs, command
-- output. Every query below prints a short preview at most, never a whole
-- fragment, so that reading the store is not the same as dumping someone's
-- repository to a terminal. `SELECT fragment` is always available when you
-- genuinely want one; it is not the default here on purpose.

.print === store summary ===
SELECT COUNT(*)                                            refs,
       COUNT(DISTINCT tenant)                              tenants,
       printf('%.1f MB', SUM(bytes)/1048576.0)             total,
       printf('%.1f KB', AVG(bytes)/1024.0)                avg,
       printf('%.1f KB', MAX(bytes)/1024.0)                largest,
       datetime(MIN(created_at),'unixepoch','localtime')   first,
       datetime(MAX(created_at),'unixepoch','localtime')   last
FROM refs;

.print
.print === liveness, using the store's own purge predicate ===
-- `CAST(... AS INTEGER)` is load-bearing: `strftime('%s','now')` returns TEXT,
-- and SQLite sorts every integer below every string, so the un-cast comparison
-- is true for every row and reports a fresh store as entirely expired.
--
-- Reproduced exactly rather than approximated: a row is dead when it is
-- STRICTLY past `last_accessed + ttl_seconds` or `created_at + max_lifetime`,
-- whichever comes first. A fragment written with a zero TTL is still
-- retrievable within the same second it was written, which an `<=` here would
-- report as already gone.
SELECT CASE WHEN last_accessed + ttl_seconds < CAST(strftime('%s','now') AS INTEGER)
              OR created_at   + max_lifetime < CAST(strftime('%s','now') AS INTEGER)
            THEN 'expired (next sweep removes it)'
            ELSE 'live' END                        state,
       COUNT(*)                                    refs,
       printf('%.1f MB', SUM(bytes)/1048576.0)     bytes
FROM refs GROUP BY 1 ORDER BY 2 DESC;

.print
.print === what expires next (live rows, soonest first) ===
SELECT ref,
       printf('%.1f KB', bytes/1024.0)                                    size,
       datetime(MIN(last_accessed + ttl_seconds, created_at + max_lifetime),
                'unixepoch','localtime')                                  expires,
       printf('%.1f h', (MIN(last_accessed + ttl_seconds, created_at + max_lifetime)
                         - CAST(strftime('%s','now') AS INTEGER)) / 3600.0)                "in"
FROM refs
WHERE last_accessed + ttl_seconds >= CAST(strftime('%s','now') AS INTEGER)
  AND created_at   + max_lifetime >= CAST(strftime('%s','now') AS INTEGER)
ORDER BY MIN(last_accessed + ttl_seconds, created_at + max_lifetime)
LIMIT 15;

.print
.print === by kind ===
-- `kind` is the STORAGE kind, not the detected content type: the engine records
-- what it put in, and on current builds that is `text` for everything. To see
-- content types, read `optimization_events.content_types` in stats.db.
SELECT kind,
       COUNT(*)                                 refs,
       printf('%.1f MB', SUM(bytes)/1048576.0)  total,
       printf('%.1f KB', AVG(bytes)/1024.0)     avg
FROM refs GROUP BY 1 ORDER BY 2 DESC;

.print
.print === size distribution ===
-- Where the bytes are, which is rarely where the rows are: elision is worth
-- most on a handful of very large payloads.
SELECT CASE WHEN bytes <     2048 THEN 'a <2 KB'
            WHEN bytes <     8192 THEN 'b 2-8 KB'
            WHEN bytes <    32768 THEN 'c 8-32 KB'
            WHEN bytes <   131072 THEN 'd 32-128 KB'
            ELSE                       'e >=128 KB' END     band,
       COUNT(*)                                             refs,
       printf('%.1f MB', SUM(bytes)/1048576.0)              total,
       printf('%.0f%%', 100.0*SUM(bytes)/(SELECT SUM(bytes) FROM refs)) share
FROM refs GROUP BY 1 ORDER BY 1;

.print
.print === largest fragments, with a one-line preview ===
SELECT ref,
       printf('%.1f KB', bytes/1024.0)                       size,
       datetime(created_at,'unixepoch','localtime')           created,
       substr(replace(replace(replace(CAST(fragment AS TEXT),
              char(10),' '), char(13),' '), char(9),' '), 1, 64) preview
FROM refs ORDER BY bytes DESC LIMIT 15;

.print
.print === busiest requests ===
-- One request can mint many handles. A request_id here joins to
-- proxy_requests.request_id and optimization_events.request_id in stats.db.
SELECT COALESCE(request_id,'(none recorded)')  request_id,
       COUNT(*)                                refs,
       printf('%.1f MB', SUM(bytes)/1048576.0) bytes,
       datetime(MIN(created_at),'unixepoch','localtime') first_seen
FROM refs GROUP BY 1 ORDER BY 2 DESC LIMIT 15;

.print
.print === touched after being stored ===
-- NOT the same as "retrieved". `last_accessed` moves on a retrieve AND on a
-- re-PUT: handles are content-addressed, so the same payload elided again on a
-- later turn upserts the same row and bumps the column. Observed on a live
-- store, three rows with deltas of 10-21 seconds — the agent re-sending its
-- history, not the model fetching anything back.
--
-- For actual retrievals read `retrieval_events` in stats.db, which records one
-- row per fetch. Across 84 measured runs that was 273 fetches against 97,400
-- handles, one per 356.
SELECT CASE WHEN last_accessed > created_at THEN 'touched again (re-stored or fetched)'
            ELSE 'stored once, never touched' END   state,
       COUNT(*)                                     refs,
       printf('%.0f%%', 100.0*COUNT(*)/(SELECT COUNT(*) FROM refs)) share
FROM refs GROUP BY 1 ORDER BY 2 DESC;

-- === looking one fragment up, or joining to the stats store ===
--
-- All of these assume the working directory holds both databases, as above.
--
-- The whole content of one handle (the deliberate opt-in):
--   sqlite3 refs.db "SELECT CAST(fragment AS TEXT) FROM refs WHERE ref='k_XXXXXXXXXXXX';"
--
-- Handles minted by a request, joined to what that request cost. The two
-- stores are separate files, so this needs an ATTACH:
--   sqlite3 refs.db <<'SQL'
--   ATTACH 'file:stats.db?mode=ro' AS s;
--   SELECT r.request_id, COUNT(*) refs, SUM(r.bytes) bytes,
--          p.model, p.prompt_tokens, p.cache_read_tokens
--   FROM refs r JOIN s.proxy_requests p ON p.request_id = r.request_id
--   GROUP BY 1 ORDER BY 2 DESC LIMIT 20;
--   SQL
--
-- Handles that were retrieved, by joining the retrieval side:
--   ATTACH 'file:stats.db?mode=ro' AS s;
--   SELECT r.ref, r.bytes, e.found, e.returned_tokens
--   FROM refs r JOIN s.retrieval_events e ON e.ref = r.ref;
--
-- `file:…?mode=ro` rather than a bare `stats.db`, for two reasons. A relative
-- ATTACH resolves against the PROCESS's working directory, never the main
-- database's — so running this from elsewhere does not reach the stats.db
-- beside refs.db. And a plain ATTACH of a missing file does not fail: it
-- CREATES an empty database there and every join then returns nothing, which
-- reads like "this run retrieved nothing" rather than like a mistake. Read-only
-- cannot create, so the wrong directory is an error instead of a wrong answer.
