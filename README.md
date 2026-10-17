# fsp-mint

FSP is the fast point. Each AIR service runs its own copy of this mint and issues its own FSP in exchange for the ESP it accepts. Taking an FSP payment costs about as much as checking a few signatures.

## Building

fsp-mint builds from tagged dependencies: mint-core at `v0.1.3` and `sp-wire` at `v0.2.0` (git tags). It does not use the `air` crate. Its tests start a real esp-mint in the process, so `esp-mint` at `v0.1.2` is a dev-dependency; the mint itself does not link esp-mint and talks to it over HTTP. To test against unpushed local copies, `source ~/sandbox-mint/scripts/local-git.sh` points git at the local folders and their local tags (the recipe, with the plain `[patch]` form, is in the top-level README in `sandbox-mint`).

## Run

```
cargo run -- config
cargo run -- service-key
FSP_SERVICE_DID=<key> cargo run -- serve
```

`config` prints the settings in use. `service-key` makes `service_key.hex` and prints the public key to put in `service.did`. Start esp-mint first: fsp-mint checks its network at startup and sends it the ESP it accepts.

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
| `rotation.period_hours` | `FSP_ROTATION_PERIOD_HOURS` | `8760` | Whole hours between rotations. 1 to 876000; **0 is refused**, not an off switch here (use `activation_hours = 0`). No policy minimum, see [DESIGN.md](DESIGN.md), "No minimum period, and why". |
| `spend.max_batch` | `FSP_SPEND_MAX_BATCH` | `1024` | Most spends written per sync. |
| `issue.max_items` | `FSP_ISSUE_MAX_ITEMS` | `1000` | The most groups in one issue request, and the most ESP notes and the most FSP outputs in one group. More is refused `400 too_many_items` before anything is spent. Served on `GET /fees`. 1 to 100,000. |
| `spend.max_items` | `FSP_SPEND_MAX_ITEMS` | `1000` | The most notes in, and the most outputs, in one spend. More is refused `400 too_many_items`. Served on `GET /fees`. 1 to 100,000. |
| `spend.max_memo_bytes` | `FSP_SPEND_MAX_MEMO_BYTES` | `256` | The longest memo on a spend. More is refused `400 memo_too_long`. Served on `GET /fees`. 0 to 65,536. |
| `esp.url` | `FSP_ESP_URL` | `http://127.0.0.1:3100` | The ESP mint (Issuer) where accepted ESP is transferred. |
| `esp.timeout_secs` | `FSP_ESP_TIMEOUT_SECS` | `30` | How long one ESP mint call may take. 1 to 600. |
| `esp.keyset_ttl_secs` | `FSP_ESP_KEYSET_TTL_SECS` | `3600` | How long the cached ESP (Issuer) current keyset is used before it is read again; refreshed at once on `keyset_retired`. A failed read uses the cached set. 1 to 604800. |
| `storage.mint_state` | `FSP_STORAGE_MINT_STATE` | none (local only) | Service IDs (64 hex, comma-separated) that will keep the mint's state; index 0 is the primary. One is valid; at most 8. **Setting only: nothing reads or writes the AIR yet.** See [DESIGN.md](DESIGN.md), "Storage policy". |

**Every fee defaults to 0.** `fees.issue_fee_per_byte` is the only fee setting in fsp-mint, and with it at 0 an issue costs no more than the ESP it exchanges. A fee is charged only if the operator sets one.

## Commands

| Command | What it does |
|---|---|
| `config` | Prints the settings in use and the rotation schedule. |
| `service-key` | Makes `service_key.hex` if there is none and prints its public half for `service.did`. |
| `serve` | Runs the HTTP API. |
| `audit [--json]` | Prints what the mint owes and holds: outstanding FSP per key set and in all, and the ESP in reserve, matured and unmatured, each in units and in PSP. Reads the data files and writes nothing, so it works while `serve` runs. See "Audit". |
| `export-esp --out DIR` | Writes the kept ESP, one hand-over file per redeemable-after day, for the service's ecash-wallet to `receive` and `redeem`. Stop `serve` first. |

## HTTP API

