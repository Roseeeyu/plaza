//! End-to-end tests: a real server on random ports, driven over plain
//! blocking sockets from the test thread.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use plaza::{Config, serve};

fn config() -> Config {
    Config {
        width: 8,
        height: 8,
        cooldown_ms: 0,
        lag_budget: 64,
        data_dir: None,
        snapshot_every: 1000,
    }
}

/// Starts a server on its own runtime thread. Returns the web and raw TCP
/// addresses.
fn start_both(config: Config) -> (SocketAddr, SocketAddr) {
    let bind = || {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        listener
    };
    let (web, tcp) = (bind(), bind());
    let addrs = (web.local_addr().unwrap(), tcp.local_addr().unwrap());
    thread::spawn(move || {
        let runtime = tokio::runtime::Runtime::new().unwrap();
        runtime.block_on(async {
            let web = tokio::net::TcpListener::from_std(web).unwrap();
            let tcp = tokio::net::TcpListener::from_std(tcp).unwrap();
            serve(web, Some(tcp), config, std::future::pending()).await
        })
    });
    addrs
}

fn start(config: Config) -> SocketAddr {
    start_both(config).1
}

struct Client {
    reader: BufReader<TcpStream>,
    writer: TcpStream,
}

impl Client {
    fn connect(addr: SocketAddr) -> Client {
        let stream = TcpStream::connect(addr).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        Client {
            writer: stream.try_clone().unwrap(),
            reader: BufReader::new(stream),
        }
    }

    fn send(&mut self, line: &str) {
        writeln!(self.writer, "{line}").unwrap();
    }

    fn recv(&mut self) -> String {
        let mut line = String::new();
        self.reader.read_line(&mut line).unwrap();
        line.trim_end().to_string()
    }

    fn ask(&mut self, line: &str) -> String {
        self.send(line);
        self.recv()
    }
}

#[test]
fn painting_requires_a_name_and_records_the_author() {
    let addr = start(config());
    let mut c = Client::connect(addr);
    assert!(c.ask("paint 1 1 red").starts_with("error:"));
    assert_eq!(c.ask("hello ann"), "welcome ann");
    assert_eq!(c.ask("paint 1 1 red"), "painted 1");
    assert_eq!(c.ask("read 1 1"), "pixel 1 1 red ann");
    assert_eq!(c.ask("read 0 0"), "pixel 0 0 white -");
    assert!(c.ask("paint 8 0 red").starts_with("error:"));
}

#[test]
fn subscribers_get_diffs_with_authors() {
    let addr = start(config());
    let mut sub = Client::connect(addr);
    let mut ann = Client::connect(addr);
    assert_eq!(sub.ask("subscribe 0 0 1 8"), "subscribed 0");
    ann.ask("hello ann");
    assert_eq!(ann.ask("stamp 0 0 2 2 5555"), "stamped 1");
    // Only the cells inside the subscribed column arrive.
    assert_eq!(sub.recv(), "diff 1 0 0 red ann");
    assert_eq!(sub.recv(), "diff 1 0 1 red ann");
    assert_eq!(sub.ask("unsubscribe"), "unsubscribed");
}

#[test]
fn cooldown_follows_the_name_across_connections() {
    let addr = start(Config {
        cooldown_ms: 60_000,
        ..config()
    });
    let mut tab1 = Client::connect(addr);
    let mut tab2 = Client::connect(addr);
    let mut bob = Client::connect(addr);
    tab1.ask("hello ann");
    tab2.ask("hello ann");
    bob.ask("hello bob");
    assert_eq!(tab1.ask("paint 0 0 red"), "painted 1");
    assert!(tab2.ask("paint 1 0 red").starts_with("cooldown "));
    assert_eq!(bob.ask("paint 2 0 blue"), "painted 2");
}

#[test]
fn slow_subscribers_are_lagged() {
    let addr = start(Config {
        lag_budget: 3,
        ..config()
    });
    let mut sub = Client::connect(addr);
    let mut ann = Client::connect(addr);
    assert_eq!(sub.ask("subscribe 0 0 8 8"), "subscribed 0");
    ann.ask("hello ann");
    assert_eq!(ann.ask("stamp 0 0 2 2 5555"), "stamped 1");
    assert_eq!(sub.recv(), "lagged");
}

#[test]
fn canvas_survives_a_restart() {
    let dir: PathBuf =
        std::env::temp_dir().join(format!("plaza-it-{}-restart", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    let persistent = || Config {
        data_dir: Some(dir.clone()),
        ..config()
    };

    let mut c = Client::connect(start(persistent()));
    c.ask("hello ann");
    assert_eq!(c.ask("stamp 0 0 2 1 5c"), "stamped 1");
    // `stamped` is sent before the commit is fsynced; give the persister a
    // moment before "crashing" by starting a second server on the same data.
    thread::sleep(Duration::from_millis(200));

    // The saved canvas keeps its own size whatever the new flags say.
    let mut c = Client::connect(start(Config {
        width: 64,
        height: 64,
        ..persistent()
    }));
    assert_eq!(c.ask("read 1 0"), "pixel 1 0 blue ann");
    assert!(c.ask("read 8 0").starts_with("error:"));
    c.send("snapshot 0 0 3 1");
    assert_eq!(c.recv(), "snapshot 1 0 0 3 1");
    assert_eq!(c.recv(), "5c0");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn serves_the_frontend_and_health_check() {
    let (web, _) = start_both(config());
    let get = |path: &str| {
        let mut stream = TcpStream::connect(web).unwrap();
        write!(
            stream,
            "GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"
        )
        .unwrap();
        let mut response = String::new();
        stream.read_to_string(&mut response).unwrap();
        response
    };
    let index = get("/");
    assert!(index.starts_with("HTTP/1.1 200"));
    assert!(index.contains("<title>Plaza</title>"));
    assert!(get("/healthz").ends_with("ok"));
    assert!(get("/nope").starts_with("HTTP/1.1 404"));
}
