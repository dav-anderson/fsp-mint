# Changelog

fsp-mint 0.1.2 builds on `sp-wire` 0.2.0 (wire version 2) and mint-core 0.1.3. Nothing was recorded per version before this file; the section below is everything the README said about what was built and when.

## 0.4.0 (2026-10-17)

- Test helpers take `&Path` instead of `&PathBuf` (lint). Documentation: the storage requirements are the numbered set (1 to 8), and the service files and health monitor are described by the SPI repository, not by a folder outside this repo. Every fee setting defaults to 0 (`fees.issue_fee_per_byte` is the only one), stated in the README.
- **Operator audit.** `GET /audit` and `fsp-mint audit [--json]`: outstanding FSP per key set and in total (`issued + change - spent`, in FSP units and PSP), FSP expired with deleted key sets, and the ESP in reserve from the kept list, matured and unmatured, in ESP and PSP, with a line per redeemable-after day and the part already written out by `export-esp`. `complete` and `consistent` flags and warnings.
- **Crash-safe counters.** `issues.json` gets `issued` (FSP units per key set, saved with the stored answer) and `uncounted_requests`; each spend log line gets `a` (input and output units); `ledger.json` (new) keeps the counters of deleted key sets, copied before their logs go and repaired at start-up from logs left behind. Old data still loads: lines and answers from 0.3.0 are reported as not counted.
- The spend log reader has a read-only mode that ignores a half-written last line instead of cutting it (used by `audit`; `serve` still cuts it at start-up).
- New module `audit`. Tests in `tests/fsp_flow.rs`: counts across issue (once, with a retry), spend, change and a refused spend; the reserve across the maturity day; rotation, expiry, the report from the files equal to the report from the mint, and a restart; a key set deleted before its counters were saved; a torn last line read without cutting; data from 0.3.0.
- No dollar figure: the payout rate is not kept in this repo. `spi audit` does that arithmetic and leaves FSP out.

## 0.3.0 (2026-10-17)

- **Setting `storage.mint_state` (`FSP_STORAGE_MINT_STATE`): which AIR services will keep this mint's state.** An ordered list of service IDs (index 0 the primary, the rest secondaries); one entry is valid, empty (the default) means local only, at most 8, no duplicates. Config and validation only: nothing reads or writes the AIR, the state is still local files. The parser and checks are `sp_wire::storage` (sp-wire 0.4.0), shared with the other mints and the wallet. In `fsp-mint.toml` the value is an array of strings or one string. DESIGN.md has "Storage policy (setting only)"; the planned behaviour is in sandbox-mint's DESIGN.md, "AIR storage requirements".
- `sp-wire` is now the `v0.4.0` tag. Tests: the setting from the file and the environment, both forms, and every kind of refusal.

## 0.2.1 (2026-10-16)

- Tests: `tests/fsp_flow.rs` builds esp-mint's state with the new `in_flight` field, so it needs esp-mint after the lock change (the local folder, or the next tag). `sandbox-mint/scripts/test-wallet-against-local-mints.sh` now also runs fsp-mint's tests against the local esp-mint.

- **Running as a service.** `serve` now stops on SIGTERM as well as SIGINT (the API drains and the process exits 0); before, it did not stop gracefully on either. `tokio` gets the `signal` feature. `launchd` and `systemd` definitions, env and monitor settings examples and `sp-health-monitor.sh` are in `sandbox-mint/ops`, with the steps in `sandbox-mint/OPERATIONS.md`. DESIGN.md: "Running as a service", with what the AIR storage has to keep.

## 0.2.0

- The cache of the ESP mint's current keyset is no longer refreshed when the ESP mint answers `keyset_not_current` (it no longer sends it: it signs with any live keyset). It is still dropped and read again on `keyset_retired`.
- **The spend memo limit is a setting.** `spend.max_memo_bytes` (`FSP_SPEND_MAX_MEMO_BYTES`, default 256, the old constant) is the longest memo on a spend, and is served on `GET /fees` as `spend_max_memo_bytes` (a new optional field of `sp-wire`'s `FeesResponse`: a mint that does not send it is read as the default). Tests: the default and the range; a memo over the limit gets `memo_too_long` and spends nothing; `GET /fees` serves it.
- The comment at the top of `keysets.rs` described rotation by `activation_date` and `period_days`; it now describes the hours the code uses.

## Moved from the README

The sections and sentences below were in the README and describe what was built, when, or what changed. They are kept here word for word. A short fragment is a part of a sentence that was cut out; the rest of the sentence stays in the README or DESIGN.md under the same heading.

### Status (as it was in the README)

Built and tested: settings, the service's DID key, `POST /issue` (ESP in, FSP out), `POST /spend` (a swap with change), `GET /keys`, `GET /fees`, `GET /rotation`, `GET /health`, key rotation, and `export-esp` to hand the kept ESP to the service's wallet for redemption. Tests: `cargo test` (unit tests in each module and `tests/fsp_flow.rs`, which runs a real esp-mint in the same process). The wallet side (`ecash-wallet fsp ...`) is built and documented in the ecash-wallet README, and the regtest runs are in its `REGTEST.md` (steps 35 to 37, 42, 43 and 48). The family and its status are in the top-level `~/sandbox-mint/README.md`.

### Request hashes

Entries stored under the v1 tags are not found by a retry after the upgrade.

### Wire version

(2: version 1 was the first release, and version 2 changed only the sealed PSP payload, which this mint does not handle, so no FSP byte moved)

The golden values were captured from this mint before the move.

### Retired keyset list

Before this both got `keyset_retired`.
