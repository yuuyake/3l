use clap::Parser;
use reqwest::{header, Client, StatusCode};
use serde::Deserialize;
use std::{
    io::Write,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::{sync::mpsc, time::sleep};

const BULK: &str = "https://api.minecraftservices.com/minecraft/profile/lookup/bulk/byname";
const BATCH: usize = 10; // Mojang bulk endpoint max

#[derive(Parser)]
#[command(name = "3l-cli", about = "Find free Minecraft usernames, verify on NameMC, save to file")]
struct Args {
    /// Name length
    #[arg(short, long, default_value_t = 3)]
    len: usize,
    /// Character set to brute force
    #[arg(short, long, default_value = "abcdefghijklmnopqrstuvwxyz")]
    chars: String,
    /// Parallel Mojang workers (each request checks 10 names)
    #[arg(short, long, default_value_t = 4)]
    workers: usize,
    /// Delay per worker between Mojang requests (ms)
    #[arg(short, long, default_value_t = 1000)]
    delay_ms: u64,
    /// Resume from this candidate index (shown in progress output)
    #[arg(short, long, default_value_t = 0)]
    skip: usize,
    /// Results file (appended)
    #[arg(short, long, default_value = "3l-results.txt")]
    out: String,
    /// NameMC cf_clearance cookie value (from your browser)
    #[arg(long)]
    cookie: Option<String>,
    /// User-Agent (must match the browser the cookie came from)
    #[arg(
        long,
        default_value = "Mozilla/5.0 (X11; Linux x86_64; rv:130.0) Gecko/20100101 Firefox/130.0"
    )]
    ua: String,
    /// Delay between NameMC checks (ms)
    #[arg(long, default_value_t = 3000)]
    namemc_delay_ms: u64,
    /// Check one name on NameMC, print the raw classification and exit
    #[arg(long)]
    probe: Option<String>,
}

#[derive(Deserialize)]
struct Profile {
    name: String,
}

enum B {
    Found(Vec<String>),
    Limited,
    Bad(StatusCode),
}

#[derive(Debug)]
enum Nm {
    Available,
    Later(Option<String>),
    Taken,
    Unverified(String),
}

async fn bulk(c: &Client, names: &[String]) -> Result<B, reqwest::Error> {
    let r = c.post(BULK).json(names).send().await?;
    Ok(match r.status() {
        StatusCode::OK => B::Found(
            r.json::<Vec<Profile>>()
                .await?
                .into_iter()
                .map(|p| p.name.to_lowercase())
                .collect(),
        ),
        StatusCode::TOO_MANY_REQUESTS => B::Limited,
        s => B::Bad(s),
    })
}

async fn fetch(
    c: &Client,
    name: &str,
    cookie: &Option<String>,
) -> Result<(StatusCode, String), reqwest::Error> {
    let mut req = c.get(format!("https://namemc.com/search?q={name}"));
    if let Some(ck) = cookie {
        req = req.header(header::COOKIE, format!("cf_clearance={ck}"));
    }
    let r = req.send().await?;
    let s = r.status();
    Ok((s, r.text().await.unwrap_or_default()))
}

fn later_date(b: &str) -> Option<String> {
    let rest = &b[b.find("available later")?..];
    let j = rest.find("datetime=\"")? + 10;
    let k = rest[j..].find('"')?;
    Some(rest[j..j + k].to_string())
}

// Heuristic: NameMC markup is not a public contract. Validate with --probe.
fn classify(s: StatusCode, body: &str) -> Nm {
    let b = body.to_lowercase();
    if s == StatusCode::FORBIDDEN
        || s == StatusCode::SERVICE_UNAVAILABLE
        || b.contains("just a moment")
        || b.contains("cf-challenge")
    {
        return Nm::Unverified("cloudflare challenge".into());
    }
    if !s.is_success() {
        return Nm::Unverified(format!("HTTP {s}"));
    }
    if b.contains("available later") {
        Nm::Later(later_date(&b))
    } else if b.contains("unavailable") {
        Nm::Taken
    } else if b.contains("available") {
        Nm::Available
    } else {
        Nm::Unverified("unrecognised page".into())
    }
}