| Route | What it does |
|---|---|
| `POST /issue` | ESP in, FSP out. Body: groups, one per redeemable-after day, each with its handed-over ESP notes (spend secret and `keyset`) and its blinded FSP outputs. `200` with the signed outputs and the keyset that signed them. `400` if refused (nothing spent), with `code` (`wrong_service`, `missing_keyset`, `too_many_items`, `note_held`, ...). `413` if the body is over `server.max_request_bytes`. `502` with `issuer_unavailable` or `issuer_refused` when the ESP mint could not be reached or refused the transfer; the request stays pending and a retry finishes it. See [DESIGN.md](DESIGN.md), "Crash ordering (ESP to FSP)". |
| `POST /spend` | A swap with change. Body: FSP inputs (each with its keyset, proof and spend signature), change outputs, the amount paid and a memo. `200` with the change signatures. `400` if refused, with `code` (`no_inputs`, `too_many_items`, `memo_too_long`, `already_spent`, `keyset_retired`, `unknown_keyset`, ...). Nothing is spent unless the whole request passes. See [DESIGN.md](DESIGN.md), "Spending (the fast path)". |
| `GET /keys` | The public keys of the current and previous keysets, by ID. |
| `GET /fees` | The fees and limits in force: `issue_fee_per_byte`, the issue size formula (`issue_group_fixed_bytes` 16, `issue_note_bytes` 106, `issue_output_bytes` 41), `max_request_bytes`, `issue_max_items`, `spend_max_items` and `spend_max_memo_bytes`. The wallet does not read these yet (it reads only the ESP mint's limits); an FSP request over a limit is refused by the mint before anything is spent. |
| `GET /rotation` | Whether rotation is on, the first-issue time and the current and previous keysets. See "`GET /rotation`". |
| `GET /audit` | The operator audit, the same report as `fsp-mint audit --json`. Read-only. Like `/audit` on psp-mint and esp-mint it is not authenticated, so the address in `bind` decides who can read it. See "Audit". |
| `GET /health` | `200` with `status` `ok`, or `503` with `status` `broken` and a `reason` (a failed write to the spent log). Also reports `network`, `service` and `wire_version` (an integer: the version of the wire format this mint speaks; a wallet refuses a mint whose number is not its own). |

### `GET /rotation`

A public, read-only query (kept separate from `/keys` so `/keys` stays the plain key list every mint has): whether rotation is active, `first_issue`, `rotation_activation_hours`, `activation`, `rotation_period_hours`, the current and previous keysets (ID, epoch, `starts`, `current_until`, `retires`), `next_rotation`, and the mint's clock `now` (all Unix seconds). The wallet uses it for `ecash-wallet fsp-window` and to time its staggered rollover.

## Audit

`fsp-mint audit` and `GET /audit` give the same report. Numbers are decimal strings in the JSON (a sum of FSP can pass what a JSON reader holds exactly).

**Outstanding FSP.** Per key set: `issued + change - spent`. `issued` is the FSP `POST /issue` signed for the key set, `change` the FSP `POST /spend` signed as change, `spent` the value of the notes spends used up. Over all key sets it is `issued - paid`: a swap moves value between key sets and only the amount paid leaves. `outstanding` is the total over the live key sets: what can still be spent. When a key set is deleted at rotation its notes expire; what was still outstanding in it is reported as `expired` and stays out of `outstanding`. Each figure is in FSP units and in PSP (`units / ESP_PER_PSP` from `sp-wire`, so 10^11 units to the PSP, exact to eleven decimals).

**ESP in reserve.** The ESP the mint kept from issues, split into `matured` (its redeemable-after day has come: `now / 86400 >= redeemable_after_day`, the rule esp-mint uses) and `unmatured`, with the notes, the ESP and the PSP of each, a line per day, and how much of it `export-esp` has already written out (`exported_esp`; the mint cannot tell whether the service's wallet has redeemed that yet).

**Warnings.** `complete` is false when issues or spends from before 0.4.0 are in the data: they did not record their amounts and cannot be counted, and the report says how many. `consistent` is false when a key set has spent more than it issued and received as change, which should never happen and is worth a look.

**No dollar figure.** The USD value of FSP, PSP and ESP depends on a payout rate that this mint does not hold (and that is not kept in any of the repos). `spi audit` has the one rate and the arithmetic, and leaves FSP out of its figure.

## Wire version

The request and response types come from [`sp-wire`](../sp-wire/README.md), and `GET /health` returns `wire_version`. A wallet refuses a mint whose number is not its own. Details in [DESIGN.md](DESIGN.md), "Wire format and versions".

## Tests

```
cargo test
cargo fmt --check
```

`tests/fsp_flow.rs` runs a real esp-mint, taken at its release tag, so a change in esp-mint is not exercised until the tag moves. Before a release in which esp-mint and fsp-mint change together, run `SP_LOCAL_ONLY=fsp-mint sh ~/sandbox-mint/scripts/test-wallet-against-local-mints.sh`, which runs these tests against the esp-mint folder on this machine without changing any repo. With the tag as it is, the esp-mint test state needs the newer field `in_flight`, so these tests build only against the local folder until esp-mint is tagged again.

Unit tests are in each module. `tests/fsp_flow.rs` starts a real esp-mint in the same process. The spend-path throughput test is `cargo test --release throughput -- --nocapture`.

## Documentation

- [DESIGN.md](DESIGN.md): how fsp-mint works and why: issuing, crash ordering, spending, key rotation, fees.
- [CHANGELOG.md](CHANGELOG.md): what was built and what changed.
- [ROADMAP.md](../sandbox-mint/ROADMAP.md) (in `sandbox-mint`): left to do and not decided yet, for every repo.
- [OPERATIONS.md](../sandbox-mint/OPERATIONS.md) (in `sandbox-mint`): running the mints as services: `launchd` and `systemd` definitions, install and uninstall, the health monitor.
- [ecash-wallet](../ecash-wallet/README.md): the wallet side (`ecash-wallet fsp ...`) and the regtest runs in its `REGTEST.md` (steps 35 to 37, 42, 43 and 48).
- [sandbox-mint README](../sandbox-mint/README.md): the family of repos.
