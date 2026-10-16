use smallchat_rs::poll;
use std::collections::HashMap;
use std::env;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::fd::{AsRawFd, RawFd};
use std::process::ExitCode;

const MAX_CLIENTS: usize = 1000;
const MAX_LINE: usize = 4096;
const MAX_PENDING: usize = 64 * 1024;
const WELCOME: &[u8] = b"Welcome to Simple Chat! Use /nick <nick> to set your nick.\n";

struct Client<T = TcpStream> {
    stream: T,
    nick: String,
    input: Vec<u8>,
    output: Vec<u8>,
    written: usize,
    read_closed: bool,
}

impl<T: Read + Write> Client<T> {
    fn queue(&mut self, message: &[u8]) -> io::Result<()> {
        if self.output.len() - self.written + message.len() > MAX_PENDING {
            return Err(io::Error::other("client output buffer is full"));
        }
        // Discard bytes already sent before appending more data.
        self.output.drain(..self.written);
        self.written = 0;
        self.output.extend_from_slice(message);
        Ok(())
    }

    fn flush(&mut self) -> io::Result<()> {
        while self.written < self.output.len() {
            match self.stream.write(&self.output[self.written..]) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(count) => self.written += count,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        self.output.clear();
        self.written = 0;
        Ok(())
    }

    fn read_lines(&mut self) -> io::Result<(Vec<Vec<u8>>, bool)> {
        let mut lines = Vec::new();
        let mut buffer = [0; 1024];
        // Bound work per event so one busy connection cannot monopolize the loop.
        for _ in 0..64 {
            match self.stream.read(&mut buffer) {
                Ok(0) => {
                    // Also accept a final line from clients that close without '\n'.
                    if !self.input.is_empty() {
                        lines.push(std::mem::take(&mut self.input));
                    }
                    return Ok((lines, true));
                }
                Ok(count) => {
                    for &byte in &buffer[..count] {
                        if byte == b'\n' {
                            let mut line = std::mem::take(&mut self.input);
                            if line.last() == Some(&b'\r') {
                                line.pop();
                            }
                            lines.push(line);
                        } else {
                            if self.input.len() == MAX_LINE {
                                return Err(io::Error::new(
                                    io::ErrorKind::InvalidData,
                                    "message exceeds 4096 bytes",
                                ));
                            }
                            self.input.push(byte);
                        }
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            }
        }
        Ok((lines, false))
    }
}

fn handle_line<T: Read + Write>(fd: RawFd, line: Vec<u8>, clients: &mut HashMap<RawFd, Client<T>>) {
    let Some(sender) = clients.get_mut(&fd) else {
        return;
    };
    if line.starts_with(b"/") {
        let reply = if let Some(nick) = line.strip_prefix(b"/nick ") {
            match std::str::from_utf8(nick) {
                Ok(nick)
                    if !nick.trim().is_empty()
                        && nick.len() <= 128
                        && !nick.chars().any(char::is_control) =>
                {
                    sender.nick = nick.to_owned();
                    return;
                }
                _ => {
                    b"Invalid nickname (use 1-128 UTF-8 bytes, no control characters)\n".as_slice()
                }
            }
        } else {
            b"Unsupported command\n".as_slice()
        };
        if sender.queue(reply).is_err() {
            clients.remove(&fd);
        }
        return;
    }

    let mut message = format!("{}> ", sender.nick).into_bytes();
    message.extend_from_slice(&line);
    message.push(b'\n');
    let _ = io::stdout().lock().write_all(&message);
    // A slow or broken recipient is removed without delaying other clients.
    clients.retain(|&recipient, client| recipient == fd || client.queue(&message).is_ok());
}

fn serve(address: &str) -> io::Result<()> {
    let listener = TcpListener::bind(address)?;
    listener.set_nonblocking(true)?;
    println!("Listening on {}", listener.local_addr()?);
    io::stdout().flush()?;
    let mut clients: HashMap<RawFd, Client> = HashMap::new();

    loop {
        let mut events = Vec::with_capacity(clients.len() + 1);
        events.push(libc::pollfd {
            fd: listener.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        });
        events.extend(clients.iter().map(|(&fd, client)| libc::pollfd {
            fd,
            events: (if client.read_closed { 0 } else { libc::POLLIN })
                | if client.written < client.output.len() {
                    libc::POLLOUT
                } else {
                    0
                },
            revents: 0,
        }));
        poll(&mut events, -1)?;

        // Process existing descriptors before accepting new ones. This prevents
        // stale readiness events from being used for a reused socket descriptor.
        for event in &events[1..] {
            if event.revents == 0 {
                continue;
            }
            if event.revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
                clients.remove(&event.fd);
                continue;
            }
            if event.revents & (libc::POLLIN | libc::POLLHUP) != 0 {
                let result = match clients.get_mut(&event.fd) {
                    Some(client) => client.read_lines(),
                    None => continue,
                };
                match result {
                    Ok((lines, closed)) => {
                        for line in lines {
                            handle_line(event.fd, line, &mut clients);
                        }
                        if closed {
                            if let Some(client) = clients.get_mut(&event.fd) {
                                client.read_closed = true;
                            }
                        }
                    }
                    Err(_) => {
                        clients.remove(&event.fd);
                    }
                }
            }
            if let Some(client) = clients.get_mut(&event.fd) {
                if client.flush().is_err()
                    || (client.read_closed && client.written == client.output.len())
                {
                    clients.remove(&event.fd);
                }
            }
        }

        if events[0].revents & libc::POLLIN != 0 {
            for _ in 0..32 {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(error),
                };
                stream.set_nonblocking(true)?;
                stream.set_nodelay(true)?;
                if clients.len() >= MAX_CLIENTS {
                    let _ = stream.write(b"Server full\n");
                    continue;
                }
                let fd = stream.as_raw_fd();
                clients.insert(
                    fd,
                    Client {
                        stream,
                        nick: format!("user:{fd}"),
                        input: Vec::new(),
                        output: WELCOME.to_vec(),
                        written: 0,
                        read_closed: false,
                    },
                );
            }
        }
    }
}

fn main() -> ExitCode {
    let args: Vec<_> = env::args().skip(1).collect();
    if args.len() == 1 && matches!(args[0].as_str(), "--help" | "-h") {
        println!("Usage: smallchat-server [bind-address:port]\nDefault: 0.0.0.0:7711");
        return ExitCode::SUCCESS;
    }
    if args.len() > 1 {
        eprintln!("Usage: smallchat-server [bind-address:port]");
        return ExitCode::FAILURE;
    }
    match serve(args.first().map(String::as_str).unwrap_or("0.0.0.0:7711")) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("smallchat-server: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// An in-memory nonblocking transport with controllable EOF and backpressure.
    #[derive(Default)]
    struct Transport {
        incoming: VecDeque<u8>,
        outgoing: Vec<u8>,
        write_allowance: usize,
        eof: bool,
    }

    impl Read for Transport {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if self.incoming.is_empty() {
                return if self.eof {
                    Ok(0)
                } else {
                    Err(io::ErrorKind::WouldBlock.into())
                };
            }
            let count = buffer.len().min(self.incoming.len());
            for byte in &mut buffer[..count] {
                *byte = self.incoming.pop_front().unwrap();
            }
            Ok(count)
        }
    }

    impl Write for Transport {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if self.write_allowance == 0 {
                return Err(io::ErrorKind::WouldBlock.into());
            }
            let count = bytes.len().min(self.write_allowance);
            self.outgoing.extend_from_slice(&bytes[..count]);
            self.write_allowance -= count;
            Ok(count)
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    fn client(nick: &str) -> Client<Transport> {
        Client {
            stream: Transport::default(),
            nick: nick.to_owned(),
            input: Vec::new(),
            output: Vec::new(),
            written: 0,
            read_closed: false,
        }
    }

    #[test]
    fn retains_fragmented_lines_and_decodes_multiple_crlf_messages() {
        let mut client = client("test");
        client.stream.incoming.extend(b"/ni");
        assert_eq!(client.read_lines().unwrap(), (vec![], false));
        client
            .stream
            .incoming
            .extend("ck 小明\r\n你好\nlast".as_bytes());
        assert_eq!(
            client.read_lines().unwrap(),
            (
                vec!["/nick 小明".as_bytes().to_vec(), "你好".as_bytes().to_vec()],
                false
            )
        );
        client.stream.eof = true;
        assert_eq!(client.read_lines().unwrap(), (vec![b"last".to_vec()], true));
    }

    #[test]
    fn accepts_maximum_line_and_rejects_oversized_unterminated_input() {
        let mut client = client("test");
        client.stream.incoming.extend(vec![b'x'; MAX_LINE]);
        client.stream.incoming.push_back(b'\n');
        assert_eq!(client.read_lines().unwrap().0, vec![vec![b'x'; MAX_LINE]]);
        client.stream.incoming.extend(vec![b'x'; MAX_LINE + 1]);
        assert_eq!(
            client.read_lines().unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn preserves_output_across_partial_writes_and_new_broadcasts() {
        let mut client = client("test");
        client.queue(b"first\n").unwrap();
        client.stream.write_allowance = 3;
        client.flush().unwrap();
        assert_eq!(client.stream.outgoing, b"fir");
        client.queue(b"second\n").unwrap();
        client.stream.write_allowance = 100;
        client.flush().unwrap();
        assert_eq!(client.stream.outgoing, b"first\nsecond\n");
        assert!(client.output.is_empty());
    }

    #[test]
    fn excludes_sender_and_removes_only_the_slow_recipient() {
        let mut slow = client("slow");
        slow.queue(&vec![b'x'; MAX_PENDING]).unwrap();
        let mut clients = HashMap::from([(1, client("alice")), (2, client("bob")), (3, slow)]);
        handle_line(1, b"hello".to_vec(), &mut clients);
        assert!(clients[&1].output.is_empty());
        assert_eq!(clients[&2].output, b"alice> hello\n");
        assert!(!clients.contains_key(&3));
    }

    #[test]
    fn validates_nicknames_and_reports_unknown_commands_to_sender_only() {
        let mut clients = HashMap::from([(1, client("alice")), (2, client("bob"))]);
        handle_line(1, "/nick 小明".as_bytes().to_vec(), &mut clients);
        assert_eq!(clients[&1].nick, "小明");
        for line in [b"/nick ".as_slice(), b"/nick \xff", b"/nick a\x1b"] {
            handle_line(1, line.to_vec(), &mut clients);
            assert_eq!(clients[&1].nick, "小明");
        }
        handle_line(1, b"/unknown".to_vec(), &mut clients);
        assert!(clients[&1].output.ends_with(b"Unsupported command\n"));
        assert!(clients[&2].output.is_empty());
    }
}
