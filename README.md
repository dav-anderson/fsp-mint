# fsp-mint

FSP is the fast point. Each AIR service runs its own copy of this mint and issues its own FSP in exchange for the ESP it accepts. Taking an FSP payment costs about as much as checking a few signatures.

## Building

fsp-mint builds from GitHub as it is: `Cargo.toml` names mint-core at `v0.2.0`, `sp-wire` at `v0.5.0` and, for the tests, `esp-mint` at `v0.7.0` (git tags), wire version 3, and the committed `Cargo.lock` matches; sp-wire's `air-client` feature is on. **AIR is the only way fsp-mint reaches the ESP mint and the only way wallets reach fsp-mint, and it is always built in: there is no cargo feature.** The tests start a real esp-mint in the process, with its AIR listener, and fsp-mint's own AIR listener beside it on the same `air_server`, so `esp-mint` is a dev-dependency (the tests need an esp-mint that has `air_listener` and `EspState::open`); the mint itself does not link esp-mint. `reqwest` is a dev-dependency too: the tests read `/audit` and `/health` over HTTP.

`air-client` needs `air_server`. sp-wire pins AIR at `eae5133` (AIR 3.0.0 plus the `SecretKey` export, on AIR's branch `air3-export-secretkey`) and air_server at `v1.0.1`, which takes `air` from crates.io; the `[patch.crates-io]` block in `Cargo.toml` points that at the same AIR commit, or two `air` crates are built and their types do not match (a patch applies only in the crate being built, so sp-wire's own block does not reach this one). To test against unpushed local copies of `sp-wire`, `esp-mint` or mint-core, `source ~/sandbox-mint/scripts/local-git.sh` points git at the local folders and their local tags. The tests also need port 5702 free: the AIR client always dials it, and the tests start an `air_server` there (they take turns on it).

## Run

```
cargo run -- config
cargo run -- service-key
FSP_SERVICE_DID=<DID, 66 hex characters> cargo run -- serve
```

`config` prints the settings in use. `service-key` makes `service_key.hex` and prints the service's DID (the 33-byte public key, 66 hex characters) to put in `service.did`. Start esp-mint first, with its AIR listener (`air.enabled`): fsp-mint asks it over AIR which network it is on before it starts, and sends it the ESP it accepts over AIR. `serve` needs the five AIR settings below (`air.esp_did`, `air.esp_server`, `air.esp_host` for the calls to the ESP mint, and `air.server`, `air.host` for this mint's own listener) and refuses to start without them. When the listener is up it prints the **`air.fsp` entry** a wallet needs for this service, `<service DID>@<AIR server key>@<host>`: put it in the wallet's `air.fsp` (`ECASH_AIR_FSP`).

## Settings

`fsp-mint.toml` in the data directory (`FSP_DATA_DIR`, default `~/.fsp-mint`; or a file named by `FSP_CONFIG`, which must then exist), then `FSP_...` environment variables. An unknown key, a wrong type or a value out of range stops the mint with every problem listed. `cargo run -- config` prints the values and what each does. Data lives in `FSP_DATA_DIR/<network>/`.

