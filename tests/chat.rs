use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

struct Server {
    child: Child,
    address: String,
}

impl Server {
    fn start() -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_smallchat-server"))
            .arg("127.0.0.1:0")
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut output = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        output.read_line(&mut line).unwrap();
        if !line.starts_with("Listening on ") {
            let status = child.wait().unwrap();
            panic!("server could not start ({status}); local TCP binding must be permitted");
        }
        let address = line
            .strip_prefix("Listening on ")
            .unwrap()
            .trim()
            .to_owned();
        // Drain server logging so a full stdout pipe never stalls the event loop.
        std::thread::spawn(move || io_copy_to_sink(&mut output));
        Self { child, address }
    }

    fn connect(&self) -> Peer {
        let stream = TcpStream::connect(&self.address).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut peer = Peer(BufReader::new(stream));
        assert_eq!(
            peer.line(),
            "Welcome to Simple Chat! Use /nick <nick> to set your nick.\n"
        );
        peer
    }
}

fn io_copy_to_sink(reader: &mut impl Read) {
    let _ = std::io::copy(reader, &mut std::io::sink());
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct Peer(BufReader<TcpStream>);

impl Peer {
    fn send(&mut self, data: &[u8]) {
        self.0.get_mut().write_all(data).unwrap();
    }

    fn line(&mut self) -> String {
        let mut line = String::new();
        assert_ne!(self.0.read_line(&mut line).unwrap(), 0, "unexpected EOF");
        line
    }
}

#[test]
fn broadcasts_to_every_other_client_and_handles_commands() {
    let server = Server::start();
    let mut alice = server.connect();
    let mut bob = server.connect();
    let mut carol = server.connect();
    alice.send(b"/nick alice\nhello\n");
    assert_eq!(bob.line(), "alice> hello\n");
    assert_eq!(carol.line(), "alice> hello\n");
    // A direct error response also proves the sender received no chat echo.
    alice.send(b"/unknown\n/nick\n/nick \n");
    assert_eq!(alice.line(), "Unsupported command\n");
    assert_eq!(alice.line(), "Unsupported command\n");
    assert!(alice.line().starts_with("Invalid nickname"));
    bob.send(b"reply\n");
    let reply = alice.line();
    assert!(reply.starts_with("user:"));
    assert!(reply.ends_with("> reply\n"));
    assert_eq!(carol.line(), reply);
}

#[test]
fn handles_fragmented_coalesced_unicode_and_long_messages() {
    let server = Server::start();
    let mut sender = server.connect();
    let mut receiver = server.connect();
    sender.send(b"/ni");
    // A fragmented command must not be executed before the newline arrives.
    sender.send("ck 小明\r\n你好\nsecond\n".as_bytes());
    assert_eq!(receiver.line(), "小明> 你好\n");
    assert_eq!(receiver.line(), "小明> second\n");
    let long = "x".repeat(4096);
    sender.send(long.as_bytes());
    sender.send(b"\n");
    assert_eq!(receiver.line(), format!("小明> {long}\n"));
}

#[test]
fn disconnects_oversized_input_without_affecting_other_clients() {
    let server = Server::start();
    let mut oversized = server.connect();
    let mut sender = server.connect();
    let mut receiver = server.connect();
    oversized.send(&vec![b'x'; 4097]);
    let mut byte = [0];
    match oversized.0.read(&mut byte) {
        Ok(0) => {}
        Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => {}
        result => panic!("oversized client was not disconnected: {result:?}"),
    }
    sender.send(b"/nick survivor\nstill here\n");
    assert_eq!(receiver.line(), "survivor> still here\n");
    let mut newcomer = server.connect();
    newcomer.send(b"/nick new\nconnected\n");
    assert_eq!(receiver.line(), "new> connected\n");
}

#[test]
fn flushes_final_unterminated_message_on_disconnect() {
    let server = Server::start();
    let mut sender = server.connect();
    let mut receiver = server.connect();
    sender.send(b"/nick goodbye\nlast message");
    sender.0.get_ref().shutdown(Shutdown::Write).unwrap();
    assert_eq!(receiver.line(), "goodbye> last message\n");
}

#[test]
fn rust_client_supports_piped_input_and_eof() {
    let server = Server::start();
    let mut receiver = server.connect();
    let port = server.address.rsplit_once(':').unwrap().1;
    let mut client = Command::new(env!("CARGO_BIN_EXE_smallchat-client"))
        .args(["127.0.0.1", port])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    client
        .stdin
        .take()
        .unwrap()
        .write_all(b"/nick rust-client\r\nhello from Rust\nfinal line")
        .unwrap();
    assert_eq!(receiver.line(), "rust-client> hello from Rust\n");
    assert_eq!(receiver.line(), "rust-client> final line\n");
    let output = client.wait_with_output().unwrap();
    assert!(output.status.success(), "{:?}", output.stderr);
    assert!(String::from_utf8(output.stdout)
        .unwrap()
        .contains("Welcome to Simple Chat!"));
}

#[test]
fn binaries_report_help_and_invalid_arguments() {
    for binary in [
        env!("CARGO_BIN_EXE_smallchat-server"),
        env!("CARGO_BIN_EXE_smallchat-client"),
    ] {
        let output = Command::new(binary).arg("--help").output().unwrap();
        assert!(output.status.success());
        assert!(String::from_utf8(output.stdout).unwrap().contains("Usage:"));
    }
    let output = Command::new(env!("CARGO_BIN_EXE_smallchat-client"))
        .args(["localhost", "invalid"])
        .output()
        .unwrap();
    assert!(!output.status.success());
}
