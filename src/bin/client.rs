use smallchat_rs::poll;
use std::env;
use std::io::{self, IsTerminal, Read, Write};
use std::mem::MaybeUninit;
use std::net::{Shutdown, TcpStream};
use std::os::fd::AsRawFd;
use std::process::ExitCode;
use std::time::Duration;

const MAX_INPUT: usize = 4096;

/// Restore terminal settings on every normal return, including I/O errors.
struct Terminal(libc::termios);

impl Terminal {
    fn enable() -> io::Result<Self> {
        let mut original = MaybeUninit::<libc::termios>::uninit();
        // SAFETY: tcgetattr initializes this correctly sized termios on success.
        if unsafe { libc::tcgetattr(libc::STDIN_FILENO, original.as_mut_ptr()) } < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: tcgetattr succeeded above.
        let original = unsafe { original.assume_init() };
        let mut raw = original;
        raw.c_iflag &= !(libc::BRKINT | libc::ICRNL | libc::INPCK | libc::ISTRIP | libc::IXON);
        raw.c_cflag |= libc::CS8;
        // Handle Ctrl-C ourselves so the terminal guard is dropped on exit.
        raw.c_lflag &= !(libc::ECHO | libc::ICANON | libc::IEXTEN | libc::ISIG);
        raw.c_cc[libc::VMIN] = 1;
        raw.c_cc[libc::VTIME] = 0;
        // SAFETY: raw is initialized and stdin remains open throughout this call.
        if unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &raw) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(original))
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        // SAFETY: this is the original initialized state of the same terminal.
        unsafe { libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.0) };
    }
}

fn redraw(output: &mut impl Write, input: &[u8]) -> io::Result<()> {
    output.write_all(b"\r\x1b[2K")?;
    output.write_all(input)?;
    output.flush()
}

fn send_line(
    stream: &mut TcpStream,
    output: &mut impl Write,
    input: &mut Vec<u8>,
    interactive: bool,
) -> io::Result<()> {
    if interactive {
        output.write_all(b"\r\x1b[2Kyou> ")?;
        output.write_all(input)?;
        output.write_all(b"\n")?;
        output.flush()?;
    }
    input.push(b'\n');
    stream.write_all(input)?;
    input.clear();
    Ok(())
}

fn chat(host: &str, port: u16) -> io::Result<()> {
    let mut stream = TcpStream::connect((host, port))?;
    stream.set_nodelay(true)?;
    stream.set_write_timeout(Some(Duration::from_secs(3)))?;
    let interactive = io::stdin().is_terminal() && io::stdout().is_terminal();
    let _terminal = if interactive {
        Some(Terminal::enable()?)
    } else {
        None
    };
    let mut output = io::stdout().lock();
    let mut input = Vec::new();
    let mut stdin_open = true;
    let mut escape = 0;

    loop {
        let mut events = [
            libc::pollfd {
                fd: stream.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            },
            libc::pollfd {
                fd: if stdin_open { libc::STDIN_FILENO } else { -1 },
                events: libc::POLLIN,
                revents: 0,
            },
        ];
        poll(&mut events, -1)?;
        if events[0].revents & (libc::POLLERR | libc::POLLNVAL) != 0 {
            return Err(io::Error::new(
                io::ErrorKind::ConnectionAborted,
                "connection lost",
            ));
        }
        if events[0].revents & (libc::POLLIN | libc::POLLHUP) != 0 {
            let mut buffer = [0; 4096];
            let count = match stream.read(&mut buffer) {
                Ok(0) => {
                    if interactive {
                        output.write_all(b"\r\x1b[2KConnection lost\n")?;
                    }
                    return Ok(());
                }
                Ok(count) => count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) => return Err(error),
            };
            if interactive {
                output.write_all(b"\r\x1b[2K")?;
            }
            output.write_all(&buffer[..count])?;
            if interactive {
                output.write_all(&input)?;
            }
            output.flush()?;
        }
        if events[1].revents & libc::POLLNVAL != 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "invalid stdin"));
        }
        if events[1].revents & (libc::POLLIN | libc::POLLHUP | libc::POLLERR) != 0 {
            let mut buffer = [0; 1024];
            // Use read(2) directly: a buffered stdin reader could prefetch data
            // that the next poll(2) call would then be unable to see.
            // SAFETY: buffer is writable for its full length and stdin is fd 0.
            let count =
                unsafe { libc::read(libc::STDIN_FILENO, buffer.as_mut_ptr().cast(), buffer.len()) };
            if count < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            if count == 0 {
                if !input.is_empty() {
                    send_line(&mut stream, &mut output, &mut input, interactive)?;
                }
                stream.shutdown(Shutdown::Write)?;
                stdin_open = false;
                continue;
            }
            for &byte in &buffer[..count as usize] {
                if interactive {
                    // Ignore terminal escape sequences (e.g. arrow keys).
                    if escape == 1 {
                        escape = if byte == b'[' || byte == b'O' { 2 } else { 0 };
                        continue;
                    }
                    if escape == 2 {
                        if (0x40..=0x7e).contains(&byte) {
                            escape = 0;
                        }
                        continue;
                    }
                    match byte {
                        3 | 4 => {
                            output.write_all(b"\r\x1b[2K\n")?;
                            return Ok(());
                        }
                        27 => {
                            escape = 1;
                            continue;
                        }
                        8 | 127 => {
                            if !input.is_empty() {
                                let mut start = input.len() - 1;
                                while start > 0 && input[start] & 0xc0 == 0x80 {
                                    start -= 1;
                                }
                                input.truncate(start);
                                redraw(&mut output, &input)?;
                            }
                            continue;
                        }
                        b'\r' => {
                            send_line(&mut stream, &mut output, &mut input, true)?;
                            continue;
                        }
                        b'\n' => continue,
                        0..=31 => continue,
                        _ => {}
                    }
                } else if byte == b'\n' {
                    if input.last() == Some(&b'\r') {
                        input.pop();
                    }
                    send_line(&mut stream, &mut output, &mut input, false)?;
                    continue;
                }
                if input.len() >= MAX_INPUT {
                    if interactive {
                        output.write_all(b"\x07")?;
                        output.flush()?;
                        continue;
                    }
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "input line exceeds 4096 bytes",
                    ));
                }
                input.push(byte);
                if interactive {
                    output.write_all(&[byte])?;
                    output.flush()?;
                }
            }
        }
    }
}

fn main() -> ExitCode {
    let args: Vec<_> = env::args().skip(1).collect();
    if args.len() == 1 && matches!(args[0].as_str(), "--help" | "-h") {
        println!("Usage: smallchat-client <host> <port>");
        return ExitCode::SUCCESS;
    }
    if args.len() != 2 {
        eprintln!("Usage: smallchat-client <host> <port>");
        return ExitCode::FAILURE;
    }
    let port = match args[1].parse::<u16>() {
        Ok(port) if port != 0 => port,
        _ => {
            eprintln!("smallchat-client: port must be between 1 and 65535");
            return ExitCode::FAILURE;
        }
    };
    match chat(&args[0], port) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("smallchat-client: {error}");
            ExitCode::FAILURE
        }
    }
}
