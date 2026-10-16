# fsp-mint

FSP is the fast point. Each AIR service runs its own copy of this mint and issues its own FSP in exchange for the ESP it accepts. Taking an FSP payment costs about as much as checking a few signatures.

## Status

Built and tested: settings, the service's DID key, `POST /issue` (ESP in, FSP out), `POST /spend` (a swap with change), `GET /keys`, `GET /fees`, `GET /rotation`, `GET /health`, key rotation, and `export-esp` to hand the kept ESP to the service's wallet for redemption. Tests: `cargo test` (unit tests in each module and `tests/fsp_flow.rs`, which runs a real esp-mint in the same process). The wallet side (`ecash-wallet fsp ...`) is built and documented in the ecash-wallet README, and the regtest runs are in its `REGTEST.md` (steps 35 to 37, 42, 43 and 48). The family and its status are in the top-level `~/sandbox-mint/README.md`.

## Design

How fsp-mint is built, and why. Each item says what the design is and gives the reason.

1. **The service operator runs fsp-mint and holds the service's DID key.** fsp-mint does the ESP transfer at the Issuer (esp-mint) itself, signed by the service, and keeps the ESP it receives to redeem later. The key is `service_key.hex` in the network's data folder (owner-only permissions), made by `fsp-mint service-key`. *Why:* the ESP must be moved to notes only the service controls before FSP is issued against it, and the mint that issues FSP is the only party that can do both steps under one stored request; a separate wallet step would leave a gap where FSP is issued against ESP the app could still spend.
2. **The app sends one request: `POST /issue` with its handed-over ESP notes (each with its spend secret) and its blinded FSP outputs.** The request is in groups, one per redeemable-after day. A failure between the ESP transfer and the FSP issuance is handled by a stored pending entry (below, "Crash ordering").
3. **Answers are stored by request hash**, as esp-mint does for exchange and transfer. A retry of the same request gets the same answer, or finishes the pending one. *Why:* a lost reply must never issue twice or not at all.
4. **FSP ignores ESP maturity.** FSP is spendable at once, and one issue may mix ESP of several days. *Why:* the day only matters when the service redeems the ESP with the Issuer; for the app, a point at the service is a point at the service. The service carries the wait until the ESP matures (`ecash-wallet esp-schedule` shows when).
5. **FSP is 1:1 with ESP units** (10^-12 USD per unit; amounts are `u64`, sums are checked), **in power-of-two denominations**, like PSP and ESP.
6. **Only ESP named for the service's own DID is accepted.** The DID is `service.did` in the config (the x-only public key printed by `service-key`); a request for another service is refused (`wrong_service`) before anything is sent, and the Issuer refuses any note that is not for that service or day.
7. **A spend is a swap with change:** FSP notes in, change outputs out, and an amount paid to the service. Inputs = outputs + pay, exactly.
8. **The ESP to FSP fee is a setting, `fees.issue_fee_per_byte`, default 0**, charged per byte of the issue group (as esp-mint charges per byte). Each group must be exact: ESP = FSP outputs + fee. The body ceiling (`server.max_request_bytes`) and the order of checks are the same as in the other mints: the size first (before parsing), then the checks that need nothing but the request, then the stored answer, then the state.

## Crash ordering (ESP to FSP)

One issue spans two mints run by different operators, so it runs as a stored entry that any retry finishes:

