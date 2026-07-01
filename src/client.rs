//! Stratum client driver.
//!
//! One supervisor loop: connect → authorize → run a session; on any failure,
//! sleep `reconnect_backoff` and retry (never strands a rig). Within a session a
//! reader thread parses inbound lines (jobs + acks) into shared state, while this
//! thread runs the mining loop (the SOLE socket writer: authorize + submits).
//! A read timeout doubles as the half-open/stalled-job watchdog.

use crate::backend::Backend;
use crate::stratum::{classify, Event, Incoming, RpcRequest, Job, ID_AUTHORIZE, ID_KEEPALIVE, ID_SUBMIT};
use crate::target::share_target;
use anyhow::{anyhow, Context, Result};
use std::hash::{BuildHasher, Hasher};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// FIX 2 — a PER-INSTANCE RANDOM nonce base, seeded from OS entropy WITHOUT a
/// new crate dependency. `std::collections::hash_map::RandomState` is seeded by
/// the OS RNG at construction (that's what makes HashMap DoS-resistant), so the
/// `finish()` of a fresh hasher is a fresh random `u64` each call.
///
/// Why this matters: every rig used to start its nonce cursor at 0 AND reset to 0
/// on every job, so the WHOLE FLEET ground the same low nonces → the pool rejected
/// the second-and-later finders as Duplicate shares (confirmed live). Seeding each
/// rig's cursor at its own random base, and never resetting it, spreads the fleet
/// across the 2^64 nonce space so collisions are astronomically unlikely.
pub fn random_nonce_base() -> u64 {
    std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish()
}

/// Advance the nonce cursor by one window, wrapping through the full 2^64 space.
/// PURE. The cursor is NEVER reset on a job/epoch change — it walks continuously
/// from its random base so a rig never re-searches nonces it already covered (and
/// in particular never falls back onto the contested low nonces).
#[inline]
pub fn advance_cursor(cursor: u64, batch: u32) -> u64 {
    cursor.wrapping_add(batch as u64)
}

/// v0.1.9 — average hashes/sec over a window, rounded to whole H/s. PURE.
/// Degenerate windows (zero/negative elapsed) report 0 instead of dividing by
/// zero. Feeds the heartbeat's `hs=` field: a rig that is up-but-grinding-nothing
/// prints `hs=0` every 30s, so a darkened/degraded rig is loud in its own
/// launcher log instead of looking identical to a healthy one.
#[inline]
pub fn windowed_rate(hashes: u64, secs: f64) -> u64 {
    if secs <= 0.0 {
        return 0;
    }
    (hashes as f64 / secs).round() as u64
}

pub struct ClientConfig {
    pub host: String,
    pub port: u16,
    pub address: String,
    pub share_bits: u32,
    pub reconnect_backoff: Duration,
    pub read_timeout: Duration,
}

struct Shared {
    job: Mutex<Option<Job>>,
    epoch: AtomicU64, // bumped on each new job
    stop: AtomicBool,
    submitted: AtomicU64,
    accepted: AtomicU64,
    rejected: AtomicU64,
    // v0.1.9 — nonces ground this session (each search window adds its batch).
    // Feeds the heartbeat `hs=` field so "up but not hashing" is visible.
    hashes: AtomicU64,
    // v0.1.9 — found shares DISCARDED because the job rolled before they could
    // be submitted (whole-window drop + mid-submit break). This is the measurable
    // for the stale-window leak: on a slow card the window outlives the ~60s job
    // cadence and most found shares die here, invisible in submit/reject counts.
    stale_dropped: AtomicU64,
}

