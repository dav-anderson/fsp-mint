# fsp-mint

FSP is the fast point. Each JAMJAR service issues its own FSP from its own copy of this mint, in exchange for points it accepts. Accepting an FSP payment has to be about as fast as checking a signature.

## Status

Not started. This file records plans for the foundation.

## Accepting PSP as well as ESP

When the ESP issuer is offline, an app can trade PSP directly for a service's FSP, so fsp-mint accepts both.

- **From ESP.** The service transfers the notes at the ESP mint (see the esp-mint README) and issues FSP once the transfer succeeds.
- **From PSP.** The same pattern at the PSP mint. The app hands over the notes and their spend secrets. The service sends them through the PSP's `/swap` with fresh outputs of its own, so only the service controls the result. The difference between inputs and outputs is the fee. The service issues FSP once the swap succeeds.
- **Redeeming PSP later.** PSP-funded FSP has no encumbrance period. The service later redeems the PSP it holds at the PSP mint and is paid in bitcoin. The PSP has no payout path yet, because the core `melt` only burns.
- **Needs.** The PSP's HTTP API running in-process, and a client in fsp-mint for each upstream mint.

## Left to do

- **Hot-path performance.** One global mutex and a disk write per spend cannot reach a thousand requests a second. The plan is an in-memory spent set with batched writes. A crash can lose an unflushed batch, which opens a small replay window.
- **Accepting ESP and PSP**, as above.