1. Checks that touch nothing: shape, denominations, the service, each spend secret against its note, each group exact.
2. A stored answer for this request hash is returned as it is; a pending entry is picked up where it stopped.
3. No ESP note or FSP output of the request may be held by another pending issue, and no ESP note may have been kept before (reserved inputs and outputs).
4. One ESP transfer per group is built (the app's notes in, fresh ESP notes of the same sizes out, signed by each note's secret and by the service's DID key). **The pending entry, with the transfers and the new notes' secrets, is written to `issues.json` before anything is sent.**
5. Each transfer is sent. Done: the new ESP notes are unblinded and kept. Refused (`400`, nothing burned at the Issuer): that group gets no FSP. Unknown (unreachable, timeout, `5xx`): the request answers `502` and stays pending; a retry sends the same transfer, which the Issuer answers from its own store.
6. When every group is settled, the FSP outputs of the transferred groups are signed (deterministic, so a crash here just signs again), the answer is stored, and returned.

*Why this order:* writing the pending entry first means a crash at any point leaves either nothing sent, or a transfer the Issuer will answer the same way again. The ESP can be transferred only once at the Issuer and the FSP answer only once here, so nothing is issued twice and nothing is lost.

## Spending (the fast path)

`POST /spend` takes FSP inputs (each with its keyset ID, its proof and a spend signature), change outputs, the amount paid and a memo. Order of checks: shape and amounts (no lock), the stored answer, keysets and signatures (no lock), then the spent lists (under the lock), then sign and write. Inputs must be on the current or previous keyset; change is signed with the current one.

**ESP keysets on `POST /issue`.** Each handed ESP note names the ESP keyset that signed it (`keyset`, 16 hex; `400 missing_keyset` without it). The transfer to the Issuer names those keysets for its inputs and, for its outputs, the Issuer's current keyset when the issue is accepted (fixed then, so a retry sends the same transfer). That keyset is **cached**: read from esp-mint's `GET /keysets`, used for `esp.keyset_ttl_secs` (default 3600 s), then read again. If the Issuer answers a transfer `keyset_retired` or `keyset_not_current`, the cache is dropped at once, the keyset read again, the group's ESP outputs re-blinded for it (nothing was burned) and the transfer sent once more. If a read fails, the cached keyset is used; only with none cached is the issue refused `502 issuer_unavailable` (nothing stored). The cache is in memory, so a restart reads it again.

**Request hashes cover every keyset field.** The issue hash (`fsp-mint/issue-request/v2`) includes each handed ESP note's `keyset` with its length; the spend hash (`fsp-mint/spend-request/v2`) adds every input keyset, length-prefixed, to the digest. Two requests that differ only in a keyset never share a hash. Entries stored under the v1 tags are not found by a retry after the upgrade. The kept ESP records the keyset the Issuer's reply names, and `export-esp` writes it into the hand-over so the service's wallet names it when it redeems. The FSP outputs are signed with the keyset chosen at acceptance, or the current one if that has since been deleted, and every reply, stored ones included, names the keyset that signed it.

**Spent lists and batching.** One spent list per live keyset, in memory, backed by an append-only log (`spends-<keyset>.jsonl`). Each spend is one line with the nonces it spent and its stored answer. A single writer thread takes every line waiting (up to `spend.max_batch`, default 1024), appends them, syncs once, and only then are those spends answered (group commit). There is no flush timer: a batch is whatever arrived during the last sync, so a lone spend waits for one sync and a busy mint shares each sync among many spends.

**Replay window: none.** No spend is answered before its line is on disk. A crash can only lose lines whose spends were never answered: their notes are unspent again after the restart and no signature for them left the mint, so the payer just retries. A torn last line is cut at startup. If a write fails, the mint stops taking spends (`/health` reports it) and keeps those notes marked spent until it restarts. *Why no window:* the cost of waiting for a sync is shared by the batch, so safety costs almost nothing in throughput.

**Throughput.** `throughput_of_the_spend_path` (run `cargo test --release throughput -- --nocapture`) sends 2000 spends (one note in, one change output) through the spend core with 8 worker threads, including proof and signature checks and the synced log. On the development Mac it measured about 62,000 spends a second (2026-10-15), well above the target of a thousand. It does not include HTTP or the network.

## HTTP API

| Route | What it does |
|---|---|
| `POST /issue` | ESP in, FSP out. Body: groups, one per redeemable-after day, each with its handed-over ESP notes (spend secret and `keyset`) and its blinded FSP outputs. `200` with the signed outputs and the keyset that signed them. `400` if refused (nothing spent), with `code` (`wrong_service`, `missing_keyset`, `too_many_items`, `note_held`, ...). `413` if the body is over `server.max_request_bytes`. `502` with `issuer_unavailable` or `issuer_refused` when the ESP mint could not be reached or refused the transfer; the request stays pending and a retry finishes it. See "Crash ordering". |
| `POST /spend` | A swap with change. Body: FSP inputs (each with its keyset, proof and spend signature), change outputs, the amount paid and a memo. `200` with the change signatures. `400` if refused, with `code` (`no_inputs`, `too_many_items`, `memo_too_long`, `already_spent`, `keyset_retired`, `unknown_keyset`, ...). Nothing is spent unless the whole request passes. See "Spending". |
| `GET /keys` | The public keys of the current and previous keysets, by ID. |
| `GET /fees` | The fees and limits in force: `issue_fee_per_byte`, the issue size formula (`issue_group_fixed_bytes` 16, `issue_note_bytes` 106, `issue_output_bytes` 41), `max_request_bytes`, `issue_max_items` and `spend_max_items`. The wallet does not read these two yet (it reads only the ESP mint's limits); an FSP request over a limit is refused by the mint before anything is spent. |
| `GET /rotation` | Whether rotation is on, the first-issue time and the current and previous keysets. See "`GET /rotation`". |
| `GET /health` | `200` with `status` `ok`, or `503` with `status` `broken` and a `reason` (a failed write to the spent log). Also reports `network` and `service`. |

## Key rotation

### Why a spent list, and how fast it grows

Notes are bearer notes with blind signatures: the mint signed each one without seeing it, so it cannot tell one of its notes from another except by the note's own nonce, and it cannot recognise a note it has already seen unless it keeps a record. Without that record the same note could be spent again and again. So every mint keeps a **spent list**: the nonce of every note it has taken in. There is no way around it for this kind of note; only the size can be managed.

Each entry is a note ID of about 32 bytes (the nonce is a 33-byte compressed key), plus the storage's own overhead. In fsp-mint the list is a hash set in memory (roughly 33 bytes of key plus some 50 to 70 bytes of hash-set overhead per note) and an append-only log on disk; each spend's log line also carries its stored answer (the signatures for the change), so on disk a spend costs about 250 bytes plus about 140 bytes per change output. psp-mint and esp-mint keep their lists in their sled database (mint-core's store). A list only grows: a million spent notes is roughly 100 MB of memory and a few hundred MB of log in fsp-mint.

### Why two key sets and two lists, and rotation

A spent list can only be cut down by cutting off the notes it protects. So the keys **rotate**: every period a new key set (keyset) is made. New notes are signed with it. Notes of the set before it stay good for one more period: they can still be spent, transferred, redeemed or swapped. At the next rotation the oldest set is deleted **together with its spent list**, because no note of that set can be presented any more, so nothing needs to be remembered about it. At any time there are at most two sets and two lists, **current** and **previous**. A spend checks the note's keyset ID against the matching list.

**Notes untouched through a full cycle are lost.** A note made just before a rotation is on the previous set at once and has one period left; a note made just after has up to two periods. A note nobody spends or swaps in that time cannot be used after its set is deleted. For FSP (period 8760 hours) that is one to two years; for PSP and ESP (period 87600 hours) about 10 to 20 years. The wallet prevents this by swapping on its own (below), so only a wallet left offline for the whole window loses notes.

### Retired keyset list

When a rotation deletes a keyset, its 8-byte ID is added to a retired list kept in `keysets.json` itself, so the one atomic save that drops the keyset's secret also lists its ID: a crash can never leave a deleted keyset unlisted. A spend naming a listed ID gets `keyset_retired` (the note expired); an ID neither live nor listed gets `unknown_keyset` (junk, or never this mint's). Before this both got `keyset_retired`. This lets the mint tell genuinely expired notes from junk, so a wallet can explain a loss.

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

### `GET /rotation`

A public, read-only query (kept separate from `/keys` so `/keys` stays the plain key list every mint has): whether rotation is active, `first_issue`, `rotation_activation_hours`, `activation`, `rotation_period_hours`, the current and previous keysets (ID, epoch, `starts`, `current_until`, `retires`), `next_rotation`, and the mint's clock `now` (all Unix seconds). The wallet uses it for `ecash-wallet fsp-window` and to time its staggered rollover. *Why public:* a holder needs to know when its notes stop being good. psp-mint and esp-mint have the same query, in the same shape.

## Fees, and what they do not stop

Fees are charged on requests that carry value. A junk request that carries no value (a malformed body, a note that does not verify, a replay of a spent note) pays nothing, because it is refused before there is anything to charge. So fees do not protect against denial of service; the body ceiling and cheap checks first are what limit the cost of such a request. Per-DID and per-connection rate limits at the AIR layer are a possible further mitigation, not yet explored.

## Settings

`fsp-mint.toml` in the data directory (`FSP_DATA_DIR`, default `~/.fsp-mint`; or a file named by `FSP_CONFIG`, which must then exist), then `FSP_...` environment variables. An unknown key, a wrong type or a value out of range stops the mint with every problem listed. `cargo run -- config` prints the values and what each does. Data lives in `FSP_DATA_DIR/<network>/`.

| Setting | Env var | Default | What it does |
|---|---|---|---|
| `server.network` | `FSP_NETWORK` | `main` | `main`, `regtest`, `signet`, `testnet`. Checked at startup against the ESP mint's `/health`; a mismatch stops the mint. Also picks the data folder. |
| `server.bind` | `FSP_BIND` | `127.0.0.1:3200` | The HTTP API's address. |
| `server.max_request_bytes` | `FSP_MAX_REQUEST_BYTES` | `1048576` | Ceiling on a request body, checked before parsing. 4096 to 1 GiB. |
| `service.did` | `FSP_SERVICE_DID` | none | The service's DID (x-only key, hex). Only ESP named for it is accepted. Required to serve. |
| `fees.issue_fee_per_byte` | `FSP_ISSUE_FEE_PER_BYTE` | `0` | ESP to FSP fee, per byte of each issue group. |
| `rotation.activation_hours` | `FSP_ROTATION_ACTIVATION_HOURS` | `8760` | Whole hours from the first issue to the first rotation. **0 turns rotation off.** 0 to 876000. |
| `rotation.period_hours` | `FSP_ROTATION_PERIOD_HOURS` | `8760` | Whole hours between rotations. 1 to 876000; **0 is refused**, not an off switch here (use `activation_hours = 0`). No policy minimum, see above. |
| `spend.max_batch` | `FSP_SPEND_MAX_BATCH` | `1024` | Most spends written per sync. |
| `issue.max_items` | `FSP_ISSUE_MAX_ITEMS` | `1000` | The most groups in one issue request, and the most ESP notes and the most FSP outputs in one group. More is refused `400 too_many_items` before anything is spent. Served on `GET /fees`. 1 to 100,000. |
| `spend.max_items` | `FSP_SPEND_MAX_ITEMS` | `1000` | The most notes in, and the most outputs, in one spend. More is refused `400 too_many_items`. Served on `GET /fees`. 1 to 100,000. |
| `esp.url` | `FSP_ESP_URL` | `http://127.0.0.1:3100` | The ESP mint (Issuer) where accepted ESP is transferred. |
| `esp.timeout_secs` | `FSP_ESP_TIMEOUT_SECS` | `30` | How long one ESP mint call may take. 1 to 600. |
| `esp.keyset_ttl_secs` | `FSP_ESP_KEYSET_TTL_SECS` | `3600` | How long the cached ESP (Issuer) current keyset is used before it is read again; refreshed at once on `keyset_retired` / `keyset_not_current`. A failed read uses the cached set. 1 to 604800. |

## Commands

| Command | What it does |
|---|---|
| `config` | Prints the settings in use and the rotation schedule. |
| `service-key` | Makes `service_key.hex` if there is none and prints its public half for `service.did`. |
| `serve` | Runs the HTTP API. |
| `export-esp --out DIR` | Writes the kept ESP, one hand-over file per redeemable-after day, for the service's ecash-wallet to `receive` and `redeem`. Stop `serve` first. |

## Left to do

The items every repo shares (all traffic over the AIR except local processes on one machine, the local store and the AIR storage rule, the AIR path dependency, the shared wire format, and the mint-core swap limit) are listed once in the top-level `~/sandbox-mint/README.md`. This list has only this repo's own items.

- **Redeem deadline: considered and declined.** ESP is not rotated by the FSP scheme, and a deadline after which a matured note can no longer be redeemed was considered for ESP and declined (see esp-mint's README). Nothing is planned.
- **Local store.** State is in local files (`keysets.json`, `issues.json`, the spent logs and the kept ESP). Only one process may use a data folder; do not copy it while `serve` runs. fsp-mint's spent list is its own append-only log, not mint-core's sled store. Whatever replaces these files in the AIR must keep the rule from mint-core v0.1.3: marking a nonce used and updating a total are one atomic step.
- **Mint keystore (deferred to the AIR).** The service key and the keyset secrets move to the AIR.