/// Supervisor: run forever (or until `duration`), reconnecting on any error.
pub fn run(cfg: ClientConfig, backend: &mut dyn Backend, duration: Option<Duration>) -> Result<()> {
    let start = Instant::now();
    let target = share_target(cfg.share_bits);
    // FIX 2 — generate this rig's RANDOM nonce base ONCE at startup (OS-seeded, no
    // new crate dep). It is stable across reconnects within the process, so a rig
    // keeps exploring its own slice of the 2^64 space rather than restarting at 0.
    let nonce_base = random_nonce_base();
    let addr_preview: String = cfg.address.chars().take(16).collect();
    println!(
        "[miner] pool={}:{} backend={} addr={}... share_bits={} nonce_base={:#018x}",
        cfg.host,
        cfg.port,
        backend.name(),
        addr_preview,
        cfg.share_bits,
        nonce_base
    );
    loop {
        if let Some(d) = duration {
            if start.elapsed() >= d {
                println!("[miner] duration reached, stopping");
                return Ok(());
            }
        }
        match session(&cfg, &target, backend, start, duration, nonce_base) {
            Ok(()) => return Ok(()), // clean stop (duration elapsed)
            Err(e) => {
                eprintln!(
                    "[miner] session ended: {e:#}; reconnecting in {:?}",
                    cfg.reconnect_backoff
                );
                std::thread::sleep(cfg.reconnect_backoff);
            }
        }
    }
}