fn name_at(mut i: usize, chars: &[char], len: usize) -> String {
    let mut buf = vec![chars[0]; len];
    for p in (0..len).rev() {
        buf[p] = chars[i % chars.len()];
        i /= chars.len();
    }
    buf.into_iter().collect()
}

#[tokio::main]
async fn main() {
    let a = Args::parse();
    let client = Client::builder()
        .user_agent(a.ua.clone())
        .timeout(Duration::from_secs(15))
        .tcp_nodelay(true)
        .build()
        .unwrap();

    if let Some(n) = &a.probe {
        match fetch(&client, n, &a.cookie).await {
            Ok((s, body)) => {
                println!("http {s}\n{:?}", classify(s, &body));
                let l = body.to_lowercase();
                if let Some(i) = l.find("available") {
                    let from = i.saturating_sub(120);
                    println!("...{}...", l.get(from..(i + 200).min(l.len())).unwrap_or(""));
                }
            }
            Err(e) => println!("error: {e}"),
        }
        return;
    }

    let chars: Arc<Vec<char>> = Arc::new(a.chars.chars().collect());
    let total = chars.len().pow(a.len as u32);
    let next = Arc::new(AtomicUsize::new(a.skip / BATCH));
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&a.out)
        .expect("cannot open output file");
    let (c2, cookie) = (client.clone(), a.cookie.clone());
    let nm_delay = Duration::from_millis(a.namemc_delay_ms);

    // Single NameMC checker, throttled, so Mojang scanning never waits on it.
    let checker = tokio::spawn(async move {
        while let Some(n) = rx.recv().await {
            let r = match fetch(&c2, &n, &cookie).await {
                Ok((s, body)) => classify(s, &body),
                Err(e) => Nm::Unverified(e.to_string()),
            };
            let line = match r {
                Nm::Available => format!("{n}\tavailable"),
                Nm::Later(d) => format!("{n}\tcooldown\t{}", d.unwrap_or_default()),
                Nm::Unverified(why) => format!("{n}\tunverified\t{why}"),
                Nm::Taken => {
                    eprintln!("{n}: free on Mojang, unavailable on NameMC (blocked or held)");
                    sleep(nm_delay).await;
                    continue;
                }
            };
            println!("{line}");
            writeln!(file, "{line}").ok();
            file.flush().ok();
            sleep(nm_delay).await;
        }
    });

    eprintln!("{total} candidates, {BATCH} per request, starting at {}", a.skip);

    let mut tasks = Vec::new();
    for _ in 0..a.workers {
        let (chars, next, client, tx) = (chars.clone(), next.clone(), client.clone(), tx.clone());
        let (len, delay) = (a.len, Duration::from_millis(a.delay_ms));
        tasks.push(tokio::spawn(async move {
            loop {
                let b = next.fetch_add(1, Ordering::Relaxed);
                let start = b * BATCH;
                if start >= total {
                    break;
                }
                let end = (start + BATCH).min(total);
                let names: Vec<String> = (start..end).map(|i| name_at(i, &chars, len)).collect();
                let found = loop {
                    match bulk(&client, &names).await {
                        Ok(B::Found(f)) => break Some(f),
                        Ok(B::Limited) => {
                            eprintln!("rate limited at index {start}, sleeping 30s");
                            sleep(Duration::from_secs(30)).await;
                        }
                        Ok(B::Bad(s)) => {
                            eprintln!("batch at {start}: HTTP {s}, skipping");
                            break None;
                        }
                        Err(e) => {
                            eprintln!("net error at {start}: {e}, retrying");
                            sleep(Duration::from_secs(3)).await;
                        }
                    }
                };
                if let Some(f) = found {
                    for n in &names {
                        if !f.contains(&n.to_lowercase()) {
                            let _ = tx.send(n.clone());
                        }
                    }
                }
                if b % 50 == 0 {
                    eprintln!("progress {start}/{total}");
                }
                sleep(delay).await;
            }
        }));
    }
    drop(tx);
    for t in tasks {
        let _ = t.await;
    }
    let _ = checker.await;
    eprintln!("done");
}
