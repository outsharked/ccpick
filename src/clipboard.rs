//! Copy text to the system clipboard with whatever the host offers, falling back to OSC 52.
use crate::env::Env;
use std::io::Write;
use std::process::{Command, Stdio};

struct Tool {
    program: &'static str,
    args: &'static [&'static str],
    utf16: bool,
}

/// Returns a short description of how the text was copied.
pub fn copy(text: &str, host: &Env) -> Result<&'static str, String> {
    let tools: &[Tool] = match host {
        Env::Wsl { .. } | Env::Windows => &[Tool {
            program: "clip.exe",
            args: &[],
            utf16: true,
        }],
        Env::MacOs => &[Tool {
            program: "pbcopy",
            args: &[],
            utf16: false,
        }],
        Env::Linux => &[
            Tool {
                program: "wl-copy",
                args: &[],
                utf16: false,
            },
            Tool {
                program: "xclip",
                args: &["-selection", "clipboard"],
                utf16: false,
            },
            Tool {
                program: "xsel",
                args: &["-b", "-i"],
                utf16: false,
            },
        ],
    };
    for tool in tools {
        let input = if tool.utf16 {
            utf16le_with_bom(text)
        } else {
            text.as_bytes().to_vec()
        };
        if pipe_to(tool.program, tool.args, &input) {
            return Ok(tool.program);
        }
    }
    osc52(text)
        .map(|_| "terminal")
        .map_err(|e| format!("could not copy: {e}"))
}

fn pipe_to(program: &str, args: &[&str], input: &[u8]) -> bool {
    let Ok(mut child) = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let wrote = child
        .stdin
        .take()
        .is_some_and(|mut stdin| stdin.write_all(input).is_ok());
    child.wait().is_ok_and(|status| status.success()) && wrote
}

fn osc52(text: &str) -> std::io::Result<()> {
    let mut out = std::io::stdout();
    write!(out, "\x1b]52;c;{}\x07", base64(text.as_bytes()))?;
    out.flush()
}

pub fn utf16le_with_bom(text: &str) -> Vec<u8> {
    let mut bytes = vec![0xFF, 0xFE];
    for unit in text.encode_utf16() {
        bytes.extend(unit.to_le_bytes());
    }
    bytes
}

pub fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = match chunk.len() {
            1 => (chunk[0] as u32) << 16,
            2 => ((chunk[0] as u32) << 16) | ((chunk[1] as u32) << 8),
            _ => ((chunk[0] as u32) << 16) | ((chunk[1] as u32) << 8) | chunk[2] as u32,
        };
        let symbols = chunk.len() + 1;
        for i in 0..4 {
            if i < symbols {
                out.push(TABLE[((n >> (18 - 6 * i)) & 0x3F) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_matches_rfc4648_vectors() {
        for (input, expected) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64(input.as_bytes()), expected);
        }
    }

    #[test]
    fn utf16le_has_bom() {
        assert_eq!(
            utf16le_with_bom("A→"),
            vec![0xFF, 0xFE, 0x41, 0x00, 0x92, 0x21]
        );
    }
}