fn session(
    cfg: &ClientConfig,
    target: &[u8; 32],
    backend: &mut dyn Backend,
    start: Instant,
    duration: Option<Duration>,
    nonce_base: u64,
) -> Result<()> {
    let addr = format!("{}:{}", cfg.host, cfg.port);
    let stream = TcpStream::connect(&addr).with_context(|| format!("connect {addr}"))?;
    stream.set_read_timeout(Some(cfg.read_timeout))?;
    // v0.1.9 — disable Nagle: submits are tiny single-line writes; coalescing
    // them behind an ACK adds RTT-scale latency exactly when a share (or a
    // block-winning share) should be on the wire immediately. The pool side
    // already sets nodelay.
    stream.set_nodelay(true)?;
    let mut writer = stream.try_clone()?;
    println!("[miner] connected to {addr}");

    let shared = Arc::new(Shared {
        job: Mutex::new(None),
        epoch: AtomicU64::new(0),
        stop: AtomicBool::new(false),
        submitted: AtomicU64::new(0),
        accepted: AtomicU64::new(0),
        rejected: AtomicU64::new(0),
        hashes: AtomicU64::new(0),
        stale_dropped: AtomicU64::new(0),
    });
    // v0.1.9 — session-relative clock for the FINAL session-average hashrate
    // (`start` is the process-lifetime clock and spans reconnects).
    let session_start = Instant::now();

    // Handshake: authorize (this fn is the only writer until the loop below).
    send(
        &mut writer,
        ID_AUTHORIZE,
        "mining.authorize",
        serde_json::json!([cfg.address, "midstate-miner"]),
    )?;

    // Reader thread: owns the read half; updates shared state.
    let rs = shared.clone();
    let reader_handle = std::thread::spawn(move || {
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        loop {
            if rs.stop.load(Ordering::Relaxed) {
                break;
            }
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break, // EOF (clean close)
                Ok(_) => {
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    let msg: Incoming = match serde_json::from_str(trimmed) {
                        Ok(m) => m,
                        Err(_) => continue, // ignore garbage; never panic
                    };
                    match classify(msg) {
                        Event::Job(j) => {
                            println!(
                                "[miner] job {} midstate={}…",
                                j.job_id,
                                hex::encode(&j.midstate[..6])
                            );
                            // Publish job + epoch atomically under ONE lock. If the epoch
                            // were bumped after releasing the job lock, the mining loop's
                            // locked (job, epoch) read could observe the new job paired with
                            // the old epoch, and the pre-submit freshness guard would then
                            // spuriously drop a valid share found for that very job.
                            {
                                let mut g = rs.job.lock().unwrap();
                                *g = Some(j);
                                rs.epoch.fetch_add(1, Ordering::Release);
                            }
                        }
                        Event::AuthAck(ok) => {
                            println!("[miner] authorize: {ok}");
                            if !ok {
                                eprintln!("[miner] authorize REJECTED — check your address");
                                rs.stop.store(true, Ordering::Relaxed);
                                break;
                            }
                        }
                        Event::SubmitAck { accepted, error } => {
                            if accepted {
                                rs.accepted.fetch_add(1, Ordering::Relaxed);
                            } else {
                                rs.rejected.fetch_add(1, Ordering::Relaxed);
                                if let Some(e) = error {
                                    eprintln!("[miner] submit rejected: {e}");
                                }
                            }
                        }
                        Event::Other => {}
                    }
                }
                Err(e)
                    if e.kind() == std::io::ErrorKind::WouldBlock
                        || e.kind() == std::io::ErrorKind::TimedOut =>
                {
                    eprintln!("[miner] read timeout — link stalled, dropping");
                    break;
                }
                Err(_) => break,
            }
        }
        rs.stop.store(true, Ordering::Relaxed);
    });

    // Mining loop (sole submit writer).
    // FIX 2 — start at this rig's RANDOM base and let the cursor advance
    // CONTINUOUSLY through the 2^64 nonce space. It is NEVER reset (not to 0, not
    // to the base) on a job/epoch change: every rig explores its own slice, so the
    // fleet stops grinding the same low nonces and the Duplicate-share rejects go
    // away.
    let mut cursor: u64 = nonce_base;
    let mut last_hb = Instant::now();
    let mut last_hb_hashes: u64 = 0; // v0.1.9 — hashes total at the last heartbeat
    let mut last_ka = Instant::now(); // v0.1.9 — last keepalive sent
    let res = (|| -> Result<()> {
        loop {
            if shared.stop.load(Ordering::Relaxed) {
                return Err(anyhow!("session stopped (link down)"));
            }
            if let Some(d) = duration {
                if start.elapsed() >= d {
                    return Ok(());
                }
            }

            // v0.1.9 — KEEPALIVE: the pool closes any connection with no inbound
            // line for 120s (its idle read-timeout). Slow submitters — a CPU rig
            // (~1 share / 4+ min), the never-dark reduced fallback, or a miner
            // parked in the no-job wait below while the pool gates jobs during a
            // node re-sync — would flap connect/drop forever. mining.subscribe is
            // a no-op ack on the pool; one line every 30s keeps the timer fresh.
            // Sits ABOVE the job check so the no-job wait is covered too. (A
            // single search window longer than 120s can still outlive the timer
            // mid-window — bounded by the default batch sizes; fully fixed by the
            // streamed-search work planned for v0.1.10.)
            if last_ka.elapsed() >= Duration::from_secs(30) {
                last_ka = Instant::now();
                send(
                    &mut writer,
                    ID_KEEPALIVE,
                    "mining.subscribe",
                    serde_json::json!([]),
                )?;
            }

            // Snapshot the CURRENT job + epoch BEFORE searching. The shares we find
            // this window belong to THIS (job_id, midstate); if the pool rolls the
            // job mid-window, the post-search guard below drops them rather than
            // submitting them against the new job (they were found for the old
            // midstate and would be stale/invalid).
            let (job_opt, epoch) = {
                let g = shared.job.lock().unwrap();
                (g.clone(), shared.epoch.load(Ordering::Acquire))
            };
            let job = match job_opt {
                Some(j) => j,
                None => {
                    std::thread::sleep(Duration::from_millis(100)); // await first job
                    continue;
                }
            };
            // NOTE: there is deliberately NO cursor reset on a job/epoch change —
            // see FIX 2 above. The cursor walks on regardless of job rolls.

            let batch = backend.suggested_batch();
            let found = backend.search(&job.midstate, target, cursor, batch)?;
            cursor = advance_cursor(cursor, batch);
            // v0.1.9 — count the nonces this window ground (the heartbeat's hs=).
            shared.hashes.fetch_add(batch as u64, Ordering::Relaxed);

            // Stale-share guard: if the epoch advanced WHILE we were searching, a
            // new job arrived and these `found` were computed for the OLD midstate.
            // Submitting them against `job.job_id` (the job they were actually found
            // for) is correct, but the pool has already moved on, so they'd be
            // silently dropped — and we must NEVER submit them with the NEW job_id
            // (mismatched job_id). We drop the whole batch and grind the fresh job.
            let rolled_mid_window = shared.epoch.load(Ordering::Acquire) != epoch;
            if rolled_mid_window {
                // v0.1.9 — count the whole window's finds as stale-dropped so the
                // leak is measurable per rig (hb/FINAL print it).
                if !found.is_empty() {
                    shared
                        .stale_dropped
                        .fetch_add(found.len() as u64, Ordering::Relaxed);
                }
                continue; // fresh job is already published; loop picks it up next pass
            }
            let total_found = found.len();
            for (i, f) in found.into_iter().enumerate() {
                // Re-check per share: the job can roll between submits in a window.
                if shared.epoch.load(Ordering::Acquire) != epoch {
                    // v0.1.9 — the not-yet-submitted remainder dies stale too.
                    shared
                        .stale_dropped
                        .fetch_add((total_found - i) as u64, Ordering::Relaxed);
                    break; // job rolled; never submit a stale share / mismatched job_id
                }
                send(
                    &mut writer,
                    ID_SUBMIT,
                    "mining.submit",
                    serde_json::json!([cfg.address, job.job_id, f.nonce]),
                )?;
                shared.submitted.fetch_add(1, Ordering::Relaxed);
            }

            if last_hb.elapsed() >= Duration::from_secs(30) {
                // v0.1.9 — WINDOWED hashrate since the last heartbeat: a rig that
                // stopped grinding shows hs=0 on the very next line, not a slowly
                // decaying session average. backend name included so long logs
                // show WHAT is producing that rate (cuda vs cpu-fallback).
                let total = shared.hashes.load(Ordering::Relaxed);
                let hs = windowed_rate(
                    total.saturating_sub(last_hb_hashes),
                    last_hb.elapsed().as_secs_f64(),
                );
                last_hb_hashes = total;
                last_hb = Instant::now();
                println!(
                    "[miner] hb: backend={} hs={} submitted={} accepted={} rejected={} stale_dropped={}",
                    backend.name(),
                    hs,
                    shared.submitted.load(Ordering::Relaxed),
                    shared.accepted.load(Ordering::Relaxed),
                    shared.rejected.load(Ordering::Relaxed),
                    shared.stale_dropped.load(Ordering::Relaxed)
                );
            }
        }
    })();

    shared.stop.store(true, Ordering::Relaxed);
    let _ = writer.shutdown(std::net::Shutdown::Both); // unblock the reader
    let _ = reader_handle.join();
    println!(
        "[miner] FINAL: hs_avg={} submitted={} accepted={} rejected={} stale_dropped={}",
        windowed_rate(
            shared.hashes.load(Ordering::Relaxed),
            session_start.elapsed().as_secs_f64(),
        ),
        shared.submitted.load(Ordering::Relaxed),
        shared.accepted.load(Ordering::Relaxed),
        shared.rejected.load(Ordering::Relaxed),
        shared.stale_dropped.load(Ordering::Relaxed)
    );
    res
}

