//! Wire types shared by every enishi layer. No I/O here.

pub mod ctl;
pub mod frame;
pub mod ipc;
pub mod pair;
pub mod session;

pub use session::SessionKind;

pub const ALPN_CTL: &[u8] = b"enishi/ctl/1";
pub const ALPN_STREAM: &[u8] = b"enishi/stream/1";
pub const ALPN_PAIR: &[u8] = b"enishi/pair/1";

/// Reserved process exit codes for enishi's own failures (remote exit codes pass through).
pub mod exit {
    pub const PEER_OFFLINE: i32 = 255;
    pub const NOT_ALLOWED: i32 = 254;
    pub const NO_SUCH_SERVICE: i32 = 253;
    pub const DAEMON_DOWN: i32 = 252;
    /// Bad invocation (unknown device, missing config…), same as clap's usage errors.
    pub const USAGE: i32 = 2;
    /// The command could not be found (raw spawn), like shells do.
    pub const COMMAND_NOT_FOUND: i32 = 127;
    /// The command was found but could not be started (raw spawn), like shells do.
    pub const CANNOT_EXECUTE: i32 = 126;
    /// `--timeout` expired and the process tree was killed, like `timeout(1)`.
    pub const TIMED_OUT: i32 = 124;

    /// Well-known Windows NTSTATUS exit codes: (value, name, what it means).
    const NTSTATUS: &[(u32, &str, &str)] = &[
        (
            0x4001_0004,
            "DBG_TERMINATE_PROCESS",
            "terminated (shutdown/kill)",
        ),
        (
            0xC000_013A,
            "STATUS_CONTROL_C_EXIT",
            "interrupted (Ctrl+C / console closed)",
        ),
        (
            0xC000_0005,
            "STATUS_ACCESS_VIOLATION",
            "crashed: access violation",
        ),
        (
            0xC000_00FD,
            "STATUS_STACK_OVERFLOW",
            "crashed: stack overflow",
        ),
        (
            0xC000_0409,
            "STATUS_STACK_BUFFER_OVERRUN",
            "crashed: fail-fast exception",
        ),
        (
            0xC000_0374,
            "STATUS_HEAP_CORRUPTION",
            "crashed: heap corruption",
        ),
        (
            0xC000_001D,
            "STATUS_ILLEGAL_INSTRUCTION",
            "crashed: illegal instruction",
        ),
        (
            0xC000_0094,
            "STATUS_INTEGER_DIVIDE_BY_ZERO",
            "crashed: division by zero",
        ),
        (0x8000_0003, "STATUS_BREAKPOINT", "crashed: breakpoint"),
        (0xC000_0017, "STATUS_NO_MEMORY", "out of memory"),
        (
            0xC000_0135,
            "STATUS_DLL_NOT_FOUND",
            "could not start: a DLL was not found",
        ),
        (
            0xC000_0142,
            "STATUS_DLL_INIT_FAILED",
            "could not start: DLL initialization failed",
        ),
        (
            0xC000_026B,
            "STATUS_DLL_INIT_FAILED_LOGOFF",
            "could not start: session logging off",
        ),
    ];

    /// A remote process exit code for humans (logs, CLI messages). Plain codes print as
    /// themselves; NTSTATUS-style values (a Windows process killed, crashed or
    /// interrupted: >= 0x40000000 as u32, i.e. also every negative i32) print in hex with
    /// their meaning, e.g. `0xC000013A (STATUS_CONTROL_C_EXIT: interrupted (Ctrl+C /
    /// console closed))`. The numeric code itself is propagated unchanged elsewhere.
    pub fn describe(code: i32) -> String {
        match ntstatus(code) {
            Some(text) => text,
            None => code.to_string(),
        }
    }

    /// The readable form of an NTSTATUS-style exit code, `None` for an ordinary code.
    pub fn ntstatus(code: i32) -> Option<String> {
        let v = code as u32;
        if v < 0x4000_0000 {
            return None;
        }
        Some(match NTSTATUS.iter().find(|(n, ..)| *n == v) {
            Some((_, name, what)) => format!("{v:#010X} ({name}: {what})"),
            None => {
                let kind = match v >> 30 {
                    3 => "error status",
                    2 => "warning status",
                    _ => "informational status",
                };
                format!("{v:#010X} (NTSTATUS {kind})")
            }
        })
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn describes_ntstatus() {
            assert_eq!(describe(0), "0");
            assert_eq!(describe(1), "1");
            assert_eq!(describe(255), "255");
            assert_eq!(describe(0x3FFF_FFFF), "1073741823");
            assert_eq!(ntstatus(7), None);
            assert_eq!(
                describe(0x4001_0004),
                "0x40010004 (DBG_TERMINATE_PROCESS: terminated (shutdown/kill))"
            );
            assert_eq!(
                describe(0xC000_013Au32 as i32),
                "0xC000013A (STATUS_CONTROL_C_EXIT: interrupted (Ctrl+C / console closed))"
            );
            assert_eq!(
                describe(-1073741819), // 0xC0000005 as Windows/Rust report it
                "0xC0000005 (STATUS_ACCESS_VIOLATION: crashed: access violation)"
            );
            assert_eq!(
                describe(0xC0DE_0001u32 as i32),
                "0xC0DE0001 (NTSTATUS error status)"
            );
            assert_eq!(
                describe(0x8000_0001u32 as i32),
                "0x80000001 (NTSTATUS warning status)"
            );
            assert_eq!(
                describe(0x4000_0000),
                "0x40000000 (NTSTATUS informational status)"
            );
            assert_eq!(describe(-1), "0xFFFFFFFF (NTSTATUS error status)");
        }
    }
}
