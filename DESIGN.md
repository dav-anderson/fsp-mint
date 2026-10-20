# fsp-mint: design notes

How fsp-mint is built, and why. The README has the build, the settings, the commands and the HTTP API; this file has the reasoning. The sections were moved here from the README as they were, except where noted in [CHANGELOG.md](CHANGELOG.md).

## Design

How fsp-mint is built, and why. Each item says what the design is and gives the reason.

1. **The service operator runs fsp-mint and holds the service's DID key.** fsp-mint does the ESP transfer at the Issuer (esp-mint) itself, signed by the service, and keeps the ESP it receives to redeem later. The key is `service_key.hex` in the network's data folder (owner-only permissions), made by `fsp-mint service-key`. *Why:* the ESP must be moved to notes only the service controls before FSP is issued against it, and the mint that issues FSP is the only party that can do both steps under one stored request; a separate wallet step would leave a gap where FSP is issued against ESP the app could still spend.
2. **The app sends one request: `POST /issue` with its handed-over ESP notes (each with its spend secret) and its blinded FSP outputs.** The request is in groups, one per redeemable-after day. A failure between the ESP transfer and the FSP issuance is handled by a stored pending entry (below, "Crash ordering").
3. **Answers are stored by request hash**, as esp-mint does for exchange and transfer. A retry of the same request gets the same answer, or finishes the pending one. *Why:* a lost reply must never issue twice or not at all.
4. **FSP ignores ESP maturity.** FSP is spendable at once, and one issue may mix ESP of several days. *Why:* the day only matters when the service redeems the ESP with the Issuer; for the app, a point at the service is a point at the service. The service carries the wait until the ESP matures (`ecash-wallet esp-schedule` shows when).
5. **FSP is 1:1 with ESP units** (10^-12 USD per unit; amounts are `u64`, sums are checked), **in power-of-two denominations**, like PSP and ESP.
6. **Only ESP named for the service's own DID is accepted.** The DID is `service.did` in the config (the 33-byte compressed public key, 66 hex characters, printed by `service-key`; at startup the whole DID is compared with the key in `service_key.hex`, the parity byte too, and a mismatch stops the mint); a request for another service is refused (`wrong_service`) before anything is sent, and the Issuer refuses any note that is not for that service or day.
7. **A spend is a swap with change:** FSP notes in, change outputs out, and an amount paid to the service. Inputs = outputs + pay, exactly.
8. **The ESP to FSP fee is a setting, `fees.issue_fee_per_byte`, default 0**, charged per byte of the issue group (as esp-mint charges per byte). Each group must be exact: ESP = FSP outputs + fee. The body ceiling (`server.max_request_bytes`) and the order of checks are the same as in the other mints: the size first (before parsing), then the checks that need nothing but the request, then the stored answer, then the state.

## Crash ordering (ESP to FSP)

One issue spans two mints run by different operators, so it runs as a stored entry that any retry finishes:

