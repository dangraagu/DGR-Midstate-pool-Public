//! Stratum client driver.
//!
//! One supervisor loop: connect → authorize → run a session; on any failure,
//! sleep `reconnect_backoff` and retry (never strands a rig). Within a session a
//! reader thread parses inbound lines (jobs + acks) into shared state, while this
//! thread runs the mining loop (the SOLE socket writer: authorize + submits).
//! A read timeout doubles as the half-open/stalled-job watchdog.

use crate::backend::Backend;
use crate::stratum::{classify, Event, Incoming, RpcRequest, Job, ID_AUTHORIZE, ID_SUBMIT};
use crate::target::share_target;
use anyhow::{anyhow, Context, Result};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

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
}

/// Supervisor: run forever (or until `duration`), reconnecting on any error.
pub fn run(cfg: ClientConfig, backend: &mut dyn Backend, duration: Option<Duration>) -> Result<()> {
    let start = Instant::now();
    let target = share_target(cfg.share_bits);
    let addr_preview: String = cfg.address.chars().take(16).collect();
    println!(
        "[miner] pool={}:{} backend={} addr={}... share_bits={}",
        cfg.host,
        cfg.port,
        backend.name(),
        addr_preview,
        cfg.share_bits
    );
    loop {
        if let Some(d) = duration {
            if start.elapsed() >= d {
                println!("[miner] duration reached, stopping");
                return Ok(());
            }
        }
        match session(&cfg, &target, backend, start, duration) {
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
) -> Result<()> {
    let addr = format!("{}:{}", cfg.host, cfg.port);
    let stream = TcpStream::connect(&addr).with_context(|| format!("connect {addr}"))?;
    stream.set_read_timeout(Some(cfg.read_timeout))?;
    let mut writer = stream.try_clone()?;
    println!("[miner] connected to {addr}");

    let shared = Arc::new(Shared {
        job: Mutex::new(None),
        epoch: AtomicU64::new(0),
        stop: AtomicBool::new(false),
        submitted: AtomicU64::new(0),
        accepted: AtomicU64::new(0),
        rejected: AtomicU64::new(0),
    });

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
    let mut last_epoch = u64::MAX;
    let mut cursor: u64 = 0;
    let mut last_hb = Instant::now();
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
            if epoch != last_epoch {
                last_epoch = epoch;
                cursor = 0;
            }

            let batch = backend.suggested_batch();
            let found = backend.search(&job.midstate, target, cursor, batch)?;
            cursor = cursor.wrapping_add(batch as u64);

            for f in found {
                if shared.epoch.load(Ordering::Acquire) != epoch {
                    break; // job rolled; pool would silently drop a stale submit
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
                last_hb = Instant::now();
                println!(
                    "[miner] hb: submitted={} accepted={} rejected={}",
                    shared.submitted.load(Ordering::Relaxed),
                    shared.accepted.load(Ordering::Relaxed),
                    shared.rejected.load(Ordering::Relaxed)
                );
            }
        }
    })();

    shared.stop.store(true, Ordering::Relaxed);
    let _ = writer.shutdown(std::net::Shutdown::Both); // unblock the reader
    let _ = reader_handle.join();
    println!(
        "[miner] FINAL: submitted={} accepted={} rejected={}",
        shared.submitted.load(Ordering::Relaxed),
        shared.accepted.load(Ordering::Relaxed),
        shared.rejected.load(Ordering::Relaxed)
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