| Setting | Env var | Default | What it does |
|---|---|---|---|
| `server.network` | `FSP_NETWORK` | `main` | `main`, `regtest`, `signet`, `testnet`. Checked at startup against the network in the ESP mint's mint info, asked over AIR; a mismatch stops the mint. Also picks the data folder. |
| `server.bind` | `FSP_BIND` | `127.0.0.1:3200` | The address of the HTTP routes that are left, `GET /audit` and `GET /health`. **Loopback only**: a non-loopback address is refused when the settings load. Wallets do not use it; they reach this mint over AIR. |
| `server.max_request_bytes` | `FSP_MAX_REQUEST_BYTES` | `1048576` | Ceiling on a request body, checked before parsing. 16,384 to 15,728,640 (15 MiB: the most one AIR message carries). |
| `service.did` | `FSP_SERVICE_DID` | none | The service's DID: its 33-byte compressed public key, 66 hex characters starting `02` or `03`. The old 32-byte x-only form is refused. Only ESP named for it is accepted. Required to serve. |
| `fees.issue_fee_per_byte` | `FSP_ISSUE_FEE_PER_BYTE` | `0` | ESP to FSP fee, per byte of each issue group. 0 to 30,000,000 (the cap keeps the worst one-note group under the value of the smallest note, 1 PSP). |
| `rotation.activation_hours` | `FSP_ROTATION_ACTIVATION_HOURS` | `8760` | Whole hours from the first issue to the first rotation. **0 turns rotation off.** 0 to 876000. |
| `rotation.period_hours` | `FSP_ROTATION_PERIOD_HOURS` | `8760` | Whole hours between rotations. 1 to 876000; **0 is refused**, not an off switch here (use `activation_hours = 0`). No policy minimum, see [DESIGN.md](DESIGN.md), "No minimum period, and why". |
| `spend.max_batch` | `FSP_SPEND_MAX_BATCH` | `1024` | Most spends written per sync. |
| `issue.max_items` | `FSP_ISSUE_MAX_ITEMS` | `1000` | The most groups in one issue request, and the most ESP notes and the most FSP outputs in one group. More is refused `400 too_many_items` before anything is spent. Served on `GET /fees`. 64 to 25,000. |
| `spend.max_items` | `FSP_SPEND_MAX_ITEMS` | `1000` | The most notes in, and the most outputs, in one spend. More is refused `400 too_many_items`. Served on `GET /fees`. 64 to 25,000. |
| `spend.max_memo_bytes` | `FSP_SPEND_MAX_MEMO_BYTES` | `256` | The longest memo on a spend. More is refused `400 memo_too_long`. Served on `GET /fees`. 0 to 1,024. |
| `air.esp_did` | `FSP_AIR_ESP_DID` | none | **Required by `serve`.** The ESP mint's DID: its 33-byte compressed key, 66 hex characters (`esp-mint pubkey` prints it). The three calls to the ESP mint (mint info at start, keysets, transfer) are sealed to it, and its replies must be signed by it. There is no built-in DID. With `air.esp_server` and `air.esp_host`. |
| `air.esp_server` | `FSP_AIR_ESP_SERVER` | none | **Required by `serve`.** The name of the AIR server that carries the calls to the ESP mint (the SPI's): its public key as AIR prints it, 66 hex characters. Checked against the server's signed challenge. Was `air.server` before 0.6.0, when that was the only server. **To be replaced by DID resolution** (finding a mint's AIR server from its DID is not built). |
| `air.esp_host` | `FSP_AIR_ESP_HOST` | none | **Required by `serve`.** Where that server is: a host or IP, no scheme, path or port (the AIR client always dials 5702). `127.0.0.1` for a server on this machine. Was `air.host`. **To be replaced by DID resolution.** |
| `air.server` | `FSP_AIR_SERVER` | none | **Required by `serve`.** The AIR server this mint's **own listener** reads requests from and writes replies to: its public key, 66 hex characters. It may be the same server as `air.esp_server`, or another (the DIDs differ, so the mailboxes do). A wallet is given it, with the service's DID and the host, in its `air.fsp` entry. **To be replaced by DID resolution.** |
| `air.host` | `FSP_AIR_HOST` | none | **Required by `serve`.** Where that server is. **To be replaced by DID resolution.** |
| `air.held_reads` | `FSP_AIR_HELD_READS` | `16` | How many mailbox slots ahead of the next one the listener holds a read on. 1 to 200 (the AIR server closes a session at its 257th held read). Requests are answered one at a time, in slot order. |
| `air.reply_wait_secs` | `FSP_AIR_REPLY_WAIT_SECS` | `30` | How long to wait for the ESP mint's reply to one try. 1 to 600. |
| `air.attempts` | `FSP_AIR_ATTEMPTS` | `3` | Tries per call to the ESP mint before it gives up. A transfer with no reply leaves the issue pending (a signed "cannot answer now" to the wallet), and its request is kept. 1 to 10. |
| `air.max_skew_secs` | `FSP_AIR_MAX_SKEW_SECS` | `300` | How far a reply's `issued_at` may be from this mint's clock. 1 to 86400. |
| `air.timeout_secs` | `FSP_AIR_TIMEOUT_SECS` | `10` | How long the AIR server may take to answer one `Create` or `Read`. 1 to 600. |
| `esp.keyset_ttl_secs` | `FSP_ESP_KEYSET_TTL_SECS` | `3600` | How long the cached ESP (Issuer) current keyset, read over AIR, is used before it is read again; refreshed at once on `keyset_retired`. A failed read uses the cached set. 1 to 604800. |
| `storage.mint_state` | `FSP_STORAGE_MINT_STATE` | none (local only) | Service DIDs (66 hex, comma-separated) that will keep the mint's state; index 0 is the primary. One is valid; at most 8. **Setting only: nothing reads or writes the AIR yet.** See [DESIGN.md](DESIGN.md), "Storage policy". |

**`esp.url` and `esp.timeout_secs` (0.5.0 and before) are gone.** fsp-mint no longer calls the ESP mint over HTTP. A file or an environment that still names `esp.url`, `esp.timeout_secs`, `FSP_ESP_URL` or `FSP_ESP_TIMEOUT_SECS` stops the mint with a message that says so and names what replaces them (the `air.*` settings above). `esp.keyset_ttl_secs` is kept: the keyset is still cached, now read over AIR.

**Every fee defaults to 0.** `fees.issue_fee_per_byte` is the only fee setting in fsp-mint, and with it at 0 an issue costs no more than the ESP it exchanges. A fee is charged only if the operator sets one.

## Commands

| Command | What it does |
|---|---|
| `config` | Prints the settings in use and the rotation schedule. |
| `service-key` | Makes `service_key.hex` if there is none and prints its public half for `service.did`. |
| `serve` | Runs the AIR listener (the only way wallets reach this mint) and, on `server.bind` (loopback), `GET /audit` and `GET /health`. Needs `air.esp_did`, `air.esp_server`, `air.esp_host`, `air.server` and `air.host` (it names any that is missing), asks the ESP mint over AIR which network it is on before it starts, and prints the wallet's `air.fsp` entry. |
| `audit [--json]` | Prints what the mint owes and holds: outstanding FSP per key set and in all, and the ESP in reserve, matured and unmatured, each in units and in PSP. Reads the data files and writes nothing, so it works while `serve` runs. See "Audit". |
| `export-esp --out DIR` | Writes the kept ESP that has not been exported, one hand-over file per redeemable-after day (`esp-<day>-<time>.json`), for the service's ecash-wallet to `receive` and `redeem`, and marks those notes exported, so a second run writes nothing. The file is `sp_wire::esp::handover::Handover` (the type the wallet reads it with; code in `src/export.rs`): the service, the day, and each note with its amount, its spend secret and the ESP keyset that signed it. **Whoever holds the file holds the notes**; keep it private. A kept note with no keyset (kept before keysets) cannot be written, so the command stops before it writes anything and names each such note. Stop `serve` first. |

## The API

**Wallets reach fsp-mint over AIR, and only over AIR.** The mint answers on its own AIR listener (`air_listener`, always running under `serve`), under the service's DID: requests are sealed to `service.did` and replies are signed by the key in `service_key.hex`, which is the key `serve` already checks against `service.did`. Each call is a kind (REQUEST-DESIGN.md section 17 in `sandbox-mint`); the reply body is the JSON the route used to return, with the same size ceilings, and the status maps as in section 16: a `400` is a signed refusal with the route's JSON as it is, any other `4xx` a refusal that carries `"http_status": N`, and a `5xx` a signed "cannot answer now".

| Kind | What it does |
|---|---|
| 11 FSP issue | ESP in, FSP out. Body: groups, one per redeemable-after day, each with its handed-over ESP notes (spend secret and `keyset`) and its blinded FSP outputs. Answered with the signed outputs and the keyset that signed them. Refused (`400`, nothing spent) with `code` (`wrong_service`, `missing_keyset`, `too_many_items`, `note_held`, ...); ESP notes already transferred to this service are refused with `note_spent_elsewhere` and `spent`, the position of every one of them counted across the groups in order. A body over `server.max_request_bytes` is refused with the `413` JSON. When the ESP mint did not answer over AIR or refused the transfer (the `502` `issuer_unavailable` or `issuer_refused` before) the reply is a signed "cannot answer now"; the request stays pending, with the request to the ESP mint kept in `issues.json`, and the wallet's retry (the same request) finishes it from the stored answer without writing a second request. See [DESIGN.md](DESIGN.md), "Crash ordering (ESP to FSP)". |
| 12 FSP spend | A swap with change. Body: FSP inputs (each with its keyset, proof and spend signature), change outputs, the amount paid and a memo. Answered with the change signatures. Refused (`400`) with `code` (`no_inputs`, `too_many_items`, `memo_too_long`, `note_spent_elsewhere`, `keyset_retired`, `unknown_keyset`, ...). Nothing is spent unless the whole request passes. Inputs that are already spent are refused with `note_spent_elsewhere` and `spent`, the position of every one of them (it was `already_spent`, naming the first only). See [DESIGN.md](DESIGN.md), "Spending (the fast path)". |
| 2 keys | The public keys of the current and previous keysets, by ID. |
| 7 fees | The fees and limits in force: `issue_fee_per_byte`, the issue size formula (`issue_group_fixed_bytes` 16, `issue_note_bytes` 106, `issue_output_bytes` 41), `max_request_bytes`, `issue_max_items`, `spend_max_items` and `spend_max_memo_bytes`. |
| 4 rotation | Whether rotation is on, the first-issue time and the current and previous keysets. See "Rotation". |
| 1 mint info | `status` `ok`, or `broken` with a `reason` (a failed write to the spent log: a signed "cannot answer now" over AIR). Also reports `network`, `service` and `wire_version` (an integer: the version of the wire format this mint speaks; a wallet refuses a mint whose number is not its own). |

**What is left on HTTP** is local to the operator's machine, on `server.bind` (loopback only):

| Route | What it does |
|---|---|
| `GET /audit` | The operator audit, the same report as `fsp-mint audit --json`. Read-only. Like `/audit` on psp-mint and esp-mint it is not authenticated, so the address in `bind` decides who can read it. See "Audit". |
| `GET /health` | The same document as mint info, for the local health monitor (`sp-health-monitor.sh`, which runs on this machine): `200` with `status` `ok`, or `503` with `status` `broken`. SPI-local traffic like esp-mint's `GET /health`; wallets use mint info over AIR. |

`POST /issue`, `POST /spend`, `GET /keys`, `GET /fees` and `GET /rotation` are gone from HTTP (404).

### Rotation

A public, read-only query (kept separate from keys so keys stays the plain key list every mint has): whether rotation is active, `first_issue`, `rotation_activation_hours`, `activation`, `rotation_period_hours`, the current and previous keysets (ID, epoch, `starts`, `current_until`, `retires`), `next_rotation`, and the mint's clock `now` (all Unix seconds). The wallet uses it for `ecash-wallet fsp-window` and to time its staggered rollover.

## Audit

`fsp-mint audit` and `GET /audit` give the same report. Numbers are decimal strings in the JSON (a sum of FSP can pass what a JSON reader holds exactly).

**Outstanding FSP.** Per key set: `issued + change - spent`. `issued` is the FSP the issue call signed for the key set, `change` the FSP the spend call signed as change, `spent` the value of the notes spends used up. Over all key sets it is `issued - paid`: a swap moves value between key sets and only the amount paid leaves. `outstanding` is the total over the live key sets: what can still be spent. When a key set is deleted at rotation its notes expire; what was still outstanding in it is reported as `expired` and stays out of `outstanding`. Each figure is in FSP units and in PSP (`units / ESP_PER_PSP` from `sp-wire`, so 10^11 units to the PSP, exact to eleven decimals).

**ESP in reserve.** The ESP the mint kept from issues, split into `matured` (its redeemable-after day has come: `now / 86400 >= redeemable_after_day`, the rule esp-mint uses) and `unmatured`, with the notes, the ESP and the PSP of each, a line per day, and how much of it `export-esp` has already written out (`exported_esp`; the mint cannot tell whether the service's wallet has redeemed that yet).

**Warnings.** `consistent` is false when a key set has spent more than it issued and received as change, which should never happen and is worth a look.

**No dollar figure.** The USD value of FSP, PSP and ESP depends on a payout rate that this mint does not hold (and that is not kept in any of the repos). `spi audit` has the one rate and the arithmetic, and leaves FSP out of its figure.

## Wire version

The request and response types come from [`sp-wire`](../sp-wire/README.md), and the mint info (over AIR, and `GET /health` on loopback) returns `wire_version`. A wallet refuses a mint whose number is not its own. Details in [DESIGN.md](DESIGN.md), "Wire format and versions".

## Tests

```
cargo test
cargo fmt --check
```

`tests/fsp_flow.rs` runs a real esp-mint behind its AIR listener, and fsp-mint's own AIR listener, on a real `air_server` started in the test process (shared helpers in `tests/common/mod.rs`; port 5702, which the AIR client always dials, must be free, and the tests take turns on it). The esp-mint is taken at its release tag, so a change in esp-mint is not exercised until the tag moves. Before a release in which esp-mint and fsp-mint change together, run `SP_LOCAL_ONLY=fsp-mint sh ~/sandbox-mint/scripts/test-wallet-against-local-mints.sh`, which runs these tests against the esp-mint folder on this machine without changing any repo. With the tag as it is, the tests need `esp_mint::server::EspState::open`, which the tag does not have, so they build only against the local folder until esp-mint is tagged again.

Unit tests are in each module. The calls the HTTP tests made now go to the listener over AIR; the tests also cover an issue and a spend end to end, a refused spend, an ESP mint that does not answer (the issue stays pending and the retry finishes it with no second issue), `serve` refusing without its listener settings, `export-esp` (the hand-over files, read back with `sp_wire::esp::handover::Handover`, and the refusal of a note with no keyset), and that only `/audit` and `/health` answer over HTTP. `tests/cli.rs` runs the real `fsp-mint` program for the messages an operator sees (an unknown command, a damaged or unknown setting, `export-esp` without `--out` or `service.did`, an audit that cannot read the data, including a file where the data folder should be); it needs no AIR server. The ESP the tests issue is signed with the ESP mint's key and booked in its ledger, as an exchange books it.

## Documentation

- [DESIGN.md](DESIGN.md): how fsp-mint works and why: issuing, crash ordering, spending, key rotation, fees.
- [CHANGELOG.md](CHANGELOG.md): what was built and what changed.
- [ROADMAP.md](../sandbox-mint/ROADMAP.md) (in `sandbox-mint`): left to do and not decided yet, for every repo.
- [OPERATIONS.md](../sandbox-mint/OPERATIONS.md) (in `sandbox-mint`): running the mints as services: `launchd` and `systemd` definitions, install and uninstall, the health monitor.
- [ecash-wallet](../ecash-wallet/README.md): the wallet side (`ecash-wallet fsp ...`) and the regtest runs in its `REGTEST.md` (steps 35 to 37, 42, 43 and 48).
- [sandbox-mint README](../sandbox-mint/README.md): the family of repos.
