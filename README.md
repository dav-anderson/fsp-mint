# fsp-mint

FSP is the fast point. Each AIR service issues its own FSP from its own copy of this mint, in exchange for points it accepts. Accepting an FSP payment has to be about as fast as checking a signature.

## Status

Not started. This file records plans for the foundation. The wallet side has placeholders: `ecash-wallet` stores FSP notes (`Kind::Fsp`) and has `get_fsp` and `spend_fsp` entry points that return "not built".

## Units

FSP uses the same unit as ESP: 10^-12 USD per unit. ESP to FSP is 1:1, so a service issues one FSP unit for each ESP unit it transfers. (1 PSP is worth 1 USD, which is 10^12 ESP.) Services price in whole units. There is no smaller unit and no fractional FSP.

- Amounts are `u64`, so one note is worth at most about 1.8e19 units, which is about $18.4 million.
- Every sum is computed in `u128` or with checked arithmetic, so an overflow is an error and never wraps. Tests cover the limits: `u64::MAX` amounts, sums that exceed one note, and a price times a quantity near the limit.
- The unit constants and checked helpers are in esp-mint (`src/units.rs`). Share them with this crate when it is built.

## Accepting ESP

fsp-mint issues FSP in exchange for ESP only. PSP cannot be traded directly for FSP.

- **From ESP.** The service transfers the notes at the ESP mint (see the esp-mint README) and issues FSP once the transfer succeeds. The transfer is an AIR message, and its cost is paid in AIR currency and charged by AIR messaging.
- **Needs.** A client in fsp-mint for the ESP mint's transfer request. The wallet's `transfer_esp` entry point is the other end: an app hands the service its ESP notes, and the service moves them to notes only it controls before it issues FSP.

## Fees

Each service sets its own FSP fees in its own FSP mint. The mints charge no fee for the exchange or transfer that leads to FSP.

## Left to do

- **Hot-path performance.** One global mutex and a disk write per spend cannot reach a thousand requests a second. The plan is an in-memory spent set with batched writes. A crash can lose an unflushed batch, which opens a small replay window.
- **Accepting ESP**, as above.
- **Local store is for development.** The mint's database (sled) is on local disk only while the other pieces are built. It will eventually be stored in the AIR. Until then: only one process can open the mint, so there is one instance and an outage while it restarts; a live `db/` must not be copied as a backup (stop the process first, or the copy may be inconsistent); and the data directory is on local disk, so losing it loses the keys and the state.
- **Mint keystore (deferred to the AIR).** The mint's keys move into the AIR. How they are protected there is not decided.
- **API over the AIR (deferred).** All mint API calls will travel over the AIR. HTTP is the development transport.