1. Checks that touch nothing: shape, denominations, the service, each spend secret against its note, each group exact.
2. A stored answer for this request hash is returned as it is; a pending entry is picked up where it stopped.
3. No ESP note or FSP output of the request may be held by another pending issue, and no ESP note may have been kept before (reserved inputs and outputs).
4. One ESP transfer per group is built (the app's notes in, fresh ESP notes of the same sizes out, signed by each note's secret and by the service's DID key). **The pending entry, with the transfers and the new notes' secrets, is written to `issues.json` before anything is sent.**
5. Each transfer is sent. Done: the new ESP notes are unblinded and kept. Refused (`400`, nothing burned at the Issuer): that group gets no FSP. Unknown (no reply over AIR after the tries, "cannot answer now", `409`, `5xx`): the request answers `502` and stays pending; a retry sends the same transfer, which the Issuer answers from its own store. See "The Issuer over AIR".
6. When every group is settled, the FSP outputs of the transferred groups are signed (deterministic, so a crash here just signs again), the answer is stored, and returned.

*Why this order:* writing the pending entry first means a crash at any point leaves either nothing sent, or a transfer the Issuer will answer the same way again. The ESP can be transferred only once at the Issuer and the FSP answer only once here, so nothing is issued twice and nothing is lost.

## The Issuer over AIR

fsp-mint reaches the ESP mint (the Issuer) over AIR and no other way. There is no HTTP path to it and no switch: `esp_air::AirIssuer` is the only `Issuer`, and `serve` refuses to start without `air.esp_did`, `air.server` and `air.host`. *Why:* the FSP mint runs on the AIR service's machine and the ESP mint on the SPI's, so every call between them is party to party, and AIR is how those calls are made (signed replies, no open port on the ESP mint). A fallback to HTTP would keep that port open and would have to be kept in step.

Three calls, each a request sealed to the ESP mint's DID and answered by its own AIR listener (`sp_wire::air_request::ask`, the flow the wallet uses):

- **Mint info** at `serve`: the network the ESP mint reports is compared with `server.network`, and a mismatch stops the mint ("Refusing to start"), as it did over HTTP. A mint that cannot be asked also stops it.
- **Keysets**: the current keyset, which new transfers name for their outputs (cached for `esp.keyset_ttl_secs`).
- **Transfer**: one per group of an issue.

*How the replies read.* The reply is checked against the ESP mint's DID (signature, request id and hash, age). Answered: `Done`, the body is the transfer response. A signed refusal whose body is the `400` body (no `http_status`, or `400`): `Refused` with its code and text, so that group gets no FSP. Any other refusal (`409` and the other 4xx the reply numbers), "cannot answer now" (the `5xx` case), a reply that fails a check, and no reply after `air.attempts` tries of `air.reply_wait_secs`: `Unknown`. The issue answers `502` and stays pending.

*The request in flight is kept with the pending issue.* The pending group in `issues.json` has an `air` field: the request id, the one-off key `r` the reply is sealed to, the mailbox slots written so far, when it was made, the ESP mint's DID and the SHA-256 of the transfer JSON. It is saved before the request is first written and after each write, by the same file write that holds the pending entry, so a crash after a write still leaves the pair that can read the reply. A retry (the same issue request sent again, or after a restart) hands the Issuer that request. It is resumed only if it was made for the configured `air.esp_did` and for this very transfer (the hash); otherwise it is dropped and a new one is made. The reply cell is read first: if the ESP mint answered after the last try, the answer is there, no request is written, and the group finishes. If not, the same request (same id and key, new seal) is written to the next free slot. The `air` field is cleared as soon as the request ends: answered, refused, or a reply that failed a check.

*A request that ended badly is not reopened.* A reply cell is write-once, so a reply that fails a check, says "cannot answer now", or is a `409` or `5xx` can never be replaced. The request is dropped and the outcome is `Unknown`; the retry is a new request with the same body, and the ESP mint answers a repeated transfer from its stored answer, so the transfer is applied once. A kept request whose reply is found but is older than `air.max_skew_secs` is dropped the same way, and the transfer is sent once more in a new request.

*Old files.* `air` is optional in `issues.json` (`serde` default, and not written when there is none). A file written by 0.5.0 or before has no such field and reads as "no request in flight": the retry makes a new request, which is safe for the reason above. (Pending entries from 0.4.0 that hold the old x-only service still do not load, see the CHANGELOG.)

*Why `esp.keyset_ttl_secs` stays.* The keyset is still read once and cached, now with a keysets request; reading it on every issue would add a round trip to every issue.

## Spending (the fast path)

## Spending (the fast path)

`POST /spend` takes FSP inputs (each with its keyset ID, its proof and a spend signature), change outputs, the amount paid and a memo. Order of checks: shape and amounts (no lock), the stored answer, keysets and signatures (no lock), then the spent lists (under the lock), then sign and write. Inputs must be on the current or previous keyset; change is signed with the current one.

**ESP keysets on `POST /issue`.** Each handed ESP note names the ESP keyset that signed it (`keyset`, 16 hex; `400 missing_keyset` without it). The transfer to the Issuer names those keysets for its inputs and, for its outputs, the Issuer's current keyset when the issue is accepted (fixed then, so a retry sends the same transfer). That keyset is **cached**: read from esp-mint over AIR (a keysets request, see "The Issuer over AIR"), used for `esp.keyset_ttl_secs` (default 3600 s), then read again. The Issuer signs with any live keyset, so a cached keyset that has only become the previous one still works. If the Issuer answers a transfer `keyset_retired` (the cached keyset was deleted), the cache is dropped at once, the keyset read again, the group's ESP outputs re-blinded for it (nothing was burned) and the transfer sent once more. If a read fails, the cached keyset is used; only with none cached is the issue refused `502 issuer_unavailable` (nothing stored). The cache is in memory, so a restart reads it again.

**Request hashes cover every keyset field.** The issue hash (`fsp-mint/issue-request/v2`) includes each handed ESP note's `keyset` with its length; the spend hash (`fsp-mint/spend-request/v2`) adds every input keyset, length-prefixed, to the digest. Two requests that differ only in a keyset never share a hash. The kept ESP records the keyset the Issuer's reply names, and `export-esp` writes it into the hand-over so the service's wallet names it when it redeems. The FSP outputs are signed with the keyset chosen at acceptance, or the current one if that has since been deleted, and every reply, stored ones included, names the keyset that signed it.

**Spent lists and batching.** One spent list per live keyset, in memory, backed by an append-only log (`spends-<keyset>.jsonl`). Each spend is one line with the nonces it spent and its stored answer. A single writer thread takes every line waiting (up to `spend.max_batch`, default 1024), appends them, syncs once, and only then are those spends answered (group commit). There is no flush timer: a batch is whatever arrived during the last sync, so a lone spend waits for one sync and a busy mint shares each sync among many spends.

**Replay window: none.** No spend is answered before its line is on disk. A crash can only lose lines whose spends were never answered: their notes are unspent again after the restart and no signature for them left the mint, so the payer just retries. A torn last line is cut at startup. If a write fails, the mint stops taking spends (`/health` reports it) and keeps those notes marked spent until it restarts. *Why no window:* the cost of waiting for a sync is shared by the batch, so safety costs almost nothing in throughput.

**Throughput.** `throughput_of_the_spend_path` (run `cargo test --release throughput -- --nocapture`) sends 2000 spends (one note in, one change output) through the spend core with 8 worker threads, including proof and signature checks and the synced log. On the development Mac it measured about 62,000 spends a second (2026-10-15), well above the target of a thousand. It does not include HTTP or the network.

## Audit (outstanding FSP and the ESP reserve)

Two figures, both counted where the thing happens, so that a crash cannot leave a counter and its cause apart.

**Issued.** `issues.json` has `issued`, FSP units per key set, added to in the same `store::save` that stores the finished answer (`Entry::Done`). A retry gets the stored answer and counts nothing. `uncounted_requests` is the number of finished answers there were the first time 0.4.0 opened the file; they carry no amounts (the request is dropped once the answer is stored) and are reported, not guessed.

**Spent and change.** Each spend's log line has `a`: the units of each input (same order as `n`) and of each output. Counting a line adds the inputs to `spent` under the key set of each note and the outputs to `change` under `r.keyset`, the key set that signed them. The counters are held in memory next to the spent lists and updated under the same lock, and rebuilt at every start from the logs, so they cannot disagree with the spent lists: a spend that was never answered is in neither. A line without `a` (0.3.0) counts as `uncounted_spends`.

**Why a ledger.** A key set's notes are spent in the log of whichever key set is current at the time, so the counters of key set X are spread over X's log and the logs after it. X's log is deleted with X. `ledger.json` therefore gets X's counters (inserted once, never changed) at the moment X is deleted, before the log goes. At start-up every log on disk is replayed, including those of key sets that are gone, and any gone key set not yet in the ledger is added from them: this repairs a crash between the key file moving on and the ledger being written, since the logs are removed only after the ledger is saved. Key sets already in the ledger are left alone, so a later log that mentions X does not count it again.

**Reading.** `GET /audit` takes the spend side first and then the issue lock, so a spend (which uses FSP already issued) can never show as more than was issued. The issue lock is held across a whole issue, including the calls to the Issuer, so the handler waits for it off the async threads. `fsp-mint audit` reads the same files without opening the mint: it never truncates a log (a half-written last line is ignored, not cut), takes the key sets as the key file lists them, and counts a deleted key set that is not yet in the ledger from its log. Rotation due since the mint last ran is applied only when the mint runs.

**ESP.** From `kept` in `issues.json`: matured when `now / 86400 >= redeemable_after_day` (the `sp-wire` rule), else unmatured. Notes are not removed from `kept` when `export-esp` writes them out, so they stay in the figure and are shown separately as exported.

**Not done.** Dollar figures (the rate is not kept here; `spi` has it). No authentication on `/audit`, like the other two mints.

## Wire format and versions

The request and response bodies (`IssueRequest`, `SpendRequest` and the rest of `src/api.rs`), their hashes and digests, the size helper `issue_group_size` and the error codes are defined in the `sp-wire` crate, not here. `src/api.rs` re-exports them, so `fsp_mint::api::*` is unchanged. The transfer request this mint sends to the ESP mint is the `sp-wire` type too. The wallet and the other mints build against the same crate, so they cannot drift from this mint by a byte.

`GET /health` returns `wire_version`, the crate's `WIRE_VERSION`. **Changing any encoded byte or JSON field in `sp-wire` means bumping `WIRE_VERSION`**, and a wallet refuses a mint that reports another number. Golden-bytes and size tests in `sp-wire` fail if a byte moves.

## Key rotation

### Why a spent list, and how fast it grows

Notes are bearer notes with blind signatures: the mint signed each one without seeing it, so it cannot tell one of its notes from another except by the note's own nonce, and it cannot recognise a note it has already seen unless it keeps a record. Without that record the same note could be spent again and again. So every mint keeps a **spent list**: the nonce of every note it has taken in. There is no way around it for this kind of note; only the size can be managed.

Each entry is a note ID of about 32 bytes (the nonce is a 33-byte compressed key), plus the storage's own overhead. In fsp-mint the list is a hash set in memory (roughly 33 bytes of key plus some 50 to 70 bytes of hash-set overhead per note) and an append-only log on disk; each spend's log line also carries its stored answer (the signatures for the change), so on disk a spend costs about 250 bytes plus about 140 bytes per change output. psp-mint and esp-mint keep their lists in their sled database (mint-core's store). A list only grows: a million spent notes is roughly 100 MB of memory and a few hundred MB of log in fsp-mint.

### Why two key sets and two lists, and rotation

A spent list can only be cut down by cutting off the notes it protects. So the keys **rotate**: every period a new key set (keyset) is made. New notes are signed with it. Notes of the set before it stay good for one more period: they can still be spent, transferred, redeemed or swapped. At the next rotation the oldest set is deleted **together with its spent list**, because no note of that set can be presented any more, so nothing needs to be remembered about it. At any time there are at most two sets and two lists, **current** and **previous**. A spend checks the note's keyset ID against the matching list.

**Notes untouched through a full cycle are lost.** A note made just before a rotation is on the previous set at once and has one period left; a note made just after has up to two periods. A note nobody spends or swaps in that time cannot be used after its set is deleted. For FSP (period 8760 hours) that is one to two years; for PSP and ESP (period 87600 hours) about 10 to 20 years. The wallet prevents this by swapping on its own (below), so only a wallet left offline for the whole window loses notes.

### Retired keyset list

When a rotation deletes a keyset, its 8-byte ID is added to a retired list kept in `keysets.json` itself, so the one atomic save that drops the keyset's secret also lists its ID: a crash can never leave a deleted keyset unlisted. A spend naming a listed ID gets `keyset_retired` (the note expired); an ID neither live nor listed gets `unknown_keyset` (junk, or never this mint's). This lets the mint tell genuinely expired notes from junk, so a wallet can explain a loss.

The list is a cache capped at 1 MB of 8-byte IDs, **131,072 entries** (`RETIRED_KEYSETS_CAP`, the same constant in psp-mint and esp-mint); when full, the oldest ID is dropped. That covers hourly rotation for about 15 years, and a yearly mint never comes close. Dropping only turns a very old `keyset_retired` answer into `unknown_keyset`. It is deliberately not a setting, an exception to "nothing hard-coded": it is too cheap to be worth tuning. In the file each ID is 16 hex characters, so a full list is about 2.5 to 3.5 MB of JSON on disk (quotes, commas and indentation included).

### Why rotation starts late (activation after the first issue)

At genesis there is one key set and one spent list, and rotation does not start until `rotation.activation_hours` after the mint's **first issuance**. The time of the first issue is written once to `first_issue.json` in the network's data folder and never changed. A new mint has few notes and a small list, so there is nothing to gain from rotating early, and every rotation costs each holder a swap. Counting from the first issue rather than from a calendar date means a mint set up and left idle does not rotate before it has users. `rotation.activation_hours = 0` turns rotation off for good (one set, one list, for ever). In psp-mint `rotation.period_hours = 0` turns it off too; fsp-mint and esp-mint reject a period of 0, so there `activation_hours = 0` is the only off switch. The default is 8760 hours (1 year) for all three mints.

### How the wallet keeps notes alive (automatic, staggered rollover)

Every time ecash-wallet talks to a mint, and at startup for every mint it has used, it reads the mint's keys. If it holds notes on the previous set it swaps them for notes on the current set (a swap with no payment). It asks nothing. The swap is stored with its request hash before it is sent, like every other request, so a crash or a lost reply is finished by sending the same request again; a failure is logged and tried again on the next connection.

The swap is **staggered** so that all wallets do not swap in the same minute after a rotation: each wallet waits a delay of its own, a fixed fraction (from a hash of the wallet and the new keyset) of a window of **0.5% of the period, capped at 72 hours**. With the 8760-hour FSP period that is up to 43.8 hours; with the 87600-hour PSP/ESP period it is the 72-hour cap. The delay is tiny next to the period the notes stay good, so it never puts them at risk.

### No minimum period, and why

There is no policy minimum. The unit is whole hours and the period can be set as low as 1 hour (fsp-mint and esp-mint reject 0 for the period). psp-mint is the exception: it has a 24-hour floor, because a BTC claim needs time to settle before a rotation. There is no floor in the others because a floor would protect nothing: a service that wants to take its users' prepaid FSP can already do so by shutting its mint down. What holds a service back is its reputation, not a setting. A short period is a legitimate choice for a service with heavy traffic and little storage, as long as its users' wallets connect often enough.

### Tuning (hours)

| Setting | Unit | Default | Why change it |
|---|---|---|---|
| `rotation.activation_hours` | whole hours after the first issue | 8760 (1 year), all three mints; 0 = off, in all three | Later for a quiet mint (fewer swaps for users), sooner if the list grows fast from the start. |
| `rotation.period_hours` | whole hours | FSP 8760 (1 year); PSP and ESP 87600 (10 years). At least 1 in fsp-mint and esp-mint (no policy minimum). psp-mint: at least 24, or 0 for off | Shorter keeps the spent list (memory, disk) small for a busy mint, at the cost of more swaps and a shorter window for offline wallets. Longer is kinder to wallets that rarely connect. FSP is spent often and its wallets are online, so a year; PSP and ESP are held as savings, so ten years. |

### In fsp-mint

- Keysets are in `keysets.json`; the first-issue time in `first_issue.json`, written at the first `POST /issue` that issues anything, never changed. Before it exists, or with `rotation.activation_hours = 0`, there is one keyset (epoch 0).
- Epochs: the first rollover is at first issue + activation hours, then one every `rotation.period_hours`. The mint rolls over when the clock passes a boundary (checked on each request), makes the new keyset, and deletes the one two back with its spent list and log.
- Notes of the previous set can be spent and swapped until the next rotation; change and new FSP are always on the current set.
- `cargo run -- config` and the startup log print the first issue, the activation and the next rotation.

## Fees, and what they do not stop

Fees are charged on requests that carry value. A junk request that carries no value (a malformed body, a note that does not verify, a replay of a spent note) pays nothing, because it is refused before there is anything to charge. So fees do not protect against denial of service; the body ceiling and cheap checks first are what limit the cost of such a request. Per-DID and per-connection rate limits at the AIR layer are a possible further mitigation, not yet explored.

## `GET /rotation`

*Why public:* a holder needs to know when its notes stop being good. psp-mint and esp-mint have the same query, in the same shape.

## Storage policy (setting only)

`storage.mint_state` (`FSP_STORAGE_MINT_STATE`) names the AIR services that will keep this mint's state: an ordered list of service IDs, index 0 the primary and the rest secondaries in promotion order. A service ID is the service's AIR DID: a 33-byte compressed key in hex (66 characters, starting `02` or `03`). The 32-byte x-only form is refused with a message that says to use the DID. In the environment the IDs are comma-separated; in `fsp-mint.toml` the value is an array of strings or one string. One entry is valid (that service holds the state and the mint keeps a local backup copy); empty, the default, means local only; at most 8, no duplicates, no empty entry. The parser and checks are `sp_wire::storage` (sp-wire), shared by all three mints and the wallet, so a list means the same thing everywhere.

This is configuration and validation only. **Nothing reads or writes the AIR yet:** the state is still in local files, whatever the list says. The mint logs the list at startup like any setting. What the list will do, and what the mint's storage layer must then provide (eight numbered requirements: 1 a spend is written to the primary and at least one secondary before the mint answers, batched; 2 a local backup copy with one service; 3 only the mint promotes a secondary and updates its config; 4 writes carry a sequence number, so a returning primary is overwritten from the working copy; 5 the wallet file carries a version and is merged; 6 networks stay apart; 7 the mint's own local copy is the primary, fsynced, with external services as backup only and no cap; 8 a request's finishing entry and its ledger movements are one atomic write, which only esp-mint's per-day ledger needs), is that list: it is the whole set, and nothing else is promised. That replication belongs in mint-core's storage layer and is a candidate for AIR upstream. The mint's own local copy is the primary and every listed service is a backup (decided 2026-10-17): every spend is fsynced before the mint answers and replicated to the backups while they are reachable; while every backup is unreachable, unreplicated spends are queued and copied out in order to the first backup that returns. There is no cap: the mint carries on with no limit and never refuses or holds spends for replication, and `/health` only reports a degraded state while backups are behind. None of this is built.