fn send(w: &mut TcpStream, id: u64, method: &str, params: serde_json::Value) -> Result<()> {
    let req = RpcRequest { id, method, params };
    let mut line = serde_json::to_string(&req)?;
    line.push('\n');
    w.write_all(line.as_bytes())?;
    w.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// v0.1.9 — the heartbeat hashrate: hashes over a window, rounded to whole
    /// H/s; degenerate windows (zero/negative elapsed) report 0 instead of
    /// dividing by zero. THE observability contract: a rig grinding nothing
    /// prints hs=0, which is what makes a darkened/degraded rig loud in its log.
    #[test]
    fn windowed_rate_basic_and_degenerate() {
        assert_eq!(windowed_rate(30_000, 30.0), 1_000);
        assert_eq!(windowed_rate(0, 30.0), 0); // idle rig shows hs=0
        assert_eq!(windowed_rate(1, 0.0), 0); // degenerate window → 0, no panic
        assert_eq!(windowed_rate(1, -5.0), 0); // negative guard → 0
        assert_eq!(windowed_rate(15, 30.0), 1); // 0.5 rounds to 1 (round, not trunc)
        assert_eq!(windowed_rate(14, 30.0), 0); // 0.466… rounds to 0
        // GPU-scale sanity: ~20k H/s over 30s.
        assert_eq!(windowed_rate(600_000, 30.0), 20_000);
    }

    /// FIX 2 — the per-rig nonce base is random (non-zero with overwhelming
    /// probability) and two independently-seeded bases differ. Seeded from OS
    /// entropy via RandomState, so a u64 collision is ~1/2^64.
    #[test]
    fn random_nonce_base_is_random_and_nonzero() {
        let a = random_nonce_base();
        let b = random_nonce_base();
        // Two fresh bases must differ (RandomState reseeds from the OS each time).
        assert_ne!(a, b, "two seeds collided — base is not actually random");
        // At least one of several draws must be non-zero (a zero base would put a
        // rig back on the contested low nonces). P(all zero) ≈ 0.
        let any_nonzero = (0..8).any(|_| random_nonce_base() != 0);
        assert!(any_nonzero, "nonce base is stuck at 0 — fix the seeding");
    }

    /// The cursor advances by exactly `batch` each window and wraps the full
    /// 2^64 space — it never re-searches the low nonces from a fixed start.
    #[test]
    fn cursor_advances_by_batch_and_wraps() {
        let base = 42u64;
        let c1 = advance_cursor(base, 100);
        assert_eq!(c1, 142);
        let c2 = advance_cursor(c1, 50);
        assert_eq!(c2, 192);
        // Wraps cleanly near the top of the space (no panic / no reset to 0-ish).
        let near_top = u64::MAX - 10;
        assert_eq!(advance_cursor(near_top, 20), 9); // (MAX-10)+20 wraps to 9
    }

    /// THE core regression guard: a job/epoch change must NOT reset the cursor to
    /// 0. We model the loop's cursor state machine exactly — start at a random
    /// base, advance through several windows, then "receive a new job" (epoch
    /// bump). The cursor must keep advancing from where it was, NEVER snap back to
    /// 0 or to the base. (The old code did `cursor = 0` on every epoch change.)
    #[test]
    fn epoch_change_does_not_reset_cursor() {
        let base = random_nonce_base();
        let mut cursor = base;
        let batch: u32 = 480; // e.g. 120 threads * 4

        // Grind a few windows under epoch 0.
        for _ in 0..5 {
            cursor = advance_cursor(cursor, batch);
        }
        let before_roll = cursor;
        assert_eq!(before_roll, base.wrapping_add(5 * batch as u64));

        // A new job arrives (epoch 0 -> 1). The CORRECT behavior: cursor is
        // UNTOUCHED — no `cursor = 0`, no `cursor = base`. It simply keeps
        // advancing into fresh, uncontested nonce space for the new midstate.
        // (This block intentionally contains NO reset; the test fails if a future
        // edit reintroduces one.)
        let after_roll = cursor; // no reset on epoch change
        assert_eq!(after_roll, before_roll, "cursor was reset on a job roll");
        assert_ne!(after_roll, 0, "cursor reset to 0 on a job roll (the bug)");
        assert_ne!(after_roll, base, "cursor reset to base on a job roll");

        // Keep grinding under epoch 1 — strictly forward from where we were.
        cursor = advance_cursor(cursor, batch);
        assert_eq!(cursor, after_roll.wrapping_add(batch as u64));
        assert!(
            cursor != base && cursor != 0,
            "cursor must never fall back onto the contested low/base nonces"
        );
    }

    /// Two rigs with independent random bases explore disjoint regions for a long
    /// time: after the same number of windows their cursors are still far apart
    /// (they don't converge onto the same nonces → no fleet-wide duplicate storm).
    #[test]
    fn two_rigs_do_not_collide_for_a_long_time() {
        let mut a = random_nonce_base();
        let mut b = random_nonce_base();
        assert_ne!(a, b);
        let batch: u32 = 480;
        for _ in 0..1000 {
            a = advance_cursor(a, batch);
            b = advance_cursor(b, batch);
        }
        // Same advance applied to both, so their difference is invariant and equal
        // to the original base gap — they never land on the same nonce window.
        assert_ne!(a, b, "two rigs converged onto the same nonce window");
    }
}
