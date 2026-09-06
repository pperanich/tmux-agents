//! `tma device`: pair, grant, revoke and list the remote devices `tma serve` will answer.
//!
//! The whole write side of U10's authorization model. Grants are CLI-only by design: there is no
//! in-app path to widen a scope, no approval prompt a device can raise, and no protocol frame that
//! asks for one. A device that wants `act:always` gets told to ask the person at the terminal.
//!
//! Exit codes: `0` done, `2` usage, `3` no device by that name, `1` the store could not be read or
//! written.

use std::process::ExitCode;

use tma_proto::Scope;
use tma_runtime::device::{Device, DeviceError, Store};
use tma_runtime::json::{JsonWriter, JSON_SCHEMA};

use crate::cli::{DeviceArgs, DeviceCommand};

pub(crate) fn run(args: DeviceArgs) -> ExitCode {
    let store = match Store::at_config_dir() {
        Ok(store) => store,
        Err(err) => return fail(&err),
    };
    match args.command {
        DeviceCommand::Pair(a) => {
            match store.pair(&a.id, &a.name, &a.scope, a.only, tma_runtime::now_ms()) {
                Ok(device) => {
                    println!("paired {} as {}", device.id, device.name);
                    println!("  scopes: {}", scope_list(&device));
                    println!(
                        "  add the forced command to ~/.ssh/authorized_keys:\n    \
                     command=\"tma serve --stdio --device {}\",restrict <the device's public key>",
                        device.id
                    );
                    ExitCode::SUCCESS
                }
                Err(err) => fail(&err),
            }
        }
        DeviceCommand::Grant(a) => match store.grant(&a.name, a.scope) {
            Ok(device) => {
                println!("{} now holds: {}", device.name, scope_list(&device));
                ExitCode::SUCCESS
            }
            Err(err) => fail(&err),
        },
        DeviceCommand::Revoke(a) => match store.revoke(&a.name) {
            Ok(device) => {
                // The record is gone, which is what every live serve process re-reads. The
                // `authorized_keys` line is the user's to remove: tma did not write it.
                println!("revoked {} ({})", device.name, device.id);
                println!(
                    "  remove its `command=\"tma serve --stdio --device {}\"` line from \
                     ~/.ssh/authorized_keys too, so the next dial is refused by sshd as well",
                    device.id
                );
                ExitCode::SUCCESS
            }
            Err(err) => fail(&err),
        },
        DeviceCommand::List(a) => match store.load() {
            Ok(devices) => {
                if a.json {
                    println!("{}", render_json(&devices));
                } else {
                    for device in &devices {
                        println!("{}", render_line(device));
                    }
                }
                ExitCode::SUCCESS
            }
            Err(err) => fail(&err),
        },
    }
}

/// Print the error and map it to an exit code: `3` for a name that is not in the store (the
/// "target does not exist" code `wait` and `act` already use), `1` for everything else.
fn fail(err: &DeviceError) -> ExitCode {
    eprintln!("tma: {err}");
    match err {
        DeviceError::NoSuchName { .. } => ExitCode::from(3),
        _ => ExitCode::FAILURE,
    }
}

fn scope_list(device: &Device) -> String {
    device
        .scopes
        .iter()
        .map(|s| s.token())
        .collect::<Vec<_>>()
        .join(", ")
}

/// One device as a tab-separated line, the shape `tma ls` uses for a row.
fn render_line(device: &Device) -> String {
    [
        device.name.clone(),
        device.id.clone(),
        device
            .scopes
            .iter()
            .map(|s| s.token().to_string())
            .collect::<Vec<_>>()
            .join(","),
        device.paired_at_ms.to_string(),
    ]
    .join("\t")
}

/// The `--json` document: `{"schema":1,"devices":[...]}`. `notify_key` is deliberately absent —
/// nothing writes it yet, and a null key nobody populates reads as a promise.
fn render_json(devices: &[Device]) -> String {
    let mut j = JsonWriter::new();
    j.begin_object();
    j.number("schema", JSON_SCHEMA);
    j.key("devices");
    j.begin_array();
    for device in devices {
        j.begin_object();
        j.string("name", &device.name);
        j.string("id", &device.id);
        j.key("scopes");
        j.begin_array();
        for scope in &device.scopes {
            j.raw_string(scope.token());
        }
        j.end_array();
        j.number("paired_at_ms", device.paired_at_ms as i64);
        j.end_object();
    }
    j.end_array();
    j.end_object();
    j.finish()
}

/// clap value parser for a scope: the wire vocabulary and nothing else, so a typo is a usage error
/// naming the four tokens rather than a grant nobody can use.
pub(crate) fn parse_scope(s: &str) -> Result<Scope, String> {
    Scope::from_token(s)
        .ok_or_else(|| format!("unknown scope {s:?} (one of: {})", Scope::TOKENS.join(", ")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(name: &str, scopes: &[Scope]) -> Device {
        Device {
            id: format!("SHA256:{name}"),
            name: name.to_string(),
            scopes: scopes.to_vec(),
            paired_at_ms: 1_730_000_000_000,
            notify_key: None,
        }
    }

    #[test]
    fn a_line_keeps_its_columns() {
        assert_eq!(
            render_line(&device("phone", &tma_runtime::device::DEFAULT_SCOPES)),
            "phone\tSHA256:phone\tread,act:answer,act:steer\t1730000000000"
        );
    }

    /// The document's exact shape, pinned: a dropped or renamed key is a breaking change and a new
    /// one is additive, the same rule every other tma `--json` surface holds.
    #[test]
    fn the_json_document_pins_its_shape() {
        assert_eq!(
            render_json(&[device("phone", &[Scope::Read, Scope::ActAlways])]),
            concat!(
                r#"{"schema":1,"devices":[{"name":"phone","id":"SHA256:phone","#,
                r#""scopes":["read","act:always"],"paired_at_ms":1730000000000}]}"#
            )
        );
        assert_eq!(render_json(&[]), r#"{"schema":1,"devices":[]}"#);
    }

    #[test]
    fn only_the_four_wire_tokens_parse() {
        for token in Scope::TOKENS {
            assert_eq!(parse_scope(token).unwrap().token(), *token);
        }
        assert!(parse_scope("act:everything").is_err());
        assert!(parse_scope("").is_err());
    }
}
