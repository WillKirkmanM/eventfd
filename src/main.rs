use clap::Parser;
use std::process::ExitCode;

#[derive(Debug, Parser)]
#[command(version, about = "Demonstrate Linux event counters")]
struct Args {
    #[arg(long, default_value_t = 0, help = "Set the initial counter value")]
    initial: u32,

    #[arg(
        long,
        value_parser = parse_addition,
        help = "Add a value before reading; may be repeated"
    )]
    add: Vec<u64>,

    #[arg(long, help = "Drain the counter one count at a time")]
    semaphore: bool,
}

fn parse_addition(value: &str) -> Result<u64, String> {
    let value = value
        .parse::<u64>()
        .map_err(|_| "expected an unsigned 64-bit integer".to_owned())?;
    if value == u64::MAX {
        return Err("18446744073709551615 is forbidden by eventfd".to_owned());
    }
    Ok(value)
}

#[cfg(target_os = "linux")]
mod linux {
    use super::Args;
    use std::ffi::{c_int, c_uint, c_void};
    use std::io::{self, Write};

    const EFD_SEMAPHORE: c_int = 1;
    const EFD_CLOEXEC: c_int = 0o2000000;

    #[cfg(any(
        target_arch = "mips",
        target_arch = "mips64",
        target_arch = "mips32r6",
        target_arch = "mips64r6"
    ))]
    const EFD_NONBLOCK: c_int = 0x80;
    #[cfg(any(target_arch = "sparc", target_arch = "sparc64"))]
    const EFD_NONBLOCK: c_int = 0x4000;
    #[cfg(not(any(
        target_arch = "mips",
        target_arch = "mips64",
        target_arch = "mips32r6",
        target_arch = "mips64r6",
        target_arch = "sparc",
        target_arch = "sparc64"
    )))]
    const EFD_NONBLOCK: c_int = 0x800;

    unsafe extern "C" {
        fn eventfd(initval: c_uint, flags: c_int) -> c_int;
        fn read(fd: c_int, buf: *mut c_void, count: usize) -> isize;
        fn write(fd: c_int, buf: *const c_void, count: usize) -> isize;
        fn close(fd: c_int) -> c_int;
    }

    struct Counter(c_int);

    impl Counter {
        fn new(initial: u32, semaphore: bool) -> io::Result<Self> {
            let flags = EFD_NONBLOCK | EFD_CLOEXEC | if semaphore { EFD_SEMAPHORE } else { 0 };
            let fd = unsafe { eventfd(initial, flags) };
            if fd < 0 {
                Err(io::Error::last_os_error())
            } else {
                Ok(Self(fd))
            }
        }

        fn add(&self, value: u64) -> io::Result<()> {
            if value == u64::MAX {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "18446744073709551615 is forbidden by eventfd",
                ));
            }
            let bytes = value.to_ne_bytes();
            loop {
                let result = unsafe { write(self.0, bytes.as_ptr().cast(), bytes.len()) };
                if result == bytes.len() as isize {
                    return Ok(());
                }
                if result >= 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "eventfd returned a short write",
                    ));
                }
                let error = io::Error::last_os_error();
                match error.kind() {
                    io::ErrorKind::Interrupted => continue,
                    io::ErrorKind::WouldBlock => {
                        return Err(io::Error::new(
                            io::ErrorKind::WouldBlock,
                            "addition would overflow the event counter",
                        ));
                    }
                    _ => return Err(error),
                }
            }
        }

        fn next(&self) -> io::Result<Option<u64>> {
            let mut bytes = [0_u8; 8];
            loop {
                let result = unsafe { read(self.0, bytes.as_mut_ptr().cast(), bytes.len()) };
                if result == bytes.len() as isize {
                    return Ok(Some(u64::from_ne_bytes(bytes)));
                }
                if result >= 0 {
                    return Err(io::Error::new(
                        io::ErrorKind::UnexpectedEof,
                        "eventfd returned a short read",
                    ));
                }
                let error = io::Error::last_os_error();
                match error.kind() {
                    io::ErrorKind::Interrupted => continue,
                    io::ErrorKind::WouldBlock => return Ok(None),
                    _ => return Err(error),
                }
            }
        }
    }

    impl Drop for Counter {
        fn drop(&mut self) {
            unsafe { close(self.0) };
        }
    }

    pub fn run(args: &Args) -> Result<(), String> {
        let counter = Counter::new(args.initial, args.semaphore)
            .map_err(|error| format!("could not create event counter: {error}"))?;
        for &value in &args.add {
            counter
                .add(value)
                .map_err(|error| format!("could not add {value}: {error}"))?;
        }
        let mut output = io::stdout().lock();
        let mut printed = false;
        loop {
            let value = counter
                .next()
                .map_err(|error| format!("could not read event counter: {error}"))?;
            let Some(value) = value else {
                if !printed {
                    writeln!(output, "empty")
                        .map_err(|error| format!("could not write output: {error}"))?;
                }
                break;
            };
            writeln!(output, "{value}")
                .map_err(|error| format!("could not write output: {error}"))?;
            printed = true;
            if !args.semaphore {
                break;
            }
        }
        output
            .flush()
            .map_err(|error| format!("could not flush output: {error}"))
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn additions_are_combined_and_read_resets_the_counter() {
            let counter = Counter::new(2, false).unwrap();
            counter.add(3).unwrap();
            counter.add(0).unwrap();
            assert_eq!(counter.next().unwrap(), Some(5));
            assert_eq!(counter.next().unwrap(), None);
        }

        #[test]
        fn semaphore_reads_consume_one_count() {
            let counter = Counter::new(2, true).unwrap();
            counter.add(1).unwrap();
            for _ in 0..3 {
                assert_eq!(counter.next().unwrap(), Some(1));
            }
            assert_eq!(counter.next().unwrap(), None);
        }

        #[test]
        fn overflowing_write_fails_without_changing_the_counter() {
            let counter = Counter::new(0, false).unwrap();
            counter.add(u64::MAX - 1).unwrap();
            assert_eq!(
                counter.add(1).unwrap_err().kind(),
                io::ErrorKind::WouldBlock
            );
            assert_eq!(counter.next().unwrap(), Some(u64::MAX - 1));
        }
    }
}

fn main() -> ExitCode {
    let args = Args::parse();
    #[cfg(target_os = "linux")]
    let result = linux::run(&args);
    #[cfg(not(target_os = "linux"))]
    let result: Result<(), String> = {
        let _ = args;
        Err("eventfd is only supported on Linux".to_owned())
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("error: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_count_must_fit_in_u32() {
        assert!(Args::try_parse_from(["eventfd", "--initial", "4294967296"]).is_err());
        let args = Args::try_parse_from(["eventfd", "--initial", "4294967295"]).unwrap();
        assert_eq!(args.initial, u32::MAX);
    }

    #[test]
    fn additions_reject_forbidden_and_malformed_values() {
        for value in ["18446744073709551615", "18446744073709551616", "-1", "abc"] {
            assert!(Args::try_parse_from(["eventfd", "--add", value]).is_err());
        }
        let args = Args::try_parse_from(["eventfd", "--add", "0", "--add", "3"]).unwrap();
        assert_eq!(args.add, vec![0, 3]);
    }
}
