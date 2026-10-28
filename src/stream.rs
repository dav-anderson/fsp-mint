//! The key set stream: the FSP mint publishes its live keysets on AIR so that
//! every wallet can see the same keys (ROADMAP item 31, finding P2-04, the mint
//! fingerprinting attack). The cells, the record and the publisher are
//! `sp_wire::air_request::stream`; this module is the mint's side of it.
//!
//! **What is published.** One record for each keyset the mint makes, in epoch
//! order, written to the stream of the service's DID: the epoch and the key for
//! every power of two. The wallet accepts a key only if it is the key for its
//! amount in the last two records, the current and the previous keyset.
//!
//! **When.** `serve` runs a pass every `sp_wire::air_request::stream::TICK_SECS`
//! (5) seconds on a thread of its own, after asking the spender for the live
//! keysets (which also makes a rotation that is due). A pass does nothing while
//! the live keysets are the ones it last published; at start, and after a
//! rotation, it reads the end of the stream and the last two records and writes
//! the keysets that are not among them.
//!
//! **A restart** keeps nothing: the stream is the memory. A failure is printed
//! and tried again after `RETRY_SECS` (30) seconds; it never stops the mint.
//!
//! **The size.** A record is 2,636 bytes; the stream grows by one record for
//! each rotation. Rotation is off while `rotation.activation_hours` is 0, and
//! `rotation.period_hours` is 1 to 876000 (default 8760, a year), so the worst
//! case is one record an hour: about 8,760 a year, 23 MB. AIR has no delete, so
//! the stream is never shortened; it is the stream of a mint that the operator
//! set to rotate that fast.

use sp_wire::air_request::stream::{KeysetPublisher, KeysetRecord};

use crate::spend::Spender;

/// The live keysets as stream records, oldest first (the previous, if any, then
/// the current), after a rotation that is due at `now`.
pub fn live_records(spender: &Spender, now: u64) -> Result<Vec<KeysetRecord>, String> {
    Ok(spender
        .keysets(now)?
        .iter()
        .map(|k| k.stream_record())
        .collect())
}

/// One pass: publish the live keysets if the stream does not hold them yet.
/// `Ok(Some(text))` says what was written.
pub fn pass(
    publisher: &mut KeysetPublisher,
    spender: &Spender,
    now: u64,
) -> Result<Option<String>, String> {
    let live = live_records(spender, now)?;
    Ok(publisher.pass(&live)?.and_then(|done| {
        (!done.written.is_empty()).then(|| {
            format!(
                "key set stream: wrote record(s) {:?}; the stream has {} record(s)",
                done.written, done.end
            )
        })
    }))
}
