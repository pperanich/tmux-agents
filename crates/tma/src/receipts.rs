//! `tma receipts`: read the dispatch ledger `tma act --slot` writes, and dispatch nothing.
//!
//! The read half of idempotent dispatch. A caller that lost the response to a fire (an ssh drop, a
//! phone that suspended mid-request) asks here instead of sending the action again to find out what
//! happened. Reads only: no tmux, no lock on any pane, no keystroke.

use std::process::ExitCode;

use tma_runtime::json::{JsonWriter, JSON_SCHEMA};
use tma_runtime::slots::{Entry, Ledger, Receipt, ReceiptFilter};

pub(crate) struct ReceiptsOpts {
    pub slot: Option<String>,
    pub since_ms: Option<u64>,
    pub json: bool,
}

pub(crate) fn run(opts: ReceiptsOpts) -> ExitCode {
    let ledger = match Ledger::at_runtime_dir() {
        Ok(ledger) => ledger,
        Err(err) => {
            eprintln!("tma: {err}");
            return ExitCode::FAILURE;
        }
    };
    let entries = match ledger.receipts(&ReceiptFilter {
        slot: opts.slot.as_deref(),
        since_ms: opts.since_ms,
    }) {
        Ok(entries) => entries,
        Err(err) => {
            eprintln!("tma: {err}");
            return ExitCode::FAILURE;
        }
    };
    if opts.json {
        println!("{}", render_json(&entries));
    } else {
        for entry in &entries {
            if let Some(receipt) = &entry.receipt {
                println!("{}", render_line(entry, receipt));
            }
        }
    }
    ExitCode::SUCCESS
}

/// One receipt as a tab-separated line, the same shape `tma ls` uses for a row: absent fields print
/// `-` so the column count never varies.
fn render_line(entry: &Entry, receipt: &Receipt) -> String {
    [
        entry.at_ms.to_string(),
        entry.slot.clone(),
        entry.pane.clone(),
        entry.action.clone(),
        receipt.outcome.clone(),
        receipt.reason.clone().unwrap_or_else(|| "-".to_string()),
        receipt.exit_code.to_string(),
        entry.device.clone().unwrap_or_else(|| "-".to_string()),
    ]
    .join("\t")
}

/// The `--json` document: `{"schema":1,"receipts":[...]}`, each element carrying the ledger record
/// including the dispatching device.
fn render_json(entries: &[Entry]) -> String {
    let mut j = JsonWriter::new();
    j.begin_object();
    j.number("schema", JSON_SCHEMA);
    j.key("receipts");
    j.begin_array();
    for entry in entries {
        let Some(receipt) = entry.receipt.as_ref() else {
            continue;
        };
        j.begin_object();
        j.string("slot", &entry.slot);
        j.string("pane", &entry.pane);
        j.string("action", &entry.action);
        match &entry.device {
            Some(device) => j.string("device", device),
            None => j.null("device"),
        }
        j.number("at_ms", entry.at_ms as i64);
        j.string("outcome", &receipt.outcome);
        j.number("exit_code", receipt.exit_code as i64);
        match &receipt.reason {
            Some(reason) => j.string("reason", reason),
            None => j.null("reason"),
        }
        j.end_object();
    }
    j.end_array();
    j.end_object();
    j.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(device: Option<&str>, reason: Option<&str>) -> Entry {
        Entry {
            slot: "s1".to_string(),
            pane: "%5".to_string(),
            action: "approve".to_string(),
            device: device.map(str::to_string),
            at_ms: 1_730_000_000_000,
            pid: 4242,
            receipt: Some(Receipt {
                outcome: "refused".to_string(),
                reason: reason.map(str::to_string),
                exit_code: 4,
            }),
        }
    }

    fn line(e: &Entry) -> String {
        render_line(e, e.receipt.as_ref().unwrap())
    }

    #[test]
    fn a_line_keeps_its_columns_when_fields_are_absent() {
        assert_eq!(
            line(&entry(Some("phone"), Some("gated"))),
            "1730000000000\ts1\t%5\tapprove\trefused\tgated\t4\tphone"
        );
        assert_eq!(
            line(&entry(None, None)).split('\t').count(),
            8,
            "an absent device and reason still print a column"
        );
    }

    /// The document's exact shape, pinned: a dropped or renamed key is a breaking change, a new
    /// one is additive and keeps `"schema": 1`.
    #[test]
    fn the_json_document_pins_its_shape() {
        assert_eq!(
            render_json(&[entry(Some("phone"), Some("gated"))]),
            concat!(
                r#"{"schema":1,"receipts":[{"slot":"s1","pane":"%5","action":"approve","#,
                r#""device":"phone","at_ms":1730000000000,"outcome":"refused","#,
                r#""exit_code":4,"reason":"gated"}]}"#
            )
        );
        assert_eq!(render_json(&[]), r#"{"schema":1,"receipts":[]}"#);
    }
}