## Running as a service

`fsp-mint serve` runs under `launchd` (macOS) or `systemd` (Linux) as a user that is not root, with its data directory (`FSP_DATA_DIR`), settings file (`FSP_CONFIG`) and network (`FSP_NETWORK`) named in an env file. `spi` never manages fsp-mint: spi is psp-mint, esp-mint and bitcoind only, and an FSP mint is run through the operator control interface of the AIR service it belongs to (a separate piece, not built). Until that exists the service's operator writes the definitions by hand; the ones the SPI repository generates for esp-mint are a model (see its README, "Running as services"), with the FSP names, data folder, settings file and network, and with `fsp-mint` started after `esp-mint` (and its AIR listener), the env file carrying `FSP_AIR_ESP_DID`, `FSP_AIR_SERVER` and `FSP_AIR_HOST`, which `serve` requires.

**Failure and restart.** `serve` exits with status 1 if it cannot reach the ESP mint at startup (to check the network), cannot open its data or cannot bind its address, and the service manager starts it again after a delay (growing under systemd, fixed under launchd) and never gives up. It does not wait for the ESP mint itself, since the manager's restart is the wait; the systemd unit starts it after esp-mint. A restart is picked up from what is on disk by the rules of a crash (see "Crash ordering").

**Stopping.** SIGTERM and SIGINT stop the API gracefully (no new connections, the requests being answered finish) and the process exits with status 0.

**Opening the data twice.** The database allows one process at a time, so a new process that starts while the old one is still stopping cannot open it; it exits with an error and the service manager starts it again. The storage that replaces sled has to give the same refusal or an equivalent single-writer guarantee, and the issue records and the spent list have to be written atomically and be durable once the call that wrote them returns, in the order "Crash ordering" gives.

**Health.** `GET /health` is `200` while the spender has no fault and `503` (`status: "broken"`, with the reason) after. The health monitor of the SPI repository (`sp-health-monitor.sh`, see its README, "The health monitor") can poll it and report or restart a mint that stays unhealthy; `spi` does not set it up for fsp-mint, so the service points the script's `HEALTH_URL` at fsp-mint's `/health` itself.
